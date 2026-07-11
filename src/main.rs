//! blindcoder CLI entry point.
//!
//! One binary, several subcommands. At M0 the real, runnable one is [`simulate`] — the offline
//! convergence harness that validates the selector before any proxy work. The daily-driver
//! subcommands (`run`/`rate`/`reveal`/`stats`) are wired to the persistent core but land in
//! later milestones.

mod run;
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
    /// Pick a blinded model and record a session. (Forwarding transport lands next milestone.)
    Run,
    /// Rate a past session after the fact (difficulty captured post-hoc; corrections supersede).
    Rate(run::RateArgs),
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
        Cmd::Run => run::run(&cfg),
        Cmd::Rate(args) => run::rate(&args),
        Cmd::Reveal | Cmd::Stats => {
            eprintln!(
                "This subcommand lands in a later milestone. Available now: `simulate`, `sweep`, \
                 `run`, `rate`.\n\
                 Try:  blindcoder run --help"
            );
            std::process::exit(2);
        }
    }
}
