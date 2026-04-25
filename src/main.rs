mod commands;
mod git;
mod langs;
mod process;
mod reports;
mod run_log;
mod runtime;
mod verdict;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::commands::pr_comment::{PrCommentArgs, pr_comment};
use crate::commands::run::{RunArgs, run};
use crate::commands::update_baseline::{UpdateBaselineArgs, update_baseline};

#[derive(Parser)]
#[command(
    name = "shield",
    version,
    about = "Wasmer anti-regression shield — upstream language suites vs. a Wasmer build"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the upstream test suite for a language.
    Run(RunArgs),
    /// Commit tests_*_results.json and tests_*_summary.json into the named baseline branch.
    #[command(name = "update-baseline")]
    UpdateBaseline(UpdateBaselineArgs),
    /// Render the PR comment body from comparison + metadata.
    PrComment(PrCommentArgs),
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args),
        Command::UpdateBaseline(args) => update_baseline(args),
        Command::PrComment(args) => pr_comment(args),
    }
}
