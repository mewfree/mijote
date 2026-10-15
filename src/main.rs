mod agent;

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
    /// List known agent backends and whether they are on PATH
    Agents,
    /// Send one prompt to an agent over ACP and stream the reply
    Ask {
        /// Agent id (see `mijote agents`)
        agent: String,
        /// The prompt
        prompt: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command.unwrap_or(Command::Agents) {
        Command::Agents => list_agents(),
        Command::Ask { agent, prompt } => ask(&agent, &prompt),
    }
}

fn list_agents() -> Result<()> {
    for spec in agent::KNOWN {
        let found = match agent::which(spec.bin) {
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

fn ask(id: &str, prompt: &str) -> Result<()> {
    let Some(spec) = agent::find(id) else {
        bail!("unknown agent `{id}` (try `mijote agents`)");
    };
    let cwd = std::env::current_dir()?;
    let mut client = AcpClient::spawn(spec.bin, spec.args, &cwd)
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
