use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::providers::{
    ensure_sse_response, needs_param_degrade, send_streaming_with_retry, send_with_retry, truncate,
    usage_from_anthropic, CatalogModel, Completion, Provider, SseParser, Usage,
};
use crate::session::{ChatMessage, ToolCall};

pub struct AnthropicProvider {
    client: reqwest::Client,
    base: String,
    key: String,
    effort: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl AnthropicProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        let key = cfg
            .anthropic_api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .context("ANTHROPIC_API_KEY missing")?;
        let base = cfg
            .anthropic_base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".into())
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()?,
            base,
            key,
            effort: cfg.effort.trim().to_lowercase(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
        })
    }

    fn messages_request(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .json(body)
    }
}

/// Extended-thinking budget for the configured effort level; `None` keeps the
/// legacy no-thinking behavior.
fn thinking_budget(effort: &str) -> Option<u32> {
    match effort {
        "high" => Some(4096),
        "xhigh" => Some(8192),
        "max" => Some(16384),
        "ultra" => Some(24576),
        _ => None,
    }
}

/// Headroom added on top of the thinking budget for `max_tokens`: the ultra
/// tier multi-passes and self-verifies, so it gets double the normal window.
fn budget_headroom(budget: u32) -> u32 {
    if budget >= 24576 {
        16384
    } else {
        8192
    }
}

/// Builds the `/v1/messages` request body. When thinking is enabled,
/// `max_tokens` becomes `budget + headroom` (unless `cfg_max_tokens` is
/// larger) and `temperature` is omitted — Anthropic rejects temperature
/// together with thinking.
fn anthropic_body(
    model: &str,
    system: &str,
    messages: &[Value],
    tools: &[Value],
    effort: &str,
    cfg_max_tokens: Option<u32>,
    cfg_temperature: Option<f32>,
) -> Value {
    let budget = thinking_budget(effort);
    let max_tokens = match budget {
        Some(b) => {
            let head = budget_headroom(b);
            cfg_max_tokens.map_or(b + head, |c| c.max(b + head))
        }
        None => cfg_max_tokens.unwrap_or(8192),
    };
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": messages,
    });
    if let Some(b) = budget {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": b});
    } else if let Some(t) = cfg_temperature {
        body["temperature"] = json!(t);
    }
    let a_tools = tools_to_anthropic(tools);
    if !a_tools.is_empty() {
        body["tools"] = Value::Array(a_tools);
    }
    body
}

fn to_anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "tool" => {
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                        "content": m.content,
                    }]
                }));
            }
            "assistant" if m.tool_calls.is_some() => {
                let mut content = Vec::new();
                if !m.content.is_empty() {
                    content.push(json!({"type":"text","text": m.content}));
                }
                if let Some(calls) = &m.tool_calls {
                    for c in calls {
                        content.push(json!({
                            "type": "tool_use",
                            "id": c.id,
                            "name": c.name,
                            "input": c.arguments,
                        }));
                    }
                }
                out.push(json!({"role":"assistant","content": content}));
            }
            role => {
                let role = if role == "system" { "user" } else { role };
                if m.images.is_empty() {
                    out.push(json!({"role": role, "content": m.content}));
                } else {
                    let mut content: Vec<Value> = m
                        .images
                        .iter()
                        .map(|img| {
                            json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": img.media_type,
                                    "data": img.data,
                                }
                            })
                        })
                        .collect();
                    content.push(json!({"type":"text","text": m.content}));
                    out.push(json!({"role": role, "content": content}));
                }
            }
        }
    }
    out
}

fn tools_to_anthropic(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f.get("name")?,
                "description": f.get("description").unwrap_or(&json!("")),
                "input_schema": f.get("parameters").cloned().unwrap_or(json!({"type":"object"}))
            }))
        })
        .collect()
}

/// Accumulated state of one Anthropic `/v1/messages` SSE stream.
#[derive(Default)]
struct AnthropicStreamAcc {
    text: String,
    usage: Option<Usage>,
    model: Option<String>,
    stop_reason: Option<String>,
    error: Option<String>,
    /// Content blocks by `index`; only tool_use blocks accumulate payloads.
    blocks: Vec<ContentBlockAcc>,
}

#[derive(Default)]
struct ContentBlockAcc {
    kind: String,
    id: String,
    name: String,
    json: String,
}

