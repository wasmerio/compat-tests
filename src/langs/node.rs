use anyhow::Result;

use super::{LangRunner, Mode, RunnerOpts, TestResult, Workspace};
use crate::run_log::RunLog;
use crate::runtime::WasmerRuntime;

pub struct NodeRunner;

impl NodeRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "node",
        git_repo: "https://github.com/nodejs/node.git",
        git_ref: "v24.13.1",
        wasmer_package: "wasmer/edgejs",
        docker_compose: None,
    };
}

impl LangRunner for NodeRunner {
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
