//! Desktop notifications when the agent needs the user.
//!
//! One OS toast per meaningful event: an approval request, a finished turn
//! (only when the terminal is not focused, or after a short re-check delay),
//! a finished subagent and a finished automation task. The notifier hangs off
//! the runtime's existing event fan-out — there is no polling daemon, no tray
//! application and no second transport.
//!
//! Notification text is a fixed label plus, for approvals, the tool name
//! only. Event text (arguments, paths, command lines, model output) never
//! reaches the OS: a toast can end up in the lock screen or a notification
//! history that outlives the session.
//!
//! Config is `notifications = "off" | "auto"` (default `off`); the
//! `VARYNTH_NOTIFICATIONS` environment variable wins over the file. On
//! Windows the toast is produced through the built-in Windows PowerShell
//! WinRT projection, so no administrator rights and no packaged app are
//! required; when PowerShell is missing the `msg.exe` message box is used as
//! a clearly reported fallback. `varynth doctor` reports the mechanism.

use crate::config::Config;
use crate::runtime::AgentEvent;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Environment switch. `off`, `false`, `0`, `no` disable; `auto`, `true`,
/// `1`, `on` enable. Any other value disables (fail closed).
pub const ENV: &str = "VARYNTH_NOTIFICATIONS";
/// Optional delay override, in seconds.
pub const DELAY_ENV: &str = "VARYNTH_NOTIFICATIONS_DELAY";
/// Default re-check delay for a finished turn while the terminal is focused.
pub const DEFAULT_DELAY_SECS: u64 = 5;
/// Repeats of the same kind+label inside this window are swallowed.
const COALESCE_WINDOW: Duration = Duration::from_secs(30);
/// Bounded in-memory record log (used by tests and the `notify test` command).
const LOG_CAP: usize = 200;
const TITLE: &str = "Varynth";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Auto,
}

impl Mode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "no" | "disabled" | "none" => Some(Self::Off),
            "auto" | "true" | "1" | "yes" | "on" => Some(Self::Auto),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
        }
    }
}

/// The config value, with the environment override applied. An unknown env
/// value fails closed (off) instead of guessing.
pub fn effective_mode(cfg: &Config) -> Mode {
    if let Ok(raw) = std::env::var(ENV) {
        return Mode::parse(&raw).unwrap_or(Mode::Off);
    }
    Mode::parse(&cfg.notifications).unwrap_or(Mode::Off)
}

pub fn delay_secs(cfg: &Config) -> u64 {
    if let Ok(raw) = std::env::var(DELAY_ENV) {
        if let Ok(secs) = raw.trim().parse::<u64>() {
            return secs.min(600);
        }
    }
    cfg.notifications_delay_secs.min(600)
}

/// Which runtime produced an event. Background work gets its own label so a
/// subagent's turn does not read as the foreground conversation finishing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Turn,
    Subagent(String),
    Task(String),
}

impl Role {
    pub fn finish_kind(&self) -> &'static str {
        match self {
            Self::Turn => "turn_finished",
            Self::Subagent(_) => "subagent_finished",
            Self::Task(_) => "task_finished",
        }
    }

    fn finish_label(&self, failed: bool) -> String {
        let status = if failed { "failed" } else { "finished" };
        match self {
            Self::Turn => format!("Turn {status}"),
            Self::Subagent(name) if !name.is_empty() => {
                format!("Subagent {name} {status}")
            }
            Self::Subagent(_) => format!("Subagent {status}"),
            Self::Task(_) => format!("Task {status}"),
        }
    }
}

/// What the notification is about. Only these four kinds ever produce a
/// toast; every other `AgentEvent` kind is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Approval,
    TurnFinished,
    SubagentFinished,
    TaskFinished,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval_request",
            Self::TurnFinished => "turn_finished",
            Self::SubagentFinished => "subagent_finished",
            Self::TaskFinished => "task_finished",
        }
    }

    fn key(self) -> &'static str {
        self.as_str()
    }
}

/// Terminal focus as far as we can observe it. `Unknown` happens on
/// platforms without a probe, or when Windows reports no foreground window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Focused,
    Unfocused,
    Unknown,
}

