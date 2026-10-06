use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex,
};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Debug, Clone, Deserialize)]
struct McpServerEntry {
    #[serde(rename = "type", default)]
    type_: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpConfigFile {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub transport: McpTransport,
}

#[derive(Debug, Clone)]
pub enum McpTransport {
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

const SESSION_HEADER: &str = "mcp-session-id";

/// Per-request deadline for stdio servers; a hung child is killed when it fires.
const STDIO_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP stdio handshake protocol version.
const STDIO_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug)]
pub struct McpClient {
    pub server: McpServer,
    /// Streamable HTTP session id, set by the server on `initialize`.
    session_id: Mutex<Option<String>>,
    initialized: AtomicBool,
    next_id: AtomicU64,
    request_timeout: Duration,
    /// Lazily spawned stdio child; unused for HTTP servers.
    stdio: Mutex<StdioState>,
}

/// Raw HTTP response, read fully on the worker thread.
struct HttpReply {
    status: reqwest::StatusCode,
    content_type: String,
    session_id: Option<String>,
    text: String,
}

impl McpClient {
    pub fn new(server: McpServer) -> Self {
        Self::with_timeout(server, STDIO_REQUEST_TIMEOUT)
    }

    fn with_timeout(server: McpServer, timeout: Duration) -> Self {
        Self {
            server,
            session_id: Mutex::new(None),
            initialized: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            request_timeout: timeout,
            stdio: Mutex::new(StdioState::fresh(timeout)),
        }
    }

    fn is_stdio(&self) -> bool {
        matches!(self.server.transport, McpTransport::Stdio { .. })
    }

    /// Sends one POST on a dedicated OS thread. reqwest's blocking client owns an
    /// internal tokio runtime that panics when created or dropped inside an async
    /// context, and every caller here runs under `#[tokio::main]`.
    fn send(&self, body: &Value) -> Result<HttpReply> {
        let (url, raw_headers) = match &self.server.transport {
            McpTransport::Http { url, headers } => (url.clone(), headers.clone()),
            McpTransport::Stdio { .. } => {
                anyhow::bail!("mcp http request on stdio server '{}'", self.server.name)
            }
        };
        let url = expand_placeholders(&url)?;
        let mut headers = HashMap::new();
        for (key, value) in raw_headers {
            headers.insert(key, expand_placeholders(&value)?);
        }
        if let Some(sid) = self.session_id.lock().unwrap().clone() {
            headers.insert(SESSION_HEADER.into(), sid);
        }
        let body = body.clone();
        let timeout = self.request_timeout;
        std::thread::scope(|s| {
            s.spawn(move || -> Result<HttpReply> {
                let client = reqwest::blocking::Client::builder()
                    .timeout(timeout)
                    .build()?;
                let mut req = client
                    .post(&url)
                    .json(&body)
                    .header("Accept", "application/json, text/event-stream");
                for (k, v) in &headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                let resp = req.send().context("mcp http request failed")?;
                let header = |name| {
                    resp.headers()
                        .get(name)
                        .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
                        .map(str::to_string)
                };
                let content_type =
                    header(reqwest::header::CONTENT_TYPE.as_str()).unwrap_or_default();
                let session_id = header(SESSION_HEADER);
                let status = resp.status();
                let text = resp.text().context("read mcp response")?;
                Ok(HttpReply {
                    status,
                    content_type,
                    session_id,
                    text,
                })
            })
            .join()
            .map_err(|_| anyhow::anyhow!("mcp http worker panicked"))?
        })
    }

    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.post_json(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    }

