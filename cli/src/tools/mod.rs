use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::sandbox::{Jail, SandboxMode, ShellChild};

pub fn schemas() -> Vec<Value> {
    let base = vec![
        tool(
            "tool_search",
            "Search configured MCP tools by name, server or description and \
             return their full schemas. Matched tools become callable on the \
             next step. Call this before using an MCP tool that is not already \
             loaded; an empty query lists every available MCP tool.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": []
            }),
        ),
        tool(
            "read_file",
            "Read a UTF-8 text file. Optional offset/limit are 1-indexed line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"}
                },
                "required": ["path"]
            }),
        ),
        tool(
            "write_file",
            "Create or overwrite a UTF-8 file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        ),
        tool(
            "edit_file",
            "Replace one unique occurrence of old_string with new_string in a file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        tool(
            "glob_search",
            "Find files by glob pattern relative to cwd or an optional path.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["pattern"]
            }),
        ),
        tool(
            "grep_search",
            "Search file contents with a regex. Returns matching lines.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                    "glob": {"type": "string"}
                },
                "required": ["pattern"]
            }),
        ),
        tool(
            "bash",
            "Run a shell command inside the workspace jail. On Windows uses \
             PowerShell; in docker-isolated mode the command runs inside the \
             sandbox container instead. In workspace-write and read-only the \
             child also runs behind an OS write boundary, so writes outside \
             the workspace fail with an access-denied error.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["command"]
            }),
        ),
        tool(
            "web_fetch",
            "HTTP GET a URL and return text (truncated). Use for docs and public pages.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"}
                },
                "required": ["url"]
            }),
        ),
        tool(
            "memory_read",
            "Read the layered Varynth MEMORY.md context.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "memory_append",
            "Append a durable fact or decision to the project MEMORY.md file.",
            json!({
                "type": "object",
                "properties": {"entry": {"type": "string"}},
                "required": ["entry"]
            }),
        ),
        tool(
            "session_send",
            "Send a message to another Varynth session on the local message bus. `to` is a session id or the alias `latest`.",
            json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string"},
                    "text": {"type": "string"}
                },
                "required": ["to", "text"]
            }),
        ),
        tool(
            "session_inbox",
            "Read and clear messages sent to this session by other Varynth sessions.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "todo_tracker",
            "Manage and track tasks/todos for the current workspace. Actions: `add` (with `text`), `done` (with `id`), `remove` (with `id`), `clear`, `list`.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "done", "remove", "clear", "list"]},
                    "text": {"type": "string"},
                    "id": {"type": "integer"}
                },
                "required": ["action"]
            }),
        ),
        tool(
            "lsp_diagnostics",
            "Probe available language servers and check compiler/linter diagnostics for a file or workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": []
            }),
        ),
    ];
    let mut all = base;
    all.extend(crate::computer_use::schemas());
    all
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

/// Per-call context for tools that need agent state beyond the sandbox jail.
pub struct ToolCtx<'a> {
    pub bus: &'a crate::mailbox::Bus,
    pub session_id: Option<&'a str>,
    /// When present, overwriting files records a pre-write snapshot for
    /// `/undo`. None in non-session contexts (exec, subagents).
    pub checkpoints: Option<&'a crate::checkpoint::CheckpointStore>,
    /// Docker settings for the `docker-isolated` sandbox (image + network).
    /// None means "use the defaults" (image `varynth-sandbox:latest`,
    /// network none).
    pub docker: Option<DockerOpts>,
}

/// Docker execution settings for the docker-isolated sandbox.
#[derive(Debug, Clone)]
pub struct DockerOpts {
    pub image: String,
    /// "none" (default), "bridge" or "host".
    pub network: String,
}

impl Default for DockerOpts {
    fn default() -> Self {
        Self {
            image: "varynth-sandbox:latest".into(),
            network: "none".into(),
        }
    }
}

pub fn dispatch(name: &str, args: &Value, jail: &Jail, ctx: &ToolCtx) -> Result<String> {
    match name {
        "read_file" => read_file(args, jail, ctx),
        "write_file" => write_file(args, jail, ctx),
        "edit_file" => edit_file(args, jail, ctx),
        "glob_search" => glob_search(args, jail, ctx),
        "grep_search" => grep_search(args, jail, ctx),
        "bash" => bash(args, jail, ctx),
        "web_fetch" => web_fetch(args, ctx),
        "session_send" => session_send(args, ctx),
        "session_inbox" => session_inbox(ctx),
        "todo_tracker" => todo_tracker(args, jail),
        "lsp_diagnostics" => lsp_diagnostics(args, jail),
        other if crate::computer_use::handles(other) => {
            crate::computer_use::dispatch_in_jail(other, args, jail)
        }
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("missing string arg `{key}`"))
}

