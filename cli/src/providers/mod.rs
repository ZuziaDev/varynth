use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    Engine,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::session::{ChatMessage, ImageAttachment, ToolCall};

mod anthropic;
mod google;
mod openai;
mod proxy;

/// Token usage as reported by the provider. `input_tokens`/`output_tokens`
/// fall back to 0 when a provider omits the fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub images: Vec<ImageAttachment>,
    pub model: String,
    /// Provider-reported token usage; `None` when the response carries none.
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    pub display_name: Option<String>,
    pub owned_by: Option<String>,
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<Completion>;

    /// Streaming variant: surfaces text deltas through `on_delta` as they
    /// arrive and returns the full `Completion` at the end. The default
    /// implementation ignores the callback and delegates to `complete`, so
    /// providers (and test mocks) without SSE support keep working.
    async fn complete_streaming(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        let _ = on_delta;
        self.complete(model, system, messages, tools).await
    }

    async fn list_models(&self) -> Result<Vec<CatalogModel>>;
}

pub fn build(cfg: &Config) -> Result<Box<dyn Provider>> {
    let effective = cfg.active_provider_config()?;
    let provider = build_single(&effective)?;
    if cfg.provider_profiles.is_empty() {
        return Ok(provider);
    }
    Ok(Box::new(ProfileProvider {
        cfg: cfg.clone(),
        active: provider,
        model: effective.model,
    }))
}

fn build_single(cfg: &Config) -> Result<Box<dyn Provider>> {
    match cfg.provider.as_str() {
        "anthropic" => Ok(Box::new(anthropic::AnthropicProvider::new(cfg)?)),
        "openai" => Ok(Box::new(openai::OpenAiProvider::new(cfg)?)),
        "proxy" => Ok(Box::new(proxy::ProxyProvider::new(cfg)?)),
        "google" | "google-vertex" | "google-antigravity" | "antigravity" => {
            Ok(Box::new(google::GoogleProvider::new(cfg)?))
        }
        "varynth" => {
            let mut cfg = cfg.clone();
            cfg.provider = "varynth".into();
            if cfg
                .openai_base_url
                .as_deref()
                .is_none_or(|url| url.trim().is_empty())
            {
                cfg.openai_base_url =
                    Some(format!("{}/v1", cfg.varynth_api_url.trim_end_matches('/')));
            }
            if cfg.openai_api_key.is_none() {
                if let Ok(Some(toks)) = crate::varynth_auth::load_auth_env() {
                    cfg.openai_api_key = Some(toks.access_token);
                } else if let Ok(value) = std::env::var("VARYNTH_ACCESS_TOKEN") {
                    if !value.trim().is_empty() {
                        cfg.openai_api_key = Some(value);
                    }
                }
            }
            Ok(Box::new(openai::OpenAiProvider::new(&cfg)?))
        }
        _ => bail!(
            "unknown provider (proxy | anthropic | openai | google | google-vertex | google-antigravity | antigravity | varynth)"
        ),
    }
}

struct ProfileProvider {
    cfg: Config,
    active: Box<dyn Provider>,
    model: String,
}

#[async_trait::async_trait]
impl Provider for ProfileProvider {
    async fn complete(
        &self,
        _model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<Completion> {
        self.active
            .complete(&self.model, system, messages, tools)
            .await
    }

    async fn complete_streaming(
        &self,
        _model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        self.active
            .complete_streaming(&self.model, system, messages, tools, on_delta)
            .await
    }

    async fn list_models(&self) -> Result<Vec<CatalogModel>> {
        let mut out = Vec::new();
        let mut failures = 0;
        let mut seen = std::collections::HashSet::new();
        if self.cfg.selected_profile_id().is_none() {
            match tokio::time::timeout(Duration::from_secs(3), self.active.list_models()).await {
                Ok(Ok(models)) => {
                    for model in models {
                        if seen.insert(model.id.clone()) {
                            out.push(model);
                        }
                    }
                }
                _ => failures += 1,
            }
        }
        // The active profile is fetched first so its live catalog survives the
        // caller's timeout even when another configured endpoint hangs.
        let active_id = self.cfg.selected_profile_id().map(str::to_string);
        let mut profiles: Vec<&crate::config::ProviderProfile> = self
            .cfg
            .provider_profiles
            .iter()
            .filter(|profile| profile.enabled)
            .collect();
        profiles
            .sort_by_key(|profile| usize::from(Some(profile.id.as_str()) != active_id.as_deref()));
        for profile in profiles {
            let mut models = configured_catalog(&profile.models)?;
            let fetched = async {
                let cfg = self.cfg.profile_config(profile)?;
                cfg.require_provider_credentials()?;
                build_single(&cfg)?.list_models().await
            };
            match tokio::time::timeout(Duration::from_secs(3), fetched).await {
                Ok(Ok(live)) => models.extend(live),
                _ => {
                    failures += 1;
                    tracing::warn!(profile = %profile.id, "provider catalog unavailable; retaining configured entries");
                }
            }
            for mut model in models {
                // Some endpoints (e.g. the Varynth gateway) already return
                // ids namespaced with the profile id; avoid `varynth/varynth/…`.
                let own_prefix = format!("{}/", profile.id);
                let native = model
                    .id
                    .strip_prefix(&own_prefix)
                    .unwrap_or(&model.id)
                    .to_string();
                model.id = format!("{}/{native}", profile.id);
                model.display_name = Some(format!(
                    "{} / {}",
                    profile.id,
                    model.display_name.as_deref().unwrap_or(&native)
                ));
                if seen.insert(model.id.clone()) {
                    out.push(model);
                }
            }
        }
        anyhow::ensure!(
            !out.is_empty() || failures == 0,
            "no provider model catalog is available"
        );
        Ok(out)
    }
}

pub(crate) fn api_v1_base(raw: &str) -> Result<String> {
    crate::config::validate_provider_url(raw)?;
    let base = raw.trim_end_matches('/');
    Ok(if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    })
}

