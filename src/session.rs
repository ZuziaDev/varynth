use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use uuid::Uuid;

use crate::config::Config;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

/// An image attached to a user message, stored base64-encoded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub media_type: String,
    pub data: String,
}

impl ImageAttachment {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cwd: String,
    pub model: String,
    pub title: String,
    /// True when a human or the LLM titler set the title explicitly; the
    /// first-user-message auto-title must never overwrite it.
    #[serde(default)]
    pub title_explicit: bool,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub meta: SessionMeta,
    pub messages: Vec<ChatMessage>,
    path: PathBuf,
}

impl Session {
    pub fn new(cwd: &str, model: &str) -> Result<Self> {
        Config::ensure_home()?;
        let id = Uuid::new_v4().to_string();
        let path = Config::sessions_dir().join(format!("{id}.jsonl"));
        Self::create_at(id, path, cwd, model)
    }

    /// Constructor with an explicit id and file path; used by tests to stay
    /// out of the real home directory.
    pub(crate) fn create_at(id: String, path: PathBuf, cwd: &str, model: &str) -> Result<Self> {
        let now = Utc::now();
        let meta = SessionMeta {
            id,
            created_at: now,
            updated_at: now,
            cwd: cwd.to_string(),
            model: model.to_string(),
            title: "new session".into(),
            title_explicit: false,
        };
        let s = Self {
            meta,
            messages: Vec::new(),
            path,
        };
        s.persist_meta()?;
        Ok(s)
    }

    pub fn load(id: &str) -> Result<Self> {
        Uuid::parse_str(id).context("invalid session id")?;
        let path = Config::sessions_dir().join(format!("{id}.jsonl"));
        Self::load_path(path)
    }

    pub fn load_latest() -> Result<Option<Self>> {
        let dir = Config::sessions_dir();
        if !dir.exists() {
            return Ok(None);
        }
        let mut files: Vec<_> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
        match files.last() {
            Some(f) => Ok(Some(Self::load_path(f.path())?)),
            None => Ok(None),
        }
    }

