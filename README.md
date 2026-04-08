# compat-tests

Runtime compatibility test harness for Wasmer using upstream language test suites.

Current first backend:

- Python / upstream CPython tests

## Invoke in Wasmer repo

In the `wasmer` repo, you can trigger this from a PR by commenting `/patchsmith test`. That dispatches the compat-tests workflow against the PR branch and posts a summary comment back on the PR with links to the workflow run and results commit.

## Run locally
For local development, the fastest path is to reuse your own Wasmer binary directly:

```bash
python3 main.py run-python --wasmer-bin ~/wasmer/wasmer/target/debug/wasmer
```

That runs the full Python upstream suite and updates [status.json](~/wasmer/compat-tests/status.json) and [metadata.json](~/wasmer/compat-tests/metadata.json), so you can inspect the diff and see the impact of your Wasmer changes before you commit anything in the main repo.

## Debug one specific test

For quick investigation of one test, use debug mode:

```bash
python3 main.py run-python --wasmer-bin ~/wasmer/wasmer/target/debug/wasmer --debug-test test.test_posixpath.PosixPathTest.test_islink
```

Debug mode prints the raw test output and exits with the test status, but it does not update `status.json` or `metadata.json`.
