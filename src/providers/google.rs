use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::providers::{
    backoff_delay, retry_after_delay, should_retry, CatalogModel, Completion, Provider, Usage,
    MAX_ATTEMPTS,
};
use crate::session::{ChatMessage, ToolCall};

const COMPLETION_TIMEOUT: Duration = Duration::from_secs(180);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_ID_PAYLOAD: usize = 64 * 1024;
const MAX_ENCODED_ID: usize = 7 + MAX_ID_PAYLOAD.div_ceil(3) * 4;
const LOCAL_CALL_PREFIX: &str = "google-local:";

pub struct GoogleProvider {
    client: reqwest::Client,
    base: reqwest::Url,
    vertex: Option<Vertex>,
    auth: Auth,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

struct Vertex {
    project: String,
    location: String,
}

enum Auth {
    ApiKey(HeaderValue),
    OAuth,
}

impl GoogleProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        let vertex = match cfg.provider.as_str() {
            "google" => None,
            "google-vertex" => Some(Vertex {
                project: configured_segment(cfg.google_project.as_deref(), "project")?,
                location: configured_segment(cfg.google_location.as_deref(), "location")?,
            }),
            _ => bail!("Google provider requires google or google-vertex configuration"),
        };
        ensure!(
            matches!(cfg.google_auth.as_str(), "api-key" | "oauth"),
            "Google auth must be api-key or oauth"
        );
        if vertex.is_some() {
            ensure!(
                cfg.google_auth == "oauth",
                "Google Vertex requires OAuth authentication"
            );
        }
        // Config resolves environment/profile credentials. Never fall back to process keys here.
        let auth = if vertex.is_some() || cfg.google_auth == "oauth" {
            Auth::OAuth
        } else {
            let key = cfg
                .google_api_key
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!("Google API key is missing from the selected configuration")
                })?;
            let mut header = HeaderValue::from_str(key)
                .map_err(|_| anyhow!("Google API key is not a valid HTTP header"))?;
            header.set_sensitive(true);
            Auth::ApiKey(header)
        };
        ensure!(
            cfg.max_tokens != Some(0),
            "Google max_tokens must be positive"
        );
        if let Some(t) = cfg.temperature {
            ensure!(
                t.is_finite() && (0.0..=2.0).contains(&t),
                "Google temperature must be finite and between 0 and 2"
            );
        }
        let default_base = match &vertex {
            Some(v) if v.location == "global" => "https://aiplatform.googleapis.com".to_string(),
            Some(v) => format!("https://{}-aiplatform.googleapis.com", v.location),
            None => "https://generativelanguage.googleapis.com".to_string(),
        };
        let base = normalize_base(
            cfg.google_base_url.as_deref().unwrap_or(&default_base),
            vertex.is_some(),
        )?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(COMPLETION_TIMEOUT)
            .build()
            .map_err(|_| anyhow!("Could not initialize the Google HTTP client"))?;
        Ok(Self {
            client,
            base,
            vertex,
            auth,
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
        })
    }

    async fn auth_header(&self) -> Result<(&'static str, HeaderValue)> {
        match &self.auth {
            Auth::ApiKey(key) => Ok(("x-goog-api-key", key.clone())),
            Auth::OAuth => {
                let token = crate::google_auth::access_token()
                    .await
                    .map_err(|_| anyhow!("Google OAuth access is unavailable; sign in again"))?;
                ensure!(
                    !token.trim().is_empty(),
                    "Google OAuth returned an empty access token"
                );
                let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| anyhow!("Google OAuth returned an invalid access token"))?;
                header.set_sensitive(true);
                Ok((AUTHORIZATION.as_str(), header))
            }
        }
    }

    fn completion_url(&self, model: &str, streaming: bool) -> Result<reqwest::Url> {
        let id = model_id(model)?;
        let method = if streaming {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("Invalid Google endpoint"))?;
            path.pop_if_empty();
            if let Some(v) = &self.vertex {
                path.extend([
                    "projects",
                    &v.project,
                    "locations",
                    &v.location,
                    "publishers",
                    "google",
                ]);
            }
            path.push("models").push(&format!("{id}:{method}"));
        }
        if streaming {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        Ok(url)
    }

    async fn generate(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: Option<&mut (dyn for<'a> FnMut(&'a str) + Send)>,
    ) -> Result<Completion> {
        let streaming = on_delta.is_some();
        let url = self.completion_url(model, streaming)?;
        let image_model = model_id(model)?.contains("-image");
        let mut body = google_body(
            system,
            messages,
            if image_model { &[] } else { tools },
            self.max_tokens,
            self.temperature,
        )?;
        if image_model {
            body["generationConfig"]["responseModalities"] = json!(["TEXT", "IMAGE"]);
        }
        let (header_name, header) = self.auth_header().await?;
        let resp = send_sanitized(|| {
            self.client
                .post(url.clone())
                .header(header_name, header.clone())
                .header(
                    "Accept",
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .timeout(COMPLETION_TIMEOUT)
                .json(&body)
        })
        .await?;
        check_status(resp.status().as_u16())?;
        if let Some(on_delta) = on_delta {
            let mime = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|s| s.to_str().ok())
                .unwrap_or("");
            ensure!(
                mime.split(';')
                    .next()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("text/event-stream")),
                "Google returned a non-SSE streaming response"
            );
            read_stream(resp, model, on_delta).await
        } else {
            let v = read_json(resp, MAX_RESPONSE_BYTES).await?;
            let mut acc = GoogleAccumulator::default();
            acc.apply(&v, false)?;
            acc.finish(model, false)
        }
    }

    async fn catalog(&self) -> Result<Vec<CatalogModel>> {
        // Vertex publisher listings do not establish project/region model availability.
        // The caller merges explicitly configured Vertex models instead.
        if self.vertex.is_some() {
            return Ok(Vec::new());
        }
        let (header_name, header) = self.auth_header().await?;
        let mut out = Vec::new();
        let mut ids = HashSet::new();
        let mut tokens = HashSet::new();
        let mut next: Option<String> = None;
        for _ in 0..100 {
            let mut url = self.base.clone();
            url.path_segments_mut()
                .map_err(|_| anyhow!("Invalid Google endpoint"))?
                .pop_if_empty()
                .push("models");
            url.query_pairs_mut().append_pair("pageSize", "1000");
            if let Some(token) = &next {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let resp = send_sanitized(|| {
                self.client
                    .get(url.clone())
                    .header(header_name, header.clone())
                    .timeout(CATALOG_TIMEOUT)
            })
            .await?;
            check_status(resp.status().as_u16())?;
            let v = read_json(resp, MAX_CATALOG_BYTES).await?;
            check_api_error(&v)?;
            ensure!(v.is_object(), "Google returned a malformed model catalog");
            let models = v
                .get("models")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("Google returned a malformed model catalog"))?;
            for m in models {
                let methods = m
                    .get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("Google returned malformed model capabilities"))?;
                ensure!(
                    methods.iter().all(Value::is_string),
                    "Google returned malformed model capabilities"
                );
                if !methods
                    .iter()
                    .any(|s| s.as_str() == Some("generateContent"))
                {
                    continue;
                }
                let name = m
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Google returned a model without an ID"))?;
                let id = model_id(name)?.to_string();
                let display_name = optional_string(m, "displayName")?.map(str::to_string);
                if ids.insert(id.clone()) {
                    out.push(CatalogModel {
                        id,
                        display_name,
                        owned_by: Some("google".into()),
                    });
                }
            }
            next = optional_string(&v, "nextPageToken")?
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            match &next {
                None => return Ok(out),
                Some(token) => {
                    ensure!(
                        token.len() <= 4096,
                        "Google model pagination token is too large"
                    );
                    ensure!(
                        tokens.insert(token.clone()),
                        "Google model pagination repeated a page token"
                    );
                }
            }
        }
        bail!("Google model catalog exceeded the pagination limit")
    }
}

