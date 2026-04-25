use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use serde::Deserialize;

use super::{LangRunner, Mode, RunnerOpts, Status, TestJob, TestResult, Workspace};
use crate::process::ProcessError;
use crate::run_log::RunLog;
use crate::runtime::{RunSpec, RunTarget, Stream, WasmerRuntime};

const TARGET: &str = "wasm32-wasmer-wasi";
const WORKSPACE_ROOTS: &[(&str, &str)] = &[
    ("root", "."),
    ("library", "library"),
    ("stdarch", "library/stdarch"),
    ("portable-simd", "library/portable-simd"),
    ("compiler-builtins", "library/compiler-builtins"),
    ("miri-test-cargo-miri", "src/tools/miri/test-cargo-miri"),
];
const BUILD_ONLY_PACKAGES: &[&str] = &["proc_macro", "std", "std_detect", "test", "unwind"];
const LOCK_UPDATES: &[(&str, &str, &str)] = &[
    ("library/portable-simd", "wasm-bindgen", "0.2.100"),
    ("library/portable-simd", "wasm-bindgen-futures", "0.4.50"),
    ("library/portable-simd", "wasm-bindgen-test", "0.3.50"),
    ("library/compiler-builtins", "getrandom", "0.3.3"),
];

pub struct RustRunner;

impl RustRunner {
    pub const OPTS: RunnerOpts = RunnerOpts {
        name: "rust",
        git_repo: "https://github.com/wasix-org/rust.git",
        git_ref: "v2025-11-07.1+rust-1.90",
        wasmer_package: None,
        wasmer_package_warmup_args: None,
        wasmer_flags: &[],
        docker_compose: None,
    };

    fn discover_package_targets(
        &self,
        workspace: &Workspace,
        filter: Option<&str>,
    ) -> Result<Vec<RustTarget>> {
        let requested = filter.and_then(requested_package);
        let mut targets = BTreeMap::new();
        for (name, rel) in WORKSPACE_ROOTS {
            let root = workspace.checkout.join(rel);
            if !root.join("Cargo.toml").is_file() {
                continue;
            }
            let output = cargo_command(&root, None)
                .args(["metadata", "--format-version", "1", "--no-deps"])
                .output()
                .with_context(|| format!("run cargo metadata in {}", root.display()))?;
            if !output.status.success() {
                bail!(
                    "cargo metadata failed in {}\n{}",
                    root.display(),
                    tail(&output.stderr)
                );
            }
            for target in parse_metadata_targets(name, &root, &output.stdout)? {
                if requested
                    .as_ref()
                    .is_some_and(|(wanted_workspace, wanted_package)| {
                        *wanted_workspace != target.workspace || *wanted_package != target.package
                    })
                {
                    continue;
                }
                targets.insert(target.manifest_path.clone(), target);
            }
        }
        let targets: Vec<_> = targets.into_values().collect();
        tracing::info!(packages = targets.len(), "discovered rust packages");
        Ok(targets)
    }

