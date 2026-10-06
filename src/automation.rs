use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::{Duration as StdDuration, Instant};
use uuid::Uuid;

use crate::config::Config;
use crate::runtime::Runtime;
use crate::session::Session;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    At {
        at: DateTime<Utc>,
    },
    Every {
        seconds: u64,
    },
    /// A normalized six-field cron expression: seconds, minutes, hours,
    /// day-of-month, month, day-of-week. `timezone` is currently always
    /// `local`; it is persisted so no implicit UTC conversion is introduced.
    Cron {
        expression: String,
        timezone: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub trigger: Trigger,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub allow_background_tools: bool,
    #[serde(default)]
    pub lease_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TaskFile {
    tasks: Vec<Task>,
}

#[derive(Debug, Clone)]
pub struct TaskStore {
    path: PathBuf,
    file: TaskFile,
}

const LOCK_RETRY: StdDuration = StdDuration::from_millis(10);
const LOCK_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const LOCK_STALE_AFTER: StdDuration = StdDuration::from_secs(60);

struct TaskFileLock {
    path: PathBuf,
}

impl TaskFileLock {
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
                            "timed out waiting for task store lock {}",
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

impl Drop for TaskFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl TaskStore {
    pub fn load() -> Result<Self> {
        Config::ensure_home()?;
        let path = Config::home_dir().join("tasks.json");
        let file = load_file(&path)?;
        Ok(Self { path, file })
    }

    pub fn list(&self) -> &[Task] {
        &self.file.tasks
    }

    pub fn add(
        &mut self,
        name: String,
        prompt: String,
        trigger: Trigger,
        allow_background_tools: bool,
    ) -> Result<Task> {
        anyhow::ensure!(!name.trim().is_empty(), "task name cannot be empty");
        anyhow::ensure!(!prompt.trim().is_empty(), "task prompt cannot be empty");
        if let Trigger::Every { seconds } = trigger {
            anyhow::ensure!(seconds > 0, "every duration must be greater than zero");
        }
        if let Trigger::Cron {
            ref expression,
            ref timezone,
        } = trigger
        {
            anyhow::ensure!(timezone == "local", "cron timezone must be local");
            validate_cron_expression(expression)?;
        }
        let next_run_at = match &trigger {
            Trigger::At { at } => *at,
            Trigger::Every { seconds } => Utc::now() + Duration::seconds(*seconds as i64),
            Trigger::Cron { expression, .. } => next_cron_utc(expression, Local::now())?,
        };
        let task = Task {
            id: Uuid::new_v4().to_string(),
            name,
            prompt,
            trigger,
            next_run_at,
            last_run_at: None,
            enabled: true,
            allow_background_tools,
            lease_until: None,
            run_id: None,
        };
        let result = task.clone();
        self.mutate(|file| {
            file.tasks.push(task);
            Ok(())
        })?;
        Ok(result)
    }

    pub fn find(&self, id: &str) -> Option<&Task> {
        self.file.tasks.iter().find(|task| task.id == id)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        self.mutate(|file| {
            let task = file
                .tasks
                .iter_mut()
                .find(|task| task.id == id)
                .context("task not found")?;
            task.enabled = enabled;
            Ok(())
        })
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        self.mutate(|file| {
            let before = file.tasks.len();
            file.tasks.retain(|task| task.id != id);
            anyhow::ensure!(before != file.tasks.len(), "task not found");
            Ok(())
        })
    }

    pub fn mark_run(&mut self, id: &str, run_id: &str, now: DateTime<Utc>) -> Result<()> {
        self.mutate(|file| {
            let task = file
                .tasks
                .iter_mut()
                .find(|task| task.id == id)
                .context("task not found")?;
            anyhow::ensure!(
                task.run_id.as_deref() == Some(run_id)
                    && task.lease_until.is_some_and(|until| until > now),
                "task lease is no longer owned"
            );
            task.last_run_at = Some(now);
            task.lease_until = None;
            task.run_id = None;
            match task.trigger {
                Trigger::At { .. } => task.enabled = false,
                Trigger::Every { seconds } => {
                    task.next_run_at = now + Duration::seconds(seconds as i64);
                }
                Trigger::Cron {
                    ref expression,
                    ref timezone,
                } => {
                    anyhow::ensure!(timezone == "local", "unsupported cron timezone: {timezone}");
                    let local_now = now.with_timezone(&Local);
                    task.next_run_at = next_cron_utc(expression, local_now)?;
                }
            }
            Ok(())
        })
    }

    pub fn due(&self, now: DateTime<Utc>) -> Vec<Task> {
        self.file
            .tasks
            .iter()
            .filter(|task| {
                task.enabled
                    && task.next_run_at <= now
                    && task.lease_until.is_none_or(|until| until <= now)
            })
            .cloned()
            .collect()
    }

    pub fn claim(&mut self, id: &str, now: DateTime<Utc>) -> Result<String> {
        self.mutate(|file| {
            let task = file
                .tasks
                .iter_mut()
                .find(|task| task.id == id)
                .context("task not found")?;
            if task.lease_until.is_some_and(|until| until > now) {
                anyhow::bail!("task is already running");
            }
            let run_id = Uuid::new_v4().to_string();
            task.lease_until = Some(now + Duration::minutes(5));
            task.run_id = Some(run_id.clone());
            Ok(run_id)
        })
    }

    pub fn release(&mut self, id: &str, run_id: &str) -> Result<()> {
        self.mutate(|file| {
            let task = file
                .tasks
                .iter_mut()
                .find(|task| task.id == id)
                .context("task not found")?;
            if task.run_id.as_deref() == Some(run_id) {
                task.lease_until = None;
                task.run_id = None;
            }
            Ok(())
        })
    }

    fn mutate<T>(&mut self, mutate: impl FnOnce(&mut TaskFile) -> Result<T>) -> Result<T> {
        let _lock = TaskFileLock::acquire(&self.path)?;
        let mut file = load_file(&self.path)?;
        #[cfg(test)]
        if let Ok(delay) = std::env::var("VARYNTH_TASK_MUTATION_DELAY_MS") {
            if let Ok(delay) = delay.parse::<u64>() {
                thread::sleep(StdDuration::from_millis(delay));
            }
        }
        let result = mutate(&mut file)?;
        save_file(&self.path, &file)?;
        self.file = file;
        Ok(result)
    }
}

fn load_file(path: &Path) -> Result<TaskFile> {
    if path.exists() {
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    } else {
        Ok(TaskFile::default())
    }
}

fn save_file(path: &Path, file: &TaskFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tasks.json"),
        std::process::id()
    ));
    fs::write(&temp, serde_json::to_vec_pretty(file)?)?;
    // rename replaces the old file in one step (also on Windows); removing it
    // first would lose every task if the process died in between.
    fs::rename(temp, path)?;
    Ok(())
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tasks.json")
    ))
}

