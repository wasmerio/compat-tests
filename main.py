#!/usr/bin/env python3
from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import threading
from datetime import datetime, timezone
from typing import Any

from php_upstream import (
    phpt_inventory,
    php_loaded_extensions,
    php_package_version,
    php_source_version,
    php_wasmer_runtime_probe,
    resolve_host_php_cgi,
    run_php_debug,
    run_php_upstream,
)
from python_upstream import append_log, run_python_debug, run_python_upstream

# TODO: We should probably take it automatically from the package via:
#       wasmer run --net python/python -- -c 'import sys; print(getattr(sys, "_git", None))'
DEFAULT_CPYTHON_REPO = "https://github.com/wasix-org/cpython.git"
DEFAULT_CPYTHON_COMMIT = "e3245fc95e570ac823deb50689041bc1f81d6b27"
DEFAULT_PHP_REPO = "https://github.com/wasix-org/php.git"
DEFAULT_PHP_REF = "v8.3.2102"
DEFAULT_PHP_COMMIT = "6dd6dd1c7e409b8e9dcba8a8d6f9b7b5f944cc9e"
DEFAULT_PHP_PACKAGE = "php/php-64@8.3.2102"
DEFAULT_TIMEOUT = 600
DEFAULT_PHP_TIMEOUT = 60
DEFAULT_LOG_FILE = "test.log"
RETEST_TIMEOUT = 300
RETEST_RUNS = 3
RESULT_STATUSES = ("PASS", "FAIL", "SKIP", "TIMEOUT", "FLAKY", "BORK", "WARN", "LEAK", "XFAIL", "XLEAK")
OK_RE = re.compile(r"^OK\b", re.MULTILINE)
FAILED_RE = re.compile(r"^FAILED \((.+)\)", re.MULTILINE)


def now_utc() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def run(cmd: list[str], *, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, cwd=cwd, text=True, check=True)


def run_capture(cmd: list[str], *, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, cwd=cwd, text=True, capture_output=True, check=True)


def slugify_ref(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "-" for ch in value.strip()) or "main"


def ensure_parent(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)


def load_json(path: Path) -> dict:
    if not path.exists():
        return {}
    text = path.read_text().strip()
    return json.loads(text) if text else {}


def write_json(path: Path, payload: dict) -> None:
    ensure_parent(path)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")


def write_text(path: Path, text: str) -> None:
    ensure_parent(path)
    path.write_text(text)


def parse_debug_unittest_status(output: str, exit_code: int) -> str:
    if "... skipped " in output:
        return "SKIP"
    if OK_RE.search(output) and exit_code == 0:
        return "PASS"
    if FAILED_RE.search(output) or exit_code != 0:
        return "FAIL"
    return "TIMEOUT"


def counts_from_status(status: dict[str, str]) -> dict[str, int]:
    counts = {name: 0 for name in RESULT_STATUSES}
    for value in status.values():
        if value in counts:
            counts[value] += 1
    return counts


def worker_count(limit: int | None = None) -> int:
    workers = (getattr(os, "process_cpu_count", os.cpu_count)() or 1) + 2
    if limit is not None:
        workers = min(workers, max(limit, 1))
    return workers


def git_head_commit(repo: Path) -> str:
    return run_capture(["git", "rev-parse", "HEAD"], cwd=repo).stdout.strip()


def git_current_branch(repo: Path) -> str:
    proc = subprocess.run(
        ["git", "symbolic-ref", "--short", "-q", "HEAD"],
        cwd=repo,
        text=True,
        capture_output=True,
    )
    branch = proc.stdout.strip()
    return branch or "local"


def git_has_ref(repo: Path, ref: str) -> bool:
    return subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", ref],
        cwd=repo,
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode == 0


def git_file_json(repo: Path, ref: str, path: str) -> dict:
    proc = subprocess.run(
        ["git", "show", f"{ref}:{path}"],
        cwd=repo,
        text=True,
        capture_output=True,
    )
    if proc.returncode != 0:
        return {}
    text = proc.stdout.strip()
    return json.loads(text) if text else {}


def clone_or_update_wasmer(repo_url: str, ref: str, work_dir: Path) -> Path:
    checkout = work_dir / "wasmer"
    if not checkout.exists():
        run(["git", "clone", "--depth", "1", "--no-tags", repo_url, str(checkout)])
    run(["git", "fetch", "--depth", "1", "--no-tags", "origin", ref], cwd=checkout)
    run(["git", "checkout", "-B", slugify_ref(ref), "FETCH_HEAD"], cwd=checkout)
    run(["git", "submodule", "update", "--init", "--depth", "1", "lib/napi"], cwd=checkout)
    return checkout


def resolve_wasmer_checkout(args: argparse.Namespace, work_dir: Path) -> Path:
    if args.wasmer_checkout:
        return Path(args.wasmer_checkout).resolve()
    return clone_or_update_wasmer("https://github.com/wasmerio/wasmer.git", args.wasmer_ref, work_dir)


def infer_wasmer_checkout_from_bin(wasmer_bin: Path) -> Path | None:
    try:
        checkout = wasmer_bin.resolve().parents[2]
    except IndexError:
        return None
    if (checkout / ".git").exists():
        return checkout
    return None


