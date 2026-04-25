use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::reports::load_metadata;

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
    git(&["config", "user.name", "compat-tests[bot]"])?;
    git(&[
        "config",
        "user.email",
        "compat-tests[bot]@users.noreply.github.com",
    ])?;
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

    git(&["commit", "-m", &commit_message(&args.source_dir, &files)?])?;
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

fn commit_message(source_dir: &Path, files: &[String]) -> Result<String> {
    let metadata = files
        .iter()
        .find(|file| file.ends_with("_summary.json"))
        .ok_or_else(|| anyhow::anyhow!("no metadata file found"))?;
    let metadata_path = source_dir.join(metadata);
    let metadata = load_metadata(&metadata_path)
        .with_context(|| format!("parse {}", metadata_path.display()))?;
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
    Ok(format!("compat: refresh snapshot for {target}"))
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
