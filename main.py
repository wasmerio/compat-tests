#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
from datetime import datetime, timezone
from typing import Any

from python_upstream import run_python_debug, run_python_upstream

DEFAULT_CPYTHON_VERSION = "v3.13.0"
DEFAULT_TIMEOUT = 1800
RESULT_STATUSES = ("PASS", "FAIL", "SKIP", "TIMEOUT")
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


def parse_debug_unittest_status(test_name: str, output: str, exit_code: int) -> dict[str, str]:
    if "... skipped " in output:
        return {test_name: "SKIP"}
    if OK_RE.search(output) and exit_code == 0:
        return {test_name: "PASS"}
    if FAILED_RE.search(output) or exit_code != 0:
        return {test_name: "FAIL"}
    return {test_name: "TIMEOUT"}


def counts_from_status(status: dict[str, str]) -> dict[str, int]:
    counts = {name: 0 for name in RESULT_STATUSES}
    for value in status.values():
        counts[value] = counts.get(value, 0) + 1
    return counts


def git_head_commit(repo: Path) -> str:
    return run_capture(["git", "rev-parse", "HEAD"], cwd=repo).stdout.strip()


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


def run_python_suite(args: argparse.Namespace) -> int:
    started_at = now_utc()
    output_dir = Path.cwd()
    work_dir = output_dir / ".work"
    work_dir.mkdir(parents=True, exist_ok=True)

    wasmer_checkout = resolve_wasmer_checkout(args, work_dir)
    cpython_checkout = ensure_cpython_checkout(args.cpython_version, work_dir)
    host_test_dir = cpython_checkout / "Lib" / "test"
    patch_faulthandler_workarounds(host_test_dir)

    run(["cargo", "build", "-p", "wasmer-cli", "--features", "llvm", "--release"], cwd=wasmer_checkout)
    wasmer_bin = wasmer_checkout / "target" / "release" / "wasmer"

    if args.debug_test:
        proc = run_python_debug(wasmer_bin=str(wasmer_bin), host_test_dir=host_test_dir, test_name=args.debug_test)
        print(proc.stdout, end="")
        print(proc.stderr, end="")
        status = parse_debug_unittest_status(args.debug_test, proc.stdout + proc.stderr, proc.returncode)
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

    write_json(output_dir / "status.json", status)
    metadata = {
        "wasmer": {
            "ref": args.wasmer_ref,
            "branch": args.wasmer_ref,
            "commit": git_head_commit(wasmer_checkout),
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
    return "NO_CHANGE"


def render_summary_text(
    comparison: dict[str, Any],
    baseline_meta: dict[str, Any],
    candidate_meta: dict[str, Any],
    *,
    results_commit: str | None = None,
    language: str = "Python",
) -> str:
    label = result_label(comparison)
    baseline_counts = comparison.get("baseline_counts", {})
    candidate_counts = comparison.get("candidate_counts", {})

    lines = [
        f"{language} upstream result: {label}",
        "",
        f"Baseline: {baseline_meta['wasmer']['branch']} @ {baseline_meta['wasmer']['commit'][:7]}",
        f"Candidate: {candidate_meta['wasmer']['branch']} @ {candidate_meta['wasmer']['commit'][:7]}",
        "",
        "Summary:",
        f"- Regressions: {len(comparison.get('regressions', []))}",
        f"- Improvements: {len(comparison.get('improvements', []))}",
        f"- PASS: {baseline_counts.get('PASS', 0)} -> {candidate_counts.get('PASS', 0)}",
        f"- FAIL: {baseline_counts.get('FAIL', 0)} -> {candidate_counts.get('FAIL', 0)}",
        f"- TIMEOUT: {baseline_counts.get('TIMEOUT', 0)} -> {candidate_counts.get('TIMEOUT', 0)}",
        f"- SKIP: {baseline_counts.get('SKIP', 0)} -> {candidate_counts.get('SKIP', 0)}",
    ]

    if results_commit:
        lines.extend(["", "Results commit:", f"- {results_commit}"])

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
        results_commit=args.results_commit,
        language="Python",
    )
    if args.output:
        write_text(Path(args.output), body)
    else:
        print(body, end="")
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
        run(["git", "checkout", "-B", branch, f"origin/{branch}"], cwd=repo)
    else:
        run(["git", "checkout", "-B", branch], cwd=repo)


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

    baseline_status = git_file_json(repo, compare_ref, "status.json") if compare_ref else {}
    baseline_meta = git_file_json(repo, compare_ref, "metadata.json") if compare_ref else {}
    comparison = compare_statuses(baseline_status, status)
    if not baseline_meta:
        baseline_meta = {
            "wasmer": {
                "branch": compare_ref or "none",
                "commit": "0000000",
            }
        }

    summary_text = render_summary_text(comparison, baseline_meta, metadata, language="Python")
    write_json(repo / "status.json", status)
    write_json(repo / "metadata.json", metadata)
    write_json(repo / "comparison.json", comparison)
    write_text(repo / "summary.md", summary_text)

    run(["git", "add", "status.json", "metadata.json", "comparison.json", "summary.md"], cwd=repo)
    if not has_staged_changes(repo):
        print("No snapshot changes to publish.", flush=True)
        return 0

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
    run_python.add_argument("--wasmer-checkout")
    run_python.add_argument("--cpython-version", default=DEFAULT_CPYTHON_VERSION)
    run_python.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
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
    comment.add_argument("--results-commit")
    comment.add_argument("--output")
    comment.set_defaults(func=render_comment)

    publish = sub.add_parser("publish-snapshot", help="Publish the current snapshot into a results branch")
    publish.add_argument("--repo", default=".")
    publish.add_argument("--source-dir", default=".")
    publish.add_argument("--branch", required=True)
    publish.add_argument("--compare-ref", default="HEAD")
    publish.add_argument("--git-user-name", default="compat-tests[bot]")
    publish.add_argument("--git-user-email", default="compat-tests[bot]@users.noreply.github.com")
    publish.set_defaults(func=publish_snapshot)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
