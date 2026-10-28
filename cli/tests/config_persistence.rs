//! Bootstrap persistence and trust-gated MCP selection.
//!
//! `varynth exec` refreshes the public catalog on every model-using run. That
//! save must never turn one-shot CLI flags, environment secrets, or an MCP
//! path into persisted configuration, and a workspace registry must stay
//! inert after `varynth trust --revoke`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SEEDED_CONFIG: &str = "model = \"keep-model\"\n\
provider = \"openai\"\n\
permission_mode = \"acceptEdits\"\n\
sandbox = \"workspace-write\"\n";

fn varynth(home: &Path, work: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_varynth"))
        .arg("-C")
        .arg(work)
        .args(args)
        .env("VARYNTH_HOME", home)
        .env("OPENAI_API_KEY", "sk-env-secret-value")
        .env("ANTHROPIC_API_KEY", "sk-ant-env-secret")
        // Make every network attempt fail immediately instead of reaching a
        // real provider or the public catalog.
        .env("VARYNTH_API_URL", "http://127.0.0.1:1")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:1")
        .env_remove("VARYNTH_MODEL")
        .env_remove("VARYNTH_PROVIDER")
        .env_remove("VARYNTH_MCP_CONFIG")
        .env_remove("VARYNTH_PROXY_TOKEN")
        .env_remove("VARYNTH_ACCESS_TOKEN")
        .env_remove("VARYNTH_REFRESH_TOKEN")
        .output()
        .expect("varynth binary runs")
}

#[test]
fn exec_flags_and_env_secrets_do_not_rewrite_config() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let config_path = home.path().join("config.toml");
    fs::create_dir_all(home.path()).unwrap();
    fs::write(&config_path, SEEDED_CONFIG).unwrap();

    let out = varynth(
        home.path(),
        work.path(),
        &[
            "--sandbox",
            "danger-full-access",
            "--permission-mode",
            "bypass",
            "exec",
            "hello",
        ],
    );
    // The model call itself is expected to fail (closed port); what matters
    // is that the bootstrap save already ran.
    assert!(
        !out.status.success(),
        "exec unexpectedly succeeded: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let raw = fs::read_to_string(&config_path).unwrap();
    for needle in [
        "danger-full-access",
        "bypass",
        "sk-env-secret-value",
        "sk-ant-env-secret",
        "mcp_config",
    ] {
        assert!(
            !raw.contains(needle),
            "`{needle}` must not be persisted:\n{raw}"
        );
    }
    assert!(raw.contains("permission_mode = \"acceptEdits\""), "{raw}");
    assert!(raw.contains("sandbox = \"workspace-write\""), "{raw}");
    assert!(raw.contains("keep-model"), "{raw}");
}

#[test]
fn one_shot_mcp_config_is_not_persisted() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let config_path = home.path().join("config.toml");
    fs::create_dir_all(home.path()).unwrap();
    fs::write(&config_path, SEEDED_CONFIG).unwrap();
    let registry = work.path().join("one-shot-mcp.json");
    fs::write(&registry, r#"{"mcpServers":{}}"#).unwrap();

    let out = varynth(
        home.path(),
        work.path(),
        &["--mcp-config", registry.to_str().unwrap(), "exec", "hello"],
    );
    assert!(!out.status.success());

    let raw = fs::read_to_string(&config_path).unwrap();
    assert!(!raw.contains("mcp_config"), "{raw}");
    assert!(
        !raw.contains("one-shot-mcp.json"),
        "a per-command registry became permanent:\n{raw}"
    );
}

#[test]
fn revoking_trust_hides_the_project_registry_again() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::create_dir_all(work.path().join(".varynth")).unwrap();
    fs::write(
        work.path().join(".varynth/mcp.json"),
        r#"{"mcpServers":{"project-evil":{"type":"stdio","command":"definitely-not-a-real-binary"}}}"#,
    )
    .unwrap();

    let trust = varynth(home.path(), work.path(), &["trust"]);
    assert!(
        trust.status.success(),
        "trust failed: {}",
        String::from_utf8_lossy(&trust.stderr)
    );

    let listed = varynth(home.path(), work.path(), &["mcp", "status"]);
    assert!(listed.status.success());
    let trusted_json = String::from_utf8_lossy(&listed.stdout);
    assert!(
        trusted_json.contains("project-evil"),
        "trusted workspace registry should be listed: {trusted_json}"
    );

    let revoke = varynth(home.path(), work.path(), &["trust", "--revoke"]);
    assert!(revoke.status.success());

    let hidden = varynth(home.path(), work.path(), &["mcp", "status"]);
    assert!(hidden.status.success());
    let hidden_json = String::from_utf8_lossy(&hidden.stdout);
    assert!(
        !hidden_json.contains("project-evil"),
        "revoked workspace registry must not be listed: {hidden_json}"
    );
    assert!(
        String::from_utf8_lossy(&hidden.stderr).contains("not trusted"),
        "the user must be told why: {}",
        String::from_utf8_lossy(&hidden.stderr)
    );

    // The explicit per-command opt-in remains available after revocation.
    let registry = work.path().join(".varynth/mcp.json");
    let explicit = varynth(
        home.path(),
        work.path(),
        &["--mcp-config", registry.to_str().unwrap(), "mcp", "status"],
    );
    assert!(explicit.status.success());
    assert!(String::from_utf8_lossy(&explicit.stdout).contains("project-evil"));
}
