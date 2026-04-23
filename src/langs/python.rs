use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::{LangRunner, Mode, RunnerOpts, Status, TestResult, Workspace};
use crate::process::ProcessError;
use crate::run_log::RunLog;
use crate::runtime::{RunSpec, Stream, WasmerRuntime};

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
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PythonRunner {
    guest_test_dir: OnceLock<String>,
}

impl PythonRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "python",
        git_repo: "https://github.com/wasix-org/cpython.git",
        git_ref: "e3245fc95e570ac823deb50689041bc1f81d6b27",
        wasmer_package: "python/python",
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

    fn discover_cases(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
    ) -> Result<Vec<String>> {
        let mut stdout = String::new();
        let result = wasmer.run(
            RunSpec {
                package: Self::OPTS.wasmer_package.to_string(),
                flags: vec!["--volume".into(), self.volume_flag(workspace)?],
                args: vec!["-c".into(), DISCOVER_CASES.into(), id.into()],
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
            return Ok(vec![id.to_string()]);
        }
        let mut names: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("unittest.loader."))
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        match result {
            Ok(()) => {}
            Err(ProcessError::AbnormalExit(_)) | Err(ProcessError::RustPanic(_)) => {
                if names.is_empty() {
                    names.push(id.to_string());
                }
            }
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
            Err(ProcessError::Timeout(_)) => unreachable!(),
        }
        Ok(names)
    }

    pub fn run_module_capture(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        let mut parser = PythonProtocol::default();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let result = wasmer.run(
            RunSpec {
                package: Self::OPTS.wasmer_package.to_string(),
                flags: vec!["--volume".into(), self.volume_flag(workspace)?],
                args: vec!["-c".into(), DISCOVER_AND_RUN.into(), id.into()],
                timeout: None,
            },
            |stream, line| {
                match stream {
                    Stream::Stdout => {
                        parser.handle_line(line);
                        if log.is_some() {
                            stdout.push_str(line);
                            stdout.push('\n');
                        }
                    }
                    Stream::Stderr => {
                        if log.is_some() {
                            stderr.push_str(line);
                            stderr.push('\n');
                        }
                    }
                }
                Ok(())
            },
        );
        if let Some(log) = log {
            log.append(
                &format!(
                    "module {id}{}",
                    if matches!(result, Err(ProcessError::Timeout(_))) {
                        " TIMEOUT"
                    } else {
                        ""
                    }
                ),
                &stdout,
                &stderr,
            )?;
        }
        match &result {
            Ok(()) | Err(ProcessError::AbnormalExit(_)) | Err(ProcessError::RustPanic(_)) => {}
            Err(ProcessError::Timeout(_)) => {}
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message.clone())),
        }
        Ok(parser.finish(matches!(result, Err(ProcessError::Timeout(_))), id))
    }
}

impl Default for PythonRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl LangRunner for PythonRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
    }

    fn prepare(&self, workspace: &Workspace, wasmer: &WasmerRuntime) -> Result<()> {
        patch_faulthandler_workarounds(&Self::host_test_dir(workspace))
            .context("applying cpython test patches")?;
        let guest_dir = resolve_guest_test_dir(wasmer)?;
        self.guest_test_dir
            .set(guest_dir)
            .map_err(|existing| anyhow!("guest_test_dir already set to {existing:?}"))?;
        Ok(())
    }

    fn discover(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        filter: Option<&str>,
    ) -> Result<Vec<String>> {
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
                        return Ok(vec![f.to_string()]);
                    }
                }
                modules
                    .into_iter()
                    .filter(|m| m.contains(f) || f.contains(m.as_str()))
                    .collect()
            }
        };
        let mut jobs = Vec::new();
        for module in selected_modules {
            jobs.extend(self.discover_cases(workspace, wasmer, &module)?);
        }
        jobs.sort();
        jobs.dedup();
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
            Mode::Capture => self.run_module_capture(workspace, wasmer, id, log)?,
            Mode::Debug => {
                let result = wasmer.run(
                    RunSpec {
                        package: Self::OPTS.wasmer_package.to_string(),
                        flags: vec!["--volume".into(), self.volume_flag(workspace)?],
                        args: vec!["-m".into(), "unittest".into(), "-v".into(), id.into()],
                        timeout: None,
                    },
                    write_stream,
                );
                let status = match result {
                    Ok(()) => Status::Pass,
                    Err(ProcessError::Timeout(_)) => Status::Timeout,
                    Err(ProcessError::AbnormalExit(_)) | Err(ProcessError::RustPanic(_)) => {
                        Status::Fail
                    }
                    Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
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
    let mut stdout = String::new();
    let mut stderr = String::new();
    let result = wasmer.run(
        RunSpec {
            package: PythonRunner::OPTS.wasmer_package.to_string(),
            flags: vec![],
            args: vec!["-c".into(), GUEST_TEST_DIR_CODE.into()],
            timeout: Some(std::time::Duration::from_secs(10)),
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

fn parse_debug_status(output: &str, result: &std::result::Result<(), ProcessError>) -> Status {
    if matches!(result, Err(ProcessError::Timeout(_))) {
        return Status::Timeout;
    }
    if output.contains("... skipped ") {
        return Status::Skip;
    }
    if output.lines().any(|line| line.starts_with("OK")) && result.is_ok() {
        return Status::Pass;
    }
    if output.lines().any(|line| line.starts_with("FAILED (")) || result.is_err() {
        return Status::Fail;
    }
    Status::Timeout
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

fn write_stream(stream: Stream, line: &str) -> Result<()> {
    match stream {
        Stream::Stdout => {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{line}")?;
            out.flush()?;
        }
        Stream::Stderr => {
            let mut err = std::io::stderr().lock();
            writeln!(err, "{line}")?;
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
            parse_debug_status("test_x ... skipped 'nope'\n", &Ok(())),
            Status::Skip
        );
    }

    #[test]
    fn debug_status_detects_pass() {
        assert_eq!(parse_debug_status("...\nOK\n", &Ok(())), Status::Pass);
    }

    #[test]
    fn debug_status_detects_timeout() {
        assert_eq!(
            parse_debug_status("", &Err(ProcessError::Timeout("timeout".into()))),
            Status::Timeout
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
