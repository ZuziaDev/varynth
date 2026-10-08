use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub provider: String,
    pub permission_mode: String,
    pub sandbox: String,
    pub proxy_url: String,
    pub proxy_token: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub openai_base_url: Option<String>,
    pub google_api_key: Option<String>,
    pub google_base_url: Option<String>,
    pub google_project: Option<String>,
    pub google_location: Option<String>,
    pub google_auth: String,
    /// Non-secret OAuth client id (installed app) used by the automatic
    /// Google sign-in preflight. Never a secret, so it is safe to keep in
    /// config.toml and is never redacted on save.
    #[serde(default)]
    pub google_client_id: Option<String>,
    /// Start the Google browser sign-in automatically when a Google OAuth
    /// provider is selected without a stored credential. Enabled by default.
    #[serde(default = "default_google_auto_login")]
    pub google_auto_login: bool,
    pub provider_profiles: Vec<ProviderProfile>,
    pub dashboard_host: String,
    pub dashboard_port: u16,
    #[serde(default)]
    pub dashboard_token: Option<String>,
    #[serde(default)]
    pub telegram_bot_token: Option<String>,
    #[serde(default)]
    pub telegram_allow_from: Vec<String>,
    pub add_dirs: Vec<PathBuf>,
    pub shell_allowlist: Vec<String>,
    pub max_tool_rounds: usize,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default)]
    pub mcp_config: Option<PathBuf>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub goal_max_rounds: Option<u32>,
    #[serde(default)]
    pub goal_max_minutes: Option<u64>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub docker_image: Option<String>,
    /// Network mode for the docker-isolated sandbox: none | bridge | host.
    #[serde(default)]
    pub docker_network: Option<String>,
    /// Read secrets from the OS keyring (service `varynth`) instead of
    /// storing them in config.toml.
    #[serde(default)]
    pub use_keyring: Option<bool>,
    #[serde(default = "default_theme")]
    pub theme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderProfile {
    pub id: String,
    pub kind: String,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub keyring_field: Option<String>,
    pub models: Vec<String>,
    pub enabled: bool,
    pub auth: String,
    pub project: Option<String>,
    pub location: Option<String>,
}

impl Default for ProviderProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: "openai".into(),
            name: None,
            base_url: None,
            api_key: None,
            api_key_env: None,
            keyring_field: None,
            models: Vec::new(),
            enabled: true,
            auth: "api-key".into(),
            project: None,
            location: None,
        }
    }
}

pub const PROVIDER_KINDS: [&str; 5] = ["proxy", "openai", "anthropic", "google", "google-vertex"];

pub fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn default_theme() -> String {
    "purple".into()
}

pub fn default_google_auto_login() -> bool {
    true
}

/// Outcome of [`Config::google_auto_login_plan`]: the decision made before
/// any model-using command or runtime reconfiguration runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoogleAutoLoginPlan {
    /// No action needed: provider does not use Google OAuth, or a credential
    /// already exists.
    Ready,
    /// Start the browser sign-in flow with this client id.
    Login { client_id: String },
    /// Sign-in is required but cannot start; carry this actionable message.
    Error(String),
}

fn default_effort() -> String {
    "xhigh".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "grok-4.7-build".into(),
            provider: "proxy".into(),
            permission_mode: "acceptEdits".into(),
            sandbox: "workspace-write".into(),
            proxy_url: "http://127.0.0.1:8787".into(),
            proxy_token: None,
            anthropic_api_key: None,
            openai_api_key: None,
            anthropic_base_url: None,
            openai_base_url: None,
            google_api_key: None,
            google_base_url: None,
            google_project: None,
            google_location: None,
            google_auth: "api-key".into(),
            google_client_id: None,
            google_auto_login: default_google_auto_login(),
            provider_profiles: Vec::new(),
            dashboard_host: "127.0.0.1".into(),
            dashboard_port: 7420,
            dashboard_token: None,
            telegram_bot_token: None,
            telegram_allow_from: Vec::new(),
            add_dirs: Vec::new(),
            shell_allowlist: default_shell_allowlist(),
            max_tool_rounds: 24,
            effort: default_effort(),
            mcp_config: None,
            max_tokens: None,
            temperature: None,
            goal_max_rounds: None,
            goal_max_minutes: None,
            stream: None,
            docker_image: None,
            docker_network: None,
            use_keyring: None,
            theme: default_theme(),
        }
    }
}

