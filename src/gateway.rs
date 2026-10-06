//! WebSocket agent gateway: a bidirectional JSON-RPC 2.0 control plane over
//! `GET /ws/gateway`.
//!
//! The dashboard broadcasts every agent turn event on
//! [`crate::dashboard::AppState::events`]; this module wraps those frames as
//! `{"jsonrpc":"2.0","method":"event","params":<frame>}` notifications and
//! fans them out to every connected client. Clients answer with JSON-RPC
//! requests (ping, status, sessions, models, remote approvals) that are
//! served by a pure dispatcher so the protocol is testable without sockets.
//!
//! Auth mirrors the dashboard: when `VARYNTH_DASHBOARD_TOKEN` is set the
//! token must arrive as `?token=` (browser WebSockets cannot set headers) or
//! as an `Authorization: Bearer` header, compared in constant time; when it
//! is unset any host-allowlisted client may connect, exactly like the
//! dashboard's own API today.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, Request, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::approval_relay::{self, ApprovalVote, RelayRequest};
use crate::dashboard::{AppState, Inner, Snapshot};
use crate::session::Session;

/// Capacity of the dashboard event broadcast every gateway client subscribes
/// to. Generous on purpose: a busy turn can emit hundreds of tool events.
pub const EVENT_CAPACITY: usize = 1024;

/// Capacity of the runtime's remote-approval relay broadcast that the
/// forwarder bridges onto the event stream.
pub const RELAY_CAPACITY: usize = 64;

/// Build the gateway router. Merged into the dashboard app under the existing
/// host guard; the WebSocket route carries its own token middleware because
/// browsers cannot attach `Authorization` headers to upgrade requests.
pub fn router(state: Arc<AppState>) -> Router {
    ws_router(Arc::new(GatewayCtx::from_state(&state)))
}

fn ws_router(ctx: Arc<GatewayCtx>) -> Router {
    Router::<Arc<GatewayCtx>>::new()
        .route("/ws/gateway", get(ws_gateway))
        .layer(middleware::from_fn_with_state(
            ctx.clone(),
            require_gateway_auth,
        ))
        .with_state(ctx)
}

/// The slice of [`AppState`] the gateway needs, plus the two knobs tests
/// override. `live` is `None` in detached (test) contexts: dispatcher methods
/// that would touch a real runtime answer with a JSON-RPC internal error
/// instead, so no test ever builds a live runtime or writes to `$HOME`.
pub struct GatewayCtx {
    pub(crate) snap: Arc<RwLock<Snapshot>>,
    pub(crate) live: Option<Arc<Mutex<Inner>>>,
    pub(crate) events: broadcast::Sender<String>,
    pub(crate) token: Option<String>,
    pub(crate) list_sessions: fn() -> Value,
}

impl GatewayCtx {
    pub fn from_state(state: &Arc<AppState>) -> Self {
        Self {
            snap: state.snap.clone(),
            live: Some(state.inner.clone()),
            events: state.events.clone(),
            token: state.dashboard_token.clone(),
            list_sessions: crate::dashboard::sessions_payload,
        }
    }

    #[cfg(test)]
    fn detached(status: Value, models: Vec<crate::providers::CatalogModel>) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            snap: Arc::new(RwLock::new(Snapshot {
                status,
                models,
                busy: false,
            })),
            live: None,
            events,
            token: Some("gateway-test-token".into()),
            list_sessions: || json!({ "ok": true, "sessions": [] }),
        }
    }
}

async fn ws_gateway(State(ctx): State<Arc<GatewayCtx>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| socket_loop(ctx, socket))
}

async fn require_gateway_auth(
    State(ctx): State<Arc<GatewayCtx>>,
    RawQuery(query): RawQuery,
    request: Request,
    next: Next,
) -> Response {
    if !crate::dashboard::same_origin(request.headers()) {
        return (StatusCode::FORBIDDEN, "foreign origin rejected").into_response();
    }
    if gateway_authorized(ctx.token.as_deref(), query.as_deref(), request.headers()) {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "gateway authentication required").into_response()
    }
}

