use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;

use super::{LangRunner, Mode, RunnerOpts, Status, TestJob, TestResult, Workspace};
use crate::process::{ProcessError, write_stream};
use crate::run_log::RunLog;
use crate::runtime::{RunSpec, RunTarget, Stream, WasmerRuntime};

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
def walk(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from walk(item)
        else:
            test_id = item.id()
            if not test_id.startswith("unittest.loader."):
                print(job, test_id, sep="\t", flush=True)
for job in sys.argv[1:]:
    try:
        suite = unittest.defaultTestLoader.loadTestsFromName(job)
    except unittest.SkipTest:
        print(job, flush=True)
        continue
    for _ in walk(suite):
        pass
"#;

const GUEST_TEST_DIR_CODE: &str = "import sys; print(f'/usr/local/lib/python{sys.version_info.major}.{sys.version_info.minor}/test')";
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PythonRunner {
    guest_test_dir: OnceLock<String>,
}

impl PythonRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "python",
        git_repo: "https://github.com/wasix-org/cpython.git",
        // TODO: I guess we could infer git_ref from the package itself
        git_ref: "e3245fc95e570ac823deb50689041bc1f81d6b27",
        wasmer_package: Some("python/python"),
        wasmer_package_warmup_args: Some(&["-c", "print('ok')"]),
        wasmer_flags: &[],
        docker_compose: None,
    };

    pub fn new() -> Self {
        Self {
            guest_test_dir: OnceLock::new(),
        }
    }

    fn host_test_dir(workspace: &Workspace) -> PathBuf {
        workspace.checkout.join("Lib").join("test")
    }

    fn guest_test_dir(&self, wasmer: &WasmerRuntime) -> Result<&str> {
        if self.guest_test_dir.get().is_none() {
            let guest_dir = resolve_guest_test_dir(wasmer)?;
            let _ = self.guest_test_dir.set(guest_dir);
        }
        Ok(self
            .guest_test_dir
            .get()
            .expect("guest_test_dir should be initialized")
            .as_str())
    }

    fn volume_flag(&self, workspace: &Workspace, wasmer: &WasmerRuntime) -> Result<String> {
        let guest = self.guest_test_dir(wasmer)?;
        Ok(format!(
            "{}:{}",
            Self::host_test_dir(workspace).display(),
            guest
        ))
    }

    fn discover_cases(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        ids: &[String],
    ) -> Result<Vec<TestJob>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut stdout = String::new();
        let mut args = vec!["-c".into(), DISCOVER_CASES.into()];
        args.extend(ids.iter().cloned());
        let result = wasmer.run(
            RunSpec {
                target: RunTarget::Package(
                    Self::OPTS
                        .wasmer_package
                        .expect("python package")
                        .to_string(),
                ),
                flags: vec!["--volume".into(), self.volume_flag(workspace, wasmer)?],
                args,
                timeout: Some(DISCOVER_TIMEOUT),
            },
            |stream, line| {
                if matches!(stream, Stream::Stdout) {
                    stdout.push_str(line);
                    stdout.push('\n');
                }
                Ok(())
            },
        );
        if matches!(result, Err(ProcessError::Timeout(_))) {
            return Ok(ids
                .iter()
                .cloned()
                .map(|id| TestJob {
                    tests: vec![id.clone()],
                    id,
                })
                .collect());
        }
        let mut names = BTreeMap::<String, Vec<String>>::new();
        for line in stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if line.starts_with("unittest.loader.") {
                continue;
            }
            if let Some((module, test_id)) = line.split_once('\t') {
                names
                    .entry(module.to_string())
                    .or_default()
                    .push(test_id.to_string());
            } else {
                names
                    .entry(line.to_string())
                    .or_default()
                    .push(line.to_string());
            }
        }
        match result {
            Ok(()) => {}
            Err(ProcessError::AbnormalExit) => {
                if names.is_empty() {
                    for id in ids {
                        names.entry(id.clone()).or_default().push(id.clone());
                    }
                }
            }
            Err(ProcessError::RustPanic(message)) => {
                return Err(anyhow!(ProcessError::RustPanic(message)));
            }
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
            Err(ProcessError::Timeout(_)) => unreachable!(),
        }
        Ok(ids
            .iter()
            .map(|id| {
                let mut tests = names.remove(id).unwrap_or_else(|| vec![id.clone()]);
                tests.sort();
                tests.dedup();
                TestJob {
                    id: id.clone(),
                    tests,
                }
            })
            .collect())
    }

    pub fn run_module_capture(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        _log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        let mut parser = PythonProtocol::default();
        let result = wasmer.run(
            RunSpec {
                target: RunTarget::Package(
                    Self::OPTS
                        .wasmer_package
                        .expect("python package")
                        .to_string(),
                ),
                flags: vec!["--volume".into(), self.volume_flag(workspace, wasmer)?],
                args: vec!["-c".into(), DISCOVER_AND_RUN.into(), job.id.clone()],
                timeout: None,
            },
            |stream, line| {
                if matches!(stream, Stream::Stdout) {
                    parser.handle_line(line);
                }
                Ok(())
            },
        );
        match &result {
            Ok(()) => {}
            Err(ProcessError::AbnormalExit) if !parser.has_results() => {
                return Err(anyhow!(ProcessError::AbnormalExit));
            }
            Err(ProcessError::AbnormalExit) => {}
            Err(ProcessError::Timeout(_)) => {}
            Err(ProcessError::RustPanic(message)) => {
                return Err(anyhow!(ProcessError::RustPanic(message.clone())));
            }
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message.clone())),
        }
        Ok(reconcile_module_results(
            &job.id,
            &job.tests,
            parser.finish(matches!(result, Err(ProcessError::Timeout(_))), &job.id),
            matches!(result, Err(ProcessError::Timeout(_))),
        ))
    }
}

