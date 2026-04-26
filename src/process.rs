use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Result, bail};
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};

use crate::run_log::RunLog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub log_output: Arc<RunLog>,
}

#[derive(Debug)]
pub enum ProcessError {
    /// Process failed to spawn
    Spawn(String),
    /// Timeout waiting for the process to finish
    Timeout(String),
    /// Process exited with a runtime crash in the output
    RustCrash(String),
    /// Process exited with non 0 exit code
    AbnormalExit(String),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::Spawn(message) => write!(f, "spawn failed: {message}"),
            ProcessError::Timeout(message) => f.write_str(message),
            ProcessError::RustCrash(message) => write!(f, "crash: {message}"),
            ProcessError::AbnormalExit(message) => {
                write!(f, "process exited abnormally: {message}")
            }
        }
    }
}

enum ProcessEvent {
    /// A single line emitted by stdout or stderr
    Line(Stream, String),
    /// Failed to read from stdout or stderr
    ReadError,
    /// One stream reached EOF
    Closed,
}

const PANIC_CAPTURE_LINE_LIMIT: usize = 40;

struct ProcessState {
    open_streams: usize,
    panic_capture: Option<PanicCapture>,
    pending_runtime_trap: Option<String>,
    timed_out: bool,
}

struct PanicCapture {
    stream: Stream,
    text: String,
    lines: usize,
}

