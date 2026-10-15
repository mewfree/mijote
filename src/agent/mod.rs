//! Agent backends: each one wraps an official CLI (or a thin ACP adapter
//! around it) so existing subscriptions are reused as-is.

pub mod acp;

use std::path::PathBuf;

/// How to launch an agent that speaks ACP over stdio.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub bin: &'static str,
    pub args: &'static [&'static str],
    /// Shown by `mijote agents`; e.g. adapter to install, ToS caveats.
    pub note: &'static str,
}

/// Known backends. See <https://agentclientprotocol.com/get-started/agents>.
pub const KNOWN: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        name: "Claude Code",
        bin: "claude-agent-acp",
        args: &[],
        note: "Zed's adapter around the Claude Agent SDK",
    },
    AgentSpec {
        id: "gemini",
        name: "Gemini CLI",
        bin: "gemini",
        args: &["--experimental-acp"],
        note: "native ACP flag",
    },
    AgentSpec {
        id: "codex",
        name: "Codex CLI",
        bin: "codex-acp",
        args: &[],
        note: "ACP adapter around Codex",
    },
    AgentSpec {
        id: "antigravity",
        name: "Antigravity",
        bin: "antigravity-acp",
        args: &[],
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
