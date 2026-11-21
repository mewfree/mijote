//! Resolve an agent's ACP binary, installing the adapter into mijote's own
//! data dir when needed. Nothing is installed globally.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{which, AgentSpec, Install};

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("`{0}` not found on PATH and no managed install is available: {1}")]
    Manual(String, &'static str),
    #[error("`npm` is required to install {0} but was not found on PATH (install Node.js)")]
    NoNpm(&'static str),
    #[error("npm install of {0} failed: {1}")]
    NpmFailed(String, String),
    #[error("installed {0} but could not find `{1}` in {2}")]
    MissingBin(String, &'static str, PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// mijote's data dir: `$MIJOTE_HOME`, else `$XDG_DATA_HOME/mijote`,
/// else `~/.local/share/mijote`.
pub fn data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("MIJOTE_HOME") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(p).join("mijote");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/mijote")
}

fn managed_dir(spec: &AgentSpec) -> PathBuf {
    data_dir().join("agents").join(spec.id)
}

fn managed_bin(spec: &AgentSpec) -> PathBuf {
    managed_dir(spec).join("node_modules/.bin").join(spec.bin)
}

/// Local install and login readiness for an agent backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Binary is installed and local auth/login state looks present (or cannot be ruled out).
    Ready,
    /// Binary is installed, but known local credential markers and API key env vars are absent.
    NeedsLogin,
    /// Binary is not found, and its npm adapter will be installed on first use.
    NeedsInstall,
    /// Binary is not found, and it must be installed manually.
    Missing,
}

/// Fast local check (no network, no subprocesses) for whether `spec` is
/// installed and appears logged in. Never reads secret file contents.
pub fn status(spec: &AgentSpec) -> AgentStatus {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    status_with(locate(spec).is_some(), spec, home.as_deref(), |name| {
        std::env::var_os(name).is_some_and(|v| !v.is_empty())
    })
}

fn status_with(
    installed: bool,
    spec: &AgentSpec,
    home: Option<&Path>,
    has_env: impl Fn(&str) -> bool,
) -> AgentStatus {
    if !installed {
        return match spec.install {
            Install::Npm { .. } => AgentStatus::NeedsInstall,
            Install::None | Install::Manual(_) => AgentStatus::Missing,
        };
    }
    let exists = |rel: &str| home.is_some_and(|h| h.join(rel).exists());
    let logged_in = match spec.id {
        "claude" => {
            has_env("ANTHROPIC_API_KEY")
                || has_env("CLAUDE_CODE_OAUTH_TOKEN")
                || exists(".claude.json")
                || exists(".claude")
        }
        "codex" => {
            has_env("OPENAI_API_KEY") || has_env("CODEX_API_KEY") || exists(".codex/auth.json")
        }
        "antigravity" => {
            has_env("GEMINI_API_KEY")
                || has_env("GOOGLE_API_KEY")
                || exists(".gemini/antigravity-cli")
                || exists(".gemini")
        }
        _ => true,
    };
    if logged_in {
        AgentStatus::Ready
    } else {
        AgentStatus::NeedsLogin
    }
}

/// Where the binary currently is, without installing anything.
pub fn locate(spec: &AgentSpec) -> Option<PathBuf> {
    let managed = managed_bin(spec);
    if managed.is_file() {
        return Some(managed);
    }
    which(spec.bin)
}

/// Find the agent's binary, installing the adapter first if it is missing
/// and mijote knows how.
pub fn ensure(spec: &AgentSpec) -> Result<PathBuf, InstallError> {
    if let Some(program) = locate(spec) {
        return Ok(program);
    }
    match spec.install {
        Install::None => Err(InstallError::Manual(
            spec.bin.to_string(),
            "install the agent CLI itself",
        )),
        Install::Manual(hint) => Err(InstallError::Manual(spec.bin.to_string(), hint)),
        Install::Npm { package, version } => {
            npm_install(&managed_dir(spec), package, version)?;
            let program = managed_bin(spec);
            if !program.is_file() {
                return Err(InstallError::MissingBin(
                    format!("{package}@{version}"),
                    spec.bin,
                    managed_dir(spec),
                ));
            }
            Ok(program)
        }
    }
}

fn npm_install(dir: &Path, package: &'static str, version: &str) -> Result<(), InstallError> {
    let npm = which("npm").ok_or(InstallError::NoNpm(package))?;
    std::fs::create_dir_all(dir)?;
    let spec = format!("{package}@{version}");
    let out = Command::new(npm)
        .args(["install", "--prefix"])
        .arg(dir)
        .args(["--no-audit", "--no-fund", "--loglevel=error", &spec])
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(InstallError::NpmFailed(spec, stderr.trim().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::find;

    #[test]
    fn manual_agent_reports_hint_instead_of_installing() {
        let spec = AgentSpec {
            id: "manual-test",
            name: "Manual Test",
            bin: "mijote-nonexistent-manual-bin",
            args: &[],
            install: Install::Manual("install manually"),
            note: "",
        };
        let err = ensure(&spec).unwrap_err();
        assert!(matches!(err, InstallError::Manual(_, "install manually")));
    }

    #[test]
    fn status_reports_needs_install_or_missing_when_binary_absent() {
        let claude = find("claude").unwrap();
        let codex = find("codex").unwrap();
        let agy = find("antigravity").unwrap();
        let manual = AgentSpec {
            id: "manual-test",
            name: "Manual",
            bin: "manual",
            args: &[],
            install: Install::Manual("hint"),
            note: "",
        };

        assert_eq!(
            status_with(false, claude, None, |_| true),
            AgentStatus::NeedsInstall
        );
        assert_eq!(
            status_with(false, codex, None, |_| true),
            AgentStatus::NeedsInstall
        );
        assert_eq!(
            status_with(false, agy, None, |_| true),
            AgentStatus::Missing
        );
        assert_eq!(
            status_with(false, &manual, None, |_| true),
            AgentStatus::Missing
        );
    }

    #[test]
    fn status_checks_claude_login_markers() {
        let claude = find("claude").unwrap();
        let home = tempfile::tempdir().unwrap();

        assert_eq!(
            status_with(true, claude, Some(home.path()), |_| false),
            AgentStatus::NeedsLogin
        );
        assert_eq!(
            status_with(true, claude, None, |_| false),
            AgentStatus::NeedsLogin
        );
        assert_eq!(
            status_with(true, claude, Some(home.path()), |k| k
                == "ANTHROPIC_API_KEY"),
            AgentStatus::Ready
        );
        assert_eq!(
            status_with(true, claude, Some(home.path()), |k| {
                k == "CLAUDE_CODE_OAUTH_TOKEN"
            }),
            AgentStatus::Ready
        );

        std::fs::write(home.path().join(".claude.json"), "{}").unwrap();
        assert_eq!(
            status_with(true, claude, Some(home.path()), |_| false),
            AgentStatus::Ready
        );

        let home_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(home_dir.path().join(".claude")).unwrap();
        assert_eq!(
            status_with(true, claude, Some(home_dir.path()), |_| false),
            AgentStatus::Ready
        );
    }

    #[test]
    fn status_checks_codex_login_markers() {
        let codex = find("codex").unwrap();
        let home = tempfile::tempdir().unwrap();

        assert_eq!(
            status_with(true, codex, Some(home.path()), |_| false),
            AgentStatus::NeedsLogin
        );
        assert_eq!(
            status_with(true, codex, Some(home.path()), |k| k == "OPENAI_API_KEY"),
            AgentStatus::Ready
        );
        assert_eq!(
            status_with(true, codex, Some(home.path()), |k| k == "CODEX_API_KEY"),
            AgentStatus::Ready
        );

        std::fs::create_dir(home.path().join(".codex")).unwrap();
        assert_eq!(
            status_with(true, codex, Some(home.path()), |_| false),
            AgentStatus::NeedsLogin
        );
        std::fs::write(home.path().join(".codex/auth.json"), "{}").unwrap();
        assert_eq!(
            status_with(true, codex, Some(home.path()), |_| false),
            AgentStatus::Ready
        );
    }

    #[test]
    fn status_checks_antigravity_login_markers() {
        let agy = find("antigravity").unwrap();
        let home = tempfile::tempdir().unwrap();

        assert_eq!(
            status_with(true, agy, Some(home.path()), |_| false),
            AgentStatus::NeedsLogin
        );
        assert_eq!(
            status_with(true, agy, Some(home.path()), |k| k == "GEMINI_API_KEY"),
            AgentStatus::Ready
        );
        assert_eq!(
            status_with(true, agy, Some(home.path()), |k| k == "GOOGLE_API_KEY"),
            AgentStatus::Ready
        );

        std::fs::create_dir_all(home.path().join(".gemini/antigravity-cli")).unwrap();
        assert_eq!(
            status_with(true, agy, Some(home.path()), |_| false),
            AgentStatus::Ready
        );

        let home_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(home_dir.path().join(".gemini")).unwrap();
        assert_eq!(
            status_with(true, agy, Some(home_dir.path()), |_| false),
            AgentStatus::Ready
        );
    }

    #[test]
    fn status_treats_unknown_installed_agent_as_ready() {
        let custom = AgentSpec {
            id: "custom-agent",
            name: "Custom",
            bin: "custom",
            args: &[],
            install: Install::None,
            note: "",
        };
        assert_eq!(
            status_with(true, &custom, None, |_| false),
            AgentStatus::Ready
        );
    }
}
