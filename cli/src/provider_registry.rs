//! Secure, optional JSON provider registry.
//!
//! The registry lives at `Config::home_dir()/config.json`. It is deliberately
//! separate from `config.toml`: the TOML file remains the compatibility
//! fallback while this file carries ordered provider/model metadata. Registry
//! access values are references only (`env` and/or `keyring`); credential
//! literals are never accepted or serialized here.

use anyhow::{Context, Result};
use serde::de::{Deserializer, Error as DeError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use crate::config::{
    normalize_keyring_field, valid_profile_id, validate_provider_url, ProviderProfile,
    PROVIDER_KINDS,
};

pub const REGISTRY_SCHEMA_VERSION: u32 = 1;
pub const MAX_REGISTRY_BYTES: u64 = 512 * 1024;
pub const MAX_PROVIDERS: usize = 64;
pub const MAX_MODELS_PER_PROVIDER: usize = 256;
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
pub const MAX_MODEL_METADATA_BYTES: usize = 16 * 1024;

const MAX_TEXT_BYTES: usize = 4096;
const MAX_ID_BYTES: usize = 512;
const MAX_PROVIDER_NAME_BYTES: usize = 256;
const MAX_PROVIDER_URL_BYTES: usize = 2048;
const MAX_ACCESS_REF_BYTES: usize = 256;
const MAX_JSON_DEPTH: usize = 8;
const MAX_JSON_ENTRIES: usize = 128;

fn default_schema_version() -> u32 {
    REGISTRY_SCHEMA_VERSION
}

fn default_provider_kind() -> String {
    "openai".into()
}

/// The only credential-bearing values accepted by the registry. An empty
/// object is allowed so a provider can be declared without credentials while
/// retaining legacy TOML credentials during the merge.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccessRef {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub keyring: Option<String>,
    /// Account catalog calls send this env var as the bearer token.
    #[serde(default, rename = "bearerEnv")]
    pub bearer_env: Option<String>,
}

impl AccessRef {
    fn validate(&self) -> Result<()> {
        if let Some(env) = &self.env {
            validate_env_name(env)?;
        }
        if let Some(env) = &self.bearer_env {
            validate_env_name(env)?;
        }
        if let Some(keyring) = &self.keyring {
            validate_keyring_ref(keyring)?;
        }
        Ok(())
    }

    fn profile_fields(&self) -> Result<(Option<String>, Option<String>)> {
        self.validate()?;
        Ok((
            self.bearer_env.clone().or(self.env.clone()),
            (self.keyring.as_deref() != Some("profile"))
                .then(|| self.keyring.clone())
                .flatten(),
        ))
    }

    fn from_profile(profile: &ProviderProfile, use_keyring: bool) -> Option<Self> {
        if profile.api_key_env.as_deref() == Some("VARYNTH_ACCESS_TOKEN") {
            return Some(Self {
                env: None,
                keyring: profile.keyring_field.clone(),
                bearer_env: Some("VARYNTH_ACCESS_TOKEN".into()),
            });
        }
        if profile.api_key_env.is_some() || profile.keyring_field.is_some() {
            return Some(Self {
                env: profile.api_key_env.clone(),
                keyring: profile.keyring_field.clone(),
                bearer_env: None,
            });
        }
        if use_keyring && profile.api_key.is_none() {
            return Some(Self {
                env: None,
                keyring: Some("profile".into()),
                bearer_env: None,
            });
        }
        None
    }
}

/// A model declaration. The map key is the model id; metadata is flattened
/// so future model fields survive a read/write cycle without being interpreted
/// as credentials by Varynth.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RegistryModel {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(rename = "ownedBy", default)]
    pub owned_by: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// One configured provider. Unknown non-secret fields are retained in
