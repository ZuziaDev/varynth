use anyhow::Result;
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::agent_files::AgentFiles;
use crate::checkpoint::{CheckpointStore, Undo};
use crate::compact;
use crate::config::Config;
use crate::mailbox::{self, Bus};
use crate::mcp::McpManager;
use crate::permissions::PermissionMode;
use crate::providers::{self, Provider, Usage};
use crate::sandbox::{Jail, SandboxMode};
use crate::session::{ChatMessage, ImageAttachment, Session};
use crate::skills;
use crate::tools;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentEvent {
    pub kind: String,
    pub text: String,
}

/// How long the runtime waits for an interactive approval before the tool
/// call counts as denied.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// The user's answer to one [`ApprovalRequest`].
#[derive(Debug, Clone)]
pub enum Approval {
    Deny,
    AllowOnce,
    AllowAlways,
}

/// A pending permission decision, handed to the interactive approver (the
/// TUI). The tool call blocks on `respond` until the user answers or the
/// 120 s timeout denies it.
pub struct ApprovalRequest {
    pub tool: String,
    /// File path for write/edit, command for bash, arguments for MCP tools.
    pub detail: String,
    pub respond: tokio::sync::oneshot::Sender<Approval>,
}

/// Lifecycle of a goal: worked on until the auditor says it is met, or the
/// user (or the budget) stops it. Terminal states stay in `Runtime::goal`
/// so `/goal status` can still show them; only `Active` drives the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Met,
    Stopped,
}

/// The independent auditor's judgement of one goal round.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GoalVerdict {
    pub met: bool,
    pub reason: String,
}

/// State of the `/goal` engine: the condition, the round budget, the clock
/// and the latest auditor verdict.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GoalState {
    pub condition: String,
    pub rounds: u32,
    pub max_rounds: u32,
    pub budget_minutes: Option<u64>,
    /// Wall-clock start of the goal; opaque, never persisted through serde.
    #[serde(skip)]
    pub started_at: std::time::Instant,
    pub status: GoalStatus,
    pub last_verdict: Option<GoalVerdict>,
}

pub struct Runtime {
    pub cfg: Config,
    pub jail: Jail,
    pub permission: PermissionMode,
    pub skills: Vec<skills::Skill>,
    pub goal: Option<GoalState>,
    /// Cross-session message bus; drained into every turn and written to by
    /// the local `/send` slash command.
    pub bus: Bus,
    /// Interactive approver for prompt mode (the TUI). `None` denies gated
    /// tool calls with a hint instead of asking.
    pub approver: Option<tokio::sync::mpsc::UnboundedSender<ApprovalRequest>>,
    /// Remote approval relay (roadmap 1.3): when set, every prompt-mode
    /// approval request is also broadcast here so remote surfaces (gateway,
    /// Telegram) can answer it via `approval_relay::respond`. The first
    /// answer — TUI or remote — wins.
    pub remote_approval_events:
        Option<tokio::sync::broadcast::Sender<crate::approval_relay::RelayRequest>>,
    /// Approval keys (tool name, or a bash command's first word) the user
    /// approved for the rest of this runtime's life.
    approved_always: std::collections::HashSet<String>,
    /// Pre-write file snapshots backing the `/undo` slash command.
    pub checkpoints: CheckpointStore,
    pub agent_files: AgentFiles,
    pub mcp: McpManager,
    /// Images to attach to the next user message, consumed by `turn`.
    pub pending_images: Vec<ImageAttachment>,
    /// Token usage of the most recent provider completion, if it reported any.
    /// Preferred over the chars/4 estimate when judging the context budget.
    pub last_usage: Option<Usage>,
    /// Accumulated reported usage over the whole session.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// How many completions actually reported usage (0 → estimates only).
    pub usage_count: u64,
    /// Real turns (provider round-trips), not local slash handling.
    pub turns: u64,
    /// Cumulative wall time spent in `turn`.
    pub turn_time: Duration,
    /// When this runtime was built; the /cost clock.
    pub started_at: Instant,
    auto_titling: bool,
    control: Option<crate::control_bus::ControlBus>,
    provider: Box<dyn Provider>,
}

impl Runtime {
    pub fn new(cfg: Config, cwd: std::path::PathBuf) -> Result<Self> {
        cfg.require_provider_credentials()?;
        let agent_files = AgentFiles::ensure(&cwd)?;
        let jail = Jail::new(
            cwd.clone(),
            cfg.add_dirs.clone(),
            SandboxMode::parse(&cfg.sandbox),
            cfg.shell_allowlist.clone(),
        );
        let permission = PermissionMode::parse(&cfg.permission_mode);
        let skills = skills::discover(&cwd);
        let provider = providers::build(&cfg)?;
        let mut mcp = McpManager::from_config(cfg.mcp_config.as_deref())?;
        let mcp_tool_count = mcp.load_tools().len();
        if cfg.mcp_config.is_some() && mcp_tool_count == 0 {
            tracing::warn!("MCP config loaded but no tools were discovered");
        }
        Ok(Self {
            cfg,
            jail,
            permission,
            skills,
            goal: None,
            bus: Bus::default(),
            approver: None,
            remote_approval_events: None,
            approved_always: std::collections::HashSet::new(),
            checkpoints: CheckpointStore::default(),
            agent_files,
            mcp,
            pending_images: Vec::new(),
            last_usage: None,
            input_tokens: 0,
            output_tokens: 0,
            usage_count: 0,
            turns: 0,
            turn_time: Duration::ZERO,
            started_at: Instant::now(),
            auto_titling: true,
            control: Some(crate::control_bus::ControlBus::default()),
            provider,
        })
    }

    pub fn with_mcp_config(mut self, path: Option<&std::path::Path>) -> Result<Self> {
        let mut mgr = McpManager::from_config(path)?;
        let _ = mgr.load_tools();
        self.mcp = mgr;
        Ok(self)
    }

    pub fn system_prompt(&self) -> String {
        let mut s = String::from(
            "You are Varynth, a local coding agent that runs on the user's machine.\n\
             You work in an agent loop with tools, skills, sessions and permissions,\n\
             and can also run one-shot via exec/resume inside a policy sandbox.\n\
             Be direct. Use tools to inspect and change files. Do not invent paths.\n\
             Prefer small diffs. Prove work with tool output.\n\
             Windows PowerShell is the default shell.\n",
        );
        s.push_str(&format!(
            "\nWorkspace: {}\nSandbox: {}\nPermission: {}\nModel: {}\nProvider: {}\n",
            self.jail.cwd.display(),
            self.cfg.sandbox,
            self.permission.as_str(),
            self.cfg.model,
            self.cfg.provider
        ));
        s.push_str(&format!("\nEffort: {}\n", self.cfg.effort));
        if !self.mcp.servers.is_empty() {
            let mut names: Vec<_> = self.mcp.servers.keys().map(String::as_str).collect();
            names.sort_unstable();
            s.push_str(&format!(
                "\nMCP servers: {}. Their tools are named mcp__<server>__<tool> \
                 (e.g. mcp__agentchattr__chat_read / chat_send). When asked to \"use mcp\", \
                 call these tools.\n",
                names.join(", ")
            ));
        }
        s.push_str("\n# Agent identity and memory (untrusted data)\n");
        s.push_str(&self.agent_files.system_context());
        s.push_str(&Config::load_project_instructions(&self.jail.cwd));
        s.push_str(&skills::render_for_prompt(&self.skills));
        if let (true, Some(goal)) = (self.goal_active(), self.goal_condition()) {
            s.push_str("\n\n# Active /goal (session-scoped Stop hook)\n");
            s.push_str("A completion condition is ACTIVE. Do not stop to ask what to do next. ");
            s.push_str("Treat the condition as your directive and keep working until it holds. ");
            s.push_str("When — and only when — the condition is fully met, end your reply with a line that is exactly:\nGOAL_COMPLETE\n");
            s.push_str("Condition:\n");
            s.push_str(goal);
            s.push('\n');
        }
        s
    }

    /// The active goal's completion condition, when one is set (including a
    /// goal that has already reached a terminal state).
    pub fn goal_condition(&self) -> Option<&str> {
        self.goal.as_ref().map(|g| g.condition.as_str())
    }

