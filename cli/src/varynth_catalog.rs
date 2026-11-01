//! Pull the public Varynth list and, when signed in, the account catalog.

use anyhow::Result;
use std::collections::HashSet;
use std::time::Duration;

use crate::config::{Config, ProviderProfile};
use crate::varynth_auth;

pub const DEFAULT_PUBLIC_MODELS: [&str; 21] = [
    "gpt-oss-120b",
    "qwen3.8-27b",
    "nemotron-3-120b-a12b",
    "qwen3-30b-a3b-fp8",
    "qwen2.5-coder-32b-instruct",
    "deepseek-r1-distill-qwen-32b",
    "qwq-32b",
    "llama-4-scout-17b-16e-instruct",
    "llama-3.3-70b-instruct-fp8-fast",
    "gemma-4-26b-a4b-it",
    "glm-4.7-flash",
    "mistral-small-3.1-24b-instruct",
    "gpt-oss-20b",
    "gemma-sea-lion-v4-27b-it",
    "llama-3.1-8b-instruct-fast",
    "llama-3.1-8b-instruct-fp8",
    "granite-4.0-h-micro",
    "llama-3.2-3b-instruct",
    "llama-3.2-1b-instruct",
    "gemma-7b-it-lora",
    "gemma-2b-it-lora",
];

pub async fn sync(cfg: &mut Config) -> Result<()> {
    let _ = varynth_auth::load_auth_env()?;
    let _ = varynth_auth::access_token_fresh(&cfg.varynth_api_url).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let base = cfg.varynth_api_url.trim_end_matches('/').to_string();
    let public_models = match client
        .get(format!("{base}/v1/models/public"))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => {
            if let Ok(body) = res.json::<serde_json::Value>().await {
                let ms = model_suffixes(&body);
                if ms.is_empty() {
                    DEFAULT_PUBLIC_MODELS.iter().map(|s| s.to_string()).collect()
                } else {
                    ms
                }
            } else {
                DEFAULT_PUBLIC_MODELS.iter().map(|s| s.to_string()).collect()
            }
        }
        _ => match client.get(format!("{base}/v1/models")).send().await {
            Ok(res) if res.status().is_success() => {
                if let Ok(body) = res.json::<serde_json::Value>().await {
                    let ms = model_suffixes(&body);
                    if ms.is_empty() {
                        DEFAULT_PUBLIC_MODELS.iter().map(|s| s.to_string()).collect()
                    } else {
                        ms
                    }
                } else {
                    DEFAULT_PUBLIC_MODELS.iter().map(|s| s.to_string()).collect()
                }
            }
            _ => DEFAULT_PUBLIC_MODELS.iter().map(|s| s.to_string()).collect(),
        },
    };
    upsert(cfg, "varynth", "Varynth", "openai", public_models, false);
    let first_public = cfg
        .provider_profiles
        .iter()
        .find(|profile| profile.id == "varynth")
        .and_then(|profile| profile.models.first().cloned());
    if let Some(first) = first_public {
        if cfg.provider == "varynth" || cfg.require_provider_credentials().is_err() {
            if !cfg.model.starts_with("varynth/") {
                cfg.model = format!("varynth/{first}");
            }
        }
    }

    let token_opt = std::env::var("VARYNTH_ACCESS_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty());

    if let Some(token) = &token_opt {
        if let Ok(response) = client
            .get(format!("{base}/v1/catalog"))
            .bearer_auth(token)
            .send()
            .await
        {
            if response.status().is_success() {
                if let Ok(catalog) = response.json::<serde_json::Value>().await {
                    apply_catalog(cfg, &catalog);
                }
            }
        }
    }

    let skills_req = client.get(format!("{base}/v1/catalog/skills"));
    let skills_req = if let Some(token) = &token_opt {
        skills_req.bearer_auth(token)
    } else {
        skills_req
    };
    if let Ok(response) = skills_req.send().await {
        if response.status().is_success() {
            if let Ok(skills_val) = response.json::<serde_json::Value>().await {
                apply_skills_catalog(&skills_val);
            }
        }
    }

    Ok(())
}

fn model_suffixes(body: &serde_json::Value) -> Vec<String> {
    body.get("data")
        .and_then(|data| data.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("id").and_then(|id| id.as_str()))
                .map(|id| {
                    id.strip_prefix("varynth/")
                        .filter(|rest| !rest.is_empty())
                        .unwrap_or(id)
                        .to_string()
                })
                .filter(|id| !id.contains("cloudflare") && !id.contains("@cf/"))
                .collect()
        })
        .unwrap_or_default()
}

fn apply_catalog(cfg: &mut Config, _catalog: &serde_json::Value) {
    // Purge any auto-created vendor profiles tied to VARYNTH_ACCESS_TOKEN.
    // All Varynth upstream models belong natively under the "varynth" provider profile!
    // We must NOT pollute the user's config with phantom vendor profiles (microsoft, google, meta, ibm, etc.)
    cfg.provider_profiles.retain(|profile| {
        profile.api_key_env.as_deref() != Some("VARYNTH_ACCESS_TOKEN") || profile.id == "varynth"
    });
}