    fn post_json(&self, body: Value) -> Result<Value> {
        let reply = self.send(&body)?;
        if let Some(sid) = reply.session_id {
            *self.session_id.lock().unwrap() = Some(sid);
        }
        let status = reply.status;
        if !status.is_success() {
            anyhow::bail!(
                "mcp http {}: {}",
                status,
                reply.text.chars().take(500).collect::<String>()
            );
        }
        let ctype = reply.content_type;
        let text = reply.text;
        if ctype.contains("text/event-stream")
            || text.contains("event:")
            || text.trim_start().starts_with("data:")
        {
            // Parse SSE: find last data: line that is JSON
            let mut last_json: Option<Value> = None;
            for line in text.lines() {
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        last_json = Some(v);
                    }
                } else if line.starts_with('{') {
                    // Some servers return pure JSON even with event-stream header
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        last_json = Some(v);
                    }
                }
            }
            if let Some(v) = last_json {
                return Ok(v);
            }
            // Fallback: try whole text as JSON
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                return Ok(v);
            }
            anyhow::bail!(
                "failed to parse SSE mcp response: {}",
                text.chars().take(1000).collect::<String>()
            );
        } else {
            let v: Value = serde_json::from_str(&text).context("parse mcp json response")?;
            Ok(v)
        }
    }

    /// Runs the MCP handshake once per client; later calls are no-ops.
    pub fn initialize(&self) -> Result<()> {
        if self.is_stdio() {
            // Spawn + `initialize` + `notifications/initialized` if not yet done.
            let spec = self.stdio_spec()?.ok_or_else(|| {
                anyhow::anyhow!("mcp server '{}' is not a stdio server", self.server.name)
            })?;
            return std::thread::scope(|s| {
                s.spawn(move || -> Result<()> {
                    let mut st = self.stdio.lock().unwrap_or_else(|e| e.into_inner());
                    stdio_ensure_running(&mut st, &spec).map_err(StdioFail::into_anyhow)
                })
                .join()
                .map_err(|_| anyhow::anyhow!("mcp stdio worker panicked"))?
            });
        }
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let resp = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "varynth",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("mcp initialize error: {}", err);
        }
        // Send initialized notification (no id, no response expected)
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let _ = self.send(&notif);
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub fn list_tools(&self) -> Result<Vec<McpTool>> {
        let resp = if self.is_stdio() {
            self.stdio_request("tools/list", json!({}))?
        } else {
            self.initialize()?;
            self.request("tools/list", json!({}))?
        };
        if let Some(err) = resp.get("error") {
            anyhow::bail!("mcp tools/list error: {}", err);
        }
        parse_tools_list(&resp)
    }

    /// Bounded, explicit liveness probe. Unlike `status`, this may spawn a
    /// stdio child or initialize an HTTP session and then list tools.
    pub fn probe(&self) -> Result<usize> {
        let tools = self.list_tools()?;
        Ok(tools.len())
    }

    /// Bounded, explicit ping. It initializes a server but does not require
    /// tools/list support; MCP servers commonly expose `ping` for this.
    pub fn ping(&self) -> Result<()> {
        let resp = if self.is_stdio() {
            self.stdio_request("ping", json!({}))?
        } else {
            self.initialize()?;
            self.request("ping", json!({}))?
        };
        if let Some(err) = resp.get("error") {
            anyhow::bail!("mcp ping error: {}", err);
        }
        Ok(())
    }

    pub fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<String> {
        let resp = if self.is_stdio() {
            self.stdio_request(
                "tools/call",
                json!({"name": tool_name, "arguments": arguments}),
            )?
        } else {
            self.initialize()?;
            self.request(
                "tools/call",
                json!({"name": tool_name, "arguments": arguments}),
            )?
        };
        if let Some(err) = resp.get("error") {
            anyhow::bail!("mcp tools/call error: {}", err);
        }
        Ok(extract_tool_text(&resp))
    }

    fn stdio_spec(&self) -> Result<Option<StdioSpec>> {
        match &self.server.transport {
            McpTransport::Stdio { command, args, env } => Ok(Some(StdioSpec {
                command: expand_placeholders(command)?,
                args: args
                    .iter()
                    .map(|arg| expand_placeholders(arg))
                    .collect::<Result<Vec<_>>>()?,
                env: env
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), expand_placeholders(value)?)))
                    .collect::<Result<HashMap<_, _>>>()?,
            })),
            _ => Ok(None),
        }
    }

    /// Runs one stdio JSON-RPC request (spawning + initializing the child on
    /// first use) on a dedicated OS thread, matching the HTTP path's blocking
    /// style so the async runtime is never blocked on a mutex/pipe.
    fn stdio_request(&self, method: &str, params: Value) -> Result<Value> {
        let spec = self.stdio_spec()?.ok_or_else(|| {
            anyhow::anyhow!("mcp server '{}' is not a stdio server", self.server.name)
        })?;
        let method = method.to_string();
        std::thread::scope(|s| {
            s.spawn(move || -> Result<Value> {
                let mut st = self.stdio.lock().unwrap_or_else(|e| e.into_inner());
                stdio_request_once(&mut st, &spec, &method, &params).map_err(StdioFail::into_anyhow)
            })
            .join()
            .map_err(|_| anyhow::anyhow!("mcp stdio worker panicked"))?
        })
    }

    #[cfg(test)]
    fn set_stdio_request_timeout(&self, timeout: Duration) {
        self.stdio.lock().unwrap().request_timeout = timeout;
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if self.is_stdio() {
            let mut st = self.stdio.lock().unwrap_or_else(|e| e.into_inner());
            st.kill_and_reset();
        }
    }
}

// ---------------------------------------------------------------------------
// stdio transport (newline-delimited JSON-RPC 2.0 over child stdin/stdout)
// ---------------------------------------------------------------------------

/// Launch parameters cloned out of the config before spawning.
#[derive(Debug, Clone)]
struct StdioSpec {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

/// Internal transport failure classification; distinguishes "the child may be
/// dead and a respawn makes sense" from "the server hung past the deadline".
#[derive(Debug)]
enum StdioFail {
    /// No response within the deadline; the child was killed and the transport reset.
    Hung(Duration),
    /// The child closed stdout / the reader channel disconnected; transport reset.
    Exited,
    /// Writing to the child failed; transport reset (request never delivered).
    Write,
    /// Spawning the child failed.
    Spawn(String),
    /// The server answered with something unusable.
    Proto(String),
}

impl StdioFail {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            StdioFail::Hung(timeout) => anyhow::anyhow!(
                "mcp stdio request timed out after {}s; server killed",
                timeout.as_secs_f32()
            ),
            StdioFail::Exited => anyhow::anyhow!("mcp stdio server exited unexpectedly"),
            StdioFail::Write => anyhow::anyhow!("failed to write to mcp stdio server"),
            StdioFail::Spawn(msg) => anyhow::anyhow!("{msg}"),
            StdioFail::Proto(msg) => anyhow::anyhow!("{msg}"),
        }
    }
}

/// Live stdio transport state. Guarded by a `Mutex` in `McpClient`; the whole
/// request/response cycle runs under the lock, so responses are never consumed
/// by a concurrent caller.
#[derive(Debug)]
struct StdioState {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// JSON messages parsed by the reader thread, correlated by id by the caller.
    responses: Option<Receiver<Value>>,
    request_timeout: Duration,
    next_id: u64,
    initialized: bool,
}

