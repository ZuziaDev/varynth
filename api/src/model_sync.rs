use std::collections::HashSet;
use std::time::Duration;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::error::ApiError;
use crate::routes::AppState;

pub const FREE_MODEL_IDS: &[&str] = &[
    "gpt-oss-120b",
    "qwen3.8-27b",
    "nemotron-3-120b-a12b",
    "qwen3-30b-a3b-fp8",
    "qwen2.5-coder-32b-instruct",
    "deepseek-r1-distill-qwen-32b",
    "qwq-32b",
    "llama-4-scout-17b-16e-instruct",
    "llama-3.3-70b-instruct-fp8-fast",
    "gemma-4-26b-a4b-it",
    "glm-4.7-flash",
    "mistral-small-3.1-24b-instruct",
    "gpt-oss-20b",
    "gemma-sea-lion-v4-27b-it",
    "llama-3.1-8b-instruct-fast",
    "llama-3.1-8b-instruct-fp8",
    "granite-4.0-h-micro",
    "llama-3.2-3b-instruct",
    "llama-3.2-1b-instruct",
    "gemma-7b-it-lora",
    "gemma-2b-it-lora",
];

#[derive(Debug, Default, serde::Serialize)]
pub struct SyncReport {
    pub total_upstream: usize,
    pub activated: usize,
    pub deactivated: usize,
    pub added: usize,
}

pub fn infer_provider(model_id: &str) -> &'static str {
    let lower = model_id.to_ascii_lowercase();
    if lower.starts_with("qwen") || lower.starts_with("qwq") {
        "qwen"
    } else if lower.starts_with("llama")
        || lower.starts_with("meta-")
        || lower.starts_with("muse")
        || lower.starts_with("codellama")
    {
        "meta"
    } else if lower.starts_with("gemini")
        || lower.starts_with("gemma")
        || lower.starts_with("codegemma")
        || lower.starts_with("imagen")
        || lower.starts_with("deplot")
        || lower.starts_with("diffusiongemma")
        || lower.starts_with("recurrentgemma")
    {
        "google"
    } else if lower.starts_with("claude") {
        "anthropic"
    } else if lower.starts_with("gpt") {
        "openai"
    } else if lower.starts_with("deepseek") || lower.starts_with("jb-deepseek") {
        "deepseek"
    } else if lower.starts_with("mistral")
        || lower.starts_with("mixtral")
        || lower.starts_with("codestral")
    {
        "mistralai"
    } else if lower.starts_with("nemotron")
        || lower.starts_with("cosmos")
        || lower.starts_with("ising")
    {
        "nvidia"
    } else if lower.starts_with("granite") {
        "ibm"
    } else if lower.starts_with("palmyra") {
        "writer"
    } else if lower.starts_with("phi") || lower.starts_with("kosmos") {
        "microsoft"
    } else if lower.starts_with("flux") {
        "black-forest-labs"
    } else if lower.starts_with("image")
        || lower.starts_with("ideogram")
        || lower.starts_with("nano-banana")
        || lower.starts_with("gen4")
    {
        "stabilityai"
    } else if lower.starts_with("kimi") {
        "moonshotai"
    } else if lower.starts_with("glm") {
        "zai"
    } else if lower.starts_with("minimax") {
        "minimax"
    } else if lower.starts_with("intern") {
        "internlm"
    } else if lower.starts_with("grok") {
        "xai"
    } else {
        "multimodal"
    }
}

pub fn infer_category(model_id: &str) -> &'static str {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("vision")
        || lower.contains("vl")
        || lower.contains("scout")
        || lower.contains("image")
        || lower.contains("flux")
        || lower.contains("fuyu")
    {
        "vision"
    } else if lower.contains("coder") || lower.contains("code") || lower.contains("instruct") {
        "coding"
    } else if lower.contains("flash")
        || lower.contains("micro")
        || lower.contains("nano")
        || lower.contains("fast")
        || lower.contains("lite")
        || lower.contains("mini")
        || lower.contains("guard")
        || lower.contains("0.")
        || lower.contains("-1b")
        || lower.contains("-2b")
        || lower.contains("-3b")
    {
        "fast"
    } else {
        "reasoning"
    }
}

