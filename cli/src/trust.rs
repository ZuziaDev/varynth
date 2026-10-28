//! Project trust: the workspace allowlist that decides whether repo-supplied
//! policy files may take effect.
//!
//! A cloned repository can ship `.varynth/permissions.json`, hooks, commands,
//! skills and an MCP registry. Loading those from an untrusted clone would
//! let the repository grant itself tool permissions or run stdio servers, so
//! every project-scoped file is inert until the user trusts the workspace by
//! its canonical path.
//! The decision lives outside the repository in `~/.varynth/trust.json`,
//! keyed by a hash of the canonical path, so moving or copying the clone does
//! not carry trust with it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;

/// Project-scoped files that require a trust decision before they load.
const PROJECT_SCOPED: [&str; 6] = [
    ".varynth/permissions.json",
    ".varynth/hooks.json",
    ".varynth/commands",
    // A project MCP registry spawns arbitrary stdio servers, so it needs the
    // same trust decision as the policy files above. An explicit
    // `--mcp-config` remains the user's own opt-in.
    ".varynth/mcp.json",
    // Skill bodies are injected into the prompt; a clone must not do that
    // before the user trusts it.
    ".varynth/skills",
    ".claude/skills",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustEntry {
    /// Canonical workspace path as recorded when the user trusted it.
    pub path: String,
    /// RFC 3339 timestamp of the decision.
    pub trusted_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default = "trust_version")]
    version: u32,
    #[serde(default)]
    workspaces: BTreeMap<String, TrustEntry>,
}

fn trust_version() -> u32 {
    1
}

/// One persisted trust decision store. Tests create one at an explicit path;
/// the CLI and runtime use [`TrustStore::load`] over the Varynth home.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    data: TrustFile,
}

impl TrustStore {
    pub fn path() -> PathBuf {
        Config::home_dir().join("trust.json")
    }

    pub fn at(path: PathBuf) -> Self {
        Self {
            data: read_file(&path),
            path,
        }
    }

    /// Load the real store. A missing or unreadable file reads as empty: trust
    /// is opt-in, and a corrupt store must never grant trust.
    pub fn load() -> Self {
        Self::at(Self::path())
    }

    pub fn is_trusted(&self, workspace: &Path) -> bool {
        self.data.workspaces.contains_key(&workspace_key(workspace))
    }

    pub fn entries(&self) -> Vec<TrustEntry> {
        self.data.workspaces.values().cloned().collect()
    }

    /// Record the decision for one workspace and persist the store.
    pub fn trust(&mut self, workspace: &Path) -> Result<TrustEntry> {
        let canonical = canonical_workspace(workspace);
        let entry = TrustEntry {
            path: display_path(&canonical),
            trusted_at: chrono::Utc::now().to_rfc3339(),
        };
        self.data
            .workspaces
            .insert(workspace_key(workspace), entry.clone());
        self.save()?;
        Ok(entry)
    }

    /// Drop the decision. Returns whether an entry existed.
    pub fn revoke(&mut self, workspace: &Path) -> Result<bool> {
        let removed = self
            .data
            .workspaces
            .remove(&workspace_key(workspace))
            .is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let raw = serde_json::to_vec_pretty(&self.data)?;
        crate::control_bus::atomic_write(&self.path, &raw)
            .with_context(|| format!("write {}", self.path.display()))
    }
}

fn read_file(path: &Path) -> TrustFile {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return TrustFile {
            version: trust_version(),
            workspaces: BTreeMap::new(),
        };
    };
    match serde_json::from_str::<TrustFile>(&raw) {
        Ok(mut file) => {
            if file.version == 0 {
                file.version = trust_version();
            }
            file
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring unreadable trust.json");
            TrustFile {
                version: trust_version(),
                workspaces: BTreeMap::new(),
            }
        }
    }
}

/// Canonical absolute workspace path. Non-existent paths still normalize to an
/// absolute lexical path so a decision can be recorded before first use.
pub fn canonical_workspace(workspace: &Path) -> PathBuf {
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(workspace)
    };
    match absolute.canonicalize() {
        Ok(path) => path,
        Err(_) => absolute,
    }
}

