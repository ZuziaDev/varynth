use anyhow::{ensure, Result};
use serde::Serialize;
use std::path::Path;

use crate::config::{Config, ProviderProfile};
use crate::mcp::{McpServerStatus, McpStatus};
use crate::plugins::{self, PluginId};
use crate::providers::CatalogModel;
use crate::skills_search::SkillEntry;

const MAX_ROWS: usize = 256;

pub(crate) fn media_type(raw: &str) -> Option<&'static str> {
    match raw.split(';').next()?.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        "image/gif" => Some("image/gif"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProviderProfileRow {
    id: String,
    name: Option<String>,
    kind: String,
    enabled: bool,
    auth: String,
    models: Vec<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    project: Option<String>,
    location: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProviderRow {
    name: String,
    model: String,
    status: &'static str,
    profile: Option<ProviderProfileRow>,
    profiles: Vec<ProviderProfileRow>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModelRow {
    id: String,
    display_name: Option<String>,
    owned_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct McpRow {
    name: String,
    transport: String,
    target: String,
    tool_count: Option<usize>,
    status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SkillRow {
    name: String,
    description: String,
    source: String,
    installs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PluginRow {
    id: &'static str,
    name: &'static str,
    hint: &'static str,
    installed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StudioResponse {
    ok: bool,
    provider: ProviderRow,
    models: Vec<ModelRow>,
    mcp: Vec<McpRow>,
    skills: Vec<SkillRow>,
    plugins: Vec<PluginRow>,
}

fn bounded(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn safe_url(raw: &str) -> Option<String> {
    crate::config::validate_provider_url(raw).ok()?;
    Some(bounded(raw, 2048))
}

fn profile_row(profile: &ProviderProfile) -> ProviderProfileRow {
    ProviderProfileRow {
        id: bounded(&profile.id, 64),
        name: profile.name.as_deref().map(|v| bounded(v, 128)),
        kind: bounded(&profile.kind, 32),
        enabled: profile.enabled,
        auth: bounded(&profile.auth, 32),
        models: profile
            .models
            .iter()
            .take(16)
            .map(|v| bounded(v, 512))
            .collect(),
        base_url: profile.base_url.as_deref().and_then(safe_url),
        api_key_env: profile.api_key_env.as_deref().map(|v| bounded(v, 128)),
        project: profile.project.as_deref().map(|v| bounded(v, 128)),
        location: profile.location.as_deref().map(|v| bounded(v, 128)),
    }
}

pub(crate) fn provider_row(cfg: &Config) -> ProviderRow {
    let profile = cfg
        .selected_profile_id()
        .and_then(|id| cfg.provider_profiles.iter().find(|p| p.id == id));
    ProviderRow {
        name: profile.map_or_else(|| bounded(&cfg.provider, 64), |p| bounded(&p.kind, 32)),
        model: bounded(&cfg.model, 512),
        status: if cfg.validate().is_ok() {
            "configured"
        } else {
            "invalid"
        },
        profile: profile.map(profile_row),
        profiles: cfg
            .provider_profiles
            .iter()
            .take(MAX_ROWS)
            .map(profile_row)
            .collect(),
    }
}

pub(crate) fn mcp_rows(statuses: Vec<McpServerStatus>) -> Vec<McpRow> {
    statuses
        .into_iter()
        .take(MAX_ROWS)
        .map(|status| McpRow {
            name: bounded(&status.name, 128),
            transport: bounded(&status.transport, 32),
            target: bounded(&status.target, 512),
            tool_count: status.tool_count,
            status: match status.status {
                McpStatus::Unknown => "unknown",
                McpStatus::Ok => "ok",
                McpStatus::Error => "error",
            },
        })
        .collect()
}

fn plugin_id(id: PluginId) -> &'static str {
    match id {
        PluginId::Github => "github",
        PluginId::Gitlab => "gitlab",
        PluginId::Slack => "slack",
        PluginId::Discord => "discord",
        PluginId::Notion => "notion",
        PluginId::Spotify => "spotify",
        PluginId::Google => "google",
    }
}

fn plugin_installed(cwd: &Path, id: &str) -> bool {
    let path = cwd.join(".varynth/plugins").join(format!("{id}.json"));
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > 64 * 1024 {
        return false;
    }
    let Ok(root) = cwd.canonicalize() else {
        return false;
    };
    let Ok(actual) = path.canonicalize() else {
        return false;
    };
    if !actual.starts_with(root) {
        return false;
    }
    std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .is_some_and(|value| value["id"].as_str() == Some(id))
}

pub(crate) fn studio_response(
    cfg: &Config,
    models: &[CatalogModel],
    mcp: Vec<McpServerStatus>,
    skills: Vec<SkillEntry>,
    cwd: &Path,
) -> StudioResponse {
    StudioResponse {
        ok: true,
        provider: provider_row(cfg),
        models: models
            .iter()
            .take(MAX_ROWS)
            .map(|m| ModelRow {
                id: bounded(&m.id, 512),
                display_name: m.display_name.as_deref().map(|v| bounded(v, 256)),
                owned_by: m.owned_by.as_deref().map(|v| bounded(v, 128)),
            })
            .collect(),
        mcp: mcp_rows(mcp),
        skills: skills
            .into_iter()
            .take(MAX_ROWS)
            .map(|s| SkillRow {
                name: bounded(&s.name, 128),
                description: bounded(&s.description, 1024),
                source: bounded(&s.source, 512),
                installs: s.installs,
            })
            .collect(),
        plugins: plugins::catalog()
            .iter()
            .map(|p| PluginRow {
                id: plugin_id(p.id),
                name: p.name,
                hint: p.hint,
                installed: plugin_installed(cwd, plugin_id(p.id)),
            })
            .collect(),
    }
}

pub(crate) fn validate_identifier(raw: &str, label: &str) -> Result<String> {
    let value = raw.trim();
    ensure!(!value.is_empty() && value.len() <= 128, "invalid {label}");
    ensure!(
        value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "invalid {label}"
    );
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types_are_strict() {
        assert_eq!(media_type("IMAGE/PNG; charset=binary"), Some("image/png"));
        assert_eq!(media_type("image/jpeg"), Some("image/jpeg"));
        assert!(media_type("image/svg+xml").is_none());
    }

    #[test]
    fn safe_projection_excludes_credentials_and_error_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            openai_api_key: Some("do-not-return-key".into()),
            provider_profiles: vec![ProviderProfile {
                id: "work".into(),
                api_key: Some("do-not-return-profile".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let raw =
            serde_json::to_string(&studio_response(&cfg, &[], vec![], vec![], dir.path())).unwrap();
        assert!(!raw.contains("do-not-return"));
        assert!(!raw.contains("api_key\""));
    }

    #[test]
    fn plugin_status_comes_from_a_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".varynth/plugins");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!plugin_installed(dir.path(), "spotify"));
        std::fs::write(root.join("spotify.json"), r#"{"id":"spotify"}"#).unwrap();
        assert!(plugin_installed(dir.path(), "spotify"));
        std::fs::write(root.join("spotify.json"), r#"{"id":"foreign"}"#).unwrap();
        assert!(!plugin_installed(dir.path(), "spotify"));
    }
}
