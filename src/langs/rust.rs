use std::collections::BTreeSet;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use super::{LangRunner, Mode, RunnerOpts, Status, TestResult, Workspace};
use crate::process::ProcessError;
use crate::run_log::RunLog;
use crate::runtime::{RunSpec, RunTarget, Stream, WasmerRuntime};

const BUILD_REPORT_NAME: &str = "build-report.json";
const RUST_TARGET: &str = "wasm32-wasmer-wasi";

pub struct RustRunner;

impl RustRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "rust",
        git_repo: "https://github.com/wasix-org/rust.git",
        git_ref: "v2025-11-07.1+rust-1.90",
        wasmer_package: None,
        wasmer_flags: &[],
        docker_compose: None,
    };

    fn build_report_path(workspace: &Workspace) -> PathBuf {
        let local = workspace.work_dir.join(BUILD_REPORT_NAME);
        if local.is_file() {
            return local;
        }
        workspace
            .output_dir
            .join(".work")
            .join("rust-upstream")
            .join(BUILD_REPORT_NAME)
    }

    fn load_build_results(workspace: &Workspace) -> Result<Vec<BuildResult>> {
        let path = Self::build_report_path(workspace);
        if !path.is_file() {
            bail!(
                "rust build report missing at {}. Run the Rust upstream build phase first",
                path.display()
            );
        }
        let report: BuildReport = serde_json::from_slice(&fs::read(&path)?)?;
        Ok(report.results)
    }

    fn artifact_id(workspace: &str, package: &str, artifact_path: &Path) -> String {
        let stem = artifact_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("artifact");
        format!("{workspace}::{package}::{stem}")
    }

    fn case_id(workspace: &str, package: &str, artifact_path: &Path, test_name: &str) -> String {
        format!(
            "{}::{test_name}",
            Self::artifact_id(workspace, package, artifact_path)
        )
    }

    fn executable_paths(result: &BuildResult) -> Vec<PathBuf> {
        if result.status != "PASS" {
            return Vec::new();
        }
        let workspace = Path::new(&result.workspace_path);
        let mut paths = Vec::new();
        for text in [&result.stdout_tail, &result.stderr_tail] {
            for line in text.lines() {
                if let Some(path) = line
                    .trim()
                    .strip_prefix("Executable ")
                    .and_then(|tail| tail.rsplit_once(" ("))
                    .map(|(_, path)| path.trim_end_matches(')'))
                {
                    let path = Path::new(path);
                    let path = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        workspace.join(path)
                    };
                    if path.exists() {
                        paths.push(path);
                    }
                }
            }
        }
        if paths.is_empty() {
            let deps_dir = workspace
                .join("target")
                .join(RUST_TARGET)
                .join("debug")
                .join("deps");
            if deps_dir.is_dir() {
                let mut candidates: BTreeSet<String> =
                    result.target_names.iter().cloned().collect();
                candidates.insert(result.package.clone());
                if let Ok(entries) = fs::read_dir(&deps_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|ext| ext.to_str()) != Some("wasm") {
                            continue;
                        }
                        let stem = path
                            .file_stem()
                            .and_then(|value| value.to_str())
                            .unwrap_or_default()
                            .replace('-', "_");
                        if candidates.iter().any(|candidate| {
                            let candidate = candidate.replace('-', "_");
                            stem == candidate || stem.starts_with(&format!("{candidate}-"))
                        }) {
                            paths.push(path);
                        }
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    fn parse_listed_tests(output: &str) -> Vec<String> {
        let mut names = Vec::new();
        for line in output.lines() {
            let line = line.trim();
            if let Some((name, kind)) = line.rsplit_once(": ") {
                if matches!(kind, "test" | "benchmark") {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    fn parse_rust_statuses(output: &str) -> Vec<(String, Status)> {
        let mut statuses = Vec::new();
        for line in output.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("test ") else {
                continue;
            };
            let Some((name, status)) = rest.rsplit_once(" ... ") else {
                continue;
            };
            let status = match status.split_whitespace().next() {
                Some("ok") => Status::Pass,
                Some("FAILED") => Status::Fail,
                Some("ignored") => Status::Skip,
                _ => continue,
            };
            statuses.push((name.to_string(), status));
        }
        statuses
    }

    fn artifact_might_contain_test(artifact_path: &Path, test_name: &str) -> bool {
        let needle = test_name.as_bytes();
        if needle.is_empty() {
            return true;
        }
        let Ok(bytes) = fs::read(artifact_path) else {
            return true;
        };
        bytes.windows(needle.len()).any(|window| window == needle)
    }

    fn requested_artifact_id(id: &str) -> Option<String> {
        let mut parts = id.split("::");
        let workspace = parts.next()?;
        let package = parts.next()?;
        let artifact = parts.next()?;
        parts.next()?;
        Some(format!("{workspace}::{package}::{artifact}"))
    }

    fn artifact_path_for_id(workspace: &Workspace, id: &str) -> Result<Option<PathBuf>> {
        let Some(requested_artifact_id) = Self::requested_artifact_id(id) else {
            return Ok(None);
        };
        for result in Self::load_build_results(workspace)? {
            for artifact in Self::executable_paths(&result) {
                if Self::artifact_id(&result.workspace, &result.package, &artifact)
                    == requested_artifact_id
                {
                    return Ok(Some(artifact));
                }
            }
        }
        Ok(None)
    }

    fn list_tests(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        artifact_path: &Path,
    ) -> Result<Vec<String>> {
        let compiled = self.compile_artifact(workspace, wasmer, artifact_path)?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        let result = wasmer.run(
            RunSpec {
                target: RunTarget::File(compiled),
                flags: vec![
                    "--volume".into(),
                    format!(
                        "{}:{}",
                        workspace.checkout.display(),
                        workspace.checkout.display()
                    ),
                    "--cwd".into(),
                    workspace.checkout.display().to_string(),
                ],
                args: vec!["--list".into(), "--format".into(), "terse".into()],
                timeout: None,
            },
            |stream, line| {
                match stream {
                    Stream::Stdout => {
                        stdout.push_str(line);
                        stdout.push('\n');
                    }
                    Stream::Stderr => {
                        stderr.push_str(line);
                        stderr.push('\n');
                    }
                }
                Ok(())
            },
        );
        match result {
            Ok(()) => Ok(Self::parse_listed_tests(&stdout)),
            Err(ProcessError::Spawn(message)) => Err(anyhow!(message)),
            Err(err) => bail!(
                "failed to list rust tests for {}: {err:?}\nstderr: {}",
                artifact_path.display(),
                stderr
            ),
        }
    }

    fn compile_artifact(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        artifact_path: &Path,
    ) -> Result<PathBuf> {
        let mut hasher = DefaultHasher::new();
        artifact_path.hash(&mut hasher);
        let digest = hasher.finish();
        let out_dir = workspace.work_dir.join("compiled");
        fs::create_dir_all(&out_dir)?;
        let out = out_dir.join(format!("{digest:016x}.wasmu"));
        if out.is_file() {
            return Ok(out);
        }
        wasmer
            .compile_file(artifact_path, &out)
            .map_err(|e| anyhow!("failed to precompile {}: {e:?}", artifact_path.display()))?;
        Ok(out)
    }

    fn resolve_case(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
    ) -> Result<RustCase> {
        let results = Self::load_build_results(workspace)?;
        let requested_artifact_id = Self::requested_artifact_id(id);
        for result in &results {
            for artifact in Self::executable_paths(result) {
                let artifact_id = Self::artifact_id(&result.workspace, &result.package, &artifact);
                if requested_artifact_id
                    .as_deref()
                    .is_some_and(|expected| artifact_id != expected)
                {
                    continue;
                }
                if id == artifact_id && artifact.is_file() {
                    return Ok(RustCase {
                        artifact_path: artifact,
                        test_name: None,
                    });
                }
                if requested_artifact_id.is_none()
                    && !artifact_id.contains(id)
                    && !id.contains(&artifact_id)
                    && !Self::artifact_might_contain_test(&artifact, id)
                {
                    continue;
                }
                let tests = self.list_tests(workspace, wasmer, &artifact)?;
                for test_name in tests {
                    let case_id =
                        Self::case_id(&result.workspace, &result.package, &artifact, &test_name);
                    if id == case_id || case_id.contains(id) || id.contains(&case_id) {
                        return Ok(RustCase {
                            artifact_path: artifact,
                            test_name: Some(test_name),
                        });
                    }
                }
            }
        }
        bail!(
            "rust test {id:?} not found in {}",
            Self::build_report_path(workspace).display()
        )
    }
}

impl LangRunner for RustRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
    }

    fn prepare(&self, workspace: &Workspace, wasmer: &WasmerRuntime, ids: &[String]) -> Result<()> {
        let mut artifacts = BTreeSet::new();
        for id in ids {
            if let Some(artifact) = Self::artifact_path_for_id(workspace, id)? {
                artifacts.insert(artifact);
                continue;
            }
            artifacts.insert(self.resolve_case(workspace, wasmer, id)?.artifact_path);
        }
        for artifact in artifacts {
            self.compile_artifact(workspace, wasmer, &artifact)?;
        }
        Ok(())
    }

    fn discover(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        filter: Option<&str>,
    ) -> Result<Vec<String>> {
        if let Some(filter) = filter {
            return Ok(vec![filter.to_string()]);
        }
        let results = Self::load_build_results(workspace)?;
        let mut ids = Vec::new();
        for result in &results {
            for artifact in Self::executable_paths(result) {
                let tests = self.list_tests(workspace, wasmer, &artifact)?;
                for test_name in tests {
                    ids.push(Self::case_id(
                        &result.workspace,
                        &result.package,
                        &artifact,
                        &test_name,
                    ));
                }
            }
        }
        ids.sort();
        ids.dedup();
        Ok(match filter {
            None => ids,
            Some(filter) => {
                let filtered: Vec<String> = ids
                    .into_iter()
                    .filter(|id| {
                        id == filter || id.contains(filter) || filter.contains(id.as_str())
                    })
                    .collect();
                if filtered.is_empty() {
                    vec![filter.to_string()]
                } else {
                    filtered
                }
            }
        })
    }

    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        mode: Mode,
        _log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        let case = self.resolve_case(workspace, wasmer, id)?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut args = vec!["--test-threads=1".into()];
        if let Some(test_name) = &case.test_name {
            args.splice(
                0..0,
                [test_name.clone(), "--exact".into(), "--nocapture".into()],
            );
        }
        let result = wasmer.run(
            RunSpec {
                target: RunTarget::File(self.compile_artifact(
                    workspace,
                    wasmer,
                    &case.artifact_path,
                )?),
                flags: vec![
                    "--volume".into(),
                    format!(
                        "{}:{}",
                        workspace.checkout.display(),
                        workspace.checkout.display()
                    ),
                    "--cwd".into(),
                    workspace.checkout.display().to_string(),
                ],
                args,
                timeout: None,
            },
            |stream, line| {
                if matches!(mode, Mode::Debug) {
                    crate::process::write_stream(stream, line)?;
                }
                match stream {
                    Stream::Stdout => {
                        stdout.push_str(line);
                        stdout.push('\n');
                    }
                    Stream::Stderr => {
                        stderr.push_str(line);
                        stderr.push('\n');
                    }
                }
                Ok(())
            },
        );

        let mut parsed = Self::parse_rust_statuses(&stdout);
        parsed.extend(Self::parse_rust_statuses(&stderr));
        let status = if let Some(test_name) = &case.test_name {
            parsed
                .into_iter()
                .find(|(name, _)| name == test_name)
                .map(|(_, status)| status)
                .unwrap_or(match result {
                    Ok(()) => Status::Pass,
                    Err(ProcessError::Timeout(_)) => Status::Timeout,
                    Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
                    Err(ProcessError::AbnormalExit(_)) | Err(ProcessError::RustPanic(_)) => {
                        Status::Fail
                    }
                })
        } else {
            match result {
                Ok(()) => Status::Pass,
                Err(ProcessError::Timeout(_)) => Status::Timeout,
                Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
                Err(ProcessError::AbnormalExit(_)) | Err(ProcessError::RustPanic(_)) => {
                    Status::Fail
                }
            }
        };

        Ok(vec![TestResult {
            id: id.to_string(),
            status,
        }])
    }
}

#[derive(Deserialize)]
struct BuildReport {
    results: Vec<BuildResult>,
}

#[derive(Deserialize)]
struct BuildResult {
    workspace: String,
    package: String,
    workspace_path: String,
    target_names: Vec<String>,
    status: String,
    stdout_tail: String,
    stderr_tail: String,
}

struct RustCase {
    artifact_path: PathBuf,
    test_name: Option<String>,
}
