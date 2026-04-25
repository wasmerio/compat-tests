use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::{LangRunner, Mode, RunnerOpts, Status, TestJob, TestResult, Workspace};
use crate::process::{ProcessError, ProcessSpec, ignore_stream, run_process, write_stream};
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
    const BATCH_SIZE: usize = 50;

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

    fn result_file(workspace: &Workspace, job: &TestJob) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        job.id.hash(&mut hasher);
        job.tests.hash(&mut hasher);
        workspace
            .work_dir
            .join(format!("node-results-{:016x}.tap", hasher.finish()))
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
        job: &TestJob,
        mode: Mode,
        log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        let wrapper = self.ensure_wrapper(workspace, wasmer)?;
        let test_dir = Self::test_dir(workspace);
        for id in &job.tests {
            let rel_test = workspace.checkout.join("test").join(id);
            if !rel_test.is_file() {
                bail!("node test not found: {}", rel_test.display());
            }
        }
        fs::create_dir_all(&workspace.work_dir)?;
        let result_file = Self::result_file(workspace, job);
        let _ = fs::remove_file(&result_file);

        let log_output = match log {
            Some(log) => Arc::new(log.clone()),
            None => Arc::new(RunLog::new(workspace.work_dir.join("node-debug.log"))),
        };
        let mut args = vec![
            workspace.checkout.join("tools").join("test.py").into(),
            "--test-root".into(),
            test_dir.display().to_string().into(),
            "--shell".into(),
            wrapper.display().to_string().into(),
            "--timeout".into(),
            NODE_TEST_TIMEOUT.as_secs().to_string().into(),
            "--progress".into(),
            "tap".into(),
            "--logfile".into(),
            result_file.display().to_string().into(),
            "-j".into(),
            "1".into(),
        ];
        args.extend(job.tests.iter().cloned().map(Into::into));

        let result = run_process(
            ProcessSpec {
                program: "python3".into(),
                args,
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

        let parsed = normalize_tap_results(parse_tap_results(&result_file)?, &job.tests);
        let fallback = match result {
            Ok(()) => Status::Pass,
            Err(ProcessError::Timeout(_)) => Status::Timeout,
            Err(ProcessError::AbnormalExit(message)) if parsed.is_empty() => {
                return Err(anyhow!(ProcessError::AbnormalExit(message)));
            }
            Err(ProcessError::AbnormalExit(_)) => Status::Fail,
            Err(ProcessError::RustPanic(message)) => {
                return Err(anyhow!(ProcessError::RustPanic(message)));
            }
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
        };
        Ok(job
            .tests
            .iter()
            .cloned()
            .chain(parsed.keys().filter(|id| !job.tests.contains(*id)).cloned())
            .map(|id| TestResult {
                status: parsed.get(&id).copied().unwrap_or(fallback),
                id,
            })
            .collect())
    }

    fn batch_jobs(ids: Vec<String>) -> Vec<TestJob> {
        ids.chunks(Self::BATCH_SIZE)
            .enumerate()
            .map(|(index, chunk)| TestJob {
                id: format!("node-batch-{index:04}"),
                tests: chunk.to_vec(),
            })
            .collect()
    }

    fn batch_filter(filter: &str) -> Option<usize> {
        filter
            .strip_prefix("node-batch-")
            .and_then(|index| index.parse().ok())
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
            None => Self::batch_jobs(tests),
            Some(filter) if Self::batch_filter(filter).is_some() => Self::batch_jobs(tests)
                .into_iter()
                .filter(|job| job.id == filter)
                .collect(),
            Some(filter) => tests
                .into_iter()
                .filter(|id| id == filter || id.contains(filter) || filter.contains(id.as_str()))
                .map(|id| TestJob {
                    tests: vec![id.clone()],
                    id,
                })
                .collect(),
        };
        let total_tests: usize = jobs.iter().map(|job| job.tests.len()).sum();
        tracing::info!(
            jobs = jobs.len(),
            tests = total_tests,
            "discovered node test files"
        );
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
        self.run_one(workspace, wasmer, job, mode, log)
    }
}

