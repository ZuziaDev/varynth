//! Client for the local Varynth decisions service.
//!
//! The service speaks the fixed decision wire shape:
//!
//! ```json
//! {"state": {...},
//!  "questions": {
//!    "cat": {"type": "choice", "instructions": "...", "criteria": {"a": "A", "b": null}},
//!    "urg": {"type": "score", "instructions": "...", "criteria": ["Low", "High"]},
//!    "sup": {"type": "noul", "instructions": "..."}
//!  }}
//! ```
//!
//! and answers with `{engine_used, answers}` where every answer carries a type,
//! its decision, a confidence, and per-option probabilities. The service runs
//! Laya and Clef locally; `engine: "auto"` lets the service route.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;

/// Environment override for the service base URL.
pub const URL_ENV: &str = "VARYNTH_DECISIONS_URL";
/// Environment override for the service checkout directory.
pub const DIR_ENV: &str = "VARYNTH_DECISIONS_DIR";
/// Environment override for on-demand autostart (`1`/`0`, `true`/`false`).
pub const AUTOSTART_ENV: &str = "VARYNTH_DECISIONS_AUTOSTART";
/// Default local service base URL.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8791";
/// How long a freshly spawned service gets to answer `/health`.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Health answer from `GET /health`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Health {
    pub laya: bool,
    pub clef: bool,
    /// Proof the listener knows the shared token the CLI generated when it
    /// spawned the service. Present only when the service was started with a
    /// token; loopback listeners that cannot produce it are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
}

impl Health {
    pub fn both(&self) -> bool {
        self.laya && self.clef
    }

    pub fn detail(&self) -> String {
        format!(
            "laya={} clef={}",
            if self.laya { "ready" } else { "missing" },
            if self.clef { "ready" } else { "missing" }
        )
    }
}

/// One decision request. `state` is any JSON value (a string, object, or array).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionRequest {
    /// `auto` (default), `laya`, or `clef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    pub state: Value,
    pub questions: Map<String, Value>,
}

impl DecisionRequest {
    pub fn new(state: Value, questions: Map<String, Value>) -> Self {
        Self {
            engine: None,
            state,
            questions,
        }
    }

    pub fn with_engine(mut self, engine: impl Into<String>) -> Self {
        self.engine = Some(engine.into());
        self
    }
}

/// One answered question, normalised by the service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionAnswer {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub choice: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub noul: Option<f64>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub legend: Option<Map<String, Value>>,
    #[serde(default)]
    pub probabilities: Map<String, Value>,
}

/// A full service answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub engine_used: String,
    pub answers: Map<String, Value>,
    #[serde(default)]
    pub routing: Option<Value>,
    #[serde(default)]
    pub usage: Option<Value>,
}

impl DecisionResponse {
    /// Typed answers, useful to callers that do not want raw JSON.
    pub fn typed_answers(&self) -> Vec<(String, DecisionAnswer)> {
        self.answers
            .iter()
            .filter_map(|(name, value)| {
                serde_json::from_value::<DecisionAnswer>(value.clone())
                    .ok()
                    .map(|answer| (name.clone(), answer))
            })
            .collect()
    }
}

/// Base URL for the service: `VARYNTH_DECISIONS_URL`, then `decisions_url`, then the default.
pub fn base_url(cfg: &Config) -> String {
    let configured = std::env::var(URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| cfg.decisions_url.clone());
    let trimmed = configured.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_URL.to_string()
    } else {
        trimmed.to_string()
    }
}

fn unavailable(url: &str, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "decisions service unavailable at {url}: {error}\n   \
         start it with: varynth decisions start\n   \
         or set `decisions_url` / {URL_ENV}"
    )
}

/// Probe the local service. Short timeout so `doctor` stays snappy.
pub async fn health(cfg: &Config) -> Result<Health> {
    let url = base_url(cfg);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    let mut request = client.get(format!("{url}/health"));
    if let Some(token) = read_token() {
        request = request
            .header("X-Varynth-Decisions-Token", token.as_str())
            .header("Authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .map_err(|error| unavailable(&url, error))?;
    let response = response
        .error_for_status()
        .map_err(|error| unavailable(&url, error))?;
    response
        .json::<Health>()
        .await
        .map_err(|error| unavailable(&url, format!("bad health response: {error}")))
}

/// Send one decision request. Clef on CPU can take minutes, and its first
/// request also streams the release shards into int8, so the client timeout is
/// deliberately generous (one hour).
pub async fn decide(cfg: &Config, request: &DecisionRequest) -> Result<DecisionResponse> {
    let url = base_url(cfg);
    // Never hand source/state to a listener that cannot prove it is ours:
    // anything can bind the loopback port before the real service starts.
    let observed = health(cfg).await?;
    verify_listener(&url, &observed)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3600))
        .build()?;
    let mut post = client.post(format!("{url}/decide")).json(request);
    if let Some(token) = read_token() {
        post = post
            .header("X-Varynth-Decisions-Token", token.as_str())
            .header("Authorization", format!("Bearer {token}"));
    }
    let response = post
        .send()
        .await
        .map_err(|error| unavailable(&url, error))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(body);
        anyhow::bail!("decisions service returned {status}: {detail}");
    }
    response
        .json::<DecisionResponse>()
        .await
        .context("decisions service returned an unexpected body")
}

/// Parse the CLI's request JSON from an inline string or a file.
pub fn load_request_file(path: &Path) -> Result<DecisionRequest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read decision request {path:?}"))?;
    parse_request(&text)
}

/// Parse the inline request JSON.
pub fn parse_request(text: &str) -> Result<DecisionRequest> {
    let value: Value = serde_json::from_str(text).context("decision request is not valid JSON")?;
    let object = value
        .as_object()
        .context("decision request must be a JSON object")?;
    let state = object
        .get("state")
        .cloned()
        .context("decision request needs a `state` field")?;
    let questions = object
        .get("questions")
        .and_then(Value::as_object)
        .cloned()
        .context("decision request needs a `questions` object")?;
    let engine = object
        .get("engine")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(DecisionRequest {
        engine,
        state,
        questions,
    })
}

