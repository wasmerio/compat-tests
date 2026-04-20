use std::cmp::min;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use subprocess::{ExitStatus, Popen, PopenConfig, Redirection};

pub struct WasmerRuntime {
    binary: PathBuf,
    default_timeout: Duration,
}

pub struct RunSpec {
    pub package: String,
    pub flags: Vec<String>,
    pub args: Vec<String>,
    pub timeout: Option<Duration>,
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Clone, Copy)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl WasmerRuntime {
    pub fn new(binary: PathBuf, default_timeout: Duration) -> Self {
        Self {
            binary,
            default_timeout,
        }
    }

    pub fn run<F>(&self, spec: RunSpec, mut on_chunk: F) -> Result<Output>
    where
        F: FnMut(Stream, &[u8]) -> Result<()>,
    {
        let timeout = spec.timeout.unwrap_or(self.default_timeout);
        let deadline = Instant::now() + timeout;
        let argv = self.argv(&spec);
        let mut p = Popen::create(
            &argv,
            PopenConfig {
                stdout: Redirection::Pipe,
                stderr: Redirection::Pipe,
                ..Default::default()
            },
        )
        .with_context(|| format!("spawn `{}`", self.binary.display()))?;
        let mut comm = p.communicate_start(None);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        loop {
            let now = Instant::now();
            if now >= deadline {
                let _ = p.kill();
                let _ = p.wait();
                return Ok(Output {
                    stdout: decode(stdout),
                    stderr: decode(stderr),
                    exit_code: None,
                    timed_out: true,
                });
            }

            comm = comm
                .limit_time(min(
                    deadline.saturating_duration_since(now),
                    Duration::from_millis(250),
                ))
                .limit_size(64 * 1024);

            match comm.read() {
                Ok((out, err)) => {
                    let out_empty = push_chunk(Stream::Stdout, out, &mut stdout, &mut on_chunk)?;
                    let err_empty = push_chunk(Stream::Stderr, err, &mut stderr, &mut on_chunk)?;
                    if out_empty && err_empty {
                        return Ok(Output {
                            stdout: decode(stdout),
                            stderr: decode(stderr),
                            exit_code: exit_of(&p.wait()?),
                            timed_out: false,
                        });
                    }
                }
                Err(e) if e.error.kind() == io::ErrorKind::TimedOut => {
                    let _ = push_chunk(Stream::Stdout, e.capture.0, &mut stdout, &mut on_chunk)?;
                    let _ = push_chunk(Stream::Stderr, e.capture.1, &mut stderr, &mut on_chunk)?;
                }
                Err(e) => {
                    let _ = p.kill();
                    let _ = p.wait();
                    return Err(e.error.into());
                }
            }
        }
    }

    fn argv(&self, spec: &RunSpec) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec![
            self.binary.as_os_str().to_owned(),
            "run".into(),
            "--net".into(),
        ];
        argv.extend(spec.flags.iter().map(OsString::from));
        argv.push((&spec.package).into());
        if !spec.args.is_empty() {
            argv.push("--".into());
            argv.extend(spec.args.iter().map(OsString::from));
        }
        argv
    }

    pub fn compile(&self, _wasm: &Path) -> Result<PathBuf> {
        unimplemented!()
    }
}

fn push_chunk<F>(
    stream: Stream,
    chunk: Option<Vec<u8>>,
    buf: &mut Vec<u8>,
    on_chunk: &mut F,
) -> Result<bool>
where
    F: FnMut(Stream, &[u8]) -> Result<()>,
{
    match chunk {
        Some(bytes) if !bytes.is_empty() => {
            on_chunk(stream, &bytes)?;
            buf.extend_from_slice(&bytes);
            Ok(false)
        }
        _ => Ok(true),
    }
}

fn decode(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

fn exit_of(status: &ExitStatus) -> Option<i32> {
    match status {
        ExitStatus::Exited(n) => Some(*n as i32),
        _ => None,
    }
}
