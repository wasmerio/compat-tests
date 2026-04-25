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
    pub repo: String,
    pub git_ref: String,
    pub commit: String,
}

#[derive(Default, Deserialize)]
pub struct RunMetadata {
    #[serde(default)]
    pub wasmer: WasmerMeta,
    #[serde(default)]
    pub counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub crashes: BTreeMap<String, String>,
}

#[derive(Default, Deserialize)]
pub struct WasmerMeta {
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub git_ref: String,
    #[serde(default)]
    pub commit: String,
}

pub struct RunConfig<'a> {
    pub timeout: Duration,
    pub filter: Option<&'a str>,
    pub runner_name: &'a str,
    pub runner_commit: &'a str,
    pub started_at: &'a str,
    pub flaky_count: usize,
}

pub fn finalize_run(
    workspace: &Workspace,
    wasmer: &WasmerIdentity,
    status: BTreeMap<String, Status>,
    errors: &[ItemError],
    config: RunConfig<'_>,
) -> Result<()> {
    if status.is_empty() {
        bail!("upstream run did not produce any test statuses");
    }

    write_json(
        &workspace
            .output_dir
            .join(status_filename(config.runner_name)),
        &status,
    )?;

    let mut counts = counts_from_status(&status);
    counts.insert("FLAKY".to_string(), config.flaky_count);
    let mut runner_metadata = serde_json::Map::new();
    runner_metadata.insert("commit".to_string(), json!(config.runner_commit));
    let metadata = json!({
        "wasmer": {
            "repo": wasmer.repo,
            "ref": wasmer.git_ref,
            "commit": wasmer.commit,
        },
        (config.runner_name): runner_metadata,
        "config": {
            "timeout_seconds": config.timeout.as_secs(),
        },
        "run": {
            "started_at": config.started_at,
            "finished_at": now_utc(),
        },
        "counts": counts,
        "crashes": error_messages(errors),
    });
    write_json(
        &workspace
            .output_dir
            .join(metadata_filename(config.runner_name)),
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
    load_status_at_ref(&workspace.output_dir, compare_ref, runner_name)
}

pub fn load_status(path: &Path) -> Result<BTreeMap<String, Status>> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn load_status_at_ref(
    output_dir: &Path,
    compare_ref: &str,
    runner_name: &str,
) -> Result<BTreeMap<String, Status>> {
    if !output_dir.join(".git").exists() || compare_ref.is_empty() {
        return Ok(BTreeMap::new());
    }
    Ok(file_json::<BTreeMap<String, Status>>(
        output_dir,
        compare_ref,
        &status_filename(runner_name),
    )?
    .unwrap_or_default())
}

pub fn load_metadata(path: &Path) -> Result<RunMetadata> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn load_metadata_at_ref(
    output_dir: &Path,
    compare_ref: &str,
    runner_name: &str,
) -> Result<RunMetadata> {
    if !output_dir.join(".git").exists() || compare_ref.is_empty() {
        return Ok(RunMetadata::default());
    }
    Ok(
        file_json::<RunMetadata>(output_dir, compare_ref, &metadata_filename(runner_name))?
            .unwrap_or_default(),
    )
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
