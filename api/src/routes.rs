use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{DefaultBodyLimit, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower_http::cors::{AllowOrigin, CorsLayer};
use uuid::Uuid;

use crate::byok::{
    call_byok_upstream, default_or_custom_models, fetch_user_byok_models, find_user_byok_for_model,
    ByokUpstream,
};
use crate::chat::{
    anthropic_body, anthropic_to_openai, completion_to_sse, list_all_enabled_models, model_to_json,
    prepare_upstream_body, resolve_dispatch, rewrite_model, rewrite_sse, scrubbed_error,
    stub_completion, CatalogModel, Dispatch, ModelRow, MODEL_SELECT_COLUMNS,
};
use crate::config::{canonical_ip, Config};
use crate::crypto::{
    append_query, expiry_from_now, pkce_s256, public_json_is_clean, random_token, read_token,
    sha256, sign_token, valid_challenge, valid_loopback_redirect, valid_state, ACCESS_TTL_SECS,
    CODE_TTL_SECS, REFRESH_TTL_SECS, WEB_TTL_SECS,
};
use crate::crypto::Claims;
use crate::error::ApiError;
use crate::limits::{
    check_and_bump_weighted, consume, consume_message, day_bucket, minute_bucket, quota,
    AuthLimiter, Quota,
};
use crate::search::{self, hits_json, SearchCursors};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub gateway_cursor: Arc<AtomicUsize>,
    pub search_cursors: Arc<SearchCursors>,
    pub auth_limiter: Arc<AuthLimiter>,
}

impl AppState {
    pub fn new(pool: PgPool, config: Arc<Config>, http: reqwest::Client) -> Self {
        Self {
            pool,
            config,
            http,
            gateway_cursor: Arc::new(AtomicUsize::new(0)),
            search_cursors: Arc::new(SearchCursors::new()),
            auth_limiter: Arc::new(AuthLimiter::default()),
        }
    }
}

pub fn router(state: AppState) -> Router {
    let origin = state.config.web_origin.clone();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::exact(
            origin
                .parse()
                .unwrap_or_else(|_| "http://127.0.0.1:3000".parse().expect("origin")),
        ))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
        .allow_credentials(true);
    Router::new()
        .route("/v1/models/public", get(public_models))
        .route("/v1/models", get(public_models))
        .route("/v1/plans", get(plans))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/search", post(search_route))
        .route("/v1/catalog", get(catalog))
        .route("/v1/catalog/skills", get(skills_catalog_route))
        .route("/v1/images/generations", post(generate_image_route))
        .route("/v1/auth/register", post(register))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/auth/me", get(me))
        .route("/v1/me/usage", get(me_usage))
        .route("/v1/me/subscription", post(subscription))
        .route("/v1/me/keys", get(list_api_keys).post(create_api_key))
        .route("/v1/me/keys/revoke", post(revoke_api_key))
        .route("/v1/me/sessions", get(list_cli_sessions).post(upsert_cli_session))
        .route("/v1/me/sessions/delete", post(delete_cli_session))
        .route("/v1/me/byok", get(list_byok_providers).post(upsert_byok_provider))
        .route("/v1/me/byok/delete", post(delete_byok_provider))
        .route("/v1/auth/oauth/providers", get(oauth_providers_status))
        .route("/v1/auth/oauth/google", post(oauth_google_exchange))
        .route("/v1/auth/oauth/github", post(oauth_github_exchange))
        .route("/v1/oauth/authorize", post(authorize))
        .route("/v1/oauth/token", post(token))
        .route("/v1/oauth/revoke", post(revoke))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(cors)
        .layer(middleware::from_fn_with_state(state.clone(), hsts))
        .with_state(state)
}

const HSTS_VALUE: &str = "max-age=63072000; includeSubDomains";

/// HTTPS deployments get HSTS, which is what keeps the `Secure` session
/// cookie from ever travelling over plaintext after the first visit.
async fn hsts(State(state): State<AppState>, request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if state.config.secure_cookies() {
        response
            .headers_mut()
            .insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static(HSTS_VALUE));
    }
    response
}

pub fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Socket peer address for the request. Extracted without failing so unit
/// tests and embedded towers without `ConnectInfo` still work; when absent,
/// forwarded headers are never trusted.
#[derive(Clone, Copy, Debug)]
struct PeerAddr(Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|connect| connect.0);
        Ok(PeerAddr(peer))
    }
}

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct ApproveBody {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    code_challenge_method: String,
    decision: String,
}

#[derive(Deserialize)]
struct TokenBody {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct RevokeBody {
    refresh_token: String,
}

async fn public_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let rows = list_all_enabled_models(&state.pool).await?;
    let mut data: Vec<Value> = rows.iter().map(model_to_json).collect();

    if let Ok(Some(claims)) = optional_user(&state, &headers).await {
        if let Ok(uid) = parse_user(&claims.sub) {
            if let Ok(byok_models) = fetch_user_byok_models(&state.pool, uid).await {
                for m in byok_models {
                    if let Some(id_str) = m.get("id").and_then(|v| v.as_str()) {
                        if !data.iter().any(|d| d.get("id").and_then(|v| v.as_str()) == Some(id_str)) {
                            data.push(m);
                        }
                    }
                }
            }
        }
    }

    let body = json!({"object": "list", "data": data});
    if !public_json_is_clean(&body.to_string()) {
        return Err(ApiError::Internal);
    }
    Ok(Json(body))
}

fn quota_json(limits: Quota) -> Value {
    json!({
        "chat_per_day": limits.chat_per_day,
        "chat_per_minute": limits.chat_per_minute,
        "search_per_day": limits.search_per_day
    })
}

async fn plans(State(state): State<AppState>) -> Json<Value> {
    let free = quota(true, "free");
    let pro = quota(true, "pro");
    let anonymous = quota(false, "free");
    Json(json!({
        "object": "list",
        "billing_configured": state.config.billing_stub,
        "data": [
            {
                "id": "free",
                "name": "Free",
                "price_per_month": 0,
                "quota": quota_json(free),
                "features": [
                    "Varynth public models",
                    "Your own endpoint keys stay local",
                    "Account catalog when signed in"
                ]
            },
            {
                "id": "pro",
                "name": "Pro",
                "price_per_month": null,
                "quota": quota_json(pro),
                "features": [
                    "Everything in Free",
                    "Higher chat and search limits",
                    "Account catalog at the same terms"
                ]
            }
        ],
        "anonymous": quota_json(anonymous)
    }))
}

async fn catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let _user = require_user(&state, &headers, &["cli", "web"]).await?;
    let providers = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, display_name, kind FROM provider_catalog ORDER BY sort_order, id",
    )
    .fetch_all(&state.pool)
    .await?;
    let models = sqlx::query_as::<_, (String, String, String)>(
        "SELECT provider_id, model_id, display_name FROM provider_models ORDER BY provider_id, model_id",
    )
    .fetch_all(&state.pool)
    .await?;
    let all_models = list_all_enabled_models(&state.pool).await?;
    let data: Vec<Value> = providers
        .into_iter()
        .map(|(id, name, kind)| {
            let owned: Vec<Value> = models
                .iter()
                .filter(|(provider, _, _)| provider == &id)
                .map(|(_, model_id, display)| {
                    let varynth_id = format!("varynth/{model_id}");
                    if let Some(m) = all_models.iter().find(|r| r.id == varynth_id || r.upstream_model == *model_id) {
                        let is_free = m.min_plan == "free" || (m.input_price_per_m == 0.0 && m.output_price_per_m == 0.0);
                        json!({
                            "id": model_id,
                            "varynth_id": m.id,
                            "name": display,
                            "display_name": m.display_name,
                            "tier": m.tier,
                            "min_plan": m.min_plan,
                            "category": m.category,
                            "pricing": {
                                "input_per_m": m.input_price_per_m,
                                "output_per_m": m.output_price_per_m,
                                "currency": "USD",
                                "is_free": is_free,
                                "request_cost": m.request_cost
                            },
                            "limits": {
                                "rpm": m.rpm_limit,
                                "rpd": m.rpd_limit,
                                "request_cost": m.request_cost
                            }
                        })
                    } else {
                        json!({"id": model_id, "name": display})
                    }
                })
                .collect();
            json!({"id": id, "name": name, "kind": kind, "models": owned})
        })
        .collect();
    let varynth_models: Vec<Value> = all_models.iter().map(model_to_json).collect();
    Ok(Json(json!({"providers": data, "models": varynth_models})))
}

