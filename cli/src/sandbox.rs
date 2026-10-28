use anyhow::{bail, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child as PlainChild, Command, ExitStatus, Stdio};

#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    /// bash commands execute inside a Docker container with the workspace
    /// mounted at /work (execution support lives in the bash tool).
    DockerIsolated,
}

impl SandboxMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "read-only" => Self::ReadOnly,
            "danger-full-access" => Self::DangerFullAccess,
            "docker-isolated" => Self::DockerIsolated,
            _ => Self::WorkspaceWrite,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
            Self::DockerIsolated => "docker-isolated",
        }
    }
}

/// What the OS boundary actually does for a mode on this host.
///
/// `Enforced` means every host child spawned for the bash tool runs behind a
/// kernel-enforced write boundary, not just Varynth's path checks.
/// `Unconfined` is only `danger-full-access`, which has no boundary by
/// design. `Degraded` means the mode cannot be confined here; it is reported
/// by `varynth doctor` instead of being silently downgraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcement {
    Enforced,
    Unconfined,
    Degraded(String),
}

impl Enforcement {
    /// One-line status for doctor/UI: `enforced`, `not-enforced (...)`, or
    /// `degraded: <reason>`.
    pub fn status(&self) -> String {
        match self {
            Self::Enforced => "enforced".to_string(),
            Self::Unconfined => "not-enforced (mode is unscoped by design)".to_string(),
            Self::Degraded(reason) => format!("degraded: {reason}"),
        }
    }

    pub fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforced)
    }
}

/// Where `bash` children stand for `mode` on this platform.
pub fn enforcement(mode: SandboxMode) -> Enforcement {
    if mode == SandboxMode::DangerFullAccess {
        return Enforcement::Unconfined;
    }
    if mode == SandboxMode::DockerIsolated {
        return match which::which("docker") {
            Ok(_) => Enforcement::Enforced,
            Err(_) => Enforcement::Degraded(
                "docker executable not found; docker-isolated commands cannot start".into(),
            ),
        };
    }
    #[cfg(windows)]
    {
        static PROBE: std::sync::LazyLock<std::result::Result<(), String>> =
            std::sync::LazyLock::new(|| windows::probe().map_err(|error| error.to_string()));
        match PROBE.as_ref() {
            Ok(()) => Enforcement::Enforced,
            Err(error) => Enforcement::Degraded(error.clone()),
        }
    }
    #[cfg(not(windows))]
    {
        Enforcement::Degraded(format!(
            "no OS-level sandbox backend on {}; only the path jail applies",
            std::env::consts::OS
        ))
    }
}

/// One place a confined child may write. Workspace roots are labelled
/// recursively so an existing checkout stays writable; future entries inherit.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteRoot {
    pub path: PathBuf,
    pub recursive: bool,
}

/// Roots whose integrity label the backend controls for this mode.
/// `workspace-write` lowers them so the child may write; `read-only` raises
/// them back so a tree lowered by an earlier write-mode run is not writable.
#[cfg(windows)]
pub(crate) fn confined_roots(jail: &Jail) -> Vec<WriteRoot> {
    if !matches!(
        jail.mode,
        SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly
    ) {
        return Vec::new();
    }
    let mut roots = Vec::new();
    roots.push(WriteRoot {
        path: strip_verbatim(jail.cwd.canonicalize().unwrap_or_else(|_| jail.cwd.clone())),
        recursive: true,
    });
    for extra in &jail.extra {
        if !extra.exists() {
            // Nothing can be written under a directory that does not exist.
            continue;
        }
        roots.push(WriteRoot {
            path: strip_verbatim(extra.canonicalize().unwrap_or_else(|_| extra.clone())),
            recursive: true,
        });
    }
    roots
}

/// A child started by the bash tool: either a plain `std::process::Child`
/// (unconfined modes, Docker, platforms without a backend) or a child behind
/// the Windows write boundary.
pub(crate) enum ShellChild {
    Plain(PlainChild),
    #[cfg(windows)]
    Confined(windows::ConfinedChild),
}

impl ShellChild {
    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        match self {
            Self::Plain(child) => child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
            #[cfg(windows)]
            Self::Confined(child) => child.take_stdout(),
        }
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        match self {
            Self::Plain(child) => child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
            #[cfg(windows)]
            Self::Confined(child) => child.take_stderr(),
        }
    }

    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            Self::Plain(child) => child.try_wait(),
            #[cfg(windows)]
            Self::Confined(child) => child.try_wait(),
        }
    }

    #[cfg(test)]
    pub(crate) fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match self {
            Self::Plain(child) => child.wait(),
            #[cfg(windows)]
            Self::Confined(child) => child.wait(),
        }
    }

    /// Kill the shell and every descendant. Timeouts must not leave a build
    /// running behind the sandbox.
    pub(crate) fn kill_tree(&mut self) {
        match self {
            Self::Plain(child) => kill_plain_tree(child),
            #[cfg(windows)]
            Self::Confined(child) => child.kill_tree(),
        }
    }
}

