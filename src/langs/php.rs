use anyhow::Result;

use super::{LangRunner, RunnerOpts, TestResult, Workspace};
use crate::wasmer::WasmerRunner;

pub struct PhpRunner;

impl LangRunner for PhpRunner {
    const OPTS: RunnerOpts = RunnerOpts {
        name: "php",
        // TODO: php_upstream.py doesn't clone; it takes `source_root` as input.
        // Pick the upstream fork/ref to pin against and fill these in.
        git_repo: "TODO",
        git_ref: "TODO",
        wasmer_package: "php/php",
        // TODO: PR #4 adds a docker-compose.yml for DB services — set this
        // to its path when that lands.
        docker_compose: None,
    };

    fn discover(&self, _workspace: &Workspace, _filter: Option<&str>) -> Result<Vec<String>> {
        // TODO: glob `**/*.phpt`. Replaces `run-tests.php -j` single-process
        // driver — the rewrite does per-file parallelism.
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
        //   env     = SERVICE_ENV_DEFAULTS (DB hosts etc.; php_upstream.py L26–33)
        //   args    = what PHP needs to run one .phpt
        // Then parse PHP's wire statuses (PHP_RESULT_MAP, php_upstream.py
        // L14–23) and normalize to Status::{Pass, Fail, Skip, Timeout}:
        //
        //   PASSED  → Pass       BORKED  → Fail    (broken harness)
        //   FAILED  → Fail       WARNED  → Pass    (passed with warnings)
        //   SKIPPED → Skip       LEAKED  → Fail    (memory leak)
        //                        XFAILED → Skip    (expected failure)
        //                        XLEAKED → Skip    (expected leak)
        //
        // Confirm these mappings against whatever semantics the reporting
        // expects before shipping.
        unimplemented!()
    }
}