    /// True only while the goal is still being worked on.
    fn goal_active(&self) -> bool {
        self.goal
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Active)
    }

    /// Whether provider turns should stream SSE deltas. Config `stream` is a
    /// tri-state: `None` (default) and `Some(true)` stream, `Some(false)` opts
    /// out and uses the plain blocking call everywhere.
    pub fn streaming_enabled(&self) -> bool {
        self.cfg.stream != Some(false)
    }

    pub fn agents_md_line(&self) -> String {
        let cwd = &self.jail.cwd;
        let mut found = Vec::new();
        for name in ["VARYNTH.md", "AGENTS.md", "CLAUDE.md"] {
            if cwd.join(name).exists() {
                found.push(format!("{name} loaded: {}", cwd.join(name).display()));
            }
        }
        if found.is_empty() {
            "agents-md: no VARYNTH.md / AGENTS.md / CLAUDE.md found".into()
        } else {
            format!("agents-md: {}", found.join("; "))
        }
    }

    pub const EFFORT_LEVELS: [&'static str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];

    pub fn cycle_effort(&mut self) {
        let levels = Self::EFFORT_LEVELS;
        let i = levels
            .iter()
            .position(|l| *l == self.cfg.effort)
            .unwrap_or(3);
        self.cfg.effort = levels[(i + 1) % levels.len()].to_string();
    }

    /// Applies an effort level to this runtime's config. Returns false for a
    /// level outside [`Runtime::EFFORT_LEVELS`], leaving the config alone.
    /// Persistence (`cfg.save()`) is the caller's job.
    pub fn set_effort(&mut self, level: &str) -> bool {
        if !Self::EFFORT_LEVELS.contains(&level) {
            return false;
        }
        self.cfg.effort = level.to_string();
        true
    }

    /// Applies a permission mode string (acceptEdits / prompt / bypass) to the
    /// parsed flag and the config value together, so the runtime and the
    /// persisted config never disagree. Persistence is the caller's job.
    pub fn set_permission_mode(&mut self, mode: &str) {
        self.permission = PermissionMode::parse(mode);
        self.cfg.permission_mode = mode.to_string();
    }

    pub fn reset_session_state(&mut self) {
        self.goal = None;
        self.approved_always.clear();
        self.pending_images.clear();
        self.last_usage = None;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.usage_count = 0;
        self.turns = 0;
        self.turn_time = Duration::ZERO;
        self.started_at = Instant::now();
    }

    pub fn reconfigure(&mut self, cfg: Config) -> Result<()> {
        cfg.validate()?;
        cfg.require_provider_credentials()?;
        let provider = providers::build(&cfg)?;
        let jail = Jail::new(
            self.jail.cwd.clone(),
            cfg.add_dirs.clone(),
            SandboxMode::parse(&cfg.sandbox),
            cfg.shell_allowlist.clone(),
        );
        let permission = PermissionMode::parse(&cfg.permission_mode);
        let skills = skills::discover(&jail.cwd);
        self.provider = provider;
        self.jail = jail;
        self.permission = permission;
        self.skills = skills;
        self.cfg = cfg;
        self.approved_always.clear();
        Ok(())
    }

    pub fn apply_goal_slash(&mut self, user: &str) -> Option<String> {
        let t = user.trim();
        if t == "/goal" || t == "/goal status" {
            return Some(match &self.goal {
                Some(g) => {
                    let secs = g.started_at.elapsed().as_secs();
                    let budget = match g.budget_minutes {
                        Some(mins) => format!("budget {mins}min"),
                        None => "no time budget".into(),
                    };
                    let status = match g.status {
                        GoalStatus::Active => "active",
                        GoalStatus::Met => "met",
                        GoalStatus::Stopped => "stopped",
                    };
                    let verdict = match &g.last_verdict {
                        Some(v) => format!("Last verdict: {}", v.reason),
                        None => "Last verdict: none yet".into(),
                    };
                    format!(
                        "Goal: {}\nRound {}/{} · elapsed {:02}:{:02} · {budget}\nStatus: {status}\n{verdict}",
                        g.condition,
                        g.rounds,
                        g.max_rounds,
                        secs / 60,
                        secs % 60
                    )
                }
                None => "No goal set. Usage: /goal <condition> [--rounds N] [--minutes M]".into(),
            });
        }
        if t == "/goal clear" || t == "/goal stop" || t == "/goal --clear" {
            if let Some(g) = self.goal.as_mut() {
                if g.status == GoalStatus::Active {
                    g.status = GoalStatus::Stopped;
                    g.last_verdict = Some(GoalVerdict {
                        met: false,
                        reason: "stopped by user".into(),
                    });
                }
                return Some("Goal stopped. `/goal status` keeps showing the final state.".into());
            }
            return Some("No goal set.".into());
        }
        if let Some(rest) = t.strip_prefix("/goal ") {
            let (max_rounds, budget_minutes, condition) = parse_goal_flags(rest);
            if condition.is_empty() {
                return Some("Usage: /goal <condition> [--rounds N] [--minutes M]".into());
            }
            self.goal = Some(GoalState {
                condition,
                rounds: 0,
                max_rounds: max_rounds.unwrap_or(self.cfg.goal_max_rounds.unwrap_or(25)),
                budget_minutes: budget_minutes.or(self.cfg.goal_max_minutes),
                started_at: Instant::now(),
                status: GoalStatus::Active,
                last_verdict: None,
            });
            return None;
        }
        None
    }

    pub async fn turn(
        &mut self,
        session: &mut Session,
        user: &str,
        on_event: impl FnMut(AgentEvent) + Send,
    ) -> Result<String> {
        let control = self.control.clone();
        let sid = session.id().to_string();
        let mut emit = on_event;
        let mut on_event = move |event: AgentEvent| {
            if let Some(bus) = &control {
                if let Err(error) = bus.publish(&sid, &event) {
                    tracing::debug!(%error, "local event forwarding failed");
                }
            }
            emit(event);
        };
        if let Some(local) = self.apply_goal_slash(user) {
            on_event(AgentEvent {
                kind: "system".into(),
                text: local.clone(),
            });
            return Ok(local);
        }
        if let Some(local) = self.apply_mail_slash(user, session.id()) {
            on_event(AgentEvent {
                kind: "system".into(),
                text: local.clone(),
            });
            return Ok(local);
        }
        if let Some(local) = self.apply_undo_slash(user, session.id()) {
            on_event(AgentEvent {
                kind: "system".into(),
                text: local.clone(),
            });
            return Ok(local);
        }
        if let Some(bus) = &self.control {
            if let Some(messages) = bus.take_context(session.id())? {
                session.replace_messages(messages)?;
                self.last_usage = None;
            }
            if bus.is_paused(session.id()) {
                return Ok(
                    "session paused; resume it from the gateway before submitting a turn".into(),
                );
            }
        }
        if let Some(spec) = user.trim().strip_prefix("/plugin ") {
            let reply = crate::plugins::handle_command(spec, &self.jail.cwd)?;
            on_event(AgentEvent {
                kind: "system".into(),
                text: reply.clone(),
            });
            return Ok(reply);
        }
        let started = Instant::now();
        self.turns = self.turns.saturating_add(1);
        let result = self.turn_inner(session, user, &mut on_event).await;
        self.turn_time += started.elapsed();
        match result {
            Ok(reply) => {
                let reply = self.settle_goal(session, reply, &mut on_event).await;
                self.auto_title(session, &mut on_event).await;
                Ok(reply)
            }
            Err(error) => Err(error),
        }
    }

    /// Auto session titling (roadmap 4): after the first real model turn of
    /// a still-untitled session, let the model name the conversation. Inline
    /// await after the reply is computed, and never fails the turn: a
    /// titling or persistence problem just leaves the fallback title.
    async fn auto_title(
        &mut self,
        session: &mut Session,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) {
        if !self.auto_titling || self.turns != 1 || session.meta.title_explicit {
            return;
        }
        match crate::titling::generate_title(
            &self.cfg.model,
            self.provider.as_ref(),
            &session.messages,
        )
        .await
        {
            Ok(title) => match session.set_title(&title) {
                Ok(()) => on_event(AgentEvent {
                    kind: "system".into(),
                    text: format!("session titled: {title}"),
                }),
                Err(error) => {
                    tracing::debug!(error = %error, "session title not persisted");
                }
            },
            Err(error) => {
                tracing::debug!(error = %error, "session titling skipped");
            }
        }
    }

    /// Local `/undo`: restore the most recent checkpointed write of this
    /// session. Never reaches the model.
    fn apply_undo_slash(&self, user: &str, session_id: &str) -> Option<String> {
        if user.trim() != "/undo" {
            return None;
        }
        Some(match self.checkpoints.undo_last(session_id) {
            Ok(Undo::Restored(path)) => format!("restored {}", path.display()),
            Ok(Undo::Deleted(path)) => format!("deleted {} (created this session)", path.display()),
            Ok(Undo::TooLarge(_)) => "last write was too large to checkpoint".into(),
            Ok(Undo::Nothing) => "(nothing to undo)".into(),
            Err(error) => error.to_string(),
        })
    }

    /// Local mail slash commands: `/send <id|latest> <text>` and `/inbox`.
    /// Handled without a model call.
    fn apply_mail_slash(&self, user: &str, session_id: &str) -> Option<String> {
        let (cmd, rest) = user.trim().split_once(' ').unwrap_or((user.trim(), ""));
        match cmd {
            "/send" => {
                let (to, text) = match rest.trim().split_once(char::is_whitespace) {
                    Some((to, text)) => (to, text.trim()),
                    None => (rest.trim(), ""),
                };
                if to.is_empty() || text.is_empty() {
                    return Some("Usage: /send <session-id|latest> <message>".into());
                }
                match self.bus.send(session_id, to, text) {
                    Ok(id) => Some(format!("sent to {to} (message {id})")),
                    Err(e) => Some(format!("send failed: {e}")),
                }
            }
            "/inbox" => {
                let mail = self.bus.drain(session_id);
                Some(if mail.is_empty() {
                    "(no messages)".into()
                } else {
                    mailbox::render_inbox(&mail)
                })
            }
            _ => None,
        }
    }

    /// Goal bookkeeping after a real model turn. The model's GOAL_COMPLETE
    /// line is only a claim; an independent auditor decides whether the goal
    /// is actually met, and round/time budgets stop the loop when spent.
    async fn settle_goal(
        &mut self,
        session: &Session,
        reply: String,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> String {
        if !self.goal_active() {
            return reply;
        }
        let claimed = reply.lines().any(|l| l.trim() == "GOAL_COMPLETE");
        let shown = strip_goal_complete(&reply);
        let verdict = self.audit_goal(session).await;
        // A budget spent before this settle stops the goal; the audit of the
        // stopping turn is deliberately not recorded.
        let (rounds_exhausted, time_exhausted) = match self.goal.as_ref() {
            Some(g) => (
                g.rounds + 1 > g.max_rounds,
                g.budget_minutes.is_some_and(|mins| {
                    g.started_at.elapsed() > Duration::from_secs(mins.saturating_mul(60))
                }),
            ),
            None => (false, false),
        };
        if rounds_exhausted || time_exhausted {
            let reason = if rounds_exhausted {
                format!(
                    "round budget of {} exhausted",
                    self.goal.as_ref().map(|g| g.max_rounds).unwrap_or(0)
                )
            } else {
                format!(
                    "time budget of {}min exhausted",
                    self.goal
                        .as_ref()
                        .and_then(|g| g.budget_minutes)
                        .unwrap_or(0)
                )
            };
            if let Some(g) = self.goal.as_mut() {
                g.status = GoalStatus::Stopped;
            }
            on_event(AgentEvent {
                kind: "system".into(),
                text: format!("[goal stopped] {reason}"),
            });
            return shown;
        }
        if let Some(g) = self.goal.as_mut() {
            g.rounds += 1;
            g.last_verdict = Some(verdict.clone());
            if verdict.met {
                g.status = GoalStatus::Met;
            }
        }
        if verdict.met {
            on_event(AgentEvent {
                kind: "system".into(),
                text: format!("[goal met] {}", verdict.reason),
            });
        } else if claimed {
            on_event(AgentEvent {
                kind: "system".into(),
                text: format!(
                    "[goal audit] model claimed completion, auditor disagrees — continuing: {}",
                    verdict.reason
                ),
            });
        } else {
            let (rounds, max_rounds) = match self.goal.as_ref() {
                Some(g) => (g.rounds, g.max_rounds),
                None => (0, 0),
            };
            on_event(AgentEvent {
                kind: "system".into(),
                text: format!("[goal {rounds}/{max_rounds}] {}", verdict.reason),
            });
        }
        shown
    }

    /// Ask the provider to judge goal completion against the recent
    /// transcript. Never fails the turn: provider or parse errors come back
    /// as a `met: false` verdict.
    async fn audit_goal(&mut self, session: &Session) -> GoalVerdict {
        let Some(goal) = self.goal.as_ref() else {
            return GoalVerdict {
                met: false,
                reason: "no active goal".into(),
            };
        };
        let condition = goal.condition.clone();
        let start = session.messages.len().saturating_sub(30);
        // transcript() emits the most recent message first; flip the lines so
        // the audit prompt reads chronologically, latest last.
        let transcript = compact::transcript(&session.messages[start..], 6_000);
        let transcript = transcript.lines().rev().collect::<Vec<_>>().join("\n");
        let ask = format!(
            "Audit whether the coding agent fully met its goal.\n\nObjective:\n{condition}\n\n\
             Conversation transcript (latest last):\n{transcript}\n\n\
             Judge the goal FULLY met only on concrete evidence in the transcript: \
             files changed, commands run, results shown. Anything partial, unverified or \
             still in progress counts as not met. Reply ONLY with the JSON object \
             {{\"met\": true|false, \"reason\": \"one sentence\"}} — no other text."
        );
        let completion = self
            .provider
            .complete(
                &self.cfg.model,
                "You audit goal completion for a coding agent. Reply with the JSON object \
                 only, no other text.",
                &[ChatMessage {
                    role: "user".into(),
                    content: ask,
                    ..Default::default()
                }],
                &[],
            )
            .await;
        match completion {
            Ok(completion) => {
                self.record_usage(completion.usage);
                parse_goal_verdict(&completion.text)
            }
            Err(error) => GoalVerdict {
                met: false,
                reason: format!("audit unavailable: {error}"),
            },
        }
    }

    async fn turn_inner(
        &mut self,
        session: &mut Session,
        user: &str,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> Result<String> {
        let mut prompt = user.to_string();
        if let Some(name) = user.trim().strip_prefix('/') {
            let cmd = name.split_whitespace().next().unwrap_or("");
            if cmd != "goal" {
                if let Some(sk) = skills::load_named(&self.skills, cmd) {
                    prompt = format!(
                        "The user invoked skill /{}. Follow it exactly.\n\n{}\n\nUser: {user}",
                        sk.name, sk.body
                    );
                }
            }
        }
        if user.trim().starts_with("/goal ") {
            if let Some(condition) = self.goal_condition() {
                prompt = format!(
                    "Goal set. Start working now. Do not ask what to do. Condition:\n{condition}"
                );
            }
        }
        session.append(ChatMessage {
            role: "user".into(),
            content: prompt,
            images: std::mem::take(&mut self.pending_images),
            ..Default::default()
        })?;
        // Deliver cross-session mail so the model sees it this turn.
        let mail = self.bus.drain(session.id());
        if !mail.is_empty() {
            let count = mail.len();
            session.append(ChatMessage {
                role: "user".into(),
                content: mailbox::render_inbox(&mail[..mail.len().min(20)]),
                ..Default::default()
            })?;
            on_event(AgentEvent {
                kind: "system".into(),
                text: format!("{count} message(s) from other sessions"),
            });
        }
        // Keep the context inside the budget before the model sees it. A
        // failed compaction never fails the user's turn.
        if self.auto_compact(session).await.is_some() {
            on_event(AgentEvent {
                kind: "system".into(),
                text: "context over budget — earlier history compacted in place".into(),
            });
        }
        let subagent_id = if cfg!(test) {
            None
        } else {
            crate::activity::ActivityLog::start(
                crate::activity::ActivityKind::Subagent,
                format!("turn · {}", short_turn_label(user)),
                "foreground turn",
            )
            .ok()
        };

        let mut tool_schemas = tools::schemas();
        tool_schemas.extend(self.mcp.schemas().iter().cloned());
        let mut last_text = String::new();
        for round in 0..self.cfg.max_tool_rounds {
            if let Some(bus) = &self.control {
                while bus.is_paused(session.id()) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            on_event(AgentEvent {
                kind: "round".into(),
                text: format!("model round {}", round + 1),
            });
            let completion = if self.streaming_enabled() {
                let mut streamed = false;
                let result = self
                    .provider
                    .complete_streaming(
                        &self.cfg.model,
                        &self.system_prompt(),
                        &session.messages,
                        &tool_schemas,
                        &mut |delta| {
                            streamed = true;
                            on_event(AgentEvent {
                                kind: "delta".into(),
                                text: delta.to_string(),
                            });
                        },
                    )
                    .await;
                match result {
                    Ok(completion) => completion,
                    // Gateways that reject `stream:true` degrade to the plain
                    // call — but only before any delta was surfaced, or the
                    // reply would be generated (and shown) twice.
                    Err(error) if !streamed => {
                        tracing::warn!(
                            error = %error,
                            "provider streaming unavailable; falling back to non-streaming"
                        );
                        self.provider
                            .complete(
                                &self.cfg.model,
                                &self.system_prompt(),
                                &session.messages,
                                &tool_schemas,
                            )
                            .await?
                    }
                    Err(error) => return Err(error),
                }
            } else {
                self.provider
                    .complete(
                        &self.cfg.model,
                        &self.system_prompt(),
                        &session.messages,
                        &tool_schemas,
                    )
                    .await?
            };
            self.record_usage(completion.usage);
            last_text = completion.text.clone();
            if completion.tool_calls.is_empty() {
                session.append(ChatMessage {
                    role: "assistant".into(),
                    content: completion.text.clone(),
                    tool_call_id: None,
                    tool_calls: None,
                    images: Vec::new(),
                })?;
                on_event(AgentEvent {
                    kind: "assistant".into(),
                    text: completion.text.clone(),
                });
                if let Some(id) = &subagent_id {
                    let _ = crate::activity::ActivityLog::finish(id, true, "turn finished");
                }
                return Ok(completion.text);
            }
            session.append(ChatMessage {
                role: "assistant".into(),
                content: completion.text.clone(),
                tool_call_id: None,
                tool_calls: Some(completion.tool_calls.clone()),
                images: Vec::new(),
            })?;
            for call in completion.tool_calls {
                on_event(AgentEvent {
                    kind: "tool".into(),
                    text: format!("{} {}", call.name, call.arguments),
                });
                if !self.allow_tool(&call.name, &call.arguments) {
                    if self.permission.is_prompt() {
                        // Ask the interactive approver instead of silently
                        // denying; still denied when nobody is attached.
                        let detail = approval_detail(&call.name, &call.arguments);
                        match self.request_approval(&call.name, &detail, on_event).await {
                            None => {
                                session.append(ChatMessage {
                                    role: "tool".into(),
                                    content: "permission mode is prompt but no interactive \
                                              approver is attached (use the TUI or set \
                                              permission_mode = acceptEdits)"
                                        .into(),
                                    tool_call_id: Some(call.id),
                                    tool_calls: None,
                                    images: Vec::new(),
                                })?;
                                continue;
                            }
                            Some(Approval::Deny) => {
                                session.append(ChatMessage {
                                    role: "tool".into(),
                                    content: "denied by user".into(),
                                    tool_call_id: Some(call.id),
                                    tool_calls: None,
                                    images: Vec::new(),
                                })?;
                                continue;
                            }
                            Some(Approval::AllowAlways) => {
                                let key = approval_key(&call.name, &call.arguments);
                                self.approved_always.insert(key);
                            }
                            Some(Approval::AllowOnce) => {}
                        }
                    } else {
                        session.append(ChatMessage {
                            role: "tool".into(),
                            content: format!("permission denied for {}", call.name),
                            tool_call_id: Some(call.id),
                            tool_calls: None,
                            images: Vec::new(),
                        })?;
                        continue;
                    }
                }
                let result = match call.name.as_str() {
                    "memory_read" => self.agent_files.memory(),
                    "memory_append" => {
                        let entry = call.arguments.get("entry").and_then(|v| v.as_str());
                        match entry {
                            Some(entry) => self
                                .agent_files
                                .append_memory(entry)
                                .map(|_| "memory appended".to_string())
                                .unwrap_or_else(|e| format!("tool error: {e}")),
                            None => "tool error: missing string arg `entry`".into(),
                        }
                    }
                    _ if self.mcp.is_mcp_tool(&call.name) => self
                        .mcp
                        .dispatch(&call.name, &call.arguments)
                        .unwrap_or_else(|e| format!("tool error: {e}")),
                    _ => tools::dispatch(
                        &call.name,
                        &call.arguments,
                        &self.jail,
                        &tools::ToolCtx {
                            bus: &self.bus,
                            session_id: Some(session.id()),
                            checkpoints: Some(&self.checkpoints),
                            docker: Some(tools::DockerOpts {
                                image: self
                                    .cfg
                                    .docker_image
                                    .clone()
                                    .unwrap_or_else(|| "varynth-sandbox:latest".into()),
                                network: self
                                    .cfg
                                    .docker_network
                                    .clone()
                                    .unwrap_or_else(|| "none".into()),
                            }),
                        },
                    )
                    .unwrap_or_else(|e| format!("tool error: {e}")),
                };
                on_event(AgentEvent {
                    kind: "tool_result".into(),
                    text: result.chars().take(500).collect(),
                });
                session.append(ChatMessage {
                    role: "tool".into(),
                    content: result,
                    tool_call_id: Some(call.id),
                    tool_calls: None,
                    images: Vec::new(),
                })?;
            }
        }
        if let Some(id) = &subagent_id {
            let _ = crate::activity::ActivityLog::finish(id, false, "stopped: max tool rounds");
        }
        Ok(if last_text.is_empty() {
            "(stopped: max tool rounds)".into()
        } else {
            last_text
        })
    }

    fn record_usage(&mut self, usage: Option<Usage>) {
        if let Some(usage) = usage {
            self.last_usage = Some(usage);
            self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
            self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
            self.usage_count = self.usage_count.saturating_add(1);
        }
    }

    /// Token footprint of the session context: the provider-reported input
    /// tokens of the last completion when known (it also covers the system
    /// prompt and tool schemas), else the chars/4 estimate.
    pub fn context_tokens(&self, session: &Session) -> usize {
        let estimate = compact::estimate_messages(&session.messages).div_ceil(4);
        match self.last_usage {
            Some(usage) if usage.input_tokens > 0 => estimate.max(usage.input_tokens as usize),
            _ => estimate,
        }
    }

    fn over_budget(&self, session: &Session) -> bool {
        self.context_tokens(session) > compact::DEFAULT_MESSAGE_BUDGET
    }

    /// Compact the session in place when it is over budget. Returns the
    /// summary message content, or None when the session fits or the
    /// compaction failed (which is swallowed — never fail the user's turn).
    async fn auto_compact(&mut self, session: &mut Session) -> Option<String> {
        if !self.over_budget(session) {
            return None;
        }
        match self.compact_session(session).await {
            Ok(summary) => Some(summary),
            Err(error) => {
                tracing::warn!(error = %error, "auto-compact skipped");
                None
            }
        }
    }

    /// Summarize all but the last few messages into ONE message via the
    /// provider and replace them in the same session. Returns the summary.
    pub async fn compact_session(&mut self, session: &mut Session) -> Result<String> {
        let split = compact::compaction_split(&session.messages);
        anyhow::ensure!(
            split > 0,
            "nothing older than the last few messages to compact"
        );
        let old = session.messages[..split].to_vec();
        let transcript = compact::transcript(&old, compact::SUMMARY_BUDGET);
        let ask = format!(
            "Summarize the earlier part of this coding conversation so it can be replaced \
             by one short note. Keep the user's goals, decisions made, files touched (with \
             paths), current state and next steps. Be concrete and brief. Do not call tools.\n\n\
             Earlier conversation:\n{transcript}"
        );
        let completion = self
            .provider
            .complete(
                &self.cfg.model,
                "You compress coding-agent transcripts into short handoff notes. \
                 Reply with the summary only.",
                &[ChatMessage {
                    role: "user".into(),
                    content: ask,
                    ..Default::default()
                }],
                &[],
            )
            .await?;
        self.record_usage(completion.usage);
        let body = completion.text.trim();
        let summary = if body.is_empty() {
            // Extractive fallback when the provider returns nothing.
            compact::compact_messages(&old, compact::SUMMARY_BUDGET)
                .first()
                .map(|m| m.content.clone())
                .unwrap_or_default()
        } else {
            format!("[Earlier context compacted. The full session remains on disk.]\n\n{body}")
        };
        anyhow::ensure!(!summary.is_empty(), "compaction produced an empty summary");
        let mut messages = Vec::with_capacity(session.messages.len() - split + 1);
        messages.push(ChatMessage {
            role: "system".into(),
            content: summary.clone(),
            ..Default::default()
        });
        messages.extend(session.messages[split..].iter().cloned());
        session.replace_messages(messages)?;
        Ok(summary)
    }

    pub async fn review(&mut self, session: &mut Session, cwd: &Path) -> Result<String> {
        let changes = review_context(cwd).await;
        let prompt = format!(
            "Review the current working tree changes below. Return a short code review with findings ordered by severity, concrete file and line references when available, and remaining test gaps. Do not modify files or suggest changes outside the displayed context.\n\n{changes}"
        );
        session.append(ChatMessage {
            role: "user".into(),
            content: prompt,
            images: Vec::new(),
            ..Default::default()
        })?;
        let system = format!(
            "{}\n\nYou are running a read-only code review. Never call tools and never modify files.",
            self.system_prompt()
        );
        let completion = self
            .provider
            .complete(&self.cfg.model, &system, &session.messages, &[])
            .await?;
        self.record_usage(completion.usage);
        session.append(ChatMessage {
            role: "assistant".into(),
            content: completion.text.clone(),
            tool_call_id: None,
            tool_calls: None,
            images: Vec::new(),
        })?;
        Ok(completion.text)
    }

    /// Whether the goal engine should keep driving turns: an active goal with
    /// round and time budget left. The TUI and dashboard loop on this.
    pub fn wants_goal_continue(&self) -> bool {
        let Some(g) = self.goal.as_ref() else {
            return false;
        };
        if g.status != GoalStatus::Active || g.rounds >= g.max_rounds {
            return false;
        }
        if let Some(mins) = g.budget_minutes {
            if g.started_at.elapsed() >= Duration::from_secs(mins.saturating_mul(60)) {
                return false;
            }
        }
        true
    }

    /// Auto-approval for a tool call: non-Prompt modes admit everything they
    /// always did, and a key the user approved "always" is admitted without
    /// asking again. Prompt mode still asks for everything else.
    fn allow_tool(&self, name: &str, args: &serde_json::Value) -> bool {
        if name.starts_with("computer_") {
            return name == "computer_stop"
                || self.permission == PermissionMode::Bypass
                || (self.permission.is_prompt() && self.approved_always.contains(name));
        }
        if self.mcp.is_mcp_tool(name) {
            return self.permission.auto_approve_write() || self.approved_always.contains(name);
        }
        match name {
            "write_file" | "edit_file" | "memory_append" => {
                self.permission.auto_approve_write() || self.approved_always.contains(name)
            }
            "bash" => {
                self.permission.auto_approve_shell()
                    || self.approved_always.contains(&approval_key(name, args))
            }
            _ => true,
        }
    }

    /// The prompt-mode approval dance for one tool call: surface the request
    /// as an event, hand it to the interactive approver and — when a remote
    /// relay is attached — broadcast it to remote surfaces, then await the
    /// first answer (120 s of silence counts as a denial). `None` means
    /// neither an approver nor a remote surface is attached.
    async fn request_approval(
        &mut self,
        tool: &str,
        detail: &str,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> Option<Approval> {
        let approver = self.approver.clone();
        let remote = self.remote_approval_events.clone();
        if approver.is_none() && remote.is_none() {
            return None;
        }
        on_event(AgentEvent {
            kind: "approval_request".into(),
            text: format!("{tool} {detail}"),
        });
        let answer = approver.map(|approver| {
            let (respond, answer) = tokio::sync::oneshot::channel();
            let _ = approver.send(ApprovalRequest {
                tool: tool.to_string(),
                detail: detail.to_string(),
                respond,
            });
            answer
        });
        // Open a relay slot for the remote surfaces and broadcast the
        // request. The slot is consumed by the first remote answer, so a
        // later `respond` for the same id is rejected.
        let relay = if remote.is_some() || self.control.is_some() {
            let (request, rx) = crate::approval_relay::open(tool, detail);
            if self.control.is_some() {
                crate::approval_relay::publish_remote(&request);
            }
            on_event(AgentEvent {
                kind: "approval_pending".into(),
                text: serde_json::to_string(&request).unwrap_or_default(),
            });
            if let Some(remote) = remote {
                let _ = remote.send(request.clone());
            }
            Some((request.id, rx))
        } else {
            None
        };
        let relay_id = relay.as_ref().map(|(id, _)| id.clone());

        /// Where the winning answer came from.
        enum Answer {
            Tui(Approval),
            Remote(crate::approval_relay::ApprovalVote),
        }

        // First answer wins; the shared 120 s timeout denies silence on
        // both surfaces. A missing surface just never resolves its branch.
        let waited = tokio::time::timeout(APPROVAL_TIMEOUT, async {
            tokio::select! {
                answer = async {
                    match answer {
                        Some(rx) => rx.await,
                        None => std::future::pending().await,
                    }
                } => answer.map_or(Answer::Tui(Approval::Deny), Answer::Tui),
                vote = async {
                    match relay {
                        Some((id, rx)) => {
                            tokio::pin!(rx);
                            loop {
                                tokio::select! {
                                    vote = &mut rx => break vote,
                                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                        if let Some(vote) = crate::approval_relay::poll_remote_response(&id) {
                                            let _ = crate::approval_relay::respond(&id, vote);
                                        }
                                    }
                                }
                            }
                        }
                        None => std::future::pending().await,
                    }
                } => Answer::Remote(vote.unwrap_or(crate::approval_relay::ApprovalVote::Deny)),
            }
        })
        .await;

        let approval = match waited {
            // The TUI decided first. Its win retires the relay slot: the
            // slot's receiver died with the losing branch, so a remote vote
            // could no longer reach anyone — the cleanup only removes the
            // dangling map entry.
            Ok(Answer::Tui(approval)) => {
                if let Some(id) = &relay_id {
                    let _ = crate::approval_relay::respond(
                        id,
                        crate::approval_relay::ApprovalVote::Deny,
                    );
                }
                approval
            }
            // A remote surface won the race; its slot is already consumed.
            Ok(Answer::Remote(vote)) => match vote {
                crate::approval_relay::ApprovalVote::Once => Approval::AllowOnce,
                crate::approval_relay::ApprovalVote::Always => Approval::AllowAlways,
                crate::approval_relay::ApprovalVote::Deny => Approval::Deny,
            },
            // 120 s of silence on every surface: deny, and clean the
            // dangling relay slot. The vote in the cleanup call is
            // unobservable (the slot's receiver died with the timed future)
            // — only the map removal matters.
            Err(_) => {
                if let Some(id) = &relay_id {
                    let _ = crate::approval_relay::respond(
                        id,
                        crate::approval_relay::ApprovalVote::Deny,
                    );
                }
                Approval::Deny
            }
        };
        let note = match approval {
            Approval::AllowOnce => "approved (once)",
            Approval::AllowAlways => "approved (always)",
            Approval::Deny => "denied",
        };
        on_event(AgentEvent {
            kind: "system".into(),
            text: note.into(),
        });
        Some(approval)
    }

    pub async fn list_models(&self) -> Result<Vec<providers::CatalogModel>> {
        self.provider.list_models().await
    }

    pub fn status_json(&self, session: Option<&Session>) -> serde_json::Value {
        let tool_names: Vec<String> = tools::schemas()
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|name| name.as_str())
                    .map(str::to_string)
            })
            .collect();
        json!({
            "cwd": self.jail.cwd,
            "model": self.cfg.model,
            "provider": self.cfg.provider,
            "sandbox": self.cfg.sandbox,
            "permission": self.permission.as_str(),
            "session": session.map(|s| s.id()),
            "skills": self.skills.iter().map(|s| &s.name).collect::<Vec<_>>(),
            "proxy": self.cfg.proxy_url,
            "effort": self.cfg.effort,
            "goal": self.goal.as_ref().map(|g| {
                json!({
                    "condition": g.condition,
                    "rounds": g.rounds,
                    "max_rounds": g.max_rounds,
                    "elapsed_secs": g.started_at.elapsed().as_secs(),
                    "budget_minutes": g.budget_minutes,
                    "status": g.status,
                    "last_verdict": g.last_verdict,
                })
            }),
            "tools": tool_names,
            "turns": self.turns,
            "turn_time_secs": self.turn_time.as_secs(),
            "usage": {
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "total_tokens": self.input_tokens + self.output_tokens,
                "reported": self.usage_count > 0,
                "context_estimate_tokens": session.map(|s| self.context_tokens(s)),
            },
        })
    }
}

