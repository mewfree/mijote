mod agent;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mijote", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List agent CLIs found on PATH
    Agents,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Agents) {
        Command::Agents => {
            for spec in agent::KNOWN {
                match agent::which(spec.bin) {
                    Some(p) => println!("{:<8} {:<12} {}", spec.id, spec.name, p.display()),
                    None => println!("{:<8} {:<12} (not found)", spec.id, spec.name),
                }
            }
        }
    }
    Ok(())
}
