//! `vigil` CLI. Reads the environment here, at the edge, and hands explicit
//! values (state root, config) to the library.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use vigil::judge::HttpJudge;
use vigil::store::state_root;
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
    /// Going cold (the work is done): cancel the named watches and holds, then
    /// print the instruction to post a BabelTele summary and end the turn, and
    /// the ids still pending. With no ids it cancels nothing: the CLI cannot
    /// tell which watches are this session's.
    Cold { ids: Vec<String> },
    /// Read or set the owner's expiring Claude run limits (default work/review: 1/1).
    Cap {
        #[command(subcommand)]
        command: Option<CapCommand>,
    },
    /// Block up to --max-s (30..=270) and print the envelope JSON.
    Keepalive {
        id: String,
        #[arg(long)]
        max_s: Option<u64>,
        /// This call is the main orchestrator session's (AI1').
        #[arg(long)]
        orchestrator: bool,
        /// The token an orchestrator challenge printed.
        #[arg(long, value_name = "TOKEN")]
        confirm: Option<String>,
    },
    /// Hold the session: a watch on the work itself. Each `--for` is one item
    /// of work: `qex:JOB`, `pid:N`, `file:PATH` (ends when it exists),
    /// `until:RFC3339`, `idle:DIR` (no qex job at or under DIR is running), or
    /// `none` (a T3 delegate: needs --label, ended by `vigil hold done`). Each
    /// item that ends raises `item ended: LABEL: WHY`; the hold is released
    /// when every item has ended. With no work it is a bare hold, live until
    /// cancelled (7 days at most); `vigil hold add` gives it items. Then
    /// `keepalive ID` (main session: add --orchestrator, a hold stores it by
    /// default).
    #[command(args_conflicts_with_subcommands = true)]
    Hold {
        #[command(subcommand)]
        item: Option<HoldItemCommand>,
        #[command(flatten)]
        create: HoldArgs,
    },
    /// Print the current advice (operator rules for agents), for hooks and
    /// scripts: one `id: message` line per advisory, or JSON with --json.
    Advice {
        #[arg(long)]
        json: bool,
    },
    /// Program advisories: rules that tell agents what to do (SPEC "Advisories").
    Advisory {
        #[command(subcommand)]
        command: AdvisoryCommand,
    },
}

#[derive(Debug, clap::Args)]
struct HoldArgs {
    /// Count every initial item as a Claude work or review run; omit for uncounted work.
    #[arg(long, value_enum)]
    cap: Option<vigil::cap::CapClass>,
    #[arg(long)]
    id: Option<String>,
    /// One item of work to wait on (repeatable): qex:JOB, pid:N, file:PATH,
    /// until:RFC3339, idle:DIR, or none.
    #[arg(long = "for", value_name = "SPEC")]
    work: Vec<String>,
    /// Name of the item (with one --for only; default: its spec). Unique per hold.
    #[arg(long)]
    label: Option<String>,
    /// Also wait for every qex job at or under the current directory (an item).
    #[arg(long)]
    until_idle: bool,
    /// Seconds until the hold expires, 1..=604800 (default 604800).
    #[arg(long)]
    deadline_s: Option<u64>,
    /// Seconds each item should take; one `item overdue` attention event
    /// after 1.5x (or 9 minutes over, whichever comes first).
    #[arg(long)]
    expect_s: Option<u64>,
    /// Store whether keepalive calls are the main session's (default true).
    #[arg(long, value_name = "BOOL")]
    orchestrator: Option<bool>,
    /// The token an orchestrator challenge printed.
    #[arg(long, value_name = "TOKEN")]
    confirm: Option<String>,
}

/// `vigil hold add` / `vigil hold done` (W42): change the items of a live hold.
#[derive(Debug, Subcommand)]
enum HoldItemCommand {
    /// Add one item of work to the live hold ID; one keepalive on ID covers it.
    Add {
        id: String,
        /// The work: qex:JOB, pid:N, file:PATH, until:RFC3339, idle:DIR, or
        /// none (a T3 delegate; needs --label).
        #[arg(long = "for", value_name = "SPEC")]
        work: String,
        /// Name of the item (default: its spec). Unique per hold.
        #[arg(long)]
        label: Option<String>,
        /// Seconds this item should take; one `item overdue` event after 1.5x.
        #[arg(long)]
        expect_s: Option<u64>,
        /// Count this item as a Claude work or review run; omit for uncounted work.
        #[arg(long, value_enum)]
        cap: Option<vigil::cap::CapClass>,
    },
    /// End the `none` item LABEL of the live hold ID (a T3 delegate finished).
    Done { id: String, label: String },
}