/// How focus is measured. Tests inject a fixed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusProbe {
    Real,
    Fixed(Focus),
}

impl FocusProbe {
    fn read(self) -> Focus {
        match self {
            Self::Real => terminal_focus(),
            Self::Fixed(focus) => focus,
        }
    }
}

/// Best-effort "is the user looking at this terminal?" probe.
pub fn terminal_focus() -> Focus {
    #[cfg(windows)]
    {
        windows_focus()
    }
    #[cfg(not(windows))]
    {
        Focus::Unknown
    }
}

#[cfg(windows)]
fn windows_focus() -> Focus {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    /// Snapshot pid → parent pid for the ancestor check. Windows Terminal,
    /// VS Code and conhost are all ancestors of the shell Varynth runs in,
    /// which makes focus detection work even when `GetConsoleWindow` returns
    /// a pseudoconsole handle.
    fn parent_map() -> HashMap<u32, u32> {
        let mut map = HashMap::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return map;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut entry) != 0;
            while ok {
                map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                ok = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
        }
        map
    }

    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return Focus::Unknown;
        }
        let console = GetConsoleWindow();
        if !console.is_null() && console == foreground {
            return Focus::Focused;
        }
        let mut foreground_pid = 0u32;
        GetWindowThreadProcessId(foreground, &mut foreground_pid);
        if foreground_pid == 0 {
            return Focus::Unknown;
        }
        if foreground_pid == std::process::id() {
            return Focus::Focused;
        }
        let parents = parent_map();
        if parents.is_empty() {
            return Focus::Unknown;
        }
        let mut pid = std::process::id();
        for _ in 0..64 {
            match parents.get(&pid).copied() {
                Some(parent) if parent != 0 && parent != pid => {
                    if parent == foreground_pid {
                        return Focus::Focused;
                    }
                    pid = parent;
                }
                _ => break,
            }
        }
        // A console-less process (serve, one-shot exec) is never "focused".
        Focus::Unfocused
    }
}

/// Transport actually used to reach the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Windows ToastNotificationManager through PowerShell's WinRT
    /// projection, using the registered Windows PowerShell AppUserModelID.
    WindowsToast,
    /// `msg.exe` fallback when PowerShell is unavailable.
    WindowsMessage,
    MacOsScript,
    LinuxNotifySend,
    /// In-memory sink used by tests: records the event, spawns nothing.
    Memory,
    Unsupported(String),
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WindowsToast => "windows-toast",
            Self::WindowsMessage => "windows-msg-fallback",
            Self::MacOsScript => "macos-osascript",
            Self::LinuxNotifySend => "linux-notify-send",
            Self::Memory => "memory",
            Self::Unsupported(_) => "unsupported",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::WindowsToast => {
                "Windows WinRT toast via built-in PowerShell (no admin, no tray app)".into()
            }
            Self::WindowsMessage => {
                "PowerShell toast unavailable; using msg.exe session message fallback".into()
            }
            Self::MacOsScript => "osascript display notification".into(),
            Self::LinuxNotifySend => "notify-send".into(),
            Self::Memory => "in-memory test sink".into(),
            Self::Unsupported(reason) => reason.clone(),
        }
    }

    pub fn available(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }

    fn program(&self) -> &'static str {
        match self {
            Self::WindowsToast => "powershell",
            Self::WindowsMessage => "msg",
            Self::MacOsScript => "osascript",
            Self::LinuxNotifySend => "notify-send",
            Self::Memory => "memory",
            Self::Unsupported(_) => "",
        }
    }

    /// The argument vector for one toast. Kept pure so it is unit-testable.
    fn args(&self, title: &str, body: &str) -> Vec<String> {
        match self {
            Self::WindowsToast => vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-Command".into(),
                windows_toast_script(title, body),
            ],
            Self::WindowsMessage => vec!["*".into(), "/TIME:10".into(), format!("{title}: {body}")],
            Self::MacOsScript => vec![
                "-e".into(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    applescript_escape(body),
                    applescript_escape(title)
                ),
            ],
            Self::LinuxNotifySend => {
                vec![
                    "-a".into(),
                    title.into(),
                    "-t".into(),
                    "8000".into(),
                    body.into(),
                ]
            }
            Self::Memory | Self::Unsupported(_) => Vec::new(),
        }
    }
}

