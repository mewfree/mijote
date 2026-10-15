mod agent;
mod app;
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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the TUI (default)
    Tui,
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
    match Cli::parse().command.unwrap_or(Command::Tui) {
        Command::Tui => tui(),
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

fn tui() -> Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;

    use crossterm::event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyEventKind,
    };
    use crossterm::execute;

    let (tx, rx) = mpsc::channel();
    let mut app = app::App::new(
        std::env::current_dir()?,
        agent::install::data_dir().join("worktrees"),
        store::Store::new(agent::install::data_dir().join("sessions")),
        tx,
    );
    app.restore();
    let mut terminal = ratatui::init();
    let mut mouse_on = false;
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
            terminal.draw(|f| ui::render(f, &app))?;
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    TermEvent::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                    TermEvent::Mouse(m) => app.handle_mouse(m, terminal.size()?.into()),
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    if mouse_on {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
