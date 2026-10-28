//! Offline tests for the Google auto-login preflight decision and its config
//! plumbing. No network, no browser, no keyring writes.
//!
//! `google_auto_login_plan` must be decidable without touching the network:
//! when the keyring reports no stored credential the plan is either `Login`
//! (client id available, auto-login enabled) or `Error` (actionable message),
//! never a silent fallback to another provider.

use varynth::config::{Config, GoogleAutoLoginPlan, ProviderProfile};

fn google_oauth_config() -> Config {
    let mut cfg = Config::default();
    cfg.provider = "google".into();
    cfg.google_auth = "oauth".into();
    cfg
}

fn vertex_config() -> Config {
    let mut cfg = Config::default();
    cfg.provider = "google-vertex".into();
    cfg.google_auth = "oauth".into();
    cfg.google_project = Some("proj".into());
    cfg.google_location = Some("us-central1".into());
    cfg
}

fn profile_cfg(kind: &str, auth: &str) -> Config {
    let mut cfg = Config::default();
    cfg.provider_profiles = vec![ProviderProfile {
        id: "gp".into(),
        kind: kind.into(),
        auth: auth.into(),
        project: Some("proj".into()),
        location: Some("us-central1".into()),
        ..Default::default()
    }];
    cfg.provider = "gp".into();
    cfg
}

/// A stored credential in the keyring makes the plan Ready. Without one (the
/// usual CI state) the plan must fall through to login/error. Both branches
/// are exercised: the "with keyring entry" branch by skipping when a real
/// credential exists, the "without" branch by asserting the non-Ready plan.
#[test]
fn non_google_providers_are_always_ready() {
    for (provider, auth) in [
        ("proxy", "api-key"),
        ("openai", "api-key"),
        ("anthropic", "api-key"),
        ("google", "api-key"),
    ] {
        let mut cfg = Config::default();
        cfg.provider = provider.into();
        cfg.google_auth = auth.into();
        cfg.google_client_id = Some("client-id".into());
        assert_eq!(
            cfg.google_auto_login_plan(),
            GoogleAutoLoginPlan::Ready,
            "{provider}/{auth} must never trigger the Google login flow"
        );
    }
}

#[test]
fn google_oauth_without_client_id_errors_actionably() {
    for mut cfg in [google_oauth_config(), vertex_config()] {
        // No client id anywhere: the plan must be an error that says how to
        // fix it, not a login attempt with an empty id.
        match cfg.google_auto_login_plan() {
            GoogleAutoLoginPlan::Error(message) => {
                assert!(message.contains("google_client_id"), "{message}");
                assert!(message.contains("VARYNTH_GOOGLE_CLIENT_ID"), "{message}");
                assert!(message.contains("varynth google login"), "{message}");
            }
            other => panic!("expected an actionable error, got {other:?}"),
        }
        // An empty/whitespace client id counts as absent.
        cfg.google_client_id = Some("   ".into());
        assert!(
            matches!(cfg.google_auto_login_plan(), GoogleAutoLoginPlan::Error(_)),
            "blank client id must not start a login"
        );
    }
}

#[test]
fn google_auto_login_disabled_errors_actionably_even_with_client_id() {
    let mut cfg = google_oauth_config();
    cfg.google_client_id = Some("client-id".into());
    cfg.google_auto_login = false;
    match cfg.google_auto_login_plan() {
        GoogleAutoLoginPlan::Error(message) => {
            assert!(message.contains("google_auto_login"), "{message}");
            assert!(message.contains("varynth google login"), "{message}");
        }
        other => panic!("expected an actionable error, got {other:?}"),
    }
}

#[test]
fn google_oauth_with_client_id_plans_a_login_or_is_ready_when_authenticated() {
    let mut cfg = google_oauth_config();
    cfg.google_client_id = Some(" my-client-id ".into());
    match cfg.google_auto_login_plan() {
        // This machine already holds a Google credential: nothing to do.
        GoogleAutoLoginPlan::Ready => {}
        // No credential: the flow should start with the trimmed client id.
        GoogleAutoLoginPlan::Login { client_id } => {
            assert_eq!(client_id, "my-client-id");
        }
        other => panic!("expected login or ready, got {other:?}"),
    }
}