fn positive_limit(args: &Value, key: &str, default: u64, max: u64) -> Result<u64> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("`{key}` must be a nonnegative integer"))?,
    };
    if value == 0 || value > max {
        anyhow::bail!("`{key}` must be between 1 and {max}");
    }
    Ok(value)
}

fn read_file(args: &Value, jail: &Jail, _ctx: &ToolCtx) -> Result<String> {
    let path = jail.resolve(&arg_str(args, "path")?)?;
    let offset = positive_limit(args, "offset", 1, 1_000_000)? as usize;
    let limit = positive_limit(args, "limit", 2000, 100_000)? as usize;
    let text = fs::read_to_string(&path)?;
    let lines: Vec<&str> = text.lines().collect();
    let start = offset.saturating_sub(1).min(lines.len());
    let end = (start + limit).min(lines.len());
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>6}\t{line}\n", start + i + 1));
    }
    if out.is_empty() {
        out = "(empty file)\n".into();
    }
    Ok(out)
}

/// Capture the file's current content so `/undo` can restore it. A failed
/// snapshot never fails the write — it is logged and the tool proceeds.
fn snapshot_for_undo(ctx: &ToolCtx, path: &Path) {
    if let (Some(store), Some(session)) = (ctx.checkpoints, ctx.session_id) {
        if let Err(error) = store.snapshot(session, path) {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "checkpoint snapshot failed; writing anyway"
            );
        }
    }
}

fn write_file(args: &Value, jail: &Jail, ctx: &ToolCtx) -> Result<String> {
    let path = jail.assert_write(&arg_str(args, "path")?)?;
    let content = arg_str(args, "content")?;
    snapshot_for_undo(ctx, &path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;
    Ok(format!("wrote {}", path.display()))
}

fn edit_file(args: &Value, jail: &Jail, ctx: &ToolCtx) -> Result<String> {
    let path = jail.assert_write(&arg_str(args, "path")?)?;
    let old = arg_str(args, "old_string")?;
    let new = arg_str(args, "new_string")?;
    let text = fs::read_to_string(&path)?;
    let count = text.matches(&old).count();
    if count == 0 {
        anyhow::bail!("old_string not found in {}", path.display());
    }
    if count > 1 {
        anyhow::bail!("old_string matched {count} times; must be unique");
    }
    let updated = text.replacen(&old, &new, 1);
    snapshot_for_undo(ctx, &path);
    fs::write(&path, updated)?;
    Ok(format!("edited {}", path.display()))
}

fn glob_search(args: &Value, jail: &Jail, _ctx: &ToolCtx) -> Result<String> {
    let pattern = arg_str(args, "pattern")?;
    // Parse before walking so malformed patterns fail even when the directory
    // is empty. The glob crate also rejects malformed absolute joins below.
    glob::Pattern::new(&pattern)
        .map_err(|error| anyhow::anyhow!("invalid glob pattern `{pattern}`: {error}"))?;
    let base = if let Some(p) = args.get("path").and_then(Value::as_str) {
        jail.resolve(p)?
    } else {
        jail.cwd.clone()
    };
    let walker = glob::glob(&base.join(&pattern).to_string_lossy())?;
    let mut hits = Vec::new();
    for p in walker.flatten().take(200) {
        if let Ok(path) = jail.resolve(&p.to_string_lossy()) {
            hits.push(path.display().to_string());
        }
    }
    if hits.is_empty() {
        Ok("(no matches)".into())
    } else {
        Ok(hits.join("\n"))
    }
}

fn grep_search(args: &Value, jail: &Jail, _ctx: &ToolCtx) -> Result<String> {
    let pattern = arg_str(args, "pattern")?;
    let re = regex::Regex::new(&pattern)?;
    let base = if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        jail.resolve(p)?
    } else {
        jail.cwd.clone()
    };
    let glob_pat = args
        .get("glob")
        .and_then(|v| v.as_str())
        .unwrap_or("*.{rs,toml,md,json,ts,js,py,txt}");
    glob::Pattern::new(glob_pat).context("invalid grep glob pattern")?;
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(&base)
        .into_iter()
        .filter_map(|e| e.ok());
    for entry in walker.take(4000) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if jail.resolve(&path.to_string_lossy()).is_err() {
            continue;
        }
        if !glob_ok(path, glob_pat)? {
            continue;
        }
        if let Ok(text) = fs::read_to_string(path) {
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    out.push(format!("{}:{}:{}", path.display(), i + 1, line));
                    if out.len() >= 200 {
                        break;
                    }
                }
            }
        }
        if out.len() >= 200 {
            break;
        }
    }
    if out.is_empty() {
        Ok("(no matches)".into())
    } else {
        Ok(out.join("\n"))
    }
}

