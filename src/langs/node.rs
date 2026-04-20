use anyhow::Result;

use super::{LangRunner, RunnerOpts, TestResult, Workspace};
use crate::wasmer::WasmerRunner;

pub struct NodeRunner;

impl LangRunner for NodeRunner {
    const OPTS: RunnerOpts = RunnerOpts {
        name: "node",
        git_repo: "https://github.com/nodejs/node.git",
        git_ref: "v24.13.1",
        // TODO: pin the actual edgejs package slug once confirmed.
        wasmer_package: "wasmer/edgejs",
        docker_compose: None,
    };

    fn discover(&self, _workspace: &Workspace, _filter: Option<&str>) -> Result<Vec<String>> {
        // TODO: rglob `test/**/*.{js,mjs,cjs}` with skip rules
        // (node_upstream.py L95–148).
        unimplemented!()
    }

    fn run_test(
        &self,
        _workspace: &Workspace,
        _wasmer: &WasmerRunner,
        _id: &str,
    ) -> Result<Vec<TestResult>> {
        // TODO: call wasmer.run() with
        //   package = Self::OPTS.wasmer_package,
        //   flags   = ["-q", "--experimental-napi", "--volume <checkout>:<guest>"]
        //   args    = ["<id>"]
        // Drop the `python3 tools/test.py --shell=<wrapper>` indirection
        // from the current impl. Parse via `parse_node_single_file_status`
        // (node_upstream.py L159–175), normalize to Status::{Pass, Fail,
        // Skip, Timeout}.
        unimplemented!()
    }
}
