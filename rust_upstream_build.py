#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_RUST_REPO_URL = "https://github.com/wasix-org/rust.git"
DEFAULT_RUST_REF = "v2025-11-07.1+rust-1.90"
DEFAULT_WORK_DIR = Path(".work/rust-upstream")
DEFAULT_TARGET = "wasm32-wasmer-wasi"
DEFAULT_TOOLCHAIN = "wasix"
DEFAULT_TIMEOUT = 1800
DEFAULT_REPORT = Path(".work/rust-upstream/build-report.json")
DEFAULT_LOG = Path(".work/rust-upstream/build.log")
DEFAULT_GIT_CONFIG = Path(".work/rust-upstream/gitconfig")
DEFAULT_CARGO_PATCH_CONFIG = Path(".work/rust-upstream/cargo-patches.toml")
DEFAULT_WASIX_SYSROOT_LINK = Path(".work/rust-upstream/wasix-sysroot32")
GENMC_REPO_URL = "https://github.com/MPI-SWS/genmc.git"
GENMC_COMMIT = "3438dd2c1202cd4a47ed7881d099abf23e4167ab"

WORKSPACES = (
    ("root", "."),
    ("library", "library"),
    ("stdarch", "library/stdarch"),
    ("portable-simd", "library/portable-simd"),
    ("compiler-builtins", "library/compiler-builtins"),
    ("miri-test-cargo-miri", "src/tools/miri/test-cargo-miri"),
)

LIBRARY_BUILD_ONLY_PACKAGES = {
    "proc_macro",
    "std",
    "std_detect",
    "test",
    "unwind",
}

SUBMODULES = (
    "library/backtrace",
    "library/compiler-builtins",
    "library/portable-simd",
    "library/stdarch",
    "src/llvm-project",
)

LOCK_UPDATES = (
    ("library/portable-simd", "wasm-bindgen", "0.2.100"),
    ("library/portable-simd", "wasm-bindgen-futures", "0.4.50"),
    ("library/portable-simd", "wasm-bindgen-test", "0.3.50"),
)

LOCAL_COMPILER_CRATES = {
    "rustc_abi",
    "rustc_arena",
    "rustc_ast",
    "rustc_ast_pretty",
    "rustc_attr_parsing",
    "rustc_const_eval",
    "rustc_data_structures",
    "rustc_driver",
    "rustc_errors",
    "rustc_expand",
    "rustc_hir",
    "rustc_hir_analysis",
    "rustc_hir_pretty",
    "rustc_hir_typeck",
    "rustc_index",
    "rustc_infer",
    "rustc_interface",
    "rustc_lexer",
    "rustc_lint",
    "rustc_lint_defs",
    "rustc_log",
    "rustc_metadata",
    "rustc_middle",
    "rustc_mir_dataflow",
    "rustc_parse",
    "rustc_parse_format",
    "rustc_resolve",
    "rustc_session",
    "rustc_span",
    "rustc_symbol_mangling",
    "rustc_target",
    "rustc_trait_selection",
}

TOOL_RUSTC_PRIVATE_DEPS = {
    "src/tools/clippy/Cargo.toml": {
        "rustc_driver",
        "rustc_interface",
        "rustc_session",
        "rustc_span",
    },
    "src/tools/clippy/clippy_config/Cargo.toml": {
        "rustc_data_structures",
        "rustc_errors",
        "rustc_hir",
        "rustc_middle",
        "rustc_session",
        "rustc_span",
    },
    "src/tools/clippy/clippy_dev/Cargo.toml": {
        "rustc-literal-escaper",
        "rustc_driver",
        "rustc_lexer",
    },
    "src/tools/clippy/clippy_lints/Cargo.toml": {
        "pulldown-cmark",
        "rustc_abi",
        "rustc_arena",
        "rustc_ast",
        "rustc_ast_pretty",
        "rustc_data_structures",
        "rustc_driver",
        "rustc_errors",
        "rustc_hir",
        "rustc_hir_analysis",
        "rustc_hir_pretty",
        "rustc_hir_typeck",
        "rustc_index",
        "rustc_infer",
        "rustc_lexer",
        "rustc_lint",
        "rustc_middle",
        "rustc_parse",
        "rustc_parse_format",
        "rustc_resolve",
        "rustc_session",
        "rustc_span",
        "rustc_target",
        "rustc_trait_selection",
        "smallvec",
        "thin-vec",
    },
    "src/tools/clippy/clippy_lints_internal/Cargo.toml": {
        "rustc_ast",
        "rustc_attr_parsing",
        "rustc_data_structures",
        "rustc_errors",
        "rustc_hir",
        "rustc_lint",
        "rustc_lint_defs",
        "rustc_middle",
        "rustc_session",
        "rustc_span",
    },
    "src/tools/clippy/clippy_utils/Cargo.toml": {
        "indexmap",
        "rustc_abi",
        "rustc_ast",
        "rustc_attr_parsing",
        "rustc_const_eval",
        "rustc_data_structures",
        "rustc_driver",
        "rustc_errors",
        "rustc_hir",
        "rustc_hir_analysis",
        "rustc_hir_typeck",
        "rustc_index",
        "rustc_infer",
        "rustc_lexer",
        "rustc_lint",
        "rustc_middle",
        "rustc_mir_dataflow",
        "rustc_session",
        "rustc_span",
        "rustc_trait_selection",
        "smallvec",
    },
    "src/tools/clippy/declare_clippy_lint/Cargo.toml": {
        "rustc_lint",
        "rustc_session",
    },
    "src/tools/miri/Cargo.toml": {
        "either",
        "rustc_abi",
        "rustc_apfloat",
        "rustc_ast",
        "rustc_const_eval",
        "rustc_data_structures",
        "rustc_driver",
        "rustc_errors",
        "rustc_hir",
        "rustc_hir_analysis",
        "rustc_index",
        "rustc_interface",
        "rustc_log",
        "rustc_metadata",
        "rustc_middle",
        "rustc_session",
        "rustc_span",
        "rustc_symbol_mangling",
        "rustc_target",
        "tracing",
    },
    "src/tools/rustfmt/Cargo.toml": {
        "rustc_ast",
        "rustc_ast_pretty",
        "rustc_data_structures",
        "rustc_driver",
        "rustc_errors",
        "rustc_expand",
        "rustc_parse",
        "rustc_session",
        "rustc_span",
        "thin-vec",
    },
}

CRATES_IO_DEP_SPECS = {
    "either": '"1.15"',
    "indexmap": '"2.10"',
    "pulldown-cmark": '{ version = "0.11", default-features = false, features = ["html"] }',
    "rustc_apfloat": '"0.2.0"',
    "rustc-literal-escaper": '"0.0.5"',
    "smallvec": '"1.15"',
    "thin-vec": '"0.2.14"',
    "tracing": '{ version = "0.1", default-features = false, features = ["std"] }',
}

