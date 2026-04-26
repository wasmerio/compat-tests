use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::reports::{RunMetadata, load_metadata, load_status};
use crate::verdict::{ChangeKind, classify_change_kind};

#[derive(Args)]
pub struct UpdateBaselineArgs {
    #[arg(long)]
    pub branch: String,
    #[arg(long, default_value = ".")]
    pub source_dir: PathBuf,
}

pub fn update_baseline(args: UpdateBaselineArgs) -> Result<()> {
    tracing::info!(branch = %args.branch, source_dir = %args.source_dir.display(), "update-baseline");
    let files = baseline_files(&args.source_dir)?;
    if files.is_empty() {
        bail!("no baseline artifacts found");
    }

    git(&["fetch", "origin"])?;
    checkout_branch(&args.branch)?;
    copy_files(&args.source_dir, &files)?;

    let mut add = Command::new("git");
    add.arg("add");
    for file in &files {
        add.arg(file);
    }
    run(add, "spawn git add")?;

    if Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .status()
        .context("spawn git diff --cached --quiet")?
        .success()
    {
        tracing::info!("no snapshot changes to commit");
        println!("{}", head_commit()?);
        return Ok(());
    }

    let message = commit_message(&args.source_dir, &files)?;
    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", &message.subject, "-m", &message.body])
        .env("GIT_AUTHOR_NAME", "shield")
        .env("GIT_AUTHOR_EMAIL", "shield@wasmer.io")
        .env("GIT_COMMITTER_NAME", "shield")
        .env("GIT_COMMITTER_EMAIL", "shield@wasmer.io");
    run(commit, "spawn git commit")?;
    git(&["push", "origin", &args.branch])?;
    println!("{}", head_commit()?);
    Ok(())
}

fn baseline_files(source_dir: &Path) -> Result<Vec<String>> {
    let mut files = std::fs::read_dir(source_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| is_baseline_file(path))
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_string())
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn is_baseline_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("tests_")
                && (name.ends_with("_results.json") || name.ends_with("_summary.json"))
        })
}

fn copy_files(source_dir: &Path, files: &[String]) -> Result<()> {
    for file in files {
        let source = source_dir.join(file);
        std::fs::copy(&source, file)
            .with_context(|| format!("copy {} -> {file}", source.display()))?;
    }
    Ok(())
}

struct CommitMessage {
    subject: String,
    body: String,
}

#[derive(Default)]
struct SummaryDelta {
    pass: isize,
    fail: isize,
    timeout: isize,
    flaky: isize,
    crash: isize,
}

fn commit_message(source_dir: &Path, files: &[String]) -> Result<CommitMessage> {
    let metadata = files
        .iter()
        .find(|file| file.ends_with("_summary.json"))
        .ok_or_else(|| anyhow::anyhow!("no metadata file found"))?;
    let metadata_path = source_dir.join(metadata);
    let metadata = load_metadata(&metadata_path)
        .with_context(|| format!("parse {}", metadata_path.display()))?;
    let change_kind = baseline_change_kind(source_dir, files)?;
    let delta = summary_delta(source_dir, files)?;
    let target = if metadata.wasmer.git_ref.is_empty() {
        metadata.wasmer.commit
    } else if metadata.wasmer.commit.is_empty() {
        metadata.wasmer.git_ref
    } else {
        format!("{} @ {}", metadata.wasmer.git_ref, metadata.wasmer.commit)
    };
    let target = if metadata.wasmer.repo.is_empty() {
        target
    } else {
        format!("{} ({target})", metadata.wasmer.repo)
    };
    Ok(CommitMessage {
        subject: format!("Baseline updated - {}", format_change_kind(change_kind)),
        body: format!("Wasmer: {target}\n{}", format_delta(&delta)),
    })
}

