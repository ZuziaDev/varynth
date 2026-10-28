use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::Config;

pub const CORE_PERSONA_FILE_NAMES: [&str; 5] = [
    "SOUL.md",
    "AGENT.md",
    "HEART.md",
    "DREAM.md",
    "USER.md",
];

pub const AGENT_FILE_NAMES: [&str; 6] = [
    "SOUL.md",
    "AGENT.md",
    "HEART.md",
    "DREAM.md",
    "USER.md",
    "MEMORY.md",
];

pub const MAX_AGENT_FILE_BYTES: usize = 64 * 1024;
pub const MAX_SYSTEM_CONTEXT_BYTES: usize = 128 * 1024;
pub const MAX_RECENT_DAILY_NOTES: usize = 5;

const DEFAULT_SOUL: &str =
    "# Soul\n\nDefine the agent's core philosophy, craftsmanship standards, and ethical boundaries here.\n";
const DEFAULT_AGENT: &str =
    "# Agent\n\nDefine technical capabilities, tool routing protocols, sandbox policies, and multi-agent roles here.\n";
const DEFAULT_HEART: &str =
    "# Heart\n\nDefine empathetic alignment, collaborative patience, and constructive error recovery here.\n";
const DEFAULT_DREAM: &str =
    "# Dream\n\nDefine long-term autonomous workspace goals, self-improving skills, and architectural horizons here.\n";
const DEFAULT_USER: &str =
    "# User\n\nRecord stable developer preferences, toolchain environment, and active project context here.\n";
const DEFAULT_MEMORY: &str = "# Memory\n\nDurable facts and decisions are appended here.\n";
const DEFAULT_VAULT_INDEX: &str =
    "# Varynth Memory Vault (`MEMORY/index.md`)\n\nCentral knowledge graph linking `[[concepts/]]`, `[[decisions/]]`, and `[[daily/]]` logs.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFileScope {
    Global,
    Project,
}

impl AgentFileScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedFile {
    pub name: String,
    pub scope: AgentFileScope,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct AgentFiles {
    cwd: PathBuf,
    global_dir: PathBuf,
    project_dir: PathBuf,
    files: Vec<LoadedFile>,
}

impl AgentFiles {
    pub fn load(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let global_dir = Config::agent_global_dir();
        let project_dir = Config::agent_project_dir(&cwd);
        let mut files = Vec::new();
        load_layer(&mut files, &global_dir, AgentFileScope::Global)?;
        load_layer(&mut files, &project_dir, AgentFileScope::Project)?;
        Ok(Self {
            cwd,
            global_dir,
            project_dir,
            files,
        })
    }

    pub fn ensure(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd.as_ref();
        let global_dir = Config::agent_global_dir();
        let project_dir = Config::agent_project_dir(cwd);
        ensure_layer(&global_dir)?;
        ensure_layer(&project_dir)?;
        Self::load(cwd)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub fn vault_dir(&self) -> PathBuf {
        self.project_dir.join("MEMORY")
    }

    pub fn files(&self) -> &[LoadedFile] {
        &self.files
    }

    pub fn system_context(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            let content = file
                .content
                .replace("</varynth-agent-file>", "<\\/varynth-agent-file>");
            let block = format!(
                "\n\n<varynth-agent-file name=\"{}\" scope=\"{}\">\n{}\n</varynth-agent-file>",
                file.name,
                file.scope.label(),
                content
            );
            if out.len() + block.len() > MAX_SYSTEM_CONTEXT_BYTES {
                let remaining = MAX_SYSTEM_CONTEXT_BYTES.saturating_sub(out.len());
                if remaining > 0 {
                    out.push_str(&truncate_with_marker(&block, remaining));
                }
                break;
            }
            out.push_str(&block);
        }
        out
    }

