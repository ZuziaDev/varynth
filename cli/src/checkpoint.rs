use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// One snapshot header as listed by `/checkpoints`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub seq: u32,
    pub path: PathBuf,
    /// Bytes held by the snapshot (0 for skipped large/binary files).
    pub bytes: u64,
    /// The file was too large or not UTF-8; `undo` cannot restore it.
    pub skipped: bool,
    /// Snapshot write time, when the filesystem reports one.
    pub at: Option<DateTime<Utc>>,
}

/// What `redo` did with the most recently undone checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redo {
    /// The post-undo content (the write that `/undo` reverted) is back.
    Restored(PathBuf),
    /// The file did not exist again when it was undone; redo removes it.
    Deleted(PathBuf),
    /// Nothing has been undone in this session yet.
    Nothing,
}

/// The state an `/undo` replaced, kept so `/redo` can put it back.
#[derive(Debug, Clone)]
struct RedoEntry {
    path: PathBuf,
    /// `None` means the file did not exist before the undo.
    content: Option<String>,
}

/// One line per applied undo, for `/undo` output. An empty action list reads
/// as "nothing to undo".
pub fn summarize(outcomes: &[Undo]) -> String {
    let mut lines = Vec::new();
    for outcome in outcomes {
        match outcome {
            Undo::Restored(path) => lines.push(format!("restored {}", path.display())),
            Undo::Deleted(path) => {
                lines.push(format!("deleted {} (created this session)", path.display()))
            }
            Undo::TooLarge(path) => {
                lines.push(format!("{} was too large to checkpoint", path.display()))
            }
            Undo::Nothing => {}
        }
    }
    if lines.is_empty() {
        "(nothing to undo)".into()
    } else {
        lines.join("\n")
    }
}

