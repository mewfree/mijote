mod agent;
mod app;
mod clipboard;
mod editor;
mod selection;
mod session;
mod store;
mod ui;
mod worktree;

use std::io::Write;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use agent::acp::{AcpClient, Event, StopReason};

#[derive(Parser)]
#[command(name = "mijote", version, about)]
struct Cli {
    /// Reopen saved sessions for the current directory instead of starting a new one
    #[arg(short, long)]
    resume: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the TUI (default)
    Tui {
        /// Reopen saved sessions for the current directory instead of starting a new one
        #[arg(short, long)]
        resume: bool,
    },
    /// List known agent backends and whether they are on PATH
    Agents,
    /// Install an agent's ACP adapter into mijote's data dir
    Install {
        /// Agent id (see `mijote agents`)
        agent: String,
    },
    /// Send one prompt to an agent over ACP and stream the reply
    Ask {
        /// Agent id (see `mijote agents`)
        agent: String,
        /// The prompt
        prompt: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui { resume: false }) {
        Command::Tui { resume } => tui(cli.resume || resume),
        Command::Agents => list_agents(),
        Command::Install { agent } => install(&agent),
        Command::Ask { agent, prompt } => ask(&agent, &prompt),
    }
}

fn list_agents() -> Result<()> {
    for spec in agent::KNOWN {
        let found = match agent::install::locate(spec) {
            Some(p) => p.display().to_string(),
            None => "(not found)".to_string(),
        };
        println!(
            "{:<12} {:<12} {:<18} {}",
            spec.id, spec.name, spec.bin, found
        );
        println!("{:<12} {}", "", spec.note);
    }
    Ok(())
}

fn resolve(id: &str) -> Result<(&'static agent::AgentSpec, std::path::PathBuf)> {
    let Some(spec) = agent::find(id) else {
        bail!("unknown agent `{id}` (try `mijote agents`)");
    };
    if agent::install::locate(spec).is_none() {
        eprintln!(
            "installing {} adapter into {}...",
            spec.name,
            agent::install::data_dir().display()
        );
    }
    Ok((spec, agent::install::ensure(spec)?))
}

fn install(id: &str) -> Result<()> {
    let (spec, program) = resolve(id)?;
    println!("{} ready: {}", spec.name, program.display());
    Ok(())
}

fn ask(id: &str, prompt: &str) -> Result<()> {
    let (spec, program) = resolve(id)?;
    let cwd = std::env::current_dir()?;
    let mut client = AcpClient::spawn(&program.to_string_lossy(), spec.args, &cwd)
        .with_context(|| format!("starting {}", spec.name))?;
    let name = client.initialize()?;
    eprintln!("connected to {name}");
    let session = client.new_session(&cwd)?;

    let stop = client.prompt(
        &session,
        prompt,
        |event| match event {
            Event::AgentText(t) => {
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            Event::ToolCall { title, kind, .. } => eprintln!("\n[tool:{kind}] {title}"),
            Event::ToolUpdate {
                id,
                status: Some(s),
                ..
            } => eprintln!("[tool {id}] {s}"),
            _ => {}
        },
        |req| {
            eprintln!(
                "\npermission requested: {}",
                req.title.as_deref().unwrap_or(&req.tool_call_id)
            );
            for (i, o) in req.options.iter().enumerate() {
                eprintln!("  {}) {} [{}]", i + 1, o.name, o.kind);
            }
            eprint!("choice (empty = cancel): ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok()?;
            let n: usize = line.trim().parse().ok()?;
            req.options.get(n.checked_sub(1)?).map(|o| o.id.clone())
        },
    )?;
    println!();
    if stop != StopReason::EndTurn {
        eprintln!("stopped: {stop:?}");
    }
    Ok(())
}

fn tui(resume: bool) -> Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;

    use crossterm::event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as TermEvent, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    };
    use crossterm::execute;

    let (tx, rx) = mpsc::channel();
    let mut app = app::App::new(
        std::env::current_dir()?,
        agent::install::data_dir().join("worktrees"),
        store::Store::new(agent::install::data_dir().join("sessions")),
        tx,
    );
    if resume {
        app.restore();
    } else {
        app.open_default_session();
    }
    let mut terminal = ratatui::init();
    // Without the kitty protocol Shift+Enter is indistinguishable from Enter.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let mut mouse_on = false;
    let mut prev_buf: Option<ratatui::buffer::Buffer> = None;
    let mut prev_links: Vec<ui::ScreenLink> = Vec::new();
    let result = (|| -> Result<()> {
        while !app.should_quit {
            if app.mouse != mouse_on {
                mouse_on = app.mouse;
                if mouse_on {
                    execute!(std::io::stdout(), EnableMouseCapture)?;
                } else {
                    execute!(std::io::stdout(), DisableMouseCapture)?;
                }
            }
            while let Ok(msg) = rx.try_recv() {
                app.handle_msg(msg);
            }
            app.tick();
            if app.pending_bell {
                app.pending_bell = false;
                let mut out = std::io::stdout();
                let _ = out.write_all(b"\x07");
                let _ = out.flush();
            }
            let completed = terminal.draw(|f| ui::render(f, &app))?;
            let links = app.visible_links.borrow().clone();
            if prev_buf.as_ref() != Some(completed.buffer) || prev_links != links {
                if !links.is_empty() {
                    let _ = ui::write_osc8_links(
                        &mut std::io::stdout(),
                        &links,
                        completed.buffer,
                        app.cursor_pos.get(),
                    );
                }
                prev_buf = Some(completed.buffer.clone());
                prev_links = links;
            }
            if let Some(text) = app.pending_copy.take() {
                if clipboard::copy(&text).is_err() {
                    app.set_message("copy failed");
                }
            }
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    TermEvent::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                    TermEvent::Paste(text) => app.handle_paste(&text),
                    TermEvent::Mouse(m) => app.handle_mouse(m, terminal.size()?.into()),
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    if mouse_on {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();

    // Stop agents and release untouched worktrees; report what was kept.
    let mut pending = app.close_all();
    while pending > 0 {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(session::Msg {
                body: session::UiMsg::Closed(text),
                ..
            }) => {
                println!("{text}");
                pending -= 1;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_to_new_session_and_supports_resume() {
        let cli = Cli::try_parse_from(["mijote"]).unwrap();
        assert!(!cli.resume);
        assert!(cli.command.is_none());

        let cli = Cli::try_parse_from(["mijote", "--resume"]).unwrap();
        assert!(cli.resume);

        let cli = Cli::try_parse_from(["mijote", "-r"]).unwrap();
        assert!(cli.resume);

        let cli = Cli::try_parse_from(["mijote", "tui", "--resume"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Tui { resume: true })));
    }
}
