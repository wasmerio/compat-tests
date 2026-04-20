use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use flate2::read::GzDecoder;
use rayon::prelude::*;
use serde::Deserialize;
use tar::Archive;

use crate::git::{current_branch, ensure_checkout, file_json, head_commit};
use crate::langs::python::PythonRunner;
use crate::langs::{LangRunner, Mode, Status, TestResult, Workspace};
use crate::reports::{ReportContext, WasmerIdentity, finalize_debug_run, finalize_run};
use crate::run_log::RunLog;
use crate::wasmer::WasmerRuntime;

const MIN_CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);
const WASMER_REPO: &str = "https://github.com/wasmerio/wasmer.git";

#[derive(Debug, Clone, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Lang {
    Python,
    Node,
    Php,
    Rust,
}

#[derive(Args)]
pub struct RunArgs {
    #[arg(long)]
    pub lang: Lang,

    pub filter: Option<String>,

    #[arg(long, conflicts_with = "wasmer_ref")]
    pub wasmer: Option<PathBuf>,

    #[arg(long)]
    pub wasmer_ref: Option<String>,

    #[arg(long, value_parser = humantime::parse_duration, default_value = "10m")]
    pub timeout: Duration,

    #[arg(long, default_value = "origin/main")]
    pub compare_ref: String,
}

#[derive(Debug, PartialEq)]
pub struct ExecutionReport {
    pub results: Vec<TestResult>,
    pub counts: StatusCounts,
    pub errors: Vec<ItemError>,
}

#[derive(Debug, PartialEq)]
pub struct StatusCounts(pub HashMap<Status, usize>);

#[derive(Debug, PartialEq)]
pub struct ItemError {
    pub id: String,
    pub message: String,
}

struct ResolvedWasmer {
    bin: PathBuf,
    identity: WasmerIdentity,
}

#[derive(Deserialize)]
struct GitHubRun {
    #[serde(rename = "databaseId")]
    database_id: u64,
    #[serde(rename = "headSha")]
    head_sha: String,
    conclusion: Option<String>,
    status: Option<String>,
    event: Option<String>,
}

impl StatusCounts {
    pub fn increment(&mut self, status: Status) {
        *self.0.entry(status).or_insert(0) += 1;
    }
}

pub fn run(args: RunArgs) -> Result<()> {
    let started_at = now_utc();
    if !matches!(args.lang, Lang::Python) {
        bail!(
            "runner for {:?} not yet implemented — only python works today",
            args.lang
        );
    }

    let runner = PythonRunner::new();
    let opts = PythonRunner::OPTS;
    let output_dir = std::env::current_dir()?;
    let work_root = output_dir.join(".work");
    let work_dir = work_root.join(opts.name);
    let checkout = ensure_checkout(&work_dir, opts.git_repo, opts.git_ref)?;
    let workspace = Workspace {
        output_dir,
        checkout,
        work_dir,
    };
    let mode = if args.filter.is_some() {
        Mode::Debug
    } else {
        Mode::Capture
    };
    let resolved_wasmer = resolve_wasmer(&args, &work_root)?;
    let wasmer = WasmerRuntime::new(
        resolved_wasmer.bin.clone(),
        if matches!(mode, Mode::Capture) {
            args.timeout.max(MIN_CAPTURE_TIMEOUT)
        } else {
            args.timeout
        },
    );
    let log =
        matches!(mode, Mode::Capture).then(|| RunLog::new(workspace.output_dir.join("test.log")));

    if let Some(log) = &log {
        log.clear()?;
    }

    if matches!(mode, Mode::Debug) {
        let report = execute_tests(
            &runner,
            &workspace,
            &wasmer,
            None,
            args.filter.as_deref(),
            mode,
        )?;
        finalize_debug_run(&report)?;
        return Ok(());
    }

    let report = runner.run_suite_capture(
        &workspace,
        &wasmer,
        log.as_ref(),
        args.timeout.max(MIN_CAPTURE_TIMEOUT),
    )?;
    if !report.errors.is_empty() {
        let message = report
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.id, e.message))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("{message}");
    }

    let status = results_to_status(&report.results);
    if status.is_empty() {
        bail!("upstream run did not produce any test statuses");
    }

    let baseline_status =
        if workspace.output_dir.join(".git").exists() && !args.compare_ref.is_empty() {
            file_json::<BTreeMap<String, String>>(
                &workspace.output_dir,
                &args.compare_ref,
                "status.json",
            )?
            .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
    let (status, flaky_count) = runner.stabilize_changed_tests(
        &workspace,
        &wasmer,
        log.as_ref(),
        &baseline_status,
        status,
    )?;

    finalize_run(
        &workspace,
        &resolved_wasmer.identity,
        args.timeout,
        args.filter.as_deref(),
        ReportContext {
            runner_name: opts.name,
            runner_commit_key: "cpython_commit",
            runner_commit: opts.git_ref,
        },
        &started_at,
        status,
        flaky_count,
    )
}

