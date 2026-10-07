use anyhow::Result;
use axum::extract::{Path, Request, State};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{http::StatusCode, Json, Router};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::providers::CatalogModel;
use crate::runtime::{AgentEvent, Runtime};
use crate::session::{Session, SessionMeta};

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) status: serde_json::Value,
    pub(crate) models: Vec<CatalogModel>,
    pub(crate) busy: bool,
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
}

pub(crate) struct Inner {
    pub(crate) runtime: Runtime,
    pub(crate) session: Session,
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
        let mut g = self.inner.lock().await;
        self.refresh_snapshot(&g, true).await;
        let Inner { runtime, session } = &mut *g;
        let sid = session.id().to_string();
        let tx = self.events.clone();
        let result = runtime
            .turn(session, message, |ev| broadcast_event(&tx, &sid, &ev))
            .await;
        self.refresh_snapshot(&g, false).await;
        result
    }

    async fn refresh_snapshot(&self, inner: &Inner, busy: bool) {
        let mut snap = self.snap.write().await;
        snap.status = inner.runtime.status_json(Some(&inner.session));
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
    let is_loopback = matches!(
        cfg.dashboard_host.as_str(),
        "127.0.0.1" | "localhost" | "::1"
    );
    if !is_loopback && cfg.dashboard_token.is_none() {
        anyhow::bail!("remote dashboard bind requires VARYNTH_DASHBOARD_TOKEN");
    }
    tokio::spawn(crate::automation::run_loop(cfg.clone(), cwd.clone()));
    let mut runtime = Runtime::new(cfg.clone(), cwd.clone())?;
    // Remote approval relay: the runtime broadcasts RelayRequest slots onto
    // this channel; the gateway forwarder bridges them onto the event stream
    // so WebSocket clients (and the deck) can answer with approval.respond.
    let (relay_tx, relay_rx) = tokio::sync::broadcast::channel::<crate::approval_relay::RelayRequest>(
        crate::gateway::RELAY_CAPACITY,
    );
    runtime.remote_approval_events = Some(relay_tx);
    let session = Session::new(&cwd.display().to_string(), &runtime.cfg.model)?;
    let host = cfg.dashboard_host.clone();
    let port = cfg.dashboard_port;
    let models = runtime.list_models().await.unwrap_or_default();
    let status = runtime.status_json(Some(&session));
    let (events_tx, _) = broadcast::channel(crate::gateway::EVENT_CAPACITY);
    let state = AppState {
        inner: Arc::new(Mutex::new(Inner { runtime, session })),
        snap: Arc::new(RwLock::new(Snapshot {
            status,
            models,
            busy: false,
        })),
        dashboard_token: cfg.dashboard_token.clone(),
        allowed_hosts: Arc::new(host_allowlist(&cfg)),
        events: events_tx,
    };
    // The gateway shares the same state through an Arc; the dashboard
    // handlers keep taking AppState by value (it is a cheap Arc-bundle).
    let shared = Arc::new(state.clone());
    let protected = Router::new()
        .route("/api/status", get(api_status))
        .route("/api/models", get(api_models))
        .route("/api/sessions", get(api_sessions))
        .route("/api/session/new", post(api_session_new))
        .route("/api/session/{id}", get(api_session))
        .route("/api/session/{id}/messages", post(api_session_message))
        .route("/api/chat", post(api_chat))
        .route("/api/chat/stream", post(api_chat_stream))
        .route("/api/channels", get(api_channels))
        .route("/api/doctor", get(api_doctor))
        .route("/v1/chat/completions", post(v1_chat_completions))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_dashboard_auth,
        ));
    let app = host_guard(
        Router::new()
            .route("/", get(index))
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
    eprintln!("varynth dashboard → http://{addr}");
    if host == "0.0.0.0" || host == "::" {
        if let Ok(ip) = local_ipv4() {
            eprintln!("varynth dashboard LAN → http://{ip}:{port}");
        }
    }
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
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    origin.eq_ignore_ascii_case(&format!("http://{host}"))
        || origin.eq_ignore_ascii_case(&format!("https://{host}"))
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
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !same_origin(request.headers()) {
        return (StatusCode::FORBIDDEN, "foreign origin rejected").into_response();
    }
    let Some(expected) = state.dashboard_token.as_deref() else {
        return next.run(request).await;
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

async fn api_models(State(st): State<AppState>) -> impl IntoResponse {
    let snap = st.snap.read().await;
    if !snap.models.is_empty() {
        return Json(serde_json::json!({"ok": true, "models": snap.models}));
    }
    drop(snap);
    let Ok(g) = st.inner.try_lock() else {
        return Json(
            serde_json::json!({"ok": false, "error": "runtime busy; retry after the active turn"}),
        );
    };
    match g.runtime.list_models().await {
        Ok(m) => Json(serde_json::json!({"ok": true, "models": m})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
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

fn prepare_chat(inner: &mut Inner, req: &ChatReq) -> Result<(), (StatusCode, String)> {
    if req.message.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message is required".into()));
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
    if let Some(model) = model {
        if model.trim().is_empty() || model.len() > 512 {
            return Err((
                StatusCode::BAD_REQUEST,
                "model must be non-empty and at most 512 bytes".into(),
            ));
        }
        if model != inner.runtime.cfg.model {
            let mut cfg = inner.runtime.cfg.clone();
            cfg.model = model.into();
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
    Ok(())
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

async fn api_chat_stream(
    State(st): State<AppState>,
    Json(req): Json<ChatReq>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    tokio::spawn(async move {
        let mut g = st.inner.lock().await;
        if let Err((_, text)) = prepare_chat(&mut g, &req) {
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
        let mut next = req.message.clone();
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
    let mut g = st.inner.lock().await;
    if let Err((status, message)) = prepare_chat(&mut g, &req) {
        return request_error(status, message);
    }
    st.refresh_snapshot(&g, true).await;
    let mut events = Vec::new();
    let Inner { runtime, session } = &mut *g;
    let sid = session.id().to_string();
    let tx = st.events.clone();
    let result = runtime
        .turn(session, &req.message, |ev| {
            events.push(format!("{}: {}", ev.kind, ev.text));
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
    };
    if let Err((status, message)) = prepare_chat(&mut g, &chat) {
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
