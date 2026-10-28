use anyhow::Result;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Path, Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{
    http::{HeaderMap, StatusCode},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::media;
use crate::providers::CatalogModel;
use crate::runtime::{AgentEvent, Runtime};
use crate::session::{Session, SessionMeta};

#[path = "studio.rs"]
mod studio;

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) status: serde_json::Value,
    pub(crate) models: Vec<CatalogModel>,
    pub(crate) busy: bool,
    /// Cached MCP summary so `/api/overview` can answer while a turn holds
    /// the runtime lock.
    pub(crate) mcp: serde_json::Value,
    /// Cached plan artifact summary, refreshed with the status snapshot.
    pub(crate) plan: serde_json::Value,
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) inner: Arc<Mutex<Inner>>,
    pub(crate) snap: Arc<RwLock<Snapshot>>,
    pub(crate) dashboard_token: Option<String>,
    pub(crate) allowed_hosts: Arc<Vec<String>>,
    /// Gateway fan-out: every agent event of every turn is serialized into a
    /// gateway frame and broadcast here. WebSocket clients subscribe; send
    /// errors (no subscribers, lagged receivers) are ignored by design.
    pub events: broadcast::Sender<String>,
    /// Effective configuration of this `serve` process, used by the
    /// read-only overview (auth state, decisions URL) without locking the
    /// runtime.
    pub(crate) cfg: Arc<Config>,
    pub(crate) serve_host: Arc<String>,
    pub(crate) serve_port: u16,
    /// Process uptime clock for the overview/usage panels.
    pub(crate) started: std::time::Instant,
    pub(crate) started_at: chrono::DateTime<chrono::Utc>,
    /// Cached decisions-service health (10s TTL) so overview polling never
    /// stacks health probes.
    pub(crate) decisions_cache: Arc<Mutex<Option<(std::time::Instant, serde_json::Value)>>>,
    /// Approval relay broadcast channel for remote listeners (such as Telegram).
    pub relay_events: Option<broadcast::Sender<crate::approval_relay::RelayRequest>>,
}

pub(crate) struct Inner {
    pub(crate) runtime: Runtime,
    pub(crate) session: Session,
}

/// Read-only MCP state for the overview: configured servers with their last
/// observed status, plus how many tools are indexed lazily (`tool_search`
/// territory) versus already exposed to the model. Never probes a server.
fn mcp_summary(mcp: &crate::mcp::McpManager) -> serde_json::Value {
    let mut eager: Vec<&str> = mcp.eager_servers.iter().map(String::as_str).collect();
    eager.sort_unstable();
    serde_json::json!({
        "configured": mcp.servers.len(),
        "servers": mcp.status(),
        "deferred_tools": mcp.tool_index.len(),
        "exposed_tools": mcp.tool_schemas.len(),
        "eager_servers": eager,
    })
}

/// The latest `/plan` artifact, without its (potentially long) text.
fn plan_summary(runtime: &Runtime) -> serde_json::Value {
    match runtime.plan.as_ref() {
        Some(plan) => serde_json::json!({
            "task": plan.task,
            "approved": plan.approved,
            "has_text": !plan.text.trim().is_empty(),
        }),
        None => serde_json::Value::Null,
    }
}

/// Runtime fields shared by `/api/overview` and `/api/usage`. Reads only the
/// in-memory runtime; no I/O and no secrets.
fn runtime_brief(runtime: &Runtime, session: &Session) -> serde_json::Value {
    serde_json::json!({
        "model": runtime.cfg.model,
        "provider": runtime.cfg.provider,
        "profile": runtime.cfg.selected_profile_id(),
        "permission": runtime.permission.as_str(),
        "sandbox": runtime.cfg.sandbox,
        "effort": runtime.cfg.effort,
        "cwd": runtime.jail.cwd.display().to_string(),
        "session": session.id(),
        "turns": runtime.turns,
        "turn_time_secs": runtime.turn_time.as_secs(),
        "input_tokens": runtime.input_tokens,
        "output_tokens": runtime.output_tokens,
        "usage_reported": runtime.usage_count > 0,
        "context_estimate_tokens": runtime.context_tokens(session),
        "plan": plan_summary(runtime),
        "mcp": mcp_summary(&runtime.mcp),
        "limits": limit_summary(&runtime.cfg),
    })
}

/// Configured ceilings an operator can compare against live counters. Absent
/// optional limits stay `null` rather than being guessed.
fn limit_summary(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "max_tool_rounds": cfg.max_tool_rounds,
        "max_tokens": cfg.max_tokens,
        "goal_max_rounds": cfg.goal_max_rounds,
        "goal_max_minutes": cfg.goal_max_minutes,
        "auto_compact": {
            "enabled": cfg.auto_compact,
            "percent": cfg.auto_compact_percent,
            "threshold_tokens": crate::compact::auto_compact_threshold(
                crate::compact::DEFAULT_MESSAGE_BUDGET,
                cfg.auto_compact_percent,
            ),
            "budget_tokens": crate::compact::DEFAULT_MESSAGE_BUDGET,
        },
    })
}

/// Gateway fan-out frame for one agent event:
/// `{"type":"event","session":<id>,"event":{"kind":…,"text":…},"ts":<rfc3339>}`.
pub fn event_frame(session: &str, ev: &AgentEvent) -> String {
    serde_json::json!({
        "type": "event",
        "session": session,
        "event": { "kind": ev.kind, "text": ev.text },
        "ts": chrono::Utc::now().to_rfc3339(),
    })
    .to_string()
}

/// Serializes one agent event and pushes it to every gateway subscriber.
/// Broadcast sends never block and never fail loudly: a missing or slow
/// subscriber is the subscriber's problem (it gets a lagged notice).
pub(crate) fn broadcast_event(tx: &broadcast::Sender<String>, session: &str, ev: &AgentEvent) {
    let _ = tx.send(event_frame(session, ev));
}

impl AppState {
    pub async fn turn(&self, message: &str) -> anyhow::Result<String> {
        self.turn_with_media(message, Vec::new()).await
    }

    pub async fn turn_with_media(
        &self,
        message: &str,
        images: Vec<crate::session::ImageAttachment>,
    ) -> anyhow::Result<String> {
        let mut g = self.inner.lock().await;
        self.refresh_snapshot(&g, true).await;
        let Inner { runtime, session } = &mut *g;
        runtime.pending_images.extend(images);
        let sid = session.id().to_string();
        let tx = self.events.clone();
        let result = runtime
            .turn(session, message, |ev| broadcast_event(&tx, &sid, &ev))
            .await;
        self.refresh_snapshot(&g, false).await;
        result
    }

    pub async fn switch_model(&self, model: &str) -> anyhow::Result<String> {
        let mut g = self.inner.lock().await;
        let m = model.trim();
        if m.is_empty() {
            return Ok(format!("Current model: `{}`", g.runtime.cfg.model));
        }
        g.runtime.cfg.model = m.to_string();
        self.refresh_snapshot(&g, false).await;
        Ok(format!("Switched model to `{m}`"))
    }

    pub async fn new_session(&self) -> anyhow::Result<String> {
        let mut g = self.inner.lock().await;
        let cwd = g.runtime.jail.cwd.clone();
        let model = g.runtime.cfg.model.clone();
        let s = Session::new(&cwd.display().to_string(), &model)?;
        let id = s.id().to_string();
        g.session = s;
        g.runtime.reset_session_state();
        self.refresh_snapshot(&g, false).await;
        Ok(id)
    }

    pub async fn status_text(&self) -> String {
        let g = self.inner.lock().await;
        let model = &g.runtime.cfg.model;
        let provider = &g.runtime.cfg.provider;
        let sid = g.session.id();
        let turns = g.runtime.turns;
        let uptime = self.started.elapsed().as_secs();
        let pending_approvals = crate::approval_relay::pending_count();
        let always = crate::approval_relay::list_always();
        let always_str = if always.is_empty() {
            "none".to_string()
        } else {
            always.join(", ")
        };
        format!(
            "📊 *Varynth Status*\n\n\
             • Model: `{model}`\n\
             • Provider: `{provider}`\n\
             • Session: `{sid}`\n\
             • Turns: {turns}\n\
             • Uptime: {:02}h {:02}m {:02}s\n\
             • Pending Approvals: {pending_approvals}\n\
             • Always Approved: {always_str}",
            uptime / 3600,
            (uptime % 3600) / 60,
            uptime % 60
        )
    }

    async fn refresh_snapshot(&self, inner: &Inner, busy: bool) {
        let mut snap = self.snap.write().await;
        snap.status = inner.runtime.status_json(Some(&inner.session));
        snap.mcp = mcp_summary(&inner.runtime.mcp);
        snap.plan = plan_summary(&inner.runtime);
        snap.busy = busy;
    }

    /// `{"ok":true,"sessions":[…]}` payload behind `GET /api/sessions`,
    /// shared with the gateway's `sessions.list` (which pins the free
    /// [`sessions_payload`] twin as a plain `fn() -> Value` pointer).
    pub(crate) async fn sessions_payload(&self) -> serde_json::Value {
        sessions_payload()
    }

    /// Full transcript payload behind `GET /api/session/{id}`; mirrors the
    /// handler exactly so gateway-facing callers can reuse it.
    pub(crate) async fn session_payload(&self, id: &str) -> serde_json::Value {
        let bus = crate::mailbox::Bus::default();
        match Session::load(id) {
            Ok(s) => with_paused(session_value(s, id, &bus), id),
            Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// Live status payload behind `GET /api/status`: the cached snapshot with
    /// the busy flag overlaid, or a fresh runtime status when no snapshot
    /// exists yet.
    pub(crate) async fn status_payload(&self) -> serde_json::Value {
        let snap = self.snap.read().await;
        let v = snap.status.clone();
        if v.is_null() {
            drop(snap);
            let g = self.inner.lock().await;
            return live_status(g.runtime.status_json(Some(&g.session)), false);
        }
        live_status(v, snap.busy)
    }

    /// Delivers a message to another session over the message bus on behalf
    /// of the dashboard; the HTTP twin is `POST /api/session/{id}/messages`,
    /// which stays state-free so tests can invoke it directly (hence this
    /// method has no in-crate caller of its own yet).
    #[allow(dead_code)]
    pub(crate) async fn message_session(&self, id: &str, text: &str) -> Response {
        message_session_send(id, text).await
    }
}

#[derive(Deserialize)]
pub struct ChatReq {
    pub message: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub media_ids: Vec<String>,
}

#[derive(Deserialize)]
pub struct CompletionMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Deserialize)]
pub struct CompletionReq {
    pub messages: Vec<CompletionMessage>,
    pub model: Option<String>,
}

/// Flattens an OpenAI-style message list into one Varynth user turn.
/// Each request gets a fresh session, so the caller's history is the only context.
pub fn render_completion_prompt(messages: &[CompletionMessage]) -> Option<String> {
    let mut system = Vec::new();
    let mut transcript = Vec::new();
    for m in messages {
        let text = m.content.trim();
        if text.is_empty() {
            continue;
        }
        match m.role.as_str() {
            "system" | "developer" => system.push(text.to_string()),
            "assistant" => transcript.push(format!("[you] {text}")),
            _ => transcript.push(text.to_string()),
        }
    }
    if transcript.is_empty() {
        return None;
    }
    // Leading header keeps a chat line like "/goal ..." from becoming a slash command.
    let mut prompt = String::from("[external chat request]\n");
    if !system.is_empty() {
        prompt.push_str("Caller instructions:\n");
        prompt.push_str(&system.join("\n"));
        prompt.push_str("\n\n");
    }
    prompt.push_str("Conversation so far (oldest first):\n");
    prompt.push_str(&transcript.join("\n"));
    prompt.push_str("\n\nReply to the latest message. Your reply is posted verbatim.");
    Some(prompt)
}