fn lock_is_stale(path: &Path) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > LOCK_STALE_AFTER)
}

pub fn parse_trigger(at: Option<&str>, every_seconds: Option<u64>) -> Result<Trigger> {
    parse_trigger_with_cron(at, every_seconds, None)
}

/// Parse exactly one scheduling mode. CLI cron input accepts the conventional
/// five fields and prepends `0` seconds; six-field input is accepted explicitly.
pub fn parse_trigger_with_cron(
    at: Option<&str>,
    every_seconds: Option<u64>,
    cron: Option<&str>,
) -> Result<Trigger> {
    let modes = at.is_some() as u8 + every_seconds.is_some() as u8 + cron.is_some() as u8;
    anyhow::ensure!(
        modes == 1,
        "choose exactly one of --at, --every-seconds, or --cron"
    );
    if let Some(value) = at {
        return Ok(Trigger::At {
            at: DateTime::parse_from_rfc3339(value)
                .with_context(|| format!("invalid RFC3339 timestamp: {value}"))?
                .with_timezone(&Utc),
        });
    }
    if let Some(seconds) = every_seconds {
        anyhow::ensure!(seconds > 0, "every duration must be greater than zero");
        return Ok(Trigger::Every { seconds });
    }
    let expression = normalize_cron_expression(cron.expect("one mode checked"))?;
    Ok(Trigger::Cron {
        expression,
        timezone: "local".into(),
    })
}