    fn apply_required_fixups(&self, workspace: &Workspace) -> Result<RustSetup> {
        apply_text_replacements(
            &workspace.checkout,
            &[
                (
                    "src/tools/compiletest/src/read2.rs",
                    &[
                        (
                            "#[cfg(not(any(unix, windows)))]\nmod imp {\n    use std::io::{self, Read};\n    use std::process::{ChildStderr, ChildStdout};\n\n    pub fn read2(\n        out_pipe: ChildStdout,\n        err_pipe: ChildStderr,",
                            "#[cfg(not(any(unix, windows)))]\nmod imp {\n    use std::io::{self, Read};\n    use std::process::{ChildStderr, ChildStdout};\n\n    pub fn read2(\n        mut out_pipe: ChildStdout,\n        mut err_pipe: ChildStderr,",
                        ),
                        (
                            "pub fn read2(\n        out_pipe: ChildStdout,\n        err_pipe: ChildStderr,",
                            "pub fn read2(\n        mut out_pipe: ChildStdout,\n        mut err_pipe: ChildStderr,",
                        ),
                    ][..],
                ),
                (
                    "compiler/rustc_driver/Cargo.toml",
                    &[("crate-type = [\"dylib\"]", "crate-type = [\"rlib\"]")][..],
                ),
                (
                    "compiler/rustc_fs_util/src/lib.rs",
                    &[(
                        "all(target_os = \"wasi\", target_env = \"p1\")",
                        "target_os = \"wasi\"",
                    )][..],
                ),
                (
                    "src/librustdoc/Cargo.toml",
                    &[(
                        "rustdoc-json-types = { path = \"../rustdoc-json-types\" }",
                        "pulldown-cmark = \"0.11.3\"\nrustc_abi = { path = \"../../compiler/rustc_abi\" }\nrustc_ast = { path = \"../../compiler/rustc_ast\" }\nrustc_ast_pretty = { path = \"../../compiler/rustc_ast_pretty\" }\nrustc_attr_parsing = { path = \"../../compiler/rustc_attr_parsing\" }\nrustc_data_structures = { path = \"../../compiler/rustc_data_structures\" }\nrustc_driver = { path = \"../../compiler/rustc_driver\" }\nrustc_errors = { path = \"../../compiler/rustc_errors\" }\nrustc_expand = { path = \"../../compiler/rustc_expand\" }\nrustc_feature = { path = \"../../compiler/rustc_feature\" }\nrustc_hir = { path = \"../../compiler/rustc_hir\" }\nrustc_hir_analysis = { path = \"../../compiler/rustc_hir_analysis\" }\nrustc_hir_pretty = { path = \"../../compiler/rustc_hir_pretty\" }\nrustc_index = { path = \"../../compiler/rustc_index\" }\nrustc_infer = { path = \"../../compiler/rustc_infer\" }\nrustc_interface = { path = \"../../compiler/rustc_interface\" }\nrustc_lexer = { path = \"../../compiler/rustc_lexer\" }\nrustc_lint = { path = \"../../compiler/rustc_lint\" }\nrustc_lint_defs = { path = \"../../compiler/rustc_lint_defs\" }\nrustc_log = { path = \"../../compiler/rustc_log\" }\nrustc_macros = { path = \"../../compiler/rustc_macros\" }\nrustc_metadata = { path = \"../../compiler/rustc_metadata\" }\nrustc_middle = { path = \"../../compiler/rustc_middle\" }\nrustc_parse = { path = \"../../compiler/rustc_parse\" }\nrustc_passes = { path = \"../../compiler/rustc_passes\" }\nrustc_resolve = { path = \"../../compiler/rustc_resolve\" }\nrustc_serialize = { path = \"../../compiler/rustc_serialize\" }\nrustc_session = { path = \"../../compiler/rustc_session\" }\nrustc_span = { path = \"../../compiler/rustc_span\" }\nrustc_target = { path = \"../../compiler/rustc_target\" }\nrustc_trait_selection = { path = \"../../compiler/rustc_trait_selection\" }\nrustdoc-json-types = { path = \"../rustdoc-json-types\" }\nthin-vec = \"0.2.14\"",
                    )][..],
                ),
                (
                    "src/tools/tidy/src/bins.rs",
                    &[(
                        "#[cfg(windows)]\nmod os_impl",
                        "#[cfg(any(windows, target_os = \"wasi\"))]\nmod os_impl",
                    )][..],
                ),
                (
                    "src/tools/opt-dist/src/environment.rs",
                    &[(
                        "#[cfg(target_family = \"unix\")]\npub fn executable_extension",
                        "#[cfg(any(target_family = \"unix\", target_os = \"wasi\"))]\npub fn executable_extension",
                    )][..],
                ),
                (
                    "src/tools/remote-test-server/src/main.rs",
                    &[
                        (
                            "#[cfg(not(windows))]\nfn get_status_code",
                            "#[cfg(all(not(windows), not(target_os = \"wasi\")))]\nfn get_status_code",
                        ),
                        (
                            "#[cfg(windows)]\nfn get_status_code(status: &ExitStatus) -> (u8, i32) {\n    (0, status.code().unwrap())\n}",
                            "#[cfg(any(windows, target_os = \"wasi\"))]\nfn get_status_code(status: &ExitStatus) -> (u8, i32) {\n    (0, status.code().unwrap_or(1))\n}",
                        ),
                        (
                            "#[cfg(not(windows))]\nfn set_permissions",
                            "#[cfg(all(not(windows), not(target_os = \"wasi\")))]\nfn set_permissions",
                        ),
                        (
                            "#[cfg(windows)]\nfn set_permissions",
                            "#[cfg(any(windows, target_os = \"wasi\"))]\nfn set_permissions",
                        ),
                    ][..],
                ),
                (
                    "src/tools/rust-installer/src/util.rs",
                    &[
                        (
                            "#[cfg(unix)]\nuse std::os::unix::fs::OpenOptionsExt;",
                            "#[cfg(unix)]\nuse std::os::unix::fs::OpenOptionsExt;\n#[cfg(target_os = \"wasi\")]\nuse std::os::wasi::fs::OpenOptionsExt;",
                        ),
                        (
                            "#[cfg(unix)]\nuse std::os::unix::fs::symlink as symlink_file;",
                            "#[cfg(unix)]\nuse std::os::unix::fs::symlink as symlink_file;\n#[cfg(target_os = \"wasi\")]\nuse std::os::wasi::fs::symlink_path as symlink_file;",
                        ),
                    ][..],
                ),
                (
                    "src/tools/rust-installer/src/lib.rs",
                    &[(
                        "#[macro_use]",
                        "#![cfg_attr(target_os = \"wasi\", feature(wasi_ext))]\n\n#[macro_use]",
                    )][..],
                ),
                (
                    "src/bootstrap/src/lib.rs",
                    &[
                        (
                            "#[cfg(unix)]\n        use std::os::unix::fs::symlink as symlink_file;",
                            "#[cfg(unix)]\n        use std::os::unix::fs::symlink as symlink_file;\n        #[cfg(target_os = \"wasi\")]\n        use std::os::wasi::fs::symlink_path as symlink_file;",
                        ),
                        (
                            "#[cfg(unix)]\nfn chmod",
                            "#[cfg(all(unix, not(target_os = \"wasi\")))]\nfn chmod",
                        ),
                        (
                            "#[cfg(windows)]\nfn chmod",
                            "#[cfg(any(windows, target_os = \"wasi\"))]\nfn chmod",
                        ),
                    ][..],
                ),
                (
                    "src/tools/x/src/main.rs",
                    &[
                        (
                            "#[cfg(unix)]\nfn x_command",
                            "#[cfg(any(unix, target_os = \"wasi\"))]\nfn x_command",
                        ),
                        (
                            "#[cfg(not(any(windows, unix)))]\nfn x_command",
                            "#[cfg(not(any(windows, unix, target_os = \"wasi\")))]\nfn x_command",
                        ),
                    ][..],
                ),
                (
                    "src/tools/miri/test-cargo-miri/build.rs",
                    &[(
                        "assert!(env::var_os(\"CARGO_CFG_MIRI\").is_some(), \"cargo failed to tell us about `--cfg miri`\");",
                        "if env::var_os(\"TARGET\").as_deref() != Some(std::ffi::OsStr::new(\"wasm32-wasmer-wasi\")) {\n        assert!(env::var_os(\"CARGO_CFG_MIRI\").is_some(), \"cargo failed to tell us about `--cfg miri`\");\n    }",
                    )][..],
                ),
                (
                    "src/tools/clippy/clippy_dev/src/lib.rs",
                    &[("rustc_private,", "rustc_private,\n    wasi_ext,")][..],
                ),
                (
                    "src/tools/clippy/clippy_dev/src/setup/toolchain.rs",
                    &[(
                        "#[cfg(not(windows))]\n    use std::os::unix::fs::symlink;",
                        "#[cfg(all(not(windows), not(target_os = \"wasi\")))]\n    use std::os::unix::fs::symlink;\n\n    #[cfg(target_os = \"wasi\")]\n    use std::os::wasi::fs::symlink_path as symlink;",
                    )][..],
                ),
                (
                    "src/tools/miri/genmc-sys/build.rs",
                    &[
                        (
                            "fn main() {",
                            "fn main() {\n    if std::env::var(\"TARGET\").as_deref() == Ok(\"wasm32-wasmer-wasi\") {\n        println!(\"cargo::rerun-if-changed={RUST_CXX_BRIDGE_FILE_PATH}\");\n        println!(\"cargo::rerun-if-changed=./src\");\n        println!(\"cargo::rerun-if-changed=./src_cpp\");\n        return;\n    }",
                        ),
                        (
                            "config.profile(GENMC_CMAKE_PROFILE);",
                            "config.profile(GENMC_CMAKE_PROFILE);\n    config.define(\"CMAKE_TRY_COMPILE_TARGET_TYPE\", \"STATIC_LIBRARY\");",
                        ),
                    ][..],
                ),
                (
                    "src/tools/miri/src/shims/unix/sync.rs",
                    &[(
                        "use rustc_abi::Size;",
                        "use rustc_abi::Size;\nuse rustc_middle::{err_machine_stop, err_unsup_format, throw_machine_stop, throw_ub, throw_ub_format, throw_unsup_format};",
                    )][..],
                ),
                (
                    "src/tools/miri/src/lib.rs",
                    &[(
                        "#![feature(abort_unwind)",
                        "#![feature(wasi_ext)]\n#![feature(abort_unwind)",
                    )][..],
                ),
                (
                    "src/tools/miri/src/shims/os_str.rs",
                    &[
                        (
                            "#[cfg(unix)]\nuse std::os::unix::ffi::{OsStrExt, OsStringExt};",
                            "#[cfg(unix)]\nuse std::os::unix::ffi::{OsStrExt, OsStringExt};\n#[cfg(target_os = \"wasi\")]\nuse std::os::wasi::ffi::{OsStrExt, OsStringExt};",
                        ),
                        (
                            "#[cfg(unix)]\npub fn bytes_to_os_str",
                            "#[cfg(any(unix, target_os = \"wasi\"))]\npub fn bytes_to_os_str",
                        ),
                        (
                            "#[cfg(not(unix))]\npub fn bytes_to_os_str",
                            "#[cfg(not(any(unix, target_os = \"wasi\")))]\npub fn bytes_to_os_str",
                        ),
                        (
                            "#[cfg(unix)]\n        return if target_os == \"windows\"",
                            "#[cfg(any(unix, target_os = \"wasi\"))]\n        return if target_os == \"windows\"",
                        ),
                    ][..],
                ),
                (
                    "src/tools/miri/src/shims/windows/foreign_items.rs",
                    &[(
                        "#[cfg(unix)]\n#[expect(clippy::get_first, clippy::arithmetic_side_effects)]\nfn win_get_full_path_name",
                        "#[cfg(any(unix, target_os = \"wasi\"))]\n#[expect(clippy::get_first, clippy::arithmetic_side_effects)]\nfn win_get_full_path_name",
                    )][..],
                ),
                (
                    "library/compiler-builtins/libm-test/Cargo.toml",
                    &[
                        (
                            "default = [\"build-mpfr\", \"unstable-float\"]",
                            "default = [\"unstable-float\"]",
                        ),
                        (
                            "build-mpfr = [\"dep:rug\", \"dep:gmp-mpfr-sys\"]",
                            "build-mpfr = [\"dep:rug\", \"dep:gmp-mpfr-sys\", \"gmp-mpfr-sys/force-cross\"]",
                        ),
                    ][..],
                ),
                (
                    "library/compiler-builtins/crates/util/Cargo.toml",
                    &[(
                        "default = [\"build-musl\", \"build-mpfr\", \"unstable-float\"]",
                        "default = [\"build-musl\", \"unstable-float\"]",
                    )][..],
                ),
                (
                    "library/rustc-std-workspace-core/Cargo.toml",
                    &[(
                        "\"compiler-builtins\",\n]",
                        "\"compiler-builtins\",\n  \"rustc-dep-of-std\",\n]",
                    )][..],
                ),
                (
                    "library/std_detect/Cargo.toml",
                    &[(
                        "cfg-if = \"1.0.0\"",
                        "cfg-if = { version = \"1.0.0\", features = [\"rustc-dep-of-std\"] }",
                    )][..],
                ),
                (
                    "library/unwind/Cargo.toml",
                    &[(
                        "cfg-if = \"1.0\"",
                        "cfg-if = { version = \"1.0\", features = [\"rustc-dep-of-std\"] }",
                    )][..],
                ),
                (
                    "library/coretests/tests/slice.rs",
                    &[("rng.gen::<i32>()", "rng.r#gen::<i32>()")][..],
                ),
                (
                    "library/portable-simd/crates/core_simd/examples/dot_product.rs",
                    &[
                        ("#![feature(array_chunks)]\n", ""),
                        (".array_chunks::<4>()", ".as_chunks::<4>().0.iter()"),
                    ][..],
                ),
                (
                    "library/portable-simd/crates/core_simd/examples/matrix_inversion.rs",
                    &[(
                        "#![feature(array_chunks, portable_simd)]",
                        "#![feature(portable_simd)]",
                    )][..],
                ),
                (
                    "src/tools/rustfmt/src/lib.rs",
                    &[(
                        "#![allow(clippy::match_like_matches_macro)]",
                        "#![allow(clippy::match_like_matches_macro)]\n#![allow(unused_extern_crates)]",
                    )][..],
                ),
            ],
        )?;
        apply_manifest_dependency_fixups(&workspace.checkout)?;
        let forks = ensure_dependency_forks(workspace)?;
        let sysroot = ensure_wasix_sysroot(workspace)?;
        let env = rust_build_env(workspace, sysroot.as_deref())?;
        let config = write_cargo_config(workspace, sysroot, &forks)?;
        prepare_dependency_locks(workspace, &config, &env)?;
        Ok(RustSetup {
            cargo_config: config,
            env,
        })
    }