def resolve_local_wasmer_identity(args: argparse.Namespace, wasmer_bin: Path) -> tuple[str, str, str]:
    checkout = Path(args.wasmer_checkout).resolve() if args.wasmer_checkout else infer_wasmer_checkout_from_bin(wasmer_bin)
    if checkout and (checkout / ".git").exists():
        branch = git_current_branch(checkout)
        commit = git_head_commit(checkout)
        return branch, branch, commit
    return "local", "local", "local"


def try_download_prebuilt_main_wasmer(work_dir: Path) -> tuple[Path, str] | None:
    if shutil.which("gh") is None:
        print("Prebuilt Wasmer main artifact unavailable: gh CLI not found", flush=True)
        return None
    if platform.system() != "Linux":
        print(f"Prebuilt Wasmer main artifact unavailable: unsupported OS {platform.system()}", flush=True)
        return None
    machine = platform.machine().lower()
    if machine not in {"x86_64", "amd64"}:
        print(f"Prebuilt Wasmer main artifact unavailable: unsupported machine {machine}", flush=True)
        return None

    proc = subprocess.run(
        [
            "gh",
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
        ],
        text=True,
        capture_output=True,
    )
    if proc.returncode != 0:
        print("Prebuilt Wasmer main artifact lookup failed:", flush=True)
        if proc.stderr:
            print(proc.stderr, end="", flush=True)
        return None
    runs = json.loads(proc.stdout or "[]")
    run = next(
        (
            row
            for row in runs
            if row.get("status") == "completed"
            and row.get("conclusion") == "success"
            and row.get("event") == "push"
        ),
        None,
    )
    if not run:
        print("Prebuilt Wasmer main artifact unavailable: no successful main push build run found", flush=True)
        return None

    commit = run["headSha"]
    cache_dir = work_dir / "prebuilt-wasmer" / commit
    install_dir = cache_dir / "install"
    wasmer_bin = install_dir / "bin" / "wasmer"
    if wasmer_bin.exists():
        print(f"Using cached prebuilt Wasmer main artifact for {commit}", flush=True)
        return wasmer_bin, commit

    shutil.rmtree(cache_dir, ignore_errors=True)
    cache_dir.mkdir(parents=True, exist_ok=True)
    download = subprocess.run(
        [
            "gh",
            "run",
            "download",
            str(run["databaseId"]),
            "--repo",
            "wasmerio/wasmer",
            "-n",
            "wasmer-linux-amd64",
            "-D",
            str(cache_dir),
        ],
        text=True,
        capture_output=True,
    )
    if download.returncode != 0:
        print("Prebuilt Wasmer main artifact download failed:", flush=True)
        if download.stderr:
            print(download.stderr, end="", flush=True)
        return None

    archive = cache_dir / "wasmer.tar.gz"
    if not archive.exists():
        print("Prebuilt Wasmer main artifact download failed: wasmer.tar.gz missing", flush=True)
        return None
    install_dir.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "r:gz") as tar:
        tar.extractall(install_dir)
    if not wasmer_bin.exists():
        print("Prebuilt Wasmer main artifact extraction failed: bin/wasmer missing", flush=True)
        return None
    return wasmer_bin, commit


def ensure_cpython_checkout(work_dir: Path) -> Path:
    cache_root = work_dir / "cpython"
    safe = DEFAULT_CPYTHON_COMMIT
    checkout = cache_root / safe
    cache_root.mkdir(parents=True, exist_ok=True)
    if not (checkout / ".git").exists():
        run(["git", "clone", "--depth", "1", DEFAULT_CPYTHON_REPO, str(checkout)])
    run(["git", "fetch", "--depth", "1", "origin", DEFAULT_CPYTHON_COMMIT], cwd=checkout)
    run(["git", "checkout", "-B", "compat-tests-cpython", "FETCH_HEAD"], cwd=checkout)
    return checkout


def ensure_php_checkout(work_dir: Path) -> Path:
    cache_root = work_dir / "php"
    safe = DEFAULT_PHP_COMMIT
    checkout = cache_root / safe
    cache_root.mkdir(parents=True, exist_ok=True)
    if not (checkout / ".git").exists():
        run(["git", "clone", "--depth", "1", "--branch", DEFAULT_PHP_REF, DEFAULT_PHP_REPO, str(checkout)])
    run(["git", "fetch", "--depth", "1", "origin", DEFAULT_PHP_COMMIT], cwd=checkout)
    run(["git", "checkout", "-B", "compat-tests-php", "FETCH_HEAD"], cwd=checkout)
    patch_php_runtests_worker_putenv(checkout)
    return checkout


def patch_php_runtests_worker_putenv(php_checkout: Path) -> None:
    """Upstream workers restore $GLOBALS['environment'] but not getenv(); SKIPIF uses getenv."""
    path = php_checkout / "run-tests.php"
    text = path.read_text()
    marker = "compat-tests: sync getenv() for workers"
    if marker in text:
        return
    needle = """    foreach ($greeting["GLOBALS"] as $var => $value) {
        if ($var !== "workerID" && $var !== "workerSock" && $var !== "GLOBALS") {
            $GLOBALS[$var] = $value;
        }
    }
    foreach ($greeting["constants"] as $const => $value) {
        define($const, $value);
    }"""
    insert = """    foreach ($greeting["GLOBALS"] as $var => $value) {
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
    }"""
    if needle not in text:
        print(f"Warning: compat-tests could not patch run-tests.php (marker block missing): {path}", flush=True)
        return
    path.write_text(text.replace(needle, insert, 1))


