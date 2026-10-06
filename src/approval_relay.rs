//! Global relay that lets remote surfaces (WebSocket gateway, Telegram)
//! answer prompt-mode approval requests opened by the TUI's approval dance.
//! The runtime opens a relay slot when it asks for approval; gateway /
//! Telegram answer with `respond`. First answer wins; the slot is consumed.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use tokio::sync::oneshot;
use uuid::Uuid;

/// A remote decision on a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalVote {
    Once,
    Always,
    Deny,
}

impl ApprovalVote {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "once" => Some(Self::Once),
            "always" => Some(Self::Always),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Deny => "deny",
        }
    }
}

/// Broadcast to remote surfaces when the agent needs a decision.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayRequest {
    pub id: String,
    pub tool: String,
    pub detail: String,
}

static PENDING: LazyLock<Mutex<HashMap<String, oneshot::Sender<ApprovalVote>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn relay_dir() -> PathBuf {
    crate::config::Config::home_dir()
        .join("control")
        .join("approvals")
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn relay_path(id: &str) -> Option<PathBuf> {
    safe_id(id).then(|| relay_dir().join(format!("{id}.json")))
}

pub fn publish_remote(request: &RelayRequest) {
    let Some(path) = relay_path(&request.id) else {
        return;
    };
    if fs::create_dir_all(relay_dir()).is_err() {
        return;
    }
    let tmp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let payload = serde_json::json!({ "request": request, "created_at": chrono::Utc::now() });
    if let Ok(raw) = serde_json::to_vec(&payload) {
        if fs::write(&tmp, raw).is_ok() {
            let _ = fs::rename(tmp, path);
        }
    }
}

fn remove_remote_request(id: &str) {
    if let Some(path) = relay_path(id) {
        let _ = fs::remove_file(path);
    }
}

pub fn clear_remote(id: &str) {
    remove_remote_request(id);
    if let Some(path) = relay_path(id) {
        let _ = fs::remove_file(path.with_extension("response.json"));
    }
}

/// Reads a pending approval created by another Varynth process and consumes
/// it atomically. The response file is written by gateway/Telegram and is
/// intentionally one-shot.
pub fn poll_remote_response(id: &str) -> Option<ApprovalVote> {
    let path = relay_path(id)?;
    let response = path.with_extension("response.json");
    let raw = fs::read(&response).ok()?;
    let vote = serde_json::from_slice::<ApprovalVote>(&raw).ok()?;
    let _ = fs::remove_file(&response);
    Some(vote)
}

/// Submit a vote to a request owned by another process. Unknown/expired ids
/// are rejected and no file is created.
pub fn respond_remote(id: &str, vote: ApprovalVote) -> bool {
    let Some(path) = relay_path(id) else {
        return false;
    };
    if !path.exists() {
        return false;
    }
    let Ok(_lock) = crate::control_bus::FileLock::acquire(path.with_extension("lock")) else {
        return false;
    };
    let Ok(raw) = fs::read(&path) else {
        return false;
    };
    let Ok(pending) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    let valid_time = pending
        .get("created_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|at| chrono::Utc::now().signed_duration_since(at).num_seconds() < 125);
    if !valid_time {
        remove_remote_request(id);
        return false;
    }
    let response = path.with_extension("response.json");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(response)
    else {
        return false;
    };
    use std::io::Write;
    serde_json::to_vec(&vote).is_ok_and(|raw| file.write_all(&raw).is_ok())
}

/// Open a relay slot for one approval decision. Returns the request to
/// broadcast and the receiver the runtime awaits.
pub fn open(tool: &str, detail: &str) -> (RelayRequest, oneshot::Receiver<ApprovalVote>) {
    let id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    if let Ok(mut map) = PENDING.lock() {
        map.insert(id.clone(), tx);
    }
    let request = RelayRequest {
        id,
        tool: tool.to_string(),
        detail: detail.to_string(),
    };
    (request, rx)
}

/// Deliver a remote decision. True when a pending slot was found and fed.
pub fn respond(id: &str, vote: ApprovalVote) -> bool {
    let sender = {
        let Ok(mut map) = PENDING.lock() else {
            return false;
        };
        map.remove(id)
    };
    match sender {
        Some(tx) => {
            remove_remote_request(id);
            tx.send(vote).is_ok()
        }
        None => respond_remote(id, vote),
    }
}

/// How many slots are still waiting (used by tests and status pages).
pub fn pending_count() -> usize {
    PENDING.lock().map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn respond_feeds_the_open_slot_once() {
        let (req, rx) = open("write_file", "src/x.rs");
        assert_eq!(req.tool, "write_file");
        assert!(respond(&req.id, ApprovalVote::Once));
        assert!(!respond(&req.id, ApprovalVote::Deny), "slot consumed");
        assert_eq!(rx.await.unwrap(), ApprovalVote::Once);
    }

    #[test]
    fn unknown_ids_are_rejected() {
        assert!(!respond("no-such-id", ApprovalVote::Deny));
        assert_eq!(ApprovalVote::parse("always"), Some(ApprovalVote::Always));
        assert_eq!(ApprovalVote::parse("nope"), None);
    }

    #[tokio::test]
    async fn dropped_receiver_still_consumes_the_slot() {
        let before = pending_count();
        let (req, rx) = open("bash", "git status");
        drop(rx);
        // The slot is removed even though the receiver is gone.
        assert!(!respond(&req.id, ApprovalVote::Deny));
        assert_eq!(pending_count(), before);
    }
}
