from __future__ import annotations

import json
from pathlib import Path
import os
import re
import shutil
import shlex
import stat
import subprocess
from typing import Any, Iterable


PHP_RESULT_MAP = {
    "PASSED": "PASS",
    "FAILED": "FAIL",
    "SKIPPED": "SKIP",
    "BORKED": "BORK",
    "WARNED": "WARN",
    "LEAKED": "LEAK",
    "XFAILED": "XFAIL",
    "XLEAKED": "XLEAK",
}

# Matches credentials in docker-compose.yml (host-published ports).
SERVICE_ENV_DEFAULTS: dict[str, str] = {
    "MYSQL_TEST_HOST": "127.0.0.1",
    "MYSQL_TEST_PORT": "3306",
    "MYSQL_TEST_USER": "root",
    "MYSQL_TEST_PASSWD": "compat_root",
    "MYSQL_TEST_DB": "test",
    "PGSQL_TEST_CONNSTR": "host=127.0.0.1 port=5432 dbname=test user=postgres password=compat_postgres",
}


def ensure_host_php() -> str:
    php = shutil.which("php")
    if php is None:
        raise RuntimeError("Host php executable not found in PATH; PHP run-tests.php is required as the PHPT runner")
    return php


def sh_quote(value: str | Path) -> str:
    s = str(value)
    return "'" + s.replace("'", "'\"'\"'") + "'"


def wasmer_guest_env_cli_flags(guest_env: dict[str, str] | None) -> str:
    """Extra `wasmer run` args so PHP inside the guest sees MYSQL_*, PGSQL_*, etc."""
    if not guest_env:
        return ""
    parts: list[str] = []
    for key, val in guest_env.items():
        parts.append(f"--env {sh_quote(f'{key}={val}')}")
    return " " + " ".join(parts)