fn kill_plain_tree(child: &mut PlainChild) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe {
            kill(-(child.id() as i32), 9);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Environment variables a tool, hook, notification or MCP child may receive.
/// Every other variable — provider keys, `VARYNTH_*TOKEN*`, `*_API_KEY`,
/// `*SECRET*` — is dropped by construction rather than filtered by name, so a
/// newly introduced secret cannot leak by accident. `USERNAME`/`USER`/
/// `USERDOMAIN` are identity, not secrets: Python's `getpass.getuser()`
/// (used by the decisions service) refuses to start without one of them.
const CHILD_ENV_KEYS: [&str; 15] = [
    "PATH",
    "HOME",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "TMPDIR",
    "SystemRoot",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "LANG",
    "LANGUAGE",
    "USERNAME",
    "USER",
    "USERDOMAIN",
];

fn is_child_env_key(name: &str) -> bool {
    name.starts_with("LC_")
        || CHILD_ENV_KEYS
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// The explicit allowlist environment for a child process. `extra` carries
/// values the caller configured deliberately (a hook's `env` map or an MCP
/// server's `env` map); they are literal values, never inherited secrets.
pub fn child_env_with(extra: &[(String, String)]) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    use std::ffi::OsString;
    let mut out: Vec<(OsString, OsString)> = Vec::new();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if !is_child_env_key(&name) {
            continue;
        }
        if out.iter().any(|(existing, _)| {
            existing
                .to_string_lossy()
                .eq_ignore_ascii_case(name.as_ref())
        }) {
            continue;
        }
        out.push((key, value));
    }
    for (key, value) in extra {
        if key.trim().is_empty() || key.contains('=') || key.contains('\0') {
            continue;
        }
        out.retain(|(existing, _)| {
            !existing
                .to_string_lossy()
                .eq_ignore_ascii_case(key.as_str())
        });
        out.push((OsString::from(key), OsString::from(value)));
    }
    #[cfg(windows)]
    // Never let a child resolve a bare executable name through its own
    // current directory (the freshly cloned repository).
    out.push((
        OsString::from("NoDefaultCurrentDirectoryInExePath"),
        OsString::from("1"),
    ));
    out
}

pub fn child_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    child_env_with(&[])
}

/// Replace a command's inherited environment with the allowlist.
pub fn apply_child_env(command: &mut Command) {
    command.env_clear();
    command.envs(child_env());
}

/// Absolute system path for a bare program name. On Windows the known
/// interpreters resolve under `%SystemRoot%`/`%ComSpec%`; anything else is
/// looked up on `PATH` only, never in the current directory.
pub fn resolve_program(program: &str) -> Result<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || program.contains('/') || program.contains('\\') {
        return Ok(path.to_path_buf());
    }
    #[cfg(windows)]
    {
        let lowered = program.to_ascii_lowercase();
        let base = lowered.strip_suffix(".exe").unwrap_or(&lowered);
        match base {
            "powershell" => return powershell_path(),
            "pwsh" => {
                if let Some(found) = search_path("pwsh.exe") {
                    return Ok(found);
                }
            }
            "cmd" => return Ok(cmd_path()),
            _ => {}
        }
    }
    if let Some(found) = search_path(program) {
        return Ok(found);
    }
    #[cfg(windows)]
    if !program.to_ascii_lowercase().ends_with(".exe") {
        if let Some(found) = search_path(&format!("{program}.exe")) {
            return Ok(found);
        }
    }
    bail!(
        "`{program}` is not on PATH at an absolute location; refusing to resolve it relative to the workspace"
    )
}

/// `PATH` search that ignores relative entries (and therefore the current
/// directory) so a repository file can never shadow a system binary.
fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

#[cfg(windows)]
fn powershell_path() -> Result<PathBuf> {
    let direct = system_root().join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if direct.is_file() {
        return Ok(direct);
    }
    if let Some(found) = search_path("powershell.exe").or_else(|| search_path("pwsh.exe")) {
        return Ok(found);
    }
    bail!(
        "no absolute Windows PowerShell found under {} ; refusing to resolve `powershell` from the workspace",
        system_root().display()
    )
}

#[cfg(windows)]
fn cmd_path() -> PathBuf {
    if let Some(comspec) = std::env::var_os("ComSpec") {
        let candidate = PathBuf::from(comspec);
        if candidate.is_absolute() && candidate.is_file() {
            return candidate;
        }
    }
    let direct = system_root().join(r"System32\cmd.exe");
    if direct.is_file() {
        return direct;
    }
    search_path("cmd.exe").unwrap_or(direct)
}