/// Auth decision for one gateway request. Token unset → allow (loopback
/// behavior as today; remote binds without a token are rejected at startup).
fn gateway_authorized(expected: Option<&str>, query: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let provided = query_token(query).or_else(|| bearer_token(headers));
    provided.is_some_and(|provided| crate::dashboard::tokens_equal(&provided, expected))
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
}

fn query_token(query: Option<&str>) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| percent_decode(value))
    })
}

/// Minimal percent-decoding for one query value: `%XX` escapes only, so a
/// token containing a literal `+` survives untouched.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// One connected client. Inbound requests and broadcast events are served
/// from the same `select!` loop, so a slow socket only lags itself: event
/// producers push into the bounded broadcast queue and never wait on I/O.
async fn socket_loop(ctx: Arc<GatewayCtx>, mut socket: WebSocket) {
    let mut events = ctx.events.subscribe();
    loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    let reply = match serde_json::from_str::<Value>(&text) {
                        Ok(v) => handle_request(&ctx, v).await,
                        Err(e) => error_response(Value::Null, -32700, &format!("parse error: {e}")),
                    };
                    if socket
                        .send(Message::Text(reply.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            frame = events.recv() => match frame {
                Ok(raw) => {
                    let wrapped = wrap_event_frame(&raw).to_string();
                    if socket.send(Message::Text(wrapped.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    let note = json!({
                        "jsonrpc": "2.0",
                        "method": "event",
                        "params": {
                            "type": "lagged",
                            "missed": missed,
                            "text": format!("gateway event stream lagged; {missed} events skipped"),
                        },
                    })
                    .to_string();
                    if socket.send(Message::Text(note.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    // Politely close; dropping the socket would also end the connection.
    let _ = socket.send(Message::Close(None)).await;
}

/// Wraps a raw broadcast frame as a JSON-RPC notification:
/// `{"jsonrpc":"2.0","method":"event","params":<frame>}`.
pub fn wrap_event_frame(frame: &str) -> Value {
    let params = serde_json::from_str(frame).unwrap_or(Value::String(frame.to_string()));
    json!({ "jsonrpc": "2.0", "method": "event", "params": params })
}

/// Pure JSON-RPC 2.0 dispatcher: one request value in, one response envelope
/// out. It only sees the narrow [`GatewayCtx`] backends, so every protocol
/// branch is unit tested without sockets or a live runtime; the socket loop
/// feeds it the requests parsed off the wire.
pub async fn handle_request(ctx: &GatewayCtx, v: Value) -> Value {
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        return error_response(id, -32602, r#"request must carry a string "method""#);
    };
    let no_params = Value::Null;
    let params = v.get("params").unwrap_or(&no_params);
    match method {
        "ping" => success_response(id, json!("pong")),
        "status.get" => success_response(id, status_payload(ctx).await),
        "sessions.list" => success_response(id, (ctx.list_sessions)()),
        "session.get" => match str_param(params, "id") {
            Ok(id_) => success_response(id, session_payload(&id_)),
            Err(msg) => error_response(id, -32602, &msg),
        },
        "session.message" => match (str_param(params, "id"), str_param(params, "text")) {
            (Ok(id_), Ok(text)) if !text.trim().is_empty() => {
                success_response(id, send_session_message(&id_, &text))
            }
            (Err(msg), _) | (_, Err(msg)) => error_response(id, -32602, &msg),
            (Ok(_), Ok(_)) => error_response(id, -32602, "params.text must not be blank"),
        },
        "session.pause" | "session.resume" => match str_param(params, "id") {
            Ok(session) => {
                let paused = method == "session.pause";
                match crate::control_bus::ControlBus::default().set_paused(&session, paused) {
                    Ok(()) => success_response(
                        id,
                        json!({"ok": true, "session": session, "paused": paused}),
                    ),
                    Err(error) => error_response(id, -32602, &error.to_string()),
                }
            }
            Err(msg) => error_response(id, -32602, &msg),
        },
        "session.context" => match (str_param(params, "id"), params.get("messages")) {
            (Ok(session), Some(messages)) => match crate::control_bus::ControlBus::default()
                .replace_context(&session, messages)
            {
                Ok(()) => success_response(
                    id,
                    json!({"ok": true, "session": session, "applies": "next turn"}),
                ),
                Err(error) => error_response(id, -32602, &error.to_string()),
            },
            (Err(msg), _) => error_response(id, -32602, &msg),
            (_, None) => error_response(id, -32602, "params.messages is required"),
        },
        "models.list" => success_response(id, models_payload(ctx).await),
        "model.set" => match str_param(params, "id") {
            Ok(model) => match set_model(ctx, &model).await {
                Ok(()) => success_response(id, json!({ "ok": true, "model": model })),
                Err(msg) => error_response(id, -32603, &msg),
            },
            Err(msg) => error_response(id, -32602, &msg),
        },
        "approval.respond" => match (str_param(params, "id"), str_param(params, "decision")) {
            (Ok(id_), Ok(decision)) => match ApprovalVote::parse(&decision) {
                Some(vote) => success_response(id, json!(approval_relay::respond(&id_, vote))),
                None => error_response(id, -32602, "params.decision must be once, always, or deny"),
            },
            (Err(msg), _) | (_, Err(msg)) => error_response(id, -32602, &msg),
        },
        other => error_response(id, -32601, &format!("method not found: {other}")),
    }
}

fn str_param(params: &Value, key: &str) -> Result<String, String> {
    match params.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(format!("params.{key} must be a non-empty string")),
        Some(_) => Err(format!("params.{key} must be a string")),
        None => Err(format!("params.{key} is required")),
    }
}

async fn status_payload(ctx: &GatewayCtx) -> Value {
    {
        let snap = ctx.snap.read().await;
        let mut status = snap.status.clone();
        if !status.is_null() {
            if let Some(obj) = status.as_object_mut() {
                obj.insert("busy".into(), json!(snap.busy));
            }
            return status;
        }
    }
    match &ctx.live {
        Some(inner) => {
            let g = inner.lock().await;
            g.runtime.status_json(Some(&g.session))
        }
        None => json!({}),
    }
}

async fn models_payload(ctx: &GatewayCtx) -> Value {
    {
        let snap = ctx.snap.read().await;
        if !snap.models.is_empty() {
            return json!({ "ok": true, "models": snap.models });
        }
    }
    match &ctx.live {
        Some(inner) => {
            let g = inner.lock().await;
            match g.runtime.list_models().await {
                Ok(models) => json!({ "ok": true, "models": models }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        None => json!({ "ok": true, "models": [] }),
    }
}

fn session_payload(id: &str) -> Value {
    match Session::load(id) {
        Ok(s) => json!({
            "ok": true,
            "meta": s.meta,
            "messages": s.messages,
            "unread": crate::mailbox::Bus::default().unread(id).len(),
        }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

fn send_session_message(id: &str, text: &str) -> Value {
    match crate::mailbox::Bus::default().send("gateway", id, text) {
        Ok(_) => json!({ "ok": true, "to": id }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

async fn set_model(ctx: &GatewayCtx, model: &str) -> Result<(), String> {
    let Some(inner) = &ctx.live else {
        return Err("gateway has no live runtime".into());
    };
    let mut g = inner.lock().await;
    let mut cfg = g.runtime.cfg.clone();
    cfg.model = model.to_string();
    g.runtime
        .reconfigure(cfg)
        .map_err(|error| error.to_string())?;
    let status = g.runtime.status_json(Some(&g.session));
    drop(g);
    let mut snap = ctx.snap.write().await;
    snap.status = status;
    Ok(())
}

pub fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Fan-out frame for one remote approval request. The relay id travels both
/// as a structured field (`event.id`) and as an `[id:<uuid>]` token in the
/// text, so thin clients can scrape whichever is easier.
pub fn approval_relay_frame(session: &str, req: &RelayRequest) -> String {
    json!({
        "type": "event",
        "session": session,
        "event": {
            "kind": "approval_request",
            "text": format!("{} {} [id:{}]", req.tool, req.detail, req.id),
            "id": req.id,
            "tool": req.tool,
            "detail": req.detail,
        },
        "ts": chrono::Utc::now().to_rfc3339(),
    })
    .to_string()
}

/// Bridges the runtime's remote-approval relay onto the gateway event stream:
/// every [`RelayRequest`] the runtime broadcasts becomes an
/// `approval_request` frame any connected surface can answer through
/// `approval.respond`. Reads the session id from the status snapshot (never
/// the runtime mutex) so a turn waiting on approval can't stall fan-out.
pub async fn control_event_forwarder(state: Arc<AppState>) {
    let bus = crate::control_bus::ControlBus::default();
    let mut positions = std::collections::HashMap::<std::path::PathBuf, u64>::new();
    loop {
        for path in bus.journals() {
            let offset = positions
                .entry(path.clone())
                .or_insert_with(|| path.metadata().map(|m| m.len()).unwrap_or(0));
            if let Ok(frames) = crate::control_bus::ControlBus::read_since(&path, offset) {
                for frame in frames {
                    if frame.pid == std::process::id() {
                        continue;
                    }
                    let _ = state
                        .events
                        .send(crate::dashboard::event_frame(&frame.session, &frame.event));
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

pub async fn approval_relay_forwarder(
    state: Arc<AppState>,
    mut relay: broadcast::Receiver<RelayRequest>,
) {
    loop {
        match relay.recv().await {
            Ok(req) => {
                let session = {
                    let snap = state.snap.read().await;
                    snap.status
                        .get("session")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string()
                };
                let _ = state.events.send(approval_relay_frame(&session, &req));
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!("approval relay fan-out lagged; {missed} requests skipped");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::CatalogModel;
    use axum::body::Body;
    use tower::ServiceExt;

    fn ctx() -> GatewayCtx {
        GatewayCtx::detached(
            json!({ "model": "test-model", "provider": "proxy", "session": "s-1" }),
            vec![CatalogModel {
                id: "m1".into(),
                display_name: None,
                owned_by: Some("test".into()),
            }],
        )
    }

    // ---------- auth ----------

    #[test]
    fn unset_token_allows_any_host_guarded_client() {
        assert!(gateway_authorized(None, None, &HeaderMap::new()));
        assert!(gateway_authorized(
            None,
            Some("token=wrong"),
            &HeaderMap::new()
        ));
    }

    #[test]
    fn set_token_rejects_missing_or_wrong_credentials() {
        let headers = HeaderMap::new();
        assert!(!gateway_authorized(Some("secret"), None, &headers));
        assert!(!gateway_authorized(
            Some("secret"),
            Some("token=secreT"),
            &headers
        ));
        assert!(!gateway_authorized(
            Some("secret"),
            Some("token=secretx"),
            &headers
        ));
        assert!(!gateway_authorized(
            Some("secret"),
            Some("other=secret"),
            &headers
        ));
    }

    #[test]
    fn set_token_accepts_query_and_bearer_credentials() {
        assert!(gateway_authorized(
            Some("secret"),
            Some("token=secret"),
            &HeaderMap::new()
        ));
        // percent-encoded and mixed with other params
        assert!(gateway_authorized(
            Some("s sec ret"),
            Some("a=1&token=s%20sec%20ret&b=2"),
            &HeaderMap::new()
        ));
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(gateway_authorized(Some("secret"), None, &headers));
        // query wins over a wrong bearer token
        assert!(gateway_authorized(
            Some("secret"),
            Some("token=secret"),
            &headers
        ));
        let mut wrong = HeaderMap::new();
        wrong.insert(AUTHORIZATION, "Bearer nope".parse().unwrap());
        assert!(!gateway_authorized(Some("secret"), None, &wrong));
    }

    #[test]
    fn percent_decode_handles_escapes_and_leaves_stray_percent() {
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("trailing%2"), "trailing%2");
        assert_eq!(percent_decode("plus+stays"), "plus+stays");
    }

    // ---------- frame wrapping ----------

    #[test]
    fn broadcast_frames_are_wrapped_as_jsonrpc_notifications() {
        let frame = crate::dashboard::event_frame(
            "sess-1",
            &crate::runtime::AgentEvent {
                kind: "tool".into(),
                text: "ls".into(),
            },
        );
        let wrapped = wrap_event_frame(&frame);
        assert_eq!(wrapped["jsonrpc"], "2.0");
        assert_eq!(wrapped["method"], "event");
        assert_eq!(wrapped["params"]["type"], "event");
        assert_eq!(wrapped["params"]["session"], "sess-1");
        assert_eq!(wrapped["params"]["event"]["kind"], "tool");
    }

    #[test]
    fn non_json_frames_are_wrapped_as_string_params() {
        let wrapped = wrap_event_frame("not json at all");
        assert_eq!(wrapped["method"], "event");
        assert_eq!(wrapped["params"], "not json at all");
    }

    #[test]
    fn relay_frames_carry_the_id_both_ways() {
        let req = RelayRequest {
            id: "11111111-2222-3333-4444-555555555555".into(),
            tool: "write_file".into(),
            detail: "src/x.rs".into(),
        };
        let frame: Value = serde_json::from_str(&approval_relay_frame("s-1", &req)).unwrap();
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["session"], "s-1");
        assert_eq!(frame["event"]["kind"], "approval_request");
        assert_eq!(frame["event"]["id"], req.id);
        assert!(frame["event"]["text"]
            .as_str()
            .unwrap()
            .contains("[id:11111111-2222-3333-4444-555555555555]"));
        chrono::DateTime::parse_from_rfc3339(frame["ts"].as_str().unwrap()).unwrap();
    }

    // ---------- json-rpc envelopes ----------

    #[test]
    fn success_and_error_envelopes_echo_the_request_id() {
        let ok = success_response(json!(7), json!("pong"));
        assert_eq!(ok["jsonrpc"], "2.0");
        assert_eq!(ok["id"], 7);
        assert_eq!(ok["result"], "pong");
        let err = error_response(json!("abc"), -32601, "method not found: x");
        assert_eq!(err["id"], "abc");
        assert_eq!(err["error"]["code"], -32601);
        assert_eq!(err["error"]["message"], "method not found: x");
        assert!(err.get("result").is_none());
    }

    // ---------- dispatcher ----------

    #[tokio::test]
    async fn ping_answers_pong() {
        let res = handle_request(&ctx(), json!({ "id": 1, "method": "ping" })).await;
        assert_eq!(res["result"], "pong");
        assert_eq!(res["id"], 1);
    }

    #[tokio::test]
    async fn status_get_serves_the_snapshot_with_busy() {
        let res = handle_request(&ctx(), json!({ "id": 2, "method": "status.get" })).await;
        assert_eq!(res["result"]["model"], "test-model");
        assert_eq!(res["result"]["busy"], false);
    }

    #[tokio::test]
    async fn models_list_serves_snapshot_models_without_a_runtime() {
        let res = handle_request(&ctx(), json!({ "id": 3, "method": "models.list" })).await;
        assert_eq!(res["result"]["ok"], true);
        assert_eq!(res["result"]["models"][0]["id"], "m1");
    }

    #[tokio::test]
    async fn sessions_list_goes_through_the_overridable_payload() {
        let res = handle_request(&ctx(), json!({ "id": 4, "method": "sessions.list" })).await;
        assert_eq!(res["result"]["ok"], true);
        assert!(res["result"]["sessions"].is_array());

        let mut custom = ctx();
        custom.list_sessions = || json!({ "ok": true, "sessions": [ { "id": "abc" } ] });
        let res = handle_request(&custom, json!({ "id": 5, "method": "sessions.list" })).await;
        assert_eq!(res["result"]["sessions"][0]["id"], "abc");
    }

    #[tokio::test]
    async fn session_get_validates_params_and_reports_missing_sessions() {
        let res = handle_request(&ctx(), json!({ "id": 6, "method": "session.get" })).await;
        assert_eq!(res["error"]["code"], -32602);
        // A non-UUID id fails validation before any disk access, so this
        // test stays out of the real ~/.varynth.
        let res = handle_request(
            &ctx(),
            json!({ "id": 7, "method": "session.get", "params": { "id": "not-a-uuid" } }),
        )
        .await;
        assert_eq!(res["result"]["ok"], false);
        assert!(res["result"]["error"].as_str().unwrap().contains("id"));
    }

    #[tokio::test]
    async fn session_message_rejects_missing_or_blank_text() {
        for params in [
            json!({ "id": "x" }),
            json!({ "id": "x", "text": "   " }),
            json!({ "id": "x", "text": 42 }),
        ] {
            let res = handle_request(
                &ctx(),
                json!({ "id": 8, "method": "session.message", "params": params }),
            )
            .await;
            assert_eq!(res["error"]["code"], -32602);
        }
    }

    #[tokio::test]
    async fn model_set_without_a_live_runtime_is_an_internal_error() {
        let res = handle_request(
            &ctx(),
            json!({ "id": 9, "method": "model.set", "params": { "id": "new-model" } }),
        )
        .await;
        assert_eq!(res["error"]["code"], -32603);
        assert!(res["error"]["message"]
            .as_str()
            .unwrap()
            .contains("runtime"));
    }

    #[tokio::test]
    async fn approval_respond_feeds_the_open_relay_slot() {
        let c = ctx();
        let (req, rx) = approval_relay::open("write_file", "src/x.rs");
        let res = handle_request(
            &c,
            json!({
                "id": 10,
                "method": "approval.respond",
                "params": { "id": req.id, "decision": "once" },
            }),
        )
        .await;
        assert_eq!(res["result"], true);
        assert_eq!(rx.await.unwrap(), ApprovalVote::Once);
        // The slot is consumed; a second answer reports false.
        let res = handle_request(
            &c,
            json!({
                "id": 11,
                "method": "approval.respond",
                "params": { "id": req.id, "decision": "deny" },
            }),
        )
        .await;
        assert_eq!(res["result"], false);
    }

    #[tokio::test]
    async fn approval_respond_validates_the_decision() {
        for decision in ["nope", "", "Once"] {
            let res = handle_request(
                &ctx(),
                json!({
                    "id": 12,
                    "method": "approval.respond",
                    "params": { "id": "whatever", "decision": decision },
                }),
            )
            .await;
            assert_eq!(res["error"]["code"], -32602);
        }
        let res = handle_request(
            &ctx(),
            json!({ "id": 13, "method": "approval.respond", "params": { "decision": "once" } }),
        )
        .await;
        assert_eq!(res["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn unknown_method_is_32601_and_missing_method_is_32602() {
        let res =
            handle_request(&ctx(), json!({ "id": 14, "method": "definitely.not.here" })).await;
        assert_eq!(res["error"]["code"], -32601);
        assert!(res["error"]["message"]
            .as_str()
            .unwrap()
            .contains("definitely.not.here"));
        let res = handle_request(&ctx(), json!({ "id": 15 })).await;
        assert_eq!(res["error"]["code"], -32602);
    }

    // ---------- router presence through auth ----------

    #[tokio::test]
    async fn router_without_a_token_is_rejected_before_the_upgrade() {
        let c = GatewayCtx {
            token: Some("secret".into()),
            ..GatewayCtx::detached(json!({}), vec![])
        };
        let app: Router<()> = ws_router(Arc::new(c));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ws/gateway")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // A wrong token is equally unwelcome.
        let c = GatewayCtx {
            token: Some("secret".into()),
            ..GatewayCtx::detached(json!({}), vec![])
        };
        let app: Router<()> = ws_router(Arc::new(c));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ws/gateway?token=wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn router_with_a_valid_query_token_reaches_the_upgrade() {
        // The dispatcher context carries the same token the handler expects.
        let app: Router<()> = ws_router(Arc::new(ctx()));
        // Auth passes, and the plain GET (no upgrade headers) is then turned
        // away by the WebSocket extractor with 400 — proof the route exists
        // and the token gate let the request through.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ws/gateway?token=gateway-test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // Unset token: loopback behavior as today — straight to the upgrade.
        let c = GatewayCtx {
            token: None,
            ..GatewayCtx::detached(json!({}), vec![])
        };
        let app: Router<()> = ws_router(Arc::new(c));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ws/gateway")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