/// Parse `--state`: JSON when it parses, otherwise the literal text.
pub fn parse_state(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

// ---------------------------------------------------------------------------
// Service lifecycle
// ---------------------------------------------------------------------------

/// `true` when `url` points at this machine.
pub fn is_loopback(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(parsed) => match parsed.host_str() {
            Some(host) => {
                let host = host.trim_matches(|c| c == '[' || c == ']');
                matches!(host, "127.0.0.1" | "localhost" | "::1")
            }
            None => false,
        },
        Err(_) => false,
    }
}

/// Accept the usual spellings of a boolean environment value.
pub fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `host:port` the service listens on, derived from the configured URL.
fn endpoint_parts(url: &str) -> Option<(String, u16)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed
        .host_str()?
        .trim_matches(|c| c == '[' || c == ']')
        .to_string();
    let port = parsed.port_or_known_default()?;
    Some((host, port))
}

fn connect_address(url: &str) -> Option<(String, u16)> {
    let (host, port) = endpoint_parts(url)?;
    let host = if host == "localhost" {
        "127.0.0.1".to_string()
    } else {
        host
    };
    Some((host, port))
}

/// Is anything already listening? Used to refuse spawning a duplicate.
fn port_in_use(url: &str) -> bool {
    let Some((host, port)) = connect_address(url) else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok())
}

fn push_ancestor_roots(start: &Path, out: &mut Vec<PathBuf>) {
    for ancestor in start.ancestors().take(6) {
        // Never treat a volume root as an install root: it would make every
        // path on the drive "owned".
        if ancestor.parent().is_none() {
            break;
        }
        out.push(ancestor.to_path_buf());
    }
}

/// Candidate roots that may contain `services/decisions/server.py`.
///
/// Only the explicit override and the tree this binary was installed into are
/// considered. The process cwd is deliberately absent: a cloned repository
/// must never be able to supply the Python interpreter autostart runs.
fn service_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(explicit) = std::env::var_os(DIR_ENV).filter(|value| !value.is_empty()) {
        out.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            push_ancestor_roots(parent, &mut out);
        }
    }
    out
}

/// Canonical containment: `path` is `root` or lives below it.
fn path_within(path: &Path, root: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    path == root || path.starts_with(&root)
}

/// Paths a service may be autostarted from: the varynth home and the
/// directory tree holding the running binary. A workspace checkout is only
/// reachable when the binary itself was built/installed there.
fn user_owned_install(dir: &Path) -> bool {
    if path_within(dir, &Config::home_dir()) {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(parent) = exe.parent() else {
        return false;
    };
    parent
        .ancestors()
        .take(6)
        .filter(|root| root.parent().is_some())
        .any(|root| path_within(dir, root))
}

/// Resolve the `services/decisions` directory (the one holding `server.py`).
pub fn service_dir() -> Option<PathBuf> {
    for candidate in service_dir_candidates() {
        for dir in [
            candidate.clone(),
            candidate.join("services").join("decisions"),
        ] {
            if dir.join("server.py").is_file() {
                // Autostart re-checks `user_owned_install`; an explicit start
                // may use a path the user configured with DIR_ENV.
                return Some(dunce_like(&dir));
            }
        }
    }
    None
}

fn dunce_like(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        // `canonicalize` returns `\\?\D:\...`; plain drive paths are friendlier
        // in messages and accepted by every spawn API we use.
        if let Some(text) = canonical.to_str() {
            if let Some(stripped) = text.strip_prefix(r"\\?") {
                let stripped = stripped.strip_prefix('\\').unwrap_or(stripped);
                if stripped.as_bytes().get(1) == Some(&b':') {
                    return PathBuf::from(stripped);
                }
            }
        }
    }
    canonical
}

/// Interpreter the README installs into `.venv`.
pub fn python_path(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.join(".venv").join("Scripts").join("python.exe")
    } else {
        dir.join(".venv").join("bin").join("python")
    }
}

/// Log every spawned service writes to.
pub fn log_path() -> PathBuf {
    Config::home_dir().join("logs").join("decisions.log")
}

fn state_path() -> PathBuf {
    state_path_in(&Config::home_dir())
}

fn state_path_in(home: &Path) -> PathBuf {
    home.join("decisions.pid.json")
}

/// Environment variable carrying the shared token to the spawned service.
pub const TOKEN_ENV: &str = "VARYNTH_DECISIONS_TOKEN";

fn token_path() -> PathBuf {
    Config::home_dir().join("decisions.token")
}

/// Shared secret the CLI and the spawned service know. Created 0600 on first
/// spawn so another local account cannot drive or impersonate the service.
fn ensure_token() -> Result<String> {
    crate::dashboard::load_or_create_token(&token_path())
}

/// Environment handed to the spawned service.
///
/// The service is a third-party-shaped Python process: it needs an
/// interpreter `PATH`, its venv and the shared token, nothing else. The CLI's
/// own environment carries provider keys and `VARYNTH_*TOKEN*` values, so the
/// child gets the same allowlist every other child does instead of inheriting
/// the parent's.
pub(crate) fn service_env(
    dir: &Path,
    token: &str,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    crate::sandbox::child_env_with(&[
        ("PYTHONUNBUFFERED".to_string(), "1".to_string()),
        (
            "VIRTUAL_ENV".to_string(),
            dir.join(".venv").display().to_string(),
        ),
        (TOKEN_ENV.to_string(), token.to_string()),
    ])
}

fn read_token() -> Option<String> {
    let text = std::fs::read_to_string(token_path()).ok()?;
    let token = text.trim().to_string();
    (token.len() >= 32).then_some(token)
}

