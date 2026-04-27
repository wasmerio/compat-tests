use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;

use crate::git::{current_branch, ensure_checkout, head_commit};
pub use crate::process::Stream;
use crate::process::{
    ProcessError, ProcessSpec, command_exists, ignore_stream, run_command, run_process,
};
use crate::reports::WasmerIdentity;
use crate::run_log::RunLog;

const WASMER_REPO: &str = "https://github.com/wasmerio/wasmer.git";
const COMPILE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
pub const WASMER_REGISTRY: &str = "wasmer.io";
const USE_PREBUILT_MAIN_WASMER: bool = true;

pub struct WasmerRuntime {
    binary: PathBuf,
    default_timeout: Duration,
    process_log: Arc<RunLog>,
}

pub enum RuntimeSource {
    LocalBinary(PathBuf),
    Git { repo: String, git_ref: String },
}

pub struct ResolvedRuntime {
    pub runtime: WasmerRuntime,
    pub identity: WasmerIdentity,
}

pub struct RunSpec {
    pub target: RunTarget,
    pub flags: Vec<String>,
    pub args: Vec<String>,
    pub timeout: Option<Duration>,
}

pub enum RunTarget {
    Package(String),
    File(PathBuf),
}

#[derive(Deserialize)]
struct GitHubRun {
    #[serde(rename = "databaseId")]
    database_id: u64,
    #[serde(rename = "headSha")]
    head_sha: String,
    conclusion: Option<String>,
    status: Option<String>,
    event: Option<String>,
}

impl WasmerRuntime {
    pub fn with_process_log(&self, process_log: Arc<RunLog>) -> Self {
        Self {
            binary: self.binary.clone(),
            default_timeout: self.default_timeout,
            process_log,
        }
    }

    pub fn resolve(
        source: RuntimeSource,
        work_root: &Path,
        default_timeout: Duration,
        process_log: Arc<RunLog>,
    ) -> Result<ResolvedRuntime> {
        match source {
            RuntimeSource::LocalBinary(path) => {
                let binary = if path.components().count() == 1 {
                    path
                } else {
                    if !path.is_file() {
                        bail!("--wasmer {} is not a file", path.display());
                    }
                    path.canonicalize()?
                };
                tracing::info!(path = %binary.display(), "using local Wasmer binary");
                Ok(ResolvedRuntime {
                    identity: resolve_local_wasmer_identity(&binary)?,
                    runtime: Self {
                        binary,
                        default_timeout,
                        process_log,
                    },
                })
            }
            RuntimeSource::Git { repo, git_ref } => {
                if USE_PREBUILT_MAIN_WASMER
                    && repo == WASMER_REPO
                    && git_ref == "main"
                    && let Some((binary, commit)) = try_download_prebuilt_main_wasmer(work_root)?
                {
                    tracing::info!(path = %binary.display(), "using prebuilt Wasmer main artifact");
                    return Ok(ResolvedRuntime {
                        identity: WasmerIdentity {
                            repo: repo.clone(),
                            git_ref: git_ref.clone(),
                            commit,
                        },
                        runtime: Self {
                            binary,
                            default_timeout,
                            process_log,
                        },
                    });
                }

                let checkout = ensure_checkout(&work_root.join("wasmer"), &repo, &git_ref)?;
                update_wasmer_submodules(&checkout)?;
                tracing::info!(path = %checkout.display(), "building Wasmer from source");
                run_command(
                    Command::new("cargo")
                        .args([
                            "build",
                            "-p",
                            "wasmer-cli",
                            "--features",
                            "llvm,napi-v8",
                            "--release",
                        ])
                        .current_dir(&checkout),
                )?;
                let binary = checkout.join("target").join("release").join("wasmer");
                if !binary.is_file() {
                    bail!("built wasmer binary missing at {}", binary.display());
                }
                Ok(ResolvedRuntime {
                    identity: WasmerIdentity {
                        repo,
                        git_ref: git_ref.clone(),
                        commit: head_commit(&checkout)?,
                    },
                    runtime: Self {
                        binary,
                        default_timeout,
                        process_log,
                    },
                })
            }
        }
    }