#[derive(sqlx::FromRow)]
struct SkillRow {
    id: String,
    name: String,
    description: String,
    tier: String,
    content: String,
}

async fn skills_catalog_route(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = optional_user(&state, &headers).await?;
    let plan = user_plan(&state, user.as_ref()).await?;
    let is_pro = plan == "pro";

    let rows = sqlx::query_as::<_, SkillRow>(
        "SELECT id, name, description, tier, content
         FROM skills_catalog
         WHERE enabled = true
         ORDER BY sort_order, id",
    )
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let entitled = row.tier == "free" || is_pro;
            json!({
                "id": row.id,
                "name": row.name,
                "description": row.description,
                "tier": row.tier,
                "unlocked": entitled,
                "content": if entitled { Some(row.content) } else { None }
            })
        })
        .collect();

    Ok(Json(json!({
        "object": "list",
        "tier": plan,
        "skills": items,
        "data": items
    })))
}

#[derive(Deserialize)]
struct ImageGenBody {
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    size: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    response_format: Option<String>,
}

const STUB_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

async fn generate_image_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ImageGenBody>,
) -> Result<Json<Value>, ApiError> {
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return Err(ApiError::bad("prompt is required"));
    }
    if prompt.len() > 4000 {
        return Err(ApiError::bad("prompt exceeds maximum length of 4000 characters"));
    }

    let user = optional_user(&state, &headers).await?;
    if user.is_none() && !state.config.stub_upstream {
        return Err(ApiError::Unauthorized);
    }
    let plan = user_plan(&state, user.as_ref()).await?;
    if plan != "pro" && !state.config.stub_upstream && state.config.worker_gateways.is_empty() {
        return Err(ApiError::Forbidden(
            "Pro subscription required for image generation".into(),
        ));
    }

    if state.config.stub_upstream {
        return Ok(Json(json!({
            "created": chrono::Utc::now().timestamp(),
            "data": [
                {
                    "b64_json": STUB_PNG_B64,
                    "url": "https://images.varynth.dev/stub.png"
                }
            ]
        })));
    }

    // Upstream Workers AI Flux
    if let (Some(account_id), Some(token)) = (
        &state.config.workers_account_id,
        &state.config.workers_token,
    ) {
        let url = format!(
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run/@cf/black-forest-labs/flux-1-schnell"
        );
        let resp = state
            .http
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&json!({ "prompt": prompt }))
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(%error, "workers_ai image transport");
                ApiError::Unavailable
            })?;

        let status = resp.status();
        if !status.is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            tracing::warn!(%status, %err_text, "workers_ai image error");
            return Err(ApiError::Unavailable);
        }

        let is_json = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("application/json"))
            .unwrap_or(false);

        let b64 = if is_json {
            let json_val: Value = resp.json().await.map_err(|_| ApiError::Unavailable)?;
            json_val
                .pointer("/result/image")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .ok_or(ApiError::Unavailable)?
        } else {
            let bytes = resp.bytes().await.map_err(|_| ApiError::Unavailable)?;
            BASE64_STANDARD.encode(&bytes)
        };

        return Ok(Json(json!({
            "created": chrono::Utc::now().timestamp(),
            "data": [
                {
                    "b64_json": b64,
                    "url": null
                }
            ]
        })));
    }

    // Upstream Worker Gateway Flux (with multi-gateway failover)
    if let Some(gateway_key) = &state.config.worker_gateway_key {
        if !state.config.worker_gateways.is_empty() {
            let model = body
                .model
                .as_deref()
                .unwrap_or("flux-1-schnell")
                .trim_start_matches("varynth/")
                .trim_start_matches("black-forest-labs/");
            let len = state.config.worker_gateways.len();
            let start = state.gateway_cursor.fetch_add(1, Ordering::Relaxed) % len;
            for offset in 0..len {
                let base = &state.config.worker_gateways[(start + offset) % len];
                let url = format!("{}/images/generations", base.trim_end_matches('/'));
                match state
                    .http
                    .post(url)
                    .bearer_auth(gateway_key)
                    .json(&json!({
                        "prompt": prompt,
                        "model": model,
                        "n": body.n.unwrap_or(1)
                    }))
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        let is_json = resp
                            .headers()
                            .get(header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(|ct| ct.contains("application/json"))
                            .unwrap_or(false);
                        if is_json {
                            if let Ok(json_val) = resp.json::<Value>().await {
                                return Ok(Json(json_val));
                            }
                        } else if let Ok(bytes) = resp.bytes().await {
                            let b64 = BASE64_STANDARD.encode(&bytes);
                            return Ok(Json(json!({
                                "created": chrono::Utc::now().timestamp(),
                                "data": [{ "b64_json": b64, "url": null }]
                            })));
                        }
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let err_text = resp.text().await.unwrap_or_default();
                        tracing::warn!(%status, %err_text, "worker_gateway image error");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "worker_gateway image transport");
                    }
                }
            }
            return Err(ApiError::Unavailable);
        }
    }

    // Upstream OpenRouter Flux
    if let Some(openrouter_key) = &state.config.openrouter_key {
        let url = "https://openrouter.ai/api/v1/images/generations";
        let model = body
            .model
            .as_deref()
            .unwrap_or("black-forest-labs/flux-1-schnell");
        let resp = state
            .http
            .post(url)
            .header("Authorization", format!("Bearer {openrouter_key}"))
            .json(&json!({
                "prompt": prompt,
                "model": model,
                "n": body.n.unwrap_or(1)
            }))
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(%error, "openrouter image transport");
                ApiError::Unavailable
            })?;

        let status = resp.status();
        if !status.is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            tracing::warn!(%status, %err_text, "openrouter image error");
            return Err(ApiError::Unavailable);
        }

        let json_val: Value = resp.json().await.map_err(|_| ApiError::Unavailable)?;
        return Ok(Json(json_val));
    }

    Err(ApiError::Unavailable)
}

async fn register(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Json<Value>, ApiError> {
    let email = normalize_email(&body.email)?;
    check_password(&body.password)?;
    let key = auth_key(client_ip(peer.0, &headers, &state.config), &email);
    throttle(&state, &key, "auth-register", REGISTER_PER_MINUTE, AUTH_THROTTLE_MESSAGE).await?;
    let id = Uuid::new_v4();
    let hash = hash_password(&body.password)?;
    let plan = account_plan(&state, &email);
    let inserted = sqlx::query(
        "INSERT INTO users (id, email, password_hash, plan) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(&email)
    .bind(hash)
    .bind(plan)
        .execute(&state.pool)
        .await;
    match inserted {
        Ok(_) => Ok(Json(json!({"id": id, "email": email}))),
        Err(error) if is_unique(&error) => Err(ApiError::Conflict("email is already registered".into())),
        Err(error) => Err(error.into()),
    }
}

async fn login(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    let email = normalize_email(&body.email)?;
    let key = auth_key(client_ip(peer.0, &headers, &state.config), &email);
    if let Some(retry) = state.auth_limiter.retry_after(&key) {
        return Err(lockout_error(retry));
    }
    throttle(&state, &key, "auth-login", LOGIN_PER_MINUTE, AUTH_THROTTLE_MESSAGE).await?;
    let user = sqlx::query_as::<_, UserSecret>(
        "SELECT id, email, password_hash FROM users WHERE email = $1",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;
    // Unknown accounts and OAuth-only accounts still pay for one Argon2
    // verification so response timing does not disclose account existence.
    let verified = match user.as_ref().and_then(|user| user.password_hash.as_deref()) {
        Some(hash) => verify_password(&body.password, hash),
        None => {
            verify_unknown_password(&body.password);
            false
        }
    };
    let Some(user) = user.filter(|_| verified) else {
        let wait = state.auth_limiter.record_failure(&key);
        if wait.is_zero() {
            return Err(ApiError::Unauthorized);
        }
        return Err(lockout_error(wait));
    };
    state.auth_limiter.record_success(&key);
    let token = sign_token(&state.config.jwt_secret, &user.id.to_string(), "web", WEB_TTL_SECS)?;
    let cookie = session_cookie(&token, state.config.secure_cookies(), WEB_TTL_SECS);
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"session_token": token, "expires_in": WEB_TTL_SECS, "email": user.email})),
    )
        .into_response())
}

