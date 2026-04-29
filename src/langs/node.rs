use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};

use super::{
    LangRunner, Mode, RunnerOpts, Status, TestIssue, TestJob, TestResult, TestRunOutput, Workspace,
};
use crate::process::{
    ProcessError, ProcessSpec, extract_runtime_crash_text, ignore_stream, run_process, write_stream,
};
use crate::run_log::RunLog;
use crate::runtime::WasmerRuntime;

const NODE_TEST_TIMEOUT: Duration = Duration::from_secs(90);
const NODE_HARNESS_TIMEOUT: Duration = Duration::from_secs(120);
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
    const BATCH_SIZE: usize = 25;

    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "node",
        git_repo: "https://github.com/nodejs/node.git",
        git_ref: "v24.13.1",
        wasmer_package: Some("wasmer/edgejs@0.0.0-f57f970"),
        wasmer_package_warmup_args: Some(&["-e", "console.log('ok')"]),
        wasmer_flags: &["--experimental-napi"],
        docker_compose: None,
    };

    fn test_dir(workspace: &Workspace) -> PathBuf {
        workspace.checkout.join("test")
    }

    fn wrapper_path(workspace: &Workspace, job: &TestJob) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        job.id.hash(&mut hasher);
        job.tests.hash(&mut hasher);
        workspace
            .work_dir
            .join(format!("node-wrapper-{:016x}.sh", hasher.finish()))
    }

    fn result_file(workspace: &Workspace, job: &TestJob) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        job.id.hash(&mut hasher);
        job.tests.hash(&mut hasher);
        workspace
            .work_dir
            .join(format!("node-results-{:016x}.tap", hasher.finish()))
    }

    fn job_namespace(job: &TestJob) -> String {
        static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut hasher = DefaultHasher::new();
        job.id.hash(&mut hasher);
        job.tests.hash(&mut hasher);
        process::id().hash(&mut hasher);
        RUN_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .hash(&mut hasher);
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut hasher);
        format!("compat-{:016x}", hasher.finish())
    }

    fn ensure_wrapper(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        test_serial_id: &str,
    ) -> Result<PathBuf> {
        let path = Self::wrapper_path(workspace, job);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_node_wrapper(
            &path,
            wasmer,
            workspace,
            Self::OPTS.wasmer_package.expect("node package"),
            Self::OPTS.wasmer_flags,
            test_serial_id,
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
    ) -> Result<TestRunOutput> {
        let test_serial_id = Self::job_namespace(job);
        let wrapper = self.ensure_wrapper(workspace, wasmer, job, &test_serial_id)?;
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
                env: vec![("TEST_SERIAL_ID".into(), test_serial_id.into())],
                cwd: workspace.checkout.clone(),
                // Let Node's own timeout handler write a TAP result before we
                // kill the whole harness process.
                timeout: NODE_HARNESS_TIMEOUT,
                log_output,
            },
            match mode {
                Mode::Debug => write_stream,
                Mode::Capture => ignore_stream,
            },
        );

        let parsed = normalize_tap_entries(parse_tap_results(&result_file)?, &job.tests);
        let fallback = match result {
            Ok(()) => Status::Pass,
            Err(ProcessError::Timeout(_)) => Status::Timeout,
            Err(ProcessError::AbnormalExit(message)) if parsed.is_empty() => {
                return Err(anyhow!(ProcessError::AbnormalExit(message)));
            }
            Err(ProcessError::AbnormalExit(_)) => Status::Fail,
            Err(ProcessError::RustCrash(message)) => {
                return Err(anyhow!(ProcessError::RustCrash(message)));
            }
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
        };
        let mut issues = vec![];
        for (id, entry) in &parsed {
            if let Some(message) = &entry.issue {
                issues.push(TestIssue {
                    id: id.clone(),
                    message: message.clone(),
                });
            }
        }
        Ok(TestRunOutput {
            results: job
                .tests
                .iter()
                .cloned()
                .chain(parsed.keys().filter(|id| !job.tests.contains(*id)).cloned())
                .map(|id| TestResult {
                    status: parsed
                        .get(&id)
                        .map(|entry| entry.status)
                        .unwrap_or(fallback),
                    id,
                })
                .collect(),
            issues,
        })
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

    fn thread_count_multiplier(&self) -> usize {
        // Node's suite is mostly IO-bound. Smaller batches reduce per-process
        // accumulated state while extra workers keep total CI time bounded.
        2
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
    ) -> Result<TestRunOutput> {
        self.run_one(workspace, wasmer, job, mode, log)
    }
}

