use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use rayon::prelude::*;

use crate::git::ensure_checkout;
use crate::langs::node::NodeRunner;
use crate::langs::php::PhpRunner;
use crate::langs::python::PythonRunner;
use crate::langs::rust::RustRunner;
use crate::langs::{LangRunner, Mode, Status, TestJob, TestResult, Workspace};
use crate::reports::{
    RunConfig, RunRegressions, finalize_run, load_baseline_status, test_regressions_filename,
    write_regressions,
};
use crate::run_log::RunLog;
use crate::runtime::{RunSpec, RunTarget, RuntimeSource, WasmerRuntime};

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

    /// Print upstream git ref used as language cache key.
    #[arg(long)]
    pub version: bool,

    /// Optional test or module filter for a debug run.
    pub filter: Option<String>,

    /// Path to existing Wasmer binary to use for testing.
    #[arg(long, conflicts_with_all = ["wasmer_ref", "wasmer_repo"])]
    pub wasmer: Option<PathBuf>,

    /// Wasmer git repository to clone when building from source.
    #[arg(long)]
    pub wasmer_repo: Option<String>,

    /// Wasmer git ref to download/build when `--wasmer` is not supplied.
    #[arg(long)]
    pub wasmer_ref: Option<String>,

    /// Per-process timeout used for Wasmer invocations. It is NOT a timeout per unit
    /// test as often tests runs in modules which may contain many unit tests inside
    #[arg(long, value_parser = humantime::parse_duration, default_value = "10m")]
    pub timeout: Duration,

    /// Git ref used to load baseline language-specific status file for stabilization/comparison.
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

#[derive(Clone, Debug, PartialEq)]
pub struct ItemError {
    pub id: String,
    pub message: String,
}

type StabilizedChange = (String, Status, bool, Option<(Status, String)>, Option<String>);

impl StatusCounts {
    pub fn increment(&mut self, status: Status) {
        *self.0.entry(status).or_insert(0) += 1;
    }
}

pub fn run(args: RunArgs) -> Result<()> {
    if args.version {
        let version = match args.lang {
            Lang::Python => PythonRunner::OPTS.git_ref,
            Lang::Node => NodeRunner::OPTS.git_ref,
            Lang::Php => PhpRunner::OPTS.git_ref,
            Lang::Rust => RustRunner::OPTS.git_ref,
        };
        println!("{version}");
        return Ok(());
    }

    let started_at = now_utc();
    match args.lang {
        Lang::Python => run_with_runner(args, &started_at, &PythonRunner::new())?,
        Lang::Node => run_with_runner(args, &started_at, &NodeRunner)?,
        Lang::Php => run_with_runner(args, &started_at, &PhpRunner)?,
        Lang::Rust => run_with_runner(args, &started_at, &RustRunner)?,
    };
    Ok(())
}

fn run_with_runner(
    args: RunArgs,
    started_at: &str,
    runner: &dyn LangRunner,
) -> Result<ExecutionReport> {
    let output_dir = std::env::current_dir()?;
    let opts = runner.opts();
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
            RuntimeSource::Git {
                repo: args
                    .wasmer_repo
                    .clone()
                    .unwrap_or_else(|| "https://github.com/wasmerio/wasmer.git".to_string()),
                git_ref: args
                    .wasmer_ref
                    .clone()
                    .unwrap_or_else(|| "main".to_string()),
            }
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

    warmup_package(runner, &wasmer).map_err(|e| anyhow::anyhow!("warmup failed: {e:?}"))?;

    let report = execute_tests(
        runner,
        &workspace,
        &wasmer,
        log.as_deref(),
        args.filter.as_deref(),
        mode,
    )?;

    if matches!(mode, Mode::Debug) {
        return Ok(report);
    }

    let status = results_by_id(&report.results);
    let baseline_status = load_baseline_status(&workspace, &args.compare_ref, opts.name)?;
    let (status, flaky_count, regressions, rerun_errors) =
        stabilize_changed_tests(runner, &workspace, &wasmer, &baseline_status, status)?;
    write_regressions(
        &workspace
            .output_dir
            .join(test_regressions_filename(opts.name)),
        &regressions,
    )?;
    let mut errors = report.errors.clone();
    errors.extend(rerun_errors);

    finalize_run(
        &workspace,
        &resolved_wasmer.identity,
        status,
        &errors,
        RunConfig {
            timeout: args.timeout,
            runner_name: opts.name,
            runner_commit: opts.git_ref,
            started_at,
            flaky_count,
        },
    )?;
    Ok(report)
}

