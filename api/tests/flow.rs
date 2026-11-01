mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use common::{boot_postgres, clean, serve, text_ok};

#[tokio::test]
async fn public_login_catalog_and_chat_hide_upstream() {
    let (database_url, _server) = boot_postgres().await;
    std::env::set_var("DATABASE_URL", &database_url);
    std::env::set_var("JWT_SECRET", "test-secret-test-secret-test-secret");
    std::env::set_var("WEB_ORIGIN", "http://127.0.0.1:3000");
    std::env::set_var("VARYNTH_API_PUBLIC_URL", "http://127.0.0.1:8080");
    std::env::set_var("BIND_ADDR", "127.0.0.1:0");
    std::env::set_var("VARYNTH_STUB_UPSTREAM", "1");
    std::env::set_var("VARYNTH_ENABLE_PUBLIC_PLACEHOLDERS", "1");
    std::env::set_var("VARYNTH_BILLING_STUB", "1");

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
    let public_body = text_ok(&client, &format!("{base}/v1/models/public")).await;
    assert!(public_body.contains("varynth/public-1"));
    assert!(public_body.contains("varynth/public-5"));
    assert!(clean(&public_body));

    let plans_body = text_ok(&client, &format!("{base}/v1/plans")).await;
    assert!(plans_body.contains("\"id\":\"free\""), "{plans_body}");
    assert!(plans_body.contains("\"id\":\"pro\""), "{plans_body}");
    assert!(plans_body.contains("\"chat_per_day\":100"), "{plans_body}");
    assert!(plans_body.contains("\"chat_per_day\":2000"), "{plans_body}");
    assert!(plans_body.contains("\"billing_configured\":true"), "{plans_body}");

    // Skills catalog unauthenticated: free skills have content, pro skills are locked (content is null)
    let anon_skills: serde_json::Value = client
        .get(format!("{base}/v1/catalog/skills"))
        .send()
        .await
        .expect("anon skills")
        .json()
        .await
        .expect("anon skills json");
    assert_eq!(anon_skills["tier"], "free");
    let anon_list = anon_skills["skills"].as_array().expect("skills list");
    assert!(anon_list.len() >= 12, "expected at least 12 skills");
    let cu = anon_list.iter().find(|s| s["id"] == "computer-use").expect("computer-use");
    assert_eq!(cu["tier"], "free");
    assert_eq!(cu["unlocked"], true);
    assert!(cu["content"].is_string());
    let mfp = anon_list.iter().find(|s| s["id"] == "motion-fream-pro").expect("motion-fream-pro");
    assert_eq!(mfp["tier"], "pro");
    assert_eq!(mfp["unlocked"], false);
    assert!(mfp["content"].is_null());

    // Verify /v1/models returns both free and pro models with pricing and limits
    let models_json: serde_json::Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("models json");
    let models_list = models_json["data"].as_array().expect("models data");
    assert!(models_list.len() >= 166, "expected >=166 models, got {}", models_list.len());
    let opus = models_list
        .iter()
        .find(|m| m["id"] == "varynth/claude-opus-4-8")
        .expect("claude-opus-4-8");
    assert_eq!(opus["min_plan"], "pro");
    assert_eq!(opus["pricing"]["request_cost"], 5);
    assert_eq!(opus["pricing"]["is_free"], false);
    assert_eq!(opus["limits"]["rpm"], 15);
    let gpt_oss = models_list
        .iter()
        .find(|m| m["id"] == "varynth/gpt-oss-120b")
        .expect("gpt-oss-120b");
    assert_eq!(gpt_oss["min_plan"], "free");
    assert_eq!(gpt_oss["pricing"]["is_free"], true);
    assert_eq!(gpt_oss["pricing"]["request_cost"], 1);

    // Anonymous caller requesting a Pro model gets 401
    let anon_pro_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"varynth/claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("anon pro chat");
    assert_eq!(anon_pro_chat.status(), 401);

    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"public-1","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("chat");
    assert!(chat.status().is_success());
    let chat_body = chat.text().await.expect("chat body");
    assert!(chat_body.contains("stub: varynth/public-1"));
    assert!(clean(&chat_body));

    let hidden = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("hidden");
    assert_eq!(hidden.status(), 401);

    let catalog = client
        .get(format!("{base}/v1/catalog"))
        .send()
        .await
        .expect("catalog");
    assert_eq!(catalog.status(), 401);

    let register = client
        .post(format!("{base}/v1/auth/register"))
        .json(&json!({"email":"dev@varynth.dev","password":"password1234"}))
        .send()
        .await
        .expect("register");
    assert!(register.status().is_success(), "{}", register.status());

    let login: serde_json::Value = client
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"email":"dev@varynth.dev","password":"password1234"}))
        .send()
        .await
        .expect("login")
        .json()
        .await
        .expect("login json");
    let session = login["session_token"].as_str().expect("session");

    let usage_anon = client
        .get(format!("{base}/v1/me/usage"))
        .send()
        .await
        .expect("usage anon");
    assert_eq!(usage_anon.status(), 401);

    let usage: serde_json::Value = client
        .get(format!("{base}/v1/me/usage"))
        .bearer_auth(session)
        .send()
        .await
        .expect("usage")
        .json()
        .await
        .expect("usage json");
    assert_eq!(usage["plan"], "free");
    assert_eq!(usage["limits"]["chat_per_day"], 100);
    assert_eq!(usage["limits"]["search_per_day"], 50);
    let chat_before = usage["today"]["chat"].as_i64().expect("chat counter");

    let web_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(session)
        .json(&json!({"model":"public-1","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("web chat");
    assert!(web_chat.status().is_success());

    let usage: serde_json::Value = client
        .get(format!("{base}/v1/me/usage"))
        .bearer_auth(session)
        .send()
        .await
        .expect("usage after chat")
        .json()
        .await
        .expect("usage after chat json");
    assert_eq!(
        usage["today"]["chat"].as_i64().expect("chat after"),
        chat_before + 1
    );
    assert_eq!(usage["remaining"]["chat"], 100 - chat_before - 1);

    // Free signed-in caller requesting a Pro model gets 402 Payment Required
    let free_pro_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(session)
        .json(&json!({"model":"varynth/claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("free pro chat");
    assert_eq!(free_pro_chat.status(), 402);

    let flip: serde_json::Value = client
        .post(format!("{base}/v1/me/subscription"))
        .bearer_auth(session)
        .json(&json!({"plan":"pro"}))
        .send()
        .await
        .expect("subscribe")
        .json()
        .await
        .expect("subscribe json");
    assert_eq!(flip["plan"], "pro");
    assert_eq!(flip["previous_plan"], "free");
    assert_eq!(flip["changed"], true);

    let usage_pro: serde_json::Value = client
        .get(format!("{base}/v1/me/usage"))
        .bearer_auth(session)
        .send()
        .await
        .expect("usage pro")
        .json()
        .await
        .expect("usage pro json");
    assert_eq!(usage_pro["plan"], "pro");
    assert_eq!(usage_pro["limits"]["chat_per_day"], 2000);
    assert_eq!(usage_pro["limits"]["search_per_day"], 500);
    let pro_chat_before = usage_pro["today"]["chat"].as_i64().expect("pro chat before");

    // Pro signed-in caller requesting a Pro flagship model (request_cost = 5) succeeds and increments usage by 5
    let pro_opus_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(session)
        .json(&json!({"model":"varynth/claude-opus-4-8","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("pro opus chat");
    assert!(pro_opus_chat.status().is_success());

    let usage_after_opus: serde_json::Value = client
        .get(format!("{base}/v1/me/usage"))
        .bearer_auth(session)
        .send()
        .await
        .expect("usage after opus")
        .json()
        .await
        .expect("usage after opus json");
    assert_eq!(
        usage_after_opus["today"]["chat"].as_i64().expect("pro chat after"),
        pro_chat_before + 5
    );

    // Skills catalog as Pro: Pro skills now have content unlocked
    let pro_skills: serde_json::Value = client
        .get(format!("{base}/v1/catalog/skills"))
        .bearer_auth(session)
        .send()
        .await
        .expect("pro skills")
        .json()
        .await
        .expect("pro skills json");
    assert_eq!(pro_skills["tier"], "pro");
    let pro_list = pro_skills["skills"].as_array().expect("pro skills list");
    let mfp_pro = pro_list.iter().find(|s| s["id"] == "motion-fream-pro").expect("motion-fream-pro");
    assert_eq!(mfp_pro["tier"], "pro");
    assert_eq!(mfp_pro["unlocked"], true);
    assert!(mfp_pro["content"].is_string(), "pro content should be unlocked");

    // Flux Image Generation as Pro
    let img_gen: serde_json::Value = client
        .post(format!("{base}/v1/images/generations"))
        .bearer_auth(session)
        .json(&json!({
            "prompt": "futuristic 3d isometric dashboard card",
            "model": "flux-1-schnell",
            "size": "1024x1024"
        }))
        .send()
        .await
        .expect("image gen")
        .json()
        .await
        .expect("image gen json");
    assert!(img_gen["data"].as_array().is_some());
    let img_data = &img_gen["data"][0];
    assert!(img_data["b64_json"].is_string() || img_data["url"].is_string());

    // Bad prompt validation
    let bad_img = client
        .post(format!("{base}/v1/images/generations"))
        .bearer_auth(session)
        .json(&json!({"prompt": "   "}))
        .send()
        .await
        .expect("bad img");
    assert_eq!(bad_img.status(), 400);

    let idempotent: serde_json::Value = client
        .post(format!("{base}/v1/me/subscription"))
        .bearer_auth(session)
        .json(&json!({"plan":"pro"}))
        .send()
        .await
        .expect("same plan")
        .json()
        .await
        .expect("same plan json");
    assert_eq!(idempotent["changed"], false);

    let bad_plan = client
        .post(format!("{base}/v1/me/subscription"))
        .bearer_auth(session)
        .json(&json!({"plan":"team"}))
        .send()
        .await
        .expect("bad plan");
    assert_eq!(bad_plan.status(), 400);

    let verifier = "verifier-value-verifier-value-verifier-value";
    let challenge = varynth_api::crypto::pkce_s256(verifier);
    let approved: serde_json::Value = client
        .post(format!("{base}/v1/oauth/authorize"))
        .bearer_auth(session)
        .json(&json!({
            "response_type": "code",
            "client_id": "varynth-cli",
            "redirect_uri": "http://127.0.0.1:4312/oauth/callback",
            "state": "state123",
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "decision": "approve"
        }))
        .send()
        .await
        .expect("approve")
        .json()
        .await
        .expect("approve json");
    let redirect = approved["redirect_to"].as_str().expect("redirect");
    assert!(redirect.starts_with("http://127.0.0.1:4312/oauth/callback?"));
    assert!(clean(redirect));
    let code = reqwest::Url::parse(redirect)
        .expect("redirect url")
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .expect("code");

    let token: serde_json::Value = client
        .post(format!("{base}/v1/oauth/token"))
        .json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": "http://127.0.0.1:4312/oauth/callback",
            "client_id": "varynth-cli",
            "code_verifier": verifier
        }))
        .send()
        .await
        .expect("token")
        .json()
        .await
        .expect("token json");
    let access = token["access_token"].as_str().expect("access");
    assert!(token["refresh_token"].as_str().is_some());

    let catalog_body = client
        .get(format!("{base}/v1/catalog"))
        .bearer_auth(access)
        .send()
        .await
        .expect("catalog authed")
        .text()
        .await
        .expect("catalog body");
    assert!(catalog_body.contains("\"id\":\"openai\""));
    assert!(catalog_body.contains("gpt-4o-mini"));
    assert!(clean(&catalog_body));

    let account_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(access)
        .json(&json!({"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .expect("account chat")
        .text()
        .await
        .expect("account body");
    assert!(account_chat.contains("stub: openai/gpt-4o-mini"));
    assert!(clean(&account_chat));

    let anon_search = client
        .post(format!("{base}/v1/search"))
        .json(&json!({"query": "varynth", "provider": "exa"}))
        .send()
        .await
        .expect("anon search");
    assert_eq!(anon_search.status(), 401);

    let authed_search = client
        .post(format!("{base}/v1/search"))
        .bearer_auth(access)
        .json(&json!({"query": "rust programming language", "provider": "exa"}))
        .send()
        .await
        .expect("authed search");
    assert_ne!(authed_search.status(), 401);
    let search_body = authed_search.text().await.expect("search body");
    assert!(!search_body.contains("sk-zuzia"));
    assert!(!search_body.to_ascii_lowercase().contains("fc-"));

    // Billing off: the same account cannot change plan, and nothing is charged.
    let mut off_config = varynth_api::Config::from_env().expect("config off");
    off_config.billing_stub = false;
    let off_pool = varynth_api::db::connect(&off_config.database_url)
        .await
        .expect("off pool");
    let off_app = varynth_api::router(varynth_api::AppState::new(
        off_pool,
        Arc::new(off_config),
        varynth_api::http_client().expect("http"),
    ));
    let off_addr = serve(off_app).await;
    let blocked = client
        .post(format!("http://{off_addr}/v1/me/subscription"))
        .bearer_auth(session)
        .json(&json!({"plan":"free"}))
        .send()
        .await
        .expect("blocked subscribe");
    assert_eq!(blocked.status(), 501);
    let blocked_body = blocked.text().await.expect("blocked body");
    assert!(blocked_body.contains("billing_unavailable"), "{blocked_body}");
}
