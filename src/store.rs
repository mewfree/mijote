//! On-disk session records: one JSON file per session under the data dir, so
//! transcripts survive a restart. Never imports from `ui` or `app`.
//!
//! A record holds what the UI shows (transcript, agent, worktree), not the
//! agent's own context: ACP `session/load` is not wired up yet, so a restored
//! session starts a fresh agent behind the old transcript.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::session::Entry;
use crate::worktree::Worktree;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Agent id (see `agent::KNOWN`).
    pub agent: String,
    /// Directory mijote was started in; records are only restored there.
    pub cwd: PathBuf,
    pub worktree: Option<Worktree>,
    pub entries: Vec<Entry>,
    /// Model id the user last had selected, re-applied on restore.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// A fresh, sortable record key: `<unix millis>-<agent>`.
pub fn new_key(agent: &str) -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{ms}-{agent}")
}

/// Key ordering for `<unix millis>-<agent>` keys: compares the leading numeric
/// timestamp when present, falling back to lexicographic order.
fn key_order(key: &str) -> (Option<u128>, &str) {
    let ts = key
        .split_once('-')
        .and_then(|(n, _)| n.parse::<u128>().ok());
    (ts, key)
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn last_agent_path(&self) -> PathBuf {
        self.dir.join("last-agent")
    }

    /// Write atomically (temp file + rename) so a crash never leaves a
    /// half-written record.
    pub fn save(&self, key: &str, record: &Record) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(key);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn remove(&self, key: &str) {
        let _ = std::fs::remove_file(self.path(key));
    }

    /// Record the agent id used in the most recent conversation.
    pub fn save_last_agent(&self, agent: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.last_agent_path();
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, agent)?;
        std::fs::rename(tmp, path)
    }

    /// Agent id from the most recent conversation: the explicitly recorded
    /// last agent, or (if none is recorded yet) the newest saved session record
    /// across all directories.
    pub fn last_agent(&self) -> Option<String> {
        if let Ok(raw) = std::fs::read_to_string(self.last_agent_path()) {
            let id = raw.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        let dir = std::fs::read_dir(&self.dir).ok()?;
        dir.filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension()? != "json" {
                    return None;
                }
                let key = path.file_stem()?.to_string_lossy().into_owned();
                let record: Record = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
                Some((key, record.agent))
            })
            .max_by(|a, b| key_order(&a.0).cmp(&key_order(&b.0)))
            .map(|(_, agent)| agent)
    }

    /// Records saved for `cwd`, oldest first. Unreadable or corrupt files are
    /// skipped rather than blocking startup.
    pub fn load(&self, cwd: &Path) -> Vec<(String, Record)> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<(String, Record)> = dir
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension()? != "json" {
                    return None;
                }
                let key = path.file_stem()?.to_string_lossy().into_owned();
                let record: Record = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
                (record.cwd == cwd).then_some((key, record))
            })
            .collect();
        out.sort_by(|a, b| key_order(&a.0).cmp(&key_order(&b.0)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(cwd: &str) -> Record {
        Record {
            agent: "claude".into(),
            cwd: PathBuf::from(cwd),
            worktree: None,
            entries: vec![Entry::User("hi".into()), Entry::Agent("hello".into())],
            model: None,
        }
    }

    #[test]
    fn save_load_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        store.save("1-claude", &record("/p")).unwrap();
        store.save("2-claude", &record("/other")).unwrap();

        let loaded = store.load(Path::new("/p"));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, "1-claude");
        assert_eq!(loaded[0].1.entries, record("/p").entries);

        store.remove("1-claude");
        assert!(store.load(Path::new("/p")).is_empty());
    }

    #[test]
    fn corrupt_and_missing_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        assert!(store.load(Path::new("/p")).is_empty());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(dir.path().join("sessions/bad.json"), "{nope").unwrap();
        store.save("1-claude", &record("/p")).unwrap();
        assert_eq!(store.load(Path::new("/p")).len(), 1);
    }

    #[test]
    fn last_agent_prefers_explicit_and_falls_back_to_newest_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        assert_eq!(store.last_agent(), None);

        let mut antigravity = record("/other");
        antigravity.agent = "antigravity".into();
        store.save("2-claude", &record("/p")).unwrap();
        store.save("10-antigravity", &antigravity).unwrap();
        assert_eq!(store.last_agent().as_deref(), Some("antigravity"));

        // Explicit last-agent takes precedence and survives removing records.
        store.save_last_agent("codex").unwrap();
        assert_eq!(store.last_agent().as_deref(), Some("codex"));
        store.remove("10-antigravity");
        assert_eq!(store.last_agent().as_deref(), Some("codex"));
        // The last-agent file must not interfere with loading session records.
        assert_eq!(store.load(Path::new("/p")).len(), 1);
    }
}