fn stabilize_changed_tests(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    baseline_status: &BTreeMap<String, Status>,
    candidate_status: BTreeMap<String, Status>,
) -> Result<(BTreeMap<String, Status>, usize, RunRegressions, Vec<ItemError>)> {
    let changed: Vec<String> = baseline_status
        .iter()
        .filter(|(test, old)| {
            candidate_status
                .get(*test)
                .is_some_and(|new| should_stabilize_status_change(**old, *new))
        })
        .map(|(test, _)| test.clone())
        .collect();
    if changed.is_empty() {
        return Ok((candidate_status, 0, RunRegressions::default(), vec![]));
    }

    tracing::info!(
        changed = changed.len(),
        reruns = RETEST_RUNS,
        timeout_seconds = RETEST_TIMEOUT.as_secs(),
        "stabilizing changed tests"
    );
    let reruns: Vec<Result<StabilizedChange>> = changed
        .par_iter()
        .map(|test_name| {
            stabilize_changed_test(
                runner,
                workspace,
                wasmer,
                baseline_status,
                &candidate_status,
                test_name,
            )
        })
        .collect();

    let mut effective = candidate_status;
    let mut flaky_count = 0;
    let mut regressions = RunRegressions::default();
    let mut errors = Vec::new();
    for rerun in reruns {
        let (test_name, status, flaky, regression, crash) = rerun?;
        effective.insert(test_name.clone(), status);
        if flaky {
            flaky_count += 1;
        }
        if let Some((status_after, output)) = regression {
            regressions.record(test_name.clone(), Status::Pass, status_after, output);
        }
        if let Some(message) = crash {
            errors.push(ItemError {
                id: test_name,
                message,
            });
        }
    }
    Ok((effective, flaky_count, regressions, errors))
}

fn should_stabilize_status_change(old: Status, new: Status) -> bool {
    old != new && (old == Status::Pass || new == Status::Pass)
}

fn stabilize_changed_test(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    baseline_status: &BTreeMap<String, Status>,
    candidate_status: &BTreeMap<String, Status>,
    test_name: &str,
) -> Result<StabilizedChange> {
    let old_status = baseline_status
        .get(test_name)
        .copied()
        .unwrap_or(Status::Fail);
    let new_status = candidate_status
        .get(test_name)
        .copied()
        .unwrap_or(Status::Fail);

    if new_status != Status::Pass {
        let (rerun_status, output, crash) = rerun_status(runner, workspace, wasmer, test_name)?;
        let confirmed = rerun_status == new_status;
        return Ok((
            test_name.to_string(),
            if confirmed { new_status } else { old_status },
            !confirmed,
            if confirmed && old_status == Status::Pass {
                output.map(|output| (new_status, output))
            } else {
                None
            },
            crash,
        ));
    }

    for _ in 0..RETEST_RUNS {
        let (rerun_status, _, crash) = rerun_status(runner, workspace, wasmer, test_name)?;
        if rerun_status != Status::Pass {
            return Ok((test_name.to_string(), old_status, true, None, crash));
        }
    }

    Ok((test_name.to_string(), Status::Pass, false, None, None))
}

fn now_utc() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

