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
    /// Wasmer flags, ex: --experimental-napi
    pub wasmer_flags: &'static [&'static str],
    /// Optional docker compose file, ex: docker-compose.yml
    pub docker_compose: Option<&'static str>,
}

pub struct Workspace {
    pub output_dir: PathBuf,
    pub checkout: PathBuf,
    pub work_dir: PathBuf,
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
        _ids: &[String],
    ) -> Result<()> {
        Ok(())
    }
    fn discover(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        filter: Option<&str>,
    ) -> Result<Vec<String>>;
    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        mode: Mode,
        log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>>;
}

#[cfg(test)]
pub mod tests {
    use anyhow::Result;

    use super::{LangRunner, Mode, RunnerOpts, Status, TestResult, Workspace};
    use crate::run_log::RunLog;
    use crate::runtime::WasmerRuntime;

    pub struct MockRunner;

    impl MockRunner {
        pub const OPTS: RunnerOpts = RunnerOpts {
            name: "mock",
            git_repo: "https://example.invalid/mock.git",
            git_ref: "HEAD",
            wasmer_package: Some("mock/mock"),
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
        ) -> Result<Vec<String>> {
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
                .map(|id| (*id).to_string())
                .collect())
        }

        fn run_test(
            &self,
            _workspace: &Workspace,
            _wasmer: &WasmerRuntime,
            id: &str,
            _mode: Mode,
            _log: Option<&RunLog>,
        ) -> Result<Vec<TestResult>> {
            let status = match id.split('_').next().unwrap_or("") {
                "fail" => Status::Fail,
                "skip" => Status::Skip,
                "timeout" => Status::Timeout,
                "flaky" => Status::Flaky,
                _ => Status::Pass,
            };
            Ok(vec![TestResult {
                id: id.to_string(),
                status,
            }])
        }
    }
}
