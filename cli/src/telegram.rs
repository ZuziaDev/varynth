use anyhow::Result;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::approval_relay::{self, ApprovalVote};
use crate::config::Config;
use crate::dashboard::AppState;
use crate::session::ImageAttachment;

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

pub fn parse_callback_data(data: &str) -> Option<(String, ApprovalVote)> {
    let parts: Vec<&str> = data.split(':').collect();
    if parts.len() != 3 || !parts[0].eq_ignore_ascii_case("approve") {
        return None;
    }
    let id = parts[1];
    let vote = ApprovalVote::parse(parts[2])?;
    Some((id.to_string(), vote))
}

async fn download_telegram_file(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    file_id: &str,
) -> Result<(String, Vec<u8>)> {
    let get_file_url = format!("{base}/getFile?file_id={file_id}");
    let resp = client.get(&get_file_url).send().await?;
    let json: Value = resp.json().await?;
    let file_path = json
        .pointer("/result/file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing file_path in getFile"))?
        .to_string();
    let download_url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
    let file_bytes = client.get(&download_url).send().await?.bytes().await?.to_vec();
    Ok((file_path, file_bytes))
}

pub async fn send_approval_prompt(
    client: &reqwest::Client,
    base: &str,
    chat_id: i64,
    req: &crate::approval_relay::RelayRequest,
) -> Result<()> {
    let text = format!(
        "⚠️ *Approval Required*\n\n\
         • *Tool*: `{}`\n\
         • *Detail*: `{}`\n\
         • *ID*: `{}`\n\n\
         Select an action below (expires in 120s):",
        req.tool, req.detail, req.id
    );
    let markup = json!({
        "inline_keyboard": [
            [
                { "text": "✅ Approve Once", "callback_data": format!("approve:{}:once", req.id) },
                { "text": "🟢 Always Approve", "callback_data": format!("approve:{}:always", req.id) },
                { "text": "❌ Deny", "callback_data": format!("approve:{}:deny", req.id) }
            ]
        ]
    });
    let body = json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "Markdown",
        "reply_markup": markup
    });
    let _ = client.post(format!("{base}/sendMessage")).json(&body).send().await;
    Ok(())
}

