use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use anyhow::{Result, bail};

use crate::langs::Status;
use crate::reports::{
    RunMetadata, RunRegressions, load_metadata, load_metadata_at_ref, load_regressions,
    load_status, load_status_at_ref, test_regressions_filename, test_results_filename,
    test_summary_filename,
};

const COMPARE_REF: &str = "origin/main";
const REGRESSION_OUTPUT_LIMIT: usize = 2_000;

#[derive(Clone, Copy)]
struct LangConfig {
    name: &'static str,
    label: &'static str,
}

const LANGS: [LangConfig; 4] = [
    LangConfig {
        name: "python",
        label: "Python",
    },
    LangConfig {
        name: "node",
        label: "Node.js",
    },
    LangConfig {
        name: "php",
        label: "PHP",
    },
    LangConfig {
        name: "rust",
        label: "Rust",
    },
];

pub struct Verdict {
    pub body: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VerdictKind {
    Regression,
    Improvement,
    NoChanges,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Regression,
    Improvement,
    NoChanges,
}

struct LanguageVerdict {
    config: LangConfig,
    total_tests: usize,
    pass_rate_now: f64,
    delta_pass: isize,
    delta_fail: isize,
    delta_timeout: isize,
    delta_crash: isize,
    improvements: Vec<TestChange>,
    regressions: Vec<TestChange>,
    crash_example: Option<CrashExample>,
    failure_example: Option<FailureExample>,
}

#[derive(Clone)]
struct TestChange {
    id: String,
    before: Status,
    after: Status,
}

struct CrashExample {
    repro_command: Option<String>,
    test_source: Option<Link>,
    output: Option<String>,
}

struct FailureExample {
    repro_command: Option<String>,
    test_source: Option<Link>,
    status_before: Status,
    status_after: Status,
    output: Option<String>,
}

struct Link {
    label: String,
    url: String,
}

pub fn build_verdict(
    output_dir: &Path,
    target_sha: &str,
    run_url: &str,
    results_branch: &str,
    results_commit: &str,
) -> Result<Verdict> {
    let languages = LANGS
        .into_iter()
        .map(|lang| collect_language_verdict(output_dir, lang, target_sha))
        .collect::<Result<Vec<_>>>()?;
    let kind = verdict_kind(&languages);
    let body = render_verdict(kind, &languages, run_url, results_branch, results_commit);
    Ok(Verdict { body })
}

fn collect_language_verdict(
    output_dir: &Path,
    config: LangConfig,
    target_sha: &str,
) -> Result<LanguageVerdict> {
    let metadata_path = output_dir.join(test_summary_filename(config.name));
    let regressions_path = output_dir.join(test_regressions_filename(config.name));
    let status_path = output_dir.join(test_results_filename(config.name));
    let metadata = load_metadata(&metadata_path)?;
    ensure_target_sha(config.name, &metadata, target_sha)?;
    let confirmed_regressions = load_regressions(&regressions_path)?;
    let status = load_status(&status_path)?;
    let baseline_metadata = load_metadata_at_ref(output_dir, COMPARE_REF, config.name)?;
    let baseline_status = load_status_at_ref(output_dir, COMPARE_REF, config.name)?;

    let improvements = changed_tests(&baseline_status, &status, is_improvement);
    let regressions = changed_tests(&baseline_status, &status, is_regression);
    let crash_example = first_new_crash(config, &metadata, &baseline_metadata);
    let failure_example = first_failure_example(config, &confirmed_regressions);

    Ok(LanguageVerdict {
        config,
        total_tests: total_tests(&metadata),
        pass_rate_now: pass_rate(&metadata),
        delta_pass: count_delta(&baseline_metadata, &metadata, "PASS"),
        delta_fail: count_delta(&baseline_metadata, &metadata, "FAIL"),
        delta_timeout: count_delta(&baseline_metadata, &metadata, "TIMEOUT"),
        delta_crash: crash_count(&metadata) as isize - crash_count(&baseline_metadata) as isize,
        improvements,
        regressions,
        crash_example,
        failure_example,
    })
}

fn ensure_target_sha(lang: &str, metadata: &RunMetadata, target_sha: &str) -> Result<()> {
    let actual = metadata.wasmer.commit.as_str();
    let prefix = actual.get(..target_sha.len()).unwrap_or("");
    if actual == target_sha || prefix == target_sha {
        return Ok(());
    }
    bail!(
        "{lang} metadata sha mismatch: expected {target_sha}, got {}",
        metadata.wasmer.commit
    )
}

fn changed_tests(
    baseline: &BTreeMap<String, Status>,
    candidate: &BTreeMap<String, Status>,
    keep: fn(Status, Status) -> bool,
) -> Vec<TestChange> {
    baseline
        .iter()
        .filter_map(|(id, before)| {
            let after = candidate.get(id)?;
            keep(*before, *after).then(|| TestChange {
                id: id.clone(),
                before: *before,
                after: *after,
            })
        })
        .collect()
}

fn is_regression(before: Status, after: Status) -> bool {
    before == Status::Pass && after != Status::Pass
}

fn is_improvement(before: Status, after: Status) -> bool {
    before != Status::Pass && after == Status::Pass
}

pub(crate) fn classify_change_kind(
    baseline_status: &BTreeMap<String, Status>,
    candidate_status: &BTreeMap<String, Status>,
    baseline_metadata: &RunMetadata,
    candidate_metadata: &RunMetadata,
) -> ChangeKind {
    if has_new_crash(candidate_metadata, baseline_metadata)
        || !changed_tests(baseline_status, candidate_status, is_regression).is_empty()
    {
        ChangeKind::Regression
    } else if !changed_tests(baseline_status, candidate_status, is_improvement).is_empty() {
        ChangeKind::Improvement
    } else {
        ChangeKind::NoChanges
    }
}

fn verdict_kind(languages: &[LanguageVerdict]) -> VerdictKind {
    if languages
        .iter()
        .any(|lang| lang.crash_example.is_some() || !lang.regressions.is_empty())
    {
        VerdictKind::Regression
    } else if languages.iter().any(|lang| !lang.improvements.is_empty()) {
        VerdictKind::Improvement
    } else {
        VerdictKind::NoChanges
    }
}

fn total_tests(metadata: &RunMetadata) -> usize {
    metadata.counts.values().sum()
}

fn pass_rate(metadata: &RunMetadata) -> f64 {
    let total = total_tests(metadata);
    if total == 0 {
        0.0
    } else {
        metadata.counts.get("PASS").copied().unwrap_or(0) as f64 * 100.0 / total as f64
    }
}

fn count_delta(before: &RunMetadata, after: &RunMetadata, key: &str) -> isize {
    after.counts.get(key).copied().unwrap_or(0) as isize
        - before.counts.get(key).copied().unwrap_or(0) as isize
}

fn crash_count(metadata: &RunMetadata) -> usize {
    metadata.crashes.len()
}

fn first_new_crash(
    config: LangConfig,
    candidate: &RunMetadata,
    baseline: &RunMetadata,
) -> Option<CrashExample> {
    candidate
        .crashes
        .iter()
        .find(|(job_id, message)| baseline.crashes.get(*job_id) != Some(*message))
        .map(|(job_id, message)| CrashExample {
            repro_command: Some(format!(
                "shield run --lang {} --wasmer [WASMER BINARY] {}",
                config.name, job_id
            )),
            test_source: None,
            output: Some(message.clone()),
        })
}

fn has_new_crash(candidate: &RunMetadata, baseline: &RunMetadata) -> bool {
    candidate
        .crashes
        .iter()
        .any(|(job_id, message)| baseline.crashes.get(job_id) != Some(message))
}

fn first_failure_example(
    config: LangConfig,
    regressions: &RunRegressions,
) -> Option<FailureExample> {
    let regression = regressions
        .regressions
        .iter()
        .min_by_key(|regression| regression.output.len())?;
    Some(FailureExample {
        repro_command: Some(format!(
            "shield run --lang {} --wasmer [WASMER BINARY] {}",
            config.name, regression.id
        )),
        test_source: None,
        status_before: regression.status_before,
        status_after: regression.status_after,
        output: Some(truncate_regression_output(&regression.output)),
    })
}

fn truncate_regression_output(output: &str) -> String {
    if output.chars().count() <= REGRESSION_OUTPUT_LIMIT {
        return output.to_string();
    }
    let mut truncated = output
        .chars()
        .take(REGRESSION_OUTPUT_LIMIT)
        .collect::<String>();
    truncated.push_str("\n[output truncated]");
    truncated
}

fn render_verdict(
    kind: VerdictKind,
    languages: &[LanguageVerdict],
    run_url: &str,
    results_branch: &str,
    results_commit: &str,
) -> String {
    let mut body = String::new();
    match kind {
        VerdictKind::Regression => render_regression(
            &mut body,
            languages,
            run_url,
            results_branch,
            results_commit,
        ),
        VerdictKind::Improvement => render_improvement(
            &mut body,
            languages,
            run_url,
            results_branch,
            results_commit,
        ),
        VerdictKind::NoChanges => render_no_changes(
            &mut body,
            languages,
            run_url,
            results_branch,
            results_commit,
        ),
    }
    body
}

fn render_regression(
    body: &mut String,
    languages: &[LanguageVerdict],
    run_url: &str,
    results_branch: &str,
    results_commit: &str,
) {
    let _ = writeln!(body, "# Shield - Regression 💩💩💩");
    let _ = writeln!(body);
    render_table(body, languages);

    for lang in languages {
        if let Some(crash) = &lang.crash_example {
            let _ = writeln!(body);
            let _ = writeln!(body, "### Example crash from {}", lang.config.label);
            let _ = writeln!(body);
            if let Some(repro) = &crash.repro_command {
                let _ = writeln!(body, "- Repro command: `{repro}`");
            }
            if let Some(source) = &crash.test_source {
                let _ = writeln!(
                    body,
                    "- Test source: {}",
                    markdown_link(&source.label, &source.url)
                );
            }
            let _ = writeln!(
                body,
                "- Full status file: {}",
                markdown_link(
                    &test_results_filename(lang.config.name),
                    &test_results_url(lang.config.name, results_branch, results_commit)
                )
            );
            if let Some(output) = &crash.output {
                let _ = writeln!(body);
                let _ = writeln!(body, "```text");
                let _ = writeln!(body, "{}", output.trim_end());
                let _ = writeln!(body, "```");
            }
        }
    }

    for lang in languages {
        if let Some(example) = &lang.failure_example {
            let _ = writeln!(body);
            let _ = writeln!(body, "### Example failed test from {}", lang.config.label);
            let _ = writeln!(body);
            if let Some(repro) = &example.repro_command {
                let _ = writeln!(body, "- Repro command: `{repro}`");
            }
            if let Some(source) = &example.test_source {
                let _ = writeln!(
                    body,
                    "- Test source: {}",
                    markdown_link(&source.label, &source.url)
                );
            }
            let _ = writeln!(
                body,
                "- Status: `{} -> {}`",
                example.status_before, example.status_after
            );
            let _ = writeln!(
                body,
                "- Full status file: {}",
                markdown_link(
                    &test_results_filename(lang.config.name),
                    &test_results_url(lang.config.name, results_branch, results_commit)
                )
            );
            if let Some(output) = &example.output {
                let _ = writeln!(body);
                let _ = writeln!(body, "```text");
                let _ = writeln!(body, "{}", output.trim_end());
                let _ = writeln!(body, "```");
            }
        }
    }

    render_more_changed_tests(body, languages, results_branch, results_commit);
    render_install(body);
    render_artifacts(body, run_url, results_branch, results_commit);
}

fn render_improvement(
    body: &mut String,
    languages: &[LanguageVerdict],
    run_url: &str,
    results_branch: &str,
    results_commit: &str,
) {
    let _ = writeln!(body, "# Shield - Improvement 🎉🎉🎉");
    let _ = writeln!(body);
    render_table(body, languages);
    let _ = writeln!(body);

    for lang in languages
        .iter()
        .filter(|lang| !lang.improvements.is_empty())
    {
        let _ = writeln!(
            body,
            "- Examples from {}:",
            markdown_link(
                &test_results_filename(lang.config.name),
                &test_results_url(lang.config.name, results_branch, results_commit)
            )
        );
        for change in lang.improvements.iter().take(5) {
            let _ = writeln!(
                body,
                "  - `{}` (`{} -> {}`)",
                change.id, change.before, change.after
            );
        }
        let _ = writeln!(body);
    }

    render_artifacts(body, run_url, results_branch, results_commit);
}

fn render_no_changes(
    body: &mut String,
    languages: &[LanguageVerdict],
    run_url: &str,
    results_branch: &str,
    results_commit: &str,
) {
    let _ = writeln!(body, "# Shield - No changes");
    let _ = writeln!(body);
    render_table(body, languages);
    let _ = writeln!(body);
    render_artifacts(body, run_url, results_branch, results_commit);
}

fn render_table(body: &mut String, languages: &[LanguageVerdict]) {
    let _ = writeln!(
        body,
        "| Language | Tests  | Pass rate now | PASS | FAIL | TIMEOUT | CRASH |"
    );
    let _ = writeln!(
        body,
        "| -------- | ------ | ------------- | ---- | ---- | ------- | ----- |"
    );
    for lang in languages {
        let _ = writeln!(
            body,
            "| {} | {} | {:.1}% | {} | {} | {} | {} |",
            lang.config.label,
            format_usize(lang.total_tests),
            lang.pass_rate_now,
            color_delta(DeltaKind::Pass, lang.delta_pass),
            color_delta(DeltaKind::Fail, lang.delta_fail),
            color_delta(DeltaKind::Timeout, lang.delta_timeout),
            color_delta(DeltaKind::Crash, lang.delta_crash),
        );
    }
}

fn render_more_changed_tests(
    body: &mut String,
    languages: &[LanguageVerdict],
    results_branch: &str,
    results_commit: &str,
) {
    let _ = writeln!(body);
    let _ = writeln!(body, "### More changed tests");
    let _ = writeln!(body);
    for lang in languages {
        let _ = writeln!(
            body,
            "- {}: {}",
            lang.config.label,
            markdown_link(
                &test_results_filename(lang.config.name),
                &test_results_url(lang.config.name, results_branch, results_commit)
            )
        );
    }
}

fn render_install(body: &mut String) {
    let _ = writeln!(body);
    let _ = writeln!(body, "## Install shield");
    let _ = writeln!(body);
    let _ = writeln!(
        body,
        "- `git clone https://github.com/wasmerio/compat-tests.git`"
    );
    let _ = writeln!(body, "- `cd compat-tests`");
    let _ = writeln!(body, "- `cargo build`");
    let _ = writeln!(
        body,
        "- `./target/debug/shield run --lang <LANG> --wasmer [WASMER BINARY] <TEST OR BATCH>`"
    );
}

fn render_artifacts(body: &mut String, run_url: &str, results_branch: &str, results_commit: &str) {
    let _ = writeln!(body);
    let _ = writeln!(body, "## Artifacts");
    let _ = writeln!(body);
    let _ = writeln!(body, "- GitHub Action: {}", markdown_link(run_url, run_url));
    if !results_commit.is_empty() {
        let url = format!("https://github.com/wasmerio/compat-tests/commit/{results_commit}");
        let _ = writeln!(body, "- Results commit: {}", markdown_link(&url, &url));
    } else if !results_branch.is_empty() {
        let url = format!("https://github.com/wasmerio/compat-tests/tree/{results_branch}");
        let _ = writeln!(body, "- Results branch: {}", markdown_link(&url, &url));
    }
}

fn test_results_url(lang: &str, results_branch: &str, results_commit: &str) -> String {
    if !results_commit.is_empty() {
        format!(
            "https://github.com/wasmerio/compat-tests/blob/{results_commit}/{}",
            test_results_filename(lang)
        )
    } else {
        format!(
            "https://github.com/wasmerio/compat-tests/blob/{results_branch}/{}",
            test_results_filename(lang)
        )
    }
}

fn markdown_link(label: &str, url: &str) -> String {
    format!("[{label}]({url})")
}

fn format_usize(value: usize) -> String {
    let s = value.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

enum DeltaKind {
    Pass,
    Fail,
    Timeout,
    Crash,
}

fn color_delta(kind: DeltaKind, delta: isize) -> String {
    if delta == 0 {
        return "0".to_string();
    }
    let good = match kind {
        DeltaKind::Pass => delta > 0,
        DeltaKind::Fail | DeltaKind::Timeout | DeltaKind::Crash => delta < 0,
    };
    let color = if good { "green" } else { "red" };
    format!("$${{\\color{{{}}}{:+}}}$$", color, delta)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_verdict_regression() {
        let rendered = render_verdict(
            VerdictKind::Regression,
            &regression_languages(),
            "https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID",
            "pr-123",
            "RESULTS_COMMIT_SHA",
        );
        let snapshot = read_snapshot("test_verdict_regression.md", &rendered);
        assert_eq!(rendered, snapshot);
    }

    #[test]
    fn test_verdict_improvements() {
        let rendered = render_verdict(
            VerdictKind::Improvement,
            &improvement_languages(),
            "https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID",
            "pr-123",
            "RESULTS_COMMIT_SHA",
        );
        let snapshot = read_snapshot("test_verdict_improvements.md", &rendered);
        assert_eq!(rendered, snapshot);
    }

    fn read_snapshot(name: &str, rendered: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            fs::write(&path, rendered).expect("write snapshot");
        }
        fs::read_to_string(path).expect("read snapshot")
    }

    fn improvement_languages() -> Vec<LanguageVerdict> {
        vec![
            LanguageVerdict {
                config: LANGS[0],
                total_tests: 37_907,
                pass_rate_now: 75.8,
                delta_pass: 435,
                delta_fail: -102,
                delta_timeout: -788,
                delta_crash: 0,
                improvements: vec![
                    change(
                        "test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later",
                        Status::Timeout,
                        Status::Pass,
                    ),
                    change(
                        "test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later_negative_delays",
                        Status::Timeout,
                        Status::Pass,
                    ),
                    change(
                        "test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_soon",
                        Status::Timeout,
                        Status::Pass,
                    ),
                    change(
                        "test.test_docxmlrpc.DocXMLRPCHTTPGETServer.test_get_css",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "test.test_docxmlrpc.DocXMLRPCHTTPGETServer.test_invalid_get_response",
                        Status::Fail,
                        Status::Pass,
                    ),
                ],
                regressions: vec![],
                crash_example: None,
                failure_example: None,
            },
            LanguageVerdict {
                config: LANGS[1],
                total_tests: 16_030,
                pass_rate_now: 51.2,
                delta_pass: 13,
                delta_fail: -11,
                delta_timeout: -2,
                delta_crash: 0,
                improvements: vec![
                    change("parallel/test-fs-stat.js", Status::Fail, Status::Pass),
                    change(
                        "parallel/test-fs-symlink-dir-junction-relative.js",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "parallel/test-stream2-httpclient-response-end.js",
                        Status::Timeout,
                        Status::Pass,
                    ),
                    change(
                        "parallel/test-http2-server-destroy-before-write.js",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "parallel/test-whatwg-url-custom-searchparams-stringifier.js",
                        Status::Timeout,
                        Status::Pass,
                    ),
                ],
                regressions: vec![],
                crash_example: None,
                failure_example: None,
            },
            LanguageVerdict {
                config: LANGS[2],
                total_tests: 19_636,
                pass_rate_now: 72.8,
                delta_pass: 3,
                delta_fail: -3,
                delta_timeout: 0,
                delta_crash: 0,
                improvements: vec![
                    change(
                        "ext/standard/tests/strings/trim_basic.phpt",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "ext/standard/tests/strings/strval_basic.phpt",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "ext/standard/tests/file/stream_copy_to_stream_empty.phpt",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "ext/standard/tests/file/statpage.phpt",
                        Status::Timeout,
                        Status::Pass,
                    ),
                    change(
                        "ext/standard/tests/file/stream_supports_lock.phpt",
                        Status::Fail,
                        Status::Pass,
                    ),
                ],
                regressions: vec![],
                crash_example: None,
                failure_example: None,
            },
            LanguageVerdict {
                config: LANGS[3],
                total_tests: 15_421,
                pass_rate_now: 84.9,
                delta_pass: 2,
                delta_fail: -2,
                delta_timeout: 0,
                delta_crash: 0,
                improvements: vec![
                    change(
                        "env::home_dir_with_relative_input",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "fs::canonicalize_handles_symlink_loop",
                        Status::Fail,
                        Status::Pass,
                    ),
                    change(
                        "process::command_preserves_exit_code",
                        Status::Timeout,
                        Status::Pass,
                    ),
                    change("net::tcp_listener_reuseaddr", Status::Fail, Status::Pass),
                    change(
                        "path::strip_prefix_handles_root",
                        Status::Fail,
                        Status::Pass,
                    ),
                ],
                regressions: vec![],
                crash_example: None,
                failure_example: None,
            },
        ]
    }

    fn regression_languages() -> Vec<LanguageVerdict> {
        vec![
            LanguageVerdict {
                config: LANGS[0],
                total_tests: 37_906,
                pass_rate_now: 75.7,
                delta_pass: -10,
                delta_fail: 7,
                delta_timeout: 3,
                delta_crash: 0,
                improvements: vec![],
                regressions: vec![change(
                    "test.test_shutil.TestMove.test_move_symlink_to_file",
                    Status::Pass,
                    Status::Fail,
                )],
                crash_example: None,
                failure_example: Some(FailureExample {
                    repro_command: Some(
                        "shield run --lang python --wasmer [WASMER BINARY] test.test_shutil.TestMove.test_move_symlink_to_file"
                            .to_string(),
                    ),
                    test_source: Some(link(
                        "test_shutil.py",
                        "https://github.com/python/cpython/blob/main/Lib/test/test_shutil.py",
                    )),
                    status_before: Status::Pass,
                    status_after: Status::Fail,
                    output: Some(
                        "======================================================================\nFAIL: test_move_symlink_to_file (test.test_shutil.TestMove)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File \"/usr/lib/python3.11/test/test_shutil.py\", line 412, in test_move_symlink_to_file\n    self.assertTrue(os.path.islink(dst))\nAssertionError: False is not true"
                            .to_string(),
                    ),
                }),
            },
            LanguageVerdict {
                config: LANGS[1],
                total_tests: 16_024,
                pass_rate_now: 51.1,
                delta_pass: -2,
                delta_fail: 1,
                delta_timeout: 1,
                delta_crash: 0,
                improvements: vec![],
                regressions: vec![change(
                    "parallel/test-fs-symlink.js",
                    Status::Pass,
                    Status::Fail,
                )],
                crash_example: None,
                failure_example: Some(FailureExample {
                    repro_command: Some(
                        "shield run --lang node --wasmer [WASMER BINARY] parallel/test-fs-symlink.js"
                            .to_string(),
                    ),
                    test_source: Some(link(
                        "test-fs-symlink.js",
                        "https://github.com/nodejs/node/blob/main/test/parallel/test-fs-symlink.js",
                    )),
                    status_before: Status::Pass,
                    status_after: Status::Fail,
                    output: Some(
                        "AssertionError [ERR_ASSERTION]: expected symbolic link to exist\n    at testValidSymLink (/node/test/parallel/test-fs-symlink.js:81:10)\n    at process.processTicksAndRejections (node:internal/process/task_queues:95:5)"
                            .to_string(),
                    ),
                }),
            },
            LanguageVerdict {
                config: LANGS[2],
                total_tests: 19_636,
                pass_rate_now: 72.8,
                delta_pass: -96,
                delta_fail: 106,
                delta_timeout: -10,
                delta_crash: 3,
                improvements: vec![],
                regressions: vec![change(
                    "ext/standard/tests/file/rename_variation5.phpt",
                    Status::Pass,
                    Status::Fail,
                )],
                crash_example: Some(CrashExample {
                    repro_command: Some(
                        "shield run --lang php --wasmer [WASMER BINARY] php-batch-0316"
                            .to_string(),
                    ),
                    test_source: Some(link(
                        "rename_variation5.phpt",
                        "https://github.com/php/php-src/blob/master/ext/standard/tests/file/rename_variation5.phpt",
                    )),
                    output: Some(
                        "rust panic: thread 'TokioTaskManager Thread Pool_thread_6' panicked at\nlib/wasix/src/syscalls/wasi/path_rename.rs:285:10:\nExpected target inode to exist, and it's too late to safely fail: Errno::noent\n\nstack backtrace:\n   0: __rustc::rust_begin_unwind\n   1: core::panicking::panic_fmt\n   2: core::result::unwrap_failed\n   3: wasmer_wasix::syscalls::wasi::path_rename::path_rename_internal\n   4: wasmer_wasix::syscalls::wasi::path_rename::path_rename\n   5: corosensei::coroutine::on_stack::wrapper\n   6: stack_call_trampoline\n   7: wasmer_vm::trap::traphandlers::on_host_stack\n\njob: php-batch-0316"
                            .to_string(),
                    ),
                }),
                failure_example: None,
            },
            LanguageVerdict {
                config: LANGS[3],
                total_tests: 15_423,
                pass_rate_now: 84.8,
                delta_pass: 0,
                delta_fail: 0,
                delta_timeout: 0,
                delta_crash: 0,
                improvements: vec![],
                regressions: vec![],
                crash_example: None,
                failure_example: None,
            },
        ]
    }

    fn change(id: &str, before: Status, after: Status) -> TestChange {
        TestChange {
            id: id.to_string(),
            before,
            after,
        }
    }

    fn link(label: &str, url: &str) -> Link {
        Link {
            label: label.to_string(),
            url: url.to_string(),
        }
    }
}