impl AnthropicStreamAcc {
    /// Applies one parsed SSE event; returns the text delta to surface to the
    /// caller (empty for every other event kind). Unknown events (`ping`,
    /// `content_block_stop`, …) are ignored.
    fn apply(&mut self, event: &str, data: &str) -> String {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return String::new();
        };
        // Anthropic always names its events; fall back to data.type when a
        // proxy strips the `event:` lines.
        let kind = if event.is_empty() {
            v.get("type").and_then(Value::as_str).unwrap_or("")
        } else {
            event
        };
        match kind {
            "message_start" => {
                self.model = v
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(usage) = v.pointer("/message").and_then(usage_from_anthropic) {
                    self.usage = Some(usage);
                }
            }
            "content_block_start" => {
                let idx = self.block_index(&v);
                while self.blocks.len() <= idx {
                    self.blocks.push(ContentBlockAcc::default());
                }
                let block = &v["content_block"];
                let slot = &mut self.blocks[idx];
                slot.kind = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                slot.id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                slot.name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
            "content_block_delta" => match v.pointer("/delta/type").and_then(Value::as_str) {
                Some("text_delta") => {
                    let text = v
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    self.text.push_str(text);
                    return text.to_string();
                }
                Some("input_json_delta") => {
                    let idx = self.block_index(&v);
                    if let Some(slot) = self.blocks.get_mut(idx) {
                        slot.json.push_str(
                            v.pointer("/delta/partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        );
                    }
                }
                _ => {}
            },
            "message_delta" => {
                // usage.output_tokens is cumulative; input_tokens is only
                // known from message_start, so keep the previous value.
                let usage = v.get("usage").filter(|u| !u.is_null());
                let input = usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(Value::as_u64);
                let output = usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64);
                if input.is_some() || output.is_some() {
                    let prev = self.usage.unwrap_or_default();
                    self.usage = Some(Usage {
                        input_tokens: input.unwrap_or(prev.input_tokens),
                        output_tokens: output.unwrap_or(prev.output_tokens),
                    });
                }
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
            }
            "error" => {
                if self.error.is_none() {
                    self.error = Some(
                        v.pointer("/error/message")
                            .or_else(|| v.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or(data)
                            .to_string(),
                    );
                }
            }
            _ => {}
        }
        String::new()
    }

    fn block_index(&self, v: &Value) -> usize {
        v.get("index")
            .and_then(Value::as_u64)
            .unwrap_or(self.blocks.len() as u64) as usize
    }

    /// Tool-use blocks accumulated over the stream, as `ToolCall`s.
    fn tool_calls(&self) -> Vec<ToolCall> {
        self.blocks
            .iter()
            .filter(|b| b.kind == "tool_use" && !b.id.is_empty() && !b.name.is_empty())
            .map(|b| ToolCall {
                id: b.id.clone(),
                name: b.name.clone(),
                arguments: serde_json::from_str(&b.json)
                    .unwrap_or(Value::Object(Default::default())),
            })
            .collect()
    }
}

/// Hardcoded catalog used when the real `/v1/models` call fails.
fn fallback_catalog() -> Vec<CatalogModel> {
    vec![
        CatalogModel {
            id: "claude-sonnet-4-5".into(),
            display_name: Some("Claude Sonnet 4.5".into()),
            owned_by: Some("anthropic".into()),
        },
        CatalogModel {
            id: "claude-opus-4-5".into(),
            display_name: Some("Claude Opus 4.5".into()),
            owned_by: Some("anthropic".into()),
        },
    ]
}

/// Parses the `/v1/models` response's `data[]` into catalog entries.
fn parse_models_catalog(v: &Value) -> Vec<CatalogModel> {
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(Value::as_array) {
        for m in arr {
            if let Some(id) = m.get("id").and_then(Value::as_str) {
                out.push(CatalogModel {
                    id: id.to_string(),
                    display_name: m
                        .get("display_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    owned_by: m
                        .get("owned_by")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
    }
    out
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<Completion> {
        let messages = to_anthropic_messages(messages);
        let mut body = anthropic_body(
            model,
            system,
            &messages,
            tools,
            &self.effort,
            self.max_tokens,
            self.temperature,
        );
        let url = format!("{}/v1/messages", self.base);
        let mut resp = send_with_retry(|| self.messages_request(&url, &body)).await?;
        if needs_param_degrade(resp.status, &resp.body, "thinking")
            && body.get("thinking").is_some()
        {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("thinking");
            }
            resp = send_with_retry(|| self.messages_request(&url, &body)).await?;
        }
        if !resp.is_success() {
            anyhow::bail!("anthropic {}: {}", resp.status, truncate(&resp.body, 2000));
        }
        let v: Value = serde_json::from_str(&resp.body)?;
        let mut out_text = String::new();
        let mut tool_calls = Vec::new();
        if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
            for block in arr {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            out_text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        tool_calls.push(ToolCall {
                            id: block
                                .get("id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            name: block
                                .get("name")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: block.get("input").cloned().unwrap_or(json!({})),
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(Completion {
            usage: usage_from_anthropic(&v),
            text: out_text,
            tool_calls,
            model: v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or(model)
                .to_string(),
        })
    }

    async fn complete_streaming(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        let messages = to_anthropic_messages(messages);
        let mut body = anthropic_body(
            model,
            system,
            &messages,
            tools,
            &self.effort,
            self.max_tokens,
            self.temperature,
        );
        body["stream"] = json!(true);
        let url = format!("{}/v1/messages", self.base);
        let resp = send_streaming_with_retry(|| self.messages_request(&url, &body)).await?;
        let resp = ensure_sse_response(resp, "anthropic").await?;

        let mut parser = SseParser::new();
        let mut acc = AnthropicStreamAcc::default();
        let mut stream = std::pin::pin!(resp.bytes_stream());
        while let Some(chunk) = stream.try_next().await? {
            for ev in parser.feed(&chunk) {
                let delta = acc.apply(&ev.event, &ev.data);
                if !delta.is_empty() {
                    on_delta(&delta);
                }
                if let Some(error) = &acc.error {
                    anyhow::bail!("anthropic stream error: {error}");
                }
            }
        }
        tracing::debug!(stop_reason = ?acc.stop_reason, "anthropic stream finished");
        Ok(Completion {
            text: acc.text.clone(),
            tool_calls: acc.tool_calls(),
            model: acc.model.clone().unwrap_or_else(|| model.to_string()),
            usage: acc.usage,
        })
    }

    async fn list_models(&self) -> Result<Vec<CatalogModel>> {
        let url = format!("{}/v1/models", self.base);
        let resp = send_with_retry(|| {
            self.client
                .get(&url)
                .header("x-api-key", &self.key)
                .header("anthropic-version", "2023-06-01")
        })
        .await;
        if let Ok(resp) = resp {
            if resp.is_success() {
                if let Ok(v) = serde_json::from_str::<Value>(&resp.body) {
                    let models = parse_models_catalog(&v);
                    if !models.is_empty() {
                        return Ok(models);
                    }
                }
            }
        }
        Ok(fallback_catalog())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ImageAttachment;

    #[test]
    fn user_images_become_base64_image_blocks_before_text() {
        let out = to_anthropic_messages(&[ChatMessage {
            role: "user".into(),
            content: "describe".into(),
            images: vec![ImageAttachment {
                media_type: "image/jpeg".into(),
                data: "QUJD".into(),
            }],
            ..Default::default()
        }]);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[0]["source"]["data"], "QUJD");
        assert_eq!(content[1], json!({"type":"text","text":"describe"}));
    }

    #[test]
    fn thinking_budget_matches_effort_levels() {
        assert_eq!(thinking_budget("low"), None);
        assert_eq!(thinking_budget("medium"), None);
        assert_eq!(thinking_budget("high"), Some(4096));
        assert_eq!(thinking_budget("xhigh"), Some(8192));
        assert_eq!(thinking_budget("max"), Some(16384));
        assert_eq!(thinking_budget("ultra"), Some(24576));
        assert_eq!(thinking_budget("bogus"), None);
    }

    #[test]
    fn anthropic_body_enables_thinking_and_skips_temperature() {
        let body = anthropic_body("claude", "sys", &[], &[], "xhigh", None, Some(0.7));
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 8192})
        );
        assert_eq!(body["max_tokens"], 16384); // budget + 8192
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn anthropic_body_max_tokens_prefers_larger_config_value() {
        let body = anthropic_body("claude", "sys", &[], &[], "max", Some(40000), None);
        assert_eq!(body["max_tokens"], 40000);
        let body = anthropic_body("claude", "sys", &[], &[], "max", Some(100), None);
        assert_eq!(body["max_tokens"], 16384 + 8192);
    }

    #[test]
    fn anthropic_body_ultra_gets_the_widest_thinking_window() {
        let body = anthropic_body("claude", "sys", &[], &[], "ultra", None, Some(0.7));
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 24576})
        );
        assert_eq!(body["max_tokens"], 24576 + 16384); // ultra headroom
        assert!(body.get("temperature").is_none());
        // A larger configured max_tokens still wins; a smaller one is raised.
        let body = anthropic_body("claude", "sys", &[], &[], "ultra", Some(60000), None);
        assert_eq!(body["max_tokens"], 60000);
        let body = anthropic_body("claude", "sys", &[], &[], "ultra", Some(100), None);
        assert_eq!(body["max_tokens"], 24576 + 16384);
    }

    #[test]
    fn anthropic_body_low_effort_keeps_legacy_shape() {
        let body = anthropic_body("claude", "sys", &[], &[], "low", None, Some(0.5));
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["temperature"], 0.5);
        let body = anthropic_body("claude", "sys", &[], &[], "medium", None, None);
        assert!(body.get("thinking").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn anthropic_body_includes_tools_only_when_present() {
        let body = anthropic_body("claude", "sys", &[], &[], "low", None, None);
        assert!(body.get("tools").is_none());
        let tool =
            json!({"type":"function","function":{"name":"ls","description":"","parameters":{}}});
        let body = anthropic_body("claude", "sys", &[], &[tool], "low", None, None);
        assert_eq!(body["tools"][0]["name"], "ls");
    }

    #[test]
    fn parse_models_catalog_reads_data_array() {
        let v = json!({"data": [
            {"id": "claude-x", "display_name": "Claude X", "type": "model"},
            {"id": "claude-y"}
        ]});
        let out = parse_models_catalog(&v);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "claude-x");
        assert_eq!(out[0].display_name.as_deref(), Some("Claude X"));
        assert_eq!(out[0].owned_by, None);
        assert_eq!(out[1].id, "claude-y");
        assert_eq!(out[1].display_name, None);
    }

    #[test]
    fn fallback_catalog_still_lists_two_models() {
        let cats = fallback_catalog();
        assert_eq!(cats.len(), 2);
        assert_eq!(cats[0].id, "claude-sonnet-4-5");
        assert_eq!(cats[1].id, "claude-opus-4-5");
    }

    #[test]
    fn anthropic_stream_full_sequence_yields_text_usage_and_tool_calls() {
        let mut acc = AnthropicStreamAcc::default();
        assert_eq!(
            acc.apply(
                "message_start",
                r#"{"type":"message_start","message":{"model":"claude-x","usage":{"input_tokens":12,"output_tokens":0}}}"#,
            ),
            ""
        );
        assert_eq!(
            acc.apply(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            ""
        );
        assert_eq!(
            acc.apply(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            ),
            "Hel"
        );
        assert_eq!(
            acc.apply(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            ),
            "lo"
        );
        // Ignored events.
        assert_eq!(acc.apply("ping", r#"{"type":"ping"}"#), "");
        assert_eq!(
            acc.apply(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            ""
        );
        // A tool_use block interleaves and accumulates its JSON arguments.
        assert_eq!(
            acc.apply(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"read_file"}}"#,
            ),
            ""
        );
        assert_eq!(
            acc.apply(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            ),
            ""
        );
        assert_eq!(
            acc.apply(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"a\"}"}}"#,
            ),
            ""
        );
        // message_delta output_tokens are cumulative; input_tokens persist.
        assert_eq!(
            acc.apply(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            ),
            ""
        );
        assert_eq!(acc.apply("message_stop", r#"{"type":"message_stop"}"#), "");

        assert_eq!(acc.text, "Hello");
        assert_eq!(
            acc.usage,
            Some(Usage {
                input_tokens: 12,
                output_tokens: 9
            })
        );
        assert_eq!(acc.model.as_deref(), Some("claude-x"));
        assert_eq!(acc.stop_reason.as_deref(), Some("tool_use"));
        let calls = acc.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tu_1");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, serde_json::json!({"path": "a"}));
    }

    #[test]
    fn anthropic_stream_records_in_band_errors_and_never_seen_usage_stays_none() {
        let mut acc = AnthropicStreamAcc::default();
        acc.apply("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#);
        assert!(acc.error.is_none());
        assert_eq!(acc.usage, None);
        acc.apply(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert_eq!(acc.error.as_deref(), Some("Overloaded"));
    }

    #[test]
    fn anthropic_stream_events_survive_arbitrary_chunk_splits() {
        let raw = "event: message_start\n\
                   data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-y\",\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\
                   \n\
                   event: content_block_delta\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好 \"}}\n\
                   \n\
                   event: content_block_delta\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"世界\"}}\n\
                   \n\
                   event: message_delta\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":6}}\n\
                   \n\
                   event: message_stop\n\
                   data: {\"type\":\"message_stop\"}\n\
                   \n";
        let bytes = raw.as_bytes();
        let mut parser = SseParser::new();
        let mut acc = AnthropicStreamAcc::default();
        let mut deltas = Vec::new();
        for chunk in bytes.chunks(7) {
            for ev in parser.feed(chunk) {
                let delta = acc.apply(&ev.event, &ev.data);
                if !delta.is_empty() {
                    deltas.push(delta);
                }
            }
        }
        assert_eq!(deltas, vec!["你好 ", "世界"]);
        assert_eq!(acc.text, "你好 世界");
        assert_eq!(
            acc.usage,
            Some(Usage {
                input_tokens: 4,
                output_tokens: 6
            })
        );
    }
}