def write_wasmer_php_shim(
    *,
    path: Path,
    wasmer_bin: str,
    php_package: str,
    source_root: Path,
    enable_net: bool,
    guest_env: dict[str, str] | None = None,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    net_arg = " --net" if enable_net else ""
    env_arg = wasmer_guest_env_cli_flags(guest_env)
    text = f"""#!/bin/sh
set -eu
exec {sh_quote(wasmer_bin)} run{net_arg}{env_arg} --volume {sh_quote(f"{source_root}:{source_root}")} {sh_quote(php_package)} -- "$@"
"""
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def prepare_php_wasmer_shims(
    *,
    work_dir: Path,
    wasmer_bin: str,
    php_package: str,
    php_cgi_package: str | None,
    phpdbg_package: str | None,
    no_php_cgi_shim: bool,
    source_root: Path,
    enable_net: bool,
    service_env: bool,
) -> tuple[Path, Path | None, Path | None]:
    """CLI wrapper is always written; CGI shim is optional; phpdbg only if a package is given."""
    guest_env = dict(SERVICE_ENV_DEFAULTS) if service_env else None
    cli = work_dir / "php-wasmer"
    write_wasmer_php_shim(
        path=cli,
        wasmer_bin=wasmer_bin,
        php_package=php_package,
        source_root=source_root,
        enable_net=enable_net,
        guest_env=guest_env,
    )

    cgi_pkg = php_cgi_package or php_package
    cgi: Path | None = None
    if not no_php_cgi_shim:
        cgi = work_dir / "php-wasmer-cgi"
        write_wasmer_php_shim(
            path=cgi,
            wasmer_bin=wasmer_bin,
            php_package=cgi_pkg,
            source_root=source_root,
            enable_net=enable_net,
            guest_env=guest_env,
        )

    phpdbg: Path | None = None
    if phpdbg_package:
        phpdbg = work_dir / "php-wasmer-phpdbg"
        write_wasmer_php_shim(
            path=phpdbg,
            wasmer_bin=wasmer_bin,
            php_package=phpdbg_package,
            source_root=source_root,
            enable_net=enable_net,
            guest_env=guest_env,
        )

    return cli, cgi, phpdbg


def php_runtests_extra_env(
    *,
    cgi_shim: Path | None,
    phpdbg_shim: Path | None,
    service_env: bool,
) -> dict[str, str]:
    extra: dict[str, str] = {}
    if cgi_shim is not None:
        extra["TEST_PHP_CGI_EXECUTABLE"] = str(cgi_shim)
    if phpdbg_shim is not None:
        extra["TEST_PHPDBG_EXECUTABLE"] = str(phpdbg_shim)
    if service_env:
        extra.update(SERVICE_ENV_DEFAULTS)
    return extra


def normalize_php_test_name(source_root: Path, name: str) -> str:
    if name.startswith("# "):
        return name
    path = Path(name)
    try:
        return path.resolve().relative_to(source_root.resolve()).as_posix()
    except ValueError:
        return name


def parse_php_results(source_root: Path, result_file: Path) -> dict[str, str]:
    if not result_file.exists():
        return {}

    status: dict[str, str] = {}
    for line in result_file.read_text(errors="replace").splitlines():
        if not line.strip():
            continue
        raw_status, _, raw_name = line.partition("\t")
        if not raw_name:
            continue
        mapped = PHP_RESULT_MAP.get(raw_status.strip(), raw_status.strip())
        name = normalize_php_test_name(source_root, raw_name.strip())
        status[name] = mapped
    return dict(sorted(status.items()))


def php_package_version(*, wasmer_bin: str, php_package: str, enable_net: bool) -> str:
    cmd = [wasmer_bin, "run"]
    if enable_net:
        cmd.append("--net")
    cmd.extend([php_package, "--", "-r", "echo PHP_VERSION;"])
    proc = subprocess.run(cmd, text=True, capture_output=True, check=False)
    if proc.returncode != 0:
        return "unknown"
    return proc.stdout.strip() or "unknown"


def php_loaded_extensions(*, wasmer_bin: str, php_package: str, enable_net: bool) -> list[str]:
    cmd = [wasmer_bin, "run"]
    if enable_net:
        cmd.append("--net")
    cmd.extend([php_package, "--", "-r", "echo implode(PHP_EOL, get_loaded_extensions());"])
    proc = subprocess.run(cmd, text=True, capture_output=True, check=False)
    if proc.returncode != 0:
        return []
    return sorted(line.strip() for line in proc.stdout.splitlines() if line.strip())


def php_wasmer_runtime_probe(*, wasmer_bin: str, php_package: str, enable_net: bool) -> dict[str, Any]:
    """Best-effort facts about the tested Wasmer PHP (root, SAPI)."""
    cmd_base = [wasmer_bin, "run"]
    if enable_net:
        cmd_base.append("--net")

    probe = (
        "$u = function_exists('posix_geteuid') ? posix_geteuid() : null; "
        "echo json_encode(['posix_geteuid' => $u, 'sapi' => PHP_SAPI]);"
    )
    proc = subprocess.run(
        [*cmd_base, php_package, "--", "-r", probe],
        text=True,
        capture_output=True,
        check=False,
    )
    out: dict[str, Any] = {"raw_stdout": (proc.stdout or "").strip(), "returncode": proc.returncode}
    if proc.returncode == 0 and proc.stdout.strip():
        try:
            parsed = json.loads(proc.stdout.strip())
            if isinstance(parsed, dict):
                out.update(parsed)
        except json.JSONDecodeError:
            pass
    uid = out.get("posix_geteuid")
    out["runs_as_root"] = uid == 0
    out["root_skip_tests_note"] = (
        "Upstream mysqli and other tests skip when posix_geteuid() is 0; "
        "Wasmer WASI PHP reports euid 0 — those skips cannot be removed from compat-tests alone."
        if out.get("runs_as_root")
        else None
    )
    return out


def phpt_inventory(source_root: Path) -> dict[str, int]:
    counts: dict[str, int] = {}
    for path in source_root.rglob("*.phpt"):
        rel = path.relative_to(source_root)
        if rel.parts[0] == "ext" and len(rel.parts) > 1:
            key = f"ext/{rel.parts[1]}"
        elif rel.parts[0] == "sapi" and len(rel.parts) > 1:
            key = f"sapi/{rel.parts[1]}"
        else:
            key = rel.parts[0]
        counts[key] = counts.get(key, 0) + 1
    return dict(sorted(counts.items(), key=lambda item: (-item[1], item[0])))


def php_source_version(source_root: Path) -> str:
    version_header = source_root / "main" / "php_version.h"
    if not version_header.exists():
        return "unknown"
    match = re.search(r'#define PHP_VERSION "([^"]+)"', version_header.read_text(errors="replace"))
    return match.group(1) if match else "unknown"


def php_test_command(
    *,
    host_php: str,
    source_root: Path,
    wrapper: Path,
    result_file: Path,
    timeout: int,
    jobs: int | None,
    offline: bool,
    tests: Iterable[str] = (),
) -> list[str]:
    cmd = [
        host_php,
        "-d",
        "error_reporting=E_ALL & ~E_DEPRECATED",
        str(source_root / "run-tests.php"),
        "-q",
        "-p",
        str(wrapper),
        "-n",
        "--set-timeout",
        str(timeout),
        "-W",
        str(result_file),
    ]
    if jobs and jobs > 1:
        cmd.append(f"-j{jobs}")
    if offline:
        cmd.append("--offline")
    cmd.extend(tests)
    return cmd


def run_php_debug(
    *,
    wasmer_bin: str,
    php_package: str,
    php_cgi_package: str | None,
    phpdbg_package: str | None,
    no_php_cgi_shim: bool,
    source_root: Path,
    work_dir: Path,
    test_name: str,
    timeout: int,
    enable_net: bool,
    offline: bool,
    service_env: bool,
) -> subprocess.CompletedProcess[str]:
    host_php = ensure_host_php()
    cli, cgi, phpdbg = prepare_php_wasmer_shims(
        work_dir=work_dir,
        wasmer_bin=wasmer_bin,
        php_package=php_package,
        php_cgi_package=php_cgi_package,
        phpdbg_package=phpdbg_package,
        no_php_cgi_shim=no_php_cgi_shim,
        source_root=source_root,
        enable_net=enable_net,
        service_env=service_env,
    )
    result_file = work_dir / "php-debug-results.tsv"
    result_file.unlink(missing_ok=True)
    cmd = php_test_command(
        host_php=host_php,
        source_root=source_root,
        wrapper=cli,
        result_file=result_file,
        timeout=timeout,
        jobs=None,
        offline=offline,
        tests=[test_name],
    )
    child_env = os.environ.copy()
    child_env.update(php_runtests_extra_env(cgi_shim=cgi, phpdbg_shim=phpdbg, service_env=service_env))
    print(shlex.join(cmd), flush=True)
    return subprocess.run(cmd, cwd=source_root, text=True, capture_output=True, env=child_env)


def run_php_upstream(
    *,
    wasmer_bin: str,
    php_package: str,
    php_cgi_package: str | None,
    phpdbg_package: str | None,
    no_php_cgi_shim: bool,
    source_root: Path,
    work_dir: Path,
    timeout: int,
    jobs: int | None,
    enable_net: bool,
    offline: bool,
    log_path: Path | None = None,
    service_env: bool = False,
) -> dict[str, str]:
    host_php = ensure_host_php()
    cli, cgi, phpdbg = prepare_php_wasmer_shims(
        work_dir=work_dir,
        wasmer_bin=wasmer_bin,
        php_package=php_package,
        php_cgi_package=php_cgi_package,
        phpdbg_package=phpdbg_package,
        no_php_cgi_shim=no_php_cgi_shim,
        source_root=source_root,
        enable_net=enable_net,
        service_env=service_env,
    )
    result_file = work_dir / "php-results.tsv"
    result_file.unlink(missing_ok=True)
    cmd = php_test_command(
        host_php=host_php,
        source_root=source_root,
        wrapper=cli,
        result_file=result_file,
        timeout=timeout,
        jobs=jobs,
        offline=offline,
    )

    print("+", shlex.join(cmd), flush=True)
    child_env = os.environ.copy()
    extra = php_runtests_extra_env(cgi_shim=cgi, phpdbg_shim=phpdbg, service_env=service_env)
    child_env.update(extra)
    log = log_path.open("a") if log_path is not None else None
    try:
        if log is not None:
            log.write("===== PHP upstream run =====\n")
            log.write("+ " + shlex.join(cmd) + "\n")
            if extra:
                log.write(
                    "env (compat-tests): "
                    + shlex.join([f"{k}={v}" for k, v in sorted(extra.items())])
                    + "\n"
                )
        proc = subprocess.Popen(
            cmd,
            cwd=source_root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            bufsize=1,
            env=child_env,
        )
        assert proc.stdout is not None
        for line in proc.stdout:
            print(line, end="", flush=True)
            if log is not None:
                log.write(line)
        exit_code = proc.wait()
        if log is not None:
            log.write(f"\n===== PHP upstream exit code: {exit_code} =====\n")
    finally:
        if log is not None:
            log.close()

    return parse_php_results(source_root, result_file)