/// `extra`, allowing newer registry producers to round-trip through Varynth.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProviderEntry {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    #[serde(rename = "baseUrl", default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default, deserialize_with = "deserialize_access")]
    pub access: Option<AccessRef>,
    #[serde(default, deserialize_with = "deserialize_models")]
    pub models: BTreeMap<String, RegistryModel>,
    #[serde(rename = "modelOrder", default)]
    pub model_order: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ProviderEntry {
    pub fn ordered_model_ids(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.models.len());
        let mut seen = HashSet::new();
        for id in &self.model_order {
            if self.models.contains_key(id) && seen.insert(id.as_str()) {
                out.push(id.clone());
            }
        }
        for id in self.models.keys() {
            if seen.insert(id.as_str()) {
                out.push(id.clone());
            }
        }
        out
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(valid_profile_id(&self.id), "invalid registry provider id");
        validate_optional_text(
            self.name.as_deref(),
            "provider name",
            MAX_PROVIDER_NAME_BYTES,
        )?;
        anyhow::ensure!(
            PROVIDER_KINDS.contains(&self.kind.as_str()),
            "invalid registry provider kind (expected proxy, openai, anthropic, google, or google-vertex)"
        );
        if let Some(base_url) = &self.base_url {
            anyhow::ensure!(
                base_url.len() <= MAX_PROVIDER_URL_BYTES,
                "registry provider baseUrl is too long"
            );
            validate_provider_url(base_url)?;
        }
        validate_optional_text(self.api.as_deref(), "provider api", 128)?;
        if let Some(access) = &self.access {
            access.validate()?;
        }
        anyhow::ensure!(
            self.models.len() <= MAX_MODELS_PER_PROVIDER,
            "registry provider has too many models"
        );
        for (id, model) in &self.models {
            validate_model_id(id)?;
            model.validate()?;
        }
        validate_model_order(&self.model_order)?;
        validate_metadata(&self.metadata, MAX_METADATA_BYTES, "provider metadata")?;
        validate_optional_text(self.auth.as_deref(), "provider auth", 32)?;
        validate_optional_text(self.project.as_deref(), "provider project", 256)?;
        validate_optional_text(self.location.as_deref(), "provider location", 256)?;
        validate_extra_map(&self.extra, "provider field")?;
        Ok(())
    }

    fn catalog_model(&self, model_id: &str) -> RegistryCatalogModel {
        let model = self.models.get(model_id);
        let native_name = model.and_then(|m| m.display_name.as_deref().or(m.name.as_deref()));
        let label = native_name
            .map(str::to_string)
            .unwrap_or_else(|| model_id.to_string());
        RegistryCatalogModel {
            id: format!("{}/{}", self.id, model_id),
            display_name: Some(format!(
                "{} / {}",
                self.name.as_deref().unwrap_or(&self.id),
                label
            )),
            owned_by: model.and_then(|m| m.owned_by.clone()),
            provider_id: self.id.clone(),
            model_id: model_id.to_string(),
        }
    }
}

impl RegistryModel {
    fn validate(&self) -> Result<()> {
        validate_optional_text(self.name.as_deref(), "model name", MAX_PROVIDER_NAME_BYTES)?;
        validate_optional_text(
            self.display_name.as_deref(),
            "model displayName",
            MAX_PROVIDER_NAME_BYTES,
        )?;
        validate_optional_text(self.owned_by.as_deref(), "model ownedBy", 128)?;
        validate_extra_map(&self.extra, "model field")?;
        let encoded = serde_json::to_vec(self).context("serialize registry model metadata")?;
        anyhow::ensure!(
            encoded.len() <= MAX_MODEL_METADATA_BYTES,
            "registry model metadata is too large"
        );
        Ok(())
    }
}

/// A catalog-safe model projection. It contains no access references or
/// provider credential fields and is useful to callers that need the ordered
/// registry catalog without making a live network request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryCatalogModel {
    pub id: String,
    pub display_name: Option<String>,
    pub owned_by: Option<String>,
    pub provider_id: String,
    pub model_id: String,
}

