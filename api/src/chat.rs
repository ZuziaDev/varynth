use serde_json::{json, Value};

use crate::config::Config;
use crate::crypto::{public_json_is_clean, scrub_upstream_error};
use crate::error::ApiError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ModelRow {
    pub id: String,
    pub display_name: String,
    pub tier: String,
    pub upstream_kind: String,
    pub upstream_model: String,
    pub upstream_base_url: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CatalogModel {
    pub provider_id: String,
    pub kind: String,
    pub model_id: String,
}

#[derive(Debug)]
pub struct Dispatch {
    pub public_model: String,
    pub upstream_kind: String,
    pub upstream_model: String,
    pub upstream_base: String,
    pub stub: bool,
}

pub fn resolve_dispatch(
    model: Option<&ModelRow>,
    catalog: Option<&CatalogModel>,
    cfg: &Config,
) -> Result<Dispatch, ApiError> {
    if let Some(row) = model {
        if !row.enabled {
            return Err(ApiError::NotFound);
        }
        let base = workers_base(row, cfg);
        let upstream_model = if row.upstream_model.is_empty() {
            row.id.clone()
        } else {
            row.upstream_model.clone()
        };
        let stub = should_stub(cfg, &row.upstream_kind, &base);
        return Ok(Dispatch {
            public_model: row.id.clone(),
            upstream_kind: row.upstream_kind.clone(),
            upstream_model,
            upstream_base: base,
            stub,
        });
    }
    let Some(row) = catalog else {
        return Err(ApiError::NotFound);
    };
    let (mut base, mut kind) = catalog_upstream(&row.provider_id, &row.kind);
    if !cfg.stub_upstream
        && cfg.upstream_key(kind).is_none()
        && cfg.worker_gateway_key.is_some()
        && !cfg.worker_gateways.is_empty()
    {
        base = "";
        kind = "worker_gateway";
    }
    let stub = should_stub(cfg, kind, base);
    Ok(Dispatch {
        public_model: format!("{}/{}", row.provider_id, row.model_id),
        upstream_kind: kind.to_string(),
        upstream_model: row.model_id.clone(),
        upstream_base: base.to_string(),
        stub,
    })
}

fn workers_base(row: &ModelRow, cfg: &Config) -> String {
    if !row.upstream_base_url.is_empty() {
        return row.upstream_base_url.clone();
    }
    if row.upstream_kind == "workers_ai" {
        if let Some(account) = &cfg.workers_account_id {
            return format!(
                "https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1"
            );
        }
    }
    String::new()
}

fn catalog_upstream(provider: &str, kind: &str) -> (&'static str, &'static str) {
    match provider {
        "openrouter" => ("https://openrouter.ai/api/v1", "openrouter"),
        _ if kind == "anthropic" => ("https://api.anthropic.com", "anthropic"),
        _ => ("https://api.openai.com/v1", "openai"),
    }
}

fn should_stub(cfg: &Config, kind: &str, base: &str) -> bool {
    if cfg.stub_upstream {
        return true;
    }
    if kind == "worker_gateway" {
        return cfg.worker_gateways.is_empty() || cfg.worker_gateway_key.is_none();
    }
    cfg.upstream_key(kind).is_none() || base.is_empty()
}

pub fn rewrite_model(mut body: Value, public_model: &str) -> Value {
    if body.get("model").is_some() {
        body["model"] = json!(public_model);
    }
    if let Some(choices) = body.get_mut("choices").and_then(|v| v.as_array_mut()) {
        for choice in choices {
            if choice.get("model").is_some() {
                choice["model"] = json!(public_model);
            }
            if let Some(msg) = choice.get_mut("message").and_then(|m| m.as_object_mut()) {
                let content_empty = match msg.get("content") {
                    None | Some(Value::Null) => true,
                    Some(Value::String(s)) => s.trim().is_empty(),
                    _ => false,
                };
                if content_empty {
                    let reasoning = msg
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .or_else(|| msg.get("reasoning").and_then(|v| v.as_str()))
                        .map(str::to_string);
                    if let Some(r) = reasoning {
                        if !r.trim().is_empty() {
                            msg.insert("content".to_string(), Value::String(r));
                        }
                    }
                }
            }
        }
    }
    body
}

pub fn stub_completion(public_model: &str) -> Value {
    json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion",
        "model": public_model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": format!("stub: {public_model}")},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    })
}

pub fn completion_to_sse(body: &Value) -> String {
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let text = body
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let chunk = json!({
        "id": body.get("id").cloned().unwrap_or(json!("chatcmpl")),
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
    });
    let done = json!({
        "id": body.get("id").cloned().unwrap_or(json!("chatcmpl")),
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n")
}

pub fn rewrite_sse(raw: &str, public_model: &str) -> String {
    let mut out = String::new();
    for line in raw.split('\n') {
        if let Some(data) = line.trim_start().strip_prefix("data:") {
            let data = data.trim();
            if data == "[DONE]" {
                out.push_str("data: [DONE]\n");
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                let mut value = rewrite_model(value, public_model);
                if let Some(error) = value.get("error") {
                    if !public_json_is_clean(&error.to_string()) {
                        value["error"] = json!({"message": "upstream request failed"});
                    }
                }
                out.push_str("data: ");
                out.push_str(&value.to_string());
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub fn prepare_upstream_body(mut body: Value, upstream_model: &str, stream: bool) -> Value {
    body["model"] = json!(upstream_model);
    if stream {
        body["stream"] = json!(true);
    } else {
        body.as_object_mut().map(|map| map.remove("stream"));
    }
    body
}

pub fn anthropic_body(body: &Value, upstream_model: &str) -> Result<Value, ApiError> {
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ApiError::bad("messages is required"))?;
    let mut system = String::new();
    let mut out = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let text = content_text(message.get("content"));
        match role {
            "system" => {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&text);
            }
            "user" | "assistant" => out.push(json!({"role": role, "content": text})),
            "tool" => out.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "content": text
                }]
            })),
            _ => {}
        }
    }
    if out.is_empty() {
        return Err(ApiError::bad("messages is required"));
    }
    let mut payload = json!({
        "model": upstream_model,
        "max_tokens": body.get("max_tokens").cloned().unwrap_or(json!(4096)),
        "messages": out
    });
    if !system.is_empty() {
        payload["system"] = json!(system);
    }
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let function = tool.get("function")?;
                Some(json!({
                    "name": function.get("name").cloned().unwrap_or(json!("")),
                    "description": function.get("description").cloned().unwrap_or(json!("")),
                    "input_schema": function.get("parameters").cloned().unwrap_or(json!({"type":"object"}))
                }))
            })
            .collect();
        if !mapped.is_empty() {
            payload["tools"] = json!(mapped);
        }
    }
    Ok(payload)
}