DEPENDENCY_FORKS = {
    "curl": {
        "repo": "https://github.com/alexcrichton/curl-rust.git",
        "ref": "0.4.48",
        "cargo_toml_replacements": (
            (r'(?m)^default\s*=\s*\["ssl"\]', 'default = []'),
        ),
        "file_replacements": {
            "curl-sys/lib.rs": (
                (r'#\[cfg\(unix\)\]', '#[cfg(any(unix, target_os = "wasi"))]'),
            ),
            "src/easy/form.rs": (
                (r'#\[cfg\(unix\)\]', '#[cfg(any(unix, target_os = "wasi"))]'),
                (
                    r'use std::os::unix::prelude::\*;',
                    '#[cfg(unix)]\n        use std::os::unix::prelude::*;\n        #[cfg(target_os = "wasi")]\n        use std::os::wasi::prelude::*;',
                ),
            ),
            "src/easy/handler.rs": (
                (r'#\[cfg\(unix\)\]', '#[cfg(any(unix, target_os = "wasi"))]'),
                (
                    r'use std::os::unix::prelude::\*;',
                    '#[cfg(unix)]\n            use std::os::unix::prelude::*;\n            #[cfg(target_os = "wasi")]\n            use std::os::wasi::prelude::*;',
                ),
            ),
            "src/multi.rs": (
                (r'#\[cfg\(unix\)\]', '#[cfg(any(unix, target_os = "wasi"))]'),
                (
                    r'use libc::\{pollfd, POLLIN, POLLOUT, POLLPRI\};',
                    'use libc::{pollfd, POLLIN, POLLOUT};\n#[cfg(unix)]\nuse libc::POLLPRI;\n#[cfg(target_os = "wasi")]\nconst POLLPRI: libc::c_short = POLLIN;',
                ),
            ),
        },
    },
    "getrandom": {
        "repo": "https://github.com/wasix-org/getrandom.git",
        "ref": "wasix-0.3.3",
    },
    "home": {
        "repo": "https://github.com/wasix-org/home.git",
        "ref": "wasix-0.5.11",
    },
    "indicatif": {
        "repo": "https://github.com/console-rs/indicatif.git",
        "ref": "0.18.4",
        "file_replacements": {
            "src/lib.rs": (
                (
                    r'#\[cfg\(all\(target_arch = "wasm32", not\(feature = "wasmbind"\)\)\)\]',
                    '#[cfg(all(target_arch = "wasm32", not(target_os = "wasi"), not(feature = "wasmbind")))]',
                ),
            ),
            "src/draw_target.rs": (
                (r'#\[cfg\(not\(target_arch = "wasm32"\)\)\]\nuse std::time::Instant;', '#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]\nuse std::time::Instant;'),
                (r'#\[cfg\(all\(target_arch = "wasm32", feature = "wasmbind"\)\)\]', '#[cfg(all(target_arch = "wasm32", not(target_os = "wasi"), feature = "wasmbind"))]'),
            ),
            "src/multi.rs": (
                (r'#\[cfg\(not\(target_arch = "wasm32"\)\)\]\nuse std::time::Instant;', '#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]\nuse std::time::Instant;'),
                (r'#\[cfg\(all\(target_arch = "wasm32", feature = "wasmbind"\)\)\]', '#[cfg(all(target_arch = "wasm32", not(target_os = "wasi"), feature = "wasmbind"))]'),
            ),
            "src/progress_bar.rs": (
                (r'#\[cfg\(not\(target_arch = "wasm32"\)\)\]\nuse std::time::Instant;', '#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]\nuse std::time::Instant;'),
                (r'#\[cfg\(all\(target_arch = "wasm32", feature = "wasmbind"\)\)\]', '#[cfg(all(target_arch = "wasm32", not(target_os = "wasi"), feature = "wasmbind"))]'),
            ),
            "src/state.rs": (
                (r'#\[cfg\(not\(target_arch = "wasm32"\)\)\]\nuse std::time::Instant;', '#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]\nuse std::time::Instant;'),
                (r'#\[cfg\(all\(target_arch = "wasm32", feature = "wasmbind"\)\)\]', '#[cfg(all(target_arch = "wasm32", not(target_os = "wasi"), feature = "wasmbind"))]'),
            ),
            "src/style.rs": (
                (r'#\[cfg\(not\(target_arch = "wasm32"\)\)\]\nuse std::time::Instant;', '#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]\nuse std::time::Instant;'),
                (r'#\[cfg\(all\(target_arch = "wasm32", feature = "wasmbind"\)\)\]', '#[cfg(all(target_arch = "wasm32", not(target_os = "wasi"), feature = "wasmbind"))]'),
            ),
        },
    },
    "libc": {
        "repo": "https://github.com/wasix-org/libc.git",
        "ref": "wasix-0.2.169",
        "version": "0.2.174",
        "file_replacements": {
            "src/wasi/mod.rs": (
                (r'feature = "rustc-dep-of-std"', 'all(feature = "rustc-dep-of-std", not(target_os = "wasi"))'),
            ),
            "src/wasi/wasix.rs": (
                (r'feature = "rustc-dep-of-std"', 'all(feature = "rustc-dep-of-std", not(target_os = "wasi"))'),
            ),
        },
    },
    "libc-git": {
        "repo": "https://github.com/wasix-org/libc.git",
        "ref": "wasix-0.2.169",
        "patch_name": "libc",
        "patch_source": "https://github.com/wasix-org/libc.git",
        "file_replacements": {
            "src/wasi/mod.rs": (
                (r'feature = "rustc-dep-of-std"', 'all(feature = "rustc-dep-of-std", not(target_os = "wasi"))'),
            ),
            "src/wasi/wasix.rs": (
                (r'feature = "rustc-dep-of-std"', 'all(feature = "rustc-dep-of-std", not(target_os = "wasi"))'),
            ),
        },
    },
    "libloading": {
        "repo": "https://github.com/nagisa/rust_libloading.git",
        "ref": "0.8.8",
        "file_replacements": {
            "Cargo.toml": (
                (
                    r"\[target\.'cfg\(unix\)'\.dependencies\.cfg-if\]",
                    '[target."cfg(any(unix, target_os = \\"wasi\\"))".dependencies.cfg-if]',
                ),
            ),
            "src/lib.rs": (
                (r'any\(unix, windows\)', 'any(unix, target_os = "wasi", windows)'),
                (r'any\(unix, windows, libloading_docs\)', 'any(unix, target_os = "wasi", windows, libloading_docs)'),
            ),
            "src/os/mod.rs": (
                (r'any\(unix, libloading_docs\)', 'any(unix, target_os = "wasi", libloading_docs)'),
            ),
            "src/os/unix/mod.rs": (
                (
                    r'#\[cfg\(all\(libloading_docs, not\(unix\)\)\)\]\nmod unix_imports \{\}\n#\[cfg\(any\(not\(libloading_docs\), unix\)\)\]\nmod unix_imports \{\n    pub\(super\) use std::os::unix::ffi::OsStrExt;\n\}',
                    '#[cfg(all(libloading_docs, not(any(unix, target_os = "wasi"))))]\nmod unix_imports {}\n#[cfg(all(not(libloading_docs), unix))]\nmod unix_imports {\n    pub(super) use std::os::unix::ffi::OsStrExt;\n}\n#[cfg(all(not(libloading_docs), target_os = "wasi"))]\nmod unix_imports {\n    pub(super) use std::os::wasi::ffi::OsStrExt;\n}',
                ),
                (
                    r'#\[cfg_attr\(any\(target_os = "linux", target_os = "android"\), link\(name = "dl"\)\)\]',
                    '#[cfg_attr(any(target_os = "linux", target_os = "android", target_os = "wasi"), link(name = "dl"))]',
                ),
            ),
            "src/safe.rs": (
                (
                    r'#\[cfg\(all\(not\(libloading_docs\), unix\)\)\]\nuse super::os::unix as imp;',
                    '#[cfg(all(not(libloading_docs), any(unix, target_os = "wasi")))]\nuse super::os::unix as imp;',
                ),
                (
                    r'#\[cfg_attr\(libloading_docs, doc\(cfg\(any\(unix, windows\)\)\)\)\]',
                    '#[cfg_attr(libloading_docs, doc(cfg(any(unix, target_os = "wasi", windows))))]',
                ),
            ),
            "src/os/unix/consts.rs": (
                (r'#\[cfg\(any\(not\(libloading_docs\), unix\)\)\]', '#[cfg(any(not(libloading_docs), unix, target_os = "wasi"))]'),
                (
                    r'target_os = "emscripten",\n',
                    'target_os = "emscripten",\n            target_os = "wasi",\n',
                ),
            ),
        },
    },
    "socket2": {
        "repo": "https://github.com/wasix-org/socket2.git",
        "ref": "v0.5.5",
        "version": "0.5.10",
        "cargo_toml_replacements": (
            (r'(?m)^version\s*=\s*"0\.5\.10"\nfeatures =', 'version = "0.52.0"\nfeatures ='),
        ),
    },
    "syn": {
        "repo": "https://github.com/dtolnay/syn.git",
        "ref": "2.0.104",
        "cargo_toml_replacements": (
            (r'(?m)^full\s*=\s*\[\]', 'full = ["visit-mut"]'),
        ),
    },
}