impl StdioState {
    fn fresh(request_timeout: Duration) -> Self {
        Self {
            child: None,
            stdin: None,
            responses: None,
            request_timeout,
            next_id: 1,
            initialized: false,
        }
    }

    fn is_alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                tracing::debug!(%status, "mcp stdio server has exited");
                false
            }
            Err(e) => {
                tracing::debug!(error = %e, "mcp stdio try_wait failed");
                false
            }
        }
    }

    /// Kills the child (if any) and clears the transport so the next request
    /// respawns and re-initializes from scratch.
    fn kill_and_reset(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        // Dropping the receiver ends the reader thread on its next send.
        self.responses = None;
        self.initialized = false;
    }
}

// --- JSON-RPC line framing helpers (pure, unit-tested) ---

fn next_request_id(next: &mut u64) -> u64 {
    let id = *next;
    *next += 1;
    id
}

fn serialize_request(id: u64, method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn serialize_notification(method: &str) -> String {
    json!({"jsonrpc": "2.0", "method": method}).to_string()
}

/// True for messages without an `id` (server-initiated notifications).
fn is_notification(msg: &Value) -> bool {
    msg.get("id").is_none()
}

/// True when `msg` is a JSON-RPC response (result or error) for request `id`.
fn is_response_for(msg: &Value, id: u64) -> bool {
    let matches_id = match msg.get("id") {
        Some(Value::Number(n)) => n.as_u64() == Some(id),
        // Some servers stringify ids; still correlate.
        Some(Value::String(s)) => s.parse::<u64>() == Ok(id),
        _ => false,
    };
    matches_id && (msg.get("result").is_some() || msg.get("error").is_some())
}

// --- process spawning ---

fn spawn_stdio_state(spec: &StdioSpec, request_timeout: Duration) -> Result<StdioState, StdioFail> {
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console flash when spawned from the TUI or tests.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| StdioFail::Spawn(format!("spawn mcp stdio server '{}': {e}", spec.command)))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| StdioFail::Spawn("mcp stdio: stdin not piped".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| StdioFail::Spawn("mcp stdio: stdout not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| StdioFail::Spawn("mcp stdio: stderr not piped".into()))?;

    // Drain stderr on a background thread so the child can never block on a
    // full pipe; lines only go to the trace log.
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            tracing::debug!(target: "mcp_stdio_stderr", line = %line);
        }
    });

    // Reader thread: parse stdout lines as JSON and forward them through a
    // channel; `recv_timeout` on the receiving end bounds how long a hung
    // server can block a request.
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(msg) => {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(target: "mcp_stdio", line = %trimmed, error = %e, "ignoring non-json mcp stdout line");
                }
            }
        }
    });

    Ok(StdioState {
        child: Some(child),
        stdin: Some(stdin),
        responses: Some(rx),
        request_timeout,
        next_id: 1,
        initialized: false,
    })
}

// --- request machinery (all take the already-locked state) ---

/// Sends one request and waits (bounded by the deadline) for the response with
/// the matching id. On timeout or disconnect the transport is killed and reset.
fn stdio_transmit(st: &mut StdioState, method: &str, params: &Value) -> Result<Value, StdioFail> {
    let id = next_request_id(&mut st.next_id);
    let line = serialize_request(id, method, params);
    let write_res = match st.stdin.as_mut() {
        Some(stdin) => stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush()),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "mcp stdio stdin closed",
        )),
    };
    if let Err(e) = write_res {
        tracing::debug!(method, error = %e, "mcp stdio write failed");
        st.kill_and_reset();
        return Err(StdioFail::Write);
    }
    let deadline = Instant::now() + st.request_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                method,
                timeout_secs = st.request_timeout.as_secs(),
                "mcp stdio request timed out; killing server"
            );
            st.kill_and_reset();
            return Err(StdioFail::Hung(st.request_timeout));
        }
        let rx = match st.responses.as_ref() {
            Some(rx) => rx,
            None => {
                st.kill_and_reset();
                return Err(StdioFail::Exited);
            }
        };
        let msg = match rx.recv_timeout(remaining) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    method,
                    timeout_secs = st.request_timeout.as_secs(),
                    "mcp stdio request timed out; killing server"
                );
                st.kill_and_reset();
                return Err(StdioFail::Hung(st.request_timeout));
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!(method, "mcp stdio server closed stdout");
                st.kill_and_reset();
                return Err(StdioFail::Exited);
            }
        };
        if is_notification(&msg) {
            continue;
        }
        if is_response_for(&msg, id) {
            return Ok(msg);
        }
        // Notification or a response for a stale id; skip it.
        tracing::debug!(method, msg = %msg, "skipping unmatched mcp stdio message");
    }
}

/// `initialize` + `notifications/initialized`; marks the state initialized.
fn stdio_handshake(st: &mut StdioState) -> Result<(), StdioFail> {
    let params = json!({
        "protocolVersion": STDIO_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "varynth",
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    let resp = stdio_transmit(st, "initialize", &params)?;
    if let Some(err) = resp.get("error") {
        st.kill_and_reset();
        return Err(StdioFail::Proto(format!(
            "mcp stdio initialize error: {err}"
        )));
    }
    if resp.get("result").is_none() {
        st.kill_and_reset();
        return Err(StdioFail::Proto(
            "mcp stdio initialize response missing result".into(),
        ));
    }
    let notif = serialize_notification("notifications/initialized");
    let write_res = match st.stdin.as_mut() {
        Some(stdin) => stdin
            .write_all(notif.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush()),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "mcp stdio stdin closed",
        )),
    };
    if let Err(e) = write_res {
        tracing::debug!(error = %e, "mcp stdio initialized notification write failed");
        st.kill_and_reset();
        return Err(StdioFail::Write);
    }
    st.initialized = true;
    Ok(())
}

