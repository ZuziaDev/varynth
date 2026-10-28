//! Trust-gate acceptance: `varynth trust` records a canonical workspace in
//! `~/.varynth/trust.json`, and a cloned repository's rules stay inert until
//! that decision exists.

use std::fs;
use std::process::Command;

use varynth::permissions::RuleSet;
use varynth::trust::TrustStore;

#[test]
fn trust_cli_round_trip_activates_project_rules() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::create_dir_all(work.path().join(".varynth")).unwrap();
    fs::write(
        work.path().join(".varynth/permissions.json"),
        r#"{"rules":[{"tool":"write_file","match":"*","effect":"allow"}]}"#,
    )
    .unwrap();

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_varynth"))
            .arg("-C")
            .arg(work.path())
            .args(args)
            .env("VARYNTH_HOME", home.path())
            .output()
            .expect("varynth binary runs")
    };

    // Untrusted: the project rule must not load.
    let user_rules = home.path().join("permissions.json");
    let untrusted = RuleSet::load_with(work.path(), &user_rules, false);
    assert!(untrusted.project_pending);
    assert_eq!(untrusted.decision("write_file", "x.txt"), None);

    let out = run(&["trust"]);
    assert!(
        out.status.success(),
        "trust failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("trusted"), "got: {stdout}");
    assert!(TrustStore::at(home.path().join("trust.json")).is_trusted(work.path()));

    // Trusted: the same file now contributes its rule.
    let trusted = RuleSet::load_with(work.path(), &user_rules, true);
    assert_eq!(trusted.project_rules, 1);
    assert_eq!(
        trusted.decision("write_file", "x.txt"),
        Some(varynth::permissions::RuleEffect::Allow)
    );

    let out = run(&["trust", "--revoke"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("revoked"));
    assert!(!TrustStore::at(home.path().join("trust.json")).is_trusted(work.path()));

    let out = run(&["trust", "--revoke"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not trusted"));
}

/// Finding #1: `.varynth/mcp.json` is project-scoped. An untrusted clone's
/// registry is ignored; after `varynth trust` the same file loads.
#[test]
fn project_mcp_registry_is_inert_until_trusted() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::create_dir_all(work.path().join(".varynth")).unwrap();
    fs::write(
        work.path().join(".varynth/mcp.json"),
        r#"{"mcpServers":{"solo":{"type":"http","url":"https://solo.example/mcp"}}}"#,
    )
    .unwrap();

    let status = || {
        let out = Command::new(env!("CARGO_BIN_EXE_varynth"))
            .arg("-C")
            .arg(work.path())
            .arg("mcp")
            .arg("status")
            .env("VARYNTH_HOME", home.path())
            .output()
            .expect("varynth binary runs");
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (code, stdout, stderr) = status();
    assert!(code.success(), "mcp status failed: {stderr}");
    assert_eq!(stdout.trim(), "[]", "untrusted project MCP must not load");
    assert!(stderr.contains("not trusted"), "got: {stderr}");

    let trust = Command::new(env!("CARGO_BIN_EXE_varynth"))
        .arg("-C")
        .arg(work.path())
        .arg("trust")
        .env("VARYNTH_HOME", home.path())
        .output()
        .expect("varynth binary runs");
    assert!(trust.status.success());

    let (code, stdout, stderr) = status();
    assert!(code.success(), "mcp status failed: {stderr}");
    assert!(
        stdout.contains("\"solo\""),
        "trusted project MCP must load: {stdout}"
    );
}
