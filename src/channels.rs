//! Channel adapters (Telegram / Discord / WhatsApp) land in v2.
//! v1 exposes the trait and a dashboard-as-channel so the web UI
//! already speaks the same inbound/outbound contract.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inbound {
    pub channel: String,
    pub sender_id: String,
    pub chat_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outbound {
    pub chat_id: String,
    pub text: String,
}

pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
}

pub struct DashboardChannel;

impl Channel for DashboardChannel {
    fn name(&self) -> &'static str {
        "dashboard"
    }
}

pub fn v2_status() -> serde_json::Value {
    let cfg = crate::config::Config::load().ok();
    let tg = match &cfg {
        Some(c)
            if c.telegram_bot_token
                .as_ref()
                .map(|t| !t.is_empty())
                .unwrap_or(false) =>
        {
            if c.telegram_allow_from.is_empty() {
                "token set, pairing (empty allowlist)"
            } else {
                "live"
            }
        }
        _ => "off — set VARYNTH_TELEGRAM_BOT_TOKEN or ~/.varynth/telegram.env",
    };
    serde_json::json!({
        "telegram": tg,
        "discord": "planned",
        "whatsapp": "planned",
        "dashboard": "live",
        "note": "Telegram polls when `varynth serve` runs. Use a dedicated bot token — do not share a token with another bot or app (409 Conflict). Allowlist: telegram_allow_from in ~/.varynth/config.toml"
    })
}