/// Value a genuine service echoes in `/health`; derived from the shared
/// token, so a listener that merely answers `{laya,clef}` cannot forge it.
fn health_proof(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"varynth-decisions-health:");
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Refuse to send state to an unauthenticated loopback listener.
///
/// A token-backed service must prove it knows the shared token. When no token
/// exists yet, only a process this CLI recorded (live pid + creation marker)
/// is trusted.
fn verify_listener(url: &str, health: &Health) -> Result<()> {
    let managed = read_state();
    verify_listener_with(url, health, read_token().as_deref(), managed.as_ref())
}

fn verify_listener_with(
    url: &str,
    health: &Health,
    token: Option<&str>,
    managed: Option<&ManagedProcess>,
) -> Result<()> {
    if !is_loopback(url) {
        // Remote services are identified by the configured URL.
        return Ok(());
    }
    if let Some(token) = token {
        let expected = health_proof(token);
        return match health.proof.as_deref() {
            Some(observed) if observed.eq_ignore_ascii_case(&expected) => Ok(()),
            _ => anyhow::bail!(
                "refusing to use {url}: the listener could not prove it is the varynth \
                 decisions service (token mismatch).\n   \
                 stop that listener and run `varynth decisions start`."
            ),
        };
    }
    let managed = managed
        .filter(|state| state.url == url)
        .is_some_and(managed_alive);
    anyhow::ensure!(
        managed,
        "refusing to use {url}: no varynth-managed service record and no shared token.\n   \
         run `varynth decisions start` so varynth starts and authenticates the service."
    );
    Ok(())
}

/// A service process this CLI started.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedProcess {
    pid: u32,
    url: String,
    server: String,
    /// Process creation marker so a recycled PID is never killed.
    #[serde(default)]
    marker: Option<String>,
    /// Child processes observed once the service was healthy (the Windows venv
    /// launcher runs the real interpreter as a child).
    #[serde(default)]
    children: Vec<ManagedChild>,
    started: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedChild {
    pid: u32,
    #[serde(default)]
    marker: Option<String>,
}

fn read_state() -> Option<ManagedProcess> {
    read_state_at(&state_path())
}

fn read_state_at(path: &Path) -> Option<ManagedProcess> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state_at(path: &Path, state: &ManagedProcess) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(state)?;
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

fn clear_state() {
    let _ = std::fs::remove_file(state_path());
}

/// Remove the record only when it still names the process this call started.
/// A concurrent starter's record is never cleared by the loser.
fn clear_state_if_ours_at(path: &Path, pid: u32, marker: Option<&str>) {
    let Some(current) = read_state_at(path) else {
        return;
    };
    if current.pid == pid && current.marker.as_deref() == marker {
        let _ = std::fs::remove_file(path);
    }
}

/// Refresh the record only while it still belongs to this process.
fn update_state_if_ours_at(path: &Path, state: &ManagedProcess) -> Result<()> {
    let Some(current) = read_state_at(path) else {
        return Ok(());
    };
    if current.pid == state.pid && current.marker == state.marker {
        write_state_at(path, state)?;
    }
    Ok(())
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == 259 // STILL_ACTIVE
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

/// Creation marker for `pid`: FILETIME on Windows, starttime tick on Linux.
/// `None` when the process is gone or the marker is unavailable.
#[cfg(windows)]
fn process_marker(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let empty = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (empty, empty, empty, empty);
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        if ok == 0 {
            None
        } else {
            Some(
                ((creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64)
                    .to_string(),
            )
        }
    }
}

#[cfg(unix)]
fn process_marker(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19).map(str::to_string)
}

/// Our record still names the same live process.
fn managed_alive(state: &ManagedProcess) -> bool {
    pid_alive(state.pid) && marker_matches(state.pid, state.marker.as_deref())
}

fn marker_matches(pid: u32, expected: Option<&str>) -> bool {
    match (expected, process_marker(pid)) {
        (Some(expected), Some(actual)) => expected == actual,
        // Marker unavailable: treat the recorded pid as alive to avoid
        // orphaning our child; the creation marker is advisory only.
        _ => true,
    }
}

/// Direct and transitive children of `pid`.
#[cfg(windows)]
fn descendant_pids(root: u32) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &pairs {
            if *ppid == parent && *pid != root && !out.contains(pid) {
                out.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    out
}

#[cfg(unix)]
fn descendant_pids(_root: u32) -> Vec<u32> {
    // The service runs in its own process group; `terminate` signals the group.
    Vec::new()
}

/// The report `decisions status` prints.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub url: String,
    pub reachable: bool,
    pub health: Option<Health>,
    pub error: Option<String>,
    pub autostart: bool,
    pub loopback: bool,
    pub service_dir: Option<String>,
    pub server_path: Option<String>,
    pub python_path: Option<String>,
    pub service_found: bool,
    pub python_found: bool,
    pub managed_pid: Option<u32>,
    pub log: String,
}

impl ServiceStatus {
    /// One-line summary for `doctor`.
    pub fn detail(&self) -> String {
        let state = match (&self.health, self.reachable) {
            (Some(health), true) => health.detail(),
            _ => format!(
                "unreachable ({})",
                self.error.as_deref().unwrap_or("no response")
            ),
        };
        let service = match &self.server_path {
            Some(path) => format!("service={path}"),
            None => "service=not found".to_string(),
        };
        let python = match (&self.python_path, self.python_found) {
            (Some(path), true) => format!("python={path}"),
            (Some(path), false) => format!("python missing ({path})"),
            (None, _) => "python=not checked".to_string(),
        };
        format!(
            "{} {}; autostart={}; {service}; {python}",
            self.url,
            state,
            if self.autostart { "on" } else { "off" }
        )
    }

    /// Everything needed to start the service on demand is present.
    pub fn can_autostart(&self) -> bool {
        self.loopback && self.autostart && self.service_found && self.python_found
    }
}

