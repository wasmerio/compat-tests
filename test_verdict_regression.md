# Shield - Regression 💩💩💩

| Language | Tests  | Pass rate now | PASS | FAIL | TIMEOUT | CRASH |
| -------- | ------ | ------------- | ---- | ---- | ------- | ----- |
| Python | 37,906 | 75.7% | $${\color{red}-10}$$ | $${\color{red}+7}$$ | $${\color{red}+3}$$ | 0 |
| Node.js | 16,024 | 51.1% | $${\color{red}-2}$$ | $${\color{red}+1}$$ | $${\color{red}+1}$$ | 0 |
| PHP | 19,636 | 72.8% | $${\color{red}-96}$$ | $${\color{red}+106}$$ | $${\color{green}-10}$$ | $${\color{red}+3}$$ |
| Rust | 15,423 | 84.8% | 0 | 0 | 0 | 0 |

### Example crash from PHP

- Repro command: `shield run --lang php --wasmer [WASMER BINARY] php-batch-0316`
- Test source: [rename_variation5.phpt](https://github.com/php/php-src/blob/master/ext/standard/tests/file/rename_variation5.phpt)
- Full status file: [status_php.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_php.json)

```text
rust panic: thread 'TokioTaskManager Thread Pool_thread_6' panicked at
lib/wasix/src/syscalls/wasi/path_rename.rs:285:10:
Expected target inode to exist, and it's too late to safely fail: Errno::noent

stack backtrace:
   0: __rustc::rust_begin_unwind
   1: core::panicking::panic_fmt
   2: core::result::unwrap_failed
   3: wasmer_wasix::syscalls::wasi::path_rename::path_rename_internal
   4: wasmer_wasix::syscalls::wasi::path_rename::path_rename
   5: corosensei::coroutine::on_stack::wrapper
   6: stack_call_trampoline
   7: wasmer_vm::trap::traphandlers::on_host_stack

job: php-batch-0316
```

### Example failed test from Python

- Repro command: `shield run --lang python --wasmer [WASMER BINARY] test.test_shutil.TestMove.test_move_symlink_to_file`
- Test source: [test_shutil.py](https://github.com/python/cpython/blob/main/Lib/test/test_shutil.py)
- Status: `PASS -> FAIL`
- Full status file: [status_python.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_python.json)

```text
======================================================================
FAIL: test_move_symlink_to_file (test.test_shutil.TestMove)
----------------------------------------------------------------------
Traceback (most recent call last):
  File "/usr/lib/python3.11/test/test_shutil.py", line 412, in test_move_symlink_to_file
    self.assertTrue(os.path.islink(dst))
AssertionError: False is not true
```

### Example failed test from Node.js

- Repro command: `shield run --lang node --wasmer [WASMER BINARY] parallel/test-fs-symlink.js`
- Test source: [test-fs-symlink.js](https://github.com/nodejs/node/blob/main/test/parallel/test-fs-symlink.js)
- Status: `PASS -> FAIL`
- Full status file: [status_node.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_node.json)

```text
AssertionError [ERR_ASSERTION]: expected symbolic link to exist
    at testValidSymLink (/node/test/parallel/test-fs-symlink.js:81:10)
    at process.processTicksAndRejections (node:internal/process/task_queues:95:5)
```

### More changed tests

- Python: [status_python.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_python.json)
- Node.js: [status_node.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_node.json)
- PHP: [status_php.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_php.json)
- Rust: [status_rust.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_rust.json)

## Install shield

- `git clone https://github.com/wasmerio/compat-tests.git`
- `cd compat-tests`
- `cargo build`
- `./target/debug/shield run --lang <LANG> --wasmer [WASMER BINARY] <TEST OR BATCH>`

## Artifacts

- GitHub Action: [https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID](https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID)
- Results commit: [https://github.com/wasmerio/compat-tests/commit/RESULTS_COMMIT_SHA](https://github.com/wasmerio/compat-tests/commit/RESULTS_COMMIT_SHA)
