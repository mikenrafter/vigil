//! `vigil` CLI. Reads the environment here, at the edge, and hands explicit
//! values (state root, config) to the library.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use vigil::judge::HttpJudge;
use vigil::store::state_root;
use vigil::keepalive::wait_exit_code;
use vigil::{Config, CreateRequest, Envelope, KeepaliveRequest, PersistNote, SystemClock, Vigil, WatchId};

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
        /// This call is the main orchestrator session's (AI1').
        #[arg(long)]
        orchestrator: bool,
    },
    /// Block with no ceiling until the watch finishes; print ONE final
    /// envelope JSON (all events in it; no streaming, R8/V21).
    ///
    /// Exit codes: 0 fired, 1 error, 2 usage, 3 expired, 4 cancelled,
    /// 5 attention (the watch is still live).
    Wait {
        id: String,
        /// Use the orchestrator window and credit checks (AI1', S3).
        #[arg(long)]
        orchestrator: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("cannot start the async runtime");
    let code = runtime.block_on(async_main(cli));
    // C9: shutting the runtime down drops every task that is still running,
    // and with them the reads whose drop SIGKILLs a command's process group.
    // The timeout only bounds threads stuck in blocking I/O (stdin); nothing
    // needs them.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}

/// Run the command; SIGINT or SIGTERM cancels it (C9): the command's future is
/// dropped here, `main` then shuts the runtime down so command cleanup runs,
/// and the process exits 130 (SIGINT) or 143 (SIGTERM).
async fn async_main(cli: Cli) -> i32 {
    use tokio::signal::unix::{signal, SignalKind};
    let (mut interrupt, mut terminate) = match (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) {
        (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("error: cannot handle signals: {error}");
            return 1;
        }
    };
    tokio::select! {
        result = run(cli) => match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        _ = interrupt.recv() => 130,
        _ = terminate.recv() => 143,
    }
}

/// Environment is read here and nowhere else (the library takes explicit
/// values): `XDG_STATE_HOME`, `HOME` and the `VIGIL_*` variables (including the
/// test-only `VIGIL_TEST_FAULT`).
fn build_vigil(root: Option<PathBuf>) -> anyhow::Result<Vigil> {
    let root = match root {
        Some(root) => root,
        None => state_root(std::env::var("XDG_STATE_HOME").ok().as_deref(), std::env::var("HOME").ok().as_deref())
            .ok_or_else(|| anyhow::anyhow!("cannot find a state directory: set XDG_STATE_HOME or HOME, or pass --root"))?,
    };
    let config = Config::from_lookup(|name| std::env::var(name).ok());
    let judge = HttpJudge::new(&config.judge_url, config.judge_timeout, config.health_timeout);
    // Test seam (fault.rs): unset in production, so the plan injects nothing.
    let faults = vigil::FaultPlan::from_env_value(std::env::var(vigil::fault::ENV_VAR).ok().as_deref())
        .map_err(|error| anyhow::anyhow!("invalid {}: {error}", vigil::fault::ENV_VAR))?;
    // A3, S4: who is calling is read here, once, from the whole environment.
    let agent = vigil::cache::AgentEnv::from_vars(std::env::vars());
    Ok(Vigil::new(root, Arc::new(SystemClock), Arc::new(judge), config).with_write_faults(faults).with_agent_env(agent))
}

fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// N3: write an envelope to the process's real standard output and say whether
/// it got there. Rust's own `stdout()` ignores a closed descriptor (EBADF) and
/// `println!` panics on any other failure, so a delivery committed after it
/// could be committed for an envelope nobody received. This writes through a
/// duplicate of descriptor 1 (the duplication itself fails when it is closed),
/// reports every error, and flushes.
fn deliver_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::fd::AsFd;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut stdout = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned().map_err(|error| anyhow::anyhow!("cannot write the envelope to stdout: {error}"))?);
    stdout.write_all(&line).and_then(|()| stdout.flush()).map_err(|error| anyhow::anyhow!("cannot write the envelope to stdout: {error}"))
}

/// DF2: the envelope on stdout is complete even when its save failed; stderr
/// gets one line per failed save naming the state root, for the person
/// watching the terminal. The envelope's `persist` note is what an agent reads.
fn warn_about_unsaved(root: &std::path::Path, envelope: &Envelope) {
    if let Some(PersistNote::NotSaved { failure, .. }) = &envelope.persist {
        eprintln!(
            "vigil: warning: {}: could not save {} (state root {}): {}; the result above is complete, free some space and keep calling",
            failure.kind.label(),
            failure.path.display(),
            root.display(),
            failure.error
        );
    }
    for other in &envelope.persist_failures {
        eprintln!(
            "vigil: warning: {}: could not save {} (state root {}): {}",
            other.kind.label(),
            other.path.display(),
            root.display(),
            other.error
        );
    }
}

/// The process exit code on success.
async fn run(cli: Cli) -> anyhow::Result<i32> {
    let vigil = build_vigil(cli.root)?;
    // D4: finished watches are deleted 7 days after they finish, by any run.
    // Housekeeping must not turn a working command into a failing one.
    let _ = vigil.prune_async().await;
    match cli.command {
        Command::Create { json } => {
            let raw = match json {
                Some(raw) => raw,
                None => {
                    let mut raw = String::new();
                    // C14: async, so the signal branches stay live while stdin is idle.
                    tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut raw).await?;
                    raw
                }
            };
            let request: CreateRequest = serde_json::from_str(&raw).map_err(|error| anyhow::anyhow!("invalid watch JSON: {error}"))?;
            print_json(&vigil.create(request).await?)?;
            Ok(0)
        }
        Command::Status { id } => print_json(&vigil.status(&WatchId::parse(&id)?).await?).map(|()| 0),
        Command::List => print_json(&vigil.list().await?).map(|()| 0),
        Command::Cancel { id } => {
            let envelope = vigil.cancel(&WatchId::parse(&id)?).await?;
            print_json(&envelope)?;
            warn_about_unsaved(vigil.store().root(), &envelope);
            Ok(0)
        }
        Command::Keepalive { id, max_s, orchestrator } => {
            let request = KeepaliveRequest { watch_id: Some(WatchId::parse(&id)?), max_s, orchestrator: orchestrator.then_some(true) };
            let (envelope, delivery) = vigil
                .keepalive_uncommitted(request, &vigil::keepalive::NoProgress, std::future::pending::<()>())
                .await?
                .expect("a call that cannot be cancelled always finishes");
            // N3: print, THEN commit. A failed write leaves the delivery
            // uncommitted (the error ends the process), so the next call replays it.
            deliver_json(&envelope)?;
            vigil.commit_delivery(delivery);
            warn_about_unsaved(vigil.store().root(), &envelope);
            Ok(0)
        }
        Command::Wait { id, orchestrator } => {
            let (envelope, delivery) = vigil.wait_uncommitted(&WatchId::parse(&id)?, orchestrator).await?;
            deliver_json(&envelope)?;
            vigil.commit_delivery(delivery);
            warn_about_unsaved(vigil.store().root(), &envelope);
            Ok(wait_exit_code(&envelope).unwrap_or(0))
        }
        Command::Mcp => vigil::mcp::serve_stdio(Arc::new(vigil)).await.map(|()| 0),
    }
}
