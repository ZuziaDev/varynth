//! Live activity the ↓ picker shows: background shells and subagents.
//!
//! Records live in `~/.varynth/activity.json`. The TUI only reads them.
//! Shells are registered by the bash tool; subagents by the runtime when
//! a turn is spawned off the foreground composer.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;

const LOCK_RETRY: Duration = Duration::from_millis(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(60);

/// Mirrors the TaskStore lock in automation.rs: a create-new lock file with
/// retry, so concurrent processes cannot interleave read-modify-write cycles
/// and corrupt activity.json.
struct ActivityFileLock {
    path: PathBuf,
}

impl ActivityFileLock {
    fn acquire(path: &Path) -> Result<Self> {
        let started = Instant::now();
        let lock_path = lock_path(path);
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return Ok(Self { path: lock_path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&lock_path) {
                        let _ = fs::remove_file(&lock_path);
                        continue;
                    }
                    if started.elapsed() >= LOCK_TIMEOUT {
                        anyhow::bail!(
                            "timed out waiting for activity lock {}",
                            lock_path.display()
                        );
                    }
                    thread::sleep(LOCK_RETRY);
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("create {}", lock_path.display()));
                }
            }
        }
    }
}

impl Drop for ActivityFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("activity.json")
    ))
}

fn lock_is_stale(path: &Path) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > LOCK_STALE_AFTER)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Shell,
    Subagent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: String,
    pub kind: ActivityKind,
    pub label: String,
    pub detail: String,
    pub state: ActivityState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ActivityFile {
    items: Vec<Activity>,
}

pub struct ActivityLog {
    path: PathBuf,
    file: ActivityFile,
}

impl ActivityLog {
    pub fn load() -> Result<Self> {
        Config::ensure_home()?;
        let path = Self::path()?;
        let file = if path.exists() {
            let raw =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            if raw.trim().is_empty() {
                ActivityFile::default()
            } else {
                serde_json::from_str(&raw).unwrap_or_default()
            }
        } else {
            ActivityFile::default()
        };
        Ok(Self { path, file })
    }

    pub fn path() -> Result<PathBuf> {
        Config::ensure_home()?;
        Ok(Config::home_dir().join("activity.json"))
    }

    pub fn list(&self) -> &[Activity] {
        &self.file.items
    }

    /// Running first, then most recently started. Caps the file so a long
    /// session cannot grow the picker forever.
    pub fn visible(&self) -> Vec<Activity> {
        let mut items = self.file.items.clone();
        items.sort_by(|a, b| {
            let ar = matches!(a.state, ActivityState::Running);
            let br = matches!(b.state, ActivityState::Running);
            br.cmp(&ar).then(b.started_at.cmp(&a.started_at))
        });
        items.truncate(40);
        items
    }

    pub fn start(
        kind: ActivityKind,
        label: impl Into<String>,
        detail: impl Into<String>,
    ) -> Result<String> {
        let path = Self::path()?;
        let _lock = ActivityFileLock::acquire(&path)?;
        let mut log = Self::load()?;
        let id = uuid::Uuid::new_v4().to_string();
        log.file.items.push(Activity {
            id: id.clone(),
            kind,
            label: clip(&label.into(), 72),
            detail: clip(&detail.into(), 120),
            state: ActivityState::Running,
            started_at: Utc::now(),
            finished_at: None,
        });
        log.prune();
        log.save()?;
        Ok(id)
    }

    pub fn finish(id: &str, ok: bool, detail: impl Into<String>) -> Result<()> {
        let path = Self::path()?;
        let _lock = ActivityFileLock::acquire(&path)?;
        let mut log = Self::load()?;
        if let Some(item) = log.file.items.iter_mut().find(|item| item.id == id) {
            item.state = if ok {
                ActivityState::Done
            } else {
                ActivityState::Failed
            };
            item.finished_at = Some(Utc::now());
            let detail = detail.into();
            if !detail.trim().is_empty() {
                item.detail = clip(&detail, 120);
            }
        }
        log.prune();
        log.save()
    }

    fn prune(&mut self) {
        let running: Vec<Activity> = self
            .file
            .items
            .iter()
            .filter(|item| matches!(item.state, ActivityState::Running))
            .cloned()
            .collect();
        let mut done: Vec<Activity> = self
            .file
            .items
            .iter()
            .filter(|item| !matches!(item.state, ActivityState::Running))
            .cloned()
            .collect();
        done.sort_by_key(|item| item.started_at);
        let keep_done = done.len().saturating_sub(24);
        done.drain(..keep_done);
        self.file.items = running.into_iter().chain(done).collect();
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(&self.file)?;
        // Write to a temp file and rename over the target: readers never see
        // a half-written activity.json, and a crash loses nothing.
        let temp = self.path.with_file_name(format!(
            ".{}.{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("activity.json"),
            std::process::id()
        ));
        fs::write(&temp, raw).with_context(|| format!("write {}", temp.display()))?;
        fs::rename(&temp, &self.path).with_context(|| format!("write {}", self.path.display()))
    }
}

fn clip(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_puts_running_before_finished() {
        let log = ActivityLog {
            path: PathBuf::from("activity.json"),
            file: ActivityFile {
                items: vec![
                    sample("a", ActivityState::Done, "2020-01-01T00:00:02Z"),
                    sample("b", ActivityState::Running, "2020-01-01T00:00:01Z"),
                    sample("c", ActivityState::Failed, "2020-01-01T00:00:03Z"),
                ],
            },
        };
        let ids: Vec<_> = log.visible().into_iter().map(|item| item.id).collect();
        assert_eq!(ids[0], "b");
    }

    #[test]
    fn clip_collapses_whitespace() {
        assert_eq!(clip("  cargo   test\n--lib  ", 12), "cargo test -");
    }

    fn sample(id: &str, state: ActivityState, at: &str) -> Activity {
        Activity {
            id: id.into(),
            kind: ActivityKind::Shell,
            label: id.into(),
            detail: String::new(),
            state,
            started_at: at.parse().unwrap(),
            finished_at: None,
        }
    }

    #[test]
    fn stale_lock_is_broken_and_replaced_by_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.json");
        let lock = lock_path(&path);
        fs::write(&lock, b"").unwrap();
        let stale_at = std::time::SystemTime::now()
            .checked_sub(LOCK_STALE_AFTER + Duration::from_secs(5))
            .unwrap();
        let file = OpenOptions::new().write(true).open(&lock).unwrap();
        file.set_modified(stale_at).unwrap();
        drop(file);

        let guard = ActivityFileLock::acquire(&path).unwrap();

        assert_eq!(guard.path, lock);
        assert!(!lock_is_stale(&lock));
        drop(guard);
        assert!(!lock.exists());
    }

    #[test]
    fn save_round_trips_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.json");
        let log = ActivityLog {
            path: path.clone(),
            file: ActivityFile {
                items: vec![sample("a", ActivityState::Running, "2020-01-01T00:00:00Z")],
            },
        };

        log.save().unwrap();

        let file: ActivityFile = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file.items.len(), 1);
        assert_eq!(file.items[0].id, "a");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "temp files left behind: {leftovers:?}");
    }
}
