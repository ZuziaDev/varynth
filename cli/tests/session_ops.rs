//! G4/G5/G6 acceptance: multi-undo/redo, session fork (CLI + parent link in
//! `sessions`) and redacted Markdown/JSON export.
//!
//! One test drives the whole flow because it sets `VARYNTH_HOME` for the
//! process; the binary invocations inherit the same isolated home.

use std::fs;
use std::process::Command;

use varynth::checkpoint::{CheckpointStore, Redo};
use varynth::export::{export_session, ExportFormat};
use varynth::session::{ChatMessage, Session, ToolCall};

fn msg(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: content.into(),
        ..Default::default()
    }
}

#[test]
fn checkpoint_fork_and_export_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("VARYNTH_HOME", home.path());
    let work = tempfile::tempdir().unwrap();

    // ---- G4: three writes, /undo 2, /redo ----
    let store = CheckpointStore::at(home.path().join("checkpoints"));
    let file = work.path().join("code.txt");
    fs::write(&file, "v0").unwrap();
    store.snapshot("session-undo", &file).unwrap();
    fs::write(&file, "v1").unwrap();
    store.snapshot("session-undo", &file).unwrap();
    fs::write(&file, "v2").unwrap();
    store.snapshot("session-undo", &file).unwrap();
    fs::write(&file, "v3").unwrap();

    assert_eq!(store.list("session-undo").unwrap().len(), 3);
    assert_eq!(store.undo_n("session-undo", 2).unwrap().len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    assert_eq!(
        store.redo("session-undo").unwrap(),
        Redo::Restored(file.clone())
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "v2");

    // ---- G5: fork at message 2 ----
    let mut source = Session::new(&work.path().display().to_string(), "mock").unwrap();
    for (role, text) in [
        ("user", "first question"),
        ("assistant", "first answer"),
        ("user", "second question"),
        ("assistant", "second answer"),
    ] {
        source.append(msg(role, text)).unwrap();
    }
    let source_id = source.id().to_string();
    let planted = "sk-planted-SECRET-1234567890";
    source
        .append(ChatMessage {
            role: "assistant".into(),
            content: format!("using api_key={planted}"),
            tool_calls: Some(vec![ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({
                    "command": "curl -H 'Authorization: Bearer topsecretvalue123' https://example.test"
                }),
            }]),
            ..Default::default()
        })
        .unwrap();
    source.append(msg("tool", "ok")).unwrap();

    let fork = Session::fork_from(&source_id, Some(2)).unwrap();
    assert_eq!(fork.meta.parent.as_deref(), Some(source_id.as_str()));
    // Summary of the abandoned tail, then the two kept messages.
    assert_eq!(fork.messages.len(), 3);
    assert_eq!(fork.messages[0].role, "system");
    assert!(fork.messages[0].content.contains("abandoned"));
    assert_eq!(fork.messages[2].content, "first answer");
    assert_eq!(
        Session::load(&source_id).unwrap().messages.len(),
        6,
        "the original keeps every message"
    );

    // `varynth sessions` lists both and shows the parent link.
    let sessions_out = Command::new(env!("CARGO_BIN_EXE_varynth"))
        .arg("sessions")
        .env("VARYNTH_HOME", home.path())
        .output()
        .unwrap();
    assert!(
        sessions_out.status.success(),
        "sessions failed: {}",
        String::from_utf8_lossy(&sessions_out.stderr)
    );
    let listing = String::from_utf8_lossy(&sessions_out.stdout);
    assert!(listing.contains(&source_id), "listing: {listing}");
    assert!(listing.contains(fork.id()), "listing: {listing}");
    assert!(
        listing.contains(&format!("(fork of {source_id})")),
        "listing: {listing}"
    );

    // `varynth sessions fork <id> --at N` works through the binary too.
    let fork_out = Command::new(env!("CARGO_BIN_EXE_varynth"))
        .args(["sessions", "fork", &source_id, "--at", "1"])
        .env("VARYNTH_HOME", home.path())
        .output()
        .unwrap();
    assert!(
        fork_out.status.success(),
        "fork failed: {}",
        String::from_utf8_lossy(&fork_out.stderr)
    );
    let cli_fork_id = String::from_utf8_lossy(&fork_out.stdout).trim().to_string();
    let cli_fork = Session::load(&cli_fork_id).unwrap();
    assert_eq!(cli_fork.meta.parent.as_deref(), Some(source_id.as_str()));

    // ---- G6: export md + json, planted secret absent ----
    let md_path = work.path().join("transcript.md");
    let json_path = work.path().join("transcript.json");
    export_session(&source, ExportFormat::Markdown, Some(&md_path), false).unwrap();
    export_session(&source, ExportFormat::Json, Some(&json_path), false).unwrap();
    for path in [&md_path, &json_path] {
        let body = fs::read_to_string(path).unwrap();
        assert!(!body.contains(planted), "secret leaked into {path:?}");
        assert!(
            !body.contains("topsecretvalue123"),
            "bearer leaked into {path:?}"
        );
        assert!(body.contains("[REDACTED]"));
        assert!(body.contains("first question"), "conversation survives");
    }

    // And through the CLI.
    let cli_md = work.path().join("cli.md");
    let export_out = Command::new(env!("CARGO_BIN_EXE_varynth"))
        .args([
            "sessions",
            "export",
            &source_id,
            "--format",
            "md",
            "--out",
            cli_md.to_str().unwrap(),
        ])
        .env("VARYNTH_HOME", home.path())
        .output()
        .unwrap();
    assert!(
        export_out.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&export_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&export_out.stdout).trim(),
        cli_md.display().to_string()
    );
    let cli_body = fs::read_to_string(&cli_md).unwrap();
    assert!(!cli_body.contains(planted));
    assert!(cli_body.contains(&source_id));

    // Overwrite protection: a second export without --force fails.
    let again = Command::new(env!("CARGO_BIN_EXE_varynth"))
        .args([
            "sessions",
            "export",
            &source_id,
            "--out",
            cli_md.to_str().unwrap(),
        ])
        .env("VARYNTH_HOME", home.path())
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));
}
