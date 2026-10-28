use varynth::config::Config;
use varynth::permissions::{evaluate, GateDecision, PermissionMode};
use varynth::sandbox::{Jail, SandboxMode};
use varynth::session::Session;

#[test]
fn shell_chaining_is_rejected() {
    let jail = Jail::new(
        std::env::current_dir().unwrap(),
        vec![],
        SandboxMode::WorkspaceWrite,
        vec!["git".into()],
    );
    assert!(jail.assert_shell("git; whoami").is_err());
    assert!(jail.assert_shell("git && whoami").is_err());
    assert!(jail.assert_shell("git $(whoami)").is_err());
}

#[test]
fn session_ids_cannot_escape_the_sessions_directory() {
    assert!(Session::load("..\\config").is_err());
    assert!(Session::load("not-a-uuid").is_err());
}

/// Finding #2/#4: an admitted interpreter still cannot be handed
/// `-EncodedCommand`; argv is validated, not the first word.
#[test]
fn powershell_encoded_command_is_denied_by_policy() {
    let jail = Jail::new(
        std::env::current_dir().unwrap(),
        vec![],
        SandboxMode::WorkspaceWrite,
        vec!["powershell".into()],
    );
    assert!(
        jail.assert_shell("powershell -NoProfile -EncodedCommand AAAA")
            .is_err(),
        "encoded commands execute arbitrary code"
    );
    // The permission gate refuses the same command even in bypass with a
    // catch-all allow rule, so an "always" approval cannot whitelist it.
    let allow_all =
        varynth::permissions::RuleSet::from_rules(vec![varynth::permissions::PermissionRule::new(
            "bash",
            "*",
            varynth::permissions::RuleEffect::Allow,
        )]);
    let decision = evaluate(
        PermissionMode::Bypass,
        &allow_all,
        "bash",
        "powershell -EncodedCommand AAAA",
        true,
    );
    assert!(
        matches!(decision, GateDecision::Deny(_)),
        "got {decision:?}"
    );
}

/// Finding #4: interpreters and network clients are not on the default
/// allowlist; adding one is the documented explicit escape hatch.
#[test]
fn default_allowlist_excludes_interpreters_and_network_clients() {
    let allow = Config::default().shell_allowlist;
    for banned in [
        "powershell",
        "pwsh",
        "cmd",
        "node",
        "npx",
        "python",
        "python3",
        "pip",
        "curl",
        "wget",
    ] {
        assert!(
            !allow.iter().any(|entry| entry.eq_ignore_ascii_case(banned)),
            "{banned} must require an explicit rule"
        );
    }
}

/// Finding #3/#13: every tool/hook/MCP child receives the explicit allowlist
/// environment, so a planted secret is absent by construction.
#[test]
fn child_environment_never_carries_planted_secrets() {
    std::env::set_var("VARYNTH_ACCESS_TOKEN", "planted-access-token");
    std::env::set_var("VARYNTH_REFRESH_TOKEN", "planted-refresh-token");
    std::env::set_var("OPENAI_API_KEY", "planted-provider-key");
    std::env::set_var("SOME_SECRET_VALUE", "planted-secret");
    std::env::set_var("VARYNTH_TEST_MARKER", "planted-marker");
    let env = varynth::sandbox::child_env();
    let names: Vec<String> = env
        .iter()
        .map(|(key, _)| key.to_string_lossy().to_ascii_uppercase())
        .collect();
    for banned in [
        "VARYNTH_ACCESS_TOKEN",
        "VARYNTH_REFRESH_TOKEN",
        "OPENAI_API_KEY",
        "SOME_SECRET_VALUE",
        "VARYNTH_TEST_MARKER",
    ] {
        assert!(
            !names.iter().any(|name| name == banned),
            "{banned} leaked into the child environment"
        );
    }
    // PATH is the one variable the child genuinely needs.
    assert!(names.iter().any(|name| name == "PATH"));
    // An explicit per-hook/MCP value is still delivered.
    let with_extra =
        varynth::sandbox::child_env_with(&[("HOOK_VALUE".into(), "configured".into())]);
    assert!(with_extra
        .iter()
        .any(|(key, value)| key == "HOOK_VALUE" && value == "configured"));
}