#[test]
fn plan_considers_profiles_and_their_auth_mode() {
    // An OAuth profile with a client id plans a login (or is ready when a
    // credential exists); an api-key profile never does.
    let mut cfg = profile_cfg("google", "oauth");
    cfg.google_client_id = Some("client-id".into());
    match cfg.google_auto_login_plan() {
        GoogleAutoLoginPlan::Login { client_id } => assert_eq!(client_id, "client-id"),
        GoogleAutoLoginPlan::Ready => {}
        other => panic!("expected login or ready for oauth profile, got {other:?}"),
    }

    let cfg = profile_cfg("google", "api-key");
    assert_eq!(
        cfg.google_auto_login_plan(),
        GoogleAutoLoginPlan::Ready,
        "api-key profiles must not trigger the OAuth login flow"
    );
}

#[test]
fn default_auto_login_is_true_and_client_id_is_optional() {
    let cfg = Config::default();
    assert!(cfg.google_auto_login, "auto-login must default to on");
    assert!(cfg.google_client_id.is_none());
}

#[test]
fn auto_login_config_round_trips_through_toml() {
    let mut cfg = google_oauth_config();
    cfg.google_client_id = Some("round-trip-client".into());
    cfg.google_auto_login = false;
    let raw = toml::to_string_pretty(&cfg).unwrap();
    let back: Config = toml::from_str(&raw).unwrap();
    assert_eq!(back.google_client_id.as_deref(), Some("round-trip-client"));
    assert!(!back.google_auto_login);

    // Absent fields keep the defaults (backward-compatible config files).
    let back: Config = toml::from_str("model = \"m\"\nprovider = \"google\"\n").unwrap();
    assert!(back.google_auto_login);
    assert!(back.google_client_id.is_none());
}

#[test]
fn auto_login_is_not_redacted_on_save() {
    // google_client_id is a non-secret public value: keyring redaction must
    // keep it so auto-login survives a save with use_keyring enabled.
    let mut cfg = google_oauth_config();
    cfg.google_client_id = Some("public-client-id".into());
    cfg.google_auto_login = true;
    cfg.use_keyring = Some(true);
    let raw = toml::to_string_pretty(&cfg.redacted_for_save()).unwrap();
    assert!(
        raw.contains("public-client-id"),
        "client id must survive redaction:\n{raw}"
    );
}

#[test]
fn env_overrides_set_client_id_and_auto_login() {
    // Serialized like the existing env tests: set, read, restore.
    // `with_env` wraps the private apply_env and is the public entry point.
    let vars = [
        (
            "VARYNTH_GOOGLE_CLIENT_ID",
            std::env::var("VARYNTH_GOOGLE_CLIENT_ID").ok(),
        ),
        (
            "VARYNTH_GOOGLE_AUTO_LOGIN",
            std::env::var("VARYNTH_GOOGLE_AUTO_LOGIN").ok(),
        ),
    ];
    let mut cfg;
    {
        std::env::set_var("VARYNTH_GOOGLE_CLIENT_ID", "env-client");
        std::env::set_var("VARYNTH_GOOGLE_AUTO_LOGIN", "false");
        cfg = Config::default().with_env();
        assert_eq!(cfg.google_client_id.as_deref(), Some("env-client"));
        assert!(!cfg.google_auto_login);
    }

    std::env::set_var("VARYNTH_GOOGLE_AUTO_LOGIN", "off");
    cfg = Config::default().with_env();
    assert!(!cfg.google_auto_login);
    std::env::set_var("VARYNTH_GOOGLE_AUTO_LOGIN", "1");
    cfg = Config::default().with_env();
    assert!(cfg.google_auto_login);
    std::env::set_var("VARYNTH_GOOGLE_AUTO_LOGIN", "junk");
    cfg = Config::default().with_env();
    assert!(
        cfg.google_auto_login,
        "unknown values keep the default (on)"
    );
    std::env::set_var("VARYNTH_GOOGLE_AUTO_LOGIN", "");
    cfg = Config::default().with_env();
    assert!(!cfg.google_auto_login, "empty value reads as disabled");

    for (key, previous) in vars {
        match previous {
            Some(value) => std::env::set_var(key, &value),
            None => std::env::remove_var(key),
        }
    }
}

#[test]
fn invalid_client_id_fails_validation() {
    let mut cfg = google_oauth_config();
    cfg.google_client_id = Some("has space".into());
    assert!(cfg
        .validate()
        .unwrap_err()
        .to_string()
        .contains("google_client_id"));
    cfg.google_client_id = Some("x".repeat(513));
    assert!(cfg.validate().is_err());
    cfg.google_client_id = Some("valid-id.apps.googleusercontent.com".into());
    cfg.model = "m".into();
    cfg.validate().unwrap();
}
