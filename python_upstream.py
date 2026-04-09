import concurrent.futures
import os
from pathlib import Path
import subprocess

DISCOVER_CODE = """\
import sys,unittest
job = sys.argv[1]
def walk(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from walk(item)
        else:
            test_id = item.id()
            if not test_id.startswith("unittest.loader."):
                print(test_id, flush=True)
try:
    suite = unittest.defaultTestLoader.loadTestsFromName(job)
except unittest.SkipTest:
    print(job, flush=True)
    raise SystemExit(0)
for _ in walk(suite):
    pass
"""

RUN_CODE = """\
import os,sys,unittest
job = sys.argv[1]
def walk(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from walk(item)
        else:
            test_id = item.id()
            if not test_id.startswith("unittest.loader."):
                print("CASE", test_id, flush=True)
                yield test_id
try:
    suite = unittest.defaultTestLoader.loadTestsFromName(job)
except unittest.SkipTest:
    print("SKIP", job, flush=True)
    raise SystemExit(0)
cases = list(walk(suite))
class Result(unittest.TextTestResult):
    def _mark(self, status, test):
        test_id = test.id()
        if not test_id.startswith("unittest.loader."):
            print(status, test_id, flush=True)
    def addSuccess(self, test): super().addSuccess(test); self._mark("PASS", test)
    def addFailure(self, test, err): super().addFailure(test, err); self._mark("FAIL", test)
    def addError(self, test, err): super().addError(test, err); self._mark("FAIL", test)
    def addSkip(self, test, reason): super().addSkip(test, reason); self._mark("SKIP", test)
    def addExpectedFailure(self, test, err): super().addExpectedFailure(test, err); self._mark("FAIL", test)
    def addUnexpectedSuccess(self, test): super().addUnexpectedSuccess(test); self._mark("FAIL", test)
stream = open(os.devnull, "w")
result = unittest.TextTestRunner(stream=stream, verbosity=0, resultclass=Result).run(suite)
stream.close()
raise SystemExit(0 if result.wasSuccessful() else 1)
"""


def guest_test_dir(wasmer_bin: str) -> str:
    out = subprocess.run(
        [
            wasmer_bin,
            "run",
            "python/python",
            "--",
            "-c",
            'import sys; print(f"/usr/local/lib/python{sys.version_info.major}.{sys.version_info.minor}/test")',
        ],
        check=True,
        text=True,
        capture_output=True,
    )
    return out.stdout.strip()


def find_jobs(testdir: Path) -> list[str]:
    jobs = []
    for entry in sorted(testdir.iterdir(), key=lambda p: p.name):
        mod = entry.stem if entry.is_file() else entry.name
        if not mod.startswith("test_"):
            continue
        if entry.is_dir() or entry.suffix == ".py":
            jobs.append(f"test.{mod}")
    return jobs


def discover_job(job: str, wasmer_bin: str, host_test_dir: Path, guest_test_dir: str, timeout: int) -> tuple[str, list[str]]:
    cmd = [
        wasmer_bin,
        "run",
        "--net",
        "python/python",
        "--volume",
        f"{host_test_dir}:{guest_test_dir}",
        "--",
        "-c",
        DISCOVER_CODE,
        job,
    ]
    try:
        proc = subprocess.run(cmd, text=True, capture_output=True, timeout=timeout)
        output = proc.stdout
    except subprocess.TimeoutExpired:
        return job, [job]
    names = sorted({line.strip() for line in output.splitlines() if line.strip() and not line.startswith("unittest.loader.")})
    if not names and proc.returncode:
        names = [job]
    return job, names