/// Runs a process with a given timeout and streams stdout/stderr to the caller in realtime
pub fn run_process<F>(spec: ProcessSpec, mut on_line: F) -> std::result::Result<(), ProcessError>
where
    F: FnMut(Stream, &str) -> Result<()>,
{
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
    cmd.current_dir(&spec.cwd);
    let mut child = spawn_process(&spec, cmd)?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| ProcessError::Spawn("stdout pipe missing".to_string()))?;
    let stderr = child
        .stderr()
        .take()
        .ok_or_else(|| ProcessError::Spawn("stderr pipe missing".to_string()))?;
    let (tx, rx) = mpsc::channel();
    let stdout_handle = spawn_reader(stdout, Stream::Stdout, tx.clone());
    let stderr_handle = spawn_reader(stderr, Stream::Stderr, tx);
    let mut state = ProcessState {
        open_streams: 2,
        panic_capture: None,
        pending_runtime_trap: None,
        timed_out: false,
    };
    let deadline = Instant::now() + spec.timeout;
    let mut abort = None;
    while state.open_streams > 0 {
        let wait_for = if state.timed_out {
            Duration::from_millis(25)
        } else {
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(25))
        };
        match rx.recv_timeout(wait_for) {
            Ok(event) => {
                if let Err(e) = handle_event(&mut state, &spec.log_output, &mut on_line, event) {
                    let _ = child.kill();
                    abort = Some(e);
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !state.timed_out && Instant::now() >= deadline {
                    match child.try_wait() {
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            let _ = child.kill();
                            state.timed_out = true;
                        }
                        Err(_) => {
                            abort = Some(ProcessError::AbnormalExit(
                                "failed to query process status".to_string(),
                            ));
                            break;
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child
        .wait()
        .map_err(|_| ProcessError::AbnormalExit("failed to wait for process".to_string()))?;
    join_reader(stdout_handle)?;
    join_reader(stderr_handle)?;
    if let Some(err) = abort {
        return Err(err);
    }
    if state.timed_out {
        return Err(ProcessError::Timeout(format!(
            "timed out after {}",
            humantime::format_duration(spec.timeout)
        )));
    }
    if let Some(capture) = state.panic_capture {
        return Err(ProcessError::RustCrash(capture.text));
    }
    if status.success() {
        return Ok(());
    }
    Err(ProcessError::AbnormalExit(format_exit_status(status)))
}

fn format_exit_status(status: ExitStatus) -> String {
    status.to_string()
}

fn spawn_process(
    spec: &ProcessSpec,
    cmd: Command,
) -> std::result::Result<Box<dyn ChildWrapper>, ProcessError> {
    let mut wrapped = CommandWrap::from(cmd);
    wrapped.wrap(ProcessGroup::leader());
    wrapped
        .spawn()
        .map_err(|e| ProcessError::Spawn(format!("spawn {}: {e}", spec.program.display())))
}

fn handle_event<F>(
    state: &mut ProcessState,
    log_output: &RunLog,
    on_line: &mut F,
    event: ProcessEvent,
) -> std::result::Result<(), ProcessError>
where
    F: FnMut(Stream, &str) -> Result<()>,
{
    match event {
        ProcessEvent::Line(stream, line) => handle_line(state, log_output, on_line, stream, line),
        ProcessEvent::ReadError => Err(ProcessError::AbnormalExit(
            "failed to read process output".to_string(),
        )),
        ProcessEvent::Closed => {
            state.open_streams -= 1;
            Ok(())
        }
    }
}

fn handle_line<F>(
    state: &mut ProcessState,
    log_output: &RunLog,
    on_line: &mut F,
    stream: Stream,
    line: String,
) -> std::result::Result<(), ProcessError>
where
    F: FnMut(Stream, &str) -> Result<()>,
{
    let mut current_line_already_captured = false;
    if stream == Stream::Stderr {
        if let Some(pending) = state.pending_runtime_trap.take() {
            if is_wasm_runtime_trap_frame(&line) {
                let mut text = String::new();
                push_panic_line(&mut text, &pending);
                push_panic_line(&mut text, &line);
                state.panic_capture = Some(PanicCapture {
                    stream,
                    text,
                    lines: 2,
                });
                current_line_already_captured = true;
            } else if starts_wasm_runtime_trap_header(&line) {
                state.pending_runtime_trap = Some(line.clone());
            }
        } else if state.panic_capture.is_none() && starts_wasm_runtime_trap_header(&line) {
            state.pending_runtime_trap = Some(line.clone());
        }
    }
    if state.panic_capture.is_none() && starts_rust_panic_capture(&line) {
        state.panic_capture = Some(PanicCapture {
            stream,
            text: String::new(),
            lines: 0,
        });
    }
    if let Some(capture) = &mut state.panic_capture
        && capture.stream == stream
        && !is_tracing_json_line(&line)
        && !current_line_already_captured
        && capture.lines < PANIC_CAPTURE_LINE_LIMIT
    {
        push_panic_line(&mut capture.text, &line);
        capture.lines += 1;
    }
    log_output
        .write_line(stream_name(stream), &line)
        .map_err(|_| ProcessError::AbnormalExit("failed to write process log".to_string()))?;
    on_line(stream, &line)
        .map_err(|_| ProcessError::AbnormalExit("process output callback failed".to_string()))?;
    Ok(())
}

fn push_panic_line(capture: &mut String, line: &str) {
    let line = strip_ansi(line).replace('\r', "\n");
    capture.push_str(&line);
    if !line.ends_with('\n') {
        capture.push('\n');
    }
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_tracing_json_line(line: &str) -> bool {
    line.starts_with("{\"timestamp\"") && line.contains("\"level\"")
}

pub(crate) fn contains_runtime_crash_text(text: &str) -> bool {
    let mut pending_runtime_trap = false;
    for line in text.lines() {
        if starts_rust_panic_capture(line)
            || line.contains("failed with runtime error: RuntimeError:")
        {
            return true;
        }
        if pending_runtime_trap && is_wasm_runtime_trap_frame(line) {
            return true;
        }
        pending_runtime_trap = starts_wasm_runtime_trap_header(line);
    }
    false
}

fn starts_rust_panic_capture(line: &str) -> bool {
    // TODO: Not super bulletproof way to detect panics, maybe there is a better way?
    line.contains("panicked at")
        || line.contains("has overflowed its stack")
        || line.contains("fatal runtime error:")
        || line.contains("thread caused non-unwinding panic")
        || line.contains("memory allocation of ")
        || line.contains("thread panicked while processing panic")
}

fn starts_wasm_runtime_trap_header(line: &str) -> bool {
    line.starts_with("RuntimeError: ")
}

fn is_wasm_runtime_trap_frame(line: &str) -> bool {
    line.trim_start().starts_with("at ")
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    stream: Stream,
    tx: Sender<ProcessEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                    }
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send(ProcessEvent::Line(stream, line)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = tx.send(ProcessEvent::ReadError);
                    let _ = tx.send(ProcessEvent::Closed);
                    return;
                }
            }
        }
        let _ = tx.send(ProcessEvent::Closed);
    })
}

fn join_reader(handle: thread::JoinHandle<()>) -> std::result::Result<(), ProcessError> {
    handle
        .join()
        .map_err(|_| ProcessError::AbnormalExit("output reader thread panicked".to_string()))
}

pub fn run_command(cmd: &mut Command) -> Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        bail!("command exited with {status}");
    }
    Ok(())
}

pub fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn write_stream(stream: Stream, line: &str) -> Result<()> {
    match stream {
        Stream::Stdout => {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{line}")?;
            out.flush()?;
        }
        Stream::Stderr => {
            let mut err = std::io::stderr().lock();
            writeln!(err, "{line}")?;
            err.flush()?;
        }
    }
    Ok(())
}

