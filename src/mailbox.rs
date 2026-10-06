//! Cross-session message bus. Every message is a JSONL record in
//! `~/.varynth/bus.jsonl`, guarded by a lock file so several processes
//! (TUI, serve, exec, dashboard) can write at once. Sessions address each
//! other by session id; the alias `latest` resolves to the most recently
//! updated session other than the sender.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::config::Config;

/// Read messages older than this are pruned on drain so the bus stays small.
const RETENTION: Duration = Duration::hours(24);
/// Bound on one message body; longer bodies are truncated on send.
const MAX_TEXT_CHARS: usize = 8_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub text: String,
    pub sent_at: DateTime<Utc>,
    pub read: bool,
}

/// A session that has appeared on the bus, with its last message time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub last_seen: DateTime<Utc>,
    pub unread: usize,
}

pub struct Bus {
    path: PathBuf,
}

impl Default for Bus {
    fn default() -> Self {
        Self {
            path: Config::home_dir().join("bus.jsonl"),
        }
    }
}

impl Bus {
    /// Bus rooted at an explicit path; used by tests to stay out of the
    /// real home directory.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn lock_path(&self) -> PathBuf {
        self.path.with_file_name(format!(
            "{}.lock",
            self.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("bus.jsonl")
        ))
    }

    /// Append a message from `from` to session `to`. The alias `latest` in
    /// `to` resolves to the most recently active session other than `from`.
    pub fn send(&self, from: &str, to: &str, text: &str) -> Result<String> {
        let _guard = self.lock()?;
        let resolved = if to == "latest" {
            self.latest_session(from)
                .context("no other session on the bus to resolve `latest`")?
        } else {
            to.to_string()
        };
        let text: String = text.chars().take(MAX_TEXT_CHARS).collect();
        let msg = BusMessage {
            id: Uuid::new_v4().to_string(),
            from: from.to_string(),
            to: resolved,
            text,
            sent_at: Utc::now(),
            read: false,
        };
        let id = msg.id.clone();
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        writeln!(f, "{}", serde_json::to_string(&msg)?)?;
        Ok(id)
    }

    /// Messages addressed to `to` that are still unread, oldest first.
    pub fn unread(&self, to: &str) -> Vec<BusMessage> {
        self.load()
            .into_iter()
            .filter(|m| m.to == to && !m.read)
            .collect()
    }

    /// Take the unread messages for `to`, mark them read, and prune read
    /// messages past the retention window. Fails soft: a corrupted or
    /// missing file just yields no mail.
    pub fn drain(&self, to: &str) -> Vec<BusMessage> {
        let taken = self.unread(to);
        if taken.is_empty() {
            return taken;
        }
        let taken_ids: Vec<String> = taken.iter().map(|m| m.id.clone()).collect();
        let _ = self.mark_read_and_prune(&taken_ids);
        taken
    }

    /// Sessions visible on the bus (as sender or receiver), newest activity
    /// first, with unread counts relative to `me`.
    pub fn peers(&self, me: &str) -> Vec<Peer> {
        let mut by_id: std::collections::BTreeMap<String, DateTime<Utc>> =
            std::collections::BTreeMap::new();
        for m in self.load() {
            for id in [&m.from, &m.to] {
                if id == me {
                    continue;
                }
                let entry = by_id.entry(id.clone()).or_insert(m.sent_at);
                if m.sent_at > *entry {
                    *entry = m.sent_at;
                }
            }
        }
        let mut peers: Vec<Peer> = by_id
            .into_iter()
            .map(|(id, last_seen)| {
                let unread = self.unread(&id).len();
                Peer {
                    id,
                    last_seen,
                    unread,
                }
            })
            .collect();
        peers.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        peers
    }

    fn latest_session(&self, exclude: &str) -> Option<String> {
        self.peers(exclude).into_iter().map(|p| p.id).next()
    }

    fn load(&self) -> Vec<BusMessage> {
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn mark_read_and_prune(&self, just_read: &[String]) -> Result<()> {
        let _guard = self.lock()?;
        let now_cut = Utc::now() - RETENTION;
        let all = self.load();
        let mut out = String::new();
        for m in &all {
            let was_taken = just_read.contains(&m.id);
            let read = m.read || was_taken;
            // Keep unread mail indefinitely; keep read mail only inside the
            // retention window.
            if !read || m.sent_at > now_cut {
                let mut line = m.clone();
                line.read = read;
                out.push_str(&serde_json::to_string(&line)?);
                out.push('\n');
            }
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(out.as_bytes())?;
        }
        fs::rename(&tmp, &self.path).with_context(|| format!("replace {}", self.path.display()))?;
        Ok(())
    }

    /// Exclusive lock file: create-new with a short retry, break it when it
    /// is older than LOCK_STALE_AFTER. Removed on drop.
    fn lock(&self) -> Result<BusLock> {
        let lock = self.lock_path();
        if let Some(parent) = lock.parent() {
            fs::create_dir_all(parent)?;
        }
        let started = std::time::Instant::now();
        loop {
            match fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock)
            {
                Ok(_) => return Ok(BusLock { path: lock }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&lock) {
                        let _ = fs::remove_file(&lock);
                        continue;
                    }
                    if started.elapsed() > LOCK_TIMEOUT {
                        anyhow::bail!("bus lock timeout at {}", lock.display());
                    }
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(e) => return Err(e).with_context(|| format!("lock {}", lock.display())),
            }
        }
    }
}

