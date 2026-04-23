use anyhow::Result;

use super::{LangRunner, Mode, RunnerOpts, TestResult, Workspace};
use crate::run_log::RunLog;
use crate::runtime::WasmerRuntime;

pub struct PhpRunner;

impl PhpRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "php",
        git_repo: "TODO",
        git_ref: "TODO",
        wasmer_package: "php/php",
        docker_compose: None,
    };
}

impl LangRunner for PhpRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
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
