use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config::Config;
use crate::providers::{
    ensure_sse_response, first_choice, needs_param_degrade, openai_messages,
    parse_openai_tool_calls, read_openai_stream, reasoning_effort, send_streaming_with_retry,
    send_with_retry, truncate, usage_from_openai, CatalogModel, Completion, Provider,
};
use crate::session::ChatMessage;

pub struct ProxyProvider {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
    effort: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl ProxyProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()?,
            base: cfg.proxy_url.trim_end_matches('/').to_string(),
            token: cfg.proxy_token.clone(),
            effort: cfg.effort.trim().to_lowercase(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
        })
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) if !t.is_empty() => req.bearer_auth(t).header("x-api-key", t),
            _ => req,
        }
    }

    fn chat_request(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        self.auth(self.client.post(url).json(body))
    }
}

/// Builds the `/v1/chat/completions` request body. `temperature` defaults to
/// 0.2 unless configured; `max_tokens` is only sent when configured;
/// `reasoning_effort` follows the same mapping as the openai provider.
fn proxy_body(
    model: &str,
    msgs: &[Value],
    tools: &[Value],
    effort: &str,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) -> Value {
    // keep the literal f64 default so the wire value stays exactly 0.2
    let temperature = match temperature {
        Some(t) => json!(t),
        None => json!(0.2),
    };
    let mut body = json!({
        "model": model,
        "messages": msgs,
        "temperature": temperature,
    });
    if let Some(mt) = max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(effort) = reasoning_effort(effort) {
        body["reasoning_effort"] = json!(effort);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = json!("auto");
    }
    body
}

#[async_trait::async_trait]
impl Provider for ProxyProvider {
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<Completion> {
        let mut msgs = vec![json!({"role":"system","content": system})];
        msgs.extend(openai_messages(messages));
        let mut body = proxy_body(
            model,
            &msgs,
            tools,
            &self.effort,
            self.max_tokens,
            self.temperature,
        );
        let url = format!("{}/v1/chat/completions", self.base);
        let mut resp = send_with_retry(|| self.chat_request(&url, &body))
            .await
            .with_context(|| format!("POST {url}"))?;
        if needs_param_degrade(resp.status, &resp.body, "reasoning_effort")
            && body.get("reasoning_effort").is_some()
        {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("reasoning_effort");
            }
            resp = send_with_retry(|| self.chat_request(&url, &body)).await?;
        }
        if !resp.is_success() {
            anyhow::bail!("proxy {}: {}", resp.status, truncate(&resp.body, 2000));
        }
        let v: Value = serde_json::from_str(&resp.body).context("proxy json")?;
        let choice = first_choice(&v).ok_or_else(|| {
            anyhow::anyhow!(
                "proxy: malformed response (missing/empty choices): {}",
                truncate(&resp.body, 500)
            )
        })?;
        let message = choice.get("message").cloned().unwrap_or(Value::Null);
        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let tool_calls = message
            .get("tool_calls")
            .map(|t| parse_openai_tool_calls(t))
            .unwrap_or_default();
        Ok(Completion {
            usage: usage_from_openai(&v),
            text: content,
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
        let mut msgs = vec![json!({"role":"system","content": system})];
        msgs.extend(openai_messages(messages));
        let mut body = proxy_body(
            model,
            &msgs,
            tools,
            &self.effort,
            self.max_tokens,
            self.temperature,
        );
        body["stream"] = json!(true);
        let url = format!("{}/v1/chat/completions", self.base);
        let resp = send_streaming_with_retry(|| self.chat_request(&url, &body)).await?;
        let resp = ensure_sse_response(resp, "proxy").await?;
        read_openai_stream(resp, on_delta, model).await
    }

    async fn list_models(&self) -> Result<Vec<CatalogModel>> {
        let url = format!("{}/v1/models", self.base);
        let resp = send_with_retry(|| self.auth(self.client.get(&url))).await?;
        if !resp.is_success() {
            anyhow::bail!(
                "proxy models {}: {}",
                resp.status,
                truncate(&resp.body, 800)
            );
        }
        let v: Value = serde_json::from_str(&resp.body)?;
        let mut out = Vec::new();
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            for m in arr {
                if let Some(id) = m.get("id").and_then(|x| x.as_str()) {
                    out.push(CatalogModel {
                        id: id.to_string(),
                        display_name: m
                            .get("display_name")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string()),
                        owned_by: m
                            .get("owned_by")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_body_defaults_temperature_and_honors_config() {
        let body = proxy_body("m", &[], &[], "low", None, None);
        assert_eq!(body["temperature"], 0.2);
        let body = proxy_body("m", &[], &[], "low", None, Some(0.9));
        assert_eq!(body["temperature"], json!(0.9_f32));
    }

    #[test]
    fn proxy_body_maps_effort_and_respects_max_tokens() {
        let body = proxy_body("m", &[], &[], "xhigh", Some(2048), None);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 2048);
        let body = proxy_body("m", &[], &[], "low", None, None);
        assert_eq!(body["reasoning_effort"], "low");
        assert!(body.get("max_tokens").is_none());
        let body = proxy_body("m", &[], &[], "unsupported", None, None);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn proxy_body_ultra_clamps_to_high() {
        let body = proxy_body("m", &[], &[], "ultra", None, None);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn proxy_body_sets_tool_choice_with_tools() {
        let body = proxy_body("m", &[], &[], "low", None, None);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        let tools = vec![json!({"type":"function","function":{"name":"f"}})];
        let body = proxy_body("m", &[], &tools, "low", None, None);
        assert_eq!(body["tools"][0]["function"]["name"], "f");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn truncate_never_panics_on_multibyte_turkish_text() {
        let s = "İğüşöç İğüşöç";
        // byte 5 lands mid-character; the cut backs off instead of panicking
        let cut = truncate(s, 5);
        assert_eq!(cut, "İğ");
        assert!(s.is_char_boundary(cut.len()));
        assert_eq!(truncate(s, 13), "İğüşöç ");
        assert_eq!(truncate(s, 100), s);
    }
}
