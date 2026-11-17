//! `titer check`: compose the tiers into one report.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::process;
use crate::report::{Report, Tier};
use crate::session::Session;
use crate::worktree::Worktree;

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
    process::install_signal_handlers()?;
    let session = Session::open(cwd, options.base.as_deref())?;
    let mut report = Report::new();
    report.base_ref = Some(session.base_ref.clone());
    report.base_sha = Some(session.base_sha.clone());
    report.head_tree = Some(session.head_tree.clone());
    report.tiers = options.tiers.clone();
    report.timing.total_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// Runs `argv` in a head worktree, the way titer runs tests. A debugging aid.
pub fn exec(cwd: &Path, base: Option<&str>, argv: &[String]) -> anyhow::Result<i32> {
    process::install_signal_handlers()?;
    let session = Session::open(cwd, base)?;
    let head = Worktree::create(
        &session.git,
        &session.head_tree,
        &session.run_dir.join("head"),
        &session.config.runners,
    )?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(&head.path);
    let out = process::run(cmd, None, &session.run_dir.join("logs"))?;
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    Ok(out.exit_code.unwrap_or(128 + out.signal.unwrap_or(0)))
}