fn upsert(
    cfg: &mut Config,
    id: &str,
    name: &str,
    kind: &str,
    models: Vec<String>,
    bearer: bool,
) -> bool {
    let base = format!("{}/v1", cfg.varynth_api_url.trim_end_matches('/'));
    if let Some(existing) = cfg
        .provider_profiles
        .iter_mut()
        .find(|profile| profile.id == id)
    {
        let direct = id != "varynth"
            && existing.api_key_env.as_deref() != Some("VARYNTH_ACCESS_TOKEN")
            && existing.base_url.as_deref() != Some(base.as_str());
        if direct {
            return false;
        }
        existing.name = Some(name.to_string());
        existing.kind = kind.to_string();
        existing.base_url = Some(base);
        existing.models = models;
        existing.enabled = true;
        existing.api_key = None;
        if bearer {
            existing.api_key_env = Some("VARYNTH_ACCESS_TOKEN".into());
        } else {
            existing.api_key_env = None;
        }
        existing.keyring_field = None;
        return true;
    }
    cfg.provider_profiles.push(ProviderProfile {
        id: id.to_string(),
        kind: kind.to_string(),
        name: Some(name.to_string()),
        base_url: Some(base),
        api_key: None,
        api_key_env: bearer.then(|| "VARYNTH_ACCESS_TOKEN".into()),
        keyring_field: None,
        models,
        enabled: true,
        auth: "api-key".into(),
        project: None,
        location: None,
    });
    true
}

/// Sync tiered skills into `<home>/.varynth/skills/<id>/SKILL.md`.
/// Only unlocked skills with content are written, tagged with a `.varynth_synced` marker.
/// Locked or revoked skills that were previously synced are automatically deleted.
pub fn apply_skills_catalog(catalog: &serde_json::Value) {
    let home = crate::config::Config::home_dir();
    let skills_root = home.join("skills");
    if std::fs::create_dir_all(&skills_root).is_err() {
        return;
    }

    let items = catalog
        .get("skills")
        .or_else(|| catalog.get("data"))
        .and_then(serde_json::Value::as_array);

    let Some(items) = items else {
        return;
    };

    let mut currently_unlocked = HashSet::new();

    for item in items {
        let Some(id) = item.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
            continue;
        }

        let unlocked = item
            .get("unlocked")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let content = item.get("content").and_then(serde_json::Value::as_str);

        let skill_dir = skills_root.join(id);
        let marker = skill_dir.join(".varynth_synced");

        if unlocked && content.is_some() {
            let content_str = content.unwrap();
            currently_unlocked.insert(id.to_string());
            if std::fs::create_dir_all(&skill_dir).is_ok() {
                let _ = std::fs::write(skill_dir.join("SKILL.md"), content_str);
                let _ = std::fs::write(&marker, "synced");
            }
        } else if marker.exists() {
            let _ = std::fs::remove_dir_all(&skill_dir);
        }
    }

    for banned in ["motion-fream", "motion-fream-pro", "motion-frames", "motion-frames-pro"] {
        let bdir = skills_root.join(banned);
        if bdir.exists() {
            let _ = std::fs::remove_dir_all(&bdir);
        }
    }

    // Clean up any previously synced directories that are no longer unlocked in the catalog.
    if let Ok(entries) = std::fs::read_dir(&skills_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let marker = path.join(".varynth_synced");
                if marker.exists() {
                    let dir_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    if !currently_unlocked.contains(dir_name) {
                        let _ = std::fs::remove_dir_all(&path);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn skills_auto_sync_writes_unlocked_and_removes_locked() {
        let dir = tempdir().unwrap();
        std::env::set_var("VARYNTH_HOME", dir.path());

        // Initial sync: 1 free unlocked skill, 1 pro unlocked skill, 1 locked skill
        let initial_catalog = json!({
            "skills": [
                {
                    "id": "free-skill",
                    "name": "Free Skill",
                    "unlocked": true,
                    "content": "# Free Skill Content"
                },
                {
                    "id": "pro-architect",
                    "name": "Pro Architect",
                    "unlocked": true,
                    "content": "# Pro Content"
                },
                {
                    "id": "locked-skill",
                    "name": "Locked Skill",
                    "unlocked": false,
                    "content": null
                }
            ]
        });

        apply_skills_catalog(&initial_catalog);

        let skills_root = dir.path().join("skills");
        assert!(skills_root.join("free-skill/SKILL.md").exists());
        assert!(skills_root.join("free-skill/.varynth_synced").exists());
        assert!(skills_root.join("pro-architect/SKILL.md").exists());
        assert!(skills_root.join("pro-architect/.varynth_synced").exists());
        assert!(!skills_root.join("locked-skill").exists());

        // Create a local custom user skill (no .varynth_synced marker)
        let custom_skill = skills_root.join("my-custom-skill");
        std::fs::create_dir_all(&custom_skill).unwrap();
        std::fs::write(custom_skill.join("SKILL.md"), "# Custom").unwrap();

        // Subsequent sync: user downgraded/unsubscribed from pro; pro-architect is now locked!
        let downgraded_catalog = json!({
            "skills": [
                {
                    "id": "free-skill",
                    "name": "Free Skill",
                    "unlocked": true,
                    "content": "# Free Skill Content"
                },
                {
                    "id": "pro-architect",
                    "name": "Pro Architect",
                    "unlocked": false,
                    "content": null
                }
            ]
        });

        apply_skills_catalog(&downgraded_catalog);

        // Free skill remains
        assert!(skills_root.join("free-skill/SKILL.md").exists());
        // Expired pro skill is removed automatically
        assert!(!skills_root.join("pro-architect").exists());
        // User custom skill without marker is preserved!
        assert!(custom_skill.join("SKILL.md").exists());

        std::env::remove_var("VARYNTH_HOME");
    }
}

