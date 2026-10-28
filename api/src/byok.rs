use std::time::Duration;

use axum::http::header;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::{
    anthropic_body, anthropic_to_openai, completion_to_sse, prepare_upstream_body, rewrite_model,
    rewrite_sse, scrubbed_error,
};
use crate::error::ApiError;

#[derive(Clone, Debug)]
pub enum ByokUpstream {
    Json(Value),
    Sse(String),
}

#[derive(Clone, Debug)]
pub struct ByokMatch {
    pub provider_id: String,
    pub name: String,
    pub api_key: String,
    pub base_url: String,
    pub protocol: String,
    pub model: String,
}

pub fn default_or_custom_models(provider_id: &str, models_str: &str) -> Vec<String> {
    let custom: Vec<String> = models_str
        .split([',', ';', '\n', '\r'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !custom.is_empty() {
        return custom;
    }
    match provider_id {
        "openai" => vec![
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "o1".into(),
            "o1-preview".into(),
            "o1-mini".into(),
            "gpt-4-turbo".into(),
            "gpt-3.5-turbo".into(),
        ],
        "anthropic" => vec![
            "claude-3-5-sonnet-20241022".into(),
            "claude-3-5-sonnet-latest".into(),
            "claude-3-5-haiku-20241022".into(),
            "claude-3-5-haiku-latest".into(),
            "claude-3-opus-20240229".into(),
        ],
        "gemini" => vec![
            "gemini-2.5-pro".into(),
            "gemini-2.5-flash".into(),
            "gemini-2.0-flash".into(),
            "gemini-2.0-flash-exp".into(),
            "gemini-1.5-pro".into(),
            "gemini-1.5-flash".into(),
        ],
        "deepseek" => vec![
            "deepseek-chat".into(),
            "deepseek-coder".into(),
            "deepseek-reasoner".into(),
        ],
        "openrouter" => vec![
            "anthropic/claude-3.5-sonnet".into(),
            "meta-llama/llama-3.3-70b-instruct".into(),
            "google/gemini-2.0-flash-exp:free".into(),
            "deepseek/deepseek-r1".into(),
        ],
        "groq" => vec![
            "llama-3.3-70b-versatile".into(),
            "mixtral-8x7b-32768".into(),
            "deepseek-r1-distill-llama-70b".into(),
        ],
        _ => Vec::new(),
    }
}

pub fn match_byok_model(provider_id: &str, model_item: &str, requested: &str) -> bool {
    if model_item.eq_ignore_ascii_case(requested) {
        return true;
    }
    if let Some((p, m)) = requested.split_once('/') {
        if p.eq_ignore_ascii_case(provider_id) && model_item.eq_ignore_ascii_case(m) {
            return true;
        }
    }
    false
}

pub async fn find_user_byok_for_model(
    pool: &PgPool,
    user_id: Uuid,
    requested: &str,
) -> Result<Option<ByokMatch>, ApiError> {
    let rows = sqlx::query_as::<_, (String, String, String, String, String, String)>(
        "SELECT provider_id, name, api_key, base_url, models, protocol
         FROM user_byok_providers
         WHERE user_id = $1 AND enabled = true AND api_key != ''",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    for (provider_id, name, api_key, base_url, models_str, protocol) in rows {
        let models = default_or_custom_models(&provider_id, &models_str);
        for m in &models {
            if match_byok_model(&provider_id, m, requested) {
                return Ok(Some(ByokMatch {
                    provider_id,
                    name,
                    api_key,
                    base_url,
                    protocol,
                    model: m.clone(),
                }));
            }
        }
    }
    Ok(None)
}

pub async fn fetch_user_byok_models(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<Value>, ApiError> {
    let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT provider_id, name, models, protocol, base_url
         FROM user_byok_providers
         WHERE user_id = $1 AND enabled = true AND api_key != ''
         ORDER BY created_at ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for (provider_id, name, models_str, protocol, _base_url) in rows {
        let models = default_or_custom_models(&provider_id, &models_str);
        for m in models {
            out.push(json!({
                "id": m,
                "display_name": format!("{} ({})", m, name),
                "owned_by": provider_id,
                "protocol": protocol,
                "byok": true
            }));
        }
    }
    Ok(out)
}

pub async fn call_byok_upstream(
    http: &reqwest::Client,
    byok: &ByokMatch,
    body: &Value,
    stream: bool,
) -> Result<ByokUpstream, ApiError> {
    if byok.protocol == "anthropic" {
        let payload = anthropic_body(body, &byok.model)?;
        let base = if byok.base_url.is_empty() {
            "https://api.anthropic.com".to_string()
        } else {
            byok.base_url.trim_end_matches('/').to_string()
        };
        let url = if base.ends_with("/messages") {
            base
        } else {
            format!("{base}/v1/messages")
        };
        let response = http
            .post(url)
            .timeout(Duration::from_secs(60))
            .header("x-api-key", byok.api_key.trim())
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send()
            .await
            .map_err(|_| ApiError::Unavailable)?;

        let status = response.status();
        let text = response.text().await.map_err(|_| ApiError::Unavailable)?;
        if !status.is_success() {
            return Err(scrubbed_error(status, &text));
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| ApiError::Unavailable)?;
        let completion = anthropic_to_openai(value, &byok.model);
        return Ok(if stream {
            ByokUpstream::Sse(completion_to_sse(&completion))
        } else {
            ByokUpstream::Json(completion)
        });
    }

    // Protocol: OpenAI / Gemini / Custom
    let base = if !byok.base_url.is_empty() {
        byok.base_url.trim_end_matches('/').to_string()
    } else {
        match byok.provider_id.as_str() {
            "gemini" => "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            "deepseek" => "https://api.deepseek.com".to_string(),
            "openrouter" => "https://openrouter.ai/api/v1".to_string(),
            "groq" => "https://api.groq.com/openai/v1".to_string(),
            _ => "https://api.openai.com/v1".to_string(),
        }
    };
    let url = if base.ends_with("/chat/completions") {
        base
    } else {
        format!("{base}/chat/completions")
    };
    let payload = prepare_upstream_body(body.clone(), &byok.model, stream);

    let response = http
        .post(url)
        .timeout(Duration::from_secs(60))
        .header(header::AUTHORIZATION, format!("Bearer {}", byok.api_key.trim()))
        .json(&payload)
        .send()
        .await
        .map_err(|_| ApiError::Unavailable)?;

    let status = response.status();
    let text = response.text().await.map_err(|_| ApiError::Unavailable)?;
    if !status.is_success() {
        return Err(scrubbed_error(status, &text));
    }
    if text.trim_start().starts_with("data:") {
        return Ok(ByokUpstream::Sse(rewrite_sse(&text, &byok.model)));
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| ApiError::Unavailable)?;
    let value = rewrite_model(value, &byok.model);
    Ok(if stream {
        ByokUpstream::Sse(completion_to_sse(&value))
    } else {
        ByokUpstream::Json(value)
    })
}
