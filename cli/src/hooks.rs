//! Lifecycle hooks: deterministic local policy at the runtime's boundaries.
//!
//! Hooks are ordinary subprocesses — there is no plugin ABI and no in-process
//! extension loader. Each invocation receives one JSON object on stdin:
//!
//! ```json
//! {"event":"pre_tool","tool":"bash","args":{"command":"rm -rf out"},
//!  "cwd":"D:/repo","session_id":"abc"}
//! ```
//!
//! and may answer on stdout with
//!
//! ```json
//! {"decision":"deny","reason":"rm is not allowed here","context":"..."}
//! ```
//!
//! `decision` is `allow`, `deny` or `ask`; `reason` explains a denial and
//! `context` is appended to the tool result (`post_tool`) or the prompt
//! (`user_prompt`). A `pre_tool` hook that exits non-zero denies the call with
//! its stderr as the reason, even in `bypass`. Every invocation is bounded by
//! a hard timeout (default 10 s) so a misbehaving hook can never wedge the
//! TUI. A timed-out `pre_tool` hook fails closed with `deny` (or `ask` when
//! `hooks_timeout_policy = "ask"`); it is never a silent allow.
//!
//! Two files are read, in this order:
//!
//! * `~/.varynth/hooks.json` — always active.
//! * `.varynth/hooks.json` in the workspace — inert until the workspace is
//!   trusted through [`crate::trust`], because a cloned repository must not be
//!   able to run code on open.
//!
//! ```json
//! {
//!   "timeout_secs": 10,
//!   "hooks": {
//!     "pre_tool":   [{ "command": "python tools/guard.py", "tools": ["bash"] }],
//!     "post_tool":  ["python tools/format.py"],
//!     "user_prompt": ["python tools/redact.py"],
//!     "session_start": ["python tools/hello.py"]
//!   }
//! }
//! ```
//!
//! Event keys are `pre_tool`, `post_tool`, `user_prompt`, `session_start` and
//! `session_stop`. A hook entry is either a shell command string or an object
//! with `command`, an optional `timeout_secs`, optional `tools` globs and an
//! optional `env` map of literal values for that child. Hooks never inherit
//! the full parent environment: they receive an allowlist (PATH, HOME/TEMP,
//! SystemRoot, ComSpec, PATHEXT, LANG/LC_*) plus this map.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Timeout applied to one hook invocation when neither the hook nor the file
/// sets one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for a configured timeout; the TUI must stay responsive.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// The runtime boundaries a hook can observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreTool,
    PostTool,
    UserPrompt,
    SessionStart,
    SessionStop,
}

impl HookEvent {
    pub const ALL: [Self; 5] = [
        Self::PreTool,
        Self::PostTool,
        Self::UserPrompt,
        Self::SessionStart,
        Self::SessionStop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::UserPrompt => "user_prompt",
            Self::SessionStart => "session_start",
            Self::SessionStop => "session_stop",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|event| event.as_str() == name.trim().to_ascii_lowercase())
    }
}

/// What one hook (or the merged report) wants to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookDecision {
    Allow,
    Deny,
    Ask,
}

impl Default for HookDecision {
    fn default() -> Self {
        Self::Allow
    }
}

/// One configured hook. `tools` empty means every tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    pub command: String,
    pub timeout: Option<Duration>,
    pub tools: Vec<String>,
    /// Literal environment values for this hook. The child otherwise gets
    /// only Varynth's allowlist environment (never the full parent env).
    pub env: Vec<(String, String)>,
}

impl HookSpec {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout: None,
            tools: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = tools.into_iter().map(Into::into).collect();
        self
    }

    fn applies_to(&self, tool: Option<&str>) -> bool {
        if self.tools.is_empty() {
            return true;
        }
        let Some(tool) = tool else {
            return false;
        };
        self.tools
            .iter()
            .any(|pattern| glob_match(pattern.trim(), tool))
    }

    /// Explicit timeout wins, then the file default.
    fn limit(&self, fallback: Duration) -> Duration {
        self.timeout
            .unwrap_or(fallback)
            .clamp(Duration::from_secs(1), MAX_TIMEOUT)
    }
}