def patch_faulthandler_workarounds(testdir: Path) -> None:
    # Temporary workaround: child Python startup with -X faulthandler is still
    # blocked by wasix-libc sigaltstack(). Remove these rewrites once libc is fixed.
    replacements = {
        testdir / "support" / "script_helper.py": [
            ("cmd_line = [sys.executable, '-X', 'faulthandler']", "cmd_line = [sys.executable]"),
            (
                'args = [sys.executable, "-E", "-X", "faulthandler", "-u", script, "-v"]',
                'args = [sys.executable, "-E", "-u", script, "-v"]',
            ),
        ],
        testdir / "test_regrtest.py": [
            (
                "args = [sys.executable, *extraargs, '-X', 'faulthandler', '-I', *args]",
                "args = [sys.executable, *extraargs, '-I', *args]",
            ),
        ],
        testdir / "bisect_cmd.py": [
            ("    cmd.extend(('-X', 'faulthandler'))\n", ""),
        ],
        testdir / "test_faulthandler.py": [
            ("import faulthandler\n", "import unittest\nraise unittest.SkipTest('blocked by wasix-libc sigaltstack() bug')\n"),
        ],
        testdir / "test_xxtestfuzz.py": [
            ("import faulthandler\n", "import unittest\nraise unittest.SkipTest('blocked by wasix-libc sigaltstack() bug')\n"),
        ],
        testdir / "libregrtest" / "setup.py": [
            ("        faulthandler.enable(all_threads=True, file=stderr_fd)\n", ""),
            (
                "        for signum in signals:\n            faulthandler.register(signum, chain=True, file=stderr_fd)\n",
                "        for signum in signals:\n            pass\n",
            ),
            ("        for signum in signals:\n\n", "        for signum in signals:\n            pass\n"),
        ],
        # HACK: CPython hardcodes errno 9 in test_interpreters cleanup.
        # WASI requires EBADF to be 8, so keep this rewrite until
        # https://github.com/python/cpython/pull/148345 lands upstream.
        testdir / "test_interpreters" / "utils.py": [
            ("import contextlib\n", "import contextlib\nimport errno\n"),
            ("        if exc.errno != 9:\n", "        if exc.errno != errno.EBADF:\n"),
        ],
    }
    for path, edits in replacements.items():
        text = path.read_text()
        for old, new in edits:
            if old in text:
                text = text.replace(old, new, 1)
        path.write_text(text)


def classify_changed_test(
    *,
    wasmer_bin: Path,
    host_test_dir: Path,
    test_name: str,
    old_status: str,
    new_status: str,
    log_path: Path | None,
    log_lock: threading.Lock | None,
) -> tuple[str, str, bool]:
    def rerun_once() -> str:
        try:
            proc = run_python_debug(
                wasmer_bin=str(wasmer_bin),
                host_test_dir=host_test_dir,
                test_name=test_name,
                timeout=RETEST_TIMEOUT,
            )
            append_log(log_path, log_lock, f"rerun {test_name}", proc.stdout or "", proc.stderr or "")
            return parse_debug_unittest_status((proc.stdout or "") + (proc.stderr or ""), proc.returncode)
        except subprocess.TimeoutExpired as exc:
            stdout = (exc.stdout.decode() if isinstance(exc.stdout, bytes) else exc.stdout) or ""
            stderr = (exc.stderr.decode() if isinstance(exc.stderr, bytes) else exc.stderr) or ""
            append_log(log_path, log_lock, f"rerun {test_name} TIMEOUT", stdout, stderr)
            return "TIMEOUT"

    if new_status != "PASS":
        outcome = rerun_once()
        if outcome == new_status:
            return test_name, new_status, False
        return test_name, old_status, True

    for _ in range(RETEST_RUNS):
        if rerun_once() != "PASS":
            return test_name, old_status, True
    return test_name, "PASS", False


def stabilize_changed_tests(
    *,
    baseline_status: dict[str, str],
    candidate_status: dict[str, str],
    wasmer_bin: Path,
    host_test_dir: Path,
    log_path: Path | None,
) -> tuple[dict[str, str], int]:
    changed = [
        test
        for test in sorted(set(baseline_status) & set(candidate_status))
        if baseline_status[test] != candidate_status[test]
    ]
    if not changed:
        return candidate_status, 0

    print(
        f"Re-running {len(changed)} changed tests with {worker_count(len(changed))} workers "
        f"({RETEST_RUNS} runs each, {RETEST_TIMEOUT}s timeout)...",
        flush=True,
    )

    effective = dict(candidate_status)
    flaky_count = 0
    log_lock = threading.Lock() if log_path is not None else None

    with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count(len(changed))) as pool:
        futures = {
            pool.submit(
                classify_changed_test,
                wasmer_bin=wasmer_bin,
                host_test_dir=host_test_dir,
                test_name=test_name,
                old_status=baseline_status[test_name],
                new_status=candidate_status[test_name],
                log_path=log_path,
                log_lock=log_lock,
            ): test_name
            for test_name in changed
        }
        completed = 0
        for future in concurrent.futures.as_completed(futures):
            test_name, effective_status, flaky = future.result()
            effective[test_name] = effective_status
            if flaky:
                flaky_count += 1
            completed += 1
            if completed % 10 == 0 or completed == len(changed):
                print(f"Re-ran {completed}/{len(changed)} changed tests", flush=True)

    return dict(sorted(effective.items())), flaky_count