/// Inspect the service without changing anything.
pub async fn status(cfg: &Config) -> ServiceStatus {
    let url = base_url(cfg);
    let health = health(cfg).await;
    let (reachable, health, error) = match health {
        Ok(health) => (true, Some(health), None),
        Err(error) => (false, None, Some(error.to_string())),
    };
    let dir = service_dir();
    let server = dir.as_ref().map(|dir| dir.join("server.py"));
    let python = dir.as_ref().map(|dir| python_path(dir));
    ServiceStatus {
        loopback: is_loopback(&url),
        reachable,
        health,
        error,
        autostart: autostart_enabled(cfg),
        service_found: server.as_ref().is_some_and(|path| path.is_file()),
        python_found: python.as_ref().is_some_and(|path| path.is_file()),
        service_dir: dir.map(|dir| dir.display().to_string()),
        server_path: server.map(|path| path.display().to_string()),
        python_path: python.map(|path| path.display().to_string()),
        managed_pid: read_state()
            .filter(|state| state.url == url)
            .and_then(|state| {
                if managed_alive(&state) {
                    Some(state.pid)
                } else {
                    state
                        .children
                        .iter()
                        .find(|child| {
                            pid_alive(child.pid)
                                && marker_matches(child.pid, child.marker.as_deref())
                        })
                        .map(|child| child.pid)
                }
            }),
        log: log_path().display().to_string(),
        url,
    }
}

/// What `decisions start` reports.
#[derive(Debug, Clone, Serialize)]
pub struct StartReport {
    /// `started` or `already_running`.
    pub outcome: String,
    pub url: String,
    pub health: Health,
    pub pid: Option<u32>,
    pub log: String,
}

/// What `decisions stop` reports.
#[derive(Debug, Clone, Serialize)]
pub struct StopReport {
    pub stopped: bool,
    pub pid: Option<u32>,
    pub detail: String,
}

fn missing_service_error(url: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "decisions service at {url} is not running and varynth could not find \
         services/decisions/server.py in its install directory.\n   \
         set {DIR_ENV} to the services/decisions directory of your checkout, or see \
         \"Start\" in services/decisions/README.md"
    )
}

fn missing_venv_error(url: &str, dir: &Path, python: &Path) -> anyhow::Error {
    let setup = if cfg!(windows) {
        format!(
            "cd {dir}\n   py -m venv .venv\n   py -m pip --python {python} install -r requirements.txt",
            dir = dir.display(),
            python = python.display()
        )
    } else {
        format!(
            "cd {dir}\n   python3 -m venv .venv\n   .venv/bin/pip install -r requirements.txt",
            dir = dir.display()
        )
    };
    anyhow::anyhow!(
        "decisions service at {url} is not running and its virtualenv is missing:\n   \
         {python}\n   \
         one-time setup (\"Start\" in services/decisions/README.md):\n   {setup}\n   \
         then run: varynth decisions start",
        python = python.display()
    )
}

/// Effective autostart switch: environment override, else config.
pub fn autostart_enabled(cfg: &Config) -> bool {
    std::env::var(AUTOSTART_ENV)
        .ok()
        .and_then(|value| parse_bool(&value))
        .unwrap_or(cfg.decisions_autostart)
}

/// Probe, and when unreachable spawn the local service on demand.
///
/// Automatic spawn only happens for a loopback URL with autostart enabled.
pub async fn ensure_running(cfg: &Config) -> Result<Health> {
    Ok(start_service(cfg, false).await?.health)
}

/// Start the service if it is not already running, ignoring `decisions_autostart`.
pub async fn start(cfg: &Config) -> Result<StartReport> {
    start_service(cfg, true).await
}

/// Take the state lock and report a live record, if any. The returned lock is
/// held across spawn + record so two consumers cannot both spawn.
fn claim_start_at(home: &Path) -> Result<(crate::control_bus::FileLock, Option<ManagedProcess>)> {
    let lock = crate::control_bus::FileLock::acquire(home.join("decisions.pid.lock"))?;
    let existing = read_state_at(&state_path_in(home)).filter(managed_alive);
    Ok((lock, existing))
}

