use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use super::{LangRunner, Mode, RunnerOpts, Status, TestJob, TestResult, TestRunOutput, Workspace};
use crate::process::{ProcessError, write_stream};
use crate::run_log::RunLog;
use crate::runtime::{RunSpec, RunTarget, WasmerRuntime};

const GUEST_PHP_BIN: &str = "/bin/php";

const SKIPPED_TESTS: &[&str] = &[
    // TODO(https://github.com/wasmerio/wasmer/issues/6522): getrusage() returns
    // invalid ru_utime.tv_usec values under WASIX, so this test randomly passes
    // or fails depending on whether the bogus value is positive or negative.
    "ext/standard/tests/general_functions/getrusage_basic.phpt",
    // TODO(https://github.com/wasmerio/wasmer/issues/6530): ftruncate() on mounted
    // host volumes can report success while leaving stale file contents behind.
    "ext/spl/tests/SplFileObject/fileobject_005.phpt",
    // TODO(https://github.com/wasmerio/wasmer/issues/6530): ftruncate() followed by
    // vfprintf() on a mounted host volume can panic in virtual-fs mem_fs writes.
    "ext/standard/tests/strings/vfprintf_variation1.phpt",
];

pub struct PhpRunner;

impl PhpRunner {
    const BATCH_SIZE: usize = 50;

    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "php",
        git_repo: "https://github.com/wasix-org/php.git",
        git_ref: "6dd6dd1c7e409b8e9dcba8a8d6f9b7b5f944cc9e",
        wasmer_package: Some("artembde9fd8b1a18420e/php-32-debug@8.3.2104"),
        wasmer_package_warmup_args: Some(&["-r", "echo \"ok\\n\";"]),
        wasmer_flags: &[],
        docker_compose: None,
    };

    fn job_run_dir(workspace: &Workspace, job: &TestJob) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        job.id.hash(&mut hasher);
        job.tests.hash(&mut hasher);
        workspace
            .work_dir
            .join(format!("php-job-{:016x}", hasher.finish()))
    }

    fn result_file(workspace: &Workspace, job: &TestJob) -> PathBuf {
        Self::job_run_dir(workspace, job).join("results.tsv")
    }

    fn run_tests_path(workspace: &Workspace) -> PathBuf {
        workspace.checkout.join("run-tests.php")
    }

    fn test_path(workspace: &Workspace, id: &str) -> PathBuf {
        workspace.checkout.join(id)
    }

    // TODO: That is common for all langs - move to higher levels
    fn volume_flags(workspace: &Workspace, job: &TestJob) -> Vec<String> {
        let job_run_dir = Self::job_run_dir(workspace, job);
        vec![
            "--volume".into(),
            format!(
                "{}:{}",
                workspace.checkout.display(),
                workspace.checkout.display()
            ),
            "--volume".into(),
            format!("{}:{}", job_run_dir.display(), job_run_dir.display()),
            "--env".into(),
            format!(
                "TEST_PHP_INFO_FILE={}",
                job_run_dir.join("run-test-info.php").display()
            ),
        ]
    }

    fn parse_results(source_root: &Path, result_file: &Path) -> Result<Vec<TestResult>> {
        if !result_file.exists() {
            return Ok(Vec::new());
        }
        let mut by_id = BTreeMap::new();
        for line in fs::read_to_string(result_file)?.lines() {
            let (status, name) = line
                .split_once('\t')
                .ok_or_else(|| anyhow!("invalid php result line: {line:?}"))?;
            by_id.insert(
                normalize_test_name(source_root, name),
                map_php_status(status),
            );
        }
        Ok(by_id
            .into_iter()
            .map(|(id, status)| TestResult { id, status })
            .collect())
    }

    fn run_one(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        mode: Mode,
    ) -> Result<Vec<TestResult>> {
        let test_paths: Vec<PathBuf> = job
            .tests
            .iter()
            .map(|id| Self::test_path(workspace, id))
            .collect();
        for test_path in &test_paths {
            if !test_path.is_file() {
                bail!("php test not found: {}", test_path.display());
            }
        }
        let job_run_dir = Self::job_run_dir(workspace, job);
        fs::create_dir_all(&job_run_dir)?;
        let result_file = Self::result_file(workspace, job);
        let _ = fs::remove_file(&result_file);
        let mut args = vec![
            "-d".into(),
            "error_reporting=E_ALL & ~E_DEPRECATED".into(),
            Self::run_tests_path(workspace).display().to_string(),
            "-q".into(),
            "-n".into(),
            "-p".into(),
            GUEST_PHP_BIN.into(),
            "-W".into(),
            result_file.display().to_string(),
        ];
        args.extend(test_paths.iter().map(|path| path.display().to_string()));

        let result = wasmer.run(
            RunSpec {
                target: RunTarget::Package(
                    Self::OPTS.wasmer_package.expect("php package").to_string(),
                ),
                flags: Self::volume_flags(workspace, job),
                args,
                timeout: None,
            },
            |stream, line| {
                if matches!(mode, Mode::Debug) {
                    write_stream(stream, line)?;
                }
                Ok(())
            },
        );

        let parsed = Self::parse_results(&workspace.checkout, &result_file)?;
        let has_parsed_results = !parsed.is_empty();
        let fallback = match result {
            Ok(()) => Status::Fail,
            Err(ProcessError::Timeout(_)) => Status::Timeout,
            Err(ProcessError::AbnormalExit(message)) if !has_parsed_results => {
                return Err(anyhow!(ProcessError::AbnormalExit(message)));
            }
            Err(ProcessError::AbnormalExit(_)) => Status::Fail,
            Err(ProcessError::RustCrash(message)) => {
                return Err(anyhow!(ProcessError::RustCrash(message)));
            }
            Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
        };
        let mut by_id = BTreeMap::new();
        for result in parsed {
            by_id.insert(result.id, result.status);
        }
        Ok(job
            .tests
            .iter()
            .cloned()
            .chain(by_id.keys().filter(|id| !job.tests.contains(*id)).cloned())
            .map(|id| TestResult {
                status: by_id.get(&id).copied().unwrap_or(fallback),
                id,
            })
            .collect())
    }

    fn batch_jobs(ids: Vec<String>) -> Vec<TestJob> {
        ids.chunks(Self::BATCH_SIZE)
            .enumerate()
            .map(|(index, chunk)| TestJob {
                id: format!("php-batch-{index:04}"),
                tests: chunk.to_vec(),
            })
            .collect()
    }

    fn batch_filter(filter: &str) -> Option<usize> {
        filter
            .strip_prefix("php-batch-")
            .and_then(|index| index.parse().ok())
    }

    fn should_skip_test(id: &str) -> bool {
        SKIPPED_TESTS.contains(&id)
    }
}

