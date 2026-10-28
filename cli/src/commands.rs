//! User-defined slash commands: parameterized prompt files.
//!
//! A command is a markdown file next to skills, in one of two places:
//!
//! * `~/.varynth/commands/<name>.md` — always active.
//! * `.varynth/commands/<name>.md` in the workspace — inert until the
//!   workspace is trusted through [`crate::trust`], because a cloned
//!   repository could otherwise ship a prompt (and shell lines) that run on
//!   invocation.
//!
//! ```markdown
//! ---
//! description: Write a commit message from the staged diff
//! argument-hint: [scope]
//! ---
//!
//! !git diff --cached
//!
//! Write a conventional commit message for the diff above. Extra context:
//! $ARGUMENTS
//! ```
//!
//! `description` is required; a file without one is ignored with a warning.
//! `$ARGUMENTS` is replaced with the raw text typed after the command name.
//! A line whose first non-space character is `!` is a shell step: the runtime
//! executes it through the existing `bash` tool and permission gate (so it can
//! prompt, and hooks/rules still apply) and injects the output in place.
//!
//! Invoking a command expands to a user message in the same turn loop; it is
//! not a new message or turn type. Built-in slash commands always win, so a
//! file may never shadow `/help`, `/model`, ... .

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;

/// One discovered command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Slash name (file stem), as typed.
    pub name: String,
    pub description: String,
    /// Optional usage hint from frontmatter, e.g. `[scope]`.
    pub argument_hint: Option<String>,
    /// Markdown body without the frontmatter block.
    pub body: String,
    pub path: PathBuf,
    /// True when the file came from the trusted project directory.
    pub project: bool,
}

/// The configured command surface plus where it came from.
#[derive(Debug, Clone, Default)]
pub struct CommandSet {
    pub commands: Vec<Command>,
    pub user_commands: usize,
    pub project_commands: usize,
    /// The project directory exists but the workspace is untrusted.
    pub project_pending: bool,
    /// Files that were skipped (missing description, bad name, unreadable).
    pub warnings: Vec<String>,
}

impl CommandSet {
    pub fn user_dir() -> PathBuf {
        Config::home_dir().join("commands")
    }

    pub fn project_dir(workspace: &Path) -> PathBuf {
        workspace.join(".varynth").join("commands")
    }

    /// User commands plus project commands when the workspace is trusted.
    pub fn discover(workspace: &Path) -> Self {
        Self::discover_with(
            workspace,
            &Self::user_dir(),
            crate::trust::is_trusted(workspace),
        )
    }

    /// Explicit-path variant for tests and embedders. `trusted` is the
    /// project-trust decision; it is ignored when the project directory is
    /// absent.
    pub fn discover_with(workspace: &Path, user_dir: &Path, trusted: bool) -> Self {
        let mut set = Self::default();
        let mut loaded = Vec::new();
        load_dir(user_dir, false, &mut loaded, &mut set.warnings);
        set.user_commands = loaded.len();
        let project = Self::project_dir(workspace);
        if project.is_dir() {
            if trusted {
                let before = loaded.len();
                load_dir(&project, true, &mut loaded, &mut set.warnings);
                set.project_commands = loaded.len() - before;
            } else {
                set.project_pending = true;
            }
        }
        // A project command with the same name replaces the user command.
        for command in loaded {
            match set
                .commands
                .iter()
                .position(|existing| existing.name.eq_ignore_ascii_case(&command.name))
            {
                Some(index) => set.commands[index] = command,
                None => set.commands.push(command),
            }
        }
        set.commands.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    pub fn get(&self, name: &str) -> Option<&Command> {
        self.commands
            .iter()
            .find(|command| command.name.eq_ignore_ascii_case(name))
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

fn load_dir(dir: &Path, project: bool, out: &mut Vec<Command>, warnings: &mut Vec<String>) {
    if !dir.is_dir() {
        return;
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    for path in files {
        match parse_file(&path, project) {
            Ok(Some(command)) => {
                if crate::composer::is_builtin(&command.name) {
                    warnings.push(format!(
                        "ignoring command `{}` ({}): it shadows a built-in slash command",
                        command.name,
                        path.display()
                    ));
                    continue;
                }
                out.push(command);
            }
            Ok(None) => warnings.push(format!(
                "ignoring command {}: frontmatter `description` is required",
                path.display()
            )),
            Err(error) => warnings.push(format!("ignoring command {}: {error}", path.display())),
        }
    }
}

/// Parse one command file. `Ok(None)` means it is a valid markdown file that
/// must be skipped (no description); `Err` is an unreadable/invalid file.
pub fn parse_file(path: &Path, project: bool) -> Result<Option<Command>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_text(&raw, path, project)
}

/// Parse the markdown text of a command file.
pub fn parse_text(raw: &str, path: &Path, project: bool) -> Result<Option<Command>> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .trim()
        .to_string();
    anyhow::ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
        "invalid command name `{name}` (use letters, digits, `-` or `_`)"
    );
    let (description, argument_hint, body) = match crate::skills::frontmatter_span(raw) {
        Some(span) => {
            let mut description = None;
            let mut argument_hint = None;
            let mut lines = raw[span.start..span.end].lines();
            lines.next(); // opening `---`
            for line in lines {
                let line = line.trim_end();
                if line.starts_with("---") {
                    break;
                }
                if let Some(value) = line.strip_prefix("description:") {
                    description = scalar(value);
                } else if let Some(value) = line
                    .strip_prefix("argument-hint:")
                    .or_else(|| line.strip_prefix("argument_hint:"))
                {
                    argument_hint = scalar(value);
                }
            }
            (
                description,
                argument_hint,
                raw[span.end..].trim().to_string(),
            )
        }
        // Body-only commands are accepted by skills for description fallback,
        // but a command must declare one explicitly.
        None => (None, None, raw.trim().to_string()),
    };
    let Some(description) = description else {
        return Ok(None);
    };
    anyhow::ensure!(!body.is_empty(), "command body is empty");
    Ok(Some(Command {
        name,
        description,
        argument_hint,
        body,
        path: path.to_path_buf(),
        project,
    }))
}

fn scalar(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
        .trim();
    (!unquoted.is_empty()).then(|| unquoted.to_string())
}

/// One piece of an expanded command body, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    /// A `!` line: run through the bash tool and permission gate.
    Shell(String),
}