pub(crate) fn parse_models_catalog(v: &Value) -> Result<Vec<CatalogModel>> {
    let entries = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("model catalog must contain a data array"))?;
    let mut out = Vec::new();
    for model in entries {
        let id = model
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| valid_provider_id(id))
            .ok_or_else(|| anyhow::anyhow!("model catalog contains an invalid model id"))?;
        out.push(CatalogModel {
            id: id.into(),
            display_name: optional_catalog_string(model, "display_name")?,
            owned_by: optional_catalog_string(model, "owned_by")?,
        });
    }
    Ok(out)
}

fn optional_catalog_string(model: &Value, field: &str) -> Result<Option<String>> {
    match model.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value))
            if value.len() <= 512 && !value.chars().any(char::is_control) =>
        {
            Ok(Some(value.clone()))
        }
        _ => bail!("model catalog contains an invalid {field}"),
    }
}

pub(crate) fn valid_provider_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
}

pub(crate) fn validate_request_ids(model: &str, messages: &[ChatMessage]) -> Result<()> {
    ensure!(valid_provider_id(model), "invalid provider model id");
    for message in messages {
        if message.role == "tool" {
            ensure!(
                message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(valid_provider_id),
                "invalid tool result id"
            );
        }
        if let Some(calls) = &message.tool_calls {
            ensure!(
                calls.iter().all(|call| valid_provider_id(&call.id)),
                "invalid tool call id"
            );
        }
    }
    Ok(())
}

pub(crate) fn configured_catalog(models: &[String]) -> Result<Vec<CatalogModel>> {
    ensure!(
        models.iter().all(|id| valid_provider_id(id)),
        "invalid configured model id"
    );
    Ok(models
        .iter()
        .map(|id| CatalogModel {
            id: id.clone(),
            display_name: None,
            owned_by: None,
        })
        .collect())
}

pub(crate) fn provider_http_error(provider: &str, status: u16, body: &str) -> anyhow::Error {
    let reason = match status {
        400 | 422 => "request rejected",
        401 => "authentication rejected",
        403 => "permission denied",
        404 => "model or endpoint unavailable",
        408 | 504 => "request timed out",
        429 => "rate limit exceeded",
        500..=599 => "service unavailable",
        300..=399 => "redirect refused",
        _ => "request failed",
    };
    let hint = if status == 404 {
        "; the provider does not serve this model at that path (run `varynth models`)"
    } else {
        ""
    };
    match upstream_error_detail(body) {
        Some(detail) => {
            // A gateway may echo the presented key in its error; the same
            // redactor used for exported transcripts keeps it out of the TUI.
            let detail = crate::export::redact(&detail);
            anyhow!("{provider} HTTP {status}: {reason}{hint}: {detail}")
        }
        None => anyhow!("{provider} HTTP {status}: {reason}{hint}"),
    }
}