fn now_utc() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

pub fn execute_tests<R: LangRunner>(
    runner: &R,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    filter: Option<&str>,
    mode: Mode,
) -> Result<ExecutionReport> {
    runner.prepare(workspace, wasmer)?;
    let ids = runner.discover(workspace, filter)?;
    if ids.is_empty() {
        match filter {
            Some(f) => bail!("no tests matched filter {f:?}"),
            None => bail!("runner discovered 0 tests"),
        }
    }
    let run_one = |id: &String| -> Result<Vec<TestResult>, ItemError> {
        if matches!(mode, Mode::Debug) {
            println!("\n=== {id} ===");
        }
        runner
            .run_test(workspace, wasmer, id, mode, log)
            .map_err(|e| ItemError {
                id: id.clone(),
                message: format!("{e:#}"),
            })
    };
    let outcomes: Vec<Result<Vec<TestResult>, ItemError>> = match mode {
        Mode::Capture => ids.par_iter().map(run_one).collect(),
        Mode::Debug => ids.iter().map(run_one).collect(),
    };
    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut counts = StatusCounts(HashMap::new());
    for outcome in outcomes {
        match outcome {
            Ok(tests) => {
                for r in tests {
                    counts.increment(r.status);
                    results.push(r);
                }
            }
            Err(e) => errors.push(e),
        }
    }
    Ok(ExecutionReport {
        results,
        counts,
        errors,
    })
}

fn results_to_status(results: &[TestResult]) -> BTreeMap<String, String> {
    let mut status = BTreeMap::new();
    for result in results {
        status.insert(result.id.clone(), status_name(result.status).to_string());
    }
    status
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Pass => "PASS",
        Status::Fail => "FAIL",
        Status::Skip => "SKIP",
        Status::Timeout => "TIMEOUT",
        Status::Flaky => "FLAKY",
    }
}

