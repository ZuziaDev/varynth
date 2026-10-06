use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::runtime::AgentEvent;

const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct ControlBus {
    dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Frame {
    pub session: String,
    pub event: AgentEvent,
    pub ts: String,
    pub pid: u32,
}

impl Default for ControlBus {
    fn default() -> Self {
        Self::at(crate::config::Config::home_dir().join("control"))
    }
}

impl ControlBus {
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, session: &str, extension: &str) -> Result<PathBuf> {
        validate_id(session)?;
        Ok(self.dir.join(format!("{session}.{extension}")))
    }

    pub fn publish(&self, session: &str, event: &AgentEvent) -> Result<()> {
        let path = self.path(session, "jsonl")?;
        fs::create_dir_all(&self.dir)?;
        let _lock = FileLock::acquire(path.with_extension("lock"))?;
        let rotate = path
            .metadata()
            .map(|m| m.len() >= MAX_JOURNAL_BYTES)
            .unwrap_or(false);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .append(!rotate)
            .truncate(rotate)
            .open(path)?;
        let frame = Frame {
            session: session.to_string(),
            event: event.clone(),
            ts: chrono::Utc::now().to_rfc3339(),
            pid: std::process::id(),
        };
        writeln!(file, "{}", serde_json::to_string(&frame)?)?;
        Ok(())
    }

    pub fn journals(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|v| v.to_str()) == Some("jsonl")
                    && entry.file_type().is_ok_and(|t| t.is_file())
                    && path
                        .file_stem()
                        .and_then(|v| v.to_str())
                        .is_some_and(|id| validate_id(id).is_ok()))
                .then_some(path)
            })
            .collect()
    }

    pub fn read_since(path: &Path, offset: &mut u64) -> Result<Vec<Frame>> {
        let mut file = fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len < *offset {
            *offset = 0;
        }
        file.seek(SeekFrom::Start(*offset))?;
        let mut reader = BufReader::new(file);
        let mut frames = Vec::new();
        for _ in 0..4096 {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 || !line.ends_with('\n') {
                break;
            }
            *offset += n as u64;
            if let Ok(frame) = serde_json::from_str(&line) {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    pub fn set_paused(&self, session: &str, paused: bool) -> Result<()> {
        let path = self.path(session, "paused")?;
        fs::create_dir_all(&self.dir)?;
        if paused {
            fs::write(path, b"paused")?;
        } else if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn is_paused(&self, session: &str) -> bool {
        self.path(session, "paused").is_ok_and(|p| p.exists())
    }

    pub fn replace_context(&self, session: &str, messages: &Value) -> Result<()> {
        let messages: Vec<crate::session::ChatMessage> = serde_json::from_value(messages.clone())?;
        anyhow::ensure!(messages.len() <= 200, "context is limited to 200 messages");
        anyhow::ensure!(
            messages.iter().all(
                |m| matches!(m.role.as_str(), "user" | "assistant" | "system")
                    && m.tool_calls.is_none()
                    && m.tool_call_id.is_none()
                    && m.images.is_empty()
            ),
            "context must contain text-only user, assistant or system messages"
        );
        let raw = serde_json::to_vec(&messages)?;
        anyhow::ensure!(raw.len() <= 512 * 1024, "context exceeds 512 KiB");
        let path = self.path(session, "context")?;
        fs::create_dir_all(&self.dir)?;
        let _lock = FileLock::acquire(path.with_extension("context.lock"))?;
        atomic_write(&path, &raw)
    }

    pub fn take_context(&self, session: &str) -> Result<Option<Vec<crate::session::ChatMessage>>> {
        let path = self.path(session, "context")?;
        if !path.exists() {
            return Ok(None);
        }
        let _lock = FileLock::acquire(path.with_extension("context.lock"))?;
        if !path.exists() {
            return Ok(None);
        }
        let messages = serde_json::from_slice(&fs::read(&path)?)?;
        fs::remove_file(path)?;
        Ok(Some(messages))
    }
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 96
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "invalid local control id"
    );
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&tmp, content)?;
    let result = fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub(crate) struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub(crate) fn acquire(path: PathBuf) -> Result<Self> {
        let started = Instant::now();
        loop {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if path
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| m.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(60))
                    {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    anyhow::ensure!(
                        started.elapsed() < Duration::from_secs(5),
                        "local control lock timeout"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_delivers_events_once_from_separate_instances() {
        let dir = tempfile::tempdir().unwrap();
        let sender = ControlBus::at(dir.path().into());
        sender
            .publish(
                "session-a",
                &AgentEvent {
                    kind: "tool".into(),
                    text: "git status".into(),
                },
            )
            .unwrap();
        let receiver = ControlBus::at(dir.path().into());
        let path = receiver.journals().pop().unwrap();
        let mut offset = 0;
        let frames = ControlBus::read_since(&path, &mut offset).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].session, "session-a");
        assert_eq!(frames[0].event.kind, "tool");
        assert!(ControlBus::read_since(&path, &mut offset)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pause_and_context_are_shared_and_traversal_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bus = ControlBus::at(dir.path().into());
        bus.set_paused("s-a", true).unwrap();
        assert!(bus.is_paused("s-a"));
        bus.set_paused("s-a", false).unwrap();
        assert!(!bus.is_paused("s-a"));
        assert!(bus.set_paused("../escape", true).is_err());
        bus.replace_context(
            "s-a",
            &serde_json::json!([{"role":"user","content":"new context"}]),
        )
        .unwrap();
        assert_eq!(
            bus.take_context("s-a").unwrap().unwrap()[0].content,
            "new context"
        );
        assert!(bus.take_context("s-a").unwrap().is_none());
        assert!(bus
            .replace_context(
                "s-a",
                &serde_json::json!([{"role":"tool","content":"spoof"}])
            )
            .is_err());
    }
}
