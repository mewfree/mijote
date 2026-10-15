# AGENTS.md — mijote

`project.org` is the canonical task tracker. Check it before starting work and
mark tasks `** DONE` when finished. Do not add features without a task entry.

## source layout

| Path | Purpose |
|------|---------|
| `src/main.rs` | clap, TUI lifecycle, event loop |
| `src/app.rs` | app state (planned) |
| `src/config.rs` | config loading (planned) |
| `src/agent/` | agent trait, registry, ACP and stream-json transports |
| `src/session.rs` | task -> session -> transcript (planned) |
| `src/worktree.rs` | git worktree per task (planned) |
| `src/ui/` | ratatui rendering (planned) |

Rule: `agent/`, `session.rs` and `worktree.rs` never import from `ui/` or `app.rs`,
so they can be lifted into a core crate later if a second frontend appears.

## conventions

- Rust edition 2021, stable; `make fmt` and `make lint` before finishing
- No `unwrap()` in non-test code; `thiserror` for typed errors in `agent/`, `anyhow` in `main.rs`
- Unit tests in-file (`#[cfg(test)]`), integration tests in `tests/`
- Backends wrap official agent CLIs; never extract or proxy subscription credentials
