use anyhow::Result;
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::agent_files::AgentFiles;
use crate::checkpoint::{CheckpointStore, Redo};
use crate::commands;
use crate::compact;
use crate::config::Config;
use crate::hooks::{HookEvent, HookSet};
use crate::mailbox::{self, Bus};
use crate::mcp::McpManager;
use crate::permissions::{self, GateDecision, PermissionMode, RuleEffect, RuleSet};
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Deny,
    AllowOnce,
    /// Approve for the rest of this runtime's life.
    AllowAlways,
    /// Approve for this runtime and persist an `allow` user rule for the call.
    AllowAlwaysSaveRule,
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

/// Tools an MCP session has seen: `used` were called, `searched` came back
/// from `tool_search`. Both are promoted into the next model call.
#[derive(Debug, Default)]
struct McpExposure {
    used: std::collections::HashSet<String>,
    searched: std::collections::HashSet<String>,
}

/// The plan artifact produced by one `/plan` turn: the task, the model's
/// final plan text, and whether the user approved it. Kept in the runtime (not
/// a database) so `/plan approve` continues the same session.
#[derive(Debug, Clone, Default)]
pub struct PlanArtifact {
    pub task: String,
    pub text: String,
    pub approved: bool,
}

/// Outcome of `/plan ...` handling inside a turn.
enum PlanSlash {
    /// Reply locally without a provider call.
    Message(String),
    /// Run the turn with this text instead of the raw slash command.
    Turn(String),
}

pub struct Runtime {
    pub cfg: Config,
    pub jail: Jail,
    pub permission: PermissionMode,
    pub skills: Vec<skills::Skill>,
    /// User-defined slash commands (user + trust-gated project files).
    pub commands: Vec<commands::Command>,
    /// Lifecycle hooks (user + trust-gated project files).
    pub hooks: HookSet,
    /// Session id whose `session_start` hook already fired. Switching to a
    /// different session fires `session_stop` for the previous one.
    active_session: Option<String>,
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
    /// Persistent rules: user rules plus trust-gated project rules.
    rules: RuleSet,
    /// Latest `/plan` artifact, cleared by a new plan.
    pub plan: Option<PlanArtifact>,
    /// Set while the next completed assistant reply should become the plan
    /// artifact for the pending `/plan` turn.
    plan_capture: bool,
    /// Pre-write file snapshots backing the `/undo` slash command.
    pub checkpoints: CheckpointStore,
    pub agent_files: AgentFiles,
    /// Desktop notifications fed from this runtime's event fan-out.
    pub notifier: crate::notify::Notifier,
    pub mcp: McpManager,
    /// Per-session MCP tools already used or surfaced by `tool_search`.
    /// Keys are session ids, so forking/resuming into a new session starts
    /// with only the eager set again.
    mcp_exposure: std::collections::HashMap<String, McpExposure>,
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
    pub side_notes: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    provider: Box<dyn Provider>,
}

struct RuntimeConfiguration {
    cfg: Config,
    provider: Box<dyn Provider>,
    mcp: McpManager,
    jail: Jail,
    permission: PermissionMode,
    skills: Vec<skills::Skill>,
    commands: Vec<commands::Command>,
    hooks: HookSet,
}

impl RuntimeConfiguration {
    fn prepare(cfg: Config, cwd: &Path) -> Result<Self> {
        cfg.validate()?;
        cfg.require_provider_credentials()?;
        ensure_workspace_mcp_allowed(cwd, cfg.mcp_config.as_deref())?;
        let provider = providers::build(&cfg)?;
        let mcp = McpManager::from_config(cfg.mcp_config.as_deref())?;
        let jail = Jail::new(
            cwd.to_path_buf(),
            cfg.add_dirs.clone(),
            SandboxMode::parse(&cfg.sandbox),
            cfg.shell_allowlist.clone(),
        );
        let permission = PermissionMode::parse(&cfg.permission_mode);
        let skills = skills::discover(&jail.cwd);
        let commands = commands::CommandSet::discover(&jail.cwd).commands;
        let hooks = HookSet::load_timed(&jail.cwd, Duration::from_secs(cfg.hooks_timeout_secs))
            .with_timeout_policy(&cfg.hooks_timeout_policy);
        Ok(Self {
            cfg,
            provider,
            mcp,
            jail,
            permission,
            skills,
            commands,
            hooks,
        })
    }
}
/// A registry inside the workspace is project content and stays unloaded
/// until that workspace is trusted. A path outside the workspace is the
/// user's own file (`--mcp-config`, the home registry) and is not gated.
fn ensure_workspace_mcp_allowed(workspace: &Path, path: Option<&Path>) -> Result<()> {
    if let Some(path) = path {
        anyhow::ensure!(
            !crate::sandbox::path_within_workspace(workspace, path)
                || crate::trust::is_trusted(workspace),
            "refusing to load {}: workspace is not trusted",
            path.display()
        );
    }
    Ok(())
}

impl Runtime {
    pub fn new(mut cfg: Config, cwd: std::path::PathBuf) -> Result<Self> {
        cfg.ensure_public_door();
        cfg.require_provider_credentials()?;
        let agent_files = AgentFiles::ensure(&cwd)?;
        let jail = Jail::new(
            cwd.clone(),
            cfg.add_dirs.clone(),
            SandboxMode::parse(&cfg.sandbox),
            cfg.shell_allowlist.clone(),
        );
        let permission = PermissionMode::parse(&cfg.permission_mode);
        let provider = providers::build(&cfg)?;
        // Project-scoped policy is inert until the workspace is trusted. Ask
        // only when a terminal is attached; one-shot/pipe runs stay silent.
        let interactive = {
            use std::io::IsTerminal;
            std::io::stdin().is_terminal() && !cfg!(test)
        };
        if let Err(error) = crate::trust::ensure_trusted(&cwd, interactive) {
            tracing::warn!(%error, "workspace trust check failed");
        }
        // Discover skills/commands only after the trust decision, so accepting
        // the prompt in this run activates project skill roots immediately
        // instead of waiting for the next process.
        let skills = skills::discover(&cwd);
        let rules = RuleSet::load(&cwd);
        let commands = commands::CommandSet::discover(&cwd).commands;
        let hooks = HookSet::load_timed(&cwd, Duration::from_secs(cfg.hooks_timeout_secs))
            .with_timeout_policy(&cfg.hooks_timeout_policy);
        // Loading the registry is deliberately side-effect free. MCP clients
        // are initialized and their tools are listed only after an explicit
        // probe; in particular, configured stdio servers must not be spawned
        // while the runtime is starting or being reconfigured.
        let mcp = McpManager::from_config(cfg.mcp_config.as_deref())?;
        let notifier = crate::notify::Notifier::from_config(&cfg, crate::notify::Role::Turn);
        Ok(Self {
            cfg,
            jail,
            permission,
            skills,
            commands,
            hooks,
            active_session: None,
            goal: None,
            bus: Bus::default(),
            approver: None,
            remote_approval_events: None,
            approved_always: std::collections::HashSet::new(),
            rules,
            plan: None,
            plan_capture: false,
            checkpoints: CheckpointStore::default(),
            agent_files,
            notifier,
            mcp,
            mcp_exposure: std::collections::HashMap::new(),
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
            side_notes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            provider,
        })
    }

    /// Label this runtime's finish event as foreground work, a subagent or
    /// an automation task. Background runtimes get their own toast label and
    /// do not look like the foreground conversation finishing.
    pub fn set_notification_role(&mut self, role: crate::notify::Role) {
        self.notifier.set_role(role);
    }