def run_python_suite(args: argparse.Namespace) -> int:
    started_at = now_utc()
    output_dir = Path.cwd()
    work_dir = output_dir / ".work"
    work_dir.mkdir(parents=True, exist_ok=True)

    cpython_checkout = ensure_cpython_checkout(work_dir)
    host_test_dir = cpython_checkout / "Lib" / "test"
    patch_faulthandler_workarounds(host_test_dir)
    log_path = output_dir / DEFAULT_LOG_FILE

    wasmer_checkout: Path | None = None
    prebuilt = None
    if args.wasmer_bin:
        wasmer_bin = Path(args.wasmer_bin).resolve()
        if not wasmer_bin.exists():
            raise SystemExit(f"Wasmer binary not found: {wasmer_bin}")
        print(f"Using local Wasmer binary at {wasmer_bin}", flush=True)
        wasmer_ref, wasmer_branch, wasmer_commit = resolve_local_wasmer_identity(args, wasmer_bin)
    else:
        if not args.wasmer_checkout and args.wasmer_ref == "main":
            prebuilt = try_download_prebuilt_main_wasmer(work_dir)
        if prebuilt is not None:
            wasmer_bin, wasmer_commit = prebuilt
            wasmer_ref = args.wasmer_ref
            wasmer_branch = args.wasmer_ref
            print(f"Using prebuilt Wasmer main artifact at {wasmer_bin}", flush=True)
        else:
            wasmer_checkout = resolve_wasmer_checkout(args, work_dir)
            print(f"Building Wasmer from source at {wasmer_checkout}", flush=True)
            run(["cargo", "build", "-p", "wasmer-cli", "--features", "llvm", "--release"], cwd=wasmer_checkout)
            wasmer_bin = wasmer_checkout / "target" / "release" / "wasmer"
            wasmer_ref = args.wasmer_ref
            wasmer_branch = args.wasmer_ref
            wasmer_commit = git_head_commit(wasmer_checkout)

    if args.debug_test:
        proc = run_python_debug(
            wasmer_bin=str(wasmer_bin),
            host_test_dir=host_test_dir,
            test_name=args.debug_test,
            timeout=args.timeout,
        )
        print(proc.stdout, end="")
        print(proc.stderr, end="")
        return 0 if proc.returncode == 0 else proc.returncode
    else:
        write_text(log_path, "")
        prev_cwd = Path.cwd()
        os.chdir(output_dir)
        try:
            status = run_python_upstream(
                wasmer_bin=str(wasmer_bin),
                host_test_dir=host_test_dir,
                timeout=args.timeout,
                log_path=log_path,
            )
        finally:
            os.chdir(prev_cwd)
        if not status:
            raise SystemExit("Python upstream run did not produce any test statuses")

    baseline_status = git_file_json(output_dir, args.compare_ref, "status.json") if args.compare_ref else {}
    status, flaky_count = stabilize_changed_tests(
        baseline_status=baseline_status,
        candidate_status=status,
        wasmer_bin=Path(wasmer_bin),
        host_test_dir=host_test_dir,
        log_path=log_path,
    )

    write_json(output_dir / "status.json", status)
    metadata = {
        "wasmer": {
            "ref": wasmer_ref,
            "branch": wasmer_branch,
            "commit": wasmer_commit,
        },
        "python": {
            "cpython_commit": DEFAULT_CPYTHON_COMMIT,
        },
        "config": {
            "timeout_seconds": args.timeout,
            "debug_test": args.debug_test,
        },
        "run": {
            "started_at": started_at,
            "finished_at": now_utc(),
        },
        "counts": counts_from_status(status),
    }
    metadata["counts"]["FLAKY"] = flaky_count
    write_json(output_dir / "metadata.json", metadata)
    return 0