    fn load_path(path: PathBuf) -> Result<Self> {
        let file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut meta: Option<SessionMeta> = None;
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&line)?;
            match v.get("kind").and_then(|k| k.as_str()) {
                Some("meta") => {
                    meta = Some(serde_json::from_value(v["meta"].clone())?);
                }
                Some("message") => {
                    messages.push(serde_json::from_value(v["message"].clone())?);
                }
                // Written by compaction: the history before this record is
                // replaced wholesale by the compacted one.
                Some("replace") => {
                    messages = serde_json::from_value(v["messages"].clone())?;
                }
                _ => {}
            }
        }
        let meta = meta.context("session missing meta")?;
        Ok(Self {
            meta,
            messages,
            path,
        })
    }

    pub fn list() -> Result<Vec<SessionMeta>> {
        Config::ensure_home()?;
        let dir = Config::sessions_dir();
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for e in fs::read_dir(dir)? {
            let e = e?;
            if e.path().extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(s) = Self::load_path(e.path()) {
                let mut meta = s.meta;
                // Meta records are only rewritten when the title changes, so
                // the file mtime is the fresher "last activity" signal.
                if let Ok(modified) = e.metadata().and_then(|m| m.modified()) {
                    let modified: DateTime<Utc> = modified.into();
                    if modified > meta.updated_at {
                        meta.updated_at = modified;
                    }
                }
                out.push(meta);
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    }

    pub fn append(&mut self, msg: ChatMessage) -> Result<()> {
        self.meta.updated_at = Utc::now();
        self.messages.push(msg.clone());
        let rec = serde_json::json!({
            "kind": "message",
            "message": msg,
        });
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        Ok(())
    }

    /// Replace the whole history in place (compaction) and persist a
    /// `replace` record so a later load resumes from the compacted history.
    pub fn replace_messages(&mut self, messages: Vec<ChatMessage>) -> Result<()> {
        self.messages = messages;
        self.meta.updated_at = Utc::now();
        let rec = serde_json::json!({
            "kind": "replace",
            "messages": self.messages,
        });
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        self.persist_meta()
    }

    fn persist_meta(&self) -> Result<()> {
        if !self.path.exists() {
            let rec = serde_json::json!({ "kind": "meta", "meta": self.meta });
            fs::write(&self.path, format!("{}\n", serde_json::to_string(&rec)?))?;
            return Ok(());
        }
        // Rewrite first line meta by rewriting the file if title changed.
        // Cheap path: append a newer meta record; load takes the last meta.
        let rec = serde_json::json!({ "kind": "meta", "meta": self.meta });
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.meta.id
    }

    /// Rename the session and persist the new title (appends a fresh meta
    /// record; load takes the last one). Marks the title explicit so the
    /// first-user-message auto-title can never overwrite it.
    pub fn set_title(&mut self, title: &str) -> Result<()> {
        let t = sanitize_title(title);
        anyhow::ensure!(!t.is_empty(), "title cannot be empty");
        self.meta.title = t;
        self.meta.title_explicit = true;
        self.meta.updated_at = Utc::now();
        self.persist_meta()
    }

    /// Whether the title was set explicitly (by a user or the LLM titler).
    pub fn clear_messages(&mut self) -> Result<()> {
        self.replace_messages(vec![ChatMessage {
            role: "system".into(),
            content: format!("context cleared by /clear at {}", Utc::now().to_rfc3339()),
            ..Default::default()
        }])
    }

    pub fn duplicate_as_fork(&self, new_id: &str) -> Result<Self> {
        let id = Uuid::parse_str(new_id).context("invalid fork session id")?;
        anyhow::ensure!(
            id.to_string() != self.meta.id,
            "fork requires a different session id"
        );
        let path = self
            .path
            .parent()
            .context("session has no parent directory")?
            .join(format!("{id}.jsonl"));
        anyhow::ensure!(!path.exists(), "fork session already exists");
        let mut fork = Self::create_at(id.to_string(), path, &self.meta.cwd, &self.meta.model)?;
        fork.set_title(&format!("{} (fork)", self.meta.title))?;
        fork.replace_messages(self.messages.clone())?;
        Ok(fork)
    }

    pub fn title_is_explicit(&self) -> bool {
        self.meta.title_explicit
    }

    /// After a turn is interrupted mid tool-call, answers every tool call in
    /// the last assistant message that has no result yet. Providers reject a
    /// history where a tool call is left unanswered. Returns how many it closed.
    pub fn close_interrupted_tools(&mut self) -> Result<usize> {
        let missing = unanswered_tool_calls(&self.messages);
        for id in &missing {
            self.append(ChatMessage {
                role: "tool".into(),
                content: "interrupted by the user before this tool finished".into(),
                tool_call_id: Some(id.clone()),
                ..Default::default()
            })?;
        }
        Ok(missing.len())
    }
}

/// One line, trimmed, ≤ 72 chars.
fn sanitize_title(title: &str) -> String {
    let t = title.trim();
    let t = t.lines().next().unwrap_or("").trim();
    let t: String = t.chars().take(72).collect();
    t.trim_end().to_string()
}

fn unanswered_tool_calls(messages: &[ChatMessage]) -> Vec<String> {
    let Some(pos) = messages
        .iter()
        .rposition(|m| m.role == "assistant" && m.tool_calls.is_some())
    else {
        return Vec::new();
    };
    let answered: Vec<&str> = messages[pos + 1..]
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    messages[pos]
        .tool_calls
        .iter()
        .flatten()
        .filter(|c| !answered.contains(&c.id.as_str()))
        .map(|c| c.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "shell".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn set_title_persists_survives_reload_and_blocks_auto_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path());
        session.append(msg("user", "first message")).unwrap();
        let auto_title = session.meta.title.clone();
        assert_eq!(auto_title, "new session");

        session.set_title("OAuth Login Fix").unwrap();
        assert_eq!(session.meta.title, "OAuth Login Fix");
        assert!(session.title_is_explicit());

        let mut reloaded = Session::load_path(session_path(&dir)).unwrap();
        assert_eq!(reloaded.meta.title, "OAuth Login Fix");
        assert!(reloaded.title_is_explicit());

        // A later append must not clobber an explicit title.
        reloaded.append(msg("user", "another message")).unwrap();
        assert_eq!(reloaded.meta.title, "OAuth Login Fix");
    }

    #[test]
    fn set_title_rejects_blank_and_caps_length() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path());
        assert!(session.set_title("   ").is_err());
        assert!(session.set_title("first\nsecond").is_ok());
        assert_eq!(session.meta.title, "first");
        let long = session.set_title(&"x".repeat(90));
        assert!(long.is_ok());
        assert_eq!(session.meta.title.chars().count(), 72);
    }

    fn session_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("session-test.jsonl")
    }

    #[test]
    fn finds_only_unanswered_calls_of_the_last_tool_message() {
        let msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                tool_calls: Some(vec![call("old")]),
                ..Default::default()
            },
            ChatMessage {
                role: "assistant".into(),
                tool_calls: Some(vec![call("a"), call("b"), call("c")]),
                ..Default::default()
            },
            ChatMessage {
                role: "tool".into(),
                tool_call_id: Some("a".into()),
                ..Default::default()
            },
        ];
        assert_eq!(unanswered_tool_calls(&msgs), vec!["b", "c"]);
    }

    #[test]
    fn nothing_to_close_after_a_finished_turn() {
        let msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                tool_calls: Some(vec![call("a")]),
                ..Default::default()
            },
            ChatMessage {
                role: "tool".into(),
                tool_call_id: Some("a".into()),
                ..Default::default()
            },
            ChatMessage {
                role: "assistant".into(),
                content: "done".into(),
                ..Default::default()
            },
        ];
        assert!(unanswered_tool_calls(&msgs).is_empty());
        assert!(unanswered_tool_calls(&[]).is_empty());
    }

    fn test_session(dir: &std::path::Path) -> Session {
        Session::create_at(
            Uuid::new_v4().to_string(),
            dir.join("session-test.jsonl"),
            "C:\\tmp",
            "mock",
        )
        .unwrap()
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn meta_record_count(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| line.contains("\"kind\":\"meta\""))
            .count()
    }

    #[test]
    fn appending_messages_persists_meta_only_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path());
        let path = dir.path().join("session-test.jsonl");
        // Message appends leave the default title for the LLM titler and
        // persist no duplicate metadata records.
        session.append(msg("user", "first message")).unwrap();
        session.append(msg("assistant", "a reply")).unwrap();
        session.append(msg("user", "second message")).unwrap();
        assert_eq!(meta_record_count(&path), 1);
        assert_eq!(session.messages.len(), 3);

        session.append(msg("user", "third message")).unwrap();
        session.append(msg("assistant", "another reply")).unwrap();
        assert_eq!(meta_record_count(&path), 1);
        assert_eq!(session.messages.len(), 5);
    }

    #[test]
    fn replace_messages_compacts_history_on_disk_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path());
        session.append(msg("user", "old question")).unwrap();
        session.append(msg("assistant", "old answer")).unwrap();
        session.append(msg("user", "recent question")).unwrap();

        session
            .replace_messages(vec![
                ChatMessage {
                    role: "system".into(),
                    content: "summary of the old turns".into(),
                    ..Default::default()
                },
                msg("user", "recent question"),
            ])
            .unwrap();

        let reloaded = Session::load_path(dir.path().join("session-test.jsonl")).unwrap();
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[0].role, "system");
        assert_eq!(reloaded.messages[0].content, "summary of the old turns");
        assert_eq!(reloaded.messages[1].content, "recent question");
        assert_eq!(reloaded.meta.title, session.meta.title);
    }
}