/// PowerShell 5.1 WinRT toast. `ToastText02` is a title + body template;
/// the text is appended as DOM text nodes, so no XML escaping is needed.
/// Single quotes are doubled for the PowerShell literal.
fn windows_toast_script(title: &str, body: &str) -> String {
    let title = title.replace('\'', "''");
    let body = body.replace('\'', "''");
    format!(
        "$ErrorActionPreference = 'Stop'; \
         [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
         $toast = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $text = $toast.GetElementsByTagName('text'); \
         $text.Item(0).AppendChild($toast.CreateTextNode('{title}')) | Out-Null; \
         $text.Item(1).AppendChild($toast.CreateTextNode('{body}')) | Out-Null; \
         $notifier = [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe'); \
         $notifier.Show([Windows.UI.Notifications.ToastNotification]::new($toast)); \
         Write-Output 'toast-fired'"
    )
}

fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(not(windows))]
fn which_program(program: &str) -> Option<std::path::PathBuf> {
    which::which(program).ok()
}

/// Cached backend detection: the programs on PATH do not change mid-process.
pub fn detect_backend() -> Backend {
    static CACHE: LazyLock<Backend> = LazyLock::new(detect_backend_uncached);
    CACHE.clone()
}

fn detect_backend_uncached() -> Backend {
    #[cfg(windows)]
    {
        if crate::sandbox::resolve_program("powershell").is_ok() {
            return Backend::WindowsToast;
        }
        if crate::sandbox::resolve_program("msg").is_ok() {
            return Backend::WindowsMessage;
        }
        Backend::Unsupported("no powershell.exe or msg.exe on PATH".into())
    }
    #[cfg(target_os = "macos")]
    {
        if which_program("osascript").is_some() {
            return Backend::MacOsScript;
        }
        Backend::Unsupported("osascript not found on PATH".into())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if which_program("notify-send").is_some() {
            return Backend::LinuxNotifySend;
        }
        Backend::Unsupported("notify-send not found on PATH".into())
    }
    #[cfg(not(any(windows, unix)))]
    {
        Backend::Unsupported("no desktop notification mechanism for this platform".into())
    }
}

/// Result of one OS send attempt. Kept as data so `varynth notify test` and
/// `doctor` can print exactly what happened.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchReport {
    pub backend: String,
    pub program: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Set when the primary mechanism failed and a fallback was attempted.
    pub fallback: Option<String>,
}

impl DispatchReport {
    fn failure(backend: &Backend, message: impl Into<String>) -> Self {
        Self {
            backend: backend.as_str().into(),
            program: backend.program().into(),
            ok: false,
            exit_code: None,
            stdout: String::new(),
            stderr: message.into(),
            fallback: None,
        }
    }
}