def run_php_suite(args: argparse.Namespace) -> int:
    started_at = now_utc()
    output_dir = Path.cwd()
    work_dir = output_dir / ".work"
    work_dir.mkdir(parents=True, exist_ok=True)

    php_checkout = ensure_php_checkout(work_dir)
    php_work_dir = work_dir / "php-runner"
    php_work_dir.mkdir(parents=True, exist_ok=True)
    log_path = output_dir / DEFAULT_LOG_FILE

    wasmer_checkout: Path | None = None
    prebuilt = None
    if args.wasmer_bin:
        wasmer_bin = Path(args.wasmer_bin).resolve()
        if not wasmer_bin.exists():
            raise SystemExit(f"Wasmer binary not found: {wasmer_bin}")
        print(f"Using local Wasmer binary at {wasmer_bin}", flush=True)
        wasmer_ref, wasmer_branch, wasmer_commit = resolve_local_wasmer_identity(args, wasmer_bin)
    else:
        if not args.wasmer_checkout and args.wasmer_ref == "main":
            prebuilt = try_download_prebuilt_main_wasmer(work_dir)
        if prebuilt is not None:
            wasmer_bin, wasmer_commit = prebuilt
            wasmer_ref = args.wasmer_ref
            wasmer_branch = args.wasmer_ref
            print(f"Using prebuilt Wasmer main artifact at {wasmer_bin}", flush=True)
        else:
            wasmer_checkout = resolve_wasmer_checkout(args, work_dir)
            print(f"Building Wasmer from source at {wasmer_checkout}", flush=True)
            run(["cargo", "build", "-p", "wasmer-cli", "--features", "llvm", "--release"], cwd=wasmer_checkout)
            wasmer_bin = wasmer_checkout / "target" / "release" / "wasmer"
            wasmer_ref = args.wasmer_ref
            wasmer_branch = args.wasmer_ref
            wasmer_commit = git_head_commit(wasmer_checkout)

    enable_net = not args.no_net
    source_version = php_source_version(php_checkout)
    package_version = php_package_version(
        wasmer_bin=str(wasmer_bin),
        php_package=args.php_package,
        enable_net=enable_net,
    )
    runtime_probe = php_wasmer_runtime_probe(
        wasmer_bin=str(wasmer_bin),
        php_package=args.php_package,
        enable_net=enable_net,
    )
    if runtime_probe.get("runs_as_root"):
        print(
            "Note: Wasmer PHP reports posix_geteuid()==0; some upstream tests skip as 'root'. "
            "See metadata.php.runtime_probe.",
            flush=True,
        )
    if not args.no_host_php_cgi and not args.no_php_cgi_shim:
        hc = resolve_host_php_cgi()
        if hc and args.php_cgi_package is None:
            print(f"Using host php-cgi for CGI sections: {hc}", flush=True)
    if source_version != "unknown" and package_version != "unknown" and source_version != package_version:
        print(
            f"Warning: PHP source checkout is {source_version}, but {args.php_package} reports {package_version}",
            flush=True,
        )

    if args.debug_test:
        proc = run_php_debug(
            wasmer_bin=str(wasmer_bin),
            php_package=args.php_package,
            php_cgi_package=args.php_cgi_package,
            phpdbg_package=args.phpdbg_package,
            no_php_cgi_shim=args.no_php_cgi_shim,
            source_root=php_checkout,
            work_dir=php_work_dir,
            test_name=args.debug_test,
            timeout=args.timeout,
            enable_net=enable_net,
            offline=args.offline,
            service_env=args.service_env,
            use_host_php_cgi=not args.no_host_php_cgi,
        )
        print(proc.stdout, end="")
        print(proc.stderr, end="")
        return 0 if proc.returncode == 0 else proc.returncode

    write_text(log_path, "")
    status = run_php_upstream(
        wasmer_bin=str(wasmer_bin),
        php_package=args.php_package,
        php_cgi_package=args.php_cgi_package,
        phpdbg_package=args.phpdbg_package,
        no_php_cgi_shim=args.no_php_cgi_shim,
        source_root=php_checkout,
        work_dir=php_work_dir,
        timeout=args.timeout,
        jobs=args.jobs,
        enable_net=enable_net,
        offline=args.offline,
        log_path=log_path,
        service_env=args.service_env,
        use_host_php_cgi=not args.no_host_php_cgi,
    )
    if not status:
        raise SystemExit("PHP upstream run did not produce any test statuses")

    inventory = phpt_inventory(php_checkout)
    loaded_extensions = php_loaded_extensions(
        wasmer_bin=str(wasmer_bin),
        php_package=args.php_package,
        enable_net=enable_net,
    )

    write_json(output_dir / "status.json", status)
    metadata = {
        "language": "php",
        "wasmer": {
            "ref": wasmer_ref,
            "branch": wasmer_branch,
            "commit": wasmer_commit,
        },
        "php": {
            "repo": DEFAULT_PHP_REPO,
            "ref": DEFAULT_PHP_REF,
            "commit": DEFAULT_PHP_COMMIT,
            "source_version": source_version,
            "package": args.php_package,
            "package_version": package_version,
            "loaded_extensions": loaded_extensions,
            "runtime_probe": runtime_probe,
            "phpt_total": sum(inventory.values()),
            "phpt_inventory": inventory,
        },
        "config": {
            "timeout_seconds": args.timeout,
            "debug_test": args.debug_test,
            "jobs": args.jobs,
            "offline": args.offline,
            "net": enable_net,
            "php_cgi_package": args.php_cgi_package,
            "phpdbg_package": args.phpdbg_package,
            "no_php_cgi_shim": args.no_php_cgi_shim,
            "service_env": args.service_env,
            "no_host_php_cgi": args.no_host_php_cgi,
        },
        "run": {
            "started_at": started_at,
            "finished_at": now_utc(),
        },
        "counts": counts_from_status(status),
    }
    write_json(output_dir / "metadata.json", metadata)
    return 0


def compare_statuses(baseline: dict[str, str], candidate: dict[str, str]) -> dict:
    baseline_counts = counts_from_status(baseline)
    candidate_counts = counts_from_status(candidate)
    all_tests = sorted(set(baseline) | set(candidate))
    pass_losses: list[dict[str, str]] = []
    pass_gains: list[dict[str, str]] = []
    changed: list[dict[str, str]] = []
    added: list[dict[str, str]] = []
    removed: list[dict[str, str]] = []

    for test in all_tests:
        old = baseline.get(test)
        new = candidate.get(test)
        if old is None:
            added.append({"test": test, "to": new})
            continue
        if new is None:
            removed.append({"test": test, "from": old})
            continue
        if old == new:
            continue
        row = {"test": test, "from": old, "to": new}
        changed.append(row)
        if old == "PASS" and new != "PASS":
            pass_losses.append(row)
        if old != "PASS" and new == "PASS":
            pass_gains.append(row)

    regressions: list[dict[str, str]] = []
    improvements: list[dict[str, str]] = []
    if candidate_counts["PASS"] < baseline_counts["PASS"]:
        regressions = pass_losses
    elif candidate_counts["PASS"] > baseline_counts["PASS"]:
        improvements = pass_gains

    return {
        "baseline_counts": baseline_counts,
        "candidate_counts": candidate_counts,
        "changed": changed,
        "regressions": regressions,
        "improvements": improvements,
        "added": added,
        "removed": removed,
    }