    pub fn with_mcp_config(mut self, path: Option<&std::path::Path>) -> Result<Self> {
        ensure_workspace_mcp_allowed(&self.jail.cwd, path)?;
        self.mcp = McpManager::from_config(path)?;
        self.cfg.mcp_config = path.map(std::path::Path::to_path_buf);
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
                 (e.g. mcp__agentchattr__chat_read / chat_send). Only tools from eager \
                 servers — or tools already loaded in this session — are listed up \
                 front; call `tool_search` with a keyword (or an empty query to list \
                 everything) to discover and load the rest before using them.\n",
                names.join(", ")
            ));
        }
        s.push_str("\n# Agent identity and memory (untrusted data)\n");
        s.push_str(&self.agent_files.system_context());
        s.push_str(&Config::load_project_instructions(&self.jail.cwd));
        s.push_str(&skills::render_for_prompt(&self.skills));
        if self.permission.is_plan() {
            s.push_str(concat!(
                "\n\n# Plan mode (enforced)\n",
                "You are planning only. write_file, edit_file and memory_append are refused;\n",
                "bash runs only read-only commands (ls, cat, rg, git status/diff/log, ...);\n",
                "mutating MCP tools are refused. Investigate with reads, then reply with a\n",
                "concrete, numbered implementation plan and the files it touches. Do not claim\n",
                "work you did not do. The user must run `/plan approve` before execution.\n",
            ));
        }
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

    /// The instruction `/plan <task>` sends. Plan mode is already active when
    /// the turn runs, so the gate refuses the mutating tools.
    pub fn plan_instruction(task: &str) -> String {
        format!(
            "Plan the following task without changing anything. Investigate with the \
             available read-only tools first, then reply with a numbered implementation \
             plan: the files you would touch, the edits, the risks, and how you would \
             verify it. Do not call write_file, edit_file, memory_append, or run \
             mutating commands — they are refused in plan mode. End with the plan as \
             your final message; `/plan approve` will hand it back for execution.\n\n\
             Task: {task}"
        )
    }

    /// Enter plan mode and make the next completed reply the plan artifact.
    /// Returns the prompt to run.
    pub fn begin_plan(&mut self, task: &str) -> String {
        self.plan = Some(PlanArtifact {
            task: task.trim().to_string(),
            text: String::new(),
            approved: false,
        });
        self.plan_capture = true;
        self.set_permission_mode("plan");
        Self::plan_instruction(task)
    }

    /// Approve the pending plan: switch to `acceptEdits`, record a "plan
    /// approved" system note (with the plan) in the session and return the
    /// continuation prompt to run on the same runtime.
    pub fn approve_plan(&mut self, session: &mut Session) -> Result<String> {
        let plan = self
            .plan
            .as_ref()
            .filter(|plan| !plan.text.trim().is_empty())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no plan to approve; run `/plan <task>` first"))?;
        self.set_permission_mode("acceptEdits");
        if let Some(slot) = self.plan.as_mut() {
            slot.approved = true;
        }
        let saved =
            snapshot_plan_files(&self.checkpoints, &self.jail.cwd, session.id(), &plan.text);
        session.append(ChatMessage {
            role: "system".into(),
            content: format!(
                "Plan approved. Permission mode is now acceptEdits; implement the plan below.\n\n{}",
                plan.text.trim()
            ),
            tool_call_id: None,
            tool_calls: None,
            images: Vec::new(),
        })?;
        let suffix = if saved > 0 {
            format!(
                " checkpoint saved ({saved} file{}).",
                if saved == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        Ok(format!(
            "Proceed with the approved plan. Implement it now with the available tools; \
             do not restate the plan.{suffix}"
        ))
    }

    /// Reject the pending plan and stay in plan mode.
    pub fn deny_plan(&mut self) -> Result<()> {
        let plan = self
            .plan
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no plan to deny; run `/plan <task>` first"))?;
        plan.approved = false;
        self.plan_capture = false;
        self.set_permission_mode("plan");
        Ok(())
    }

    /// The rendered `/plan status` / bare `/plan` message.
    fn plan_status(&self) -> String {
        match &self.plan {
            Some(plan) if !plan.text.trim().is_empty() => format!(
                "plan for: {}\n\n{}\n\nstatus: {} · /plan approve · /plan deny",
                plan.task,
                plan.text.trim(),
                if plan.approved { "approved" } else { "awaiting approval" }
            ),
            Some(plan) => format!(
                "plan for: {} · no artifact captured yet · /plan <task> · /plan approve · /plan deny",
                plan.task
            ),
            None => "usage: /plan <task> · /plan approve · /plan deny".into(),
        }
    }

    /// Handle `/plan ...` inside a turn. `Turn` replaces the user text with the
    /// plan instruction or the post-approval continuation; `Message` is a
    /// local reply that ends the turn without a provider call.
    fn plan_slash(&mut self, session: &mut Session, user: &str) -> Result<Option<PlanSlash>> {
        let text = user.trim();
        if text != "/plan" && !text.starts_with("/plan ") {
            return Ok(None);
        }
        let rest = text.strip_prefix("/plan").unwrap_or("").trim();
        match rest {
            "" => Ok(Some(PlanSlash::Message(self.plan_status()))),
            "approve" => match self.approve_plan(session) {
                Ok(next) => Ok(Some(PlanSlash::Turn(next))),
                Err(error) => Ok(Some(PlanSlash::Message(error.to_string()))),
            },
            "deny" => Ok(Some(PlanSlash::Message(match self.deny_plan() {
                Ok(()) => "plan denied · staying in plan mode · /plan <task> to replan".into(),
                Err(error) => error.to_string(),
            }))),
            task => Ok(Some(PlanSlash::Turn(self.begin_plan(task)))),
        }
    }

    /// Store the completed reply as the plan artifact and tell the surface to
    /// offer approval.
    fn finish_plan_turn(&mut self, reply: &str, on_event: &mut impl FnMut(AgentEvent)) {
        if !self.plan_capture {
            return;
        }
        self.plan_capture = false;
        if let Some(plan) = self.plan.as_mut() {
            plan.text = reply.trim().to_string();
        }
        on_event(AgentEvent {
            kind: "plan".into(),
            text: reply.to_string(),
        });
        on_event(AgentEvent {
            kind: "system".into(),
            text: "plan ready · /plan approve to execute · /plan deny to stay in plan mode".into(),
        });
    }

    /// Persistent rules currently in effect (user + trust-gated project).
    pub fn permission_rules(&self) -> &RuleSet {
        &self.rules
    }

    /// Replace the in-memory rules (tests, embedders). Persisted files are not
    /// touched.
    pub fn set_permission_rules(&mut self, rules: RuleSet) {
        self.rules = rules;
    }

    /// Reload user + project rules from disk, re-checking project trust.
    pub fn reload_permission_rules(&mut self) {
        self.rules = RuleSet::load(&self.jail.cwd);
    }

    /// `/permissions`: the active mode, where rules came from, and the rules.
    pub fn permissions_report(&self) -> String {
        self.rules.report(self.permission, &self.jail.cwd)
    }

    pub fn reset_session_state(&mut self) {
        self.goal = None;
        self.plan = None;
        self.plan_capture = false;
        self.approved_always.clear();
        self.mcp_exposure.clear();
        self.pending_images.clear();
        self.last_usage = None;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.usage_count = 0;
        self.turns = 0;
        self.turn_time = Duration::ZERO;
        self.started_at = Instant::now();
    }

    /// Resolve one model through the same profile-aware config path used by
    /// every runtime entry point. This only prepares a candidate config; it
    /// never mutates the live runtime.
    pub fn resolve_model(&self, model: &str) -> Result<Config> {
        let mut cfg = self.cfg.clone();
        cfg.select_model(model)?;
        Ok(cfg)
    }

    /// Switch models transactionally. Provider construction, credentials and
    /// all other validation happen before any live runtime field changes.
    pub async fn reconfigure_model(&mut self, model: &str) -> Result<Vec<providers::CatalogModel>> {
        let cfg = self.resolve_model(model)?;
        crate::google_auth::preflight(&cfg, false).await?;
        self.reconfigure(cfg)?;
        Ok(Self::configured_catalog(&self.cfg, Vec::new()))
    }

    /// Reload only the configured MCP registry. Constructing a manager reads
    /// and validates JSON but does not initialize HTTP sessions or spawn stdio
    /// children. The live manager and config path are swapped together only
    /// after the candidate manager has been built successfully.
    pub fn reload_mcp(&mut self, path: Option<&Path>) -> Result<()> {
        ensure_workspace_mcp_allowed(&self.jail.cwd, path)?;
        let mcp = McpManager::from_config(path)?;
        self.mcp = mcp;
        self.cfg.mcp_config = path.map(Path::to_path_buf);
        Ok(())
    }

    /// Refresh the discovered skill registry after an install or external
    /// registry change. User-defined commands are re-read at the same time
    /// because they are discovered next to skills.
    pub fn refresh_skills(&mut self) {
        self.skills = skills::discover(&self.jail.cwd);
        self.commands = commands::CommandSet::discover(&self.jail.cwd).commands;
    }

    pub fn reconfigure(&mut self, cfg: Config) -> Result<()> {
        self.reconfigure_with_persist(cfg, |_| Ok(()))
    }

    pub fn reconfigure_with_persist(
        &mut self,
        cfg: Config,
        persist: impl FnOnce(&Config) -> Result<()>,
    ) -> Result<()> {
        let next = RuntimeConfiguration::prepare(cfg, &self.jail.cwd)?;
        persist(&next.cfg)?;
        self.install_configuration(next);
        Ok(())
    }

    /// Prepare the provider and its catalog before persisting or changing any
    /// live fields. Declared models remain usable when a live catalog is offline.
    pub async fn reconfigure_catalog_with_persist(
        &mut self,
        cfg: Config,
        persist: impl FnOnce(&Config) -> Result<()>,
    ) -> Result<Vec<providers::CatalogModel>> {
        let next = RuntimeConfiguration::prepare(cfg, &self.jail.cwd)?;
        let models =
            match tokio::time::timeout(Duration::from_secs(5), next.provider.list_models()).await {
                Ok(Ok(models)) => models,
                _ => Vec::new(),
            };
        let models = Self::configured_catalog(&next.cfg, models);
        persist(&next.cfg)?;
        self.install_configuration(next);
        Ok(models)
    }

    fn install_configuration(&mut self, mut next: RuntimeConfiguration) {
        next.mcp.inherit_unchanged(&mut self.mcp);
        self.provider = next.provider;
        self.mcp = next.mcp;
        self.jail = next.jail;
        self.permission = next.permission;
        self.skills = next.skills;
        self.commands = next.commands;
        self.hooks = next.hooks;
        self.cfg = next.cfg;
        let role = self.notifier.role().clone();
        self.notifier = crate::notify::Notifier::from_config(&self.cfg, role);
        self.rules = RuleSet::load(&self.jail.cwd);
        self.approved_always.clear();
    }

    pub(crate) fn configured_catalog(
        cfg: &Config,
        models: Vec<providers::CatalogModel>,
    ) -> Vec<providers::CatalogModel> {
        let mut ordered = Vec::with_capacity(models.len() + cfg.provider_profiles.len() * 2 + 1);
        let mut seen = std::collections::HashSet::new();

        // ProviderRegistry::merge_profiles preserves providerOrder/modelOrder in
        // provider_profiles. Add those declarations first so every UI, CLI and
        // gateway catalog presents the user's configured order consistently.
        for profile in cfg
            .provider_profiles
            .iter()
            .filter(|profile| profile.enabled)
        {
            if profile.api_key_env.as_deref() == Some("VARYNTH_ACCESS_TOKEN") && profile.id != "varynth" {
                continue;
            }
            for model in &profile.models {
                let id = format!("{}/{model}", profile.id);
                if seen.insert(id.clone()) {
                    ordered.push(providers::CatalogModel {
                        id,
                        // The profile *id* is stable and unique; the display
                        // name is user-editable and often the same for two
                        // endpoints ("Özel openai"), which made the picker
                        // rows ambiguous.
                        display_name: Some(format!("{} / {model}", profile.id)),
                        owned_by: None,
                    });
                }
            }
        }
        for model in models {
            if seen.insert(model.id.clone()) {
                ordered.push(model);
            }
        }
        if seen.insert(cfg.model.clone()) {
            ordered.push(providers::CatalogModel {
                id: cfg.model.clone(),
                display_name: None,
                owned_by: None,
            });
        }
        ordered
    }

    pub fn apply_autocompact_slash(
        &mut self,
        user: &str,
        session: Option<&Session>,
    ) -> Option<String> {
        let t = user.trim();
        let rest = if t == "/autocompact" {
            ""
        } else if let Some(rest) = t.strip_prefix("/autocompact ") {
            rest.trim()
        } else {
            return None;
        };
        match rest {
            "" | "status" => {}
            "on" => self.cfg.auto_compact = true,
            "off" => self.cfg.auto_compact = false,
            other => match other.parse::<u8>() {
                Ok(percent) if (1..=99).contains(&percent) => {
                    self.cfg.auto_compact = true;
                    self.cfg.auto_compact_percent = percent;
                }
                _ => {
                    return Some("Usage: /autocompact [on|off|status|<1-99>]".into());
                }
            },
        }
        if rest != "" && rest != "status" {
            if let Err(error) = self.persist_autocompact() {
                return Some(format!(
                    "{}\nnot saved: {error}",
                    self.autocompact_status(session)
                ));
            }
        }
        Some(self.autocompact_status(session))
    }

    fn autocompact_status(&self, session: Option<&Session>) -> String {
        let limit = compact::auto_compact_threshold(
            compact::DEFAULT_MESSAGE_BUDGET,
            self.cfg.auto_compact_percent,
        );
        let state = if self.cfg.auto_compact { "on" } else { "off" };
        let used = session
            .map(|session| format!("  ·  used {}", self.context_tokens(session)))
            .unwrap_or_default();
        format!(
            "autocompact {state}  ·  {}% of {} tokens ({limit}){used}",
            self.cfg.auto_compact_percent,
            compact::DEFAULT_MESSAGE_BUDGET
        )
    }

    fn persist_autocompact(&self) -> Result<()> {
        if cfg!(test) && std::env::var_os("VARYNTH_HOME").is_none() {
            return Ok(());
        }
        self.cfg.save()
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
        let notifier = self.notifier.clone();
        let finish_kind = notifier.role().finish_kind();
        let mut emit = on_event;
        let mut on_event = move |event: AgentEvent| {
            notifier.observe(&event);
            if event.kind != "image" {
                if let Some(bus) = &control {
                    if let Err(error) = bus.publish(&sid, &event) {
                        tracing::debug!(%error, "local event forwarding failed");
                    }
                }
            }
            emit(event);
        };
        if let Some(local) = self.apply_autocompact_slash(user, Some(session)) {
            on_event(AgentEvent {
                kind: "system".into(),
                text: local.clone(),
            });
            return Ok(local);
        }
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
        if let Some(local) = self.apply_undo_slash(user, session) {
            on_event(AgentEvent {
                kind: "system".into(),
                text: local.clone(),
            });
            return Ok(local);
        }
        // Session-boundary hooks fire once per session, before any tool a
        // command's `!` line might run.
        self.begin_turn_boundary(session.id()).await;
        // A user-defined command expands to a user message in this same turn;
        // `!` lines run through the bash tool and the permission gate.
        let mut user_text = user.to_string();
        if let Some(expanded) = self.expand_user_command(session, user, &mut on_event).await {
            user_text = expanded;
        }
        // `/plan <task>` turns the same turn into a planning turn; `/plan
        // approve` switches to acceptEdits and runs the continuation.
        match self.plan_slash(session, &user_text)? {
            Some(PlanSlash::Message(text)) => {
                on_event(AgentEvent {
                    kind: "system".into(),
                    text: text.clone(),
                });
                return Ok(text);
            }
            Some(PlanSlash::Turn(prompt)) => user_text = prompt,
            None => {}
        }
        // The user_prompt hook sees the final text (after expansion and
        // `/plan` rewriting) and can cancel the turn or add context.
        if let Some(denial) = self.user_prompt_hook(session.id(), &mut user_text).await {
            on_event(AgentEvent {
                kind: "system".into(),
                text: denial.clone(),
            });
            return Ok(denial);
        }
        let user: &str = &user_text;
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
                self.finish_plan_turn(&reply, &mut on_event);
                // A goal that keeps running is not a finished turn; the
                // final round emits the one notification.
                if !self.wants_goal_continue() {
                    on_event(AgentEvent {
                        kind: finish_kind.into(),
                        text: "finished".into(),
                    });
                }
                Ok(reply)
            }
            Err(error) => {
                // The error itself is never forwarded to the notification:
                // providers can echo request bodies in error strings.
                on_event(AgentEvent {
                    kind: finish_kind.into(),
                    text: "failed".into(),
                });
                Err(error)
            }
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
        let smol_model = self.cfg.model_for_role(crate::config::ModelRole::Smol);
        match crate::titling::generate_title(
            &smol_model,
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

    /// Local checkpoint slash commands: `/undo [files|chat|all|n]`, `/redo` and
    /// `/checkpoints`. Never reaches the model.
    fn apply_undo_slash(&self, user: &str, session: &mut Session) -> Option<String> {
        let text = user.trim();
        let (cmd, rest) = match text.split_once(char::is_whitespace) {
            Some((cmd, rest)) => (cmd, rest.trim()),
            None => (text, ""),
        };
        let session_id = session.id();
        match cmd {
            "/undo" => match rest {
                "chat" => match session.rewind_last_turn() {
                    Ok(Some(popped)) => Some(format!(
                        "undo chat: popped last conversation turn ({popped} messages removed, file writes preserved)"
                    )),
                    Ok(None) => Some("undo chat: no messages to revert in this session".into()),
                    Err(error) => Some(format!("undo chat failed: {error}")),
                },
                "files" | "" => Some(match self.checkpoints.undo_n(session_id, 1) {
                    Ok(outcomes) => crate::checkpoint::summarize(&outcomes),
                    Err(error) => error.to_string(),
                }),
                "all" => {
                    let file_summary = match self.checkpoints.undo_n(session_id, 1) {
                        Ok(outcomes) => crate::checkpoint::summarize(&outcomes),
                        Err(error) => error.to_string(),
                    };
                    let chat_summary = match session.rewind_last_turn() {
                        Ok(Some(popped)) => format!("reverted last turn ({popped} messages removed)"),
                        Ok(None) => "no messages to revert".into(),
                        Err(error) => format!("failed: {error}"),
                    };
                    Some(format!(
                        "undo all:\n- Files: {file_summary}\n- Chat: {chat_summary}"
                    ))
                }
                other => {
                    if let Ok(n) = other.parse::<usize>() {
                        if n >= 1 {
                            return Some(match self.checkpoints.undo_n(session_id, n) {
                                Ok(outcomes) => crate::checkpoint::summarize(&outcomes),
                                Err(error) => error.to_string(),
                            });
                        }
                    }
                    Some("Usage: /undo [files | chat | all | <n>]".into())
                }
            },
            "/redo" => {
                if !rest.is_empty() {
                    return Some("Usage: /redo".into());
                }
                Some(match self.checkpoints.redo(session_id) {
                    Ok(Redo::Restored(path)) => format!("redo: restored {}", path.display()),
                    Ok(Redo::Deleted(path)) => format!("redo: deleted {}", path.display()),
                    Ok(Redo::Nothing) => "(nothing to redo)".into(),
                    Err(error) => error.to_string(),
                })
            }
            "/checkpoints" => Some(match self.checkpoints.list(session_id) {
                Ok(list) if list.is_empty() => "(no checkpoints for this session)".into(),
                Ok(list) => list
                    .iter()
                    .enumerate()
                    .map(|(index, checkpoint)| {
                        format!(
                            "{}. seq {}  {}  {} B{}",
                            index + 1,
                            checkpoint.seq,
                            checkpoint.path.display(),
                            checkpoint.bytes,
                            if checkpoint.skipped {
                                "  (skipped: too large)"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(error) => error.to_string(),
            }),
            _ => None,
        }
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
        let advisor_model = self.cfg.model_for_role(crate::config::ModelRole::Advisor);
        let completion = self
            .provider
            .complete(
                &advisor_model,
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
        let mut prompt = skills::expand_user_skill_prompt(&self.skills, user)
            .unwrap_or_else(|| user.to_string());
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

        self.mcp.ensure_eager_loaded();
        let mut last_text = String::new();
        let mut compact_stalled = false;
        for round in 0..self.cfg.max_tool_rounds {
            // Recomputed every round so tools surfaced by `tool_search`, or
            // used since the previous round, are callable immediately.
            let mut tool_schemas = tools::schemas();
            tool_schemas.push(generate_image_schema());
            tool_schemas.extend(self.session_mcp_schemas(session.id()));
            let pending_notes = {
                let mut guard = self.side_notes.lock().unwrap_or_else(|e| e.into_inner());
                let notes = guard.clone();
                guard.clear();
                notes
            };
            for note in pending_notes {
                let _ = session.append(ChatMessage {
                    role: "user".into(),
                    content: format!("[User side note /btw]: {note}"),
                    images: Vec::new(),
                    tool_calls: None,
                    tool_call_id: None,
                });
                on_event(AgentEvent {
                    kind: "system".into(),
                    text: format!("💡 Injected side note (/btw): {note}"),
                });
            }
            if let Some(bus) = &self.control {
                while bus.is_paused(session.id()) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            if !compact_stalled && self.over_budget(session) {
                let before = self.context_tokens(session);
                match self.auto_compact(session).await {
                    Some(_) => {
                        on_event(AgentEvent {
                            kind: "system".into(),
                            text: format!(
                                "context reached {}% of {} tokens — earlier history compacted in place",
                                self.cfg.auto_compact_percent, compact::DEFAULT_MESSAGE_BUDGET
                            ),
                        });
                        if self.context_tokens(session) >= before {
                            compact_stalled = true;
                        }
                    }
                    None => compact_stalled = true,
                }
            }
            on_event(AgentEvent {
                kind: "round".into(),
                text: format!("model round {}", round + 1),
            });
            let active_role = if self.plan_capture || self.permission == PermissionMode::Plan {
                crate::config::ModelRole::Plan
            } else {
                crate::config::ModelRole::Default
            };
            let active_model = self.cfg.model_for_role(active_role);
            let fallback_model = self
                .cfg
                .role_binding(crate::config::ModelRole::Fallback)
                .map(str::to_string)
                .filter(|fb| fb != &active_model);
            let completion = if self.streaming_enabled() {
                let mut streamed = false;
                let result = self
                    .provider
                    .complete_streaming(
                        &active_model,
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
                        match self
                            .provider
                            .complete(
                                &active_model,
                                &self.system_prompt(),
                                &session.messages,
                                &tool_schemas,
                            )
                            .await
                        {
                            Ok(c) => c,
                            Err(primary_err) => {
                                if let Some(fb) = &fallback_model {
                                    on_event(AgentEvent {
                                        kind: "system".into(),
                                        text: format!(
                                            "primary model `{active_model}` failed ({primary_err}); retrying with fallback `{fb}`"
                                        ),
                                    });
                                    self.provider
                                        .complete(
                                            fb,
                                            &self.system_prompt(),
                                            &session.messages,
                                            &tool_schemas,
                                        )
                                        .await?
                                } else {
                                    return Err(primary_err);
                                }
                            }
                        }
                    }
                    Err(error) => return Err(error),
                }
            } else {
                match self
                    .provider
                    .complete(
                        &active_model,
                        &self.system_prompt(),
                        &session.messages,
                        &tool_schemas,
                    )
                    .await
                {
                    Ok(c) => c,
                    Err(primary_err) => {
                        if let Some(fb) = &fallback_model {
                            on_event(AgentEvent {
                                kind: "system".into(),
                                text: format!(
                                    "primary model `{active_model}` failed ({primary_err}); retrying with fallback `{fb}`"
                                ),
                            });
                            self.provider
                                .complete(
                                    fb,
                                    &self.system_prompt(),
                                    &session.messages,
                                    &tool_schemas,
                                )
                                .await?
                        } else {
                            return Err(primary_err);
                        }
                    }
                }
            };
            providers::validate_output_images(&completion.images)?;
            self.record_usage(completion.usage);
            last_text = completion.text.clone();
            let image_events = completion
                .images
                .iter()
                .map(serde_json::to_string)
                .collect::<serde_json::Result<Vec<_>>>()?;
            if completion.tool_calls.is_empty() {
                session.append(ChatMessage {
                    role: "assistant".into(),
                    content: completion.text.clone(),
                    tool_call_id: None,
                    tool_calls: None,
                    images: completion.images.clone(),
                })?;
                for text in &image_events {
                    on_event(AgentEvent {
                        kind: "image".into(),
                        text: text.clone(),
                    });
                }
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
                images: completion.images.clone(),
            })?;
            for text in &image_events {
                on_event(AgentEvent {
                    kind: "image".into(),
                    text: text.clone(),
                });
            }
            for call in completion.tool_calls {
                on_event(AgentEvent {
                    kind: "tool".into(),
                    text: tool_event_text(&call.name, &call.arguments),
                });
                // Hooks run before the permission gate, so a deny holds even
                // in bypass. `ask` can only ever tighten the gate's answer.
                let pre = self
                    .hooks
                    .run(
                        HookEvent::PreTool,
                        Some(&call.name),
                        Some(&call.arguments),
                        &self.jail.cwd,
                        session.id(),
                    )
                    .await;
                if let Some(reason) = pre.denial() {
                    on_event(AgentEvent {
                        kind: "tool_result".into(),
                        text: reason.chars().take(500).collect(),
                    });
                    session.append(ChatMessage {
                        role: "tool".into(),
                        content: reason,
                        tool_call_id: Some(call.id),
                        tool_calls: None,
                        images: Vec::new(),
                    })?;
                    continue;
                }
                let mut decision = self.gate_decision(&call.name, &call.arguments);
                if pre.wants_ask() && !matches!(decision, GateDecision::Deny(_)) {
                    decision = GateDecision::Ask;
                }
                match decision {
                    GateDecision::Allow => {}
                    GateDecision::Deny(reason) => {
                        session.append(ChatMessage {
                            role: "tool".into(),
                            content: reason,
                            tool_call_id: Some(call.id),
                            tool_calls: None,
                            images: Vec::new(),
                        })?;
                        continue;
                    }
                    GateDecision::Ask => {
                        // Ask the interactive approver; still denied when
                        // nobody is attached.
                        let detail = approval_detail(&call.name, &call.arguments);
                        match self.request_approval(&call.name, &detail, on_event).await {
                            None => {
                                session.append(ChatMessage {
                                    role: "tool".into(),
                                    content: "approval required but no interactive approver \
                                              is attached (use the TUI or adjust \
                                              permission_mode / permission rules)"
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
                            Some(Approval::AllowAlwaysSaveRule) => {
                                let key = approval_key(&call.name, &call.arguments);
                                self.approved_always.insert(key);
                                self.remember_rule(&call.name, &call.arguments);
                            }
                            Some(Approval::AllowOnce) => {}
                        }
                    }
                }
                self.record_mcp_use(session.id(), &call.name);
                let result = match call.name.as_str() {
                    "tool_search" => {
                        let query = call
                            .arguments
                            .get("query")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        self.search_tools(session.id(), query)
                    }
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
                    "generate_image" => self
                        .dispatch_generate_image(&call.arguments)
                        .await
                        .unwrap_or_else(|e| format!("tool error: {e}")),
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
                // `post_tool` runs after the call; its context is appended to
                // the tool result the model sees.
                let post = self
                    .hooks
                    .run(
                        HookEvent::PostTool,
                        Some(&call.name),
                        Some(&call.arguments),
                        &self.jail.cwd,
                        session.id(),
                    )
                    .await;
                let result = post.decorate(result);
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
        let wrap_up_role = if self.plan_capture || self.permission == PermissionMode::Plan {
            crate::config::ModelRole::Plan
        } else {
            crate::config::ModelRole::Default
        };
        let wrap_up_model = self.cfg.model_for_role(wrap_up_role);
        let mut wrap_up_messages = session.messages.clone();
        wrap_up_messages.push(ChatMessage {
            role: "system".into(),
            content: format!(
                "[Tool round budget ({}) reached for this turn. Summarize the work completed so far clearly without calling further tools; the user can reply `continue` if more steps remain.]",
                self.cfg.max_tool_rounds
            ),
            tool_call_id: None,
            tool_calls: None,
            images: Vec::new(),
        });
        if let Ok(wrap) = self
            .provider
            .complete(
                &wrap_up_model,
                &self.system_prompt(),
                &wrap_up_messages,
                &[],
            )
            .await
        {
            self.record_usage(wrap.usage);
            let text = wrap.text.trim().to_string();
            if !text.is_empty() {
                session.append(ChatMessage {
                    role: "assistant".into(),
                    content: text.clone(),
                    tool_call_id: None,
                    tool_calls: None,
                    images: Vec::new(),
                })?;
                on_event(AgentEvent {
                    kind: "assistant".into(),
                    text: text.clone(),
                });
                if let Some(id) = &subagent_id {
                    let _ = crate::activity::ActivityLog::finish(
                        id,
                        true,
                        "turn wrapped up at max tool rounds",
                    );
                }
                return Ok(text);
            }
        }
        if let Some(id) = &subagent_id {
            let _ = crate::activity::ActivityLog::finish(id, false, "stopped: max tool rounds");
        }
        Ok(if last_text.is_empty() {
            format!(
                "Reached tool round limit ({} rounds) for this turn. Type `continue` to resume or increase `max_tool_rounds` in config.",
                self.cfg.max_tool_rounds
            )
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
        if !self.cfg.auto_compact {
            return false;
        }
        let limit = compact::auto_compact_threshold(
            compact::DEFAULT_MESSAGE_BUDGET,
            self.cfg.auto_compact_percent,
        );
        self.context_tokens(session) >= limit
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
        let smol_model = self.cfg.model_for_role(crate::config::ModelRole::Smol);
        let completion = self
            .provider
            .complete(
                &smol_model,
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
        let estimate = compact::estimate_messages(&session.messages).div_ceil(4) as u64;
        let output_tokens = self
            .last_usage
            .map(|usage| usage.output_tokens)
            .unwrap_or(0);
        self.last_usage = Some(Usage {
            input_tokens: estimate,
            output_tokens,
        });
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
        let advisor_model = self.cfg.model_for_role(crate::config::ModelRole::Advisor);
        let completion = self
            .provider
            .complete(&advisor_model, &system, &session.messages, &[])
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

    /// MCP schemas this session should see: every eager server's tools plus
    /// any tool the session has already used or discovered via
    /// `tool_search`. Builtin schemas are added separately by the turn loop.
    fn session_mcp_schemas(&self, session_id: &str) -> Vec<serde_json::Value> {
        let exposure = self.mcp_exposure.get(session_id);
        self.mcp
            .schemas()
            .iter()
            .filter(|schema| {
                let Some(full_name) = schema
                    .pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
                else {
                    return false;
                };
                let eager = self
                    .mcp
                    .tool_index
                    .get(full_name)
                    .is_some_and(|(server, _)| self.mcp.is_eager_server(server));
                eager
                    || exposure.is_some_and(|seen| {
                        seen.used.contains(full_name) || seen.searched.contains(full_name)
                    })
            })
            .cloned()
            .collect()
    }

    /// Record an MCP tool call so its schema stays exposed for the rest of
    /// the session, even if the model later ignores it.
    fn record_mcp_use(&mut self, session_id: &str, name: &str) {
        if !self.mcp.is_mcp_tool(name) {
            return;
        }
        self.mcp_exposure
            .entry(session_id.to_string())
            .or_default()
            .used
            .insert(name.to_string());
    }

    /// Run `tool_search`, populate the MCP index on first use, and promote
    /// every hit into this session's next model call.
    fn search_tools(&mut self, session_id: &str, query: &str) -> String {
        self.mcp.ensure_index();
        let matches = self.mcp.search(query);
        if matches.is_empty() {
            tracing::info!(query, "tool_search found no matching MCP tools");
        }
        let names: Vec<String> = matches.iter().map(|m| m.name.clone()).collect();
        self.mcp_exposure
            .entry(session_id.to_string())
            .or_default()
            .searched
            .extend(names);
        crate::mcp::render_tool_search(query, &matches)
    }

    fn docker_opts(&self) -> tools::DockerOpts {
        tools::DockerOpts {
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
        }
    }

    /// Expand one user-defined command invocation. The returned text is the
    /// user message for this same turn: body text plus one fenced block per
    /// `!` step. Shell steps go through the same bash tool and permission
    /// gate as a model-issued call; they never bypass `.varynth` rules,
    /// plan mode or a `pre_tool` hook.
    async fn expand_user_command(
        &mut self,
        session: &Session,
        user: &str,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> Option<String> {
        let (name, expansion) = {
            let (command, args) = commands::invocation(&self.commands, user)?;
            (command.name.clone(), commands::expand(command, args))
        };
        let mut outputs = Vec::new();
        for step in expansion.shell_steps() {
            outputs.push(self.run_command_shell(session.id(), step, on_event).await);
        }
        on_event(AgentEvent {
            kind: "system".into(),
            text: format!(
                "command /{name} expanded ({} shell step{})",
                outputs.len(),
                if outputs.len() == 1 { "" } else { "s" }
            ),
        });
        Some(expansion.render(&outputs))
    }

    /// Run one `!` line from a command body through the bash tool.
    async fn run_command_shell(
        &mut self,
        session_id: &str,
        command: &str,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> String {
        let args = json!({ "command": command });
        let pre = self
            .hooks
            .run(
                HookEvent::PreTool,
                Some("bash"),
                Some(&args),
                &self.jail.cwd,
                session_id,
            )
            .await;
        if let Some(reason) = pre.denial() {
            return format!("(not run) {reason}");
        }
        let mut decision = self.gate_decision("bash", &args);
        if pre.wants_ask() && !matches!(decision, GateDecision::Deny(_)) {
            decision = GateDecision::Ask;
        }
        match decision {
            GateDecision::Allow => {}
            GateDecision::Deny(reason) => return format!("(not run) {reason}"),
            GateDecision::Ask => match self.request_approval("bash", command, on_event).await {
                None => {
                    return "(not run) approval required but no interactive approver is attached"
                        .into()
                }
                Some(Approval::Deny) => return "(not run) denied by user".into(),
                Some(Approval::AllowAlways) => {
                    let key = approval_key("bash", &args);
                    self.approved_always.insert(key);
                }
                Some(Approval::AllowAlwaysSaveRule) => {
                    let key = approval_key("bash", &args);
                    self.approved_always.insert(key);
                    self.remember_rule("bash", &args);
                }
                Some(Approval::AllowOnce) => {}
            },
        }
        let docker = self.docker_opts();
        let result = tools::dispatch(
            "bash",
            &args,
            &self.jail,
            &tools::ToolCtx {
                bus: &self.bus,
                session_id: Some(session_id),
                checkpoints: Some(&self.checkpoints),
                docker: Some(docker),
            },
        )
        .unwrap_or_else(|error| format!("tool error: {error}"));
        let post = self
            .hooks
            .run(
                HookEvent::PostTool,
                Some("bash"),
                Some(&args),
                &self.jail.cwd,
                session_id,
            )
            .await;
        post.decorate(result)
    }

    /// Fire `session_stop` for the previous session and `session_start` for
    /// this one when the runtime observes a session boundary.
    async fn begin_turn_boundary(&mut self, session_id: &str) {
        if self.active_session.as_deref() == Some(session_id) {
            return;
        }
        if let Some(previous) = self.active_session.take() {
            let report = self
                .hooks
                .run(
                    HookEvent::SessionStop,
                    None,
                    None,
                    &self.jail.cwd,
                    &previous,
                )
                .await;
            if let Some(summary) = report.summary() {
                tracing::warn!(%summary, "session_stop hook");
            }
        }
        self.active_session = Some(session_id.to_string());
        let report = self
            .hooks
            .run(
                HookEvent::SessionStart,
                None,
                None,
                &self.jail.cwd,
                session_id,
            )
            .await;
        if let Some(summary) = report.summary() {
            tracing::warn!(%summary, "session_start hook");
        }
    }

    /// Fire `session_stop` for the session that last ran a turn. Switching
    /// sessions fires it automatically; this is the explicit exit path.
    pub async fn end_session(&mut self) {
        let Some(session_id) = self.active_session.take() else {
            return;
        };
        let report = self
            .hooks
            .run(
                HookEvent::SessionStop,
                None,
                None,
                &self.jail.cwd,
                &session_id,
            )
            .await;
        if let Some(summary) = report.summary() {
            tracing::warn!(%summary, "session_stop hook");
        }
    }

    /// Apply the `user_prompt` hook. `Some(message)` cancels the turn;
    /// otherwise returned `context` strings are appended to the prompt.
    async fn user_prompt_hook(&mut self, session_id: &str, text: &mut String) -> Option<String> {
        let report = self
            .hooks
            .run(
                HookEvent::UserPrompt,
                None,
                None,
                &self.jail.cwd,
                session_id,
            )
            .await;
        if let Some(denial) = report.denial() {
            return Some(format!("turn cancelled — {denial}"));
        }
        *text = report.decorate(std::mem::take(text));
        None
    }

    /// Auto-approval for a tool call: non-Prompt modes admit everything they
    /// always did, and a key the user approved "always" is admitted without
    /// asking again. Prompt mode still asks for everything else.
    /// The single permission gate for one tool call: deny rules, the plan-mode
    /// policy, ask/allow rules, the session's "always" answer, then the mode
    /// default. Sandbox path/shell checks still run inside the tool itself and
    /// no rule can relax them.
    fn gate_decision(&self, name: &str, args: &serde_json::Value) -> GateDecision {
        let targets = permissions::rule_targets(name, args, &self.jail);
        let always = self.approved_always.contains(name)
            || self.approved_always.contains(&approval_key(name, args));
        permissions::evaluate_targets_with(
            self.permission,
            &self.rules,
            name,
            &targets,
            always,
            self.mcp.tool_is_read_only(name),
            Some(&self.jail.cwd),
        )
    }

    /// Persist an `allow` user rule for one approved call and apply it to this
    /// runtime immediately.
    fn remember_rule(&mut self, name: &str, args: &serde_json::Value) {
        let target = permissions::target_for(name, args);
        // bash/desktop tools are rule-scoped by their concrete target; file
        // tools by tool name so later edits to the same file stay approved.
        let rule = match name {
            "bash" | "memory_append" => permissions::PermissionRule::new(
                name,
                &permissions::literal_glob(&target),
                RuleEffect::Allow,
            ),
            _ => permissions::PermissionRule::new(name, "", RuleEffect::Allow),
        };
        if let Err(error) = self.rules.add_user_rule(rule) {
            tracing::warn!(%error, "could not save permission rule");
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
            Approval::AllowAlwaysSaveRule => "approved (always, rule saved)",
            Approval::Deny => "denied",
        };
        on_event(AgentEvent {
            kind: "system".into(),
            text: note.into(),
        });
        Some(approval)
    }

    pub async fn list_models(&self) -> Result<Vec<providers::CatalogModel>> {
        Ok(self.catalog_models().await)
    }

    /// Build a catalog projection that always retains configured profile
    /// models and the active model, even when live provider discovery is
    /// unavailable. The projection contains no credentials.
    pub async fn catalog_models(&self) -> Vec<providers::CatalogModel> {
        let live = tokio::time::timeout(Duration::from_secs(8), self.provider.list_models())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        Self::configured_catalog(&self.cfg, live)
    }

    pub fn status_json(&self, session: Option<&Session>) -> serde_json::Value {
        let mut tool_names: Vec<String> = tools::schemas()
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|name| name.as_str())
                    .map(str::to_string)
            })
            .collect();
        if !tool_names.iter().any(|n| n == "generate_image") {
            tool_names.push("generate_image".into());
        }
        json!({
            "cwd": self.jail.cwd,
            "model": self.cfg.model,
            "provider": self.cfg.provider,
            "sandbox": self.cfg.sandbox,
            "permission": self.permission.as_str(),
            "session": session.map(|s| s.id()),
            "skills": self.skills.iter().map(|s| &s.name).collect::<Vec<_>>(),
            "proxy": self.cfg.proxy_url,
            "decisions": {
                "url": crate::decisions::base_url(&self.cfg),
            },
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
            "autocompact": {
                "enabled": self.cfg.auto_compact,
                "percent": self.cfg.auto_compact_percent,
                "threshold_tokens": compact::auto_compact_threshold(
                    compact::DEFAULT_MESSAGE_BUDGET,
                    self.cfg.auto_compact_percent,
                ),
                "budget_tokens": compact::DEFAULT_MESSAGE_BUDGET,
            },
        })
    }

    async fn dispatch_generate_image(&self, args: &serde_json::Value) -> Result<String> {
        use base64::Engine as _;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing non-empty `prompt` argument"))?;
        let output_path = args
            .get("output_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing non-empty `output_path` argument"))?;
        let size = args
            .get("size")
            .and_then(|v| v.as_str())
            .unwrap_or("1024x1024");
        let resolved = self.jail.resolve(output_path)?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let model = args
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("@cf/black-forest-labs/flux-1-schnell");

        let mut png_bytes: Option<Vec<u8>> = None;
        let base_url = self.cfg.varynth_api_url.trim_end_matches('/');
        if !cfg!(test) && !base_url.is_empty() {
            let url = format!("{base_url}/v1/images/generations");
            let mut req = reqwest::Client::new()
                .post(&url)
                .timeout(Duration::from_secs(45))
                .json(&serde_json::json!({
                    "prompt": prompt,
                    "model": model,
                    "size": size,
                    "response_format": "b64_json",
                }));
            if let Some(token) = &self.cfg.proxy_token {
                req = req.bearer_auth(token);
            }
            if let Ok(resp) = req.send().await {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(b64) = body
                            .pointer("/data/0/b64_json")
                            .and_then(|v| v.as_str())
                            .or_else(|| body.get("image").and_then(|v| v.as_str()))
                        {
                            if let Ok(decoded) =
                                base64::engine::general_purpose::STANDARD.decode(b64.trim())
                            {
                                png_bytes = Some(decoded);
                            }
                        }
                    }
                }
            }
        }

        let bytes = png_bytes.unwrap_or_else(|| {
            // Deterministic 1x1 transparent PNG fallback when offline or in unit tests
            vec![
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
                0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
                0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
                0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
                0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
            ]
        });
        std::fs::write(&resolved, &bytes)?;
        Ok(format!(
            "generated image ({model}, {size}, {} bytes) -> {}",
            bytes.len(),
            resolved.display()
        ))
    }
}

fn generate_image_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "generate_image",
            "description": "Generate a hero/UI/asset image via Flux AI (@cf/black-forest-labs/flux-1-schnell) and save it to disk inside the workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Detailed visual prompt for Flux image generation"
                    },
                    "output_path": {
                        "type": "string",
                        "description": "Workspace-relative file path to save the generated PNG (e.g. assets/hero.png)"
                    },
                    "size": {
                        "type": "string",
                        "description": "Image dimensions such as 1024x1024, 1920x1080, or 1080x1920"
                    }
                },
                "required": ["prompt", "output_path"]
            }
        }
    })
}

/// Candidate file paths a plan names: backticked tokens first, then bare
/// whitespace tokens that look path-like. Existence is checked later, so a
/// command or a symbol never becomes a snapshot.
fn plan_named_paths(plan: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |token: &str| {
        // Leading dots are part of a real filename (.env, .gitignore); only
        // sentence punctuation and quoting are stripped, from the right.
        let token = token.trim();
        let token = token.trim_start_matches(['`', '"', '\'', '(', '[', '{']);
        let token = token.trim_end_matches(|c: char| {
            matches!(
                c,
                '`' | '"' | '\'' | ')' | ']' | '}' | ',' | ':' | ';' | '!' | '?'
            )
        });
        // A sentence-final period is not part of a filename unless the path
        // itself ends in dots (e.g. `..`, which is rejected later anyway).
        let token = token.strip_suffix('.').unwrap_or(token);
        if token.is_empty() || token.len() > 512 {
            return;
        }
        let mut chars = token.chars();
        let hidden_file = chars.next() == Some('.') && {
            let rest: String = chars.collect();
            !rest.is_empty() && !rest.contains(['/', '\\', '.'])
        };
        let path_like = hidden_file
            || token.contains('/')
            || token.contains('\\')
            || token
                .rsplit_once('.')
                .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty() && ext.len() <= 8);
        if path_like && !out.iter().any(|known| known == token) {
            out.push(token.to_string());
        }
    };
    let mut rest = plan;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        push(&after[..end]);
        rest = &after[end + 1..];
    }
    for token in plan.split_whitespace() {
        push(token);
    }
    out
}

/// Rejects UNC/device prefixes and `..` lexically, before `canonicalize` opens
/// the path. On Windows a `\\attacker\share` token would otherwise trigger an
/// SMB connection during approval.
fn plan_path_is_lexically_local(path: &std::path::Path) -> bool {
    use std::path::{Component, Prefix};
    !path.components().any(|component| match component {
        Component::Prefix(prefix) => {
            !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        }
        Component::ParentDir => true,
        _ => false,
    })
}

/// Snapshot every existing file the plan names that resolves inside `cwd`.
/// Returns how many files were recorded; anything outside the workspace or
/// missing is silently skipped, never invented.
fn snapshot_plan_files(
    store: &CheckpointStore,
    cwd: &std::path::Path,
    session_id: &str,
    plan: &str,
) -> usize {
    let Ok(canonical_cwd) = cwd.canonicalize() else {
        return 0;
    };
    let mut saved = 0;
    for candidate in plan_named_paths(plan) {
        let candidate_path = std::path::Path::new(&candidate);
        if !plan_path_is_lexically_local(candidate_path) {
            continue;
        }
        let resolved = if candidate_path.is_absolute() {
            candidate_path.to_path_buf()
        } else {
            cwd.join(candidate_path)
        };
        let Ok(resolved) = resolved.canonicalize() else {
            continue;
        };
        if !resolved.starts_with(&canonical_cwd) || !resolved.is_file() {
            continue;
        }
        if store.snapshot(session_id, &resolved).is_ok() {
            saved += 1;
        }
    }
    saved
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
/// for bash where the *exact* command line is the key. Keying on the first
/// word let one "always" for `git status` approve every later `git …` call.
fn approval_key(tool: &str, args: &serde_json::Value) -> String {
    if tool == "bash" {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        return format!("bash:{command}");
    }
    tool.to_string()
}

/// One-line tool call for the transcript: the tool name plus the argument a
/// human needs (command, path, pattern) instead of the raw JSON object.
pub(crate) fn tool_event_text(tool: &str, args: &serde_json::Value) -> String {
    let field = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let detail = match tool {
        "bash" => field("command").to_string(),
        "read_file" | "write_file" | "edit_file" | "list_dir" => field("path").to_string(),
        "glob_search" | "grep_search" => {
            let pattern = field("pattern");
            let path = field("path");
            match (pattern.is_empty(), path.is_empty()) {
                (false, false) => format!("{pattern}  ·  {path}"),
                (false, true) => pattern.to_string(),
                _ => path.to_string(),
            }
        }
        "tool_search" => field("query").to_string(),
        "memory_append" => field("entry").to_string(),
        "generate_image" => field("output_path").to_string(),
        _ => {
            let keys: Vec<String> = args
                .as_object()
                .map(|map| map.keys().take(3).cloned().collect())
                .unwrap_or_default();
            keys.join(", ")
        }
    };
    let detail = detail.replace(['\n', '\r'], " ");
    let detail: String = detail.chars().take(180).collect();
    if detail.trim().is_empty() {
        tool.to_string()
    } else {
        format!("{tool}  {detail}")
    }
}

/// Human-readable detail for the approval dialog: the file path for
/// write/edit, the command for bash, the entry for memory_append, the
/// argument object for everything else (MCP tools).
fn approval_detail(tool: &str, args: &serde_json::Value) -> String {
    let field = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let detail = match tool {
        "write_file" | "edit_file" => field("path"),
        "generate_image" => field("output_path"),
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
mod catalog_tests {
    use super::Runtime;
    use crate::config::{Config, ProviderProfile};
    use crate::providers::CatalogModel;

    #[test]
    fn configured_catalog_preserves_declared_order_and_deduplicates_live_models() {
        let cfg = Config {
            model: "second/active".into(),
            provider_profiles: vec![
                ProviderProfile {
                    id: "second".into(),
                    models: vec!["z".into(), "y".into()],
                    ..Default::default()
                },
                ProviderProfile {
                    id: "first".into(),
                    models: vec!["a".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let live = vec![
            CatalogModel {
                id: "second/z".into(),
                display_name: Some("live duplicate".into()),
                owned_by: None,
            },
            CatalogModel {
                id: "second/live".into(),
                display_name: Some("Live".into()),
                owned_by: None,
            },
        ];
        let ids = Runtime::configured_catalog(&cfg, live)
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "second/z",
                "second/y",
                "first/a",
                "second/live",
                "second/active"
            ]
        );
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
        /// Tool schema lists every complete() call received.
        tools_seen: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
    }

    impl MockProvider {
        fn completion(text: &str) -> Completion {
            Completion {
                text: text.into(),
                tool_calls: Vec::new(),
                images: Vec::new(),
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
                images: Vec::new(),
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
            tools: &[serde_json::Value],
        ) -> Result<Completion> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.tools_seen.lock().unwrap().push(tools.to_vec());
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
                images: Vec::new(),
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
                images: Vec::new(),
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
                tools_seen: Arc::new(Mutex::new(Vec::new())),
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
            commands: Vec::new(),
            hooks: HookSet::default(),
            active_session: None,
            goal: None,
            // Test bus lives inside the tempdir, never the real ~/.varynth.
            bus: Bus::at(cwd.join("bus.jsonl")),
            approver: None,
            remote_approval_events: None,
            approved_always: std::collections::HashSet::new(),
            rules: RuleSet::default(),
            plan: None,
            plan_capture: false,
            checkpoints: CheckpointStore::at(cwd.join("checkpoints")),
            agent_files: AgentFiles::ensure(cwd).unwrap(),
            notifier: crate::notify::Notifier::from_config(&cfg, crate::notify::Role::Turn),
            mcp: McpManager::from_config(None).unwrap(),
            mcp_exposure: std::collections::HashMap::new(),
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
            side_notes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            provider,
        }
    }

    /// Index a synthetic MCP registry without clients: schemas and the
    /// name→(server, tool) index are exactly what `load_tools` would produce.
    fn mcp_manager(tools: &[(&str, &str, &str)], eager: &[&str]) -> crate::mcp::McpManager {
        let mut manager = crate::mcp::McpManager::default();
        for (server, tool, description) in tools {
            let schema = serde_json::json!({
                "type": "function",
                "function": {
                    "name": crate::mcp::mcp_tool_name(server, tool),
                    "description": description,
                    "parameters": {"type": "object", "properties": {}}
                }
            });
            let name = schema
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            manager
                .tool_index
                .insert(name, (server.to_string(), tool.to_string()));
            manager.tool_schemas.push(schema);
        }
        manager.eager_servers = eager.iter().map(|name| name.to_string()).collect();
        manager
    }

    fn test_runtime_with_mcp(
        cwd: &Path,
        replies: Vec<Result<Completion, String>>,
        mcp: crate::mcp::McpManager,
    ) -> (Runtime, Arc<Mutex<Vec<Vec<serde_json::Value>>>>) {
        let tools_seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = test_runtime_with(
            cwd,
            Box::new(MockProvider {
                replies: Mutex::new(replies.into()),
                seen: Arc::new(Mutex::new(Vec::new())),
                tools_seen: Arc::clone(&tools_seen),
            }),
        );
        runtime.mcp = mcp;
        (runtime, tools_seen)
    }

    fn tool_names(tools: &[serde_json::Value]) -> Vec<String> {
        tools
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(|name| name.as_str())
                    .map(str::to_string)
            })
            .collect()
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

    #[test]
    fn revoked_trust_blocks_reconfigure_from_reloading_project_mcp() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let registry = workspace.path().join(".varynth").join("mcp.json");
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        std::fs::write(
            &registry,
            r#"{"mcpServers":{"project":{"type":"http","url":"https://example.test/mcp"}}}"#,
        )
        .unwrap();
        let _env = crate::config::lock_process_env();
        let previous = std::env::var_os("VARYNTH_HOME");
        std::env::set_var("VARYNTH_HOME", home.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("VARYNTH_HOME", value),
                    None => std::env::remove_var("VARYNTH_HOME"),
                }
            }
        }
        let _restore = Restore(previous);

        crate::trust::trust(workspace.path()).unwrap();
        let (mut rt, _) = test_runtime(workspace.path(), Vec::new());
        let mut cfg = rt.cfg.clone();
        cfg.provider = "varynth".into();
        cfg.mcp_config = Some(registry.clone());
        rt.reconfigure(cfg.clone()).unwrap();
        assert!(rt.mcp.servers.contains_key("project"));

        crate::trust::revoke(workspace.path()).unwrap();
        std::fs::write(
            &registry,
            r#"{"mcpServers":{"project":{"type":"http","url":"https://example.test/mcp"},"later":{"type":"http","url":"https://example.test/later"}}}"#,
        )
        .unwrap();
        let err = rt.reconfigure(cfg).unwrap_err();
        assert!(err.to_string().contains("not trusted"), "{err}");
        assert!(rt.mcp.servers.contains_key("project"));
        assert!(
            !rt.mcp.servers.contains_key("later"),
            "revoked trust must not reload the project registry"
        );

        match rt.with_mcp_config(Some(registry.as_path())) {
            Ok(_) => panic!("with_mcp_config must re-check trust"),
            Err(err) => assert!(err.to_string().contains("not trusted"), "{err}"),
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
    async fn autocompact_fires_at_seventy_six_percent_and_the_command_can_stop_it() {
        let _env = crate::config::lock_process_env();
        let line = compact::auto_compact_threshold(compact::DEFAULT_MESSAGE_BUDGET, 76) as u64;

        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(dir.path(), vec![]);
        rt.last_usage = Some(Usage {
            input_tokens: line - 1,
            output_tokens: 0,
        });
        let mut session = test_session(dir.path());
        for message in history(6) {
            session.append(message).unwrap();
        }
        rt.turn(&mut session, "still under", |_| {}).await.unwrap();
        assert!(session
            .messages
            .iter()
            .any(|message| message.content == "message number 0"));
        assert_eq!(contexts(&seen).len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::completion("compacted at the line"))],
        );
        rt.last_usage = Some(Usage {
            input_tokens: line,
            output_tokens: 0,
        });
        let mut session = test_session(dir.path());
        for message in history(6) {
            session.append(message).unwrap();
        }
        rt.turn(&mut session, "at the line", |_| {}).await.unwrap();
        assert_eq!(session.messages[0].role, "system");
        assert!(session.messages[0]
            .content
            .contains("compacted at the line"));
        assert!(!session
            .messages
            .iter()
            .any(|message| message.content == "message number 0"));
        assert_eq!(contexts(&seen).len(), 2);

        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(dir.path(), vec![]);
        let off = rt
            .apply_autocompact_slash("/autocompact off", None)
            .unwrap();
        assert!(off.contains("off"));
        assert!(!rt.cfg.auto_compact);
        rt.last_usage = Some(Usage {
            input_tokens: line,
            output_tokens: 0,
        });
        let mut session = test_session(dir.path());
        for message in history(6) {
            session.append(message).unwrap();
        }
        rt.turn(&mut session, "disabled", |_| {}).await.unwrap();
        assert!(session
            .messages
            .iter()
            .any(|message| message.content == "message number 0"));
        assert_eq!(contexts(&seen).len(), 1);

        let moved = rt.apply_autocompact_slash("/autocompact 80", None).unwrap();
        assert!(moved.contains("80%"));
        assert!(moved.contains("on"));
        assert_eq!(rt.cfg.auto_compact_percent, 80);
        assert!(rt
            .apply_autocompact_slash("/autocompact later", None)
            .unwrap()
            .contains("Usage"));
        assert!(rt.apply_autocompact_slash("/compact", None).is_none());
        assert_eq!(rt.status_json(None)["autocompact"]["percent"], 80);
    }

    #[tokio::test]
    async fn tool_result_over_the_line_compacts_before_the_next_model_call() {
        let dir = tempfile::tempdir().unwrap();
        let chars = compact::auto_compact_threshold(compact::DEFAULT_MESSAGE_BUDGET, 76) * 4 + 64;
        std::fs::write(dir.path().join("big.txt"), "x".repeat(chars)).unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::tool_call_completion(
                    "1",
                    "read_file",
                    json!({"path": "big.txt", "limit": 1}),
                )),
                Ok(MockProvider::completion("summarized the earlier turns")),
                Ok(MockProvider::completion("done after compact")),
            ],
        );
        let mut session = test_session(dir.path());
        for message in history(6) {
            session.append(message).unwrap();
        }
        let reply = rt
            .turn(&mut session, "read the file", |_| {})
            .await
            .unwrap();
        assert!(reply.contains("done after compact"));
        let calls = contexts(&seen);
        assert_eq!(calls.len(), 3);
        assert!(calls[0]
            .iter()
            .any(|message| message.content == "message number 0"));
        assert!(calls[2]
            .iter()
            .any(|message| message.content.contains("summarized the earlier turns")));
        assert!(!calls[2]
            .iter()
            .any(|message| message.content == "message number 0"));
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
            events.iter().any(|e| e.starts_with("assistant: ")),
            "events: {events:?}"
        );
        // The reply text is the last assistant event; the turn then closes
        // with the finish event the notifier consumes.
        assert_eq!(
            events
                .iter()
                .rev()
                .find_map(|e| e.strip_prefix("assistant: ")),
            Some("Hello world")
        );
        assert_eq!(
            events.last().map(String::as_str),
            Some("turn_finished: finished"),
            "events: {events:?}"
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
    fn undo_two_and_redo_through_the_slash_intercept() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _seen) = test_runtime(dir.path(), vec![]);
        let mut session = test_session(dir.path());
        let s_id = session.id().to_string();
        let file = dir.path().join("code.txt");
        std::fs::write(&file, "v0").unwrap();
        rt.checkpoints.snapshot(&s_id, &file).unwrap();
        std::fs::write(&file, "v1").unwrap();
        rt.checkpoints.snapshot(&s_id, &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        rt.checkpoints.snapshot(&s_id, &file).unwrap();
        std::fs::write(&file, "v3").unwrap();

        let listed = rt.apply_undo_slash("/checkpoints", &mut session).unwrap();
        assert_eq!(listed.lines().count(), 3, "got: {listed}");
        assert!(listed.contains("code.txt"));

        let out = rt.apply_undo_slash("/undo 2", &mut session).unwrap();
        assert_eq!(out.lines().count(), 2, "got: {out}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");

        assert_eq!(
            rt.apply_undo_slash("/redo", &mut session).unwrap(),
            format!("redo: restored {}", file.display())
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");

        // Usage errors stay local; unrelated slashes fall through.
        assert!(rt
            .apply_undo_slash("/undo nope", &mut session)
            .unwrap()
            .starts_with("Usage:"));
        assert!(rt.apply_undo_slash("/status", &mut session).is_none());
    }

    #[tokio::test]
    async fn first_model_call_carries_only_eager_mcp_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = mcp_manager(
            &[
                ("jira", "search_issues", "Search Jira issues by JQL"),
                ("github", "create_pr", "Open a pull request"),
            ],
            &["jira"],
        );
        let (mut rt, tools_seen) =
            test_runtime_with_mcp(dir.path(), vec![Ok(MockProvider::completion("ready"))], mcp);
        let mut session = test_session(dir.path());

        rt.turn(&mut session, "hello", |_| {}).await.unwrap();

        let calls = tools_seen.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let names = tool_names(&calls[0]);
        assert!(names.iter().any(|name| name == "read_file"));
        assert!(names.iter().any(|name| name == "tool_search"));
        assert!(names.iter().any(|name| name == "mcp__jira__search_issues"));
        assert!(
            !names.iter().any(|name| name == "mcp__github__create_pr"),
            "non-eager server must stay deferred"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| name.starts_with("mcp__"))
                .count(),
            1,
            "only the eager server's tool is present"
        );
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "no duplicate schemas");
    }

    #[tokio::test]
    async fn tool_search_promotes_the_matching_tool_for_the_next_step() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = mcp_manager(
            &[
                ("jira", "search_issues", "Search Jira issues by JQL"),
                ("github", "create_pr", "Open a pull request"),
            ],
            &[],
        );
        let (mut rt, tools_seen) = test_runtime_with_mcp(
            dir.path(),
            vec![
                Ok(MockProvider::tool_call_completion(
                    "call_1",
                    "tool_search",
                    serde_json::json!({"query": "jira"}),
                )),
                Ok(MockProvider::tool_call_completion(
                    "call_2",
                    "mcp__jira__search_issues",
                    serde_json::json!({}),
                )),
                Ok(MockProvider::completion("done")),
            ],
            mcp,
        );
        // MCP calls ask in acceptEdits now; this test exercises routing.
        rt.permission = PermissionMode::Bypass;
        let mut session = test_session(dir.path());

        let reply = rt.turn(&mut session, "find jira", |_| {}).await.unwrap();
        assert_eq!(reply, "done");

        let calls = tools_seen.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(
            !tool_names(&calls[0])
                .iter()
                .any(|name| name.starts_with("mcp__")),
            "deferred registry starts without MCP schemas"
        );
        assert!(tool_names(&calls[1])
            .iter()
            .any(|name| name == "mcp__jira__search_issues"));
        assert!(
            !tool_names(&calls[1])
                .iter()
                .any(|name| name == "mcp__github__create_pr"),
            "only searched tools are promoted"
        );
        assert!(
            tool_names(&calls[2])
                .iter()
                .any(|name| name == "mcp__jira__search_issues"),
            "a used tool stays exposed"
        );
        let searched = tool_names(&calls[1])
            .iter()
            .filter(|name| name.as_str() == "mcp__jira__search_issues")
            .count();
        assert_eq!(searched, 1, "no duplicate schema after promotion");
        assert!(
            session.messages.iter().any(|message| {
                message.role == "tool"
                    && message.content.contains("mcp__jira__search_issues")
                    && message.content.contains("\"schema\"")
            }),
            "tool_search result must carry the full schema"
        );
        assert!(
            session.messages.iter().any(|message| {
                message.role == "tool" && message.content.contains("mcp server not found: jira")
            }),
            "the promoted tool is routed to the MCP dispatcher"
        );
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

    fn rules(entries: &[(&str, &str, RuleEffect)]) -> RuleSet {
        RuleSet::from_rules(
            entries
                .iter()
                .map(|(tool, target, effect)| {
                    permissions::PermissionRule::new(tool, target, *effect)
                })
                .collect(),
        )
    }

    #[tokio::test]
    async fn deny_rule_blocks_even_in_bypass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/keep.txt"), "keep").unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::tool_call_completion(
                    "call_1",
                    "bash",
                    serde_json::json!({"command": "rm -rf out"}),
                )),
                Ok(MockProvider::completion("done")),
            ],
        );
        rt.permission = PermissionMode::Bypass;
        rt.set_permission_rules(rules(&[("bash", "rm *", RuleEffect::Deny)]));
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "delete it", event_sink(&events))
            .await
            .unwrap();

        assert!(
            dir.path().join("out/keep.txt").exists(),
            "denied command must not run, even in bypass"
        );
        let tool = session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .expect("denial recorded");
        assert!(
            tool.content.contains("denied by permission rule"),
            "got: {}",
            tool.content
        );
        assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));
    }

    #[tokio::test]
    async fn allow_rule_suppresses_only_the_matching_prompt() {
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
                    serde_json::json!({"path": "notes.md", "content": "two"}),
                )),
                Ok(MockProvider::completion("done")),
            ],
        );
        rt.permission = PermissionMode::Prompt;
        rt.set_permission_rules(rules(&[("write_file", "*.txt", RuleEffect::Allow)]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.approver = Some(tx);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(async move {
            let result = rt
                .turn(&mut session, "write two files", event_sink(&events))
                .await;
            (result, rt, session, events)
        });
        // Only the non-matching path asks.
        let req = rx.recv().await.expect("approval for notes.md");
        assert_eq!(req.tool, "write_file");
        assert_eq!(req.detail, "notes.md");
        req.respond.send(Approval::AllowOnce).unwrap();

        let (result, _rt, _session, events) = handle.await.unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.md")).unwrap(),
            "two"
        );
        assert!(rx.try_recv().is_err(), "the allowed file never prompted");
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("approval_request: "))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn plan_mode_refuses_writes_and_non_allowlisted_bash() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::tool_call_completion(
                    "call_1",
                    "write_file",
                    serde_json::json!({"path": "out.txt", "content": "x"}),
                )),
                Ok(MockProvider::tool_call_completion(
                    "call_2",
                    "bash",
                    serde_json::json!({"command": "rm -rf out"}),
                )),
                Ok(MockProvider::tool_call_completion(
                    "call_3",
                    "mcp__jira__create_issue",
                    serde_json::json!({"summary": "x"}),
                )),
                Ok(MockProvider::completion("done")),
            ],
        );
        rt.set_permission_mode("plan");
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "investigate", event_sink(&events))
            .await
            .unwrap();

        assert!(!dir.path().join("out.txt").exists());
        let denials: Vec<String> = session
            .messages
            .iter()
            .filter(|message| message.role == "tool")
            .map(|message| message.content.clone())
            .collect();
        assert_eq!(denials.len(), 3, "all calls were refused: {denials:?}");
        assert!(denials[0].contains("plan mode") && denials[0].contains("write_file"));
        assert!(denials[1].contains("plan mode") && denials[1].contains("read-only"));
        assert!(
            denials[2].contains("plan mode") && denials[2].contains("read-only"),
            "got: {}",
            denials[2]
        );
        // Read-only investigation still passes the gate.
        assert_eq!(
            rt.gate_decision("bash", &serde_json::json!({"command": "git status"})),
            GateDecision::Allow
        );
    }

    #[test]
    fn tool_event_text_shows_the_argument_not_raw_json() {
        assert_eq!(
            tool_event_text("bash", &serde_json::json!({"command": "npm -v"})),
            "bash  npm -v"
        );
        assert_eq!(
            tool_event_text("read_file", &serde_json::json!({"path": "src/main.rs"})),
            "read_file  src/main.rs"
        );
        let multi = tool_event_text("bash", &serde_json::json!({"command": "a\nb"}));
        assert!(!multi.contains('\n'));
        assert_eq!(
            tool_event_text("tool_search", &serde_json::json!({})),
            "tool_search"
        );
    }

    #[test]
    fn plan_paths_keep_hidden_names_and_reject_escapes() {
        let names =
            plan_named_paths("edit `.env`, `./src/main.rs`, ../escape.txt and ./.gitignore");
        assert!(names.contains(&".env".to_string()), "{names:?}");
        assert!(names.contains(&"./src/main.rs".to_string()), "{names:?}");
        assert!(names.contains(&"./.gitignore".to_string()), "{names:?}");
        assert!(!plan_path_is_lexically_local(std::path::Path::new(
            "../escape.txt"
        )));
        let sentence = plan_named_paths("then edit src/main.rs.");
        assert!(
            sentence.contains(&"src/main.rs".to_string()),
            "{sentence:?}"
        );
        #[cfg(windows)]
        assert!(!plan_path_is_lexically_local(std::path::Path::new(
            r"\\attacker.tld\share\x"
        )));
    }

    #[test]
    fn plan_approval_snapshots_only_existing_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src.txt"), "old").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "no").unwrap();
        let store = CheckpointStore::at(dir.path().join("checkpoints"));
        let plan = format!(
            "edit `src.txt`, create new.txt, and read {}",
            outside.path().join("secret.txt").display()
        );
        assert_eq!(snapshot_plan_files(&store, dir.path(), "s1", &plan), 1);
        let listed = store.list("s1").unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].path.ends_with("src.txt"));
    }

    /// The scripted plan → approve → write flow on one Runtime: the plan turn
    /// produces the artifact, approve switches the mode and the continuation
    /// performs the write.
    #[tokio::test]
    async fn plan_approve_switches_mode_and_performs_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::completion(
                    "## Plan\n1. write out.txt with hello",
                )),
                Ok(MockProvider::tool_call_completion(
                    "call_2",
                    "write_file",
                    serde_json::json!({"path": "out.txt", "content": "hello"}),
                )),
                Ok(MockProvider::completion("wrote it")),
            ],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let plan = rt
            .turn(&mut session, "/plan add out.txt", event_sink(&events))
            .await
            .unwrap();
        assert!(plan.contains("## Plan"));
        assert_eq!(rt.permission, PermissionMode::Plan);
        let artifact = rt.plan.clone().expect("plan artifact");
        assert_eq!(artifact.text, "## Plan\n1. write out.txt with hello");
        assert!(!artifact.approved);
        assert!(!dir.path().join("out.txt").exists());
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("plan: ")));
        // The provider saw the plan instruction in plan mode.
        let first_prompt = seen.lock().unwrap()[0]
            .iter()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap();
        assert!(first_prompt.contains("without changing anything"));

        let continuation = rt
            .turn(&mut session, "/plan approve", event_sink(&events))
            .await
            .unwrap();
        assert_eq!(continuation, "wrote it");
        assert_eq!(rt.permission, PermissionMode::AcceptEdits);
        assert_eq!(rt.cfg.permission_mode, "acceptEdits");
        assert!(rt.plan.as_ref().unwrap().approved);
        assert!(
            session.messages.iter().any(|message| {
                message.role == "system"
                    && message.content.contains("Plan approved")
                    && message.content.contains("write out.txt")
            }),
            "the session carries the plan-approved system note"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn plan_deny_keeps_plan_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::completion("## Plan\n1. do nothing"))],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));
        rt.turn(&mut session, "/plan nothing", event_sink(&events))
            .await
            .unwrap();
        let reply = rt
            .turn(&mut session, "/plan deny", event_sink(&events))
            .await
            .unwrap();
        assert!(reply.contains("plan denied"));
        assert_eq!(rt.permission, PermissionMode::Plan);
        assert!(!rt.plan.as_ref().unwrap().approved);
    }

    #[tokio::test]
    async fn allow_rule_never_relaxes_the_sandbox_path_jail() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("varynth-escape-{}.txt", uuid::Uuid::new_v4());
        let escaped = dir.path().parent().unwrap().join(&name);
        let _ = std::fs::remove_file(&escaped);
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::tool_call_completion(
                    "call_1",
                    "write_file",
                    serde_json::json!({"path": format!("../{name}"), "content": "nope"}),
                )),
                Ok(MockProvider::completion("done")),
            ],
        );
        rt.permission = PermissionMode::Prompt;
        // The rule clears the permission gate; the jail still refuses.
        rt.set_permission_rules(rules(&[("write_file", "", RuleEffect::Allow)]));
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "escape", event_sink(&events))
            .await
            .unwrap();

        assert!(!escaped.exists(), "jail must hold regardless of rules");
        let tool = session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .expect("tool result");
        assert!(
            tool.content.contains("outside the workspace jail"),
            "got: {}",
            tool.content
        );
    }

    #[test]
    fn permissions_report_names_the_mode_rules_and_trust_state() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![]);
        rt.set_permission_mode("plan");
        rt.set_permission_rules(rules(&[("bash", "rm *", RuleEffect::Deny)]));
        let report = rt.permissions_report();
        assert!(report.contains("permission mode: plan"));
        assert!(report.contains("deny  bash rm *"));
    }

    // ---- lifecycle hooks -------------------------------------------------

    fn hook_json(payload: &str) -> String {
        if cfg!(windows) {
            format!("Write-Output '{payload}'")
        } else {
            format!("printf '%s' '{payload}'")
        }
    }

    fn hook_set(hooks: Vec<crate::hooks::Hook>) -> HookSet {
        HookSet::from_hooks(Duration::from_secs(5), hooks)
    }

    fn user_message(session: &Session) -> String {
        session
            .messages
            .iter()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_deny_pre_tool_hook_blocks_bash_even_in_bypass() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("keep.txt");
        std::fs::write(&victim, "keep").unwrap();
        let command = format!("rm -rf \"{}\"", victim.display());
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "bash",
                serde_json::json!({ "command": command }),
            ))],
        );
        rt.permission = PermissionMode::Bypass;
        rt.hooks = hook_set(vec![crate::hooks::Hook::new(
            HookEvent::PreTool,
            hook_json(r#"{"decision":"deny","reason":"rm is blocked here"}"#),
        )]);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "clean up", event_sink(&events))
            .await
            .unwrap();

        assert!(victim.exists(), "a deny hook holds even in bypass");
        let tool = session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .expect("tool result");
        assert!(
            tool.content.contains("denied by hook: rm is blocked here"),
            "got: {}",
            tool.content
        );
    }

    #[tokio::test]
    async fn post_tool_context_is_appended_to_the_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "bash",
                serde_json::json!({"command": "echo hello"}),
            ))],
        );
        // acceptEdits no longer auto-approves bash; this test is about the
        // post-hook decoration, so run without approvals.
        rt.permission = PermissionMode::Bypass;
        rt.hooks = hook_set(vec![crate::hooks::Hook::new(
            HookEvent::PostTool,
            hook_json(r#"{"decision":"allow","context":"formatted by the hook"}"#),
        )]);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "say hello", event_sink(&events))
            .await
            .unwrap();

        let tool = session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .expect("tool result");
        assert!(tool.content.contains("hello"), "got: {}", tool.content);
        assert!(
            tool.content.contains("[hook] formatted by the hook"),
            "got: {}",
            tool.content
        );
    }

    #[tokio::test]
    async fn a_timed_out_pre_tool_hook_fails_closed_without_wedging_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let sleep = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "bash",
                serde_json::json!({"command": "echo still runs"}),
            ))],
        );
        rt.permission = PermissionMode::Bypass;
        rt.hooks = HookSet::from_hooks(
            Duration::from_secs(1),
            vec![crate::hooks::Hook::new(HookEvent::PreTool, sleep)],
        );
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();

        rt.turn(&mut session, "go", event_sink(&events))
            .await
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the turn waited on a hanging hook: {:?}",
            started.elapsed()
        );
        // The guarded call is denied, not silently allowed, and the command
        // never ran.
        let tool = session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .expect("tool result");
        assert!(
            tool.content.contains("denied by hook"),
            "a timed-out pre_tool hook must fail closed: {}",
            tool.content
        );
        assert!(
            !tool.content.contains("still runs"),
            "the guarded command must not run: {}",
            tool.content
        );
    }

    #[tokio::test]
    async fn a_user_prompt_hook_can_cancel_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, seen) = test_runtime(dir.path(), vec![Ok(MockProvider::completion("hi"))]);
        rt.hooks = hook_set(vec![crate::hooks::Hook::new(
            HookEvent::UserPrompt,
            hook_json(r#"{"decision":"deny","reason":"no prompts today"}"#),
        )]);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let reply = rt
            .turn(&mut session, "do something", event_sink(&events))
            .await
            .unwrap();

        assert!(reply.contains("turn cancelled"), "got: {reply}");
        assert!(reply.contains("no prompts today"));
        assert!(
            seen.lock().unwrap().is_empty(),
            "the model was never called"
        );
        assert!(session.messages.is_empty());
    }

    // ---- user-defined commands -------------------------------------------

    fn install_command(workspace: &Path, name: &str, body: &str) {
        let dir = workspace.join(".varynth").join("commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.md")), body).unwrap();
    }

    #[tokio::test]
    async fn a_command_expands_arguments_into_the_same_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![Ok(MockProvider::completion("done"))]);
        install_command(
            dir.path(),
            "reviewcmd",
            "---\ndescription: review\nargument-hint: [scope]\n---\nReview $ARGUMENTS carefully.",
        );
        rt.commands = commands::CommandSet::discover_with(
            dir.path(),
            &dir.path().join("no-user-commands"),
            true,
        )
        .commands;
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "/reviewcmd the parser", event_sink(&events))
            .await
            .unwrap();

        let prompt = user_message(&session);
        assert!(
            prompt.contains("Review the parser carefully."),
            "got: {prompt}"
        );
        assert!(!prompt.contains("/reviewcmd"));
    }

    #[tokio::test]
    async fn a_bang_line_runs_through_the_prompt_gate_and_injects_output() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![Ok(MockProvider::completion("done"))]);
        install_command(
            dir.path(),
            "commitcmd",
            "---\ndescription: commit\n---\n!echo injected-output\n\nWrite a commit for $ARGUMENTS",
        );
        rt.commands = commands::CommandSet::discover_with(
            dir.path(),
            &dir.path().join("no-user-commands"),
            true,
        )
        .commands;
        rt.permission = PermissionMode::Prompt;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.approver = Some(tx);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(async move {
            let result = rt
                .turn(&mut session, "/commitcmd staged", event_sink(&events))
                .await;
            (result, session)
        });
        // A `!` line is a bash call: prompt mode must ask before it runs.
        let request = rx.recv().await.expect("`!` line approval request");
        assert_eq!(request.tool, "bash");
        assert_eq!(request.detail, "echo injected-output");
        request.respond.send(Approval::AllowOnce).unwrap();

        let (result, session) = handle.await.unwrap();
        result.unwrap();
        let prompt = user_message(&session);
        assert!(prompt.contains("injected-output"), "got: {prompt}");
        assert!(
            prompt.contains("Write a commit for staged"),
            "got: {prompt}"
        );
    }

    #[tokio::test]
    async fn a_bang_line_denied_by_the_gate_is_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(dir.path(), vec![Ok(MockProvider::completion("done"))]);
        let marker = dir.path().join("marker.txt");
        install_command(
            dir.path(),
            "markcmd",
            &format!(
                "---\ndescription: marker\n---\n!echo written > \"{}\"",
                marker.display()
            ),
        );
        rt.commands = commands::CommandSet::discover_with(
            dir.path(),
            &dir.path().join("no-user-commands"),
            true,
        )
        .commands;
        rt.set_permission_rules(rules(&[("bash", "", RuleEffect::Deny)]));
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));

        rt.turn(&mut session, "/markcmd", event_sink(&events))
            .await
            .unwrap();

        assert!(!marker.exists());
        let prompt = user_message(&session);
        assert!(prompt.contains("(not run)"), "got: {prompt}");
    }

    /// Scripted demo: `cargo test demo_hooks_and_commands -- --nocapture`
    /// prints the deny/context hook results and one command expansion.
    #[tokio::test]
    async fn demo_hooks_and_commands() {
        let dir = tempfile::tempdir().unwrap();
        let deny = if cfg!(windows) {
            "Write-Output '{\"decision\":\"deny\",\"reason\":\"rm is blocked\"}'"
        } else {
            "printf '%s' '{\"decision\":\"deny\",\"reason\":\"rm is blocked\"}'"
        };
        let context = hook_json(r#"{"decision":"allow","context":"formatted by the hook"}"#);

        // Hook phase: a deny hook wins even in bypass.
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "bash",
                serde_json::json!({"command": "rm -rf out"}),
            ))],
        );
        rt.permission = PermissionMode::Bypass;
        rt.hooks = hook_set(vec![crate::hooks::Hook::new(HookEvent::PreTool, deny)]);
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));
        rt.turn(&mut session, "clean up", event_sink(&events))
            .await
            .unwrap();
        let denial = session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .map(|message| message.content.clone())
            .unwrap();
        println!("[hook] pre_tool bash `rm -rf out` -> {denial}");
        assert!(denial.contains("denied by hook: rm is blocked"));

        // Post-hook phase: context lands on the tool result.
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![Ok(MockProvider::tool_call_completion(
                "call_1",
                "bash",
                serde_json::json!({"command": "echo hi"}),
            ))],
        );
        rt.permission = PermissionMode::Bypass;
        rt.hooks = hook_set(vec![crate::hooks::Hook::new(HookEvent::PostTool, context)]);
        let mut session = test_session(dir.path());
        rt.turn(&mut session, "say hi", event_sink(&events))
            .await
            .unwrap();
        let tool = session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .map(|message| message.content.clone())
            .unwrap();
        println!("[hook] post_tool bash `echo hi` -> {tool}");
        assert!(tool.contains("[hook] formatted by the hook"));

        // Command phase: `/commitdemo staged` runs its `!` line and expands.
        install_command(
            dir.path(),
            "commitdemo",
            "---\ndescription: Write a commit message\nargument-hint: [scope]\n---\n\
             !echo 2 files changed\n\nWrite a commit for $ARGUMENTS",
        );
        let (mut rt, _seen) = test_runtime(dir.path(), vec![Ok(MockProvider::completion("done"))]);
        rt.permission = PermissionMode::Bypass;
        rt.commands = commands::CommandSet::discover_with(
            dir.path(),
            &dir.path().join("no-user-commands"),
            true,
        )
        .commands;
        let mut session = test_session(dir.path());
        rt.turn(&mut session, "/commitdemo staged", event_sink(&events))
            .await
            .unwrap();
        let prompt = user_message(&session);
        println!("[command] /commitdemo staged ->\n{prompt}");
        assert!(prompt.contains("Write a commit for staged"));
        assert!(prompt.contains("2 files changed"));
    }

    #[tokio::test]
    async fn an_untrusted_project_command_never_reaches_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        install_command(
            dir.path(),
            "secretcmd",
            "---\ndescription: secret\n---\nleak secrets",
        );
        let untrusted = commands::CommandSet::discover_with(
            dir.path(),
            &dir.path().join("no-user-commands"),
            false,
        );
        assert!(untrusted.project_pending);
        assert!(untrusted.commands.is_empty());
        let trusted = commands::CommandSet::discover_with(
            dir.path(),
            &dir.path().join("no-user-commands"),
            true,
        );
        assert_eq!(trusted.commands.len(), 1);
    }

    #[test]
    fn an_always_approval_is_scoped_to_the_exact_bash_command() {
        let git_status = approval_key("bash", &serde_json::json!({"command": "git status"}));
        let git_push = approval_key("bash", &serde_json::json!({"command": "git push"}));
        assert_eq!(git_status, "bash:git status");
        assert_ne!(git_status, git_push);
        assert_eq!(
            approval_key("write_file", &serde_json::json!({"path": "a.txt"})),
            "write_file"
        );
    }

    #[test]
    fn accept_edits_asks_for_bash_and_allows_file_edits() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _seen) = test_runtime(dir.path(), vec![]);
        assert_eq!(rt.permission, PermissionMode::AcceptEdits);
        assert_eq!(
            rt.gate_decision("bash", &serde_json::json!({"command": "git status"})),
            GateDecision::Ask,
            "acceptEdits must not auto-approve the shell"
        );
        assert_eq!(
            rt.gate_decision(
                "write_file",
                &serde_json::json!({"path": "a.txt", "content": "x"})
            ),
            GateDecision::Allow
        );
        assert_eq!(
            rt.gate_decision("mcp__jira__get_issue", &serde_json::json!({"id": "1"})),
            GateDecision::Ask,
            "MCP tools are not edits"
        );
    }

    #[tokio::test]
    async fn generate_image_tool_writes_png_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Ok(MockProvider::tool_call_completion(
                    "call_img",
                    "generate_image",
                    serde_json::json!({
                        "prompt": "neon cyberpunk terminal logo",
                        "output_path": "assets/hero.png",
                        "size": "1024x1024"
                    }),
                )),
                Ok(MockProvider::completion("image generated")),
            ],
        );
        rt.permission = PermissionMode::Bypass;
        let mut session = test_session(dir.path());
        let reply = rt
            .turn(&mut session, "create hero image", |_| {})
            .await
            .unwrap();
        assert_eq!(reply, "image generated");
        let target = dir.path().join("assets").join("hero.png");
        assert!(target.exists(), "expected generated PNG at {}", target.display());
        let bytes = std::fs::read(&target).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[tokio::test]
    async fn role_routing_falls_back_when_primary_model_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rt, _seen) = test_runtime(
            dir.path(),
            vec![
                Err("primary rate limited".into()),
                Ok(MockProvider::completion("fallback succeeded")),
            ],
        );
        rt.cfg.stream = Some(false);
        rt.cfg
            .set_role_model(crate::config::ModelRole::Default, Some("primary-model".into()));
        rt.cfg
            .set_role_model(crate::config::ModelRole::Fallback, Some("backup-model".into()));
        let mut session = test_session(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));
        let reply = rt
            .turn(&mut session, "hello", event_sink(&events))
            .await
            .unwrap();
        assert_eq!(reply, "fallback succeeded");
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("retrying with fallback `backup-model`")));
    }
}