/// Synchronous send used by `notify test` and the dispatch worker.
pub fn send_blocking(backend: &Backend, title: &str, body: &str) -> DispatchReport {
    if !backend.available() {
        return DispatchReport::failure(backend, backend.detail());
    }
    let program = match crate::sandbox::resolve_program(backend.program()) {
        Ok(program) => program,
        Err(error) => return DispatchReport::failure(backend, format!("resolve failed: {error}")),
    };
    let mut command = Command::new(program);
    command.args(backend.args(title, body));
    crate::sandbox::apply_child_env(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    match command.output() {
        Ok(output) => DispatchReport {
            backend: backend.as_str().into(),
            program: backend.program().into(),
            ok: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            fallback: None,
        },
        Err(error) => DispatchReport::failure(backend, format!("spawn failed: {error}")),
    }
}

/// Send one test notification, retaining the primary report and adding a
/// fallback report when Windows Toast fails at runtime. Ignores config mode
/// and coalescing: this is the explicit `varynth notify test` path.
pub fn send_test(label: &str) -> Vec<DispatchReport> {
    let label = sanitize_label(label);
    let primary = detect_backend();
    let mut report = send_blocking(&primary, TITLE, &label);
    let mut reports = vec![report.clone()];
    #[cfg(windows)]
    {
        if !report.ok && primary == Backend::WindowsToast {
            let fallback = Backend::WindowsMessage;
            if fallback.available() {
                let mut second = send_blocking(&fallback, TITLE, &label);
                second.fallback = Some("windows-msg".into());
                report.fallback = Some(fallback.as_str().into());
                reports.push(second);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = &mut report;
    }
    reports
}

/// One observed notification. `dispatched` is true when an OS send was
/// attempted; `coalesced` marks a repeat swallowed inside the window;
/// `delivered` is filled in when the worker thread learns the exit status.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationRecord {
    pub id: u64,
    pub kind: String,
    pub label: String,
    pub backend: String,
    pub dispatched: bool,
    pub coalesced: bool,
    pub delayed: bool,
    pub delivered: Option<bool>,
    pub detail: String,
    pub at: String,
}

#[derive(Default)]
struct State {
    last: HashMap<String, Instant>,
    log: VecDeque<NotificationRecord>,
    next_id: u64,
}

fn shared_state() -> Arc<Mutex<State>> {
    static STATE: LazyLock<Arc<Mutex<State>>> =
        LazyLock::new(|| Arc::new(Mutex::new(State::default())));
    STATE.clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Notify,
    Schedule,
}

/// Pure policy: mode + kind + focus → what should happen. Kept separate so
/// the decision is testable without spawning anything.
fn decide(kind: Kind, focus: Focus) -> Action {
    match kind {
        // An approval blocks the agent; a background job ending is the whole
        // point of the notification. Both fire regardless of focus.
        Kind::Approval | Kind::SubagentFinished | Kind::TaskFinished => Action::Notify,
        Kind::TurnFinished => match focus {
            Focus::Unfocused => Action::Notify,
            // The user is watching the terminal: re-check after the delay
            // and only notify if they have moved on.
            Focus::Focused | Focus::Unknown => Action::Schedule,
        },
    }
}

/// A notifier bound to one mode/backend. Production code shares the
/// process-wide record/coalescing state; tests construct isolated ones.
#[derive(Clone)]
pub struct Notifier {
    mode: Mode,
    delay: Duration,
    role: Role,
    backend: Backend,
    probe: FocusProbe,
    state: Arc<Mutex<State>>,
}

impl Notifier {
    pub fn new(
        mode: Mode,
        delay: Duration,
        role: Role,
        backend: Backend,
        probe: FocusProbe,
    ) -> Self {
        Self {
            mode,
            delay,
            role,
            backend,
            probe,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Production notifier: shared state so two runtimes (foreground and a
    /// subagent) still coalesce the same burst into one toast.
    pub fn from_config(cfg: &Config, role: Role) -> Self {
        Self {
            mode: effective_mode(cfg),
            delay: Duration::from_secs(delay_secs(cfg)),
            role,
            backend: detect_backend(),
            probe: FocusProbe::Real,
            state: shared_state(),
        }
    }

    pub fn role(&self) -> &Role {
        &self.role
    }

    pub fn set_role(&mut self, role: Role) {
        self.role = role;
    }

    /// Feed one agent event. Non-notifiable kinds return immediately.
    pub fn observe(&self, event: &AgentEvent) {
        if self.mode == Mode::Off {
            return;
        }
        let Some((kind, label)) = self.classify(event) else {
            return;
        };
        self.fire(kind, label, false);
    }

    fn classify(&self, event: &AgentEvent) -> Option<(Kind, String)> {
        let failed = event.text.trim().eq_ignore_ascii_case("failed");
        match event.kind.as_str() {
            "approval_request" | "approval_requested" => {
                Some((Kind::Approval, approval_label(&event.text)))
            }
            // A runtime marked as a subagent/task emits its finish event
            // under that role's kind; accept the plain aliases as well so a
            // hand-written event cannot slip through.
            "subagent_finished" => Some((
                Kind::SubagentFinished,
                match &self.role {
                    Role::Subagent(_) => self.role.finish_label(failed),
                    _ => format!("Subagent {}", if failed { "failed" } else { "finished" }),
                },
            )),
            "task_finished" | "task finished" => {
                Some((Kind::TaskFinished, self.role.finish_label(failed)))
            }
            "turn_finished" => Some((self.finish_kind(), self.role.finish_label(failed))),
            _ => None,
        }
    }

    fn finish_kind(&self) -> Kind {
        match &self.role {
            Role::Turn => Kind::TurnFinished,
            Role::Subagent(_) => Kind::SubagentFinished,
            Role::Task(_) => Kind::TaskFinished,
        }
    }

    fn fire(&self, kind: Kind, label: String, delayed: bool) {
        let focus = if kind == Kind::TurnFinished {
            self.probe.read()
        } else {
            Focus::Unfocused
        };
        match decide(kind, focus) {
            Action::Notify => self.dispatch(kind, &label, delayed),
            Action::Schedule => {
                let notifier = self.clone();
                let delay = self.delay;
                std::thread::spawn(move || {
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    // Re-check after the delay: a user who came back to the
                    // terminal does not need a toast.
                    if notifier.probe.read() == Focus::Focused {
                        notifier.record(kind, label, true, false, "suppressed: terminal focused");
                        return;
                    }
                    notifier.dispatch(kind, &label, true);
                });
            }
        }
    }

    fn dispatch(&self, kind: Kind, label: &str, delayed: bool) {
        let key = format!("{}:{}", kind.key(), label);
        let id = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            let repeat = state
                .last
                .get(&key)
                .is_some_and(|last| now.duration_since(*last) < COALESCE_WINDOW);
            if repeat {
                let id = state.next_id;
                state.next_id += 1;
                state.log.push_back(NotificationRecord {
                    id,
                    kind: kind.as_str().into(),
                    label: label.into(),
                    backend: self.backend.as_str().into(),
                    dispatched: false,
                    coalesced: true,
                    delayed,
                    delivered: None,
                    detail: "coalesced repeat".into(),
                    at: chrono::Utc::now().to_rfc3339(),
                });
                trim(&mut state.log);
                return;
            }
            state.last.insert(key, now);
            let id = state.next_id;
            state.next_id += 1;
            state.log.push_back(NotificationRecord {
                id,
                kind: kind.as_str().into(),
                label: label.into(),
                backend: self.backend.as_str().into(),
                dispatched: true,
                coalesced: false,
                delayed,
                delivered: None,
                detail: "dispatch started".into(),
                at: chrono::Utc::now().to_rfc3339(),
            });
            trim(&mut state.log);
            id
        };

        if self.backend == Backend::Memory {
            self.finish_record(id, true, "memory backend");
            return;
        }
        let state = self.state.clone();
        let backend = self.backend.clone();
        let label = label.to_string();
        std::thread::spawn(move || {
            let mut report = send_blocking(&backend, TITLE, &label);
            #[cfg(windows)]
            {
                if !report.ok && backend == Backend::WindowsToast {
                    let fallback = Backend::WindowsMessage;
                    if fallback.available() {
                        let mut second = send_blocking(&fallback, TITLE, &label);
                        second.fallback = Some(fallback.as_str().into());
                        report = second;
                    }
                }
            }
            let ok = report.ok;
            let detail = if ok {
                format!(
                    "{} exited {}",
                    report.program,
                    report
                        .exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "?".into())
                )
            } else {
                format!(
                    "{} failed: {} {}",
                    report.program,
                    report
                        .exit_code
                        .map(|c| format!("exit {c}"))
                        .unwrap_or_default(),
                    report.stderr
                )
                .trim()
                .to_string()
            };
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(record) = state.log.iter_mut().find(|record| record.id == id) {
                record.delivered = Some(ok);
                record.detail = detail;
            }
        });
    }

    fn record(&self, kind: Kind, label: String, delayed: bool, dispatched: bool, detail: &str) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let id = state.next_id;
        state.next_id += 1;
        state.log.push_back(NotificationRecord {
            id,
            kind: kind.as_str().into(),
            label,
            backend: self.backend.as_str().into(),
            dispatched,
            coalesced: false,
            delayed,
            delivered: None,
            detail: detail.into(),
            at: chrono::Utc::now().to_rfc3339(),
        });
        trim(&mut state.log);
    }

    fn finish_record(&self, id: u64, delivered: bool, detail: &str) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = state.log.iter_mut().find(|record| record.id == id) {
            record.delivered = Some(delivered);
            record.detail = detail.into();
        }
    }

    /// Snapshot of the bounded log, oldest first.
    pub fn records(&self) -> Vec<NotificationRecord> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.log.iter().cloned().collect()
    }

    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.log.clear();
        state.last.clear();
    }

    /// Force one toast now, bypassing focus, delay and coalescing. Used by
    /// `varynth notify test`.
    pub fn send_now(&self, label: &str) -> Vec<DispatchReport> {
        send_test(label)
    }
}

