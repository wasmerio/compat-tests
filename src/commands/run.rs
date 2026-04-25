use std::collections::{BTreeMap, HashMap};
use std::fs;
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
use crate::reports::{RunConfig, finalize_run, load_baseline_status};
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
        status,
        &report.errors,
        RunConfig {
            timeout: args.timeout,
            filter: args.filter.as_deref(),
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
                    message: job_error_message(runner, wasmer, job, &e),
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

fn job_error_message(
    runner: &dyn LangRunner,
    wasmer: &WasmerRuntime,
    job: &TestJob,
    error: &anyhow::Error,
) -> String {
    let mut message = format!(
        "{error:#}\njob: {}\nrepro: cargo run -- run --lang {} --wasmer {} {}\ntests:",
        job.id,
        runner.opts().name,
        wasmer.binary_path().display(),
        job.id,
    );
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
    log: Option<&RunLog>,
) -> Result<Status> {
    let tests = runner.run_test(
        workspace,
        wasmer,
        &TestJob {
            id: id.to_string(),
            tests: vec![id.to_string()],
        },
        Mode::Debug,
        log,
    )?;
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
    use crate::git::ensure_checkout;
    use crate::langs::tests::MockRunner;
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
                filter: None,
                runner_name: MockRunner::OPTS.name,
                runner_commit: MockRunner::OPTS.git_ref,
                started_at: "1970-01-01T00:00:00Z",
                flaky_count: 0,
            },
        )
        .expect("finalize");

        let status: BTreeMap<String, Status> =
            serde_json::from_slice(&fs::read(dir.path().join("status_mock.json")).expect("status"))
                .expect("parse status");
        assert_eq!(status["pass_a"], Status::Pass);
        assert_eq!(status["panic_g"], Status::Fail);

        let metadata: serde_json::Value = serde_json::from_slice(
            &fs::read(dir.path().join("metadata_mock.json")).expect("metadata"),
        )
        .expect("parse metadata");
        let error = metadata["crashes"]["panic_g"].as_str().expect("job crash");
        assert!(error.contains("crash: fatal runtime error: stack overflow, aborting"));
        assert!(error.contains("repro: cargo run -- run --lang mock"));
        assert!(error.contains("- panic_g"));
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
