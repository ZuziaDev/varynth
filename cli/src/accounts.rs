//! One stored credential per official account.
//!
//! `varynth account connect` replaces that single slot. It does not rotate
//! accounts, and it does not talk to private Antigravity or Cloud Code APIs.
//! Model accounts become provider profiles whose secrets stay in the OS
//! keyring. GitHub and GitLab are plugin tokens only.

use anyhow::{bail, ensure, Result};

use crate::config::{valid_profile_id, validate_provider_url, Config, ProviderProfile};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelLink {
    pub kind: &'static str,
    /// `https` origin. `{account_id}` is replaced for Cloudflare Workers AI.
    pub base_url: &'static str,
    pub models: &'static [&'static str],
    pub usage: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    pub label: &'static str,
    pub keyring_field: &'static str,
    pub model: Option<ModelLink>,
    pub note: &'static str,
}

const OPENAI: ModelLink = ModelLink {
    kind: "openai",
    base_url: "https://api.openai.com/v1",
    models: &[],
    usage: "openai/<model>",
};

const CLAUDE: ModelLink = ModelLink {
    kind: "anthropic",
    base_url: "https://api.anthropic.com",
    models: &[],
    usage: "claude/<model>",
};

const OPENROUTER: ModelLink = ModelLink {
    kind: "openai",
    base_url: "https://openrouter.ai/api/v1",
    models: &[],
    usage: "openrouter/<model>",
};

const VERCEL: ModelLink = ModelLink {
    kind: "openai",
    base_url: "https://ai-gateway.vercel.sh/v1",
    models: &[],
    usage: "vercel/<provider>/<model>",
};

const OPENCODE: ModelLink = ModelLink {
    kind: "openai",
    base_url: "https://opencode.ai/inference/v1",
    models: &[
        "deepseek-v4-pro",
        "glm-5.2",
        "kimi-k2.6",
        "qwen3.8-max",
        "minimax-m3",
    ],
    usage: "opencode/<model>",
};

const CLOUDFLARE: ModelLink = ModelLink {
    kind: "openai",
    base_url: "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1",
    models: &["@cf/meta/llama-3.1-8b-instruct"],
    usage: "cloudflare/<model>",
};

pub const ACCOUNTS: &[Account] = &[
    Account {
        id: "openai",
        aliases: &[],
        label: "OpenAI",
        keyring_field: "openai-api-key",
        model: Some(OPENAI),
        note: "official OpenAI API, one key",
    },
    Account {
        id: "claude",
        aliases: &["anthropic"],
        label: "Claude",
        keyring_field: "anthropic-api-key",
        model: Some(CLAUDE),
        note: "official Anthropic API, one key",
    },
    Account {
        id: "openrouter",
        aliases: &[],
        label: "OpenRouter",
        keyring_field: "openrouter-api-key",
        model: Some(OPENROUTER),
        note: "official OpenRouter API, one key",
    },
    Account {
        id: "vercel",
        aliases: &[],
        label: "Vercel",
        keyring_field: "vercel-api-key",
        model: Some(VERCEL),
        note: "Vercel AI Gateway, one key",
    },
    Account {
        id: "opencode",
        aliases: &[],
        label: "OpenCode",
        keyring_field: "opencode-api-key",
        model: Some(OPENCODE),
        note: "OpenCode Console inference, chat-completions models",
    },
    Account {
        id: "cloudflare",
        aliases: &["workers", "cloudflare-workers"],
        label: "Cloudflare",
        keyring_field: "cloudflare-api-token",
        model: Some(CLOUDFLARE),
        note: "one token for Cloudflare and Workers AI",
    },
    Account {
        id: "github",
        aliases: &[],
        label: "GitHub",
        keyring_field: "github-token",
        model: None,
        note: "plugin token only; GitHub Models was retired",
    },
    Account {
        id: "gitlab",
        aliases: &[],
        label: "GitLab",
        keyring_field: "gitlab-token",
        model: None,
        note: "plugin token only",
    },
];

