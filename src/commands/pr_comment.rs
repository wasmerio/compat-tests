use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::reports::load_language_summary;

#[derive(Args)]
pub struct PrCommentArgs {
    /// Wasmer repository under test, for example `wasmerio/wasmer`.
    #[arg(long)]
    pub target_repo: String,
    /// Exact Wasmer commit SHA this compat run is expected to cover.
    #[arg(long)]
    pub target_sha: String,
    /// URL of the compat-tests workflow run to link from the PR comment.
    #[arg(long)]
    pub run_url: String,
    /// Repository where the PR comment should be posted.
    #[arg(long)]
    pub comment_repo: String,
    /// Pull request number to comment on.
    #[arg(long)]
    pub comment_pr_number: String,
    /// GitHub token used to post the PR comment.
    #[arg(long)]
    pub github_token: String,
    /// Optional compat-tests branch that stores the published PR snapshot.
    #[arg(long, default_value = "")]
    pub results_branch: String,
    /// Optional compat-tests commit that stores the published PR snapshot.
    #[arg(long, default_value = "")]
    pub results_commit: String,
}

pub fn pr_comment(args: PrCommentArgs) -> Result<()> {
    tracing::info!(repo = %args.comment_repo, pr = %args.comment_pr_number, "pr-comment");
    let langs = ["python", "node", "php", "rust"]
        .into_iter()
        .map(|lang| load_language_summary(Path::new("."), lang, &args.target_sha))
        .collect::<Result<Vec<_>>>()?;
    let (body, ok) = build_pr_comment(
        &args.target_repo,
        &args.target_sha,
        &args.run_url,
        &args.results_branch,
        &args.results_commit,
        &langs,
    );
    let body_path = write_body(&body)?;
    post_comment(
        &args.comment_repo,
        &args.comment_pr_number,
        &args.github_token,
        &body_path,
    )?;
    print!("{body}");
    if ok {
        Ok(())
    } else {
        bail!("compat-tests detected missing artifacts or SHA mismatches")
    }
}

fn build_pr_comment(
    target_repo: &str,
    target_sha: &str,
    run_url: &str,
    results_branch: &str,
    results_commit: &str,
    languages: &[(&'static str, String, bool)],
) -> (String, bool) {
    let mut body = String::new();
    let ok = languages.iter().all(|(_, _, ok)| *ok);
    let status = if ok { "OK" } else { "ERROR" };

    let _ = writeln!(body, "compat-tests: {status}");
    let _ = writeln!(body);
    let _ = writeln!(body, "- Wasmer repo: `{target_repo}`");
    let _ = writeln!(body, "- Wasmer SHA: `{target_sha}`");
    let _ = writeln!(body, "- Workflow: {run_url}");
    if !results_commit.is_empty() {
        let _ = writeln!(
            body,
            "- Results commit: https://github.com/wasmerio/compat-tests/commit/{results_commit}"
        );
    }
    if !results_branch.is_empty() {
        let _ = writeln!(
            body,
            "- Results branch: https://github.com/wasmerio/compat-tests/tree/{results_branch}"
        );
    }
    let _ = writeln!(body);
    let _ = writeln!(body, "Languages:");
    for (name, summary, _) in languages {
        let _ = writeln!(body, "- `{name}`: {summary}");
    }

    (body, ok)
}
fn write_body(body: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("shield-pr-comment-{}.md", std::process::id()));
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn post_comment(
    repo: &str,
    pr_number: &str,
    github_token: &str,
    body_path: &PathBuf,
) -> Result<()> {
    let status = Command::new("gh")
        .args([
            "pr",
            "comment",
            pr_number,
            "--repo",
            repo,
            "--body-file",
            &body_path.display().to_string(),
        ])
        .env("GH_TOKEN", github_token)
        .status()
        .context("spawn gh pr comment")?;
    if status.success() {
        Ok(())
    } else {
        bail!("gh pr comment exited with {status}")
    }
}