fn default_shell_allowlist() -> Vec<String> {
    [
        "git",
        "cargo",
        "rustc",
        "npm",
        "npx",
        "pnpm",
        "node",
        "python",
        "python3",
        "pip",
        "rg",
        "fd",
        "dir",
        "ls",
        "type",
        "cat",
        "echo",
        "where",
        "whoami",
        "pwd",
        "cd",
        "mkdir",
        "copy",
        "move",
        "del",
        "rm",
        "curl",
        "wget",
        "tar",
        "zip",
        "unzip",
        "powershell",
        "pwsh",
        "cmd",
        "go",
        "dotnet",
        "tsc",
        "bun",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.model.trim().is_empty(), "model cannot be empty");
        anyhow::ensure!(
            self.max_tool_rounds > 0 && self.max_tool_rounds <= 512,
            "max_tool_rounds must be between 1 and 512"
        );
        anyhow::ensure!(self.max_tokens != Some(0), "max_tokens must be positive");
        if let Some(temperature) = self.temperature {
            anyhow::ensure!(
                temperature.is_finite() && (0.0..=2.0).contains(&temperature),
                "temperature must be finite and between 0 and 2"
            );
        }
        anyhow::ensure!(
            self.goal_max_rounds != Some(0) && self.goal_max_minutes != Some(0),
            "goal budgets must be positive"
        );
        anyhow::ensure!(
            matches!(
                self.theme.as_str(),
                "purple" | "cyberpunk" | "dark" | "minimal"
            ),
            "invalid theme (expected purple, cyberpunk, dark, or minimal)"
        );
        if !matches!(
            self.permission_mode.as_str(),
            "acceptEdits" | "prompt" | "bypass"
        ) {
            anyhow::bail!(
                "invalid permission_mode `{}` (expected acceptEdits, prompt, or bypass)",
                self.permission_mode
            );
        }
        if !matches!(
            self.sandbox.as_str(),
            "read-only" | "workspace-write" | "danger-full-access" | "docker-isolated"
        ) {
            anyhow::bail!(
                "invalid sandbox `{}` (expected read-only, workspace-write, danger-full-access, or docker-isolated)",
                self.sandbox
            );
        }
        if let Some(net) = &self.docker_network {
            if !matches!(net.as_str(), "none" | "bridge" | "host") {
                anyhow::bail!(
                    "invalid docker_network `{}` (expected none, bridge, or host)",
                    net
                );
            }
        }
        if !matches!(
            self.effort.as_str(),
            "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
        ) {
            anyhow::bail!(
                "invalid effort `{}` (expected low, medium, high, xhigh, max, or ultra)",
                self.effort
            );
        }
        anyhow::ensure!(
            matches!(self.google_auth.as_str(), "api-key" | "oauth"),
            "google_auth must be api-key or oauth"
        );
        if let Some(client_id) = &self.google_client_id {
            anyhow::ensure!(
                !client_id.trim().is_empty()
                    && client_id.len() <= 512
                    && !client_id
                        .chars()
                        .any(|c| c.is_control() || c.is_whitespace()),
                "google_client_id must be a non-empty single-line OAuth client id"
            );
        }
        let mut ids = std::collections::HashSet::new();
        for profile in &self.provider_profiles {
            anyhow::ensure!(valid_profile_id(&profile.id), "invalid provider profile id");
            anyhow::ensure!(
                ids.insert(&profile.id),
                "duplicate provider profile id: {}",
                profile.id
            );
            anyhow::ensure!(PROVIDER_KINDS.contains(&profile.kind.as_str()), "invalid profile kind (expected proxy, openai, anthropic, google, or google-vertex)");
            anyhow::ensure!(
                matches!(profile.auth.as_str(), "api-key" | "oauth"),
                "profile auth must be api-key or oauth"
            );
            anyhow::ensure!(
                profile.auth != "oauth"
                    || matches!(profile.kind.as_str(), "google" | "google-vertex"),
                "OAuth is supported only by official Google providers"
            );
            if let Some(base) = &profile.base_url {
                validate_provider_url(base)?;
            }
            if let Some(field) = &profile.keyring_field {
                normalize_keyring_field(field)?;
            }
            if let Some(name) = &profile.api_key_env {
                anyhow::ensure!(
                    !name.is_empty()
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "invalid api_key_env name"
                );
            }
            anyhow::ensure!(
                profile.models.iter().all(|m| !m.trim().is_empty()
                    && m.len() <= 512
                    && !m.chars().any(char::is_control)),
                "invalid configured model id"
            );
        }
        if !PROVIDER_KINDS.contains(&self.provider.as_str())
            && !self
                .provider_profiles
                .iter()
                .any(|p| p.enabled && p.id == self.provider)
        {
            anyhow::bail!(
                "invalid provider `{}` (expected proxy, anthropic, openai, google, google-vertex, or an enabled profile id)",
                self.provider
            );
        }
        if let Some(id) = self.selected_profile_id() {
            anyhow::ensure!(
                self.provider_profiles
                    .iter()
                    .any(|p| p.enabled && p.id == id),
                "selected provider profile is disabled"
            );
        }
        Ok(())
    }

    pub fn selected_profile_id(&self) -> Option<&str> {
        if let Some((id, _)) = self.model.split_once('/') {
            if let Some(profile) = self
                .provider_profiles
                .iter()
                .find(|p| p.id == id && p.enabled)
            {
                return Some(&profile.id);
            }
        }
        self.provider_profiles
            .iter()
            .find(|p| p.id == self.provider && p.enabled)
            .map(|p| p.id.as_str())
    }

    pub fn select_model(&mut self, model: &str) -> Result<()> {
        anyhow::ensure!(
            !model.trim().is_empty() && model.len() <= 512 && !model.chars().any(char::is_control),
            "model must be non-empty and at most 512 bytes"
        );
        if let Some((id, raw)) = model.split_once('/') {
            if let Some(profile) = self.provider_profiles.iter().find(|p| p.id == id) {
                anyhow::ensure!(
                    profile.enabled && !raw.trim().is_empty(),
                    "selected provider profile is disabled or model is empty"
                );
                self.provider = profile.id.clone();
            }
        }
        self.model = model.into();
        Ok(())
    }

    pub fn profile_config(&self, profile: &ProviderProfile) -> Result<Self> {
        let mut cfg = self.clone();
        cfg.provider_profiles.clear();
        cfg.provider = profile.kind.clone();
        cfg.proxy_token = None;
        cfg.openai_api_key = None;
        cfg.anthropic_api_key = None;
        cfg.google_api_key = None;
        cfg.proxy_url = profile
            .base_url
            .clone()
            .unwrap_or_else(|| "http://127.0.0.1:8787".into());
        cfg.openai_base_url = profile.base_url.clone();
        cfg.anthropic_base_url = profile.base_url.clone();
        cfg.google_base_url = profile.base_url.clone();
        cfg.google_auth = profile.auth.clone();
        cfg.google_project = profile.project.clone();
        cfg.google_location = profile.location.clone();
        if let Some((id, raw)) = self.model.split_once('/') {
            if id == profile.id {
                cfg.model = raw.into();
            }
        }
        let mut key = profile.api_key.clone().filter(|s| !s.trim().is_empty());
        if let Some(name) = &profile.api_key_env {
            if let Ok(value) = std::env::var(name) {
                key = (!value.trim().is_empty()).then_some(value);
            }
        }
        if key.is_none() && self.use_keyring == Some(true) && profile.auth != "oauth" {
            key = match &profile.keyring_field {
                Some(field) => keyring_get(field)?,
                None => profile_keyring_get(&profile.id)?,
            };
        }
        match profile.kind.as_str() {
            "proxy" => cfg.proxy_token = key,
            "openai" => cfg.openai_api_key = key,
            "anthropic" => cfg.anthropic_api_key = key,
            "google" | "google-vertex" => cfg.google_api_key = key,
            _ => anyhow::bail!("unsupported provider kind"),
        }
        Ok(cfg)
    }

    pub fn active_provider_config(&self) -> Result<Self> {
        match self.selected_profile_id() {
            Some(id) => {
                let profile = self
                    .provider_profiles
                    .iter()
                    .find(|p| p.id == id && p.enabled)
                    .context("selected provider profile is disabled")?;
                self.profile_config(profile)
            }
            None => {
                let mut cfg = self.clone();
                cfg.provider_profiles.clear();
                Ok(cfg)
            }
        }
    }

    pub fn require_provider_credentials(&self) -> Result<()> {
        if self.selected_profile_id().is_some() {
            return self
                .active_provider_config()?
                .require_provider_credentials();
        }
        let provider = self.provider.trim().to_ascii_lowercase();
        let configured = match provider.as_str() {
            "proxy" => self
                .proxy_token
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty()),
            "openai" => self
                .openai_api_key
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty()),
            "anthropic" => self
                .anthropic_api_key
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty()),
            "google" if self.google_auth == "api-key" => self
                .google_api_key
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty()),
            "google" | "google-vertex" => crate::google_auth::is_authenticated()?,
            other => anyhow::bail!("unsupported provider: {other}"),
        };
        if configured {
            Ok(())
        } else if provider == "google" && self.google_auth == "api-key" {
            anyhow::bail!(
                "provider credentials are required for 'google'; set google_api_key (or GOOGLE_API_KEY), or switch to google_auth = \"oauth\" and run `varynth google login --client-id <CLIENT_ID>`"
            )
        } else if provider == "google" || provider == "google-vertex" {
            anyhow::bail!(
                "provider credentials are required for '{provider}'; run `varynth google login --client-id <CLIENT_ID>` or set google_client_id (VARYNTH_GOOGLE_CLIENT_ID) to enable automatic sign-in"
            )
        } else {
            anyhow::bail!(
                "provider credentials are required for '{}'; configure the provider API key/token before starting the agent",
                self.provider
            )
        }
    }

    pub fn home_dir() -> PathBuf {
        if let Some(path) = std::env::var_os("VARYNTH_HOME").filter(|p| !p.is_empty()) {
            return PathBuf::from(path);
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".varynth")
    }

    /// Decide what the automatic Google sign-in preflight should do for this
    /// configuration before a model-using command starts.
    ///
    /// `Ready` means a Google OAuth credential already exists, or the selected
    /// provider does not use Google OAuth at all, so nothing must happen.
    /// `Login` means the selected provider is `google`/`google-vertex` with
    /// `google_auth = "oauth"` (directly or through a profile), no stored
    /// credential exists, auto-login is enabled and a client id is available.
    /// `Error` returns an explicit, actionable message when login cannot run.
    pub fn google_auto_login_plan(&self) -> GoogleAutoLoginPlan {
        let effective = match self.selected_profile_id() {
            Some(_) => match self.active_provider_config() {
                Ok(cfg) => cfg,
                Err(_) => return GoogleAutoLoginPlan::Ready,
            },
            None => {
                let mut cfg = self.clone();
                cfg.provider_profiles.clear();
                cfg
            }
        };
        let is_google_oauth = (effective.provider == "google-vertex"
            || (effective.provider == "google" && effective.google_auth == "oauth"))
            && effective.google_auth == "oauth";
        if !is_google_oauth {
            return GoogleAutoLoginPlan::Ready;
        }
        // A stored credential satisfies the OAuth requirement without any
        // browser flow; keyring backend failures read as "not authenticated"
        // so the login path (or its actionable error) still runs.
        match crate::google_auth::is_authenticated() {
            Ok(true) => GoogleAutoLoginPlan::Ready,
            _ => self.google_login_decision(),
        }
    }

    fn google_login_decision(&self) -> GoogleAutoLoginPlan {
        if !self.google_auto_login {
            return GoogleAutoLoginPlan::Error(
                "Google OAuth is selected without a stored credential and google_auto_login is disabled; run `varynth google login --client-id <CLIENT_ID>`"
                    .into(),
            );
        }
        match self.google_client_id.as_deref() {
            Some(id) if !id.trim().is_empty() => GoogleAutoLoginPlan::Login {
                client_id: id.trim().to_string(),
            },
            _ => GoogleAutoLoginPlan::Error(format!(
                "Google OAuth is selected but no client id is configured; set google_client_id in {} (or VARYNTH_GOOGLE_CLIENT_ID) or run `varynth google login --client-id <CLIENT_ID>`",
                Self::config_path().display()
            )),
        }
    }

    pub fn config_path() -> PathBuf {
        Self::home_dir().join("config.toml")
    }

    pub fn sessions_dir() -> PathBuf {
        Self::home_dir().join("sessions")
    }

    pub fn agent_global_dir() -> PathBuf {
        Self::home_dir()
    }

    pub fn agent_project_dir(cwd: &Path) -> PathBuf {
        cwd.join(".varynth")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            let cfg = Self::default().with_env();
            cfg.validate()?;
            return Ok(cfg);
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw).context("parse config.toml")?;
        cfg.apply_env();
        cfg.validate().context("invalid config.toml")?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let dir = Self::home_dir();
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(Self::sessions_dir())?;
        let raw = toml::to_string_pretty(&self.redacted_for_save())?;
        crate::control_bus::atomic_write(&Self::config_path(), raw.as_bytes())?;
        Ok(())
    }

    /// Returns a copy of this config safe to persist to config.toml: when
    /// `use_keyring` is enabled, secret fields are cleared so plaintext
    /// credentials never reach disk; every other field is kept as-is.
    pub fn redacted_for_save(&self) -> Config {
        let mut cfg = self.clone();
        if self.use_keyring == Some(true) {
            cfg.proxy_token = None;
            cfg.anthropic_api_key = None;
            cfg.openai_api_key = None;
            cfg.google_api_key = None;
            for profile in &mut cfg.provider_profiles {
                profile.api_key = None;
            }
            cfg.telegram_bot_token = None;
            cfg.dashboard_token = None;
        }
        cfg
    }

    pub fn with_env(mut self) -> Self {
        self.apply_env();
        if let Err(e) = self.validate() {
            eprintln!("config warning: {e}");
        }
        self
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("VARYNTH_MODEL") {
            self.model = v;
        }
        if let Ok(v) = std::env::var("VARYNTH_PROVIDER") {
            self.provider = v;
        }
        if let Ok(v) = std::env::var("VARYNTH_PROFILE") {
            self.provider = v;
        }
        if let Ok(v) = std::env::var("VARYNTH_PROXY_URL") {
            self.proxy_url = v;
        }
        if let Ok(v) = std::env::var("VARYNTH_PROXY_TOKEN") {
            self.proxy_token = Some(v);
        }
        if let Ok(v) = std::env::var("ANTHROPIC_API_KEY") {
            self.anthropic_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("OPENAI_API_KEY") {
            self.openai_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("ANTHROPIC_BASE_URL") {
            self.anthropic_base_url = Some(v);
        }
        if let Ok(v) = std::env::var("OPENAI_BASE_URL") {
            self.openai_base_url = Some(v);
        }
        if let Ok(v) = std::env::var("GOOGLE_API_KEY").or_else(|_| std::env::var("GEMINI_API_KEY"))
        {
            self.google_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("GOOGLE_BASE_URL") {
            self.google_base_url = Some(v);
        }
        if let Ok(v) = std::env::var("GOOGLE_CLOUD_PROJECT") {
            self.google_project = Some(v);
        }
        if let Ok(v) = std::env::var("GOOGLE_CLOUD_LOCATION") {
            self.google_location = Some(v);
        }
        if let Ok(v) = std::env::var("VARYNTH_GOOGLE_AUTH") {
            self.google_auth = v;
        }
        if let Ok(v) = std::env::var("VARYNTH_GOOGLE_CLIENT_ID") {
            if !v.trim().is_empty() {
                self.google_client_id = Some(v);
            }
        }
        if let Ok(v) = std::env::var("VARYNTH_GOOGLE_AUTO_LOGIN") {
            match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => self.google_auto_login = true,
                "0" | "false" | "no" | "off" | "" => self.google_auto_login = false,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("VARYNTH_DASHBOARD_PORT") {
            if let Ok(p) = v.parse() {
                self.dashboard_port = p;
            }
        }
        if let Ok(v) = std::env::var("VARYNTH_DASHBOARD_TOKEN") {
            if !v.trim().is_empty() {
                self.dashboard_token = Some(v);
            }
        }
        if let Ok(v) = std::env::var("VARYNTH_TELEGRAM_BOT_TOKEN") {
            if !v.is_empty() {
                self.telegram_bot_token = Some(v);
            }
        } else if self.telegram_bot_token.is_none() {
            self.telegram_bot_token = load_telegram_token();
        }
        if let Ok(v) = std::env::var("VARYNTH_TELEGRAM_ALLOW") {
            self.telegram_allow_from = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("VARYNTH_MCP_CONFIG") {
            if !v.trim().is_empty() {
                self.mcp_config = Some(PathBuf::from(v));
            }
        }
        if self.use_keyring == Some(true) {
            self.apply_keyring_secrets();
        }
    }

    /// Fills secret fields that are still missing after env overrides from
    /// the OS keyring. Missing entries are silently ignored; backend failures
    /// produce a single warning and never abort configuration loading.
    fn apply_keyring_secrets(&mut self) {
        let secrets = [
            ("proxy-token", &mut self.proxy_token),
            ("anthropic-api-key", &mut self.anthropic_api_key),
            ("openai-api-key", &mut self.openai_api_key),
            ("google-api-key", &mut self.google_api_key),
            ("telegram-bot-token", &mut self.telegram_bot_token),
            ("dashboard-token", &mut self.dashboard_token),
        ];
        for (field, slot) in secrets {
            let missing = slot.as_deref().is_none_or(|v| v.trim().is_empty());
            if !missing {
                continue;
            }
            match keyring_get(field) {
                Ok(Some(value)) if !value.is_empty() => *slot = Some(value),
                Ok(_) => {}
                Err(e) => tracing::warn!(field, error = %e, "keyring read failed; skipping secret"),
            }
        }
    }

    pub fn ensure_home() -> Result<()> {
        fs::create_dir_all(Self::home_dir())?;
        fs::create_dir_all(Self::sessions_dir())?;
        if !Self::config_path().exists() {
            Self::default().save()?;
        }
        Ok(())
    }

    pub fn load_project_instructions(cwd: &Path) -> String {
        let mut out = String::new();
        for name in ["VARYNTH.md", "AGENTS.md", "CLAUDE.md"] {
            let p = cwd.join(name);
            if let Ok(s) = fs::read_to_string(&p) {
                out.push_str(&format!("\n\n# {name}\n{s}"));
            }
        }
        out
    }
}

pub fn validate_provider_url(raw: &str) -> Result<()> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid provider base URL"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "provider URL must be HTTP(S)"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "provider URL cannot contain credentials, query, or fragment"
    );
    Ok(())
}

pub fn profile_keyring_name(id: &str) -> Result<String> {
    anyhow::ensure!(valid_profile_id(id), "invalid provider profile id");
    Ok(format!("varynth/provider-profile/{id}/api-key"))
}

fn profile_keyring_entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new("varynth", &profile_keyring_name(id)?).context("open profile keyring entry")
}

pub fn profile_keyring_get(id: &str) -> Result<Option<String>> {
    match profile_keyring_entry(id)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => anyhow::bail!("profile keyring read failed"),
    }
}