#[derive(Serialize)]
pub struct ChatRes {
    pub session_id: String,
    pub reply: String,
    pub events: Vec<String>,
}

pub async fn serve(cfg: Config, cwd: PathBuf) -> Result<()> {
    Config::ensure_home()?;
    // Every bind needs a token, loopback included: `127.0.0.1` is reachable by
    // every local process and by sandboxed children, so the bind address is
    // not an authentication boundary. An explicitly configured token wins;
    // otherwise the token is generated for this process only. It is never
    // written under the varynth home: a sandboxed child runs as the same user
    // with read access to that directory, which is exactly the credential the
    // dashboard must not hand out.
    let mut cfg = cfg;
    let token = install_process_dashboard_token(&mut cfg)?;
    let configured_token = !cfg.dashboard_token_ephemeral;
    // A token persisted by an older build stays on disk after this one stops
    // using it; remove it so it cannot be replayed by anything that can read
    // the home directory.
    retire_legacy_token(&token_path());
    tokio::spawn(crate::automation::run_loop(cfg.clone(), cwd.clone()));
    let mut runtime = Runtime::new(cfg.clone(), cwd.clone())?;
    // Remote approval relay: the runtime broadcasts RelayRequest slots onto
    // this channel; the gateway forwarder bridges them onto the event stream
    // so WebSocket clients (and the deck) can answer with approval.respond.
    let (relay_tx, relay_rx) = tokio::sync::broadcast::channel::<crate::approval_relay::RelayRequest>(
        crate::gateway::RELAY_CAPACITY,
    );
    runtime.remote_approval_events = Some(relay_tx.clone());
    let session = Session::new(&cwd.display().to_string(), &runtime.cfg.model)?;
    let host = cfg.dashboard_host.clone();
    let port = cfg.dashboard_port;
    let models = runtime.catalog_models().await;
    let status = runtime.status_json(Some(&session));
    let (events_tx, _) = broadcast::channel(crate::gateway::EVENT_CAPACITY);
    let mcp = mcp_summary(&runtime.mcp);
    let plan = plan_summary(&runtime);
    let state = AppState {
        inner: Arc::new(Mutex::new(Inner { runtime, session })),
        snap: Arc::new(RwLock::new(Snapshot {
            status,
            models,
            busy: false,
            mcp,
            plan,
        })),
        dashboard_token: cfg.dashboard_token.clone(),
        allowed_hosts: Arc::new(host_allowlist(&cfg)),
        events: events_tx,
        cfg: Arc::new(cfg.clone()),
        serve_host: Arc::new(host.clone()),
        serve_port: port,
        started: std::time::Instant::now(),
        started_at: chrono::Utc::now(),
        decisions_cache: Arc::new(Mutex::new(None)),
        relay_events: Some(relay_tx.clone()),
    };
    // The gateway shares the same state through an Arc; the dashboard
    // handlers keep taking AppState by value (it is a cheap Arc-bundle).
    let shared = Arc::new(state.clone());
    let protected = Router::new()
        .route("/api/status", get(api_status))
        .route("/api/overview", get(api_overview))
        .route("/api/usage", get(api_usage))
        .route("/api/approvals", get(api_approvals))
        .route("/api/approvals/{id}", post(api_approval_respond))
        .route("/api/models", get(api_models))
        .route("/api/sessions", get(api_sessions))
        .route("/api/session/new", post(api_session_new))
        .route("/api/session/{id}", get(api_session))
        .route("/api/session/{id}/messages", post(api_session_message))
        .route("/api/session/{id}/fork", post(api_session_fork))
        .route("/api/session/{id}/export", get(api_session_export))
        .route(
            "/api/session/{id}/checkpoints",
            get(api_session_checkpoints),
        )
        .route("/api/chat", post(api_chat))
        .route("/api/chat/stream", post(api_chat_stream))
        .route("/api/channels", get(api_channels))
        .route("/api/doctor", get(api_doctor))
        .route(
            "/api/media",
            post(api_media_upload).layer(DefaultBodyLimit::max(media::MAX_MEDIA_BYTES)),
        )
        .route("/api/media/{id}", get(api_media_get))
        .route("/api/studio", get(api_studio))
        .route("/api/studio/mcp/probe", post(api_studio_mcp_probe))
        .route("/api/studio/mcp", post(api_studio_mcp_add))
        .route("/api/studio/plugin", post(api_studio_plugin))
        .route("/api/studio/provider", post(api_studio_provider))
        .route(
            "/api/studio/skill",
            post(api_studio_skill).layer(DefaultBodyLimit::max(512 * 1024)),
        )
        .route(
            "/api/theme",
            get(api_theme)
                .put(api_theme_update)
                .layer(DefaultBodyLimit::max(crate::theme_tokens::MAX_THEME_BYTES)),
        )
        .route("/v1/chat/completions", post(v1_chat_completions))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            AuthGuard {
                token: state.dashboard_token.clone(),
            },
            require_dashboard_auth,
        ));
    let app = host_guard(
        Router::new()
            .route("/", get(index))
            .route("/studio", get(index))
            .route("/studio/", get(index))
            .route("/health", get(|| async { "ok" }))
            .route("/diff", get(diff_page))
            .route("/diff.html", get(diff_page))
            .route("/assets/{name}", get(public_asset))
            .route("/favicon.svg", get(favicon))
            .route("/GATEWAY.md", get(gateway_doc))
            .with_state(state.clone())
            .merge(protected)
            .merge(crate::gateway::router(shared.clone())),
        state.allowed_hosts.clone(),
    );

    crate::telegram::spawn(cfg.clone(), state.clone());
    tokio::spawn(crate::gateway::control_event_forwarder(shared.clone()));
    tokio::spawn(crate::gateway::approval_relay_forwarder(shared, relay_rx));

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // The token is printed only to an interactive stderr. A redirected stderr
    // is usually a log file with wider permissions than the process, so a
    // non-interactive start announces the address without the secret and
    // expects `dashboard_token`/`VARYNTH_DASHBOARD_TOKEN` for automation.
    let public_bind = host == "0.0.0.0" || host == "::";
    print_dashboard_url(&addr, public_bind.then_some(port), &token, configured_token);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Wraps any router with the dashboard host allowlist. `serve` guards the
/// whole app with it; tests mount the same guard over smaller route sets.
fn host_guard<S>(routes: Router<S>, allowed_hosts: Arc<Vec<String>>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    routes.layer(middleware::from_fn_with_state(
        allowed_hosts,
        require_dashboard_host,
    ))
}

async fn require_dashboard_host(
    State(allowed_hosts): State<Arc<Vec<String>>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if host_header_allowed(host, &allowed_hosts) {
        next.run(request).await
    } else {
        (StatusCode::BAD_REQUEST, "invalid host header").into_response()
    }
}

fn host_allowlist(cfg: &Config) -> Vec<String> {
    let port = cfg.dashboard_port;
    [cfg.dashboard_host.as_str(), "127.0.0.1", "localhost"]
        .into_iter()
        .map(|h| format!("{h}:{port}"))
        .collect()
}

fn host_header_allowed(header: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|h| h.eq_ignore_ascii_case(header))
}

pub(crate) fn same_origin(headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    origin
        .to_str()
        .is_ok_and(|origin| origin_matches_host(origin, headers))
}

fn origin_matches_host(origin: &str, headers: &axum::http::HeaderMap) -> bool {
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    origin.eq_ignore_ascii_case(&format!("http://{host}"))
        || origin.eq_ignore_ascii_case(&format!("https://{host}"))
}

/// State-changing requests must prove their origin. Browsers always send
/// `Sec-Fetch-Site` (or `Origin`) on POST/PUT/PATCH/DELETE, including
/// same-origin fetches, so a request carrying neither — or a foreign value —
/// is a local process or another site and is refused even on loopback.
pub(crate) fn state_change_allowed(headers: &axum::http::HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site
            .to_str()
            .is_ok_and(|value| value.eq_ignore_ascii_case("same-origin"));
    }
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origin_matches_host(origin, headers)
}

fn is_read_only(method: &axum::http::Method) -> bool {
    matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    )
}

/// Auth inputs for [`require_dashboard_auth`]. Separate from [`AppState`] so
/// route tests can mount the guard without building a runtime.
#[derive(Clone)]
pub(crate) struct AuthGuard {
    pub(crate) token: Option<String>,
}

pub(crate) fn tokens_equal(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as u64;
    }
    diff == 0
}

async fn require_dashboard_auth(
    State(guard): State<AuthGuard>,
    request: Request,
    next: Next,
) -> Response {
    if !same_origin(request.headers()) {
        return (StatusCode::FORBIDDEN, "foreign origin rejected").into_response();
    }
    if !is_read_only(request.method()) && !state_change_allowed(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "state-changing request requires a matching Origin or Sec-Fetch-Site",
        )
            .into_response();
    }
    // Fail closed: `serve` always installs a token, so an absent one is a bug
    // or a test harness, never an invitation.
    let Some(expected) = guard.token.as_deref().filter(|token| !token.is_empty()) else {
        return (
            StatusCode::UNAUTHORIZED,
            "dashboard authentication required",
        )
            .into_response();
    };
    let authorized = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|provided| tokens_equal(provided, expected));
    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "dashboard authentication required",
        )
            .into_response()
    }
}

/// Token file older builds persisted. Nothing reads it any more; `serve`
/// removes it so a sandboxed child cannot replay a token.
pub(crate) fn token_path() -> PathBuf {
    Config::home_dir().join("dashboard.token")
}

/// Delete a token file left behind by a build that persisted the dashboard
/// token. Best effort: an absent or unreadable path is not an error.
pub(crate) fn retire_legacy_token(path: &FsPath) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
            let _ = std::fs::remove_file(path);
        }
        _ => {}
    }
}