impl Default for PythonRunner {
    fn default() -> Self {
        Self::new()
    }
}

fn worker_count(total: usize) -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    total.max(1).min(cpus + 2)
}

impl LangRunner for PythonRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
    }

    fn prepare(
        &self,
        workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        _jobs: &[TestJob],
    ) -> Result<()> {
        patch_faulthandler_workarounds(&Self::host_test_dir(workspace))
            .context("applying cpython test patches")
    }

    fn discover(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        filter: Option<&str>,
        mode: Mode,
    ) -> Result<Vec<TestJob>> {
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
        let selected_modules: Vec<String> = match filter {
            None => modules,
            Some(f) => {
                if let Some((prefix_end, _)) = f.match_indices('.').nth(1) {
                    let prefix = &f[..prefix_end];
                    if modules.iter().any(|m| m == prefix) {
                        return Ok(vec![TestJob {
                            id: f.to_string(),
                            tests: vec![f.to_string()],
                        }]);
                    }
                }
                modules
                    .into_iter()
                    .filter(|m| m.contains(f) || f.contains(m.as_str()))
                    .collect()
            }
        };
        let workers = worker_count(selected_modules.len());
        tracing::info!(
            modules = selected_modules.len(),
            workers,
            mode = ?mode,
            "discovering python tests"
        );
        let completed = AtomicUsize::new(0);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .map_err(|e| anyhow!("build python discovery pool: {e}"))?;
        let discovered: Vec<Result<Vec<TestJob>>> = pool.install(|| {
            selected_modules
                .par_iter()
                .map(|module| {
                    let result =
                        self.discover_cases(workspace, wasmer, std::slice::from_ref(module));
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 25 == 0 || done == selected_modules.len() {
                        tracing::info!(
                            completed = done,
                            total = selected_modules.len(),
                            "python discovery progress"
                        );
                    }
                    result
                })
                .collect()
        });
        let mut jobs = Vec::new();
        for result in discovered {
            jobs.extend(result?);
        }
        jobs.sort_by(|a, b| a.id.cmp(&b.id));
        jobs.dedup_by(|a, b| a.id == b.id);
        if matches!(mode, Mode::Debug) {
            let filter = filter.expect("debug filter");
            let mut tests: Vec<TestJob> = jobs
                .into_iter()
                .flat_map(|job| {
                    job.tests.into_iter().filter_map(move |test| {
                        (test == filter || test.contains(filter) || filter.contains(test.as_str()))
                            .then(|| TestJob {
                                id: test.clone(),
                                tests: vec![test],
                            })
                    })
                })
                .collect();
            tests.sort_by(|a, b| a.id.cmp(&b.id));
            tests.dedup_by(|a, b| a.id == b.id);
            if tests.is_empty() {
                return Ok(vec![TestJob {
                    id: filter.to_string(),
                    tests: vec![filter.to_string()],
                }]);
            }
            tracing::info!(tests = tests.len(), "discovered python debug tests");
            return Ok(tests);
        }
        let total_tests: usize = jobs.iter().map(|job| job.tests.len()).sum();
        tracing::info!(
            modules = jobs.len(),
            tests = total_tests,
            workers,
            "discovered python module jobs"
        );
        Ok(jobs)
    }

    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        mode: Mode,
        log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        Ok(match mode {
            Mode::Capture => self.run_module_capture(workspace, wasmer, job, log)?,
            Mode::Debug => {
                let result = wasmer.run(
                    RunSpec {
                        target: RunTarget::Package(
                            Self::OPTS
                                .wasmer_package
                                .expect("python package")
                                .to_string(),
                        ),
                        flags: vec!["--volume".into(), self.volume_flag(workspace, wasmer)?],
                        args: vec!["-m".into(), "unittest".into(), "-v".into(), job.id.clone()],
                        timeout: None,
                    },
                    write_stream,
                );
                let status = match result {
                    Ok(()) => Status::Pass,
                    Err(ProcessError::Timeout(_)) => Status::Timeout,
                    Err(ProcessError::AbnormalExit) => Status::Fail,
                    Err(ProcessError::RustPanic(message)) => {
                        return Err(anyhow!(ProcessError::RustPanic(message)));
                    }
                    Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
                };
                vec![TestResult {
                    id: job.id.clone(),
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
    let mut stdout = String::new();
    let mut stderr = String::new();
    let result = wasmer.run(
        RunSpec {
            target: RunTarget::Package(
                PythonRunner::OPTS
                    .wasmer_package
                    .expect("python package")
                    .to_string(),
            ),
            flags: vec![],
            args: vec!["-c".into(), GUEST_TEST_DIR_CODE.into()],
            timeout: None,
        },
        |stream, line| {
            match stream {
                Stream::Stdout => {
                    stdout.push_str(line);
                    stdout.push('\n');
                }
                Stream::Stderr => {
                    stderr.push_str(line);
                    stderr.push('\n');
                }
            }
            Ok(())
        },
    );
    if let Err(err) = result {
        bail!("guest test dir probe failed: {err:?}\nstderr: {}", stderr);
    }
    let dir = stdout.trim();
    if !dir.starts_with('/') {
        bail!(
            "guest test dir probe produced garbage (expected absolute path):\n\
             stdout: {:?}\nstderr: {}",
            stdout,
            stderr,
        );
    }
    Ok(dir.to_string())
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
    fn has_results(&self) -> bool {
        !self.cases.is_empty() || !self.statuses.is_empty()
    }

    #[cfg(test)]
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
