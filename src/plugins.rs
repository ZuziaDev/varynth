//! Curated local integrations and MCP manifests.
//!
//! Installing a plugin is deliberately declarative: it writes a local manifest,
//! a placeholder-only `.env.example`, and (when applicable) an MCP entry. It
//! never downloads or executes a package. Secrets stay in the caller's
//! environment and are referenced by `${ENV_NAME}` placeholders.

use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::mcp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginId {
    Github,
    Gitlab,
    Slack,
    Discord,
    Notion,
    Spotify,
    /// Kept for source compatibility with older TUI integrations. Google has
    /// no curated MCP manifest yet and is not shown in the marketplace.
    Google,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plugin {
    pub id: PluginId,
    pub name: &'static str,
    /// One-line description for the right side of the select row.
    pub hint: &'static str,
    /// Env var that holds the secret. Read only to test emptiness.
    pub token_env: &'static str,
    /// Env var that holds a display name. Never a token.
    pub account_env: &'static str,
}

pub const GITHUB: Plugin = Plugin {
    id: PluginId::Github,
    name: "GitHub",
    hint: "official remote MCP",
    token_env: "GITHUB_PERSONAL_ACCESS_TOKEN",
    account_env: "VARYNTH_GITHUB_ACCOUNT",
};
pub const GITLAB: Plugin = Plugin {
    id: PluginId::Gitlab,
    name: "GitLab",
    hint: "official archived MCP server",
    token_env: "GITLAB_PERSONAL_ACCESS_TOKEN",
    account_env: "VARYNTH_GITLAB_ACCOUNT",
};
pub const SLACK: Plugin = Plugin {
    id: PluginId::Slack,
    name: "Slack",
    hint: "official archived MCP server",
    token_env: "SLACK_BOT_TOKEN",
    account_env: "VARYNTH_SLACK_ACCOUNT",
};
pub const DISCORD: Plugin = Plugin {
    id: PluginId::Discord,
    name: "Discord",
    hint: "local MCP server path",
    token_env: "DISCORD_TOKEN",
    account_env: "VARYNTH_DISCORD_ACCOUNT",
};
pub const NOTION: Plugin = Plugin {
    id: PluginId::Notion,
    name: "Notion",
    hint: "official self-hosted MCP server",
    token_env: "NOTION_TOKEN",
    account_env: "VARYNTH_NOTION_ACCOUNT",
};
pub const SPOTIFY: Plugin = Plugin {
    id: PluginId::Spotify,
    name: "Spotify",
    hint: "currently-playing Web API client",
    token_env: "SPOTIFY_ACCESS_TOKEN",
    account_env: "VARYNTH_SPOTIFY_ACCOUNT",
};
pub const GOOGLE: Plugin = Plugin {
    id: PluginId::Google,
    name: "Google",
    hint: "account link",
    token_env: "VARYNTH_GOOGLE_TOKEN",
    account_env: "VARYNTH_GOOGLE_ACCOUNT",
};

const CATALOG: &[Plugin] = &[GITHUB, GITLAB, SLACK, DISCORD, NOTION, SPOTIFY];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    Missing,
    Linked { account: String },
}

pub fn catalog() -> &'static [Plugin] {
    CATALOG
}

pub fn get(id: PluginId) -> &'static Plugin {
    match id {
        PluginId::Github => &GITHUB,
        PluginId::Gitlab => &GITLAB,
        PluginId::Slack => &SLACK,
        PluginId::Discord => &DISCORD,
        PluginId::Notion => &NOTION,
        PluginId::Spotify => &SPOTIFY,
        PluginId::Google => &GOOGLE,
    }
}

/// `token` and `account` are already-read values. Empty or whitespace is absent.
pub fn link_state(token: Option<&str>, account: Option<&str>) -> LinkState {
    if token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return LinkState::Missing;
    }
    let account = account
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("bağlı");
    LinkState::Linked {
        account: account.to_string(),
    }
}

pub fn status_label(state: &LinkState) -> &str {
    match state {
        LinkState::Missing => "token yok",
        LinkState::Linked { account } => account,
    }
}

/// Reads the two env slots for `plugin`. Does not log or return the token.
pub fn status(plugin: &Plugin) -> LinkState {
    let token = std::env::var(plugin.token_env)
        .ok()
        .or_else(|| match plugin.id {
            // Compatibility with the pre-marketplace connector name.
            PluginId::Github => std::env::var("VARYNTH_GITHUB_TOKEN").ok(),
            _ => None,
        });
    let account = std::env::var(plugin.account_env).ok();
    link_state(token.as_deref(), account.as_deref())
}