#[async_trait::async_trait]
impl Provider for GoogleProvider {
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<Completion> {
        tokio::time::timeout(
            COMPLETION_TIMEOUT,
            self.generate(model, system, messages, tools, None),
        )
        .await
        .map_err(|_| anyhow!("Google completion timed out"))?
    }

    async fn complete_streaming(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        tokio::time::timeout(
            COMPLETION_TIMEOUT,
            self.generate(model, system, messages, tools, Some(on_delta)),
        )
        .await
        .map_err(|_| anyhow!("Google completion timed out"))?
    }

    async fn list_models(&self) -> Result<Vec<CatalogModel>> {
        tokio::time::timeout(CATALOG_TIMEOUT, self.catalog())
            .await
            .map_err(|_| anyhow!("Google model catalog timed out"))?
    }
}

fn configured_segment(value: Option<&str>, field: &str) -> Result<String> {
    let value = value.filter(|s| valid_segment(s)).ok_or_else(|| {
        anyhow!("Google Vertex {field} is required and must be a valid path segment")
    })?;
    Ok(value.to_string())
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn model_id(model: &str) -> Result<&str> {
    let model = model
        .strip_prefix("publishers/google/models/")
        .or_else(|| model.strip_prefix("models/"))
        .unwrap_or(model);
    ensure!(
        valid_segment(model),
        "Google model must be a valid model ID, not a URL or arbitrary resource path"
    );
    Ok(model)
}

fn normalize_base(raw: &str, vertex: bool) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(raw).map_err(|_| anyhow!("Invalid Google base URL"))?;
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let loopback = host
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|_| host.eq_ignore_ascii_case("localhost"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Google base URL must use HTTPS (HTTP is allowed only for loopback tests)"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Google base URL cannot contain user information, query parameters, or a fragment"
    );
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let suffix = segments.last().copied().unwrap_or("");
    let has_version = if vertex {
        matches!(suffix, "v1" | "v1beta1")
    } else {
        matches!(suffix, "v1" | "v1beta")
    };
    ensure!(
        !segments[..segments.len().saturating_sub(1)]
            .iter()
            .any(|segment| matches!(*segment, "v1" | "v1beta" | "v1beta1")),
        "Google endpoint has an API version in an invalid path position"
    );
    if !has_version {
        ensure!(
            !matches!(suffix, "v1" | "v1beta" | "v1beta1"),
            "Google endpoint has an incompatible API version"
        );
        url.path_segments_mut()
            .map_err(|_| anyhow!("Invalid Google endpoint"))?
            .pop_if_empty()
            .push(if vertex { "v1" } else { "v1beta" });
    }
    Ok(url)
}