/// A hook bound to its event and origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub event: HookEvent,
    pub spec: HookSpec,
    pub project: bool,
}

impl Hook {
    pub fn new(event: HookEvent, command: impl Into<String>) -> Self {
        Self {
            event,
            spec: HookSpec::new(command),
            project: false,
        }
    }
}

/// Format accepted by one hook entry: a bare command string or an object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawHook {
    Command(String),
    Full {
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        tools: Vec<String>,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
    },
}

impl RawHook {
    fn into_spec(self) -> Option<HookSpec> {
        let (command, timeout_secs, tools, env) = match self {
            Self::Command(command) => (command, None, Vec::new(), Default::default()),
            Self::Full {
                command,
                timeout_secs,
                tools,
                env,
            } => (command, timeout_secs, tools, env),
        };
        if command.trim().is_empty() {
            return None;
        }
        Some(HookSpec {
            command,
            timeout: timeout_secs.map(Duration::from_secs),
            tools,
            env: env.into_iter().collect(),
        })
    }
}

/// What a timed-out `pre_tool` hook asks for. It never silently allows the
/// guarded call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookTimeoutPolicy {
    /// Fail closed: the call is denied.
    #[default]
    Deny,
    /// Fail safe: the call needs interactive approval.
    Ask,
}