fn glob_ok(path: &Path, pat: &str) -> Result<bool> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let pattern = glob::Pattern::new(pat)
        .map_err(|e| anyhow::anyhow!("invalid glob pattern `{pat}`: {e}"))?;
    Ok(pattern.matches(name))
}

fn bash(args: &Value, jail: &Jail, ctx: &ToolCtx) -> Result<String> {
    let command = arg_str(args, "command")?;
    jail.assert_shell(&command)?;
    let timeout_ms = positive_limit(args, "timeout_ms", 120_000, 600_000)?;
    let activity_id = if cfg!(test) {
        None
    } else {
        crate::activity::ActivityLog::start(crate::activity::ActivityKind::Shell, &command, "shell")
            .ok()
    };
    let started = Instant::now();
    let result = (|| -> Result<String> {
        if let Some(routed) = crate::computer_use::try_shared_cli(&command) {
            return routed;
        }
        let (child, container) = spawn_shell(jail, ctx, &command)?;
        let (status, stdout, stderr) = capture_process(child, Duration::from_millis(timeout_ms))?;
        drop(container);
        if !status.success() && jail.mode == SandboxMode::DockerIsolated {
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            anyhow::bail!("docker run failed ({status}): {}", truncate(detail, 4000));
        }
        let mut out = stdout;
        if !stderr.is_empty() {
            if !out.is_empty() {
                out.push_str("\n--- stderr ---\n");
            }
            out.push_str(&stderr);
        }
        if out.is_empty() {
            out = format!("(exit {status})");
        } else if !status.success() {
            out.push_str(&format!("\n(exit {status})"));
        }
        Ok(truncate(&out, 80_000))
    })();
    if let Some(id) = activity_id {
        let _ = crate::activity::ActivityLog::finish(
            &id,
            result.is_ok(),
            format!("finished in {}ms", started.elapsed().as_millis()),
        );
    }
    result
}

const MAX_PIPE_BYTES: usize = 80_000;

fn drain_pipe(pipe: impl Read + Send + 'static) -> mpsc::Receiver<Result<String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> Result<String> {
            let mut pipe = pipe;
            let mut kept = Vec::new();
            let mut total = 0usize;
            let mut buffer = [0u8; 8192];
            loop {
                let n = pipe.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                total = total.saturating_add(n);
                let remaining = MAX_PIPE_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..n.min(remaining)]);
            }
            let mut text = String::from_utf8_lossy(&kept).into_owned();
            if total > kept.len() {
                text.push_str(&format!("\n... truncated {} bytes", total - kept.len()));
            }
            Ok(text)
        })();
        let _ = tx.send(result);
    });
    rx
}

fn capture_process(
    mut child: ShellChild,
    timeout: Duration,
) -> Result<(ExitStatus, String, String)> {
    let stdout = drain_pipe(child.take_stdout().context("missing stdout pipe")?);
    let stderr = drain_pipe(child.take_stderr().context("missing stderr pipe")?);
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                child.kill_tree();
                anyhow::bail!("command timed out after {}ms", timeout.as_millis());
            }
            Err(error) => {
                child.kill_tree();
                return Err(error.into());
            }
        }
    };
    // A detached descendant may inherit the pipe after the shell exits.
    let read_output = |rx: mpsc::Receiver<Result<String>>| -> Result<String> {
        let remaining = timeout
            .saturating_sub(started.elapsed())
            .min(Duration::from_secs(2));
        rx.recv_timeout(remaining)
            .context("shell exited but an inherited output pipe remains open")?
    };
    let result = (|| Ok((status, read_output(stdout)?, read_output(stderr)?)))();
    if result.is_err() {
        child.kill_tree();
    }
    result
}

struct ContainerCleanup {
    docker: PathBuf,
    name: String,
}
impl Drop for ContainerCleanup {
    fn drop(&mut self) {
        if let Ok(mut child) = Command::new(&self.docker)
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
    }
}

