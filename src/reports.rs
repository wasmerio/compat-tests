use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::json;

use crate::commands::run::ExecutionReport;
use crate::langs::{Status, Workspace};

pub struct WasmerIdentity {
    pub git_ref: String,
    pub branch: String,
    pub commit: String,
}

pub struct ReportContext<'a> {
    pub runner_name: &'a str,
    pub runner_commit_key: &'a str,
    pub runner_commit: &'a str,
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
    context: ReportContext<'_>,
    started_at: &str,
    status: BTreeMap<String, String>,
    flaky_count: usize,
) -> Result<()> {
    if status.is_empty() {
        bail!("upstream run did not produce any test statuses");
    }

    write_json(&workspace.output_dir.join("status.json"), &status)?;

    let mut counts = counts_from_status(&status);
    counts.insert("FLAKY".to_string(), flaky_count);
    let mut runner_metadata = serde_json::Map::new();
    runner_metadata.insert(
        context.runner_commit_key.to_string(),
        json!(context.runner_commit),
    );
    let metadata = json!({
        "wasmer": {
            "ref": wasmer.git_ref,
            "branch": wasmer.branch,
            "commit": wasmer.commit,
        },
        (context.runner_name): runner_metadata,
        "config": {
            "timeout_seconds": timeout.as_secs(),
            "debug_test": filter,
        },
        "run": {
            "started_at": started_at,
            "finished_at": now_utc(),
        },
        "counts": counts,
    });
    write_json(&workspace.output_dir.join("metadata.json"), &metadata)?;
    tracing::info!(counts = ?counts, errors = 0usize, "done");
    Ok(())
}

fn counts_from_status(status: &BTreeMap<String, String>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::from([
        ("PASS".to_string(), 0),
        ("FAIL".to_string(), 0),
        ("SKIP".to_string(), 0),
        ("TIMEOUT".to_string(), 0),
        ("FLAKY".to_string(), 0),
    ]);
    for value in status.values() {
        if let Some(count) = counts.get_mut(value) {
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