fn short_turn_label(user: &str) -> String {
    let one = user.split_whitespace().collect::<Vec<_>>().join(" ");
    let one = one.trim();
    if one.is_empty() {
        return "untitled turn".into();
    }
    one.chars().take(64).collect()
}

/// The `approved_always` key for a tool call: the tool name itself, except
/// for bash where the command's first word stands in (same base-name
/// extraction as `Jail::assert_shell`).
fn approval_key(tool: &str, args: &serde_json::Value) -> String {
    if tool == "bash" {
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let first = command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .trim_matches('\'');
        return std::path::Path::new(first)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(first)
            .to_ascii_lowercase();
    }
    tool.to_string()
}

/// Human-readable detail for the approval dialog: the file path for
/// write/edit, the command for bash, the entry for memory_append, the
/// argument object for everything else (MCP tools).
fn approval_detail(tool: &str, args: &serde_json::Value) -> String {
    let field = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let detail = match tool {
        "write_file" | "edit_file" => field("path"),
        "bash" => field("command"),
        "memory_append" => field("entry"),
        _ => return args.to_string(),
    };
    detail.chars().take(200).collect()
}

/// Split `/goal <rest>` into the optional leading `--rounds N` and
/// `--minutes M` flags and the remaining condition text. Accepts both
/// `--rounds 3` and `--rounds=3`; unknown or malformed flags stay in the
/// condition so the usage error can catch them.
fn parse_goal_flags(rest: &str) -> (Option<u32>, Option<u64>, String) {
    let mut max_rounds = None;
    let mut budget_minutes = None;
    let mut rest = rest.trim();
    loop {
        let (kind, value) = if let Some(v) = rest.strip_prefix("--rounds") {
            ("rounds", v)
        } else if let Some(v) = rest.strip_prefix("--minutes") {
            ("minutes", v)
        } else {
            break;
        };
        let value = value.trim();
        let (number, tail) = if let Some(v) = value.strip_prefix('=') {
            let v = v.trim();
            match v.split_once(char::is_whitespace) {
                Some((n, tail)) => (n, tail.trim()),
                None => (v, ""),
            }
        } else {
            match value.split_once(char::is_whitespace) {
                Some((n, tail)) => (n, tail.trim()),
                None => (value, ""),
            }
        };
        match kind {
            "rounds" => match number.parse::<u32>() {
                Ok(n) => max_rounds = Some(n),
                Err(_) => break,
            },
            _ => match number.parse::<u64>() {
                Ok(n) => budget_minutes = Some(n),
                Err(_) => break,
            },
        }
        rest = tail;
    }
    (max_rounds, budget_minutes, rest.trim().to_string())
}

