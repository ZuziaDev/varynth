use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::Config;

/// How many snapshot records are kept per session; older ones are pruned
/// on every snapshot.
const MAX_RECORDS_PER_SESSION: usize = 100;

/// Files larger than this are recorded as skipped instead of snapshotted.
const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// One recorded pre-write state, stored as `<seq:04>.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    path: String,
    existed_before: bool,
    content: Option<String>,
    skipped: bool,
}

/// What `undo_last` did with the most recent snapshot of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undo {
    /// The pre-write content was written back.
    Restored(PathBuf),
    /// The file was created this session; undo removed it again.
    Deleted(PathBuf),
    /// The last write was too large to checkpoint; nothing was restored.
    TooLarge(PathBuf),
    /// No snapshot is left for this session.
    Nothing,
}

/// Session-scoped store of pre-write file snapshots backing `/undo`.
///
/// Every `write_file`/`edit_file` records the file's content before it is
/// overwritten; `/undo` restores the most recent snapshot (LIFO).
pub struct CheckpointStore {
    dir: PathBuf,
    max_bytes: u64,
}

impl Default for CheckpointStore {
    fn default() -> Self {
        Self::at(Config::home_dir().join("checkpoints"))
    }
}

impl CheckpointStore {
    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir,
            max_bytes: MAX_SNAPSHOT_BYTES,
        }
    }

    /// Test-only constructor with a smaller size limit so the skipped-large
    /// behavior can be exercised without multi-megabyte fixtures.
    #[cfg(test)]
    fn with_limit(dir: PathBuf, max_bytes: u64) -> Self {
        Self { dir, max_bytes }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.dir.join(session_id)
    }

    /// Sorted sequence numbers of the records kept for the session.
    fn seqs(&self, session_id: &str) -> Vec<u32> {
        let mut seqs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.session_dir(session_id)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(seq) = stem.parse::<u32>() {
                        seqs.push(seq);
                    }
                }
            }
        }
        seqs.sort_unstable();
        seqs
    }

    /// Record the file's current content before an overwrite. Files over the
    /// size limit (or non-UTF-8 files) are recorded as skipped so `/undo`
    /// can say so instead of silently restoring nothing.
    pub fn snapshot(&self, session_id: &str, path: &Path) -> Result<()> {
        let (existed_before, content, skipped) = match std::fs::read(path) {
            Ok(bytes) => {
                if bytes.len() as u64 > self.max_bytes {
                    (true, None, true)
                } else {
                    match String::from_utf8(bytes) {
                        Ok(text) => (true, Some(text), false),
                        // Binary content cannot be restored faithfully.
                        Err(_) => (true, None, true),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, None, false),
            Err(e) => return Err(e.into()),
        };
        let session_dir = self.session_dir(session_id);
        std::fs::create_dir_all(&session_dir)?;
        let seq = self.seqs(session_id).last().copied().unwrap_or(0) + 1;
        let record = Record {
            path: path.display().to_string(),
            existed_before,
            content,
            skipped,
        };
        std::fs::write(
            session_dir.join(format!("{seq:04}.json")),
            serde_json::to_string(&record)?,
        )?;
        self.prune(session_id);
        Ok(())
    }

    /// Keep at most [`MAX_RECORDS_PER_SESSION`] records; drop the oldest.
    fn prune(&self, session_id: &str) {
        let seqs = self.seqs(session_id);
        let excess = seqs.len().saturating_sub(MAX_RECORDS_PER_SESSION);
        for seq in &seqs[..excess] {
            let _ =
                std::fs::remove_file(self.session_dir(session_id).join(format!("{seq:04}.json")));
        }
    }

    /// Restore the most recent snapshot for the session (LIFO). The consumed
    /// record is deleted either way, so repeated `/undo` walks backwards
    /// through the session's writes.
    pub fn undo_last(&self, session_id: &str) -> Result<Undo> {
        let Some(&seq) = self.seqs(session_id).last() else {
            return Ok(Undo::Nothing);
        };
        let record_path = self.session_dir(session_id).join(format!("{seq:04}.json"));
        let record: Record = serde_json::from_str(&std::fs::read_to_string(&record_path)?)?;
        std::fs::remove_file(&record_path)?;
        let path = PathBuf::from(&record.path);
        if record.skipped {
            return Ok(Undo::TooLarge(path));
        }
        if !record.existed_before {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            return Ok(Undo::Deleted(path));
        }
        let Some(content) = record.content else {
            anyhow::bail!("snapshot {seq:04} holds no content to restore");
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        Ok(Undo::Restored(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn undo_restores_the_pre_write_content() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "before").unwrap();

        store.snapshot("s1", &file).unwrap();
        std::fs::write(&file, "after").unwrap();

        assert_eq!(store.undo_last("s1").unwrap(), Undo::Restored(file.clone()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        // Consumed: nothing left to undo.
        assert_eq!(store.undo_last("s1").unwrap(), Undo::Nothing);
    }

    #[test]
    fn undo_of_a_created_file_removes_it() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = dir.path().join("fresh.txt");

        store.snapshot("s1", &file).unwrap();
        assert!(!file.exists());
        std::fs::write(&file, "created by the agent").unwrap();

        assert_eq!(store.undo_last("s1").unwrap(), Undo::Deleted(file.clone()));
        assert!(!file.exists());
    }

    #[test]
    fn undo_walks_backwards_through_multiple_files() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        // Each snapshot records the content just before an overwrite, like
        // write_file does.
        std::fs::write(&a, "a1").unwrap();
        store.snapshot("s1", &a).unwrap();
        std::fs::write(&b, "b1").unwrap();
        store.snapshot("s1", &b).unwrap();
        std::fs::write(&a, "a2").unwrap();
        store.snapshot("s1", &a).unwrap();
        std::fs::write(&a, "a3").unwrap();

        // LIFO: the last snapshot first, then the earlier ones.
        assert_eq!(store.undo_last("s1").unwrap(), Undo::Restored(a.clone()));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a2");
        assert_eq!(store.undo_last("s1").unwrap(), Undo::Restored(b.clone()));
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b1");
        assert_eq!(store.undo_last("s1").unwrap(), Undo::Restored(a.clone()));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1");
        assert_eq!(store.undo_last("s1").unwrap(), Undo::Nothing);
    }

    #[test]
    fn snapshots_are_pruned_to_one_hundred_per_session() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = dir.path().join("churn.txt");
        std::fs::write(&file, "v0").unwrap();
        for i in 1..=105_u32 {
            store.snapshot("s1", &file).unwrap();
            std::fs::write(&file, format!("v{i}")).unwrap();
        }

        let seqs = store.seqs("s1");
        assert_eq!(seqs.len(), 100);
        assert_eq!(seqs[0], 6); // the 5 oldest were pruned
        assert_eq!(seqs[99], 105);

        // Undo still works on the newest surviving record: the content the
        // file had before the 105th overwrite.
        assert_eq!(store.undo_last("s1").unwrap(), Undo::Restored(file.clone()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v104");
    }

    #[test]
    fn oversized_files_are_recorded_as_skipped() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![b'x'; MAX_SNAPSHOT_BYTES as usize + 1]).unwrap();

        store.snapshot("s1", &big).unwrap();
        // Consumed and reported, not restored.
        assert_eq!(store.undo_last("s1").unwrap(), Undo::TooLarge(big.clone()));
        assert_eq!(store.undo_last("s1").unwrap(), Undo::Nothing);

        // The same behavior through a store with a small limit.
        let small = CheckpointStore::with_limit(dir.path().join("cp2"), 64);
        let tiny = dir.path().join("tiny.txt");
        std::fs::write(&tiny, vec![b'y'; 65]).unwrap();
        small.snapshot("s2", &tiny).unwrap();
        assert_eq!(small.undo_last("s2").unwrap(), Undo::TooLarge(tiny.clone()));
    }

    #[test]
    fn sessions_are_isolated_by_id() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = dir.path().join("shared.txt");
        std::fs::write(&file, "one").unwrap();
        store.snapshot("s-a", &file).unwrap();
        std::fs::write(&file, "two").unwrap();
        store.snapshot("s-b", &file).unwrap();

        assert_eq!(
            store.undo_last("s-a").unwrap(),
            Undo::Restored(file.clone())
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");
        assert_eq!(
            store.undo_last("s-b").unwrap(),
            Undo::Restored(file.clone())
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
    }
}