// The opaque ID survives session persistence and tool-result matching without changing tool args.
// Locally synthesized IDs are not sent as provider-assigned functionCall/functionResponse IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoogleCallId {
    id: String,
    signature: Option<String>,
}

fn encode_call_id(metadata: &GoogleCallId) -> Result<String> {
    validate_call_id(metadata)?;
    let bytes = serde_json::to_vec(metadata)
        .map_err(|_| anyhow!("Could not encode Google tool-call metadata"))?;
    ensure!(
        bytes.len() <= MAX_ID_PAYLOAD,
        "Google tool-call metadata is too large"
    );
    Ok(format!("google:{}", STANDARD.encode(bytes)))
}

fn decode_call_id(id: &str) -> Result<GoogleCallId> {
    let metadata = match id.strip_prefix("google:") {
        Some(encoded) => {
            ensure!(
                id.len() <= MAX_ENCODED_ID,
                "Google tool-call ID is too large"
            );
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| anyhow!("Invalid Google tool-call metadata encoding"))?;
            ensure!(
                bytes.len() <= MAX_ID_PAYLOAD,
                "Google tool-call metadata is too large"
            );
            serde_json::from_slice(&bytes)
                .map_err(|_| anyhow!("Invalid Google tool-call metadata"))?
        }
        None => GoogleCallId {
            id: id.into(),
            signature: None,
        },
    };
    validate_call_id(&metadata)?;
    Ok(metadata)
}

fn validate_call_id(metadata: &GoogleCallId) -> Result<()> {
    ensure!(
        !metadata.id.is_empty()
            && metadata.id.len() <= 1024
            && !metadata.id.chars().any(char::is_control),
        "Invalid Google function-call ID"
    );
    if let Some(signature) = &metadata.signature {
        ensure!(
            !signature.is_empty() && signature.len() <= MAX_ID_PAYLOAD,
            "Invalid or oversized Google thought signature"
        );
    }
    Ok(())
}

fn wire_call_id(metadata: &GoogleCallId) -> Option<&str> {
    (!metadata.id.starts_with(LOCAL_CALL_PREFIX)).then_some(metadata.id.as_str())
}

