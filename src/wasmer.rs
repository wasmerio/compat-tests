//! The one place that knows where the `wasmer` binary lives and how to
//! invoke it. Every `LangRunner` goes through this — no runner ever
//! constructs its own `Command` for wasmer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

/// Flags applied to every `wasmer run` invocation automatically.
/// Kept minimal on purpose — `--net` is universal across all 4 language
/// suites; anything else is per-call.
const DEFAULT_RUN_FLAGS: &[&str] = &["--net"];

pub struct WasmerRunner {
    binary: PathBuf,
    default_timeout: Duration,
}

/// What to hand to one `wasmer run` invocation.
pub struct RunSpec {
    /// Registry package name (e.g. `python/python`) or absolute path to a
    /// local `.wasm` / `.wasmu` file.
    pub package: String,
    /// Args passed to the guest after `--`.
    pub args: Vec<String>,
    /// Extra `wasmer run` flags on top of `DEFAULT_RUN_FLAGS`
    /// (e.g. `--volume a:b`, `--experimental-napi`).
    pub flags: Vec<String>,
    /// Env vars for the `wasmer` process itself.
    pub env: Vec<(String, String)>,
    /// cwd for the `wasmer` process (needed by Rust wasm binaries).
    pub cwd: Option<PathBuf>,
    /// Override the runner's default timeout. `None` uses the default.
    pub timeout: Option<Duration>,
}

/// Everything captured from one invocation.
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
}

impl WasmerRunner {
    pub fn new(binary: PathBuf, default_timeout: Duration) -> Self {
        Self {
            binary,
            default_timeout,
        }
    }

    /// Spawn `wasmer run [DEFAULT_RUN_FLAGS] [spec.flags] <package> -- <args>`,
    /// enforce the timeout, capture stdout/stderr.
    pub fn run(&self, _spec: RunSpec) -> Result<Output> {
        // TODO: assemble Command from self.binary + DEFAULT_RUN_FLAGS + spec,
        // spawn with timeout, capture output.
        let _ = &self.binary;
        let _ = self.default_timeout;
        let _ = DEFAULT_RUN_FLAGS;
        unimplemented!()
    }

    /// Precompile a `.wasm` to `.wasmu` via `wasmer compile`. Returns the
    /// path to the produced artifact. Used by Rust's `prepare` to amortize
    /// LLVM compile time across many test runs of the same binary.
    pub fn compile(&self, _wasm: &Path) -> Result<PathBuf> {
        // TODO: wasmer compile <wasm> -o <wasm>.wasmu ; return that path.
        unimplemented!()
    }
}