RUST_SOURCE_FIXUPS = {
    "src/tools/compiletest/src/read2.rs": (
        (r'pub fn read2\(\n        out_pipe: ChildStdout,\n        err_pipe: ChildStderr,', 'pub fn read2(\n        mut out_pipe: ChildStdout,\n        mut err_pipe: ChildStderr,'),
    ),
    "src/tools/tidy/src/bins.rs": (
        (r'#\[cfg\(windows\)\]\nmod os_impl', '#[cfg(any(windows, target_os = "wasi"))]\nmod os_impl'),
    ),
    "src/tools/opt-dist/src/environment.rs": (
        (r'#\[cfg\(target_family = "unix"\)\]\npub fn executable_extension', '#[cfg(any(target_family = "unix", target_os = "wasi"))]\npub fn executable_extension'),
    ),
    "src/tools/remote-test-server/src/main.rs": (
        (r'#\[cfg\(not\(windows\)\)\]\nfn get_status_code', '#[cfg(all(not(windows), not(target_os = "wasi")))]\nfn get_status_code'),
        (
            r'#\[cfg\(windows\)\]\nfn get_status_code\(status: &ExitStatus\) -> \(u8, i32\) \{\n    \(0, status.code\(\).unwrap\(\)\)\n\}',
            '#[cfg(any(windows, target_os = "wasi"))]\nfn get_status_code(status: &ExitStatus) -> (u8, i32) {\n    (0, status.code().unwrap_or(1))\n}',
        ),
        (r'#\[cfg\(not\(windows\)\)\]\nfn set_permissions', '#[cfg(all(not(windows), not(target_os = "wasi")))]\nfn set_permissions'),
        (r'#\[cfg\(windows\)\]\nfn set_permissions', '#[cfg(any(windows, target_os = "wasi"))]\nfn set_permissions'),
    ),
    "src/tools/rust-installer/src/util.rs": (
        (r'#\[cfg\(unix\)\]\nuse std::os::unix::fs::OpenOptionsExt;', '#[cfg(unix)]\nuse std::os::unix::fs::OpenOptionsExt;\n#[cfg(target_os = "wasi")]\nuse std::os::wasi::fs::OpenOptionsExt;'),
        (r'#\[cfg\(unix\)\]\nuse std::os::unix::fs::symlink as symlink_file;', '#[cfg(unix)]\nuse std::os::unix::fs::symlink as symlink_file;\n#[cfg(target_os = "wasi")]\nuse std::os::wasi::fs::symlink_path as symlink_file;'),
    ),
    "src/tools/rust-installer/src/lib.rs": (
        (r'^#\[macro_use\]', '#![cfg_attr(target_os = "wasi", feature(wasi_ext))]\n\n#[macro_use]'),
    ),
    "src/bootstrap/src/lib.rs": (
        (r'#\[cfg\(unix\)\]\n        use std::os::unix::fs::symlink as symlink_file;', '#[cfg(unix)]\n        use std::os::unix::fs::symlink as symlink_file;\n        #[cfg(target_os = "wasi")]\n        use std::os::wasi::fs::symlink_path as symlink_file;'),
        (r'#\[cfg\(unix\)\]\nfn chmod', '#[cfg(all(unix, not(target_os = "wasi")))]\nfn chmod'),
        (r'#\[cfg\(windows\)\]\nfn chmod', '#[cfg(any(windows, target_os = "wasi"))]\nfn chmod'),
    ),
    "compiler/rustc_driver/Cargo.toml": (
        (r'crate-type = \["dylib"\]', 'crate-type = ["rlib"]'),
    ),
    "compiler/rustc_fs_util/src/lib.rs": (
        (r'all\(target_os = "wasi", target_env = "p1"\)', 'target_os = "wasi"'),
    ),
    "src/librustdoc/Cargo.toml": (
        (
            r'rustdoc-json-types = \{ path = "../rustdoc-json-types" \}',
            'pulldown-cmark = "0.11.3"\nrustc_abi = { path = "../../compiler/rustc_abi" }\nrustc_ast = { path = "../../compiler/rustc_ast" }\nrustc_ast_pretty = { path = "../../compiler/rustc_ast_pretty" }\nrustc_attr_parsing = { path = "../../compiler/rustc_attr_parsing" }\nrustc_data_structures = { path = "../../compiler/rustc_data_structures" }\nrustc_driver = { path = "../../compiler/rustc_driver" }\nrustc_errors = { path = "../../compiler/rustc_errors" }\nrustc_expand = { path = "../../compiler/rustc_expand" }\nrustc_feature = { path = "../../compiler/rustc_feature" }\nrustc_hir = { path = "../../compiler/rustc_hir" }\nrustc_hir_analysis = { path = "../../compiler/rustc_hir_analysis" }\nrustc_hir_pretty = { path = "../../compiler/rustc_hir_pretty" }\nrustc_index = { path = "../../compiler/rustc_index" }\nrustc_infer = { path = "../../compiler/rustc_infer" }\nrustc_interface = { path = "../../compiler/rustc_interface" }\nrustc_lexer = { path = "../../compiler/rustc_lexer" }\nrustc_lint = { path = "../../compiler/rustc_lint" }\nrustc_lint_defs = { path = "../../compiler/rustc_lint_defs" }\nrustc_log = { path = "../../compiler/rustc_log" }\nrustc_macros = { path = "../../compiler/rustc_macros" }\nrustc_metadata = { path = "../../compiler/rustc_metadata" }\nrustc_middle = { path = "../../compiler/rustc_middle" }\nrustc_parse = { path = "../../compiler/rustc_parse" }\nrustc_passes = { path = "../../compiler/rustc_passes" }\nrustc_resolve = { path = "../../compiler/rustc_resolve" }\nrustc_serialize = { path = "../../compiler/rustc_serialize" }\nrustc_session = { path = "../../compiler/rustc_session" }\nrustc_span = { path = "../../compiler/rustc_span" }\nrustc_target = { path = "../../compiler/rustc_target" }\nrustc_trait_selection = { path = "../../compiler/rustc_trait_selection" }\nrustdoc-json-types = { path = "../rustdoc-json-types" }\nthin-vec = "0.2.14"',
        ),
    ),
    "src/tools/x/src/main.rs": (
        (r'#\[cfg\(unix\)\]\nfn x_command', '#[cfg(any(unix, target_os = "wasi"))]\nfn x_command'),
        (r'#\[cfg\(not\(any\(windows, unix\)\)\)\]\nfn x_command', '#[cfg(not(any(windows, unix, target_os = "wasi")))]\nfn x_command'),
    ),
    "src/tools/miri/test-cargo-miri/build.rs": (
        (
            r'assert!\(env::var_os\("CARGO_CFG_MIRI"\)\.is_some\(\), "cargo failed to tell us about `--cfg miri`"\);',
            'if env::var_os("TARGET").as_deref() != Some(std::ffi::OsStr::new("wasm32-wasmer-wasi")) {\n        assert!(env::var_os("CARGO_CFG_MIRI").is_some(), "cargo failed to tell us about `--cfg miri`");\n    }',
        ),
    ),
    "src/tools/clippy/clippy_dev/src/lib.rs": (
        (r'rustc_private,', 'rustc_private,\n    wasi_ext,'),
    ),
    "src/tools/clippy/clippy_dev/src/setup/toolchain.rs": (
        (r'#\[cfg\(not\(windows\)\)\]\n    use std::os::unix::fs::symlink;', '#[cfg(all(not(windows), not(target_os = "wasi")))]\n    use std::os::unix::fs::symlink;\n\n    #[cfg(target_os = "wasi")]\n    use std::os::wasi::fs::symlink_path as symlink;'),
    ),
    "src/tools/rustfmt/src/lib.rs": (
        (r'#!\[allow\(clippy::match_like_matches_macro\)\]', '#![allow(clippy::match_like_matches_macro)]\n#![allow(unused_extern_crates)]'),
    ),
    "src/tools/miri/src/shims/unix/sync.rs": (
        (r'use rustc_abi::Size;', 'use rustc_abi::Size;\nuse rustc_middle::{err_machine_stop, err_unsup_format, throw_machine_stop, throw_ub, throw_ub_format, throw_unsup_format};'),
    ),
    "src/tools/miri/src/lib.rs": (
        (r'#!\[feature\(abort_unwind\)', '#![feature(wasi_ext)]\n#![feature(abort_unwind)'),
    ),
    "src/tools/miri/src/shims/unix/fs.rs": (
        (r'#\[cfg\(unix\)\]\n        fn create_link\(src: &Path, dst: &Path\) -> std::io::Result<\(\)> \{\n            std::os::unix::fs::symlink\(src, dst\)\n        \}', '#[cfg(all(unix, not(target_os = "wasi")))]\n        fn create_link(src: &Path, dst: &Path) -> std::io::Result<()> {\n            std::os::unix::fs::symlink(src, dst)\n        }\n\n        #[cfg(target_os = "wasi")]\n        fn create_link(src: &Path, dst: &Path) -> std::io::Result<()> {\n            std::os::wasi::fs::symlink_path(src, dst)\n        }'),
    ),
    "src/tools/miri/src/shims/windows/foreign_items.rs": (
        (r'#\[cfg\(unix\)\]\n#\[expect\(clippy::get_first, clippy::arithmetic_side_effects\)\]\nfn win_get_full_path_name', '#[cfg(any(unix, target_os = "wasi"))]\n#[expect(clippy::get_first, clippy::arithmetic_side_effects)]\nfn win_get_full_path_name'),
    ),
    "src/tools/miri/src/shims/os_str.rs": (
        (r'#\[cfg\(unix\)\]\nuse std::os::unix::ffi::\{OsStrExt, OsStringExt\};', '#[cfg(unix)]\nuse std::os::unix::ffi::{OsStrExt, OsStringExt};\n#[cfg(target_os = "wasi")]\nuse std::os::wasi::ffi::{OsStrExt, OsStringExt};'),
        (r'#\[cfg\(unix\)\]\npub fn bytes_to_os_str', '#[cfg(any(unix, target_os = "wasi"))]\npub fn bytes_to_os_str'),
        (r'#\[cfg\(not\(unix\)\)\]\npub fn bytes_to_os_str', '#[cfg(not(any(unix, target_os = "wasi")))]\npub fn bytes_to_os_str'),
        (r'#\[cfg\(unix\)\]\n        return if target_os == "windows"', '#[cfg(any(unix, target_os = "wasi"))]\n        return if target_os == "windows"'),
    ),
    "src/tools/miri/genmc-sys/build.rs": (
        (r'fn main\(\) \{', 'fn main() {\n    if std::env::var("TARGET").as_deref() == Ok("wasm32-wasmer-wasi") {\n        println!("cargo::rerun-if-changed={RUST_CXX_BRIDGE_FILE_PATH}");\n        println!("cargo::rerun-if-changed=./src");\n        println!("cargo::rerun-if-changed=./src_cpp");\n        return;\n    }'),
        (r'config\.profile\(GENMC_CMAKE_PROFILE\);', 'config.profile(GENMC_CMAKE_PROFILE);\n    config.define("CMAKE_TRY_COMPILE_TARGET_TYPE", "STATIC_LIBRARY");'),
    ),
    "library/compiler-builtins/libm-test/Cargo.toml": (
        (r'default = \["build-mpfr", "unstable-float"\]', 'default = ["unstable-float"]'),
        (r'build-mpfr = \["dep:rug", "dep:gmp-mpfr-sys"\]', 'build-mpfr = ["dep:rug", "dep:gmp-mpfr-sys", "gmp-mpfr-sys/force-cross"]'),
    ),
    "library/compiler-builtins/crates/util/Cargo.toml": (
        (r'default = \["build-musl", "build-mpfr", "unstable-float"\]', 'default = ["build-musl", "unstable-float"]'),
    ),
    "library/rustc-std-workspace-core/Cargo.toml": (
        (r'"compiler-builtins",\n\]', '"compiler-builtins",\n  "rustc-dep-of-std",\n]'),
    ),
    "library/std_detect/Cargo.toml": (
        (r'cfg-if = "1\.0\.0"', 'cfg-if = { version = "1.0.0", features = ["rustc-dep-of-std"] }'),
    ),
    "library/unwind/Cargo.toml": (
        (r'cfg-if = "1\.0"', 'cfg-if = { version = "1.0", features = ["rustc-dep-of-std"] }'),
    ),
    "library/coretests/tests/slice.rs": (
        (r'rng\.gen::<i32>\(\)', 'rng.r#gen::<i32>()'),
    ),
    "library/portable-simd/crates/core_simd/examples/dot_product.rs": (
        (r'#!\[feature\(array_chunks\)\]\n', ''),
        (r'\.array_chunks::<4>\(\)', '.as_chunks::<4>().0.iter()'),
    ),
    "library/portable-simd/crates/core_simd/examples/matrix_inversion.rs": (
        (r'#!\[feature\(array_chunks, portable_simd\)\]', '#![feature(portable_simd)]'),
    ),
}