pub fn profile_keyring_set(id: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty() && !value.chars().any(char::is_control),
        "invalid profile credential"
    );
    profile_keyring_entry(id)?
        .set_password(value)
        .map_err(|_| anyhow::anyhow!("profile keyring write failed"))
}

pub fn profile_keyring_delete(id: &str) -> Result<()> {
    match profile_keyring_entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => anyhow::bail!("profile keyring delete failed"),
    }
}

/// Secret fields that can live in the OS keyring (service `varynth`), in
/// their canonical dash-separated form.
pub const KEYRING_FIELDS: [&str; 6] = [
    "proxy-token",
    "anthropic-api-key",
    "openai-api-key",
    "google-api-key",
    "telegram-bot-token",
    "dashboard-token",
];

/// Maps a user-supplied field name onto a canonical [`KEYRING_FIELDS`] entry.
/// Accepts underscores for dashes and any letter casing; unknown fields are
/// rejected with an error listing the valid names.
pub fn normalize_keyring_field(field: &str) -> Result<&'static str> {
    let candidate = field.trim().replace('_', "-").to_ascii_lowercase();
    KEYRING_FIELDS
        .iter()
        .copied()
        .find(|known| *known == candidate)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown keyring field `{}` (expected one of: {})",
                field.trim(),
                KEYRING_FIELDS.join(", ")
            )
        })
}

