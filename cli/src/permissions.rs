//! Permission modes and persistent, scoped tool rules.
//!
//! Modes decide the default answer for a tool call (`prompt`, `acceptEdits`,
//! `bypass`, `plan`). Rules refine that default per tool and per target:
//!
//! ```json
//! { "rules": [
//!   { "tool": "bash",  "match": "cargo *",     "effect": "allow" },
//!   { "tool": "bash",  "match": "rm *",        "effect": "deny"  },
//!   { "tool": "write_file", "match": "src/**", "effect": "ask"   }
//! ] }
//! ```
//!
//! Evaluation order is deny > plan-mode policy > ask > allow > mode default.
//! A deny rule wins even in `bypass`; a plan-mode policy refusal cannot be
//! lifted by an allow rule. Rules only decide whether a call may run — they
//! never relax [`crate::sandbox::Jail`] path or shell checks.
//!
//! Rule files are read from `~/.varynth/permissions.json` (always) and from
//! `.varynth/permissions.json` in the workspace, which stays inert until the
//! workspace is trusted through [`crate::trust`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Prompt,
    AcceptEdits,
    Bypass,
    /// Investigate and propose only: writes, memory edits, non-read-only
    /// shell commands and mutating MCP tools are refused.
    Plan,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "prompt" | "manual" => Self::Prompt,
            "bypassPermissions" | "bypass" | "dangerously-skip-permissions" => Self::Bypass,
            "plan" => Self::Plan,
            _ => Self::AcceptEdits,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::AcceptEdits => "acceptEdits",
            Self::Bypass => "bypass",
            Self::Plan => "plan",
        }
    }

    /// True when every gated tool call needs an interactive approval.
    pub fn is_prompt(self) -> bool {
        matches!(self, Self::Prompt)
    }

    pub fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
    }
}

/// What a matching rule does with the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleEffect {
    Deny,
    Ask,
    Allow,
}

impl RuleEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Allow => "allow",
        }
    }
}

/// One scoped rule. `tool` is a glob over the tool name (`bash`, `write_file`,
/// `mcp__*`); `match` is a glob over the call target (command, path, or tool
/// name) and empty means "any target".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    #[serde(rename = "match", default)]
    pub target: String,
    pub effect: RuleEffect,
    /// True when the rule came from the user file (including a rule saved by
    /// the approval dialog). Origin is not serialized: it is determined by
    /// which file the rule was read from, never by the file's own content.
    /// Plan-mode MCP admission filters on this flag instead of a count, so a
    /// project rule can never be mistaken for a user decision.
    #[serde(skip)]
    pub user: bool,
}

impl PermissionRule {
    pub fn new(tool: &str, target: &str, effect: RuleEffect) -> Self {
        Self {
            tool: tool.into(),
            target: target.into(),
            effect,
            user: false,
        }
    }

    /// Whether two rules grant the same scope, ignoring where they came from.
    fn same_scope(&self, other: &Self) -> bool {
        self.tool == other.tool && self.target == other.target && self.effect == other.effect
    }

    fn matches(&self, tool: &str, target: &str) -> bool {
        glob_match(&self.tool, tool)
            && (self.target.trim().is_empty() || glob_match(self.target.trim(), target))
    }

    fn describe(&self) -> String {
        let scope = if self.target.trim().is_empty() {
            "*".to_string()
        } else {
            self.target.clone()
        };
        format!("{:5} {} {}", self.effect.as_str(), self.tool, scope)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Ask,
    /// Denied without asking; the string is the message shown to the model.
    Deny(String),
}

/// A loaded rule set plus where its rules came from.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    pub rules: Vec<PermissionRule>,
    /// Rules read from the user file.
    pub user_rules: usize,
    /// Rules read from the trusted project file.
    pub project_rules: usize,
    /// The project file exists but the workspace is untrusted, so it is inert.
    pub project_pending: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RulesFile {
    Wrapped { rules: Vec<PermissionRule> },
    Bare(Vec<PermissionRule>),
}

impl RulesFile {
    fn into_rules(self) -> Vec<PermissionRule> {
        match self {
            Self::Wrapped { rules } | Self::Bare(rules) => rules,
        }
    }
}

impl RuleSet {
    pub fn user_path() -> PathBuf {
        Config::home_dir().join("permissions.json")
    }

    pub fn project_path(workspace: &Path) -> PathBuf {
        workspace.join(".varynth").join("permissions.json")
    }

    /// User rules plus project rules when the workspace is trusted.
    pub fn load(workspace: &Path) -> Self {
        Self::load_with(
            workspace,
            &Self::user_path(),
            crate::trust::is_trusted(workspace),
        )
    }

    /// Explicit-path variant used by tests and embedders. `trusted` is the
    /// project-trust decision; it is ignored when no project file exists.
    pub fn load_with(workspace: &Path, user_path: &Path, trusted: bool) -> Self {
        let mut set = Self::default();
        match read_rules(user_path) {
            Ok(mut rules) => {
                for rule in &mut rules {
                    rule.user = true;
                }
                set.user_rules = rules.len();
                set.rules.extend(rules);
            }
            Err(error) => {
                tracing::warn!(path = %user_path.display(), %error, "ignoring permission rules")
            }
        }
        let project = Self::project_path(workspace);
        if project.is_file() {
            if trusted {
                match read_rules(&project) {
                    Ok(rules) => {
                        set.project_rules = rules.len();
                        set.rules.extend(rules);
                    }
                    Err(error) => {
                        tracing::warn!(path = %project.display(), %error, "ignoring project permission rules")
                    }
                }
            } else {
                set.project_pending = true;
            }
        }
        set
    }