/// Cap for an upstream error body: enough for a JSON error, small enough that
/// a hostile proxy cannot make the CLI buffer gigabytes.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// Reads at most `limit` bytes of a response body. Used for non-2xx replies,
/// whose content is only ever reduced to a one-line diagnostic.
pub(crate) async fn read_body_bounded(mut resp: reqwest::Response, limit: usize) -> String {
    let mut out: Vec<u8> = Vec::new();
    while out.len() < limit {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = limit - out.len();
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extracts a bounded, single-line message from an upstream error body so the
/// operator sees *why* a request was rejected. Provider bodies are untrusted:
/// control characters are dropped, whitespace is collapsed, and the result is
/// capped so a hostile or huge response cannot flood the TUI.
pub(crate) fn upstream_error_detail(body: &str) -> Option<String> {
    const MAX_CHARS: usize = 240;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .or_else(|| value.pointer("/error").and_then(Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_else(|| trimmed.to_string());
    let mut out = String::new();
    let mut last_space = false;
    for ch in candidate.chars() {
        if out.chars().count() >= MAX_CHARS - 1 {
            out.push('…');
            break;
        }
        // Cc plus the bidi/format controls that can visually reorder a line.
        let ch = if ch.is_control()
            || matches!(ch,
                '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}')
        {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if last_space || out.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

pub(crate) fn sanitized_transport_error(error: &reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("provider request timed out")
    } else if error.is_connect() {
        anyhow::anyhow!("provider connection failed")
    } else if error.is_decode() {
        anyhow::anyhow!("provider response body interrupted or failed to decode")
    } else if error.is_builder() {
        anyhow::anyhow!("provider request could not be built")
    } else {
        anyhow::anyhow!("provider request transport failed")
    }
}

pub(crate) fn sanitized_stream_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("provider stream timed out waiting for next chunk")
    } else if error.is_connect() {
        anyhow::anyhow!("provider stream connection lost")
    } else if error.is_decode() {
        anyhow::anyhow!("provider stream closed unexpectedly mid-response (connection reset by server or proxy)")
    } else {
        anyhow::anyhow!("provider stream error: {error}")
    }
}

/// A provider HTTP response reduced to its status code and body text.
pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Total send attempts per request: the first try plus 2 retries.
pub(crate) const MAX_ATTEMPTS: u32 = 3;

/// One parsed server-sent event: the `event:` field (empty when absent) and
/// the joined `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Incremental SSE parser. Feed raw network chunks — possibly split mid-line —
/// and complete events come back as soon as their terminating blank line
/// arrives. Handles `event:`/`data:` fields (multiple `data:` lines join with
/// `\n`), `:`-prefixed comment/heartbeat lines, CR, LF and CRLF terminators,
/// and buffers partial lines across chunks.
#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: String,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((line, consumed)) = Self::next_line(&self.buf) {
            self.buf.drain(..consumed);
            if line.is_empty() {
                // Blank line: dispatch the pending event (per spec, an event
                // without data is dropped), then reset the field buffers.
                if !self.data.is_empty() {
                    out.push(SseEvent {
                        event: std::mem::take(&mut self.event),
                        data: std::mem::take(&mut self.data),
                    });
                }
                self.event.clear();
                self.data.clear();
                continue;
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if line.starts_with(':') {
                continue; // comment / keep-alive heartbeat
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line.as_str(), ""),
            };
            match field {
                "event" => self.event = value.to_string(),
                "data" => {
                    if self.data.is_empty() {
                        self.data = value.to_string();
                    } else {
                        self.data.push('\n');
                        self.data.push_str(value);
                    }
                }
                _ => {} // id:, retry: and unknown fields
            }
        }
        out
    }

    /// Next complete line (terminator stripped) and the bytes it occupies,
    /// terminator included. UTF-8 is safe: `\n`/`\r` never appear inside a
    /// multi-byte character.
    fn next_line(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
        let pos = buf.iter().position(|&b| b == b'\n' || b == b'\r')?;
        let mut end = pos + 1;
        if buf[pos] == b'\r' && buf.get(end) == Some(&b'\n') {
            end += 1;
        }
        Some((buf[..pos].to_vec(), end))
    }
}

/// Sends a streaming request with the same retry/backoff policy as
/// `send_with_retry`, but returns the live response so the body can be
/// streamed. Retries happen only while nothing of the body was consumed.
pub(crate) async fn send_streaming_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let resp = match (build)().send().await {
            Ok(resp) => resp,
            Err(e) => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(sanitized_transport_error(&e));
                }
                let delay = backoff_delay(attempt);
                tracing::warn!(attempt, delay = ?delay, "provider stream request failed; retrying");
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        let status = resp.status().as_u16();
        if should_retry(status, attempt) {
            let delay = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_delay)
                .unwrap_or_else(|| backoff_delay(attempt));
            tracing::warn!(attempt, status, delay = ?delay, "provider stream returned retryable status; retrying");
            tokio::time::sleep(delay).await;
            continue;
        }
        return Ok(resp);
    }
}

/// Rejects non-2xx or non-SSE responses. Error bodies are reduced to one
/// bounded message by `upstream_error_detail`; successful bodies never leave
/// this function.
pub(crate) async fn ensure_sse_response(
    resp: reqwest::Response,
    provider: &str,
) -> Result<reqwest::Response> {
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if (200..300).contains(&status) && content_type.contains("text/event-stream") {
        return Ok(resp);
    }
    if !(200..300).contains(&status) {
        let body = read_body_bounded(resp, MAX_ERROR_BODY_BYTES).await;
        return Err(provider_http_error(provider, status, &body));
    }
    bail!("{provider} response was not an SSE stream")
}