async fn answer_callback(
    client: &reqwest::Client,
    base: &str,
    callback_query_id: &str,
    text: &str,
    show_alert: bool,
) -> Result<()> {
    let body = json!({
        "callback_query_id": callback_query_id,
        "text": text,
        "show_alert": show_alert
    });
    let _ = client
        .post(format!("{base}/answerCallbackQuery"))
        .json(&body)
        .send()
        .await;
    Ok(())
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

    let initial_chat: i64 = allow
        .first()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let latest_chat_id = Arc::new(AtomicI64::new(initial_chat));

    // Listen for background approval requests and broadcast inline keyboard prompts.
    if let Some(relay_tx) = &state.relay_events {
        let mut rx = relay_tx.subscribe();
        let client_relay = client.clone();
        let base_relay = base.clone();
        let latest_chat_relay = latest_chat_id.clone();
        tokio::spawn(async move {
            while let Ok(req) = rx.recv().await {
                let cid = latest_chat_relay.load(Ordering::Relaxed);
                if cid != 0 {
                    let _ = send_approval_prompt(&client_relay, &base_relay, cid, &req).await;
                }
            }
        });
    }

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

            // Handle inline button callbacks (approvals).
            if let Some(cq) = upd.get("callback_query") {
                let cq_id = cq.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let sender = cq
                    .pointer("/from/id")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                let sid = sender.to_string();
                if !allow.is_empty() && !allow.iter().any(|a| a == &sid) {
                    let _ = answer_callback(&client, &base, cq_id, "Unauthorized sender.", true).await;
                    continue;
                }
                let data = cq.get("data").and_then(|x| x.as_str()).unwrap_or("");
                if let Some((id, vote)) = parse_callback_data(data) {
                    if approval_relay::is_expired(&id) {
                        let _ = answer_callback(&client, &base, cq_id, "Approval request expired.", true).await;
                    } else if approval_relay::respond(&id, vote) {
                        let outcome = match vote {
                            ApprovalVote::Once => "Approved (once).",
                            ApprovalVote::Always => "Approved (always).",
                            ApprovalVote::Deny => "Denied.",
                        };
                        let _ = answer_callback(&client, &base, cq_id, outcome, false).await;
                    } else {
                        let _ = answer_callback(
                            &client,
                            &base,
                            cq_id,
                            "No pending approval or already answered.",
                            false,
                        )
                        .await;
                    }
                } else {
                    let _ = answer_callback(&client, &base, cq_id, "Unknown callback action.", false).await;
                }
                continue;
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

            // Remember latest active chat id for proactive approval notifications.
            latest_chat_id.store(chat_id, Ordering::Relaxed);

            let mut body = msg
                .get("text")
                .and_then(|t| t.as_str())
                .or_else(|| msg.get("caption").and_then(|t| t.as_str()))
                .unwrap_or("")
                .trim()
                .to_string();

            let mut images: Vec<ImageAttachment> = Vec::new();

            // Handle photos: pick the highest resolution.
            if let Some(photos) = msg.get("photo").and_then(|p| p.as_array()) {
                if let Some(highest) = photos.last() {
                    if let Some(file_id) = highest.get("file_id").and_then(|f| f.as_str()) {
                        if let Ok((file_path, bytes)) =
                            download_telegram_file(&client, &base, &token, file_id).await
                        {
                            let media_type = if file_path.ends_with(".png") {
                                "image/png"
                            } else if file_path.ends_with(".webp") {
                                "image/webp"
                            } else if file_path.ends_with(".gif") {
                                "image/gif"
                            } else {
                                "image/jpeg"
                            };
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            images.push(ImageAttachment {
                                media_type: media_type.to_string(),
                                data: b64,
                            });
                        }
                    }
                }
            }

            // Handle documents: download to <cwd>/.varynth/uploads/<file_name>.
            if let Some(doc) = msg.get("document") {
                if let Some(file_id) = doc.get("file_id").and_then(|f| f.as_str()) {
                    let raw_name = doc
                        .get("file_name")
                        .and_then(|f| f.as_str())
                        .unwrap_or("document.bin");
                    let safe_name = std::path::Path::new(raw_name)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("document.bin");
                    if let Ok((_, bytes)) =
                        download_telegram_file(&client, &base, &token, file_id).await
                    {
                        let uploads_dir = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            .join(".varynth")
                            .join("uploads");
                        let _ = std::fs::create_dir_all(&uploads_dir);
                        let file_dest = uploads_dir.join(safe_name);
                        if std::fs::write(&file_dest, &bytes).is_ok() {
                            let tag = format!("[Uploaded document: .varynth/uploads/{safe_name}]");
                            if body.is_empty() {
                                body = tag;
                            } else {
                                body.push_str(&format!("\n\n{tag}"));
                            }
                        }
                    }
                }
            }

            if body.is_empty() && !images.is_empty() {
                body = "Here is an image.".to_string();
            }

            if body.is_empty() {
                continue;
            }

            if too_long(&body) {
                let _ = send(&client, &base, chat_id, "message too long; split it up").await;
                continue;
            }

            // Slash command handling.
            let first_word = body.split_whitespace().next().unwrap_or("");
            if first_word.eq_ignore_ascii_case("/help") {
                let help_text = "🤖 *Varynth Telegram Gateway*\n\n\
                    • /help — Show available commands\n\
                    • /status — Show runtime status, active model, and approvals\n\
                    • /model [name] — View or switch active LLM model\n\
                    • /todo — Check goal progress and todo state\n\
                    • /undo — Revert recent changes\n\
                    • /redo — Re-apply reverted changes\n\
                    • /clear — Clear conversation and start fresh session\n\
                    • /revoke [pattern] — Revoke always-approved permissions\n\
                    • `approve <id> once|always|deny` — Approve or deny tool execution";
                let _ = send(&client, &base, chat_id, help_text).await;
                continue;
            }

            if first_word.eq_ignore_ascii_case("/status") {
                let status = state.status_text().await;
                let _ = send(&client, &base, chat_id, &status).await;
                continue;
            }

            if first_word.eq_ignore_ascii_case("/model") {
                let arg = body.strip_prefix("/model").unwrap_or("").trim();
                let reply = state
                    .switch_model(arg)
                    .await
                    .unwrap_or_else(|e| format!("Error switching model: {e}"));
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }

            if first_word.eq_ignore_ascii_case("/clear") {
                let reply = match state.new_session().await {
                    Ok(id) => format!("Cleared conversation. Started fresh session `{id}`."),
                    Err(e) => format!("Failed to clear session: {e}"),
                };
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }

            if first_word.eq_ignore_ascii_case("/todo") {
                let reply = state
                    .turn("/goal status")
                    .await
                    .unwrap_or_else(|e| format!("Error: {e}"));
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }

            if first_word.eq_ignore_ascii_case("/undo") {
                let reply = state
                    .turn("/undo")
                    .await
                    .unwrap_or_else(|e| format!("Error: {e}"));
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }

            if first_word.eq_ignore_ascii_case("/redo") {
                let reply = state
                    .turn("/redo")
                    .await
                    .unwrap_or_else(|e| format!("Error: {e}"));
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }

            if first_word.eq_ignore_ascii_case("/revoke") {
                let arg = body.strip_prefix("/revoke").unwrap_or("").trim();
                let pattern = if arg.is_empty() { None } else { Some(arg) };
                let revoked = crate::approval_relay::revoke_always(pattern);
                let reply = if revoked.is_empty() {
                    "No always-approved rules to revoke.".to_string()
                } else {
                    format!("Revoked always-approved rule(s):\n• {}", revoked.join("\n• "))
                };
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }

            // Fallback text-based approval command.
            if first_word.eq_ignore_ascii_case("approve") {
                let reply = match parse_approve(&body) {
                    Some((id, vote)) => {
                        if approval_relay::is_expired(&id) {
                            format!("approval {id} has expired")
                        } else if approval_relay::respond(&id, vote) {
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
                    None => "usage: approve <id> once|always|deny".to_string(),
                };
                let _ = send(&client, &base, chat_id, &reply).await;
                continue;
            }

            // Regular message turn with live typing heartbeat.
            let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
            let client_hb = client.clone();
            let base_hb = base.clone();
            let cid_hb = chat_id;
            let hb_task = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(4));
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        _ = ticker.tick() => {
                            let action_body = json!({ "chat_id": cid_hb, "action": "typing" });
                            let _ = client_hb.post(format!("{base_hb}/sendChatAction")).json(&action_body).send().await;
                        }
                    }
                }
            });

            let turn_result = state.turn_with_media(&body, images).await;
            let _ = stop_tx.send(());
            let _ = hb_task.await;

            match turn_result {
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
    use super::{chunk_text, clip_error, parse_approve, parse_callback_data, too_long};
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

    #[test]
    fn parse_callback_data_works() {
        assert_eq!(
            parse_callback_data("approve:abc-123:once"),
            Some(("abc-123".to_string(), ApprovalVote::Once))
        );
        assert_eq!(
            parse_callback_data("approve:abc-123:always"),
            Some(("abc-123".to_string(), ApprovalVote::Always))
        );
        assert_eq!(
            parse_callback_data("approve:abc-123:deny"),
            Some(("abc-123".to_string(), ApprovalVote::Deny))
        );
        assert_eq!(parse_callback_data("invalid:callback"), None);
        assert_eq!(parse_callback_data("approve:abc-123:unknown"), None);
    }
}