fn spawn_shell(
    jail: &Jail,
    ctx: &ToolCtx,
    command: &str,
) -> Result<(ShellChild, Option<ContainerCleanup>)> {
    if jail.mode != SandboxMode::DockerIsolated {
        // Host execution goes through the OS write boundary when the mode
        // has one; a mode that requires the boundary fails here rather than
        // running unbounded.
        return Ok((crate::sandbox::spawn_host_shell(jail, command)?, None));
    }
    let docker = which::which("docker").map_err(|_| anyhow::anyhow!(
        "docker is required for the docker-isolated sandbox (install Docker Desktop or set sandbox = \"workspace-write\")"))?;
    let opts = docker_opts(ctx);
    validate_docker_image(&opts.image)?;
    let name = format!("varynth-{}", uuid::Uuid::new_v4());
    let mut argv = docker_run_argv(jail, &opts, command);
    argv.splice(1..1, ["--name".to_string(), name.clone()]);
    let mut cmd = Command::new(&docker);
    cmd.args(argv);
    let child = cmd
        .current_dir(&jail.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    Ok((
        ShellChild::Plain(child),
        Some(ContainerCleanup { docker, name }),
    ))
}

fn validate_docker_image(image: &str) -> Result<()> {
    anyhow::ensure!(
        !image.is_empty()
            && !image.starts_with('-')
            && image
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-/:@".contains(&c)),
        "invalid Docker image name"
    );
    Ok(())
}

/// Docker settings for this call: the configured values when present, the
/// safe defaults (`varynth-sandbox:latest`, network none) otherwise.
fn docker_opts(ctx: &ToolCtx) -> DockerOpts {
    ctx.docker.clone().unwrap_or_default()
}

/// Map the configured network onto a `--network` value: "none", "bridge"
/// and "host" pass through (case-insensitive); anything else, including an
/// empty value, falls back to the safest default, "none".
fn normalize_docker_network(network: &str) -> &str {
    match network.trim().to_ascii_lowercase().as_str() {
        "bridge" => "bridge",
        "host" => "host",
        _ => "none",
    }
}

/// Build the full argv for `docker <argv>` that runs `command` inside the
/// sandbox container with the jail cwd mounted at /work. Pure so the exact
/// invocation stays unit-testable without a daemon.
pub fn docker_run_argv(jail: &Jail, opts: &DockerOpts, command: &str) -> Vec<String> {
    // Docker Desktop accepts forward-slash Windows paths for bind mounts.
    let mount = jail.cwd.to_string_lossy().replace('\\', "/");
    vec![
        "run".into(),
        "--rm".into(),
        "-i".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--pids-limit".into(),
        "256".into(),
        "--memory".into(),
        "1g".into(),
        "--cpus".into(),
        "2".into(),
        "--read-only".into(),
        "--tmpfs".into(),
        "/tmp:rw,nosuid,nodev,size=128m".into(),
        "-e".into(),
        "HOME=/tmp".into(),
        "--network".into(),
        normalize_docker_network(&opts.network).into(),
        "-v".into(),
        format!("{mount}:/work"),
        "-w".into(),
        "/work".into(),
        opts.image.clone(),
        "sh".into(),
        "-c".into(),
        command.into(),
    ]
}

fn web_fetch(args: &Value, _ctx: &ToolCtx) -> Result<String> {
    let parsed = reqwest::Url::parse(&arg_str(args, "url")?)?;
    validate_fetch_url(&parsed)?;
    thread::scope(|scope| {
        scope
            .spawn(move || fetch_blocking(parsed))
            .join()
            .map_err(|_| anyhow::anyhow!("web_fetch worker panicked"))?
    })
}

fn validate_fetch_url(url: &reqwest::Url) -> Result<()> {
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "scheme `{}` blocked; only http/https",
        url.scheme()
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are blocked"
    );
    let host = url
        .host_str()
        .context("URL has no host")?
        .trim_matches(['[', ']']);
    if let Ok(ip) = host.parse::<IpAddr>() {
        anyhow::ensure!(
            ip_is_allowed(ip),
            "host `{host}` is blocked; only public hosts are fetchable"
        );
    }
    Ok(())
}

fn validated_addresses(url: &reqwest::Url, addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>> {
    validate_fetch_url(url)?;
    anyhow::ensure!(!addresses.is_empty(), "host resolved to no addresses");
    anyhow::ensure!(
        addresses.iter().all(|address| ip_is_allowed(address.ip())),
        "host is blocked; resolved addresses must all be public"
    );
    Ok(addresses)
}

fn resolve_fetch_target(url: &reqwest::Url) -> Result<Vec<SocketAddr>> {
    validate_fetch_url(url)?;
    let host = url
        .host_str()
        .context("URL has no host")?
        .trim_matches(['[', ']'])
        .to_string();
    let port = url.port_or_known_default().context("URL has no port")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (host.as_str(), port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect::<Vec<_>>());
        let _ = tx.send(result);
    });
    let addresses = rx
        .recv_timeout(Duration::from_secs(5))
        .context("DNS resolution timed out")??;
    validated_addresses(url, addresses)
}

const MAX_FETCH_BYTES: usize = 256 * 1024;