/// Accumulates one OpenAI-style streaming response: text deltas, tool-call
/// deltas and the optional usage object some proxies send in the final chunk.
#[derive(Default)]
pub(crate) struct OpenAiStreamAcc {
    text: String,
    images: Vec<ImageAttachment>,
    error: Option<String>,
    tool_calls: Vec<OpenAiToolCallAcc>,
    usage: Option<Usage>,
    model: Option<String>,
}

#[derive(Default)]
struct OpenAiToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiStreamAcc {
    /// Applies one `data:` payload. Returns the text delta to surface (empty
    /// for `[DONE]`, junk, null content and tool-call-only chunks).
    pub(crate) fn apply(&mut self, data: &str) -> String {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return String::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return String::new();
        };
        if self.model.is_none() {
            self.model = v
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string);
        }
        if v.get("error").is_some_and(|error| !error.is_null()) {
            self.error = Some("OpenAI-compatible stream returned a provider error".into());
            return String::new();
        }
        let mut delta_text = String::new();
        if let Some(delta) = first_choice(&v).and_then(|c| c.get("delta")) {
            match parse_openai_message(delta) {
                Ok((text, images)) => {
                    self.text.push_str(&text);
                    delta_text = text;
                    self.images.extend(images);
                    if validate_output_images(&self.images).is_err() {
                        self.error = Some("OpenAI stream returned invalid image content".into());
                    }
                }
                Err(_) => {
                    self.error = Some("OpenAI stream returned malformed content".into());
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let Some(idx) = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.tool_calls.len() as u64)
                        .try_into()
                        .ok()
                        .filter(|idx: &usize| *idx < 512)
                    else {
                        self.error =
                            Some("OpenAI stream returned an invalid tool call index".into());
                        continue;
                    };
                    while self.tool_calls.len() <= idx {
                        self.tool_calls.push(OpenAiToolCallAcc::default());
                    }
                    let slot = &mut self.tool_calls[idx];
                    if let Some(id) = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    {
                        if !valid_provider_id(id) || (!slot.id.is_empty() && slot.id != id) {
                            self.error =
                                Some("OpenAI stream returned an invalid tool call id".into());
                        } else {
                            slot.id = id.to_string();
                        }
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        slot.name.push_str(name);
                    }
                    if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str)
                    {
                        slot.arguments.push_str(args);
                    }
                }
            }
        }
        if let Some(usage) = usage_from_openai(&v) {
            self.usage = Some(usage);
        }
        delta_text
    }

    /// The complete response once the stream has ended.
    pub(crate) fn finish(&self, fallback_model: &str) -> Completion {
        Completion {
            text: self.text.clone(),
            images: self.images.clone(),
            tool_calls: self
                .tool_calls
                .iter()
                .filter(|c| !c.id.is_empty() && !c.name.is_empty())
                .map(|c| ToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: serde_json::from_str(&c.arguments)
                        .unwrap_or(Value::Object(Default::default())),
                })
                .collect(),
            model: self
                .model
                .clone()
                .unwrap_or_else(|| fallback_model.to_string()),
            usage: self.usage,
        }
    }
}

/// Consumes an OpenAI-style SSE response body, surfacing each text delta
/// through `on_delta` and accumulating the final `Completion`.
pub(crate) async fn read_openai_stream(
    resp: reqwest::Response,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    fallback_model: &str,
) -> Result<Completion> {
    let mut parser = SseParser::new();
    let mut acc = OpenAiStreamAcc::default();
    let mut stream = std::pin::pin!(resp.bytes_stream());
    while let Some(chunk) = stream.try_next().await.map_err(sanitized_stream_error)? {
        for ev in parser.feed(&chunk) {
            let delta = acc.apply(&ev.data);
            if let Some(error) = &acc.error {
                bail!("{error}");
            }
            if !delta.is_empty() {
                on_delta(&delta);
            }
        }
    }
    let completion = acc.finish(fallback_model);
    ensure!(
        !completion.text.trim().is_empty()
            || !completion.tool_calls.is_empty()
            || !completion.images.is_empty(),
        "OpenAI stream returned an empty completion"
    );
    Ok(completion)
}

/// Whether a response status should be retried. `attempt` is the number of
/// attempts already made (1-based): 429 and 5xx are retryable while attempts
/// remain (attempt >= 3 gives up), every other status is final.
pub(crate) fn should_retry(status: u16, attempt: u32) -> bool {
    let retryable = status == 429 || (500..600).contains(&status);
    retryable && attempt < MAX_ATTEMPTS
}

