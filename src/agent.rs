//! Agent backends: each one wraps an official CLI so existing
//! subscriptions (login, quota, billing) are reused as-is.

use std::path::PathBuf;

/// How we talk to an agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Agent Client Protocol (JSON-RPC over stdio).
    Acp,
    /// Fallback: parse the CLI's streaming JSON output.
    StreamJson,
}

#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub bin: &'static str,
    pub args: &'static [&'static str],
    pub transport: Transport,
}

/// Known backends. Argument lists are starting points to verify per CLI.
pub const KNOWN: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        name: "Claude Code",
        bin: "claude",
        args: &["-p", "--output-format", "stream-json", "--verbose"],
        transport: Transport::StreamJson,
    },
    AgentSpec {
        id: "gemini",
        name: "Gemini CLI",
        bin: "gemini",
        args: &["--experimental-acp"],
        transport: Transport::Acp,
    },
    AgentSpec {
        id: "codex",
        name: "Codex CLI",
        bin: "codex",
        args: &["exec", "--json"],
        transport: Transport::StreamJson,
    },
];

/// Locate `bin` on `PATH`.
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}