fn parse_tap_results(path: &Path) -> Result<BTreeMap<String, Status>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let mut results = BTreeMap::new();
    for line in fs::read_to_string(path)?.lines() {
        let line = line.trim();
        if let Some(id) = line.strip_prefix("ok ").and_then(parse_tap_id) {
            let status = if line.contains(" # skip ") {
                Status::Skip
            } else {
                Status::Pass
            };
            results.insert(id, status);
        } else if let Some(id) = line.strip_prefix("not ok ").and_then(parse_tap_id) {
            results.insert(id, Status::Fail);
        }
    }
    Ok(results)
}

fn parse_tap_id(line: &str) -> Option<String> {
    let (_, rest) = line.split_once(' ')?;
    let id = rest.split(" # ").next()?.trim();
    (!id.is_empty()).then(|| id.replace('\\', "/"))
}

fn normalize_tap_results(
    parsed: BTreeMap<String, Status>,
    expected: &[String],
) -> BTreeMap<String, Status> {
    parsed
        .into_iter()
        .map(|(id, status)| {
            let id = expected
                .iter()
                .find(|expected| expected.as_str() == id || test_id_without_suffix(expected) == id)
                .cloned()
                .unwrap_or(id);
            (id, status)
        })
        .collect()
}

fn test_id_without_suffix(id: &str) -> &str {
    NODE_SUFFIXES
        .iter()
        .find_map(|suffix| id.strip_suffix(&format!(".{suffix}")))
        .unwrap_or(id)
}

fn collect_node_tests(root: &Path, dir: &Path, tests: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|e| anyhow!("strip prefix {}: {e}", path.display()))?;
        let parts: Vec<&str> = rel.iter().filter_map(|part| part.to_str()).collect();
        if path.is_dir() {
            if let Some(top) = parts.first()
                && SKIP_TOP_LEVEL_DIRS.contains(top)
            {
                continue;
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
        if parts.first() == Some(&"sqlite")
            && parts.len() == 2
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
            && SQLITE_ROOT_JUNK.contains(&name)
        {
            continue;
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
    let mut script = String::from(
        "#!/usr/bin/env bash\nset -euo pipefail\nchild=\"\"\ncleanup() {\n  if [[ -n \"$child\" ]]; then\n    kill -KILL \"$child\" 2>/dev/null || true\n  fi\n}\ntrap cleanup TERM INT ABRT\n",
    );
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
    script.push_str(" -- \"$@\" &\nchild=$!\nwait \"$child\"\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_node_tests() {
        let ids: Vec<String> = (0..121).map(|i| format!("parallel/test-{i}.js")).collect();
        let jobs = NodeRunner::batch_jobs(ids);
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].tests.len(), 50);
        assert_eq!(jobs[1].tests.len(), 50);
        assert_eq!(jobs[2].tests.len(), 21);
        assert_eq!(jobs[0].id, "node-batch-0000");
    }

    #[test]
    fn node_batch_filter_selects_whole_batch() {
        let ids: Vec<String> = (0..121).map(|i| format!("parallel/test-{i}.js")).collect();
        let jobs = NodeRunner::batch_jobs(ids);
        let selected: Vec<_> = jobs
            .into_iter()
            .filter(|job| job.id == "node-batch-0001")
            .collect();
        assert_eq!(NodeRunner::batch_filter("node-batch-0001"), Some(1));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].tests.len(), 50);
        assert_eq!(selected[0].tests[0], "parallel/test-50.js");
    }

    #[test]
    fn parses_tap_results() {
        let dir = tempdir::TempDir::new("node-tap").unwrap();
        let path = dir.path().join("results.tap");
        fs::write(
            &path,
            "\
TAP version 13
1..3
ok 1 parallel/test-pass.js
ok 2 parallel/test-skip.js # skip unsupported
not ok 3 parallel/test-fail.js
",
        )
        .unwrap();

        let results = parse_tap_results(&path).unwrap();
        assert_eq!(results["parallel/test-pass.js"], Status::Pass);
        assert_eq!(results["parallel/test-skip.js"], Status::Skip);
        assert_eq!(results["parallel/test-fail.js"], Status::Fail);
    }

    #[test]
    fn normalizes_tap_ids_without_js_suffix() {
        let parsed = BTreeMap::from([("parallel/test-global".to_string(), Status::Pass)]);
        let normalized = normalize_tap_results(parsed, &["parallel/test-global.js".to_string()]);
        assert_eq!(normalized["parallel/test-global.js"], Status::Pass);
        assert!(!normalized.contains_key("parallel/test-global"));
    }
}
