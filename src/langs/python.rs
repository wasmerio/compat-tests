use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rayon::{ThreadPoolBuilder, prelude::*};

use super::{LangRunner, Mode, RunnerOpts, Status, TestResult, Workspace};
use crate::commands::run::{ExecutionReport, ItemError, StatusCounts};
use crate::run_log::RunLog;
use crate::wasmer::{RunSpec, Stream, WasmerRuntime};

const DISCOVER_AND_RUN: &str = r#"import os,sys,unittest
job = sys.argv[1]
def walk(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from walk(item)
        else:
            test_id = item.id()
            if not test_id.startswith("unittest.loader."):
                print("CASE", test_id, flush=True)
                yield test_id
try:
    suite = unittest.defaultTestLoader.loadTestsFromName(job)
except unittest.SkipTest:
    print("SKIP", job, flush=True)
    raise SystemExit(0)
cases = list(walk(suite))
class Result(unittest.TextTestResult):
    def _mark(self, status, test):
        test_id = test.id()
        if not test_id.startswith("unittest.loader."):
            print(status, test_id, flush=True)
    def addSuccess(self, test): super().addSuccess(test); self._mark("PASS", test)
    def addFailure(self, test, err): super().addFailure(test, err); self._mark("FAIL", test)
    def addError(self, test, err): super().addError(test, err); self._mark("FAIL", test)
    def addSkip(self, test, reason): super().addSkip(test, reason); self._mark("SKIP", test)
    def addExpectedFailure(self, test, err): super().addExpectedFailure(test, err); self._mark("FAIL", test)
    def addUnexpectedSuccess(self, test): super().addUnexpectedSuccess(test); self._mark("FAIL", test)
result = unittest.TextTestRunner(stream=sys.stderr, verbosity=2, resultclass=Result).run(suite)
raise SystemExit(0 if result.wasSuccessful() else 1)
"#;

const DISCOVER_CASES: &str = r#"import sys,unittest
job = sys.argv[1]
def walk(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from walk(item)
        else:
            test_id = item.id()
            if not test_id.startswith("unittest.loader."):
                print(test_id, flush=True)
try:
    suite = unittest.defaultTestLoader.loadTestsFromName(job)
except unittest.SkipTest:
    print(job, flush=True)
    raise SystemExit(0)
for _ in walk(suite):
    pass
"#;

const GUEST_TEST_DIR_CODE: &str = "import sys; print(f'/usr/local/lib/python{sys.version_info.major}.{sys.version_info.minor}/test')";
const RETEST_TIMEOUT: Duration = Duration::from_secs(300);
const RETEST_RUNS: usize = 3;

pub struct PythonRunner {
    guest_test_dir: OnceLock<String>,
}

pub struct CapturedModuleRun {
    pub results: Vec<TestResult>,
    pub timed_out: bool,
}

impl PythonRunner {
    pub fn new() -> Self {
        Self {
            guest_test_dir: OnceLock::new(),
        }
    }

    fn host_test_dir(workspace: &Workspace) -> PathBuf {
        workspace.checkout.join("Lib").join("test")
    }

    fn volume_flag(&self, workspace: &Workspace) -> Result<String> {
        let guest = self
            .guest_test_dir
            .get()
            .ok_or_else(|| anyhow!("guest test dir unresolved — prepare() must run first"))?;
        Ok(format!(
            "{}:{}",
            Self::host_test_dir(workspace).display(),
            guest
        ))
    }

    pub fn discover_cases(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        timeout: Duration,
    ) -> Result<Vec<String>> {
        let out = wasmer.run(
            RunSpec {
                package: Self::OPTS.wasmer_package.to_string(),
                flags: vec!["--volume".into(), self.volume_flag(workspace)?],
                args: vec!["-c".into(), DISCOVER_CASES.into(), id.into()],
                timeout: Some(timeout),
            },
            |_, _| Ok(()),
        )?;
        if out.timed_out {
            return Ok(vec![id.to_string()]);
        }
        let mut names: Vec<String> = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("unittest.loader."))
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        if names.is_empty() && out.exit_code != Some(0) {
            names.push(id.to_string());
        }
        Ok(names)
    }

    pub fn rerun_status(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        log: Option<&RunLog>,
        timeout: Duration,
    ) -> Result<String> {
        let out = wasmer.run(
            RunSpec {
                package: Self::OPTS.wasmer_package.to_string(),
                flags: vec!["--volume".into(), self.volume_flag(workspace)?],
                args: vec!["-m".into(), "unittest".into(), "-v".into(), id.into()],
                timeout: Some(timeout),
            },
            |_, _| Ok(()),
        )?;
        if let Some(log) = log {
            log.append(
                &format!("rerun {id}{}", if out.timed_out { " TIMEOUT" } else { "" }),
                &out.stdout,
                &out.stderr,
            )?;
        }
        Ok(parse_debug_status(
            &(out.stdout.clone() + &out.stderr),
            out.exit_code,
            out.timed_out,
        ))
    }

    pub fn run_suite_capture(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        log: Option<&RunLog>,
        timeout: Duration,
    ) -> Result<ExecutionReport> {
        self.prepare(workspace, wasmer)?;
        let ids = self.discover(workspace, None)?;
        if ids.is_empty() {
            bail!("runner discovered 0 tests");
        }

        let workers = worker_count(ids.len());
        println!(
            "Discovering leaf tests in {} modules with {workers} workers...",
            ids.len()
        );
        let discover_done = AtomicUsize::new(0);
        let pool = ThreadPoolBuilder::new().num_threads(workers).build()?;
        let discovered_outcomes: Vec<Result<(String, Vec<String>), ItemError>> =
            pool.install(|| {
                ids.par_iter()
                    .map(|id| {
                        let names = self
                            .discover_cases(workspace, wasmer, id, timeout)
                            .map_err(|e| ItemError {
                                id: id.clone(),
                                message: format!("{e:#}"),
                            })?;
                        let done = discover_done.fetch_add(1, Ordering::Relaxed) + 1;
                        if done % 25 == 0 || done == ids.len() {
                            println!("Discovered {done}/{} modules", ids.len());
                        }
                        Ok((id.clone(), names))
                    })
                    .collect()
            });

        let mut discovered = Vec::new();
        let mut discovery_errors = Vec::new();
        for outcome in discovered_outcomes {
            match outcome {
                Ok((id, names)) if !names.is_empty() => discovered.push((id, names)),
                Ok(_) => {}
                Err(error) => discovery_errors.push(error),
            }
        }
        if !discovery_errors.is_empty() {
            return Ok(ExecutionReport {
                results: vec![],
                counts: StatusCounts(HashMap::new()),
                errors: discovery_errors,
            });
        }

        let total_cases = discovered
            .iter()
            .map(|(_, names)| names.len())
            .sum::<usize>();
        println!(
            "Running {} module jobs covering {total_cases} tests with {workers} workers...",
            discovered.len()
        );
        let completed_cases = AtomicUsize::new(0);
        let mut results = Vec::new();
        let mut errors = Vec::new();
        let mut counts = StatusCounts(HashMap::new());
        let run_outcomes: Vec<Result<Vec<TestResult>, ItemError>> = pool.install(|| {
            discovered
                .par_iter()
                .map(|(id, expected)| {
                    self.run_module_capture(workspace, wasmer, id, log)
                        .map(|run| {
                            reconcile_module_results(id, expected, run.results, run.timed_out)
                        })
                        .map_err(|e| ItemError {
                            id: id.clone(),
                            message: format!("{e:#}"),
                        })
                })
                .collect()
        });
        for outcome in run_outcomes {
            match outcome {
                Ok(tests) => {
                    for result in tests {
                        let completed = completed_cases.fetch_add(1, Ordering::Relaxed) + 1;
                        println!(
                            "[{completed}/{total_cases}] {} {}",
                            result.id,
                            status_name(result.status)
                        );
                        counts.increment(result.status);
                        results.push(result);
                    }
                }
                Err(error) => errors.push(error),
            }
        }
        results.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(ExecutionReport {
            results,
            counts,
            errors,
        })
    }

    pub fn stabilize_changed_tests(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        log: Option<&RunLog>,
        baseline_status: &BTreeMap<String, String>,
        candidate_status: BTreeMap<String, String>,
    ) -> Result<(BTreeMap<String, String>, usize)> {
        let changed: Vec<String> = baseline_status
            .iter()
            .filter(|(test, old)| candidate_status.get(*test).is_some_and(|new| new != *old))
            .map(|(test, _)| test.clone())
            .collect();
        if changed.is_empty() {
            return Ok((candidate_status, 0));
        }

        let workers = worker_count(changed.len());
        println!(
            "Re-running {} changed tests with {workers} workers ({RETEST_RUNS} runs each, {}s timeout)...",
            changed.len(),
            RETEST_TIMEOUT.as_secs()
        );
        let rerun_done = AtomicUsize::new(0);
        let pool = ThreadPoolBuilder::new().num_threads(workers).build()?;
        let reruns: Vec<Result<(String, String, bool)>> = pool.install(|| {
            changed
                .par_iter()
                .map(|test_name| {
                    let result = classify_changed_test(
                        self,
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
                    );
                    let done = rerun_done.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 10 == 0 || done == changed.len() {
                        println!("Re-ran {done}/{} changed tests", changed.len());
                    }
                    result
                })
                .collect()
        });

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

    pub fn run_module_capture(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        log: Option<&RunLog>,
    ) -> Result<CapturedModuleRun> {
        let mut parser = PythonProtocol::default();
        let out = wasmer.run(
            RunSpec {
                package: Self::OPTS.wasmer_package.to_string(),
                flags: vec!["--volume".into(), self.volume_flag(workspace)?],
                args: vec!["-c".into(), DISCOVER_AND_RUN.into(), id.into()],
                timeout: None,
            },
            |stream, chunk| {
                if matches!(stream, Stream::Stdout) {
                    parser.feed(chunk);
                }
                Ok(())
            },
        )?;
        if let Some(log) = log {
            log.append(
                &format!("module {id}{}", if out.timed_out { " TIMEOUT" } else { "" }),
                &out.stdout,
                &out.stderr,
            )?;
        }
        Ok(CapturedModuleRun {
            results: parser.finish(out.timed_out, id),
            timed_out: out.timed_out,
        })
    }
}

impl Default for PythonRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl LangRunner for PythonRunner {
    const OPTS: RunnerOpts = RunnerOpts {
        name: "python",
        git_repo: "https://github.com/wasix-org/cpython.git",
        git_ref: "e3245fc95e570ac823deb50689041bc1f81d6b27",
        wasmer_package: "python/python",
        docker_compose: None,
    };

    fn prepare(&self, workspace: &Workspace, wasmer: &WasmerRuntime) -> Result<()> {
        patch_faulthandler_workarounds(&Self::host_test_dir(workspace))
            .context("applying cpython test patches")?;
        let guest_dir = resolve_guest_test_dir(wasmer)?;
        self.guest_test_dir
            .set(guest_dir)
            .map_err(|existing| anyhow!("guest_test_dir already set to {existing:?}"))?;
        Ok(())
    }

    fn discover(&self, workspace: &Workspace, filter: Option<&str>) -> Result<Vec<String>> {
        let testdir = Self::host_test_dir(workspace);
        let modules: Vec<String> = std::fs::read_dir(&testdir)
            .with_context(|| format!("reading {}", testdir.display()))?
            .filter_map(|r| {
                let path = r.ok()?.path();
                let stem = path.file_stem()?.to_str()?.to_owned();
                let is_py = path.extension().is_some_and(|e| e == "py");
                (stem.starts_with("test_") && (path.is_dir() || is_py))
                    .then(|| format!("test.{stem}"))
            })
            .collect();
        let mut jobs: Vec<String> = match filter {
            None => modules,
            Some(f) => {
                if let Some((prefix_end, _)) = f.match_indices('.').nth(1) {
                    let prefix = &f[..prefix_end];
                    if modules.iter().any(|m| m == prefix) {
                        return Ok(vec![f.to_string()]);
                    }
                }
                modules
                    .into_iter()
                    .filter(|m| m.contains(f) || f.contains(m.as_str()))
                    .collect()
            }
        };
        jobs.sort();
        Ok(jobs)
    }

    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        mode: Mode,
        log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        Ok(match mode {
            Mode::Capture => self.run_module_capture(workspace, wasmer, id, log)?.results,
            Mode::Debug => {
                let out = wasmer.run(
                    RunSpec {
                        package: Self::OPTS.wasmer_package.to_string(),
                        flags: vec!["--volume".into(), self.volume_flag(workspace)?],
                        args: vec!["-m".into(), "unittest".into(), "-v".into(), id.into()],
                        timeout: None,
                    },
                    write_stream,
                )?;
                let status = if out.timed_out {
                    Status::Timeout
                } else if out.exit_code == Some(0) {
                    Status::Pass
                } else {
                    Status::Fail
                };
                vec![TestResult {
                    id: id.to_string(),
                    status,
                }]
            }
        })
    }
}

#[cfg(test)]
fn parse_run_output(stdout: &str, timed_out: bool, module_id: &str) -> Vec<TestResult> {
    let mut parser = PythonProtocol::default();
    parser.feed(stdout.as_bytes());
    parser.finish(timed_out, module_id)
}

fn resolve_guest_test_dir(wasmer: &WasmerRuntime) -> Result<String> {
    let out = wasmer
        .run(
            RunSpec {
                package: PythonRunner::OPTS.wasmer_package.to_string(),
                flags: vec![],
                args: vec!["-c".into(), GUEST_TEST_DIR_CODE.into()],
                timeout: Some(std::time::Duration::from_secs(10)),
            },
            |_, _| Ok(()),
        )
        .context("resolving guest test dir via wasmer run")?;
    let dir = out.stdout.trim();
    if !dir.starts_with('/') {
        bail!(
            "guest test dir probe produced garbage (expected absolute path):\n\
             stdout: {:?}\nstderr: {}",
            out.stdout,
            out.stderr,
        );
    }
    Ok(dir.to_string())
}

fn parse_debug_status(output: &str, exit_code: Option<i32>, timed_out: bool) -> String {
    if timed_out {
        return "TIMEOUT".to_string();
    }
    if output.contains("... skipped ") {
        return "SKIP".to_string();
    }
    if output.lines().any(|line| line.starts_with("OK")) && exit_code == Some(0) {
        return "PASS".to_string();
    }
    if output.lines().any(|line| line.starts_with("FAILED (")) || exit_code != Some(0) {
        return "FAIL".to_string();
    }
    "TIMEOUT".to_string()
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

fn reconcile_module_results(
    module_id: &str,
    expected: &[String],
    results: Vec<TestResult>,
    timed_out: bool,
) -> Vec<TestResult> {
    let mut by_id = BTreeMap::new();
    for result in results {
        if result.id != module_id || expected.iter().any(|name| name == &result.id) {
            by_id.insert(result.id, result.status);
        }
    }
    let fallback = if timed_out {
        Status::Timeout
    } else {
        Status::Fail
    };
    for name in expected {
        by_id.entry(name.clone()).or_insert(fallback);
    }
    by_id
        .into_iter()
        .map(|(id, status)| TestResult { id, status })
        .collect()
}

fn classify_changed_test(
    runner: &PythonRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    log: Option<&RunLog>,
    test_name: &str,
    old_status: &str,
    new_status: &str,
) -> Result<(String, String, bool)> {
    let rerun_once = || runner.rerun_status(workspace, wasmer, test_name, log, RETEST_TIMEOUT);

    if new_status != "PASS" {
        let outcome = rerun_once()?;
        if outcome == new_status {
            Ok((test_name.to_string(), new_status.to_string(), false))
        } else {
            Ok((test_name.to_string(), old_status.to_string(), true))
        }
    } else {
        for _ in 0..RETEST_RUNS {
            if rerun_once()? != "PASS" {
                return Ok((test_name.to_string(), old_status.to_string(), true));
            }
        }
        Ok((test_name.to_string(), "PASS".to_string(), false))
    }
}

fn worker_count(limit: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .saturating_add(2)
        .min(limit.max(1))
}

fn patch_faulthandler_workarounds(testdir: &Path) -> Result<()> {
    type Edits = &'static [(&'static str, &'static str)];
    let replacements: &[(&str, Edits)] = &[
        (
            "support/script_helper.py",
            &[
                (
                    "cmd_line = [sys.executable, '-X', 'faulthandler']",
                    "cmd_line = [sys.executable]",
                ),
                (
                    r#"args = [sys.executable, "-E", "-X", "faulthandler", "-u", script, "-v"]"#,
                    r#"args = [sys.executable, "-E", "-u", script, "-v"]"#,
                ),
            ],
        ),
        (
            "test_regrtest.py",
            &[(
                "args = [sys.executable, *extraargs, '-X', 'faulthandler', '-I', *args]",
                "args = [sys.executable, *extraargs, '-I', *args]",
            )],
        ),
        (
            "bisect_cmd.py",
            &[("    cmd.extend(('-X', 'faulthandler'))\n", "")],
        ),
        (
            "test_faulthandler.py",
            &[(
                "import faulthandler\n",
                "import unittest\nraise unittest.SkipTest('blocked by wasix-libc sigaltstack() bug')\n",
            )],
        ),
        (
            "test_xxtestfuzz.py",
            &[(
                "import faulthandler\n",
                "import unittest\nraise unittest.SkipTest('blocked by wasix-libc sigaltstack() bug')\n",
            )],
        ),
        (
            "libregrtest/setup.py",
            &[
                (
                    "        faulthandler.enable(all_threads=True, file=stderr_fd)\n",
                    "",
                ),
                (
                    "        for signum in signals:\n            faulthandler.register(signum, chain=True, file=stderr_fd)\n",
                    "        for signum in signals:\n            pass\n",
                ),
                (
                    "        for signum in signals:\n\n",
                    "        for signum in signals:\n            pass\n",
                ),
            ],
        ),
        (
            "test_interpreters/utils.py",
            &[
                ("import contextlib\n", "import contextlib\nimport errno\n"),
                (
                    "        if exc.errno != 9:\n",
                    "        if exc.errno != errno.EBADF:\n",
                ),
            ],
        ),
    ];

    for (rel_path, edits) in replacements {
        let path = testdir.join(rel_path);
        let mut text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut changed = false;
        for (old, new) in *edits {
            if let Some(pos) = text.find(old) {
                text.replace_range(pos..pos + old.len(), new);
                changed = true;
            }
        }
        if changed {
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct PythonProtocol {
    pending: String,
    cases: Vec<String>,
    statuses: HashMap<String, Status>,
}

impl PythonProtocol {
    fn feed(&mut self, chunk: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        while let Some(idx) = self.pending.find('\n') {
            let line = self.pending[..idx].trim_end_matches('\r').to_string();
            self.pending.drain(..=idx);
            self.handle_line(&line);
        }
    }

    fn finish(mut self, timed_out: bool, module_id: &str) -> Vec<TestResult> {
        let tail = self.pending.trim_end_matches(['\r', '\n']).to_string();
        if !tail.is_empty() {
            self.handle_line(&tail);
        }

        let fallback = if timed_out {
            Status::Timeout
        } else {
            Status::Fail
        };

        if self.cases.is_empty() {
            if let Some((id, status)) = self.statuses.into_iter().next() {
                return vec![TestResult { id, status }];
            }
            return vec![TestResult {
                id: module_id.to_string(),
                status: fallback,
            }];
        }

        self.cases
            .into_iter()
            .map(|id| TestResult {
                status: self.statuses.remove(&id).unwrap_or(fallback),
                id,
            })
            .collect()
    }

    fn handle_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("CASE ") {
            self.cases.push(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("PASS ") {
            self.statuses.insert(rest.trim().to_string(), Status::Pass);
        } else if let Some(rest) = line.strip_prefix("FAIL ") {
            self.statuses.insert(rest.trim().to_string(), Status::Fail);
        } else if let Some(rest) = line.strip_prefix("SKIP ") {
            self.statuses.insert(rest.trim().to_string(), Status::Skip);
        }
    }
}

fn write_stream(stream: Stream, chunk: &[u8]) -> Result<()> {
    match stream {
        Stream::Stdout => {
            let mut out = std::io::stdout().lock();
            out.write_all(chunk)?;
            out.flush()?;
        }
        Stream::Stderr => {
            let mut err = std::io::stderr().lock();
            err.write_all(chunk)?;
            err.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_pass() {
        let stdout = "\
CASE test.test_foo.TestFoo.test_a
CASE test.test_foo.TestFoo.test_b
PASS test.test_foo.TestFoo.test_a
PASS test.test_foo.TestFoo.test_b
";
        let results = parse_run_output(stdout, false, "test.test_foo");
        assert_eq!(
            results,
            vec![
                TestResult {
                    id: "test.test_foo.TestFoo.test_a".into(),
                    status: Status::Pass
                },
                TestResult {
                    id: "test.test_foo.TestFoo.test_b".into(),
                    status: Status::Pass
                },
            ],
        );
    }

    #[test]
    fn mixed_statuses_preserve_case_order() {
        let stdout = "\
CASE mod.A
CASE mod.B
CASE mod.C
SKIP mod.B
PASS mod.A
FAIL mod.C
";
        let results = parse_run_output(stdout, false, "mod");
        assert_eq!(
            results,
            vec![
                TestResult {
                    id: "mod.A".into(),
                    status: Status::Pass
                },
                TestResult {
                    id: "mod.B".into(),
                    status: Status::Skip
                },
                TestResult {
                    id: "mod.C".into(),
                    status: Status::Fail
                },
            ],
        );
    }

    #[test]
    fn missing_cases_fill_with_fail_on_crash() {
        let stdout = "\
CASE mod.A
CASE mod.B
CASE mod.C
PASS mod.A
";
        let results = parse_run_output(stdout, false, "mod");
        assert_eq!(
            results,
            vec![
                TestResult {
                    id: "mod.A".into(),
                    status: Status::Pass
                },
                TestResult {
                    id: "mod.B".into(),
                    status: Status::Fail
                },
                TestResult {
                    id: "mod.C".into(),
                    status: Status::Fail
                },
            ],
        );
    }

    #[test]
    fn missing_cases_fill_with_timeout_when_timed_out() {
        let stdout = "\
CASE mod.A
CASE mod.B
PASS mod.A
";
        let results = parse_run_output(stdout, true, "mod");
        assert_eq!(
            results,
            vec![
                TestResult {
                    id: "mod.A".into(),
                    status: Status::Pass
                },
                TestResult {
                    id: "mod.B".into(),
                    status: Status::Timeout
                },
            ],
        );
    }

    #[test]
    fn module_skip_before_enumeration() {
        let stdout = "SKIP test.test_foo\n";
        let results = parse_run_output(stdout, false, "test.test_foo");
        assert_eq!(
            results,
            vec![TestResult {
                id: "test.test_foo".into(),
                status: Status::Skip
            }],
        );
    }

    #[test]
    fn no_output_crash_reports_module_level_fail() {
        let results = parse_run_output("", false, "test.test_foo");
        assert_eq!(
            results,
            vec![TestResult {
                id: "test.test_foo".into(),
                status: Status::Fail
            }],
        );
    }

    #[test]
    fn no_output_timeout_reports_module_level_timeout() {
        let results = parse_run_output("", true, "test.test_foo");
        assert_eq!(
            results,
            vec![TestResult {
                id: "test.test_foo".into(),
                status: Status::Timeout
            }],
        );
    }

    #[test]
    fn debug_status_detects_skip() {
        assert_eq!(
            parse_debug_status("test_x ... skipped 'nope'\n", Some(0), false),
            "SKIP"
        );
    }

    #[test]
    fn debug_status_detects_pass() {
        assert_eq!(parse_debug_status("...\nOK\n", Some(0), false), "PASS");
    }

    #[test]
    fn debug_status_detects_timeout() {
        assert_eq!(parse_debug_status("", None, true), "TIMEOUT");
    }

    #[test]
    fn reconcile_expands_module_fail_to_expected_cases() {
        let results = reconcile_module_results(
            "test.mod",
            &["test.mod.A".into(), "test.mod.B".into()],
            vec![TestResult {
                id: "test.mod".into(),
                status: Status::Fail,
            }],
            false,
        );

        assert_eq!(
            results,
            vec![
                TestResult {
                    id: "test.mod.A".into(),
                    status: Status::Fail,
                },
                TestResult {
                    id: "test.mod.B".into(),
                    status: Status::Fail,
                },
            ]
        );
    }

    #[test]
    fn reconcile_fills_missing_expected_cases_on_timeout() {
        let results = reconcile_module_results(
            "test.mod",
            &["test.mod.A".into(), "test.mod.B".into()],
            vec![TestResult {
                id: "test.mod.A".into(),
                status: Status::Pass,
            }],
            true,
        );

        assert_eq!(
            results,
            vec![
                TestResult {
                    id: "test.mod.A".into(),
                    status: Status::Pass,
                },
                TestResult {
                    id: "test.mod.B".into(),
                    status: Status::Timeout,
                },
            ]
        );
    }
}