    pub fn memory(&self) -> String {
        self.files
            .iter()
            .filter(|file| file.name == "MEMORY.md" || file.name.starts_with("MEMORY/"))
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn append_memory(&mut self, entry: &str) -> Result<()> {
        let entry = entry.trim();
        if entry.is_empty() {
            return Ok(());
        }
        fs::create_dir_all(&self.project_dir)
            .with_context(|| format!("create {}", self.project_dir.display()))?;
        let path = existing_path(&self.project_dir, "MEMORY.md");
        let current = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let addition = format!("\n\n{entry}\n");
        if current.len() + addition.len() > MAX_AGENT_FILE_BYTES {
            anyhow::bail!(
                "{} exceeds the {} byte limit",
                path.display(),
                MAX_AGENT_FILE_BYTES
            );
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.write_all(addition.as_bytes())?;

        let vault_index = self.vault_dir().join("index.md");
        if vault_index.is_file() {
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let daily_rel = format!("daily/{today}.md");
            let daily_path = self.vault_dir().join(&daily_rel);
            if let Some(parent) = daily_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let daily_current = fs::read_to_string(&daily_path).unwrap_or_default();
            if daily_current.len() + addition.len() <= MAX_AGENT_FILE_BYTES {
                if let Ok(mut daily_file) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&daily_path)
                {
                    let _ = daily_file.write_all(addition.as_bytes());
                }
            }
        }

        self.reload()?;
        Ok(())
    }

    /// Reads a note from `.varynth/MEMORY/` by relative path or wikilink target
    /// (e.g., `"index.md"`, `"concepts/gateway-protocol"`, `"decisions/001-obsidian-memory-vault.md"`).
    pub fn read_vault_note(&self, note_ref: &str) -> Result<String> {
        let path = self
            .resolve_wikilink(note_ref)
            .or_else(|| sanitize_vault_relative_path(&self.vault_dir(), note_ref))
            .with_context(|| format!("vault note not found: {note_ref}"))?;
        if !path.is_file() {
            anyhow::bail!("vault note does not exist: {}", path.display());
        }
        read_limited(&path)
    }

    /// Writes or overwrites a note inside `.varynth/MEMORY/<relative_path>` and reloads the vault.
    pub fn write_vault_note(&mut self, relative_path: &str, content: &str) -> Result<PathBuf> {
        if content.len() > MAX_AGENT_FILE_BYTES {
            anyhow::bail!(
                "vault note {} exceeds the {} byte limit",
                relative_path,
                MAX_AGENT_FILE_BYTES
            );
        }
        let vault_root = self.vault_dir();
        let target = sanitize_vault_relative_path(&vault_root, relative_path)
            .with_context(|| format!("invalid vault note path: {relative_path}"))?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&target, content).with_context(|| format!("write {}", target.display()))?;
        self.reload()?;
        Ok(target)
    }

    /// Appends an entry to a note inside `.varynth/MEMORY/<relative_path>` and reloads the vault.
    pub fn append_vault_note(&mut self, relative_path: &str, entry: &str) -> Result<PathBuf> {
        let entry = entry.trim();
        let vault_root = self.vault_dir();
        let target = sanitize_vault_relative_path(&vault_root, relative_path)
            .with_context(|| format!("invalid vault note path: {relative_path}"))?;
        if entry.is_empty() {
            return Ok(target);
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let current = match fs::read_to_string(&target) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err).with_context(|| format!("read {}", target.display())),
        };
        let addition = if current.is_empty() {
            format!("{entry}\n")
        } else {
            format!("\n\n{entry}\n")
        };
        if current.len() + addition.len() > MAX_AGENT_FILE_BYTES {
            anyhow::bail!(
                "{} exceeds the {} byte limit",
                target.display(),
                MAX_AGENT_FILE_BYTES
            );
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&target)
            .with_context(|| format!("open {}", target.display()))?;
        file.write_all(addition.as_bytes())?;
        self.reload()?;
        Ok(target)
    }