/// Makes sure a live, initialized child exists, respawning + re-initializing
/// when the child was found dead.
fn stdio_ensure_running(st: &mut StdioState, spec: &StdioSpec) -> Result<(), StdioFail> {
    if !st.is_alive() {
        let timeout = st.request_timeout;
        *st = spawn_stdio_state(spec, timeout)?;
        tracing::info!(command = %spec.command, "spawned mcp stdio server");
    }
    if !st.initialized {
        stdio_handshake(st)?;
    }
    Ok(())
}

fn stdio_request_once(
    st: &mut StdioState,
    spec: &StdioSpec,
    method: &str,
    params: &Value,
) -> Result<Value, StdioFail> {
    stdio_ensure_running(st, spec)?;
    match stdio_transmit(st, method, params) {
        // The child died between the aliveness check and the write, so the
        // request was never delivered: respawn once, re-initialize, re-send.
        Err(StdioFail::Write) => {
            stdio_ensure_running(st, spec)?;
            stdio_transmit(st, method, params)
        }
        other => other,
    }
}

// --- shared response parsing (HTTP and stdio render results identically) ---

/// Parses a `tools/list` response body into tools.
fn parse_tools_list(resp: &Value) -> Result<Vec<McpTool>> {
    let tools = resp
        .pointer("/result/tools")
        .or_else(|| resp.pointer("/result"))
        .cloned()
        .unwrap_or(Value::Null);
    if tools.is_null() {
        return Ok(Vec::new());
    }
    // tools may be array or object with tools
    let arr = if tools.is_array() {
        tools.as_array().unwrap().clone()
    } else if let Some(arr) = tools.get("tools").and_then(|v| v.as_array()) {
        arr.clone()
    } else {
        anyhow::bail!("unexpected mcp tools/list result: {}", tools);
    };
    let mut out = Vec::new();
    for item in arr {
        let tool: McpTool = serde_json::from_value(item.clone()).context("parse mcp tool")?;
        out.push(tool);
    }
    Ok(out)
}

/// Renders a `tools/call` response: joins text content blocks, falling back to
/// the raw result payload.
fn extract_tool_text(resp: &Value) -> String {
    // Result may be {content: [{type:"text", text:"..."}]} or direct
    if let Some(content) = resp.pointer("/result/content") {
        if let Some(arr) = content.as_array() {
            let mut texts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    texts.push(t.to_string());
                } else {
                    texts.push(item.to_string());
                }
            }
            if texts.is_empty() {
                return resp.pointer("/result").unwrap_or(&Value::Null).to_string();
            }
            return texts.join("\n");
        }
    }
    if let Some(result) = resp.get("result") {
        if result.is_string() {
            return result.as_str().unwrap().to_string();
        }
        return serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
    }
    resp.to_string()
}

// ---------------------------------------------------------------------------
// config loading
// ---------------------------------------------------------------------------

pub fn load_mcp_config(path: &Path) -> Result<HashMap<String, McpServer>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: McpConfigFile = serde_json::from_str(&raw).context("parse mcp config json")?;
    let mut out = HashMap::new();
    for (name, entry) in cfg.mcp_servers {
        let server = entry.into_server(name.clone())?;
        out.insert(name.clone(), server);
    }
    Ok(out)
}

impl McpServerEntry {
    /// Validates one entry and picks its transport. Entries typed `stdio`, or
    /// with a `command` and no type, launch a child process; everything else is
    /// treated as an HTTP server.
    fn into_server(self, name: String) -> Result<McpServer> {
        let type_lc = self.type_.as_deref().map(str::to_ascii_lowercase);
        let wants_stdio = match type_lc.as_deref() {
            Some("stdio") => true,
            None => self.command.is_some(),
            Some("http" | "streamable-http" | "streamable_http" | "http-streamable") => false,
            Some(other) => anyhow::bail!("mcp server '{name}' has unsupported transport '{other}'"),
        };
        if wants_stdio {
            anyhow::ensure!(
                self.url.is_none(),
                "mcp stdio server '{name}' cannot also specify a URL"
            );
            let command = self
                .command
                .filter(|c| !c.trim().is_empty() && !c.chars().any(char::is_control))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "mcp server '{}' is a stdio server and requires a non-empty 'command'",
                        name
                    )
                })?;
            Ok(McpServer {
                name,
                transport: McpTransport::Stdio {
                    command,
                    args: self.args.unwrap_or_default(),
                    env: self.env.unwrap_or_default(),
                },
            })
        } else {
            anyhow::ensure!(
                self.command.is_none(),
                "mcp HTTP server '{name}' cannot also specify a command"
            );
            let url = self
                .url
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("mcp server '{}' url is empty", name))?;
            // Validate URL
            let parsed = reqwest::Url::parse(&url)
                .with_context(|| format!("mcp server '{}' invalid url", name))?;
            match parsed.scheme() {
                "http" | "https" => {}
                other => anyhow::bail!(
                    "mcp server '{}' scheme '{}' blocked; only http/https",
                    name,
                    other
                ),
            }
            let headers = self.headers.unwrap_or_default();
            Ok(McpServer {
                name,
                transport: McpTransport::Http { url, headers },
            })
        }
    }
}