/// Top-level provider registry. `providers` accepts both the documented array
/// form and the OpenCode-like object form keyed by provider id; writes use the
/// array form so each entry can carry its explicit `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderRegistry {
    #[serde(rename = "schemaVersion", default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(rename = "providerOrder", default)]
    pub provider_order: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_providers")]
    pub providers: Vec<ProviderEntry>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            provider_order: Vec::new(),
            providers: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl ProviderRegistry {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.schema_version == REGISTRY_SCHEMA_VERSION,
            "unsupported provider registry schemaVersion {}",
            self.schema_version
        );
        anyhow::ensure!(
            self.providers.len() <= MAX_PROVIDERS,
            "provider registry contains too many providers"
        );
        let mut ids = HashSet::new();
        for provider in &self.providers {
            anyhow::ensure!(
                ids.insert(provider.id.as_str()),
                "duplicate registry provider id: {}",
                provider.id
            );
            provider.validate()?;
        }
        validate_order_ids(&self.provider_order, "providerOrder")?;
        validate_metadata(&self.extra, MAX_METADATA_BYTES, "registry field")?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        Self::load_optional(path)?.context("provider registry is missing")
    }

    pub fn load_optional(path: &Path) -> Result<Option<Self>> {
        validate_registry_path(path)?;
        let Some(metadata) = existing_file_metadata(path)? else {
            return Ok(None);
        };
        anyhow::ensure!(
            metadata.len() <= MAX_REGISTRY_BYTES,
            "provider registry exceeds {} bytes",
            MAX_REGISTRY_BYTES
        );
        let raw =
            fs::read(path).with_context(|| format!("read provider registry {}", path.display()))?;
        anyhow::ensure!(
            raw.len() as u64 <= MAX_REGISTRY_BYTES,
            "provider registry exceeds {} bytes",
            MAX_REGISTRY_BYTES
        );
        let registry: Self =
            serde_json::from_slice(&raw).context("parse provider registry JSON")?;
        registry.validate().context("invalid provider registry")?;
        Ok(Some(registry))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate().context("invalid provider registry")?;
        validate_registry_path(path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        validate_registry_path(path)?;
        let raw = serde_json::to_vec_pretty(self).context("serialize provider registry")?;
        anyhow::ensure!(
            raw.len() as u64 <= MAX_REGISTRY_BYTES,
            "provider registry exceeds {} bytes",
            MAX_REGISTRY_BYTES
        );
        let lock = path.with_extension("json.lock");
        let _lock = crate::control_bus::FileLock::acquire(lock)?;
        crate::control_bus::atomic_write(path, &raw)
    }

    /// Return provider entries in `providerOrder`, then append providers not
    /// listed there in their file order. Unknown order ids are ignored.
    pub fn ordered_entries(&self) -> Vec<&ProviderEntry> {
        let mut out = Vec::with_capacity(self.providers.len());
        let mut seen = HashSet::new();
        for id in &self.provider_order {
            if let Some(provider) = self.providers.iter().find(|p| p.id == *id) {
                if seen.insert(provider.id.as_str()) {
                    out.push(provider);
                }
            }
        }
        for provider in &self.providers {
            if seen.insert(provider.id.as_str()) {
                out.push(provider);
            }
        }
        out
    }

    pub fn ordered_provider_ids(&self) -> Vec<String> {
        self.ordered_entries()
            .into_iter()
            .map(|provider| provider.id.clone())
            .collect()
    }

    pub fn qualified_model_ids(&self) -> Vec<String> {
        self.catalog_models()
            .into_iter()
            .map(|model| model.id)
            .collect()
    }

    pub fn catalog_models(&self) -> Vec<RegistryCatalogModel> {
        self.ordered_entries()
            .into_iter()
            .flat_map(|provider| {
                provider
                    .ordered_model_ids()
                    .into_iter()
                    .map(|id| provider.catalog_model(&id))
            })
            .collect()
    }

    /// Merge JSON providers ahead of legacy TOML profiles. A JSON provider
    /// with an access reference owns the credential reference; when access is
    /// omitted, a matching TOML credential is retained as a compatibility
    /// fallback without copying it into the JSON registry.
    pub fn merge_profiles(&self, legacy: &[ProviderProfile]) -> Result<Vec<ProviderProfile>> {
        self.validate()?;
        let mut merged = Vec::with_capacity(self.providers.len() + legacy.len());
        let mut consumed = HashSet::new();
        for entry in self.ordered_entries() {
            let mut profile = legacy
                .iter()
                .find(|profile| profile.id == entry.id)
                .cloned()
                .unwrap_or_else(|| ProviderProfile {
                    id: entry.id.clone(),
                    ..ProviderProfile::default()
                });
            apply_entry_to_profile(entry, &mut profile)?;
            consumed.insert(entry.id.as_str());
            merged.push(profile);
        }
        merged.extend(
            legacy
                .iter()
                .filter(|profile| !consumed.contains(profile.id.as_str()))
                .cloned(),
        );
        Ok(merged)
    }

    /// Update known provider fields from effective legacy profiles while
    /// retaining unknown registry fields and model metadata. Credential
    /// literals in `ProviderProfile::api_key` are intentionally ignored.
    pub fn replace_profiles(
        &mut self,
        profiles: &[ProviderProfile],
        use_keyring: bool,
    ) -> Result<()> {
        let old = std::mem::take(&mut self.providers);
        let mut next = Vec::with_capacity(profiles.len());
        for profile in profiles {
            let mut entry = old
                .iter()
                .find(|entry| entry.id == profile.id)
                .cloned()
                .unwrap_or_else(|| ProviderEntry {
                    id: profile.id.clone(),
                    ..ProviderEntry::default()
                });
            entry.id = profile.id.clone();
            entry.name = profile.name.clone();
            entry.kind = profile.kind.clone();
            entry.base_url = profile.base_url.clone();
            entry.enabled = Some(profile.enabled);
            entry.auth = Some(profile.auth.clone());
            entry.project = profile.project.clone();
            entry.location = profile.location.clone();
            entry.api = entry
                .api
                .or_else(|| Some(default_api(&profile.kind).into()));

            let old_models = std::mem::take(&mut entry.models);
            entry.models = profile
                .models
                .iter()
                .map(|id| (id.clone(), old_models.get(id).cloned().unwrap_or_default()))
                .collect();
            entry.model_order = profile.models.clone();

            if let Some(access) = AccessRef::from_profile(profile, use_keyring) {
                access.validate()?;
                entry.access = Some(access);
            }
            next.push(entry);
        }
        self.providers = next;
        self.provider_order = profiles.iter().map(|profile| profile.id.clone()).collect();
        self.validate()
    }
}