async fn logout(State(state): State<AppState>) -> Response {
    let cookie = session_cookie("", state.config.secure_cookies(), 0);
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let claims = require_user(&state, &headers, &["cli", "web"]).await?;
    let id = parse_user(&claims.sub)?;
    let email = sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    Ok(Json(json!({"id": claims.sub, "email": email})))
}

async fn me_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let plan = user_plan(&state, Some(&user)).await?;
    let limits = quota(true, &plan);
    let subject = usage_subject(Some(&user), &headers, None, &state.config);
    let now = chrono::Utc::now();
    let day = day_bucket(now);
    let minute = minute_bucket(now);
    let rows = sqlx::query_as::<_, (String, String, i32)>(
        "SELECT kind, bucket, count FROM usage_counters
         WHERE subject = $1 AND kind IN ('chat-day', 'search-day', 'chat-minute')
           AND bucket IN ($2, $3)",
    )
    .bind(&subject)
    .bind(&day)
    .bind(&minute)
    .fetch_all(&state.pool)
    .await?;
    let counter = |kind: &str, bucket: &str| -> i32 {
        rows.iter()
            .find(|(row_kind, row_bucket, _)| row_kind == kind && row_bucket == bucket)
            .map(|(_, _, count)| *count)
            .unwrap_or(0)
    };
    let chat_today = counter("chat-day", &day);
    let search_today = counter("search-day", &day);
    let chat_minute = counter("chat-minute", &minute);
    Ok(Json(json!({
        "plan": plan,
        "limits": quota_json(limits),
        "today": {
            "day": now.format("%Y-%m-%d").to_string(),
            "chat": chat_today,
            "search": search_today,
            "chat_this_minute": chat_minute
        },
        "remaining": {
            "chat": (limits.chat_per_day - chat_today).max(0),
            "search": (limits.search_per_day - search_today).max(0),
            "chat_this_minute": (limits.chat_per_minute - chat_minute).max(0)
        }
    })))
}

#[derive(Deserialize)]
struct PlanBody {
    plan: String,
}

async fn subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PlanBody>,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let id = parse_user(&user.sub)?;
    let target = body.plan.trim().to_ascii_lowercase();
    if target != "free" && target != "pro" {
        return Err(ApiError::bad("plan must be free or pro"));
    }
    let current = sqlx::query_scalar::<_, String>("SELECT plan FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if current == target {
        return Ok(Json(json!({
            "plan": current,
            "changed": false,
            "billing_configured": state.config.billing_stub
        })));
    }
    if !state.config.billing_stub {
        return Err(ApiError::BillingUnavailable);
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE users SET plan = $1, updated_at = now() WHERE id = $2")
        .bind(&target)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO subscription_events (id, user_id, from_plan, to_plan, source)
         VALUES ($1, $2, $3, $4, 'stub')",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .bind(&current)
    .bind(&target)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "plan": target,
        "previous_plan": current,
        "changed": true,
        "source": "stub"
    })))
}

async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ApproveBody>,
) -> Result<Json<Value>, ApiError> {
    let claims = require_user(&state, &headers, &["web"]).await?;
    if body.decision == "deny" {
        if !valid_loopback_redirect(&body.redirect_uri) || !valid_state(&body.state) {
            return Err(ApiError::bad("redirect_uri is invalid"));
        }
        let redirect_to = append_query(&body.redirect_uri, &[("error", "access_denied"), ("state", &body.state)])?;
        return Ok(Json(json!({"redirect_to": redirect_to})));
    }
    if body.decision != "approve" {
        return Err(ApiError::bad("decision must be approve or deny"));
    }
    validate_authorize(&state, &body).await?;
    let code = random_token();
    let user_id = parse_user(&claims.sub)?;
    sqlx::query(
        "INSERT INTO auth_codes (code, user_id, client_id, redirect_uri, code_challenge, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&code)
    .bind(user_id)
    .bind(&body.client_id)
    .bind(&body.redirect_uri)
    .bind(&body.code_challenge)
    .bind(expiry_from_now(CODE_TTL_SECS))
    .execute(&state.pool)
    .await?;
    let redirect_to = append_query(&body.redirect_uri, &[("code", &code), ("state", &body.state)])?;
    Ok(Json(json!({"redirect_to": redirect_to})))
}

async fn token(State(state): State<AppState>, Json(body): Json<TokenBody>) -> Result<Json<Value>, ApiError> {
    match body.grant_type.as_str() {
        "authorization_code" => authorization_code(&state, &body).await,
        "refresh_token" => refresh(&state, &body).await,
        _ => Err(ApiError::bad("unsupported grant_type")),
    }
}

async fn revoke(State(state): State<AppState>, Json(body): Json<RevokeBody>) -> Result<StatusCode, ApiError> {
    let hash = sha256(body.refresh_token.as_bytes());
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE token_hash = $1 AND revoked_at IS NULL")
        .bind(hash)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn chat(
    State(state): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let requested = body
        .get("model")
        .and_then(|value| value.as_str())
        .ok_or_else(|| ApiError::bad("model is required"))?;
    if !valid_model(requested) {
        return Err(ApiError::bad("model is invalid"));
    }
    let user = optional_user(&state, &headers).await?;

    if let Some(claims) = &user {
        if let Ok(uid) = parse_user(&claims.sub) {
            if let Some(byok) = find_user_byok_for_model(&state.pool, uid, requested).await? {
                let stream = body.get("stream").and_then(|value| value.as_bool()).unwrap_or(false);
                let upstream = call_byok_upstream(&state.http, &byok, &body, stream).await?;
                return Ok(match upstream {
                    ByokUpstream::Json(value) => Json(value).into_response(),
                    ByokUpstream::Sse(text) => Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(text))
                        .map_err(|_| ApiError::Internal)?,
                });
            }
        }
    }

    let model = find_model(&state, requested).await?;
    let catalog = if model.is_none() {
        find_catalog(&state, requested).await?
    } else {
        None
    };
    let plan = user_plan(&state, user.as_ref()).await?;
    if let Some(row) = &model {
        if row.min_plan == "pro" {
            if user.is_none() {
                return Err(ApiError::UnauthorizedMsg(
                    "Bu model ücretli (Pro) kategorisindedir. Kullanmak için giriş yapın: varynth login".into(),
                ));
            }
            if plan != "pro" {
                return Err(ApiError::PaymentRequired(format!(
                    "Model '{}' requires a Pro plan (min_plan: pro, pricing: ${:.2}/1M input, ${:.2}/1M output, cost: {}x request quota). Upgrade at /pricing or configure a custom BYOK key.",
                    row.id, row.input_price_per_m, row.output_price_per_m, row.request_cost
                )));
            }
        } else if row.tier != "public" && user.is_none() {
            return Err(ApiError::Unauthorized);
        }
    } else if catalog.is_some() && user.is_none() {
        return Err(ApiError::Unauthorized);
    } else if model.is_none() && catalog.is_none() {
        return Err(ApiError::NotFound);
    }
    let mut dispatch = resolve_dispatch(model.as_ref(), catalog.as_ref(), &state.config)?;
    if requested.contains('/') && requested != dispatch.public_model {
        if let Some((req_prov, _)) = requested.split_once('/') {
            if req_prov != "varynth" {
                dispatch.public_model = requested.to_string();
            }
        }
    }
    enforce_chat(&state, &headers, user.as_ref(), peer.0, &plan, model.as_ref()).await?;
    let stream = body.get("stream").and_then(|value| value.as_bool()).unwrap_or(false);
    let upstream = call_upstream(&state, &dispatch, &body, stream).await?;
    Ok(match upstream {
        Upstream::Json(value) => Json(value).into_response(),
        Upstream::Sse(text) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(text))
            .map_err(|_| ApiError::Internal)?,
    })
}

enum Upstream {
    Json(Value),
    Sse(String),
}