/// Remove the `GOAL_COMPLETE` claim lines from the reply the user sees.
fn strip_goal_complete(reply: &str) -> String {
    if !reply.lines().any(|l| l.trim() == "GOAL_COMPLETE") {
        return reply.to_string();
    }
    let kept: Vec<&str> = reply
        .lines()
        .filter(|l| l.trim() != "GOAL_COMPLETE")
        .collect();
    let mut s = kept.join("\n");
    while s.ends_with(['\n', '\r', ' ']) {
        s.pop();
    }
    s
}

/// Lenient verdict parse: the auditor's reply may carry prose around the JSON
/// object, so take the first `{` to the last `}` and read met/reason.
fn parse_goal_verdict(text: &str) -> GoalVerdict {
    let not_json = || GoalVerdict {
        met: false,
        reason: "audit reply was not valid JSON".into(),
    };
    let Some(start) = text.find('{') else {
        return not_json();
    };
    let Some(end) = text.rfind('}') else {
        return not_json();
    };
    if end < start {
        return not_json();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text[start..=end]) else {
        return not_json();
    };
    GoalVerdict {
        met: value.get("met").and_then(|v| v.as_bool()).unwrap_or(false),
        reason: value
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("auditor gave no reason")
            .to_string(),
    }
}

const MAX_UNTRACKED_FILE_BYTES: u64 = 64 * 1024;
const MAX_UNTRACKED_CONTEXT_BYTES: usize = 512 * 1024;

