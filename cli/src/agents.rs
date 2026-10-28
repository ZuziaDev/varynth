//! One-shot subagents. A named prompt runs on its own runtime and session
//! so it cannot trample the foreground conversation. The caller posts the
//! reply back into the chat.

use anyhow::Result;

use crate::activity::{ActivityKind, ActivityLog};
use crate::config::Config;
use crate::runtime::Runtime;
use crate::session::Session;

pub struct AgentResult {
    pub name: String,
    pub reply: String,
}

/// A built-in sub-agent persona addressable as `@name` in the composer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PersonaSpec {
    pub name: String,
    pub prompt: String,
    pub category: &'static str,
    pub summary: &'static str,
}

impl PersonaSpec {
    /// Short role badge shown in `@` autocomplete popups and subagent swarm blocks
    /// (`"Planner"`, `"Builder"`, `"QA"`, `"SecOps"`, `"Scout"`).
    pub fn role_badge(&self) -> &'static str {
        if !self.category.is_empty() {
            self.category
        } else {
            role_badge_for(&self.name)
        }
    }

    /// Alias for [`Self::role_badge`].
    pub fn badge(&self) -> &'static str {
        self.role_badge()
    }

    /// One-line summary suitable for autocomplete rows.
    pub fn short_description(&self) -> &'static str {
        if !self.summary.is_empty() {
            self.summary
        } else {
            summary_for(&self.name)
        }
    }
}

/// Role badge for a persona or subagent name (`"Planner"`, `"Builder"`, `"QA"`, `"SecOps"`, `"Scout"`).
pub fn role_badge_for(name: &str) -> &'static str {
    match name
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase()
        .as_str()
    {
        "architect" => "Planner",
        "coder" => "Builder",
        "reviewer" | "verifier" => "QA",
        "security" => "SecOps",
        "explorer" | "scout" => "Scout",
        _ => "Agent",
    }
}

/// Short description for a built-in persona name.
pub fn summary_for(name: &str) -> &'static str {
    match name
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase()
        .as_str()
    {
        "architect" => "System architecture, AST planning & decomposition",
        "coder" => "Fast implementation, refactoring & patch synthesis",
        "reviewer" | "verifier" => "Code review, edge cases, regressions & test verification",
        "security" => "Vulnerability audit, sandbox boundaries & secret hygiene",
        "explorer" | "scout" => "Fast codebase reconnaissance, symbol search & dependency mapping",
        _ => "Autonomous subagent",
    }
}

/// Built-in personas for the composer's `@` popup. Each runs through the
/// same one-shot subagent machinery as `/agents`.
pub fn personas() -> Vec<PersonaSpec> {
    vec![
        PersonaSpec {
            name: "coder".into(),
            category: "Builder",
            summary: "Fast implementation, refactoring & patch synthesis",
            prompt: "Fast implementation, refactoring & patch synthesis.\n\
                     Implement the requested change. Prefer small diffs, run the relevant \
                     tests or build, and report exactly what changed and what was verified."
                .into(),
        },
        PersonaSpec {
            name: "reviewer".into(),
            category: "QA",
            summary: "Code review, edge cases, regressions & test verification",
            prompt: "Code review, edge cases, regressions & test verification.\n\
                     Review the given code or diff. Find bugs, risks and test gaps; order \
                     findings by severity with concrete file references. Do not modify files."
                .into(),
        },
        PersonaSpec {
            name: "architect".into(),
            category: "Planner",
            summary: "System architecture, AST planning & decomposition",
            prompt: "System architecture, AST planning & decomposition.\n\
                     Design before code. Propose the smallest architecture that fits the \
                     request, list trade-offs, and give a concrete step plan."
                .into(),
        },
        PersonaSpec {
            name: "security".into(),
            category: "SecOps",
            summary: "Vulnerability audit, sandbox boundaries & secret hygiene",
            prompt: "Vulnerability audit, sandbox boundaries & secret hygiene.\n\
                     Audit the target code, configuration, and tool boundaries for \
                     vulnerabilities, secret exposure, and sandbox escapes. Do not modify files."
                .into(),
        },
        PersonaSpec {
            name: "explorer".into(),
            category: "Scout",
            summary: "Fast codebase reconnaissance, symbol search & dependency mapping",
            prompt: "Fast codebase reconnaissance, symbol search & dependency mapping.\n\
                     Search the repository quickly, map key files, symbols, and dependencies \
                     relevant to the task, and report concise findings with exact file paths."
                .into(),
        },
    ]
}

