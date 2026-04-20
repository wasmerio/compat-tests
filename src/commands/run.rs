use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Error, Result};
use clap::{Args, ValueEnum};

#[derive(Debug, Clone, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Lang {
    Python,
    Node,
    Php,
    Rust,
}

/// Resolved `--wasmer` source.
#[derive(Debug, Clone)]
pub enum WasmerSource {
    /// Prebuilt wasmer binary at the given path.
    Binary(PathBuf),
    /// Git ref to fetch + build (or fetch a prebuilt artifact for `main`).
    GitRef(String),
}

impl FromStr for WasmerSource {
    type Err = Error;

    fn from_str(source: &str) -> Result<Self> {
        let path = PathBuf::from(source);
        if path.is_file() {
            Ok(Self::Binary(path))
        } else {
            Ok(Self::GitRef(source.to_string()))
        }
    }
}

#[derive(Args)]
pub struct RunArgs {
    /// Language to run.
    #[arg(long)]
    pub lang: Lang,

    /// Test filer - when set, runs tests matching passed substring and uses debug mode: raw stdout/stderr,
    /// no status.json / metadata.json written.
    pub filter: Option<String>,

    /// Wasmer to test against - either path to the local wasmer binary or git ref otherwise
    #[arg(long)]
    pub wasmer: Option<WasmerSource>,

    /// Per-test timeout (e.g. `30s`, `10m`, `1h`).
    #[arg(long, value_parser = humantime::parse_duration, default_value = "10m")]
    pub timeout: Duration,

    /// Git ref inside the shield repo to compare against
    /// (drives flaky detection and comparison.json).
    #[arg(long, default_value = "origin/main")]
    pub compare_ref: String,
}

pub fn run(args: RunArgs) -> Result<()> {
    let wasmer = args
        .wasmer
        .unwrap_or_else(|| WasmerSource::GitRef("main".to_string()));
    tracing::info!(
        lang = ?args.lang,
        filter = args.filter.as_deref(),
        ?wasmer,
        timeout = %humantime::format_duration(args.timeout),
        "run",
    );
    // TODO: wire language plugins, test execution, status/metadata writing.
    Ok(())
}