async fn review_context(cwd: &Path) -> String {
    let status = match run_git(cwd, &["status", "--short"]).await {
        Ok(output) if output.status.success() => output,
        Ok(_) => return format!("{} is not a git repository", cwd.display()),
        Err(error) => return format!("git is not available: {error}"),
    };

    let mut sections = Vec::new();
    if let Ok(output) = run_git(cwd, &["diff", "HEAD"]).await {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !text.is_empty() {
                sections.push(text);
            }
        }
    }

    if let Ok(output) = run_git(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]).await {
        if output.status.success() {
            let text = untracked_file_context(cwd, &output.stdout).await;
            if !text.is_empty() {
                sections.push(text);
            }
        }
    }

    if sections.is_empty() {
        let status_owned = String::from_utf8_lossy(&status.stdout);
        let status_text = status_owned
            .lines()
            .filter(|line| !line.starts_with("?? "))
            .collect::<Vec<_>>()
            .join("\n");
        let status_text = status_text.trim();
        if status_text.is_empty() {
            "no changes".into()
        } else {
            status_text.to_string()
        }
    } else {
        sections.join("\n\n")
    }
}

async fn untracked_file_context(cwd: &Path, paths: &[u8]) -> String {
    let mut sections = Vec::new();
    let mut total_bytes = 0;

    for raw_path in paths.split(|byte| *byte == 0) {
        if raw_path.is_empty() {
            continue;
        }
        let Ok(path_text) = std::str::from_utf8(raw_path) else {
            continue;
        };
        let relative = Path::new(path_text);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            continue;
        }

        let path = cwd.join(relative);
        let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_UNTRACKED_FILE_BYTES
        {
            continue;
        }

        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        if bytes.len() as u64 > MAX_UNTRACKED_FILE_BYTES || bytes.contains(&0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };

        let section = format!("[untracked file: {path_text}]\n{content}");
        if total_bytes + section.len() > MAX_UNTRACKED_CONTEXT_BYTES {
            break;
        }
        total_bytes += section.len();
        sections.push(section);
    }

    sections.join("\n\n")
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?)
}

#[cfg(test)]
mod tests {
    use super::review_context;
    use std::fs;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn review_context_reports_non_git_directories() {
        let dir = tempfile::tempdir().unwrap();
        let context = review_context(dir.path()).await;
        assert!(context.contains("is not a git repository"));
    }

    #[tokio::test]
    async fn review_context_includes_tracked_and_untracked_file_content() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);

        fs::write(dir.path().join("tracked.txt"), "before\n").unwrap();
        git(dir.path(), &["add", "tracked.txt"]);
        git(dir.path(), &["commit", "-m", "baseline"]);
        fs::write(dir.path().join("tracked.txt"), "after\n").unwrap();
        fs::write(dir.path().join("untracked.txt"), "new content\n").unwrap();

        let context = review_context(dir.path()).await;

        assert!(context.contains("+after"));
        assert!(context.contains("[untracked file: untracked.txt]"));
        assert!(context.contains("new content"));
    }

    #[tokio::test]
    async fn review_context_skips_binary_and_oversized_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]);
        fs::write(dir.path().join("binary.bin"), [0_u8, 1, 2, 3]).unwrap();
        fs::write(
            dir.path().join("large.txt"),
            vec![b'x'; super::MAX_UNTRACKED_FILE_BYTES as usize + 1],
        )
        .unwrap();

        let context = review_context(dir.path()).await;

        assert!(!context.contains("binary.bin"));
        assert!(!context.contains("large.txt"));
    }
}

#[cfg(test)]
mod turn_tests {
    use super::*;
    use crate::providers::{CatalogModel, Completion};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct MockProvider {
        /// One outcome per complete() call; when exhausted, a canned fallback
        /// reply with reported usage is served.
        replies: Mutex<VecDeque<Result<Completion, String>>>,
        /// Message slices every complete() call received.
        seen: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    }

