//! Disk-backed temporary dirs for one titer run.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;

use crate::process;

/// The parent of every run dir: `$TITER_TMPDIR`, else the user cache dir. `/tmp` is often
/// RAM-backed, and worktrees with symlinked dependencies can be large.
pub fn base_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("TITER_TMPDIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("titer"));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("HOME is not set; set TITER_TMPDIR to a disk-backed directory")?;
    let cache = if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Caches")
    } else {
        PathBuf::from(home).join(".cache")
    };
    Ok(cache.join("titer"))
}

/// A temp dir for one run. Dropping it, or a signal, removes it.
#[derive(Debug)]
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn create() -> anyhow::Result<RunDir> {
        let base = base_dir()?;
        std::fs::create_dir_all(&base)
            .with_context(|| format!("cannot create {}", base.display()))?;
        sweep_stale(&base);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = base.join(format!("run-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
        process::register_dir(&path);
        Ok(RunDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A fresh subdirectory path inside the run dir. Not created.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        process::unregister_dir(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Removes run dirs left by titer processes that no longer exist, such as after SIGKILL.
fn sweep_stale(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("run-"))
            .and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() as i32 && !process::pid_alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