impl LangRunner for PhpRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
    }

    fn prepare(
        &self,
        workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        _jobs: &[TestJob],
    ) -> Result<()> {
        patch_php_runtests_worker_putenv(&workspace.checkout)?;
        patch_php_runtests_guest_exec(&workspace.checkout)
    }

    fn discover(
        &self,
        workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        filter: Option<&str>,
        _mode: Mode,
    ) -> Result<Vec<TestJob>> {
        tracing::info!("discovering php tests");
        let mut tests = Vec::new();
        collect_phpt(&workspace.checkout, &workspace.checkout, &mut tests)?;
        tests.retain(|id| !Self::should_skip_test(id));
        tests.sort();
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
            "discovered php tests"
        );
        Ok(jobs)
    }

    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        mode: Mode,
        _log: Option<&RunLog>,
    ) -> Result<TestRunOutput> {
        Ok(TestRunOutput {
            results: self.run_one(workspace, wasmer, job, mode)?,
            issues: vec![],
        })
    }
}

fn collect_phpt(root: &Path, dir: &Path, tests: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_phpt(root, &path, tests)?;
        } else if path.extension().is_some_and(|ext| ext == "phpt") {
            tests.push(
                path.strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}

fn map_php_status(status: &str) -> Status {
    match status.trim() {
        "PASSED" => Status::Pass,
        "SKIPPED" => Status::Skip,
        "BORKED" => Status::Fail,
        _ => Status::Fail,
    }
}

fn normalize_test_name(source_root: &Path, name: &str) -> String {
    let name = name.trim();
    if let Some((redirector, redirected)) = name.split_once(": ")
        && let Some(redirector) = redirector.strip_prefix("# ")
    {
        return format!(
            "# {}: {}",
            normalize_test_path(source_root, redirector),
            normalize_test_path(source_root, redirected)
        );
    }
    normalize_test_path(source_root, name)
}

fn normalize_test_path(source_root: &Path, name: &str) -> String {
    let path = Path::new(name.trim());
    if let Ok(rel) = path.strip_prefix(source_root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path.to_string_lossy().replace('\\', "/")
}

fn replace_once(text: &mut String, needle: &str, replacement: &str, what: &str) -> Result<()> {
    if !text.contains(needle) {
        bail!("could not patch php run-tests.php for {what}");
    }
    *text = text.replacen(needle, replacement, 1);
    Ok(())
}

fn replace_if_present(text: &mut String, needle: &str, replacement: &str) {
    if text.contains(needle) {
        *text = text.replacen(needle, replacement, 1);
    }
}

fn patch_php_runtests_worker_putenv(checkout: &Path) -> Result<()> {
    let path = checkout.join("run-tests.php");
    let mut text = fs::read_to_string(&path)?;
    let marker = "compat-tests: sync getenv() for workers";
    if text.contains(marker) {
        return Ok(());
    }
    let needle = r#"    foreach ($greeting["GLOBALS"] as $var => $value) {
        if ($var !== "workerID" && $var !== "workerSock" && $var !== "GLOBALS") {
            $GLOBALS[$var] = $value;
        }
    }
    foreach ($greeting["constants"] as $const => $value) {
        define($const, $value);
    }"#;
    let insert = r#"    foreach ($greeting["GLOBALS"] as $var => $value) {
        if ($var !== "workerID" && $var !== "workerSock" && $var !== "GLOBALS") {
            $GLOBALS[$var] = $value;
        }
    }
    // compat-tests: sync getenv() for workers (TEST_PHP_EXECUTABLE_ESCAPED etc. for SKIPIF).
    if (!empty($GLOBALS['environment']) && is_array($GLOBALS['environment'])) {
        foreach ($GLOBALS['environment'] as $__ct_k => $__ct_v) {
            if (is_string($__ct_k) && (is_string($__ct_v) || is_int($__ct_v) || is_float($__ct_v))) {
                putenv($__ct_k . '=' . $__ct_v);
            }
        }
    }
    foreach ($greeting["constants"] as $const => $value) {
        define($const, $value);
    }"#;
    replace_once(&mut text, needle, insert, "worker getenv sync")?;
    fs::write(path, text)?;
    Ok(())
}

