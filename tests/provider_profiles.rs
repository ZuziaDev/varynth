//! Offline tests for provider profiles, qualified model selection, and the
//! Google OAuth helpers. No network, no keyring, no browser.

use varynth::config::{valid_profile_id, Config, ProviderProfile, PROVIDER_KINDS};

fn profile(id: &str, kind: &str) -> ProviderProfile {
    ProviderProfile {
        id: id.into(),
        kind: kind.into(),
        ..Default::default()
    }
}

fn config_with_profiles() -> Config {
    let mut cfg = Config::default();
    cfg.provider_profiles = vec![
        ProviderProfile {
            id: "work".into(),
            kind: "openai".into(),
            name: Some("Work endpoint".into()),
            models: vec!["model-a".into()],
            ..Default::default()
        },
        ProviderProfile {
            id: "second".into(),
            kind: "openai".into(),
            models: vec!["model-b".into()],
            enabled: false,
            ..Default::default()
        },
    ];
    cfg
}

#[test]
fn valid_profile_id_rules() {
    for id in ["a", "Antigravity-1", "a_b", "x".repeat(64).as_str()] {
        assert!(valid_profile_id(id), "{id} should be valid");
    }
    for id in ["", "has space", "has/slash", "ä", "x".repeat(65).as_str()] {
        assert!(!valid_profile_id(id), "{id} should be invalid");
    }
}

#[test]
fn validate_rejects_bad_profiles() {
    let mut cfg = config_with_profiles();
    cfg.provider = "proxy".into();
    cfg.validate().unwrap();

    let mut dup = cfg.clone();
    dup.provider_profiles.push(profile("work", "openai"));
    assert!(dup
        .validate()
        .unwrap_err()
        .to_string()
        .contains("duplicate"));

    let mut bad_kind = cfg.clone();
    bad_kind.provider_profiles[0].kind = "antigravity".into();
    assert!(bad_kind
        .validate()
        .unwrap_err()
        .to_string()
        .contains("profile kind"));

    let mut bad_auth = cfg.clone();
    bad_auth.provider_profiles[0].kind = "openai".into();
    bad_auth.provider_profiles[0].auth = "oauth".into();
    assert!(bad_auth
        .validate()
        .unwrap_err()
        .to_string()
        .contains("OAuth is supported only"));

    let mut bad_id = cfg.clone();
    bad_id.provider_profiles[0].id = "not ok".into();
    assert!(bad_id
        .validate()
        .unwrap_err()
        .to_string()
        .contains("profile id"));

    let mut bad_model = cfg.clone();
    bad_model.provider_profiles[0].models = vec!["   ".into()];
    assert!(bad_model
        .validate()
        .unwrap_err()
        .to_string()
        .contains("model id"));

    let mut bad_env = cfg.clone();
    bad_env.provider_profiles[0].api_key_env = Some("no-dashes".into());
    assert!(bad_env
        .validate()
        .unwrap_err()
        .to_string()
        .contains("api_key_env"));
}

#[test]
fn validate_rejects_bad_google_auth_value() {
    let mut cfg = Config::default();
    cfg.google_auth = "token".into();
    assert!(cfg
        .validate()
        .unwrap_err()
        .to_string()
        .contains("google_auth"));
}

#[test]
fn provider_accepts_enabled_profile_id_and_rejects_unknown() {
    let mut cfg = config_with_profiles();
    cfg.provider = "work".into();
    cfg.validate().unwrap();

    cfg.provider = "missing".into();
    let err = cfg.validate().unwrap_err().to_string();
    assert!(err.contains("invalid provider"), "{err}");
    assert!(err.contains("missing"), "{err}");
}

#[test]
fn disabled_profile_cannot_be_selected_or_routed_by_model() {
    let mut cfg = config_with_profiles();
    cfg.provider = "second".into();
    assert!(
        cfg.validate().is_err(),
        "disabled provider id must fail validate"
    );

    let mut cfg = config_with_profiles();
    cfg.provider = "proxy".into();
    cfg.model = "second/model-b".into();
    assert!(
        cfg.selected_profile_id().is_none(),
        "a model prefix alone must not bypass the enabled check"
    );

    let mut select = config_with_profiles();
    select.provider = "proxy".into();
    assert!(select.select_model("second/model-b").is_err());
}

