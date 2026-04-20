use anyhow::Result;

use super::{LangRunner, RunnerOpts, TestResult, Workspace};
use crate::wasmer::WasmerRunner;

pub struct RustRunner;

impl LangRunner for RustRunner {
    const OPTS: RunnerOpts = RunnerOpts {
        name: "rust",
        git_repo: "https://github.com/wasix-org/rust.git",
        git_ref: "v2025-11-07.1+rust-1.90",
        // Rust dispatches to many different local `.wasm` binaries, not a
        // registry package. This field is informational — `run_test` builds
        // the actual absolute path per test id.
        wasmer_package: "rust",
        docker_compose: None,
    };

    fn prepare(&self, _workspace: &Workspace, _wasmer: &WasmerRunner) -> Result<()> {
        // By far the heaviest `prepare` of the four. Port, in order:
        //   1. submodule sync/update for `SUBMODULES` (rust_upstream_build.py L700–717)
        //   2. `RUST_SOURCE_FIXUPS` + manifest dependency fixups (L409–518)
        //   3. `ensure_dependency_patches` — clone `DEPENDENCY_FORKS`
        //      into `work_dir/vendor`, write cargo patch config (L840–891)
        //   4. `ensure_wasix_sysroot` symlink (L731–758)
        //   5. optional genmc checkout (L720–728)
        //   6. serial `cargo wasix test --no-run` per selected package (L2052–2055)
        //   7. serial `wasmer.compile(binary_path)` per artifact to amortize
        //      LLVM compile across all future test runs (L1275–1360).
        //
        // Everything here runs serially — cargo + wasmer compile already
        // saturate all cores.
        Ok(())
    }

    fn discover(&self, _workspace: &Workspace, _filter: Option<&str>) -> Result<Vec<String>> {
        // TODO: `cargo metadata` (rust_upstream_build.py L956–978) → one id
        // per wasm test binary. Individual test cases inside a binary are
        // extracted later by `run_test`'s output parser.
        unimplemented!()
    }

    fn run_test(
        &self,
        _workspace: &Workspace,
        _wasmer: &WasmerRunner,
        _id: &str,
    ) -> Result<Vec<TestResult>> {
        // TODO: call wasmer.run() with
        //   package = "<absolute path to the wasm test binary>"   (local, not Self::OPTS.wasmer_package)
        //   args    = ["--test-threads=1", …]
        //   cwd     = Some(binary.workspace_path)
        // Then port `parse_rust_test_statuses` (rust_upstream_build.py
        // L1504–1517): `ok` → Status::Pass, `ignored` → Status::Skip,
        // else → Status::Fail; handle `::precompile` / `::list` synthetic
        // rows (L1529–1587); apply Status::Timeout on timeout.
        unimplemented!()
    }
}