const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(10);
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const LOCK_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

struct BusLock {
    path: PathBuf,
}

impl Drop for BusLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > LOCK_STALE_AFTER)
}

/// Render drained mail as a context block for injection into a turn.
pub fn render_inbox(messages: &[BusMessage]) -> String {
    let mut out = String::from("[messages from other sessions]\n");
    for m in messages {
        out.push_str(&format!("[from {} at {}] {}\n", m.from, m.sent_at, m.text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(dir: &Path) -> Bus {
        Bus::at(dir.join("bus.jsonl"))
    }

    #[test]
    fn send_then_drain_returns_mail_once() {
        let dir = tempfile::tempdir().unwrap();
        let b = bus(dir.path());
        b.send("s-a", "s-b", "hello from a").unwrap();
        b.send("s-c", "s-b", "hello from c").unwrap();
        assert_eq!(b.unread("s-b").len(), 2);
        assert!(b.unread("s-a").is_empty());

        let drained = b.drain("s-b");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].from, "s-a");
        assert!(drained[0].text.contains("hello from a"));
        // Drained mail is not delivered twice.
        assert!(b.drain("s-b").is_empty());
        assert!(b.unread("s-b").is_empty());
    }

    #[test]
    fn latest_alias_resolves_to_the_other_newest_session() {
        let dir = tempfile::tempdir().unwrap();
        let b = bus(dir.path());
        b.send("s-a", "s-b", "first").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        b.send("s-a", "s-c", "second").unwrap();

        let id = b.latest_session("s-a").unwrap();
        assert_eq!(id, "s-c");
        b.send("s-a", "latest", "ping").unwrap();
        let drained = b.drain("s-c");
        assert_eq!(drained.len(), 2);
        assert!(drained.iter().any(|m| m.text == "ping"));
    }

    #[test]
    fn drain_prunes_old_read_mail() {
        let dir = tempfile::tempdir().unwrap();
        let b = bus(dir.path());
        b.send("s-a", "s-b", "old mail").unwrap();
        // Age the message beyond retention, then mark it read via drain.
        let aged = Utc::now() - Duration::hours(30);
        rewrite_all_aged(&b, aged);
        assert_eq!(b.drain("s-b").len(), 1);
        // Prune runs on the next drain; the aged, read record disappears.
        b.drain("s-b");
        let raw = fs::read_to_string(&b.path).unwrap();
        assert!(raw.trim().is_empty(), "aged read mail should be pruned");
    }

    fn rewrite_all_aged(b: &Bus, at: DateTime<Utc>) {
        let msgs: Vec<BusMessage> = b
            .load()
            .into_iter()
            .map(|mut m| {
                m.sent_at = at;
                m
            })
            .collect();
        fs::write(&b.path, String::new()).unwrap();
        for m in &msgs {
            let mut f = OpenOptions::new().append(true).open(&b.path).unwrap();
            writeln!(f, "{}", serde_json::to_string(m).unwrap()).unwrap();
        }
    }

    #[test]
    fn peers_lists_sessions_newest_first_with_unread_counts() {
        let dir = tempfile::tempdir().unwrap();
        let b = bus(dir.path());
        b.send("s-a", "me", "from a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        b.send("s-b", "me", "from b").unwrap();
        b.send("s-b", "me", "again from b").unwrap();
        b.send("me", "s-b", "reply to b").unwrap();

        let peers = b.peers("me");
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].id, "s-b");
        assert_eq!(peers[0].unread, 1);
        assert_eq!(peers[1].id, "s-a");
        assert_eq!(peers[1].unread, 0);
        // The session itself is not listed as its own peer.
        assert!(b.peers("s-a").iter().all(|p| p.id != "s-a"));
    }

    #[test]
    fn long_bodies_are_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let b = bus(dir.path());
        let long = "x".repeat(MAX_TEXT_CHARS + 500);
        b.send("s-a", "s-b", &long).unwrap();
        let mail = b.drain("s-b");
        assert_eq!(mail[0].text.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn corrupted_lines_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let b = bus(dir.path());
        b.send("s-a", "s-b", "real mail").unwrap();
        let mut f = OpenOptions::new().append(true).open(&b.path).unwrap();
        writeln!(f, "{{not json").unwrap();
        drop(f);
        assert_eq!(b.drain("s-b").len(), 1);
    }

    #[test]
    fn render_inbox_formats_sender_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let b = bus(dir.path());
        b.send("s-a", "s-b", "status update").unwrap();
        let mail = b.drain("s-b");
        let text = render_inbox(&mail);
        assert!(text.contains("[from s-a"));
        assert!(text.contains("status update"));
    }
}