    /// Resolves an Obsidian `[[wikilink]]` target (or `[[target|alias]]`) against the project or global `MEMORY/` vault.
    pub fn resolve_wikilink(&self, link: &str) -> Option<PathBuf> {
        resolve_wikilink_in_vault(&self.vault_dir(), link)
            .or_else(|| resolve_wikilink_in_vault(&self.global_dir.join("MEMORY"), link))
    }

    fn reload(&mut self) -> Result<()> {
        let fresh = Self::load(&self.cwd)?;
        self.files = fresh.files;
        Ok(())
    }
}

/// Extracts normalized Obsidian `[[wikilink]]` targets from Markdown content in appearance order.
/// Supports `[[target]]`, `[[target|alias]]`, and `[[target#heading|alias]]`, while ignoring image embeds `![[...]]`.
pub fn extract_wikilinks(markdown: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let bytes = markdown.as_bytes();
    let mut idx = 0;

    while idx + 1 < bytes.len() {
        if bytes[idx] == b'[' && bytes[idx + 1] == b'[' {
            // Skip embedded images/media like ![[image.png]]
            if idx > 0 && bytes[idx - 1] == b'!' {
                idx += 2;
                continue;
            }
            let start = idx + 2;
            let mut end = start;
            while end + 1 < bytes.len() && !(bytes[end] == b']' && bytes[end + 1] == b']') {
                if bytes[end] == b'\n' || bytes[end] == b'[' {
                    break;
                }
                end += 1;
            }
            if end + 1 < bytes.len() && bytes[end] == b']' && bytes[end + 1] == b']' {
                if let Ok(raw_inner) = std::str::from_utf8(&bytes[start..end]) {
                    if let Some(cleaned) = normalize_wikilink_target(raw_inner) {
                        if seen.insert(cleaned.clone()) {
                            out.push(cleaned);
                        }
                    }
                }
                idx = end + 2;
                continue;
            }
        }
        idx += 1;
    }
    out
}

fn normalize_wikilink_target(raw: &str) -> Option<String> {
    let before_alias = raw.split('|').next().unwrap_or("").trim();
    let before_heading = before_alias.split('#').next().unwrap_or("").trim();
    let normalized = before_heading.replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    // Reject path traversal segments
    if trimmed.split('/').any(|seg| seg == ".." || seg == ".") {
        return None;
    }
    Some(trimmed.to_string())
}

fn sanitize_vault_relative_path(vault_dir: &Path, raw: &str) -> Option<PathBuf> {
    let cleaned = normalize_wikilink_target(raw)?;
    let with_ext = if cleaned.to_ascii_lowercase().ends_with(".md") {
        cleaned
    } else {
        format!("{cleaned}.md")
    };
    let mut path = vault_dir.to_path_buf();
    for segment in with_ext.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
        path.push(segment);
    }
    Some(path)
}

pub fn resolve_wikilink_in_vault(vault_dir: &Path, link: &str) -> Option<PathBuf> {
    let wrapped_stripped = link
        .trim()
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
        .unwrap_or(link);
    let cleaned = normalize_wikilink_target(wrapped_stripped)?;
    if !vault_dir.is_dir() {
        return None;
    }

    let candidate_rel = if cleaned.to_ascii_lowercase().ends_with(".md") {
        cleaned.clone()
    } else {
        format!("{cleaned}.md")
    };

    // 1. Direct relative path inside MEMORY/ (e.g. "concepts/gateway-protocol.md" or "index.md")
    if let Some(direct) = sanitize_vault_relative_path(vault_dir, &candidate_rel) {
        if direct.is_file() {
            return Some(direct);
        }
    }

    // 2. If a short note name was given without subdirectory, search standard subdirectories in order.
    if !cleaned.contains('/') {
        for subdir in ["concepts", "decisions", "daily"] {
            let sub_candidate = vault_dir.join(subdir).join(&candidate_rel);
            if sub_candidate.is_file() {
                return Some(sub_candidate);
            }
        }
    }

    None
}

