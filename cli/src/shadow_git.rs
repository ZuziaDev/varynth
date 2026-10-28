//! Shadow Git & Granular Checkpoint Engine for Varynth.
//! Enables micro-second workspace snapshots and granular undo/redo:
//! - `/undo files` (reverts files only, preserves chat context)
//! - `/undo chat` (pops the last conversation turn, preserves file writes)
//! - `/undo all` (reverts both files and conversation turn)
//! - `/redo` (re-applies the reverted file writes)

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowSnapshot {
    pub id: u32,
    pub turn: u64,
    pub created_at: DateTime<Utc>,
    pub changed_files: Vec<String>,
    pub description: String,
}

pub struct ShadowGit {
    pub root: PathBuf,
    pub shadow_dir: PathBuf,
}

impl ShadowGit {
    pub fn new(workspace_root: &Path) -> Self {
        let shadow_dir = workspace_root.join(".varynth").join("shadow_git");
        Self {
            root: workspace_root.to_path_buf(),
            shadow_dir,
        }
    }

    pub fn init_if_needed(&self) -> Result<()> {
        if !self.shadow_dir.exists() {
            fs::create_dir_all(&self.shadow_dir)?;
        }
        let snaps_dir = self.shadow_dir.join("snapshots");
        if !snaps_dir.exists() {
            fs::create_dir_all(&snaps_dir)?;
        }
        Ok(())
    }

    /// Creates a lightweight snapshot of target files before a tool or turn writes to them.
    pub fn snapshot_files(&self, turn: u64, files: &[PathBuf], desc: &str) -> Result<u32> {
        self.init_if_needed()?;
        let meta_file = self.shadow_dir.join("snapshots.json");
        let mut list: Vec<ShadowSnapshot> = if meta_file.exists() {
            let data = fs::read(&meta_file)?;
            serde_json::from_slice(&data).unwrap_or_default()
        } else {
            Vec::new()
        };

        let next_id = list.last().map(|s| s.id + 1).unwrap_or(1);
        let turn_dir = self
            .shadow_dir
            .join("snapshots")
            .join(format!("{:04}", next_id));
        fs::create_dir_all(&turn_dir)?;

        let mut changed_str = Vec::new();
        for file in files {
            if let Ok(rel) = file.strip_prefix(&self.root) {
                let rel_str = rel.to_string_lossy().to_string();
                if file.is_file() {
                    let dest = turn_dir.join(&rel_str);
                    if let Some(parent) = dest.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::copy(file, &dest);
                }
                changed_str.push(rel_str);
            }
        }

        list.push(ShadowSnapshot {
            id: next_id,
            turn,
            created_at: Utc::now(),
            changed_files: changed_str,
            description: desc.to_string(),
        });

        // Cap at 50 snapshots
        if list.len() > 50 {
            let to_remove = list.drain(0..list.len() - 50);
            for item in to_remove {
                let old_dir = self
                    .shadow_dir
                    .join("snapshots")
                    .join(format!("{:04}", item.id));
                let _ = fs::remove_dir_all(old_dir);
            }
        }

        let bytes = serde_json::to_vec_pretty(&list)?;
        fs::write(meta_file, bytes)?;
        Ok(next_id)
    }

    /// Restores the most recent snapshot files back into the workspace.
    pub fn revert_last_files(&self) -> Result<Option<ShadowSnapshot>> {
        let meta_file = self.shadow_dir.join("snapshots.json");
        if !meta_file.exists() {
            return Ok(None);
        }
        let data = fs::read(&meta_file)?;
        let mut list: Vec<ShadowSnapshot> = serde_json::from_slice(&data)?;
        let Some(last) = list.pop() else {
            return Ok(None);
        };

        let turn_dir = self
            .shadow_dir
            .join("snapshots")
            .join(format!("{:04}", last.id));
        for rel in &last.changed_files {
            let snap_file = turn_dir.join(rel);
            let target_file = self.root.join(rel);
            if snap_file.is_file() {
                if let Some(parent) = target_file.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::copy(&snap_file, &target_file);
            } else if target_file.is_file() {
                // File was created in this snapshot, revert deletes it
                let _ = fs::remove_file(&target_file);
            }
        }

        let bytes = serde_json::to_vec_pretty(&list)?;
        fs::write(meta_file, bytes)?;
        Ok(Some(last))
    }
}