async fn call_upstream(
    state: &AppState,
    dispatch: &Dispatch,
    body: &Value,
    stream: bool,
) -> Result<Upstream, ApiError> {
    tracing::info!(
        model = %dispatch.public_model,
        upstream_kind = %dispatch.upstream_kind,
        stub = dispatch.stub,
        "chat upstream"
    );
    if dispatch.stub {
        let completion = stub_completion(&dispatch.public_model);
        return Ok(if stream {
            Upstream::Sse(completion_to_sse(&completion))
        } else {
            Upstream::Json(completion)
        });
    }
    let key = state
        .config
        .upstream_key(&dispatch.upstream_kind)
        .ok_or(ApiError::Unavailable)?;
    if dispatch.upstream_kind == "anthropic" {
        let payload = anthropic_body(body, &dispatch.upstream_model)?;
        let url = format!("{}/v1/messages", dispatch.upstream_base.trim_end_matches('/'));
        let response = state
            .http
            .post(url)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(%error, "upstream transport");
                ApiError::Unavailable
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|_| ApiError::Unavailable)?;
        if !status.is_success() {
            return Err(scrubbed_error(status, &text));
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| ApiError::Unavailable)?;
        let completion = anthropic_to_openai(value, &dispatch.public_model);
        return Ok(if stream {
            Upstream::Sse(completion_to_sse(&completion))
        } else {
            Upstream::Json(completion)
        });
    }
    let upstream_stream = stream && dispatch.upstream_kind != "worker_gateway";
    let payload = prepare_upstream_body(body.clone(), &dispatch.upstream_model, upstream_stream);
    let mut targets: Vec<(String, String)> = if dispatch.upstream_kind == "worker_gateway" && !state.config.worker_gateways.is_empty() {
        let len = state.config.worker_gateways.len();
        let start = state.gateway_cursor.fetch_add(1, Ordering::Relaxed) % len;
        (0..len)
            .map(|offset| (state.config.worker_gateways[(start + offset) % len].clone(), key.clone()))
            .collect()
    } else {
        vec![(gateway_base(state, dispatch), key.clone())]
    };
    if dispatch.fallback_worker_gateway && dispatch.upstream_kind == "hrmn" {
        if let Some(gw_key) = &state.config.worker_gateway_key {
            let len = state.config.worker_gateways.len();
            if len > 0 {
                let start = state.gateway_cursor.fetch_add(1, Ordering::Relaxed) % len;
                for offset in 0..len {
                    targets.push((
                        state.config.worker_gateways[(start + offset) % len].clone(),
                        gw_key.clone(),
                    ));
                }
            }
        }
    }

    let mut last_err: Option<(reqwest::StatusCode, String)> = None;
    let mut ok_text: Option<String> = None;
    for (base, target_key) in targets {
        let url = format!("{}/chat/completions", base.trim_end_matches('/'));
        match state
            .http
            .post(url)
            .bearer_auth(&target_key)
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                if status.is_success() {
                    ok_text = Some(text);
                    break;
                }
                last_err = Some((status, text));
            }
            Err(error) => {
                tracing::warn!(%error, "upstream transport");
            }
        }
    }
    let Some(text) = ok_text else {
        if let Some((status, err_text)) = last_err {
            return Err(scrubbed_error(status, &err_text));
        }
        return Err(ApiError::Unavailable);
    };
    if text.trim_start().starts_with("data:") {
        return Ok(Upstream::Sse(rewrite_sse(&text, &dispatch.public_model)));
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| ApiError::Unavailable)?;
    let value = rewrite_model(value, &dispatch.public_model);
    Ok(if stream {
        Upstream::Sse(completion_to_sse(&value))
    } else {
        Upstream::Json(value)
    })
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
    #[serde(default)]
    provider: String,
}

async fn search_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SearchBody>,
) -> Result<Json<Value>, ApiError> {
    let query = body.query.trim();
    if query.is_empty() || query.len() > 500 {
        return Err(ApiError::bad("query is required"));
    }
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let plan = user_plan(&state, Some(&user)).await?;
    let limits = quota(true, &plan);
    let subject = usage_subject(Some(&user), &headers, None, &state.config);
    let now = chrono::Utc::now();
    consume(
        &state.pool,
        &subject,
        "search-day",
        &day_bucket(now),
        limits.search_per_day,
    )
    .await?;
    let (provider, hits) = search::search(
        &state.http,
        &state.config,
        &state.search_cursors,
        &body.provider,
        query,
    )
    .await?;
    Ok(Json(hits_json(&provider, &hits)))
}

async fn enforce_chat(
    state: &AppState,
    headers: &HeaderMap,
    user: Option<&Claims>,
    peer: Option<SocketAddr>,
    plan: &str,
    model: Option<&ModelRow>,
) -> Result<(), ApiError> {
    let limits = quota(user.is_some(), plan);
    let subject = usage_subject(user, headers, peer, &state.config);
    let now = chrono::Utc::now();
    let min_b = minute_bucket(now);
    let day_b = day_bucket(now);

    if let Some(m) = model {
        let model_min_kind = format!("chat_model_min:{}", m.id);
        let model_day_kind = format!("chat_model_day:{}", m.id);
        let rpm_msg = format!("model {} rate limit reached ({} RPM)", m.id, m.rpm_limit);
        let rpd_msg = format!("model {} daily limit reached ({} RPD)", m.id, m.rpd_limit);
        check_and_bump_weighted(
            &state.pool,
            &subject,
            &model_min_kind,
            &min_b,
            m.rpm_limit,
            1,
            Some(&rpm_msg),
        )
        .await?;
        check_and_bump_weighted(
            &state.pool,
            &subject,
            &model_day_kind,
            &day_b,
            m.rpd_limit,
            1,
            Some(&rpd_msg),
        )
        .await?;
    }

    let weight = model.map(|m| m.request_cost.max(1)).unwrap_or(1);
    check_and_bump_weighted(
        &state.pool,
        &subject,
        "chat-minute",
        &min_b,
        limits.chat_per_minute,
        weight,
        None,
    )
    .await?;
    check_and_bump_weighted(
        &state.pool,
        &subject,
        "chat-day",
        &day_b,
        limits.chat_per_day,
        weight,
        None,
    )
    .await?;
    Ok(())
}

async fn user_plan(state: &AppState, user: Option<&Claims>) -> Result<String, ApiError> {
    let Some(user) = user else {
        return Ok("free".into());
    };
    let id = parse_user(&user.sub)?;
    let plan = sqlx::query_scalar::<_, String>("SELECT plan FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    Ok(plan.unwrap_or_else(|| "free".into()))
}

fn usage_subject(
    user: Option<&Claims>,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    config: &Config,
) -> String {
    if let Some(user) = user {
        return format!("user:{}", user.sub);
    }
    match client_ip(peer, headers, config) {
        Some(ip) => format!("anon:{ip}"),
        None => "anon:local".into(),
    }
}

/// Resolves the caller's address for quota purposes. The socket peer is
/// authoritative; `X-Forwarded-For` is read only when the peer itself is a
/// configured trusted proxy, and then only the last (proxy-appended) hop is
/// used so a client-supplied list cannot mint fresh quota buckets.
fn client_ip(peer: Option<SocketAddr>, headers: &HeaderMap, config: &Config) -> Option<IpAddr> {
    let peer_ip = peer.map(|addr| canonical_ip(addr.ip()));
    let Some(peer_ip) = peer_ip else {
        return None;
    };
    if !config.is_trusted_proxy(peer_ip) {
        return Some(peer_ip);
    }
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|hop| hop.trim().parse::<IpAddr>().ok())
        .map(canonical_ip)
        .or(Some(peer_ip))
}

fn auth_key(ip: Option<IpAddr>, email: &str) -> String {
    format!("{}|{}", ip.map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".into()), email)
}

const LOGIN_PER_MINUTE: i32 = 10;
const REGISTER_PER_MINUTE: i32 = 5;
const AUTH_THROTTLE_MESSAGE: &str = "too many sign-in attempts; try again in a minute";

fn lockout_error(retry: Duration) -> ApiError {
    ApiError::RateLimited(format!(
        "too many failed sign-in attempts; retry in {}s",
        retry.as_secs().max(1)
    ))
}

async fn throttle(
    state: &AppState,
    key: &str,
    kind: &str,
    limit: i32,
    message: &str,
) -> Result<(), ApiError> {
    consume_message(
        &state.pool,
        &format!("auth:{key}"),
        kind,
        &minute_bucket(chrono::Utc::now()),
        limit,
        Some(message),
    )
    .await
    .map(|_| ())
}

fn account_plan(state: &AppState, email: &str) -> &'static str {
    if state
        .config
        .pro_emails
        .iter()
        .any(|item| item.eq_ignore_ascii_case(email))
    {
        "pro"
    } else {
        "free"
    }
}