pub fn rows() -> Vec<(&'static str, &'static str, String)> {
    catalog()
        .iter()
        .map(|plugin| {
            let state = status(plugin);
            (plugin.name, plugin.hint, status_label(&state).to_string())
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct InstalledManifest {
    id: String,
    name: String,
    source: String,
    #[serde(default)]
    required_env: Vec<String>,
    #[serde(default)]
    entry: Option<Value>,
    #[serde(default)]
    owns_mcp_entry: bool,
}

#[derive(Debug, Clone)]
struct Definition {
    id: &'static str,
    plugin: Plugin,
    source: &'static str,
    required_env: &'static [&'static str],
    entry: Option<Value>,
}

fn definitions() -> Vec<Definition> {
    vec![
        Definition {
            id: "github",
            plugin: GITHUB,
            source: "https://github.com/github/github-mcp-server",
            required_env: &["GITHUB_PERSONAL_ACCESS_TOKEN"],
            entry: Some(json!({
                "type": "http",
                "url": "https://api.githubcopilot.com/mcp/",
                "headers": {
                    "Authorization": "Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}"
                }
            })),
        },
        Definition {
            id: "gitlab",
            plugin: GITLAB,
            source: "https://github.com/modelcontextprotocol/servers-archived/tree/main/src/gitlab",
            required_env: &["GITLAB_PERSONAL_ACCESS_TOKEN"],
            entry: Some(json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-gitlab"],
                "env": {
                    "GITLAB_PERSONAL_ACCESS_TOKEN": "${GITLAB_PERSONAL_ACCESS_TOKEN}"
                }
            })),
        },
        Definition {
            id: "slack",
            plugin: SLACK,
            source: "https://github.com/modelcontextprotocol/servers-archived/tree/main/src/slack",
            required_env: &["SLACK_BOT_TOKEN", "SLACK_TEAM_ID"],
            entry: Some(json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-slack"],
                "env": {
                    "SLACK_BOT_TOKEN": "${SLACK_BOT_TOKEN}",
                    "SLACK_TEAM_ID": "${SLACK_TEAM_ID}"
                }
            })),
        },
        Definition {
            id: "discord",
            plugin: DISCORD,
            source: "https://github.com/v-3/discordmcp",
            required_env: &["DISCORD_TOKEN", "VARYNTH_DISCORD_MCP_PATH"],
            entry: Some(json!({
                "type": "stdio",
                "command": "node",
                "args": ["${VARYNTH_DISCORD_MCP_PATH}"],
                "env": {
                    "DISCORD_TOKEN": "${DISCORD_TOKEN}"
                }
            })),
        },
        Definition {
            id: "notion",
            plugin: NOTION,
            source: "https://github.com/makenotion/notion-mcp-server",
            required_env: &["NOTION_TOKEN"],
            entry: Some(json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@notionhq/notion-mcp-server"],
                "env": {
                    "NOTION_TOKEN": "${NOTION_TOKEN}"
                }
            })),
        },
        Definition {
            id: "spotify",
            plugin: SPOTIFY,
            source: "https://developer.spotify.com/documentation/web-api/reference/get-the-users-currently-playing-track",
            required_env: &["SPOTIFY_ACCESS_TOKEN"],
            // Spotify's official currently-playing endpoint is an HTTP client
            // integration, not an invented MCP package.
            entry: None,
        },
    ]
}