fn valid_function_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn google_body(
    system: &str,
    messages: &[ChatMessage],
    tools: &[Value],
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) -> Result<Value> {
    let mut system_parts = Vec::new();
    if !system.is_empty() {
        system_parts.push(json!({"text": system}));
    }
    let mut contents: Vec<Value> = Vec::new();
    let mut pending: HashMap<&str, (&str, GoogleCallId)> = HashMap::new();
    for message in messages {
        let mut parts = Vec::new();
        let role = match message.role.as_str() {
            "system" => {
                ensure!(
                    message.images.is_empty()
                        && message.tool_calls.as_ref().is_none_or(Vec::is_empty)
                        && message.tool_call_id.is_none(),
                    "Google system messages cannot contain images or tool calls"
                );
                if !message.content.is_empty() {
                    system_parts.push(json!({"text": message.content}));
                }
                continue;
            }
            "tool" => {
                ensure!(
                    message.images.is_empty()
                        && message.tool_calls.as_ref().is_none_or(Vec::is_empty),
                    "Google tool results cannot contain images or tool calls"
                );
                let id = message
                    .tool_call_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("Google tool result is missing its call ID"))?;
                let (name, metadata) = pending
                    .remove(id)
                    .ok_or_else(|| anyhow!("Google tool result has no matching function call"))?;
                let result = serde_json::from_str::<Value>(&message.content)
                    .unwrap_or_else(|_| Value::String(message.content.clone()));
                let response = if result.is_object() {
                    result
                } else {
                    json!({"result": result})
                };
                let mut function_response = json!({"name": name, "response": response});
                if let Some(id) = wire_call_id(&metadata) {
                    function_response["id"] = json!(id);
                }
                parts.push(json!({"functionResponse": function_response}));
                "user"
            }
            "user" | "assistant" => {
                ensure!(
                    message.tool_call_id.is_none(),
                    "Google non-tool message has an unexpected tool-result ID"
                );
                if !message.content.is_empty() {
                    parts.push(json!({"text": message.content}));
                }
                for image in &message.images {
                    ensure!(
                        image.media_type.starts_with("image/") && !image.data.is_empty(),
                        "Google image attachment is missing its image MIME type or data"
                    );
                    parts.push(
                        json!({"inlineData": {"mimeType": image.media_type, "data": image.data}}),
                    );
                }
                if let Some(calls) = &message.tool_calls {
                    ensure!(
                        message.role == "assistant" || calls.is_empty(),
                        "Google function calls must belong to an assistant message"
                    );
                    let mut seen = HashSet::new();
                    for call in calls {
                        ensure!(
                            valid_function_name(&call.name) && call.arguments.is_object(),
                            "Invalid Google function call name or arguments"
                        );
                        ensure!(
                            seen.insert(&call.id),
                            "Duplicate Google function-call ID in one turn"
                        );
                        let metadata = decode_call_id(&call.id)?;
                        let mut function_call = json!({"name": call.name, "args": call.arguments});
                        if let Some(id) = wire_call_id(&metadata) {
                            function_call["id"] = json!(id);
                        }
                        let mut part = json!({"functionCall": function_call});
                        if let Some(signature) = &metadata.signature {
                            part["thoughtSignature"] = json!(signature);
                        }
                        parts.push(part);
                        pending.insert(&call.id, (&call.name, metadata));
                    }
                }
                if message.role == "assistant" {
                    "model"
                } else {
                    "user"
                }
            }
            _ => bail!("Unsupported Google chat message role"),
        };
        ensure!(
            !parts.is_empty(),
            "Google conversation contains an empty message"
        );
        if let Some(last) = contents
            .last_mut()
            .filter(|v| v["role"].as_str() == Some(role))
        {
            last["parts"]
                .as_array_mut()
                .expect("contents are built with parts arrays")
                .extend(parts);
        } else {
            contents.push(json!({"role": role, "parts": parts}));
        }
    }
    ensure!(
        !contents.is_empty(),
        "Google completion needs conversation content"
    );
    let mut declarations = Vec::new();
    let mut names = HashSet::new();
    for tool in tools {
        ensure!(
            tool.get("type").and_then(Value::as_str) == Some("function"),
            "Google supports function tool declarations only"
        );
        let function = tool
            .get("function")
            .filter(|f| f.is_object())
            .ok_or_else(|| anyhow!("Malformed Google function declaration"))?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| valid_function_name(s))
            .ok_or_else(|| anyhow!("Invalid Google function declaration name"))?;
        ensure!(
            names.insert(name),
            "Duplicate Google function declaration name"
        );
        let schema = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object", "properties":{}}));
        ensure!(
            schema.is_object(),
            "Google function parameters must be a JSON schema object"
        );
        // The wire field name is `parameters`; Google's v1beta schema subset
        // keeps the declaration shape compatible with OpenAI-style tools.
        let mut declaration = json!({"name": name, "parameters": schema});
        if let Some(description) = optional_string(function, "description")? {
            declaration["description"] = json!(description);
        }
        declarations.push(declaration);
    }
    let mut body = json!({"contents": contents, "generationConfig": {"candidateCount": 1}});
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({"parts": system_parts});
    }
    if !declarations.is_empty() {
        body["tools"] = json!([{"functionDeclarations": declarations}]);
    }
    if let Some(max_tokens) = max_tokens {
        body["generationConfig"]["maxOutputTokens"] = json!(max_tokens);
    }
    if let Some(temperature) = temperature {
        body["generationConfig"]["temperature"] = json!(temperature);
    }
    Ok(body)
}