    fn compile_test_harnesses(
        &self,
        _workspace: &Workspace,
        setup: &RustSetup,
        targets: &[RustTarget],
    ) -> Result<Vec<RustBuild>> {
        let groups = targets_by_workspace(targets);
        let total = groups.len();
        let mut builds = Vec::new();
        for (index, ((workspace_path, _build_only), targets)) in groups.into_iter().enumerate() {
            tracing::info!(
                completed = index,
                total,
                packages = targets.len(),
                build_only = targets[0].build_only,
                workspace = %workspace_path.display(),
                "building rust test packages"
            );
            builds.extend(build_targets_or_split(&workspace_path, setup, &targets)?);
        }
        Ok(builds)
    }

    fn extract_produced_wasm_files(
        &self,
        _workspace: &Workspace,
        builds: &[RustBuild],
    ) -> Result<Vec<RustArtifact>> {
        tracing::info!(builds = builds.len(), "extracting rust wasm test binaries");
        let mut artifacts = Vec::new();
        for build in builds {
            for wasm in executable_paths(build)? {
                let Some(target) = target_for_wasm(&build.targets, &wasm) else {
                    continue;
                };
                artifacts.push(RustArtifact {
                    target: target.clone(),
                    wasm,
                });
            }
        }
        if artifacts.is_empty() {
            let targets: Vec<_> = builds
                .iter()
                .flat_map(|build| build.targets.iter().cloned())
                .collect();
            tracing::info!(
                targets = targets.len(),
                first_workspace = %targets
                    .first()
                    .map(|target| target.workspace_path.display().to_string())
                    .unwrap_or_default(),
                first_names = ?targets
                    .first()
                    .map(|target| &target.target_names),
                "falling back to rust target directory artifact scan"
            );
            artifacts = artifacts_from_target_dirs(&targets)?;
        }
        tracing::info!(
            artifacts = artifacts.len(),
            "extracted rust wasm test binaries"
        );
        Ok(artifacts)
    }