fn default_api(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "anthropic-messages",
        "google" | "google-vertex" => "google",
        "proxy" | "openai" => "openai-compatible",
        _ => "openai-compatible",
    }
}

fn apply_entry_to_profile(entry: &ProviderEntry, profile: &mut ProviderProfile) -> Result<()> {
    profile.id = entry.id.clone();
    profile.kind = entry.kind.clone();
    profile.name = entry.name.clone();
    profile.base_url = entry.base_url.clone();
    profile.models = entry.ordered_model_ids();
    if let Some(enabled) = entry.enabled {
        profile.enabled = enabled;
    }
    if let Some(auth) = &entry.auth {
        profile.auth = auth.clone();
    }
    profile.project = entry.project.clone();
    profile.location = entry.location.clone();
    if let Some(access) = &entry.access {
        let (env, keyring) = access.profile_fields()?;
        profile.api_key = None;
        profile.api_key_env = env;
        profile.keyring_field = keyring;
    }
    Ok(())
}

fn deserialize_access<'de, D>(deserializer: D) -> Result<Option<AccessRef>, D::Error>
where
    D: Deserializer<'de>,
{
    let access = Option::<AccessRef>::deserialize(deserializer)?;
    if let Some(access) = &access {
        access.validate().map_err(D::Error::custom)?;
    }
    Ok(access)
}

fn deserialize_providers<'de, D>(deserializer: D) -> Result<Vec<ProviderEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| serde_json::from_value(item).map_err(D::Error::custom))
            .collect(),
        Value::Object(entries) => entries
            .into_iter()
            .map(|(id, item)| {
                let mut object = item.as_object().cloned().ok_or_else(|| {
                    D::Error::custom(format!("provider `{id}` must be an object"))
                })?;
                object.entry("id").or_insert(Value::String(id));
                serde_json::from_value(Value::Object(object)).map_err(D::Error::custom)
            })
            .collect(),
        _ => Err(D::Error::custom("providers must be an array or object")),
    }
}

fn deserialize_models<'de, D>(deserializer: D) -> Result<BTreeMap<String, RegistryModel>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Object(entries) => entries
            .into_iter()
            .map(|(id, item)| {
                let model = match item {
                    Value::String(name) => RegistryModel {
                        name: Some(name),
                        ..RegistryModel::default()
                    },
                    other => serde_json::from_value(other).map_err(D::Error::custom)?,
                };
                Ok((id, model))
            })
            .collect(),
        Value::Array(items) => {
            let mut models = BTreeMap::new();
            for item in items {
                let (id, model) = match item {
                    Value::String(id) => (id, RegistryModel::default()),
                    Value::Object(mut object) => {
                        let id = object
                            .remove("id")
                            .and_then(|value| value.as_str().map(str::to_string))
                            .ok_or_else(|| D::Error::custom("model array entries need an id"))?;
                        let model = serde_json::from_value(Value::Object(object))
                            .map_err(D::Error::custom)?;
                        (id, model)
                    }
                    _ => return Err(D::Error::custom("models must contain ids or objects")),
                };
                if models.insert(id.clone(), model).is_some() {
                    return Err(D::Error::custom(format!("duplicate model id: {id}")));
                }
            }
            Ok(models)
        }
        _ => Err(D::Error::custom("models must be an array or object")),
    }
}

fn validate_registry_path(path: &Path) -> Result<()> {
    let path_text = path.to_string_lossy();
    anyhow::ensure!(
        !path_text.is_empty() && path_text.len() <= 4096,
        "invalid registry path"
    );
    validate_parent_chain(path)?;
    Ok(())
}