// The shared send helpers include URLs/bodies in errors and logs. Reuse their retry policy,
// but never retain a raw reqwest error or read an HTTP error body in this adapter.
async fn send_sanitized(build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
    for attempt in 1..=MAX_ATTEMPTS {
        match build().send().await {
            Ok(resp) => {
                if !should_retry(resp.status().as_u16(), attempt) {
                    return Ok(resp);
                }
                let delay = resp
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|s| s.to_str().ok())
                    .and_then(retry_after_delay)
                    .unwrap_or_else(|| backoff_delay(attempt));
                drop(resp);
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                if attempt == MAX_ATTEMPTS || error.is_builder() {
                    return Err(transport_error(&error));
                }
                tokio::time::sleep(backoff_delay(attempt)).await;
            }
        }
    }
    unreachable!("retry loop always returns on the final attempt")
}

fn transport_error(error: &reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("Google request timed out")
    } else if error.is_connect() {
        anyhow!("Google connection failed")
    } else {
        anyhow!("Google request transport failed")
    }
}

fn check_status(status: u16) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
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
    bail!("Google HTTP {status}: {reason}")
}

async fn read_json(resp: reqwest::Response, limit: usize) -> Result<Value> {
    ensure!(
        resp.content_length().is_none_or(|n| n <= limit as u64),
        "Google response is too large"
    );
    let mut bytes = Vec::new();
    let mut stream = std::pin::pin!(resp.bytes_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| transport_error(&e))?;
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= limit,
            "Google response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("Google returned invalid JSON"))
}

fn optional_string<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        _ => bail!("Google returned a malformed string field"),
    }
}

fn check_api_error(v: &Value) -> Result<()> {
    if let Some(error) = v.get("error").filter(|s| !s.is_null()) {
        if let Some(code) = error
            .get("code")
            .and_then(Value::as_u64)
            .filter(|n| (400..600).contains(n))
        {
            return check_status(code as u16);
        }
        bail!("Google returned an API error");
    }
    Ok(())
}

#[derive(Default)]
struct GoogleAccumulator {
    text: String,
    images: Vec<crate::session::ImageAttachment>,
    calls: Vec<ToolCall>,
    usage: Option<Usage>,
    model: Option<String>,
    saw_candidate: bool,
    finished: bool,
}

