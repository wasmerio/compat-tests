use anyhow::Result;

use super::{LangRunner, RunnerOpts, TestResult, Workspace};
use crate::wasmer::WasmerRunner;

pub struct PythonRunner;

impl LangRunner for PythonRunner {
    const OPTS: RunnerOpts = RunnerOpts {
        name: "python",
        git_repo: "https://github.com/wasix-org/cpython.git",
        git_ref: "e3245fc95e570ac823deb50689041bc1f81d6b27",
        wasmer_package: "python/python",
        docker_compose: None,
    };

    fn prepare(&self, _workspace: &Workspace, _wasmer: &WasmerRunner) -> Result<()> {
        // TODO: port `patch_faulthandler_workarounds` (main.py L278–325).
        Ok(())
    }

    fn discover(&self, _workspace: &Workspace, _filter: Option<&str>) -> Result<Vec<String>> {
        // TODO: port `find_jobs` (python_upstream.py L98–106) — scan
        // `Lib/test` for `test_*` files/dirs, return module names like
        // "test.test_os".
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
        //   flags   = ["--volume <checkout>:<guest>"]
        //   args    = ["-c", RUN_CODE, "<id>"]
        // and parse PASS/FAIL/SKIP lines (python_upstream.py L46–58, L133–174),
        // normalizing to Status::{Pass, Fail, Skip, Timeout}.
        unimplemented!()
    }
}
