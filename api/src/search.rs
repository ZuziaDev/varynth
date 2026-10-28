use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::ApiError;

#[derive(Debug)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub async fn search(
    http: &reqwest::Client,
    cfg: &Config,
    cursors: &SearchCursors,
    provider: &str,
    query: &str,
) -> Result<(String, Vec<Hit>), ApiError> {
    let names: Vec<&str> = match provider {
        "" | "auto" => vec!["parallel", "exa", "firecrawl"],
        "parallel" | "exa" | "firecrawl" => vec![provider],
        _ => return Err(ApiError::bad("provider must be auto, parallel, exa, or firecrawl")),
    };
    let mut last = ApiError::Unavailable;
    for name in names {
        match one(http, cfg, cursors, name, query).await {
            Ok(hits) if !hits.is_empty() => return Ok((name.to_string(), hits)),
            Ok(_) => last = ApiError::Unavailable,
            Err(error) => last = error,
        }
    }
    Err(last)
}

async fn one(
    http: &reqwest::Client,
    cfg: &Config,
    cursors: &SearchCursors,
    provider: &str,
    query: &str,
) -> Result<Vec<Hit>, ApiError> {
    let (keys, cursor) = match provider {
        "parallel" => (&cfg.parallel_keys, &cursors.parallel),
        "exa" => (&cfg.exa_keys, &cursors.exa),
        "firecrawl" => (&cfg.firecrawl_keys, &cursors.firecrawl),
        _ => return Err(ApiError::bad("unknown search provider")),
    };
    if keys.is_empty() {
        return Err(ApiError::Unavailable);
    }
    let key = keys[cursor.fetch_add(1, Ordering::Relaxed) % keys.len()].clone();
    let response = match provider {
        "parallel" => {
            http.post("https://api.parallel.ai/v1beta/search")
                .header("x-api-key", &key)
                .json(&json!({
                    "objective": query,
                    "search_queries": [query],
                    "max_results": 5
                }))
                .send()
                .await
        }
        "exa" => {
            http.post("https://api.exa.ai/search")
                .header("x-api-key", &key)
                .json(&json!({
                    "query": query,
                    "numResults": 5,
                    "contents": {"text": {"maxCharacters": 400}}
                }))
                .send()
                .await
        }
        "firecrawl" => {
            http.post("https://api.firecrawl.dev/v1/search")
                .bearer_auth(&key)
                .json(&json!({"query": query, "limit": 5}))
                .send()
                .await
        }
        _ => return Err(ApiError::bad("unknown search provider")),
    };
    let response = response.map_err(|error| {
        tracing::warn!(%error, provider, "search transport");
        ApiError::Unavailable
    })?;
    let status = response.status();
    let text = response.text().await.map_err(|_| ApiError::Unavailable)?;
    if !status.is_success() {
        tracing::warn!(provider, status = %status, "search upstream failed");
        return Err(ApiError::Unavailable);
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| ApiError::Unavailable)?;
    Ok(redact_hits(extract(provider, &value), cfg))
}

fn extract(provider: &str, value: &Value) -> Vec<Hit> {
    let rows = match provider {
        "exa" => value.get("results").and_then(|v| v.as_array()),
        "firecrawl" => value
            .pointer("/data/web")
            .and_then(|v| v.as_array())
            .or_else(|| value.get("data").and_then(|v| v.as_array())),
        _ => value
            .get("results")
            .and_then(|v| v.as_array())
            .or_else(|| value.pointer("/data/results").and_then(|v| v.as_array())),
    };
    let Some(rows) = rows else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let url = row
                .get("url")
                .or_else(|| row.get("link"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if url.is_empty() {
                return None;
            }
            let title = row
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&url)
                .trim()
                .to_string();
            let snippet = row
                .get("snippet")
                .or_else(|| row.get("text"))
                .or_else(|| row.get("description"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    row.get("excerpts")
                        .and_then(|v| v.as_array())
                        .and_then(|items| items.first())
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .chars()
                .take(400)
                .collect();
            Some(Hit { title, url, snippet })
        })
        .take(5)
        .collect()
}

fn redact_hits(mut hits: Vec<Hit>, cfg: &Config) -> Vec<Hit> {
    for hit in &mut hits {
        hit.title = redact(&hit.title, cfg);
        hit.url = redact(&hit.url, cfg);
        hit.snippet = redact(&hit.snippet, cfg);
    }
    hits
}

fn redact(text: &str, cfg: &Config) -> String {
    let mut out = text.to_string();
    for secret in cfg
        .parallel_keys
        .iter()
        .chain(cfg.exa_keys.iter())
        .chain(cfg.firecrawl_keys.iter())
        .chain(cfg.worker_gateway_key.iter())
    {
        if secret.len() >= 8 {
            out = out.replace(secret, "[redacted]");
        }
    }
    out
}

pub struct SearchCursors {
    pub parallel: AtomicUsize,
    pub exa: AtomicUsize,
    pub firecrawl: AtomicUsize,
}

impl SearchCursors {
    pub fn new() -> Self {
        Self {
            parallel: AtomicUsize::new(0),
            exa: AtomicUsize::new(0),
            firecrawl: AtomicUsize::new(0),
        }
    }
}

pub fn hits_json(provider: &str, hits: &[Hit]) -> Value {
    json!({
        "provider": provider,
        "results": hits.iter().map(|hit| json!({
            "title": hit.title,
            "url": hit.url,
            "snippet": hit.snippet
        })).collect::<Vec<_>>()
    })
}