#[test]
fn selected_profile_id_resolves_provider_and_model_prefix() {
    let mut cfg = config_with_profiles();
    cfg.provider = "work".into();
    cfg.model = "model-a".into();
    assert_eq!(cfg.selected_profile_id(), Some("work"));

    cfg.provider = "proxy".into();
    cfg.model = "work/model-a".into();
    assert_eq!(cfg.selected_profile_id(), Some("work"));

    cfg.model = "plain-model".into();
    assert!(cfg.selected_profile_id().is_none());
}

#[test]
fn select_model_updates_provider_for_prefix_and_keeps_qualified_id() {
    let mut cfg = config_with_profiles();
    cfg.provider = "proxy".into();
    cfg.select_model("work/model-a").unwrap();
    assert_eq!(cfg.provider, "work");
    assert_eq!(cfg.model, "work/model-a");
    assert_eq!(cfg.selected_profile_id(), Some("work"));

    // A plain id on a plain provider keeps prior behavior.
    let mut plain = Config::default();
    plain.select_model("grok-4.7").unwrap();
    assert_eq!(plain.model, "grok-4.7");
    assert_eq!(plain.provider, "proxy");

    // Unknown prefix ids are kept verbatim: no silent rewrite, no error.
    let mut unknown = Config::default();
    unknown.select_model("nope/model").unwrap();
    assert_eq!(unknown.model, "nope/model");
}

#[test]
fn select_model_rejects_empty_and_control_characters() {
    let mut cfg = Config::default();
    assert!(cfg.select_model("").is_err());
    assert!(cfg.select_model("   ").is_err());
    assert!(cfg.select_model("bad\u{1}model").is_err());
    assert!(cfg.select_model(&"x".repeat(513)).is_err());
}

#[test]
fn profile_config_builds_isolated_single_provider_config() {
    let mut cfg = config_with_profiles();
    cfg.provider = "work".into();
    cfg.model = "work/model-a".into();
    cfg.proxy_token = Some("top-proxy".into());
    cfg.openai_api_key = Some("top-openai".into());
    cfg.anthropic_api_key = Some("top-anthropic".into());
    cfg.google_api_key = Some("top-google".into());

    let work = &cfg.provider_profiles[0];
    let effective = cfg.profile_config(work).unwrap();
    assert_eq!(effective.provider, "openai");
    assert_eq!(effective.model, "model-a");
    assert!(effective.provider_profiles.is_empty());
    // All top-level credentials are cleared; only the profile's key lands.
    assert_eq!(effective.proxy_token, None);
    assert_eq!(effective.anthropic_api_key, None);
    assert_eq!(effective.google_api_key, None);
    assert_eq!(effective.openai_api_key, None);
}

#[test]
fn profile_config_routes_each_kind_to_its_credential_field() {
    let cfg = Config::default();
    let key = "shared-key".to_string();

    let mut openai = profile("p1", "openai");
    openai.api_key = Some(key.clone());
    let effective = cfg.profile_config(&openai).unwrap();
    assert_eq!(effective.provider, "openai");
    assert_eq!(effective.openai_api_key.as_deref(), Some("shared-key"));

    let mut anthropic = profile("p2", "anthropic");
    anthropic.api_key = Some(key.clone());
    let effective = cfg.profile_config(&anthropic).unwrap();
    assert_eq!(effective.provider, "anthropic");
    assert_eq!(effective.anthropic_api_key.as_deref(), Some("shared-key"));

    let mut google = profile("p3", "google");
    google.api_key = Some(key.clone());
    let effective = cfg.profile_config(&google).unwrap();
    assert_eq!(effective.provider, "google");
    assert_eq!(effective.google_api_key.as_deref(), Some("shared-key"));

    let mut proxy = profile("p4", "proxy");
    proxy.base_url = Some("http://127.0.0.1:9999".into());
    proxy.api_key = Some(key.clone());
    let effective = cfg.profile_config(&proxy).unwrap();
    assert_eq!(effective.provider, "proxy");
    assert_eq!(effective.proxy_token.as_deref(), Some("shared-key"));
    assert_eq!(effective.proxy_url, "http://127.0.0.1:9999");
}