    pub fn from_rules(rules: Vec<PermissionRule>) -> Self {
        let mut rules = rules;
        for rule in &mut rules {
            rule.user = true;
        }
        Self {
            user_rules: rules.len(),
            rules,
            ..Self::default()
        }
    }

    /// Strongest rule matching this call, if any.
    pub fn decision(&self, tool: &str, target: &str) -> Option<RuleEffect> {
        self.decision_scan(tool, std::iter::once(target))
    }

    /// Like [`Self::decision`] but a rule matches when it matches any of the
    /// equivalent spellings of the call target (raw path and jail-normalized
    /// workspace-relative path).
    pub fn decision_multi(&self, tool: &str, targets: &[String]) -> Option<RuleEffect> {
        self.decision_scan(tool, targets.iter().map(String::as_str))
    }

    fn decision_scan<'a>(
        &self,
        tool: &str,
        targets: impl Iterator<Item = &'a str> + Clone,
    ) -> Option<RuleEffect> {
        let mut ask = false;
        let mut allow = false;
        for rule in &self.rules {
            if !targets.clone().any(|target| rule.matches(tool, target)) {
                continue;
            }
            match rule.effect {
                RuleEffect::Deny => return Some(RuleEffect::Deny),
                RuleEffect::Ask => ask = true,
                RuleEffect::Allow => allow = true,
            }
        }
        if ask {
            Some(RuleEffect::Ask)
        } else if allow {
            Some(RuleEffect::Allow)
        } else {
            None
        }
    }

    /// Persist one rule in the user file (deduplicating an identical rule) and
    /// push it into this set. Used by the approval dialog's "always, save
    /// rule" action.
    pub fn add_user_rule(&mut self, rule: PermissionRule) -> Result<()> {
        self.add_user_rule_at(&Self::user_path(), rule)
    }

    pub fn add_user_rule_at(&mut self, path: &Path, rule: PermissionRule) -> Result<()> {
        let existing = read_rules(path)?;
        let mut rule = rule;
        rule.user = true;
        if existing.iter().any(|saved| saved.same_scope(&rule)) {
            if !self.rules.iter().any(|saved| saved.same_scope(&rule)) {
                self.rules.push(rule);
                self.user_rules += 1;
            }
            return Ok(());
        }
        let mut written: Vec<PermissionRule> = existing;
        written.push(rule.clone());
        write_rules(path, &written)?;
        self.rules.push(rule);
        self.user_rules += 1;
        Ok(())
    }

    /// Human-readable mode + rule summary for `/permissions`.
    pub fn report(&self, mode: PermissionMode, workspace: &Path) -> String {
        let mut out = format!(
            "permission mode: {}  ·  {}\n",
            mode.as_str(),
            match mode {
                PermissionMode::Plan => "writes refused; bash limited to read-only commands",
                PermissionMode::Prompt => "every gated call asks",
                PermissionMode::AcceptEdits => "edits auto-approved; shell and MCP calls ask",
                PermissionMode::Bypass => "no approvals (deny rules still apply)",
            }
        );
        out.push_str(&format!(
            "user rules:   {}  ·  {}\n",
            self.user_rules,
            Self::user_path().display()
        ));
        let project = Self::project_path(workspace);
        if self.project_pending {
            out.push_str(&format!(
                "project rules: ignored (workspace not trusted)  ·  {}\n\
                 run `varynth trust` to activate them\n",
                project.display()
            ));
        } else if self.project_rules > 0 {
            out.push_str(&format!(
                "project rules: {}  ·  {}\n",
                self.project_rules,
                project.display()
            ));
        } else {
            out.push_str("project rules: none\n");
        }
        if self.rules.is_empty() {
            out.push_str("rules: none");
        } else {
            out.push_str("rules (deny > plan policy > ask > allow):");
            for rule in &self.rules {
                out.push_str("\n  ");
                out.push_str(&rule.describe());
            }
        }
        out
    }
}

fn read_rules(path: &Path) -> Result<Vec<PermissionRule>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: RulesFile =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let rules = parsed.into_rules();
    for rule in &rules {
        anyhow::ensure!(
            !rule.tool.trim().is_empty(),
            "permission rule in {} has an empty tool",
            path.display()
        );
        // A malformed pattern silently matching nothing is a security trap,
        // so an invalid glob invalidates the file (and is reported).
        glob::Pattern::new(rule.tool.trim())
            .with_context(|| format!("invalid tool glob `{}` in {}", rule.tool, path.display()))?;
        if !rule.target.trim().is_empty() {
            glob::Pattern::new(rule.target.trim()).with_context(|| {
                format!("invalid match glob `{}` in {}", rule.target, path.display())
            })?;
        }
    }
    Ok(rules)
}

fn write_rules(path: &Path, rules: &[PermissionRule]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let raw = serde_json::to_vec_pretty(&serde_json::json!({ "rules": rules }))?;
    crate::control_bus::atomic_write(path, &raw)
}