/// Announce the dashboard address. The token appears only when stderr is a
/// terminal, because redirected stderr commonly lands in a log file that is
/// readable beyond this process.
fn print_dashboard_url(addr: &str, lan_port: Option<u16>, token: &str, configured: bool) {
    use std::io::IsTerminal;
    let lan = lan_port.and_then(|port| local_ipv4().ok().map(|ip| format!("http://{ip}:{port}/")));
    let interactive = std::io::stderr().is_terminal();
    for line in dashboard_banner(addr, lan.as_deref(), token, configured, interactive) {
        eprintln!("{line}");
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ServeAuthRecord {
    pub password: String,
    pub created_at: i64,
    pub expires_at: i64,
}

pub(crate) fn serve_auth_path() -> PathBuf {
    Config::home_dir().join("serve_auth.json")
}

pub(crate) fn resolve_30day_serve_password() -> Result<(String, bool, i64)> {
    let now = chrono::Utc::now().timestamp();
    let path = serve_auth_path();

    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(rec) = serde_json::from_str::<ServeAuthRecord>(&content) {
            if now < rec.expires_at && valid_token_shape(&rec.password) {
                return Ok((rec.password, false, rec.expires_at));
            }
        }
    }

    // Generate fresh 30-day rotating password: "vry-serve-" + 32-char UUID
    let password = format!("vry-serve-{}", uuid::Uuid::new_v4().simple());
    let expires_at = now + 30 * 86400; // 30 days
    let record = ServeAuthRecord {
        password: password.clone(),
        created_at: now,
        expires_at,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json_str) = serde_json::to_string_pretty(&record) {
        let _ = std::fs::write(&path, json_str);
    }

    Ok((password, true, expires_at))
}

/// Lines `serve` announces. The token appears only for an interactive start;
/// `interactive` is a parameter so the rule is testable without a terminal.
fn dashboard_banner(
    addr: &str,
    lan: Option<&str>,
    token: &str,
    configured: bool,
    interactive: bool,
) -> Vec<String> {
    if interactive {
        let mut lines = vec![
            "┌────────────────────────────────────────────────────────────────────────┐".into(),
            "│  🔑 VARYNTH SERVE — 30-DAY ROTATING ACCESS GATEWAY                     │".into(),
            "├────────────────────────────────────────────────────────────────────────┤".into(),
            format!("│  • Terminal Pass : {token}"),
            "│  • Pass Rotation : Rotates every 30 days automatically".into(),
            format!("│  • Web Deck URL  : http://{addr}/?token={token}"),
        ];
        if let Some(lan) = lan {
            lines.push(format!("│  • LAN Access URL: {lan}?token={token}"));
        }
        lines.push("└────────────────────────────────────────────────────────────────────────┘".into());
        return lines;
    }
    let mut lines = vec![format!("varynth dashboard → http://{addr}/")];
    if let Some(lan) = lan {
        lines.push(format!("varynth dashboard LAN → {lan}"));
    }
    if configured {
        lines.push(
            "varynth: authenticated with the configured dashboard token (not printed)".into(),
        );
    } else {
        lines.push(
            "varynth: the dashboard token is ephemeral and rotates every 30 days (not printed because stderr is not a terminal).\n   set `dashboard_token` or VARYNTH_DASHBOARD_TOKEN for non-interactive access".into(),
        );
    }
    lines
}

/// Effective dashboard token: an explicit config/env token wins, otherwise a
/// 30-day rotating password is resolved and persisted in `~/.varynth/serve_auth.json`.
pub(crate) fn install_process_dashboard_token(cfg: &mut Config) -> Result<String> {
    let configured = cfg
        .dashboard_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|token| !token.is_empty());
    let token = if configured {
        resolve_dashboard_token(cfg)?
    } else {
        let (pass, _rotated, _exp) = resolve_30day_serve_password()?;
        pass
    };
    cfg.dashboard_token = Some(token.clone());
    cfg.dashboard_token_ephemeral = !configured;
    Ok(token)
}

pub(crate) fn resolve_dashboard_token(cfg: &Config) -> Result<String> {
    if let Some(token) = cfg
        .dashboard_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        return Ok(token.to_string());
    }
    Ok(generate_token())
}

fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn valid_token_shape(token: &str) -> bool {
    token.len() >= 32
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// Read `path` as a token, generating and persisting one when it is missing
/// or malformed. Unix creates the file 0600 before the secret is written.
/// Used for the decisions service's shared secret, not the dashboard token.
pub(crate) fn load_or_create_token(path: &FsPath) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let existing = text.trim();
        if valid_token_shape(existing) {
            return Ok(existing.to_string());
        }
    }
    let token = generate_token();
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            meta.file_type().is_file() && !meta.file_type().is_symlink(),
            "invalid token file {}",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    {
        use std::io::Write;
        writeln!(file, "{token}")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

fn local_ipv4() -> Result<std::net::Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    sock.connect("8.8.8.8:80")?;
    match sock.local_addr()?.ip() {
        std::net::IpAddr::V4(v) => Ok(v),
        _ => anyhow::bail!("no ipv4"),
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

/// Side-by-side unified-diff viewer served from `web/diff.html` (no deps).
async fn diff_page() -> Html<&'static str> {
    Html(include_str!("../web/diff.html"))
}

/// Gateway usage note served from `web/GATEWAY.md`.
async fn gateway_doc() -> &'static str {
    include_str!("../web/GATEWAY.md")
}

fn embedded_asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match name {
        "control-room.css" => (
            "text/css",
            include_str!("../web/assets/control-room.css").as_bytes(),
        ),
        "core.mjs" => (
            "text/javascript",
            include_str!("../web/assets/core.mjs").as_bytes(),
        ),
        "control-room.mjs" => (
            "text/javascript",
            include_str!("../web/assets/control-room.mjs").as_bytes(),
        ),
        "diff.mjs" => (
            "text/javascript",
            include_str!("../web/assets/diff.mjs").as_bytes(),
        ),
        "mark.svg" => (
            "image/svg+xml",
            include_str!("../web/assets/mark.svg").as_bytes(),
        ),
        "icons.svg" => (
            "image/svg+xml",
            include_str!("../web/assets/icons.svg").as_bytes(),
        ),
        "geist-latin.woff2" => (
            "font/woff2",
            include_bytes!("../web/assets/geist-latin.woff2"),
        ),
        "GEIST-LICENSE" => (
            "text/plain",
            include_str!("../web/assets/GEIST-LICENSE").as_bytes(),
        ),
        "LUCIDE-LICENSE" => (
            "text/plain",
            include_str!("../web/assets/LUCIDE-LICENSE").as_bytes(),
        ),
        _ => return None,
    })
}

async fn public_asset(Path(name): Path<String>) -> Response {
    match embedded_asset(&name) {
        Some((mime, body)) => ([(axum::http::header::CONTENT_TYPE, mime)], body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn favicon() -> Response {
    public_asset(Path("mark.svg".into())).await
}

async fn api_status(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.status_payload().await)
}

/// Read-only operator summary: serve endpoint/uptime, runtime identity, auth
/// state, the plan artifact, decisions-service health, and MCP/deferred-tool
/// posture. Never includes secrets; token presence is a boolean.
async fn api_overview(State(st): State<AppState>) -> Response {
    let (runtime, stale) = match st.inner.try_lock() {
        Ok(g) => (runtime_brief(&g.runtime, &g.session), false),
        Err(_) => {
            let snap = st.snap.read().await;
            let mut brief = snap.status.clone();
            if let Some(obj) = brief.as_object_mut() {
                obj.insert("mcp".into(), snap.mcp.clone());
                obj.insert("plan".into(), snap.plan.clone());
            }
            (brief, true)
        }
    };
    let auth_cfg = st.cfg.clone();
    let auth = tokio::task::spawn_blocking(move || match auth_cfg.require_provider_credentials() {
        Ok(()) => serde_json::json!({"ok": true, "detail": "provider credentials ready"}),
        Err(error) => serde_json::json!({"ok": false, "detail": error.to_string()}),
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({"ok": false, "detail": "auth check unavailable"}));
    let decisions = decisions_health(&st).await;
    Json(serde_json::json!({
        "ok": true,
        "stale": stale,
        "serve": {
            "host": st.serve_host.as_str(),
            "port": st.serve_port,
            "url": format!("http://{}:{}", st.serve_host, st.serve_port),
            "token_required": st.dashboard_token.is_some(),
            "started_at": st.started_at,
            "uptime_secs": st.started.elapsed().as_secs(),
        },
        "runtime": runtime,
        "auth": auth,
        "decisions": decisions,
    }))
    .into_response()
}

/// `GET /api/decisions`-style cached probe used by the overview. The service
/// runs in another process (Laya/Clef at 127.0.0.1:8791 by default), so this
/// is the only network call the dashboard makes and its result is cached.
async fn decisions_health(st: &AppState) -> serde_json::Value {
    let cache = st.decisions_cache.lock().await;
    if let Some((at, value)) = cache.as_ref() {
        if at.elapsed() < std::time::Duration::from_secs(10) {
            return value.clone();
        }
    }
    drop(cache);
    let cfg = st.cfg.clone();
    let url = crate::decisions::base_url(&cfg);
    let value = match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        crate::decisions::health(&cfg),
    )
    .await
    {
        Ok(Ok(health)) => serde_json::json!({
            "url": url,
            "ok": health.both(),
            "laya": health.laya,
            "clef": health.clef,
        }),
        Ok(Err(error)) => serde_json::json!({
            "url": url,
            "ok": false,
            "laya": false,
            "clef": false,
            "error": error.to_string(),
        }),
        Err(_) => serde_json::json!({
            "url": url,
            "ok": false,
            "laya": false,
            "clef": false,
            "error": "health probe timed out after 4s",
        }),
    };
    *st.decisions_cache.lock().await = Some((std::time::Instant::now(), value.clone()));
    value
}

/// Request/token counters accumulated by this `serve` process, plus the
/// configured ceilings. Counters are scoped to the process lifetime (the
/// only real counter the runtime keeps) and labelled accordingly.
async fn api_usage(State(st): State<AppState>) -> Response {
    let (usage, source) = match st.inner.try_lock() {
        Ok(g) => {
            let r = &g.runtime;
            (
                serde_json::json!({
                    "requests": r.turns,
                    "turns": r.turns,
                    "input_tokens": r.input_tokens,
                    "output_tokens": r.output_tokens,
                    "total_tokens": r.input_tokens + r.output_tokens,
                    "reported": r.usage_count > 0,
                    "turn_time_secs": r.turn_time.as_secs(),
                    "context_estimate_tokens": r.context_tokens(&g.session),
                }),
                "runtime",
            )
        }
        Err(_) => {
            let snap = st.snap.read().await;
            let status = &snap.status;
            let reported = status
                .get("usage")
                .and_then(|u| u.get("reported"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            (
                serde_json::json!({
                    "requests": status.get("turns").cloned().unwrap_or(serde_json::json!(0)),
                    "turns": status.get("turns").cloned().unwrap_or(serde_json::json!(0)),
                    "input_tokens": status.get("usage").and_then(|u| u.get("input_tokens")).cloned().unwrap_or(serde_json::json!(0)),
                    "output_tokens": status.get("usage").and_then(|u| u.get("output_tokens")).cloned().unwrap_or(serde_json::json!(0)),
                    "total_tokens": status.get("usage").and_then(|u| u.get("total_tokens")).cloned().unwrap_or(serde_json::json!(0)),
                    "reported": reported,
                    "turn_time_secs": status.get("turn_time_secs").cloned().unwrap_or(serde_json::json!(0)),
                    "context_estimate_tokens": status.get("usage").and_then(|u| u.get("context_estimate_tokens")).cloned().unwrap_or(serde_json::Value::Null),
                }),
                "snapshot",
            )
        }
    };
    let mut body = usage;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("ok".into(), serde_json::json!(true));
        obj.insert("scope".into(), serde_json::json!("serve"));
        obj.insert("source".into(), serde_json::json!(source));
        obj.insert("started_at".into(), serde_json::json!(st.started_at));
        obj.insert(
            "uptime_secs".into(),
            serde_json::json!(st.started.elapsed().as_secs()),
        );
        obj.insert("limits".into(), limit_summary(&st.cfg));
    }
    Json(body).into_response()
}

/// Pending approvals published by the runtime's relay to
/// `~/.varynth/control/approvals/*.json`. Same expiry rule as the relay
/// (125s); response sidecars are skipped.
fn pending_approvals(dir: &FsPath) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".json") || name.contains(".response.") {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(pending) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(request) = pending.get("request") else {
            continue;
        };
        let Some(id) = request.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let fresh = pending
            .get("created_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|at| chrono::Utc::now().signed_duration_since(at).num_seconds() < 125);
        if !fresh {
            continue;
        }
        out.push(serde_json::json!({
            "id": id,
            "tool": request.get("tool").and_then(serde_json::Value::as_str).unwrap_or("tool"),
            "detail": request.get("detail").and_then(serde_json::Value::as_str).unwrap_or(""),
            "created_at": pending.get("created_at").cloned().unwrap_or(serde_json::Value::Null),
        }));
    }
    out.sort_by(|a, b| {
        a["created_at"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["created_at"].as_str().unwrap_or_default())
    });
    out
}

async fn api_approvals() -> Response {
    let dir = Config::home_dir().join("control").join("approvals");
    match tokio::task::spawn_blocking(move || pending_approvals(&dir)).await {
        Ok(pending) => Json(serde_json::json!({"ok": true, "pending": pending})).into_response(),
        Err(error) => request_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct ApprovalDecision {
    decision: String,
}

/// Answer one relayed approval over HTTP (same relay the gateway uses).
/// Useful before the WebSocket connects; the first answer still wins.
async fn api_approval_respond(
    Path(id): Path<String>,
    Json(body): Json<ApprovalDecision>,
) -> Response {
    let valid_id = !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
    if !valid_id {
        return request_error(StatusCode::BAD_REQUEST, "invalid approval id");
    }
    let Some(vote) = crate::approval_relay::ApprovalVote::parse(&body.decision) else {
        return request_error(
            StatusCode::BAD_REQUEST,
            "decision must be once, always, or deny",
        );
    };
    match tokio::task::spawn_blocking(move || crate::approval_relay::respond(&id, vote)).await {
        Ok(true) => Json(serde_json::json!({"ok": true, "accepted": true})).into_response(),
        Ok(false) => request_error(StatusCode::CONFLICT, "approval is no longer pending"),
        Err(error) => request_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

/// Fork a saved session into a new one (`varynth sessions fork` over HTTP).
async fn api_session_fork(Path(id): Path<String>) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return request_error(StatusCode::BAD_REQUEST, "invalid session id");
    }
    match tokio::task::spawn_blocking(move || Session::fork_from(&id, None)).await {
        Ok(Ok(fork)) => Json(serde_json::json!({
            "ok": true,
            "session_id": fork.meta.id,
            "parent": fork.meta.parent,
            "title": fork.meta.title,
        }))
        .into_response(),
        Ok(Err(error)) => request_error(StatusCode::NOT_FOUND, error),
        Err(error) => request_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

/// Redacted Markdown/JSON transcript download (`varynth sessions export`).
async fn api_session_export(Path(id): Path<String>, Query(query): Query<ExportQuery>) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return request_error(StatusCode::BAD_REQUEST, "invalid session id");
    }
    let format = match crate::export::ExportFormat::parse(query.format.as_deref().unwrap_or("md")) {
        Ok(format) => format,
        Err(error) => return request_error(StatusCode::BAD_REQUEST, error),
    };
    let session = match tokio::task::spawn_blocking(move || Session::load(&id)).await {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => return request_error(StatusCode::NOT_FOUND, error),
        Err(error) => return request_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let body = match format {
        crate::export::ExportFormat::Markdown => crate::export::render_markdown(&session),
        crate::export::ExportFormat::Json => match crate::export::render_json(&session) {
            Ok(body) => body,
            Err(error) => return request_error(StatusCode::INTERNAL_SERVER_ERROR, error),
        },
    };
    let filename = format!("varynth-session-{}.{}", session.meta.id, format.extension());
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(if format == crate::export::ExportFormat::Markdown {
            "text/markdown; charset=utf-8"
        } else {
            "application/json; charset=utf-8"
        }),
    );
    if let Ok(value) =
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, value);
    }
    (StatusCode::OK, headers, body).into_response()
}

/// Read-only `/checkpoints` listing (the undo history for one session).
async fn api_session_checkpoints(Path(id): Path<String>) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return request_error(StatusCode::BAD_REQUEST, "invalid session id");
    }
    let session = id.clone();
    match tokio::task::spawn_blocking(move || {
        crate::checkpoint::CheckpointStore::default().list(&session)
    })
    .await
    {
        Ok(Ok(list)) => {
            let checkpoints: Vec<serde_json::Value> = list
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "seq": c.seq,
                        "path": c.path.display().to_string(),
                        "bytes": c.bytes,
                        "skipped": c.skipped,
                        "at": c.at,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "ok": true,
                "session": id,
                "count": checkpoints.len(),
                "checkpoints": checkpoints,
            }))
            .into_response()
        }
        Ok(Err(error)) => request_error(StatusCode::NOT_FOUND, error),
        Err(error) => request_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn api_models(State(st): State<AppState>) -> impl IntoResponse {
    let Ok(g) = st.inner.try_lock() else {
        return Json(
            serde_json::json!({"ok": false, "error": "runtime busy; retry after the active turn"}),
        );
    };
    let models = g.runtime.catalog_models().await;
    {
        let mut snap = st.snap.write().await;
        snap.models = models.clone();
    }
    Json(serde_json::json!({"ok": true, "models": models}))
}