#[test]
fn profile_config_passes_google_fields_through() {
    let cfg = Config::default();
    let mut vertex = profile("vtx", "google-vertex");
    vertex.auth = "oauth".into();
    vertex.project = Some("proj-1".into());
    vertex.location = Some("us-central1".into());
    let effective = cfg.profile_config(&vertex).unwrap();
    assert_eq!(effective.provider, "google-vertex");
    assert_eq!(effective.google_auth, "oauth");
    assert_eq!(effective.google_project.as_deref(), Some("proj-1"));
    assert_eq!(effective.google_location.as_deref(), Some("us-central1"));
}

#[test]
fn active_provider_config_uses_selected_profile_or_plain_config() {
    let mut cfg = config_with_profiles();
    cfg.provider = "proxy".into();
    cfg.model = "model-x".into();
    let plain = cfg.active_provider_config().unwrap();
    assert_eq!(plain.provider, "proxy");
    assert_eq!(plain.model, "model-x");
    assert!(plain.provider_profiles.is_empty());

    cfg.model = "work/model-a".into();
    let effective = cfg.active_provider_config().unwrap();
    assert_eq!(effective.provider, "openai");
    assert_eq!(effective.model, "model-a");
}

#[test]
fn provider_kinds_catalog_is_stable() {
    assert_eq!(
        PROVIDER_KINDS,
        ["proxy", "openai", "anthropic", "google", "google-vertex"]
    );
}

#[test]
fn google_oauth_credentials_are_required_for_google_auth_modes() {
    let mut cfg = Config::default();
    cfg.provider = "google".into();
    cfg.google_auth = "api-key".into();
    let err = cfg.require_provider_credentials().unwrap_err().to_string();
    assert!(err.contains("credentials are required"), "{err}");

    cfg.google_api_key = Some("g-key".into());
    cfg.require_provider_credentials().unwrap();

    // OAuth mode: without a stored refresh token the check must fail rather
    // than silently fall back to an API key.
    cfg.google_auth = "oauth".into();
    cfg.google_api_key = Some("g-key".into());
    let result = cfg.require_provider_credentials();
    // The keyring may be unusable in CI; both failure shapes are acceptable
    // as long as the key is never accepted as an OAuth credential.
    match result {
        Ok(()) => panic!("oauth mode must not succeed without a stored refresh token"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("g-key"),
                "error must not echo the API key: {msg}"
            );
        }
    }
}

#[test]
fn provider_url_validation_blocks_credentials_query_and_fragment() {
    assert!(varynth::config::validate_provider_url("https://api.example.com").is_ok());
    assert!(varynth::config::validate_provider_url("http://127.0.0.1:8787").is_ok());
    assert!(varynth::config::validate_provider_url("ftp://api.example.com").is_err());
    assert!(varynth::config::validate_provider_url("https://user:pass@api.example.com").is_err());
    assert!(varynth::config::validate_provider_url("https://api.example.com?x=1").is_err());
    assert!(varynth::config::validate_provider_url("https://api.example.com#frag").is_err());
    assert!(varynth::config::validate_provider_url("https://").is_err());
}

#[test]
fn profile_toml_round_trip_keeps_qualified_model_and_profiles() {
    let mut cfg = config_with_profiles();
    cfg.provider = "work".into();
    cfg.model = "work/model-a".into();
    let raw = toml::to_string_pretty(&cfg).unwrap();
    let back: Config = toml::from_str(&raw).unwrap();
    assert_eq!(back.model, "work/model-a");
    assert_eq!(back.provider, "work");
    assert_eq!(back.provider_profiles.len(), 2);
    assert_eq!(
        back.provider_profiles[0].models,
        vec!["model-a".to_string()]
    );
    assert_eq!(back.provider_profiles[1].enabled, false);
    back.validate().unwrap();
}