fn read_bounded_body(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FETCH_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let clipped = bytes.len() > MAX_FETCH_BYTES;
    bytes.truncate(MAX_FETCH_BYTES);
    let mut body = String::from_utf8_lossy(&bytes).into_owned();
    if clipped {
        body.push_str("\n... response size limit reached");
    }
    Ok(body)
}

fn fetch_blocking(mut url: reqwest::Url) -> Result<String> {
    let started = Instant::now();
    for hop in 0..=5 {
        let addresses = resolve_fetch_target(&url)?;
        let remaining = Duration::from_secs(30).saturating_sub(started.elapsed());
        anyhow::ensure!(!remaining.is_zero(), "web_fetch timed out");
        let host = url
            .host_str()
            .context("URL has no host")?
            .trim_matches(['[', ']']);
        // The validated addresses must be the ones used by the connection;
        // automatic redirects and environment proxies would bypass that check.
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(remaining)
            .connect_timeout(remaining.min(Duration::from_secs(10)))
            .resolve_to_addrs(host, &addresses)
            .build()?;
        let response = client
            .get(url.clone())
            .header("user-agent", "varynth/0.1")
            .send()?;
        let status = response.status();
        if status.is_redirection() {
            anyhow::ensure!(hop < 5, "too many redirects");
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("redirect has no Location header")?
                .to_str()?;
            url = url.join(location)?;
            validate_fetch_url(&url)?;
            continue;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = read_bounded_body(response)?;
        return Ok(format!(
            "HTTP {status}\ncontent-type: {content_type}\n\n{}",
            truncate(&body, 24_000)
        ));
    }
    anyhow::bail!("too many redirects")
}

#[cfg(test)]
fn host_is_allowed(host: &str) -> bool {
    let host = host.trim().trim_matches(['[', ']']);
    host.parse::<IpAddr>().map(ip_is_allowed).unwrap_or(false)
}

fn ip_is_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let shared = o[0] == 100 && (o[1] & 0xc0) == 64;
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || shared)
        }
        IpAddr::V6(v6) => {
            let o = v6.octets();
            if matches!(&o[..12], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]) {
                return ip_is_allowed(IpAddr::V4(Ipv4Addr::new(o[12], o[13], o[14], o[15])));
            }
            let unique_local = (o[0] & 0xfe) == 0xfc;
            let link_local = o[0] == 0xfe && (o[1] & 0xc0) == 0x80;
            let documentation = o[..4] == [0x20, 0x01, 0x0d, 0xb8];
            let compatible = o[..12] == [0; 12];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || unique_local
                || link_local
                || documentation
                || compatible)
        }
    }
}

fn session_send(args: &Value, ctx: &ToolCtx) -> Result<String> {
    let to = arg_str(args, "to")?;
    let text = arg_str(args, "text")?;
    let from = ctx.session_id.unwrap_or("agent");
    let id = ctx.bus.send(from, &to, &text)?;
    Ok(format!("sent to {to} (message {id})"))
}

/// Upper bound on how many drained messages `session_inbox` renders.
const SESSION_INBOX_MAX: usize = 50;

fn session_inbox(ctx: &ToolCtx) -> Result<String> {
    let Some(session) = ctx.session_id else {
        return Ok("(no session context; nothing to read)".into());
    };
    let mail = ctx.bus.drain(session);
    if mail.is_empty() {
        return Ok("(no messages)".into());
    }
    let (head, note) = if mail.len() > SESSION_INBOX_MAX {
        (
            &mail[..SESSION_INBOX_MAX],
            format!(
                "\n… {} further messages truncated",
                mail.len() - SESSION_INBOX_MAX
            ),
        )
    } else {
        (&mail[..], String::new())
    };
    Ok(format!("{}{}", crate::mailbox::render_inbox(head), note))
}

fn todo_tracker(args: &Value, jail: &Jail) -> Result<String> {
    let action = arg_str(args, "action")?;
    let path = jail.cwd.join(".varynth").join("todos.json");
    let mut store = crate::todo::TodoStore::load_or_new(&path);
    match action.as_str() {
        "add" => {
            let text = arg_str(args, "text")?;
            let id = store.add(&text);
            store.save(&path)?;
            Ok(format!("Added todo #{id}: {text}"))
        }
        "done" => {
            let id = args
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("missing numeric `id`"))?
                as usize;
            if store.set_done(id, true) {
                store.save(&path)?;
                Ok(format!("Marked todo #{id} as completed"))
            } else {
                anyhow::bail!("todo #{id} not found")
            }
        }
        "remove" => {
            let id = args
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("missing numeric `id`"))?
                as usize;
            if store.remove(id) {
                store.save(&path)?;
                Ok(format!("Removed todo #{id}"))
            } else {
                anyhow::bail!("todo #{id} not found")
            }
        }
        "clear" => {
            store.clear();
            store.save(&path)?;
            Ok("Cleared all todos".into())
        }
        "list" => Ok(store.format_list()),
        other => {
            anyhow::bail!("unknown action `{other}`, must be add, done, remove, clear, or list")
        }
    }
}

