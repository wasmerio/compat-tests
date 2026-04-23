use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::json;

use crate::commands::run::{ExecutionReport, ItemError};
use crate::langs::{Status, Workspace};

pub struct WasmerIdentity {
    pub git_ref: String,
    pub branch: String,
    pub commit: String,
}

pub fn finalize_debug_run(report: &ExecutionReport) -> Result<()> {
    let bad = report.counts.0.get(&Status::Fail).copied().unwrap_or(0)
        + report.counts.0.get(&Status::Timeout).copied().unwrap_or(0)
        + report.errors.len();
    if bad > 0 {
        bail!(
            "{bad} of {} items failed",
            report.results.len() + report.errors.len()
        );
    }
    Ok(())
}

pub fn finalize_run(
    workspace: &Workspace,
    wasmer: &WasmerIdentity,
    timeout: Duration,
    filter: Option<&str>,
    runner_name: &str,
    runner_commit: &str,
    started_at: &str,
    status: BTreeMap<String, Status>,
    flaky_count: usize,
    errors: &[ItemError],
) -> Result<()> {
    if status.is_empty() {
        bail!("upstream run did not produce any test statuses");
    }

    write_json(&workspace.output_dir.join("status.json"), &status)?;

    let mut counts = counts_from_status(&status);
    counts.insert("FLAKY".to_string(), flaky_count);
    let mut runner_metadata = serde_json::Map::new();
    runner_metadata.insert(
        runner_commit_key(runner_name).to_string(),
        json!(runner_commit),
    );
    let metadata = json!({
        "wasmer": {
            "ref": wasmer.git_ref,
            "branch": wasmer.branch,
            "commit": wasmer.commit,
        },
        (runner_name): runner_metadata,
        "config": {
            "timeout_seconds": timeout.as_secs(),
            "debug_test": filter,
        },
        "run": {
            "started_at": started_at,
            "finished_at": now_utc(),
        },
        "counts": counts,
        "errors": {
            "panics": error_messages(errors),
        },
    });
    write_json(&workspace.output_dir.join("metadata.json"), &metadata)?;
    tracing::info!(counts = ?counts, errors = errors.len(), "done");
    Ok(())
}

fn counts_from_status(status: &BTreeMap<String, Status>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::from([
        ("PASS".to_string(), 0),
        ("FAIL".to_string(), 0),
        ("SKIP".to_string(), 0),
        ("TIMEOUT".to_string(), 0),
        ("FLAKY".to_string(), 0),
    ]);
    for value in status.values() {
        if let Some(count) = counts.get_mut(&value.to_string()) {
            *count += 1;
        }
    }
    counts
}

fn now_utc() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn runner_commit_key(name: &str) -> &str {
    match name {
        "python" => "cpython_commit",
        _ => "upstream_commit",
    }
}

fn error_messages(errors: &[ItemError]) -> BTreeMap<String, String> {
    errors
        .iter()
        .map(|error| (error.id.clone(), error.message.clone()))
        .collect()
}
