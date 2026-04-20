mod commands;
mod git;
mod langs;
mod reports;
mod run_log;
mod wasmer;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::commands::issue::{IssueArgs, issue};
use crate::commands::pr_comment::{PrCommentArgs, pr_comment};
use crate::commands::publish::{PublishArgs, publish};
use crate::commands::run::{RunArgs, run};

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
    /// Commit results/<lang>/… into the named results branch.
    Publish(PublishArgs),
    /// Render the PR comment body from comparison + metadata.
    PrComment(PrCommentArgs),
    /// Create a regression issue when comparisons show regressions.
    Issue(IssueArgs),
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
        Command::Publish(args) => publish(args),
        Command::PrComment(args) => pr_comment(args),
        Command::Issue(args) => issue(args),
    }
}
