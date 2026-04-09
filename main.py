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
from datetime import datetime, timezone
from typing import Any

from python_upstream import run_python_debug, run_python_upstream

DEFAULT_CPYTHON_VERSION = "v3.13.0"
DEFAULT_TIMEOUT = 600
RETEST_TIMEOUT = 300
RETEST_RUNS = 3
RESULT_STATUSES = ("PASS", "FAIL", "SKIP", "TIMEOUT", "FLAKY")
REGRESSION_TRANSITIONS = {
    ("PASS", "FAIL"),
    ("PASS", "TIMEOUT"),
    ("FAIL", "TIMEOUT"),
    ("SKIP", "FAIL"),
}
IMPROVEMENT_TRANSITIONS = {
    ("FAIL", "PASS"),
    ("TIMEOUT", "PASS"),
    ("TIMEOUT", "FAIL"),
}
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
            "databaseId,headSha,conclusion,status",
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
    run = next((row for row in runs if row.get("status") == "completed" and row.get("conclusion") == "success"), None)
    if not run:
        print("Prebuilt Wasmer main artifact unavailable: no successful main build run found", flush=True)
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


def ensure_cpython_checkout(version: str, work_dir: Path) -> Path:
    cache_root = work_dir / "cpython"
    safe = "".join(ch for ch in version.replace("/", "_").replace(":", "_").replace("@", "_") if ch.isalnum() or ch in "._-")
    checkout = cache_root / safe
    cache_root.mkdir(parents=True, exist_ok=True)
    if not (checkout / ".git").exists():
        run(["git", "clone", "--depth", "1", "--branch", version, "https://github.com/python/cpython.git", str(checkout)])
    return checkout


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
) -> tuple[str, str, bool]:
    def rerun_once() -> str:
        try:
            proc = run_python_debug(
                wasmer_bin=str(wasmer_bin),
                host_test_dir=host_test_dir,
                test_name=test_name,
                timeout=RETEST_TIMEOUT,
            )
            return parse_debug_unittest_status((proc.stdout or "") + (proc.stderr or ""), proc.returncode)
        except subprocess.TimeoutExpired:
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

    with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count(len(changed))) as pool:
        futures = {
            pool.submit(
                classify_changed_test,
                wasmer_bin=wasmer_bin,
                host_test_dir=host_test_dir,
                test_name=test_name,
                old_status=baseline_status[test_name],
                new_status=candidate_status[test_name],
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

    cpython_checkout = ensure_cpython_checkout(args.cpython_version, work_dir)
    host_test_dir = cpython_checkout / "Lib" / "test"
    patch_faulthandler_workarounds(host_test_dir)

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
        prev_cwd = Path.cwd()
        os.chdir(output_dir)
        try:
            status = run_python_upstream(
                wasmer_bin=str(wasmer_bin),
                host_test_dir=host_test_dir,
                timeout=args.timeout,
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
    )

    write_json(output_dir / "status.json", status)
    metadata = {
        "wasmer": {
            "ref": wasmer_ref,
            "branch": wasmer_branch,
            "commit": wasmer_commit,
        },
        "python": {
            "cpython_version": args.cpython_version,
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


def compare_statuses(baseline: dict[str, str], candidate: dict[str, str]) -> dict:
    all_tests = sorted(set(baseline) | set(candidate))
    regressions: list[dict[str, str]] = []
    improvements: list[dict[str, str]] = []
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
        if (old, new) in REGRESSION_TRANSITIONS:
            regressions.append(row)
        if (old, new) in IMPROVEMENT_TRANSITIONS:
            improvements.append(row)

    return {
        "baseline_counts": counts_from_status(baseline),
        "candidate_counts": counts_from_status(candidate),
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
    if result["regressions"]:
        return "REGRESSION"
    if result["improvements"]:
        return "IMPROVEMENT"
    if result.get("added") or result.get("removed") or result.get("changed"):
        return "CHANGED"
    return "NO_CHANGE"


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
    body = render_summary_text(
        comparison,
        baseline_meta,
        candidate_meta,
        results_repo=args.results_repo,
        results_commit=args.results_commit,
        language="Python",
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
    if expected and actual != expected:
        body = (
            "Python upstream result: ERROR\n\n"
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
            language="Python",
        ).rstrip()
        body += (
            "\n\nMore info:\n"
            f"- compat-tests workflow: {args.run_url}\n"
            f"- compat-tests results commit: https://github.com/{args.results_repo}/commit/{args.results_commit}\n"
            f"- compat-tests branch: https://github.com/{args.results_repo}/tree/{candidate_meta['wasmer']['branch']}\n"
        )

    if args.output:
        write_text(Path(args.output), body + ("\n" if not body.endswith("\n") else ""))
    else:
        print(body, end="" if body.endswith("\n") else "\n")
    return 0


def make_regression_issue_title(candidate_meta: dict[str, Any], language: str = "Python") -> str:
    branch = candidate_meta["wasmer"]["branch"]
    commit = candidate_meta["wasmer"]["commit"][:7]
    return f"{language} upstream regressions on {branch} @ {commit}"


def create_regression_issue(args: argparse.Namespace) -> int:
    comparison = load_json(Path(args.comparison))
    if not comparison.get("regressions"):
        print("No regressions detected; skipping issue creation.", flush=True)
        return 0

    baseline_meta = load_json(Path(args.baseline_metadata))
    candidate_meta = load_json(Path(args.candidate_metadata))
    repo = Path(args.repo).resolve() if args.repo else None
    results_commit = git_head_commit(repo) if repo and (repo / ".git").exists() else None

    body = render_summary_text(
        comparison,
        baseline_meta,
        candidate_meta,
        results_repo=args.results_repo,
        results_commit=results_commit,
        language="Python",
    )
    if args.run_url:
        body += f"\nWorkflow run:\n- {args.run_url}\n"

    title = make_regression_issue_title(candidate_meta, language="Python")
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
    same_identity = (
        branch_metadata.get("wasmer", {}) == metadata.get("wasmer", {})
        and branch_metadata.get("python", {}) == metadata.get("python", {})
        and branch_metadata.get("config", {}) == metadata.get("config", {})
    )
    if branch_status == status and same_identity:
        print("No snapshot changes to publish.", flush=True)
        print(git_head_commit(repo), flush=True)
        return 0

    summary_text = render_summary_text(comparison, compare_meta, metadata, language="Python")
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
    run_python.add_argument("--cpython-version", default=DEFAULT_CPYTHON_VERSION)
    run_python.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    run_python.add_argument("--compare-ref", default="origin/main")
    run_python.add_argument("--debug-test")
    run_python.set_defaults(func=run_python_suite)

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
    comment.add_argument("--output")
    comment.set_defaults(func=render_comment)

    pr_comment = sub.add_parser("prepare-pr-comment", help="Prepare the full PR comment body")
    pr_comment.add_argument("--comparison", required=True)
    pr_comment.add_argument("--baseline-metadata", required=True)
    pr_comment.add_argument("--candidate-metadata", required=True)
    pr_comment.add_argument("--results-repo", required=True)
    pr_comment.add_argument("--results-commit", required=True)
    pr_comment.add_argument("--run-url", required=True)
    pr_comment.add_argument("--expected-wasmer-sha")
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
    issue.set_defaults(func=create_regression_issue)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
