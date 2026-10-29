//! `vigil` CLI. Reads the environment here, at the edge, and hands explicit
//! values (state root, config) to the library.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use vigil::judge::HttpJudge;
use vigil::store::state_root;
use vigil::keepalive::wait_exit_code;
use vigil::{Config, CreateRequest, KeepaliveRequest, SystemClock, Vigil, WatchId};

#[derive(Debug, Parser)]
#[command(name = "vigil", version, about = "Keep an agent's turn open while a cheap judge watches things")]
struct Cli {
    /// State root; default `$XDG_STATE_HOME/vigil`, else `~/.local/state/vigil`.
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the stdio MCP server.
    Mcp,
    /// Create a watch from one JSON object (argument, or stdin if absent).
    Create { json: Option<String> },
    Status { id: String },
    List,
    Cancel { id: String },
    /// Block up to --max-s (30..=270) and print the envelope JSON.
    Keepalive {
        id: String,
        #[arg(long)]
        max_s: Option<u64>,
    },
    /// Block with no ceiling until the watch finishes; print ONE final
    /// envelope JSON (all events in it; no streaming, R8/V21).
    ///
    /// Exit codes: 0 fired, 1 error, 2 usage, 3 expired, 4 cancelled.
    Wait { id: String },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => {}
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

/// Environment is read here and nowhere else (the library takes explicit
/// values): `XDG_STATE_HOME`, `HOME` and the `VIGIL_*` variables.
fn build_vigil(root: Option<PathBuf>) -> anyhow::Result<Vigil> {
    let root = match root {
        Some(root) => root,
        None => state_root(std::env::var("XDG_STATE_HOME").ok().as_deref(), std::env::var("HOME").ok().as_deref())
            .ok_or_else(|| anyhow::anyhow!("cannot find a state directory: set XDG_STATE_HOME or HOME, or pass --root"))?,
    };
    let config = Config::from_lookup(|name| std::env::var(name).ok());
    let judge = HttpJudge::new(&config.judge_url, config.judge_timeout, config.health_timeout);
    Ok(Vigil::new(root, Arc::new(SystemClock), Arc::new(judge), config))
}

fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let vigil = build_vigil(cli.root)?;
    // D4: finished watches are deleted 7 days after they finish, by any run.
    // Housekeeping must not turn a working command into a failing one.
    let _ = vigil.prune();
    match cli.command {
        Command::Create { json } => {
            let raw = match json {
                Some(raw) => raw,
                None => {
                    let mut raw = String::new();
                    std::io::stdin().read_to_string(&mut raw)?;
                    raw
                }
            };
            let request: CreateRequest = serde_json::from_str(&raw).map_err(|error| anyhow::anyhow!("invalid watch JSON: {error}"))?;
            print_json(&vigil.create(request).await?)
        }
        Command::Status { id } => print_json(&vigil.status(&WatchId::parse(&id)?).await?),
        Command::List => print_json(&vigil.list().await?),
        Command::Cancel { id } => print_json(&vigil.cancel(&WatchId::parse(&id)?).await?),
        Command::Keepalive { id, max_s } => {
            let request = KeepaliveRequest { watch_id: Some(WatchId::parse(&id)?), max_s };
            print_json(&vigil.keepalive(request).await?)
        }
        Command::Wait { id } => {
            let envelope = vigil.wait(&WatchId::parse(&id)?).await?;
            print_json(&envelope)?;
            if let Some(code) = wait_exit_code(&envelope) {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Mcp => vigil::mcp::serve_stdio(Arc::new(vigil)).await,
    }
}
