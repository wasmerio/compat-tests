use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use clap::{Args, ValueEnum};
use rayon::prelude::*;

use crate::git::{ensure_checkout, file_json};
use crate::langs::python::PythonRunner;
use crate::langs::{LangRunner, Mode, Status, TestResult, Workspace};
use crate::reports::{ReportContext, finalize_debug_run, finalize_run};
use crate::run_log::RunLog;
use crate::wasmer::WasmerRuntime;

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

impl StatusCounts {
    pub fn increment(&mut self, status: Status) {
        *self.0.entry(status).or_insert(0) += 1;
    }
}

pub fn run(args: RunArgs) -> Result<()> {
    let started_at = now_utc();
    let wasmer_path = args.wasmer.clone().ok_or_else(|| {
        anyhow!("only --wasmer <PATH> is wired up so far; --wasmer-ref / default `main` not yet implemented")
    })?;
    if !wasmer_path.is_file() {
        bail!("--wasmer {} is not a file", wasmer_path.display());
    }
    if !matches!(args.lang, Lang::Python) {
        bail!(
            "runner for {:?} not yet implemented — only python works today",
            args.lang
        );
    }

    let runner = PythonRunner::new();
    let opts = PythonRunner::OPTS;
    let output_dir = std::env::current_dir()?;
    let work_dir = output_dir.join(".work").join(opts.name);
    let checkout = ensure_checkout(&work_dir, opts.git_repo, opts.git_ref)?;
    let workspace = Workspace {
        output_dir,
        checkout,
        work_dir,
    };
    let wasmer = WasmerRuntime::new(wasmer_path.clone(), args.timeout);
    let mode = if args.filter.is_some() {
        Mode::Debug
    } else {
        Mode::Capture
    };
    let log =
        matches!(mode, Mode::Capture).then(|| RunLog::new(workspace.output_dir.join("test.log")));

    if let Some(log) = &log {
        log.clear()?;
    }

    let report = execute_tests(
        &runner,
        &workspace,
        &wasmer,
        log.as_ref(),
        args.filter.as_deref(),
        mode,
    )?;
    if matches!(mode, Mode::Debug) {
        finalize_debug_run(&report)?;
        return Ok(());
    }

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
    let (status, flaky_count) = stabilize_changed_tests(
        &runner,
        &workspace,
        &wasmer,
        log.as_ref(),
        &baseline_status,
        status,
    )?;

    finalize_run(
        &workspace,
        &wasmer_path,
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
            Err(e) => {
                errors.push(e);
            }
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

fn classify_single_test(results: &[TestResult], test_name: &str) -> String {
    results
        .iter()
        .find(|r| r.id == test_name)
        .or_else(|| results.first())
        .map(|r| status_name(r.status).to_string())
        .unwrap_or_else(|| "FAIL".to_string())
}

fn stabilize_changed_tests<R: LangRunner>(
    runner: &R,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    baseline_status: &BTreeMap<String, String>,
    candidate_status: BTreeMap<String, String>,
) -> Result<(BTreeMap<String, String>, usize)> {
    let changed: Vec<String> = baseline_status
        .iter()
        .filter_map(|(test, old)| {
            candidate_status
                .get(test)
                .filter(|new| *new != old)
                .map(|_| test.clone())
        })
        .collect();
    if changed.is_empty() {
        return Ok((candidate_status, 0));
    }

    let reruns: Vec<Result<(String, String, bool)>> = changed
        .par_iter()
        .map(|test_name| {
            classify_changed_test(
                runner,
                workspace,
                wasmer,
                log,
                test_name,
                baseline_status
                    .get(test_name)
                    .map(String::as_str)
                    .unwrap_or("FAIL"),
                candidate_status
                    .get(test_name)
                    .map(String::as_str)
                    .unwrap_or("FAIL"),
            )
        })
        .collect();

    let mut effective = candidate_status;
    let mut flaky_count = 0;
    for rerun in reruns {
        let (test_name, effective_status, flaky) = rerun?;
        effective.insert(test_name, effective_status);
        if flaky {
            flaky_count += 1;
        }
    }
    Ok((effective, flaky_count))
}

fn classify_changed_test<R: LangRunner>(
    runner: &R,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    test_name: &str,
    old_status: &str,
    new_status: &str,
) -> Result<(String, String, bool)> {
    let rerun_once = || -> Result<String> {
        let results = runner.run_test(workspace, wasmer, test_name, Mode::Capture, log)?;
        Ok(classify_single_test(&results, test_name))
    };

    if new_status != "PASS" {
        let outcome = rerun_once()?;
        if outcome == new_status {
            Ok((test_name.to_string(), new_status.to_string(), false))
        } else {
            Ok((test_name.to_string(), old_status.to_string(), true))
        }
    } else {
        for _ in 0..3 {
            if rerun_once()? != "PASS" {
                return Ok((test_name.to_string(), old_status.to_string(), true));
            }
        }
        Ok((test_name.to_string(), "PASS".to_string(), false))
    }
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