pub fn ignore_stream(_: Stream, _: &str) -> Result<()> {
    Ok(())
}

fn stream_name(stream: Stream) -> &'static str {
    match stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempdir::TempDir;

    fn sh(script: &str) -> ProcessSpec {
        ProcessSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            cwd: std::env::current_dir().expect("cwd"),
            timeout: Duration::from_secs(1),
            log_output: Arc::new(RunLog::new(
                TempDir::new("shield-process")
                    .expect("tempdir")
                    .into_path()
                    .join("default.log"),
            )),
        }
    }

    #[test]
    fn process_times_out() {
        let mut spec = sh("sleep 2");
        spec.timeout = Duration::from_millis(50);
        assert!(matches!(
            run_process(spec, |_, _| Ok(())),
            Err(ProcessError::Timeout(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn process_times_out_with_background_child_holding_pipes() {
        let mut spec = sh("(sleep 5) & sleep 5");
        spec.timeout = Duration::from_millis(50);
        assert!(matches!(
            run_process(spec, |_, _| Ok(())),
            Err(ProcessError::Timeout(_))
        ));
    }

    #[test]
    fn process_streams_lines() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        run_process(
            sh("printf 'a\\n'; printf 'b\\n' 1>&2"),
            move |stream, line| {
                sink.lock().expect("lock").push((stream, line.to_string()));
                Ok(())
            },
        )
        .expect("run");
        let seen = seen.lock().expect("lock");
        assert!(seen.contains(&(Stream::Stdout, "a".to_string())));
        assert!(seen.contains(&(Stream::Stderr, "b".to_string())));
    }

    #[test]
    fn process_writes_log_file() {
        let dir = TempDir::new("shield-process").expect("tempdir");
        let path = dir.path().join("log.txt");
        let mut spec = sh("printf 'a\\n'; printf 'b\\n' 1>&2");
        spec.log_output = Arc::new(RunLog::new(path.clone()));
        run_process(spec, |_, _| Ok(())).expect("run");
        let text = std::fs::read_to_string(path).expect("read log");
        assert!(text.contains("[stdout] a"));
        assert!(text.contains("[stderr] b"));
    }

    #[test]
    fn process_detects_rust_panic() {
        let err = run_process(
            sh("printf \"before\\nthread 'main' panicked at boom\\nnext\\n\" 1>&2; exit 101"),
            |_, _| Ok(()),
        )
        .expect_err("panic");
        match err {
            ProcessError::RustCrash(text) => {
                assert_eq!(text, "thread 'main' panicked at boom\nnext\n")
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_detects_rust_panic_even_when_exit_success() {
        let err = run_process(
            sh("printf \"thread 'main' panicked at boom\\nnext\\n\" 1>&2"),
            |_, _| Ok(()),
        )
        .expect_err("panic");
        match err {
            ProcessError::RustCrash(text) => {
                assert_eq!(text, "thread 'main' panicked at boom\nnext\n")
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_detects_stack_overflow_runtime_abort() {
        let err = run_process(
            sh("printf \"thread '<unknown>' has overflowed its stack\\nfatal runtime error: stack overflow, aborting\\n\" 1>&2; exit 134"),
            |_, _| Ok(()),
        )
        .expect_err("panic");
        match err {
            ProcessError::RustCrash(text) => {
                assert!(text.contains("has overflowed its stack"));
                assert!(text.contains("fatal runtime error: stack overflow"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_detects_wasm_runtime_trap() {
        let err = run_process(
            sh("printf \"RuntimeError: out of bounds memory access\\n    at <unnamed> (<module>[9015]:0xffffffff)\\n\" 1>&2; exit 1"),
            |_, _| Ok(()),
        )
        .expect_err("runtime trap");
        match err {
            ProcessError::RustCrash(text) => {
                assert!(text.contains("RuntimeError: out of bounds memory access"));
                assert!(text.contains("<module>[9015]"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_runtime_trap_capture_uses_stderr_only() {
        let err = run_process(
            sh("printf \"RuntimeError: out of bounds memory access\\n    at <unnamed> (<module>[9015]:0xffffffff)\\n\" 1>&2; printf \"TEST 1/1\\r\\033[1;32mPASS\\033[0m noisy\\n\"; exit 1"),
            |_, _| Ok(()),
        )
        .expect_err("runtime trap");
        match err {
            ProcessError::RustCrash(text) => {
                assert_eq!(
                    text,
                    "RuntimeError: out of bounds memory access\n    at <unnamed> (<module>[9015]:0xffffffff)\n"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_ignores_stdout_runtime_error_text() {
        let err = run_process(
            sh("printf \"RuntimeError: out of bounds memory access\\n\"; exit 1"),
            |_, _| Ok(()),
        )
        .expect_err("exit");
        match err {
            ProcessError::AbnormalExit(message) => assert!(
                message.contains("1"),
                "expected exit status in message, got {message:?}"
            ),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_ignores_guest_runtime_error_without_wasm_stack() {
        let err = run_process(
            sh("printf \"RuntimeError: ffi_prep_cif_var failed\\nTraceback detail\\n\" 1>&2; exit 1"),
            |_, _| Ok(()),
        )
        .expect_err("exit");
        match err {
            ProcessError::AbnormalExit(message) => assert!(
                message.contains("1"),
                "expected exit status in message, got {message:?}"
            ),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn runtime_crash_text_detects_panic_markers() {
        assert!(contains_runtime_crash_text(
            "thread 'TokioTaskManager Thread Pool_thread_6' panicked at boom"
        ));
        assert!(contains_runtime_crash_text(
            "fatal runtime error: stack overflow, aborting"
        ));
    }

    #[test]
    fn runtime_crash_text_detects_runtime_trap_markers() {
        assert!(contains_runtime_crash_text(
            "RuntimeError: out of bounds memory access\n    at <unnamed> (<module>[9015]:0xffffffff)\n"
        ));
        assert!(contains_runtime_crash_text(
            "Thread 3 of process 1 failed with runtime error: RuntimeError: out of bounds memory access"
        ));
    }

    #[test]
    fn runtime_crash_text_ignores_guest_runtime_error_without_stack() {
        assert!(!contains_runtime_crash_text(
            "RuntimeError: ffi_prep_cif_var failed\nTraceback detail\n"
        ));
    }

    #[test]
    fn process_reports_abnormal_exit_status() {
        let err = run_process(sh("exit 7"), |_, _| Ok(())).expect_err("exit");
        match err {
            ProcessError::AbnormalExit(message) => assert!(
                message.contains("7"),
                "expected exit status in message, got {message:?}"
            ),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_panic_capture_uses_panic_stream_only() {
        let err = run_process(
            sh("printf \"thread 'main' panicked at boom\\nnext\\n\" 1>&2; printf \"TEST 1/1\\r\\033[1;32mPASS\\033[0m noisy\\n\"; exit 101"),
            |_, _| Ok(()),
        )
        .expect_err("panic");
        match err {
            ProcessError::RustCrash(text) => {
                assert_eq!(text, "thread 'main' panicked at boom\nnext\n");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_panic_capture_sanitizes_carriage_returns_and_ansi() {
        let err = run_process(
            sh("printf \"thread 'main' panicked at boom\\r\\033[1;31mFAIL\\033[0m detail\\n\" 1>&2; exit 101"),
            |_, _| Ok(()),
        )
        .expect_err("panic");
        match err {
            ProcessError::RustCrash(text) => {
                assert_eq!(text, "thread 'main' panicked at boom\nFAIL detail\n");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_panic_capture_ignores_tracing_json() {
        let err = run_process(
            sh("printf \"thread 'main' panicked at boom\\n{\\\"timestamp\\\":\\\"now\\\",\\\"level\\\":\\\"INFO\\\"}\\n\" 1>&2; exit 101"),
            |_, _| Ok(()),
        )
        .expect_err("panic");
        match err {
            ProcessError::RustCrash(text) => {
                assert_eq!(text, "thread 'main' panicked at boom\n");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_panic_capture_stops_after_line_limit() {
        let mut script = String::from("i=1; while [ \"$i\" -le 50 ]; do printf \"");
        script.push_str("line-$i");
        script.push_str("\\n\" 1>&2; i=$((i + 1)); done; exit 101");
        let err = run_process(
            sh(&format!("printf \"thread 'main' panicked at boom\\n\" 1>&2; {script}")),
            |_, _| Ok(()),
        )
        .expect_err("panic");
        match err {
            ProcessError::RustCrash(text) => {
                assert!(text.contains("thread 'main' panicked at boom"));
                assert!(text.contains("line-39"));
                assert!(!text.contains("line-40"));
                assert!(!text.contains("line-50"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_allows_non_utf8_output() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        run_process(sh("printf '\\377\\n'"), move |_, line| {
            sink.lock().expect("lock").push(line.to_string());
            Ok(())
        })
        .expect("run");
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.as_slice(), ["\u{fffd}"]);
    }
}