fn gateway_base(state: &AppState, dispatch: &Dispatch) -> String {
    if dispatch.upstream_kind != "worker_gateway" || state.config.worker_gateways.is_empty() {
        return dispatch.upstream_base.clone();
    }
    let index = state.gateway_cursor.fetch_add(1, Ordering::Relaxed);
    state.config.worker_gateways[index % state.config.worker_gateways.len()].clone()
}

async fn find_model(state: &AppState, requested: &str) -> Result<Option<ModelRow>, ApiError> {
    let query = format!("SELECT {MODEL_SELECT_COLUMNS} FROM models WHERE id = $1");
    let mut exact = sqlx::query_as::<_, ModelRow>(&query)
        .bind(requested)
        .fetch_optional(&state.pool)
        .await?;
    if exact.is_none() {
        if let Some((_, short_id)) = requested.split_once('/') {
            let varynth_prefixed = format!("varynth/{short_id}");
            if varynth_prefixed != requested {
                exact = sqlx::query_as::<_, ModelRow>(&query)
                    .bind(varynth_prefixed)
                    .fetch_optional(&state.pool)
                    .await?;
            }
        } else {
            let prefixed = format!("varynth/{requested}");
            exact = sqlx::query_as::<_, ModelRow>(&query)
                .bind(prefixed)
                .fetch_optional(&state.pool)
                .await?;
        }
    }
    if let Some(ref mut row) = exact {
        if row.id.starts_with("varynth/public-")
            && (state.config.enable_public_placeholders || state.config.worker_gateway_key.is_some())
        {
            row.enabled = true;
        }
    }
    Ok(exact)
}

async fn find_catalog(state: &AppState, requested: &str) -> Result<Option<CatalogModel>, ApiError> {
    if let Some((provider, model_id)) = requested.split_once('/') {
        if provider == "varynth" {
            return Ok(None);
        }
        let row = sqlx::query_as::<_, CatalogModel>(
            "SELECT pm.provider_id, pc.kind, pm.model_id
             FROM provider_models pm
             JOIN provider_catalog pc ON pc.id = pm.provider_id
             WHERE pm.provider_id = $1 AND pm.model_id = $2",
        )
        .bind(provider)
        .bind(model_id)
        .fetch_optional(&state.pool)
        .await?;
        return Ok(row);
    }
    let rows = sqlx::query_as::<_, CatalogModel>(
        "SELECT pm.provider_id, pc.kind, pm.model_id
         FROM provider_models pm
         JOIN provider_catalog pc ON pc.id = pm.provider_id
         WHERE pm.model_id = $1",
    )
    .bind(requested)
    .fetch_all(&state.pool)
    .await?;
    match rows.len() {
        0 => Ok(None),
        1 => Ok(rows.into_iter().next()),
        _ => Err(ApiError::bad("model is ambiguous")),
    }
}

async fn validate_authorize(state: &AppState, body: &ApproveBody) -> Result<(), ApiError> {
    if body.response_type != "code" || body.code_challenge_method != "S256" {
        return Err(ApiError::bad("unsupported authorize request"));
    }
    if body.client_id != "varynth-cli" || !valid_loopback_redirect(&body.redirect_uri) {
        return Err(ApiError::bad("redirect_uri is invalid"));
    }
    if !valid_state(&body.state) || !valid_challenge(&body.code_challenge) {
        return Err(ApiError::bad("pkce challenge is invalid"));
    }
    let known = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_clients WHERE client_id = $1")
        .bind(&body.client_id)
        .fetch_one(&state.pool)
        .await?;
    if known == 0 {
        return Err(ApiError::bad("unknown client"));
    }
    Ok(())
}

async fn authorization_code(state: &AppState, body: &TokenBody) -> Result<Json<Value>, ApiError> {
    let code = body.code.as_deref().unwrap_or("");
    let redirect = body.redirect_uri.as_deref().unwrap_or("");
    let client_id = body.client_id.as_deref().unwrap_or("");
    let verifier = body.code_verifier.as_deref().unwrap_or("");
    if client_id != "varynth-cli" || !valid_loopback_redirect(redirect) || !valid_challenge(verifier) {
        return Err(ApiError::bad("invalid_grant"));
    }
    let row = sqlx::query_as::<_, CodeRow>(
        "UPDATE auth_codes SET used_at = now()
         WHERE code = $1 AND used_at IS NULL AND expires_at > now()
         RETURNING user_id, client_id, redirect_uri, code_challenge",
    )
    .bind(code)
    .fetch_optional(&state.pool)
    .await?;
    let Some(row) = row else {
        return Err(ApiError::bad("invalid_grant"));
    };
    if row.client_id != client_id || row.redirect_uri != redirect || pkce_s256(verifier) != row.code_challenge {
        return Err(ApiError::bad("invalid_grant"));
    }
    issue_cli_token(state, row.user_id).await
}

async fn refresh(state: &AppState, body: &TokenBody) -> Result<Json<Value>, ApiError> {
    let token = body.refresh_token.as_deref().unwrap_or("");
    if token.is_empty() {
        return Err(ApiError::bad("invalid_grant"));
    }
    let hash = sha256(token.as_bytes());
    let row = sqlx::query_as::<_, RefreshRow>(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()
         RETURNING user_id",
    )
    .bind(hash)
    .fetch_optional(&state.pool)
    .await?;
    let Some(row) = row else {
        return Err(ApiError::bad("invalid_grant"));
    };
    issue_cli_token(state, row.user_id).await
}

async fn issue_cli_token(state: &AppState, user_id: Uuid) -> Result<Json<Value>, ApiError> {
    let access = sign_token(&state.config.jwt_secret, &user_id.to_string(), "cli", ACCESS_TTL_SECS)?;
    let refresh = random_token();
    let hash = sha256(refresh.as_bytes());
    sqlx::query(
        "INSERT INTO refresh_tokens (id, user_id, token_hash, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hash)
    .bind(expiry_from_now(REFRESH_TTL_SECS))
    .execute(&state.pool)
    .await?;
    Ok(Json(json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": ACCESS_TTL_SECS,
        "token_type": "Bearer"
    })))
}

#[derive(sqlx::FromRow)]
struct UserSecret {
    id: Uuid,
    email: String,
    password_hash: Option<String>,
}

#[derive(sqlx::FromRow)]
struct CodeRow {
    user_id: Uuid,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
}

#[derive(sqlx::FromRow)]
struct RefreshRow {
    user_id: Uuid,
}

#[derive(sqlx::FromRow)]
struct ApiKeyRow {
    id: Uuid,
    name: String,
    key_prefix: String,
    created_at: chrono::DateTime<chrono::Utc>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize)]
struct CreateKeyBody {
    name: String,
}

#[derive(Deserialize)]
struct RevokeKeyBody {
    id: String,
}

async fn list_api_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let user_id = parse_user(&user.sub)?;
    let rows = sqlx::query_as::<_, ApiKeyRow>(
        "SELECT id, name, key_prefix, created_at, last_used_at, revoked_at
         FROM api_keys
         WHERE user_id = $1
         ORDER BY created_at DESC
         LIMIT 50",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "name": row.name,
                "key_prefix": row.key_prefix,
                "created_at": row.created_at.to_rfc3339(),
                "last_used_at": row.last_used_at.map(|t| t.to_rfc3339()),
                "revoked": row.revoked_at.is_some()
            })
        })
        .collect();

    Ok(Json(json!({ "object": "list", "data": items })))
}