impl HookTimeoutPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "deny" => Some(Self::Deny),
            "ask" => Some(Self::Ask),
            _ => None,
        }
    }

    fn decision(self) -> HookDecision {
        match self {
            Self::Deny => HookDecision::Deny,
            Self::Ask => HookDecision::Ask,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    hooks: std::collections::BTreeMap<String, Vec<RawHook>>,
}

/// The loaded hook configuration plus where it came from.
#[derive(Debug, Clone, Default)]
pub struct HookSet {
    pub hooks: Vec<Hook>,
    /// Default per-invocation timeout from the user file or [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
    /// Decision for a timed-out `pre_tool` hook (deny by default).
    pub timeout_policy: HookTimeoutPolicy,
    pub user_hooks: usize,
    pub project_hooks: usize,
    /// The project file exists but the workspace is untrusted, so it is inert.
    pub project_pending: bool,
    /// Human-readable problems found while loading (unknown event, bad JSON).
    pub warnings: Vec<String>,
}

impl HookSet {
    pub fn user_path() -> PathBuf {
        Config::home_dir().join("hooks.json")
    }

    pub fn project_path(workspace: &Path) -> PathBuf {
        workspace.join(".varynth").join("hooks.json")
    }

    /// User hooks plus project hooks when the workspace is trusted.
    pub fn load(workspace: &Path) -> Self {
        Self::load_timed(workspace, DEFAULT_TIMEOUT)
    }

    /// [`Self::load`] with the configured default timeout.
    pub fn load_timed(workspace: &Path, default_timeout: Duration) -> Self {
        Self::load_with(
            workspace,
            &Self::user_path(),
            crate::trust::is_trusted(workspace),
            default_timeout,
        )
    }

    /// Explicit-path variant for tests and embedders. `trusted` is the
    /// project-trust decision; it is ignored when no project file exists.
    pub fn load_with(
        workspace: &Path,
        user_path: &Path,
        trusted: bool,
        default_timeout: Duration,
    ) -> Self {
        let mut set = Self {
            timeout: default_timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT),
            ..Self::default()
        };
        let user = read_file(user_path, &mut set.warnings);
        set.timeout = user
            .timeout_secs
            .map(Duration::from_secs)
            .map(|duration| duration.clamp(Duration::from_secs(1), MAX_TIMEOUT))
            .unwrap_or(set.timeout);
        set.user_hooks = append_hooks(&mut set.hooks, user.hooks, false, "user", &mut set.warnings);
        let project = Self::project_path(workspace);
        if project.is_file() {
            if trusted {
                let parsed = read_file(&project, &mut set.warnings);
                set.project_hooks = append_hooks(
                    &mut set.hooks,
                    parsed.hooks,
                    true,
                    "project",
                    &mut set.warnings,
                );
            } else {
                set.project_pending = true;
            }
        }
        set
    }

    /// Build a set directly (tests, embedders). Files are not touched.
    pub fn from_hooks(timeout: Duration, hooks: Vec<Hook>) -> Self {
        let user_hooks = hooks.iter().filter(|hook| !hook.project).count();
        let project_hooks = hooks.len() - user_hooks;
        Self {
            hooks,
            timeout: timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT),
            timeout_policy: HookTimeoutPolicy::default(),
            user_hooks,
            project_hooks,
            project_pending: false,
            warnings: Vec::new(),
        }
    }

    /// Override the timeout decision (from `hooks_timeout_policy`). A value
    /// other than deny/ask is ignored with a warning.
    pub fn with_timeout_policy(mut self, policy: &str) -> Self {
        match HookTimeoutPolicy::parse(policy) {
            Some(parsed) => self.timeout_policy = parsed,
            None => self.warnings.push(format!(
                "ignoring hooks_timeout_policy `{policy}`: expected deny or ask"
            )),
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn for_event(&self, event: HookEvent) -> impl Iterator<Item = &Hook> {
        self.hooks.iter().filter(move |hook| hook.event == event)
    }

    /// Run every hook subscribed to `event` and merge the answers.
    ///
    /// Hooks run sequentially in file order, so a deny always wins and a
    /// later hook cannot undo it. Every invocation is timeout-bounded; a
    /// malformed hook contributes a warning. A timed-out `pre_tool` hook
    /// fails closed using [`Self::timeout_policy`] instead of silently
    /// allowing the guarded call.
    pub async fn run(
        &self,
        event: HookEvent,
        tool: Option<&str>,
        args: Option<&serde_json::Value>,
        cwd: &Path,
        session_id: &str,
    ) -> HookReport {
        let mut report = HookReport::default();
        if self.hooks.is_empty() {
            return report;
        }
        let payload = serde_json::json!({
            "event": event.as_str(),
            "tool": tool,
            "args": args,
            "cwd": cwd.display().to_string(),
            "session_id": session_id,
        });
        for hook in self.hooks.iter().filter(|hook| hook.event == event) {
            if !hook.spec.applies_to(tool) {
                continue;
            }
            report.ran += 1;
            let timeout_decision = if event == HookEvent::PreTool {
                self.timeout_policy.decision()
            } else {
                HookDecision::Allow
            };
            match run_one(
                &hook.spec,
                &payload,
                cwd,
                hook.spec.limit(self.timeout),
                timeout_decision,
            )
            .await
            {
                Ok(answer) => report.absorb(answer, event, &hook.spec, &mut |line| {
                    tracing::warn!(event = event.as_str(), "{line}");
                }),
                Err(error) => {
                    let line = format!(
                        "hook `{}` for {} could not run: {error}",
                        hook.spec.command,
                        event.as_str()
                    );
                    tracing::warn!("{line}");
                    report.failed.push(line);
                }
            }
        }
        report
    }
}

/// One parsed hook answer before merging.
#[derive(Debug, Clone, Default)]
pub struct HookAnswer {
    pub decision: HookDecision,
    pub reason: Option<String>,
    pub context: Option<String>,
    /// Non-zero exit status or an unparseable stdout payload.
    pub warning: Option<String>,
    /// Stderr text of a failed hook, used as the denial reason.
    pub stderr: Option<String>,
    /// Exit status when non-zero.
    pub exit_failed: bool,
}

/// Merged answers for one event.
#[derive(Debug, Clone, Default)]
pub struct HookReport {
    /// `Deny` when any hook denied, then `Ask`, then `Allow`.
    pub decision: HookDecision,
    pub reasons: Vec<String>,
    pub contexts: Vec<String>,
    /// Number of hooks that actually ran (subscribed and matching the tool).
    pub ran: usize,
    /// Timeouts, spawn failures and malformed output. Never a decision.
    pub failed: Vec<String>,
}

impl HookReport {
    fn absorb(
        &mut self,
        mut answer: HookAnswer,
        event: HookEvent,
        spec: &HookSpec,
        warn: &mut impl FnMut(String),
    ) {
        // Spec: a non-zero `pre_tool` exit is a denial with stderr as the
        // reason. Other events treat it as a warning only.
        if event == HookEvent::PreTool
            && answer.exit_failed
            && answer.decision != HookDecision::Deny
        {
            answer.decision = HookDecision::Deny;
        }
        let exit_note = answer.warning.clone();
        if let Some(warning) = answer.warning.take() {
            let line = format!("hook `{}` for {}: {warning}", spec.command, event.as_str());
            warn(line.clone());
            self.failed.push(line);
        }
        let reason = answer.reason.take().or_else(|| {
            if answer.exit_failed {
                answer
                    .stderr
                    .take()
                    .filter(|text| !text.trim().is_empty())
                    .or(exit_note)
            } else {
                None
            }
        });
        match answer.decision {
            HookDecision::Deny => {
                self.decision = HookDecision::Deny;
                self.reasons
                    .push(reason.unwrap_or_else(|| format!("denied by hook `{}`", spec.command)));
            }
            HookDecision::Ask if self.decision != HookDecision::Deny => {
                self.decision = HookDecision::Ask;
                if let Some(reason) = reason {
                    self.reasons.push(reason);
                }
            }
            HookDecision::Allow | HookDecision::Ask => {
                if let Some(reason) = reason {
                    self.reasons.push(reason);
                }
            }
        }
        if let Some(context) = answer.context.filter(|text| !text.trim().is_empty()) {
            self.contexts.push(context);
        }
    }

    /// The message for a blocked call, if a hook denied it.
    pub fn denial(&self) -> Option<String> {
        if self.decision != HookDecision::Deny {
            return None;
        }
        let detail = if self.reasons.is_empty() {
            "blocked by a lifecycle hook".to_string()
        } else {
            self.reasons.join("; ")
        };
        Some(format!("denied by hook: {detail}"))
    }

    /// Whether the hook demands approval even where the mode would not ask.
    pub fn wants_ask(&self) -> bool {
        self.decision == HookDecision::Ask
    }

    /// Append every `context` to a tool result (or prompt).
    pub fn decorate(&self, text: String) -> String {
        if self.contexts.is_empty() {
            return text;
        }
        let mut out = text;
        for context in &self.contexts {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("[hook] ");
            out.push_str(context.trim_end());
        }
        out
    }

    /// One-line summary for the TUI/system event stream.
    pub fn summary(&self) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        Some(self.failed.join("; "))
    }
}