pub fn infer_pricing_and_limits(model_id: &str, is_free: bool) -> (f64, f64, i32, i32, i32) {
    if is_free {
        return (0.0, 0.0, 1, 20, 100);
    }
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("opus")
        || lower.contains("340b")
        || lower.contains("397b")
        || lower.contains("ultra")
        || lower.contains("550b")
        || lower.contains("gpt-5.6")
    {
        (12.0, 60.0, 5, 15, 200)
    } else if lower.contains("sonnet")
        || lower.contains("70b")
        || lower.contains("72b")
        || lower.contains("pro")
        || lower.contains("large")
    {
        (3.0, 15.0, 2, 35, 600)
    } else if lower.contains("flash")
        || lower.contains("mini")
        || lower.contains("micro")
        || lower.contains("nano")
        || lower.contains("haiku")
        || lower.contains("fast")
        || lower.contains("8b")
        || lower.contains("9b")
        || lower.contains("12b")
        || lower.contains("27b")
    {
        (0.6, 2.4, 1, 60, 2000)
    } else {
        (1.5, 6.0, 2, 40, 1000)
    }
}

pub fn format_display_name(model_id: &str) -> String {
    let parts: Vec<&str> = model_id.split('-').collect();
    let capitalized: Vec<String> = parts
        .into_iter()
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect();
    capitalized.join(" ")
}

pub async fn sync_models_with_upstream(state: &AppState) -> Result<SyncReport, ApiError> {
    let key = match state.config.hrmn_api_key.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => {
            warn!("HRMN API key is not configured; skipping upstream model sync");
            return Ok(SyncReport::default());
        }
    };

    let base_url = state.config.hrmn_base_url.trim_end_matches('/');
    let url = format!("{}/models", base_url);

    let resp = state
        .http
        .get(&url)
        .bearer_auth(key)
        .header("User-Agent", "varynth-api/0.1.3")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            error!("Failed to request models from upstream {}: {}", url, e);
            ApiError::Unavailable
        })?;

    if !resp.status().is_success() {
        error!("Upstream {} returned HTTP status: {}", url, resp.status());
        return Err(ApiError::Unavailable);
    }

    let body = resp.json::<Value>().await.map_err(|e| {
        error!("Failed to parse JSON response from upstream {}: {}", url, e);
        ApiError::Unavailable
    })?;

    let upstream_models: HashSet<String> = body
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    if upstream_models.len() < 10 {
        warn!(
            "Upstream returned suspiciously low model count ({}). Skipping sync to prevent accidental wipe.",
            upstream_models.len()
        );
        return Ok(SyncReport {
            total_upstream: upstream_models.len(),
            ..Default::default()
        });
    }

    info!(
        "Upstream returned {} active models. Syncing with Varynth database...",
        upstream_models.len()
    );

    // 1. Fetch current models with upstream_kind = 'hrmn'
    let current_hrmn_rows = sqlx::query_as::<_, (String, String, bool)>(
        "SELECT id, upstream_model, enabled FROM models WHERE upstream_kind = 'hrmn'",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut report = SyncReport {
        total_upstream: upstream_models.len(),
        ..Default::default()
    };

    let mut db_model_ids = HashSet::new();

    // 2. Disable & clean up models that are no longer present upstream
    for (id, upstream_model, enabled) in current_hrmn_rows {
        db_model_ids.insert(upstream_model.clone());
        if upstream_models.contains(&upstream_model) {
            if !enabled {
                sqlx::query("UPDATE models SET enabled = true WHERE id = $1")
                    .bind(&id)
                    .execute(&state.pool)
                    .await?;
                report.activated += 1;
            }
        } else {
            // Model removed from upstream (e.g. muse-wonder-1.0)
            if enabled {
                sqlx::query("UPDATE models SET enabled = false WHERE id = $1")
                    .bind(&id)
                    .execute(&state.pool)
                    .await?;
                report.deactivated += 1;
            }
            // Delete from provider_models so it never displays in provider vendor listings
            sqlx::query("DELETE FROM provider_models WHERE model_id = $1")
                .bind(&upstream_model)
                .execute(&state.pool)
                .await?;
        }
    }

    // 3. Insert newly discovered models from upstream
    for upstream_id in &upstream_models {
        if !db_model_ids.contains(upstream_id) {
            let is_free = FREE_MODEL_IDS.contains(&upstream_id.as_str());
            let varynth_id = format!("varynth/{}", upstream_id);

            // Check if this model ID is already registered under a non-hrmn kind (e.g. worker_gateway)
            let exists_non_hrmn = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM models WHERE id = $1 AND upstream_kind != 'hrmn')",
            )
            .bind(&varynth_id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(false);

            if exists_non_hrmn {
                continue;
            }

            let category = infer_category(upstream_id);
            let provider = infer_provider(upstream_id);
            let display_name = format!("{} (Varynth)", format_display_name(upstream_id));
            let (inp_p, out_p, req_cost, rpm, rpd) = infer_pricing_and_limits(upstream_id, is_free);
            let min_plan = if is_free { "free" } else { "pro" };
            let tier = if is_free { "public" } else { "account" };

            sqlx::query(
                r#"
                INSERT INTO models (
                    id, display_name, tier, min_plan, category,
                    upstream_kind, upstream_model, upstream_base_url,
                    enabled, input_price_per_m, output_price_per_m,
                    request_cost, rpm_limit, rpd_limit, sort_order
                ) VALUES (
                    $1, $2, $3, $4, $5,
                    'hrmn', $6, $7,
                    true, $8, $9,
                    $10, $11, $12, 120
                )
                ON CONFLICT (id) DO UPDATE SET
                    enabled = true,
                    upstream_base_url = EXCLUDED.upstream_base_url
                "#,
            )
            .bind(&varynth_id)
            .bind(&display_name)
            .bind(tier)
            .bind(min_plan)
            .bind(category)
            .bind(upstream_id)
            .bind(base_url)
            .bind(inp_p)
            .bind(out_p)
            .bind(req_cost)
            .bind(rpm)
            .bind(rpd)
            .execute(&state.pool)
            .await?;

            sqlx::query(
                r#"
                INSERT INTO provider_models (provider_id, model_id, display_name)
                VALUES ($1, $2, $3)
                ON CONFLICT (provider_id, model_id) DO UPDATE SET
                    display_name = EXCLUDED.display_name
                "#,
            )
            .bind(provider)
            .bind(upstream_id)
            .bind(&display_name)
            .execute(&state.pool)
            .await?;

            report.added += 1;
        }
    }

    info!(
        "Upstream model sync completed: {} total upstream, {} activated, {} deactivated, {} newly added",
        report.total_upstream, report.activated, report.deactivated, report.added
    );

    Ok(report)
}

