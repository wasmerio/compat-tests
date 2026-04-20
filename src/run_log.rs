use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

pub struct RunLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl RunLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn clear(&self) -> Result<()> {
        std::fs::write(&self.path, "")?;
        Ok(())
    }

    pub fn append(&self, header: &str, stdout: &str, stderr: &str) -> Result<()> {
        let _guard = self.lock.lock().map_err(|_| anyhow!("log lock poisoned"))?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(
            f,
            "===== [{}] {} =====",
            humantime::format_rfc3339_seconds(std::time::SystemTime::now()),
            header
        )?;
        if !stdout.is_empty() {
            writeln!(f, "[stdout]")?;
            f.write_all(stdout.as_bytes())?;
            if !stdout.ends_with('\n') {
                writeln!(f)?;
            }
        }
        if !stderr.is_empty() {
            writeln!(f, "[stderr]")?;
            f.write_all(stderr.as_bytes())?;
            if !stderr.ends_with('\n') {
                writeln!(f)?;
            }
        }
        writeln!(f)?;
        Ok(())
    }
}