async fn start_service(cfg: &Config, explicit: bool) -> Result<StartReport> {
    let url = base_url(cfg);
    let log = log_path().display().to_string();
    // A listener that answers /health but cannot prove it knows the shared
    // token is an impostor; report that distinctly instead of pretending the
    // service is simply down.
    let mut unauthenticated_listener = false;
    let initial_error = match health(cfg).await {
        Ok(observed) => {
            if verify_listener(&url, &observed).is_ok() {
                let managed = read_state().filter(managed_alive);
                return Ok(StartReport {
                    outcome: "already_running".into(),
                    url,
                    health: observed,
                    pid: managed.map(|state| state.pid),
                    log,
                });
            }
            unauthenticated_listener = true;
            unavailable(
                &url,
                "the listener answered /health but failed authentication",
            )
        }
        Err(error) => error,
    };

    if !explicit && !autostart_enabled(cfg) {
        anyhow::bail!(
            "{initial_error}\n   \
             autostart is off (`decisions_autostart = false` or {AUTOSTART_ENV}=0); \
             run `varynth decisions start` or start the service yourself"
        );
    }
    if !is_loopback(&url) {
        anyhow::bail!(
            "{initial_error}\n   \
             autostart only manages a loopback service; start the remote one yourself \
             or change `decisions_url` / {URL_ENV}"
        );
    }

    // Serialize check → spawn → record across processes: two consumers racing
    // to autostart must produce exactly one service and one record. A loser
    // re-reads the winner's record under the lock and waits for it instead of
    // spawning or overwriting.
    let home = Config::home_dir();
    let pid_path = state_path_in(&home);
    let (pid_lock, existing) = claim_start_at(&home)?;

    // A process we started earlier for this URL is alive but not healthy yet
    // (or at all): wait for it instead of spawning a duplicate. A service we
    // started for a different URL is never overwritten silently.
    if let Some(state) = existing {
        if state.url != url {
            anyhow::bail!(
                "a decisions service started by varynth is still running at {} (pid {});\n   \
                 run `varynth decisions stop` before switching to {url}",
                state.url,
                state.pid
            );
        }
        // Wait outside the lock: the process that owns the record may still
        // be starting up, and holding the lock would block it.
        drop(pid_lock);
        let health = wait_for_health(&url, START_TIMEOUT, None)
            .await
            .with_context(|| {
                format!(
                    "decisions service (pid {}) did not answer; see {log}",
                    state.pid
                )
            })?;
        verify_listener(&url, &health)?;
        return Ok(StartReport {
            outcome: "already_running".into(),
            url,
            health,
            pid: Some(state.pid),
            log,
        });
    }

    if port_in_use(&url) {
        if unauthenticated_listener {
            anyhow::bail!(
                "decisions service at {url} answered /health but could not prove it is the \
                 varynth decisions service (token mismatch); refusing to send it any state.\n   \
                 stop that listener and run `varynth decisions start`."
            );
        }
        anyhow::bail!(
            "decisions service at {url} is not answering and port {} is already in use by \
             a process varynth has no record of starting; refusing to spawn a duplicate.\n   \
             stop that process, or change `decisions_url` / {URL_ENV}",
            endpoint_parts(&url)
                .map(|(_, port)| port.to_string())
                .unwrap_or_default()
        );
    }

    let dir = service_dir().ok_or_else(|| missing_service_error(&url))?;
    if !explicit && !user_owned_install(&dir) {
        anyhow::bail!(
            "decisions autostart refuses to run {}: it is outside the varynth install \
             directory and was not confirmed.\n   \
             run `varynth decisions start` to confirm that path, or set {DIR_ENV} to a \
             trusted checkout",
            dir.display()
        );
    }
    let python = python_path(&dir);
    if !python.is_file() {
        return Err(missing_venv_error(&url, &dir, &python));
    }
    anyhow::ensure!(
        path_within(&python, &dir.join(".venv")),
        "decisions interpreter {} resolves outside its venv; refusing to run it",
        python.display()
    );
    let server = dir.join("server.py");
    anyhow::ensure!(
        path_within(&server, &dir),
        "decisions server.py resolves outside {}; refusing to run it",
        dir.display()
    );
    // Shared secret: the spawned service requires it and proves it knows it.
    let token = ensure_token()?;
    let (bind_host, port) = connect_address(&url).unwrap_or_else(|| ("127.0.0.1".into(), 8791));
    let port = port.to_string();

    // Append so repeated starts keep one history.
    if let Some(parent) = log_path().parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create log directory {}", parent.display()))?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .with_context(|| format!("cannot open {}", log_path().display()))?;
    let log_err = log_file.try_clone()?;

    let mut command = Command::new(&python);
    command
        .arg(&server)
        .args(["--host", &bind_host])
        .args(["--port", &port])
        .current_dir(&dir)
        .env_clear()
        .envs(service_env(&dir, &token))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console flash, and Ctrl+C in the parent's console stays out.
        command.creation_flags(0x0800_0000 | 0x0000_0200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group so the service outlives the CLI and its signals.
        command.process_group(0);
    }
    #[cfg(windows)]
    let _inherit_guard = StdioInheritGuard::disarm();
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start {}", python.display()))?;
    let pid = child.id();
    let mut state = ManagedProcess {
        pid,
        url: url.clone(),
        server: server.display().to_string(),
        marker: process_marker(pid),
        children: Vec::new(),
        started: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(error) = write_state_at(&pid_path, &state) {
        let _ = child.kill();
        return Err(error);
    }
    // The record is published: release the lock so a concurrent starter can
    // observe it and wait for this child instead of spawning its own.
    drop(pid_lock);

    let health = match wait_for_health(&url, START_TIMEOUT, Some(&mut child)).await {
        Ok(health) => health,
        Err(error) => {
            // The child is ours and failed to come up; do not leave it behind.
            terminate(pid).ok();
            clear_state_if_ours_at(&pid_path, pid, state.marker.as_deref());
            return Err(error.context(format!(
                "decisions service did not start; full output: {log}"
            )));
        }
    };
    if let Err(error) = verify_listener(&url, &health) {
        terminate(pid).ok();
        clear_state_if_ours_at(&pid_path, pid, state.marker.as_deref());
        return Err(error);
    }
    // The Windows venv launcher runs the real interpreter as a child; record
    // every process so `stop` can verify and end exactly this tree.
    state.children = descendant_pids(pid)
        .into_iter()
        .map(|child| ManagedChild {
            pid: child,
            marker: process_marker(child),
        })
        .collect();
    if let Err(error) = update_state_if_ours_at(&pid_path, &state) {
        terminate(pid).ok();
        clear_state_if_ours_at(&pid_path, pid, state.marker.as_deref());
        return Err(error);
    }
    Ok(StartReport {
        outcome: "started".into(),
        url,
        health,
        pid: Some(pid),
        log,
    })
}

async fn wait_for_health(
    url: &str,
    timeout: Duration,
    mut child: Option<&mut Child>,
) -> Result<Health> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    loop {
        if let Some(child) = child.as_deref_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                anyhow::bail!(
                    "decisions service exited during startup ({status}); last lines of {}:\n{}",
                    log_path().display(),
                    log_tail()
                );
            }
        }
        let token = read_token();
        let mut request = client.get(format!("{url}/health"));
        if let Some(token) = token.as_deref() {
            request = request
                .header("X-Varynth-Decisions-Token", token)
                .header("Authorization", format!("Bearer {token}"));
        }
        let observed = match request.send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<Health>().await {
                    Ok(health) if verify_listener(url, &health).is_ok() => return Ok(health),
                    Ok(_) => "listener failed authentication".to_string(),
                    Err(_) => "bad health response".to_string(),
                }
            }
            Ok(response) => format!("HTTP {}", response.status()),
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            anyhow::bail!(
                "decisions service at {url} did not become healthy within {}s ({observed})",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Windows spawn inherits every inheritable handle in this process. The
/// service outlives the CLI, so an inherited stdout pipe would keep a caller's
/// pipeline (`varynth decisions start | grep ...`) open forever. Mark our std
/// handles non-inheritable for the duration of the spawn, then restore them.
#[cfg(windows)]
struct StdioInheritGuard {
    saved: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

#[cfg(windows)]
impl StdioInheritGuard {
    fn disarm() -> Self {
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut saved = Vec::new();
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(id) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut flags = 0u32;
            if unsafe { GetHandleInformation(handle, &mut flags) } != 0
                && flags & HANDLE_FLAG_INHERIT != 0
                && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0
            {
                saved.push((handle, flags));
            }
        }
        Self { saved }
    }
}

#[cfg(windows)]
impl Drop for StdioInheritGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        for (handle, flags) in &self.saved {
            let _ = unsafe {
                SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT)
            };
        }
    }
}