    pub fn run<F>(&self, spec: RunSpec, on_line: F) -> std::result::Result<(), ProcessError>
    where
        F: FnMut(Stream, &str) -> Result<()>,
    {
        let timeout = spec.timeout.unwrap_or(self.default_timeout);
        let mut args: Vec<OsString> = vec!["run".into()];
        match &spec.target {
            RunTarget::Package(package) => {
                args.push("--registry".into());
                args.push(WASMER_REGISTRY.into());
                args.push("--net".into());
                args.extend(spec.flags.iter().map(OsString::from));
                args.push(package.into());
            }
            RunTarget::File(path) => {
                args.extend(spec.flags.iter().map(OsString::from));
                args.push(path.into());
            }
        }
        if !spec.args.is_empty() {
            args.push("--".into());
            args.extend(spec.args.iter().map(OsString::from));
        }
        run_process(
            ProcessSpec {
                program: self.binary.clone(),
                args,
                env: vec![("RUST_BACKTRACE".into(), "full".into())],
                cwd: std::env::current_dir()
                    .map_err(|e| ProcessError::Spawn(format!("resolve cwd: {e}")))?,
                timeout,
                log_output: self.process_log.clone(),
            },
            on_line,
        )
    }

    pub(crate) fn binary_path(&self) -> &Path {
        &self.binary
    }

    pub fn compile_file(
        &self,
        wasm: &Path,
        output: &Path,
    ) -> std::result::Result<(), ProcessError> {
        run_process(
            ProcessSpec {
                program: self.binary.clone(),
                args: vec![
                    "compile".into(),
                    "-o".into(),
                    output.as_os_str().to_os_string(),
                    wasm.as_os_str().to_os_string(),
                ],
                env: vec![("RUST_BACKTRACE".into(), "full".into())],
                cwd: std::env::current_dir()
                    .map_err(|e| ProcessError::Spawn(format!("resolve cwd: {e}")))?,
                timeout: self.default_timeout.max(COMPILE_TIMEOUT),
                log_output: self.process_log.clone(),
            },
            ignore_stream,
        )
    }
}

fn resolve_local_wasmer_identity(wasmer_bin: &Path) -> Result<WasmerIdentity> {
    if let Some(checkout) = infer_wasmer_checkout_from_bin(wasmer_bin)
        && checkout.join(".git").exists()
    {
        let branch = current_branch(&checkout)?;
        let commit = head_commit(&checkout)?;
        return Ok(WasmerIdentity {
            repo: WASMER_REPO.to_string(),
            git_ref: branch.clone(),
            commit,
        });
    }
    Ok(WasmerIdentity {
        repo: "local".to_string(),
        git_ref: "local".to_string(),
        commit: "local".to_string(),
    })
}

fn infer_wasmer_checkout_from_bin(wasmer_bin: &Path) -> Option<PathBuf> {
    wasmer_bin
        .canonicalize()
        .ok()?
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
}

