# Anti-Regression Shield

Runtime compatibility test harness for Wasmer using upstream language test suites.

Current upstreams:

- Python
- PHP
- Node.js
- Rust

Our time budget for CI runs in Github Action is max 1 hour total time, if job took longer than that - feel free to open an issue.

## Invoke in Wasmer repo

In the `wasmer` repo tests run automatically if PR was created by a maintainer. For forks maintainers can request a test manually: review the PR first then write a comment `/patchsmith test [last-sha-commit-from-fork]`. That dispatches the `pr` workflow against the PR branch and posts a summary comment back on the PR with links to the workflow run and results commit.

Specifying the SHA is a security measure to ensure tests run exactly what maintainers reviewed.

## Run locally

For local development, the fastest path is to reuse your own Wasmer binary directly:

```bash
cargo run -- run --lang python --wasmer ~/wasmer/wasmer2/target/debug/wasmer 
```

Available languages: `python`, `php`, `node`, or `rust`.


## Debug one specific test

Pass a filter after the command args:

```bash
cargo run -- run --lang python --wasmer ~/wasmer/wasmer2/target/debug/wasmer \
  test.test_posixpath.PosixPathTest.test_islink
```

Debug mode prints the raw test output and does not update the status/metadata files.

## Development

The first local run is expensive. Upstream checkouts live under `.work/`, and reusable generated data lives under `.cache/`.

To prepare the heavy dependencies and caches:

```bash
WASMER_BINARY=~/wasmer/target/debug/wasmer cargo test test_dependencies --ignored
```

That helper clones all four upstreams, warms the Wasmer packages, and runs Rust discovery. Rust discovery is intentionally heavy: it builds Rust test harnesses to wasm, precompiles wasm with Wasmer, lists tests, and writes caches under `.cache/rust`. This is the slow path we want to pay once, not on every local run.

The normal run path also caches discovered tests in `.cache/<lang>/tests.json`.

## About the code

This project was vibe coded and it looks like it. There are hacks, patches, wrappers, and language-specific weirdness because the job is to make big upstream test suites run inside Wasmer, not to build a beautiful generic test framework.

That is fine for this repo.

The goal is not 100% upstream tests passing, and not even 100% upstream tests running. The goal is broad useful coverage and a stable signal: if something worked before, we want to know when it breaks.