/// Run one hook subprocess with a bounded wait.
async fn run_one(
    spec: &HookSpec,
    payload: &serde_json::Value,
    cwd: &Path,
    timeout: Duration,
    timeout_decision: HookDecision,
) -> Result<HookAnswer> {
    let mut command = shell_command(&spec.command)?;
    command
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn hook `{}`", spec.command))?;
    let mut stdin = child.stdin.take();
    let input = serde_json::to_vec(payload)?;
    let waited = tokio::time::timeout(timeout, async move {
        if let Some(mut stdin) = stdin.take() {
            use tokio::io::AsyncWriteExt;
            // A hook that never reads stdin still gets the payload; a broken
            // pipe is the child's choice and must not fail the invocation.
            let _ = stdin.write_all(&input).await;
            // Dropping the handle closes the pipe: the child needs EOF even
            // though every hook call writes exactly one payload.
            drop(stdin);
        }
        child.wait_with_output().await
    })
    .await;
    match waited {
        Ok(Ok(output)) => {
            let mut answer = parse_output(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !output.status.success() {
                answer.exit_failed = true;
                answer.warning = Some(format!("exited with {}", output.status));
                answer.stderr = (!stderr.is_empty()).then_some(stderr);
            } else if answer.warning.is_none() && !stderr.is_empty() {
                answer.stderr = Some(stderr);
            }
            Ok(answer)
        }
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Ok(HookAnswer {
            // A timeout is never a silent allow for a guard: the caller
            // passes deny (default) or ask for `pre_tool`; other events
            // cannot block and keep the warning-only answer.
            decision: timeout_decision,
            reason: Some(format!(
                "hook timed out after {}s; failing closed instead of allowing the call",
                timeout.as_secs_f64()
            )),
            warning: Some(format!(
                "timed out after {}s (fails closed: {})",
                timeout.as_secs_f64(),
                match timeout_decision {
                    HookDecision::Deny => "deny",
                    HookDecision::Ask => "ask",
                    HookDecision::Allow => "allow",
                }
            )),
            ..HookAnswer::default()
        }),
    }
}

fn parse_output(stdout: &[u8]) -> HookAnswer {
    let text = String::from_utf8_lossy(stdout);
    let text = text.trim();
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) if text.starts_with('{') => {
            let decision = match value.get("decision") {
                None => HookDecision::Allow,
                Some(serde_json::Value::String(name)) => match name.trim() {
                    "allow" => HookDecision::Allow,
                    "deny" => HookDecision::Deny,
                    "ask" => HookDecision::Ask,
                    other => {
                        return HookAnswer {
                            decision: HookDecision::Allow,
                            warning: Some(format!(
                                "ignored invalid decision `{other}` (expected allow, deny or ask)"
                            )),
                            ..HookAnswer::default()
                        }
                    }
                },
                Some(other) => {
                    return HookAnswer {
                        decision: HookDecision::Allow,
                        warning: Some(format!("ignored non-string decision {other}")),
                        ..HookAnswer::default()
                    }
                }
            };
            HookAnswer {
                decision,
                reason: value
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                context: value
                    .get("context")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                ..HookAnswer::default()
            }
        }
        _ if text.is_empty() => HookAnswer::default(),
        _ => HookAnswer {
            decision: HookDecision::Allow,
            warning: Some("stdout is not a JSON object; ignored".into()),
            ..HookAnswer::default()
        },
    }
}

