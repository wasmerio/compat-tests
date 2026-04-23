use anyhow::Result;

use super::{LangRunner, Mode, RunnerOpts, TestResult, Workspace};
use crate::run_log::RunLog;
use crate::runtime::WasmerRuntime;

pub struct RustRunner;

impl RustRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "rust",
        git_repo: "https://github.com/wasix-org/rust.git",
        git_ref: "v2025-11-07.1+rust-1.90",
        wasmer_package: "rust",
        docker_compose: None,
    };
}

impl LangRunner for RustRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
    }

    fn prepare(&self, _workspace: &Workspace, _wasmer: &WasmerRuntime) -> Result<()> {
        Ok(())
    }

    fn discover(
        &self,
        _workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        _filter: Option<&str>,
    ) -> Result<Vec<String>> {
        unimplemented!()
    }

    fn run_test(
        &self,
        _workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        _id: &str,
        _mode: Mode,
        _log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        unimplemented!()
    }
}