pub fn find(name: &str) -> Result<&'static Account> {
    let needle = name.trim().to_ascii_lowercase();
    ACCOUNTS
        .iter()
        .find(|account| account.id == needle || account.aliases.contains(&needle.as_str()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown account `{name}` (expected {})",
                ACCOUNTS
                    .iter()
                    .map(|account| account.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

pub fn keyring_field_for_env(name: &str) -> Option<&'static str> {
    match name {
        "GITHUB_PERSONAL_ACCESS_TOKEN" | "VARYNTH_GITHUB_TOKEN" => Some("github-token"),
        "GITLAB_PERSONAL_ACCESS_TOKEN" => Some("gitlab-token"),
        _ => None,
    }
}

/// Inserts or replaces the single profile for a model account.
/// Plugin-only accounts do not create a profile.
pub fn link_config(cfg: &mut Config, account: &Account, account_id: Option<&str>) -> Result<()> {
    let Some(model) = account.model else {
        ensure!(
            account_id.is_none(),
            "{} does not take an account id",
            account.id
        );
        return Ok(());
    };
    let base_url = resolve_base(cfg, account, model, account_id)?;
    validate_provider_url(&base_url)?;
    let profile = ProviderProfile {
        id: account.id.into(),
        kind: model.kind.into(),
        name: Some(account.label.into()),
        base_url: Some(base_url),
        api_key: None,
        api_key_env: None,
        keyring_field: Some(account.keyring_field.into()),
        models: model
            .models
            .iter()
            .map(|model| (*model).to_string())
            .collect(),
        enabled: true,
        auth: "api-key".into(),
        project: None,
        location: None,
    };
    if let Some(index) = cfg
        .provider_profiles
        .iter()
        .position(|item| item.id == account.id)
    {
        cfg.provider_profiles[index] = profile;
    } else {
        ensure!(
            cfg.provider_profiles.len() < 64,
            "provider profile limit reached"
        );
        cfg.provider_profiles.push(profile);
    }
    Ok(())
}

pub fn unlink_config(cfg: &mut Config, account: &Account) {
    cfg.provider_profiles
        .retain(|profile| profile.id != account.id);
    if cfg.provider == account.id {
        cfg.provider = "proxy".into();
    }
    if cfg
        .model
        .split_once('/')
        .is_some_and(|(id, _)| id == account.id)
    {
        cfg.model = "grok-4.7-build".into();
    }
}

/// One OpenAI-compatible or Anthropic-compatible endpoint. The secret stays
/// in the per-profile keyring slot and is never written to config.
pub fn link_custom(cfg: &mut Config, id: &str, kind: &str, base_url: &str) -> Result<()> {
    let id = id.trim();
    ensure!(valid_profile_id(id), "geçersiz profil adı");
    ensure!(
        !id.eq_ignore_ascii_case("antigravity"),
        "unsupported account `{id}`"
    );
    ensure!(
        matches!(kind, "openai" | "anthropic"),
        "kind openai veya anthropic olmalı"
    );
    let base_url = base_url.trim().trim_end_matches('/').to_string();
    validate_provider_url(&base_url)?;
    let profile = ProviderProfile {
        id: id.to_string(),
        kind: kind.to_string(),
        name: Some(format!("Özel {kind}")),
        base_url: Some(base_url),
        api_key: None,
        api_key_env: None,
        keyring_field: None,
        models: Vec::new(),
        enabled: true,
        auth: "api-key".into(),
        project: None,
        location: None,
    };
    if let Some(index) = cfg.provider_profiles.iter().position(|item| item.id == id) {
        cfg.provider_profiles[index] = profile;
    } else {
        ensure!(
            cfg.provider_profiles.len() < 64,
            "provider profile limit reached"
        );
        cfg.provider_profiles.push(profile);
    }
    Ok(())
}

fn resolve_base(
    cfg: &Config,
    account: &Account,
    model: ModelLink,
    account_id: Option<&str>,
) -> Result<String> {
    if !model.base_url.contains("{account_id}") {
        ensure!(
            account_id.is_none(),
            "{} does not take an account id",
            account.id
        );
        return Ok(model.base_url.to_string());
    }
    let id = match account_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => existing_account_id(cfg, account.id)
            .ok_or_else(|| anyhow::anyhow!("cloudflare requires --account-id <32 hex>"))?,
    };
    ensure!(
        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "cloudflare account id must be 32 hex characters"
    );
    Ok(model
        .base_url
        .replace("{account_id}", &id.to_ascii_lowercase()))
}

fn existing_account_id(cfg: &Config, id: &str) -> Option<String> {
    let base = cfg
        .provider_profiles
        .iter()
        .find(|profile| profile.id == id)?
        .base_url
        .as_deref()?;
    let marker = "/accounts/";
    let start = base.find(marker)? + marker.len();
    let rest = &base[start..];
    let end = rest.find('/')?;
    let id = &rest[..end];
    (id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| id.to_string())
}

pub fn usage_label(account: &Account) -> String {
    account
        .model
        .map(|model| model.usage.to_string())
        .unwrap_or_else(|| account.note.to_string())
}

pub fn reject_unknown_provider_name(name: &str) -> Result<&'static Account> {
    if name.eq_ignore_ascii_case("antigravity") {
        bail!("unsupported account `{name}`");
    }
    find(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_share_one_cloudflare_account() {
        let from_workers = find("workers").unwrap();
        let from_cf = find("cloudflare-workers").unwrap();
        assert_eq!(from_workers.id, "cloudflare");
        assert_eq!(from_cf.id, from_workers.id);
        assert_eq!(from_workers.keyring_field, "cloudflare-api-token");
    }

    #[test]
    fn connect_replaces_the_single_profile_and_stores_no_secret() {
        let mut cfg = Config::default();
        let account = find("openai").unwrap();
        link_config(&mut cfg, account, None).unwrap();
        link_config(&mut cfg, account, None).unwrap();
        assert_eq!(cfg.provider_profiles.len(), 1);
        let profile = &cfg.provider_profiles[0];
        assert_eq!(profile.id, "openai");
        assert_eq!(profile.base_url.as_deref(), Some(OPENAI.base_url));
        assert!(profile.api_key.is_none());
        assert_eq!(profile.keyring_field.as_deref(), Some("openai-api-key"));
        cfg.validate().unwrap();
    }

    #[test]
    fn official_model_bases_are_fixed() {
        let mut cfg = Config::default();
        for (name, base) in [
            ("claude", CLAUDE.base_url),
            ("openrouter", OPENROUTER.base_url),
            ("vercel", VERCEL.base_url),
            ("opencode", OPENCODE.base_url),
        ] {
            let account = find(name).unwrap();
            link_config(&mut cfg, account, None).unwrap();
            let profile = cfg
                .provider_profiles
                .iter()
                .find(|profile| profile.id == account.id)
                .unwrap();
            assert_eq!(profile.base_url.as_deref(), Some(base));
            assert!(profile.api_key.is_none());
        }
        cfg.validate().unwrap();
    }

    #[test]
    fn cloudflare_workers_ai_url_uses_one_account_id() {
        let mut cfg = Config::default();
        let account = find("workers").unwrap();
        let err = link_config(&mut cfg, account, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--account-id"), "{err}");
        link_config(&mut cfg, account, Some("ABCDEF0123456789ABCDEF0123456789")).unwrap();
        link_config(&mut cfg, find("cloudflare").unwrap(), None).unwrap();
        assert_eq!(cfg.provider_profiles.len(), 1);
        assert_eq!(
            cfg.provider_profiles[0].base_url.as_deref(),
            Some(
                "https://api.cloudflare.com/client/v4/accounts/abcdef0123456789abcdef0123456789/ai/v1"
            )
        );
        assert!(link_config(&mut cfg, account, Some("nope")).is_err());
    }

    #[test]
    fn github_and_gitlab_do_not_create_model_profiles() {
        let mut cfg = Config::default();
        link_config(&mut cfg, find("github").unwrap(), None).unwrap();
        link_config(&mut cfg, find("gitlab").unwrap(), None).unwrap();
        assert!(cfg.provider_profiles.is_empty());
        assert!(find("github").unwrap().model.is_none());
        assert!(link_config(&mut cfg, find("gitlab").unwrap(), Some("x")).is_err());
    }

    #[test]
    fn antigravity_is_not_an_account() {
        let err = reject_unknown_provider_name("antigravity")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported"), "{err}");
        assert!(find("nope").is_err());
    }

    #[test]
    fn plugin_env_names_map_to_the_single_token_slot() {
        assert_eq!(
            keyring_field_for_env("GITHUB_PERSONAL_ACCESS_TOKEN"),
            Some("github-token")
        );
        assert_eq!(
            keyring_field_for_env("VARYNTH_GITHUB_TOKEN"),
            Some("github-token")
        );
        assert_eq!(
            keyring_field_for_env("GITLAB_PERSONAL_ACCESS_TOKEN"),
            Some("gitlab-token")
        );
        assert_eq!(keyring_field_for_env("SLACK_BOT_TOKEN"), None);
    }

    #[test]
    fn unlink_drops_the_profile_and_resets_a_selected_provider() {
        let mut cfg = Config::default();
        let account = find("vercel").unwrap();
        link_config(&mut cfg, account, None).unwrap();
        cfg.provider = "vercel".into();
        cfg.model = "vercel/openai/gpt-5".into();
        unlink_config(&mut cfg, account);
        assert!(cfg.provider_profiles.is_empty());
        assert_eq!(cfg.provider, "proxy");
        assert_eq!(cfg.model, "grok-4.7-build");
        cfg.validate().unwrap();
    }

    #[test]
    fn custom_endpoint_is_one_profile_without_a_plaintext_key() {
        let mut cfg = Config::default();
        link_custom(&mut cfg, "ozel", "anthropic", "https://example.com/v1/").unwrap();
        link_custom(&mut cfg, "ozel", "openai", "https://example.com/v1").unwrap();
        assert_eq!(cfg.provider_profiles.len(), 1);
        assert_eq!(cfg.provider_profiles[0].kind, "openai");
        assert!(cfg.provider_profiles[0].api_key.is_none());
        assert!(link_custom(&mut cfg, "antigravity", "openai", "https://example.com").is_err());
        assert!(link_custom(&mut cfg, "ozel", "google", "https://example.com").is_err());
        cfg.validate().unwrap();
    }
}