#[derive(Debug, Subcommand)]
enum CapCommand {
    /// Set positive limits until a required future RFC3339 time.
    Set {
        #[arg(long)]
        work: std::num::NonZeroUsize,
        #[arg(long)]
        review: Option<std::num::NonZeroUsize>,
        #[arg(long)]
        until: chrono::DateTime<chrono::FixedOffset>,
    },
    /// Restore work/review limits to 1/1.
    Reset,
}

#[derive(Debug, Subcommand)]
enum AdvisoryCommand {
    /// List advisories (id, status, params, condition, cache time).
    List,
    /// One advisory: its spec and its current evaluation.
    Show { id: String },
    /// Add an advisory from one JSON object (argument, or stdin if absent).
    Add {
        id: String,
        json: Option<String>,
        /// Replace an existing advisory of that id.
        #[arg(long)]
        replace: bool,
    },
    /// Change settings: `NAME=VALUE` for a parameter (e.g. threshold=15), or
    /// for cache_s, condition, message_when_true, message_when_false.
    Set {
        id: String,
        #[arg(required = true, num_args = 1..)]
        settings: Vec<String>,
    },
    /// Remove an advisory.
    Rm { id: String },
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
        Command::Hold { item: None, create: HoldArgs { id, work, label, until_idle, deadline_s, expect_s, orchestrator, cap, confirm } } => {
            print_json(&vigil.hold(vigil::HoldRequest { id, work, until_idle, deadline_s, expect_s, orchestrator, label, cap, confirm }).await?)?;
            Ok(0)
        }
        Command::Hold { item: Some(HoldItemCommand::Add { id, work, label, expect_s, cap }), .. } => {
            print_json(&vigil.hold_add(&WatchId::parse(&id)?, vigil::HoldItemRequest { work, expect_s, label, cap }).await?)?;
            Ok(0)
        }
        Command::Hold { item: Some(HoldItemCommand::Done { id, label }), .. } => {
            print_json(&vigil.hold_done(&WatchId::parse(&id)?, &label).await?)?;
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
        Command::Cold { ids } => {
            let ids = ids.iter().map(|id| WatchId::parse(id)).collect::<Result<Vec<_>, _>>()?;
            let report = vigil.cold(Some(ids)).await?;
            print_json(&report)?;
            for warning in &report.warnings { eprintln!("vigil: warning: {warning}"); }
            for error in &report.errors { eprintln!("vigil: {error}"); }
            Ok(if report.errors.is_empty() { 0 } else { 1 })
        }
        Command::Cap { command } => {
            let limits = match command {
                None => vigil.cap_limits()?,
                Some(CapCommand::Reset) => vigil.cap_reset().await?,
                Some(CapCommand::Set { work, review, until }) => {
                    match vigil.cap_set(work, review, vigil::Timestamp(until.with_timezone(&chrono::Utc))).await {
                        Ok(limits) => limits,
                        Err(vigil::VigilError::Item(vigil::ItemError::CapUntil)) => {
                            eprintln!("vigil: cap --until must be a future RFC3339 time");
                            return Ok(2);
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            };
            print_json(&limits)?;
            Ok(0)
        }
        Command::Keepalive { id, max_s, orchestrator, confirm } => {
            let request = KeepaliveRequest { watch_id: Some(WatchId::parse(&id)?), max_s, orchestrator: orchestrator.then_some(true), confirm };
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
        Command::Advice { json } => {
            let report = vigil.advice_report().await?;
            if json {
                print_json(&report)?;
            } else {
                for entry in report["advice"].as_array().into_iter().flatten() {
                    println!("{}: {}", entry["id"].as_str().unwrap_or("?"), entry["message"].as_str().unwrap_or(""));
                }
            }
            Ok(0)
        }
        Command::Advisory { command } => {
            let shown = match command {
                AdvisoryCommand::List => vigil.advisory_list()?,
                AdvisoryCommand::Show { id } => vigil.advisory_show(&id).await?,
                AdvisoryCommand::Add { id, json, replace } => {
                    let raw = match json {
                        Some(raw) => raw,
                        None => {
                            let mut raw = String::new();
                            tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut raw).await?;
                            raw
                        }
                    };
                    let spec = serde_json::from_str(&raw).map_err(|error| anyhow::anyhow!("invalid advisory JSON: {error}"));
                    // The id is checked before the body, so a bad id is named first.
                    vigil::AdvisoryId::parse(&id)?;
                    vigil.advisory_add(&id, spec?, replace)?
                }
                AdvisoryCommand::Set { id, settings } => vigil.advisory_set(&id, &settings)?,
                AdvisoryCommand::Rm { id } => vigil.advisory_remove(&id)?,
            };
            print_json(&shown)?;
            Ok(0)
        }
        Command::Mcp => vigil::mcp::serve_stdio(Arc::new(vigil)).await.map(|()| 0),
    }
}
