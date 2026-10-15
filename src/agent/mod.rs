//! Agent backends: each one wraps an official CLI (or a thin ACP adapter
//! around it) so existing subscriptions are reused as-is.

pub mod acp;
pub mod install;

use std::path::PathBuf;

/// Where an agent's ACP binary comes from when it is not already on `PATH`.
#[derive(Debug, Clone, Copy)]
pub enum Install {
    /// Nothing to install (the agent CLI itself speaks ACP).
    None,
    /// An npm package, installed into mijote's own data dir at a pinned version.
    Npm {
        package: &'static str,
        version: &'static str,
    },
    /// Not auto-installed; show this hint instead.
    Manual(&'static str),
}

/// How to launch an agent that speaks ACP over stdio.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub bin: &'static str,
    pub args: &'static [&'static str],
    pub install: Install,
    /// Shown by `mijote agents`; e.g. caveats.
    pub note: &'static str,
}

/// Known backends. See <https://agentclientprotocol.com/get-started/agents>.
pub const KNOWN: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        name: "Claude Code",
        bin: "claude-agent-acp",
        args: &[],
        install: Install::Npm {
            package: "@agentclientprotocol/claude-agent-acp",
            version: "0.90.0",
        },
        note: "ACP adapter around the Claude Agent SDK",
    },
    AgentSpec {
        id: "gemini",
        name: "Gemini CLI",
        bin: "gemini",
        args: &["--experimental-acp"],
        install: Install::None,
        note: "native ACP flag",
    },
    AgentSpec {
        id: "codex",
        name: "Codex CLI",
        bin: "codex-acp",
        args: &[],
        install: Install::Npm {
            package: "@agentclientprotocol/codex-acp",
            version: "2.2.2",
        },
        note: "ACP adapter around Codex",
    },
    AgentSpec {
        id: "antigravity",
        name: "Antigravity",
        bin: "antigravity-acp",
        args: &[],
        install: Install::Manual(
            "install https://github.com/shubzkothekar/antigravity-acp (needs Bun or a prebuilt binary, plus `agy`)",
        ),
        note: "community adapter around `agy`; Google's ToS may forbid third-party access",
    },
];

pub fn find(id: &str) -> Option<&'static AgentSpec> {
    KNOWN.iter().find(|a| a.id == id)
}

/// Locate `bin` on `PATH`.
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}
