//! On-disk session records: one JSON file per session under the data dir, so
//! transcripts survive a restart. Never imports from `ui` or `app`.
//!
//! A record holds what the UI shows (transcript, agent, worktree), not the
//! agent's own context: ACP `session/load` is not wired up yet, so a restored
//! session starts a fresh agent behind the old transcript.

use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// Effort level id the user last had selected, re-applied on restore.
    #[serde(default)]
    pub effort: Option<String>,
    /// Session mode id the user last had selected, re-applied on restore.
    #[serde(default)]
    pub mode: Option<String>,
    /// Optional user-assigned session label, restored across `--resume`.
    #[serde(default)]
    pub label: Option<String>,
}

/// Maximum number of submitted prompts kept in persistent history.
pub const HISTORY_LIMIT: usize = 500;

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

/// Canonical identifier for the project containing `cwd`: the main git
/// repository root (shared across subdirectories and linked worktrees), or
/// the canonicalized `cwd` when outside a git repository.
pub fn project_root(cwd: &Path) -> PathBuf {
    if let Some(root) = git_repo_root(cwd) {
        return root;
    }
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.components().collect())
}

fn git_repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let common = cwd.join(raw);
    let canonical = std::fs::canonicalize(&common).unwrap_or(common);
    if canonical.file_name().is_some_and(|n| n == ".git") {
        if let Some(parent) = canonical.parent() {
            return Some(parent.to_path_buf());
        }
    }
    Some(canonical)
}

fn project_slug(project: &Path) -> String {
    let raw = project
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let slug: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

/// Deterministic 64-bit FNV-1a hash (stable across Rust versions and runs).
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
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

    fn history_path(&self, project: &Path) -> PathBuf {
        let slug = project_slug(project);
        let hash = fnv1a_64(project.as_os_str().as_encoded_bytes());
        self.dir
            .join("history")
            .join(format!("{slug}-{hash:016x}.json"))
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

    /// Persist submitted prompt history for `project` atomically, keeping at
    /// most `HISTORY_LIMIT` most recent entries. Stored under `history/` so
    /// concurrent instances in different projects never overwrite each other
    /// and top-level session loading ignores them.
    pub fn save_history(&self, project: &Path, entries: &[String]) -> std::io::Result<()> {
        let path = self.history_path(project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start = entries.len().saturating_sub(HISTORY_LIMIT);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&entries[start..])?)?;
        std::fs::rename(tmp, path)
    }

    /// Load persisted prompt history for `project`, oldest first. Missing or
    /// corrupt files return an empty list.
    pub fn load_history(&self, project: &Path) -> Vec<String> {
        let Ok(bytes) = std::fs::read(self.history_path(project)) else {
            return Vec::new();
        };
        let Ok(mut entries) = serde_json::from_slice::<Vec<String>>(&bytes) else {
            return Vec::new();
        };
        entries.retain(|s| !s.trim().is_empty());
        if entries.len() > HISTORY_LIMIT {
            let excess = entries.len() - HISTORY_LIMIT;
            entries.drain(..excess);
        }
        entries
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
            effort: None,
            mode: None,
            label: None,
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

    #[test]
    fn label_persists_and_old_records_without_label_deserialize() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));

        let mut labeled = record("/p");
        labeled.label = Some("fix-retry".into());
        labeled.effort = Some("high".into());
        labeled.mode = Some("acceptEdits".into());
        store.save("1-claude", &labeled).unwrap();

        // Simulate an older on-disk JSON record that lacks `label`, `effort`, and `mode`.
        let legacy_json = r#"{
            "agent": "codex",
            "cwd": "/p",
            "worktree": null,
            "entries": [{ "User": "old" }],
            "model": null
        }"#;
        std::fs::write(dir.path().join("sessions/2-codex.json"), legacy_json).unwrap();

        let loaded = store.load(Path::new("/p"));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].0, "1-claude");
        assert_eq!(loaded[0].1.label.as_deref(), Some("fix-retry"));
        assert_eq!(loaded[0].1.effort.as_deref(), Some("high"));
        assert_eq!(loaded[0].1.mode.as_deref(), Some("acceptEdits"));
        assert_eq!(loaded[1].0, "2-codex");
        assert_eq!(loaded[1].1.label, None);
        assert_eq!(loaded[1].1.effort, None);
        assert_eq!(loaded[1].1.mode, None);
    }

    #[test]
    fn prompt_history_is_scoped_by_project_caps_and_ignores_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        let proj_a = Path::new("/work/a/app");
        let proj_b = Path::new("/work/b/app");
        assert!(store.load_history(proj_a).is_empty());

        // An old global prompt-history file is ignored so prompts never leak across projects.
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions/prompt-history"),
            r#"["legacy global"]"#,
        )
        .unwrap();
        assert!(store.load_history(proj_a).is_empty());

        // Two projects with the same directory name stay isolated.
        let prompts_a = vec!["first".to_string(), "multi\nline".to_string()];
        let prompts_b = vec!["other repo".to_string()];
        store.save_history(proj_a, &prompts_a).unwrap();
        store.save_history(proj_b, &prompts_b).unwrap();
        assert_eq!(store.load_history(proj_a), prompts_a);
        assert_eq!(store.load_history(proj_b), prompts_b);

        // Must not interfere with session records or last_agent.
        assert!(store.load(proj_a).is_empty());
        assert_eq!(store.last_agent(), None);

        // Caps at HISTORY_LIMIT, keeping the newest entries.
        let many: Vec<String> = (0..(HISTORY_LIMIT + 10)).map(|i| format!("p{i}")).collect();
        store.save_history(proj_a, &many).unwrap();
        let loaded = store.load_history(proj_a);
        assert_eq!(loaded.len(), HISTORY_LIMIT);
        assert_eq!(loaded.first().map(String::as_str), Some("p10"));
        assert_eq!(
            loaded.last().map(String::as_str),
            Some(format!("p{}", HISTORY_LIMIT + 9).as_str())
        );

        // Corrupt history file returns empty instead of failing.
        std::fs::write(store.history_path(proj_a), "{bad").unwrap();
        assert!(store.load_history(proj_a).is_empty());
        assert_eq!(store.load_history(proj_b), prompts_b);
    }

    #[test]
    fn project_root_groups_git_subdirs_and_worktrees_and_separates_other_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let sub = repo.join("src/nested");
        let other = dir.path().join("other");
        let wt = dir.path().join("wt-feature");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let run_git = |cwd: &Path, args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("README.md"), "hi\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "-q", "-m", "init"]);
        run_git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );

        let root_from_repo = project_root(&repo);
        let root_from_sub = project_root(&sub);
        let root_from_wt = project_root(&wt);
        let root_from_other = project_root(&other);

        assert_eq!(root_from_repo, std::fs::canonicalize(&repo).unwrap());
        assert_eq!(root_from_sub, root_from_repo);
        assert_eq!(root_from_wt, root_from_repo);
        assert_ne!(root_from_other, root_from_repo);
        assert_eq!(root_from_other, std::fs::canonicalize(&other).unwrap());
    }
}