    fn precompile_wasm_files(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        artifacts: &[RustArtifact],
    ) -> Result<Vec<RustCompiledArtifact>> {
        let total = artifacts.len();
        let completed = AtomicUsize::new(0);
        let compiled: Vec<Option<RustCompiledArtifact>> = artifacts
            .par_iter()
            .map(|artifact| {
                let index = completed.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    completed = index,
                    total,
                    artifact = %artifact.wasm.display(),
                    "precompiling rust wasm test binary"
                );
                let run_path = match self.compile_artifact(workspace, wasmer, &artifact.wasm) {
                    Ok(path) => path,
                    Err(err) => {
                        tracing::warn!(
                            artifact = %artifact.wasm.display(),
                            error = ?err,
                            "skipping rust artifact that failed precompile"
                        );
                        return Ok(None);
                    }
                };
                Ok(Some(RustCompiledArtifact {
                    target: artifact.target.clone(),
                    wasm: artifact.wasm.clone(),
                    run_path,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(compiled.into_iter().flatten().collect())
    }

    fn list_tests(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        artifacts: &[RustCompiledArtifact],
    ) -> Result<Vec<RustListedArtifact>> {
        let total = artifacts.len();
        let completed = AtomicUsize::new(0);
        let listed: Vec<Option<RustListedArtifact>> = artifacts
            .par_iter()
            .map(|artifact| {
                let index = completed.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    completed = index,
                    total,
                    artifact = %artifact.wasm.display(),
                    "listing rust tests"
                );
                if let Some(tests) = read_test_list_cache(workspace, &artifact.run_path)? {
                    return Ok(Some(RustListedArtifact {
                        target: artifact.target.clone(),
                        wasm: artifact.wasm.clone(),
                        tests,
                    }));
                }
                let mut stdout = String::new();
                let mut stderr = String::new();
                let result = wasmer.run(
                    RunSpec {
                        target: RunTarget::File(artifact.run_path.clone()),
                        flags: rust_run_flags(workspace),
                        args: vec!["--list".into(), "--format".into(), "terse".into()],
                        timeout: None,
                    },
                    |stream, line| {
                        match stream {
                            Stream::Stdout => push_line(&mut stdout, line),
                            Stream::Stderr => push_line(&mut stderr, line),
                        }
                        Ok(())
                    },
                );
                if let Err(err) = result {
                    tracing::info!(
                        artifact = %artifact.wasm.display(),
                        error = ?err,
                        "skipping rust artifact that failed test listing"
                    );
                    return Ok(None);
                }
                let tests = parse_listed_tests(&stdout);
                if tests.is_empty() {
                    tracing::info!(
                        artifact = %artifact.wasm.display(),
                        "skipping rust artifact with no listed tests"
                    );
                    return Ok(None);
                }
                write_test_list_cache(workspace, &artifact.run_path, &tests)?;
                Ok(Some(RustListedArtifact {
                    target: artifact.target.clone(),
                    wasm: artifact.wasm.clone(),
                    tests,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(listed.into_iter().flatten().collect())
    }

    fn return_discovered_jobs(
        &self,
        listed: Vec<RustListedArtifact>,
        mode: Mode,
        filter: Option<&str>,
    ) -> Result<Vec<TestJob>> {
        let jobs = jobs_from_listed(listed, mode, filter);
        let total_tests: usize = jobs.iter().map(|job| job.tests.len()).sum();
        tracing::info!(
            artifacts = jobs.len(),
            tests = total_tests,
            mode = ?mode,
            "discovered rust test jobs"
        );
        Ok(jobs)
    }

    fn compile_artifact(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        wasm: &Path,
    ) -> Result<PathBuf> {
        let out_dir = workspace
            .output_dir
            .join(".cache")
            .join("rust")
            .join("wasmu");
        fs::create_dir_all(&out_dir)?;
        let out = out_dir.join(format!(
            "{:016x}.wasmu",
            cache_hash(wasmer.binary_path(), wasm)?
        ));
        if out.is_file() {
            return Ok(out);
        }
        wasmer
            .compile_file(wasm, &out)
            .map_err(|e| anyhow!("failed to precompile {}: {e:?}", wasm.display()))?;
        Ok(out)
    }

    fn resolve_run_path(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
    ) -> Result<PathBuf> {
        let artifact = artifact_path_from_job(workspace, &job.id)?;
        self.compile_artifact(workspace, wasmer, &artifact)
    }
}

impl LangRunner for RustRunner {
    fn opts(&self) -> &'static RunnerOpts {
        &Self::OPTS
    }

    fn discover(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        filter: Option<&str>,
        mode: Mode,
    ) -> Result<Vec<TestJob>> {
        let targets = self.discover_package_targets(workspace, filter)?;
        let setup = self.apply_required_fixups(workspace)?;
        let builds = self.compile_test_harnesses(workspace, &setup, &targets)?;
        let artifacts = self.extract_produced_wasm_files(workspace, &builds)?;
        let compiled = self.precompile_wasm_files(workspace, wasmer, &artifacts)?;
        let listed = self.list_tests(workspace, wasmer, &compiled)?;
        self.return_discovered_jobs(listed, mode, filter)
    }

    fn prepare(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        jobs: &[TestJob],
    ) -> Result<()> {
        let mut artifacts = artifacts_from_jobs(workspace, jobs)?;
        if artifacts.missing.is_empty() {
            precompile_job_artifacts(self, workspace, wasmer, &artifacts.paths)?;
            return Ok(());
        }

        tracing::info!(
            missing = artifacts.missing.len(),
            "preparing rust artifacts for cached discovery"
        );
        let packages = packages_from_jobs(jobs)?;
        let targets = self
            .discover_package_targets(workspace, None)?
            .into_iter()
            .filter(|target| packages.contains(&(target.workspace.clone(), target.package.clone())))
            .collect::<Vec<_>>();
        let setup = self.apply_required_fixups(workspace)?;
        self.compile_test_harnesses(workspace, &setup, &targets)?;

        artifacts = artifacts_from_jobs(workspace, jobs)?;
        if !artifacts.missing.is_empty() {
            bail!(
                "rust artifacts still missing after prepare: {}",
                artifacts.missing.join(", ")
            );
        }
        precompile_job_artifacts(self, workspace, wasmer, &artifacts.paths)
    }

    fn run_test(
        &self,
        workspace: &Workspace,
        wasmer: &WasmerRuntime,
        job: &TestJob,
        mode: Mode,
        _log: Option<&RunLog>,
    ) -> Result<Vec<TestResult>> {
        let mut stdout = String::new();
        let mut stderr = String::new();
        let single_test = (job.tests.len() == 1).then(|| test_name_from_case_id(&job.tests[0]));
        let mut args = single_test
            .as_ref()
            .map(|name| vec![name.clone(), "--exact".into(), "--nocapture".into()])
            .unwrap_or_default();
        args.push("--test-threads=1".into());
        let result = wasmer.run(
            RunSpec {
                target: RunTarget::File(self.resolve_run_path(workspace, wasmer, job)?),
                flags: rust_run_flags(workspace),
                args,
                timeout: None,
            },
            |stream, line| {
                if matches!(mode, Mode::Debug) {
                    crate::process::write_stream(stream, line)?;
                }
                match stream {
                    Stream::Stdout => push_line(&mut stdout, line),
                    Stream::Stderr => push_line(&mut stderr, line),
                }
                Ok(())
            },
        );
        rust_results(job, &stdout, &stderr, result)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RustTarget {
    workspace: String,
    workspace_path: PathBuf,
    package: String,
    package_id: String,
    manifest_path: PathBuf,
    target_names: Vec<String>,
    build_only: bool,
}

struct RustSetup {
    cargo_config: PathBuf,
    env: Vec<(String, String)>,
}

struct RustBuild {
    workspace_path: PathBuf,
    targets: Vec<RustTarget>,
    stdout: String,
    stderr: String,
}

#[derive(Clone)]
struct RustArtifact {
    target: RustTarget,
    wasm: PathBuf,
}

struct RustCompiledArtifact {
    target: RustTarget,
    wasm: PathBuf,
    run_path: PathBuf,
}

struct RustListedArtifact {
    target: RustTarget,
    wasm: PathBuf,
    tests: Vec<String>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    targets: Vec<MetadataTarget>,
}

#[derive(Deserialize)]
struct MetadataTarget {
    name: String,
    test: bool,
}

fn cargo_command(cwd: &Path, config: Option<&Path>) -> Command {
    let mut command = Command::new("cargo");
    command.arg("+wasix");
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    command
        .current_dir(cwd)
        .env("RUST_BACKTRACE", "1")
        .env("CARGO_TERM_COLOR", "never");
    command
}

fn build_targets(
    workspace_path: &Path,
    setup: &RustSetup,
    targets: &[RustTarget],
) -> Result<std::result::Result<RustBuild, String>> {
    let mut command = cargo_command(workspace_path, Some(&setup.cargo_config));
    command.envs(setup.env.iter().map(|(key, value)| (key, value)));
    if targets.iter().all(|target| target.build_only) {
        command.arg("build");
    } else {
        command.arg("test");
    }
    for target in targets {
        command.args(["-p", &target.package_id]);
    }
    command.args(["--target", TARGET]);
    if targets.iter().any(|target| !target.build_only) {
        command.arg("--no-run");
    }
    let output = command
        .output()
        .with_context(|| format!("build rust workspace {}", workspace_path.display()))?;
    if output.status.success() {
        return Ok(Ok(RustBuild {
            workspace_path: workspace_path.to_path_buf(),
            targets: targets.to_vec(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    Ok(Err(format!(
        "stdout:\n{}\nstderr:\n{}",
        tail(&output.stdout),
        tail(&output.stderr)
    )))
}

fn build_targets_or_split(
    workspace_path: &Path,
    setup: &RustSetup,
    targets: &[RustTarget],
) -> Result<Vec<RustBuild>> {
    match build_targets(workspace_path, setup, targets)? {
        Ok(build) => return Ok(vec![build]),
        Err(error) if targets.len() == 1 => {
            let target = &targets[0];
            let error = build_error_summary(&error);
            tracing::warn!(
                package = target.package,
                workspace = target.workspace,
                error,
                "skipping rust package that failed to build"
            );
            return Ok(Vec::new());
        }
        Err(error) => {
            let error = build_error_summary(&error);
            tracing::warn!(
                workspace = %workspace_path.display(),
                packages = targets.len(),
                error,
                "rust package group failed; splitting"
            );
        }
    }
    let mid = targets.len() / 2;
    let (left, right) = targets.split_at(mid);
    let mut builds = build_targets_or_split(workspace_path, setup, left)?;
    builds.extend(build_targets_or_split(workspace_path, setup, right)?);
    Ok(builds)
}

fn build_error_summary(error: &str) -> String {
    let lines: Vec<_> = error
        .lines()
        .filter(|line| {
            line.contains("error:")
                || line.contains("error[")
                || line.contains("failed")
                || line.contains("was not used")
        })
        .take(12)
        .collect();
    if lines.is_empty() {
        tail(error.as_bytes())
    } else {
        lines.join("\n")
    }
}

fn parse_metadata_targets(
    workspace: &str,
    workspace_path: &Path,
    data: &[u8],
) -> Result<Vec<RustTarget>> {
    let metadata: Metadata = serde_json::from_slice(data)?;
    Ok(metadata
        .packages
        .into_iter()
        .map(|package| {
            let target_names: Vec<_> = package
                .targets
                .into_iter()
                .filter(|target| target.test)
                .map(|target| target.name)
                .collect();
            RustTarget {
                build_only: workspace == "library"
                    && BUILD_ONLY_PACKAGES.contains(&package.name.as_str()),
                workspace: workspace.to_string(),
                workspace_path: workspace_path.to_path_buf(),
                package: package.name,
                package_id: package.id,
                manifest_path: package.manifest_path,
                target_names,
            }
        })
        .collect())
}

fn targets_by_workspace(targets: &[RustTarget]) -> BTreeMap<(PathBuf, bool), Vec<RustTarget>> {
    let mut groups: BTreeMap<(PathBuf, bool), Vec<RustTarget>> = BTreeMap::new();
    for target in targets {
        groups
            .entry((target.workspace_path.clone(), target.build_only))
            .or_default()
            .push(target.clone());
    }
    groups
}

fn write_cargo_config(
    workspace: &Workspace,
    sysroot: Option<PathBuf>,
    forks: &[DependencyForkPath],
) -> Result<PathBuf> {
    let config = workspace.work_dir.join("rust-cargo-config.toml");
    fs::create_dir_all(config.parent().unwrap())?;
    let mut text = String::from(
        r#"[net]
git-fetch-with-cli = true

[patch.crates-io]
"#,
    );
    for fork in forks.iter().filter(|fork| fork.source.is_none()) {
        text.push_str(&format!(
            "{} = {{ path = \"{}\" }}\n",
            fork.patch_name,
            fork.path.display()
        ));
    }
    for fork in forks.iter().filter(|fork| fork.source.is_some()) {
        text.push_str(&format!(
            "\n[patch.\"{}\"]\n{} = {{ path = \"{}\" }}\n",
            fork.source.unwrap(),
            fork.patch_name,
            fork.path.display()
        ));
    }
    text.push_str(
        r#"
[target.wasm32-wasmer-wasi]
rustflags = [
  "-Zforce-unstable-if-unmarked",
  "-Cdebuginfo=0",
  "-Clink-arg=--threads=1",
"#,
    );
    if let Some(sysroot) = sysroot {
        let libdir = sysroot.join("lib").join("wasm32-wasi");
        text.push_str(&format!(
            "  \"-Lnative={}\",\n  \"-lstatic=c\",\n  \"-lstatic=c++\",\n  \"-lstatic=c++abi\",\n  \"-lstatic=dl\",\n  \"-lstatic=wasi-emulated-mman\",\n",
            libdir.display()
        ));
    }
    text.push_str("]\n");
    fs::write(&config, text)?;
    Ok(config)
}

fn prepare_dependency_locks(
    workspace: &Workspace,
    config: &Path,
    env: &[(String, String)],
) -> Result<()> {
    for (rel, package, version) in LOCK_UPDATES {
        let root = workspace.checkout.join(rel);
        if !root.join("Cargo.toml").is_file() {
            continue;
        }
        tracing::info!(
            package,
            version,
            workspace = %root.display(),
            "updating rust dependency lock"
        );
        let mut command = cargo_command(&root, Some(config));
        command.envs(env.iter().map(|(key, value)| (key, value)));
        command.args(["update", "-p", package, "--precise", version]);
        let output = command
            .output()
            .with_context(|| format!("update rust dependency lock in {}", root.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("did not match any packages") {
                bail!(
                    "rust dependency lock update failed in {}\n{}",
                    root.display(),
                    tail(&output.stderr)
                );
            }
        }
    }
    Ok(())
}

struct DependencyForkPath {
    patch_name: &'static str,
    source: Option<&'static str>,
    path: PathBuf,
}

struct DependencyFork {
    name: &'static str,
    patch_name: &'static str,
    repo: &'static str,
    git_ref: &'static str,
    source: Option<&'static str>,
    replacements: &'static [(&'static str, &'static [(&'static str, &'static str)])],
}

const DEPENDENCY_FORKS: &[DependencyFork] = &[
    DependencyFork {
        name: "curl",
        patch_name: "curl",
        repo: "https://github.com/alexcrichton/curl-rust.git",
        git_ref: "0.4.48",
        source: None,
        replacements: &[
            ("Cargo.toml", &[("default = [\"ssl\"]", "default = []")]),
            (
                "curl-sys/lib.rs",
                &[("#[cfg(unix)]", "#[cfg(any(unix, target_os = \"wasi\"))]")],
            ),
            (
                "src/easy/form.rs",
                &[
                    ("#[cfg(unix)]", "#[cfg(any(unix, target_os = \"wasi\"))]"),
                    (
                        "use std::os::unix::prelude::*;",
                        "#[cfg(unix)]\n        use std::os::unix::prelude::*;\n        #[cfg(target_os = \"wasi\")]\n        use std::os::wasi::prelude::*;",
                    ),
                ],
            ),
            (
                "src/easy/handler.rs",
                &[
                    ("#[cfg(unix)]", "#[cfg(any(unix, target_os = \"wasi\"))]"),
                    (
                        "use std::os::unix::prelude::*;",
                        "#[cfg(unix)]\n            use std::os::unix::prelude::*;\n            #[cfg(target_os = \"wasi\")]\n            use std::os::wasi::prelude::*;",
                    ),
                ],
            ),
            (
                "src/multi.rs",
                &[
                    ("#[cfg(unix)]", "#[cfg(any(unix, target_os = \"wasi\"))]"),
                    (
                        "use libc::{pollfd, POLLIN, POLLOUT, POLLPRI};",
                        "use libc::{pollfd, POLLIN, POLLOUT};\n#[cfg(unix)]\nuse libc::POLLPRI;\n#[cfg(target_os = \"wasi\")]\nconst POLLPRI: libc::c_short = POLLIN;",
                    ),
                ],
            ),
        ],
    },
    DependencyFork {
        name: "getrandom",
        patch_name: "getrandom",
        repo: "https://github.com/wasix-org/getrandom.git",
        git_ref: "wasix-0.3.3",
        source: None,
        replacements: &[],
    },
    DependencyFork {
        name: "home",
        patch_name: "home",
        repo: "https://github.com/wasix-org/home.git",
        git_ref: "wasix-0.5.11",
        source: None,
        replacements: &[],
    },
    DependencyFork {
        name: "indicatif",
        patch_name: "indicatif",
        repo: "https://github.com/console-rs/indicatif.git",
        git_ref: "0.18.4",
        source: None,
        replacements: &[
            (
                "src/lib.rs",
                &[(
                    "#[cfg(all(target_arch = \"wasm32\", not(feature = \"wasmbind\")))]",
                    "#[cfg(all(target_arch = \"wasm32\", not(target_os = \"wasi\"), not(feature = \"wasmbind\")))]",
                )],
            ),
            (
                "src/draw_target.rs",
                &[
                    (
                        "#[cfg(not(target_arch = \"wasm32\"))]\nuse std::time::Instant;",
                        "#[cfg(not(all(target_arch = \"wasm32\", not(target_os = \"wasi\"))))]\nuse std::time::Instant;",
                    ),
                    (
                        "#[cfg(all(target_arch = \"wasm32\", feature = \"wasmbind\"))]",
                        "#[cfg(all(target_arch = \"wasm32\", not(target_os = \"wasi\"), feature = \"wasmbind\"))]",
                    ),
                ],
            ),
            (
                "src/multi.rs",
                &[
                    (
                        "#[cfg(not(target_arch = \"wasm32\"))]\nuse std::time::Instant;",
                        "#[cfg(not(all(target_arch = \"wasm32\", not(target_os = \"wasi\"))))]\nuse std::time::Instant;",
                    ),
                    (
                        "#[cfg(all(target_arch = \"wasm32\", feature = \"wasmbind\"))]",
                        "#[cfg(all(target_arch = \"wasm32\", not(target_os = \"wasi\"), feature = \"wasmbind\"))]",
                    ),
                ],
            ),
            (
                "src/progress_bar.rs",
                &[
                    (
                        "#[cfg(not(target_arch = \"wasm32\"))]\nuse std::time::Instant;",
                        "#[cfg(not(all(target_arch = \"wasm32\", not(target_os = \"wasi\"))))]\nuse std::time::Instant;",
                    ),
                    (
                        "#[cfg(all(target_arch = \"wasm32\", feature = \"wasmbind\"))]",
                        "#[cfg(all(target_arch = \"wasm32\", not(target_os = \"wasi\"), feature = \"wasmbind\"))]",
                    ),
                ],
            ),
            (
                "src/state.rs",
                &[
                    (
                        "#[cfg(not(target_arch = \"wasm32\"))]\nuse std::time::Instant;",
                        "#[cfg(not(all(target_arch = \"wasm32\", not(target_os = \"wasi\"))))]\nuse std::time::Instant;",
                    ),
                    (
                        "#[cfg(all(target_arch = \"wasm32\", feature = \"wasmbind\"))]",
                        "#[cfg(all(target_arch = \"wasm32\", not(target_os = \"wasi\"), feature = \"wasmbind\"))]",
                    ),
                ],
            ),
            (
                "src/style.rs",
                &[
                    (
                        "#[cfg(not(target_arch = \"wasm32\"))]\nuse std::time::Instant;",
                        "#[cfg(not(all(target_arch = \"wasm32\", not(target_os = \"wasi\"))))]\nuse std::time::Instant;",
                    ),
                    (
                        "#[cfg(all(target_arch = \"wasm32\", feature = \"wasmbind\"))]",
                        "#[cfg(all(target_arch = \"wasm32\", not(target_os = \"wasi\"), feature = \"wasmbind\"))]",
                    ),
                ],
            ),
        ],
    },
    DependencyFork {
        name: "libc",
        patch_name: "libc",
        repo: "https://github.com/wasix-org/libc.git",
        git_ref: "wasix-0.2.169",
        source: None,
        replacements: &[
            (
                "Cargo.toml",
                &[("version = \"0.2.169\"", "version = \"0.2.174\"")],
            ),
            (
                "src/wasi/mod.rs",
                &[
                    (
                        "feature = \"rustc-dep-of-std\"",
                        "all(feature = \"rustc-dep-of-std\", not(target_os = \"wasi\"))",
                    ),
                    (
                        "if #[cfg(target_vendor = \"wasmer\")]",
                        "if #[cfg(any(target_vendor = \"wasmer\", target_os = \"wasi\"))]",
                    ),
                ],
            ),
            (
                "src/wasi/wasix.rs",
                &[(
                    "feature = \"rustc-dep-of-std\"",
                    "all(feature = \"rustc-dep-of-std\", not(target_os = \"wasi\"))",
                )],
            ),
        ],
    },
    DependencyFork {
        name: "libc-git",
        patch_name: "libc",
        repo: "https://github.com/wasix-org/libc.git",
        git_ref: "wasix-0.2.169",
        source: Some("https://github.com/wasix-org/libc.git"),
        replacements: &[
            (
                "src/wasi/mod.rs",
                &[
                    (
                        "feature = \"rustc-dep-of-std\"",
                        "all(feature = \"rustc-dep-of-std\", not(target_os = \"wasi\"))",
                    ),
                    (
                        "if #[cfg(target_vendor = \"wasmer\")]",
                        "if #[cfg(any(target_vendor = \"wasmer\", target_os = \"wasi\"))]",
                    ),
                ],
            ),
            (
                "src/wasi/wasix.rs",
                &[(
                    "feature = \"rustc-dep-of-std\"",
                    "all(feature = \"rustc-dep-of-std\", not(target_os = \"wasi\"))",
                )],
            ),
        ],
    },
    DependencyFork {
        name: "libloading",
        patch_name: "libloading",
        repo: "https://github.com/nagisa/rust_libloading.git",
        git_ref: "0.8.8",
        source: None,
        replacements: &[
            (
                "Cargo.toml",
                &[(
                    "[target.'cfg(unix)'.dependencies.cfg-if]",
                    "[target.\"cfg(any(unix, target_os = \\\"wasi\\\"))\".dependencies.cfg-if]",
                )],
            ),
            (
                "src/lib.rs",
                &[
                    (
                        "any(unix, windows)",
                        "any(unix, target_os = \"wasi\", windows)",
                    ),
                    (
                        "any(unix, windows, libloading_docs)",
                        "any(unix, target_os = \"wasi\", windows, libloading_docs)",
                    ),
                ],
            ),
            (
                "src/os/mod.rs",
                &[(
                    "any(unix, libloading_docs)",
                    "any(unix, target_os = \"wasi\", libloading_docs)",
                )],
            ),
            (
                "src/os/unix/mod.rs",
                &[
                    (
                        "#[cfg(all(libloading_docs, not(unix)))]\nmod unix_imports {}\n#[cfg(any(not(libloading_docs), unix))]\nmod unix_imports {\n    pub(super) use std::os::unix::ffi::OsStrExt;\n}",
                        "#[cfg(all(libloading_docs, not(any(unix, target_os = \"wasi\"))))]\nmod unix_imports {}\n#[cfg(all(not(libloading_docs), unix))]\nmod unix_imports {\n    pub(super) use std::os::unix::ffi::OsStrExt;\n}\n#[cfg(all(not(libloading_docs), target_os = \"wasi\"))]\nmod unix_imports {\n    pub(super) use std::os::wasi::ffi::OsStrExt;\n}",
                    ),
                    (
                        "#[cfg_attr(any(target_os = \"linux\", target_os = \"android\"), link(name = \"dl\"))]",
                        "#[cfg_attr(any(target_os = \"linux\", target_os = \"android\", target_os = \"wasi\"), link(name = \"dl\"))]",
                    ),
                ],
            ),
            (
                "src/safe.rs",
                &[
                    (
                        "#[cfg(all(not(libloading_docs), unix))]\nuse super::os::unix as imp;",
                        "#[cfg(all(not(libloading_docs), any(unix, target_os = \"wasi\")))]\nuse super::os::unix as imp;",
                    ),
                    (
                        "#[cfg_attr(libloading_docs, doc(cfg(any(unix, windows))))]",
                        "#[cfg_attr(libloading_docs, doc(cfg(any(unix, target_os = \"wasi\", windows))))]",
                    ),
                ],
            ),
            (
                "src/os/unix/consts.rs",
                &[
                    (
                        "#[cfg(any(not(libloading_docs), unix))]",
                        "#[cfg(any(not(libloading_docs), unix, target_os = \"wasi\"))]",
                    ),
                    (
                        "target_os = \"emscripten\",\n",
                        "target_os = \"emscripten\",\n            target_os = \"wasi\",\n",
                    ),
                ],
            ),
        ],
    },
    DependencyFork {
        name: "socket2",
        patch_name: "socket2",
        repo: "https://github.com/wasix-org/socket2.git",
        git_ref: "v0.5.5",
        source: None,
        replacements: &[(
            "Cargo.toml",
            &[("version       = \"0.5.5\"", "version       = \"0.5.10\"")],
        )],
    },
    DependencyFork {
        name: "syn",
        patch_name: "syn",
        repo: "https://github.com/dtolnay/syn.git",
        git_ref: "2.0.104",
        source: None,
        replacements: &[("Cargo.toml", &[("full = []", "full = [\"visit-mut\"]")])],
    },
];

const TOOL_RUSTC_PRIVATE_DEPS: &[(&str, &[&str])] = &[
    (
        "src/tools/clippy/Cargo.toml",
        &[
            "rustc_driver",
            "rustc_interface",
            "rustc_session",
            "rustc_span",
        ],
    ),
    (
        "src/tools/clippy/clippy_config/Cargo.toml",
        &[
            "rustc_data_structures",
            "rustc_errors",
            "rustc_hir",
            "rustc_middle",
            "rustc_session",
            "rustc_span",
        ],
    ),
    (
        "src/tools/clippy/clippy_dev/Cargo.toml",
        &["rustc-literal-escaper", "rustc_driver", "rustc_lexer"],
    ),
    (
        "src/tools/clippy/clippy_lints/Cargo.toml",
        &[
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
        ],
    ),
    (
        "src/tools/clippy/clippy_lints_internal/Cargo.toml",
        &[
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
        ],
    ),
    (
        "src/tools/clippy/clippy_utils/Cargo.toml",
        &[
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
        ],
    ),
    (
        "src/tools/clippy/declare_clippy_lint/Cargo.toml",
        &["rustc_lint", "rustc_session"],
    ),
    (
        "src/tools/miri/Cargo.toml",
        &[
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
        ],
    ),
    (
        "src/tools/rustfmt/Cargo.toml",
        &[
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
        ],
    ),
];

const LOCAL_COMPILER_CRATES: &[&str] = &[
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
];

fn apply_manifest_dependency_fixups(repo: &Path) -> Result<()> {
    for (relative, deps) in TOOL_RUSTC_PRIVATE_DEPS {
        let manifest = repo.join(relative);
        if !manifest.is_file() {
            continue;
        }
        let mut lines = Vec::new();
        let text = fs::read_to_string(&manifest)?;
        for dep in *deps {
            if !dependency_present(&text, dep) {
                lines.push(manifest_dependency_line(repo, &manifest, dep)?);
            }
        }
        if !lines.is_empty() {
            fs::write(manifest, insert_manifest_dependencies(&text, &lines))?;
        }
    }
    Ok(())
}

fn manifest_dependency_line(repo: &Path, manifest: &Path, name: &str) -> Result<String> {
    if LOCAL_COMPILER_CRATES.contains(&name) {
        let path = pathdiff(
            &repo.join("compiler").join(name),
            manifest.parent().unwrap(),
        );
        return Ok(format!("{name} = {{ path = \"{}\" }}", path.display()));
    }
    let spec = match name {
        "either" => "\"1.15\"",
        "indexmap" => "\"2.10\"",
        "pulldown-cmark" => {
            "{ version = \"0.11\", default-features = false, features = [\"html\"] }"
        }
        "rustc_apfloat" => "\"0.2.0\"",
        "rustc-literal-escaper" => "\"0.0.5\"",
        "smallvec" => "\"1.15\"",
        "thin-vec" => "\"0.2.14\"",
        "tracing" => "{ version = \"0.1\", default-features = false, features = [\"std\"] }",
        _ => bail!("missing Rust dependency spec for {name}"),
    };
    Ok(format!("{name} = {spec}"))
}

fn dependency_present(text: &str, name: &str) -> bool {
    let normalized = name.replace('-', "_");
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with(&format!("{name} =")) || line.starts_with(&format!("{normalized} ="))
    })
}

fn insert_manifest_dependencies(text: &str, lines: &[String]) -> String {
    if let Some(index) = text.find("[dependencies]\n") {
        let insert_at = index + "[dependencies]\n".len();
        return format!(
            "{}{}\n{}",
            &text[..insert_at],
            lines.join("\n"),
            &text[insert_at..]
        );
    }
    format!(
        "{}\n[dependencies]\n{}\n",
        text.trim_end(),
        lines.join("\n")
    )
}

fn pathdiff(path: &Path, base: &Path) -> PathBuf {
    path.strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_dependency_forks(workspace: &Workspace) -> Result<Vec<DependencyForkPath>> {
    let vendor = workspace
        .output_dir
        .join(".cache")
        .join("rust")
        .join("vendor");
    fs::create_dir_all(&vendor)?;
    let mut paths = Vec::new();
    for fork in DEPENDENCY_FORKS {
        let path = vendor.join(fork.name);
        if !path.join(".git").is_dir() {
            tracing::info!(dependency = fork.name, "cloning rust dependency fork");
            let status = Command::new("git")
                .args(["clone", "--depth", "1", "--branch", fork.git_ref, fork.repo])
                .arg(&path)
                .status()
                .with_context(|| format!("clone dependency fork {}", fork.name))?;
            if !status.success() {
                bail!("failed to clone dependency fork {}", fork.name);
            }
        }
        apply_text_replacements(&path, fork.replacements)?;
        paths.push(DependencyForkPath {
            patch_name: fork.patch_name,
            source: fork.source,
            path,
        });
    }
    Ok(paths)
}

fn rust_build_env(_workspace: &Workspace, sysroot: Option<&Path>) -> Result<Vec<(String, String)>> {
    let mut env = vec![
        ("CARGO_NET_GIT_FETCH_WITH_CLI".into(), "true".into()),
        ("CARGO_INCREMENTAL".into(), "0".into()),
        ("RUSTC_BOOTSTRAP".into(), "1".into()),
        ("CFG_RELEASE".into(), "1.90.0-dev".into()),
        ("CFG_RELEASE_CHANNEL".into(), "dev".into()),
        ("CFG_VERSION".into(), "1.90.0-dev".into()),
        ("CFG_VER_HASH".into(), "local".into()),
        ("CFG_VER_DATE".into(), "1970-01-01".into()),
        ("RUSTC_INSTALL_BINDIR".into(), "bin".into()),
        ("MIRI_LOCAL_CRATES".into(), "".into()),
        ("DOC_RUST_LANG_ORG_CHANNEL".into(), "nightly".into()),
        ("CFG_COMPILER_HOST_TRIPLE".into(), rust_host()?),
        ("REAL_LIBRARY_PATH_VAR".into(), real_library_path_var()),
        (
            "REAL_LIBRARY_PATH".into(),
            std::env::var(real_library_path_var()).unwrap_or_default(),
        ),
    ];
    if let Some(llvm_config) = find_llvm_config() {
        env.push(("LLVM_CONFIG".into(), llvm_config.display().to_string()));
        env.push(("LLVM_CONFIG_PATH".into(), llvm_config.display().to_string()));
    }
    if let Some(sysroot) = sysroot {
        let sysroot_path = sysroot.display().to_string();
        env.push(("WASI_SYSROOT".into(), sysroot_path.clone()));
        env.push(("WASIX_SYSROOT".into(), sysroot_path.clone()));
        env.push((
            "CFLAGS_wasm32_wasmer_wasi".into(),
            format!("--sysroot={sysroot_path} -D_WASI_EMULATED_MMAN"),
        ));
        env.push((
            "CXXFLAGS_wasm32_wasmer_wasi".into(),
            format!("--sysroot={sysroot_path} -D_WASI_EMULATED_MMAN -isystem {sysroot_path}/include/c++/v1 -std=c++17 -fexceptions"),
        ));
        env.push((
            "BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasmer_wasi".into(),
            format!("--sysroot={sysroot_path} -D_WASI_EMULATED_MMAN"),
        ));
        let libdir = sysroot.join("lib").join("wasm32-wasi");
        let rustflags = [
            "-Zforce-unstable-if-unmarked".to_string(),
            "-Cdebuginfo=0".to_string(),
            "-Clink-arg=--threads=1".to_string(),
            format!("-Lnative={}", libdir.display()),
            "-lstatic=c".to_string(),
            "-lstatic=c++".to_string(),
            "-lstatic=c++abi".to_string(),
            "-lstatic=dl".to_string(),
            "-lstatic=wasi-emulated-mman".to_string(),
        ];
        env.push((
            "CARGO_TARGET_WASM32_WASMER_WASI_RUSTFLAGS".into(),
            rustflags.join(" "),
        ));
        env.push(("CARGO_ENCODED_RUSTFLAGS".into(), rustflags.join("\x1f")));
        if let Some(llvm_config) = find_llvm_config() {
            let llvm_bin = llvm_config.parent().unwrap();
            env.push((
                "CC_wasm32_wasmer_wasi".into(),
                llvm_bin.join("clang").display().to_string(),
            ));
            env.push((
                "CXX_wasm32_wasmer_wasi".into(),
                llvm_bin.join("clang++").display().to_string(),
            ));
        }
    }
    Ok(env)
}

fn rust_host() -> Result<String> {
    let output = Command::new("cargo")
        .arg("+wasix")
        .args(["-vV"])
        .output()
        .context("run cargo +wasix -vV")?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.to_string());
        }
    }
    bail!("cargo +wasix -vV did not report host triple")
}

fn real_library_path_var() -> String {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH".into()
    } else if cfg!(target_os = "windows") {
        "PATH".into()
    } else {
        "LD_LIBRARY_PATH".into()
    }
}

fn find_llvm_config() -> Option<PathBuf> {
    [
        "/opt/homebrew/opt/llvm@22/bin/llvm-config",
        "/opt/homebrew/opt/llvm/bin/llvm-config",
        "/usr/local/opt/llvm@22/bin/llvm-config",
        "/usr/local/opt/llvm/bin/llvm-config",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

fn find_wasix_sysroot() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WASIX_SYSROOT").map(PathBuf::from) {
        return path.is_dir().then_some(path);
    }
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join("Library/Application Support/cargo-wasix/toolchains");
    fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("sysroot").join("sysroot32"))
        .find(|path| path.is_dir())
}

fn ensure_wasix_sysroot(workspace: &Workspace) -> Result<Option<PathBuf>> {
    let Some(source) = find_wasix_sysroot() else {
        return Ok(None);
    };
    let link = workspace.work_dir.join("wasix-sysroot32");
    if link.exists() {
        return Ok(Some(link));
    }
    fs::create_dir_all(&workspace.work_dir)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&source, &link)?;
    #[cfg(not(unix))]
    fs::create_dir_all(&link)?;
    Ok(Some(link))
}

fn apply_text_replacements(repo: &Path, files: &[(&str, &[(&str, &str)])]) -> Result<()> {
    for (relative, replacements) in files {
        let path = repo.join(relative);
        if !path.is_file() {
            continue;
        }
        let mut text = fs::read_to_string(&path)?;
        let original = text.clone();
        for (from, to) in *replacements {
            if to.is_empty() || !text.contains(to) {
                text = text.replace(from, to);
            }
        }
        if text != original {
            fs::write(path, text)?;
        }
    }
    Ok(())
}

fn executable_paths(build: &RustBuild) -> Result<Vec<PathBuf>> {
    if build.targets.iter().all(|target| target.build_only) {
        return Ok(Vec::new());
    }
    let mut paths = BTreeSet::new();
    for text in [&build.stdout, &build.stderr] {
        for line in text.lines() {
            if let Some(path) = executable_line_path(line) {
                let path = absolutize(&build.workspace_path, path);
                if path.extension().and_then(|ext| ext.to_str()) == Some("wasm") {
                    paths.insert(path);
                }
            }
        }
    }
    if paths.is_empty() {
        let deps = build
            .workspace_path
            .join("target")
            .join(TARGET)
            .join("debug")
            .join("deps");
        let names: Vec<_> = build.targets.iter().flat_map(artifact_candidates).collect();
        collect_matching_wasms(&deps, &names, &mut paths)?;
    }
    Ok(paths.into_iter().collect())
}

fn target_for_wasm<'a>(targets: &'a [RustTarget], wasm: &Path) -> Option<&'a RustTarget> {
    let stem = normalized_stem(wasm);
    targets.iter().find(|target| {
        artifact_candidates(target).any(|name| {
            let name = name.replace('-', "_");
            stem == name || stem.starts_with(&format!("{name}_"))
        })
    })
}

fn artifacts_from_target_dirs(targets: &[RustTarget]) -> Result<Vec<RustArtifact>> {
    let mut artifacts = Vec::new();
    let mut seen = BTreeSet::new();
    for target in targets {
        let deps = target
            .workspace_path
            .join("target")
            .join(TARGET)
            .join("debug")
            .join("deps");
        let mut paths = BTreeSet::new();
        let names: Vec<_> = artifact_candidates(target).collect();
        collect_matching_wasms(&deps, &names, &mut paths)?;
        for wasm in paths {
            if seen.insert(wasm.clone()) {
                artifacts.push(RustArtifact {
                    target: target.clone(),
                    wasm,
                });
            }
        }
    }
    Ok(artifacts)
}

fn artifact_candidates(target: &RustTarget) -> impl Iterator<Item = String> + '_ {
    target
        .target_names
        .iter()
        .cloned()
        .chain(std::iter::once(target.package.clone()))
}

fn executable_line_path(line: &str) -> Option<&Path> {
    let path = line
        .trim()
        .strip_prefix("Executable ")?
        .rsplit_once(" (")?
        .1
        .trim_end_matches(')');
    Some(Path::new(path))
}

fn collect_matching_wasms(dir: &Path, names: &[String], out: &mut BTreeSet<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_matching_wasms(&path, names, out)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("wasm") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .replace('-', "_");
        if names.iter().any(|name| {
            let name = name.replace('-', "_");
            stem == name || stem.starts_with(&format!("{name}_"))
        }) {
            out.insert(path);
        }
    }
    Ok(())
}

fn normalized_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .replace('-', "_")
}

fn jobs_from_listed(
    listed: Vec<RustListedArtifact>,
    mode: Mode,
    filter: Option<&str>,
) -> Vec<TestJob> {
    let mut jobs = Vec::new();
    for artifact in listed {
        let artifact_id = artifact_id(&artifact.target, &artifact.wasm);
        let tests: Vec<_> = artifact
            .tests
            .into_iter()
            .filter_map(|test| {
                let case_id = format!("{artifact_id}::{test}");
                match filter {
                    Some(filter) if filter == artifact_id || filter == case_id => {
                        Some(filter.to_string())
                    }
                    Some(filter) if case_matches_filter(&case_id, &test, filter) => {
                        Some(filter.to_string())
                    }
                    Some(_) => None,
                    None if matches!(mode, Mode::Debug) => Some(artifact_id.clone()),
                    None => Some(case_id),
                }
            })
            .collect();
        if !tests.is_empty() {
            jobs.push(TestJob {
                id: artifact_id,
                tests,
            });
        }
    }
    jobs.sort_by(|a, b| a.id.cmp(&b.id));
    jobs
}

fn parse_listed_tests(output: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    for line in output.lines() {
        if let Some((name, kind)) = line.trim().rsplit_once(": ")
            && matches!(kind, "test" | "benchmark")
        {
            names.insert(name.to_string());
        }
    }
    names.into_iter().collect()
}

fn rust_results(
    job: &TestJob,
    stdout: &str,
    stderr: &str,
    result: std::result::Result<(), ProcessError>,
) -> Result<Vec<TestResult>> {
    let mut parsed = parse_rust_statuses(stdout);
    parsed.extend(parse_rust_statuses(stderr));
    let fallback = match result {
        Ok(()) => Status::Pass,
        Err(ProcessError::Timeout(_)) => Status::Timeout,
        Err(ProcessError::Spawn(message)) => return Err(anyhow!(message)),
        Err(ProcessError::AbnormalExit(message)) if parsed.is_empty() => {
            return Err(anyhow!(ProcessError::AbnormalExit(message)));
        }
        Err(ProcessError::AbnormalExit(_)) => Status::Fail,
        Err(ProcessError::RustPanic(message)) => {
            if parsed.is_empty() {
                return Err(anyhow!(ProcessError::RustPanic(message)));
            }
            Status::Fail
        }
    };
    Ok(job
        .tests
        .iter()
        .map(|id| {
            let name = test_name_from_case_id(id);
            let status = parsed.get(&name).copied().unwrap_or(fallback);
            TestResult {
                id: id.clone(),
                status,
            }
        })
        .collect())
}

fn parse_rust_statuses(output: &str) -> BTreeMap<String, Status> {
    let mut statuses = BTreeMap::new();
    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix("test ") else {
            continue;
        };
        let Some((name, status)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let status = match status.split_whitespace().next() {
            Some("ok") => Status::Pass,
            Some("FAILED") => Status::Fail,
            Some("ignored") => Status::Skip,
            _ => continue,
        };
        statuses.insert(name.to_string(), status);
    }
    statuses
}

fn artifact_path_from_job(workspace: &Workspace, id: &str) -> Result<PathBuf> {
    let (workspace_name, package, artifact) =
        split_artifact_id(id).ok_or_else(|| anyhow!("invalid rust test job id {id:?}"))?;
    let root = WORKSPACE_ROOTS
        .iter()
        .find(|(name, _)| *name == workspace_name)
        .map(|(_, rel)| workspace.checkout.join(rel))
        .ok_or_else(|| anyhow!("unknown rust workspace {workspace_name:?}"))?;
    let deps = root.join("target").join(TARGET).join("debug").join("deps");
    let mut matches = BTreeSet::new();
    collect_matching_wasms(
        &deps,
        &[artifact.to_string(), package.to_string()],
        &mut matches,
    )?;
    matches
        .into_iter()
        .find(|path| path.file_stem().and_then(|stem| stem.to_str()) == Some(artifact))
        .ok_or_else(|| anyhow!("rust artifact {id:?} missing under {}", deps.display()))
}

struct JobArtifacts {
    paths: Vec<PathBuf>,
    missing: Vec<String>,
}

fn artifacts_from_jobs(workspace: &Workspace, jobs: &[TestJob]) -> Result<JobArtifacts> {
    let mut paths = BTreeSet::new();
    let mut missing = Vec::new();
    for job in jobs {
        match artifact_path_from_job(workspace, &job.id) {
            Ok(path) => {
                paths.insert(path);
            }
            Err(_) => missing.push(job.id.clone()),
        }
    }
    Ok(JobArtifacts {
        paths: paths.into_iter().collect(),
        missing,
    })
}

fn packages_from_jobs(jobs: &[TestJob]) -> Result<BTreeSet<(String, String)>> {
    let mut packages = BTreeSet::new();
    for job in jobs {
        let (workspace, package, _) = split_artifact_id(&job.id)
            .ok_or_else(|| anyhow!("invalid rust test job id {:?}", job.id))?;
        packages.insert((workspace.to_string(), package.to_string()));
    }
    Ok(packages)
}

fn precompile_job_artifacts(
    runner: &RustRunner,
    workspace: &Workspace,
    wasmer: &WasmerRuntime,
    paths: &[PathBuf],
) -> Result<()> {
    let total = paths.len();
    let completed = AtomicUsize::new(0);
    paths.par_iter().try_for_each(|path| {
        let index = completed.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            completed = index,
            total,
            artifact = %path.display(),
            "preparing rust test artifact"
        );
        runner.compile_artifact(workspace, wasmer, path)?;
        Ok(())
    })
}

fn split_artifact_id(id: &str) -> Option<(&str, &str, &str)> {
    let mut parts = id.split("::");
    Some((parts.next()?, parts.next()?, parts.next()?))
}

fn requested_package(filter: &str) -> Option<(String, String)> {
    let (workspace, package, _) = split_artifact_id(filter)?;
    Some((workspace.to_string(), package.to_string()))
}

fn test_name_from_case_id(id: &str) -> String {
    id.splitn(4, "::").nth(3).unwrap_or(id).to_string()
}

fn artifact_id(target: &RustTarget, wasm: &Path) -> String {
    let stem = wasm
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("artifact");
    format!("{}::{}::{stem}", target.workspace, target.package)
}

fn case_matches_filter(case_id: &str, test_name: &str, filter: &str) -> bool {
    case_id.contains(filter) || filter.contains(case_id) || filter.ends_with(test_name)
}

fn rust_run_flags(workspace: &Workspace) -> Vec<String> {
    vec![
        "--volume".into(),
        format!(
            "{}:{}",
            workspace.checkout.display(),
            workspace.checkout.display()
        ),
        "--cwd".into(),
        workspace.checkout.display().to_string(),
    ]
}

fn absolutize(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn push_line(buffer: &mut String, line: &str) {
    buffer.push_str(line);
    buffer.push('\n');
}

fn cache_hash(wasmer: &Path, wasm: &Path) -> Result<u64> {
    let mut hasher = DefaultHasher::new();
    wasmer.hash(&mut hasher);
    fs::read(wasm)?.hash(&mut hasher);
    Ok(hasher.finish())
}

fn read_test_list_cache(workspace: &Workspace, run_path: &Path) -> Result<Option<Vec<String>>> {
    let path = test_list_cache_path(workspace, run_path);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn write_test_list_cache(workspace: &Workspace, run_path: &Path, tests: &[String]) -> Result<()> {
    let path = test_list_cache_path(workspace, run_path);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, serde_json::to_vec_pretty(tests)?)?;
    Ok(())
}

fn test_list_cache_path(workspace: &Workspace, run_path: &Path) -> PathBuf {
    workspace
        .output_dir
        .join(".cache")
        .join("rust")
        .join("lists")
        .join(format!("{:016x}.json", path_hash(run_path)))
}

fn path_hash(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<_> = text.lines().rev().take(400).collect();
    lines.reverse();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_targets() {
        let json = br#"{
          "packages": [
            {"id":"path+file:///repo/library/alloc#alloc@0.0.0","name":"alloc","manifest_path":"/repo/library/alloc/Cargo.toml","targets":[{"name":"alloc","test":true}]},
            {"id":"path+file:///repo/library/std#std@0.0.0","name":"std","manifest_path":"/repo/library/std/Cargo.toml","targets":[{"name":"std","test":true}]},
            {"id":"path+file:///repo/helper#helper@0.0.0","name":"helper","manifest_path":"/repo/helper/Cargo.toml","targets":[{"name":"helper","test":false}]}
          ]
        }"#;
        let targets = parse_metadata_targets("library", Path::new("/repo/library"), json).unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].package, "alloc");
        assert_eq!(targets[0].target_names, vec!["alloc"]);
        assert!(!targets[0].build_only);
        assert_eq!(targets[1].package, "std");
        assert!(targets[1].build_only);
        assert_eq!(targets[2].package, "helper");
        assert!(targets[2].target_names.is_empty());
    }

    #[test]
    fn extracts_executable_paths_from_cargo_output() {
        let build = RustBuild {
            workspace_path: PathBuf::from("/repo/library"),
            targets: vec![RustTarget {
                workspace: "library".into(),
                workspace_path: PathBuf::from("/repo/library"),
                package: "alloctests".into(),
                package_id: "path+file:///repo/library/alloc#alloctests@0.0.0".into(),
                manifest_path: PathBuf::from("/repo/library/alloc/Cargo.toml"),
                target_names: vec!["alloctests".into()],
                build_only: false,
            }],
            stdout: String::new(),
            stderr: "Executable unittests src/lib.rs (target/wasm32-wasmer-wasi/debug/deps/alloctests-123.wasm)\n".into(),
        };
        let paths = executable_paths(&build).unwrap();
        assert_eq!(
            paths,
            vec![PathBuf::from(
                "/repo/library/target/wasm32-wasmer-wasi/debug/deps/alloctests-123.wasm"
            )]
        );
    }

    #[test]
    fn parses_rust_list_output() {
        assert_eq!(
            parse_listed_tests("vec::test_append: test\nbench_foo: benchmark\nhelper: module\n"),
            vec!["bench_foo", "vec::test_append"]
        );
    }

    #[test]
    fn creates_jobs_from_listed_tests() {
        let listed = vec![RustListedArtifact {
            target: RustTarget {
                workspace: "library".into(),
                workspace_path: PathBuf::from("/repo/library"),
                package: "alloctests".into(),
                package_id: "path+file:///repo/library/alloc#alloctests@0.0.0".into(),
                manifest_path: PathBuf::from("/repo/library/alloc/Cargo.toml"),
                target_names: vec!["alloctests".into()],
                build_only: false,
            },
            wasm: PathBuf::from(
                "/repo/library/target/wasm32-wasmer-wasi/debug/deps/alloctests-123.wasm",
            ),
            tests: vec!["vec::test_append".into()],
        }];
        assert_eq!(
            jobs_from_listed(listed, Mode::Capture, None),
            vec![TestJob {
                id: "library::alloctests::alloctests-123".into(),
                tests: vec!["library::alloctests::alloctests-123::vec::test_append".into()],
            }]
        );
    }

    #[test]
    fn filter_keeps_user_requested_case_id() {
        let filter = "library::alloctests::old-hash::vec::test_append";
        let listed = vec![RustListedArtifact {
            target: RustTarget {
                workspace: "library".into(),
                workspace_path: PathBuf::from("/repo/library"),
                package: "alloctests".into(),
                package_id: "path+file:///repo/library/alloc#alloctests@0.0.0".into(),
                manifest_path: PathBuf::from("/repo/library/alloc/Cargo.toml"),
                target_names: vec!["alloctests".into()],
                build_only: false,
            },
            wasm: PathBuf::from(
                "/repo/library/target/wasm32-wasmer-wasi/debug/deps/alloctests-new.wasm",
            ),
            tests: vec!["vec::test_append".into()],
        }];
        assert_eq!(
            jobs_from_listed(listed, Mode::Debug, Some(filter)),
            vec![TestJob {
                id: "library::alloctests::alloctests-new".into(),
                tests: vec![filter.into()],
            }]
        );
    }
}