impl GoogleAccumulator {
    fn apply(&mut self, v: &Value, streaming: bool) -> Result<String> {
        ensure!(v.is_object(), "Google returned a malformed response");
        check_api_error(v)?;
        if let Some(feedback) = v.get("promptFeedback").filter(|f| !f.is_null()) {
            ensure!(
                feedback.is_object(),
                "Google returned malformed prompt feedback"
            );
            if optional_string(feedback, "blockReason")?
                .is_some_and(|s| !s.is_empty() && s != "BLOCK_REASON_UNSPECIFIED")
            {
                bail!("Google blocked the prompt for safety or policy reasons");
            }
            check_safety(feedback)?;
        }
        let usage = parse_usage(v, self.usage)?;
        if let Some(model) = optional_string(v, "modelVersion")?.filter(|s| !s.is_empty()) {
            ensure!(
                model.len() <= 512 && !model.chars().any(char::is_control),
                "Google returned a malformed model version"
            );
            self.model = Some(model.into());
        }
        let candidates = v.get("candidates").and_then(Value::as_array);
        let candidate = candidates.and_then(|c| c.first());
        let Some(candidate) = candidate else {
            ensure!(
                streaming && self.saw_candidate && self.finished && usage.is_some(),
                "Google response has missing or empty candidates"
            );
            self.usage = usage;
            return Ok(String::new());
        };
        ensure!(
            candidate.is_object(),
            "Google returned a malformed candidate"
        );
        if let Some(index) = candidate.get("index") {
            ensure!(
                index.as_u64() == Some(0),
                "Google returned an unexpected candidate index"
            );
        }
        check_safety(candidate)?;
        let reason = optional_string(candidate, "finishReason")?;
        match reason {
            None | Some("") | Some("FINISH_REASON_UNSPECIFIED") => {}
            Some("STOP" | "MAX_TOKENS") => self.finished = true,
            Some("MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL") => {
                bail!("Google returned an invalid function call")
            }
            Some(_) => bail!("Google blocked or could not complete the candidate"),
        }
        let mut delta = String::new();
        if let Some(content) = candidate.get("content").filter(|s| !s.is_null()) {
            ensure!(
                content.is_object(),
                "Google returned malformed candidate content"
            );
            if let Some(role) = optional_string(content, "role")? {
                ensure!(role == "model", "Google returned an invalid candidate role");
            }
            let parts = content
                .get("parts")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("Google returned malformed candidate parts"))?;
            ensure!(
                !parts.is_empty(),
                "Google returned a candidate with no content parts"
            );
            for part in parts {
                ensure!(part.is_object(), "Google returned a malformed content part");
                let thought = match part.get("thought") {
                    None => false,
                    Some(Value::Bool(b)) => *b,
                    _ => bail!("Google returned malformed thought metadata"),
                };
                let signature = optional_string(part, "thoughtSignature")?;
                if let Some(data) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                    ensure!(
                        part.get("text").is_none() && part.get("functionCall").is_none(),
                        "Google returned ambiguous image content"
                    );
                    let media_type = data
                        .get("mimeType")
                        .or_else(|| data.get("mime_type"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Google image is missing its MIME type"))?;
                    let encoded = data
                        .get("data")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Google image is missing its data"))?;
                    let image = crate::session::ImageAttachment {
                        media_type: media_type.into(),
                        data: encoded.into(),
                    };
                    crate::media::validate_attachment(&image)
                        .map_err(|_| anyhow!("Google returned an invalid or oversized image"))?;
                    if !thought && !self.images.contains(&image) {
                        self.images.push(image);
                        super::validate_output_images(&self.images)
                            .map_err(|_| anyhow!("Google images exceeded the response limits"))?;
                    }
                    continue;
                }
                match (part.get("text"), part.get("functionCall")) {
                    (Some(text), None) => {
                        let text = text
                            .as_str()
                            .ok_or_else(|| anyhow!("Google returned malformed text content"))?;
                        if !thought {
                            delta.push_str(text);
                        }
                    }
                    (None, Some(call)) if !thought => {
                        ensure!(
                            call.is_object(),
                            "Google returned a malformed function call"
                        );
                        let name = call
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|s| valid_function_name(s))
                            .ok_or_else(|| anyhow!("Google returned an invalid function name"))?;
                        let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                        ensure!(
                            arguments.is_object(),
                            "Google returned invalid JSON function arguments"
                        );
                        let id = match optional_string(call, "id")? {
                            Some(id) => id.to_string(),
                            None => {
                                let material =
                                    serde_json::to_vec(&(self.calls.len(), name, &arguments))
                                        .map_err(|_| {
                                            anyhow!("Could not identify the Google function call")
                                        })?;
                                format!("{LOCAL_CALL_PREFIX}{:x}", Sha256::digest(material))
                            }
                        };
                        let id = encode_call_id(&GoogleCallId {
                            id,
                            signature: signature.map(str::to_string),
                        })?;
                        ensure!(
                            !self.calls.iter().any(|c| c.id == id),
                            "Google returned a duplicate function-call ID"
                        );
                        self.calls.push(ToolCall {
                            id,
                            name: name.into(),
                            arguments,
                        });
                    }
                    _ => bail!("Google returned an unsupported or malformed content part"),
                }
            }
        } else {
            ensure!(
                streaming && self.saw_candidate && self.finished,
                "Google candidate is missing content"
            );
        }
        self.saw_candidate = true;
        self.usage = usage;
        self.text.push_str(&delta);
        Ok(delta)
    }

    fn finish(self, fallback_model: &str, streaming: bool) -> Result<Completion> {
        ensure!(
            self.saw_candidate,
            "Google response has missing or empty candidates"
        );
        ensure!(
            !streaming || self.finished,
            "Google stream ended before a finish reason"
        );
        ensure!(
            !self.text.trim().is_empty() || !self.calls.is_empty() || !self.images.is_empty(),
            "Google returned an empty completion"
        );
        Ok(Completion {
            text: self.text,
            tool_calls: self.calls,
            images: self.images,
            model: self.model.unwrap_or_else(|| fallback_model.into()),
            usage: self.usage,
        })
    }
}