/// Session-scoped store of pre-write file snapshots backing `/undo`.
///
/// Every `write_file`/`edit_file` records the file's content before it is
/// overwritten; `/undo` restores the most recent snapshot (LIFO), and the
/// state it replaced is remembered for `/redo`. Only files are restored:
/// no conversation snapshot exists, so there is nothing else to roll back.
pub struct CheckpointStore {
    dir: PathBuf,
    max_bytes: u64,
    /// Per-session redo stack of states replaced by an undo. In memory only;
    /// a new snapshot invalidates it.
    redo: Mutex<HashMap<String, Vec<RedoEntry>>>,
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
            redo: Mutex::new(HashMap::new()),
        }
    }

    /// Test-only constructor with a smaller size limit so the skipped-large
    /// behavior can be exercised without multi-megabyte fixtures.
    #[cfg(test)]
    fn with_limit(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            redo: Mutex::new(HashMap::new()),
        }
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

    fn record_path(&self, session_id: &str, seq: u32) -> PathBuf {
        self.session_dir(session_id).join(format!("{seq:04}.json"))
    }

    fn read_record(&self, session_id: &str, seq: u32) -> Result<Record> {
        let text = std::fs::read_to_string(self.record_path(session_id, seq))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Every snapshot kept for the session, oldest first. This is the data
    /// behind the `/checkpoints` picker; it never mutates the store.
    pub fn list(&self, session_id: &str) -> Result<Vec<Checkpoint>> {
        let mut out = Vec::new();
        for seq in self.seqs(session_id) {
            let record = self.read_record(session_id, seq)?;
            let at = std::fs::metadata(self.record_path(session_id, seq))
                .and_then(|m| m.modified())
                .ok()
                .map(|m| DateTime::<Utc>::from(m));
            out.push(Checkpoint {
                seq,
                path: PathBuf::from(record.path),
                bytes: record.content.as_ref().map(|c| c.len() as u64).unwrap_or(0),
                skipped: record.skipped,
                at,
            });
        }
        Ok(out)
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
        let record_path = session_dir.join(format!("{seq:04}.json"));
        std::fs::write(&record_path, serde_json::to_string(&record)?)?;
        // Snapshots can contain whatever a plan named, including `.env`.
        // Other local users must not read them on Unix hosts.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o600));
        }
        // A new write forks history forward again: anything undone before it
        // can no longer be redone without clobbering this snapshot.
        if let Ok(mut redo) = self.redo.lock() {
            redo.remove(session_id);
        }
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
    /// through the session's writes. The replaced state is pushed onto the
    /// session's redo stack.
    pub fn undo_last(&self, session_id: &str) -> Result<Undo> {
        let Some(&seq) = self.seqs(session_id).last() else {
            return Ok(Undo::Nothing);
        };
        let record_path = self.record_path(session_id, seq);
        let record: Record = serde_json::from_str(&std::fs::read_to_string(&record_path)?)?;
        std::fs::remove_file(&record_path)?;
        let path = PathBuf::from(&record.path);
        if record.skipped {
            return Ok(Undo::TooLarge(path));
        }
        // Remember what the undo is about to replace so `/redo` can put it
        // back. Uncapturable (huge/binary) current content disables redo for
        // this step instead of recording a wrong state.
        if let Some(content) = self.capture_current(&path) {
            if let Ok(mut redo) = self.redo.lock() {
                redo.entry(session_id.to_string())
                    .or_default()
                    .push(RedoEntry {
                        path: path.clone(),
                        content,
                    });
            }
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

    /// Undo up to `n` snapshots, newest first. Stops early when the session
    /// runs out of snapshots; every applied step is redoable.
    pub fn undo_n(&self, session_id: &str, n: usize) -> Result<Vec<Undo>> {
        let mut done = Vec::new();
        for _ in 0..n {
            let outcome = self.undo_last(session_id)?;
            let stop = outcome == Undo::Nothing;
            done.push(outcome);
            if stop {
                break;
            }
        }
        Ok(done)
    }

    /// Put back the state the most recent `/undo` replaced.
    pub fn redo(&self, session_id: &str) -> Result<Redo> {
        let entry = {
            let Ok(mut redo) = self.redo.lock() else {
                return Ok(Redo::Nothing);
            };
            let Some(stack) = redo.get_mut(session_id) else {
                return Ok(Redo::Nothing);
            };
            let entry = stack.pop();
            if stack.is_empty() {
                redo.remove(session_id);
            }
            entry
        };
        let Some(entry) = entry else {
            return Ok(Redo::Nothing);
        };
        match entry.content {
            Some(content) => {
                if let Some(parent) = entry.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&entry.path, content)?;
                Ok(Redo::Restored(entry.path))
            }
            None => {
                if entry.path.exists() {
                    std::fs::remove_file(&entry.path)?;
                }
                Ok(Redo::Deleted(entry.path))
            }
        }
    }

    /// The file's current text, `Some(None)` when it is absent, and `None`
    /// when it cannot be captured faithfully.
    fn capture_current(&self, path: &Path) -> Option<Option<String>> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() as u64 <= self.max_bytes => {
                String::from_utf8(bytes).ok().map(Some)
            }
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
        }
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

    /// Three writes of one file, like the acceptance script: v0 recorded,
    /// v1 recorded, v2 recorded, then v3 is the live state.
    fn three_writes(store: &CheckpointStore, dir: &tempfile::TempDir) -> PathBuf {
        let file = dir.path().join("code.txt");
        std::fs::write(&file, "v0").unwrap();
        store.snapshot("s1", &file).unwrap();
        std::fs::write(&file, "v1").unwrap();
        store.snapshot("s1", &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        store.snapshot("s1", &file).unwrap();
        std::fs::write(&file, "v3").unwrap();
        file
    }

    #[test]
    fn list_reports_every_snapshot_oldest_first() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = three_writes(&store, &dir);

        let listed = store.list("s1").unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed.iter().map(|c| c.seq).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(listed.iter().all(|c| c.path == file && !c.skipped));
        assert!(listed.iter().all(|c| c.bytes == 2));
        assert!(store.list("unknown").unwrap().is_empty());
    }

    #[test]
    fn undo_two_restores_the_state_before_the_second_write_then_redo() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = three_writes(&store, &dir);

        let outcomes = store.undo_n("s1", 2).unwrap();
        assert_eq!(
            outcomes,
            vec![Undo::Restored(file.clone()), Undo::Restored(file.clone())]
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        // One snapshot left: the state before the second write was undone.
        assert_eq!(store.list("s1").unwrap().len(), 1);

        assert_eq!(store.redo("s1").unwrap(), Redo::Restored(file.clone()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
        assert_eq!(store.redo("s1").unwrap(), Redo::Restored(file.clone()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v3");
        assert_eq!(store.redo("s1").unwrap(), Redo::Nothing);
    }

    #[test]
    fn undo_n_stops_at_nothing_and_new_snapshots_clear_redo() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = dir.path().join("code.txt");
        std::fs::write(&file, "v0").unwrap();
        store.snapshot("s1", &file).unwrap();
        std::fs::write(&file, "v1").unwrap();

        let outcomes = store.undo_n("s1", 5).unwrap();
        assert_eq!(outcomes, vec![Undo::Restored(file.clone()), Undo::Nothing]);

        // A fresh write invalidates the redo branch.
        store.snapshot("s1", &file).unwrap();
        assert_eq!(store.redo("s1").unwrap(), Redo::Nothing);
    }

    #[test]
    fn redo_of_a_deleted_created_file_restores_it() {
        let dir = tempdir().unwrap();
        let store = CheckpointStore::at(dir.path().join("cp"));
        let file = dir.path().join("fresh.txt");
        store.snapshot("s1", &file).unwrap();
        std::fs::write(&file, "created").unwrap();

        assert_eq!(store.undo_last("s1").unwrap(), Undo::Deleted(file.clone()));
        assert!(!file.exists());
        assert_eq!(store.redo("s1").unwrap(), Redo::Restored(file.clone()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "created");
    }
}