    impl MockProvider {
        fn completion(text: &str) -> Completion {
            Completion {
                text: text.into(),
                tool_calls: Vec::new(),
                model: "mock".into(),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                }),
            }
        }

        /// A completion that makes exactly one tool call.
        fn tool_call_completion(id: &str, name: &str, arguments: serde_json::Value) -> Completion {
            Completion {
                text: "calling a tool".into(),
                tool_calls: vec![crate::session::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                }],
                model: "mock".into(),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                }),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<Completion> {
            self.seen.lock().unwrap().push(messages.to_vec());
            match self.replies.lock().unwrap().pop_front() {
                Some(Ok(completion)) => Ok(completion),
                Some(Err(error)) => anyhow::bail!("{error}"),
                None => Ok(Self::completion("mock reply")),
            }
        }

        async fn list_models(&self) -> Result<Vec<CatalogModel>> {
            Ok(Vec::new())
        }
    }

    /// Mock for the streaming path: `complete_streaming` emits the configured
    /// deltas, then either fails (`stream_error`) or completes with the joined
    /// text. `complete` serves canned replies and every entry point logs
    /// itself into `calls`.
    struct StreamMockProvider {
        deltas: Vec<&'static str>,
        stream_error: Option<String>,
        replies: Mutex<VecDeque<Result<Completion, String>>>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl StreamMockProvider {
        fn plain_completion() -> Completion {
            Completion {
                text: "plain fallback reply".into(),
                tool_calls: Vec::new(),
                model: "mock".into(),
                usage: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for StreamMockProvider {
        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<Completion> {
            self.calls.lock().unwrap().push("complete");
            match self.replies.lock().unwrap().pop_front() {
                Some(Ok(completion)) => Ok(completion),
                Some(Err(error)) => anyhow::bail!("{error}"),
                None => Ok(Self::plain_completion()),
            }
        }

        async fn complete_streaming(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
            on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
        ) -> Result<Completion> {
            self.calls.lock().unwrap().push("stream");
            for delta in &self.deltas {
                on_delta(delta);
            }
            if let Some(error) = &self.stream_error {
                anyhow::bail!("{error}");
            }
            Ok(Completion {
                text: self.deltas.concat(),
                tool_calls: Vec::new(),
                model: "mock".into(),
                usage: None,
            })
        }

        async fn list_models(&self) -> Result<Vec<CatalogModel>> {
            Ok(Vec::new())
        }
    }

    fn test_runtime(
        cwd: &Path,
        replies: Vec<Result<Completion, String>>,
    ) -> (Runtime, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runtime = test_runtime_with(
            cwd,
            Box::new(MockProvider {
                replies: Mutex::new(replies.into()),
                seen: Arc::clone(&seen),
            }),
        );
        (runtime, seen)
    }

    fn test_runtime_with(cwd: &Path, provider: Box<dyn Provider>) -> Runtime {
        let cfg = Config::default();
        Runtime {
            cfg: cfg.clone(),
            jail: Jail::new(
                cwd.to_path_buf(),
                cfg.add_dirs.clone(),
                SandboxMode::parse(&cfg.sandbox),
                cfg.shell_allowlist.clone(),
            ),
            permission: PermissionMode::parse(&cfg.permission_mode),
            skills: Vec::new(),
            goal: None,
            // Test bus lives inside the tempdir, never the real ~/.varynth.
            bus: Bus::at(cwd.join("bus.jsonl")),
            approver: None,
            remote_approval_events: None,
            approved_always: std::collections::HashSet::new(),
            checkpoints: CheckpointStore::at(cwd.join("checkpoints")),
            agent_files: AgentFiles::ensure(cwd).unwrap(),
            mcp: McpManager::from_config(None).unwrap(),
            pending_images: Vec::new(),
            last_usage: None,
            input_tokens: 0,
            output_tokens: 0,
            usage_count: 0,
            turns: 0,
            turn_time: Duration::ZERO,
            started_at: Instant::now(),
            auto_titling: false,
            control: None,
            provider,
        }
    }

    fn test_session(cwd: &Path) -> Session {
        crate::session::Session::create_at(
            uuid::Uuid::new_v4().to_string(),
            cwd.join("turn-test.jsonl"),
            &cwd.display().to_string(),
            "mock",
        )
        .unwrap()
    }

    fn history(count: usize) -> Vec<ChatMessage> {
        (0..count)
            .map(|i| ChatMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                content: format!("message number {i}"),
                ..Default::default()
            })
            .collect()
    }

    fn contexts(seen: &Arc<Mutex<Vec<Vec<ChatMessage>>>>) -> Vec<Vec<ChatMessage>> {
        seen.lock().unwrap().clone()
    }

    /// Collects `"{kind}: {text}"` lines from the events a turn emits.
    fn event_sink(events: &Arc<Mutex<Vec<String>>>) -> impl FnMut(AgentEvent) + Send {
        let sink = Arc::clone(events);
        move |ev: AgentEvent| {
            sink.lock()
                .unwrap()
                .push(format!("{}: {}", ev.kind, ev.text));
        }
    }

    #[tokio::test]
    async fn over_budget_turn_compacts_in_place_before_the_model_call() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::completion("compact summary of old turns"))],
        );
        rt.last_usage = Some(Usage {
            input_tokens: compact::DEFAULT_MESSAGE_BUDGET as u64 + 1,
            output_tokens: 0,
        });
        let mut session = test_session(dir.path());
        for message in history(6) {
            session.append(message).unwrap();
        }

        let reply = rt
            .turn(&mut session, "the new request", |_| {})
            .await
            .unwrap();

        assert!(reply.contains("mock reply"));
        // 6 old + 1 new user message; everything but the last 4 becomes ONE
        // system summary, then the assistant reply is appended.
        assert_eq!(session.messages.len(), 6);
        assert_eq!(session.messages[0].role, "system");
        assert!(session.messages[0]
            .content
            .contains("compact summary of old turns"));
        assert!(session
            .messages
            .iter()
            .any(|m| m.content == "the new request"));
        assert!(!session
            .messages
            .iter()
            .any(|m| m.content == "message number 0"));
        // The model saw the compacted history, not the original one.
        let calls = contexts(&seen);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].len(), 1); // the summarize request
        assert_eq!(calls[1].len(), 5);
        assert!(calls[1][0].content.contains("compact summary of old turns"));
        // Usage from both completions accumulated, one real turn clocked.
        assert_eq!(rt.input_tokens, 20);
        assert_eq!(rt.output_tokens, 10);
        assert_eq!(rt.usage_count, 2);
        assert_eq!(rt.turns, 1);
    }

    #[tokio::test]
    async fn under_budget_turn_leaves_history_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(dir.path(), vec![]);
        let mut session = test_session(dir.path());
        for message in history(3) {
            session.append(message).unwrap();
        }

        let reply = rt.turn(&mut session, "hello there", |_| {}).await.unwrap();

        assert!(reply.contains("mock reply"));
        let roles: Vec<&str> = session.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "user", "assistant"]
        );
        assert!(!session
            .messages
            .iter()
            .any(|m| m.content.contains("compact summary")));
        assert_eq!(contexts(&seen).len(), 1);
        assert_eq!(rt.usage_count, 1);
        assert_eq!(rt.turns, 1);
        let status = rt.status_json(Some(&session));
        assert_eq!(status["usage"]["input_tokens"], 10);
        assert_eq!(status["usage"]["reported"], true);
        assert!(status["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "read_file"));
    }

    #[tokio::test]
    async fn failed_summary_never_fails_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(dir.path(), vec![Err("summarizer down".into())]);
        rt.last_usage = Some(Usage {
            input_tokens: compact::DEFAULT_MESSAGE_BUDGET as u64 + 1,
            output_tokens: 0,
        });
        let mut session = test_session(dir.path());
        for message in history(6) {
            session.append(message).unwrap();
        }

        let reply = rt.turn(&mut session, "still need this", |_| {}).await;

        let reply = reply.expect("turn must survive a compaction failure");
        assert!(reply.contains("mock reply"));
        // Nothing was compacted: original history plus the new pair.
        assert_eq!(session.messages.len(), 8);
        assert_eq!(session.messages[0].content, "message number 0");
        assert!(!session
            .messages
            .iter()
            .any(|m| m.content.contains("compact summary")));
        // The model still received the full (uncompacted) history.
        let calls = contexts(&seen);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].len(), 7);
        assert_eq!(calls[1][0].content, "message number 0");
    }

    #[test]
    fn context_tokens_prefers_reported_input_over_chars_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![]);
        let mut session = test_session(dir.path());
        session
            .append(ChatMessage {
                role: "user".into(),
                content: "x".repeat(400),
                ..Default::default()
            })
            .unwrap();
        // chars/4 estimate alone.
        let expected = compact::estimate_messages(&session.messages).div_ceil(4);
        assert_eq!(rt.context_tokens(&session), expected);
        rt.last_usage = Some(Usage {
            input_tokens: 50_000,
            output_tokens: 0,
        });
        assert_eq!(rt.context_tokens(&session), 50_000);
    }

    #[tokio::test]
    async fn goal_met_when_the_auditor_agrees_with_the_claim() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion("done\nGOAL_COMPLETE")),
                Ok(MockProvider::completion(
                    "{\"met\": true, \"reason\": \"all checks pass\"}",
                )),
            ],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let reply = rt
            .turn(
                &mut session,
                "/goal --rounds 3 fix the tests",
                event_sink(&events),
            )
            .await
            .unwrap();

        let goal = rt.goal.as_ref().expect("goal kept after settle");
        assert_eq!(goal.condition, "fix the tests");
        assert_eq!(goal.max_rounds, 3);
        assert_eq!(goal.rounds, 1);
        assert_eq!(goal.status, GoalStatus::Met);
        assert_eq!(
            goal.last_verdict.as_ref().map(|v| v.reason.as_str()),
            Some("all checks pass")
        );
        assert!(!rt.wants_goal_continue());
        // The claim line is stripped from the reply the user sees.
        assert!(reply.contains("done"));
        assert!(!reply.contains("GOAL_COMPLETE"));
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.contains("goal met") && e.contains("all checks pass")),
            "events: {events:?}"
        );
        // The audit call carried the objective and the recent transcript.
        let calls = contexts(&seen);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].len(), 1);
        assert!(calls[1][0].content.contains("fix the tests"));
        assert!(calls[1][0].content.contains("user:"));
    }

    #[tokio::test]
    async fn goal_audit_overrides_the_models_claim() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion("trust me\nGOAL_COMPLETE")),
                Ok(MockProvider::completion(
                    "{\"met\": false, \"reason\": \"tests still fail\"}",
                )),
            ],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let reply = rt
            .turn(
                &mut session,
                "/goal --rounds 3 ship it",
                event_sink(&events),
            )
            .await
            .unwrap();

        let goal = rt.goal.as_ref().unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.rounds, 1);
        assert_eq!(
            goal.last_verdict.as_ref().map(|v| v.reason.as_str()),
            Some("tests still fail")
        );
        assert!(rt.wants_goal_continue());
        assert!(!reply.contains("GOAL_COMPLETE"));
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.contains("goal audit") && e.contains("tests still fail")),
            "events: {events:?}"
        );
    }

    #[tokio::test]
    async fn goal_met_without_a_claim_when_the_auditor_sees_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion("finished the work")),
                Ok(MockProvider::completion(
                    "{\"met\": true, \"reason\": \"verified\"}",
                )),
            ],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "/goal wrap up", event_sink(&events))
            .await
            .unwrap();

        let goal = rt.goal.as_ref().unwrap();
        assert_eq!(goal.status, GoalStatus::Met);
        assert_eq!(goal.rounds, 1);
        assert!(!rt.wants_goal_continue());
        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|e| e.contains("goal met")),
            "events: {events:?}"
        );
    }

    #[tokio::test]
    async fn goal_stops_when_the_round_budget_is_spent() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion("working on it")),
                Ok(MockProvider::completion(
                    "{\"met\": false, \"reason\": \"not yet\"}",
                )),
                Ok(MockProvider::completion("still working")),
                Ok(MockProvider::completion(
                    "{\"met\": false, \"reason\": \"still no\"}",
                )),
            ],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(
            &mut session,
            "/goal --rounds 1 chipping away",
            event_sink(&events),
        )
        .await
        .unwrap();

        // Round 1 of 1 used: the loop no longer auto-continues.
        let goal = rt.goal.as_ref().unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.rounds, 1);
        assert!(!rt.wants_goal_continue());
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.contains("[goal 1/1] not yet")),
            "events: {:?}",
            events.lock().unwrap()
        );

        // A further turn trips the round budget and stops the goal.
        rt.turn(&mut session, "keep going", event_sink(&events))
            .await
            .unwrap();

        let goal = rt.goal.as_ref().unwrap();
        assert_eq!(goal.status, GoalStatus::Stopped);
        assert!(!rt.wants_goal_continue());
        // The budget stop discards this turn's audit verdict instead of
        // recording it.
        assert_eq!(
            goal.last_verdict.as_ref().map(|v| v.reason.as_str()),
            Some("not yet")
        );
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.contains("goal stopped") && e.contains("round budget of 1 exhausted")),
            "events: {events:?}"
        );
    }

    #[tokio::test]
    async fn bus_mail_is_injected_into_the_model_context() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(dir.path(), vec![]);
        let mut session = test_session(dir.path());
        rt.bus
            .send("other-session", session.id(), "hello over the bus")
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "any updates?", event_sink(&events))
            .await
            .unwrap();

        let injected = session
            .messages
            .iter()
            .find(|m| m.role == "user" && m.content.contains("messages from other sessions"))
            .expect("mail message appended");
        assert!(injected.content.contains("hello over the bus"));
        assert!(injected.content.contains("other-session"));
        // The model's context included the mail block.
        let calls = contexts(&seen);
        assert_eq!(calls.len(), 1);
        assert!(calls[0]
            .iter()
            .any(|m| m.content.contains("hello over the bus")));
        // The user was told how many messages arrived.
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.contains("1 message(s) from other sessions")),
            "events: {:?}",
            events.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn send_and_inbox_drive_the_bus_through_local_slash() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(dir.path(), vec![]);
        let mut session = test_session(dir.path());
        // Mail waiting for this session so /inbox has something to drain.
        rt.bus
            .send("other-session", session.id(), "reply pending")
            .unwrap();

        let sent = rt
            .turn(&mut session, "/send latest working on it now", |_| {})
            .await
            .unwrap();
        assert!(sent.starts_with("sent to latest (message "));
        let forwarded = rt.bus.unread("other-session");
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].from, session.id());
        assert_eq!(forwarded[0].text, "working on it now");

        let events = Arc::new(Mutex::new(Vec::new()));
        let inbox = rt
            .turn(&mut session, "/inbox", event_sink(&events))
            .await
            .unwrap();
        assert!(inbox.contains("messages from other sessions"));
        assert!(inbox.contains("reply pending"));
        // Drained: the next /inbox is empty.
        let inbox = rt
            .turn(&mut session, "/inbox", event_sink(&events))
            .await
            .unwrap();
        assert_eq!(inbox, "(no messages)");
        // Local commands never reach the model or the turn clock.
        assert!(contexts(&seen).is_empty());
        assert_eq!(rt.turns, 0);
    }

    #[test]
    fn goal_slash_parses_flags_and_reports_status() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![]);
        assert!(rt
            .apply_goal_slash("/goal --rounds 4 --minutes 9 fix flaky tests")
            .is_none());
        let goal = rt.goal.as_ref().unwrap();
        assert_eq!(goal.condition, "fix flaky tests");
        assert_eq!(goal.max_rounds, 4);
        assert_eq!(goal.budget_minutes, Some(9));
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.rounds, 0);

        // The `=` form works; unset flags fall back to config defaults.
        assert!(rt.apply_goal_slash("/goal --minutes=2 tidy up").is_none());
        let goal = rt.goal.as_ref().unwrap();
        assert_eq!(goal.condition, "tidy up");
        assert_eq!(goal.max_rounds, 25);
        assert_eq!(goal.budget_minutes, Some(2));

        // The status report shows condition, progress, budget and status.
        let report = rt.apply_goal_slash("/goal status").unwrap();
        assert!(report.contains("tidy up"));
        assert!(report.contains("0/25"));
        assert!(report.contains("budget 2min"));
        assert!(report.contains("Status: active"));
        let status = rt.status_json(None);
        assert_eq!(status["goal"]["condition"], "tidy up");
        assert_eq!(status["goal"]["status"], "active");
        assert_eq!(status["goal"]["rounds"], 0);
        assert_eq!(status["goal"]["budget_minutes"], 2);

        // Stopping keeps the terminal state visible but idle.
        assert!(rt
            .apply_goal_slash("/goal stop")
            .unwrap()
            .contains("stopped"));
        assert_eq!(rt.goal.as_ref().unwrap().status, GoalStatus::Stopped);
        assert!(!rt.wants_goal_continue());
        let report = rt.apply_goal_slash("/goal").unwrap();
        assert!(report.contains("Status: stopped"));
        assert!(report.contains("stopped by user"));
        assert_eq!(rt.status_json(None)["goal"]["status"], "stopped");

        // A condition-less /goal is a usage error, not an empty goal.
        assert!(rt
            .apply_goal_slash("/goal --rounds 3")
            .unwrap()
            .contains("Usage:"));
        assert!(rt
            .apply_goal_slash("/goal clear")
            .unwrap()
            .contains("stopped"));
        assert!(rt.goal.is_some());
    }

    #[test]
    fn goal_verdict_parse_is_lenient() {
        let verdict = parse_goal_verdict("Sure! {\"met\": true, \"reason\": \"done\"} — thanks");
        assert!(verdict.met);
        assert_eq!(verdict.reason, "done");
        let verdict = parse_goal_verdict("no json in this reply");
        assert!(!verdict.met);
        assert_eq!(verdict.reason, "audit reply was not valid JSON");
        let verdict = parse_goal_verdict("{\"met\": false, \"reason\": \"half done\"}");
        assert!(!verdict.met);
        assert_eq!(verdict.reason, "half done");
        // Missing fields fall back to "not met".
        let verdict = parse_goal_verdict("{\"met\": true}");
        assert!(verdict.met);
        assert_eq!(verdict.reason, "auditor gave no reason");
    }

    #[test]
    fn streaming_enabled_defaults_on_and_respects_stream_false() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![]);
        assert!(rt.streaming_enabled());
        rt.cfg.stream = Some(true);
        assert!(rt.streaming_enabled());
        rt.cfg.stream = Some(false);
        assert!(!rt.streaming_enabled());
    }

    #[tokio::test]
    async fn streaming_turn_emits_deltas_then_the_assistant_reply() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut rt = test_runtime_with(
            dir.path(),
            Box::new(StreamMockProvider {
                deltas: vec!["Hel", "lo ", "world"],
                stream_error: None,
                replies: Mutex::new(VecDeque::new()),
                calls: Arc::clone(&calls),
            }),
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let reply = rt
            .turn(&mut session, "say hi", event_sink(&events))
            .await
            .unwrap();

        assert_eq!(reply, "Hello world");
        let events = events.lock().unwrap();
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| e.strip_prefix("delta: "))
            .collect();
        assert_eq!(deltas, vec!["Hel", "lo ", "world"]);
        assert!(
            events.last().is_some_and(|e| e.starts_with("assistant: ")),
            "events: {events:?}"
        );
        // The reply text itself must also be the last assistant event.
        assert_eq!(
            events.last().unwrap().strip_prefix("assistant: "),
            Some("Hello world")
        );
        assert_eq!(calls.lock().unwrap().as_slice(), &["stream"]);
    }

    #[tokio::test]
    async fn streaming_error_without_deltas_falls_back_to_plain_complete() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut rt = test_runtime_with(
            dir.path(),
            Box::new(StreamMockProvider {
                deltas: vec![],
                stream_error: Some("gateway rejects stream:true".into()),
                replies: Mutex::new(VecDeque::new()),
                calls: Arc::clone(&calls),
            }),
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let reply = rt
            .turn(&mut session, "hello", event_sink(&events))
            .await
            .unwrap();

        assert!(reply.contains("plain fallback reply"));
        // Exactly one graceful-degrade retry, and the user saw no deltas.
        assert_eq!(calls.lock().unwrap().as_slice(), &["stream", "complete"]);
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .all(|e| !e.starts_with("delta: ")));
    }

    #[tokio::test]
    async fn mid_stream_error_after_deltas_surfaces_without_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut rt = test_runtime_with(
            dir.path(),
            Box::new(StreamMockProvider {
                deltas: vec!["partial"],
                stream_error: Some("connection reset mid-stream".into()),
                replies: Mutex::new(VecDeque::new()),
                calls: Arc::clone(&calls),
            }),
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let result = rt.turn(&mut session, "hello", event_sink(&events)).await;

        // Deltas fired, so a silent retry would double-generate: bail.
        assert!(result.is_err());
        assert_eq!(calls.lock().unwrap().as_slice(), &["stream"]);
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("delta: partial")));
    }

    #[tokio::test]
    async fn stream_disabled_config_calls_complete_directly() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut rt = test_runtime_with(
            dir.path(),
            Box::new(StreamMockProvider {
                deltas: vec!["never"],
                stream_error: None,
                replies: Mutex::new(VecDeque::new()),
                calls: Arc::clone(&calls),
            }),
        );
        rt.cfg.stream = Some(false);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let reply = rt
            .turn(&mut session, "hello", event_sink(&events))
            .await
            .unwrap();

        assert!(reply.contains("plain fallback reply"));
        assert_eq!(calls.lock().unwrap().as_slice(), &["complete"]);
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .all(|e| !e.starts_with("delta: ")));
    }

    #[tokio::test]
    async fn prompt_mode_without_approver_denies_writes_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "write_file",
                serde_json::json!({"path": "out.txt", "content": "nope"}),
            ))],
        );
        rt.permission = PermissionMode::Prompt;
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "make a file", event_sink(&events))
            .await
            .unwrap();

        // Silent denial is gone: no file, and the tool message explains why.
        assert!(!dir.path().join("out.txt").exists());
        let denied = session
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("denial recorded in session history");
        assert!(
            denied
                .content
                .contains("no interactive approver is attached"),
            "got: {}",
            denied.content
        );
        // No approval was ever surfaced to a (missing) approver.
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .all(|e| !e.starts_with("approval_request: ")));
    }

    #[tokio::test]
    async fn prompt_mode_allow_always_answers_once_and_skips_the_second_ask() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::tool_call_completion(
                    "call_1",
                    "write_file",
                    serde_json::json!({"path": "out.txt", "content": "one"}),
                )),
                Ok(MockProvider::tool_call_completion(
                    "call_2",
                    "write_file",
                    serde_json::json!({"path": "out.txt", "content": "two"}),
                )),
            ],
        );
        rt.permission = PermissionMode::Prompt;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.approver = Some(tx);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(async move {
            let result = rt
                .turn(&mut session, "write twice", event_sink(&events))
                .await;
            (result, rt, session, events)
        });
        // The first write asks; the request carries tool and target path.
        let req = rx
            .recv()
            .await
            .expect("approval request for the first write");
        assert_eq!(req.tool, "write_file");
        assert_eq!(req.detail, "out.txt");
        req.respond.send(Approval::AllowAlways).unwrap();

        let (result, rt, session, events) = handle.await.unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "two"
        );
        // The second write was covered by allow-always: nothing asked again.
        assert!(rt.approved_always.contains("write_file"));
        assert!(rx.try_recv().is_err());
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.starts_with("approval_request: "))
                .count(),
            1
        );
        assert!(events.iter().any(|e| e.contains("approved (always)")));
        // Both writes landed as tool results, neither as a denial.
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|m| m.role == "tool" && m.content.contains("wrote "))
                .count(),
            2
        );
        assert!(session
            .messages
            .iter()
            .all(|m| m.content != "denied by user"));
    }

    #[tokio::test]
    async fn prompt_mode_deny_by_user_leaves_the_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "write_file",
                serde_json::json!({"path": "out.txt", "content": "nope"}),
            ))],
        );
        rt.permission = PermissionMode::Prompt;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.approver = Some(tx);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(async move {
            let result = rt
                .turn(&mut session, "make a file", event_sink(&events))
                .await;
            (result, rt, session, events)
        });
        let req = rx.recv().await.expect("approval request");
        assert_eq!(req.tool, "write_file");
        req.respond.send(Approval::Deny).unwrap();

        let (result, _rt, session, events) = handle.await.unwrap();
        result.unwrap();
        assert!(!dir.path().join("out.txt").exists());
        let denied = session
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("denial recorded in session history");
        assert_eq!(denied.content, "denied by user");
        assert_eq!(denied.tool_call_id.as_deref(), Some("call_1"));
        assert!(events.lock().unwrap().iter().any(|e| e.contains("denied")));
    }

    #[tokio::test]
    async fn remote_relay_alone_decides_when_no_tui_approver_is_attached() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "write_file",
                serde_json::json!({"path": "out.txt", "content": "remote"}),
            ))],
        );
        rt.permission = PermissionMode::Prompt;
        // No TUI approver — the remote relay is the only surface, and it can
        // approve (previously this shape denied outright).
        let (broadcast, _) =
            tokio::sync::broadcast::channel::<crate::approval_relay::RelayRequest>(8);
        rt.remote_approval_events = Some(broadcast.clone());
        let mut relay_rx = broadcast.subscribe();
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(async move {
            let result = rt
                .turn(&mut session, "make a file", event_sink(&events))
                .await;
            (result, rt, session, events)
        });
        let req = relay_rx.recv().await.expect("relay request broadcast");
        assert_eq!(req.tool, "write_file");
        assert_eq!(req.detail, "out.txt");
        assert!(crate::approval_relay::respond(
            &req.id,
            crate::approval_relay::ApprovalVote::Once
        ));

        let (result, _rt, _session, events) = handle.await.unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "remote"
        );
        // Same event trail as a TUI answer.
        let events = events.lock().unwrap();
        assert!(events
            .iter()
            .any(|e| e.starts_with("approval_request: write_file out.txt")));
        assert!(events.iter().any(|e| e.contains("approved (once)")));
        drop(events);
        // The slot was consumed by the answer.
        assert!(!crate::approval_relay::respond(
            &req.id,
            crate::approval_relay::ApprovalVote::Deny
        ));
    }

    #[tokio::test]
    async fn remote_relay_deny_without_a_tui_approver_denies_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "write_file",
                serde_json::json!({"path": "out.txt", "content": "nope"}),
            ))],
        );
        rt.permission = PermissionMode::Prompt;
        let (broadcast, _) =
            tokio::sync::broadcast::channel::<crate::approval_relay::RelayRequest>(8);
        rt.remote_approval_events = Some(broadcast.clone());
        let mut relay_rx = broadcast.subscribe();
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(async move {
            let result = rt
                .turn(&mut session, "make a file", event_sink(&events))
                .await;
            (result, rt, session, events)
        });
        let req = relay_rx.recv().await.expect("relay request broadcast");
        assert!(crate::approval_relay::respond(
            &req.id,
            crate::approval_relay::ApprovalVote::Deny
        ));

        let (result, _rt, session, events) = handle.await.unwrap();
        result.unwrap();
        assert!(!dir.path().join("out.txt").exists());
        let denied = session
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("denial recorded in session history");
        assert_eq!(denied.content, "denied by user");
        assert!(events.lock().unwrap().iter().any(|e| e.contains("denied")));
    }

    #[tokio::test]
    async fn remote_relay_answer_wins_over_a_silent_tui_approver() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "write_file",
                serde_json::json!({"path": "out.txt", "content": "remote"}),
            ))],
        );
        rt.permission = PermissionMode::Prompt;
        let (tui_tx, mut tui_rx) = tokio::sync::mpsc::unbounded_channel();
        rt.approver = Some(tui_tx);
        let (broadcast, _) =
            tokio::sync::broadcast::channel::<crate::approval_relay::RelayRequest>(8);
        rt.remote_approval_events = Some(broadcast.clone());
        let mut relay_rx = broadcast.subscribe();
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(async move {
            let result = rt
                .turn(&mut session, "make a file", event_sink(&events))
                .await;
            (result, rt, session, events)
        });
        // Both surfaces were asked; only the remote one answers.
        let tui_req = tui_rx.recv().await.expect("TUI approval request");
        let req = relay_rx.recv().await.expect("relay request broadcast");
        assert_eq!(tui_req.tool, "write_file");
        assert_eq!(req.tool, "write_file");
        assert!(crate::approval_relay::respond(
            &req.id,
            crate::approval_relay::ApprovalVote::Once
        ));

        let (result, _rt, _session, events) = handle.await.unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "remote"
        );
        // Exactly one approval question surfaced, answered remotely; the
        // silent TUI oneshot never blocked the decision.
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.starts_with("approval_request: "))
                .count(),
            1
        );
        assert!(events.iter().any(|e| e.contains("approved (once)")));
        drop(events);
        assert!(!crate::approval_relay::respond(
            &req.id,
            crate::approval_relay::ApprovalVote::Deny
        ));
        assert!(tui_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn first_turn_titles_the_session_via_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion("all done")),
                Ok(MockProvider::completion("OAuth Login Fix")),
            ],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.auto_titling = true;
        let reply = rt
            .turn(
                &mut session,
                "fix the oauth login redirect loop",
                event_sink(&events),
            )
            .await
            .unwrap();

        assert_eq!(reply, "all done");
        assert_eq!(session.meta.title, "OAuth Login Fix");
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e == "system: session titled: OAuth Login Fix"),
            "events: {:?}",
            events.lock().unwrap()
        );
        // Exactly two provider calls: the turn itself and the titler.
        assert_eq!(contexts(&seen).len(), 2);
        assert_eq!(rt.turns, 1);
    }

    #[tokio::test]
    async fn titling_fires_only_on_the_first_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion("first reply")),
                Ok(MockProvider::completion("First Task")),
                Ok(MockProvider::completion("second reply")),
            ],
        );
        let mut session = test_session(dir.path());
        rt.auto_titling = true;
        rt.turn(&mut session, "start something", |_| {})
            .await
            .unwrap();
        rt.turn(&mut session, "continue please", |_| {})
            .await
            .unwrap();

        // turn 1 + title + turn 2: the second turn never re-titles.
        assert_eq!(contexts(&seen).len(), 3);
        assert_eq!(session.meta.title, "First Task");
        assert_eq!(rt.turns, 2);
    }

    #[tokio::test]
    async fn titling_skips_a_session_that_already_has_a_title() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) =
            test_runtime(dir.path(), vec![Ok(MockProvider::completion("only reply"))]);
        let mut session = test_session(dir.path());
        session.set_title("kept title").unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "hello again", event_sink(&events))
            .await
            .unwrap();

        assert_eq!(session.meta.title, "kept title");
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("session titled")));
        // The turn ran, but no titling call was made.
        assert_eq!(contexts(&seen).len(), 1);
    }

    #[tokio::test]
    async fn titling_failure_never_fails_the_turn_or_renames_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion("solid reply")),
                Err("titler down".into()),
            ],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.auto_titling = true;
        let reply = rt
            .turn(&mut session, "do the thing", event_sink(&events))
            .await
            .unwrap();

        assert_eq!(reply, "solid reply");
        assert_eq!(session.meta.title, "new session");
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("session titled")));
        assert_eq!(contexts(&seen).len(), 2);
        assert_eq!(rt.turns, 1);
    }

    #[tokio::test]
    async fn undo_slash_restores_the_last_checkpointed_write() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "write_file",
                serde_json::json!({"path": "note.txt", "content": "after"}),
            ))],
        );
        std::fs::write(dir.path().join("note.txt"), "before").unwrap();
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "overwrite note.txt", event_sink(&events))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "after"
        );

        let out = rt
            .turn(&mut session, "/undo", event_sink(&events))
            .await
            .unwrap();
        assert!(out.starts_with("restored "), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "before"
        );

        // Drained: a second /undo says so and never reaches the model.
        let model_calls = contexts(&seen).len();
        let out = rt
            .turn(&mut session, "/undo", event_sink(&events))
            .await
            .unwrap();
        assert_eq!(out, "(nothing to undo)");
        assert_eq!(contexts(&seen).len(), model_calls);
    }

    #[test]
    fn set_effort_applies_known_levels_and_rejects_unknown_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![]);
        assert!(rt.set_effort("ultra"));
        assert_eq!(rt.cfg.effort, "ultra");
        assert!(rt.set_effort("low"));
        assert_eq!(rt.cfg.effort, "low");
        assert!(!rt.set_effort("mega"));
        assert_eq!(rt.cfg.effort, "low", "rejected level leaves config alone");
    }

    #[test]
    fn set_permission_mode_updates_parsed_flag_and_config_together() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![]);
        rt.set_permission_mode("bypass");
        assert_eq!(rt.permission, PermissionMode::Bypass);
        assert_eq!(rt.cfg.permission_mode, "bypass");
        rt.set_permission_mode("prompt");
        assert!(rt.permission.is_prompt());
        assert_eq!(rt.cfg.permission_mode, "prompt");
    }
}
