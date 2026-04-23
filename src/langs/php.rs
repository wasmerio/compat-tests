use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use super::{LangRunner, Mode, RunnerOpts, Status, TestResult, Workspace};
use crate::process::{ProcessError, write_stream};
use crate::run_log::RunLog;
use crate::runtime::{RunSpec, RunTarget, WasmerRuntime};

const GUEST_PHP_BIN: &str = "/bin/php";

pub struct PhpRunner;

impl PhpRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "php",
        git_repo: "https://github.com/wasix-org/php.git",
        git_ref: "6dd6dd1c7e409b8e9dcba8a8d6f9b7b5f944cc9e",
        wasmer_package: Some("php/php-32"),
        wasmer_flags: &[],
        docker_compose: None,
    };

    fn result_file(workspace: &Workspace) -> PathBuf {
        workspace.work_dir.join("php-results.tsv")
    }

    fn run_tests_path(workspace: &Workspace) -> PathBuf {
        workspace.checkout.join("run-tests.php")
    }

    fn test_path(workspace: &Workspace, id: &str) -> PathBuf {
        workspace.checkout.join(id)
    }

    // TODO: That is common for all langs - move to higher levels
    fn volume_flags(workspace: &Workspace) -> Vec<String> {
        vec![
            "--volume".into(),
            format!(
                "{}:{}",
                workspace.checkout.display(),
                workspace.checkout.display()
            ),
            "--volume".into(),
            format!(
                "{}:{}",
                workspace.work_dir.display(),
                workspace.work_dir.display()
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
        id: &str,
        mode: Mode,
    ) -> Result<Vec<TestResult>> {
        let test_path = Self::test_path(workspace, id);
        if !test_path.is_file() {
            bail!("php test not found: {}", test_path.display());
        }
        fs::create_dir_all(&workspace.work_dir)?;
        let result_file = Self::result_file(workspace);
        let _ = fs::remove_file(&result_file);

        let result = wasmer.run(
            RunSpec {
                target: RunTarget::Package(
                    Self::OPTS.wasmer_package.expect("php package").to_string(),
                ),
                flags: Self::volume_flags(workspace),
                args: vec![
                    "-d".into(),
                    "error_reporting=E_ALL & ~E_DEPRECATED".into(),
                    Self::run_tests_path(workspace).display().to_string(),
                    "-q".into(),
                    "-n".into(),
                    "-p".into(),
                    GUEST_PHP_BIN.into(),
                    "-W".into(),
                    result_file.display().to_string(),
                    test_path.display().to_string(),
                ],
                timeout: None,
            },
            |stream, line| {
                if matches!(mode, Mode::Debug) {
                    write_stream(stream, line)?;
                }
                Ok(())
            },
        );

        let mut results = Self::parse_results(&workspace.checkout, &result_file)?;
        if results.is_empty() {
            results.push(TestResult {
                id: id.to_string(),
                status: match result {
                    Ok(()) => Status::Pass,
                    Err(ProcessError::Timeout(_)) => Status::Timeout,
                    Err(ProcessError::AbnormalExit(_)) | Err(ProcessError::RustPanic(_)) => {
                        Status::Fail
                    }
                    Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
                },
            });
        }
        Ok(results)
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
        _ids: &[String],
    ) -> Result<()> {
        patch_php_runtests_worker_putenv(&workspace.checkout)?;
        patch_php_runtests_guest_exec(&workspace.checkout)
    }

    fn discover(
        &self,
        workspace: &Workspace,
        _wasmer: &WasmerRuntime,
        filter: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut tests = Vec::new();
        collect_phpt(&workspace.checkout, &workspace.checkout, &mut tests)?;
        tests.sort();
        Ok(match filter {
            None => tests,
            Some(filter) => tests
                .into_iter()
                .filter(|id| id == filter || id.contains(filter) || filter.contains(id.as_str()))
                .collect(),
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
        self.run_one(workspace, wasmer, id, mode)
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