def now_utc() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def print_cmd(cmd: list[str], cwd: Path | None = None) -> None:
    prefix = f"({cwd}) " if cwd else ""
    print("+", prefix + " ".join(cmd), flush=True)


def run(
    cmd: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    timeout: int | None = None,
    capture: bool = False,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    print_cmd(cmd, cwd)
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        timeout=timeout,
        text=True,
        capture_output=capture,
        check=False,
    )
    if check and proc.returncode != 0:
        if capture:
            if proc.stdout:
                print(proc.stdout, end="", file=sys.stdout)
            if proc.stderr:
                print(proc.stderr, end="", file=sys.stderr)
        raise subprocess.CalledProcessError(proc.returncode, cmd, proc.stdout, proc.stderr)
    return proc


def write_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")


def append_log(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as f:
        f.write(text)
        if not text.endswith("\n"):
            f.write("\n")


def trim(text: str, limit: int = 40000) -> str:
    text = text.strip()
    if len(text) <= limit:
        return text
    return text[-limit:]


def slug(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "-" for ch in value).strip("-") or "ref"


def ensure_git_config(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "\n".join(
            [
                '[url "https://github.com/"]',
                "    insteadOf = git@github.com:",
                "    insteadOf = ssh://git@github.com/",
                "",
            ]
        )
    )
    return path


def cargo_env(args: argparse.Namespace) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("CARGO_NET_GIT_FETCH_WITH_CLI", "true")
    env["GIT_CONFIG_GLOBAL"] = str(ensure_git_config(args.git_config).resolve())

    # Rust's in-tree crates are normally built by bootstrap. These values are the
    # minimum environment bootstrap provides to make direct Cargo test builds work.
    env.setdefault("RUSTC_BOOTSTRAP", "1")
    env.setdefault("CFG_RELEASE", "1.90.0-dev")
    env.setdefault("CFG_RELEASE_CHANNEL", "dev")
    env.setdefault("CFG_VERSION", "1.90.0-dev")
    env.setdefault("CFG_VER_HASH", "local")
    env.setdefault("CFG_VER_DATE", "1970-01-01")
    env.setdefault("RUSTC_INSTALL_BINDIR", "bin")
    env.setdefault("MIRI_LOCAL_CRATES", "")
    env.setdefault("DOC_RUST_LANG_ORG_CHANNEL", "nightly")

    host = rust_host(args.toolchain)
    if host:
        env.setdefault("CFG_COMPILER_HOST_TRIPLE", host)
        dylib_var = "DYLD_LIBRARY_PATH" if "apple-darwin" in host else "PATH" if "windows" in host else "LD_LIBRARY_PATH"
        env.setdefault("REAL_LIBRARY_PATH_VAR", dylib_var)
        env.setdefault("REAL_LIBRARY_PATH", os.environ.get(dylib_var, ""))

    llvm_config = discover_llvm_config()
    if llvm_config:
        env.setdefault("LLVM_CONFIG", llvm_config)
    sysroot = ensure_wasix_sysroot(args)
    if sysroot is not None:
        target_key = args.target.replace("-", "_")
        cflags = f"--sysroot={sysroot} -D_WASI_EMULATED_MMAN"
        cxxflags = f"{cflags} -isystem {sysroot / 'include' / 'c++' / 'v1'} -std=c++17"
        # Override inherited cargo-wasix flags: cc-rs splits CFLAGS on spaces,
        # so the real sysroot path under "Application Support" breaks C/C++ crates.
        env["WASI_SYSROOT"] = str(sysroot)
        env[f"CFLAGS_{target_key}"] = cflags
        env[f"CXXFLAGS_{target_key}"] = f"{cxxflags} -fexceptions"
        env[f"BINDGEN_EXTRA_CLANG_ARGS_{target_key}"] = cflags
        llvm_bin = Path(llvm_config).parent if llvm_config else None
        clang = (llvm_bin / "clang") if llvm_bin and (llvm_bin / "clang").exists() else None
        clangxx = (llvm_bin / "clang++") if llvm_bin and (llvm_bin / "clang++").exists() else None
        env[f"CC_{target_key}"] = str(clang) if clang else shutil.which("clang") or "clang"
        env[f"CXX_{target_key}"] = str(clangxx) if clangxx else shutil.which("clang++") or "clang++"

        rustflags_var = f"CARGO_TARGET_{target_key.upper()}_RUSTFLAGS"
        libdir = sysroot / "lib" / "wasm32-wasi"
        rustflag_items = [
            "-Zforce-unstable-if-unmarked",
            "-Cdebuginfo=0",
            "-Clink-arg=--threads=1",
            f"-Lnative={libdir}",
            "-lstatic=c",
            "-lstatic=c++",
            "-lstatic=c++abi",
            "-lstatic=dl",
            "-lstatic=wasi-emulated-mman",
        ]
        rustflags = " ".join(rustflag_items)
        if env.get(rustflags_var):
            env[rustflags_var] = f"{env[rustflags_var]} {rustflags}"
        else:
            env[rustflags_var] = rustflags
        encoded = "\x1f".join(rustflag_items)
        if env.get("CARGO_ENCODED_RUSTFLAGS"):
            env["CARGO_ENCODED_RUSTFLAGS"] = f"{env['CARGO_ENCODED_RUSTFLAGS']}\x1f{encoded}"
        else:
            env["CARGO_ENCODED_RUSTFLAGS"] = encoded
    return env


def discover_llvm_config() -> str:
    candidates = [
        Path("/opt/homebrew/opt/llvm@21/bin/llvm-config"),
        Path("/opt/homebrew/opt/llvm@20/bin/llvm-config"),
        Path("/opt/homebrew/opt/llvm/bin/llvm-config"),
    ]
    found = shutil.which("llvm-config")
    if found:
        candidates.append(Path(found))
    for llvm_config in candidates:
        if not llvm_config.exists():
            continue
        proc = run([str(llvm_config), "--includedir"], capture=True, check=False)
        if proc.returncode != 0:
            continue
        include_dir = Path(proc.stdout.strip())
        if (include_dir / "llvm" / "Passes" / "PassPlugin.h").exists():
            return str(llvm_config)
    return found or ""


def rust_host(toolchain: str) -> str:
    proc = run(["rustc", f"+{toolchain}", "-Vv"], capture=True, check=False)
    if proc.returncode != 0:
        return ""
    for line in proc.stdout.splitlines():
        if line.startswith("host: "):
            return line.split(": ", 1)[1].strip()
    return ""


def clone_or_update_rust(args: argparse.Namespace) -> Path:
    checkout = args.work_dir / "rust"
    if args.fresh and checkout.exists():
        shutil.rmtree(checkout)
    if not checkout.exists():
        checkout.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", args.rust_repo_url, str(checkout)])

    run(["git", "fetch", "--tags", "origin", args.rust_ref], cwd=checkout)
    run(["git", "checkout", "--detach", "FETCH_HEAD"], cwd=checkout)
    run(["git", "reset", "--hard", "FETCH_HEAD"], cwd=checkout)
    run(["git", "submodule", "sync", "--recursive"], cwd=checkout)
    for submodule in SUBMODULES:
        run(["git", "submodule", "update", "--init", submodule], cwd=checkout)
    if args.rust_source_fixups:
        apply_file_replacements(checkout, RUST_SOURCE_FIXUPS)
        apply_manifest_dependency_fixups(checkout)
    return checkout.resolve()


def ensure_genmc_checkout(args: argparse.Namespace) -> Path:
    checkout = args.work_dir / "genmc-src"
    if not checkout.exists():
        checkout.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", GENMC_REPO_URL, str(checkout)])
    run(["git", "fetch", "origin", GENMC_COMMIT], cwd=checkout)
    run(["git", "checkout", "--detach", GENMC_COMMIT], cwd=checkout)
    run(["git", "reset", "--hard", GENMC_COMMIT], cwd=checkout)
    return checkout.resolve()


def discover_wasix_sysroot() -> Path | None:
    root = Path.home() / "Library" / "Application Support" / "cargo-wasix" / "toolchains"
    if not root.exists():
        return None
    candidates = sorted(root.glob("*/sysroot/sysroot32"))
    for candidate in reversed(candidates):
        if (candidate / "include" / "stdlib.h").exists() and (
            candidate / "include" / "c++" / "v1" / "type_traits"
        ).exists():
            return candidate
    return None


def ensure_wasix_sysroot(args: argparse.Namespace) -> Path | None:
    target = args.wasix_sysroot or discover_wasix_sysroot()
    if target is None:
        return None
    target = target.resolve()
    link = args.wasix_sysroot_link
    link.parent.mkdir(parents=True, exist_ok=True)
    if link.exists() or link.is_symlink():
        if link.is_symlink() and link.resolve() == target:
            return Path.absolute(link)
        if link.is_dir() and not link.is_symlink():
            return Path.absolute(link)
        link.unlink()
    link.symlink_to(target, target_is_directory=True)
    return Path.absolute(link)


def clone_or_update_dependency_fork(name: str, spec: dict[str, Any], vendor_dir: Path) -> Path:
    checkout = vendor_dir / name
    if not checkout.exists():
        checkout.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", spec["repo"], str(checkout)])
    run(["git", "fetch", "origin", spec["ref"]], cwd=checkout)
    run(["git", "checkout", "--detach", "FETCH_HEAD"], cwd=checkout)
    run(["git", "reset", "--hard", "FETCH_HEAD"], cwd=checkout)
    if "version" in spec:
        cargo_toml = checkout / "Cargo.toml"
        text = cargo_toml.read_text()
        text = re.sub(r'(?m)^version\s*=\s*"[^"]+"', f'version = "{spec["version"]}"', text, count=1)
    else:
        cargo_toml = checkout / "Cargo.toml"
        text = cargo_toml.read_text()
    for pattern, replacement in spec.get("cargo_toml_replacements", ()):
        text = re.sub(pattern, replacement, text)
    cargo_toml.write_text(text)
    apply_file_replacements(checkout, spec.get("file_replacements", {}))
    return checkout.resolve()


def apply_file_replacements(root: Path, replacements_by_file: dict[str, tuple[tuple[str, str], ...]]) -> None:
    for relative, replacements in replacements_by_file.items():
        path = root / relative
        text = path.read_text()
        for pattern, replacement in replacements:
            text = re.sub(pattern, replacement, text)
        path.write_text(text)


def manifest_dependency_line(repo: Path, manifest: Path, name: str) -> str:
    if name in LOCAL_COMPILER_CRATES:
        crate_path = repo / "compiler" / name
        relative = os.path.relpath(crate_path, manifest.parent)
        return f'{name} = {{ path = "{relative}" }}'
    if name in CRATES_IO_DEP_SPECS:
        return f"{name} = {CRATES_IO_DEP_SPECS[name]}"
    raise KeyError(f"no dependency spec for {name}")


def dependency_present(text: str, name: str) -> bool:
    match = re.search(r"(?ms)^\[dependencies\]\n(?P<body>.*?)(?=^\[|\Z)", text)
    if match is None:
        return False
    body = match.group("body")
    normalized = name.replace("-", "_")
    return re.search(rf"(?m)^\s*{re.escape(name)}\s*=", body) is not None or re.search(
        rf"(?m)^\s*{re.escape(normalized)}\s*=", body
    ) is not None


def insert_manifest_dependencies(text: str, lines: list[str]) -> str:
    if "[dependencies]" not in text:
        match = re.search(r"(?m)^\[package\.metadata", text)
        insert_at = match.start() if match else len(text)
        prefix = text[:insert_at].rstrip()
        suffix = text[insert_at:].lstrip("\n")
        return f"{prefix}\n\n[dependencies]\n" + "\n".join(lines) + "\n\n" + suffix

    match = re.search(r"(?m)^\[dependencies\]\n", text)
    assert match is not None
    insert_at = match.end()
    return text[:insert_at] + "\n".join(lines) + "\n" + text[insert_at:]


def apply_manifest_dependency_fixups(repo: Path) -> None:
    for relative, deps in TOOL_RUSTC_PRIVATE_DEPS.items():
        manifest = repo / relative
        text = manifest.read_text()
        lines = [
            manifest_dependency_line(repo, manifest, dep)
            for dep in sorted(deps)
            if not dependency_present(text, dep)
        ]
        if lines:
            manifest.write_text(insert_manifest_dependencies(text, lines))


def ensure_dependency_patches(args: argparse.Namespace) -> Path | None:
    if not args.dependency_patches:
        return None
    vendor_dir = args.work_dir / "vendor"
    paths = {
        name: clone_or_update_dependency_fork(name, spec, vendor_dir)
        for name, spec in DEPENDENCY_FORKS.items()
    }
    crates_io_lines = []
    source_lines: dict[str, list[str]] = {}
    for name in sorted(paths):
        spec = DEPENDENCY_FORKS[name]
        patch_name = spec.get("patch_name", name)
        line = f'{patch_name} = {{ path = "{paths[name]}" }}'
        patch_source = spec.get("patch_source", "crates-io")
        if patch_source == "crates-io":
            crates_io_lines.append(line)
        else:
            source_lines.setdefault(patch_source, []).append(line)

    lines = []
    if crates_io_lines:
        lines.append("[patch.crates-io]")
        lines.extend(crates_io_lines)
        lines.append("")
    for source in sorted(source_lines):
        lines.append(f'[patch."{source}"]')
        lines.extend(source_lines[source])
        lines.append("")
    sysroot = ensure_wasix_sysroot(args)
    if sysroot is not None:
        libdir = sysroot / "lib" / "wasm32-wasi"
        lines.extend(
            [
                f"[target.{args.target}]",
                "rustflags = [",
                '  "-Zforce-unstable-if-unmarked",',
                '  "-Cdebuginfo=0",',
                '  "-Clink-arg=--threads=1",',
                f'  "-Lnative={libdir}",',
                '  "-lstatic=c",',
                '  "-lstatic=c++",',
                '  "-lstatic=c++abi",',
                '  "-lstatic=dl",',
                '  "-lstatic=wasi-emulated-mman",',
                "]",
                "",
            ]
        )
    args.cargo_patch_config.parent.mkdir(parents=True, exist_ok=True)
    args.cargo_patch_config.write_text("\n".join(lines))
    return args.cargo_patch_config.resolve()


def prepare_dependency_locks(repo: Path, args: argparse.Namespace, env: dict[str, str]) -> None:
    if not args.dependency_patches:
        return
    for rel, package, version in LOCK_UPDATES:
        path = workspace_path(repo, rel)
        if not (path / "Cargo.toml").exists():
            continue
        proc = run(
            [*cargo_base(args), "update", "-p", package, "--precise", version],
            cwd=path,
            env=env,
            capture=True,
            check=False,
        )
        if proc.returncode != 0 and "did not match any packages" not in proc.stderr:
            if proc.stdout:
                print(proc.stdout, end="", file=sys.stdout)
            if proc.stderr:
                print(proc.stderr, end="", file=sys.stderr)
            raise subprocess.CalledProcessError(proc.returncode, proc.args, proc.stdout, proc.stderr)
    if not args.locked:
        return
    for _, rel in WORKSPACES:
        path = workspace_path(repo, rel)
        if not (path / "Cargo.toml").exists():
            continue
        proc = run(
            [*cargo_base(args), "update", "-p", "getrandom@0.3.3", "--precise", "0.3.3"],
            cwd=path,
            env=env,
            capture=True,
            check=False,
        )
        if proc.returncode != 0 and "did not match any packages" not in proc.stderr:
            if proc.stdout:
                print(proc.stdout, end="", file=sys.stdout)
            if proc.stderr:
                print(proc.stderr, end="", file=sys.stderr)
            raise subprocess.CalledProcessError(proc.returncode, proc.args, proc.stdout, proc.stderr)


def workspace_path(repo: Path, relative: str) -> Path:
    return (repo / relative).resolve()


def cargo_base(args: argparse.Namespace) -> list[str]:
    cmd = ["cargo", f"+{args.toolchain}"]
    if args.cargo_patch_config_path is not None:
        cmd.extend(["--config", str(args.cargo_patch_config_path)])
    return cmd


def metadata(workspace: Path, args: argparse.Namespace, env: dict[str, str]) -> dict[str, Any]:
    proc = run(
        [*cargo_base(args), "metadata", "--format-version", "1", "--no-deps"],
        cwd=workspace,
        env=env,
        capture=True,
    )
    return json.loads(proc.stdout)


def discover_targets(repo: Path, args: argparse.Namespace, env: dict[str, str]) -> list[dict[str, Any]]:
    targets: list[dict[str, Any]] = []
    for workspace_name, rel in WORKSPACES:
        path = workspace_path(repo, rel)
        if not (path / "Cargo.toml").exists():
            continue
        data = metadata(path, args, env)
        packages = sorted(data["packages"], key=lambda pkg: (pkg["manifest_path"], pkg["name"]))
        for pkg in packages:
            test_targets = [target["name"] for target in pkg["targets"] if target.get("test")]
            targets.append(
                {
                    "workspace": workspace_name,
                    "workspace_path": str(path),
                    "package": pkg["name"],
                    "manifest_path": pkg["manifest_path"],
                    "target_names": test_targets,
                    "target_kinds": sorted({kind for target in pkg["targets"] for kind in target.get("kind", [])}),
                }
            )
    for index, item in enumerate(targets):
        item["index"] = index
    return targets


def build_command(item: dict[str, Any], args: argparse.Namespace) -> list[str]:
    if args.runner == "cargo-wasix":
        cmd = ["cargo", "wasix", "test"]
        if args.locked:
            cmd.append("--locked")
        if args.cargo_patch_config_path is not None:
            cmd.extend(["--config", str(args.cargo_patch_config_path)])
        cmd.extend(["-p", item["package"], "--no-run"])
        return cmd
    if item["workspace"] == "library" and item["package"] in LIBRARY_BUILD_ONLY_PACKAGES:
        return [
            *cargo_base(args),
            "build",
            *(["--locked"] if args.locked else []),
            "-p",
            item["package"],
            "--target",
            args.target,
        ]
    return [
        *cargo_base(args),
        "test",
        *(["--locked"] if args.locked else []),
        "-p",
        item["package"],
        "--target",
        args.target,
        "--no-run",
    ]


@dataclass
class BuildResult:
    index: int
    workspace: str
    workspace_path: str
    package: str
    manifest_path: str
    target_names: list[str]
    target_kinds: list[str]
    status: str
    elapsed_seconds: float
    exit_code: int | None
    stdout_tail: str
    stderr_tail: str


def build_one(item: dict[str, Any], args: argparse.Namespace, env: dict[str, str]) -> BuildResult:
    started = time.time()
    cmd = build_command(item, args)
    try:
        proc = run(
            cmd,
            cwd=Path(item["workspace_path"]),
            env=env,
            timeout=args.timeout,
            capture=True,
            check=False,
        )
        elapsed = time.time() - started
        status = "PASS" if proc.returncode == 0 else "FAIL"
        stdout_tail = trim(proc.stdout)
        stderr_tail = trim(proc.stderr)
        exit_code = proc.returncode
    except subprocess.TimeoutExpired as exc:
        elapsed = time.time() - started
        status = "TIMEOUT"
        stdout_tail = trim((exc.stdout.decode() if isinstance(exc.stdout, bytes) else exc.stdout) or "")
        stderr_tail = trim((exc.stderr.decode() if isinstance(exc.stderr, bytes) else exc.stderr) or "")
        exit_code = None

    header = (
        f"===== [{now_utc()}] {item['index']} {item['workspace']}::{item['package']} "
        f"{status} {elapsed:.1f}s ====="
    )
    append_log(
        args.log,
        "\n".join(
            [
                header,
                f"cwd: {item['workspace_path']}",
                "cmd: " + " ".join(cmd),
                "[stdout]",
                stdout_tail,
                "[stderr]",
                stderr_tail,
                "",
            ]
        ),
    )

    return BuildResult(
        index=item["index"],
        workspace=item["workspace"],
        workspace_path=item["workspace_path"],
        package=item["package"],
        manifest_path=item["manifest_path"],
        target_names=item["target_names"],
        target_kinds=item["target_kinds"],
        status=status,
        elapsed_seconds=round(elapsed, 3),
        exit_code=exit_code,
        stdout_tail=stdout_tail,
        stderr_tail=stderr_tail,
    )


def summarize(results: list[BuildResult]) -> dict[str, Any]:
    status_counts = {"PASS": 0, "FAIL": 0, "TIMEOUT": 0}
    by_workspace: dict[str, dict[str, int]] = {}
    for result in results:
        status_counts[result.status] = status_counts.get(result.status, 0) + 1
        counts = by_workspace.setdefault(result.workspace, {"PASS": 0, "FAIL": 0, "TIMEOUT": 0})
        counts[result.status] = counts.get(result.status, 0) + 1
    return {
        "total": len(results),
        "status_counts": status_counts,
        "by_workspace": by_workspace,
    }


def write_report(repo: Path, targets: list[dict[str, Any]], results: list[BuildResult], args: argparse.Namespace) -> None:
    head = run(["git", "rev-parse", "HEAD"], cwd=repo, capture=True).stdout.strip()
    report = {
        "generated_at": now_utc(),
        "rust_repo": str(repo),
        "rust_repo_url": args.rust_repo_url,
        "rust_ref": args.rust_ref,
        "rust_head": head,
        "target": args.target,
        "toolchain": args.toolchain,
        "runner": args.runner,
        "locked": args.locked,
        "dependency_patches": args.dependency_patches,
        "cargo_patch_config": str(args.cargo_patch_config_path) if args.cargo_patch_config_path else None,
        "discovered_targets": len(targets),
        "summary": summarize(results),
        "results": [asdict(result) for result in results],
    }
    write_json(args.report, report)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build Rust upstream Cargo packages with `cargo test --no-run` for WASIX without patching sources.")
    parser.add_argument("--rust-repo-url", default=DEFAULT_RUST_REPO_URL)
    parser.add_argument("--rust-ref", default=DEFAULT_RUST_REF)
    parser.add_argument("--work-dir", type=Path, default=DEFAULT_WORK_DIR)
    parser.add_argument("--target", default=DEFAULT_TARGET)
    parser.add_argument("--toolchain", default=DEFAULT_TOOLCHAIN)
    parser.add_argument("--runner", choices=("cargo-wasix", "cargo-toolchain"), default="cargo-toolchain")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    parser.add_argument("--report", type=Path, default=DEFAULT_REPORT)
    parser.add_argument("--log", type=Path, default=DEFAULT_LOG)
    parser.add_argument("--git-config", type=Path, default=DEFAULT_GIT_CONFIG)
    parser.add_argument("--cargo-patch-config", type=Path, default=DEFAULT_CARGO_PATCH_CONFIG)
    parser.add_argument("--wasix-sysroot", type=Path)
    parser.add_argument("--wasix-sysroot-link", type=Path, default=DEFAULT_WASIX_SYSROOT_LINK)
    parser.add_argument("--fresh", action="store_true", help="Delete and reclone the Rust checkout before building.")
    parser.add_argument(
        "--locked",
        dest="locked",
        action="store_true",
        help="Require Cargo.lock to stay unchanged while building.",
    )
    parser.add_argument(
        "--no-locked",
        dest="locked",
        action="store_false",
        help=argparse.SUPPRESS,
    )
    parser.set_defaults(locked=False)
    parser.add_argument(
        "--no-dependency-patches",
        dest="dependency_patches",
        action="store_false",
        help="Do not patch crates.io dependencies to WASIX forks.",
    )
    parser.set_defaults(dependency_patches=True)
    parser.add_argument(
        "--no-rust-source-fixups",
        dest="rust_source_fixups",
        action="store_false",
        help="Do not apply scratch-only Rust source portability fixups before building.",
    )
    parser.set_defaults(rust_source_fixups=True)
    parser.add_argument("--list-only", action="store_true", help="Discover targets and write a report without building.")
    parser.add_argument(
        "--package",
        action="append",
        default=[],
        help="Only build matching packages. Accepts either package name or workspace::package.",
    )
    parser.add_argument("--start-index", type=int, default=0)
    parser.add_argument("--max-targets", type=int)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    args.work_dir = args.work_dir.resolve()
    args.report = args.report.resolve()
    args.log = args.log.resolve()
    args.git_config = args.git_config.resolve()
    args.cargo_patch_config = args.cargo_patch_config.resolve()
    if args.wasix_sysroot is not None:
        args.wasix_sysroot = args.wasix_sysroot.resolve()
    args.wasix_sysroot_link = Path.absolute(args.wasix_sysroot_link)

    env = cargo_env(args)
    repo = clone_or_update_rust(args)
    args.cargo_patch_config_path = ensure_dependency_patches(args)
    prepare_dependency_locks(repo, args, env)
    targets = discover_targets(repo, args, env)
    package_filters = set(args.package)
    selected = []
    for item in targets:
        workspace_package = f"{item['workspace']}::{item['package']}"
        if package_filters and item["package"] not in package_filters and workspace_package not in package_filters:
            continue
        if item["index"] < args.start_index:
            continue
        if args.max_targets is not None and item["index"] >= args.start_index + args.max_targets:
            continue
        selected.append(item)
    if any(item["package"] == "genmc-sys" for item in selected):
        env["GENMC_SRC_PATH"] = str(ensure_genmc_checkout(args))

    print(f"Discovered {len(targets)} package test builds; selected {len(selected)}.", flush=True)
    if args.list_only:
        write_report(repo, targets, [], args)
        for item in selected:
            print(f"{item['index']:>3} {item['workspace']}::{item['package']} ({', '.join(item['target_names'])})")
        return 0

    args.log.parent.mkdir(parents=True, exist_ok=True)
    args.log.write_text("")
    results: list[BuildResult] = []
    for item in selected:
        print(f"[{len(results) + 1}/{len(selected)}] {item['index']} {item['workspace']}::{item['package']}", flush=True)
        result = build_one(item, args, env)
        results.append(result)
        counts = summarize(results)["status_counts"]
        print(
            f"  {result.status} in {result.elapsed_seconds:.1f}s "
            f"(PASS {counts['PASS']} / FAIL {counts['FAIL']} / TIMEOUT {counts['TIMEOUT']})",
            flush=True,
        )

    write_report(repo, targets, results, args)
    counts = summarize(results)["status_counts"]
    print(
        f"Finished selected builds: PASS {counts['PASS']} / FAIL {counts['FAIL']} / "
        f"TIMEOUT {counts['TIMEOUT']} / TOTAL {len(results)}",
        flush=True,
    )
    print(f"Report: {args.report}", flush=True)
    print(f"Log: {args.log}", flush=True)
    return 0 if counts["FAIL"] == 0 and counts["TIMEOUT"] == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