async fn api_session_new(State(st): State<AppState>) -> Response {
    let Ok(mut g) = st.inner.try_lock() else {
        return request_error(
            StatusCode::CONFLICT,
            "runtime busy; cannot create a session during a turn",
        );
    };
    let cwd = g.runtime.jail.cwd.display().to_string();
    let model = g.runtime.cfg.model.clone();
    match Session::new(&cwd, &model) {
        Ok(s) => {
            let id = s.id().to_string();
            let Inner { runtime, session } = &mut *g;
            install_session(session, s, || runtime.reset_session_state());
            st.refresh_snapshot(&g, false).await;
            Json(serde_json::json!({"ok": true, "session_id": id})).into_response()
        }
        Err(e) => request_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn api_sessions(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.sessions_payload().await)
}

/// Session list payload shared by `/api/sessions` and the gateway's
/// `sessions.list` method.
pub(crate) fn sessions_payload() -> serde_json::Value {
    let bus = crate::mailbox::Bus::default();
    match Session::list() {
        Ok(sessions) => sessions_value(sessions, &bus),
        Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
    }
}

/// Shapes the session list; split from [`sessions_payload`] so tests can run
/// it against a tempdir bus instead of the real `~/.varynth`.
fn sessions_value(sessions: Vec<SessionMeta>, bus: &crate::mailbox::Bus) -> serde_json::Value {
    let sessions: Vec<serde_json::Value> = sessions
        .into_iter()
        .map(|meta| {
            let mut v = serde_json::to_value(&meta).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "unread".into(),
                    serde_json::json!(bus.unread(&meta.id).len()),
                );
            }
            v
        })
        .collect();
    serde_json::json!({"ok": true, "sessions": sessions})
}

/// Shapes one loaded session transcript — the success shape of
/// `GET /api/session/{id}`; unread is counted for the requested id.
fn session_value(s: Session, id: &str, bus: &crate::mailbox::Bus) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "meta": s.meta,
        "messages": s.messages,
        "unread": bus.unread(id).len()
    })
}

/// Inserts the live busy flag into a status snapshot — the non-empty shape of
/// `GET /api/status`.
fn overlay_busy(mut status: serde_json::Value, busy: bool) -> serde_json::Value {
    if let Some(obj) = status.as_object_mut() {
        obj.insert("busy".into(), serde_json::json!(busy));
    }
    status
}

pub(crate) fn with_paused(mut value: serde_json::Value, id: &str) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "paused".into(),
            serde_json::json!(crate::control_bus::ControlBus::default().is_paused(id)),
        );
    }
    value
}

pub(crate) fn live_status(status: serde_json::Value, busy: bool) -> serde_json::Value {
    let id = status
        .get("session")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let status = overlay_busy(status, busy);
    match id {
        Some(id) => with_paused(status, &id),
        None => status,
    }
}

fn request_error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        Json(serde_json::json!({"ok": false, "error": message.to_string()})),
    )
        .into_response()
}

fn install_session(current: &mut Session, selected: Session, reset: impl FnOnce()) {
    if current.id() != selected.id() {
        reset();
        *current = selected;
    }
}

fn check_workspace(cwd: &FsPath, recorded: &str) -> Result<()> {
    let active = cwd.canonicalize()?;
    let selected = FsPath::new(recorded)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("session workspace unavailable: {recorded}: {e}"))?;
    anyhow::ensure!(
        active == selected,
        "session workspace mismatch: session uses {recorded}; serve uses {}",
        cwd.display()
    );
    Ok(())
}

async fn prepare_chat(inner: &mut Inner, req: &ChatReq) -> Result<(), (StatusCode, String)> {
    inner.runtime.pending_images.clear();
    if req.message.trim().is_empty() && req.media_ids.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "message or image is required".into(),
        ));
    }
    if !req.media_ids.is_empty() && local_media_command(&req.message) {
        return Err((
            StatusCode::BAD_REQUEST,
            "images cannot be attached to local commands".into(),
        ));
    }
    if req.message.len() > 512 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "message exceeds 512 KiB".into(),
        ));
    }
    let selected = match req.session_id.as_deref() {
        Some(id) => {
            uuid::Uuid::parse_str(id)
                .map_err(|_| (StatusCode::BAD_REQUEST, "invalid session id".into()))?;
            if id != inner.session.id() {
                let session =
                    Session::load(id).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
                if session.id() != id {
                    return Err((
                        StatusCode::CONFLICT,
                        "session metadata id does not match requested id".into(),
                    ));
                }
                check_workspace(&inner.runtime.jail.cwd, &session.meta.cwd)
                    .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
                Some(session)
            } else {
                None
            }
        }
        None => None,
    };
    let model = req
        .model
        .as_deref()
        .or_else(|| selected.as_ref().map(|s| s.meta.model.as_str()));
    if crate::control_bus::ControlBus::default()
        .is_paused(req.session_id.as_deref().unwrap_or(inner.session.id()))
    {
        return Err((
            StatusCode::CONFLICT,
            "session is paused; resume it before sending a message".into(),
        ));
    }
    let ids = req.media_ids.clone();
    let images = if ids.is_empty() {
        Vec::new()
    } else {
        tokio::task::spawn_blocking(move || media::load_attachments(&ids))
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "media loader unavailable".into(),
                )
            })?
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "invalid, missing, duplicate, or oversized image attachment".into(),
                )
            })?
    };

    if let Some(model) = model {
        if model.trim().is_empty() || model.len() > 512 {
            return Err((
                StatusCode::BAD_REQUEST,
                "model must be non-empty and at most 512 bytes".into(),
            ));
        }
        if model != inner.runtime.cfg.model {
            let mut cfg = inner.runtime.cfg.clone();
            cfg.select_model(model)
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            // Async Google OAuth preflight: a profile/model switch that lands
            // on Google OAuth without a stored credential either completes
            // the browser sign-in here or fails with an actionable message,
            // leaving the current runtime untouched.
            crate::google_auth::preflight(&cfg, false)
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            inner
                .runtime
                .reconfigure(cfg)
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        }
    }
    if let Some(selected) = selected {
        let Inner { runtime, session } = inner;
        install_session(session, selected, || runtime.reset_session_state());
    }
    inner.runtime.pending_images = images;
    Ok(())
}