fn check_safety(v: &Value) -> Result<()> {
    if let Some(ratings) = v.get("safetyRatings").filter(|r| !r.is_null()) {
        let ratings = ratings
            .as_array()
            .ok_or_else(|| anyhow!("Google returned malformed safety ratings"))?;
        for rating in ratings {
            ensure!(
                rating.is_object(),
                "Google returned malformed safety ratings"
            );
            match rating.get("blocked") {
                Some(Value::Bool(true)) => {
                    bail!("Google blocked the candidate for safety or policy reasons")
                }
                None | Some(Value::Bool(false)) => {}
                _ => bail!("Google returned malformed safety ratings"),
            }
        }
    }
    Ok(())
}

fn parse_usage(v: &Value, previous: Option<Usage>) -> Result<Option<Usage>> {
    let Some(usage) = v.get("usageMetadata").filter(|u| !u.is_null()) else {
        return Ok(previous);
    };
    ensure!(usage.is_object(), "Google returned malformed token usage");
    let count = |field: &str| -> Result<Option<u64>> {
        match usage.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| anyhow!("Google returned malformed token usage")),
        }
    };
    let previous = previous.unwrap_or_default();
    let input = count("promptTokenCount")?.unwrap_or(previous.input_tokens);
    let output = match (count("candidatesTokenCount")?, count("thoughtsTokenCount")?) {
        (None, None) => previous.output_tokens,
        (candidate, thought) => candidate
            .unwrap_or(0)
            .checked_add(thought.unwrap_or(0))
            .ok_or_else(|| anyhow!("Google returned overflowing token usage"))?,
    };
    Ok(Some(Usage {
        input_tokens: input,
        output_tokens: output,
    }))
}

// Keep frame boundaries strict, including CRLF split across network chunks and incomplete EOF.
#[derive(Default)]
struct GoogleSseParser {
    buf: Vec<u8>,
    data: Vec<u8>,
    error_event: bool,
}

impl GoogleSseParser {
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>> {
        ensure!(
            self.buf.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Google SSE frame is too large"
        );
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n' || *b == b'\r') {
            if self.buf[pos] == b'\r' && pos + 1 == self.buf.len() {
                break;
            }
            let consumed = pos
                + 1
                + usize::from(self.buf[pos] == b'\r' && self.buf.get(pos + 1) == Some(&b'\n'));
            let line: Vec<u8> = self.buf.drain(..consumed).take(pos).collect();
            if line.is_empty() {
                ensure!(!self.error_event, "Google returned a streaming API error");
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data));
                }
                self.error_event = false;
                continue;
            }
            if line.starts_with(b":") {
                continue;
            }
            let (field, value) = match line.iter().position(|b| *b == b':') {
                Some(n) => (
                    &line[..n],
                    line[n + 1..].strip_prefix(b" ").unwrap_or(&line[n + 1..]),
                ),
                None => (line.as_slice(), b"".as_slice()),
            };
            if field == b"data" {
                ensure!(
                    self.data.len().saturating_add(value.len()) < MAX_RESPONSE_BYTES,
                    "Google SSE frame is too large"
                );
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
            } else if field == b"event" {
                self.error_event = value == b"error";
            }
        }
        Ok(events)
    }

    fn finish(&self) -> Result<()> {
        ensure!(
            self.buf.is_empty() && self.data.is_empty() && !self.error_event,
            "Google stream ended with an incomplete SSE frame"
        );
        Ok(())
    }
}

