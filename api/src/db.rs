use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::config::Config;

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
}

pub async fn prepare(pool: &PgPool, cfg: &Config) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations").run(pool).await?;
    if !cfg.stub_upstream && cfg.worker_gateway_key.is_some() && !cfg.worker_gateways.is_empty() {
        // Hide placeholder public-1..5 from /v1/models/public when real worker gateways are active,
        // while keeping their upstream mappings configured for legacy callers.
        sqlx::query(
            "UPDATE models SET enabled = false, upstream_kind = 'worker_gateway'
             WHERE tier = 'public' AND id LIKE 'varynth/public-%'",
        )
        .execute(pool)
        .await?;
        for (id, upstream) in [
            ("varynth/public-1", "gpt-oss-120b"),
            ("varynth/public-2", "qwen2.5-coder-32b-instruct"),
            ("varynth/public-3", "deepseek-r1-distill-qwen-32b"),
            ("varynth/public-4", "qwq-32b"),
            ("varynth/public-5", "qwen3.8-27b"),
        ] {
            sqlx::query(
                "UPDATE models SET upstream_model = $1 WHERE id = $2",
            )
            .bind(upstream)
            .bind(id)
            .execute(pool)
            .await?;
        }
        if cfg.openai_key.is_none() && cfg.hrmn_api_key.is_none() {
            sqlx::query("DELETE FROM provider_models WHERE model_id = 'gpt-4o-mini'")
                .execute(pool)
                .await?;
        }
    } else if cfg.enable_public_placeholders || cfg.worker_gateway_key.is_some() {
        sqlx::query(
            "UPDATE models SET enabled = true WHERE tier = 'public' AND id LIKE 'varynth/public-%'",
        )
        .execute(pool)
        .await?;
    }
    Ok(())
}