fn resolve_wasmer(args: &RunArgs, work_root: &Path) -> Result<ResolvedWasmer> {
    if let Some(path) = &args.wasmer {
        if !path.is_file() {
            bail!("--wasmer {} is not a file", path.display());
        }
        let bin = path.canonicalize()?;
        println!("Using local Wasmer binary at {}", bin.display());
        return Ok(ResolvedWasmer {
            identity: resolve_local_wasmer_identity(&bin)?,
            bin,
        });
    }

    let git_ref = args.wasmer_ref.as_deref().unwrap_or("main");
    if git_ref == "main" {
        if let Some((bin, commit)) = try_download_prebuilt_main_wasmer(work_root)? {
            println!("Using prebuilt Wasmer main artifact at {}", bin.display());
            return Ok(ResolvedWasmer {
                bin,
                identity: WasmerIdentity {
                    git_ref: git_ref.to_string(),
                    branch: git_ref.to_string(),
                    commit,
                },
            });
        }
    }

    let checkout = ensure_checkout(&work_root.join("wasmer"), WASMER_REPO, git_ref)?;
    update_wasmer_submodules(&checkout)?;
    println!("Building Wasmer from source at {}", checkout.display());
    run_command(
        Command::new("cargo")
            .args([
                "build",
                "-p",
                "wasmer-cli",
                "--features",
                "llvm",
                "--release",
            ])
            .current_dir(&checkout),
    )?;
    let bin = checkout.join("target").join("release").join("wasmer");
    if !bin.is_file() {
        bail!("built wasmer binary missing at {}", bin.display());
    }
    Ok(ResolvedWasmer {
        bin,
        identity: WasmerIdentity {
            git_ref: git_ref.to_string(),
            branch: git_ref.to_string(),
            commit: head_commit(&checkout)?,
        },
    })
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

fn try_download_prebuilt_main_wasmer(work_root: &Path) -> Result<Option<(PathBuf, String)>> {
    if !command_exists("gh") {
        println!("Prebuilt Wasmer main artifact unavailable: gh CLI not found");
        return Ok(None);
    }
    if std::env::consts::OS != "linux" {
        println!(
            "Prebuilt Wasmer main artifact unavailable: unsupported OS {}",
            std::env::consts::OS
        );
        return Ok(None);
    }
    if !matches!(std::env::consts::ARCH, "x86_64" | "amd64") {
        println!(
            "Prebuilt Wasmer main artifact unavailable: unsupported machine {}",
            std::env::consts::ARCH
        );
        return Ok(None);
    }

    let out = Command::new("gh")
        .args([
            "run",
            "list",
            "--repo",
            "wasmerio/wasmer",
            "--workflow",
            "build.yml",
            "--branch",
            "main",
            "--limit",
            "10",
            "--json",
            "databaseId,headSha,conclusion,status,event",
        ])
        .output()?;
    if !out.status.success() {
        println!("Prebuilt Wasmer main artifact lookup failed:");
        if !out.stderr.is_empty() {
            print!("{}", String::from_utf8_lossy(&out.stderr));
        }
        return Ok(None);
    }
    let runs: Vec<GitHubRun> = serde_json::from_slice(&out.stdout)?;
    let Some(run) = runs.into_iter().find(|run| {
        run.status.as_deref() == Some("completed")
            && run.conclusion.as_deref() == Some("success")
            && run.event.as_deref() == Some("push")
    }) else {
        println!(
            "Prebuilt Wasmer main artifact unavailable: no successful main push build run found"
        );
        return Ok(None);
    };

    let cache_dir = work_root.join("prebuilt-wasmer").join(&run.head_sha);
    let install_dir = cache_dir.join("install");
    let wasmer_bin = install_dir.join("bin").join("wasmer");
    if wasmer_bin.exists() {
        println!(
            "Using cached prebuilt Wasmer main artifact for {}",
            run.head_sha
        );
        return Ok(Some((wasmer_bin, run.head_sha)));
    }

    if cache_dir.exists() {
        std::fs::remove_dir_all(&cache_dir)?;
    }
    std::fs::create_dir_all(&cache_dir)?;
    let download = Command::new("gh")
        .args([
            "run",
            "download",
            &run.database_id.to_string(),
            "--repo",
            "wasmerio/wasmer",
            "-n",
            "wasmer-linux-amd64",
            "-D",
        ])
        .arg(&cache_dir)
        .output()?;
    if !download.status.success() {
        println!("Prebuilt Wasmer main artifact download failed:");
        if !download.stderr.is_empty() {
            print!("{}", String::from_utf8_lossy(&download.stderr));
        }
        return Ok(None);
    }

    let archive = cache_dir.join("wasmer.tar.gz");
    if !archive.exists() {
        println!("Prebuilt Wasmer main artifact download failed: wasmer.tar.gz missing");
        return Ok(None);
    }
    std::fs::create_dir_all(&install_dir)?;
    let archive_file = std::fs::File::open(&archive)?;
    if let Err(error) = Archive::new(GzDecoder::new(archive_file)).unpack(&install_dir) {
        println!("Prebuilt Wasmer main artifact extraction failed: {error}");
        return Ok(None);
    }
    if !wasmer_bin.exists() {
        println!("Prebuilt Wasmer main artifact extraction failed: bin/wasmer missing");
        return Ok(None);
    }
    Ok(Some((wasmer_bin, run.head_sha)))
}

fn update_wasmer_submodules(checkout: &Path) -> Result<()> {
    run_command(
        Command::new("git")
            .args(["submodule", "update", "--init", "--depth", "1", "lib/napi"])
            .current_dir(checkout),
    )
}

fn run_command(cmd: &mut Command) -> Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        bail!("command exited with {status}");
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::langs::tests::MockRunner;

    #[test]
    fn mock_runner_reports_mixed_statuses() {
        let workspace = Workspace {
            output_dir: PathBuf::new(),
            checkout: PathBuf::new(),
            work_dir: PathBuf::new(),
        };
        let wasmer = WasmerRuntime::new(PathBuf::new(), Duration::ZERO);
        let report = execute_tests(&MockRunner, &workspace, &wasmer, None, None, Mode::Capture)
            .expect("execute_tests should succeed");

        assert_eq!(
            report,
            ExecutionReport {
                results: vec![
                    TestResult {
                        id: "pass_a".into(),
                        status: Status::Pass
                    },
                    TestResult {
                        id: "pass_b".into(),
                        status: Status::Pass
                    },
                    TestResult {
                        id: "fail_c".into(),
                        status: Status::Fail
                    },
                    TestResult {
                        id: "skip_d".into(),
                        status: Status::Skip
                    },
                    TestResult {
                        id: "timeout_e".into(),
                        status: Status::Timeout
                    },
                    TestResult {
                        id: "flaky_f".into(),
                        status: Status::Flaky
                    },
                ],
                counts: StatusCounts(HashMap::from([
                    (Status::Pass, 2),
                    (Status::Fail, 1),
                    (Status::Skip, 1),
                    (Status::Timeout, 1),
                    (Status::Flaky, 1),
                ])),
                errors: vec![],
            }
        );
    }
}
