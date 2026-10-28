//! End-to-end checks for the OS-level write boundary behind `--sandbox`.
//!
//! These go through the real `bash` tool (path jail + shell allowlist + OS
//! boundary), not just the platform spawn helper, so a regression in the
//! wiring shows up here.

use serde_json::json;
use std::path::Path;
use tempfile::tempdir;
use varynth::mailbox::Bus;
use varynth::sandbox::{Jail, SandboxMode};
use varynth::tools::{dispatch, ToolCtx};

fn run(jail: &Jail, workspace: &Path, command: &str) -> String {
    let bus = Bus::at(workspace.join("bus.jsonl"));
    let ctx = ToolCtx {
        bus: &bus,
        session_id: None,
        checkpoints: None,
        docker: None,
    };
    match dispatch("bash", &json!({ "command": command }), jail, &ctx) {
        Ok(output) => output,
        Err(error) => format!("error: {error}"),
    }
}

#[cfg(windows)]
#[test]
fn workspace_write_is_denied_outside_the_workspace_by_the_os() {
    let base = tempdir().unwrap();
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let jail = Jail::new(
        workspace.clone(),
        vec![],
        SandboxMode::WorkspaceWrite,
        vec!["set-content".into(), "get-content".into()],
    );

    let inside = run(&jail, &workspace, "Set-Content -Path ok.txt -Value hi");
    println!("[workspace-write inside] {inside}");
    assert!(
        workspace.join("ok.txt").exists(),
        "write inside the workspace failed: {inside}"
    );

    // Outside the workspace *and* outside the sandbox temp directory: the
    // kernel denies the write, so the file never appears.
    let escape = base.path().join("escape.txt");
    let outside = run(
        &jail,
        &workspace,
        &format!("Set-Content -LiteralPath '{}' -Value hi", escape.display()),
    );
    println!("[workspace-write outside] {outside}");
    assert!(
        !escape.exists(),
        "write outside the workspace landed: {outside}"
    );
    assert!(
        outside.to_ascii_lowercase().contains("denied")
            || outside.to_ascii_lowercase().contains("access"),
        "expected an OS-level access error, got: {outside}"
    );
}

#[cfg(windows)]
#[test]
fn read_only_allows_reads_and_denies_writes_at_the_os() {
    let workspace = tempdir().unwrap();
    let jail = Jail::new(
        workspace.path().to_path_buf(),
        vec![],
        SandboxMode::ReadOnly,
        vec!["set-content".into(), "get-content".into()],
    );
    std::fs::write(workspace.path().join("seed.txt"), "seed-content").unwrap();

    let read = run(&jail, workspace.path(), "Get-Content seed.txt");
    println!("[read-only read] {read}");
    assert!(
        read.contains("seed-content"),
        "reads must stay allowed: {read}"
    );

    let blocked = workspace.path().join("blocked.txt");
    let write = run(
        &jail,
        workspace.path(),
        &format!("Set-Content -LiteralPath '{}' -Value hi", blocked.display()),
    );
    println!("[read-only write] {write}");
    assert!(!blocked.exists(), "read-only mode allowed a write: {write}");
}

#[cfg(not(windows))]
#[test]
fn unenforced_platforms_stay_honest() {
    // No backend has been implemented or exercised on this platform, so the
    // status must say degraded rather than pretend.
    let status = varynth::sandbox::enforcement(SandboxMode::WorkspaceWrite);
    println!("[non-windows enforcement] {}", status.status());
    assert!(
        matches!(status, varynth::sandbox::Enforcement::Degraded(_)),
        "{}",
        status.status()
    );
}