/// A command body with `$ARGUMENTS` already substituted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub name: String,
    pub segments: Vec<Segment>,
}

impl Expansion {
    /// The shell steps, in body order.
    pub fn shell_steps(&self) -> Vec<&str> {
        self.segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Shell(command) => Some(command.as_str()),
                Segment::Text(_) => None,
            })
            .collect()
    }

    /// The user message: text plus one fenced block per shell step. `outputs`
    /// is indexed the same way as [`Self::shell_steps`].
    pub fn render(&self, outputs: &[String]) -> String {
        let mut out = String::new();
        let mut index = 0;
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => {
                    out.push_str(text);
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                Segment::Shell(command) => {
                    let output = outputs.get(index).map(String::as_str).unwrap_or("");
                    index += 1;
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&format!("$ {command}\n```\n{}\n```\n", output.trim_end()));
                }
            }
        }
        out.trim().to_string()
    }
}

/// Split the body into text and `!` shell steps, substituting `$ARGUMENTS`
/// only inside text segments. Substituting first let pasted user text that
/// contains a line starting with `!` become a shell step.
pub fn expand(command: &Command, raw_args: &str) -> Expansion {
    let body = command.body.clone();
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut after_shell = false;
    for line in body.lines() {
        if let Some(shell) = line.trim_start().strip_prefix('!') {
            let shell = shell.trim();
            if !shell.is_empty() {
                if !text.is_empty() {
                    segments.push(Segment::Text(std::mem::take(&mut text)));
                }
                segments.push(Segment::Shell(shell.to_string()));
                after_shell = true;
                continue;
            }
        }
        // Do not carry the blank separator line that follows a `!` step into
        // the rendered prompt.
        if after_shell && text.is_empty() && line.trim().is_empty() {
            continue;
        }
        after_shell = false;
        text.push_str(line);
        text.push('\n');
    }
    if !text.is_empty() {
        // Drop the trailing newline added by the loop.
        text.pop();
        segments.push(Segment::Text(text));
    }
    // Substitute only now, and only in text: a `!` step stays exactly what
    // the command author wrote.
    for segment in &mut segments {
        if let Segment::Text(text) = segment {
            *text = text.replace("$ARGUMENTS", raw_args);
        }
    }
    Expansion {
        name: command.name.clone(),
        segments,
    }
}

