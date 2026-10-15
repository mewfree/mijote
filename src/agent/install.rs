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
        let spec = find("antigravity").unwrap();
        // Only meaningful when the adapter is absent, which is the CI case.
        if locate(spec).is_none() {
            let err = ensure(spec).unwrap_err();
            assert!(matches!(err, InstallError::Manual(..)));
        }
    }
}
