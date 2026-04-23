use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
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
    /// Process exited with Rust panic in the output
    RustPanic(String),
    /// Process exited with non 0 exit code
    AbnormalExit(String),
}

enum ProcessEvent {
    /// A single line emitted by stdout or stderr
    Line(Stream, String),
    /// Failed to read from stdout or stderr
    ReadError(Stream, String),
    /// One stream reached EOF
    Closed,
}

struct ProcessState {
    open_streams: usize,
    panic_capture: Option<String>,
    timed_out: bool,
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
                        Err(e) => {
                            abort = Some(ProcessError::AbnormalExit(format!("wait failed: {e}")));
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
        .map_err(|e| ProcessError::AbnormalExit(format!("wait failed: {e}")))?;
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
    if status.success() {
        return Ok(());
    }
    let details = format!(
        "exit {}",
        status
            .code()
            .map_or("signal".to_string(), |n| n.to_string())
    );
    if let Some(capture) = state.panic_capture {
        Err(ProcessError::RustPanic(capture))
    } else {
        Err(ProcessError::AbnormalExit(details))
    }
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
        ProcessEvent::ReadError(stream, message) => Err(ProcessError::AbnormalExit(format!(
            "{} read failed: {message}",
            stream_name(stream)
        ))),
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
    if state.panic_capture.is_none() && starts_rust_panic_capture(&line) {
        state.panic_capture = Some(String::new());
    }
    if let Some(capture) = &mut state.panic_capture {
        capture.push_str(&line);
        capture.push('\n');
    }
    log_output
        .write_line(stream_name(stream), &line)
        .map_err(|e| ProcessError::AbnormalExit(format!("log write failed: {e:#}")))?;
    on_line(stream, &line)
        .map_err(|e| ProcessError::AbnormalExit(format!("line handler failed: {e:#}")))?;
    Ok(())
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
                Err(e) => {
                    let _ = tx.send(ProcessEvent::ReadError(stream, e.to_string()));
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
        .map_err(|_| ProcessError::AbnormalExit("reader thread panicked".to_string()))
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
            ProcessError::RustPanic(text) => {
                assert_eq!(text, "thread 'main' panicked at boom\nnext\n")
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