fn local_media_command(message: &str) -> bool {
    let text = message.trim();
    matches!(
        text,
        "/goal"
            | "/goal status"
            | "/goal clear"
            | "/goal stop"
            | "/goal --clear"
            | "/undo"
            | "/inbox"
    ) || ["/plugin ", "/send ", "/undo ", "/inbox "]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn chat_prompt(req: &ChatReq) -> &str {
    if req.message.trim().is_empty() {
        "Describe these images."
    } else {
        &req.message
    }
}

fn turn_progress(runtime: &Runtime, session: &Session) -> (u32, u64, usize) {
    (
        runtime.goal.as_ref().map_or(0, |g| g.rounds),
        runtime.turns,
        session.messages.len(),
    )
}

/// Message-bus delivery shared by `POST /api/session/{id}/messages` and
/// [`AppState::message_session`].
async fn message_session_send(id: &str, text: &str) -> Response {
    if text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "text is required"})),
        )
            .into_response();
    }
    match crate::mailbox::Bus::default().send("dashboard", id, text) {
        Ok(_) => Json(serde_json::json!({"ok": true, "to": id})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn api_session(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    Json(st.session_payload(&id).await)
}

/// Delivers a dashboard message to another session over the message bus.
async fn api_session_message(
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let text = body
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    message_session_send(&id, text).await
}

async fn api_channels() -> impl IntoResponse {
    Json(crate::channels::v2_status())
}

async fn api_doctor(State(st): State<AppState>) -> impl IntoResponse {
    let g = st.inner.lock().await;
    match crate::doctor::run(&g.runtime.cfg).await {
        Ok(v) => Json(v),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

fn request_media_type(headers: &HeaderMap) -> Option<&'static str> {
    let explicit = headers
        .get("x-media-type")
        .map(|value| value.to_str().ok().and_then(studio::media_type));
    if let Some(media) = explicit {
        return media;
    }
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(studio::media_type)
}

async fn api_media_upload(
    State(_st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        return request_error(StatusCode::BAD_REQUEST, "media body is empty");
    }
    if body.len() > media::MAX_MEDIA_BYTES {
        return request_error(StatusCode::PAYLOAD_TOO_LARGE, "media exceeds 8 MiB");
    }
    let Some(media) = request_media_type(&headers) else {
        return request_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "allowed media types are image/png, image/jpeg, image/webp, and image/gif",
        );
    };
    let result = tokio::task::spawn_blocking(move || {
        media::validate_image(media, &body).map_err(|_| StatusCode::UNSUPPORTED_MEDIA_TYPE)?;
        media::store_image(media, &body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await;
    match result {
        Ok(Ok(stored)) => Json(serde_json::json!({"ok": true, "id": stored.id, "mime": stored.mime, "size": stored.size, "url": stored.url})).into_response(),
        Ok(Err(StatusCode::UNSUPPORTED_MEDIA_TYPE)) => request_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid image type, bytes, or image limits"),
        _ => request_error(StatusCode::INTERNAL_SERVER_ERROR, "media storage unavailable"),
    }
}

async fn api_media_get(Path(id): Path<String>) -> Response {
    match tokio::task::spawn_blocking(move || media::read_image(&id)).await {
        Ok(Ok((mime, bytes))) => (
            [
                (axum::http::header::CONTENT_TYPE, mime),
                (
                    axum::http::header::CACHE_CONTROL,
                    "private, no-store".into(),
                ),
            ],
            bytes,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_studio(State(st): State<AppState>) -> Response {
    let Ok(g) = st.inner.try_lock() else {
        return request_error(
            StatusCode::CONFLICT,
            "runtime busy; retry after the active turn",
        );
    };
    let models = g.runtime.catalog_models().await;
    {
        let mut snap = st.snap.write().await;
        snap.models = models.clone();
    }
    let skills = g
        .runtime
        .skills
        .iter()
        .map(|skill| crate::skills_search::SkillEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            source: skill.path.display().to_string(),
            installs: None,
        })
        .collect();
    Json(studio::studio_response(
        &g.runtime.cfg,
        &models,
        g.runtime.mcp.status(),
        skills,
        &g.runtime.jail.cwd,
    ))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct McpProbeReq {
    #[serde(alias = "id")]
    name: String,
}

async fn api_studio_mcp_probe(
    State(st): State<AppState>,
    Json(req): Json<McpProbeReq>,
) -> Response {
    let name = match studio::validate_identifier(&req.name, "server name") {
        Ok(name) => name,
        Err(error) => return request_error(StatusCode::BAD_REQUEST, error),
    };
    let mut g = st.inner.lock().await;
    match g.runtime.mcp.probe_server_with_tools(&name) {
        Ok(status) => {
            st.refresh_snapshot(&g, false).await;
            Json(serde_json::json!({
                "ok": true,
                "server": studio::mcp_rows(vec![status]).into_iter().next(),
            }))
            .into_response()
        }
        Err(_) => {
            let status = g
                .runtime
                .mcp
                .status()
                .into_iter()
                .find(|status| status.name == name)
                .map(|status| studio::mcp_rows(vec![status]).into_iter().next());
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "ok": false,
                    "error": "mcp probe failed",
                    "server": status.flatten(),
                })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct StudioMcpAddReq {
    id: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    entry: Option<serde_json::Value>,
    /// Explicit local confirmation. The dashboard UI sends it for every
    /// studio mutation from its own page; nothing else may add an MCP server.
    #[serde(default)]
    confirm: bool,
}

/// `.varynth/mcp.json` under the workspace is repository content: it can
/// launch arbitrary stdio servers. It only ever loads after `varynth trust`,
/// exactly like the startup selection in `main`.
fn is_workspace_registry(path: &FsPath, cwd: &FsPath) -> bool {
    let expected = cwd.join(".varynth").join("mcp.json");
    if path == expected {
        return true;
    }
    match (path.canonicalize(), expected.canonicalize()) {
        (Ok(path), Ok(expected)) => path == expected,
        _ => false,
    }
}

fn untrusted_workspace_error(cwd: &FsPath) -> String {
    format!(
        "workspace {} is not trusted; its .varynth/mcp.json is not loaded.\n   \
         run `varynth trust` in that directory to enable it",
        cwd.display()
    )
}

/// Refuse a workspace registry in an untrusted checkout. `trusted` is passed
/// in so the decision is testable without a trust store on the real home.
fn ensure_registry_loadable(path: &FsPath, cwd: &FsPath, trusted: bool) -> Result<()> {
    anyhow::ensure!(
        trusted || !is_workspace_registry(path, cwd),
        "{}",
        untrusted_workspace_error(cwd)
    );
    Ok(())
}

/// HTTP-API additions may only name a credentialless HTTP endpoint. A stdio
/// entry (or an untyped entry with a `command`) launches a host process, so it
/// is refused regardless of the `confirm` flag.
fn entry_spawns_host_process(entry: &serde_json::Value) -> bool {
    entry
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("stdio"))
        || entry.get("command").is_some()
}

async fn api_studio_mcp_add(
    State(st): State<AppState>,
    Json(req): Json<StudioMcpAddReq>,
) -> Response {
    if !req.confirm {
        return request_error(
            StatusCode::FORBIDDEN,
            "studio writes require explicit confirmation (send confirm: true)",
        );
    }
    let id = match studio::validate_identifier(&req.id, "MCP server id") {
        Ok(value) => value,
        Err(error) => return request_error(StatusCode::BAD_REQUEST, error),
    };
    let entry = match (req.entry, req.url) {
        (Some(value), None) => value,
        (Some(_), Some(_)) => {
            return request_error(StatusCode::BAD_REQUEST, "provide either entry or url")
        }
        (None, Some(url)) => {
            let url = match reqwest::Url::parse(url.trim()) {
                Ok(value)
                    if matches!(value.scheme(), "http" | "https")
                        && value.username().is_empty()
                        && value.password().is_none() =>
                {
                    value.to_string()
                }
                _ => {
                    return request_error(
                        StatusCode::BAD_REQUEST,
                        "MCP URL must be an HTTP(S) URL without credentials",
                    )
                }
            };
            serde_json::json!({"type":"http","url":url})
        }
        (None, None) => {
            return request_error(StatusCode::BAD_REQUEST, "MCP entry or url is required")
        }
    };
    let transport = entry
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("http")
        .to_ascii_lowercase();
    if !matches!(
        transport.as_str(),
        "http" | "streamable-http" | "streamable_http" | "http-streamable" | "stdio"
    ) {
        return request_error(StatusCode::BAD_REQUEST, "unsupported MCP transport");
    }
    // A stdio entry names a program the CLI spawns outside the OS sandbox.
    // The HTTP API cannot host an interactive confirmation, so it may only
    // add credentialless HTTP endpoints; stdio servers belong to the user's
    // own registry files.
    if entry_spawns_host_process(&entry) {
        return request_error(
            StatusCode::FORBIDDEN,
            "stdio MCP servers cannot be added through the dashboard API; \
             add the entry to .varynth/mcp.json (or a user registry) on the host",
        );
    }
    let Ok(mut g) = st.inner.try_lock() else {
        return request_error(
            StatusCode::CONFLICT,
            "runtime busy; retry after the active turn",
        );
    };
    let cwd = g.runtime.jail.cwd.clone();
    let path = g
        .runtime
        .cfg
        .mcp_config
        .clone()
        .unwrap_or_else(|| cwd.join(".varynth/mcp.json"));
    if let Err(error) = ensure_registry_loadable(&path, &cwd, crate::trust::is_trusted(&cwd)) {
        return request_error(StatusCode::FORBIDDEN, error.to_string());
    }
    let snapshot = match crate::mcp::snapshot_config(&path) {
        Ok(snapshot) => snapshot,
        Err(error) => return request_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    if let Err(error) = crate::mcp::add_server_entry(&path, &id, &entry, false) {
        return request_error(StatusCode::BAD_REQUEST, error.to_string());
    }
    let mut cfg = g.runtime.cfg.clone();
    cfg.mcp_config = Some(path.clone());
    if let Err(error) = g
        .runtime
        .reconfigure_with_persist(cfg, persist_studio_config)
    {
        let _ = snapshot.restore();
        return request_error(
            StatusCode::BAD_REQUEST,
            format!("MCP server could not be reloaded: {error}"),
        );
    }
    st.refresh_snapshot(&g, false).await;
    let models = g.runtime.catalog_models().await;
    {
        let mut snap = st.snap.write().await;
        snap.models = models;
    }
    Json(serde_json::json!({"ok":true,"id":id,"entry":entry,"mcp":studio::mcp_rows(g.runtime.mcp.status())})).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StudioProviderReq {
    id: String,
    kind: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    confirm: bool,
}

fn profile_update(cfg: &Config, req: StudioProviderReq) -> Result<Config> {
    anyhow::ensure!(
        crate::config::valid_profile_id(&req.id),
        "invalid profile id"
    );
    anyhow::ensure!(
        !req.models.is_empty() && req.models.len() <= 16,
        "provide between 1 and 16 model ids"
    );
    let mut next = cfg.clone();
    let existing = next
        .provider_profiles
        .iter()
        .find(|profile| profile.id == req.id)
        .cloned();
    let mut profile = existing.clone().unwrap_or_default();
    profile.id = req.id;
    profile.kind = req.kind.trim().to_ascii_lowercase();
    profile.models = req.models;
    profile.base_url = req.base_url.filter(|value| !value.trim().is_empty());
    profile.api_key_env = req.api_key_env.filter(|value| !value.trim().is_empty());
    if let Some(auth) = req.auth.filter(|value| !value.trim().is_empty()) {
        profile.auth = auth;
    }
    profile.project = req.project.filter(|value| !value.trim().is_empty());
    profile.location = req.location.filter(|value| !value.trim().is_empty());
    if let Some(index) = next
        .provider_profiles
        .iter()
        .position(|item| item.id == profile.id)
    {
        next.provider_profiles[index] = profile;
    } else {
        anyhow::ensure!(
            next.provider_profiles.len() < 64,
            "provider profile limit reached"
        );
        next.provider_profiles.push(profile);
    }
    next.validate()?;
    Ok(next)
}

fn persist_studio_config(next: &Config) -> Result<()> {
    let path = Config::config_path();
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        anyhow::ensure!(
            !meta.file_type().is_symlink() && meta.is_file() && meta.len() <= 512 * 1024,
            "invalid configuration file"
        );
    }
    std::fs::create_dir_all(Config::home_dir())?;
    let _lock =
        crate::control_bus::FileLock::acquire(Config::home_dir().join(".studio-config.lock"))?;
    let saved: Config = if path.exists() {
        toml::from_str(&std::fs::read_to_string(&path)?)?
    } else {
        Config::default()
    };
    merge_studio_config(saved, next).save()
}

/// Merge the runtime config's studio-owned fields into the on-disk config.
/// `mcp_config` is deliberately never copied: a workspace path (or a
/// studio-added server) stays scoped to this process, not to every later
/// session the user starts from another directory.
fn merge_studio_config(mut saved: Config, next: &Config) -> Config {
    let old = std::mem::take(&mut saved.provider_profiles);
    saved.provider_profiles = next
        .provider_profiles
        .iter()
        .map(|profile| {
            let mut safe = profile.clone();
            safe.api_key = old
                .iter()
                .find(|previous| previous.id == profile.id)
                .and_then(|previous| previous.api_key.clone());
            safe
        })
        .collect();
    saved
}

async fn api_studio_provider(
    State(st): State<AppState>,
    Json(req): Json<StudioProviderReq>,
) -> Response {
    if !req.confirm {
        return request_error(
            StatusCode::FORBIDDEN,
            "studio writes require explicit confirmation (send confirm: true)",
        );
    }
    let Ok(mut g) = st.inner.try_lock() else {
        return request_error(
            StatusCode::CONFLICT,
            "runtime busy; retry after the active turn",
        );
    };
    let id = req.id.clone();
    let cfg = match profile_update(&g.runtime.cfg, req) {
        Ok(cfg) => cfg,
        Err(error) => return request_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    if g.runtime
        .reconfigure_with_persist(cfg, persist_studio_config)
        .is_err()
    {
        return request_error(
            StatusCode::BAD_REQUEST,
            "profile could not be validated or saved; current runtime is unchanged",
        );
    }
    let models = g.runtime.catalog_models().await;
    {
        let mut snap = st.snap.write().await;
        snap.models = models;
    }
    st.refresh_snapshot(&g, false).await;
    Json(serde_json::json!({"ok":true,"profile":id})).into_response()
}

#[derive(Debug, Deserialize)]
struct StudioSkillReq {
    source: String,
    #[serde(default)]
    confirm: bool,
}

async fn api_studio_skill(State(st): State<AppState>, Json(req): Json<StudioSkillReq>) -> Response {
    if !req.confirm {
        return request_error(
            StatusCode::FORBIDDEN,
            "studio writes require explicit confirmation (send confirm: true)",
        );
    }
    let source = req.source.trim();
    if source.is_empty() || source.len() > 512 || source.chars().any(char::is_control) {
        return request_error(StatusCode::BAD_REQUEST, "invalid skill source");
    }
    let resolved = match crate::skills_search::resolve_source(source) {
        Ok(value) => value,
        Err(_) => return request_error(StatusCode::BAD_REQUEST, "invalid skill source"),
    };
    let dir = Config::home_dir().join("skills");
    let Ok(mut g) = st.inner.try_lock() else {
        return request_error(
            StatusCode::CONFLICT,
            "runtime busy; retry after the active turn",
        );
    };
    let result = match tokio::task::spawn_blocking(move || {
        crate::skills_search::install_to(&resolved, &dir, false)
    })
    .await
    {
        Ok(Ok(report)) => report,
        _ => return request_error(StatusCode::BAD_REQUEST, "skill installation failed"),
    };
    g.runtime.refresh_skills();
    st.refresh_snapshot(&g, false).await;
    Json(serde_json::json!({"ok":true,"skill":result})).into_response()
}
#[derive(Debug, Deserialize)]
struct PluginActionReq {
    action: String,
    id: String,
    #[serde(default)]
    confirm: bool,
}

async fn api_studio_plugin(
    State(st): State<AppState>,
    Json(req): Json<PluginActionReq>,
) -> Response {
    if !req.confirm {
        return request_error(
            StatusCode::FORBIDDEN,
            "studio writes require explicit confirmation (send confirm: true)",
        );
    }
    let action = req.action.trim().to_ascii_lowercase();
    if !matches!(action.as_str(), "install" | "remove") {
        return request_error(StatusCode::BAD_REQUEST, "action must be install or remove");
    }
    let id = match studio::validate_identifier(&req.id, "plugin id") {
        Ok(id) => id,
        Err(error) => return request_error(StatusCode::BAD_REQUEST, error),
    };
    let Ok(mut g) = st.inner.try_lock() else {
        return request_error(
            StatusCode::CONFLICT,
            "runtime busy; retry after the active turn",
        );
    };
    let cwd = g.runtime.jail.cwd.clone();
    // Installing a plugin writes `.varynth/plugins` and, for most plugins, a
    // `.varynth/mcp.json` entry; both are repository policy and only take
    // effect in a trusted workspace. Refuse before writing anything.
    if action == "install" && !crate::trust::is_trusted(&cwd) {
        return request_error(StatusCode::FORBIDDEN, untrusted_workspace_error(&cwd));
    }
    let message = match crate::plugins::handle_command(&format!("{action} {id}"), &cwd) {
        Ok(message) => message,
        Err(_) => return request_error(StatusCode::BAD_REQUEST, "plugin action failed"),
    };
    let mcp_path = cwd.join(".varynth/mcp.json");
    // Never reload a workspace registry in an untrusted checkout: the runtime
    // was started without it, and reloading would hand the repository the
    // stdio servers `main` deliberately refused.
    if !crate::trust::is_trusted(&cwd) {
        g.runtime.refresh_skills();
    } else if mcp_path.is_file() {
        let mut cfg = g.runtime.cfg.clone();
        cfg.mcp_config = Some(mcp_path);
        if let Err(error) = g
            .runtime
            .reconfigure_with_persist(cfg, persist_studio_config)
        {
            return request_error(
                StatusCode::BAD_REQUEST,
                format!("plugin MCP registry could not be reloaded: {error}"),
            );
        }
    } else {
        g.runtime.refresh_skills();
    }
    st.refresh_snapshot(&g, false).await;
    Json(serde_json::json!({
        "ok": true,
        "action": action,
        "id": id,
        "message": message,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeReq {
    #[serde(default)]
    tokens: serde_json::Value,
    #[serde(default)]
    text: Option<String>,
}

fn theme_payload(tokens: serde_json::Value) -> serde_json::Value {
    let tokens = tokens
        .as_object()
        .into_iter()
        .flat_map(|tokens| tokens.iter())
        .map(|(name, value)| (format!("--{name}"), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({"ok":true,"tokens":tokens})
}

async fn api_theme() -> Response {
    match tokio::task::spawn_blocking(crate::theme_tokens::load_theme).await {
        Ok(Ok(tokens)) => Json(theme_payload(tokens)).into_response(),
        _ => request_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "theme storage unavailable",
        ),
    }
}

async fn api_theme_update(Json(req): Json<ThemeReq>) -> Response {
    let tokens = match req.text {
        Some(text) => crate::theme_tokens::parse_theme_text(&text),
        None => crate::theme_tokens::validate_theme_json(&req.tokens),
    };
    let tokens = match tokens {
        Ok(tokens) => tokens,
        Err(_) => {
            return request_error(
                StatusCode::BAD_REQUEST,
                "invalid theme tokens or unsupported CSS values",
            )
        }
    };
    let saved = tokens.clone();
    match tokio::task::spawn_blocking(move || crate::theme_tokens::save_theme(&saved)).await {
        Ok(Ok(())) => Json(theme_payload(tokens)).into_response(),
        _ => request_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "theme storage unavailable",
        ),
    }
}

async fn api_chat_stream(
    State(st): State<AppState>,
    Json(req): Json<ChatReq>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    tokio::spawn(async move {
        let mut g = match st.inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                let _ = tx.send(AgentEvent {
                    kind: "error".into(),
                    text: "runtime busy; retry after the active turn".into(),
                });
                return;
            }
        };
        if let Err((_, text)) = prepare_chat(&mut g, &req).await {
            let _ = tx.send(AgentEvent {
                kind: "error".into(),
                text,
            });
            return;
        }
        st.refresh_snapshot(&g, true).await;
        let sid = g.session.id().to_string();
        let tx_ev = tx.clone();
        let ev_tx = st.events.clone();
        let mut next = chat_prompt(&req).to_string();
        let mut last_ok: Option<String> = None;
        let mut last_err: Option<String> = None;
        loop {
            let Inner { runtime, session } = &mut *g;
            let progress = turn_progress(runtime, session);
            let result = runtime
                .turn(session, &next, |ev| {
                    let _ = tx_ev.send(ev.clone());
                    broadcast_event(&ev_tx, &sid, &ev);
                })
                .await;
            let cont = runtime.wants_goal_continue();
            match result {
                Ok(reply) => {
                    last_ok = Some(reply);
                    if !cont || turn_progress(runtime, session) == progress {
                        break;
                    }
                    let _ = tx.send(AgentEvent {
                        kind: "system".into(),
                        text: "goal still active — continuing".into(),
                    });
                    next = "Goal still active. Continue uninterrupted. Do not ask what to do. End with a line that is exactly GOAL_COMPLETE only when the condition is fully met.".into();
                }
                Err(e) => {
                    last_err = Some(e.to_string());
                    break;
                }
            }
        }
        st.refresh_snapshot(&g, false).await;
        if let Some(e) = last_err {
            let _ = tx.send(AgentEvent {
                kind: "error".into(),
                text: e,
            });
        } else if let Some(reply) = last_ok {
            let _ = tx.send(AgentEvent {
                kind: "done".into(),
                text: reply,
            });
            let _ = tx.send(AgentEvent {
                kind: "session".into(),
                text: sid,
            });
        }
    });
    let stream = UnboundedReceiverStream::new(rx).map(|ev| {
        Ok(Event::default()
            .event(ev.kind.clone())
            .json_data(&ev)
            .unwrap_or_else(|_| Event::default().data(ev.text)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn api_chat(State(st): State<AppState>, Json(req): Json<ChatReq>) -> Response {
    let Ok(mut g) = st.inner.try_lock() else {
        return request_error(
            StatusCode::CONFLICT,
            "runtime busy; retry after the active turn",
        );
    };
    if let Err((status, message)) = prepare_chat(&mut g, &req).await {
        return request_error(status, message);
    }
    st.refresh_snapshot(&g, true).await;
    let mut events = Vec::new();
    let Inner { runtime, session } = &mut *g;
    let sid = session.id().to_string();
    let tx = st.events.clone();
    let result = runtime
        .turn(session, chat_prompt(&req), |ev| {
            if ev.kind != "image" {
                events.push(format!("{}: {}", ev.kind, ev.text));
            }
            broadcast_event(&tx, &sid, &ev);
        })
        .await;
    st.refresh_snapshot(&g, false).await;
    match result {
        Ok(reply) => Json(serde_json::json!({
            "ok": true,
            "session_id": sid,
            "reply": reply,
            "events": events,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "ok": false,
                "error": e.to_string(),
                "events": events,
            })),
        )
            .into_response(),
    }
}

fn completion_error(status: StatusCode, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {"message": message, "type": "varynth_error"}
        })),
    )
        .into_response()
}

async fn v1_chat_completions(
    State(st): State<AppState>,
    Json(req): Json<CompletionReq>,
) -> Response {
    let Some(prompt) = render_completion_prompt(&req.messages) else {
        return completion_error(
            StatusCode::BAD_REQUEST,
            "messages must contain at least one non-system message".into(),
        );
    };
    let mut g = st.inner.lock().await;
    let chat = ChatReq {
        message: prompt.clone(),
        session_id: None,
        model: req.model.clone(),
        media_ids: Vec::new(),
    };
    if let Err((status, message)) = prepare_chat(&mut g, &chat).await {
        return completion_error(status, message);
    }
    let cwd = g.runtime.jail.cwd.display().to_string();
    let model = g.runtime.cfg.model.clone();
    let result = match Session::new(&cwd, &model) {
        Ok(mut session) => {
            g.runtime.reset_session_state();
            {
                let mut snap = st.snap.write().await;
                snap.status = g.runtime.status_json(Some(&session));
                snap.busy = true;
            }
            let sid = session.id().to_string();
            let tx = st.events.clone();
            let result = g
                .runtime
                .turn(&mut session, &prompt, |ev| broadcast_event(&tx, &sid, &ev))
                .await;
            g.runtime.reset_session_state();
            result
        }
        Err(e) => Err(e),
    };
    st.refresh_snapshot(&g, false).await;
    match result {
        Ok(reply) => Json(serde_json::json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": req.model.unwrap_or(model),
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": reply},
                "finish_reason": "stop"
            }]
        }))
        .into_response(),
        Err(e) => completion_error(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[test]
    fn session_installation_resets_only_for_new_or_switched_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let mut current =
            Session::create_at("first".into(), dir.path().join("first.jsonl"), &cwd, "mock")
                .unwrap();
        let mut resets = 0;
        install_session(
            &mut current,
            Session::create_at("new".into(), dir.path().join("new.jsonl"), &cwd, "mock").unwrap(),
            || resets += 1,
        );
        assert_eq!(current.id(), "new");
        assert_eq!(resets, 1);
        let same = current.clone();
        install_session(&mut current, same, || resets += 1);
        assert_eq!(resets, 1);
        let switched =
            Session::create_at("saved".into(), dir.path().join("saved.jsonl"), &cwd, "mock")
                .unwrap();
        install_session(&mut current, switched, || resets += 1);
        assert_eq!(current.id(), "saved");
        assert_eq!(resets, 2);
    }

    #[test]
    fn workspace_check_accepts_alias_and_rejects_other_or_missing_workspace() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        assert!(
            check_workspace(first.path(), &first.path().join(".").display().to_string()).is_ok()
        );
        let err = check_workspace(first.path(), &second.path().display().to_string()).unwrap_err();
        assert!(err.to_string().contains("workspace mismatch"));
        assert!(check_workspace(
            first.path(),
            &first.path().join("missing").display().to_string()
        )
        .is_err());
    }

    #[tokio::test]
    async fn asset_routes_embed_exact_whitelist_with_mime_and_reject_unknown() {
        let app = Router::new()
            .route("/assets/{name}", get(public_asset))
            .route("/favicon.svg", get(favicon));
        for (name, mime) in [
            ("control-room.css", "text/css"),
            ("core.mjs", "text/javascript"),
            ("control-room.mjs", "text/javascript"),
            ("diff.mjs", "text/javascript"),
            ("mark.svg", "image/svg+xml"),
            ("icons.svg", "image/svg+xml"),
            ("geist-latin.woff2", "font/woff2"),
            ("GEIST-LICENSE", "text/plain"),
            ("LUCIDE-LICENSE", "text/plain"),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/assets/{name}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{name}");
            assert_eq!(res.headers()[axum::http::header::CONTENT_TYPE], mime);
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(bytes.as_ref(), embedded_asset(name).unwrap().1);
            assert!(!bytes.is_empty(), "{name}");
        }
        for name in ["unknown.css", "README.md", "..", "mark.svg.bak"] {
            assert!(embedded_asset(name).is_none());
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/assets/{name}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND);
        }
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/favicon.svg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()[axum::http::header::CONTENT_TYPE],
            "image/svg+xml"
        );
    }

    #[test]
    fn token_compare_is_exact_and_length_sensitive() {
        assert!(tokens_equal("secret", "secret"));
        assert!(tokens_equal("", ""));
        assert!(!tokens_equal("secret", "secreT"));
        assert!(!tokens_equal("secret", "secret "));
        assert!(!tokens_equal("secret", "secrets"));
        assert!(!tokens_equal("", "secret"));
    }

    fn guarded_app(token: Option<&str>) -> Router {
        Router::new()
            .route("/api/probe", get(|| async { "ok" }))
            .route("/api/mutate", post(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                AuthGuard {
                    token: token.map(str::to_string),
                },
                require_dashboard_auth,
            ))
    }

    fn request(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn loopback_requests_require_a_token() {
        let app = guarded_app(Some("loopback-token"));
        // No token, even on the loopback host.
        let res = app
            .clone()
            .oneshot(request("GET", "/api/probe", &[("host", "127.0.0.1:7420")]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        // Wrong token.
        let res = app
            .clone()
            .oneshot(request(
                "GET",
                "/api/probe",
                &[
                    ("host", "127.0.0.1:7420"),
                    ("authorization", "Bearer wrong"),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        // The auto-generated token opens the route.
        let res = app
            .oneshot(request(
                "GET",
                "/api/probe",
                &[
                    ("host", "127.0.0.1:7420"),
                    ("authorization", "Bearer loopback-token"),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // A missing token is fail-closed, not tokenless loopback.
        let app = guarded_app(None);
        let res = app
            .oneshot(request("GET", "/api/probe", &[("host", "127.0.0.1:7420")]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn state_changing_routes_require_matching_origin() {
        let app = guarded_app(Some("loopback-token"));
        let auth = ("authorization", "Bearer loopback-token");
        let host = ("host", "127.0.0.1:7420");

        // Missing Origin and Sec-Fetch-Site.
        let res = app
            .clone()
            .oneshot(request("POST", "/api/mutate", &[host, auth]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Foreign Origin.
        let res = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/mutate",
                &[host, auth, ("origin", "http://evil.example")],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Browsers mark cross-site requests explicitly.
        let res = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/mutate",
                &[host, auth, ("sec-fetch-site", "cross-site")],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Same-origin browser POST.
        let res = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/mutate",
                &[host, auth, ("origin", "http://127.0.0.1:7420")],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .oneshot(request(
                "POST",
                "/api/mutate",
                &[host, auth, ("sec-fetch-site", "same-origin")],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn shared_secret_file_is_created_0600_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.token");
        let first = load_or_create_token(&path).unwrap();
        assert!(valid_token_shape(&first), "{first}");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), first);
        assert_eq!(load_or_create_token(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A malformed file is replaced rather than trusted.
        std::fs::write(&path, "short").unwrap();
        let replaced = load_or_create_token(&path).unwrap();
        assert_ne!(replaced, "short");
        assert!(valid_token_shape(&replaced));
    }

    #[test]
    fn dashboard_token_is_ephemeral_and_never_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("dashboard.token");
        let legacy_value = "persisted-token-from-an-older-build-0123456789";
        std::fs::write(&legacy, legacy_value).unwrap();

        let cfg = Config::default();
        let first = resolve_dashboard_token(&cfg).unwrap();
        let second = resolve_dashboard_token(&cfg).unwrap();
        assert!(valid_token_shape(&first), "{first}");
        assert_ne!(first, legacy_value, "a persisted token is never reused");
        assert_ne!(first, second, "each process gets its own token");
        assert_eq!(
            std::fs::read_to_string(&legacy).unwrap().trim(),
            legacy_value,
            "resolution never reads or rewrites the token file"
        );

        // `serve` retires the file so a sandboxed child cannot replay it.
        retire_legacy_token(&legacy);
        assert!(!legacy.exists());

        // An explicitly configured token is used verbatim and is never printed
        // by the resolver.
        let configured = Config {
            dashboard_token: Some("configured-token".into()),
            ..Config::default()
        };
        assert_eq!(
            resolve_dashboard_token(&configured).unwrap(),
            "configured-token"
        );
        let blank = Config {
            dashboard_token: Some("   ".into()),
            ..Config::default()
        };
        assert!(valid_token_shape(&resolve_dashboard_token(&blank).unwrap()));
    }

    #[tokio::test]
    async fn autocompact_via_dashboard_does_not_persist_ephemeral_token() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let _env = crate::config::lock_process_env();
        let previous = std::env::var_os("VARYNTH_HOME");
        std::env::set_var("VARYNTH_HOME", home.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("VARYNTH_HOME", value),
                    None => std::env::remove_var("VARYNTH_HOME"),
                }
            }
        }
        let _restore = Restore(previous);

        let mut cfg = Config::default();
        cfg.provider = "varynth".into();
        cfg.use_keyring = Some(false);
        let token = install_process_dashboard_token(&mut cfg).unwrap();
        assert!(cfg.dashboard_token_ephemeral);
        assert_eq!(cfg.dashboard_token.as_deref(), Some(token.as_str()));

        let runtime = Runtime::new(cfg.clone(), workspace.path().to_path_buf()).unwrap();
        let session =
            Session::new(&workspace.path().display().to_string(), &runtime.cfg.model).unwrap();
        let (events, _) = broadcast::channel(8);
        let state = AppState {
            inner: Arc::new(Mutex::new(Inner { runtime, session })),
            snap: Arc::new(RwLock::new(Snapshot::default())),
            dashboard_token: Some(token.clone()),
            allowed_hosts: Arc::new(vec!["127.0.0.1".into()]),
            events,
            cfg: Arc::new(cfg),
            serve_host: Arc::new("127.0.0.1".into()),
            serve_port: 7420,
            started: std::time::Instant::now(),
            started_at: chrono::Utc::now(),
            decisions_cache: Arc::new(Mutex::new(None)),
            relay_events: None,
        };
        let reply = state.turn("/autocompact off").await.unwrap();
        assert!(reply.contains("off"), "{reply}");
        let raw = std::fs::read_to_string(Config::config_path()).unwrap();
        assert!(
            raw.contains("auto_compact = false"),
            "autocompact did not save:\n{raw}"
        );
        assert!(!raw.contains("dashboard_token"), "{raw}");
        assert!(!raw.contains(&token), "{raw}");
    }

    #[test]
    fn dashboard_banner_prints_the_token_only_for_a_terminal() {
        let token = "ephemeral-token-value-0123456789abcdef";
        let lan = Some("http://192.0.2.10:7420/");

        let tty = dashboard_banner("127.0.0.1:7420", lan, token, false, true);
        assert!(
            tty.iter()
                .any(|line| line.contains(&format!("?token={token}"))),
            "{tty:?}"
        );
        assert!(tty
            .iter()
            .any(|line| line.contains("192.0.2.10") && line.contains(token)));

        // Redirected stderr: no secret anywhere in the banner.
        let quiet = dashboard_banner("127.0.0.1:7420", lan, token, false, false);
        assert!(quiet.iter().all(|line| !line.contains(token)), "{quiet:?}");
        assert!(
            quiet.iter().all(|line| !line.contains("?token=")),
            "{quiet:?}"
        );
        assert!(
            quiet.iter().any(|line| line.contains("ephemeral")),
            "{quiet:?}"
        );

        // A configured token is equally never echoed into a log.
        let configured = dashboard_banner("127.0.0.1:7420", None, token, true, false);
        assert!(
            configured.iter().all(|line| !line.contains(token)),
            "{configured:?}"
        );
        assert!(configured
            .iter()
            .any(|line| line.contains("configured dashboard token")));
    }

    #[test]
    fn studio_refuses_an_untrusted_workspace_registry() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let workspace = cwd.join(".varynth").join("mcp.json");
        std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
        std::fs::write(
            &workspace,
            r#"{"mcpServers":{"pre":{"type":"stdio","command":"cmd","args":["/c","calc"]}}}"#,
        )
        .unwrap();

        let error = ensure_registry_loadable(&workspace, cwd, false).unwrap_err();
        assert!(error.to_string().contains("not trusted"), "{error}");
        assert!(ensure_registry_loadable(&workspace, cwd, true).is_ok());
        // Equivalent spellings resolve to the same repository file.
        let decorated = cwd
            .join(".varynth")
            .join("..")
            .join(".varynth")
            .join("mcp.json");
        assert!(ensure_registry_loadable(&decorated, cwd, false).is_err());
        // A user-level registry is the user's own opt-in, not repo content.
        let user_registry = dir.path().join("home").join("mcp.json");
        assert!(ensure_registry_loadable(&user_registry, cwd, false).is_ok());
    }

    #[test]
    fn studio_api_refuses_stdio_entries() {
        assert!(entry_spawns_host_process(&serde_json::json!({
            "type": "stdio", "command": "cmd", "args": ["/c", "calc"]
        })));
        // Untyped entries with a command are stdio by inference.
        assert!(entry_spawns_host_process(&serde_json::json!({
            "command": "npx", "args": ["-y", "some-server"]
        })));
        assert!(entry_spawns_host_process(&serde_json::json!({
            "type": "STDIO", "command": "python"
        })));
        assert!(!entry_spawns_host_process(&serde_json::json!({
            "type": "http", "url": "https://example.test/mcp"
        })));
        assert!(!entry_spawns_host_process(&serde_json::json!({
            "url": "https://example.test/mcp"
        })));
    }

    #[test]
    fn studio_save_never_persists_a_workspace_mcp_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join(".varynth/mcp.json");
        let next = Config {
            mcp_config: Some(workspace.clone()),
            ..Config::default()
        };
        let merged = merge_studio_config(Config::default(), &next);
        assert!(merged.mcp_config.is_none());
        let rendered = toml::to_string_pretty(&merged.redacted_for_save()).unwrap();
        assert!(!rendered.contains("mcp_config"), "{rendered}");
        assert!(!rendered.contains(&workspace.display().to_string()));

        // A pre-existing on-disk value is left untouched, not rewritten with
        // the workspace path.
        let existing = Config {
            mcp_config: Some(PathBuf::from("/opt/shared/mcp.json")),
            ..Config::default()
        };
        let merged = merge_studio_config(existing, &next);
        assert_eq!(
            merged.mcp_config.as_deref(),
            Some(FsPath::new("/opt/shared/mcp.json"))
        );
    }

    #[test]
    fn event_frame_shape_and_timestamp() {
        let ev = AgentEvent {
            kind: "tool".into(),
            text: "ls -la".into(),
        };
        let frame: serde_json::Value = serde_json::from_str(&event_frame("sess-1", &ev)).unwrap();
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["session"], "sess-1");
        assert_eq!(frame["event"]["kind"], "tool");
        assert_eq!(frame["event"]["text"], "ls -la");
        chrono::DateTime::parse_from_rfc3339(frame["ts"].as_str().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn broadcast_event_fans_out_to_every_subscriber() {
        let (tx, _) = broadcast::channel(256);
        let mut first = tx.subscribe();
        let mut second = tx.subscribe();
        let ev = AgentEvent {
            kind: "assistant".into(),
            text: "hi".into(),
        };
        broadcast_event(&tx, "s-9", &ev);
        for rx in [&mut first, &mut second] {
            let frame: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(frame["type"], "event");
            assert_eq!(frame["session"], "s-9");
            assert_eq!(frame["event"]["text"], "hi");
        }
        // With no subscriber attached the send errors and is ignored.
        let (lonely, _dropped) = broadcast::channel::<String>(4);
        broadcast_event(&lonely, "s-9", &ev);
    }

    #[test]
    fn chat_fanout_hook_keeps_inner_sink_and_lands_frames() {
        // The exact hook shape /api/chat installs: keep the chat log AND
        // broadcast each event. A subscribed receiver sees one JSON frame
        // per event; the inner sink behavior is untouched.
        let (tx, mut rx) = broadcast::channel::<String>(16);
        let sid = "sess-42".to_string();
        let mut events: Vec<String> = Vec::new();
        let mut hook = |ev: AgentEvent| {
            events.push(format!("{}: {}", ev.kind, ev.text));
            broadcast_event(&tx, &sid, &ev);
        };
        hook(AgentEvent {
            kind: "tool".into(),
            text: "ls".into(),
        });
        hook(AgentEvent {
            kind: "assistant".into(),
            text: "done".into(),
        });
        assert_eq!(
            events,
            vec!["tool: ls".to_string(), "assistant: done".to_string()]
        );
        let f1: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(f1["type"], "event");
        assert_eq!(f1["session"], "sess-42");
        assert_eq!(f1["event"]["kind"], "tool");
        assert_eq!(f1["event"]["text"], "ls");
        let f2: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(f2["event"]["kind"], "assistant");
        assert_eq!(f2["event"]["text"], "done");
    }

    #[test]
    fn payload_helpers_return_sane_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let bus = crate::mailbox::Bus::at(dir.path().join("bus.jsonl"));

        // Session list: one meta row gains an `unread` counter.
        let meta = SessionMeta {
            id: "sess-a".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cwd: dir.path().display().to_string(),
            model: "mock".into(),
            title: "temp".into(),
            title_explicit: false,
            parent: None,
        };
        let v = sessions_value(vec![meta], &bus);
        assert_eq!(v["ok"], true);
        let arr = v["sessions"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "sess-a");
        assert_eq!(arr[0]["unread"], 0);

        // Single session: meta + messages + unread, written to a tempdir
        // session file — never the real home directory.
        let s = Session::create_at(
            "sess-b".into(),
            dir.path().join("sess-b.jsonl"),
            &dir.path().display().to_string(),
            "mock",
        )
        .unwrap();
        let v = session_value(s, "sess-b", &bus);
        assert_eq!(v["ok"], true);
        assert_eq!(v["meta"]["id"], "sess-b");
        assert_eq!(v["messages"], serde_json::json!([]));
        assert_eq!(v["unread"], 0);

        // Status: busy is overlaid, existing keys survive, false stays false.
        let v = overlay_busy(serde_json::json!({"model": "m", "session": "s"}), true);
        assert_eq!(v["busy"], true);
        assert_eq!(v["model"], "m");
        assert_eq!(
            overlay_busy(serde_json::json!({"model": "m"}), false)["busy"],
            false
        );
    }

    #[test]
    fn pending_approvals_skips_stale_and_response_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = serde_json::json!({
            "request": {"id": "req-fresh", "tool": "bash", "detail": "git status"},
            "created_at": chrono::Utc::now(),
        });
        let stale = serde_json::json!({
            "request": {"id": "req-stale", "tool": "bash", "detail": "old"},
            "created_at": chrono::Utc::now() - chrono::Duration::seconds(200),
        });
        std::fs::write(
            dir.path().join("req-fresh.json"),
            serde_json::to_vec(&fresh).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("req-stale.json"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("req-fresh.response.json"), br#""once""#).unwrap();
        let pending = pending_approvals(dir.path());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["id"], "req-fresh");
        assert_eq!(pending[0]["tool"], "bash");
        assert_eq!(pending[0]["detail"], "git status");
        assert!(pending_approvals(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn limit_summary_reports_configured_ceilings_without_guessing() {
        let cfg = Config {
            max_tool_rounds: 24,
            max_tokens: Some(4096),
            goal_max_rounds: Some(8),
            goal_max_minutes: None,
            ..Config::default()
        };
        let limits = limit_summary(&cfg);
        assert_eq!(limits["max_tool_rounds"], 24);
        assert_eq!(limits["max_tokens"], 4096);
        assert_eq!(limits["goal_max_rounds"], 8);
        assert_eq!(limits["goal_max_minutes"], serde_json::Value::Null);
        assert_eq!(limits["auto_compact"]["enabled"], true);
        assert!(limits["auto_compact"]["threshold_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn session_export_fork_and_checkpoints_reject_bad_ids() {
        let app = Router::new()
            .route("/api/session/{id}/fork", post(api_session_fork))
            .route("/api/session/{id}/export", get(api_session_export))
            .route(
                "/api/session/{id}/checkpoints",
                get(api_session_checkpoints),
            );
        for (method, uri) in [
            ("POST", "/api/session/../../etc/fork"),
            ("GET", "/api/session/not-a-uuid/export?format=md"),
            ("GET", "/api/session/not-a-uuid/checkpoints"),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            // The traversal path may be normalized to a 404 by the router;
            // the UUID-shaped rejects must answer 400 from the handler.
            assert!(
                res.status() == StatusCode::BAD_REQUEST || res.status() == StatusCode::NOT_FOUND,
                "{method} {uri} -> {}",
                res.status()
            );
        }
        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/session/not-a-uuid/export?format=md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn approval_respond_validates_id_and_decision_shape() {
        let bad_id = api_approval_respond(
            Path("../escape".into()),
            Json(ApprovalDecision {
                decision: "once".into(),
            }),
        )
        .await;
        assert_eq!(bad_id.status(), StatusCode::BAD_REQUEST);
        let bad_vote = api_approval_respond(
            Path("req-1".into()),
            Json(ApprovalDecision {
                decision: "maybe".into(),
            }),
        )
        .await;
        assert_eq!(bad_vote.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn host_header_check_rejects_foreign_hosts() {
        let cfg = Config {
            dashboard_host: "127.0.0.1".into(),
            dashboard_port: 7420,
            ..Config::default()
        };
        let allowed = host_allowlist(&cfg);
        assert!(host_header_allowed("127.0.0.1:7420", &allowed));
        assert!(host_header_allowed("LOCALHOST:7420", &allowed));
        assert!(!host_header_allowed("evil.example:7420", &allowed));
        assert!(!host_header_allowed("127.0.0.1:9999", &allowed));
        assert!(!host_header_allowed("localhost", &allowed));
        assert!(!host_header_allowed("", &allowed));
    }

    #[tokio::test]
    async fn message_handler_rejects_missing_or_blank_text() {
        // The validation path returns before the bus is ever touched.
        for body in [
            serde_json::json!({}),
            serde_json::json!({"text": "   "}),
            serde_json::json!({"text": 42}),
        ] {
            let res = api_session_message(Path("s-x".into()), Json(body)).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["ok"], false);
            assert!(v["error"].as_str().unwrap().contains("text"));
        }
    }

    #[tokio::test]
    async fn messages_route_requires_dashboard_host() {
        let cfg = Config {
            dashboard_host: "127.0.0.1".into(),
            dashboard_port: 7420,
            ..Config::default()
        };
        let app = host_guard(
            Router::new().route("/api/session/{id}/messages", post(api_session_message)),
            Arc::new(host_allowlist(&cfg)),
        );

        // A foreign Host header is stopped by the guard: plain-text 400, and
        // the handler (which would answer with JSON) is never reached.
        let evil = Request::builder()
            .method("POST")
            .uri("/api/session/s-x/messages")
            .header("host", "evil.example")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"text":"hi"}"#))
            .unwrap();
        let res = app.clone().oneshot(evil).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(bytes.to_vec()).unwrap(),
            "invalid host header"
        );

        // The loopback host reaches the handler, which rejects the blank body
        // with a JSON validation error — proof the guard let it through. No
        // bus I/O happens on this path.
        let local = Request::builder()
            .method("POST")
            .uri("/api/session/s-x/messages")
            .header("host", "127.0.0.1:7420")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"text":"   "}"#))
            .unwrap();
        let res = app.oneshot(local).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("text"));
    }
}