/// A cached result for one configured MCP server. `status()` never performs
/// network or process work; callers opt into `probe()` or `ping()` explicitly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpStatus {
    Unknown,
    Ok,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: String,
    /// HTTP targets omit userinfo, query and fragment; stdio targets include
    /// only the executable, never arguments or environment values.
    pub target: String,
    pub tool_count: Option<usize>,
    pub status: McpStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn expand_placeholders(value: &str) -> Result<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail
            .find('}')
            .ok_or_else(|| anyhow::anyhow!("unterminated environment placeholder in MCP value"))?;
        let name = &tail[..end];
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
                && !name.chars().next().is_some_and(|ch| ch.is_ascii_digit()),
            "invalid MCP environment placeholder `${{{name}}}`"
        );
        let replacement = std::env::var(name)
            .map_err(|_| anyhow::anyhow!("MCP environment variable `{name}` is not set"))?;
        out.push_str(&replacement);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}
fn transport_name(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Http { .. } => "http",
        McpTransport::Stdio { .. } => "stdio",
    }
}

fn safe_command_target(command: &str) -> String {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_string()
}

fn safe_target(transport: &McpTransport) -> String {
    match transport {
        McpTransport::Stdio { command, .. } => safe_command_target(command),
        McpTransport::Http { url, .. } => {
            let Ok(parsed) = reqwest::Url::parse(url) else {
                return "<invalid-url>".into();
            };
            let host = parsed.host_str().unwrap_or("<unknown-host>");
            let port = parsed
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
            let path = parsed.path();
            format!("{}://{host}{port}{path}", parsed.scheme())
        }
    }
}

fn status_for(name: &str, server: &McpServer) -> McpServerStatus {
    McpServerStatus {
        name: name.to_string(),
        transport: transport_name(&server.transport).to_string(),
        target: safe_target(&server.transport),
        tool_count: None,
        status: McpStatus::Unknown,
        error: None,
    }
}

fn validate_server_entry(name: &str, value: &Value) -> Result<McpServer> {
    anyhow::ensure!(
        value.is_object(),
        "MCP server `{name}` entry must be a JSON object"
    );
    let entry: McpServerEntry = serde_json::from_value(value.clone())
        .with_context(|| format!("parse MCP server entry `{name}`"))?;
    let server = entry.into_server(name.to_string())?;
    if let McpTransport::Http { url, .. } = &server.transport {
        let parsed = reqwest::Url::parse(url)?;
        anyhow::ensure!(
            parsed.username().is_empty(),
            "MCP server `{name}` URL must not contain userinfo"
        );
        anyhow::ensure!(
            parsed.password().is_none(),
            "MCP server `{name}` URL must not contain userinfo"
        );
    }
    Ok(server)
}

struct ConfigFileLock {
    path: PathBuf,
}

impl ConfigFileLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_file_name(format!(
            ".{}.lock",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("mcp.json")
        ));
        let started = Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return Ok(Self { path: lock_path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&lock_path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(60));
                    if stale {
                        let _ = fs::remove_file(&lock_path);
                        continue;
                    }
                    anyhow::ensure!(
                        started.elapsed() < Duration::from_secs(10),
                        "timed out waiting for MCP config lock {}",
                        lock_path.display()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("create {}", lock_path.display()))
                }
            }
        }
    }
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn resolved_config_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn load_config_value(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({"mcpServers": {}}));
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    anyhow::ensure!(value.is_object(), "MCP config root must be a JSON object");
    Ok(value)
}

fn save_config_value(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mcp.json"),
        std::process::id()
    ));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Add or replace one MCP server entry while preserving every unknown field in
/// the surrounding config. Existing equal entries are a no-op; mismatches
/// require `force` so accidental credential/config replacement is rejected.
pub fn add_server_entry(path: &Path, name: &str, entry: &Value, force: bool) -> Result<()> {
    anyhow::ensure!(!name.trim().is_empty(), "MCP server name cannot be empty");
    let _ = validate_server_entry(name, entry)?;
    let path = resolved_config_path(path);
    let _lock = ConfigFileLock::acquire(&path)?;
    let mut root = load_config_value(&path)?;
    let servers = root
        .as_object_mut()
        .and_then(|root| {
            root.entry("mcpServers")
                .or_insert_with(|| json!({}))
                .as_object_mut()
        })
        .ok_or_else(|| anyhow::anyhow!("MCP config `mcpServers` must be an object"))?;
    if let Some(existing) = servers.get(name) {
        if existing == entry {
            return Ok(());
        }
        anyhow::ensure!(
            force,
            "MCP server `{name}` already exists with a different entry; pass force to replace"
        );
    }
    servers.insert(name.to_string(), entry.clone());
    save_config_value(&path, &root)
}

/// Remove an entry only when it still exactly matches the manifest-owned JSON.
/// A changed or foreign entry is never deleted.
pub fn remove_server_entry(path: &Path, name: &str, expected: &Value) -> Result<()> {
    let path = resolved_config_path(path);
    let _lock = ConfigFileLock::acquire(&path)?;
    let mut root = load_config_value(&path)?;
    let servers = root
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("MCP config `mcpServers` must be an object"))?;
    let Some(existing) = servers.get(name) else {
        return Ok(());
    };
    anyhow::ensure!(
        existing == expected,
        "MCP server `{name}` no longer matches the installed manifest; refusing to remove it"
    );
    servers.remove(name);
    save_config_value(&path, &root)
}
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    let s = sanitize(server);
    let t = sanitize(tool);
    format!("mcp__{}__{}", s, t)
}

fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "x".into()
    } else {
        out
    }
}

pub fn parse_mcp_tool_name(full: &str) -> Option<(String, String)> {
    // format mcp__server__tool
    if !full.starts_with("mcp__") {
        return None;
    }
    let rest = &full["mcp__".len()..];
    let idx = rest.find("__")?;
    let server = &rest[..idx];
    let tool = &rest[idx + 2..];
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_string(), tool.to_string()))
}

pub fn mcp_tools_to_openai(server_name: &str, tools: &[McpTool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let name = mcp_tool_name(server_name, &tool.name);
            let desc = tool.description.clone().unwrap_or_else(|| format!("MCP tool {}", tool.name));
            let mut params = tool.input_schema.clone();
            // Ensure params is object with type object
            if params.is_null() || params == Value::Object(Default::default()) {
                params = json!({"type":"object","properties":{}});
            } else if params.get("type").is_none() {
                // If schema is not standard, wrap
                params = json!({"type":"object","properties": params.as_object().cloned().unwrap_or_default() });
            }
            // Ensure required is array if missing
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": params
                }
            })
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct McpManager {
    pub servers: HashMap<String, McpClient>,
    pub tool_index: HashMap<String, (String, String)>, // full_name -> (server, tool)
    pub tool_schemas: Vec<Value>,
    statuses: HashMap<String, McpServerStatus>,
}

impl McpManager {
    pub fn from_config(path: Option<&Path>) -> Result<Self> {
        let mut mgr = Self::default();
        let Some(path) = path else {
            return Ok(mgr);
        };
        let path = resolved_config_path(path);
        if !path.exists() {
            anyhow::bail!("mcp config file not found: {}", path.display());
        }
        let servers = load_mcp_config(&path)?;
        for (name, server) in servers {
            let client = McpClient::new(server.clone());
            mgr.statuses
                .insert(name.clone(), status_for(&name, &server));
            mgr.servers.insert(name.clone(), client);
        }
        Ok(mgr)
    }

    /// Returns the last known status for every configured server without
    /// spawning processes or making network requests.
    pub fn status(&self) -> Vec<McpServerStatus> {
        let mut statuses: Vec<_> = self.statuses.values().cloned().collect();
        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        statuses
    }

    /// Explicitly initialize and list one server, updating its cached status.
    pub fn probe_server(&mut self, name: &str) -> Result<McpServerStatus> {
        let client = self
            .servers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("mcp server not found: {name}"))?;
        let result = client.probe();
        let status = self
            .statuses
            .get_mut(name)
            .expect("status for every server");
        match result {
            Ok(tool_count) => {
                status.tool_count = Some(tool_count);
                status.status = McpStatus::Ok;
                status.error = None;
                Ok(status.clone())
            }
            Err(error) => {
                status.status = McpStatus::Error;
                status.error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Explicitly initialize and list every configured server, updating the
    /// cached statuses. This is the only manager operation that probes all
    /// servers; `status()` remains side-effect free.
    pub fn probe(&mut self) -> Result<Vec<McpServerStatus>> {
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            let _ = self.probe_server(&name);
        }
        Ok(self.status())
    }
    /// Explicitly ping one server with the client's bounded request timeout.
    pub fn ping(&mut self, name: &str) -> Result<McpServerStatus> {
        let client = self
            .servers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("mcp server not found: {name}"))?;
        let result = client.ping();
        let status = self
            .statuses
            .get_mut(name)
            .expect("status for every server");
        match result {
            Ok(()) => {
                status.status = McpStatus::Ok;
                status.error = None;
                Ok(status.clone())
            }
            Err(error) => {
                status.status = McpStatus::Error;
                status.error = Some(error.to_string());
                Err(error)
            }
        }
    }

    pub fn load_tools(&mut self) -> Vec<Value> {
        let mut all = Vec::new();
        let mut new_index = HashMap::new();
        for (server_name, client) in &self.servers {
            match client.list_tools() {
                Ok(tools) => {
                    tracing::info!(server=%server_name, count=tools.len(), "mcp tools listed");
                    for tool in tools {
                        let Some(openai) =
                            mcp_tools_to_openai(server_name, std::slice::from_ref(&tool))
                                .into_iter()
                                .next()
                        else {
                            continue;
                        };
                        if let Some(name) =
                            openai.pointer("/function/name").and_then(|v| v.as_str())
                        {
                            new_index
                                .insert(name.to_string(), (server_name.clone(), tool.name.clone()));
                        }
                        all.push(openai);
                    }
                }
                Err(e) => {
                    tracing::warn!(server=%server_name, error=%e, "mcp list_tools failed");
                }
            }
        }
        self.tool_index = new_index;
        self.tool_schemas = all.clone();
        all
    }

    pub fn schemas(&self) -> &[Value] {
        &self.tool_schemas
    }

    pub fn is_mcp_tool(&self, name: &str) -> bool {
        self.tool_index.contains_key(name) || parse_mcp_tool_name(name).is_some()
    }