fn try_download_prebuilt_main_wasmer(work_root: &Path) -> Result<Option<(PathBuf, String)>> {
    if !command_exists("gh") {
        tracing::info!("prebuilt Wasmer main artifact unavailable: gh CLI not found");
        return Ok(None);
    }
    if std::env::consts::OS != "linux" {
        tracing::info!(
            os = std::env::consts::OS,
            "prebuilt Wasmer main artifact unavailable: unsupported OS"
        );
        return Ok(None);
    }
    if !matches!(std::env::consts::ARCH, "x86_64" | "amd64") {
        tracing::info!(
            arch = std::env::consts::ARCH,
            "prebuilt Wasmer main artifact unavailable: unsupported machine"
        );
        return Ok(None);
    }

    let out = Command::new("gh")
        .args([
            "run",
            "list",
            "--repo",
            "wasmerio/wasmer",
            "--workflow",
            "build.yml",
            "--branch",
            "main",
            "--limit",
            "10",
            "--json",
            "databaseId,headSha,conclusion,status,event",
        ])
        .output()?;
    if !out.status.success() {
        tracing::warn!("prebuilt Wasmer main artifact lookup failed");
        if !out.stderr.is_empty() {
            tracing::warn!("{}", String::from_utf8_lossy(&out.stderr));
        }
        return Ok(None);
    }
    let runs: Vec<GitHubRun> = serde_json::from_slice(&out.stdout)?;
    let Some(run) = runs.into_iter().find(|run| {
        run.status.as_deref() == Some("completed")
            && run.conclusion.as_deref() == Some("success")
            && run.event.as_deref() == Some("push")
    }) else {
        tracing::info!(
            "prebuilt Wasmer main artifact unavailable: no successful main push build run found"
        );
        return Ok(None);
    };

    let cache_dir = work_root.join("prebuilt-wasmer").join(&run.head_sha);
    let install_dir = cache_dir.join("install");
    let wasmer_bin = install_dir.join("bin").join("wasmer");
    if wasmer_bin.exists() {
        tracing::info!(sha = %run.head_sha, "using cached prebuilt Wasmer main artifact");
        return Ok(Some((wasmer_bin, run.head_sha)));
    }

    if cache_dir.exists() {
        std::fs::remove_dir_all(&cache_dir)?;
    }
    std::fs::create_dir_all(&cache_dir)?;
    let download = Command::new("gh")
        .args([
            "run",
            "download",
            &run.database_id.to_string(),
            "--repo",
            "wasmerio/wasmer",
            "-n",
            "wasmer-linux-amd64",
            "-D",
        ])
        .arg(&cache_dir)
        .output()?;
    if !download.status.success() {
        tracing::warn!("prebuilt Wasmer main artifact download failed");
        if !download.stderr.is_empty() {
            tracing::warn!("{}", String::from_utf8_lossy(&download.stderr));
        }
        return Ok(None);
    }

    let archive = cache_dir.join("wasmer.tar.gz");
    if !archive.exists() {
        tracing::warn!("prebuilt Wasmer main artifact download failed: wasmer.tar.gz missing");
        return Ok(None);
    }
    std::fs::create_dir_all(&install_dir)?;
    let archive_file = std::fs::File::open(&archive)?;
    if let Err(error) = Archive::new(GzDecoder::new(archive_file)).unpack(&install_dir) {
        tracing::warn!("prebuilt Wasmer main artifact extraction failed: {error}");
        return Ok(None);
    }
    if !wasmer_bin.exists() {
        tracing::warn!("prebuilt Wasmer main artifact extraction failed: bin/wasmer missing");
        return Ok(None);
    }
    Ok(Some((wasmer_bin, run.head_sha)))
}

fn update_wasmer_submodules(checkout: &Path) -> Result<()> {
    run_command(
        Command::new("git")
            .args(["submodule", "update", "--init", "--depth", "1", "lib/napi"])
            .current_dir(checkout),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn runtime_resolves_binary() {
        let dir = TempDir::new("shield-runtime").expect("tempdir");
        let resolved = WasmerRuntime::resolve(
            RuntimeSource::LocalBinary("wasmer".into()),
            dir.path(),
            Duration::from_secs(10),
            Arc::new(RunLog::new(dir.path().join("process.log"))),
        )
        .expect("resolve");
        let mut version = String::new();
        run_process(
            ProcessSpec {
                program: resolved.runtime.binary.clone(),
                args: vec!["--version".into()],
                env: vec![("RUST_BACKTRACE".into(), "full".into())],
                cwd: std::env::current_dir().expect("cwd"),
                timeout: Duration::from_secs(10),
                log_output: Arc::new(RunLog::new(dir.path().join("process.log"))),
            },
            |stream, line| {
                if matches!(stream, Stream::Stdout) {
                    version.push_str(line);
                    version.push('\n');
                }
                Ok(())
            },
        )
        .expect("version");
        assert!(version.to_lowercase().contains("wasmer"));
    }

    #[test]
    #[ignore = "slow: resolves Wasmer main by downloading or building from source"]
    fn runtime_resolves_git() {
        let dir = TempDir::new("shield-runtime-main").expect("tempdir");
        let resolved = WasmerRuntime::resolve(
            RuntimeSource::Git {
                repo: WASMER_REPO.to_string(),
                git_ref: "main".to_string(),
            },
            dir.path(),
            Duration::from_secs(10),
            Arc::new(RunLog::new(dir.path().join("process.log"))),
        )
        .expect("resolve");
        assert!(!resolved.identity.commit.is_empty());
        assert!(resolved.runtime.binary.is_file());
    }
}