pub async fn run_periodic_sync(state: AppState, interval_duration: Duration) {
    info!(
        "Starting periodic upstream model synchronizer (interval: {:?})",
        interval_duration
    );

    // Initial sync on startup
    if let Err(e) = sync_models_with_upstream(&state).await {
        error!("Initial upstream model sync failed: {}", e);
    }

    let mut interval = tokio::time::interval(interval_duration);
    interval.tick().await; // skip initial immediate tick

    loop {
        interval.tick().await;
        info!("Running scheduled 5-minute upstream model sync...");
        if let Err(e) = sync_models_with_upstream(&state).await {
            error!("Scheduled upstream model sync failed: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inference_helpers() {
        assert_eq!(infer_provider("qwen3.6-27b"), "qwen");
        assert_eq!(infer_provider("meta-llama-3_3-70b-instruct"), "meta");
        assert_eq!(infer_provider("mistral-small-3.2-24b-instruct-2506"), "mistralai");
        assert_eq!(infer_provider("granite-4.0-h-micro"), "ibm");
        assert_eq!(infer_provider("qwen2.5-vl-72b-instruct"), "qwen");

        assert_eq!(infer_category("qwen2.5-coder-32b-instruct"), "coding");
        assert_eq!(infer_category("qwen2.5-vl-72b-instruct"), "vision");
        assert_eq!(infer_category("qwen3guard-gen-0.6b"), "fast");

        let (inp, out, cost, rpm, rpd) = infer_pricing_and_limits("gpt-oss-120b", true);
        assert_eq!(inp, 0.0);
        assert_eq!(out, 0.0);
        assert_eq!(cost, 1);
        assert_eq!(rpm, 20);
        assert_eq!(rpd, 100);

        let (inp, out, cost, rpm, rpd) = infer_pricing_and_limits("qwen3.5-397b-a17b", false);
        assert_eq!(inp, 12.0);
        assert_eq!(out, 60.0);
        assert_eq!(cost, 5);
        assert_eq!(rpm, 15);
        assert_eq!(rpd, 200);
    }

    #[test]
    fn test_format_display_name() {
        assert_eq!(
            format_display_name("qwen3.6-27b"),
            "Qwen3.6 27b"
        );
        assert_eq!(
            format_display_name("mistral-nemo-instruct-2407"),
            "Mistral Nemo Instruct 2407"
        );
    }
}