fn keyring_entry(field: &str) -> Result<keyring::Entry> {
    let name = format!("varynth/{field}");
    keyring::Entry::new("varynth", &name)
        .with_context(|| format!("open keyring entry varynth/{field}"))
}

/// Reads a secret from the OS keyring. Returns `Ok(None)` when no entry
/// exists; other backend failures are reported as context-rich errors.
pub fn keyring_get(field: &str) -> Result<Option<String>> {
    let field = normalize_keyring_field(field)?;
    let entry = keyring_entry(field)?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read keyring entry varynth/{field}")),
    }
}

/// Writes a secret to the OS keyring, replacing any existing value.
pub fn keyring_set(field: &str, value: &str) -> Result<()> {
    let field = normalize_keyring_field(field)?;
    let entry = keyring_entry(field)?;
    entry
        .set_password(value)
        .with_context(|| format!("write keyring entry varynth/{field}"))
}

/// Removes a secret from the OS keyring. Deleting a missing entry is a
/// no-op; other backend failures are reported as context-rich errors.
pub fn keyring_delete(field: &str) -> Result<()> {
    let field = normalize_keyring_field(field)?;
    let entry = keyring_entry(field)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("delete keyring entry varynth/{field}")),
    }
}

/// Reports which keyring fields currently hold a secret, in
/// [`KEYRING_FIELDS`] order. Backend failures read as "absent".
pub fn keyring_status() -> Vec<(String, bool)> {
    KEYRING_FIELDS
        .iter()
        .map(|field| {
            let present = keyring_get(field).map(|v| v.is_some()).unwrap_or(false);
            ((*field).to_string(), present)
        })
        .collect()
}