def run_job(job: str, expected: list[str], wasmer_bin: str, host_test_dir: Path, guest_test_dir: str, timeout: int) -> tuple[str, list[str], list[str], list[str], list[str]]:
    cmd = [
        wasmer_bin,
        "run",
        "--net",
        "python/python",
        "--volume",
        f"{host_test_dir}:{guest_test_dir}",
        "--",
        "-c",
        RUN_CODE,
        job,
    ]
    timed_out = False
    try:
        proc = subprocess.run(cmd, text=True, capture_output=True, timeout=timeout)
        output = proc.stdout
    except subprocess.TimeoutExpired as exc:
        output = (exc.stdout.decode() if isinstance(exc.stdout, bytes) else exc.stdout) or ""
        proc = None
        timed_out = True
    passed, failed, skipped = set(), set(), set()
    for line in output.splitlines():
        if line.startswith("PASS "):
            passed.add(line[5:].strip())
        elif line.startswith("FAIL "):
            failed.add(line[5:].strip())
        elif line.startswith("SKIP "):
            skipped.add(line[5:].strip())
    known = set(expected)
    if timed_out:
        timed_out_names = known - passed - failed - skipped
        if not known:
            timed_out_names = {job}
        return job, sorted(passed), sorted(failed), sorted(skipped), sorted(timed_out_names)
    if not known and proc and proc.returncode:
        failed.add(job)
    else:
        failed.update(name for name in known if name not in passed and name not in failed and name not in skipped)
    return job, sorted(passed), sorted(failed), sorted(skipped), []


def run_python_debug(*, wasmer_bin: str, host_test_dir: Path, test_name: str, timeout: int | None = None) -> subprocess.CompletedProcess[str]:
    guest_dir = guest_test_dir(wasmer_bin)
    cmd = [
        wasmer_bin,
        "run",
        "--net",
        "python/python",
        "--volume",
        f"{host_test_dir}:{guest_dir}",
        "--",
        "-m",
        "unittest",
        "-v",
        test_name,
    ]
    print(" ".join(cmd), flush=True)
    return subprocess.run(cmd, text=True, capture_output=True, timeout=timeout)


def run_python_upstream(*, wasmer_bin: str, host_test_dir: Path, timeout: int) -> dict[str, str]:
    guest_dir = guest_test_dir(wasmer_bin)
    jobs_list = find_jobs(host_test_dir)
    workers = (getattr(os, "process_cpu_count", os.cpu_count)() or 1) + 2
    workers = min(workers, len(jobs_list))

    discovered: dict[str, list[str]] = {}
    print(f"Discovering leaf tests in {len(jobs_list)} modules with {workers} workers...", flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {
            pool.submit(discover_job, job, wasmer_bin, host_test_dir, guest_dir, max(timeout, 2)): job
            for job in jobs_list
        }
        completed = 0
        for future in concurrent.futures.as_completed(futures):
            job, names = future.result()
            if names:
                discovered[job] = names
            completed += 1
            if completed % 25 == 0 or completed == len(jobs_list):
                print(f"Discovered {completed}/{len(jobs_list)} modules", flush=True)

    total_cases = sum(len(names) for names in discovered.values())
    print(f"Running {len(discovered)} module jobs covering {total_cases} tests with {workers} workers...", flush=True)

    status: dict[str, str] = {}
    completed_cases = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {
            pool.submit(run_job, job, discovered[job], wasmer_bin, host_test_dir, guest_dir, max(timeout, 2)): job
            for job in discovered
        }
        for future in concurrent.futures.as_completed(futures):
            _, job_pass, job_fail, job_skip, job_timeout = future.result()
            for name in job_pass:
                completed_cases += 1
                print(f"[{completed_cases}/{total_cases}] {name} PASS", flush=True)
                status[name] = "PASS"
            for name in job_fail:
                completed_cases += 1
                print(f"[{completed_cases}/{total_cases}] {name} FAIL", flush=True)
                status[name] = "FAIL"
            for name in job_skip:
                completed_cases += 1
                print(f"[{completed_cases}/{total_cases}] {name} SKIP", flush=True)
                status[name] = "SKIP"
            for name in job_timeout:
                completed_cases += 1
                print(f"[{completed_cases}/{total_cases}] {name} TIMEOUT", flush=True)
                status[name] = "TIMEOUT"
    return dict(sorted(status.items()))