/// Windows-only: load the WinRT notification types without showing a toast.
/// Success means the primary mechanism is usable; failure lets doctor report
/// the `msg.exe` fallback instead of claiming a toast that cannot fire.
#[cfg(windows)]
fn probe_winrt() -> Result<(), String> {
    let program =
        crate::sandbox::resolve_program("powershell").map_err(|error| error.to_string())?;
    let mut command = Command::new(program);
    crate::sandbox::apply_child_env(&mut command);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; Write-Output 'winrt-ok'",
    ]);
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    match command.output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// `capability` plus a Windows WinRT probe, for `varynth doctor`. Never sends
/// a notification; only checks that the notification API loads.
pub fn doctor_capability(cfg: &Config) -> serde_json::Value {
    #[allow(unused_mut)]
    let mut cap = capability(cfg);
    #[cfg(windows)]
    {
        if cap["backend"] == "windows-toast" {
            match probe_winrt() {
                Ok(()) => {
                    cap["winrt_probe"] = serde_json::json!("ok");
                }
                Err(error) => {
                    let mode = effective_mode(cfg);
                    let fallback = Backend::WindowsMessage;
                    let available = fallback.available();
                    cap["winrt_probe"] = serde_json::json!("unavailable");
                    cap["windows_toast_error"] = serde_json::json!(error);
                    cap["backend"] = serde_json::json!(fallback.as_str());
                    cap["mechanism"] = serde_json::json!(fallback.detail());
                    cap["enabled"] = serde_json::json!(mode == Mode::Auto && available);
                    cap["ok"] = serde_json::json!(mode == Mode::Off || available);
                    cap["detail"] = serde_json::json!(format!(
                        "WinRT toast unavailable ({}); {}",
                        error,
                        fallback.detail()
                    ));
                }
            }
        }
    }
    cap
}