async fn read_stream(
    resp: reqwest::Response,
    model: &str,
    on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<Completion> {
    let mut parser = GoogleSseParser::default();
    let mut acc = GoogleAccumulator::default();
    let mut total: usize = 0;
    let mut stream = std::pin::pin!(resp.bytes_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| transport_error(&e))?;
        total = total.saturating_add(chunk.len());
        ensure!(total <= MAX_RESPONSE_BYTES, "Google stream is too large");
        for data in parser.feed(&chunk)? {
            let v: Value = serde_json::from_slice(&data)
                .map_err(|_| anyhow!("Google stream returned invalid JSON"))?;
            let delta = acc.apply(&v, true)?;
            if !delta.is_empty() {
                on_delta(&delta);
            }
        }
    }
    parser.finish()?;
    acc.finish(model, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ChatMessage, ImageAttachment, ToolCall};
    use serde_json::json;

    #[test]
    fn call_id_envelope_round_trips_arguments_outside_metadata() {
        let metadata = GoogleCallId {
            id: "server-call".into(),
            signature: Some("opaque-signature".into()),
        };
        let encoded = encode_call_id(&metadata).unwrap();
        assert!(encoded.starts_with("google:"));
        assert_eq!(decode_call_id(&encoded).unwrap(), metadata);
        let arguments = json!({"path":"src/lib.rs", "nested":{"keep":true}});
        let call = ToolCall {
            id: encoded,
            name: "read_file".into(),
            arguments: arguments.clone(),
        };
        assert_eq!(call.arguments, arguments);
    }

    #[test]
    fn call_id_bounds_reject_oversized_envelope_but_allow_bounded_legacy_id() {
        let legacy = "x".repeat(1024);
        assert!(decode_call_id(&legacy).is_ok());
        assert!(decode_call_id(&"x".repeat(1025)).is_err());
        let signature = "s".repeat(MAX_ID_PAYLOAD - 64);
        let encoded = encode_call_id(&GoogleCallId {
            id: "id".into(),
            signature: Some(signature),
        })
        .unwrap();
        assert!(encoded.len() <= MAX_ENCODED_ID);
        assert!(decode_call_id(&encoded).is_ok());
        assert!(decode_call_id(&(encoded + "x")).is_err());
    }

    #[test]
    fn gemini_translation_groups_tool_results_and_preserves_images() {
        let id = encode_call_id(&GoogleCallId {
            id: "call-1".into(),
            signature: Some("sig".into()),
        })
        .unwrap();
        let call = ToolCall {
            id: id.clone(),
            name: "read_file".into(),
            arguments: json!({"path":"a.txt"}),
        };
        let body = google_body(
            "system",
            &[
                ChatMessage {
                    role: "user".into(),
                    content: "inspect".into(),
                    images: vec![ImageAttachment {
                        media_type: "image/png".into(),
                        data: "QUJD".into(),
                    }],
                    ..Default::default()
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: Some(vec![call]),
                    ..Default::default()
                },
                ChatMessage {
                    role: "tool".into(),
                    content: "{\"ok\":true}".into(),
                    tool_call_id: Some(id),
                    ..Default::default()
                },
            ],
            &[json!({"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}})],
            Some(100),
            Some(0.2),
        )
        .unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "system");
        assert_eq!(
            body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionCall"]["id"],
            "call-1"
        );
        assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], "sig");
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["id"],
            "call-1"
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(body["generationConfig"]["temperature"], json!(0.2_f32));
    }

    #[test]
    fn malformed_and_safety_responses_do_not_become_empty_successes() {
        let mut acc = GoogleAccumulator::default();
        assert!(acc.apply(&json!({"candidates": []}), false).is_err());
        assert!(acc
            .apply(&json!({"promptFeedback":{"blockReason":"SAFETY"}}), false)
            .is_err());
        assert!(acc.apply(&json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}]}), false).is_err());
    }
}
