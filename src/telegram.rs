use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

use crate::approval_relay::{self, ApprovalVote};
use crate::config::Config;
use crate::dashboard::AppState;

pub fn spawn(cfg: Config, state: AppState) {
    let Some(token) = cfg.telegram_bot_token.clone().filter(|t| !t.is_empty()) else {
        eprintln!("varynth telegram: no token (set VARYNTH_TELEGRAM_BOT_TOKEN or ~/.varynth/telegram.env)");
        return;
    };
    tokio::spawn(async move {
        if let Err(e) = poll_loop(token, cfg.telegram_allow_from.clone(), state).await {
            eprintln!("varynth telegram stopped: {e}");
        }
    });
}

async fn poll_loop(token: String, allow: Vec<String>, state: AppState) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()?;
    let base = format!("https://api.telegram.org/bot{token}");
    let me = client.get(format!("{base}/getMe")).send().await?;
    let me_txt = me.text().await?;
    let me_v: Value = serde_json::from_str(&me_txt).unwrap_or(json!({}));
    let username = me_v
        .pointer("/result/username")
        .and_then(|v| v.as_str())
        .unwrap_or("bot");
    eprintln!("varynth telegram: polling as @{username}");

    let mut offset: i64 = 0;
    loop {
        let url = format!("{base}/getUpdates?timeout=25&offset={offset}");
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("varynth telegram poll error: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        let text = resp.text().await.unwrap_or_default();
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        if v.get("ok") != Some(&json!(true)) {
            let desc = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown");
            if desc.contains("Conflict") {
                eprintln!("varynth telegram: 409 Conflict — another poller holds this token. Set a dedicated bot in ~/.varynth/telegram.env");
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
            continue;
        }
        let Some(arr) = v.get("result").and_then(|r| r.as_array()) else {
            continue;
        };
        for upd in arr {
            if let Some(id) = upd.get("update_id").and_then(|x| x.as_i64()) {
                offset = id + 1;
            }
            let Some(msg) = upd.get("message") else {
                continue;
            };
            let chat_id = msg
                .pointer("/chat/id")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let sender = msg
                .pointer("/from/id")
                .and_then(|x| x.as_i64())
                .unwrap_or(chat_id);
            let chat_type = msg
                .pointer("/chat/type")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if chat_type != "private" {
                continue;
            }
            let body = msg
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if body.is_empty() {
                continue;
            }
            let sid = sender.to_string();
            if !allow.is_empty() && !allow.iter().any(|a| a == &sid) {
                let _ = send(
                    &client,
                    &base,
                    chat_id,
                    &format!(
                        "Not allowlisted. Add this Telegram user id to ~/.varynth/config.toml:\n\ntelegram_allow_from = [\"{sid}\"]\n\nThen restart `varynth serve`."
                    ),
                )
                .await;
                continue;
            }
            if allow.is_empty() {
                let _ = send(
                    &client,
                    &base,
                    chat_id,
                    &format!(
                        "Pairing: add your id, then restart serve.\n\ntelegram_allow_from = [\"{sid}\"]\n\nin ~/.varynth/config.toml"
                    ),
                )
                .await;
                continue;
            }
            if too_long(&body) {
                let _ = send(&client, &base, chat_id, "message too long; split it up").await;
                continue;
            }
            if body
                .split_whitespace()
                .next()
                .is_some_and(|w| w.eq_ignore_ascii_case("approve"))
            {
                let reply = match parse_approve(&body) {
                    Some((id, vote)) => {
                        if approval_relay::respond(&id, vote) {
                            match vote {
                                ApprovalVote::Once => "approved (once)",
                                ApprovalVote::Always => "approved (always)",
                                ApprovalVote::Deny => "denied",
                            }
                            .to_string()
                        } else {
                            format!("no pending approval {id}")
                        }
                    }
                    None => "kullanım: approve <id> once|always|deny".to_string(),
                };
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }
            let _ = send(&client, &base, chat_id, "working…").await;
            match state.turn(&body).await {
                Ok(reply) => {
                    for chunk in chunk_text(&reply, 3900) {
                        let _ = send(&client, &base, chat_id, &chunk).await;
                    }
                }
                Err(e) => {
                    let _ =
                        send(&client, &base, chat_id, &clip_error(&format!("error: {e}"))).await;
                }
            }
        }
    }
}

async fn send(client: &reqwest::Client, base: &str, chat_id: i64, text: &str) -> Result<()> {
    let body = json!({ "chat_id": chat_id, "text": text });
    let resp = client
        .post(format!("{base}/sendMessage"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("sendMessage failed: {t}");
    }
    Ok(())
}

fn chunk_text(s: &str, n: usize) -> Vec<String> {
    if s.len() <= n {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = s;
    while rest.len() > n {
        let mut cut = n;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(i) = rest[..cut].rfind('\n') {
            if i > n / 4 {
                cut = i + 1;
            }
        }
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

const MAX_INPUT_CHARS: usize = 8_000;
const MAX_ERROR_CHARS: usize = 500;

/// True when an incoming Telegram message exceeds the accepted input size.
fn too_long(s: &str) -> bool {
    s.chars().count() > MAX_INPUT_CHARS
}

/// Clips an error string to MAX_ERROR_CHARS chars (char-boundary safe),
/// appending an ellipsis when truncated.
fn clip_error(s: &str) -> String {
    if s.chars().count() <= MAX_ERROR_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_ERROR_CHARS).collect();
    out.push('…');
    out
}

/// Parses an `approve <id> <decision>` command from a message body.
/// The command word is case-insensitive; the decision must be one of
/// `once|always|deny` (as accepted by [`crate::approval_relay::ApprovalVote::parse`]).
/// Returns `None` for non-approve messages and malformed commands.
fn parse_approve(body: &str) -> Option<(String, crate::approval_relay::ApprovalVote)> {
    let mut words = body.split_whitespace();
    let cmd = words.next()?;
    if !cmd.eq_ignore_ascii_case("approve") {
        return None;
    }
    let id = words.next()?;
    let decision = words.next()?;
    if words.next().is_some() {
        return None;
    }
    let vote = crate::approval_relay::ApprovalVote::parse(decision)?;
    Some((id.to_string(), vote))
}

#[cfg(test)]
mod tests {
    use super::{chunk_text, clip_error, parse_approve, too_long};
    use crate::approval_relay::ApprovalVote;

    #[test]
    fn chunks_short_unchanged() {
        assert_eq!(chunk_text("hi", 10), vec!["hi".to_string()]);
    }

    #[test]
    fn chunks_long() {
        let s = "a".repeat(100);
        let parts = chunk_text(&s, 30);
        assert!(parts.len() >= 3);
        assert_eq!(parts.concat().len(), 100);
    }

    #[test]
    fn too_long_flags_only_over_cap() {
        assert!(!too_long(""));
        assert!(!too_long(&"a".repeat(8_000)));
        assert!(too_long(&"a".repeat(8_001)));
    }

    #[test]
    fn clip_error_leaves_short_text_alone() {
        assert_eq!(clip_error("error: boom"), "error: boom");
    }

    #[test]
    fn clip_error_truncates_and_is_char_boundary_safe() {
        let long = format!("error: {}", "x".repeat(1_000));
        let clipped = clip_error(&long);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), 501);

        // Multi-byte text: byte slicing at 500 would panic; char-based cut must not.
        let uni = "héllo→".repeat(200);
        let clipped = clip_error(&uni);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), 501);
    }

    #[test]
    fn parse_approve_accepts_valid_forms() {
        assert_eq!(
            parse_approve("approve abc-123 once"),
            Some(("abc-123".to_string(), ApprovalVote::Once))
        );
        assert_eq!(
            parse_approve("approve abc-123 always"),
            Some(("abc-123".to_string(), ApprovalVote::Always))
        );
        assert_eq!(
            parse_approve("approve abc-123 deny"),
            Some(("abc-123".to_string(), ApprovalVote::Deny))
        );
    }

    #[test]
    fn parse_approve_command_is_case_insensitive() {
        assert_eq!(
            parse_approve("Approve abc-123 once"),
            Some(("abc-123".to_string(), ApprovalVote::Once))
        );
        assert_eq!(
            parse_approve("APPROVE abc-123 deny"),
            Some(("abc-123".to_string(), ApprovalVote::Deny))
        );
        assert_eq!(
            parse_approve("aPpRoVe abc-123 always"),
            Some(("abc-123".to_string(), ApprovalVote::Always))
        );
    }

    #[test]
    fn parse_approve_tolerates_extra_whitespace() {
        assert_eq!(
            parse_approve("  approve \t id-1   deny  "),
            Some(("id-1".to_string(), ApprovalVote::Deny))
        );
    }

    #[test]
    fn parse_approve_rejects_bad_decision() {
        assert_eq!(parse_approve("approve abc-123 sometimes"), None);
        assert_eq!(parse_approve("approve abc-123 yes"), None);
        assert_eq!(parse_approve("approve abc-123 ONCE"), None);
        assert_eq!(parse_approve("approve abc-123 \"once\""), None);
    }

    #[test]
    fn parse_approve_rejects_wrong_arity() {
        assert_eq!(parse_approve("approve"), None);
        assert_eq!(parse_approve("approve abc-123"), None);
        assert_eq!(parse_approve("approve abc-123 once extra"), None);
    }

    #[test]
    fn parse_approve_ignores_non_approve_messages() {
        assert_eq!(parse_approve(""), None);
        assert_eq!(parse_approve("hello there"), None);
        assert_eq!(parse_approve("approved abc-123 once"), None);
        assert_eq!(parse_approve("approval abc-123 once"), None);
        assert_eq!(parse_approve("approveabc-123 once"), None);
        assert_eq!(parse_approve("please approve abc-123 once"), None);
    }
}