/// Match an invocation like `/commit staged` against the command set.
/// Returns the command and the raw text after its name.
pub fn invocation<'a>(commands: &'a [Command], user: &'a str) -> Option<(&'a Command, &'a str)> {
    let text = user.trim_start();
    let rest = text.strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args),
        None => (rest, ""),
    };
    let command = commands
        .iter()
        .find(|command| command.name.eq_ignore_ascii_case(name))?;
    Some((command, args.trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn command(body: &str) -> Command {
        let dir = tempdir().unwrap();
        let path = dir.path().join("demo.md");
        write(
            &path,
            &format!("---\ndescription: demo\nargument-hint: [scope]\n---\n{body}"),
        );
        parse_file(&path, false).unwrap().unwrap()
    }

    #[test]
    fn frontmatter_is_required_and_hint_is_optional() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("commit.md");
        write(
            &path,
            "---\ndescription: \"Write a commit\"\nargument-hint: [scope]\n---\nbody",
        );
        let parsed = parse_file(&path, true).unwrap().unwrap();
        assert_eq!(parsed.name, "commit");
        assert_eq!(parsed.description, "Write a commit");
        assert_eq!(parsed.argument_hint.as_deref(), Some("[scope]"));
        assert_eq!(parsed.body, "body");
        assert!(parsed.project);

        write(&path, "---\ndescription: bare\n---\nbody");
        let parsed = parse_file(&path, false).unwrap().unwrap();
        assert_eq!(parsed.argument_hint, None);
    }

    #[test]
    fn a_command_without_description_is_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nodesc.md");
        write(&path, "---\nargument-hint: [x]\n---\nbody");
        assert!(parse_file(&path, false).unwrap().is_none());
    }

    #[test]
    fn arguments_replace_every_marker_and_shell_lines_split() {
        let cmd =
            command("!git diff --cached\n\nWrite a commit for the diff above.\nScope: $ARGUMENTS");
        let expansion = expand(&cmd, "core");
        assert_eq!(
            expansion.segments,
            vec![
                Segment::Shell("git diff --cached".into()),
                Segment::Text("Write a commit for the diff above.\nScope: core".into()),
            ]
        );
        assert_eq!(expansion.shell_steps(), vec!["git diff --cached"]);
        let rendered = expansion.render(&["diff --git a/x b/x".into()]);
        assert!(rendered.contains("$ git diff --cached"));
        assert!(rendered.contains("```\ndiff --git a/x b/x\n```"));
        assert!(rendered.contains("Scope: core"));
    }

    #[test]
    fn arguments_never_create_shell_steps() {
        let cmd = command("Summarize $ARGUMENTS");
        let expansion = expand(&cmd, "line one\n!rm -rf out");
        assert!(
            expansion.shell_steps().is_empty(),
            "pasted argument text must not become a `!` shell step"
        );
        let rendered = expansion.render(&[]);
        assert!(rendered.contains("!rm -rf out"));
        // A `$ARGUMENTS` marker inside an authored shell line is left alone.
        let cmd = command("!echo literal $ARGUMENTS");
        let expansion = expand(&cmd, "ignored");
        assert_eq!(expansion.shell_steps(), vec!["echo literal $ARGUMENTS"]);
    }

    #[test]
    fn a_shell_line_without_output_still_renders() {
        let cmd = command("!echo hi");
        let rendered = expand(&cmd, "").render(&[]);
        assert!(rendered.contains("$ echo hi"));
        assert!(rendered.contains("```\n\n```"));
    }

    #[test]
    fn invocation_matches_by_name_and_keeps_raw_arguments() {
        let cmd = command("$ARGUMENTS");
        let commands = vec![cmd];
        let (found, args) = invocation(&commands, "/DEMO  two words ").unwrap();
        assert_eq!(found.name, "demo");
        assert_eq!(args, "two words ");
        assert!(invocation(&commands, "/other x").is_none());
        assert!(invocation(&commands, "demo").is_none());
    }

    #[test]
    fn user_commands_load_and_project_commands_wait_for_trust() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let user_dir = home.path().join("commands");
        write(
            &user_dir.join("reviewcmd.md"),
            "---\ndescription: user reviewcmd\n---\nreview $ARGUMENTS",
        );
        write(
            &CommandSet::project_dir(workspace.path()).join("ship.md"),
            "---\ndescription: project ship\n---\nship it",
        );

        let untrusted = CommandSet::discover_with(workspace.path(), &user_dir, false);
        assert!(untrusted.project_pending);
        assert_eq!(untrusted.user_commands, 1);
        assert_eq!(untrusted.project_commands, 0);
        assert!(untrusted.get("ship").is_none());

        let trusted = CommandSet::discover_with(workspace.path(), &user_dir, true);
        assert!(!trusted.project_pending);
        assert_eq!(trusted.project_commands, 1);
        assert!(trusted.get("ship").is_some());
        assert!(trusted.get("SHIP").unwrap().project);
    }

    #[test]
    fn project_commands_override_user_commands_with_the_same_name() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let user_dir = home.path().join("commands");
        write(
            &user_dir.join("task.md"),
            "---\ndescription: user task\n---\nuser body",
        );
        write(
            &CommandSet::project_dir(workspace.path()).join("task.md"),
            "---\ndescription: project task\n---\nproject body",
        );
        let set = CommandSet::discover_with(workspace.path(), &user_dir, true);
        assert_eq!(set.commands.len(), 1);
        let task = set.get("task").unwrap();
        assert_eq!(task.description, "project task");
        assert!(task.project);
    }

    #[test]
    fn missing_description_and_builtin_shadowing_warn_and_skip() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let user_dir = home.path().join("commands");
        write(&user_dir.join("nodesc.md"), "---\n---\nbody");
        write(
            &user_dir.join("help.md"),
            "---\ndescription: shadows help\n---\nbody",
        );
        write(
            &user_dir.join("good.md"),
            "---\ndescription: fine\n---\nbody",
        );
        let set = CommandSet::discover_with(workspace.path(), &user_dir, false);
        assert_eq!(set.commands.len(), 1);
        assert_eq!(set.commands[0].name, "good");
        assert_eq!(set.warnings.len(), 2);
        assert!(set.warnings.iter().any(|w| w.contains("description")));
        assert!(set.warnings.iter().any(|w| w.contains("built-in")));
    }
}