/// Escape glob metacharacters so an approved command/entry is matched
/// literally. Without this, approving `rg a*b src` once would save a rule
/// that also matches unrelated commands.
pub fn literal_glob(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '*' => out.push_str("[*]"),
            '?' => out.push_str("[?]"),
            '[' => out.push_str("[[]"),
            _ => out.push(ch),
        }
    }
    out
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let options = glob::MatchOptions {
        // Windows paths and tool names are case-insensitive by convention.
        case_sensitive: !cfg!(windows),
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    glob::Pattern::new(pattern)
        .map(|pattern| pattern.matches_with(text, options))
        .unwrap_or(false)
}

/// The target a rule's `match` glob is compared against: the path for file
/// tools, the command for bash, the entry for memory, the tool name otherwise.
pub fn target_for(tool: &str, args: &serde_json::Value) -> String {
    let field = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match tool {
        "write_file" | "edit_file" => {
            let path = field("path");
            if path.is_empty() {
                tool.to_string()
            } else {
                path.to_string()
            }
        }
        "bash" => field("command").to_string(),
        "memory_append" => field("entry").to_string(),
        _ => tool.to_string(),
    }
}

/// Every spelling of a call target that rules must be evaluated against for
/// file tools: the raw argument, the path resolved by the sandbox jail, and
/// the jail-resolved path relative to the workspace. A deny rule written as
/// `.env` therefore also blocks `./.env`, `src/../.env` and an absolute
/// in-workspace path, which all normalize to `.env`.
pub fn rule_targets(
    tool: &str,
    args: &serde_json::Value,
    jail: &crate::sandbox::Jail,
) -> Vec<String> {
    let raw = target_for(tool, args);
    if !matches!(tool, "write_file" | "edit_file") {
        return vec![raw];
    }
    let mut targets = vec![raw.clone()];
    let Ok(resolved) = jail.resolve(&raw) else {
        return targets;
    };
    if let Some(relative) = crate::sandbox::workspace_relative(&jail.cwd, &resolved) {
        let relative = relative.to_string_lossy().replace('\\', "/");
        if !relative.is_empty() {
            targets.push(relative);
        }
    }
    let absolute = resolved.to_string_lossy().replace('\\', "/");
    targets.push(absolute);
    targets
}

/// Tools whose default depends on the permission mode.
pub fn is_gated_tool(tool: &str) -> bool {
    matches!(
        tool,
        "write_file"
            | "edit_file"
            | "memory_append"
            | "bash"
            | "web_fetch"
            | "session_send"
            | "session_inbox"
    ) || is_mcp_tool(tool)
}

pub fn is_mcp_tool(tool: &str) -> bool {
    tool.starts_with("mcp__")
}

/// Whether plan mode may use this MCP tool. A tool name (`read_*`, `get_*`)
/// is server-controlled and proves nothing, so admission requires either the
/// server's explicit `read_only: true` annotation (passed by the runtime) or
/// an exact, wildcard-free `allow` rule in the *user* rule file.
fn plan_mcp_allowed(tool: &str, rules: &RuleSet, server_read_only: bool) -> bool {
    if !is_mcp_tool(tool) {
        return false;
    }
    if server_read_only {
        return true;
    }
    rules.rules.iter().filter(|rule| rule.user).any(|rule| {
        rule.effect == RuleEffect::Allow
            && !rule
                .tool
                .chars()
                .any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
            && rule.tool.eq_ignore_ascii_case(tool)
    })
}

/// Commands plan mode may run. Deliberately narrower than the sandbox shell
/// allowlist, which also admits mutating commands (`rm`, `del`, `curl`).
const PLAN_SHELL_COMMANDS: [&str; 10] = [
    "ls", "dir", "cat", "type", "pwd", "whoami", "where", "rg", "fd", "git",
];

/// Read-only subcommands for `git`; everything else (`commit`, `push`,
/// `checkout`, `branch -D`, ...) mutates.
const PLAN_GIT_SUBCOMMANDS: [&str; 9] = [
    "status",
    "diff",
    "log",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
    "grep",
    "describe",
];

fn has_shell_metacharacters(command: &str) -> bool {
    crate::sandbox::has_shell_metacharacters(command)
}

/// Argument forms that turn a nominally read-only command into an exec or
/// write path. Plan mode validates argv, not just the first word.
fn plan_argv_refusal(base: &str, words: &[String]) -> Option<String> {
    if let Some(reason) = crate::sandbox::exec_flag_refusal(base, words)
        .or_else(|| crate::sandbox::git_forbidden_args(words))
    {
        return Some(reason);
    }
    let args = words.get(1..).unwrap_or(&[]);
    let has = |name: &str| {
        args.iter().any(|arg| {
            let lowered = arg.to_ascii_lowercase();
            lowered == name
                || lowered
                    .split_once('=')
                    .is_some_and(|(head, _)| head == name)
        })
    };
    let prefixed = |prefixes: &[&str]| {
        args.iter().any(|arg| {
            let lowered = arg.to_ascii_lowercase();
            prefixes.iter().any(|prefix| lowered.starts_with(prefix))
        })
    };
    match base {
        // `rg --pre` runs an arbitrary program for every file.
        "rg" => {
            if has("--pre") || has("--pre-glob") || has("--hostname-bin") {
                return Some(format!("`rg {base}` can execute an external program"));
            }
        }
        // `fd -x/-X` execute a command per result.
        "fd" => {
            if prefixed(&["-x", "-X", "--exec"]) {
                return Some("`fd` command execution flags are not read-only".into());
            }
        }
        _ => {}
    }
    // `--output` writes a file from otherwise read-only subcommands
    // (`git log --output=...`, `git diff --output=...`).
    if has("--output") {
        return Some("`--output` writes a file".into());
    }
    None
}

