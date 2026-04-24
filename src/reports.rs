use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::commands::run::ItemError;
use crate::git::file_json;
use crate::langs::{Status, Workspace};

pub struct WasmerIdentity {
    pub git_ref: String,
    pub branch: String,
    pub commit: String,
}

#[derive(Default, Deserialize)]
pub struct RunMetadata {
    #[serde(default)]
    pub wasmer: WasmerMeta,
    #[serde(default)]
    pub counts: BTreeMap<String, usize>,
}

#[derive(Default, Deserialize)]
pub struct WasmerMeta {
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub commit: String,
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

    write_json(
        &workspace.output_dir.join(status_filename(runner_name)),
        &status,
    )?;

    let mut counts = counts_from_status(&status);
    counts.insert("FLAKY".to_string(), flaky_count);
    let mut runner_metadata = serde_json::Map::new();
    runner_metadata.insert("commit".to_string(), json!(runner_commit));
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
    write_json(
        &workspace.output_dir.join(metadata_filename(runner_name)),
        &metadata,
    )?;
    tracing::info!(counts = ?counts, errors = errors.len(), "done");
    Ok(())
}

pub fn status_filename(runner_name: &str) -> String {
    format!("status_{runner_name}.json")
}

pub fn metadata_filename(runner_name: &str) -> String {
    format!("metadata_{runner_name}.json")
}

pub fn load_baseline_status(
    workspace: &Workspace,
    compare_ref: &str,
    runner_name: &str,
) -> Result<BTreeMap<String, Status>> {
    if !workspace.output_dir.join(".git").exists() || compare_ref.is_empty() {
        return Ok(BTreeMap::new());
    }
    Ok(file_json::<BTreeMap<String, Status>>(
        &workspace.output_dir,
        compare_ref,
        &status_filename(runner_name),
    )?
    .unwrap_or_default())
}

pub fn load_metadata(path: &Path) -> Result<RunMetadata> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn load_language_summary(
    output_dir: &Path,
    lang: &'static str,
    expected_sha: &str,
) -> Result<(&'static str, String, bool)> {
    let path = output_dir.join(metadata_filename(lang));
    if !path.is_file() {
        return Ok((lang, "missing artifact".to_string(), false));
    }

    let metadata = match load_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) => return Ok((lang, format!("invalid metadata: {err}"), false)),
    };

    let actual_sha = metadata
        .wasmer
        .commit
        .get(..expected_sha.len())
        .unwrap_or("");
    let sha_ok = metadata.wasmer.commit == expected_sha || actual_sha == expected_sha;
    let mut counts = ["PASS", "FAIL", "TIMEOUT", "SKIP", "FLAKY"]
        .into_iter()
        .map(|key| format!("{key}={}", metadata.counts.get(key).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(", ");
    if !metadata.wasmer.branch.is_empty() {
        counts = format!(
            "{} @ {} ({counts})",
            metadata.wasmer.branch, metadata.wasmer.commit
        );
    }

    Ok((
        lang,
        if sha_ok {
            counts
        } else {
            format!(
                "sha mismatch: expected {expected_sha}, got {} ({counts})",
                metadata.wasmer.commit
            )
        },
        sha_ok,
    ))
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

fn error_messages(errors: &[ItemError]) -> BTreeMap<String, String> {
    errors
        .iter()
        .map(|error| (error.id.clone(), error.message.clone()))
        .collect()
}