fn lsp_diagnostics(args: &Value, jail: &Jail) -> Result<String> {
    let registry = crate::lsp::LspRegistry::probe();
    let servers = registry.available_servers();
    let path = args.get("path").and_then(Value::as_str);
    if let Some(p) = path {
        let abs = jail.resolve(p)?;
        let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
        let relevant = match ext {
            "rs" => "rust-analyzer",
            "ts" | "tsx" | "js" | "jsx" => "typescript-language-server",
            "py" => "pyright",
            "go" => "gopls",
            _ => "generic",
        };
        let available = servers.contains(&relevant);
        let mut out = format!(
            "Target: {}\nLanguage Server: {} (installed: {})\nSystem Servers Detected: {}",
            abs.display(),
            relevant,
            if available {
                "yes"
            } else {
                "no (install for live diagnostics)"
            },
            if servers.is_empty() {
                "none".to_string()
            } else {
                servers.join(", ")
            }
        );
        let diags = crate::lsp::check_file_diagnostics(&abs);
        if let Some(feedback) = crate::lsp::format_diagnostics_feedback(&diags) {
            out.push_str(&format!("\n\n{feedback}"));
        } else {
            out.push_str("\n\nNo compiler/syntax diagnostics reported.");
        }
        Ok(out)
    } else {
        Ok(format!(
            "Active Language Servers Detected:\n- rust-analyzer: {}\n- typescript-language-server: {}\n- pyright: {}\n- gopls: {}\nWorkspace: {}",
            if registry.rust_analyzer { "AVAILABLE" } else { "NOT FOUND" },
            if registry.ts_server { "AVAILABLE" } else { "NOT FOUND" },
            if registry.pyright { "AVAILABLE" } else { "NOT FOUND" },
            if registry.gopls { "AVAILABLE" } else { "NOT FOUND" },
            jail.cwd.display()
        ))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated {} bytes", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Jail, SandboxMode};
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["git".into()],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: None,
        };
        let args = json!({"path": "hello.txt", "content": "hi\n"});
        write_file(&args, &jail, &ctx).unwrap();
        let got = read_file(&json!({"path": "hello.txt"}), &jail, &ctx).unwrap();
        assert!(got.contains("hi"));
    }

    #[test]
    fn write_file_snapshots_previous_content_for_undo() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let store = crate::checkpoint::CheckpointStore::at(dir.path().join("cp"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: Some("s-undo"),
            checkpoints: Some(&store),
            docker: None,
        };
        write_file(&json!({"path": "f.txt", "content": "v1"}), &jail, &ctx).unwrap();
        write_file(&json!({"path": "f.txt", "content": "v2"}), &jail, &ctx).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "v2"
        );

        match store.undo_last("s-undo").unwrap() {
            crate::checkpoint::Undo::Restored(path) => {
                assert_eq!(std::fs::read_to_string(path).unwrap(), "v1");
            }
            other => panic!("expected a restore, got {other:?}"),
        }
    }

    #[test]
    fn edit_requires_unique() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: None,
        };
        write_file(&json!({"path": "a.txt", "content": "aa aa"}), &jail, &ctx).unwrap();
        let err = edit_file(
            &json!({"path": "a.txt", "old_string": "aa", "new_string": "bb"}),
            &jail,
            &ctx,
        )
        .unwrap_err();
        assert!(err.to_string().contains("matched"));
    }

    #[test]
    fn web_fetch_blocks_file_scheme() {
        let dir = tempdir().unwrap();
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: None,
        };
        let err = web_fetch(&json!({"url": "file:///etc/passwd"}), &ctx).unwrap_err();
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn web_fetch_blocks_loopback_hosts() {
        let dir = tempdir().unwrap();
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: None,
        };
        let err = web_fetch(&json!({"url": "http://127.0.0.1:8787/health"}), &ctx).unwrap_err();
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn truncate_never_splits_utf8() {
        let s = "abcd✓ héllo";
        assert_eq!(truncate(s, 4), "abcd\n… truncated 10 bytes");
        assert_eq!(truncate(s, 5), truncate(s, 4));
        assert_eq!(truncate(s, 6), truncate(s, 4));
        assert_eq!(truncate(s, 7), "abcd✓\n… truncated 7 bytes");
        for n in 0..s.len() {
            truncate(s, n);
        }
    }

    #[test]
    fn ip_allowlist_blocks_internal_targets() {
        for blocked in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.5",
            "::1",
            "fe80::1",
            "192.168.1.10",
            "172.16.0.9",
            "100.64.0.1",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "0.0.0.0",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = blocked.parse().unwrap();
            assert!(!ip_is_allowed(ip), "should block {blocked}");
        }
        for allowed in ["93.184.216.34", "2606:2800::1", "8.8.8.8"] {
            let ip: IpAddr = allowed.parse().unwrap();
            assert!(ip_is_allowed(ip), "should allow {allowed}");
        }
    }

    #[test]
    fn host_check_accepts_public_literals_and_rejects_internal() {
        assert!(host_is_allowed("93.184.216.34"));
        assert!(host_is_allowed("2606:2800::1"));
        assert!(!host_is_allowed("127.0.0.1"));
        assert!(!host_is_allowed("[::1]"));
        assert!(!host_is_allowed(""));
    }

    #[test]
    fn schemas_include_session_tools() {
        let schemas = schemas();
        let find = |name: &str| {
            schemas
                .iter()
                .find(|s| s["function"]["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("missing schema for {name}"))
        };
        let send = find("session_send");
        assert_eq!(
            send["function"]["parameters"]["required"],
            json!(["to", "text"])
        );
        assert!(send["function"]["parameters"]["properties"]["to"].is_object());
        assert!(send["function"]["parameters"]["properties"]["text"].is_object());
        let inbox = find("session_inbox");
        assert!(inbox["function"]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_tools_roundtrip_through_dispatch() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: Some("s-test"),
            checkpoints: None,
            docker: None,
        };

        let out = dispatch(
            "session_send",
            &json!({"to": "s-other", "text": "hi"}),
            &jail,
            &ctx,
        )
        .unwrap();
        assert!(out.starts_with("sent to s-other (message "));

        let other = ToolCtx {
            bus: &bus,
            session_id: Some("s-other"),
            checkpoints: None,
            docker: None,
        };
        dispatch(
            "session_send",
            &json!({"to": "s-test", "text": "reply"}),
            &jail,
            &other,
        )
        .unwrap();

        // Both messages are deliverable on the same bus file.
        let for_other = bus.drain("s-other");
        assert_eq!(for_other.len(), 1);
        assert_eq!(for_other[0].from, "s-test");
        assert_eq!(for_other[0].text, "hi");

        let inbox = dispatch("session_inbox", &json!({}), &jail, &ctx).unwrap();
        assert!(inbox.contains("[from s-other"));
        assert!(inbox.contains("reply"));
        assert_eq!(
            dispatch("session_inbox", &json!({}), &jail, &ctx).unwrap(),
            "(no messages)"
        );
    }

    #[test]
    fn session_send_requires_to_and_text() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: Some("s-test"),
            checkpoints: None,
            docker: None,
        };
        let err = dispatch("session_send", &json!({"text": "hi"}), &jail, &ctx).unwrap_err();
        assert!(err.to_string().contains("`to`"));
        let err = dispatch("session_send", &json!({"to": "s-other"}), &jail, &ctx).unwrap_err();
        assert!(err.to_string().contains("`text`"));
    }

    #[test]
    fn session_send_latest_alias_targets_newest_other_session() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        bus.send("s-a", "s-b", "seed one").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Seeded from the dispatching session itself: a sender counts as a
        // peer, so this makes s-c the strictly newest other session instead
        // of tying with its own sender.
        bus.send("s-q", "s-c", "seed two").unwrap();

        let ctx = ToolCtx {
            bus: &bus,
            session_id: Some("s-q"),
            checkpoints: None,
            docker: None,
        };
        let out = dispatch(
            "session_send",
            &json!({"to": "latest", "text": "ping"}),
            &jail,
            &ctx,
        )
        .unwrap();
        assert!(out.starts_with("sent to latest (message "));

        // The alias resolved to s-c, the newest other session on the bus.
        let mail = bus.drain("s-c");
        assert!(mail.iter().any(|m| m.text == "ping"));
        assert!(bus.drain("s-b").iter().all(|m| m.text != "ping"));
    }

    #[test]
    fn session_inbox_without_session_context_reports_noop() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: None,
        };
        let out = dispatch("session_inbox", &json!({}), &jail, &ctx).unwrap();
        assert_eq!(out, "(no session context; nothing to read)");
    }

    #[test]
    fn session_inbox_renders_at_most_fifty_messages() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        for i in 0..55 {
            bus.send("s-a", "s-b", &format!("m{i}")).unwrap();
        }
        let ctx = ToolCtx {
            bus: &bus,
            session_id: Some("s-b"),
            checkpoints: None,
            docker: None,
        };
        let out = dispatch("session_inbox", &json!({}), &jail, &ctx).unwrap();
        assert!(out.contains("m0"));
        assert!(out.contains("m49"));
        assert!(!out.contains("m50"));
        assert!(out.contains("truncated"));
        // Drain marked every message read even though only 50 were rendered.
        assert_eq!(
            dispatch("session_inbox", &json!({}), &jail, &ctx).unwrap(),
            "(no messages)"
        );
    }

    #[test]
    fn docker_run_argv_builds_expected_invocation() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::DockerIsolated,
            vec![],
        );
        let argv = docker_run_argv(&jail, &DockerOpts::default(), "echo hi");
        let mount = format!("{}:/work", dir.path().to_string_lossy().replace('\\', "/"));
        let expected: Vec<&str> = vec![
            "run",
            "--rm",
            "-i",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            "256",
            "--memory",
            "1g",
            "--cpus",
            "2",
            "--read-only",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=128m",
            "-e",
            "HOME=/tmp",
            "--network",
            "none",
            "-v",
            &mount,
            "-w",
            "/work",
            "varynth-sandbox:latest",
            "sh",
            "-c",
            "echo hi",
        ];
        assert_eq!(
            argv,
            expected.into_iter().map(String::from).collect::<Vec<_>>()
        );
    }

    #[test]
    fn docker_run_argv_normalizes_network() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::DockerIsolated,
            vec![],
        );
        let net = |network: &str| {
            let opts = DockerOpts {
                image: "img:1".into(),
                network: network.into(),
            };
            let argv = docker_run_argv(&jail, &opts, "true");
            let i = argv.iter().position(|a| a == "--network").unwrap();
            argv[i + 1].clone()
        };
        assert_eq!(net("none"), "none");
        assert_eq!(net("bridge"), "bridge");
        assert_eq!(net("host"), "host");
        assert_eq!(net("BRIDGE"), "bridge");
        // Anything unrecognized (including empty) falls back to none.
        assert_eq!(net("bogus"), "none");
        assert_eq!(net(""), "none");
    }

    #[test]
    fn docker_run_argv_converts_windows_mount_paths() {
        // Backslashed cwds must be forward-slashed so Docker Desktop accepts
        // the bind mount. On Unix backslashes are ordinary characters in
        // paths, so this checks the conversion deterministically everywhere.
        let jail = Jail::new(
            PathBuf::from(r"C:\Users\me\proj"),
            vec![],
            SandboxMode::DockerIsolated,
            vec![],
        );
        let opts = DockerOpts {
            image: "img:1".into(),
            network: "none".into(),
        };
        let argv = docker_run_argv(&jail, &opts, "ls -la");
        assert!(argv.contains(&"C:/Users/me/proj:/work".to_string()));
        // The command is passed verbatim as the sh -c payload.
        assert_eq!(argv.last().unwrap(), "ls -la");
    }

    #[test]
    fn docker_opts_fall_back_to_defaults() {
        let dir = tempdir().unwrap();
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let without = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: None,
        };
        let opts = docker_opts(&without);
        assert_eq!(opts.image, "varynth-sandbox:latest");
        assert_eq!(opts.network, "none");

        let with = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: Some(DockerOpts {
                image: "custom:dev".into(),
                network: "bridge".into(),
            }),
        };
        let opts = docker_opts(&with);
        assert_eq!(opts.image, "custom:dev");
        assert_eq!(opts.network, "bridge");
    }

    /// Live check gated on a real daemon AND the sandbox image existing
    /// locally. A machine without either returns early, so the suite never
    /// fails on environment grounds.
    #[test]
    fn docker_isolated_bash_runs_command_in_container() {
        if !docker_daemon_available() {
            return;
        }
        let opts = DockerOpts::default();
        if !docker_image_present(&opts.image) {
            return;
        }
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::DockerIsolated,
            vec!["echo".into()],
        );
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));
        let ctx = ToolCtx {
            bus: &bus,
            session_id: None,
            checkpoints: None,
            docker: None,
        };
        let out = dispatch(
            "bash",
            &json!({"command": "echo varynth-in-container"}),
            &jail,
            &ctx,
        )
        .unwrap();
        assert!(
            out.contains("varynth-in-container"),
            "unexpected output: {out}"
        );
    }

    /// Probe `docker info` once with a 3s budget; false when the binary is
    /// missing, the daemon is down, or the probe hangs.
    fn docker_daemon_available() -> bool {
        let Ok(mut child) = Command::new("docker")
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => {
                    if start.elapsed() >= std::time::Duration::from_secs(3) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(_) => return false,
            }
        }
    }

    fn docker_image_present(image: &str) -> bool {
        Command::new("docker")
            .args(["image", "inspect", image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