async fn create_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateKeyBody>,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let user_id = parse_user(&user.sub)?;
    let name = body.name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err(ApiError::bad("key name must be between 1 and 80 characters"));
    }

    let active_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM api_keys WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await?;

    if active_count >= 20 {
        return Err(ApiError::bad("maximum of 20 active API keys reached; revoke an unused key first"));
    }

    let raw_secret = random_token();
    let full_key = format!("vk_live_{raw_secret}");
    let prefix = full_key.chars().take(16).collect::<String>();
    let hash = sha256(full_key.as_bytes());
    let key_id = Uuid::new_v4();
    let now = chrono::Utc::now();

    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_prefix, key_hash, created_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(key_id)
    .bind(user_id)
    .bind(name)
    .bind(&prefix)
    .bind(hash)
    .bind(now)
    .execute(&state.pool)
    .await?;

    Ok(Json(json!({
        "id": key_id,
        "name": name,
        "key_prefix": prefix,
        "secret_key": full_key,
        "created_at": now.to_rfc3339()
    })))
}

async fn revoke_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RevokeKeyBody>,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let user_id = parse_user(&user.sub)?;
    let key_id = Uuid::parse_str(body.id.trim()).map_err(|_| ApiError::bad("invalid key id"))?;

    let updated = sqlx::query(
        "UPDATE api_keys SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(key_id)
    .bind(user_id)
    .execute(&state.pool)
    .await?;

    Ok(Json(json!({
        "id": key_id,
        "revoked": updated.rows_affected() > 0
    })))
}

#[derive(sqlx::FromRow)]
struct CliSessionRow {
    id: Uuid,
    session_key: String,
    title: String,
    model: String,
    workspace: String,
    message_count: i32,
    preview: String,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
struct UpsertSessionBody {
    session_key: String,
    title: String,
    model: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    message_count: i32,
    #[serde(default)]
    preview: String,
}

#[derive(Deserialize)]
struct DeleteSessionBody {
    session_key: String,
}

async fn list_cli_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let user_id = parse_user(&user.sub)?;

    let rows = sqlx::query_as::<_, CliSessionRow>(
        "SELECT id, session_key, title, model, workspace, message_count, preview, updated_at
         FROM cli_sessions
         WHERE user_id = $1
         ORDER BY updated_at DESC
         LIMIT 50",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "session_key": row.session_key,
                "title": row.title,
                "model": row.model,
                "workspace": row.workspace,
                "message_count": row.message_count,
                "preview": row.preview,
                "updated_at": row.updated_at.to_rfc3339()
            })
        })
        .collect();

    Ok(Json(json!({ "object": "list", "data": items })))
}

async fn upsert_cli_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UpsertSessionBody>,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let user_id = parse_user(&user.sub)?;

    let session_key = body.session_key.trim();
    if session_key.is_empty() || session_key.len() > 120 {
        return Err(ApiError::bad("session_key is required"));
    }
    let title = if body.title.trim().is_empty() {
        "Untitled CLI Session"
    } else {
        body.title.trim()
    };
    let model = if body.model.trim().is_empty() {
        "varynth/gpt-oss-120b"
    } else {
        body.model.trim()
    };
    let workspace: String = body.workspace.trim().chars().take(240).collect();
    let preview: String = body.preview.trim().chars().take(500).collect();
    let msg_count = body.message_count.clamp(0, 100_000);
    let now = chrono::Utc::now();

    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cli_sessions (id, user_id, session_key, title, model, workspace, message_count, preview, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (user_id, session_key)
         DO UPDATE SET
           title = EXCLUDED.title,
           model = EXCLUDED.model,
           workspace = EXCLUDED.workspace,
           message_count = EXCLUDED.message_count,
           preview = EXCLUDED.preview,
           updated_at = EXCLUDED.updated_at
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(session_key)
    .bind(title)
    .bind(model)
    .bind(&workspace)
    .bind(msg_count)
    .bind(&preview)
    .bind(now)
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(json!({
        "id": id,
        "session_key": session_key,
        "updated_at": now.to_rfc3339()
    })))
}

async fn delete_cli_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DeleteSessionBody>,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers, &["cli", "web"]).await?;
    let user_id = parse_user(&user.sub)?;
    let deleted = sqlx::query(
        "DELETE FROM cli_sessions WHERE user_id = $1 AND session_key = $2",
    )
    .bind(user_id)
    .bind(body.session_key.trim())
    .execute(&state.pool)
    .await?;

    Ok(Json(json!({
        "deleted": deleted.rows_affected() > 0
    })))
}

#[derive(Deserialize)]
struct UpsertByokBody {
    provider_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    models: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct DeleteByokBody {
    provider_id: String,
}

async fn list_byok_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let claims = require_user(&state, &headers, &["web", "cli"]).await?;
    let user_id = parse_user(&claims.sub)?;

    let rows = sqlx::query_as::<_, (Uuid, String, String, String, String, String, String, bool, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, provider_id, name, api_key, base_url, models, protocol, enabled, created_at, updated_at
         FROM user_byok_providers
         WHERE user_id = $1
         ORDER BY created_at ASC",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;

    let list: Vec<Value> = rows
        .into_iter()
        .map(|(id, provider_id, name, api_key, base_url, models_str, protocol, enabled, created_at, updated_at)| {
            let has_key = !api_key.trim().is_empty();
            let key_masked = if has_key {
                let k = api_key.trim();
                if k.len() > 8 {
                    format!("{}...{}", &k[..4], &k[k.len() - 4..])
                } else {
                    "••••••••".to_string()
                }
            } else {
                String::new()
            };
            let model_list = default_or_custom_models(&provider_id, &models_str);
            json!({
                "id": id,
                "provider_id": provider_id,
                "name": name,
                "has_key": has_key,
                "key_masked": key_masked,
                "base_url": base_url,
                "models": model_list,
                "models_raw": models_str,
                "protocol": protocol,
                "enabled": enabled,
                "created_at": created_at.to_rfc3339(),
                "updated_at": updated_at.to_rfc3339()
            })
        })
        .collect();

    Ok(Json(json!({ "providers": list })))
}

async fn upsert_byok_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UpsertByokBody>,
) -> Result<Json<Value>, ApiError> {
    let claims = require_user(&state, &headers, &["web", "cli"]).await?;
    let user_id = parse_user(&claims.sub)?;

    let provider_id = body.provider_id.trim();
    if provider_id.is_empty() || provider_id.len() > 64 {
        return Err(ApiError::bad("provider_id is invalid"));
    }

    let name = body.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
        match provider_id {
            "openai" => "OpenAI".into(),
            "anthropic" => "Anthropic Claude".into(),
            "gemini" => "Google Gemini".into(),
            "deepseek" => "DeepSeek".into(),
            "openrouter" => "OpenRouter".into(),
            "groq" => "Groq LPU".into(),
            _ => "Custom API".into(),
        }
    });

    let base_url = body.base_url.unwrap_or_default().trim().to_string();
    let models = body.models.unwrap_or_default().trim().to_string();
    let protocol = body.protocol.unwrap_or_else(|| "openai".into()).trim().to_string();
    let enabled = body.enabled.unwrap_or(true);
    let new_key = body.api_key.unwrap_or_default().trim().to_string();

    let id = Uuid::new_v4();

    let existing = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, api_key FROM user_byok_providers WHERE user_id = $1 AND provider_id = $2",
    )
    .bind(user_id)
    .bind(provider_id)
    .fetch_optional(&state.pool)
    .await?;

    if let Some((existing_id, existing_key)) = existing {
        let final_key = if new_key.is_empty() { existing_key } else { new_key };
        sqlx::query(
            "UPDATE user_byok_providers
             SET name = $1, api_key = $2, base_url = $3, models = $4, protocol = $5, enabled = $6, updated_at = now()
             WHERE id = $7",
        )
        .bind(&name)
        .bind(&final_key)
        .bind(&base_url)
        .bind(&models)
        .bind(&protocol)
        .bind(enabled)
        .bind(existing_id)
        .execute(&state.pool)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO user_byok_providers (id, user_id, provider_id, name, api_key, base_url, models, protocol, enabled)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(user_id)
        .bind(provider_id)
        .bind(&name)
        .bind(&new_key)
        .bind(&base_url)
        .bind(&models)
        .bind(&protocol)
        .bind(enabled)
        .execute(&state.pool)
        .await?;
    }

    Ok(Json(json!({ "ok": true, "provider_id": provider_id })))
}

