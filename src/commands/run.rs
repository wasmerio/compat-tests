use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use rayon::prelude::*;

use crate::git::{ensure_checkout, file_json};
use crate::langs::node::NodeRunner;
use crate::langs::php::PhpRunner;
use crate::langs::python::PythonRunner;
use crate::langs::rust::RustRunner;
use crate::langs::{LangRunner, Mode, Status, TestResult, Workspace};
use crate::reports::{finalize_debug_run, finalize_run};
use crate::run_log::RunLog;
use crate::runtime::{RuntimeSource, WasmerRuntime};

const RETEST_TIMEOUT: Duration = Duration::from_secs(300);
const RETEST_RUNS: usize = 3;

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
    /// Upstream language suite to run.
    #[arg(long)]
    pub lang: Lang,

    /// Optional test or module filter for a debug run.
    pub filter: Option<String>,

    /// Path to existing Wasmer binary to use for testing.
    #[arg(long, conflicts_with = "wasmer_ref")]
    pub wasmer: Option<PathBuf>,

    /// Wasmer git ref to download/build when `--wasmer` is not supplied.
    #[arg(long)]
    pub wasmer_ref: Option<String>,

    /// Per-process timeout used for Wasmer invocations. It is NOT a timeout per unit
    /// test as often tests runs in modules which may contain many unit tests inside
    #[arg(long, value_parser = humantime::parse_duration, default_value = "10m")]
    pub timeout: Duration,

    /// Git ref used to load baseline `status.json` for stabilization/comparison.
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
    match args.lang {
        Lang::Python => run_with_runner(args, &started_at, &PythonRunner::new()),
        Lang::Node => run_with_runner(args, &started_at, &NodeRunner),
        Lang::Php => run_with_runner(args, &started_at, &PhpRunner),
        Lang::Rust => run_with_runner(args, &started_at, &RustRunner),
    }
}

fn run_with_runner(args: RunArgs, started_at: &str, runner: &dyn LangRunner) -> Result<()> {
    let opts = runner.opts();
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
    let process_log = Arc::new(RunLog::new(
        workspace.output_dir.join(format!("test_{}.log", opts.name)),
    ));
    let resolved_wasmer = WasmerRuntime::resolve(
        if let Some(path) = &args.wasmer {
            RuntimeSource::LocalBinary(path.clone())
        } else {
            RuntimeSource::GitRef(
                args.wasmer_ref
                    .clone()
                    .unwrap_or_else(|| "main".to_string()),
            )
        },
        &work_root,
        args.timeout,
        process_log.clone(),
    )?;
    let wasmer = resolved_wasmer.runtime;
    let log = matches!(mode, Mode::Capture).then_some(process_log);

    if let Some(log) = &log {
        log.clear()?;
    }

    if matches!(mode, Mode::Debug) {
        let report = execute_tests(
            runner,
            &workspace,
            &wasmer,
            None,
            args.filter.as_deref(),
            mode,
        )?;
        finalize_debug_run(&report)?;
        return Ok(());
    }

    let report = execute_tests(
        runner,
        &workspace,
        &wasmer,
        log.as_deref(),
        None,
        Mode::Capture,
    )?;

    let status = results_by_id(&report.results);
    let baseline_status =
        if workspace.output_dir.join(".git").exists() && !args.compare_ref.is_empty() {
            file_json::<BTreeMap<String, Status>>(
                &workspace.output_dir,
                &args.compare_ref,
                "status.json",
            )?
            .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
    let (status, flaky_count) = stabilize_changed_tests(
        runner,
        &workspace,
        &wasmer,
        log.as_deref(),
        &baseline_status,
        status,
    )?;

    finalize_run(
        &workspace,
        &resolved_wasmer.identity,
        args.timeout,
        args.filter.as_deref(),
        opts.name,
        opts.git_ref,
        started_at,
        status,
        flaky_count,
        &report.errors,
    )
}

fn stabilize_changed_tests(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    baseline_status: &BTreeMap<String, Status>,
    candidate_status: BTreeMap<String, Status>,
) -> Result<(BTreeMap<String, Status>, usize)> {
    let changed: Vec<String> = baseline_status
        .iter()
        .filter(|(test, old)| candidate_status.get(*test).is_some_and(|new| new != *old))
        .map(|(test, _)| test.clone())
        .collect();
    if changed.is_empty() {
        return Ok((candidate_status, 0));
    }

    tracing::info!(
        changed = changed.len(),
        reruns = RETEST_RUNS,
        timeout_seconds = RETEST_TIMEOUT.as_secs(),
        "stabilizing changed tests"
    );
    let reruns: Vec<Result<(String, Status, bool)>> = changed
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
                    .copied()
                    .unwrap_or(Status::Fail),
                candidate_status
                    .get(test_name)
                    .copied()
                    .unwrap_or(Status::Fail),
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

fn classify_changed_test(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    test_name: &str,
    old_status: Status,
    new_status: Status,
) -> Result<(String, Status, bool)> {
    let rerun_once = || rerun_status(runner, workspace, wasmer, test_name, log);

    if new_status != Status::Pass {
        let outcome = rerun_once()?;
        if outcome == new_status {
            Ok((test_name.to_string(), new_status, false))
        } else {
            Ok((test_name.to_string(), old_status, true))
        }
    } else {
        for _ in 0..RETEST_RUNS {
            if rerun_once()? != Status::Pass {
                return Ok((test_name.to_string(), old_status, true));
            }
        }
        Ok((test_name.to_string(), Status::Pass, false))
    }
}

fn now_utc() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

fn execute_tests(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    filter: Option<&str>,
    mode: Mode,
) -> Result<ExecutionReport> {
    runner.prepare(workspace, wasmer)?;
    let ids = runner.discover(workspace, wasmer, filter)?;
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

fn rerun_status(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    id: &str,
    log: Option<&RunLog>,
) -> Result<Status> {
    let tests = runner.run_test(workspace, wasmer, id, Mode::Debug, log)?;
    if tests.len() != 1 {
        bail!("debug rerun for {id} produced {} results", tests.len());
    }
    Ok(match tests.into_iter().next().unwrap().status {
        Status::Pass => Status::Pass,
        Status::Fail => Status::Fail,
        Status::Skip => Status::Skip,
        Status::Timeout => Status::Timeout,
        Status::Flaky => bail!("debug rerun for {id} returned FLAKY"),
    })
}

fn results_by_id(results: &[TestResult]) -> BTreeMap<String, Status> {
    results
        .iter()
        .map(|result| (result.id.clone(), result.status))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::langs::tests::MockRunner;
    use crate::runtime::RuntimeSource;
    use tempdir::TempDir;

    #[test]
    fn mock_runner_reports_mixed_statuses() {
        let workspace = Workspace {
            output_dir: PathBuf::new(),
            checkout: PathBuf::new(),
            work_dir: PathBuf::new(),
        };
        let dir = TempDir::new("shield-run").expect("tempdir");
        let wasmer = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary("wasmer".into()),
            dir.path(),
            Duration::ZERO,
            Arc::new(RunLog::new(dir.path().join("process.log"))),
        )
        .expect("resolve")
        .runtime;
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