fn warmup_package(runner: &dyn LangRunner, wasmer: &WasmerRuntime) -> Result<()> {
    let opts = runner.opts();
    let (package, args) = match (opts.wasmer_package, opts.wasmer_package_warmup_args) {
        (Some(package), Some(args)) => (package, args),
        _ => return Ok(()),
    };
    tracing::info!(
        runner = opts.name,
        package,
        "warming up language runtime package"
    );
    wasmer
        .run(
            RunSpec {
                target: RunTarget::Package(package.to_string()),
                flags: opts
                    .wasmer_flags
                    .iter()
                    .map(|flag| (*flag).to_string())
                    .collect(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                timeout: None,
            },
            crate::process::ignore_stream,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(())
}

fn capture_thread_count(jobs: usize, multiplier: usize) -> usize {
    jobs.max(1)
        .min(num_cpus::get().saturating_mul(multiplier.max(1)))
}

fn execute_tests(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    filter: Option<&str>,
    mode: Mode,
) -> Result<ExecutionReport> {
    let cache_path = workspace
        .output_dir
        .join(".cache")
        .join(runner.opts().name)
        .join("tests.json");
    let use_cache = filter.is_none();
    let jobs = if use_cache && cache_path.is_file() {
        serde_json::from_slice(&fs::read(&cache_path)?)?
    } else {
        let jobs = runner.discover(workspace, wasmer, filter, mode)?;
        if use_cache {
            fs::create_dir_all(cache_path.parent().unwrap())?;
            fs::write(&cache_path, serde_json::to_vec_pretty(&jobs)?)?;
        }
        jobs
    };
    if jobs.is_empty() {
        match filter {
            Some(f) => bail!("no tests matched filter {f:?}"),
            None => bail!("runner discovered 0 tests"),
        }
    }
    runner.prepare(workspace, wasmer, &jobs)?;
    let total_tests: usize = jobs.iter().map(|job| job.tests.len()).sum();
    let completed_tests = AtomicUsize::new(0);
    let run_one = |job: &TestJob| -> (Vec<TestResult>, Option<ItemError>) {
        if matches!(mode, Mode::Debug) {
            println!("\n=== {} ===", job.id);
        }
        if matches!(mode, Mode::Capture) {
            tracing::info!(job = job.id, tests = job.tests.len(), "running test job");
        }
        let (results, error) = match runner.run_test(workspace, wasmer, job, mode, log) {
            Ok(results) => (results, None),
            Err(e) => (
                job.tests
                    .iter()
                    .map(|id| TestResult {
                        id: id.clone(),
                        status: Status::Fail,
                    })
                    .collect(),
                Some(ItemError {
                    id: job.id.clone(),
                    message: job_error_message(job, &e),
                }),
            ),
        };
        if matches!(mode, Mode::Capture) {
            for result in &results {
                let completed = completed_tests.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::info!(
                    completed,
                    total = total_tests,
                    remaining = total_tests.saturating_sub(completed),
                    test = result.id,
                    status = %result.status,
                    "test result"
                );
            }
        }
        (results, error)
    };
    let outcomes: Vec<(Vec<TestResult>, Option<ItemError>)> = match mode {
        Mode::Capture if runner.thread_count_multiplier() > 1 => {
            let threads = capture_thread_count(jobs.len(), runner.thread_count_multiplier());
            tracing::info!(
                threads,
                multiplier = runner.thread_count_multiplier(),
                "using runner-specific capture worker pool"
            );
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .map_err(|e| anyhow::anyhow!("build capture pool: {e}"))?;
            pool.install(|| jobs.par_iter().map(run_one).collect())
        }
        Mode::Capture => jobs.par_iter().map(run_one).collect(),
        Mode::Debug => jobs.iter().map(run_one).collect(),
    };
    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut counts = StatusCounts(HashMap::new());
    for (tests, error) in outcomes {
        for r in tests {
            counts.increment(r.status);
            results.push(r);
        }
        if let Some(error) = error {
            errors.push(error);
        }
    }
    Ok(ExecutionReport {
        results,
        counts,
        errors,
    })
}

fn job_error_message(job: &TestJob, error: &anyhow::Error) -> String {
    if format!("{error}").starts_with("crash: ") {
        return format!("{error:#}");
    }
    let mut message = format!("{error:#}\njob: {}\ntests:", job.id);
    for test in &job.tests {
        message.push_str("\n- ");
        message.push_str(test);
    }
    message
}

fn rerun_status(
    runner: &dyn LangRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    id: &str,
) -> Result<(Status, Option<String>, Option<String>)> {
    let rerun_log_path = rerun_log_path(workspace, id);
    if let Some(parent) = rerun_log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rerun_log = Arc::new(RunLog::new(rerun_log_path.clone()));
    rerun_log.clear()?;
    let rerun_wasmer = wasmer.with_process_log(rerun_log.clone());
    let tests = match runner.run_test(
        workspace,
        &rerun_wasmer,
        &TestJob {
            id: id.to_string(),
            tests: vec![id.to_string()],
        },
        Mode::Debug,
        Some(rerun_log.as_ref()),
    ) {
        Ok(tests) => tests,
        Err(err) => {
            let output = read_rerun_log(&rerun_log_path)?;
            let message = format!("{err:#}");
            return Ok((
                Status::Fail,
                Some(match output {
                    Some(output) if !output.trim().is_empty() => format!("{message}\n\n{output}"),
                    _ => message.clone(),
                }),
                message.starts_with("crash: ").then_some(message),
            ));
        }
    };
    if tests.is_empty() {
        let output = read_rerun_log(&rerun_log_path)?;
        return Ok((
            Status::Fail,
            Some(match output {
                Some(output) if !output.trim().is_empty() => {
                    format!("debug rerun for {id} produced 0 results\n\n{output}")
                }
                _ => format!("debug rerun for {id} produced 0 results"),
            }),
            None,
        ));
    }
    if tests.len() != 1 {
        let output = read_rerun_log(&rerun_log_path)?;
        bail!("{}", match output {
            Some(output) if !output.trim().is_empty() => {
                format!("debug rerun for {id} produced {} results\n\n{output}", tests.len())
            }
            _ => format!("debug rerun for {id} produced {} results", tests.len()),
        });
    }
    let status = match tests.into_iter().next().unwrap().status {
        Status::Pass => Status::Pass,
        Status::Fail => Status::Fail,
        Status::Skip => Status::Skip,
        Status::Timeout => Status::Timeout,
        Status::Flaky => {
            let output = read_rerun_log(&rerun_log_path)?;
            return Ok((
                Status::Fail,
                Some(match output {
                    Some(output) if !output.trim().is_empty() => {
                        format!("debug rerun for {id} returned FLAKY\n\n{output}")
                    }
                    _ => format!("debug rerun for {id} returned FLAKY"),
                }),
                None,
            ));
        }
    };
    Ok((status, read_rerun_log(&rerun_log_path)?, None))
}

fn rerun_log_path(workspace: &Workspace, id: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    workspace
        .work_dir
        .join("reruns")
        .join(format!("{:016x}.log", hasher.finish()))
}

fn read_rerun_log(path: &PathBuf) -> Result<Option<String>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let output = String::from_utf8_lossy(&bytes).into_owned();
    Ok((!output.trim().is_empty()).then_some(output))
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
    use crate::git::ensure_checkout;
    use crate::langs::tests::MockRunner;
    use crate::reports::{test_results_filename, test_summary_filename};
    use crate::runtime::RuntimeSource;
    use tempdir::TempDir;

    #[test]
    fn mock_runner_reports_mixed_statuses() {
        let cwd = std::env::current_dir().expect("cwd");
        let dir = TempDir::new("shield-run").expect("tempdir");
        let workspace = Workspace {
            output_dir: dir.path().to_path_buf(),
            checkout: cwd,
            work_dir: dir.path().to_path_buf(),
        };
        let wasmer = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary("wasmer".into()),
            dir.path(),
            Duration::ZERO,
            Arc::new(RunLog::new(dir.path().join("process.log"))),
        )
        .expect("resolve");
        let report = execute_tests(
            &MockRunner,
            &workspace,
            &wasmer.runtime,
            None,
            None,
            Mode::Capture,
        )
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

    #[test]
    fn mock_runner_panic_is_written_to_metadata() {
        let cwd = std::env::current_dir().expect("cwd");
        let dir = TempDir::new("shield-run").expect("tempdir");
        let workspace = Workspace {
            output_dir: dir.path().to_path_buf(),
            checkout: cwd,
            work_dir: dir.path().to_path_buf(),
        };
        let wasmer = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary("wasmer".into()),
            dir.path(),
            Duration::ZERO,
            Arc::new(RunLog::new(dir.path().join("process.log"))),
        )
        .expect("resolve");
        let report = execute_tests(
            &MockRunner,
            &workspace,
            &wasmer.runtime,
            None,
            Some("panic"),
            Mode::Capture,
        )
        .expect("execute_tests should succeed");

        assert_eq!(
            report.results,
            vec![
                TestResult {
                    id: "pass_a".into(),
                    status: Status::Pass,
                },
                TestResult {
                    id: "panic_g".into(),
                    status: Status::Fail,
                }
            ]
        );
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.contains("stack overflow"));

        finalize_run(
            &workspace,
            &wasmer.identity,
            results_by_id(&report.results),
            &report.errors,
            RunConfig {
                timeout: Duration::from_secs(30),
                runner_name: MockRunner::OPTS.name,
                runner_commit: MockRunner::OPTS.git_ref,
                started_at: "1970-01-01T00:00:00Z",
                flaky_count: 0,
            },
        )
        .expect("finalize");

        let status: BTreeMap<String, Status> = serde_json::from_slice(
            &fs::read(dir.path().join(test_results_filename("mock"))).expect("status"),
        )
        .expect("parse status");
        assert_eq!(status["pass_a"], Status::Pass);
        assert_eq!(status["panic_g"], Status::Fail);

        let metadata: serde_json::Value = serde_json::from_slice(
            &fs::read(dir.path().join(test_summary_filename("mock"))).expect("metadata"),
        )
        .expect("parse metadata");
        let error = metadata["crashes"]["panic_g"].as_str().expect("job crash");
        assert!(error.contains("crash: fatal runtime error: stack overflow, aborting"));
        assert!(error.contains("- panic_g"));
    }

    #[test]
    fn stabilization_only_retries_pass_boundary_changes() {
        assert!(should_stabilize_status_change(Status::Pass, Status::Fail));
        assert!(should_stabilize_status_change(
            Status::Pass,
            Status::Timeout
        ));
        assert!(should_stabilize_status_change(Status::Fail, Status::Pass));
        assert!(should_stabilize_status_change(
            Status::Timeout,
            Status::Pass
        ));

        assert!(!should_stabilize_status_change(
            Status::Fail,
            Status::Timeout
        ));
        assert!(!should_stabilize_status_change(
            Status::Timeout,
            Status::Fail
        ));
        assert!(!should_stabilize_status_change(Status::Skip, Status::Fail));
        assert!(!should_stabilize_status_change(Status::Pass, Status::Pass));
    }

    #[test]
    fn stabilization_treats_rerun_errors_as_failed_tests() {
        let cwd = std::env::current_dir().expect("cwd");
        let dir = TempDir::new("shield-stabilize-error").expect("tempdir");
        let workspace = Workspace {
            output_dir: dir.path().to_path_buf(),
            checkout: cwd,
            work_dir: dir.path().to_path_buf(),
        };
        let wasmer = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary("wasmer".into()),
            dir.path(),
            Duration::ZERO,
            Arc::new(RunLog::new(dir.path().join("process.log"))),
        )
        .expect("resolve");
        let baseline = BTreeMap::from([("panic_g".to_string(), Status::Pass)]);
        let candidate = BTreeMap::from([("panic_g".to_string(), Status::Fail)]);

        let (status, flaky_count, regressions, errors) = stabilize_changed_tests(
            &MockRunner,
            &workspace,
            &wasmer.runtime,
            &baseline,
            candidate,
        )
        .expect("stabilize");

        assert_eq!(status["panic_g"], Status::Fail);
        assert_eq!(flaky_count, 0);
        assert_eq!(regressions.regressions.len(), 1);
        assert_eq!(regressions.regressions[0].id, "panic_g");
        assert_eq!(regressions.regressions[0].status_before, Status::Pass);
        assert_eq!(regressions.regressions[0].status_after, Status::Fail);
        assert!(
            regressions.regressions[0]
                .output
                .contains("fatal runtime error: stack overflow")
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].id, "panic_g");
        assert!(errors[0].message.contains("fatal runtime error: stack overflow"));
    }

    #[test]
    fn stabilization_treats_flaky_reruns_as_failed_tests() {
        let cwd = std::env::current_dir().expect("cwd");
        let dir = TempDir::new("shield-stabilize-flaky").expect("tempdir");
        let workspace = Workspace {
            output_dir: dir.path().to_path_buf(),
            checkout: cwd,
            work_dir: dir.path().to_path_buf(),
        };
        let wasmer = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary("wasmer".into()),
            dir.path(),
            Duration::ZERO,
            Arc::new(RunLog::new(dir.path().join("process.log"))),
        )
        .expect("resolve");
        let baseline = BTreeMap::from([("flaky_f".to_string(), Status::Pass)]);
        let candidate = BTreeMap::from([("flaky_f".to_string(), Status::Fail)]);

        let (status, flaky_count, regressions, errors) = stabilize_changed_tests(
            &MockRunner,
            &workspace,
            &wasmer.runtime,
            &baseline,
            candidate,
        )
        .expect("stabilize");

        assert_eq!(status["flaky_f"], Status::Fail);
        assert_eq!(flaky_count, 0);
        assert_eq!(regressions.regressions.len(), 1);
        assert!(regressions.regressions[0].output.contains("returned FLAKY"));
        assert!(errors.is_empty());
    }

    #[test]
    fn reads_log_output() {
        let dir = TempDir::new("shield-log-tail").expect("tempdir");
        let path = dir.path().join("test_mock.log");
        fs::write(&path, "[stderr] detail line 1\n[stdout] detail line 2\n").expect("write log");

        let output = read_rerun_log(&path)
            .expect("read log output")
            .expect("captured output");
        assert_eq!(output, "[stderr] detail line 1\n[stdout] detail line 2\n");
    }

    #[test]
    fn run_python_debug() {
        let report = run_with_runner(
            RunArgs {
                lang: Lang::Python,
                version: false,
                filter: Some(
                    "test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later".into(),
                ),
                wasmer: Some("/Users/fessguid/wasmer/wasmer2/target/debug/wasmer".into()),
                wasmer_repo: None,
                wasmer_ref: None,
                timeout: Duration::from_secs(30),
                compare_ref: "origin/main".into(),
            },
            "1970-01-01T00:00:00Z",
            &PythonRunner::new(),
        )
        .expect("run");
        assert_eq!(
            report,
            ExecutionReport {
                results: vec![TestResult {
                    id: "test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later"
                        .into(),
                    status: Status::Pass,
                }],
                counts: StatusCounts(HashMap::from([(Status::Pass, 1)])),
                errors: vec![],
            }
        );
    }

    #[test]
    fn run_php_debug() {
        let report = run_with_runner(
            RunArgs {
                lang: Lang::Php,
                version: false,
                filter: Some("tests/basic/001.phpt".into()),
                wasmer: Some("/Users/fessguid/wasmer/wasmer2/target/debug/wasmer".into()),
                wasmer_repo: None,
                wasmer_ref: None,
                timeout: Duration::from_secs(30),
                compare_ref: "origin/main".into(),
            },
            "1970-01-01T00:00:00Z",
            &PhpRunner,
        )
        .expect("run");
        assert_eq!(
            report,
            ExecutionReport {
                results: vec![TestResult {
                    id: "tests/basic/001.phpt".into(),
                    status: Status::Pass,
                }],
                counts: StatusCounts(HashMap::from([(Status::Pass, 1)])),
                errors: vec![],
            }
        );
    }

    #[test]
    fn run_node_debug() {
        let report = run_with_runner(
            RunArgs {
                lang: Lang::Node,
                version: false,
                filter: Some("parallel/test-global.js".into()),
                wasmer: Some("/Users/fessguid/wasmer/wasmer2/target/debug/wasmer".into()),
                wasmer_repo: None,
                wasmer_ref: None,
                timeout: Duration::from_secs(30),
                compare_ref: "origin/main".into(),
            },
            "1970-01-01T00:00:00Z",
            &NodeRunner,
        )
        .expect("run");
        assert_eq!(
            report,
            ExecutionReport {
                results: vec![TestResult {
                    id: "parallel/test-global.js".into(),
                    status: Status::Pass,
                }],
                counts: StatusCounts(HashMap::from([(Status::Pass, 1)])),
                errors: vec![],
            }
        );
    }

    #[test]
    fn run_rust_debug() {
        let report = run_with_runner(
            RunArgs {
                lang: Lang::Rust,
                version: false,
                filter: Some(
                    "library::alloctests::alloctests-47068aef54e24049::vec::test_append".into(),
                ),
                wasmer: Some("/Users/fessguid/wasmer/wasmer2/target/debug/wasmer".into()),
                wasmer_repo: None,
                wasmer_ref: None,
                timeout: Duration::from_secs(30000),
                compare_ref: "origin/main".into(),
            },
            "1970-01-01T00:00:00Z",
            &RustRunner,
        )
        .expect("run");
        assert_eq!(
            report,
            ExecutionReport {
                results: vec![TestResult {
                    id: "library::alloctests::alloctests-47068aef54e24049::vec::test_append".into(),
                    status: Status::Pass,
                }],
                counts: StatusCounts(HashMap::from([(Status::Pass, 1)])),
                errors: vec![],
            }
        );
    }

    #[test]
    fn runner_warmups() {
        let work_root = std::env::current_dir().expect("cwd").join(".work");
        let wasmer = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary("/Users/fessguid/wasmer/wasmer2/target/debug/wasmer".into()),
            &work_root,
            Duration::from_secs(30),
            Arc::new(RunLog::new(work_root.join("runner_warmups.log"))),
        )
        .expect("resolve")
        .runtime;
        let python = PythonRunner::new();
        let node = NodeRunner;
        let php = PhpRunner;
        let rust = RustRunner;
        for runner in [
            &python as &dyn LangRunner,
            &node as &dyn LangRunner,
            &php as &dyn LangRunner,
            &rust as &dyn LangRunner,
        ] {
            warmup_package(runner, &wasmer).unwrap_or_else(|_| panic!("{}", runner.opts().name));
        }
    }

    #[test]
    #[ignore = "local setup helper; run with WASMER_BINARY=/path/to/wasmer cargo test test_dependencies --ignored"]
    fn test_dependencies() {
        let output_dir = std::env::current_dir().expect("cwd");
        let work_root = output_dir.join(".work");
        let wasmer_binary = std::env::var("WASMER_BINARY")
            .expect("WASMER_BINARY must be set, ex: WASMER_BINARY=~/my/wasmer cargo test test_dependencies --ignored");
        let wasmer = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary(wasmer_binary.into()),
            &work_root,
            Duration::from_secs(1800),
            Arc::new(RunLog::new(output_dir.join("test_dependencies.log"))),
        )
        .expect("resolve")
        .runtime;

        let python = PythonRunner::new();
        let node = NodeRunner;
        let php = PhpRunner;
        let rust = RustRunner;
        for runner in [
            &python as &dyn LangRunner,
            &node as &dyn LangRunner,
            &php as &dyn LangRunner,
            &rust as &dyn LangRunner,
        ] {
            let opts = runner.opts();
            let work_dir = work_root.join(opts.name);
            let checkout =
                ensure_checkout(&work_dir, opts.git_repo, opts.git_ref).expect("checkout");
            let workspace = Workspace {
                output_dir: output_dir.clone(),
                checkout,
                work_dir,
            };
            warmup_package(runner, &wasmer).expect("warmup");
            if opts.name == RustRunner::OPTS.name {
                let jobs = runner
                    .discover(&workspace, &wasmer, None, Mode::Capture)
                    .expect("rust discovery");
                let cache_path = output_dir.join(".cache").join(opts.name).join("tests.json");
                fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache dir");
                fs::write(
                    cache_path,
                    serde_json::to_vec_pretty(&jobs).expect("serialize rust tests"),
                )
                .expect("write rust tests cache");
            }
        }
    }
}
