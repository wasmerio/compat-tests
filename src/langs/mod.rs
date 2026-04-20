//! Per-language test runner plugins.
//!
//! Everything generic — git clone, docker-compose, worker pool, streaming
//! results to console + log, writing `results/<lang>/…` JSON, flaky rerun,
//! publish / PR-comment / issue — lives in the orchestrator.
//!
//! Each language only answers:
//!
//! 1. Static metadata (`OPTS`): name, git repo/ref, wasmer package,
//!    optional docker-compose file
//! 2. How to patch / build the checkout (via `WasmerRunner::compile` if needed)
//! 3. How to enumerate runnable items
//! 4. Given one item, how to run it (via `WasmerRunner::run`) and classify
//!    its output into per-test statuses

pub mod node;
pub mod php;
pub mod python;
pub mod rust;

use std::path::PathBuf;

use anyhow::Result;

use crate::wasmer::WasmerRunner;

/// Static per-runner metadata. Accessed as `<Runner as LangRunner>::OPTS`.
pub struct RunnerOpts {
    /// Short name used for `results/<name>/…` paths and log prefixes.
    pub name: &'static str,
    /// Upstream git repo URL.
    pub git_repo: &'static str,
    /// Commit/tag/branch to pin.
    pub git_ref: &'static str,
    /// Wasmer package the runner dispatches to. Registry slug for
    /// Python/Node/PHP; informational for Rust (which uses local wasm
    /// binaries and overrides per test id inside `run_test`).
    pub wasmer_package: &'static str,
    /// Optional docker-compose file, path relative to the checkout root.
    /// Orchestrator brings it up before `prepare` and tears it down after.
    pub docker_compose: Option<&'static str>,
}

/// Paths available to every `LangRunner` method after clone + compose-up.
pub struct Workspace {
    /// Root of the cloned upstream repo.
    pub checkout: PathBuf,
    /// Per-language scratch directory (logs, intermediate artifacts).
    pub work_dir: PathBuf,
}

/// Canonical test outcome. Each runner parses its own wire statuses
/// (Python's unittest lines, PHP's `PASSED`/`BORKED`/`WARNED`/`LEAKED`/
/// `XFAILED`/`XLEAKED`, Rust's `ok`/`ignored`/`FAILED`, Node's
/// exit-code rules) and normalizes to one of these four.
pub enum Status {
    Pass,
    Fail,
    Skip,
    Timeout,
}

pub struct TestResult {
    pub id: String,
    pub status: Status,
}

pub trait LangRunner: Send + Sync {
    /// Static metadata. Everything that doesn't depend on runtime state.
    const OPTS: RunnerOpts;

    /// Apply patches, write shims, precompile artifacts.
    ///
    /// Called **serially**. Do not spawn a worker pool here — `wasmer compile`
    /// and `cargo build` already saturate all cores (wasmer's LLVM pool
    /// defaults to `available_parallelism()`); extra parallelism
    /// oversubscribes CPU on 4-core CI runners.
    fn prepare(&self, _workspace: &Workspace, _wasmer: &WasmerRunner) -> Result<()> {
        Ok(())
    }

    /// Enumerate runnable items. Each string is the input for exactly one
    /// `wasmer run` — a Python module, a Node test file, a `.phpt`, or a
    /// wasm test binary.
    ///
    /// When `filter` is `Some`, narrow to items whose output would contain
    /// a test id matching it. Used for debug and flaky stabilization runs.
    fn discover(&self, workspace: &Workspace, filter: Option<&str>) -> Result<Vec<String>>;

    /// Run item `id`: call `wasmer.run(...)`, parse its output, return
    /// per-test results. One item may yield many results (e.g. a Python
    /// module produces one per test method inside).
    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRunner,
        id: &str,
    ) -> Result<Vec<TestResult>>;
}