async fn delete_byok_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DeleteByokBody>,
) -> Result<Json<Value>, ApiError> {
    let claims = require_user(&state, &headers, &["web", "cli"]).await?;
    let user_id = parse_user(&claims.sub)?;

    sqlx::query(
        "DELETE FROM user_byok_providers WHERE user_id = $1 AND provider_id = $2",
    )
    .bind(user_id)
    .bind(body.provider_id.trim())
    .execute(&state.pool)
    .await?;

    Ok(Json(json!({ "ok": true })))
}

async fn oauth_providers_status() -> Result<Json<Value>, ApiError> {
    let github_configured = std::env::var("GITHUB_CLIENT_ID")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let google_configured = std::env::var("GOOGLE_CLIENT_ID")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);

    Ok(Json(json!({
        "github": {
            "enabled": github_configured,
            "client_id": std::env::var("GITHUB_CLIENT_ID").ok(),
            "authorize_url": std::env::var("GITHUB_CLIENT_ID").ok().map(|cid| {
                format!("https://github.com/login/oauth/authorize?client_id={cid}&scope=user:email")
            })
        },
        "google": {
            "enabled": google_configured,
            "client_id": std::env::var("GOOGLE_CLIENT_ID").ok(),
            "authorize_url": std::env::var("GOOGLE_CLIENT_ID").ok().map(|cid| {
                format!("https://accounts.google.com/o/oauth2/v2/auth?client_id={cid}&response_type=code&scope=openid%20email%20profile")
            })
        }
    })))
}

#[derive(Deserialize)]
struct OAuthExchangeBody {
    code: String,
    redirect_uri: String,
}

async fn oauth_google_exchange(
    State(state): State<AppState>,
    Json(body): Json<OAuthExchangeBody>,
) -> Result<Response, ApiError> {
    let client_id = std::env::var("GOOGLE_CLIENT_ID")
        .map_err(|_| ApiError::bad("GOOGLE_CLIENT_ID is not configured"))?;
    let client_secret = std::env::var("GOOGLE_CLIENT_SECRET")
        .map_err(|_| ApiError::bad("GOOGLE_CLIENT_SECRET is not configured"))?;

    let client = reqwest::Client::new();
    let token_res = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", body.code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", body.redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| ApiError::bad(format!("google token error: {e}")))?;

    if !token_res.status().is_success() {
        let err_text = token_res.text().await.unwrap_or_default();
        return Err(ApiError::bad(format!("google exchange failed: {err_text}")));
    }

    let token_data: serde_json::Value = token_res
        .json()
        .await
        .map_err(|e| ApiError::bad(format!("google json parse error: {e}")))?;

    let access_token = token_data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad("missing access_token from google"))?;

    let userinfo_res = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| ApiError::bad(format!("google userinfo error: {e}")))?;

    let userinfo: serde_json::Value = userinfo_res
        .json()
        .await
        .map_err(|e| ApiError::bad(format!("google userinfo parse error: {e}")))?;

    let raw_email = userinfo
        .get("email")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad("missing email from google profile"))?;

    let email = normalize_email(raw_email)?;
    let plan = account_plan(&state, &email);

    let id = Uuid::new_v4();
    let user_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO users (id, email, password_hash, plan)
         VALUES ($1, $2, NULL, $3)
         ON CONFLICT (email) DO UPDATE SET updated_at = now()
         RETURNING id",
    )
    .bind(id)
    .bind(&email)
    .bind(plan)
    .fetch_one(&state.pool)
    .await?;

    let token = sign_token(&state.config.jwt_secret, &user_id.to_string(), "web", WEB_TTL_SECS)?;
    let cookie = session_cookie(&token, state.config.secure_cookies(), WEB_TTL_SECS);

    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"session_token": token, "expires_in": WEB_TTL_SECS, "email": email})),
    )
        .into_response())
}

async fn oauth_github_exchange(
    State(state): State<AppState>,
    Json(body): Json<OAuthExchangeBody>,
) -> Result<Response, ApiError> {
    let client_id = std::env::var("GITHUB_CLIENT_ID")
        .map_err(|_| ApiError::bad("GITHUB_CLIENT_ID is not configured"))?;
    let client_secret = std::env::var("GITHUB_CLIENT_SECRET")
        .map_err(|_| ApiError::bad("GitHub Client Secret henüz yapılandırılmadı. Lütfen GitHub Developer Settings üzerinden Client Secret oluşturup ekleyin."))?;

    let client = reqwest::Client::new();
    let token_res = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", body.code.as_str()),
            ("redirect_uri", body.redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| ApiError::bad(format!("github token error: {e}")))?;

    if !token_res.status().is_success() {
        let err_text = token_res.text().await.unwrap_or_default();
        return Err(ApiError::bad(format!("github exchange failed: {err_text}")));
    }

    let token_data: serde_json::Value = token_res
        .json()
        .await
        .map_err(|e| ApiError::bad(format!("github json parse error: {e}")))?;

    let access_token = token_data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad("missing access_token from github"))?;

    let emails_res = client
        .get("https://api.github.com/user/emails")
        .header("User-Agent", "varynth-oauth")
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| ApiError::bad(format!("github emails error: {e}")))?;

    let emails: serde_json::Value = emails_res
        .json()
        .await
        .map_err(|e| ApiError::bad(format!("github emails parse error: {e}")))?;

    let primary_email = emails
        .as_array()
        .and_then(|list| {
            list.iter()
                .find(|row| row.get("primary").and_then(|v| v.as_bool()).unwrap_or(false))
                .or_else(|| list.first())
        })
        .and_then(|row| row.get("email").and_then(|v| v.as_str()))
        .ok_or_else(|| ApiError::bad("no primary email found on github account"))?;

    let email = normalize_email(primary_email)?;
    let plan = account_plan(&state, &email);

    let id = Uuid::new_v4();
    let user_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO users (id, email, password_hash, plan)
         VALUES ($1, $2, NULL, $3)
         ON CONFLICT (email) DO UPDATE SET updated_at = now()
         RETURNING id",
    )
    .bind(id)
    .bind(&email)
    .bind(plan)
    .fetch_one(&state.pool)
    .await?;

    let token = sign_token(&state.config.jwt_secret, &user_id.to_string(), "web", WEB_TTL_SECS)?;
    let cookie = session_cookie(&token, state.config.secure_cookies(), WEB_TTL_SECS);

    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"session_token": token, "expires_in": WEB_TTL_SECS, "email": email})),
    )
        .into_response())
}

async fn require_user(
    state: &AppState,
    headers: &HeaderMap,
    scopes: &[&str],
) -> Result<Claims, ApiError> {
    let claims = optional_user(state, headers)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if scopes.iter().any(|scope| scope == &claims.scope) {
        Ok(claims)
    } else {
        Err(ApiError::Unauthorized)
    }
}

async fn optional_user(state: &AppState, headers: &HeaderMap) -> Result<Option<Claims>, ApiError> {
    let Some(token) = session_token(headers) else {
        return Ok(None);
    };
    if token.starts_with("vk_live_") {
        let hash = sha256(token.as_bytes());
        let user_id = sqlx::query_scalar::<_, Uuid>(
            "UPDATE api_keys SET last_used_at = now()
             WHERE key_hash = $1 AND revoked_at IS NULL
             RETURNING user_id",
        )
        .bind(hash)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::Unauthorized)?;

        return Ok(Some(Claims {
            sub: user_id.to_string(),
            scope: "cli".to_string(),
            iat: 0,
            exp: usize::MAX,
        }));
    }
    Ok(Some(read_token(&state.config.jwt_secret, &token)?))
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = value.strip_prefix("Bearer ") {
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        // Accept both names so an http deployment and an https deployment
        // can share a session across a cookie-flag rollout.
        for name in [HOST_SESSION_COOKIE, SESSION_COOKIE] {
            if let Some(token) = part.strip_prefix(name) {
                if let Some(token) = token.strip_prefix('=') {
                    if !token.is_empty() {
                        return Some(token.to_string());
                    }
                }
            }
        }
    }
    None
}

const SESSION_COOKIE: &str = "varynth_session";
const HOST_SESSION_COOKIE: &str = "__Host-varynth_session";

fn session_cookie_name(secure: bool) -> &'static str {
    if secure {
        HOST_SESSION_COOKIE
    } else {
        SESSION_COOKIE
    }
}