fn validate_parent_chain(path: &Path) -> Result<()> {
    let mut current = path.parent();
    let mut depth = 0;
    while let Some(parent) = current {
        depth += 1;
        anyhow::ensure!(depth <= 64, "registry path is too deep");
        match fs::symlink_metadata(parent) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "registry path contains a symlink"
                );
                anyhow::ensure!(metadata.is_dir(), "registry path parent is not a directory");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        current = parent.parent();
    }
    Ok(())
}

fn existing_file_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "provider registry cannot be a symlink"
            );
            anyhow::ensure!(
                metadata.is_file(),
                "provider registry must be a regular file"
            );
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn validate_env_name(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_ACCESS_REF_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "registry access env must be an environment variable name"
    );
    Ok(())
}

fn validate_keyring_ref(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_ACCESS_REF_BYTES
            && !value.chars().any(char::is_control),
        "registry access keyring reference is invalid"
    );
    if value != "profile" {
        normalize_keyring_field(value)?;
    }
    Ok(())
}

fn validate_optional_text(value: Option<&str>, label: &str, max: usize) -> Result<()> {
    if let Some(value) = value {
        anyhow::ensure!(
            value.len() <= max && !value.chars().any(char::is_control),
            "{label} is invalid"
        );
    }
    Ok(())
}

fn validate_model_id(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty()
            && value.len() <= MAX_ID_BYTES
            && !value.chars().any(char::is_control),
        "invalid registry model id"
    );
    Ok(())
}

fn validate_order_ids(order: &[String], label: &str) -> Result<()> {
    anyhow::ensure!(
        order.len() <= MAX_PROVIDERS,
        "{label} contains too many ids"
    );
    let mut ids = HashSet::new();
    for id in order {
        anyhow::ensure!(valid_profile_id(id), "invalid id in {label}");
        anyhow::ensure!(ids.insert(id.as_str()), "duplicate id in {label}");
    }
    Ok(())
}

fn validate_model_order(order: &[String]) -> Result<()> {
    anyhow::ensure!(
        order.len() <= MAX_MODELS_PER_PROVIDER,
        "modelOrder contains too many ids"
    );
    let mut ids = HashSet::new();
    for id in order {
        validate_model_id(id)?;
        anyhow::ensure!(ids.insert(id.as_str()), "duplicate id in modelOrder");
    }
    Ok(())
}

fn validate_metadata(
    values: &BTreeMap<String, Value>,
    max_bytes: usize,
    label: &str,
) -> Result<()> {
    let encoded = serde_json::to_vec(values).context("serialize registry metadata")?;
    anyhow::ensure!(encoded.len() <= max_bytes, "{label} is too large");
    for (key, value) in values {
        validate_json_key(key, label)?;
        validate_json_value(value, 0, label)?;
    }
    Ok(())
}

fn validate_extra_map(values: &BTreeMap<String, Value>, label: &str) -> Result<()> {
    for (key, value) in values {
        validate_json_key(key, label)?;
        validate_json_value(value, 0, label)?;
    }
    Ok(())
}

fn validate_json_key(key: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
        "invalid {label} name"
    );
    anyhow::ensure!(!is_unsafe_key(key), "unsafe secret-like {label} name");
    Ok(())
}

fn is_unsafe_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    normalized == "apikey"
        || normalized.ends_with("apikey")
        || normalized == "secret"
        || normalized.ends_with("secret")
        || normalized == "password"
        || normalized.ends_with("password")
        || normalized == "token"
        || normalized.ends_with("token")
        || normalized.contains("credential")
        || normalized == "authorization"
        || normalized == "cookie"
}

fn validate_json_value(value: &Value, depth: usize, label: &str) -> Result<()> {
    anyhow::ensure!(depth <= MAX_JSON_DEPTH, "{label} is nested too deeply");
    match value {
        Value::String(text) => anyhow::ensure!(
            text.len() <= MAX_TEXT_BYTES && !text.chars().any(char::is_control),
            "{label} contains invalid text"
        ),
        Value::Array(items) => {
            anyhow::ensure!(
                items.len() <= MAX_JSON_ENTRIES,
                "{label} contains too many values"
            );
            for item in items {
                validate_json_value(item, depth + 1, label)?;
            }
        }
        Value::Object(object) => {
            anyhow::ensure!(
                object.len() <= MAX_JSON_ENTRIES,
                "{label} contains too many fields"
            );
            for (key, item) in object {
                validate_json_key(key, label)?;
                validate_json_value(item, depth + 1, label)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}