fn definition(id: &str) -> Result<Definition> {
    let id = id.trim().to_ascii_lowercase();
    definitions()
        .into_iter()
        .find(|definition| definition.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown plugin `{id}` (available: github, gitlab, slack, discord, notion, spotify)"))
}

fn plugin_root(cwd: &Path) -> PathBuf {
    cwd.join(".varynth").join("plugins")
}

fn mcp_path(cwd: &Path) -> PathBuf {
    cwd.join(".varynth").join("mcp.json")
}

fn manifest_path(cwd: &Path, id: &str) -> PathBuf {
    plugin_root(cwd).join(format!("{id}.json"))
}

fn env_example_path(cwd: &Path, id: &str) -> PathBuf {
    plugin_root(cwd).join(format!("{id}.env.example"))
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn installed(cwd: &Path, id: &str) -> Result<Option<InstalledManifest>> {
    let path = manifest_path(cwd, id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?,
    ))
}

fn install_plugin(def: Definition, cwd: &Path) -> Result<String> {
    if installed(cwd, def.id)?.is_some() {
        anyhow::bail!("plugin `{}` is already installed", def.id);
    }
    let mcp_path = mcp_path(cwd);
    let mut owns_mcp_entry = false;
    if let Some(entry) = &def.entry {
        let existing = read_server_entry(&mcp_path, def.id)?;
        match existing {
            Some(value) if value != *entry => {
                anyhow::bail!(
                    "MCP server `{}` already exists with a different entry; refusing to replace it",
                    def.id
                );
            }
            Some(_) => {}
            None => {
                mcp::add_server_entry(&mcp_path, def.id, entry, false)?;
                owns_mcp_entry = true;
            }
        }
    }

    let manifest = InstalledManifest {
        id: def.id.to_string(),
        name: def.plugin.name.to_string(),
        source: def.source.to_string(),
        required_env: def
            .required_env
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        entry: def.entry.clone(),
        owns_mcp_entry,
    };
    let manifest_value = serde_json::to_value(&manifest)?;
    if let Err(error) = write_json_atomic(&manifest_path(cwd, def.id), &manifest_value) {
        if owns_mcp_entry {
            let _ = mcp::remove_server_entry(&mcp_path, def.id, def.entry.as_ref().unwrap());
        }
        return Err(error);
    }
    let env = def
        .required_env
        .iter()
        .map(|name| format!("{name}=<set externally>"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        env_example_path(cwd, def.id),
        format!("# Varynth plugin `{}`\n{}\n", def.id, env),
    )?;
    let note = if def.entry.is_some() {
        format!("installed {} (MCP config: {})", def.id, mcp_path.display())
    } else {
        format!(
            "installed {} (official Web API client; no MCP server entry)",
            def.id
        )
    };
    Ok(note)
}

fn remove_plugin(id: &str, cwd: &Path) -> Result<String> {
    let def = definition(id)?;
    let id = def.id;
    let manifest =
        installed(cwd, id)?.ok_or_else(|| anyhow::anyhow!("plugin `{id}` is not installed"))?;
    let mcp_path = mcp_path(cwd);
    if manifest.owns_mcp_entry {
        let expected = manifest.entry.as_ref().ok_or_else(|| {
            anyhow::anyhow!("plugin `{id}` manifest is missing its owned MCP entry")
        })?;
        mcp::remove_server_entry(&mcp_path, id, expected)?;
    }
    fs::remove_file(manifest_path(cwd, id))?;
    let _ = fs::remove_file(env_example_path(cwd, id));
    Ok(format!("removed {id}"))
}

fn read_server_entry(path: &Path, name: &str) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(root
        .get("mcpServers")
        .and_then(Value::as_object)
        .and_then(|servers| servers.get(name))
        .cloned())
}

/// Handle `list`, `search <query>`, `install <id>`, and `remove <id>`.
pub fn handle_command(spec: &str, cwd: &Path) -> Result<String> {
    let mut parts = spec.split_whitespace();
    let command = parts.next().unwrap_or("list").to_ascii_lowercase();
    match command.as_str() {
        "list" => Ok(definitions()
            .into_iter()
            .map(|definition| format!("{}\t{}\t{}", definition.id, definition.plugin.name, definition.plugin.hint))
            .collect::<Vec<_>>()
            .join("\n")),
        "search" => {
            let query = parts.collect::<Vec<_>>().join(" ").to_ascii_lowercase();
            anyhow::ensure!(!query.trim().is_empty(), "usage: plugin search <query>");
            let result = definitions()
                .into_iter()
                .filter(|definition| {
                    definition.id.contains(&query)
                        || definition.plugin.name.to_ascii_lowercase().contains(&query)
                        || definition.plugin.hint.to_ascii_lowercase().contains(&query)
                })
                .map(|definition| format!("{}\t{}\t{}", definition.id, definition.plugin.name, definition.plugin.hint))
                .collect::<Vec<_>>();
            Ok(if result.is_empty() { "(no matches)".into() } else { result.join("\n") })
        }
        "install" => {
            let id = parts.next().context("usage: plugin install <id>")?;
            anyhow::ensure!(parts.next().is_none(), "usage: plugin install <id>");
            install_plugin(definition(id)?, cwd)
        }
        "remove" | "uninstall" => {
            let id = parts.next().context("usage: plugin remove <id>")?;
            anyhow::ensure!(parts.next().is_none(), "usage: plugin remove <id>");
            remove_plugin(&id.to_ascii_lowercase(), cwd)
        }
        "now-playing" | "now_playing" => now_playing(),
        other => anyhow::bail!("unknown plugin command `{other}` (expected list, search, install, remove, or now-playing)"),
    }
}

/// Fetch the currently playing item from Spotify's official Web API. This is
/// explicit about missing credentials, 204/no-content, and non-success status.
pub fn now_playing() -> Result<String> {
    let token = std::env::var("SPOTIFY_ACCESS_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context("SPOTIFY_ACCESS_TOKEN is required for plugin now-playing")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client
        .get("https://api.spotify.com/v1/me/player/currently-playing")
        .bearer_auth(token)
        .send()
        .context("Spotify currently-playing request failed")?;
    match response.status() {
        StatusCode::NO_CONTENT => Ok("nothing playing".into()),
        status if status.is_success() => {
            let value: Value = response
                .json()
                .context("parse Spotify currently-playing response")?;
            let item = value.get("item").cloned().unwrap_or(Value::Null);
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown track");
            let artists = item
                .get("artists")
                .and_then(Value::as_array)
                .map(|artists| {
                    artists
                        .iter()
                        .filter_map(|artist| artist.get("name").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            Ok(if artists.is_empty() {
                name.to_string()
            } else {
                format!("{name} — {artists}")
            })
        }
        status => anyhow::bail!("Spotify currently-playing returned HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn catalog_is_curated_and_legacy_google_stays_resolvable() {
        let ids: Vec<_> = catalog().iter().map(|plugin| plugin.id).collect();
        assert_eq!(
            ids,
            [
                PluginId::Github,
                PluginId::Gitlab,
                PluginId::Slack,
                PluginId::Discord,
                PluginId::Notion,
                PluginId::Spotify,
            ]
        );
        assert_eq!(get(PluginId::Google).name, "Google");
    }

    #[test]
    fn missing_token_says_token_yok() {
        assert_eq!(link_state(None, None), LinkState::Missing);
        assert_eq!(link_state(Some("  "), Some("ada")), LinkState::Missing);
        assert_eq!(status_label(&LinkState::Missing), "token yok");
    }

    #[test]
    fn token_with_account_shows_name_only() {
        let state = link_state(Some("secret"), Some(" octocat "));
        assert_eq!(
            state,
            LinkState::Linked {
                account: "octocat".into()
            }
        );
        assert_eq!(status_label(&state), "octocat");
        assert!(!status_label(&state).contains("secret"));
    }

    #[test]
    fn install_and_remove_preserve_foreign_mcp_entry() {
        let dir = tempdir().unwrap();
        let config = dir.path().join(".varynth/mcp.json");
        let foreign = json!({"type":"http","url":"https://example.invalid/mcp"});
        mcp::add_server_entry(&config, "foreign", &foreign, false).unwrap();
        let out = handle_command("install spotify", dir.path()).unwrap();
        assert!(out.contains("official Web API client"));
        assert!(config.exists());
        handle_command("remove spotify", dir.path()).unwrap();
        assert_eq!(
            read_server_entry(&config, "foreign").unwrap(),
            Some(foreign)
        );
    }

    #[test]
    fn install_mcp_then_remove_only_owned_entry() {
        let dir = tempdir().unwrap();
        handle_command("install gitlab", dir.path()).unwrap();
        let config = dir.path().join(".varynth/mcp.json");
        let root: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert!(root["mcpServers"]["gitlab"].is_object());
        assert!(fs::read_to_string(manifest_path(dir.path(), "gitlab"))
            .unwrap()
            .contains("GITLAB_PERSONAL_ACCESS_TOKEN"));
        handle_command("remove gitlab", dir.path()).unwrap();
        let root: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert!(root["mcpServers"].get("gitlab").is_none());
    }

    #[test]
    fn install_rejects_conflicting_mcp_entry() {
        let dir = tempdir().unwrap();
        let config = dir.path().join(".varynth/mcp.json");
        mcp::add_server_entry(
            &config,
            "gitlab",
            &json!({"type":"http","url":"https://foreign.invalid"}),
            false,
        )
        .unwrap();
        let error = handle_command("install gitlab", dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("different entry"));
        assert!(!manifest_path(dir.path(), "gitlab").exists());
    }

    #[test]
    fn now_playing_requires_explicit_token() {
        std::env::remove_var("SPOTIFY_ACCESS_TOKEN");
        let error = now_playing().unwrap_err().to_string();
        assert!(error.contains("SPOTIFY_ACCESS_TOKEN"));
    }
}
