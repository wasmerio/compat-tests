use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

pub fn ensure_checkout(work_dir: &Path, repo: &str, git_ref: &str) -> Result<PathBuf> {
    let checkout = work_dir.join("checkout");
    std::fs::create_dir_all(work_dir)?;
    if !checkout.join(".git").exists() {
        git(
            &[
                "clone",
                "--depth",
                "1",
                repo,
                &checkout.display().to_string(),
            ],
            None,
        )?;
    }
    let target = if has_local_commit(&checkout, git_ref) {
        rev_parse(&checkout, git_ref)?
    } else {
        git(
            &["fetch", "--depth", "1", "origin", git_ref],
            Some(&checkout),
        )?;
        rev_parse(&checkout, "FETCH_HEAD")?
    };
    if head_commit(&checkout)? != target {
        git(
            &["checkout", "-B", "shield-checkout", &target],
            Some(&checkout),
        )?;
    }
    Ok(checkout)
}

pub fn head_commit(checkout: &Path) -> Result<String> {
    rev_parse(checkout, "HEAD")
}

pub fn current_branch(checkout: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .current_dir(checkout)
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("spawn git symbolic-ref in {}", checkout.display()))?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if branch.is_empty() {
        "local".to_string()
    } else {
        branch
    })
}

pub fn file_json<T: DeserializeOwned>(repo: &Path, git_ref: &str, path: &str) -> Result<Option<T>> {
    let out = Command::new("git")
        .args(["show", &format!("{git_ref}:{path}")])
        .current_dir(repo)
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("spawn git show {git_ref}:{path}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&text)?))
    }
}

fn has_local_commit(checkout: &Path, sha: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "-q", "--verify", &format!("{sha}^{{commit}}")])
        .current_dir(checkout)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn rev_parse(checkout: &Path, rev: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(checkout)
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("spawn git rev-parse {rev}"))?;
    if !out.status.success() {
        bail!("git rev-parse {rev:?} exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<()> {
    tracing::info!(?args, ?cwd, "git");
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let status = cmd
        .status()
        .with_context(|| format!("spawn git {args:?}"))?;
    if !status.success() {
        bail!("git {args:?} exited with {status}");
    }
    Ok(())
}
