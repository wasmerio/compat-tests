use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::json;

use crate::commands::run::ExecutionReport;
use crate::git::{current_branch, head_commit};
use crate::langs::{Status, Workspace};

struct WasmerIdentity {
    git_ref: String,
    branch: String,
    commit: String,
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
    wasmer_bin: &Path,
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

    let identity = resolve_local_wasmer_identity(wasmer_bin)?;
    let mut counts = counts_from_status(&status);
    counts.insert("FLAKY".to_string(), flaky_count);
    let mut runner_metadata = serde_json::Map::new();
    runner_metadata.insert(
        context.runner_commit_key.to_string(),
        json!(context.runner_commit),
    );
    let metadata = json!({
        "wasmer": {
            "ref": identity.git_ref,
            "branch": identity.branch,
            "commit": identity.commit,
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

fn resolve_local_wasmer_identity(wasmer_bin: &Path) -> Result<WasmerIdentity> {
    if let Some(checkout) = infer_wasmer_checkout_from_bin(wasmer_bin) {
        if checkout.join(".git").exists() {
            let branch = current_branch(&checkout)?;
            let commit = head_commit(&checkout)?;
            return Ok(WasmerIdentity {
                git_ref: branch.clone(),
                branch,
                commit,
            });
        }
    }
    Ok(WasmerIdentity {
        git_ref: "local".to_string(),
        branch: "local".to_string(),
        commit: "local".to_string(),
    })
}

fn infer_wasmer_checkout_from_bin(wasmer_bin: &Path) -> Option<PathBuf> {
    wasmer_bin
        .canonicalize()
        .ok()?
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
}

fn now_utc() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}
