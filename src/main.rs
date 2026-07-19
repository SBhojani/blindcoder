//! blindcoder CLI entry point.
//!
//! One binary, several subcommands. [`simulate`] is the offline convergence harness that validates
//! the selector with synthetic raters. [`run`] launches a blinded proxy that routes to a picked
//! model and streams responses back; [`rate`] records (or corrects) a past session's quality.
//! [`stats`] prints a per-model leaderboard from the event store. [`reveal`] lands in a later
//! milestone.

mod run;
mod simulate;
mod stats;

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
    /// Launch an agentic CLI (e.g. `opencode`) on a blinded model, or run a standing proxy.
    Run(run::RunArgs),
    /// Rate a past session after the fact (difficulty captured post-hoc; corrections supersede).
    Rate(run::RateArgs),
    /// Unmask a session's model — gated and logged. (Later milestone.)
    Reveal,
    /// Show per-alias quality/cost/value leaderboards.
    Stats(stats::StatsArgs),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::load(cli.config.as_deref())?;

    match cli.cmd {
        Cmd::Simulate(args) => simulate::run(&args, &cfg),
        Cmd::Sweep(args) => simulate::run_sweep(&args, &cfg),
        Cmd::Run(args) => run::run(&cfg, &args),
        Cmd::Rate(args) => run::rate(&args),
        Cmd::Stats(args) => stats::run(&args, &cfg),
        Cmd::Reveal => {
            eprintln!(
                "`reveal` lands in a later milestone. Available now: `simulate`, `sweep`, \
                 `run`, `rate`, `stats`.\n\
                 Try:  blindcoder stats --help"
            );
            std::process::exit(2);
        }
    }
}