/// Backoff between retries: ~500 ms after the first failure, ~2 s after that.
pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    if attempt <= 1 {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(2)
    }
}

/// Numeric `Retry-After` seconds, capped at 10 s. Non-numeric values (HTTP
/// dates) are ignored.
pub(crate) fn retry_after_delay(raw: &str) -> Option<Duration> {
    const CAP_SECS: u64 = 10;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs.min(CAP_SECS)))
}

/// Sends a request with retry + backoff on 429/5xx and connection/send errors.
/// `build` is called once per attempt because `RequestBuilder` is consumed by
/// `send()`.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<HttpResponse> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let resp = match (build)().send().await {
            Ok(resp) => resp,
            Err(e) => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(sanitized_transport_error(&e));
                }
                let delay = backoff_delay(attempt);
                tracing::warn!(attempt, delay = ?delay, "provider request failed; retrying");
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(retry_after_delay);
        let body = if (200..300).contains(&status) {
            resp.text()
                .await
                .map_err(|e| sanitized_transport_error(&e))?
        } else {
            read_body_bounded(resp, MAX_ERROR_BODY_BYTES).await
        };
        if should_retry(status, attempt) {
            let delay = retry_after.unwrap_or_else(|| backoff_delay(attempt));
            tracing::warn!(attempt, status, delay = ?delay, "provider returned retryable status; retrying");
            tokio::time::sleep(delay).await;
            continue;
        }
        return Ok(HttpResponse { status, body });
    }
}

/// Whether a response should trigger the graceful-degradation retry: a 400
/// whose body mentions the request parameter that was added.
pub(crate) fn needs_param_degrade(status: u16, body: &str, param: &str) -> bool {
    status == 400 && body.contains(param)
}

/// Maps the configured effort level to an OpenAI-style `reasoning_effort`
/// value. `xhigh`/`max`/`ultra` clamp to `"high"`; unknown values return
/// `None` (parameter omitted).
pub(crate) fn reasoning_effort(effort: &str) -> Option<&'static str> {
    match effort {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" | "ultra" => Some("high"),
        _ => None,
    }
}

fn usage_from(v: &Value, input_key: &str, output_key: &str) -> Option<Usage> {
    let u = v.get("usage")?;
    if u.is_null() {
        return None;
    }
    Some(Usage {
        input_tokens: u.get(input_key).and_then(Value::as_u64).unwrap_or(0),
        output_tokens: u.get(output_key).and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Parses Anthropic usage: `usage.input_tokens` / `usage.output_tokens`.
/// Returns `None` when the `usage` object itself is missing; absent fields
/// fall back to 0.
pub(crate) fn usage_from_anthropic(v: &Value) -> Option<Usage> {
    usage_from(v, "input_tokens", "output_tokens")
}

/// Parses OpenAI-compatible usage: `usage.prompt_tokens` /
/// `usage.completion_tokens`. Returns `None` when the `usage` object itself
/// is missing; absent fields fall back to 0.
pub(crate) fn usage_from_openai(v: &Value) -> Option<Usage> {
    usage_from(v, "prompt_tokens", "completion_tokens")
}

/// First element of a chat-completions `choices` array; `None` when missing
/// or empty (malformed response).
pub(crate) fn first_choice(v: &Value) -> Option<&Value> {
    v.get("choices")?.as_array()?.first()
}

/// Truncates to at most `n` bytes without splitting a UTF-8 character.
#[allow(dead_code)]
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

pub fn openai_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let content = if message.images.is_empty() {
                Value::String(message.content.clone())
            } else {
                let mut parts = vec![serde_json::json!({"type": "text", "text": message.content})];
                parts.extend(message.images.iter().map(|image| {
                    serde_json::json!({"type": "image_url", "image_url": {"url": image.data_url()}})
                }));
                Value::Array(parts)
            };
            let mut value = serde_json::json!({"role": message.role, "content": content});
            if let Some(id) = &message.tool_call_id {
                value["tool_call_id"] = serde_json::json!(id);
            }
            if let Some(calls) = &message.tool_calls {
                value["tool_calls"] =
                    serde_json::json!(calls.iter().map(|call| {
                    serde_json::json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments.to_string()}
                    })
                }).collect::<Vec<_>>());
            }
            value
        })
        .collect()
}

/// Validate provider-generated image attachments using the same bounded media
/// decoder used for user uploads, including batch count and decoded-size caps.
pub(crate) fn validate_output_images(images: &[ImageAttachment]) -> Result<()> {
    ensure!(
        images.len() <= crate::media::MAX_MEDIA_COUNT,
        "provider returned too many images"
    );
    let mut total = 0usize;
    for image in images {
        crate::media::validate_attachment(image)
            .map_err(|_| anyhow::anyhow!("invalid provider image attachment"))?;
        let bytes = decode_image_base64(&image.data)?;
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow::anyhow!("provider image size overflow"))?;
        ensure!(
            total <= crate::media::MAX_TOTAL_MEDIA_BYTES,
            "provider images exceed the combined decoded size limit"
        );
    }
    Ok(())
}