pub fn normalize_cron_expression(expression: &str) -> Result<String> {
    let fields: Vec<_> = expression.split_whitespace().collect();
    let normalized = match fields.len() {
        5 => format!("0 {}", fields.join(" ")),
        6 => fields.join(" "),
        _ => anyhow::bail!(
            "cron expression must have five fields (seconds are prepended) or six explicit fields"
        ),
    };
    validate_cron_expression(&normalized)?;
    Ok(normalized)
}

fn validate_cron_expression(expression: &str) -> Result<()> {
    cron::Schedule::from_str(expression).context("invalid cron expression")?;
    Ok(())
}

/// Pure next occurrence helper. Production passes Local; tests can pass UTC or
/// a fixed offset to verify the expression without depending on machine time.
pub fn next_cron_utc<Tz: TimeZone>(expression: &str, after: DateTime<Tz>) -> Result<DateTime<Utc>> {
    let schedule = cron::Schedule::from_str(expression).context("invalid cron expression")?;
    schedule
        .after(&after)
        .next()
        .map(|time| time.with_timezone(&Utc))
        .context("cron expression has no future occurrence")
}

pub async fn run_due_once(cfg: &Config, cwd: &std::path::Path) -> Result<usize> {
    let now = Utc::now();
    let mut store = TaskStore::load()?;
    let due = store.due(now);
    let mut completed = 0;
    for task in due {
        let run_id = match store.claim(&task.id, now) {
            Ok(run_id) => run_id,
            Err(error) => {
                tracing::debug!(task_id = %task.id, error = %error, "automation task claim skipped");
                continue;
            }
        };
        let mut task_cfg = cfg.clone();
        if !task.allow_background_tools {
            task_cfg.permission_mode = "prompt".into();
        }
        let mut runtime = Runtime::new(task_cfg.clone(), cwd.to_path_buf())?;
        let mut session = Session::new(&cwd.display().to_string(), &task_cfg.model)?;
        match runtime.turn(&mut session, &task.prompt, |_| {}).await {
            Ok(reply) => {
                tracing::info!(task_id = %task.id, reply_len = reply.len(), "automation task completed");
                match store.mark_run(&task.id, &run_id, Utc::now()) {
                    Ok(()) => completed += 1,
                    Err(error) => tracing::warn!(
                        task_id = %task.id,
                        error = %error,
                        "automation task completion skipped because lease was lost"
                    ),
                }
            }
            Err(error) => {
                tracing::warn!(task_id = %task.id, error = %error, "automation task failed");
                store.release(&task.id, &run_id)?;
            }
        }
    }
    Ok(completed)
}