/// Whether one bash command is read-only enough for plan mode.
pub fn plan_shell_allowed(command: &str) -> bool {
    plan_shell_allowed_in(command, None)
}

/// [`plan_shell_allowed`] plus the workspace path jail: an argument that
/// names a path outside the workspace (`type C:\Users\...\.varynth\auth.env`)
/// is not read-only-in-jail and is refused. Plan mode is an approval mode,
/// not a confidentiality sandbox, so this keeps read-only commands inside the
/// tree the user actually opened.
pub fn plan_shell_allowed_in(command: &str, workspace: Option<&Path>) -> bool {
    let command = command.trim();
    if command.is_empty() || has_shell_metacharacters(command) {
        return false;
    }
    let Some(words) = crate::sandbox::split_argv(command) else {
        return false;
    };
    let Some(first) = words.first() else {
        return false;
    };
    if crate::sandbox::qualified_program_refusal(first).is_some() {
        return false;
    }
    let base = crate::sandbox::program_base(first);
    if !PLAN_SHELL_COMMANDS.contains(&base.as_str()) {
        return false;
    }
    if plan_argv_refusal(&base, &words).is_some() {
        return false;
    }
    if !words[1..]
        .iter()
        .all(|word| plan_argument_path_ok(workspace, word))
    {
        return false;
    }
    if base == "git" {
        let Some(subcommand) = words[1..]
            .iter()
            .find(|word| !word.starts_with('-'))
            .map(|word| word.to_ascii_lowercase())
        else {
            return false;
        };
        return PLAN_GIT_SUBCOMMANDS.contains(&subcommand.as_str());
    }
    true
}

/// Read-only arguments may name files, but every path-shaped argument must
/// stay inside the workspace. Values attached to a flag (`--file=x`) are
/// checked as well as bare words.
fn plan_argument_path_ok(workspace: Option<&Path>, word: &str) -> bool {
    let Some(workspace) = workspace else {
        return true;
    };
    let candidate = match word.split_once('=') {
        Some((head, value)) if head.starts_with('-') => value,
        _ => word,
    };
    let path = Path::new(candidate);
    let path_shaped = path.is_absolute()
        || candidate.contains("..")
        || candidate.contains('/')
        || candidate.contains('\\');
    if candidate.is_empty() || !path_shaped {
        return true;
    }
    crate::sandbox::path_within_workspace(workspace, path)
}

/// A plan-mode refusal for this call, if plan mode forbids it outright.
fn plan_refusal(
    tool: &str,
    target: &str,
    rules: &RuleSet,
    mcp_server_read_only: bool,
    workspace: Option<&Path>,
) -> Option<String> {
    if tool.starts_with("computer_") {
        return (tool != "computer_stop").then(|| {
            format!(
                "plan mode: `{tool}` controls the desktop and is disabled; propose it in the plan"
            )
        });
    }
    match tool {
        // Arbitrary egress has no read-only variant: an injected read followed
        // by a fetch is exactly the exfiltration path plan mode must not allow.
        "web_fetch" => Some(
            "plan mode: `web_fetch` is network egress and is disabled while planning".into(),
        ),
        "write_file" | "edit_file" | "memory_append" => Some(format!(
            "plan mode: `{tool}` is disabled. Propose the change in the plan, then `/plan approve`."
        )),
        // `session_send` posts to another session; `session_inbox` drains and
        // clears this one. Neither is a read.
        "session_send" | "session_inbox" => Some(format!(
            "plan mode: `{tool}` changes session state and is disabled while planning"
        )),
        "bash" if !plan_shell_allowed_in(target, workspace) => Some(format!(
            "plan mode: `{target}` is not a read-only in-workspace command. Only {} may run while planning.",
            PLAN_SHELL_COMMANDS.join(", ")
        )),
        tool if is_mcp_tool(tool) && !plan_mcp_allowed(tool, rules, mcp_server_read_only) => {
            Some(format!(
                "plan mode: `{tool}` is not declared read-only by its server and has no exact \
                 allow rule; MCP tools are denied while planning."
            ))
        }
        _ => None,
    }
}

/// One permission decision for a tool call.
///
/// `always_approved` is the session's in-memory "always" answer for this exact
/// call; it ranks below ask/deny rules so a rule stays authoritative.
pub fn evaluate(
    mode: PermissionMode,
    rules: &RuleSet,
    tool: &str,
    target: &str,
    always_approved: bool,
) -> GateDecision {
    evaluate_with(mode, rules, tool, target, always_approved, false)
}

/// [`evaluate`] plus the registry's `read_only: true` annotation for an MCP
/// server. The annotation is the only server-controlled signal plan mode
/// accepts; it is false for non-MCP tools.
pub fn evaluate_with(
    mode: PermissionMode,
    rules: &RuleSet,
    tool: &str,
    target: &str,
    always_approved: bool,
    mcp_server_read_only: bool,
) -> GateDecision {
    evaluate_targets_with(
        mode,
        rules,
        tool,
        &[target.to_string()],
        always_approved,
        mcp_server_read_only,
        None,
    )
}