fn load_telegram_token() -> Option<String> {
    let path = Config::home_dir().join("telegram.env");
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("TELEGRAM_BOT_TOKEN=") {
            let tok = rest.trim().trim_matches('"').trim_matches('\'');
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
        if let Some(rest) = line.strip_prefix("VARYNTH_TELEGRAM_BOT_TOKEN=") {
            let tok = rest.trim().trim_matches('"').trim_matches('\'');
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn mutated(f: impl FnOnce(&mut Config)) -> String {
        let mut cfg = Config::default();
        f(&mut cfg);
        cfg.validate().unwrap_err().to_string()
    }

    #[test]
    fn validate_accepts_defaults_and_known_values() {
        Config::default().validate().unwrap();
        let mut cfg = Config::default();
        cfg.provider = "openai".into();
        cfg.permission_mode = "prompt".into();
        cfg.sandbox = "danger-full-access".into();
        cfg.effort = "low".into();
        cfg.validate().unwrap();
        cfg.effort = "ultra".into();
        cfg.validate().unwrap();
    }

    #[test]
    fn validate_rejects_unknown_values() {
        assert!(mutated(|c| c.provider = "azure".into()).contains("proxy"));
        assert!(mutated(|c| c.permission_mode = "yolo".into()).contains("bypass"));
        assert!(mutated(|c| c.sandbox = "off".into()).contains("workspace-write"));
        assert!(mutated(|c| c.effort = "mega".into()).contains("ultra"));
    }

    /// Runs `f` with the given env vars forced to a value (or removed), then
    /// restores the previous values. Serializes all env-mutating tests.
    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, saved) in saved {
            match saved {
                Some(v) => std::env::set_var(k, &v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn varynth_proxy_token_env_sets_proxy_token() {
        with_env(&[("VARYNTH_PROXY_TOKEN", Some("proxy-tok-123"))], || {
            let cfg = Config::default().with_env();
            assert_eq!(cfg.proxy_token.as_deref(), Some("proxy-tok-123"));
        });
    }

    #[test]
    fn anthropic_auth_token_env_does_not_set_proxy_token() {
        with_env(
            &[
                ("ANTHROPIC_AUTH_TOKEN", Some("sk-ant-secret")),
                ("VARYNTH_PROXY_TOKEN", None),
            ],
            || {
                let cfg = Config::default().with_env();
                assert!(cfg.proxy_token.is_none());
            },
        );
    }

    #[test]
    fn normalize_keyring_field_accepts_dashes_underscores_and_case() {
        for (input, expected) in [
            ("proxy-token", "proxy-token"),
            ("proxy_token", "proxy-token"),
            ("PROXY-TOKEN", "proxy-token"),
            ("Proxy_Token", "proxy-token"),
            ("anthropic_api_key", "anthropic-api-key"),
            ("OPENAI_API_KEY", "openai-api-key"),
            ("telegram-bot-token", "telegram-bot-token"),
            ("TELEGRAM_BOT_TOKEN", "telegram-bot-token"),
            ("dashboard-token", "dashboard-token"),
            ("Dashboard_Token", "dashboard-token"),
        ] {
            assert_eq!(normalize_keyring_field(input).unwrap(), expected);
        }
    }

    #[test]
    fn normalize_keyring_field_rejects_unknown() {
        let err = normalize_keyring_field("nope").unwrap_err().to_string();
        assert!(err.contains("nope"), "error should echo input: {err}");
        for field in KEYRING_FIELDS {
            assert!(err.contains(field), "error should list `{field}`: {err}");
        }
    }

    #[test]
    fn redacted_for_save_hides_secrets_when_keyring_enabled() {
        let mut cfg = Config::default();
        cfg.use_keyring = Some(true);
        cfg.proxy_token = Some("secret-proxy".into());
        cfg.anthropic_api_key = Some("secret-anthropic".into());
        cfg.openai_api_key = Some("secret-openai".into());
        cfg.telegram_bot_token = Some("secret-telegram".into());
        cfg.dashboard_token = Some("secret-dashboard".into());
        let raw = toml::to_string_pretty(&cfg.redacted_for_save()).unwrap();
        for secret in [
            "secret-proxy",
            "secret-anthropic",
            "secret-openai",
            "secret-telegram",
            "secret-dashboard",
        ] {
            assert!(
                !raw.contains(secret),
                "redacted config leaked `{secret}`:\n{raw}"
            );
        }
        // Non-secret fields must survive redaction untouched.
        assert!(raw.contains("grok-4.7-build"));
        assert!(raw.contains("use_keyring"));
    }

    #[test]
    fn redacted_for_save_keeps_secrets_when_keyring_disabled() {
        for use_keyring in [None, Some(false)] {
            let mut cfg = Config::default();
            cfg.use_keyring = use_keyring;
            cfg.proxy_token = Some("secret-proxy".into());
            cfg.anthropic_api_key = Some("secret-anthropic".into());
            cfg.openai_api_key = Some("secret-openai".into());
            cfg.telegram_bot_token = Some("secret-telegram".into());
            cfg.dashboard_token = Some("secret-dashboard".into());
            let raw = toml::to_string_pretty(&cfg.redacted_for_save()).unwrap();
            for secret in [
                "secret-proxy",
                "secret-anthropic",
                "secret-openai",
                "secret-telegram",
                "secret-dashboard",
            ] {
                assert!(
                    raw.contains(secret),
                    "use_keyring={use_keyring:?} should keep `{secret}`:\n{raw}"
                );
            }
        }
    }

    #[test]
    fn apply_env_keyring_missing_entries_leaves_secrets_none() {
        with_env(
            &[
                ("VARYNTH_PROXY_TOKEN", None),
                ("ANTHROPIC_API_KEY", None),
                ("OPENAI_API_KEY", None),
                ("VARYNTH_TELEGRAM_BOT_TOKEN", None),
                ("VARYNTH_DASHBOARD_TOKEN", None),
            ],
            || {
                // Skip when this machine's keyring already holds real varynth
                // entries: the test must not depend on (or disturb) them, and
                // a legitimate fill would contradict the "stays None" premise.
                if keyring_status().iter().any(|(_, present)| *present) {
                    return;
                }
                let telegram_env_token = load_telegram_token();
                let mut cfg = Config::default();
                cfg.use_keyring = Some(true);
                cfg.apply_env();
                assert!(cfg.proxy_token.is_none());
                assert!(cfg.anthropic_api_key.is_none());
                assert!(cfg.openai_api_key.is_none());
                if telegram_env_token.is_none() {
                    assert!(cfg.telegram_bot_token.is_none());
                }
                assert!(cfg.dashboard_token.is_none());
            },
        );
    }

    /// Optional round-trip against the real backend. Skips (early-return)
    /// whenever the backend is unusable, and restores any pre-existing value
    /// so real user data is never lost.
    #[test]
    fn keyring_roundtrip_set_get_delete() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original = match keyring_get("proxy-token") {
            Ok(v) => v,
            Err(_) => return, // backend unusable; skip
        };
        if keyring_set("proxy-token", "varynth-roundtrip-secret").is_err() {
            return; // backend unusable; skip
        }
        let got = keyring_get("proxy-token");
        let deleted = keyring_delete("proxy-token");
        // Restore any pre-existing user value before asserting, so a failure
        // above can never lose real data.
        if let Some(v) = &original {
            let _ = keyring_set("proxy-token", v);
        }
        assert_eq!(
            got.expect("keyring get after set").as_deref(),
            Some("varynth-roundtrip-secret")
        );
        deleted.expect("keyring delete after set");
    }
}