/// Last few KB of the service log, for actionable start failures.
fn log_tail() -> String {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 4000;
    let path = log_path();
    let Ok(mut file) = std::fs::File::open(&path) else {
        return "(no log yet)".into();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if len > TAIL && file.seek(SeekFrom::Start(len - TAIL)).is_err() {
        return format!("(cannot read {})", path.display());
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return format!("(cannot read {})", path.display());
    }
    let text = String::from_utf8_lossy(&bytes);
    if text.trim().is_empty() {
        "(log is empty)".into()
    } else {
        text.into_owned()
    }
}

/// Stop the service, but only when this CLI started it.
pub fn stop() -> Result<StopReport> {
    let Some(state) = read_state() else {
        return Ok(StopReport {
            stopped: false,
            pid: None,
            detail: "no decisions service was started by varynth; nothing to stop".into(),
        });
    };
    let root_alive = pid_alive(state.pid);
    if root_alive && !marker_matches(state.pid, state.marker.as_deref()) {
        clear_state();
        return Ok(StopReport {
            stopped: false,
            pid: Some(state.pid),
            detail: format!(
                "pid {} now belongs to a different process; refusing to stop it",
                state.pid
            ),
        });
    }

    // Children first (deepest recorded/present descendants), root last. Every
    // recorded child must still match its creation marker.
    let mut targets: Vec<u32> = state
        .children
        .iter()
        .filter(|child| pid_alive(child.pid) && marker_matches(child.pid, child.marker.as_deref()))
        .map(|child| child.pid)
        .collect();
    if root_alive {
        for child in descendant_pids(state.pid) {
            if !targets.contains(&child) && child != state.pid {
                targets.push(child);
            }
        }
    }

    if !root_alive && targets.is_empty() {
        if !port_in_use(&state.url) {
            clear_state();
            return Ok(StopReport {
                stopped: false,
                pid: Some(state.pid),
                detail: format!(
                    "recorded service (pid {}) is no longer running; cleared the stale record",
                    state.pid
                ),
            });
        }
        return Ok(StopReport {
            stopped: false,
            pid: Some(state.pid),
            detail: format!(
                "recorded launcher (pid {}) is gone and {} still accepts connections from a process varynth cannot attribute; not stopping it",
                state.pid, state.url
            ),
        });
    }

    let mut failures = Vec::new();
    for child in &targets {
        if let Err(error) = kill_pid(*child) {
            if pid_alive(*child) {
                failures.push(error.to_string());
            }
        }
    }
    if root_alive {
        if let Err(error) = kill_pid(state.pid) {
            failures.push(format!("pid {}: {error}", state.pid));
        }
    }

    // Killing a process is asynchronous: wait for every recorded pid, then for
    // the port, so "stopped" is only reported when the service is really gone.
    let mut all_targets = targets.clone();
    all_targets.push(state.pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    while all_targets.iter().any(|pid| pid_alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let port_deadline = Instant::now() + Duration::from_secs(5);
    while port_in_use(&state.url) && Instant::now() < port_deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let alive: Vec<u32> = all_targets
        .iter()
        .copied()
        .filter(|pid| pid_alive(*pid))
        .collect();
    let port_busy = port_in_use(&state.url);

    if alive.is_empty() && !port_busy {
        clear_state();
        return Ok(StopReport {
            stopped: true,
            pid: Some(state.pid),
            detail: format!("stopped decisions service (pid {})", state.pid),
        });
    }
    let mut detail = Vec::new();
    if !alive.is_empty() {
        detail.push(format!(
            "still running: {}",
            alive
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if port_busy {
        detail.push(format!("{} still accepts connections", state.url));
    }
    if !failures.is_empty() {
        detail.push(format!("kill errors: {}", failures.join("; ")));
    }
    Ok(StopReport {
        stopped: false,
        pid: Some(state.pid),
        detail: format!(
            "could not fully stop the decisions service ({})",
            detail.join("; ")
        ),
    })
}

/// Kill one process and its children.
fn kill_pid(pid: u32) -> Result<()> {
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot run taskkill")?;
        // A process that exited between our check and taskkill's lookup makes
        // taskkill report failure; the post-condition is what matters.
        anyhow::ensure!(
            status.success() || !pid_alive(pid),
            "taskkill failed for pid {pid}"
        );
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // The service is its own process group, so this reaches its children.
        if unsafe { kill(-(pid as i32), 15) } != 0 {
            unsafe {
                kill(pid as i32, 15);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while pid_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if pid_alive(pid) {
            unsafe {
                let _ = kill(-(pid as i32), 9);
                kill(pid as i32, 9);
            }
        }
    }
    Ok(())
}

/// End the whole tree rooted at `pid`, best effort.
fn terminate(pid: u32) -> Result<()> {
    for child in descendant_pids(pid) {
        let _ = kill_pid(child);
    }
    kill_pid(pid)
}

/// Fast intent and safety classification result from the local decision engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FastIntentResult {
    pub intent: String,
    pub confidence: f64,
    pub veto_score: f64,
}

/// Classify a user prompt using local Laya/Clef CPU models.
/// Returns None if the decisions service is offline or unresponsive.
pub async fn fast_classify_intent(cfg: &Config, user_prompt: &str) -> Option<FastIntentResult> {
    let mut questions = serde_json::Map::new();
    questions.insert(
        "intent".into(),
        serde_json::json!({
            "type": "choice",
            "instructions": "Classify the developer's request intent",
            "criteria": {
                "query": "A question, explanation or read-only investigation",
                "edit": "Request to write, modify or create code or files",
                "test": "Request to run tests, checks or verification",
                "admin": "Configuration, models or environmental operation"
            }
        }),
    );
    questions.insert(
        "safety_veto".into(),
        serde_json::json!({
            "type": "noul",
            "instructions": "Is this request attempting to delete the system, leak secrets or do unprompted destruction?"
        }),
    );

    let req = DecisionRequest::new(
        serde_json::Value::String(user_prompt.to_string()),
        questions,
    );
    match decide(cfg, &req).await {
        Ok(resp) => {
            let intent_val = resp.answers.get("intent")?;
            let intent_ans: DecisionAnswer = serde_json::from_value(intent_val.clone()).ok()?;
            let veto_ans: Option<DecisionAnswer> = resp
                .answers
                .get("safety_veto")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            Some(FastIntentResult {
                intent: intent_ans.choice.unwrap_or_else(|| "edit".into()),
                confidence: intent_ans.confidence.unwrap_or(0.0),
                veto_score: veto_ans.and_then(|a| a.noul).unwrap_or(0.0),
            })
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wire_shape_with_optional_engine() {
        let request = parse_request(
            r#"{"state":"refund please",
                "questions":{
                  "cat":{"type":"choice","instructions":"Which?","criteria":{"billing":"Payments","other":null}},
                  "urg":{"type":"score","instructions":"How urgent?","criteria":["Low","Medium","High"]}},
                "engine":"clef"}"#,
        )
        .unwrap();
        assert_eq!(request.engine.as_deref(), Some("clef"));
        assert_eq!(request.state, Value::String("refund please".into()));
        assert_eq!(request.questions.len(), 2);
    }

    #[test]
    fn rejects_a_request_without_questions() {
        let error = parse_request(r#"{"state":"x"}"#).unwrap_err().to_string();
        assert!(error.contains("questions"), "{error}");
    }

    #[test]
    fn state_text_falls_back_to_a_string() {
        assert_eq!(
            parse_state("refund please"),
            Value::String("refund please".into())
        );
        assert_eq!(parse_state(r#"{"a":1}"#), serde_json::json!({"a": 1}));
    }

    #[test]
    fn answer_round_trips_choice_probabilities() {
        let response: DecisionResponse = serde_json::from_value(serde_json::json!({
            "engine_used": "laya",
            "answers": {"cat": {"type": "choice", "choice": "billing",
                "confidence": 0.91, "probabilities": {"billing": 0.91, "other": 0.09}}}
        }))
        .unwrap();
        assert_eq!(response.engine_used, "laya");
        let typed = response.typed_answers();
        assert_eq!(typed[0].1.choice.as_deref(), Some("billing"));
        assert_eq!(typed[0].1.probabilities.len(), 2);
    }

    #[test]
    fn loopback_detection_is_strict() {
        assert!(is_loopback("http://127.0.0.1:8791"));
        assert!(is_loopback("http://localhost:8791"));
        assert!(is_loopback("http://[::1]:8791"));
        assert!(!is_loopback("https://decisions.example.com"));
        assert!(!is_loopback("not a url"));
    }

    #[test]
    fn endpoint_parts_use_the_url_port() {
        assert_eq!(
            endpoint_parts("http://127.0.0.1:8799/"),
            Some(("127.0.0.1".into(), 8799))
        );
        assert_eq!(
            connect_address("http://localhost:9010"),
            Some(("127.0.0.1".into(), 9010))
        );
    }

    #[test]
    fn boolean_env_spellings_parse() {
        for yes in ["1", "true", "TRUE", "yes", "on"] {
            assert_eq!(parse_bool(yes), Some(true), "{yes}");
        }
        for no in ["0", "false", "no", "off"] {
            assert_eq!(parse_bool(no), Some(false), "{no}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn venv_interpreter_matches_the_platform() {
        let path = python_path(Path::new("/srv/decisions"));
        if cfg!(windows) {
            assert!(path.ends_with("Scripts/python.exe"));
        } else {
            assert!(path.ends_with("bin/python"));
        }
    }

    #[test]
    fn listener_must_prove_the_shared_token() {
        let url = "http://127.0.0.1:8791";
        let token = "a-shared-secret-token-value-012345";
        let plain = Health {
            laya: true,
            clef: true,
            proof: None,
        };
        // A listener that only returns {laya,clef} is refused.
        let error = verify_listener_with(url, &plain, Some(token), None).unwrap_err();
        assert!(error.to_string().contains("could not prove"), "{error}");
        // A forged or stale proof is refused too.
        let wrong = Health {
            proof: Some(health_proof("some-other-token-value-0123456789")),
            ..plain.clone()
        };
        assert!(verify_listener_with(url, &wrong, Some(token), None).is_err());
        // The genuine service (same token) is accepted.
        let genuine = Health {
            proof: Some(health_proof(token)),
            ..plain.clone()
        };
        assert!(verify_listener_with(url, &genuine, Some(token), None).is_ok());
        // Without a token only a live, matching managed record is trusted.
        assert!(verify_listener_with(url, &plain, None, None).is_err());
        let record = ManagedProcess {
            pid: std::process::id(),
            url: url.into(),
            server: "server.py".into(),
            marker: None,
            children: Vec::new(),
            started: "now".into(),
        };
        assert!(verify_listener_with(url, &plain, None, Some(&record)).is_ok());
        let other_url = ManagedProcess {
            url: "http://127.0.0.1:9999".into(),
            ..record
        };
        assert!(verify_listener_with(url, &plain, None, Some(&other_url)).is_err());
        // Remote services are identified by configuration, not by a token.
        assert!(verify_listener_with("https://decisions.example.com", &plain, None, None).is_ok());
    }

    #[test]
    fn service_env_carries_the_token_and_runtime_keys_only() {
        let dir = tempfile::tempdir().unwrap();
        let token = "decisions-shared-token-value-0123456789";
        let env = service_env(dir.path(), token);
        let name_of = |(key, _): &(std::ffi::OsString, std::ffi::OsString)| {
            key.to_string_lossy().to_ascii_lowercase()
        };
        let names: Vec<String> = env.iter().map(name_of).collect();
        assert!(names.iter().any(|n| n == "path"), "{names:?}");
        assert!(names.iter().any(|n| n == "virtual_env"), "{names:?}");
        assert!(names.iter().any(|n| n == "pythonunbuffered"), "{names:?}");
        for name in &names {
            assert!(
                !name.contains("api_key")
                    && !name.contains("secret")
                    && !name.contains("access_token")
                    && !name.contains("refresh_token"),
                "unexpected child env key {name}"
            );
        }
        let delivered = env
            .iter()
            .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(TOKEN_ENV))
            .map(|(_, value)| value.to_string_lossy().into_owned());
        assert_eq!(delivered.as_deref(), Some(token));
    }

    fn test_python() -> Option<&'static str> {
        ["python3", "python", "py"].into_iter().find(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    /// The spawned service must not see the CLI's own secrets, even when they
    /// are present in the parent environment. The child prints only the sorted
    /// key names, never a value.
    #[test]
    fn spawned_service_does_not_inherit_parent_secrets() {
        let Some(python) = test_python() else {
            return;
        };
        let planted = [
            ("VARYNTH_DECISIONS_TEST_SECRET", "planted-secret-value"),
            ("VARYNTH_ACCESS_TOKEN", "planted-access-token"),
            ("OPENAI_API_KEY", "planted-provider-key"),
        ];
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = planted
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in planted {
            std::env::set_var(key, value);
        }
        let dir = tempfile::tempdir().unwrap();
        let env = service_env(dir.path(), "decisions-shared-token-value-0123456789");
        // Restore immediately: only the built environment matters, not a
        // process-wide mutation.
        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        let output = Command::new(python)
            .args([
                "-c",
                "import json, os; print(json.dumps(sorted(os.environ)))",
            ])
            .env_clear()
            .envs(env)
            .output()
            .expect("run python");
        assert!(output.status.success(), "{output:?}");
        let keys: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        for key in [
            "VARYNTH_DECISIONS_TEST_SECRET",
            "VARYNTH_ACCESS_TOKEN",
            "OPENAI_API_KEY",
        ] {
            assert!(
                !keys.iter().any(|seen| seen == key),
                "{key} leaked: {keys:?}"
            );
        }
        assert!(keys.iter().any(|seen| seen == TOKEN_ENV), "{keys:?}");
        assert!(keys.iter().any(|seen| seen == "PATH"), "{keys:?}");
    }

    #[test]
    fn autostart_only_accepts_user_owned_install_paths() {
        assert!(user_owned_install(&Config::home_dir()));
        let workspace = tempfile::tempdir().unwrap();
        assert!(!user_owned_install(workspace.path()));
    }

    #[test]
    fn venv_and_server_must_stay_inside_the_service_dir() {
        let dir = tempfile::tempdir().unwrap();
        let venv = dir.path().join(".venv/bin");
        std::fs::create_dir_all(&venv).unwrap();
        let python = venv.join("python");
        std::fs::write(&python, b"stub").unwrap();
        let server = dir.path().join("server.py");
        std::fs::write(&server, b"stub").unwrap();
        assert!(path_within(&python, &dir.path().join(".venv")));
        assert!(path_within(&server, dir.path()));
        let outside = tempfile::tempdir().unwrap();
        let escaped = outside.path().join("python");
        std::fs::write(&escaped, b"stub").unwrap();
        assert!(!path_within(&escaped, &dir.path().join(".venv")));
    }

    #[test]
    fn concurrent_starts_claim_exactly_one_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let home = home.clone();
                std::thread::spawn(move || {
                    let (_lock, existing) = claim_start_at(&home).unwrap();
                    match existing {
                        Some(_) => "wait",
                        None => {
                            // Simulate spawn → record while the lock is held.
                            let record = ManagedProcess {
                                pid: std::process::id(),
                                url: DEFAULT_URL.into(),
                                server: "stub".into(),
                                marker: process_marker(std::process::id()),
                                children: Vec::new(),
                                started: "now".into(),
                            };
                            write_state_at(&state_path_in(&home), &record).unwrap();
                            std::thread::sleep(Duration::from_millis(150));
                            "spawn"
                        }
                    }
                })
            })
            .collect();
        let mut outcomes: Vec<&str> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        outcomes.sort_unstable();
        assert_eq!(outcomes, vec!["spawn", "wait"]);
        let record = read_state_at(&state_path_in(&home)).expect("one managed record");
        assert_eq!(record.pid, std::process::id());
        // The lock is released after both claims.
        assert!(!home.join("decisions.pid.lock").exists());
    }

    #[test]
    fn clearing_a_record_never_touches_a_concurrent_winners_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.pid.json");
        let winner = ManagedProcess {
            pid: 4242,
            url: DEFAULT_URL.into(),
            server: "stub".into(),
            marker: Some("winner".into()),
            children: Vec::new(),
            started: "now".into(),
        };
        write_state_at(&path, &winner).unwrap();
        // A loser that started pid 4343 must not clear the winner's record.
        clear_state_if_ours_at(&path, 4343, Some("loser"));
        assert!(read_state_at(&path).is_some());
        // The owner may clear its own record.
        clear_state_if_ours_at(&path, 4242, Some("winner"));
        assert!(read_state_at(&path).is_none());
    }
}