/// [`evaluate_with`] over every equivalent spelling of the target (see
/// [`rule_targets`]) and with the workspace available to the plan-mode path
/// jail.
pub fn evaluate_targets_with(
    mode: PermissionMode,
    rules: &RuleSet,
    tool: &str,
    targets: &[String],
    always_approved: bool,
    mcp_server_read_only: bool,
    workspace: Option<&Path>,
) -> GateDecision {
    let target = targets.first().map(String::as_str).unwrap_or(tool);
    let matched = rules.decision_multi(tool, targets);
    if matched == Some(RuleEffect::Deny) {
        return GateDecision::Deny(format!(
            "denied by permission rule: `{}` is matched by a deny rule",
            if target.is_empty() { tool } else { target }
        ));
    }
    // Exec-shaped argv (git -c/! aliases, powershell -EncodedCommand, ...) is
    // refused before any allow rule or "always" approval can admit it.
    if tool == "bash" {
        if let Some(reason) = crate::sandbox::command_policy_refusal(target) {
            return GateDecision::Deny(format!("denied by shell policy: {reason}"));
        }
    }
    if mode.is_plan() {
        if let Some(reason) = plan_refusal(tool, target, rules, mcp_server_read_only, workspace) {
            return GateDecision::Deny(reason);
        }
    }
    match matched {
        Some(RuleEffect::Ask) => return GateDecision::Ask,
        Some(RuleEffect::Allow) => return GateDecision::Allow,
        // Deny already returned above; None falls through to the mode.
        Some(RuleEffect::Deny) | None => {}
    }
    if always_approved {
        return GateDecision::Allow;
    }
    default_decision(mode, tool)
}

