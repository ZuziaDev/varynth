use varynth::config::Config;

#[test]
fn dashboard_token_is_loaded_from_environment() {
    let previous = std::env::var("VARYNTH_DASHBOARD_TOKEN").ok();
    std::env::set_var("VARYNTH_DASHBOARD_TOKEN", "dashboard-test-token");
    let cfg = Config::default().with_env();
    assert_eq!(cfg.dashboard_token.as_deref(), Some("dashboard-test-token"));
    match previous {
        Some(value) => std::env::set_var("VARYNTH_DASHBOARD_TOKEN", value),
        None => std::env::remove_var("VARYNTH_DASHBOARD_TOKEN"),
    }
}
