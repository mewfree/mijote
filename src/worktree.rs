//! One git worktree (and branch) per session, so agents never share a
//! working directory. Shells out to `git`; never imports from `ui` or `app`.
//!
//! Worktrees live under a caller-supplied root (mijote's data dir), not in
//! the user's checkout. They branch from the repo's current `HEAD`, so
//! uncommitted changes in the main checkout are not visible to the agent.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("{0} is not inside a git repository")]
    NotARepo(PathBuf),
    #[error("repository has no commits yet")]
    NoCommits,
    #[error("git {0} failed: {1}")]
    Git(String, String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    repo: PathBuf,
    base: String,
}

/// What a session did inside its worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub dirty: bool,
    pub commits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Released {
    /// Untouched, so worktree and branch were deleted.
    Removed,
    /// Had changes or commits; left on disk for the user.
    Kept { path: PathBuf, branch: String },
}

fn git(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(WorktreeError::Git(
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

impl Worktree {
    /// Create `mijote/<label>-<n>` in a new worktree under `root`, based on
    /// the current `HEAD` of the repository containing `cwd`.
    pub fn create(cwd: &Path, root: &Path, label: &str) -> Result<Self, WorktreeError> {
        let repo = git(cwd, &["rev-parse", "--show-toplevel"])
            .map(PathBuf::from)
            .map_err(|_| WorktreeError::NotARepo(cwd.to_path_buf()))?;
        let base =
            git(&repo, &["rev-parse", "--verify", "HEAD"]).map_err(|_| WorktreeError::NoCommits)?;
        let repo_name = repo
            .file_name()
            .map(|n| slug(&n.to_string_lossy()))
            .unwrap_or_else(|| "repo".into());
        let dir = root.join(repo_name);
        std::fs::create_dir_all(&dir)?;

        let label = slug(label);
        for n in 1.. {
            let branch = format!("mijote/{label}-{n}");
            let path = dir.join(format!("{label}-{n}"));
            let branch_exists = git(
                &repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .is_ok();
            if branch_exists || path.exists() {
                continue;
            }
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    &path.to_string_lossy(),
                    &base,
                ],
            )?;
            return Ok(Self {
                path,
                branch,
                repo,
                base,
            });
        }
        unreachable!("unbounded loop returns or continues")
    }

    pub fn state(&self) -> Result<State, WorktreeError> {
        let dirty = !git(&self.path, &["status", "--porcelain"])?.is_empty();
        let range = format!("{}..{}", self.base, self.branch);
        let commits = git(&self.path, &["rev-list", "--count", &range])?
            .parse()
            .unwrap_or(0);
        Ok(State { dirty, commits })
    }

    /// Delete the worktree and branch if the session changed nothing;
    /// otherwise leave both in place and say where they are.
    pub fn release(&self) -> Result<Released, WorktreeError> {
        let st = self.state()?;
        if st.dirty || st.commits > 0 {
            return Ok(Released::Kept {
                path: self.path.clone(),
                branch: self.branch.clone(),
            });
        }
        git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )?;
        git(&self.repo, &["branch", "-D", &self.branch])?;
        Ok(Released::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        run(d.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(d.path().join("a.txt"), "a").unwrap();
        run(d.path(), &["add", "."]);
        run(d.path(), &["commit", "-q", "-m", "init"]);
        d
    }

    #[test]
    fn creates_isolated_worktrees_with_unique_branches() {
        let r = repo();
        let root = tempfile::tempdir().unwrap();
        let a = Worktree::create(r.path(), root.path(), "claude").unwrap();
        let b = Worktree::create(r.path(), root.path(), "claude").unwrap();
        assert_ne!(a.path, b.path);
        assert_ne!(a.branch, b.branch);
        assert!(a.path.join("a.txt").is_file());
        assert_eq!(
            a.state().unwrap(),
            State {
                dirty: false,
                commits: 0
            }
        );
        // Main checkout is untouched.
        assert!(git(r.path(), &["status", "--porcelain"])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn untouched_worktree_is_removed_on_release() {
        let r = repo();
        let root = tempfile::tempdir().unwrap();
        let w = Worktree::create(r.path(), root.path(), "codex").unwrap();
        assert_eq!(w.release().unwrap(), Released::Removed);
        assert!(!w.path.exists());
        assert!(git(r.path(), &["branch", "--list", &w.branch])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn changed_worktree_is_kept() {
        let r = repo();
        let root = tempfile::tempdir().unwrap();
        let dirty = Worktree::create(r.path(), root.path(), "x").unwrap();
        std::fs::write(dirty.path.join("new.txt"), "n").unwrap();
        assert!(matches!(dirty.release().unwrap(), Released::Kept { .. }));
        assert!(dirty.path.exists());

        let committed = Worktree::create(r.path(), root.path(), "y").unwrap();
        std::fs::write(committed.path.join("c.txt"), "c").unwrap();
        run(&committed.path, &["add", "."]);
        run(&committed.path, &["commit", "-q", "-m", "work"]);
        assert_eq!(
            committed.state().unwrap(),
            State {
                dirty: false,
                commits: 1
            }
        );
        assert!(matches!(
            committed.release().unwrap(),
            Released::Kept { .. }
        ));
    }

    #[test]
    fn non_repo_and_empty_repo_are_errors() {
        let root = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap();
        assert!(matches!(
            Worktree::create(plain.path(), root.path(), "a"),
            Err(WorktreeError::NotARepo(_))
        ));
        let empty = tempfile::tempdir().unwrap();
        run(empty.path(), &["init", "-q"]);
        assert!(matches!(
            Worktree::create(empty.path(), root.path(), "a"),
            Err(WorktreeError::NoCommits)
        ));
    }
}
