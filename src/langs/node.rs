use anyhow::Result;

use super::{LangRunner, Mode, RunnerOpts, TestResult, Workspace};
use crate::run_log::RunLog;
use crate::wasmer::WasmerRuntime;

pub struct NodeRunner;

impl LangRunner for NodeRunner {
    const OPTS: RunnerOpts = RunnerOpts {
        name: "node",
        git_repo: "https://github.com/nodejs/node.git",
        git_ref: "v24.13.1",
        wasmer_package: "wasmer/edgejs",
        docker_compose: None,
    };

    fn discover(&self, _workspace: &Workspace, _filter: Option<&str>) -> Result<Vec<String>> {
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
