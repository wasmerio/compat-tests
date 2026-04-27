pub mod node;
pub mod php;
pub mod python;
pub mod rust;

use std::fmt;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::run_log::RunLog;
use crate::runtime::WasmerRuntime;

pub struct RunnerOpts {
    /// Runner name, ex: python
    pub name: &'static str,
    /// Upstream git repo. ex: https://github.com/python/cpython.git
    pub git_repo: &'static str,
    /// Upstream git ref, ex: main
    pub git_ref: &'static str,
    /// Wasmer package name, ex: python/python
    pub wasmer_package: Option<&'static str>,
    /// Wasmer package warmup args, ex: -c print('ok')
    pub wasmer_package_warmup_args: Option<&'static [&'static str]>,
    /// Wasmer flags, ex: --experimental-napi
    pub wasmer_flags: &'static [&'static str],
    /// Optional docker compose file, ex: docker-compose.yml
    #[allow(unused)] // TODO: Add docker with MySQL for PHP to test DB as well
    pub docker_compose: Option<&'static str>,
}

pub struct Workspace {
    pub output_dir: PathBuf,
    pub checkout: PathBuf,
    pub work_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestJob {
    pub id: String,
    pub tests: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Pass,
    Fail,
    Skip,
    Timeout,
    Flaky,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
            Status::Timeout => "TIMEOUT",
            Status::Flaky => "FLAKY",
        };
        f.write_str(name)
    }
}

#[derive(Debug, PartialEq)]
pub struct TestResult {
    pub id: String,
    pub status: Status,
}

#[derive(Debug, PartialEq)]
pub struct TestIssue {
    pub id: String,
    pub message: String,
}

#[derive(Debug, PartialEq)]
pub struct TestRunOutput {
    pub results: Vec<TestResult>,
    pub issues: Vec<TestIssue>,
}

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Capture,
    Debug,
}

pub trait LangRunner: Send + Sync {
    fn opts(&self) -> &'static RunnerOpts;
    fn prepare(
        &self,
        _workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        _jobs: &[TestJob],
    ) -> Result<()> {
        Ok(())
    }
    fn discover(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        filter: Option<&str>,
        mode: Mode,
    ) -> Result<Vec<TestJob>>;
    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        mode: Mode,
        log: Option<&RunLog>,
    ) -> Result<TestRunOutput>;

    /// Multiplies the default capture parallelism for IO-heavy runners that can
    /// benefit from keeping more test jobs in flight than there are CPU cores.
    fn thread_count_multiplier(&self) -> usize {
        1
    }
}

#[cfg(test)]
pub mod tests {
    use anyhow::Result;

    use super::{
        LangRunner, Mode, RunnerOpts, Status, TestIssue, TestJob, TestResult, TestRunOutput,
        Workspace,
    };
    use crate::process::ProcessError;
    use crate::run_log::RunLog;
    use crate::runtime::WasmerRuntime;

    pub struct MockRunner;

    impl MockRunner {
        pub const OPTS: RunnerOpts = RunnerOpts {
            name: "mock",
            git_repo: "https://example.invalid/mock.git",
            git_ref: "HEAD",
            wasmer_package: Some("mock/mock"),
            wasmer_package_warmup_args: Some(&["-c", "print('ok')"]),
            wasmer_flags: &[],
            docker_compose: None,
        };
    }

    impl LangRunner for MockRunner {
        fn opts(&self) -> &'static RunnerOpts {
            &Self::OPTS
        }

        fn discover(
            &self,
            _workspace: &Workspace,
            _wasmer: &WasmerRuntime,
            filter: Option<&str>,
            _mode: Mode,
        ) -> Result<Vec<TestJob>> {
            if filter == Some("panic") {
                return Ok(vec![
                    TestJob {
                        id: "pass_a".to_string(),
                        tests: vec!["pass_a".to_string()],
                    },
                    TestJob {
                        id: "panic_g".to_string(),
                        tests: vec!["panic_g".to_string()],
                    },
                ]);
            }
            let all = [
                "pass_a",
                "pass_b",
                "fail_c",
                "skip_d",
                "timeout_e",
                "flaky_f",
            ];
            Ok(all
                .iter()
                .filter(|id| filter.is_none_or(|f| id.contains(f)))
                .map(|id| TestJob {
                    id: (*id).to_string(),
                    tests: vec![(*id).to_string()],
                })
                .collect())
        }

        fn run_test(
            &self,
            _workspace: &Workspace,
            _wasmer: &WasmerRuntime,
            job: &TestJob,
            _mode: Mode,
            _log: Option<&RunLog>,
        ) -> Result<TestRunOutput> {
            let status = match job.id.split('_').next().unwrap_or("") {
                "fail" => Status::Fail,
                "skip" => Status::Skip,
                "timeout" => Status::Timeout,
                "flaky" => Status::Flaky,
                "panic" => {
                    return Err(anyhow::anyhow!(ProcessError::RustCrash(
                        "fatal runtime error: stack overflow, aborting".into()
                    )));
                }
                _ => Status::Pass,
            };
            Ok(TestRunOutput {
                results: vec![TestResult {
                    id: job.id.clone(),
                    status,
                }],
                issues: Vec::<TestIssue>::new(),
            })
        }
    }
}
