# Shield - Improvement 🎉🎉🎉

| Language | Tests  | Pass rate now | PASS | FAIL | TIMEOUT | CRASH |
| -------- | ------ | ------------- | ---- | ---- | ------- | ----- |
| Python | 37,907 | 75.8% | $${\color{green}+435}$$ | $${\color{green}-102}$$ | $${\color{green}-788}$$ | 0 |
| Node.js | 16,030 | 51.2% | $${\color{green}+13}$$ | $${\color{green}-11}$$ | $${\color{green}-2}$$ | 0 |
| PHP | 19,636 | 72.8% | $${\color{green}+3}$$ | $${\color{green}-3}$$ | 0 | 0 |
| Rust | 15,421 | 84.9% | $${\color{green}+2}$$ | $${\color{green}-2}$$ | 0 | 0 |

- Examples from [status_python.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_python.json):
  - `test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later` (`TIMEOUT -> PASS`)
  - `test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_later_negative_delays` (`TIMEOUT -> PASS`)
  - `test.test_asyncio.test_base_events.BaseEventLoopTests.test_call_soon` (`TIMEOUT -> PASS`)
  - `test.test_docxmlrpc.DocXMLRPCHTTPGETServer.test_get_css` (`FAIL -> PASS`)
  - `test.test_docxmlrpc.DocXMLRPCHTTPGETServer.test_invalid_get_response` (`FAIL -> PASS`)

- Examples from [status_node.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_node.json):
  - `parallel/test-fs-stat.js` (`FAIL -> PASS`)
  - `parallel/test-fs-symlink-dir-junction-relative.js` (`FAIL -> PASS`)
  - `parallel/test-stream2-httpclient-response-end.js` (`TIMEOUT -> PASS`)
  - `parallel/test-http2-server-destroy-before-write.js` (`FAIL -> PASS`)
  - `parallel/test-whatwg-url-custom-searchparams-stringifier.js` (`TIMEOUT -> PASS`)

- Examples from [status_php.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_php.json):
  - `ext/standard/tests/strings/trim_basic.phpt` (`FAIL -> PASS`)
  - `ext/standard/tests/strings/strval_basic.phpt` (`FAIL -> PASS`)
  - `ext/standard/tests/file/stream_copy_to_stream_empty.phpt` (`FAIL -> PASS`)
  - `ext/standard/tests/file/statpage.phpt` (`TIMEOUT -> PASS`)
  - `ext/standard/tests/file/stream_supports_lock.phpt` (`FAIL -> PASS`)

- Examples from [status_rust.json](https://github.com/wasmerio/compat-tests/blob/RESULTS_COMMIT_SHA/status_rust.json):
  - `env::home_dir_with_relative_input` (`FAIL -> PASS`)
  - `fs::canonicalize_handles_symlink_loop` (`FAIL -> PASS`)
  - `process::command_preserves_exit_code` (`TIMEOUT -> PASS`)
  - `net::tcp_listener_reuseaddr` (`FAIL -> PASS`)
  - `path::strip_prefix_handles_root` (`FAIL -> PASS`)


## Artifacts

- GitHub Action: [https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID](https://github.com/wasmerio/compat-tests/actions/runs/RUN_ID)
- Results commit: [https://github.com/wasmerio/compat-tests/commit/RESULTS_COMMIT_SHA](https://github.com/wasmerio/compat-tests/commit/RESULTS_COMMIT_SHA)