/// Stable identifier for a workspace: SHA-256 over the canonical path with
/// separators normalized and, on Windows, case folded (NTFS is
/// case-insensitive, so `D:/Repo` and `d:\repo` are one workspace).
pub fn workspace_key(workspace: &Path) -> String {
    let canonical = canonical_workspace(workspace);
    let mut normalized = display_path(&canonical).replace('\\', "/");
    if cfg!(windows) {
        normalized = normalized.to_lowercase();
    }
    let digest = Sha256::digest(normalized.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Project-scoped files present in this workspace (relative paths).
pub fn pending_project_files(workspace: &Path) -> Vec<String> {
    PROJECT_SCOPED
        .iter()
        .filter(|rel| workspace.join(rel).exists())
        .map(|rel| (*rel).to_string())
        .collect()
}

/// Whether project-scoped files in `workspace` may take effect.
pub fn is_trusted(workspace: &Path) -> bool {
    TrustStore::load().is_trusted(workspace)
}

/// Trust a workspace by canonical path. Idempotent.
pub fn trust(workspace: &Path) -> Result<TrustEntry> {
    TrustStore::load().trust(workspace)
}

/// Revoke a workspace's decision. Returns whether one existed.
pub fn revoke(workspace: &Path) -> Result<bool> {
    TrustStore::load().revoke(workspace)
}

/// Ensure the workspace may activate its project-scoped files.
///
/// * already trusted → `true`
/// * no project-scoped file → `false` (nothing to activate, no prompt)
/// * untrusted with files → ask on the terminal when `interactive`, otherwise
///   leave the files inert and return `false`.
///
/// The prompt is the only place that grants trust implicitly; every other
/// caller must go through `varynth trust`.
pub fn ensure_trusted(workspace: &Path, interactive: bool) -> Result<bool> {
    ensure_trusted_with(&TrustStore::load(), workspace, interactive)
}

/// The user's profile directory (`%USERPROFILE%` on Windows, `$HOME` on
/// Unix), canonicalized when it exists.
fn user_home_dir() -> Option<PathBuf> {
    ["USERPROFILE", "HOME"]
        .iter()
        .find_map(|name| std::env::var_os(name))
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok())
}

fn is_user_home(workspace: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    workspace == home
}

/// Whether this workspace is the user's own home directory. Project-scoped
/// files there are user-level config and never need a trust decision.
pub fn is_user_home_workspace(workspace: &Path) -> bool {
    is_user_home(workspace, user_home_dir().as_deref())
}

fn ensure_trusted_with(store: &TrustStore, workspace: &Path, interactive: bool) -> Result<bool> {
    if store.is_trusted(workspace) {
        return Ok(true);
    }
    // Launching from the user's home directory is not a cloned workspace:
    // `~/.varynth` and `~/.claude/skills` there are user-scoped files, already
    // trusted by being the user's own config. Prompting would block the TUI
    // on every start from `C:\Users\<name>`.
    if is_user_home(workspace, user_home_dir().as_deref()) {
        return Ok(false);
    }
    let pending = pending_project_files(workspace);
    if pending.is_empty() {
        return Ok(false);
    }
    if !interactive {
        tracing::warn!(
            workspace = %canonical_workspace(workspace).display(),
            files = %pending.join(", "),
            "project-scoped policy ignored: workspace is not trusted"
        );
        return Ok(false);
    }
    eprintln!(
        "\nThis workspace ships project-scoped policy:\n  {}\n\
         It is ignored until you trust the workspace. Trust {}? [y/N]",
        pending.join("\n  "),
        canonical_workspace(workspace).display()
    );
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        eprintln!("not trusted; project-scoped policy stays inert");
        return Ok(false);
    }
    let mut store = store.clone();
    store.trust(workspace)?;
    eprintln!("trusted {}", canonical_workspace(workspace).display());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn home_directory_is_not_a_project_that_needs_trust() {
        let home = tempdir().unwrap();
        let canonical_home = home.path().canonicalize().unwrap();
        assert!(is_user_home(home.path(), Some(&canonical_home)));
        let project = tempdir().unwrap();
        assert!(!is_user_home(project.path(), Some(&canonical_home)));
        assert!(!is_user_home(home.path(), None));
    }

    #[test]
    fn trust_round_trips_and_survives_a_reload() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let store_path = home.path().join("trust.json");
        let mut store = TrustStore::at(store_path.clone());
        assert!(!store.is_trusted(workspace.path()));

        let entry = store.trust(workspace.path()).unwrap();
        assert_eq!(
            entry.path,
            canonical_workspace(workspace.path()).display().to_string()
        );
        assert!(store.is_trusted(workspace.path()));
        // A fresh store reads the same decision off disk.
        assert!(TrustStore::at(store_path.clone()).is_trusted(workspace.path()));

        assert!(store.revoke(workspace.path()).unwrap());
        assert!(!store.is_trusted(workspace.path()));
        assert!(!store.revoke(workspace.path()).unwrap());
    }

    #[test]
    fn workspace_key_is_canonical_and_case_folded_on_windows() {
        let workspace = tempdir().unwrap();
        let key = workspace_key(workspace.path());
        let dotted = workspace.path().join(".");
        assert_eq!(key, workspace_key(&dotted));
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        if cfg!(windows) {
            let upper = PathBuf::from(
                canonical_workspace(workspace.path())
                    .to_string_lossy()
                    .to_uppercase(),
            );
            assert_eq!(key, workspace_key(&upper));
        }
    }

    #[test]
    fn ensure_trusted_is_inert_and_silent_when_nothing_is_project_scoped() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let store = TrustStore::at(home.path().join("trust.json"));
        assert!(!ensure_trusted_with(&store, workspace.path(), false).unwrap());
        assert!(pending_project_files(workspace.path()).is_empty());
    }

    #[test]
    fn a_cloned_repository_cannot_grant_itself_trust() {
        let home = tempdir().unwrap();
        let clone = tempdir().unwrap();
        std::fs::create_dir_all(clone.path().join(".varynth")).unwrap();
        std::fs::write(
            clone.path().join(".varynth/permissions.json"),
            r#"{"rules":[{"tool":"write_file","match":"*","effect":"allow"}]}"#,
        )
        .unwrap();
        assert_eq!(
            pending_project_files(clone.path()),
            vec![".varynth/permissions.json".to_string()]
        );
        let store = TrustStore::at(home.path().join("trust.json"));
        assert!(!ensure_trusted_with(&store, clone.path(), false).unwrap());
        // Still not trusted, and nothing was written for it.
        assert!(!TrustStore::at(home.path().join("trust.json")).is_trusted(clone.path()));
    }

    #[test]
    fn project_skill_roots_participate_in_the_trust_prompt() {
        let home = tempdir().unwrap();
        let clone = tempdir().unwrap();
        for rel in [".varynth/skills/one", ".claude/skills/two"] {
            let dir = clone.path().join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "---\nname: x\n---\nbody\n").unwrap();
        }
        let pending = pending_project_files(clone.path());
        assert!(
            pending.contains(&".varynth/skills".to_string()),
            "a repo shipping only project skills must trigger the prompt: {pending:?}"
        );
        assert!(
            pending.contains(&".claude/skills".to_string()),
            "the Claude skill root is project-scoped too: {pending:?}"
        );
        let mut store = TrustStore::at(home.path().join("trust.json"));
        assert!(!ensure_trusted_with(&store, clone.path(), false).unwrap());
        // Accepting the decision makes the same check admit the workspace,
        // which is what lets the runtime discover project skills.
        store.trust(clone.path()).unwrap();
        assert!(ensure_trusted_with(&store, clone.path(), false).unwrap());
    }

    #[test]
    fn a_project_mcp_registry_requires_trust() {
        let home = tempdir().unwrap();
        let clone = tempdir().unwrap();
        std::fs::create_dir_all(clone.path().join(".varynth")).unwrap();
        std::fs::write(
            clone.path().join(".varynth/mcp.json"),
            r#"{"mcpServers":{"x":{"type":"stdio","command":"cmd","args":["/c","calc"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            pending_project_files(clone.path()),
            vec![".varynth/mcp.json".to_string()]
        );
        let store = TrustStore::at(home.path().join("trust.json"));
        assert!(!ensure_trusted_with(&store, clone.path(), false).unwrap());
        assert!(!TrustStore::at(home.path().join("trust.json")).is_trusted(clone.path()));
    }
}
