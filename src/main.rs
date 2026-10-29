//! `vigil` CLI. Reads the environment here, at the edge, and hands explicit
//! values (state root, config) to the library.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// Block with no ceiling until the watch finishes; print the final
    /// envelope JSON. Exit 0 fired, 2 expired, 3 cancelled, 1 error.
    /// With --stream, print one compact JSON line per new event first.
    Wait {
        id: String,
        #[arg(long)]
        stream: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let _ = cli;
    todo!("dispatch subcommands to vigil::Vigil")
}
