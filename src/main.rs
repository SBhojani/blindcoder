//! blindcoder CLI entry point.
//!
//! One binary, several subcommands. At M0 the real, runnable one is [`simulate`] — the offline
//! convergence harness that validates the selector before any proxy work. The daily-driver
//! subcommands (`run`/`rate`/`reveal`/`stats`) are wired to the persistent core but land in
//! later milestones.

mod simulate;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "blindcoder",
    version,
    about = "A blind, cost/quality-aware router for agentic coding CLIs."
)]
struct Cli {
    /// Path to config.toml (overrides the XDG default).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Offline convergence harness: synthetic raters drive the real selector (the M0 go/no-go).
    Simulate(simulate::SimulateArgs),
    /// Grid form of `simulate`: sweep pool × exploration, CSV to stdout.
    Sweep(simulate::SweepArgs),
    /// Launch a blinded agentic session. (Lands in M0's `run`/M1 — not yet implemented.)
    Run,
    /// Rate a past session after the fact. (Later milestone.)
    Rate,
    /// Unmask a session's model — gated and logged. (Later milestone.)
    Reveal,
    /// Show per-alias quality/cost/value leaderboards. (Later milestone.)
    Stats,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::load(cli.config.as_deref())?;

    match cli.cmd {
        Cmd::Simulate(args) => simulate::run(&args, &cfg),
        Cmd::Sweep(args) => simulate::run_sweep(&args, &cfg),
        Cmd::Run | Cmd::Rate | Cmd::Reveal | Cmd::Stats => {
            eprintln!(
                "This subcommand lands in a later milestone. M0 delivers `simulate` \
                 (and the persistent selector/store/config/alias core it exercises).\n\
                 Try:  blindcoder simulate --help"
            );
            std::process::exit(2);
        }
    }
}
