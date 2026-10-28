mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{boot_postgres, serve};
use serde_json::json;

#[tokio::test]
async fn rotating_forwarded_for_does_not_reset_anonymous_quota() {
    let (database_url, _server) = boot_postgres().await;
    std::env::set_var("DATABASE_URL", &database_url);
    std::env::set_var("JWT_SECRET", "test-secret-test-secret-test-secret");
    std::env::set_var("WEB_ORIGIN", "http://127.0.0.1:3000");
    std::env::set_var("VARYNTH_API_PUBLIC_URL", "http://127.0.0.1:8080");
    std::env::set_var("BIND_ADDR", "127.0.0.1:0");
    std::env::set_var("VARYNTH_STUB_UPSTREAM", "1");
    std::env::set_var("VARYNTH_ENABLE_PUBLIC_PLACEHOLDERS", "1");
    std::env::remove_var("VARYNTH_TRUSTED_PROXIES");

    let config = varynth_api::Config::from_env().expect("config");
    assert!(
        config.trusted_proxies.is_empty(),
        "test requires no trusted proxies configured"
    );
    let pool = varynth_api::db::connect(&config.database_url)
        .await
        .expect("pool");
    varynth_api::db::prepare(&pool, &config)
        .await
        .expect("migrate");
    let app = varynth_api::router(varynth_api::AppState::new(
        pool,
        Arc::new(config),
        varynth_api::http_client().expect("http"),
    ));
    let addr = serve(app).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("client");
    let base = format!("http://{addr}");
    let payload = json!({"model":"public-1","messages":[{"role":"user","content":"hi"}]});

    // Anonymous limit is 5 chat requests/minute. Rotating the client-supplied
    // header used to mint a fresh bucket per request; it must not any more.
    for attempt in 0..5 {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header("x-forwarded-for", format!("198.51.100.{}", attempt + 1))
            .json(&payload)
            .send()
            .await
            .expect("chat");
        assert!(
            response.status().is_success(),
            "attempt {attempt} should be inside quota, got {}",
            response.status()
        );
    }

    let blocked = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-forwarded-for", "203.0.113.250")
        .json(&payload)
        .send()
        .await
        .expect("blocked chat");
    assert_eq!(blocked.status(), 429, "rotating X-Forwarded-For reset the quota");
    let body = blocked.text().await.expect("body");
    assert!(body.contains("chat-minute limit reached"), "{body}");
}
