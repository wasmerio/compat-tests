pub mod node;
pub mod php;
pub mod python;
pub mod rust;

use std::path::PathBuf;

use anyhow::Result;

use crate::run_log::RunLog;
use crate::wasmer::WasmerRuntime;

pub struct RunnerOpts {
    pub name: &'static str,
    pub git_repo: &'static str,
    pub git_ref: &'static str,
    pub wasmer_package: &'static str,
    pub docker_compose: Option<&'static str>,
}

pub struct Workspace {
    pub output_dir: PathBuf,
    pub checkout: PathBuf,
    pub work_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Pass,
    Fail,
    Skip,
    Timeout,
    Flaky,
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
    const OPTS: RunnerOpts;
    fn prepare(&self, _workspace: &Workspace, _wasmer: &WasmerRuntime) -> Result<()> {
        Ok(())
    }
    fn discover(&self, workspace: &Workspace, filter: Option<&str>) -> Result<Vec<String>>;
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
    use crate::wasmer::WasmerRuntime;

    pub struct MockRunner;

    impl LangRunner for MockRunner {
        const OPTS: RunnerOpts = RunnerOpts {
            name: "mock",
            git_repo: "https://example.invalid/mock.git",
            git_ref: "HEAD",
            wasmer_package: "mock/mock",
            docker_compose: None,
        };

        fn prepare(&self, _workspace: &Workspace, _wasmer: &WasmerRuntime) -> Result<()> {
            Ok(())
        }

        fn discover(&self, _workspace: &Workspace, filter: Option<&str>) -> Result<Vec<String>> {
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