fn default_content(name: &str) -> &'static str {
    match name {
        "SOUL.md" => DEFAULT_SOUL,
        "AGENT.md" => DEFAULT_AGENT,
        "HEART.md" => DEFAULT_HEART,
        "DREAM.md" => DEFAULT_DREAM,
        "USER.md" => DEFAULT_USER,
        "MEMORY.md" => DEFAULT_MEMORY,
        _ => "",
    }
}

fn ensure_layer(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for name in AGENT_FILE_NAMES {
        let path = dir.join(name);
        if path.exists() || dir.join(name.to_ascii_lowercase()).exists() {
            continue;
        }
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", path.display()));
            }
        };
        file.write_all(default_content(name).as_bytes())?;
    }

    // Ensure Obsidian MEMORY/ vault structure exists with non-destructive defaults.
    let vault_dir = dir.join("MEMORY");
    for subdir in ["daily", "concepts", "decisions"] {
        fs::create_dir_all(vault_dir.join(subdir))
            .with_context(|| format!("create {}", vault_dir.join(subdir).display()))?;
    }
    let index_path = vault_dir.join("index.md");
    if !index_path.exists() {
        if let Ok(mut file) = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&index_path)
        {
            file.write_all(DEFAULT_VAULT_INDEX.as_bytes())?;
        }
    }
    Ok(())
}

fn existing_path(dir: &Path, name: &str) -> PathBuf {
    let canonical = dir.join(name);
    if canonical.exists() {
        return canonical;
    }
    let lowercase = dir.join(name.to_ascii_lowercase());
    if lowercase.exists() {
        return lowercase;
    }
    canonical
}

fn load_layer(files: &mut Vec<LoadedFile>, dir: &Path, scope: AgentFileScope) -> Result<()> {
    for name in AGENT_FILE_NAMES {
        let path = existing_path(dir, name);
        if !path.is_file() {
            continue;
        }
        let content = read_limited(&path)?;
        files.push(LoadedFile {
            name: name.to_string(),
            scope,
            content,
        });
    }
    load_vault_layer(files, dir, scope)?;
    Ok(())
}

fn load_vault_layer(files: &mut Vec<LoadedFile>, dir: &Path, scope: AgentFileScope) -> Result<()> {
    let vault_dir = existing_path(dir, "MEMORY");
    if !vault_dir.is_dir() {
        return Ok(());
    }

    let mut loaded_rel_paths = HashSet::new();
    let mut discovered_links = Vec::new();

    // 1. Load MEMORY/index.md first if present.
    let index_path = existing_path(&vault_dir, "index.md");
    if index_path.is_file() {
        let content = read_limited(&index_path)?;
        for link in extract_wikilinks(&content) {
            discovered_links.push(link);
        }
        loaded_rel_paths.insert("MEMORY/index.md".to_string());
        files.push(LoadedFile {
            name: "MEMORY/index.md".to_string(),
            scope,
            content,
        });
    }

    // 2. Scan recent MEMORY/daily/*.md notes (sorted descending by filename, up to MAX_RECENT_DAILY_NOTES).
    let daily_dir = existing_path(&vault_dir, "daily");
    if daily_dir.is_dir() {
        let mut daily_files: Vec<PathBuf> = fs::read_dir(&daily_dir)
            .with_context(|| format!("read_dir {}", daily_dir.display()))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("md"))
                        .unwrap_or(false)
            })
            .collect();
        daily_files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        for path in daily_files.into_iter().take(MAX_RECENT_DAILY_NOTES) {
            if let Some(rel_name) = vault_relative_label(&vault_dir, &path) {
                if loaded_rel_paths.insert(rel_name.clone()) {
                    let content = read_limited(&path)?;
                    for link in extract_wikilinks(&content) {
                        discovered_links.push(link);
                    }
                    files.push(LoadedFile {
                        name: rel_name,
                        scope,
                        content,
                    });
                }
            }
        }
    }

    // 3. Resolve [[wikilinks]] discovered in index.md and recent daily notes.
    for link in discovered_links {
        if let Some(resolved_path) = resolve_wikilink_in_vault(&vault_dir, &link) {
            if let Some(rel_name) = vault_relative_label(&vault_dir, &resolved_path) {
                if loaded_rel_paths.insert(rel_name.clone()) {
                    let content = read_limited(&resolved_path)?;
                    files.push(LoadedFile {
                        name: rel_name,
                        scope,
                        content,
                    });
                }
            }
        }
    }

    // 4. Load any remaining Markdown notes in concepts/ and decisions/ in deterministic order.
    for section in ["concepts", "decisions"] {
        let section_dir = existing_path(&vault_dir, section);
        if !section_dir.is_dir() {
            continue;
        }
        let mut section_files: Vec<PathBuf> = fs::read_dir(&section_dir)
            .with_context(|| format!("read_dir {}", section_dir.display()))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("md"))
                        .unwrap_or(false)
            })
            .collect();
        section_files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        for path in section_files {
            if let Some(rel_name) = vault_relative_label(&vault_dir, &path) {
                if loaded_rel_paths.insert(rel_name.clone()) {
                    let content = read_limited(&path)?;
                    files.push(LoadedFile {
                        name: rel_name,
                        scope,
                        content,
                    });
                }
            }
        }
    }

    Ok(())
}