fn default_decision(mode: PermissionMode, tool: &str) -> GateDecision {
    // Desktop control is never covered by acceptEdits; it asks in prompt and
    // acceptEdits so the documented approval path can actually run, and plan
    // mode refuses it earlier in `plan_refusal`.
    if tool.starts_with("computer_") {
        return if tool == "computer_stop" || mode == PermissionMode::Bypass {
            GateDecision::Allow
        } else if mode.is_plan() {
            GateDecision::Deny(format!("plan mode: `{tool}` controls the desktop"))
        } else {
            GateDecision::Ask
        };
    }
    match mode {
        PermissionMode::Prompt => {
            if is_gated_tool(tool) {
                GateDecision::Ask
            } else {
                GateDecision::Allow
            }
        }
        PermissionMode::AcceptEdits => match tool {
            // The mode's documented promise: edits are auto-approved.
            "write_file" | "edit_file" => GateDecision::Allow,
            // Shell, MCP and memory writes were never covered by that promise;
            // they ask unless a rule/approval already decided.
            tool if is_gated_tool(tool) => GateDecision::Ask,
            _ => GateDecision::Allow,
        },
        PermissionMode::Bypass | PermissionMode::Plan => GateDecision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rule(tool: &str, target: &str, effect: RuleEffect) -> PermissionRule {
        PermissionRule::new(tool, target, effect)
    }

    #[test]
    fn modes_parse_and_round_trip() {
        assert_eq!(PermissionMode::parse("plan"), PermissionMode::Plan);
        assert_eq!(PermissionMode::Plan.as_str(), "plan");
        assert!(!PermissionMode::Plan.is_prompt());
        assert!(PermissionMode::Plan.is_plan());
    }

    #[test]
    fn deny_beats_ask_beats_allow_in_one_set() {
        let set = RuleSet::from_rules(vec![
            rule("bash", "cargo *", RuleEffect::Allow),
            rule("bash", "cargo publish", RuleEffect::Ask),
            rule("bash", "cargo publish --dry-run", RuleEffect::Deny),
        ]);
        assert_eq!(set.decision("bash", "cargo test"), Some(RuleEffect::Allow));
        assert_eq!(set.decision("bash", "cargo publish"), Some(RuleEffect::Ask));
        assert_eq!(
            set.decision("bash", "cargo publish --dry-run"),
            Some(RuleEffect::Deny)
        );
        assert_eq!(set.decision("write_file", "x"), None);
    }

    #[test]
    fn tool_and_target_globs_scope_rules() {
        let set = RuleSet::from_rules(vec![
            rule("mcp__*", "", RuleEffect::Allow),
            rule("write_file", "src/**", RuleEffect::Allow),
        ]);
        assert_eq!(
            set.decision("mcp__jira__issue", "mcp__jira__issue"),
            Some(RuleEffect::Allow)
        );
        assert_eq!(
            set.decision("write_file", "src/lib.rs"),
            Some(RuleEffect::Allow)
        );
        assert_eq!(set.decision("write_file", "README.md"), None);
    }

    #[test]
    fn deny_rule_holds_in_bypass_and_allow_rules_never_lift_plan_mode() {
        let set = RuleSet::from_rules(vec![rule("bash", "rm *", RuleEffect::Deny)]);
        assert!(matches!(
            evaluate(PermissionMode::Bypass, &set, "bash", "rm -rf x", false),
            GateDecision::Deny(_)
        ));
        let allow_all = RuleSet::from_rules(vec![rule("*", "", RuleEffect::Allow)]);
        assert!(matches!(
            evaluate(
                PermissionMode::Plan,
                &allow_all,
                "write_file",
                "x.txt",
                true
            ),
            GateDecision::Deny(_)
        ));
        // And plan mode admits a read-only command.
        assert_eq!(
            evaluate(
                PermissionMode::Plan,
                &allow_all,
                "bash",
                "git status",
                false
            ),
            GateDecision::Allow
        );
    }

    #[test]
    fn ask_rules_force_a_prompt_even_in_accept_edits() {
        let set = RuleSet::from_rules(vec![rule("bash", "curl *", RuleEffect::Ask)]);
        assert_eq!(
            evaluate(
                PermissionMode::AcceptEdits,
                &set,
                "bash",
                "curl https://x",
                false
            ),
            GateDecision::Ask
        );
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, &set, "bash", "ls", false),
            GateDecision::Ask,
            "acceptEdits never auto-approves the shell"
        );
    }

    #[test]
    fn accept_edits_auto_approves_edits_only() {
        let set = RuleSet::default();
        assert_eq!(
            evaluate(
                PermissionMode::AcceptEdits,
                &set,
                "write_file",
                "a.txt",
                false
            ),
            GateDecision::Allow
        );
        assert_eq!(
            evaluate(
                PermissionMode::AcceptEdits,
                &set,
                "edit_file",
                "a.txt",
                false
            ),
            GateDecision::Allow
        );
        for tool in ["bash", "memory_append", "mcp__jira__get_issue"] {
            assert_eq!(
                evaluate(PermissionMode::AcceptEdits, &set, tool, tool, false),
                GateDecision::Ask,
                "{tool} must follow rules/prompt in acceptEdits"
            );
        }
        // Read-only tools stay frictionless.
        assert_eq!(
            evaluate(
                PermissionMode::AcceptEdits,
                &set,
                "read_file",
                "a.txt",
                false
            ),
            GateDecision::Allow
        );
    }

    #[test]
    fn plan_mode_validates_git_arguments_and_exec_flags() {
        // Writing / exec-shaped git options are refused even though the
        // subcommand is nominally read-only.
        for command in [
            "git diff --output=out.txt",
            "git log --output out.txt",
            "git show -Ocalc",
            "git -c alias.p=!calc p",
            "git --exec-path=/tmp p",
            "git grep --ext-diff x",
            "git diff --open-files-in-pager=calc",
        ] {
            assert!(!plan_shell_allowed(command), "{command} must be refused");
        }
        // Exec-shaped flags on otherwise read-only tools.
        assert!(!plan_shell_allowed("rg --pre calc TODO"));
        assert!(!plan_shell_allowed("fd -x calc"));
        assert!(!plan_shell_allowed("fd -X calc"));
        // Plain read-only commands still pass.
        assert!(plan_shell_allowed("git diff HEAD"));
        assert!(plan_shell_allowed("rg TODO src"));
        assert!(plan_shell_allowed("fd -e rs src"));
    }

    #[test]
    fn shell_policy_refuses_exec_flags_and_git_config_injection() {
        let words = |command: &str| crate::sandbox::split_argv(command).unwrap();
        assert!(crate::sandbox::exec_flag_refusal(
            "powershell",
            &words("powershell -EncodedCommand AAAA")
        )
        .is_some());
        assert!(
            crate::sandbox::exec_flag_refusal("python", &words("python -c print(1)")).is_some()
        );
        assert!(crate::sandbox::git_forbidden_args(&words("git -c alias.p=!calc p")).is_some());
        assert!(crate::sandbox::git_forbidden_args(&words("git diff --output=x")).is_some());
        assert!(crate::sandbox::git_forbidden_args(&words("git status")).is_none());
        // An allow rule cannot lift it.
        let allow_all = RuleSet::from_rules(vec![rule("bash", "*", RuleEffect::Allow)]);
        assert!(matches!(
            evaluate(
                PermissionMode::Bypass,
                &allow_all,
                "bash",
                "git -c alias.p=!calc p",
                true
            ),
            GateDecision::Deny(_)
        ));
    }

    #[test]
    fn literal_glob_rules_match_only_the_exact_command() {
        let exact = literal_glob("rg a*b src");
        let set = RuleSet::from_rules(vec![rule("bash", &exact, RuleEffect::Allow)]);
        assert_eq!(set.decision("bash", "rg a*b src"), Some(RuleEffect::Allow));
        assert_eq!(set.decision("bash", "rg axxb src"), None);
    }

    #[test]
    fn plan_shell_allowlist_rejects_mutating_commands_and_chaining() {
        assert!(plan_shell_allowed("ls -la"));
        assert!(plan_shell_allowed("git diff HEAD"));
        assert!(plan_shell_allowed("rg TODO src"));
        assert!(!plan_shell_allowed("rm -rf out"));
        assert!(!plan_shell_allowed("git commit -m x"));
        assert!(!plan_shell_allowed("git push"));
        assert!(!plan_shell_allowed("cat a > b"));
        assert!(!plan_shell_allowed("cargo test"));
    }

    #[test]
    fn plan_mode_refuses_session_send() {
        let empty = RuleSet::default();
        let allow_all = RuleSet::from_rules(vec![rule("*", "", RuleEffect::Allow)]);
        for tool in ["session_send", "session_inbox"] {
            assert!(
                is_gated_tool(tool),
                "{tool} changes session state and must be gated"
            );
            assert!(
                matches!(
                    evaluate(PermissionMode::Plan, &empty, tool, tool, false),
                    GateDecision::Deny(_)
                ),
                "plan mode must refuse {tool}"
            );
            assert!(
                matches!(
                    evaluate(PermissionMode::Plan, &allow_all, tool, "s-other", true),
                    GateDecision::Deny(_)
                ),
                "an allow rule must not lift plan mode for {tool}"
            );
            assert_eq!(
                evaluate(PermissionMode::Prompt, &empty, tool, tool, false),
                GateDecision::Ask
            );
        }
    }

    #[test]
    fn plan_mode_refuses_powershell_grouping() {
        let empty = RuleSet::default();
        for command in ["type (ii evil.bat)", "echo (calc)", "type (cmd /c x)"] {
            assert!(!plan_shell_allowed(command), "{command}");
            let decision = evaluate(PermissionMode::Plan, &empty, "bash", command, false);
            println!("plan denied: {command} -> {decision:?}");
            assert!(
                matches!(decision, GateDecision::Deny(_)),
                "plan mode must deny {command}"
            );
            assert!(
                matches!(
                    evaluate(PermissionMode::Bypass, &empty, "bash", command, true),
                    GateDecision::Deny(_)
                ),
                "shell policy must deny {command}"
            );
        }
    }

    #[test]
    fn plan_mode_never_trusts_mcp_tool_names() {
        // A read-looking name is not a read-only signal: the server controls
        // it, so plan mode refuses until the server is annotated read-only.
        let empty = RuleSet::default();
        for tool in [
            "mcp__jira__get_issue",
            "mcp__fs__read_file",
            "mcp__fs__read_and_delete",
            "mcp__jira__search",
            "mcp__jira__create_issue",
        ] {
            assert!(
                matches!(
                    evaluate_with(PermissionMode::Plan, &empty, tool, tool, false, false),
                    GateDecision::Deny(_)
                ),
                "{tool} must be denied by default in plan mode"
            );
        }

        // A server marked `read_only: true` admits its tools.
        assert_eq!(
            evaluate_with(
                PermissionMode::Plan,
                &empty,
                "mcp__fs__read_file",
                "mcp__fs__read_file",
                false,
                true
            ),
            GateDecision::Allow
        );

        // An exact user allow rule admits exactly that tool, no wildcard.
        let exact = RuleSet::from_rules(vec![rule("mcp__jira__get_issue", "", RuleEffect::Allow)]);
        assert_eq!(
            evaluate(
                PermissionMode::Plan,
                &exact,
                "mcp__jira__get_issue",
                "mcp__jira__get_issue",
                false
            ),
            GateDecision::Allow
        );
        let wildcard = RuleSet::from_rules(vec![rule("mcp__*", "", RuleEffect::Allow)]);
        assert!(matches!(
            evaluate(
                PermissionMode::Plan,
                &wildcard,
                "mcp__jira__read_and_delete",
                "mcp__jira__read_and_delete",
                false
            ),
            GateDecision::Deny(_)
        ));
    }

    #[test]
    fn targets_map_to_command_path_and_tool_name() {
        assert_eq!(
            target_for("bash", &serde_json::json!({"command": "git status"})),
            "git status"
        );
        assert_eq!(
            target_for("edit_file", &serde_json::json!({"path": "src/main.rs"})),
            "src/main.rs"
        );
        assert_eq!(
            target_for("tool_search", &serde_json::json!({})),
            "tool_search"
        );
    }

    #[test]
    fn project_rules_are_inert_until_trusted_and_user_rules_always_load() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".varynth")).unwrap();
        std::fs::write(
            RuleSet::project_path(workspace.path()),
            r#"{"rules":[{"tool":"write_file","match":"*","effect":"allow"}]}"#,
        )
        .unwrap();
        let user = home.path().join("permissions.json");
        std::fs::write(&user, r#"[{"tool":"bash","match":"rm *","effect":"deny"}]"#).unwrap();

        let untrusted = RuleSet::load_with(workspace.path(), &user, false);
        assert!(untrusted.project_pending);
        assert_eq!(untrusted.project_rules, 0);
        assert_eq!(untrusted.user_rules, 1);
        assert_eq!(
            untrusted.decision("write_file", "x.txt"),
            None,
            "a cloned repo cannot activate its allow rule"
        );
        assert!(untrusted
            .report(PermissionMode::Prompt, workspace.path())
            .contains("not trusted"));

        let trusted = RuleSet::load_with(workspace.path(), &user, true);
        assert!(!trusted.project_pending);
        assert_eq!(trusted.project_rules, 1);
        assert_eq!(
            trusted.decision("write_file", "x.txt"),
            Some(RuleEffect::Allow)
        );
        assert_eq!(trusted.decision("bash", "rm -rf x"), Some(RuleEffect::Deny));
    }

    #[test]
    fn adding_a_user_rule_persists_and_deduplicates() {
        let home = tempdir().unwrap();
        let path = home.path().join("permissions.json");
        let mut set = RuleSet::default();
        set.add_user_rule_at(&path, rule("bash", "cargo test", RuleEffect::Allow))
            .unwrap();
        set.add_user_rule_at(&path, rule("bash", "cargo test", RuleEffect::Allow))
            .unwrap();
        assert_eq!(set.user_rules, 1);
        let reloaded = RuleSet::load_with(home.path(), &path, false);
        assert_eq!(reloaded.rules.len(), 1);
        assert_eq!(
            reloaded.decision("bash", "cargo test"),
            Some(RuleEffect::Allow)
        );
    }

    fn workspace_jail(workspace: &Path) -> crate::sandbox::Jail {
        crate::sandbox::Jail::new(
            workspace.to_path_buf(),
            Vec::new(),
            crate::sandbox::SandboxMode::WorkspaceWrite,
            Vec::new(),
        )
    }

    #[test]
    fn deny_rules_match_every_spelling_of_a_workspace_path() {
        let workspace = tempdir().unwrap();
        let cwd = workspace.path();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join(".env"), "SECRET=1").unwrap();
        let jail = workspace_jail(cwd);
        let set = RuleSet::from_rules(vec![rule("write_file", ".env", RuleEffect::Deny)]);
        let absolute = cwd.join(".env");
        for path in ["./.env", "src/../.env", absolute.to_str().unwrap()] {
            let args = serde_json::json!({ "path": path });
            let targets = rule_targets("write_file", &args, &jail);
            assert!(
                matches!(
                    evaluate_targets_with(
                        PermissionMode::Bypass,
                        &set,
                        "write_file",
                        &targets,
                        false,
                        false,
                        Some(cwd)
                    ),
                    GateDecision::Deny(_)
                ),
                "deny .env was bypassed by `{path}` (targets: {targets:?})"
            );
        }
    }

    #[test]
    fn path_qualified_allowlisted_programs_are_denied() {
        let empty = RuleSet::default();
        for command in [r".\git.exe status", r"tools\git.exe status", "C:git status"] {
            for mode in [PermissionMode::AcceptEdits, PermissionMode::Plan] {
                assert!(
                    matches!(
                        evaluate(mode, &empty, "bash", command, false),
                        GateDecision::Deny(_)
                    ),
                    "`{command}` must be denied in {}",
                    mode.as_str()
                );
            }
        }
        assert_eq!(
            evaluate(
                PermissionMode::AcceptEdits,
                &empty,
                "bash",
                "git status",
                false
            ),
            GateDecision::Ask
        );
    }

    #[test]
    fn plan_mode_jails_reads_and_refuses_network_egress() {
        let workspace = tempdir().unwrap();
        let cwd = workspace.path();
        std::fs::write(cwd.join(".env"), "LOCAL=1").unwrap();
        let outside = std::env::temp_dir().join("varynth-plan-outside.env");
        std::fs::write(&outside, "SECRET=2").unwrap();
        let empty = RuleSet::default();
        let gate = |tool: &str, target: &str| {
            evaluate_targets_with(
                PermissionMode::Plan,
                &empty,
                tool,
                &[target.to_string()],
                false,
                false,
                Some(cwd),
            )
        };
        for command in [
            format!("type {}", outside.display()),
            format!("cat {}", outside.display()),
            "type ../../outside.env".to_string(),
            "type src/../../outside.env".to_string(),
        ] {
            assert!(
                matches!(gate("bash", &command), GateDecision::Deny(_)),
                "plan mode read `{command}` must be jailed"
            );
        }
        assert_eq!(gate("bash", "cat .env"), GateDecision::Allow);
        assert_eq!(gate("bash", "git status"), GateDecision::Allow);
        assert!(matches!(
            gate("web_fetch", "web_fetch"),
            GateDecision::Deny(_)
        ));
        assert_eq!(
            evaluate(
                PermissionMode::AcceptEdits,
                &empty,
                "web_fetch",
                "web_fetch",
                false
            ),
            GateDecision::Ask,
            "web_fetch is gated outside plan mode too"
        );
    }

    #[test]
    fn computer_tools_ask_in_prompt_and_accept_edits_but_not_plan() {
        let empty = RuleSet::default();
        for mode in [PermissionMode::Prompt, PermissionMode::AcceptEdits] {
            assert_eq!(
                evaluate(
                    mode,
                    &empty,
                    "computer_screenshot",
                    "computer_screenshot",
                    false
                ),
                GateDecision::Ask,
                "{} must issue an approval request",
                mode.as_str()
            );
        }
        assert!(matches!(
            evaluate(
                PermissionMode::Plan,
                &empty,
                "computer_click",
                "computer_click",
                false
            ),
            GateDecision::Deny(_)
        ));
        assert_eq!(
            evaluate(
                PermissionMode::Prompt,
                &empty,
                "computer_stop",
                "computer_stop",
                false
            ),
            GateDecision::Allow
        );
        let deny = RuleSet::from_rules(vec![rule("computer_*", "", RuleEffect::Deny)]);
        assert!(matches!(
            evaluate(
                PermissionMode::Prompt,
                &deny,
                "computer_click",
                "computer_click",
                false
            ),
            GateDecision::Deny(_)
        ));
    }

    #[test]
    fn saved_user_rules_stay_distinct_from_project_rules() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".varynth")).unwrap();
        std::fs::write(
            RuleSet::project_path(workspace.path()),
            r#"{"rules":[{"tool":"mcp__jira__read","match":"","effect":"allow"}]}"#,
        )
        .unwrap();
        let user = home.path().join("permissions.json");
        let mut set = RuleSet::load_with(workspace.path(), &user, true);
        assert_eq!(set.project_rules, 1);
        set.add_user_rule_at(&user, rule("bash", "ls", RuleEffect::Allow))
            .unwrap();
        assert_eq!(
            set.user_rules, 1,
            "the project rule must not be counted as user-authored"
        );
        assert!(
            !plan_mcp_allowed("mcp__jira__read", &set, false),
            "a project allow rule must not admit an MCP tool in plan mode"
        );
        set.add_user_rule_at(&user, rule("mcp__jira__read", "", RuleEffect::Allow))
            .unwrap();
        assert!(
            plan_mcp_allowed("mcp__jira__read", &set, false),
            "an explicit user allow rule does admit it"
        );
    }
}
