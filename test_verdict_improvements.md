# Shield - Improvement 🎉🎉🎉

| Language | Tests  | Pass rate now | PASS | FAIL | TIMEOUT | CRASH |
| -------- | ------ | ------------- | ---- | ---- | ------- | ----- |
| Python | 37,907 | 75.8% | $${\color{green}+435}$$ | $${\color{green}-102}$$ | $${\color{green}-788}$$ | 0 |
| Node.js | 16,030 | 51.2% | $${\color{green}+13}$$ | $${\color{green}-11}$$ | $${\color{green}-2}$$ | 0 |
| PHP | 19,636 | 72.8% | $${\color{green}+3}$$ | $${\color{green}-3}$$ | 0 | 0 |
| Rust | 15,421 | 84.9% | $${\color{green}+2}$$ | $${\color{green}-2}$$ | 0 | 0 |

- Examples from [tests_python_results.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/tests_python_results.json):
  - `test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later` (`TIMEOUT -> PASS`)
  - `test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later_negative_delays` (`TIMEOUT -> PASS`)
  - `test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_soon` (`TIMEOUT -> PASS`)
  - `test.test_docxmlrpc.DocXMLRPCHTTPGETServer.test_get_css` (`FAIL -> PASS`)
  - `test.test_docxmlrpc.DocXMLRPCHTTPGETServer.test_invalid_get_response` (`FAIL -> PASS`)

- Examples from [tests_php_results.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/tests_php_results.json):
  - `ext/standard/tests/strings/trim_basic.phpt` (`FAIL -> PASS`)
  - `ext/standard/tests/strings/strval_basic.phpt` (`FAIL -> PASS`)
  - `ext/standard/tests/file/stream_copy_to_stream_empty.phpt` (`FAIL -> PASS`)
  - `ext/standard/tests/file/statpage.phpt` (`TIMEOUT -> PASS`)
  - `ext/standard/tests/file/stream_supports_lock.phpt` (`FAIL -> PASS`)

- Examples from [tests_rust_results.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/tests_rust_results.json):
  - `env::home_dir_with_relative_input` (`FAIL -> PASS`)
  - `fs::canonicalize_handles_symlink_loop` (`FAIL -> PASS`)
  - `process::command_preserves_exit_code` (`TIMEOUT -> PASS`)
  - `net::tcp_listener_reuseaddr` (`FAIL -> PASS`)
  - `path::strip_prefix_handles_root` (`FAIL -> PASS`)


## Artifacts

- GitHub Action: [https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID](https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID)
- Results commit: [https://github.com/wasmerio/compat-tests/commit/RESULTS_COMMIT_SHA](https://github.com/wasmerio/compat-tests/commit/RESULTS_COMMIT_SHA)