    pub fn dispatch(&self, full_name: &str, args: &Value) -> Result<String> {
        let (server_name, tool_name) = self
            .tool_index
            .get(full_name)
            .cloned()
            .or_else(|| parse_mcp_tool_name(full_name))
            .ok_or_else(|| anyhow::anyhow!("unknown mcp tool: {}", full_name))?;
        let client = self
            .servers
            .get(&server_name)
            .ok_or_else(|| anyhow::anyhow!("mcp server not found: {}", server_name))?;
        // arguments may be object or empty
        let arguments = if args.is_object() {
            // If args is the outer tool arguments object, it already is the arguments
            // For MCP we expect arguments as object; if args contains wrapped, unwrap
            args.clone()
        } else {
            json!({})
        };
        client.call_tool(&tool_name, arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::OnceLock;
    use tempfile::NamedTempFile;

    #[test]
    fn loads_empty_config() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"mcpServers":{{}}}}"#).unwrap();
        let m = load_mcp_config(f.path()).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn loads_single_http_server() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"mcpServers":{{"myserver":{{"type":"http","url":"https://example.com/mcp","headers":{{"Authorization":"Bearer x"}}}}}}}}"#
        )
        .unwrap();
        let m = load_mcp_config(f.path()).unwrap();
        assert_eq!(m.len(), 1);
        let s = m.get("myserver").unwrap();
        match &s.transport {
            McpTransport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer x");
            }
            other => panic!("expected http transport, got {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_url() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"mcpServers":{{"bad":{{"type":"http","url":"ftp://example.com"}}}}}}"#
        )
        .unwrap();
        assert!(load_mcp_config(f.path()).is_err());
    }

    #[test]
    fn rejects_missing_file() {
        let p = Path::new("/tmp/does-not-exist-12345.json");
        assert!(load_mcp_config(p).is_err());
    }

    #[test]
    fn tool_name_sanitize_and_parse() {
        let full = mcp_tool_name("my-server", "my tool");
        assert_eq!(full, "mcp__my_server__my_tool");
        let parsed = parse_mcp_tool_name(&full).unwrap();
        assert_eq!(parsed.0, "my_server");
        assert_eq!(parsed.1, "my_tool");
    }

    #[test]
    fn converts_mcp_tools_to_openai() {
        let tools = vec![McpTool {
            name: "search".into(),
            description: Some("Search docs".into()),
            input_schema: json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}),
        }];
        let openai = mcp_tools_to_openai("srv", &tools);
        assert_eq!(openai.len(), 1);
        assert_eq!(
            openai[0].pointer("/function/name").unwrap(),
            "mcp__srv__search"
        );
    }

    #[test]
    fn manager_handles_missing_config() {
        let mgr = McpManager::from_config(None).unwrap();
        assert!(mgr.servers.is_empty());
    }

    #[test]
    fn streamable_http_type_accepted() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"mcpServers":{{"srv":{{"type":"streamable-http","url":"https://example.com/mcp"}}}}}}"#
        )
        .unwrap();
        let m = load_mcp_config(f.path()).unwrap();
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn manager_exposes_loaded_tool_schemas() {
        let mut manager = McpManager::default();
        manager.tool_schemas.push(json!({
            "type": "function",
            "function": {"name": "mcp__srv__search"}
        }));

        assert_eq!(manager.schemas().len(), 1);
        assert_eq!(manager.schemas()[0]["function"]["name"], "mcp__srv__search");
    }

    // --- stdio config parsing ---

    #[test]
    fn stdio_config_inferred_from_command() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"mcpServers":{{"fs":{{"command":"npx","args":["-y","some-mcp-server"],"env":{{"KEY":"value"}}}}}}}}"#
        )
        .unwrap();
        let m = load_mcp_config(f.path()).unwrap();
        let s = m.get("fs").unwrap();
        match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 2);
                assert_eq!(args[0], "-y");
                assert_eq!(args[1], "some-mcp-server");
                assert_eq!(env.get("KEY").map(String::as_str), Some("value"));
            }
            other => panic!("expected stdio transport inferred from command, got {other:?}"),
        }
    }

    #[test]
    fn stdio_config_explicit_type() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"mcpServers":{{"fetch":{{"type":"stdio","command":"uvx","args":["mcp-server-fetch"]}}}}}}"#
        )
        .unwrap();
        let m = load_mcp_config(f.path()).unwrap();
        let s = m.get("fetch").unwrap();
        match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "uvx");
                assert_eq!(args.len(), 1);
                assert_eq!(args[0], "mcp-server-fetch");
                assert!(env.is_empty());
            }
            other => panic!("expected stdio transport, got {other:?}"),
        }
    }

    #[test]
    fn stdio_config_missing_command_errors() {
        let cases = [
            // typed stdio without a command
            r#"{"mcpServers":{"bad":{"type":"stdio"}}}"#,
            // typed stdio with a blank command
            r#"{"mcpServers":{"bad":{"type":"stdio","command":"   "}}}"#,
            // no type, no command, no url
            r#"{"mcpServers":{"bad":{}}}"#,
        ];
        for case in cases {
            let mut f = NamedTempFile::new().unwrap();
            writeln!(f, "{case}").unwrap();
            assert!(
                load_mcp_config(f.path()).is_err(),
                "expected error for {case}"
            );
        }
    }

    #[test]
    fn http_config_missing_url_errors() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"mcpServers":{{"bad":{{"type":"http"}}}}}}"#).unwrap();
        let err = load_mcp_config(f.path()).unwrap_err();
        assert!(err.to_string().contains("url is empty"), "got: {err}");
    }

    // --- JSON-RPC line framing helpers ---

    #[test]
    fn jsonrpc_framing_request_serialization() {
        let mut next = 1u64;
        let id1 = next_request_id(&mut next);
        let id2 = next_request_id(&mut next);
        assert_eq!((id1, id2), (1, 2));
        assert_eq!(next, 3);

        let line = serialize_request(id1, "tools/list", &json!({}));
        assert!(!line.contains('\n'), "framing must be single-line");
        let v: Value = serde_json::from_str(&line).expect("request line is valid json");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "tools/list");
        assert_eq!(v["params"], json!({}));

        let notif_line = serialize_notification("notifications/initialized");
        let n: Value = serde_json::from_str(&notif_line).unwrap();
        assert!(is_notification(&n));
        assert!(n.get("id").is_none());
        assert_eq!(n["method"], "notifications/initialized");
    }

    #[test]
    fn jsonrpc_framing_response_matching() {
        let ok = json!({"jsonrpc":"2.0","id":7,"result":{"tools":[]}});
        assert!(is_response_for(&ok, 7));
        assert!(!is_response_for(&ok, 8));
        let err = json!({"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}});
        assert!(is_response_for(&err, 7));
        // ids echoed as strings still correlate
        let str_id = json!({"jsonrpc":"2.0","id":"7","result":{}});
        assert!(is_response_for(&str_id, 7));
        let notif = json!({"jsonrpc":"2.0","method":"notifications/message"});
        assert!(is_notification(&notif));
        assert!(!is_response_for(&notif, 7));
        assert!(!is_notification(&ok));
    }

    // --- live stdio transport against fake PowerShell servers ---

    /// Base64 of the script, UTF-16LE, for `powershell -EncodedCommand` (this
    /// avoids all command-line quoting pitfalls around embedded JSON quotes).
    #[cfg(windows)]
    fn encode_ps_command(script: &str) -> String {
        use base64::Engine as _;
        let utf16le: Vec<u8> = script
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        base64::engine::general_purpose::STANDARD.encode(utf16le)
    }

    /// Probe once per test process; tests return early when PowerShell cannot
    /// be spawned for environmental reasons (must not fail on privileges).
    #[cfg(windows)]
    fn powershell_available() -> bool {
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| {
            Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    #[cfg(windows)]
    const ROUNDTRIP_PS: &str = r#"$ErrorActionPreference = 'Stop'
while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { break }
  $t = $line.Trim()
  if ($t.Length -eq 0) { continue }
  try { $m = $t | ConvertFrom-Json } catch { continue }
  if ($null -eq $m.id) { continue }
  $id = [string]$m.id
  if ($m.method -eq 'initialize') {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $id + ',"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake-echo","version":"0.0.1"}}}')
  } elseif ($m.method -eq 'tools/list') {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $id + ',"result":{"tools":[{"name":"echo_tool","description":"Echoes back a canned result","inputSchema":{"type":"object","properties":{"msg":{"type":"string"}}}}]}}')
  } elseif ($m.method -eq 'tools/call') {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"canned echo result"}]}}')
  }
}"#;

    #[cfg(windows)]
    const HANG_PS: &str = r#"$ErrorActionPreference = 'Stop'