#[derive(Debug, PartialEq)]
struct TapResult {
    status: Status,
    issue: Option<String>,
}

struct CurrentTapResult {
    id: String,
    status: Status,
    block: Vec<String>,
    exit_code: Option<i32>,
    expect_stack_line: bool,
}

fn parse_tap_results(path: &Path) -> Result<BTreeMap<String, TapResult>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let mut results = BTreeMap::new();
    let mut current = None;
    for raw_line in fs::read_to_string(path)?.lines() {
        let line = raw_line.trim();
        if let Some(id) = line.strip_prefix("ok ").and_then(parse_tap_id) {
            flush_tap_result(&mut results, &mut current);
            current = Some(CurrentTapResult {
                id,
                status: if line.contains(" # skip ") {
                    Status::Skip
                } else {
                    Status::Pass
                },
                block: vec![raw_line.to_string()],
                exit_code: None,
                expect_stack_line: false,
            });
            continue;
        }
        if let Some(id) = line.strip_prefix("not ok ").and_then(parse_tap_id) {
            flush_tap_result(&mut results, &mut current);
            current = Some(CurrentTapResult {
                id,
                status: Status::Fail,
                block: vec![raw_line.to_string()],
                exit_code: None,
                expect_stack_line: false,
            });
            continue;
        }
        if let Some(current) = current.as_mut() {
            current.block.push(raw_line.to_string());
            if current.expect_stack_line {
                if line == "timeout" {
                    current.status = Status::Timeout;
                }
                current.expect_stack_line = false;
            } else if line == "stack: |-" {
                current.expect_stack_line = true;
            } else if let Some(exit_code) = line
                .strip_prefix("exitcode: ")
                .and_then(|value| value.trim().parse().ok())
            {
                current.exit_code = Some(exit_code);
            }
        }
        if line == "..." {
            flush_tap_result(&mut results, &mut current);
        }
    }
    flush_tap_result(&mut results, &mut current);
    Ok(results)
}

fn flush_tap_result(
    results: &mut BTreeMap<String, TapResult>,
    current: &mut Option<CurrentTapResult>,
) {
    if let Some(current) = current.take() {
        let issue = node_crash_issue(&current);
        results.insert(
            current.id,
            TapResult {
                status: current.status,
                issue,
            },
        );
    }
}

fn node_crash_issue(result: &CurrentTapResult) -> Option<String> {
    if result.status == Status::Timeout {
        return None;
    }
    let block = result.block.join("\n");
    extract_runtime_crash_text(&block).map(|crash| format!("crash: {crash}"))
}

fn parse_tap_id(line: &str) -> Option<String> {
    let (_, rest) = line.split_once(' ')?;
    let id = rest.split(" # ").next()?.trim();
    (!id.is_empty()).then(|| id.replace('\\', "/"))
}