/// Secure mode uses the `__Host-` prefix, which requires `Secure`, `Path=/`
/// and no `Domain`; the attributes below satisfy that contract.
fn session_cookie(token: &str, secure: bool, max_age: i64) -> String {
    let name = session_cookie_name(secure);
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age={max_age}{secure}")
}

fn normalize_email(email: &str) -> Result<String, ApiError> {
    let email = email.trim().to_lowercase();
    if !(3..=320).contains(&email.len()) || !email.contains('@') || email.contains(char::is_whitespace) {
        return Err(ApiError::bad("email is invalid"));
    }
    Ok(email)
}

const MIN_PASSWORD_CHARS: usize = 12;

fn check_password(password: &str) -> Result<(), ApiError> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_CHARS || password.len() > 200 {
        return Err(ApiError::bad(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters and at most 200 bytes"
        )));
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| {
            tracing::error!(%error, "password hash");
            ApiError::Internal
        })
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .ok()
        .and_then(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .ok()
        })
        .is_some()
}

const DUMMY_PASSWORD: &str = "varynth-dummy-password-not-a-real-account";

/// Performs one real Argon2 verification against a throwaway hash so that
/// unknown-account logins cost the same as wrong-password logins.
fn verify_unknown_password(password: &str) {
    static DUMMY_HASH: LazyLock<String> =
        LazyLock::new(|| hash_password(DUMMY_PASSWORD).unwrap_or_default());
    if DUMMY_HASH.is_empty() {
        return;
    }
    let _ = verify_password(password, &DUMMY_HASH);
}

fn parse_user(sub: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(sub).map_err(|_| ApiError::Unauthorized)
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 256
        && model
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '-' | '.' | ':'))
}

fn is_unique(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|db| db.code().map(|code| code == "23505"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IpCidr;
    use axum::http::HeaderValue;

    fn config(public_url: &str, trusted: &[&str]) -> Config {
        Config {
            database_url: String::new(),
            jwt_secret: String::new(),
            web_origin: String::new(),
            public_url: public_url.into(),
            bind: String::new(),
            workers_account_id: None,
            workers_token: None,
            openai_key: None,
            anthropic_key: None,
            openrouter_key: None,
            hrmn_base_url: "https://api.hrmn.tr/v1".into(),
            hrmn_api_key: None,
            worker_gateways: Vec::new(),
            worker_gateway_key: None,
            parallel_keys: Vec::new(),
            exa_keys: Vec::new(),
            firecrawl_keys: Vec::new(),
            pro_emails: Vec::new(),
            trusted_proxies: trusted.iter().filter_map(|item| IpCidr::parse(item)).collect(),
            stub_upstream: false,
            enable_public_placeholders: false,
            billing_stub: false,
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("ip")
    }

    fn peer(text: &str) -> Option<SocketAddr> {
        Some(SocketAddr::new(ip(text), 443))
    }

    fn with_xff(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_str(value).expect("header"));
        headers
    }

    #[test]
    fn secure_cookie_uses_host_prefix_and_secure_attribute() {
        let cookie = session_cookie("token-value", true, 3600);
        assert!(cookie.starts_with("__Host-varynth_session=token-value;"), "{cookie}");
        assert!(cookie.contains("; Secure"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(!cookie.contains("Domain"), "{cookie}");
    }

    #[test]
    fn plain_http_cookie_keeps_local_dev_shape() {
        let cookie = session_cookie("token-value", false, 0);
        assert!(cookie.starts_with("varynth_session=token-value;"), "{cookie}");
        assert!(!cookie.contains("Secure"), "{cookie}");
        assert!(!cookie.contains("__Host-"), "{cookie}");
    }

    #[test]
    fn session_token_accepts_both_cookie_names() {
        for name in [SESSION_COOKIE, HOST_SESSION_COOKIE] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("other=1; {name}=abc.def; trailing=2")).expect("cookie"),
            );
            assert_eq!(session_token(&headers).as_deref(), Some("abc.def"), "{name}");
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str("varynth_session_extra=abc").expect("cookie"),
        );
        assert!(session_token(&headers).is_none());
    }

    #[test]
    fn forwarded_for_is_ignored_without_a_trusted_proxy() {
        let cfg = config("http://127.0.0.1:8080", &[]);
        let headers = with_xff("198.51.100.7, 10.0.0.1");
        assert_eq!(client_ip(peer("203.0.113.9"), &headers, &cfg), Some(ip("203.0.113.9")));
    }

    #[test]
    fn trusted_proxy_takes_the_last_forwarded_hop() {
        let cfg = config("https://api.varynth.dev", &["10.0.0.1"]);
        let headers = with_xff("198.51.100.7, 203.0.113.9");
        assert_eq!(client_ip(peer("10.0.0.1"), &headers, &cfg), Some(ip("203.0.113.9")));
        // A spoofed first hop does not matter; the proxy-appended last hop wins.
        let spoof = with_xff("1.2.3.4, 5.5.5.5, 203.0.113.9");
        assert_eq!(client_ip(peer("10.0.0.1"), &spoof, &cfg), Some(ip("203.0.113.9")));
        // Trusted proxy without the header falls back to the peer.
        assert_eq!(client_ip(peer("10.0.0.1"), &HeaderMap::new(), &cfg), Some(ip("10.0.0.1")));
        // Untrusted peer cannot use the header even when trusted proxies exist.
        assert_eq!(
            client_ip(peer("203.0.113.9"), &headers, &cfg),
            Some(ip("203.0.113.9"))
        );
    }

    #[test]
    fn anonymous_subject_is_stable_while_xff_rotates() {
        let cfg = config("http://127.0.0.1:8080", &[]);
        let subject = |xff: &str| usage_subject(None, &with_xff(xff), peer("203.0.113.9"), &cfg);
        assert_eq!(subject("198.51.100.1"), "anon:203.0.113.9");
        assert_eq!(subject("198.51.100.2"), "anon:203.0.113.9");
        // No peer at all (embedded tower) cannot be spoofed into a bucket.
        assert_eq!(
            usage_subject(None, &with_xff("198.51.100.3"), None, &cfg),
            "anon:local"
        );
    }

    #[test]
    fn password_policy_requires_twelve_characters() {
        let error = check_password("password123").expect_err("short");
        let ApiError::BadRequest(message) = error else {
            panic!("expected a bad-request error");
        };
        assert!(message.contains("12"), "{message}");
        assert!(check_password("password1234").is_ok());
    }

    #[test]
    fn dummy_hash_is_real_argon2_and_never_verifies() {
        let started = std::time::Instant::now();
        verify_unknown_password("any-candidate-password");
        assert!(
            started.elapsed() >= Duration::from_millis(1),
            "dummy path must run Argon2, took {:?}",
            started.elapsed()
        );
        let hash = hash_password(DUMMY_PASSWORD).expect("hash");
        assert!(hash.starts_with("$argon2"));
        assert!(!verify_password("any-candidate-password", &hash));
        assert!(verify_password(DUMMY_PASSWORD, &hash));
    }

    #[test]
    fn skills_entitlement_filter_masks_secret_content() {
        let items = [
            ("computer-use", "free", "Secret CU"),
            ("motion-fream-pro", "pro", "Secret Pro"),
        ];

        // Free plan caller:
        let is_pro = false;
        let free_view: Vec<Value> = items
            .iter()
            .map(|(id, tier, content)| {
                let unlocked = *tier == "free" || is_pro;
                json!({
                    "id": id,
                    "tier": tier,
                    "unlocked": unlocked,
                    "content": if unlocked { Some(content) } else { None }
                })
            })
            .collect();

        assert_eq!(free_view[0]["content"], "Secret CU");
        assert!(free_view[1]["content"].is_null());
        assert_eq!(free_view[1]["unlocked"], false);

        // Pro plan caller:
        let is_pro = true;
        let pro_view: Vec<Value> = items
            .iter()
            .map(|(id, tier, content)| {
                let unlocked = *tier == "free" || is_pro;
                json!({
                    "id": id,
                    "tier": tier,
                    "unlocked": unlocked,
                    "content": if unlocked { Some(content) } else { None }
                })
            })
            .collect();

        assert_eq!(pro_view[0]["content"], "Secret CU");
        assert_eq!(pro_view[1]["content"], "Secret Pro");
        assert_eq!(pro_view[1]["unlocked"], true);
    }
}
