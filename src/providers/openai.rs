use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config::Config;
use crate::providers::{
    ensure_sse_response, first_choice, needs_param_degrade, openai_messages,
    parse_openai_tool_calls, read_openai_stream, reasoning_effort, send_streaming_with_retry,
    send_with_retry, truncate, usage_from_openai, CatalogModel, Completion, Provider,
};
use crate::session::ChatMessage;

pub struct OpenAiProvider {
    client: reqwest::Client,
    base: String,
    key: String,
    effort: String,
    temperature: Option<f32>,
}

impl OpenAiProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        let key = cfg
            .openai_api_key
            .clone()
            .filter(|key| !key.trim().is_empty())
            .context("OpenAI API key missing")?;
        let base = super::api_v1_base(
            cfg.openai_base_url
                .as_deref()
                .unwrap_or("https://api.openai.com"),
        )?;
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(180))
                .build()?,
            base,
            key,
            effort: cfg.effort.trim().to_lowercase(),
            temperature: cfg.temperature,
        })
    }

    fn chat_request(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client.post(url).bearer_auth(&self.key).json(body)
    }
}

/// Builds the `/v1/chat/completions` request body. `temperature` is only sent
/// when configured; `reasoning_effort` is sent whenever the effort maps to a
/// known level (models that reject it are handled by graceful degradation).
fn openai_body(
    model: &str,
    msgs: &[Value],
    tools: &[Value],
    effort: &str,
    temperature: Option<f32>,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": msgs,
    });
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = reasoning_effort(effort) {
        body["reasoning_effort"] = json!(effort);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
    }
    body
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<Completion> {
        let mut msgs = vec![json!({"role":"system","content": system})];
        msgs.extend(openai_messages(messages));
        let mut body = openai_body(model, &msgs, tools, &self.effort, self.temperature);
        let url = format!("{}/chat/completions", self.base);
        let mut resp = send_with_retry(|| self.chat_request(&url, &body)).await?;
        if needs_param_degrade(resp.status, &resp.body, "reasoning_effort")
            && body.get("reasoning_effort").is_some()
        {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("reasoning_effort");
            }
            resp = send_with_retry(|| self.chat_request(&url, &body)).await?;
        }
        if !resp.is_success() {
            anyhow::bail!("openai {}: {}", resp.status, truncate(&resp.body, 2000));
        }
        let v: Value = serde_json::from_str(&resp.body)?;
        let choice = first_choice(&v).ok_or_else(|| {
            anyhow::anyhow!(
                "openai: malformed response (missing/empty choices): {}",
                truncate(&resp.body, 500)
            )
        })?;
        let message = choice.get("message").cloned().unwrap_or(Value::Null);
        let tool_calls = message
            .get("tool_calls")
            .map(|t| parse_openai_tool_calls(t))
            .unwrap_or_default();
        let (text, images) = super::parse_openai_message(&message)?;
        anyhow::ensure!(
            !text.trim().is_empty() || !images.is_empty() || !tool_calls.is_empty(),
            "OpenAI returned an empty completion"
        );
        Ok(Completion {
            usage: usage_from_openai(&v),
            text,
            images,
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
        let mut body = openai_body(model, &msgs, tools, &self.effort, self.temperature);
        body["stream"] = json!(true);
        let url = format!("{}/chat/completions", self.base);
        let resp = send_streaming_with_retry(|| self.chat_request(&url, &body)).await?;
        let resp = ensure_sse_response(resp, "openai").await?;
        read_openai_stream(resp, on_delta, model).await
    }

    async fn list_models(&self) -> Result<Vec<CatalogModel>> {
        let url = format!("{}/models", self.base);
        let resp = send_with_retry(|| self.client.get(&url).bearer_auth(&self.key)).await?;
        if !resp.is_success() {
            anyhow::bail!("openai model catalog HTTP {}", resp.status);
        }
        let v: Value = serde_json::from_str(&resp.body).context("invalid model catalog JSON")?;
        super::parse_models_catalog(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_body_maps_effort_and_omits_unset_temperature() {
        let body = openai_body("gpt-5", &[], &[], "max", None);
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("temperature").is_none());
        let body = openai_body("gpt-5", &[], &[], "medium", Some(0.3));
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["temperature"], json!(0.3_f32));
        let body = openai_body("gpt-5", &[], &[], "low", None);
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn openai_body_ultra_clamps_to_high() {
        let body = openai_body("gpt-5", &[], &[], "ultra", None);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn openai_body_includes_tools_only_when_present() {
        let body = openai_body("m", &[], &[], "low", None);
        assert!(body.get("tools").is_none());
        let tools = vec![json!({"type":"function","function":{"name":"f"}})];
        let body = openai_body("m", &[], &tools, "low", None);
        assert_eq!(body["tools"][0]["function"]["name"], "f");
    }

    #[test]
    fn first_choice_requires_nonempty_choices() {
        assert!(first_choice(&json!({"error": "quota"})).is_none());
        assert!(first_choice(&json!({"choices": []})).is_none());
        let ok = json!({"choices": [{"message": {"content": "hi"}}]});
        assert_eq!(first_choice(&ok).unwrap()["message"]["content"], "hi");
    }
}