fn read_file(path: &Path, warnings: &mut Vec<String>) -> HooksFile {
    if !path.is_file() {
        return HooksFile::default();
    }
    let parsed = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))
        .and_then(|raw| serde_json::from_str::<HooksFile>(&raw).map_err(Into::into));
    match parsed {
        Ok(file) => file,
        Err(error) => {
            warnings.push(format!("ignoring {}: {error}", path.display()));
            HooksFile::default()
        }
    }
}

fn append_hooks(
    out: &mut Vec<Hook>,
    entries: std::collections::BTreeMap<String, Vec<RawHook>>,
    project: bool,
    origin: &str,
    warnings: &mut Vec<String>,
) -> usize {
    let mut count = 0;
    for (name, raw) in entries {
        let Some(event) = HookEvent::parse(&name) else {
            warnings.push(format!(
                "ignoring {origin} hook group `{name}`: unknown event (expected {})",
                HookEvent::ALL
                    .iter()
                    .map(|event| event.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        };
        for raw in raw {
            match raw.into_spec() {
                Some(spec) => {
                    out.push(Hook {
                        event,
                        spec,
                        project,
                    });
                    count += 1;
                }
                None => warnings.push(format!(
                    "ignoring {origin} {} hook: empty command",
                    event.as_str()
                )),
            }
        }
    }
    count
}

/// A user-authored hook runs through the platform shell, like the bash tool.
/// The shell is resolved to an absolute system path and the child receives
/// only Varynth's allowlist environment (plus the hook's own `env` map).
fn shell_command(command: &str) -> Result<tokio::process::Command> {
    let (program, args) = crate::sandbox::resolve_host_shell(command)?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    cmd.envs(crate::sandbox::child_env());
    Ok(cmd)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let options = glob::MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    glob::Pattern::new(pattern)
        .map(|pattern| pattern.matches_with(text, options))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_hooks(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn json_hook(body: &str) -> String {
        if cfg!(windows) {
            format!("Write-Output '{}'", body.replace('\'', ""))
        } else {
            format!("printf '%s' '{}'", body.replace('\'', ""))
        }
    }

    #[test]
    fn events_parse_and_round_trip() {
        for event in HookEvent::ALL {
            assert_eq!(HookEvent::parse(event.as_str()), Some(event));
        }
        assert_eq!(HookEvent::parse("PRE_TOOL"), Some(HookEvent::PreTool));
        assert_eq!(HookEvent::parse("nope"), None);
    }

    #[test]
    fn user_hooks_load_and_project_hooks_wait_for_trust() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let user = home.path().join("hooks.json");
        write_hooks(
            &user,
            r#"{"hooks":{"pre_tool":[{"command":"guard","tools":["bash"]}]}}"#,
        );
        write_hooks(
            &HookSet::project_path(workspace.path()),
            r#"{"hooks":{"post_tool":["format"]}}"#,
        );

        let untrusted = HookSet::load_with(workspace.path(), &user, false, DEFAULT_TIMEOUT);
        assert!(untrusted.project_pending);
        assert_eq!(untrusted.user_hooks, 1);
        assert_eq!(untrusted.project_hooks, 0);
        assert_eq!(untrusted.hooks.len(), 1);
        assert_eq!(untrusted.hooks[0].spec.command, "guard");
        assert_eq!(untrusted.hooks[0].spec.tools, vec!["bash"]);

        let trusted = HookSet::load_with(workspace.path(), &user, true, DEFAULT_TIMEOUT);
        assert!(!trusted.project_pending);
        assert_eq!(trusted.hooks.len(), 2);
        assert!(trusted.hooks[1].project);
        assert_eq!(trusted.hooks[1].event, HookEvent::PostTool);
    }

    #[test]
    fn unknown_events_and_empty_commands_are_reported_not_fatal() {
        let home = tempdir().unwrap();
        let user = home.path().join("hooks.json");
        write_hooks(
            &user,
            r#"{"hooks":{"pre_typo":["x"],"post_tool":["  "],"user_prompt":["ok"]}}"#,
        );
        let set = HookSet::load_with(home.path(), &user, false, DEFAULT_TIMEOUT);
        assert_eq!(set.hooks.len(), 1);
        assert_eq!(set.warnings.len(), 2);
        assert!(set.warnings.iter().any(|w| w.contains("pre_typo")));
        assert!(set.warnings.iter().any(|w| w.contains("empty command")));
    }

    #[test]
    fn tool_globs_scope_a_hook() {
        let hook = HookSpec::new("guard").with_tools(["bash", "write_*"]);
        assert!(hook.applies_to(Some("bash")));
        assert!(hook.applies_to(Some("write_file")));
        assert!(!hook.applies_to(Some("read_file")));
        assert!(!hook.applies_to(None));
        assert!(HookSpec::new("all").applies_to(None));
    }

    #[test]
    fn a_file_timeout_is_clamped_and_used_as_the_default() {
        let home = tempdir().unwrap();
        let user = home.path().join("hooks.json");
        write_hooks(&user, r#"{"timeout_secs":0,"hooks":{}}"#);
        let set = HookSet::load_with(home.path(), &user, false, DEFAULT_TIMEOUT);
        assert_eq!(set.timeout, Duration::from_secs(1));
        write_hooks(&user, r#"{"timeout_secs":99999,"hooks":{}}"#);
        let set = HookSet::load_with(home.path(), &user, false, DEFAULT_TIMEOUT);
        assert_eq!(set.timeout, MAX_TIMEOUT);
    }

    #[tokio::test]
    async fn deny_wins_over_allow_and_context_is_collected() {
        let set = HookSet::from_hooks(
            Duration::from_secs(5),
            vec![
                Hook::new(HookEvent::PreTool, json_hook(r#"{"decision":"allow"}"#)),
                Hook::new(
                    HookEvent::PreTool,
                    json_hook(r#"{"decision":"deny","reason":"no rm"}"#),
                ),
                Hook::new(
                    HookEvent::PreTool,
                    json_hook(r#"{"decision":"allow","context":"checked"}"#),
                ),
            ],
        );
        let report = set
            .run(
                HookEvent::PreTool,
                Some("bash"),
                Some(&serde_json::json!({"command":"rm -rf out"})),
                Path::new("."),
                "s1",
            )
            .await;
        assert_eq!(report.ran, 3);
        assert_eq!(report.decision, HookDecision::Deny);
        assert_eq!(report.denial().unwrap(), "denied by hook: no rm");
        assert_eq!(report.contexts, vec!["checked"]);
        assert!(report.failed.is_empty());
    }

    #[tokio::test]
    async fn a_nonzero_pre_tool_exit_denies_with_stderr() {
        let command = if cfg!(windows) {
            "Write-Error 'nope'; exit 3"
        } else {
            "echo nope >&2; exit 3"
        };
        let set = HookSet::from_hooks(
            Duration::from_secs(5),
            vec![Hook::new(HookEvent::PreTool, command)],
        );
        let report = set
            .run(HookEvent::PreTool, Some("bash"), None, Path::new("."), "s1")
            .await;
        assert_eq!(report.decision, HookDecision::Deny);
        assert!(report.denial().unwrap().contains("nope"));
    }

    #[tokio::test]
    async fn malformed_output_never_denies() {
        let set = HookSet::from_hooks(
            Duration::from_secs(5),
            vec![Hook::new(HookEvent::PostTool, "echo not-json")],
        );
        let report = set
            .run(
                HookEvent::PostTool,
                Some("bash"),
                None,
                Path::new("."),
                "s1",
            )
            .await;
        assert_eq!(report.decision, HookDecision::Allow);
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].contains("not a JSON object"));
    }

    #[tokio::test]
    async fn a_hanging_hook_times_out_instead_of_waiting() {
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let set = HookSet::from_hooks(
            Duration::from_secs(1),
            vec![Hook::new(HookEvent::PreTool, command)],
        );
        let started = std::time::Instant::now();
        let report = set
            .run(HookEvent::PreTool, Some("bash"), None, Path::new("."), "s1")
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "hook wait was not bounded: {:?}",
            started.elapsed()
        );
        // A timed-out guard fails closed by default.
        assert_eq!(report.decision, HookDecision::Deny);
        assert!(report.denial().unwrap().contains("timed out"));
        assert_eq!(report.failed.len(), 1);

        // The configurable policy can ask instead, but never silently allow.
        let ask = HookSet::from_hooks(
            Duration::from_secs(1),
            vec![Hook::new(HookEvent::PreTool, command)],
        )
        .with_timeout_policy("ask");
        let report = ask
            .run(HookEvent::PreTool, Some("bash"), None, Path::new("."), "s1")
            .await;
        assert_eq!(report.decision, HookDecision::Ask);
    }

    #[tokio::test]
    async fn non_guard_events_time_out_with_a_warning_only() {
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let set = HookSet::from_hooks(
            Duration::from_secs(1),
            vec![Hook::new(HookEvent::PostTool, command)],
        );
        let report = set
            .run(
                HookEvent::PostTool,
                Some("bash"),
                None,
                Path::new("."),
                "s1",
            )
            .await;
        assert_eq!(report.decision, HookDecision::Allow);
        assert!(report.failed[0].contains("timed out"));
    }

    #[test]
    fn hook_env_map_is_parsed_and_applied() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        write_hooks(
            &path,
            r#"{"hooks":{"post_tool":[{"command":"echo ok","env":{"HOOK_VALUE":"literal"}}]}}"#,
        );
        let set = HookSet::load_with(dir.path(), &path, false, DEFAULT_TIMEOUT);
        assert_eq!(
            set.hooks[0].spec.env,
            vec![("HOOK_VALUE".to_string(), "literal".to_string())]
        );
        assert_eq!(
            set.hooks[0].spec.env.len(),
            1,
            "no inherited environment is added to the spec"
        );
    }

    #[tokio::test]
    async fn stdin_carries_the_expected_payload() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("payload.json");
        let command = if cfg!(windows) {
            format!(
                "$c = $input | Out-String; [IO.File]::WriteAllText('{}', $c)",
                out.display()
            )
        } else {
            format!("cat > '{}'", out.display())
        };
        let set = HookSet::from_hooks(
            Duration::from_secs(5),
            vec![Hook::new(HookEvent::UserPrompt, command)],
        );
        let report = set
            .run(HookEvent::UserPrompt, None, None, dir.path(), "session-42")
            .await;
        let raw = std::fs::read_to_string(&out).unwrap_or_default();
        // The PowerShell pipeline may add a newline; the JSON stays.
        assert!(
            raw.contains("\"event\""),
            "payload: {raw}  exists={}  failed={:?}",
            out.exists(),
            report.failed
        );
        assert!(raw.contains("\"session_id\""), "payload: {raw}");
        assert!(raw.contains("session-42"), "payload: {raw}");
    }
}
