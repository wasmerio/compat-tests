use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use super::{LangRunner, Mode, RunnerOpts, Status, TestJob, TestResult, Workspace};
use crate::process::{ignore_stream, run_process, write_stream, ProcessError, ProcessSpec};
use crate::run_log::RunLog;
use crate::runtime::WasmerRuntime;

const NODE_TEST_TIMEOUT: Duration = Duration::from_secs(120);
const SKIP_TOP_LEVEL_DIRS: &[&str] = &[
    "cctest",
    "benchmark",
    "addons",
    "doctool",
    "embedding",
    "overlapped-checker",
    "wasi",
    "v8-updates",
    "code-cache",
    "internet",
    "tick-processor",
    "pummel",
    "wpt",
    "system-ca",
];
const SKIP_PATH_PARTS: &[&str] = &["common", "fixtures", "tmp", "testpy"];
const SQLITE_ROOT_JUNK: &[&str] = &["next-db.js", "worker.js"];
const NODE_SUFFIXES: &[&str] = &["js", "mjs", "cjs"];

pub struct NodeRunner;

impl NodeRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "node",
        git_repo: "https://github.com/nodejs/node.git",
        git_ref: "v24.13.1",
        wasmer_package: Some("wasmer/edgejs"),
        wasmer_package_warmup_args: Some(&["-e", "console.log('ok')"]),
        wasmer_flags: &["--experimental-napi"],
        docker_compose: None,
    };

    fn test_dir(workspace: &Workspace) -> PathBuf {
        workspace.checkout.join("test")
    }

    fn wrapper_path(workspace: &Workspace) -> PathBuf {
        workspace.work_dir.join("node_via_wasmer.sh")
    }

    fn ensure_wrapper(&self, workspace: &Workspace, wasmer: &WasmerRuntime) -> Result<PathBuf> {
        let path = Self::wrapper_path(workspace);
        write_node_wrapper(
            &path,
            wasmer,
            workspace,
            Self::OPTS.wasmer_package.expect("node package"),
            Self::OPTS.wasmer_flags,
        )?;
        Ok(path)
    }

    fn run_one(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        id: &str,
        mode: Mode,
        log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        let wrapper = self.ensure_wrapper(workspace, wasmer)?;
        let test_dir = Self::test_dir(workspace);
        let rel_test = workspace.checkout.join("test").join(id);
        if !rel_test.is_file() {
            bail!("node test not found: {}", rel_test.display());
        }

        let log_output = match log {
            Some(log) => Arc::new(log.clone()),
            None => Arc::new(RunLog::new(workspace.work_dir.join("node-debug.log"))),
        };

        let result = run_process(
            ProcessSpec {
                program: "python3".into(),
                args: vec![
                    workspace.checkout.join("tools").join("test.py").into(),
                    "--test-root".into(),
                    test_dir.display().to_string().into(),
                    "--shell".into(),
                    wrapper.display().to_string().into(),
                    "--timeout".into(),
                    NODE_TEST_TIMEOUT.as_secs().to_string().into(),
                    "--progress".into(),
                    "mono".into(),
                    id.into(),
                ],
                env: vec![],
                cwd: workspace.checkout.clone(),
                timeout: NODE_TEST_TIMEOUT,
                log_output,
            },
            match mode {
                Mode::Debug => write_stream,
                Mode::Capture => ignore_stream,
            },
        );

        let status = match result {
            Ok(()) => Status::Pass,
            Err(ProcessError::Timeout(_)) => Status::Timeout,
            Err(ProcessError::AbnormalExit(_)) | Err(ProcessError::RustPanic(_)) => Status::Fail,
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
        };
        Ok(vec![TestResult {
            id: id.to_string(),
            status,
        }])
    }
}

impl LangRunner for NodeRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
    }

    fn discover(
        &self,
        workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        filter: Option<&str>,
        _mode: Mode,
    ) -> Result<Vec<TestJob>> {
        if let Some(filter) = filter {
            let direct = Self::test_dir(workspace).join(filter);
            if direct.is_file() {
                tracing::info!(tests = 1, "discovered node test files");
                return Ok(vec![TestJob {
                    id: filter.to_string(),
                    tests: vec![filter.to_string()],
                }]);
            }
        }

        tracing::info!("discovering node test files");
        let mut tests = BTreeSet::new();
        collect_node_tests(
            &Self::test_dir(workspace),
            &Self::test_dir(workspace),
            &mut tests,
        )?;
        let tests: Vec<String> = tests.into_iter().collect();
        let jobs: Vec<TestJob> = match filter {
            None => tests,
            Some(filter) => tests
                .into_iter()
                .filter(|id| id == filter || id.contains(filter) || filter.contains(id.as_str()))
                .collect(),
        }
        .into_iter()
        .map(|id| TestJob {
            tests: vec![id.clone()],
            id,
        })
        .collect();
        tracing::info!(tests = jobs.len(), "discovered node test files");
        Ok(jobs)
    }

    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        mode: Mode,
        log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        self.run_one(workspace, wasmer, &job.id, mode, log)
    }
}

fn collect_node_tests(root: &Path, dir: &Path, tests: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|e| anyhow!("strip prefix {}: {e}", path.display()))?;
        let parts: Vec<&str> = rel.iter().filter_map(|part| part.to_str()).collect();
        if path.is_dir() {
            if let Some(top) = parts.first() {
                if SKIP_TOP_LEVEL_DIRS.contains(top) {
                    continue;
                }
            }
            if parts.iter().any(|part| SKIP_PATH_PARTS.contains(part)) {
                continue;
            }
            collect_node_tests(root, &path, tests)?;
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| NODE_SUFFIXES.contains(&ext))
        {
            continue;
        }
        if parts
            .iter()
            .any(|part| SKIP_PATH_PARTS.contains(part) || *part == "node_modules")
        {
            continue;
        }
        if parts.first() == Some(&"sqlite") && parts.len() == 2 {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                if SQLITE_ROOT_JUNK.contains(&name) {
                    continue;
                }
            }
        }
        tests.insert(rel.to_string_lossy().replace('\\', "/"));
    }
    Ok(())
}

fn write_node_wrapper(
    path: &Path,
    wasmer: &WasmerRuntime,
    workspace: &Workspace,
    package: &str,
    flags: &[&str],
) -> Result<()> {
    let wasmer_bin = wasmer.binary_path();
    let mut script = String::from("#!/usr/bin/env bash\nset -euo pipefail\nexec ");
    script.push_str(&shell_quote(&wasmer_bin.display().to_string()));
    script.push_str(" run --registry ");
    script.push_str(&shell_quote(crate::runtime::WASMER_REGISTRY));
    script.push_str(" --net");
    for flag in flags {
        script.push(' ');
        script.push_str(&shell_quote(flag));
    }
    script.push_str(" --volume ");
    script.push_str(&shell_quote(&format!(
        "{}:{}",
        workspace.checkout.display(),
        workspace.checkout.display()
    )));
    script.push(' ');
    script.push_str(&shell_quote(package));
    script.push_str(" -- \"$@\"\n");
    fs::write(path, script)?;
    let mut perms = fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() | 0o111);
    }
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}
