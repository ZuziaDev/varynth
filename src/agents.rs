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
}

/// Built-in personas for the composer's `@` popup. Each runs through the
/// same one-shot subagent machinery as `/agents`.
pub fn personas() -> Vec<PersonaSpec> {
    vec![
        PersonaSpec {
            name: "coder".into(),
            prompt: "Implement the requested change. Prefer small diffs, run the relevant \
                     tests or build, and report exactly what changed and what was verified."
                .into(),
        },
        PersonaSpec {
            name: "reviewer".into(),
            prompt: "Review the given code or diff. Find bugs, risks and test gaps; order \
                     findings by severity with concrete file references. Do not modify files."
                .into(),
        },
        PersonaSpec {
            name: "architect".into(),
            prompt: "Design before code. Propose the smallest architecture that fits the \
                     request, list trade-offs, and give a concrete step plan."
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
}