/// The absolute host shell argv for a command string. Windows uses the
/// system PowerShell (resolved under `%SystemRoot%`), everything else
/// `/bin/sh`; a bare name never reaches process creation.
pub fn resolve_host_shell(command: &str) -> Result<(String, Vec<String>)> {
    #[cfg(windows)]
    {
        let program = powershell_path()?;
        Ok((
            program.to_string_lossy().into_owned(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
        ))
    }
    #[cfg(not(windows))]
    {
        let program = if Path::new("/bin/sh").is_file() {
            "/bin/sh"
        } else if Path::new("/usr/bin/sh").is_file() {
            "/usr/bin/sh"
        } else {
            "sh"
        };
        Ok((
            program.to_string(),
            vec!["-c".to_string(), command.to_string()],
        ))
    }
}

fn host_shell_argv(command: &str) -> Result<(String, Vec<String>)> {
    resolve_host_shell(command)
}

/// Spawn the host shell for `command`, going through the OS boundary when the
/// jail's mode calls for one. A mode that requires the boundary fails the
/// spawn if it cannot be established — it never falls back to an unbounded
/// child.
pub(crate) fn spawn_host_shell(jail: &Jail, command: &str) -> Result<ShellChild> {
    let (program, args) = host_shell_argv(command)?;
    #[cfg(windows)]
    if matches!(
        jail.mode,
        SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite
    ) {
        return windows::spawn(&program, &args, &jail.cwd, &confined_roots(jail), jail.mode)
            .map(ShellChild::Confined);
    }
    spawn_plain(&program, &args, &jail.cwd).map(ShellChild::Plain)
}

fn spawn_plain(program: &str, args: &[String], cwd: &Path) -> Result<PlainChild> {
    let mut command = Command::new(program);
    command.args(args);
    apply_child_env(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Ok(command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?)
}

/// Operators a shell runs even when they are not separate argv words.
/// PowerShell grouping `(...)` and script blocks `{...}` are the important
/// case: `split_argv` keeps them inside tokens, so a first-word allowlist
/// would otherwise admit `type (ii evil.bat)`.
pub fn has_shell_metacharacters(command: &str) -> bool {
    command.chars().any(|c| {
        matches!(
            c,
            ';' | '&'
                | '|'
                | '>'
                | '<'
                | '`'
                | '$'
                | '\n'
                | '\r'
                | '('
                | ')'
                | '{'
                | '}'
                | ','
                | '!'
                | '#'
                | '@'
                | '['
                | ']'
        )
    })
}

/// Split a command string into argv words for policy checks. Quotes and
/// whitespace are honored; an unbalanced quote yields `None` (and the caller
/// refuses the command). Backslashes only escape whitespace/quotes, so Windows
/// paths survive unchanged.
pub fn split_argv(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(delimiter) => {
                if ch == delimiter {
                    quote = None;
                } else if delimiter == '"' && ch == '\\' {
                    match chars.peek().copied() {
                        Some('"') | Some('\\') => {
                            current.push(chars.next().unwrap());
                        }
                        _ => current.push(ch),
                    }
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    in_word = true;
                }
                '\\' => match chars.peek().copied() {
                    Some(' ') | Some('\t') | Some('\'') | Some('"') | Some('\\') => {
                        current.push(chars.next().unwrap());
                        in_word = true;
                    }
                    _ => {
                        current.push(ch);
                        in_word = true;
                    }
                },
                ch if ch.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                _ => {
                    current.push(ch);
                    in_word = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

/// Lowercase base name of the first argv word (`C:\...\git.exe` → `git`).
pub fn program_base(word: &str) -> String {
    Path::new(word)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(word)
        .to_ascii_lowercase()
}

fn arg_is(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| {
        let lowered = arg.to_ascii_lowercase();
        lowered == name
            || lowered
                .split_once('=')
                .is_some_and(|(head, _)| head == name)
    })
}

fn arg_has_prefix(args: &[String], prefixes: &[&str]) -> bool {
    args.iter().any(|arg| {
        let lowered = arg.to_ascii_lowercase();
        prefixes.iter().any(|prefix| lowered.starts_with(prefix))
    })
}

/// Interpreter/exec-shaped arguments for an allowlisted binary. The shell
/// policy is argv-based: admitting a binary does not admit its code-execution
/// flags. A user can still opt in by putting the binary on `shell_allowlist`,
/// but these flags stay refused.
pub fn exec_flag_refusal(base: &str, words: &[String]) -> Option<String> {
    let args = words.get(1..).unwrap_or(&[]);
    let refusal = |flag: &str| Some(format!("`{base} {flag}` executes arbitrary code"));
    // Short options that accept an attached value: `python -cprint(1)`,
    // `python -mhttp.server`, `node -eprint(1)` tokenize as one word, so an
    // exact match alone would miss them.
    let attached_short = |names: &[&str]| {
        args.iter().any(|arg| {
            let lowered = arg.to_ascii_lowercase();
            !lowered.starts_with("--")
                && lowered.len() > 2
                && names.iter().any(|name| {
                    lowered.starts_with(name) && !lowered.starts_with(&format!("{name}-"))
                })
        })
    };
    // Flags that are dangerous for every binary.
    if arg_has_prefix(args, &["--exec", "--ext-diff"]) {
        return refusal("--exec");
    }
    if arg_has_prefix(args, &["-encodedcommand", "--encodedcommand"]) {
        return refusal("-EncodedCommand");
    }
    match base {
        "powershell" | "pwsh" => {
            if arg_has_prefix(args, &["-enc", "-e", "-command", "-c"]) {
                return refusal("-EncodedCommand/-Command");
            }
        }
        "cmd" => {
            if arg_has_prefix(args, &["/c", "/k"]) {
                return refusal("/c");
            }
        }
        "node" | "nodejs" => {
            if arg_is(args, "-e")
                || arg_is(args, "--eval")
                || arg_is(args, "-p")
                || arg_is(args, "--print")
                || attached_short(&["-e", "-p"])
            {
                return refusal("-e");
            }
        }
        "bun" => {
            // `bun -e` evaluates source, `bun x` fetches and runs a package.
            if arg_is(args, "-e")
                || arg_is(args, "--eval")
                || arg_is(args, "-x")
                || arg_is(args, "--exec")
                || attached_short(&["-e"])
                || words.get(1).is_some_and(|arg| {
                    arg.eq_ignore_ascii_case("x") || arg.eq_ignore_ascii_case("exec")
                })
            {
                return refusal("-e/-x");
            }
        }
        b if b.starts_with("python") => {
            if arg_is(args, "-c") || arg_is(args, "-m") || attached_short(&["-c", "-m"]) {
                return refusal("-c");
            }
        }
        "sh" | "bash" | "zsh" | "dash" | "ksh" => {
            if arg_is(args, "-c") || attached_short(&["-c"]) {
                return refusal("-c");
            }
        }
        "perl" | "ruby" => {
            if arg_is(args, "-e") || attached_short(&["-e"]) {
                return refusal("-e");
            }
        }
        "npm" => {
            // `npm exec` / `npm x` install and run a package's binary.
            if words.get(1).is_some_and(|arg| {
                arg.eq_ignore_ascii_case("exec") || arg.eq_ignore_ascii_case("x")
            }) {
                return Some("`npm exec` fetches and runs an arbitrary package".into());
            }
        }
        "tar" => {
            // `--to-command`, `-I/--use-compress-program`, checkpoint and
            // volume scripts, and the remote-shell helpers all spawn programs.
            // `-F` is case-sensitive: `-f` is the archive name.
            let info_script = args.iter().any(|arg| {
                arg.strip_prefix('-')
                    .is_some_and(|body| !body.starts_with('-') && body.contains('F'))
            });
            if info_script
                || arg_has_prefix(
                    args,
                    &[
                        "--to-command",
                        "-i",
                        "--use-compress-program",
                        "--checkpoint-action",
                        "--info-script",
                        "--new-volume-script",
                        "--rsh-command",
                        "--rmt-command",
                    ],
                )
            {
                return Some("`tar` compression/command hooks execute an external program".into());
            }
        }
        _ => {}
    }
    None
}

/// `git` arguments that execute code or write a file regardless of the
/// subcommand. Checked by the permission gate (so no allow rule can whitelist
/// them) and by the sandbox.
pub fn git_forbidden_args(words: &[String]) -> Option<String> {
    if words.first().map(|word| program_base(word)).as_deref() != Some("git") {
        return None;
    }
    let mut after_subcommand = false;
    for arg in &words[1..] {
        if arg.starts_with('!') {
            return Some("`git` alias execution (`!`) is not allowed".into());
        }
        let lowered = arg.to_ascii_lowercase();
        if !after_subcommand {
            if !arg.starts_with('-') {
                after_subcommand = true;
                continue;
            }
            // Global options that inject config or an external program.
            if lowered == "-c"
                || (lowered.starts_with("-c") && lowered.contains('='))
                || lowered.starts_with("--config-env")
                || lowered.starts_with("--exec-path")
            {
                return Some(format!("`git {arg}` can execute arbitrary code"));
            }
            continue;
        }
        if (lowered.starts_with("-o") && !lowered.starts_with("--"))
            || lowered == "--output"
            || lowered.starts_with("--output=")
            || lowered.starts_with("--open-files-in-pager")
            || lowered.starts_with("--ext-diff")
            || lowered.starts_with("--upload-pack")
            || lowered.starts_with("--receive-pack")
            || lowered.contains("=!")
        {
            return Some(format!("`git {arg}` can write a file or run a program"));
        }
    }
    None
}

/// Policy refusal for a command string that does not depend on the allowlist.
/// Used by the permission gate so `allow` rules cannot admit exec-shaped argv.
pub fn command_policy_refusal(command: &str) -> Option<String> {
    if has_shell_metacharacters(command) {
        return Some("shell command chaining and substitution are blocked by the sandbox".into());
    }
    let words = split_argv(command)?;
    let first = words.first()?;
    if let Some(reason) = qualified_program_refusal(first) {
        return Some(reason);
    }
    let base = program_base(first);
    exec_flag_refusal(&base, &words).or_else(|| git_forbidden_args(&words))
}

#[derive(Debug, Clone)]
pub struct Jail {
    pub cwd: PathBuf,
    pub extra: Vec<PathBuf>,
    pub mode: SandboxMode,
    pub shell_allowlist: Vec<String>,
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

fn normalize_existing_or_parent(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return strip_verbatim(c);
    }
    // The path may not exist yet, with one or more intermediate components
    // being symlinks pointing elsewhere. Walk up the ancestor chain to the
    // first ancestor that actually exists, canonicalize it, then re-append
    // the missing tail — the containment check then sees the real location
    // behind any symlink instead of the purely lexical path.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    while let Some(parent) = cur.parent() {
        if let Ok(c) = parent.canonicalize() {
            // `cur` itself is the deepest missing component under `parent`;
            // it belongs to the re-appended tail too.
            if let Some(name) = cur.file_name() {
                tail.push(name.to_os_string());
            }
            let mut out = strip_verbatim(c);
            for comp in tail.iter().rev() {
                out.push(comp);
            }
            return out;
        }
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        cur = parent.to_path_buf();
    }
    strip_verbatim(path.to_path_buf())
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let p = strip_verbatim(path.to_path_buf());
    let r = strip_verbatim(root.to_path_buf());
    p.starts_with(&r)
}

/// Whether `path` resolves inside `workspace`, following symlinks for the
/// deepest existing ancestor. Used by trust and plan-mode checks that run
/// before a [`Jail`] exists.
pub fn path_within_workspace(workspace: &Path, path: &Path) -> bool {
    workspace_relative(workspace, path).is_some()
}

/// Path relative to `workspace` after normalization, or `None` when it lies
/// outside. Both sides are normalized the same way, so verbatim Windows
/// prefixes and `..` segments cannot make a comparison miss.
pub fn workspace_relative(workspace: &Path, path: &Path) -> Option<PathBuf> {
    let root = normalize_existing_or_parent(workspace);
    let resolved = normalize_existing_or_parent(path);
    resolved.strip_prefix(&root).ok().map(Path::to_path_buf)
}

/// Directory prefixes whose absolute binaries are safe to execute despite
/// being path-qualified: system-installed tools, never a workspace file.
#[cfg(windows)]
fn trusted_program_roots() -> Vec<PathBuf> {
    let mut roots = vec![system_root()];
    for name in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(name).map(PathBuf::from) {
            if dir.is_absolute() {
                roots.push(dir);
            }
        }
    }
    roots
}

#[cfg(not(windows))]
fn trusted_program_roots() -> Vec<PathBuf> {
    [
        "/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/sbin",
        "/usr/sbin",
        "/opt/homebrew/bin",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// Refuse a first argv word that names a program by path instead of a bare
/// name on PATH. `.\git.exe`, `tools\git.exe`, `..\bin\git.exe` and `C:git`
/// all resolve relative to the repository, so matching such a word by its
/// stem would let a cloned checkout shadow an allowlisted binary. Absolute
/// paths remain usable only under a trusted system directory.
pub fn qualified_program_refusal(word: &str) -> Option<String> {
    let path = Path::new(word);
    let drive_relative =
        word.len() > 1 && word.as_bytes()[0].is_ascii_alphabetic() && word.as_bytes()[1] == b':';
    let qualified =
        path.is_absolute() || drive_relative || word.contains('/') || word.contains('\\');
    if !qualified {
        return None;
    }
    if path.is_absolute() {
        let Ok(resolved) = path.canonicalize() else {
            return Some(format!(
                "`{word}` cannot be resolved; only PATH/system binaries may run"
            ));
        };
        if trusted_program_roots()
            .iter()
            .any(|root| path_is_within(&resolved, &normalize_existing_or_parent(root)))
        {
            return None;
        }
    }
    Some(format!(
        "`{word}` is a path-qualified program; only programs resolved on PATH or under a system directory may run"
    ))
}

impl Jail {
    pub fn new(cwd: PathBuf, extra: Vec<PathBuf>, mode: SandboxMode, allow: Vec<String>) -> Self {
        Self {
            cwd,
            extra,
            mode,
            shell_allowlist: allow,
        }
    }

    pub fn allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.cwd.clone()];
        roots.extend(self.extra.iter().cloned());
        roots
    }

    /// Resolve a path against the jail. Only `DangerFullAccess` escapes
    /// containment; `DockerIsolated` behaves exactly like `WorkspaceWrite`
    /// because the file tools run on the host against the directory that is
    /// mounted into the container.
    pub fn resolve(&self, p: &str) -> Result<PathBuf> {
        let path = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.cwd.join(p)
        };
        let resolved = normalize_existing_or_parent(&path);
        if self.mode == SandboxMode::DangerFullAccess {
            return Ok(resolved);
        }
        let ok = self.allowed_roots().iter().any(|root| {
            let r = normalize_existing_or_parent(root);
            path_is_within(&resolved, &r)
        });
        if !ok {
            bail!("path {} is outside the workspace jail", resolved.display());
        }
        Ok(resolved)
    }

    pub fn assert_write(&self, p: &str) -> Result<PathBuf> {
        if self.mode == SandboxMode::ReadOnly {
            bail!("sandbox is read-only; writes are blocked");
        }
        self.resolve(p)
    }

    /// Gate a shell command. `DockerIsolated` intentionally shares the
    /// `WorkspaceWrite` rules (chaining blocked, allowlist enforced): the
    /// command string runs inside the container, but the same policy decides
    /// what may run at all. Only `DangerFullAccess` bypasses the check.
    pub fn assert_shell(&self, command: &str) -> Result<()> {
        if self.mode == SandboxMode::DangerFullAccess {
            return Ok(());
        }
        if has_shell_metacharacters(command) {
            bail!("shell command chaining and substitution are blocked by the sandbox");
        }
        let words = split_argv(command)
            .filter(|words| !words.is_empty())
            .ok_or_else(|| anyhow::anyhow!("shell command could not be parsed"))?;
        if let Some(reason) = qualified_program_refusal(&words[0]) {
            bail!("shell command rejected by policy: {reason}");
        }
        let base = program_base(&words[0]);
        if !self
            .shell_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&base))
        {
            bail!("shell command `{base}` is not on the sandbox allowlist");
        }
        // Admitting a binary does not admit its code-execution flags.
        if let Some(reason) =
            exec_flag_refusal(&base, &words).or_else(|| git_forbidden_args(&words))
        {
            bail!("shell command rejected by policy: {reason}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(windows)]
    fn run_confined(jail: &Jail, command: &str) -> String {
        let mut child = match spawn_host_shell(jail, command) {
            Ok(child) => child,
            Err(error) => return format!("spawn failed: {error}"),
        };
        let read = |pipe: Option<Box<dyn Read + Send>>| -> String {
            let Some(mut pipe) = pipe else {
                return String::new();
            };
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            if bytes.is_empty() {
                return String::new();
            }
            format!(
                "[{} bytes] {}",
                bytes.len(),
                String::from_utf8_lossy(&bytes)
            )
        };
        let out = read(child.take_stdout());
        let err = read(child.take_stderr());
        let status = child
            .wait()
            .map(|status| status.to_string())
            .unwrap_or_else(|error| error.to_string());
        format!("[exit {status}] {out}{err}")
    }

    #[cfg(windows)]
    #[test]
    fn confined_roots_follow_the_mode() {
        let dir = tempdir().unwrap();
        let extra = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![extra.path().to_path_buf()],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let roots = confined_roots(&jail);
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|root| root.recursive));
        // read-only controls the same roots, only in the opposite direction.
        let read_only = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::ReadOnly,
            vec![],
        );
        assert_eq!(confined_roots(&read_only).len(), 1);
        let danger = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::DangerFullAccess,
            vec![],
        );
        assert!(confined_roots(&danger).is_empty());
    }

    #[test]
    fn enforcement_is_reported_never_assumed() {
        assert_eq!(
            enforcement(SandboxMode::DangerFullAccess),
            Enforcement::Unconfined
        );
        #[cfg(windows)]
        {
            let status = enforcement(SandboxMode::WorkspaceWrite);
            assert_eq!(
                status,
                Enforcement::Enforced,
                "workspace-write not enforced: {}",
                status.status()
            );
            assert_eq!(
                enforcement(SandboxMode::ReadOnly),
                Enforcement::Enforced,
                "read-only not enforced: {}",
                enforcement(SandboxMode::ReadOnly).status()
            );
        }
        #[cfg(not(windows))]
        {
            let status = enforcement(SandboxMode::WorkspaceWrite);
            assert!(
                matches!(status, Enforcement::Degraded(_)),
                "an untestable platform must not claim enforcement: {}",
                status.status()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn workspace_write_boundary_allows_inside_and_denies_outside() {
        let base = tempdir().unwrap();
        let workspace = base.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let jail = Jail::new(
            workspace.clone(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );

        let echo = run_confined(&jail, "Write-Output hello");
        println!("echo: {echo}");
        assert!(
            echo.contains("hello"),
            "child output was not captured: {echo}"
        );

        let inside = run_confined(&jail, "Set-Content -Path ok.txt -Value hi");
        let wrote_inside = workspace.join("ok.txt").exists();
        println!("inside exit/output: {inside}");
        assert!(wrote_inside, "workspace write was denied: {inside}");

        let escape = base.path().join("escape.txt");
        let script = format!(
            "$ErrorActionPreference='Stop'; try {{ Set-Content -LiteralPath '{}' -Value hi; 'WROTE-OUTSIDE' }} catch {{ 'DENIED: ' + $_.Exception.Message }}",
            escape.display()
        );
        let outside = run_confined(&jail, &script);
        println!("outside output: {outside}");
        assert!(
            !escape.exists(),
            "write outside the workspace landed: {outside}"
        );
        assert!(
            outside.to_ascii_lowercase().contains("denied"),
            "expected an OS-level denial, got: {outside}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn read_only_revokes_a_previously_writable_tree() {
        let workspace = tempdir().unwrap();
        let ws = workspace.path();
        let write_jail = Jail::new(
            ws.to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let out = run_confined(&write_jail, "Set-Content -Path before.txt -Value hi");
        assert!(ws.join("before.txt").exists(), "{out}");
        assert!(
            windows::is_low(ws),
            "workspace-write must leave the tree writable by the child"
        );

        // Same tree, read-only: the label is raised back to Medium before the
        // child starts, so the previously writable path is denied again.
        let read_jail = Jail::new(ws.to_path_buf(), vec![], SandboxMode::ReadOnly, vec![]);
        let target = ws.join("after.txt");
        let script = format!(
            "$ErrorActionPreference='Stop'; try {{ Set-Content -LiteralPath '{}' -Value hi; 'WROTE' }} catch {{ 'DENIED: ' + $_.Exception.Message }}",
            target.display()
        );
        let denied = run_confined(&read_jail, &script);
        println!("read-only over a previously writable tree: {denied}");
        assert!(!target.exists(), "read-only wrote a Low tree: {denied}");
        assert!(denied.to_ascii_lowercase().contains("denied"), "{denied}");
        assert!(
            !windows::is_low(ws),
            "read-only must not leave the tree Low"
        );
    }

    #[cfg(windows)]
    #[test]
    fn read_only_boundary_denies_writes_but_allows_reads() {
        let workspace = tempdir().unwrap();
        std::fs::write(workspace.path().join("readable.txt"), "hello").unwrap();
        let jail = Jail::new(
            workspace.path().to_path_buf(),
            vec![],
            SandboxMode::ReadOnly,
            vec![],
        );

        let blocked = workspace.path().join("blocked.txt");
        let script = format!(
            "$ErrorActionPreference='Stop'; try {{ Set-Content -LiteralPath '{}' -Value hi; 'WROTE' }} catch {{ 'DENIED: ' + $_.Exception.Message }}",
            blocked.display()
        );
        let out = run_confined(&jail, &script);
        println!("read-only write output: {out}");
        assert!(!blocked.exists(), "read-only mode allowed a write: {out}");
        assert!(out.to_ascii_lowercase().contains("denied"), "{out}");

        let read = run_confined(&jail, "Get-Content readable.txt");
        println!("read-only read output: {read}");
        assert!(read.contains("hello"), "reads must stay allowed: {read}");
    }

    #[test]
    fn jail_blocks_outside_path() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["git".into()],
        );
        let outside = std::env::temp_dir().join("varynth-jail-should-fail.txt");
        if outside.starts_with(dir.path()) {
            return;
        }
        assert!(jail.resolve(outside.to_str().unwrap()).is_err());
        assert!(jail.resolve("inside.txt").is_ok());
    }

    #[test]
    fn allowlisted_binaries_still_reject_exec_flags() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["powershell".into(), "git".into(), "python".into()],
        );
        assert!(jail
            .assert_shell("powershell -NoProfile -EncodedCommand AAAA")
            .is_err());
        assert!(jail.assert_shell("python -c \"print(1)\"").is_err());
        assert!(jail.assert_shell("git -c alias.p=!calc p").is_err());
        assert!(jail.assert_shell("git diff --output=out.txt").is_err());
        assert!(jail.assert_shell("git status").is_ok());
    }

    #[test]
    fn interpreter_and_package_exec_forms_are_refused() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![
                "bun".into(),
                "npm".into(),
                "tar".into(),
                "python".into(),
                "node".into(),
                "perl".into(),
            ],
        );
        for command in [
            "bun -e \"console.log(1)\"",
            "bun --eval \"console.log(1)\"",
            "bun -x some-package",
            "bun --exec some-package",
            "bun x some-package",
            "bun exec some-package",
            "npm exec some-package",
            "npm x some-package",
            "tar --to-command=calc -xf a.tar",
            "tar -Icalc -xf a.tar",
            "tar --use-compress-program=calc -xf a.tar",
            "python -cprint(1)",
            "python -mhttp.server",
            "node -eprint(1)",
            "perl -eprint(1)",
        ] {
            assert!(
                jail.assert_shell(command).is_err(),
                "{command} must be refused"
            );
        }
        assert!(jail.assert_shell("tar -xf archive.tar").is_ok());
        assert!(jail.assert_shell("npm install").is_ok());
    }

    #[test]
    fn powershell_grouping_and_untokenized_metacharacters_are_denied() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["type".into(), "echo".into(), "cmd".into()],
        );
        for command in ["type (ii evil.bat)", "echo (calc)", "type (cmd /c x)"] {
            let policy = command_policy_refusal(command);
            let sandbox = jail.assert_shell(command);
            println!(
                "denied: {command} -> shell policy: {} | assert_shell: {}",
                policy.clone().unwrap_or_else(|| "<allowed>".into()),
                sandbox
                    .as_ref()
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "<allowed>".into())
            );
            assert!(policy.is_some(), "{command} must be denied by shell policy");
            assert!(sandbox.is_err(), "{command} must be denied by assert_shell");
            assert!(
                !crate::permissions::plan_shell_allowed(command),
                "{command} must be denied in plan mode"
            );
        }
        for command in [
            "echo {calc}",
            "echo a,b",
            "echo hello!",
            "echo #c",
            "echo @x",
            "echo [x]",
        ] {
            assert!(
                jail.assert_shell(command).is_err(),
                "{command} must be denied"
            );
        }
        assert!(jail.assert_shell("echo hello").is_ok());
    }

    #[test]
    fn tar_checkpoint_and_remote_scripts_are_denied() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["tar".into()],
        );
        for command in [
            "tar --checkpoint-action=exec=calc -xf a.tar",
            "tar --checkpoint-action exec=calc -xf a.tar",
            "tar -F script -xf a.tar",
            "tar --info-script=script -xf a.tar",
            "tar --new-volume-script=script -xf a.tar",
            "tar --rsh-command=ssh -xf a.tar",
            "tar --rmt-command=rmt -xf a.tar",
        ] {
            assert!(
                jail.assert_shell(command).is_err(),
                "{command} must be refused"
            );
            assert!(
                command_policy_refusal(command).is_some(),
                "{command} must be denied by shell policy"
            );
        }
        assert!(jail.assert_shell("tar -xf archive.tar").is_ok());
        assert!(jail.assert_shell("tar -f archive.tar -x").is_ok());
    }

    #[test]
    fn child_env_keeps_username_and_drops_provider_secrets() {
        let _guard = crate::config::lock_process_env();
        let keys = [
            "USERNAME",
            "USER",
            "USERDOMAIN",
            "VARYNTH_ACCESS_TOKEN",
            "OPENAI_API_KEY",
        ];
        let saved: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        std::env::set_var("USERNAME", "decisions-user");
        std::env::set_var("USER", "decisions-user");
        std::env::set_var("USERDOMAIN", "WORKGROUP");
        std::env::set_var("VARYNTH_ACCESS_TOKEN", "super-secret-token-value");
        std::env::set_var("OPENAI_API_KEY", "sk-super-secret-key");
        let env = child_env();
        let value_of = |name: &str| {
            env.iter().find_map(|(key, value)| {
                key.to_string_lossy()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.to_string_lossy().into_owned())
            })
        };
        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        assert_eq!(value_of("USERNAME").as_deref(), Some("decisions-user"));
        assert_eq!(value_of("USER").as_deref(), Some("decisions-user"));
        assert_eq!(value_of("USERDOMAIN").as_deref(), Some("WORKGROUP"));
        assert!(value_of("VARYNTH_ACCESS_TOKEN").is_none());
        assert!(value_of("OPENAI_API_KEY").is_none());
        assert!(env.iter().all(|(_, value)| {
            let text = value.to_string_lossy();
            !text.contains("super-secret-token-value") && !text.contains("sk-super-secret-key")
        }));
    }

    #[test]
    fn path_qualified_programs_cannot_shadow_the_allowlist() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("git.exe"), "not a binary").unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["git".into()],
        );
        for command in [
            r".\git.exe status",
            r"tools\git.exe status",
            "C:git status",
            "./git.exe status",
            r"..\git.exe status",
        ] {
            assert!(
                jail.assert_shell(command).is_err(),
                "{command} must be refused"
            );
            assert!(
                command_policy_refusal(command).is_some(),
                "{command} must be denied before approval"
            );
            println!(
                "denied: {command} -> {}",
                command_policy_refusal(command).unwrap()
            );
        }
        // Bare names resolved on PATH still pass.
        assert!(jail.assert_shell("git status").is_ok());
    }

    #[test]
    fn host_shell_is_resolved_to_an_absolute_path() {
        let (program, _args) = resolve_host_shell("echo hi").unwrap();
        assert!(
            Path::new(&program).is_absolute(),
            "the host shell must never be a bare name: {program}"
        );
    }

    #[test]
    fn allowlist_rejects_unknown_bin() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["git".into()],
        );
        assert!(jail.assert_shell("format C:").is_err());
        assert!(jail.assert_shell("git status").is_ok());
        assert!(jail.assert_shell("git; format C:").is_err());
        assert!(jail.assert_shell("git && whoami").is_err());
    }

    #[test]
    fn symlink_escape_blocked_even_for_missing_paths() {
        let dir = tempdir().unwrap();
        let jail_root = dir.path();
        std::fs::create_dir_all(jail_root.join("real")).unwrap();
        let outside = tempdir().unwrap();
        if outside.path().starts_with(jail_root) {
            return;
        }
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(outside.path(), jail_root.join("link"));
        #[cfg(not(windows))]
        let linked = std::os::unix::fs::symlink(outside.path(), jail_root.join("link"));
        if linked.is_err() {
            // Symlinks need OS privileges (e.g. Windows developer mode or
            // elevation). Directory junctions need neither, and canonicalize
            // resolves them the same way; fall back to one before skipping.
            #[cfg(windows)]
            {
                let made = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(jail_root.join("link"))
                    .arg(outside.path())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !made {
                    // The environment cannot create links at all; skip rather
                    // than fail on environment restrictions.
                    return;
                }
            }
            #[cfg(not(windows))]
            return;
        }
        let jail = Jail::new(
            jail_root.to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        std::fs::write(outside.path().join("existing.txt"), b"x").unwrap();

        // Existing target behind the symlink resolves outside the jail.
        assert!(jail.resolve("link/existing.txt").is_err());
        // One missing level behind the symlink still resolves outside.
        assert!(jail.resolve("link/missing.txt").is_err());
        // Multi-level missing tail behind the symlink: the lexical fallback
        // used to pass containment here; the resolved location must not.
        assert!(jail.resolve("link/sub/dir/newfile.txt").is_err());
    }

    #[test]
    fn benign_missing_paths_still_resolve() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("real/deep")).unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        assert!(jail.resolve("inside.txt").is_ok());
        assert!(jail.resolve("real/a.txt").is_ok());
        assert!(jail.resolve("real/deep/very/new.txt").is_ok());
    }

    #[test]
    fn sandbox_mode_roundtrip_including_docker_isolated() {
        for mode in [
            SandboxMode::ReadOnly,
            SandboxMode::WorkspaceWrite,
            SandboxMode::DangerFullAccess,
            SandboxMode::DockerIsolated,
        ] {
            assert_eq!(SandboxMode::parse(mode.as_str()), mode);
        }
        assert_eq!(
            SandboxMode::parse("docker-isolated"),
            SandboxMode::DockerIsolated
        );
        assert_eq!(SandboxMode::DockerIsolated.as_str(), "docker-isolated");
        // Unrecognized values keep falling back to workspace-write.
        assert_eq!(SandboxMode::parse("yolo"), SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn docker_isolated_matches_workspace_write_rules() {
        let dir = tempdir().unwrap();
        let mk = |mode| Jail::new(dir.path().to_path_buf(), vec![], mode, vec!["git".into()]);
        let docker = mk(SandboxMode::DockerIsolated);
        let workspace = mk(SandboxMode::WorkspaceWrite);

        // File tools run on the host against the mounted dir, so resolution
        // and write gating must be identical to workspace-write.
        assert!(docker.resolve("inside.txt").is_ok());
        assert!(docker.assert_write("new.txt").is_ok());
        let outside = std::env::temp_dir().join("varynth-docker-jail-escape.txt");
        if !outside.starts_with(dir.path()) {
            assert_eq!(
                docker.resolve(outside.to_str().unwrap()).is_err(),
                workspace.resolve(outside.to_str().unwrap()).is_err()
            );
            assert!(docker.resolve(outside.to_str().unwrap()).is_err());
            assert!(docker.assert_write(outside.to_str().unwrap()).is_err());
        }

        // The shell allowlist still applies under docker-isolated.
        assert!(docker.assert_shell("git status").is_ok());
        assert!(docker.assert_shell("format C:").is_err());
        assert!(docker.assert_shell("git && whoami").is_err());
        assert!(docker.assert_shell("git $(whoami)").is_err());
    }

    #[test]
    fn normalize_reappends_multi_level_missing_tail() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("real")).unwrap();
        let p = dir.path().join("real/sub/deep/newfile.txt");
        // `p` and all its missing ancestors must resolve to the real location
        // of the first existing ancestor plus the missing tail — not be lost
        // or re-rooted by the ancestor walk.
        let out = normalize_existing_or_parent(&p);
        assert_eq!(
            out,
            dir.path()
                .join("real")
                .join("sub")
                .join("deep")
                .join("newfile.txt")
        );
    }
}