impl Default for Notifier {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            delay: Duration::from_secs(DEFAULT_DELAY_SECS),
            role: Role::Turn,
            backend: Backend::Memory,
            probe: FocusProbe::Fixed(Focus::Unfocused),
            state: Arc::new(Mutex::new(State::default())),
        }
    }
}

fn trim(log: &mut VecDeque<NotificationRecord>) {
    while log.len() > LOG_CAP {
        log.pop_front();
    }
}

/// Approval text is `<tool> <detail>`; only the tool name is retained.
/// Everything after the first whitespace (commands, paths, arguments, API
/// payloads) is dropped, and the name is restricted to a modest identifier
/// alphabet.
pub fn approval_label(text: &str) -> String {
    let tool = text.split_whitespace().next().unwrap_or("");
    let tool: String = tool
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
        .take(32)
        .collect();
    if tool.is_empty() {
        "Approval needed".into()
    } else {
        format!("Approval needed: {tool}")
    }
}

/// Test/user-supplied label: one line, bounded, no control characters.
pub fn sanitize_label(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(80)
        .collect();
    out = out.trim().to_string();
    if out.is_empty() {
        "Notification test".into()
    } else {
        out
    }
}

/// Doctor-facing capability summary. Never probes by sending a toast.
pub fn capability(cfg: &Config) -> serde_json::Value {
    let mode = effective_mode(cfg);
    let backend = detect_backend();
    let delay = delay_secs(cfg);
    let env_override = std::env::var(ENV).ok();
    let enabled = mode == Mode::Auto && backend.available();
    let ok = mode == Mode::Off || backend.available();
    let detail = if mode == Mode::Off {
        format!(
            "off (set notifications = \"auto\" or {ENV}=auto to enable); mechanism: {}",
            backend.detail()
        )
    } else if backend.available() {
        format!(
            "auto; {delay}s delay for a focused terminal; mechanism: {}",
            backend.detail()
        )
    } else {
        format!("auto requested but unavailable: {}", backend.detail())
    };
    serde_json::json!({
        "ok": ok,
        "mode": mode.as_str(),
        "enabled": enabled,
        "backend": backend.as_str(),
        "mechanism": backend.detail(),
        "delay_secs": delay,
        "env_override": env_override,
        "detail": detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, text: &str) -> AgentEvent {
        AgentEvent {
            kind: kind.into(),
            text: text.into(),
        }
    }

    fn auto(role: Role, focus: Focus) -> Notifier {
        Notifier::new(
            Mode::Auto,
            Duration::ZERO,
            role,
            Backend::Memory,
            FocusProbe::Fixed(focus),
        )
    }

    #[test]
    fn approval_event_produces_exactly_one_dispatch_record() {
        let notifier = auto(Role::Turn, Focus::Unfocused);
        notifier.observe(&event("approval_request", "bash rm -rf build"));
        let records = notifier.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].dispatched);
        assert!(!records[0].coalesced);
        assert_eq!(records[0].kind, "approval_request");
        assert_eq!(records[0].label, "Approval needed: bash");
    }

    #[test]
    fn repeats_coalesce_inside_the_window() {
        let notifier = auto(Role::Turn, Focus::Unfocused);
        notifier.observe(&event("approval_request", "bash one"));
        notifier.observe(&event("approval_request", "bash two"));
        notifier.observe(&event("approval_request", "bash three"));
        let records = notifier.records();
        assert_eq!(records.len(), 3);
        assert_eq!(records.iter().filter(|r| r.dispatched).count(), 1);
        assert_eq!(records.iter().filter(|r| r.coalesced).count(), 2);
    }

    #[test]
    fn different_tools_do_not_coalesce() {
        let notifier = auto(Role::Turn, Focus::Unfocused);
        notifier.observe(&event("approval_request", "bash ls"));
        notifier.observe(&event("approval_request", "write_file out.txt"));
        assert_eq!(
            notifier.records().iter().filter(|r| r.dispatched).count(),
            2
        );
    }

    #[test]
    fn off_mode_records_nothing() {
        let notifier = Notifier::new(
            Mode::Off,
            Duration::ZERO,
            Role::Turn,
            Backend::Memory,
            FocusProbe::Fixed(Focus::Unfocused),
        );
        notifier.observe(&event("approval_request", "bash ls"));
        notifier.observe(&event("turn_finished", ""));
        assert!(notifier.records().is_empty());
    }

    #[test]
    fn env_off_overrides_auto_config() {
        let _guard = env_lock();
        let mut cfg = Config::default();
        cfg.notifications = "auto".into();
        std::env::set_var(ENV, "off");
        assert_eq!(effective_mode(&cfg), Mode::Off);
        std::env::set_var(ENV, "auto");
        cfg.notifications = "off".into();
        assert_eq!(effective_mode(&cfg), Mode::Auto);
        std::env::set_var(ENV, "garbage");
        assert_eq!(effective_mode(&cfg), Mode::Off);
        std::env::remove_var(ENV);
        assert_eq!(effective_mode(&cfg), Mode::Off);
    }

    #[test]
    fn notification_text_never_contains_tool_arguments_or_secrets() {
        let notifier = auto(Role::Turn, Focus::Unfocused);
        notifier.observe(&event(
            "approval_request",
            "bash curl -H 'Authorization: Bearer sk-live-secret' https://example.test/x?token=abc",
        ));
        let records = notifier.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].label, "Approval needed: bash");
        let rendered = serde_json::to_string(&records).unwrap();
        for secret in [
            "sk-live-secret",
            "Authorization",
            "https://example.test",
            "token=abc",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret} in {rendered}");
        }
    }

    #[test]
    fn assistant_and_tool_events_never_notify() {
        let notifier = auto(Role::Turn, Focus::Unfocused);
        for kind in [
            "assistant",
            "delta",
            "tool",
            "tool_result",
            "system",
            "round",
            "image",
        ] {
            notifier.observe(&event(kind, "secret body text"));
        }
        assert!(notifier.records().is_empty());
    }

    #[test]
    fn focused_terminal_suppresses_turn_finished_immediately() {
        let notifier = auto(Role::Turn, Focus::Focused);
        notifier.observe(&event("turn_finished", ""));
        // Focused turns are scheduled, never dispatched in-line. The zero
        // delay re-check may log a suppression record, but nothing is sent.
        assert!(!notifier.records().iter().any(|record| record.dispatched));
    }

    #[test]
    fn unfocused_terminal_notifies_turn_finished() {
        let notifier = auto(Role::Turn, Focus::Unfocused);
        notifier.observe(&event("turn_finished", ""));
        let records = notifier.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].dispatched);
        assert_eq!(records[0].label, "Turn finished");
    }

    #[test]
    fn subagent_role_labels_subagent_finish() {
        let notifier = auto(Role::Subagent("reviewer".into()), Focus::Unfocused);
        notifier.observe(&event("subagent_finished", ""));
        let records = notifier.records();
        assert_eq!(records[0].kind, "subagent_finished");
        assert_eq!(records[0].label, "Subagent reviewer finished");
    }

    #[test]
    fn task_role_labels_task_finish() {
        let notifier = auto(Role::Task("t1".into()), Focus::Unfocused);
        notifier.observe(&event("turn_finished", "failed"));
        let records = notifier.records();
        assert_eq!(records[0].kind, "task_finished");
        assert_eq!(records[0].label, "Task failed");
    }

    #[test]
    fn approval_label_is_bounded_and_sanitized() {
        assert_eq!(approval_label("bash ls"), "Approval needed: bash");
        assert_eq!(approval_label(""), "Approval needed");
        assert_eq!(approval_label("../../\nsecret"), "Approval needed: ../../");
        assert!(approval_label(&"m".repeat(200)).len() <= "Approval needed: ".len() + 32);
    }

    #[test]
    fn windows_toast_script_uses_registered_app_id_and_text_nodes() {
        let script = windows_toast_script("Varynth", "Approval needed: bash");
        assert!(script.contains("ToastText02"));
        assert!(script.contains("WindowsPowerShell"));
        assert!(script.contains("Approval needed: bash"));
        assert!(!script.contains("<toast"));
    }

    #[test]
    fn decide_policy_matrix() {
        assert_eq!(decide(Kind::Approval, Focus::Focused), Action::Notify);
        assert_eq!(
            decide(Kind::SubagentFinished, Focus::Focused),
            Action::Notify
        );
        assert_eq!(decide(Kind::TaskFinished, Focus::Unknown), Action::Notify);
        assert_eq!(decide(Kind::TurnFinished, Focus::Unfocused), Action::Notify);
        assert_eq!(decide(Kind::TurnFinished, Focus::Focused), Action::Schedule);
        assert_eq!(decide(Kind::TurnFinished, Focus::Unknown), Action::Schedule);
    }

    #[test]
    fn config_defaults_are_off() {
        let cfg = Config::default();
        assert_eq!(cfg.notifications, "off");
        assert!(cfg.notifications_delay_secs > 0);
        assert_eq!(Mode::parse(&cfg.notifications), Some(Mode::Off));
    }

    #[test]
    fn capability_reports_mechanism_without_sending() {
        let mut cfg = Config::default();
        let _guard = env_lock();
        std::env::remove_var(ENV);
        cfg.notifications = "auto".into();
        let cap = capability(&cfg);
        assert_eq!(cap["mode"], "auto");
        assert!(cap["backend"].as_str().unwrap().len() > 0);
        assert!(cap["mechanism"].as_str().unwrap().len() > 0);
    }

    /// Serializes the tests that mutate the process environment.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
