//! Registry of live terminals. Registry mutations hold an OS file lock and
//! replace the file atomically; peers are addressed by session, never by the
//! terminal's registration UUID.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::config::Config;

const STALE_AFTER: Duration = Duration::minutes(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: String,
    pub pid: u32,
    pub cwd: String,
    pub label: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Registry {
    path: PathBuf,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            path: Config::home_dir().join("terminals.json"),
        }
    }
}

impl Registry {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Legacy registrations remain readable, but cannot receive session mail.
    pub fn heartbeat(&self, id: &str, pid: u32, cwd: &str, label: Option<&str>) -> Result<()> {
        self.heartbeat_session(id, pid, cwd, label, None)
    }

    pub fn heartbeat_session(
        &self,
        id: &str,
        pid: u32,
        cwd: &str,
        label: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.mutate(|all| {
            let now = Utc::now();
            all.retain(|t| now - t.last_seen < STALE_AFTER || t.id == id);
            match all.iter_mut().find(|t| t.id == id) {
                Some(entry) => {
                    entry.last_seen = now;
                    entry.pid = pid;
                    entry.cwd = cwd.to_string();
                    entry.label = label.map(str::to_string);
                    if let Some(session_id) = session_id {
                        entry.session_id = Some(session_id.into());
                    }
                }
                None => all.push(TerminalInfo {
                    id: id.into(),
                    pid,
                    cwd: cwd.into(),
                    label: label.map(str::to_string),
                    session_id: session_id.map(str::to_string),
                    last_seen: now,
                }),
            }
        })
    }

    pub fn live(&self) -> Vec<TerminalInfo> {
        let now = Utc::now();
        let mut live: Vec<_> = self
            .load_raw()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| now - t.last_seen < STALE_AFTER)
            .collect();
        live.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(a.id.cmp(&b.id)));
        live
    }

    /// `term:N`, terminal UUID, or a unique case-insensitive label.
    pub fn resolve_handle(&self, handle: &str) -> Option<TerminalInfo> {
        let handle = handle.trim_start_matches('@').strip_prefix("term:")?;
        let live = self.live();
        if let Ok(n) = handle.parse::<usize>() {
            return live.get(n.checked_sub(1)?).cloned();
        }
        let mut matches = live.into_iter().filter(|t| {
            t.id == handle
                || t.label
                    .as_deref()
                    .is_some_and(|label| label.eq_ignore_ascii_case(handle))
        });
        let found = matches.next()?;
        matches.next().is_none().then_some(found)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.mutate(|all| all.retain(|t| t.id != id))
    }

    fn load_raw(&self) -> Result<Vec<TerminalInfo>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).context("parse terminal registry"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).context("read terminal registry"),
        }
    }

    fn mutate(&self, update: impl FnOnce(&mut Vec<TerminalInfo>)) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock_path = self.path.with_extension("json.lock");
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.lock().context("lock terminal registry")?;
        let mut all = self.load_raw()?;
        update(&mut all);
        self.save(&all)
    }

    fn save(&self, entries: &[TerminalInfo]) -> Result<()> {
        let tmp = self
            .path
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(serde_json::to_string_pretty(entries)?.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path).context("replace terminal registry")
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }

    pub fn register(&self, session_id: &str, cwd: &str, title: &str) -> Result<Registration> {
        let registration = Registration {
            registry: self.clone(),
            id: uuid::Uuid::new_v4().to_string(),
        };
        registration.refresh(session_id, cwd, title)?;
        Ok(registration)
    }
}

/// Removing the registration on drop covers normal exit, terminal IO errors,
/// and cancellation. The persistent lock file is not removed while peers use it.
pub struct Registration {
    registry: Registry,
    pub id: String,
}

impl Registration {
    pub fn refresh(&self, session_id: &str, cwd: &str, title: &str) -> Result<()> {
        self.registry.heartbeat_session(
            &self.id,
            std::process::id(),
            cwd,
            Some(title),
            Some(session_id),
        )
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.registry.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry(dir: &std::path::Path) -> Registry {
        Registry::at(dir.join("terminals.json"))
    }

    #[test]
    fn heartbeat_creates_and_refreshes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let r = registry(dir.path());
        r.heartbeat_session("t-a", 111, "C:/proj", Some("worker"), Some("s-a"))
            .unwrap();
        r.heartbeat("t-b", 222, "C:/proj", None).unwrap();
        r.heartbeat("t-a", 111, "C:/other", Some("worker")).unwrap();
        let live = r.live();
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].cwd, "C:/other");
        assert_eq!(live[0].session_id.as_deref(), Some("s-a"));
    }

    #[test]
    fn resolve_handle_maps_number_and_unique_name() {
        let dir = tempfile::tempdir().unwrap();
        let r = registry(dir.path());
        r.heartbeat("t-1", 1, ".", Some("worker")).unwrap();
        r.heartbeat("t-2", 2, ".", Some("other")).unwrap();
        assert_eq!(r.resolve_handle("term:1").unwrap().pid, 2);
        assert_eq!(r.resolve_handle("term:worker").unwrap().pid, 1);
        assert!(r.resolve_handle("term:3").is_none());
        r.heartbeat("t-3", 3, ".", Some("worker")).unwrap();
        assert!(r.resolve_handle("term:worker").is_none());
        assert!(r.resolve_handle("bogus").is_none());
    }

    #[test]
    fn stale_entries_are_pruned_and_legacy_entries_parse() {
        let dir = tempfile::tempdir().unwrap();
        let r = registry(dir.path());
        let raw = serde_json::json!([{"id":"old","pid":1,"cwd":".","label":null,
            "last_seen":Utc::now() - Duration::minutes(10)}]);
        fs::write(&r.path, raw.to_string()).unwrap();
        assert!(r.live().is_empty());
        r.heartbeat("fresh", 2, ".", None).unwrap();
        assert_eq!(r.load_raw().unwrap().len(), 1);
    }

    #[test]
    fn parallel_heartbeats_and_removals_preserve_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let r = registry(dir.path());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let threads: Vec<_> = (0..12)
            .map(|n| {
                let r = r.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..5 {
                        r.heartbeat_session(
                            &format!("t-{n}"),
                            n,
                            ".",
                            None,
                            Some(&format!("s-{n}")),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(r.live().len(), 12);
        r.remove("t-2").unwrap();
        assert_eq!(r.live().len(), 11);
        assert!(!r.live().iter().any(|t| t.id == "t-2"));
    }

    #[test]
    fn registration_tracks_sessions_and_unregisters_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let r = registry(dir.path());
        {
            let registration = r.register("s-first", ".", "first").unwrap();
            registration.refresh("s-next", ".", "next").unwrap();
            assert_eq!(r.live()[0].session_id.as_deref(), Some("s-next"));
        }
        assert!(r.live().is_empty());
    }
}
