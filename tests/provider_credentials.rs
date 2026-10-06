use varynth::config::Config;

#[test]
fn missing_proxy_token_is_rejected() {
    let cfg = Config::default();
    let error = cfg.require_provider_credentials().unwrap_err().to_string();
    assert!(error.contains("credentials are required"));
    assert!(!error.contains("test-secret"));
}

#[test]
fn configured_provider_key_is_accepted() {
    let mut cfg = Config::default();
    cfg.openai_api_key = Some("test-key".into());
    cfg.provider = "openai".into();
    cfg.require_provider_credentials().unwrap();
}

#[test]
fn empty_provider_key_is_rejected() {
    let mut cfg = Config::default();
    cfg.anthropic_api_key = Some("  ".into());
    cfg.provider = "anthropic".into();
    assert!(cfg.require_provider_credentials().is_err());
}