fn decode_image_base64(data: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(data)
        .or_else(|_| STANDARD_NO_PAD.decode(data))
        .context("decode image base64")
}

fn data_url_image(raw: &str) -> Result<Option<ImageAttachment>> {
    if !raw.starts_with("data:") {
        return Ok(None);
    }
    let (metadata, data) = raw
        .split_once(',')
        .ok_or_else(|| anyhow::anyhow!("malformed image data URL"))?;
    let mut fields = metadata[5..].split(';');
    let media_type = fields
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("image data URL is missing its MIME type"))?;
    ensure!(
        fields.any(|field| field.eq_ignore_ascii_case("base64")),
        "image data URL must use base64 encoding"
    );
    let image = ImageAttachment {
        media_type: media_type.to_string(),
        data: data.to_string(),
    };
    Ok(Some(image))
}

#[derive(Default)]
struct OpenAiContent {
    text: String,
    images: Vec<ImageAttachment>,
    external_images: usize,
}

fn parse_openai_content(content: &Value) -> Result<OpenAiContent> {
    let mut out = OpenAiContent::default();
    match content {
        Value::Null => {}
        Value::String(text) => out.text.push_str(text),
        Value::Array(parts) => {
            for part in parts {
                let kind = part
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("OpenAI content part is missing its type"))?;
                match kind {
                    "text" => out.text.push_str(
                        part.get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("OpenAI text part is malformed"))?,
                    ),
                    "image_url" => {
                        let url = part
                            .pointer("/image_url/url")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("OpenAI image part is malformed"))?;
                        match data_url_image(url)? {
                            Some(image) => out.images.push(image),
                            None => out.external_images += 1,
                        }
                    }
                    other => bail!("unsupported OpenAI content part type `{other}`"),
                }
            }
        }
        _ => bail!("OpenAI message content is malformed"),
    }
    Ok(out)
}