fn normalize_tap_entries(
    parsed: BTreeMap<String, TapResult>,
    expected: &[String],
) -> BTreeMap<String, TapResult> {
    parsed
        .into_iter()
        .map(|(id, entry)| {
            let id = expected
                .iter()
                .find(|expected| expected.as_str() == id || test_id_without_suffix(expected) == id)
                .cloned()
                .unwrap_or(id);
            (id, entry)
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
    test_serial_id: &str,
) -> Result<()> {
    let wasmer_bin = wasmer.binary_path();
    let mut script = String::from(
        "#!/usr/bin/env bash\nset -euo pipefail\nchild=\"\"\ncleanup() {\n  if [[ -n \"$child\" ]]; then\n    kill -KILL \"$child\" 2>/dev/null || true\n  fi\n}\ntrap cleanup TERM INT ABRT\n",
    );
    script.push_str("\ntest_serial_base=${TEST_SERIAL_ID:-");
    script.push_str(&shell_quote(test_serial_id));
    script
        .push_str("}\ntest_serial_id=\"${test_serial_base}-${BASHPID:-$$}-${RANDOM}-${RANDOM}\"\n");
    script.push_str(&shell_quote(&wasmer_bin.display().to_string()));
    script.push_str(" run --registry ");
    script.push_str(&shell_quote(crate::runtime::WASMER_REGISTRY));
    script.push_str(" --net");
    for flag in flags {
        script.push(' ');
        script.push_str(&shell_quote(flag));
    }
    script.push_str(" --env \"TEST_SERIAL_ID=${test_serial_id}\"");
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
    use std::collections::BTreeSet;

    #[test]
    fn batches_node_tests() {
        let ids: Vec<String> = (0..121).map(|i| format!("parallel/test-{i}.js")).collect();
        let jobs = NodeRunner::batch_jobs(ids);
        assert_eq!(jobs.len(), 5);
        assert_eq!(jobs[0].tests.len(), 25);
        assert_eq!(jobs[1].tests.len(), 25);
        assert_eq!(jobs[2].tests.len(), 25);
        assert_eq!(jobs[3].tests.len(), 25);
        assert_eq!(jobs[4].tests.len(), 21);
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
        assert_eq!(selected[0].tests.len(), 25);
        assert_eq!(selected[0].tests[0], "parallel/test-25.js");
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
        assert_eq!(results["parallel/test-pass.js"].status, Status::Pass);
        assert_eq!(results["parallel/test-skip.js"].status, Status::Skip);
        assert_eq!(results["parallel/test-fail.js"].status, Status::Fail);
    }

    #[test]
    fn parses_tap_timeout_results() {
        let dir = tempdir::TempDir::new("node-tap-timeout").unwrap();
        let path = dir.path().join("results.tap");
        fs::write(
            &path,
            "\
TAP version 13
1..1
not ok 1 parallel/test-timeout.js
  ---
  duration_ms: 3065.85500
  severity: fail
  exitcode: 143
  stack: |-
    timeout
  ...
",
        )
        .unwrap();

        let results = parse_tap_results(&path).unwrap();
        assert_eq!(results["parallel/test-timeout.js"].status, Status::Timeout);
    }

    #[test]
    fn normalizes_tap_ids_without_js_suffix() {
        let parsed = BTreeMap::from([("parallel/test-global".to_string(), Status::Pass)]);
        let normalized = normalize_tap_entries(
            parsed
                .into_iter()
                .map(|(id, status)| {
                    (
                        id,
                        TapResult {
                            status,
                            issue: None,
                        },
                    )
                })
                .collect(),
            &["parallel/test-global.js".to_string()],
        );
        assert_eq!(normalized["parallel/test-global.js"].status, Status::Pass);
        assert!(!normalized.contains_key("parallel/test-global"));
    }

    #[test]
    fn parses_tap_crash_issue() {
        let dir = tempdir::TempDir::new("node-tap-crash").unwrap();
        let path = dir.path().join("results.tap");
        fs::write(
            &path,
            "\
TAP version 13
1..1
not ok 1 parallel/test-crash.js
  ---
  duration_ms: 12.34
  severity: fail
  exitcode: 139
  stack: |-
    RuntimeError: out of bounds memory access
        at <unnamed> (<module>[9015]:0xffffffff)
  ...
",
        )
        .unwrap();

        let results = parse_tap_results(&path).unwrap();
        assert_eq!(results["parallel/test-crash.js"].status, Status::Fail);
        assert!(
            results["parallel/test-crash.js"]
                .issue
                .as_ref()
                .is_some_and(|message| message.starts_with("crash: "))
        );
    }

    #[test]
    fn node_crash_issue_ignores_wrapper_only_signal() {
        let issue = node_crash_issue(&CurrentTapResult {
            id: "parallel/test-crash.js".to_string(),
            status: Status::Fail,
            block: vec![
                "not ok 1 parallel/test-crash".to_string(),
                "  ---".to_string(),
                "  stack: |-".to_string(),
                "    AssertionError [ERR_ASSERTION]: guest failure".to_string(),
                "    /tmp/node-wrapper-123.sh: line 12: 79368 Segmentation fault      (core dumped) '/tmp/wasmer' run".to_string(),
                "  ...".to_string(),
            ],
            exit_code: Some(139),
            expect_stack_line: false,
        });

        assert_eq!(issue, None);
    }

    #[test]
    fn does_not_treat_guest_assertion_as_crash() {
        let dir = tempdir::TempDir::new("node-tap-assert").unwrap();
        let path = dir.path().join("results.tap");
        fs::write(
            &path,
            "\
TAP version 13
1..1
not ok 1 parallel/test-assert.js
  ---
  duration_ms: 12.34
  severity: fail
  exitcode: 139
  stack: |-
    node:assert:850
        throw newErr;
        ^
    
    AssertionError [ERR_ASSERTION]: ifError got unwanted exception: command not supported
        at Object.<anonymous> (/tmp/test.js:1:1)
  ...
",
        )
        .unwrap();

        let results = parse_tap_results(&path).unwrap();
        assert_eq!(results["parallel/test-assert.js"].status, Status::Fail);
        assert_eq!(results["parallel/test-assert.js"].issue, None);
    }

    #[test]
    #[ignore = "requires a full local Node log; set NODE_CRASH_AUDIT_LOG or keep test_node.log"]
    fn node_crash_audit_log_matches_actionable_crashes() {
        let path = std::env::var("NODE_CRASH_AUDIT_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_node.log"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read audit log {}: {error}", path.display()));
        let normalized = normalize_node_audit_log(&text);
        let blocks = collect_node_audit_blocks(&normalized);
        let actual: BTreeSet<_> = blocks
            .iter()
            .filter_map(|(id, result)| node_crash_issue(result).map(|issue| (id.clone(), issue)))
            .collect();
        let expected: BTreeSet<_> = blocks
            .iter()
            .filter(|(_, result)| audit_block_expects_actionable_crash(result))
            .map(|(id, _)| id.clone())
            .collect();
        let actual_ids: BTreeSet<_> = actual.iter().map(|(id, _)| id.clone()).collect();

        assert_eq!(
            actual_ids,
            expected,
            "crash extractor output differs from actionable crash audit for {}",
            path.display()
        );
        assert!(
            !actual_ids.is_empty(),
            "expected at least one captured crash"
        );
        assert!(
            actual
                .iter()
                .any(|(_, issue)| issue.contains("RuntimeError:") || issue.contains("panicked at")),
            "expected at least one runtime trap or Rust panic; captured={actual_ids:#?}"
        );
        for id in [
            "async-hooks/test-async-await",
            "parallel/test-http2-session-timeout",
            "parallel/test-tls-write-error",
        ] {
            assert!(
                !actual_ids.contains(id),
                "non-actionable failure was captured as crash for {id}"
            );
        }
        for (id, issue) in actual {
            assert!(
                issue.contains("panicked at")
                    || issue.contains("edgejs/src/")
                    || issue.contains("edgejs/deps/")
                    || issue.contains("Program received fatal signal:")
                    || issue.contains("Program recieved fatal signal:")
                    || issue
                        .contains("[callback trampoline] error calling function: RuntimeError:")
                    || issue.contains("failed with runtime error: RuntimeError:")
                    || issue.contains("RuntimeError: "),
                "captured crash for {id} lacks actionable runtime/native context:\n{issue}"
            );
            assert!(
                !issue.contains("WASI exited with code: ExitCode::0"),
                "wrapper-only WASI exit was captured as crash for {id}:\n{issue}"
            );
            assert!(
                !issue.contains("AssertionError [ERR_ASSERTION]"),
                "guest assertion was captured as crash for {id}:\n{issue}"
            );
        }
    }

    fn normalize_node_audit_log(text: &str) -> String {
        text.lines()
            .map(|line| {
                line.strip_prefix("[stdout] ")
                    .or_else(|| line.strip_prefix("[stderr] "))
                    .unwrap_or(line)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn collect_node_audit_blocks(text: &str) -> Vec<(String, CurrentTapResult)> {
        let mut results = Vec::new();
        let mut current = None;
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if let Some(id) = line.strip_prefix("ok ").and_then(parse_tap_id) {
                flush_node_audit_block(&mut results, &mut current);
                current = Some(CurrentTapResult {
                    id,
                    status: if line.contains(" # skip ") {
                        Status::Skip
                    } else {
                        Status::Pass
                    },
                    block: vec![raw_line.to_string()],
                    exit_code: None,
                    expect_stack_line: false,
                });
                continue;
            }
            if let Some(id) = line.strip_prefix("not ok ").and_then(parse_tap_id) {
                flush_node_audit_block(&mut results, &mut current);
                current = Some(CurrentTapResult {
                    id,
                    status: Status::Fail,
                    block: vec![raw_line.to_string()],
                    exit_code: None,
                    expect_stack_line: false,
                });
                continue;
            }
            if let Some(current) = current.as_mut() {
                current.block.push(raw_line.to_string());
                if current.expect_stack_line {
                    if line == "timeout" {
                        current.status = Status::Timeout;
                    }
                    current.expect_stack_line = false;
                } else if line == "stack: |-" {
                    current.expect_stack_line = true;
                } else if let Some(exit_code) = line
                    .strip_prefix("exitcode: ")
                    .and_then(|value| value.trim().parse().ok())
                {
                    current.exit_code = Some(exit_code);
                }
            }
            if line == "..." {
                flush_node_audit_block(&mut results, &mut current);
            }
        }
        flush_node_audit_block(&mut results, &mut current);
        results
    }

    fn flush_node_audit_block(
        results: &mut Vec<(String, CurrentTapResult)>,
        current: &mut Option<CurrentTapResult>,
    ) {
        if let Some(current) = current.take() {
            results.push((current.id.clone(), current));
        }
    }

    fn audit_block_expects_actionable_crash(result: &CurrentTapResult) -> bool {
        if result.status == Status::Timeout {
            return false;
        }
        let lines: Vec<_> = result.block.iter().map(String::as_str).collect();
        lines.iter().enumerate().any(|(index, line)| {
            line.contains("panicked at")
                || line.contains("has overflowed its stack")
                || line.contains("thread caused non-unwinding panic")
                || line.contains("memory allocation of ")
                || line.contains("thread panicked while processing panic")
                || is_native_assertion_audit_line(line)
                || (is_runtime_trap_audit_header(line)
                    && lines
                        .get(index + 1)
                        .is_some_and(|next| next.trim_start().starts_with("at ")))
        })
    }

    fn is_native_assertion_audit_line(line: &str) -> bool {
        line.contains("Assertion failed:")
            && (line.contains("edgejs/src/")
                || line.contains("/edgejs/")
                || line.contains("edge_runtime_")
                || line.contains("/deps/uv/")
                || line.contains("libuv"))
    }

    fn is_runtime_trap_audit_header(line: &str) -> bool {
        line.trim_start().starts_with("RuntimeError: ")
            || line.contains("failed with runtime error: RuntimeError:")
            || line.contains("error calling function: RuntimeError:")
    }

    #[test]
    fn wrapper_path_is_unique_per_job() {
        let workspace = Workspace {
            output_dir: PathBuf::from("/tmp/out"),
            checkout: PathBuf::from("/tmp/checkout"),
            work_dir: PathBuf::from("/tmp/work"),
        };
        let a = TestJob {
            id: "node-batch-0001".into(),
            tests: vec!["a.js".into()],
        };
        let b = TestJob {
            id: "node-batch-0002".into(),
            tests: vec!["b.js".into()],
        };
        assert_ne!(
            NodeRunner::wrapper_path(&workspace, &a),
            NodeRunner::wrapper_path(&workspace, &b)
        );
        let first = NodeRunner::job_namespace(&a);
        let second = NodeRunner::job_namespace(&a);
        let other_job = NodeRunner::job_namespace(&b);
        assert_ne!(first, second);
        assert_ne!(first, other_job);
        assert!(first.starts_with("compat-"));
        assert!(!first.contains('/'));
    }
}