def compare_command(args: argparse.Namespace) -> int:
    baseline = load_json(Path(args.baseline))
    candidate = load_json(Path(args.candidate))
    result = compare_statuses(baseline, candidate)
    if args.output:
        write_json(Path(args.output), result)
    else:
        print(json.dumps(result, indent=2, sort_keys=True))
    return 0


def result_label(result: dict) -> str:
    baseline_pass = result.get("baseline_counts", {}).get("PASS", 0)
    candidate_pass = result.get("candidate_counts", {}).get("PASS", 0)
    if candidate_pass < baseline_pass:
        return "REGRESSION"
    if candidate_pass > baseline_pass:
        return "IMPROVEMENT"
    return "NO_CHANGE"


def metadata_language(meta: dict[str, Any], default: str = "Python") -> str:
    language = str(meta.get("language") or "").strip().lower()
    if language == "php" or ("php" in meta and "python" not in meta):
        return "PHP"
    if language == "python" or "python" in meta:
        return "Python"
    return default


def metadata_language_key(meta: dict[str, Any]) -> str:
    return metadata_language(meta).lower()


def render_summary_text(
    comparison: dict[str, Any],
    baseline_meta: dict[str, Any],
    candidate_meta: dict[str, Any],
    *,
    results_repo: str | None = None,
    results_commit: str | None = None,
    language: str = "Python",
) -> str:
    label = result_label(comparison)
    baseline_counts = baseline_meta.get("counts") or comparison.get("baseline_counts", {})
    candidate_counts = candidate_meta.get("counts") or comparison.get("candidate_counts", {})

    lines = [
        f"{language} upstream result: {label}",
        "",
        f"Baseline: {baseline_meta['wasmer']['branch']} @ {baseline_meta['wasmer']['commit'][:7]}",
        f"Candidate: {candidate_meta['wasmer']['branch']} @ {candidate_meta['wasmer']['commit'][:7]}",
        "",
        "Summary:",
        f"- Regressions: {len(comparison.get('regressions', []))}",
        f"- Improvements: {len(comparison.get('improvements', []))}",
        f"- FLAKY: {baseline_counts.get('FLAKY', 0)} -> {candidate_counts.get('FLAKY', 0)}",
        f"- PASS: {baseline_counts.get('PASS', 0)} -> {candidate_counts.get('PASS', 0)}",
        f"- FAIL: {baseline_counts.get('FAIL', 0)} -> {candidate_counts.get('FAIL', 0)}",
        f"- TIMEOUT: {baseline_counts.get('TIMEOUT', 0)} -> {candidate_counts.get('TIMEOUT', 0)}",
        f"- SKIP: {baseline_counts.get('SKIP', 0)} -> {candidate_counts.get('SKIP', 0)}",
    ]

    if results_commit and results_repo:
        lines.extend(["", f"See full details at [commit](https://github.com/{results_repo}/commit/{results_commit})."])

    if comparison.get("regressions"):
        lines.extend(["", "Top regressions:"])
        lines.extend(f"- `{row['test']}` ({row['from']} -> {row['to']})" for row in comparison["regressions"][:10])

    if comparison.get("improvements"):
        lines.extend(["", "Top improvements:"])
        lines.extend(f"- `{row['test']}` ({row['from']} -> {row['to']})" for row in comparison["improvements"][:10])

    return "\n".join(lines) + "\n"


def render_comment(args: argparse.Namespace) -> int:
    comparison = load_json(Path(args.comparison))
    baseline_meta = load_json(Path(args.baseline_metadata))
    candidate_meta = load_json(Path(args.candidate_metadata))
    language = args.language or metadata_language(candidate_meta)
    body = render_summary_text(
        comparison,
        baseline_meta,
        candidate_meta,
        results_repo=args.results_repo,
        results_commit=args.results_commit,
        language=language,
    )
    if args.output:
        write_text(Path(args.output), body)
    else:
        print(body, end="")
    return 0


def prepare_pr_comment(args: argparse.Namespace) -> int:
    comparison = load_json(Path(args.comparison))
    baseline_meta = load_json(Path(args.baseline_metadata))
    candidate_meta = load_json(Path(args.candidate_metadata))

    expected = (args.expected_wasmer_sha or "").strip()
    actual = candidate_meta.get("wasmer", {}).get("commit", "")
    language = args.language or metadata_language(candidate_meta)
    if expected and actual != expected:
        body = (
            f"{language} upstream result: ERROR\n\n"
            "compat-tests completed, but the published snapshot does not match the expected Wasmer SHA.\n\n"
            "More info:\n"
            f"- expected Wasmer SHA: `{expected}`\n"
            f"- published Wasmer SHA: `{actual}`\n"
            f"- compat-tests workflow: {args.run_url}\n"
            f"- compat-tests results commit: https://github.com/{args.results_repo}/commit/{args.results_commit}\n"
        )
    else:
        body = render_summary_text(
            comparison,
            baseline_meta,
            candidate_meta,
            results_repo=args.results_repo,
            results_commit=args.results_commit,
            language=language,
        ).rstrip()
        body += (
            "\n\nMore info:\n"
            f"- compat-tests workflow: {args.run_url}\n"
            f"- compat-tests results commit: https://github.com/{args.results_repo}/commit/{args.results_commit}\n"
            f"- compat-tests branch: https://github.com/{args.results_repo}/tree/{candidate_meta['wasmer']['branch']}\n"
            f"- full test log: {args.log_artifact_url}\n"
        )

    if args.output:
        write_text(Path(args.output), body + ("\n" if not body.endswith("\n") else ""))
    else:
        print(body, end="" if body.endswith("\n") else "\n")
    return 0