while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { break }
}"#;

    #[cfg(windows)]
    fn write_stdio_config(f: &mut NamedTempFile, name: &str, script: &str) {
        writeln!(
            f,
            r#"{{"mcpServers":{{"{name}":{{"type":"stdio","command":"powershell","args":["-NoProfile","-EncodedCommand","{}"]}}}}}}"#,
            encode_ps_command(script)
        )
        .unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn stdio_roundtrip_with_fake_server() {
        if !powershell_available() {
            return;
        }
        let mut f = NamedTempFile::new().unwrap();
        write_stdio_config(&mut f, "fakesrv", ROUNDTRIP_PS);
        let mut mgr = McpManager::from_config(Some(f.path())).unwrap();

        // Client level: handshake + tools/list over the real stdio pipe.
        let client = mgr.servers.get("fakesrv").expect("stdio server registered");
        let tools = client
            .list_tools()
            .expect("fake stdio server handshake and tools/list");
        assert!(
            tools.iter().any(|t| t.name == "echo_tool"),
            "echo_tool not discovered, got: {tools:?}"
        );

        // Manager level: schemas populated and dispatch returns the canned text.
        mgr.load_tools();
        assert!(!mgr.schemas().is_empty());
        assert!(mgr.is_mcp_tool("mcp__fakesrv__echo_tool"));
        let out = mgr
            .dispatch("mcp__fakesrv__echo_tool", &json!({"msg": "hi"}))
            .expect("dispatch through the stdio pipe");
        assert_eq!(out, "canned echo result");
    }

    #[test]
    #[cfg(windows)]
    fn stdio_hang_server_times_out_and_recovers() {
        if !powershell_available() {
            return;
        }
        let mut f = NamedTempFile::new().unwrap();
        write_stdio_config(&mut f, "hangsrv", HANG_PS);
        let mgr = McpManager::from_config(Some(f.path())).unwrap();
        let client = mgr.servers.get("hangsrv").unwrap();
        client.set_stdio_request_timeout(Duration::from_millis(1500));

        let start = Instant::now();
        let res = client.list_tools();
        let elapsed = start.elapsed();
        assert!(res.is_err(), "unresponsive server must produce an error");
        assert!(
            elapsed < Duration::from_secs(15),
            "timeout should fire near the 1.5s deadline, took {elapsed:?}"
        );

        // After the timeout the transport was reset; the next request respawns
        // the child once, re-initializes, and fails again without wedging.
        let start = Instant::now();
        let res = client.call_tool("echo_tool", json!({}));
        assert!(
            res.is_err(),
            "second attempt against a hanging server must error"
        );
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "respawn + re-initialize must stay bounded, took {:?}",
            start.elapsed()
        );
    }
}