fn baseline_change_kind(source_dir: &Path, files: &[String]) -> Result<ChangeKind> {
    let mut saw_improvement = false;
    for summary_file in files.iter().filter(|file| file.ends_with("_summary.json")) {
        let runner = summary_file
            .strip_prefix("tests_")
            .and_then(|name| name.strip_suffix("_summary.json"))
            .ok_or_else(|| anyhow::anyhow!("invalid summary filename: {summary_file}"))?;
        let results_file = format!("tests_{runner}_results.json");
        let baseline_metadata = if Path::new(summary_file).is_file() {
            load_metadata(Path::new(summary_file))?
        } else {
            RunMetadata::default()
        };
        let candidate_metadata = load_metadata(&source_dir.join(summary_file))?;
        let baseline_status = if Path::new(&results_file).is_file() {
            load_status(Path::new(&results_file))?
        } else {
            Default::default()
        };
        let candidate_status = load_status(&source_dir.join(&results_file))?;
        match classify_change_kind(
            &baseline_status,
            &candidate_status,
            &baseline_metadata,
            &candidate_metadata,
        ) {
            ChangeKind::Regression => return Ok(ChangeKind::Regression),
            ChangeKind::Improvement => saw_improvement = true,
            ChangeKind::NoChanges => {}
        }
    }
    Ok(if saw_improvement {
        ChangeKind::Improvement
    } else {
        ChangeKind::NoChanges
    })
}

fn summary_delta(source_dir: &Path, files: &[String]) -> Result<SummaryDelta> {
    let mut delta = SummaryDelta::default();
    for file in files.iter().filter(|file| file.ends_with("_summary.json")) {
        let new_metadata = load_metadata(&source_dir.join(file))?;
        let old_metadata = if Path::new(file).is_file() {
            load_metadata(Path::new(file))?
        } else {
            RunMetadata::default()
        };
        delta.pass += count_delta(&old_metadata, &new_metadata, "PASS");
        delta.fail += count_delta(&old_metadata, &new_metadata, "FAIL");
        delta.timeout += count_delta(&old_metadata, &new_metadata, "TIMEOUT");
        delta.flaky += count_delta(&old_metadata, &new_metadata, "FLAKY");
        delta.crash += new_metadata.crashes.len() as isize - old_metadata.crashes.len() as isize;
    }
    Ok(delta)
}

fn count_delta(old: &RunMetadata, new: &RunMetadata, key: &str) -> isize {
    new.counts.get(key).copied().unwrap_or_default() as isize
        - old.counts.get(key).copied().unwrap_or_default() as isize
}

fn format_change_kind(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Regression => "REGRESSION",
        ChangeKind::Improvement => "IMPROVEMENT",
        ChangeKind::NoChanges => "NO CHANGE",
    }
}

fn format_delta(delta: &SummaryDelta) -> String {
    format!(
        "Delta: PASS {:+}, FAIL {:+}, TIMEOUT {:+}, FLAKY {:+}, CRASHES {:+}",
        delta.pass, delta.fail, delta.timeout, delta.flaky, delta.crash
    )
}

fn checkout_branch(branch: &str) -> Result<()> {
    if has_ref(&format!("refs/remotes/origin/{branch}"))? {
        git(&["checkout", "-f", "-B", branch, &format!("origin/{branch}")])
    } else {
        git(&["checkout", "-f", "-B", branch])
    }
}

fn has_ref(reference: &str) -> Result<bool> {
    Ok(Command::new("git")
        .args(["show-ref", "--verify", "--quiet", reference])
        .status()
        .with_context(|| format!("spawn git show-ref {reference}"))?
        .success())
}

fn head_commit() -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("spawn git rev-parse HEAD")?;
    if !out.status.success() {
        bail!("git rev-parse HEAD exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git(args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    run(cmd, &format!("spawn git {args:?}"))
}

fn run(mut cmd: Command, context: &str) -> Result<()> {
    let status = cmd.status().context(context.to_string())?;
    if status.success() {
        Ok(())
    } else {
        bail!("{context}: exited with {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::{SummaryDelta, format_change_kind, format_delta};
    use crate::verdict::ChangeKind;

    #[test]
    fn classifies_regression_when_failures_increase() {
        assert_eq!(
            format_change_kind(ChangeKind::Regression),
            "REGRESSION"
        );
    }

    #[test]
    fn classifies_improvement_when_failures_drop() {
        assert_eq!(
            format_change_kind(ChangeKind::Improvement),
            "IMPROVEMENT"
        );
    }

    #[test]
    fn falls_back_to_no_change_when_deltas_cancel_out() {
        assert_eq!(format_change_kind(ChangeKind::NoChanges), "NO CHANGE");
    }

    #[test]
    fn formats_delta_summary() {
        assert_eq!(
            format_delta(&SummaryDelta {
                pass: 4,
                fail: -2,
                timeout: 0,
                flaky: 1,
                crash: -3,
            }),
            "Delta: PASS +4, FAIL -2, TIMEOUT +0, FLAKY +1, CRASHES -3"
        );
    }
}