/// `spec` is `name: prompt` or, with no colon, a one-word name plus the
/// rest of the line as the prompt.
pub fn parse_spec(spec: &str) -> Result<(String, String)> {
    let spec = spec.trim();
    anyhow::ensure!(!spec.is_empty(), "usage: /agents <name>: <prompt>");
    let (name, prompt) = if let Some((name, prompt)) = spec.split_once(':') {
        (name.trim(), prompt.trim())
    } else {
        let mut parts = spec.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").trim();
        let prompt = parts.next().unwrap_or("").trim();
        (name, prompt)
    };
    anyhow::ensure!(!name.is_empty(), "agent name is missing");
    anyhow::ensure!(!prompt.is_empty(), "agent prompt is missing");
    let name: String = name.chars().take(32).collect();
    Ok((name, prompt.to_string()))
}

pub async fn run(
    cfg: &Config,
    cwd: &std::path::Path,
    name: &str,
    prompt: &str,
) -> Result<AgentResult> {
    let id = ActivityLog::start(ActivityKind::Subagent, format!("agent {name}"), prompt).ok();
    let mut rt = Runtime::new(cfg.clone(), cwd.to_path_buf())?;
    rt.set_notification_role(crate::notify::Role::Subagent(name.to_string()));
    let mut session = Session::new(&cwd.display().to_string(), &cfg.model)?;
    let brief = format!(
        "You are the subagent `{name}`. Do this one task, then stop. \
         Do not ask what to do next. Reply with the result only.\n\n{prompt}"
    );
    let turn = rt.turn(&mut session, &brief, |_| {}).await;
    match turn {
        Ok(reply) => {
            if let Some(id) = &id {
                let _ = ActivityLog::finish(id, true, "agent finished");
            }
            Ok(AgentResult {
                name: name.to_string(),
                reply,
            })
        }
        Err(err) => {
            if let Some(id) = &id {
                let _ = ActivityLog::finish(id, false, "agent failed");
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colon_and_word_forms() {
        let (name, prompt) = parse_spec("reviewer: look at the diff").unwrap();
        assert_eq!(name, "reviewer");
        assert_eq!(prompt, "look at the diff");
        let (name, prompt) = parse_spec("scout find the tests").unwrap();
        assert_eq!(name, "scout");
        assert_eq!(prompt, "find the tests");
    }

    #[test]
    fn rejects_a_name_without_a_prompt() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("onlyname").is_err());
        assert!(parse_spec(": missing name").is_err());
    }

    #[test]
    fn builtin_personas_cover_all_five_roles_and_badges() {
        let all = personas();
        assert_eq!(all.len(), 5);
        for (name, expected_badge, expected_summary) in [
            (
                "architect",
                "Planner",
                "System architecture, AST planning & decomposition",
            ),
            (
                "coder",
                "Builder",
                "Fast implementation, refactoring & patch synthesis",
            ),
            (
                "reviewer",
                "QA",
                "Code review, edge cases, regressions & test verification",
            ),
            (
                "security",
                "SecOps",
                "Vulnerability audit, sandbox boundaries & secret hygiene",
            ),
            (
                "explorer",
                "Scout",
                "Fast codebase reconnaissance, symbol search & dependency mapping",
            ),
        ] {
            let p = all.iter().find(|p| p.name == name).expect(name);
            assert_eq!(p.role_badge(), expected_badge);
            assert_eq!(p.category, expected_badge);
            assert_eq!(p.short_description(), expected_summary);
            assert_eq!(role_badge_for(name), expected_badge);
            assert_eq!(summary_for(name), expected_summary);
        }
    }
}