def make_regression_issue_title(candidate_meta: dict[str, Any], language: str | None = None) -> str:
    language = language or metadata_language(candidate_meta)
    branch = candidate_meta["wasmer"]["branch"]
    commit = candidate_meta["wasmer"]["commit"][:7]
    return f"{language} upstream regressions on {branch} @ {commit}"


def create_regression_issue(args: argparse.Namespace) -> int:
    comparison = load_json(Path(args.comparison))
    if result_label(comparison) != "REGRESSION":
        print("No regressions detected; skipping issue creation.", flush=True)
        return 0

    baseline_meta = load_json(Path(args.baseline_metadata))
    candidate_meta = load_json(Path(args.candidate_metadata))
    language = args.language or metadata_language(candidate_meta)
    repo = Path(args.repo).resolve() if args.repo else None
    results_commit = git_head_commit(repo) if repo and (repo / ".git").exists() else None

    body = render_summary_text(
        comparison,
        baseline_meta,
        candidate_meta,
        results_repo=args.results_repo,
        results_commit=results_commit,
        language=language,
    )
    if args.run_url:
        body += f"\nWorkflow run:\n- {args.run_url}\n"

    title = make_regression_issue_title(candidate_meta, language=language)
    body_file = (repo or Path.cwd()) / ".git" / "compat-tests-issue-body.txt" if repo else Path(".git/compat-tests-issue-body.txt")
    write_text(body_file, body)
    proc = run_capture(
        [
            "gh",
            "issue",
            "create",
            "--repo",
            args.repo_full_name,
            "--title",
            title,
            "--body-file",
            str(body_file),
        ],
        cwd=repo,
    )
    print(proc.stdout.strip(), flush=True)
    return 0


def maybe_setup_gh_git_auth(repo: Path) -> None:
    if shutil.which("gh"):
        subprocess.run(["gh", "auth", "setup-git"], cwd=repo, text=True, check=False)


def ensure_git_identity(repo: Path, name: str, email: str) -> None:
    current_name = subprocess.run(
        ["git", "config", "--get", "user.name"],
        cwd=repo,
        text=True,
        capture_output=True,
    ).stdout.strip()
    current_email = subprocess.run(
        ["git", "config", "--get", "user.email"],
        cwd=repo,
        text=True,
        capture_output=True,
    ).stdout.strip()
    if not current_name:
        run(["git", "config", "user.name", name], cwd=repo)
    if not current_email:
        run(["git", "config", "user.email", email], cwd=repo)


def ensure_branch_checked_out(repo: Path, branch: str) -> None:
    if git_has_ref(repo, f"refs/remotes/origin/{branch}"):
        run(["git", "checkout", "-f", "-B", branch, f"origin/{branch}"], cwd=repo)
    else:
        run(["git", "checkout", "-f", "-B", branch], cwd=repo)


def has_staged_changes(repo: Path) -> bool:
    proc = subprocess.run(
        ["git", "diff", "--cached", "--quiet"],
        cwd=repo,
        text=True,
    )
    return proc.returncode != 0


