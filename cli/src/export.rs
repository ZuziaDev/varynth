//! Session transcript export (`varynth sessions export`, `/export`).
//!
//! Renders a loaded [`Session`] as Markdown or JSON. Everything written —
//! message bodies, tool arguments and session metadata — goes through
//! [`redact`]: PEM/OpenSSH private-key blocks, well-known token shapes, long
//! base64/hex blobs, and `api_key=…`/`MYSQL_PWD=…`-style pairs are replaced
//! before they reach the file. Keyring/auth material is never part of a
//! session, and image bytes are not exported (only a placeholder), so a
//! transcript can be shared without leaking credentials or blobs.

use anyhow::{Context, Result};
use regex::Regex;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::session::{ChatMessage, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Json,
}

impl ExportFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            other => anyhow::bail!("unknown export format `{other}` (use md or json)"),
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Json => "json",
        }
    }
}

const REDACTED: &str = "[REDACTED]";

struct Redactor {
    /// Private-key blocks (PEM/OpenSSH/PKCS#8) and well-known credential
    /// shapes (OpenAI/Anthropic/GitHub/Slack/Google/AWS/JWT) that are replaced
    /// wherever they appear. Blocks run first so their base64 body never
    /// reaches the blob rules. A terminated block is removed whole; an
    /// unterminated one loses its header plus every contiguous base64 line.
    tokens: Vec<Regex>,
    /// `name = value` / `"name": "value"` pairs whose value is replaced but
    /// whose name is kept, so the transcript still reads sensibly.
    pairs: Regex,
}

/// Minimum unbroken run of `[A-Za-z0-9+/]` treated as a base64 blob. Normal
/// prose (and paths/URLs, which contain separators) never reaches this length.
const BASE64_BLOB_MIN: usize = 80;
/// Minimum unbroken run of hex digits treated as a binary/hash blob.
const HEX_BLOB_MIN: usize = 64;

fn redactor() -> &'static Redactor {
    static REDACTOR: LazyLock<Redactor> = LazyLock::new(|| {
        let mut tokens: Vec<Regex> = [
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?:\r?\n[A-Za-z0-9+/=]{16,})*",
            r"sk-ant-[A-Za-z0-9_-]{8,}",
            r"sk-[A-Za-z0-9_-]{8,}",
            r"gh[pous]_[A-Za-z0-9]{16,}",
            r"github_pat_[A-Za-z0-9_]{16,}",
            r"xox[baprs]-[A-Za-z0-9-]{10,}",
            r"AKIA[0-9A-Z]{16}",
            r"AIza[0-9A-Za-z_-]{30,}",
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("static redaction pattern"))
        .collect();
        tokens.push(
            Regex::new(&format!(r"[A-Za-z0-9+/]{{{BASE64_BLOB_MIN},}}={{0,2}}"))
                .expect("static redaction pattern"),
        );
        tokens.push(
            Regex::new(&format!(r"\b[0-9a-fA-F]{{{HEX_BLOB_MIN},}}\b"))
                .expect("static redaction pattern"),
        );
        let pairs = Regex::new(
            r#"(?i)(^|[^A-Za-z0-9_])((?:[a-z0-9]+[_-])?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|token|secret|password|passwd|pwd|pass|authorization))(\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s"',;}]{6,})"#,
        )
        .expect("static redaction pattern");
        Redactor { tokens, pairs }
    });
    &REDACTOR
}

/// Replace credential-looking strings with `[REDACTED]`.
pub fn redact(text: &str) -> String {
    let redactor = redactor();
    let mut out = text.to_string();
    for token in &redactor.tokens {
        out = token.replace_all(&out, REDACTED).into_owned();
    }
    redactor
        .pairs
        .replace_all(&out, format!("${{1}}${{2}}${{3}}{REDACTED}"))
        .into_owned()
}

fn redact_message(message: &ChatMessage) -> ChatMessage {
    ChatMessage {
        role: message.role.clone(),
        content: redact(&message.content),
        tool_call_id: message.tool_call_id.clone(),
        tool_calls: message.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|call| crate::session::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: redact_json(&call.arguments),
                })
                .collect()
        }),
        // Media blobs are never exported.
        images: Vec::new(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "auth_token",
        "authorization",
        "token",
        "secret",
        "password",
        "passwd",
    ]
    .iter()
    .any(|needle| key.contains(needle))
        || key
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|part| part == "pwd" || part == "pass")
}

fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        Value::String(REDACTED.into())
                    } else {
                        redact_json(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn redacted_messages(session: &Session) -> Vec<ChatMessage> {
    session.messages.iter().map(redact_message).collect()
}

/// Markdown transcript: title, metadata, then one section per message.
pub fn render_markdown(session: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", redact(&session.meta.title)));
    out.push_str(&format!("- session: `{}`\n", session.meta.id));
    out.push_str(&format!("- model: `{}`\n", redact(&session.meta.model)));
    out.push_str(&format!("- cwd: `{}`\n", redact(&session.meta.cwd)));
    out.push_str(&format!(
        "- created: {}\n",
        session.meta.created_at.to_rfc3339()
    ));
    out.push_str(&format!(
        "- updated: {}\n",
        session.meta.updated_at.to_rfc3339()
    ));
    if let Some(parent) = &session.meta.parent {
        out.push_str(&format!("- forked from: `{}`\n", redact(parent)));
    }
    out.push('\n');

    for message in redacted_messages(session) {
        out.push_str(&format!("## {}\n\n", message.role));
        if !message.content.trim().is_empty() {
            out.push_str(message.content.trim_end());
            out.push_str("\n\n");
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                out.push_str(&format!("### tool call: {}\n\n", call.name));
                out.push_str("```json\n");
                out.push_str(
                    &serde_json::to_string_pretty(&call.arguments)
                        .unwrap_or_else(|_| "null".into()),
                );
                out.push_str("\n```\n\n");
            }
        }
    }
    out
}

/// JSON export: the session record with meta and messages, redacted.
pub fn render_json(session: &Session) -> Result<String> {
    let meta = redact_json(&serde_json::to_value(&session.meta)?);
    let record = serde_json::json!({
        "format": "varynth-session/1",
        "meta": meta,
        "messages": redacted_messages(session),
    });
    Ok(serde_json::to_string_pretty(&record)?)
}

/// Default output path for a session in the current directory.
pub fn default_path(session: &Session, format: ExportFormat) -> PathBuf {
    PathBuf::from(format!(
        "varynth-session-{}.{}",
        session.meta.id,
        format.extension()
    ))
}

/// Write the transcript to `out` (default: `./varynth-session-<id>.<ext>`).
/// Refuses to overwrite an existing file unless `force` is set.
pub fn export_session(
    session: &Session,
    format: ExportFormat,
    out: Option<&Path>,
    force: bool,
) -> Result<PathBuf> {
    let path = match out {
        Some(path) => path.to_path_buf(),
        None => default_path(session, format),
    };
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists (pass --force to overwrite)",
            path.display()
        );
    }
    let body = match format {
        ExportFormat::Markdown => render_markdown(session),
        ExportFormat::Json => render_json(session)?,
    };
    let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(body.as_bytes())?;
    if !body.ends_with('\n') {
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCall;
    use uuid::Uuid;

    fn session_with_secret(dir: &std::path::Path) -> Session {
        let mut session = Session::create_at(
            Uuid::new_v4().to_string(),
            dir.join("session.jsonl"),
            "C:\\tmp",
            "mock",
        )
        .unwrap();
        session.set_title("debug export").unwrap();
        session
            .append(ChatMessage {
                role: "user".into(),
                content: "here is my key sk-proj-abcdefghijklmnop and password: hunter2".into(),
                ..Default::default()
            })
            .unwrap();
        session
            .append(ChatMessage {
                role: "assistant".into(),
                content: "stored".into(),
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({
                        "command": "curl -H 'Authorization: Bearer abcdefgh12345' https://x",
                        "env": { "OPENAI_API_KEY": "sk-live-0123456789abcdef" },
                        "headers": { "api_key": "plain-visible-value" }
                    }),
                }]),
                ..Default::default()
            })
            .unwrap();
        session
    }

    #[test]
    fn markdown_and_json_redact_secrets_and_media() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_secret(dir.path());

        let md = render_markdown(&session);
        let json = render_json(&session).unwrap();
        for body in [&md, &json] {
            for secret in [
                "sk-proj-abcdefghijklmnop",
                "hunter2",
                "abcdefgh12345",
                "sk-live-0123456789abcdef",
                "plain-visible-value",
                "Bearer ",
            ] {
                assert!(!body.contains(secret), "leaked {secret}: {body}");
            }
            assert!(body.contains(REDACTED));
        }
        // The visible conversation survives; the session id stays.
        assert!(md.contains("here is my key"));
        assert!(md.contains(&session.meta.id));
        assert!(json.contains("\"role\": \"assistant\""));
        assert!(json.contains("varynth-session/1"));
    }

    #[test]
    fn planted_secrets_never_reach_markdown_or_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create_at(
            Uuid::new_v4().to_string(),
            dir.path().join("session.jsonl"),
            "C:\\work\\sk-proj-cwdleak0123456789",
            "sk-ant-modelleak0123456789",
        )
        .unwrap();
        session.meta.parent = Some("sk-proj-parentleak0123456789".into());
        session.set_title("debug export").unwrap();

        let openssh_key = concat!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZWQyNTUxOQAAACD\n",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            "-----END OPENSSH PRIVATE KEY-----",
        );
        let blob = "QUJD".repeat(50);
        assert_eq!(blob.len(), 200);
        // Short enough that only the hex rule can catch it.
        let hex_blob = "0123456789abcdef".repeat(4);
        assert_eq!(hex_blob.len(), 64);
        let ordinary =
            "The quick brown fox jumps over the lazy dog; cargo test --all stays readable.";

        session
            .append(ChatMessage {
                role: "user".into(),
                content: format!(
                    "{openssh_key}\n\nblob: {blob}\nhex: {hex_blob}\nMYSQL_PWD=hunter2supersecret\nDB_PASS=anothersecret\n{ordinary}"
                ),
                ..Default::default()
            })
            .unwrap();

        let md = render_markdown(&session);
        let json = render_json(&session).unwrap();
        // Exercise the real write path too, not just the renderers.
        let md_path = dir.path().join("proof.md");
        let json_path = dir.path().join("proof.json");
        export_session(&session, ExportFormat::Markdown, Some(&md_path), true).unwrap();
        export_session(&session, ExportFormat::Json, Some(&json_path), true).unwrap();
        let written = [
            std::fs::read_to_string(&md_path).unwrap(),
            std::fs::read_to_string(&json_path).unwrap(),
        ];
        // Opt-in fixed output for reproducible `grep` audits.
        if let Some(proof_dir) = std::env::var_os("VARYNTH_EXPORT_PROOF_DIR") {
            let proof_dir = std::path::PathBuf::from(proof_dir);
            std::fs::create_dir_all(&proof_dir).unwrap();
            export_session(
                &session,
                ExportFormat::Markdown,
                Some(&proof_dir.join("proof.md")),
                true,
            )
            .unwrap();
            export_session(
                &session,
                ExportFormat::Json,
                Some(&proof_dir.join("proof.json")),
                true,
            )
            .unwrap();
        }
        for body in [&md, &json, &written[0], &written[1]] {
            for secret in [
                openssh_key,
                blob.as_str(),
                hex_blob.as_str(),
                "hunter2supersecret",
                "anothersecret",
                "sk-proj-cwdleak0123456789",
                "sk-ant-modelleak0123456789",
                "sk-proj-parentleak0123456789",
            ] {
                assert!(!body.contains(secret), "leaked {secret}: {body}");
            }
            assert!(body.contains(ordinary), "ordinary text mangled: {body}");
            assert!(body.contains(REDACTED));
        }
    }

    #[test]
    fn export_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_secret(dir.path());
        let out = dir.path().join("out.md");

        let written = export_session(&session, ExportFormat::Markdown, Some(&out), false).unwrap();
        assert_eq!(written, out);

        let err = export_session(&session, ExportFormat::Markdown, Some(&out), false).unwrap_err();
        assert!(err.to_string().contains("--force"));
        assert!(export_session(&session, ExportFormat::Markdown, Some(&out), true).is_ok());
    }

    #[test]
    fn default_path_uses_the_session_id_and_format() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_secret(dir.path());
        let md = default_path(&session, ExportFormat::Markdown);
        assert_eq!(
            md.file_name().unwrap().to_string_lossy(),
            format!("varynth-session-{}.md", session.meta.id)
        );
        assert_eq!(ExportFormat::parse("json").unwrap(), ExportFormat::Json);
        assert!(ExportFormat::parse("yaml").is_err());
    }
}