pub fn anthropic_to_openai(body: Value, public_model: &str) -> Value {
    let text = body
        .get("content")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    if part.get("type").and_then(|kind| kind.as_str()) == Some("text") {
                        part.get("text").and_then(|value| value.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let tool_calls: Vec<Value> = body
        .get("content")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(|kind| kind.as_str()) == Some("tool_use"))
                .map(|part| {
                    json!({
                        "id": part.get("id").cloned().unwrap_or(json!("tool")),
                        "type": "function",
                        "function": {
                            "name": part.get("name").cloned().unwrap_or(json!("")),
                            "arguments": part.get("input").map(|v| v.to_string()).unwrap_or_else(|| "{}".into())
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut message = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    json!({
        "id": body.get("id").cloned().unwrap_or(json!("msg")),
        "object": "chat.completion",
        "model": public_model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}]
    })
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub fn scrubbed_error(status: reqwest::StatusCode, body: &str) -> ApiError {
    tracing::warn!(status = %status, "upstream chat failed");
    ApiError::bad(scrub_upstream_error(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(stub: bool) -> Config {
        Config {
            database_url: String::new(),
            jwt_secret: "x".repeat(32),
            web_origin: "http://127.0.0.1:3000".into(),
            public_url: "http://127.0.0.1:8080".into(),
            bind: "127.0.0.1:8080".into(),
            workers_account_id: None,
            workers_token: None,
            openai_key: None,
            anthropic_key: None,
            openrouter_key: None,
            worker_gateways: Vec::new(),
            worker_gateway_key: None,
            parallel_keys: Vec::new(),
            exa_keys: Vec::new(),
            firecrawl_keys: Vec::new(),
            pro_emails: Vec::new(),
            trusted_proxies: Vec::new(),
            stub_upstream: stub,
            enable_public_placeholders: true,
            billing_stub: false,
        }
    }

    #[test]
    fn public_dispatch_stubs_without_leaking_upstream() {
        let row = ModelRow {
            id: "varynth/public-1".into(),
            display_name: "Varynth Public 1".into(),
            tier: "public".into(),
            upstream_kind: "workers_ai".into(),
            upstream_model: "hidden".into(),
            upstream_base_url: String::new(),
            enabled: true,
        };
        let dispatch = resolve_dispatch(Some(&row), None, &cfg(false)).unwrap();
        assert!(dispatch.stub);
        assert_eq!(dispatch.public_model, "varynth/public-1");
        let body = stub_completion(&dispatch.public_model);
        assert!(public_json_is_clean(&body.to_string()));
    }

    #[test]
    fn sse_rewrite_replaces_model() {
        let raw = "data: {\"model\":\"hidden\",\"choices\":[]}\n\ndata: [DONE]\n";
        let rewritten = rewrite_sse(raw, "varynth/public-1");
        assert!(rewritten.contains("varynth/public-1"));
        assert!(!rewritten.contains("hidden"));
    }
}