def publish_snapshot(args: argparse.Namespace) -> int:
    repo = Path(args.repo).resolve()
    source_dir = Path(args.source_dir).resolve()
    branch = args.branch
    compare_ref = args.compare_ref

    status = load_json(source_dir / "status.json")
    metadata = load_json(source_dir / "metadata.json")
    if not status or not metadata:
        raise SystemExit(f"Missing status.json or metadata.json in {source_dir}")

    run(["git", "fetch", "--all", "--tags"], cwd=repo)
    ensure_branch_checked_out(repo, branch)

    compare_status = git_file_json(repo, compare_ref, "status.json") if compare_ref else {}
    compare_meta = git_file_json(repo, compare_ref, "metadata.json") if compare_ref else {}
    comparison = compare_statuses(compare_status, status)
    if not compare_meta:
        compare_meta = {
            "wasmer": {
                "branch": compare_ref or "none",
                "commit": "0000000",
            },
            "counts": counts_from_status(compare_status),
        }
    write_json(repo / ".git" / "compat-tests-baseline-metadata.json", compare_meta)
    write_json(repo / ".git" / "compat-tests-comparison.json", comparison)

    branch_status = git_file_json(repo, "HEAD", "status.json")
    branch_metadata = git_file_json(repo, "HEAD", "metadata.json")
    language_key = metadata_language_key(metadata)
    same_identity = (
        metadata_language(branch_metadata, metadata_language(metadata)) == metadata_language(metadata)
        and branch_metadata.get("wasmer", {}) == metadata.get("wasmer", {})
        and branch_metadata.get(language_key, {}) == metadata.get(language_key, {})
        and branch_metadata.get("config", {}) == metadata.get("config", {})
    )
    if branch_status == status and same_identity:
        print("No snapshot changes to publish.", flush=True)
        print(git_head_commit(repo), flush=True)
        return 0

    language = metadata_language(metadata)
    summary_text = render_summary_text(comparison, compare_meta, metadata, language=language)
    write_json(repo / "status.json", status)
    write_json(repo / "metadata.json", metadata)

    run(["git", "add", "status.json", "metadata.json"], cwd=repo)

    maybe_setup_gh_git_auth(repo)
    ensure_git_identity(repo, args.git_user_name, args.git_user_email)
    msg_file = repo / ".git" / "compat-tests-commit-message.txt"
    write_text(msg_file, summary_text)
    run(["git", "commit", "-F", str(msg_file)], cwd=repo)
    run(["git", "push", "origin", branch], cwd=repo)

    commit = git_head_commit(repo)
    print(commit, flush=True)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    run_python = sub.add_parser("run-python", help="Run the upstream CPython suite against a Wasmer checkout")
    run_python.add_argument("--wasmer-ref", default="main")
    run_python.add_argument("--wasmer-bin")
    run_python.add_argument("--wasmer-checkout")
    run_python.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    run_python.add_argument("--compare-ref", default="origin/main")
    run_python.add_argument("--debug-test")
    run_python.set_defaults(func=run_python_suite)

    run_php = sub.add_parser("run-php", help="Run the upstream PHP PHPT suite against a Wasmer checkout")
    run_php.add_argument("--wasmer-ref", default="main")
    run_php.add_argument("--wasmer-bin")
    run_php.add_argument("--wasmer-checkout")
    run_php.add_argument("--timeout", type=int, default=DEFAULT_PHP_TIMEOUT, help="Per-test PHPT timeout in seconds")
    run_php.add_argument("--jobs", type=int, default=worker_count(), help="Parallel PHPT workers for run-tests.php")
    run_php.add_argument("--php-package", default=DEFAULT_PHP_PACKAGE)
    run_php.add_argument(
        "--php-cgi-package",
        default=None,
        help="Wasmer package for the CGI shim (default: same as --php-package). "
        "Unless --no-php-cgi-shim is set, compat-tests writes php-wasmer-cgi and sets TEST_PHP_CGI_EXECUTABLE.",
    )
    run_php.add_argument(
        "--phpdbg-package",
        default=None,
        help="Optional Wasmer package that provides phpdbg; if unset, phpdbg PHPTs stay skipped.",
    )
    run_php.add_argument(
        "--no-php-cgi-shim",
        action="store_true",
        help="Do not set TEST_PHP_CGI_EXECUTABLE (upstream skips CGI sections).",
    )
    run_php.add_argument(
        "--no-host-php-cgi",
        action="store_true",
        help="Use the Wasmer package for the CGI shim even if host php-cgi exists (default: host php-cgi when found).",
    )
    run_php.add_argument(
        "--service-env",
        action="store_true",
        help="Merge MYSQL_* / PGSQL_TEST_CONNSTR defaults matching docker-compose.yml (host 127.0.0.1).",
    )
    run_php.add_argument("--offline", action="store_true", help="Pass --offline to run-tests.php")
    run_php.add_argument("--no-net", action="store_true", help="Do not pass --net to wasmer run")
    run_php.add_argument("--debug-test", help="Run one .phpt path, relative to the PHP checkout or absolute")
    run_php.set_defaults(func=run_php_suite)

    compare = sub.add_parser("compare-status", help="Compare two status.json snapshots")
    compare.add_argument("--baseline", required=True)
    compare.add_argument("--candidate", required=True)
    compare.add_argument("--output")
    compare.set_defaults(func=compare_command)

    comment = sub.add_parser("render-pr-comment", help="Render the PR summary comment from a comparison")
    comment.add_argument("--comparison", required=True)
    comment.add_argument("--baseline-metadata", required=True)
    comment.add_argument("--candidate-metadata", required=True)
    comment.add_argument("--results-repo")
    comment.add_argument("--results-commit")
    comment.add_argument("--language")
    comment.add_argument("--output")
    comment.set_defaults(func=render_comment)

    pr_comment = sub.add_parser("prepare-pr-comment", help="Prepare the full PR comment body")
    pr_comment.add_argument("--comparison", required=True)
    pr_comment.add_argument("--baseline-metadata", required=True)
    pr_comment.add_argument("--candidate-metadata", required=True)
    pr_comment.add_argument("--results-repo", required=True)
    pr_comment.add_argument("--results-commit", required=True)
    pr_comment.add_argument("--run-url", required=True)
    pr_comment.add_argument("--log-artifact-url", required=True)
    pr_comment.add_argument("--expected-wasmer-sha")
    pr_comment.add_argument("--language")
    pr_comment.add_argument("--output")
    pr_comment.set_defaults(func=prepare_pr_comment)

    publish = sub.add_parser("publish-snapshot", help="Publish the current snapshot into a results branch")
    publish.add_argument("--repo", default=".")
    publish.add_argument("--source-dir", default=".")
    publish.add_argument("--branch", required=True)
    publish.add_argument("--compare-ref", default="origin/main")
    publish.add_argument("--git-user-name", default="compat-tests[bot]")
    publish.add_argument("--git-user-email", default="compat-tests[bot]@users.noreply.github.com")
    publish.set_defaults(func=publish_snapshot)

    issue = sub.add_parser("create-regression-issue", help="Create a GitHub issue when regressions are present")
    issue.add_argument("--repo", default=".")
    issue.add_argument("--repo-full-name", required=True)
    issue.add_argument("--comparison", required=True)
    issue.add_argument("--baseline-metadata", required=True)
    issue.add_argument("--candidate-metadata", required=True)
    issue.add_argument("--results-repo")
    issue.add_argument("--run-url")
    issue.add_argument("--language")
    issue.set_defaults(func=create_regression_issue)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