pub async fn run_loop(cfg: Config, cwd: std::path::PathBuf) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        interval.tick().await;
        if let Err(error) = run_due_once(&cfg, &cwd).await {
            tracing::warn!(error = %error, "automation scheduler tick failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn parses_at_and_every_triggers() {
        assert!(matches!(
            parse_trigger(Some("2030-01-01T00:00:00Z"), None).unwrap(),
            Trigger::At { .. }
        ));
        assert_eq!(
            parse_trigger(None, Some(60)).unwrap(),
            Trigger::Every { seconds: 60 }
        );
    }

    #[test]
    fn rejects_ambiguous_trigger() {
        assert!(parse_trigger(Some("2030-01-01T00:00:00Z"), Some(60)).is_err());
    }

    #[test]
    fn lease_hides_claimed_task_until_release() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TaskStore {
            path: dir.path().join("tasks.json"),
            file: TaskFile::default(),
        };
        let task = store
            .add(
                "lease".into(),
                "check".into(),
                Trigger::At { at: Utc::now() },
                false,
            )
            .unwrap();
        let now = Utc::now();
        let run_id = store.claim(&task.id, now).unwrap();
        assert!(store.due(now).is_empty());
        store.release(&task.id, &run_id).unwrap();
        assert_eq!(store.due(now).len(), 1);
    }

    #[test]
    fn stale_run_cannot_complete_reclaimed_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TaskStore {
            path: dir.path().join("tasks.json"),
            file: TaskFile::default(),
        };
        let task = store
            .add(
                "lease-expiry".into(),
                "check".into(),
                Trigger::Every { seconds: 60 },
                false,
            )
            .unwrap();
        let first_now = Utc::now();
        let first_run_id = store.claim(&task.id, first_now).unwrap();
        let second_now = first_now + Duration::minutes(6);
        let second_run_id = store.claim(&task.id, second_now).unwrap();

        assert_ne!(first_run_id, second_run_id);
        assert!(store.mark_run(&task.id, &first_run_id, second_now).is_err());
        assert_eq!(
            store.find(&task.id).unwrap().run_id.as_deref(),
            Some(second_run_id.as_str())
        );
        assert!(store.mark_run(&task.id, &second_run_id, second_now).is_ok());
    }

    #[test]
    fn expired_lease_becomes_claimable_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TaskStore {
            path: dir.path().join("tasks.json"),
            file: TaskFile::default(),
        };
        let task = store
            .add(
                "lease-expiry".into(),
                "check".into(),
                Trigger::Every { seconds: 60 },
                false,
            )
            .unwrap();
        let first_now = Utc::now();
        store.claim(&task.id, first_now).unwrap();
        let expiry = first_now + Duration::minutes(5);

        assert_eq!(store.due(expiry).len(), 1);
        assert!(store.claim(&task.id, expiry).is_ok());
    }

    #[test]
    fn two_processes_cannot_claim_the_same_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let task = Task {
            id: "race-task".into(),
            name: "race".into(),
            prompt: "check".into(),
            trigger: Trigger::Every { seconds: 60 },
            next_run_at: Utc::now(),
            last_run_at: None,
            enabled: true,
            allow_background_tools: false,
            lease_until: None,
            run_id: None,
        };
        fs::write(
            &path,
            serde_json::to_vec(&TaskFile { tasks: vec![task] }).unwrap(),
        )
        .unwrap();
        let ready_dir = dir.path().join("ready");
        fs::create_dir(&ready_dir).unwrap();
        let go = dir.path().join("go");
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for index in 0..2 {
            let ready = ready_dir.join(index.to_string());
            children.push(
                Command::new(&exe)
                    .args([
                        "--exact",
                        "automation::tests::child_claims_task",
                        "--nocapture",
                    ])
                    .env("VARYNTH_RACE_TASK_PATH", &path)
                    .env("VARYNTH_RACE_READY_PATH", ready)
                    .env("VARYNTH_RACE_GO_PATH", &go)
                    .env("VARYNTH_TASK_MUTATION_DELAY_MS", "150")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !(ready_dir.join("0").exists() && ready_dir.join("1").exists()) {
            assert!(
                Instant::now() < deadline,
                "child processes did not become ready"
            );
            thread::sleep(LOCK_RETRY);
        }
        fs::write(&go, b"go").unwrap();
        let outputs = children
            .into_iter()
            .map(|child| child.wait_with_output().unwrap())
            .collect::<Vec<_>>();
        assert!(outputs.iter().all(|output| output.status.success()));
        let claimed = outputs
            .iter()
            .filter(|output| String::from_utf8_lossy(&output.stdout).contains("CLAIMED"))
            .count();
        assert_eq!(claimed, 1, "outputs: {:?}", outputs);
    }

    #[test]
    fn child_claims_task() {
        let Ok(path) = std::env::var("VARYNTH_RACE_TASK_PATH") else {
            return;
        };
        let ready = std::env::var("VARYNTH_RACE_READY_PATH").unwrap();
        let go = std::env::var("VARYNTH_RACE_GO_PATH").unwrap();
        fs::write(ready, b"ready").unwrap();
        let deadline = Instant::now() + StdDuration::from_secs(10);
        while !Path::new(&go).exists() {
            assert!(Instant::now() < deadline, "parent did not release race");
            thread::sleep(LOCK_RETRY);
        }
        let mut store = TaskStore {
            path: PathBuf::from(path),
            file: TaskFile::default(),
        };
        match store.claim("race-task", Utc::now()) {
            Ok(_) => println!("CLAIMED"),
            Err(error) => println!("SKIPPED: {error}"),
        }
    }
}
