mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{boot_postgres, serve};
use serde_json::json;

fn set_cookie(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn login_hardening_cookie_flags_and_lockout() {
    let (database_url, _server) = boot_postgres().await;
    std::env::set_var("DATABASE_URL", &database_url);
    std::env::set_var("JWT_SECRET", "test-secret-test-secret-test-secret");
    std::env::set_var("WEB_ORIGIN", "http://127.0.0.1:3000");
    std::env::set_var("VARYNTH_API_PUBLIC_URL", "https://api.varynth.test");
    std::env::set_var("BIND_ADDR", "127.0.0.1:0");
    std::env::set_var("VARYNTH_STUB_UPSTREAM", "1");
    std::env::remove_var("VARYNTH_TRUSTED_PROXIES");

    let config = varynth_api::Config::from_env().expect("config");
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
    let login_url = format!("{base}/v1/auth/login");
    let register_url = format!("{base}/v1/auth/register");

    // Password policy: 11 characters is rejected with a clear message.
    let short = client
        .post(&register_url)
        .json(&json!({"email":"short@varynth.dev","password":"password123"}))
        .send()
        .await
        .expect("short register");
    assert_eq!(short.status(), 400);
    let short_body = short.text().await.expect("short body");
    assert!(short_body.contains("12 characters"), "{short_body}");

    let password = "password1234";
    for email in ["lock@varynth.dev", "control@varynth.dev"] {
        let response = client
            .post(&register_url)
            .json(&json!({"email": email, "password": password}))
            .send()
            .await
            .expect("register");
        assert!(response.status().is_success(), "{email}: {}", response.status());
    }

    // Cookie flags: the HTTPS public URL means __Host- + Secure on the session
    // cookie, and HSTS on the response.
    let login = client
        .post(&login_url)
        .json(&json!({"email":"lock@varynth.dev","password":password}))
        .send()
        .await
        .expect("login");
    assert!(login.status().is_success(), "{}", login.status());
    let cookie = set_cookie(&login);
    assert!(cookie.starts_with("__Host-varynth_session="), "{cookie}");
    assert!(cookie.contains("; Secure"), "{cookie}");
    assert!(cookie.contains("; HttpOnly"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert_eq!(
        login
            .headers()
            .get("strict-transport-security")
            .and_then(|value| value.to_str().ok()),
        Some("max-age=63072000; includeSubDomains")
    );
    let session = login
        .json::<serde_json::Value>()
        .await
        .expect("login body")["session_token"]
        .as_str()
        .expect("session token")
        .to_string();

    // Logout is symmetric with login.
    let logout = client
        .post(format!("{base}/v1/auth/logout"))
        .bearer_auth(&session)
        .send()
        .await
        .expect("logout");
    assert_eq!(logout.status(), 204);
    let cleared = set_cookie(&logout);
    assert!(cleared.starts_with("__Host-varynth_session=;"), "{cleared}");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    assert!(cleared.contains("; Secure"), "{cleared}");

    // Unknown accounts take the constant-work dummy Argon2 path and still get 401.
    let unknown = client
        .post(&login_url)
        .json(&json!({"email":"ghost@varynth.dev","password":password}))
        .send()
        .await
        .expect("unknown login");
    assert_eq!(unknown.status(), 401);

    // Per-IP+account lockout: repeated failures lock the account key.
    for attempt in 1..=5 {
        let response = client
            .post(&login_url)
            .json(&json!({"email":"lock@varynth.dev","password":"wrong-password-99"}))
            .send()
            .await
            .expect("failed login");
        let status = response.status().as_u16();
        if attempt < 5 {
            assert_eq!(status, 401, "attempt {attempt}");
        } else {
            assert_eq!(status, 429, "fifth failure should lock the account");
            let body = response.text().await.expect("lock body");
            assert!(body.contains("retry in"), "{body}");
        }
    }

    // Correct credentials are refused while the lockout is active.
    let locked = client
        .post(&login_url)
        .json(&json!({"email":"lock@varynth.dev","password":password}))
        .send()
        .await
        .expect("locked login");
    assert_eq!(locked.status(), 429);
    let locked_body = locked.text().await.expect("locked body");
    assert!(locked_body.contains("retry in"), "{locked_body}");

    // The lockout is per account: the same IP can still try another account.
    let control = client
        .post(&login_url)
        .json(&json!({"email":"control@varynth.dev","password":"wrong-password-99"}))
        .send()
        .await
        .expect("control login");
    assert_eq!(control.status(), 401);
}