fn patch_php_runtests_guest_exec(checkout: &Path) -> Result<()> {
    let path = checkout.join("run-tests.php");
    let mut text = fs::read_to_string(&path)?;
    let marker = "compat-tests: guest direct command helpers";
    if !text.contains(marker) {
        replace_once(
            &mut text,
            "function compute_summary(): void",
            r#"// compat-tests: guest direct command helpers.
function compat_split_command(string $command): array
{
    $args = [];
    $current = '';
    $quote = null;
    $length = strlen($command);

    for ($i = 0; $i < $length; $i++) {
        $ch = $command[$i];
        if ($quote === null) {
            if (ctype_space($ch)) {
                if ($current !== '') {
                    $args[] = $current;
                    $current = '';
                }
                continue;
            }
            if ($ch === '"' || $ch === "'") {
                $quote = $ch;
                continue;
            }
            if ($ch === '\\' && $i + 1 < $length) {
                $current .= $command[++$i];
                continue;
            }
            $current .= $ch;
            continue;
        }
        if ($quote === '"' && $ch === '\\' && $i + 1 < $length) {
            $current .= $command[++$i];
            continue;
        }
        if ($ch === $quote) {
            $quote = null;
            continue;
        }
        $current .= $ch;
    }

    if ($current !== '') {
        $args[] = $current;
    }

    return $args;
}

function compat_command($command): array
{
    return is_array($command) ? $command : compat_split_command($command);
}

function compat_command_key($command): string
{
    return implode("\0", compat_command($command));
}

function compat_command_display($command): string
{
    return implode(' ', array_map('escapeshellarg', compat_command($command)));
}

function compat_build_php_command(string $php, string ...$parts): array
{
    $command = [$php];
    foreach ($parts as $part) {
        if ($part !== '') {
            array_push($command, ...compat_split_command($part));
        }
    }
    return $command;
}

function compat_shell_exec($command, ?array $env = null)
{
    return system_with_timeout(compat_command($command), $env ?? [], null, false, true, true);
}

function compute_summary(): void"#,
            "guest helpers",
        )?;
    }

    replace_if_present(&mut text, "    string $commandline,", "    $commandline,");
    replace_if_present(
        &mut text,
        r#"    $php_info = shell_exec("$php_escaped $pass_options $info_params $no_file_cache \"$info_file\"");"#,
        r#"    $php_info = compat_shell_exec(array_merge(
        compat_build_php_command($php, $pass_options, $info_params, $no_file_cache),
        [$info_file]
    ));"#,
    );
    replace_if_present(
        &mut text,
        r#"    define('TESTED_PHP_VERSION', shell_exec("$php_escaped -n -r \"echo PHP_VERSION;\""));"#,
        r#"    define('TESTED_PHP_VERSION', compat_shell_exec([$php, '-n', '-r', 'echo PHP_VERSION;']));"#,
    );
    replace_if_present(
        &mut text,
        r#"        $php_info_cgi = shell_exec("$php_cgi_escaped $pass_options $info_params $no_file_cache -q \"$info_file\"");"#,
        r#"        $php_info_cgi = compat_shell_exec(array_merge(
            compat_build_php_command($php_cgi, $pass_options, $info_params, $no_file_cache, '-q'),
            [$info_file]
        ));"#,
    );
    replace_if_present(
        &mut text,
        r#"        $phpdbg_info = shell_exec("$phpdbg_escaped $pass_options $info_params $no_file_cache -qrr \"$info_file\"");"#,
        r#"        $phpdbg_info = compat_shell_exec(array_merge(
            compat_build_php_command($phpdbg, $pass_options, $info_params, $no_file_cache, '-qrr'),
            [$info_file]
        ));"#,
    );
    replace_if_present(
        &mut text,
        r#"    $extensionsNames = explode(',', shell_exec("$php_escaped $pass_options $info_params $no_file_cache \"$info_file\""));"#,
        r#"    $extensionsNames = explode(',', (string) compat_shell_exec(array_merge(
        compat_build_php_command($php, $pass_options, $info_params, $no_file_cache),
        [$info_file]
    )));"#,
    );
    replace_if_present(
        &mut text,
        r#"    $info_file = __DIR__ . '/run-test-info.php';"#,
        r#"    $info_file = getenv('TEST_PHP_INFO_FILE') ?: (__DIR__ . '/run-test-info.php');"#,
    );
    replace_if_present(
        &mut text,
        r#"        $key = "$php => $dir";"#,
        r#"        $key = compat_command_key($php) . " => $dir";"#,
    );
    replace_if_present(
        &mut text,
        r#"        $result = trim(system_with_timeout("$php \"$checkFile\"", $env));"#,
        r#"        $result = trim((string) system_with_timeout(array_merge(compat_command($php), [$checkFile]), $env));"#,
    );
    replace_if_present(
        &mut text,
        r#"    public function checkSkip(string $php, string $code, string $checkFile, string $tempFile, array $env): string"#,
        r#"    public function checkSkip($php, string $code, string $checkFile, string $tempFile, array $env): string"#,
    );
    replace_if_present(
        &mut text,
        r#"    public function getExtensions(string $php): array
    {
        if (isset($this->extensions[$php])) {
            $this->extHits++;
            return $this->extensions[$php];
        }"#,
        r#"    public function getExtensions($php): array
    {
        $php_key = compat_command_key($php);
        if (isset($this->extensions[$php_key])) {
            $this->extHits++;
            return $this->extensions[$php_key];
        }"#,
    );
    replace_if_present(
        &mut text,
        r#"        $extDir = shell_exec("$php -d display_errors=0 -r \"echo ini_get('extension_dir');\"");"#,
        r#"        $extDir = compat_shell_exec(array_merge(compat_command($php), ['-d', 'display_errors=0', '-r', 'echo ini_get(\'extension_dir\');']));"#,
    );
    replace_if_present(
        &mut text,
        r#"        $extensionsNames = explode(",", shell_exec("$php -d display_errors=0 -r \"echo implode(',', get_loaded_extensions());\""));"#,
        r#"        $extensionsNames = explode(",", (string) compat_shell_exec(array_merge(compat_command($php), ['-d', 'display_errors=0', '-r', 'echo implode(\',\', get_loaded_extensions());'])));"#,
    );
    replace_if_present(
        &mut text,
        r#"        $result = [$extDir, $extensions];
        $this->extensions[$php] = $result;
        $this->extMisses++;"#,
        r#"        $result = [$extDir, $extensions];
        $this->extensions[$php_key] = $result;
        $this->extMisses++;"#,
    );
    replace_if_present(
        &mut text,
        r#"        [$ext_dir, $loaded] = $skipCache->getExtensions("$orig_php $pass_options $extra_options $ext_params $no_file_cache");"#,
        r#"        [$ext_dir, $loaded] = $skipCache->getExtensions(
            compat_build_php_command($orig_php, $pass_options, $extra_options, $ext_params, $no_file_cache)
        );"#,
    );
    replace_if_present(
        &mut text,
        r#"        $commandLine = "$extra $php $pass_options $extra_options -q $orig_ini_settings $no_file_cache -d display_errors=1 -d display_startup_errors=0";"#,
        r#"        $commandLine = compat_build_php_command(
            $orig_php,
            $pass_options,
            $extra_options,
            '-q',
            $orig_ini_settings,
            $no_file_cache,
            '-d display_errors=1',
            '-d display_startup_errors=0'
        );"#,
    );
    replace_if_present(
        &mut text,
        r#"    $args = $test->hasSection('ARGS') ? ' -- ' . $test->getSection('ARGS') : '';"#,
        r#"    $args = $test->hasSection('ARGS') ? ' -- ' . $test->getSection('ARGS') : '';
    $stdin = null;"#,
    );
    replace_if_present(
        &mut text,
        r#"        save_text($tmp_post, $request);
        $cmd = "$php $pass_options $ini_settings -f \"$test_file\"$cmdRedirect < \"$tmp_post\"";"#,
        r#"        save_text($tmp_post, $request);
        $stdin = $request;
        $cmd = array_merge(
            compat_build_php_command($orig_php, $pass_options, $ini_settings),
            ['-f', $test_file]
        );"#,
    );
    replace_if_present(
        &mut text,
        r#"        save_text($tmp_post, $request);
        $cmd = "$php $pass_options $ini_settings -f \"$test_file\"$cmdRedirect < \"$tmp_post\"";"#,
        r#"        save_text($tmp_post, $request);
        $stdin = $request;
        $cmd = array_merge(
            compat_build_php_command($orig_php, $pass_options, $ini_settings),
            ['-f', $test_file]
        );"#,
    );
    replace_if_present(
        &mut text,
        r#"        $cmd = "$php $pass_options $ini_settings -f \"$test_file\"$cmdRedirect < \"$tmp_post\"";"#,
        r#"        $stdin = $post;
        $cmd = array_merge(
            compat_build_php_command($orig_php, $pass_options, $ini_settings),
            ['-f', $test_file]
        );"#,
    );
    replace_if_present(
        &mut text,
        r#"        $cmd = "$php $pass_options $ini_settings -f \"$test_file\"$cmdRedirect < \"$tmp_post\"";"#,
        r#"        $stdin = $post;
        $cmd = array_merge(
            compat_build_php_command($orig_php, $pass_options, $ini_settings),
            ['-f', $test_file]
        );"#,
    );
    replace_if_present(
        &mut text,
        r#"        $cmd = "$php $pass_options $ini_settings -f \"$test_file\"$cmdRedirect < \"$tmp_post\"";"#,
        r#"        $stdin = $post;
        $cmd = array_merge(
            compat_build_php_command($orig_php, $pass_options, $ini_settings),
            ['-f', $test_file]
        );"#,
    );
    replace_if_present(
        &mut text,
        r#"        $cmd = "$php $pass_options $repeat_option $ini_settings -f \"$test_file\" $args$cmdRedirect";"#,
        r#"        $cmd = array_merge(
            compat_build_php_command($orig_php, $pass_options, $repeat_option, $ini_settings),
            ['-f', $test_file],
            compat_split_command($args)
        );"#,
    );
    replace_if_present(
        &mut text,
        "COMMAND $cmd\n",
        "COMMAND \" . compat_command_display($cmd) . \"\n",
    );
    replace_if_present(
        &mut text,
        r#"    $orig_cmd = $cmd;"#,
        r#"    $orig_cmd = compat_command_display($cmd);"#,
    );
    replace_if_present(
        &mut text,
        r#"    $stdin = $test->hasSection('STDIN') ? $test->getSection('STDIN') : null;"#,
        r#"    if ($stdin === null) {
        $stdin = $test->hasSection('STDIN') ? $test->getSection('STDIN') : null;
    }"#,
    );
    replace_if_present(
        &mut text,
        r#"            $clean_output = system_with_timeout("$extra $orig_php $pass_options -q $orig_ini_settings $no_file_cache \"$test_clean\"", $env);"#,
        r#"            $clean_output = system_with_timeout(
                array_merge(
                    compat_build_php_command($orig_php, $pass_options, '-q', $orig_ini_settings, $no_file_cache),
                    [$test_clean]
                ),
                $env
            );"#,
    );

    fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{PhpRunner, normalize_test_name};

    #[test]
    fn php_batch_filter_selects_whole_batch() {
        let tests = (0..101)
            .map(|index| format!("test-{index:03}.phpt"))
            .collect();
        let jobs = PhpRunner::batch_jobs(tests);
        let selected: Vec<_> = jobs
            .into_iter()
            .filter(|job| job.id == "php-batch-0001")
            .collect();
        assert_eq!(PhpRunner::batch_filter("php-batch-0001"), Some(1));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].tests.len(), 50);
        assert_eq!(selected[0].tests[0], "test-050.phpt");
    }

    #[test]
    fn php_skips_known_unstable_tests() {
        assert!(PhpRunner::should_skip_test(
            "ext/standard/tests/general_functions/getrusage_basic.phpt"
        ));
        assert!(PhpRunner::should_skip_test(
            "ext/spl/tests/SplFileObject/fileobject_005.phpt"
        ));
        assert!(PhpRunner::should_skip_test(
            "ext/standard/tests/strings/vfprintf_variation1.phpt"
        ));
        assert!(!PhpRunner::should_skip_test("tests/basic/001.phpt"));
    }

    #[test]
    fn normalize_redirected_php_test_keeps_wrapper_context() {
        let source_root = Path::new("/repo");
        let name = "# /repo/ext/pdo_sqlite/tests/common.phpt: /repo/ext/pdo/tests/bug_34630.phpt";
        assert_eq!(
            normalize_test_name(source_root, name),
            "# ext/pdo_sqlite/tests/common.phpt: ext/pdo/tests/bug_34630.phpt"
        );
    }

    #[test]
    fn php_volume_flags_include_per_job_info_file() {
        let workspace = super::Workspace {
            output_dir: "/tmp/out".into(),
            checkout: "/tmp/checkout".into(),
            work_dir: "/tmp/work".into(),
        };
        let job = super::TestJob {
            id: "php-batch-0001".into(),
            tests: vec!["ext/standard/tests/general_functions/getrusage_basic.phpt".into()],
        };
        let flags = PhpRunner::volume_flags(&workspace, &job);

        assert!(flags.iter().any(|flag| flag == "--env"));
        assert!(flags.iter().any(
            |flag| flag.starts_with("TEST_PHP_INFO_FILE=/tmp/work/php-job-")
                && flag.ends_with("/run-test-info.php")
        ));
    }
}
