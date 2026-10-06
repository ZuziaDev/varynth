use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::Config;

pub const AGENT_FILE_NAMES: [&str; 5] = ["SOUL.md", "USER.md", "MEMORY.md", "DREAM.md", "HEART.md"];
pub const MAX_AGENT_FILE_BYTES: usize = 64 * 1024;
pub const MAX_SYSTEM_CONTEXT_BYTES: usize = 128 * 1024;

const DEFAULT_SOUL: &str = "# Soul\n\nDefine the agent's identity, values, and boundaries here.\n";
const DEFAULT_USER: &str = "# User\n\nRecord stable preferences and context about the user here.\n";
const DEFAULT_MEMORY: &str = "# Memory\n\nDurable facts and decisions are appended here.\n";
const DEFAULT_DREAM: &str = "# Dreams\n\nIdeas and longer-term goals can be kept here.\n";
const DEFAULT_HEART: &str =
    "# Heart\n\nCurrent priorities and emotional context can be kept here.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFileScope {
    Global,
    Project,
}

impl AgentFileScope {
    fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
struct LoadedFile {
    name: &'static str,
    scope: AgentFileScope,
    content: String,
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
            .filter(|file| file.name == "MEMORY.md")
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
        self.reload()?;
        Ok(())
    }

    fn reload(&mut self) -> Result<()> {
        let fresh = Self::load(&self.cwd)?;
        self.files = fresh.files;
        Ok(())
    }
}

fn default_content(name: &str) -> &'static str {
    match name {
        "SOUL.md" => DEFAULT_SOUL,
        "USER.md" => DEFAULT_USER,
        "MEMORY.md" => DEFAULT_MEMORY,
        "DREAM.md" => DEFAULT_DREAM,
        "HEART.md" => DEFAULT_HEART,
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
    Ok(())
}

fn existing_path(dir: &Path, name: &'static str) -> PathBuf {
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
            name,
            scope,
            content,
        });
    }
    Ok(())
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
