//! `titer check`: compose the tiers into one report.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::config::Config;
use crate::report::{Report, Tier};

#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub base: Option<String>,
    pub tiers: Vec<Tier>,
    pub budget: Option<Duration>,
    pub workers: Option<u32>,
    pub mutants: Option<PathBuf>,
    pub coverage: Option<PathBuf>,
}

/// Runs a check from `cwd`, which must be inside a git checkout that has opted in.
pub fn run(cwd: &Path, options: &CheckOptions) -> anyhow::Result<Report> {
    let started = Instant::now();
    let root = repo_root(cwd)?;
    let _config = Config::load(&root)?;
    let mut report = Report::new();
    report.tiers = options.tiers.clone();
    report.timing.total_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

fn repo_root(cwd: &Path) -> anyhow::Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "not a git checkout: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}