/// Parses an OpenAI-compatible assistant message, including OpenRouter's
/// `message.images` extension. External image URLs are deliberately never
/// fetched; they are ignored when usable text or data-URL images accompany
/// them, and rejected when they are the only proposed output.
pub(crate) fn parse_openai_message(message: &Value) -> Result<(String, Vec<ImageAttachment>)> {
    let mut parsed = parse_openai_content(message.get("content").unwrap_or(&Value::Null))?;
    if let Some(images) = message.get("images") {
        let images = images
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("OpenAI message images must be an array"))?;
        for image in images {
            let url = image
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("OpenAI message image is malformed"))?;
            match data_url_image(url)? {
                Some(image) => parsed.images.push(image),
                None => parsed.external_images += 1,
            }
        }
    }
    if parsed.text.trim().is_empty() && parsed.images.is_empty() && parsed.external_images > 0 {
        bail!("OpenAI returned only unsupported external image URLs")
    }
    validate_output_images(&parsed.images)?;
    Ok((parsed.text, parsed.images))
}
pub fn parse_openai_tool_calls(v: &Value) -> Vec<ToolCall> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|c| {
            let id = c.get("id")?.as_str()?.to_string();
            let name = c
                .pointer("/function/name")
                .and_then(|x| x.as_str())?
                .to_string();
            let args_raw = c
                .pointer("/function/arguments")
                .and_then(|x| x.as_str())
                .unwrap_or("{}");
            let arguments =
                serde_json::from_str(args_raw).unwrap_or(Value::Object(Default::default()));
            Some(ToolCall {
                id,
                name,
                arguments,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ImageAttachment;

    fn png() -> ImageAttachment {
        ImageAttachment {
            media_type: "image/png".into(),
            data: "QUJD".into(),
        }
    }

    #[test]
    fn openai_messages_keep_plain_text_without_images() {
        let msgs = openai_messages(&[ChatMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }]);
        assert_eq!(msgs[0]["content"], "hi");
    }

    #[test]
    fn openai_messages_send_images_as_content_parts() {
        let msgs = openai_messages(&[ChatMessage {
            role: "user".into(),
            content: "what is this?".into(),
            images: vec![png()],
            ..Default::default()
        }]);
        let parts = msgs[0]["content"].as_array().unwrap();
        assert_eq!(
            parts[0],
            serde_json::json!({"type":"text","text":"what is this?"})
        );
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn messages_without_images_field_still_deserialize() {
        let m: ChatMessage = serde_json::from_str(r#"{"role":"user","content":"old"}"#).unwrap();
        assert!(m.images.is_empty());
        let out = serde_json::to_string(&m).unwrap();
        assert!(!out.contains("images"));
    }

    #[test]
    fn upstream_error_detail_extracts_and_bounds_provider_messages() {
        assert_eq!(
            upstream_error_detail(r#"{"error":{"message":"model not found"}}"#),
            Some("model not found".into())
        );
        assert_eq!(
            upstream_error_detail(r#"{"message":"API anahtarı eksik"}"#),
            Some("API anahtarı eksik".into())
        );
        assert_eq!(upstream_error_detail("   "), None);
        assert_eq!(
            upstream_error_detail("bad\r\nrequest\tbody"),
            Some("bad request body".into())
        );
        let long = "x".repeat(1_000);
        let detail = upstream_error_detail(&long).unwrap();
        assert!(detail.chars().count() <= 240, "detail was not capped");
        let bidi = upstream_error_detail("a\u{202E}b\u{200B}c").unwrap();
        assert_eq!(bidi, "a b c");
    }

    #[test]
    fn provider_errors_redact_echoed_credentials() {
        let message = provider_http_error(
            "OpenAI",
            401,
            r#"{"error":{"message":"invalid key sk-abcdefgh12345"}}"#,
        )
        .to_string();
        assert!(!message.contains("sk-abcdefgh12345"), "{message}");
        assert!(
            message.contains("[REDACTED]") || message.contains("invalid key"),
            "{message}"
        );
    }

    #[test]
    fn not_found_errors_name_the_model_and_path_hint() {
        let message = provider_http_error("OpenAI", 404, "").to_string();
        assert!(message.contains("HTTP 404"), "{message}");
        assert!(message.contains("varynth models"), "{message}");
        let message = provider_http_error("OpenAI", 401, r#"{"error":{"message":"missing key"}}"#)
            .to_string();
        assert!(message.contains("missing key"), "{message}");
    }

    #[test]
    fn should_retry_only_retries_rate_limits_and_server_errors() {
        assert!(should_retry(429, 1));
        assert!(should_retry(500, 1));
        assert!(should_retry(502, 2));
        assert!(should_retry(503, 2));
        assert!(!should_retry(401, 1));
        assert!(!should_retry(400, 1));
        assert!(!should_retry(404, 2));
        assert!(!should_retry(429, 3));
        assert!(!should_retry(500, 4));
        assert!(!should_retry(200, 1));
    }

    #[test]
    fn backoff_is_500ms_then_2s() {
        assert_eq!(backoff_delay(1), Duration::from_millis(500));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(2));
    }

    #[test]
    fn retry_after_is_numeric_seconds_capped_at_10s() {
        assert_eq!(retry_after_delay("3"), Some(Duration::from_secs(3)));
        assert_eq!(retry_after_delay(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(retry_after_delay("30"), Some(Duration::from_secs(10)));
        assert_eq!(retry_after_delay("soon"), None);
        assert_eq!(retry_after_delay("-2"), None);
    }

    #[test]
    fn degrade_only_for_400_mentioning_the_parameter() {
        assert!(needs_param_degrade(
            400,
            r#"{"error":"Unsupported parameter: reasoning_effort"}"#,
            "reasoning_effort"
        ));
        assert!(!needs_param_degrade(401, "mentions thinking", "thinking"));
        assert!(!needs_param_degrade(
            400,
            r#"{"error":"bad request"}"#,
            "thinking"
        ));
    }

    #[test]
    fn reasoning_effort_maps_xhigh_and_max_to_high() {
        assert_eq!(reasoning_effort("low"), Some("low"));
        assert_eq!(reasoning_effort("medium"), Some("medium"));
        assert_eq!(reasoning_effort("high"), Some("high"));
        assert_eq!(reasoning_effort("xhigh"), Some("high"));
        assert_eq!(reasoning_effort("max"), Some("high"));
        assert_eq!(reasoning_effort("ultra"), Some("high"));
        assert_eq!(reasoning_effort("turbo"), None);
    }

    #[test]
    fn usage_parses_provider_field_names() {
        let v = serde_json::json!({"usage": {"prompt_tokens": 11, "completion_tokens": 5}});
        assert_eq!(
            usage_from_openai(&v),
            Some(Usage {
                input_tokens: 11,
                output_tokens: 5
            })
        );
        let v = serde_json::json!({"usage": {"input_tokens": 3, "output_tokens": 9}});
        assert_eq!(
            usage_from_anthropic(&v),
            Some(Usage {
                input_tokens: 3,
                output_tokens: 9
            })
        );
    }

    #[test]
    fn usage_missing_object_is_none_but_absent_fields_default_to_zero() {
        assert_eq!(usage_from_openai(&serde_json::json!({"id": "x"})), None);
        assert_eq!(
            usage_from_anthropic(&serde_json::json!({"usage": null})),
            None
        );
        assert_eq!(
            usage_from_openai(&serde_json::json!({"usage": {}})),
            Some(Usage::default())
        );
    }

    #[test]
    fn truncate_never_splits_utf8_characters() {
        let s = "İğüşöç";
        // byte 5 lands in the middle of `ş`; back off to the boundary
        assert_eq!(truncate(s, 5), "İğ");
        assert_eq!(truncate(s, 7), "İğü");
        assert_eq!(truncate(s, 1), "");
        assert_eq!(truncate(s, 2), "İ");
        assert_eq!(truncate(s, 100), s);
        assert_eq!(truncate("plain", 3), "pla");
    }

    #[test]
    fn sse_parser_buffers_partial_lines_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(b"event: message_start\ndata: {\"a\"").is_empty());
        let evs = p.feed(b":1}\n\n");
        assert_eq!(
            evs,
            vec![SseEvent {
                event: "message_start".into(),
                data: "{\"a\":1}".into()
            }]
        );
    }

    #[test]
    fn sse_parser_accepts_crlf_and_joins_multi_data_lines() {
        let mut p = SseParser::new();
        let evs = p.feed(b"data: line1\r\ndata: line2\r\n\r\n");
        assert_eq!(
            evs,
            vec![SseEvent {
                event: String::new(),
                data: "line1\nline2".into()
            }]
        );
    }

    #[test]
    fn sse_parser_skips_comments_and_dispatches_only_events_with_data() {
        let mut p = SseParser::new();
        // The first blank line follows an event field with no data: dropped.
        let evs = p.feed(b": ping\nid: 5\revent: ping\n\ndata: [DONE]\n\n");
        assert_eq!(
            evs,
            vec![SseEvent {
                event: String::new(),
                data: "[DONE]".into()
            }]
        );
    }

    #[test]
    fn sse_parser_survives_crlf_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: x\r").is_empty());
        // The dangling \n completes the first event; the rest parses on.
        let evs = p.feed(b"\ndata: y\n\n");
        assert_eq!(
            evs,
            vec![
                SseEvent {
                    event: String::new(),
                    data: "x".into()
                },
                SseEvent {
                    event: String::new(),
                    data: "y".into()
                },
            ]
        );
        // Nothing left pending.
        assert!(p.feed(b"").is_empty());
    }

    #[test]
    fn openai_stream_accumulates_deltas_usage_and_done() {
        let mut acc = OpenAiStreamAcc::default();
        assert_eq!(
            acc.apply(
                r#"{"model":"gpt-5","choices":[{"index":0,"delta":{"role":"assistant","content":"He"}}]}"#
            ),
            "He"
        );
        assert_eq!(
            acc.apply(r#"{"choices":[{"index":0,"delta":{"content":"llo"}}]}"#),
            "llo"
        );
        // Null content and tool-call chunks surface no text delta.
        assert_eq!(
            acc.apply(r#"{"choices":[{"index":0,"delta":{"content":null}}]}"#),
            ""
        );
        assert_eq!(acc.apply("[DONE]"), "");
        assert_eq!(
            acc.apply(r#"{"choices":[{"index":0,"delta":{"content":"!"}}]}"#),
            "!"
        );
        assert_eq!(
            acc.apply(
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#
            ),
            ""
        );
        let done = acc.finish("fallback-model");
        assert_eq!(done.text, "Hello!");
        assert_eq!(
            done.usage,
            Some(Usage {
                input_tokens: 7,
                output_tokens: 3
            })
        );
        assert_eq!(done.model, "gpt-5");
        assert!(done.tool_calls.is_empty());
    }

    #[test]
    fn openai_stream_accumulates_tool_call_fragments_by_index() {
        let mut acc = OpenAiStreamAcc::default();
        acc.apply(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
        );
        acc.apply(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
        );
        acc.apply(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"}}]}}]}"#,
        );
        let done = acc.finish("m");
        assert_eq!(done.tool_calls.len(), 1);
        assert_eq!(done.tool_calls[0].id, "call_1");
        assert_eq!(done.tool_calls[0].name, "read_file");
        assert_eq!(
            done.tool_calls[0].arguments,
            serde_json::json!({"path": "x"})
        );
    }

    #[test]
    fn openai_stream_keeps_fallback_model_and_none_usage() {
        let mut acc = OpenAiStreamAcc::default();
        acc.apply(r#"{"choices":[{"delta":{"content":"hi"}}]}"#);
        // A null usage object (the common per-chunk shape) must not invent one.
        acc.apply(r#"{"choices":[],"usage":null}"#);
        let done = acc.finish("requested-model");
        assert_eq!(done.text, "hi");
        assert_eq!(done.usage, None);
        assert_eq!(done.model, "requested-model");
    }
}