fn vault_relative_label(vault_dir: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(vault_dir).ok()?;
    let normalized = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        None
    } else {
        Some(format!("MEMORY/{normalized}"))
    }
}

fn read_limited(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut bytes = Vec::with_capacity(MAX_AGENT_FILE_BYTES.min(8192));
    Read::by_ref(&mut file)
        .take((MAX_AGENT_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(truncate_with_marker(&text, MAX_AGENT_FILE_BYTES))
}

fn truncate_with_marker(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    const MARKER: &str = "\n[truncated by Varynth]\n";
    let target = max_bytes.saturating_sub(MARKER.len());
    let mut end = target.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = value[..end].to_string();
    if out.len() + MARKER.len() <= max_bytes {
        out.push_str(MARKER);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_all_five_core_persona_files_and_obsidian_vault() {
        let root = tempdir().unwrap();
        let varynth_dir = root.path().join(".varynth");
        let vault_dir = varynth_dir.join("MEMORY");
        fs::create_dir_all(vault_dir.join("daily")).unwrap();
        fs::create_dir_all(vault_dir.join("concepts")).unwrap();
        fs::create_dir_all(vault_dir.join("decisions")).unwrap();

        for name in CORE_PERSONA_FILE_NAMES {
            fs::write(varynth_dir.join(name), format!("content of {name}")).unwrap();
        }

        fs::write(
            vault_dir.join("index.md"),
            "# Vault Index\nLinks: [[concepts/gateway-protocol]] and [[decisions/001-obsidian-memory-vault|ADR 001]] and [[daily/2026-10-25]]",
        )
        .unwrap();
        fs::write(
            vault_dir.join("daily/2026-10-25.md"),
            "# Daily 2026-10-25\nSee [[concepts/flux-pipeline]]",
        )
        .unwrap();
        fs::write(
            vault_dir.join("concepts/gateway-protocol.md"),
            "# Gateway Protocol Spec",
        )
        .unwrap();
        fs::write(
            vault_dir.join("concepts/flux-pipeline.md"),
            "# Flux Pipeline Spec",
        )
        .unwrap();
        fs::write(
            vault_dir.join("decisions/001-obsidian-memory-vault.md"),
            "# ADR 001 Vault",
        )
        .unwrap();

        let files = AgentFiles::load(root.path()).unwrap();
        let ctx = files.system_context();

        for name in CORE_PERSONA_FILE_NAMES {
            assert!(
                ctx.contains(&format!("name=\"{name}\" scope=\"project\"")),
                "missing persona file {name} in system_context"
            );
        }
        assert!(ctx.contains("name=\"MEMORY/index.md\" scope=\"project\""));
        assert!(ctx.contains("name=\"MEMORY/daily/2026-10-25.md\" scope=\"project\""));
        assert!(ctx.contains("name=\"MEMORY/concepts/gateway-protocol.md\" scope=\"project\""));
        assert!(ctx.contains("name=\"MEMORY/concepts/flux-pipeline.md\" scope=\"project\""));
        assert!(
            ctx.contains("name=\"MEMORY/decisions/001-obsidian-memory-vault.md\" scope=\"project\"")
        );

        let memory_combined = files.memory();
        assert!(memory_combined.contains("# Vault Index"));
        assert!(memory_combined.contains("# Gateway Protocol Spec"));
        assert!(memory_combined.contains("# Flux Pipeline Spec"));
        assert!(memory_combined.contains("# ADR 001 Vault"));
    }

    #[test]
    fn extracts_and_resolves_wikilinks_correctly() {
        let root = tempdir().unwrap();
        let vault_dir = root.path().join(".varynth/MEMORY");
        fs::create_dir_all(vault_dir.join("concepts")).unwrap();
        fs::create_dir_all(vault_dir.join("decisions")).unwrap();
        fs::create_dir_all(vault_dir.join("daily")).unwrap();

        let gw_path = vault_dir.join("concepts/gateway-protocol.md");
        let adr_path = vault_dir.join("decisions/002-role-fallback-chaining.md");
        fs::write(&gw_path, "gateway details").unwrap();
        fs::write(&adr_path, "role fallback details").unwrap();

        let sample_md = "Check [[concepts/gateway-protocol]], [[002-role-fallback-chaining#Section|Role ADR]], and ignore ![[diagram.png]] or [[../secret]].";
        let links = extract_wikilinks(sample_md);
        assert_eq!(
            links,
            vec![
                "concepts/gateway-protocol".to_string(),
                "002-role-fallback-chaining".to_string()
            ]
        );

        let files = AgentFiles::load(root.path()).unwrap();
        assert_eq!(
            files.resolve_wikilink("[[concepts/gateway-protocol]]"),
            Some(gw_path)
        );
        assert_eq!(
            files.resolve_wikilink("002-role-fallback-chaining|Alias"),
            Some(adr_path)
        );
        assert_eq!(files.resolve_wikilink("../outside"), None);
    }

    #[test]
    fn vault_note_read_write_and_append_helpers_work() {
        let root = tempdir().unwrap();
        let mut files = AgentFiles::ensure(root.path()).unwrap();

        files
            .write_vault_note("concepts/test-note", "# Test Concept\nInitial body.")
            .unwrap();
        files
            .append_vault_note("concepts/test-note.md", "Appended paragraph.")
            .unwrap();

        let note = files.read_vault_note("[[concepts/test-note]]").unwrap();
        assert!(note.contains("# Test Concept"));
        assert!(note.contains("Initial body."));
        assert!(note.contains("Appended paragraph."));
        assert!(files.system_context().contains("Appended paragraph."));
    }

    #[test]
    fn workspace_varynth_vault_and_persona_files_load_cleanly() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let files = AgentFiles::load(workspace_root).unwrap();
        let ctx = files.system_context();

        for name in CORE_PERSONA_FILE_NAMES {
            assert!(
                ctx.contains(&format!("name=\"{name}\" scope=\"project\"")),
                "workspace missing {name}"
            );
        }
        assert!(ctx.contains("name=\"MEMORY/index.md\" scope=\"project\""));
        assert!(ctx.contains("name=\"MEMORY/daily/2026-10-25.md\" scope=\"project\""));
        assert!(ctx.contains("name=\"MEMORY/concepts/gateway-protocol.md\" scope=\"project\""));
        assert!(ctx.contains("name=\"MEMORY/concepts/flux-pipeline.md\" scope=\"project\""));
        assert!(
            ctx.contains("name=\"MEMORY/decisions/001-obsidian-memory-vault.md\" scope=\"project\"")
        );
        assert!(ctx.contains(
            "name=\"MEMORY/decisions/002-role-fallback-chaining.md\" scope=\"project\""
        ));
    }
}
