//! Child processes in their own process groups, with timeouts and signal-safe cleanup.
//!
//! titer kills only the process groups it started. On SIGINT, SIGTERM, or SIGHUP it kills
//! every live group, removes every registered temp dir, and exits.

use std::collections::BTreeSet;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

#[derive(Default)]
struct State {
    groups: BTreeSet<i32>,
    dirs: Vec<PathBuf>,
    shutting_down: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    groups: BTreeSet::new(),
    dirs: Vec::new(),
    shutting_down: false,
});
static INSTALL: Once = Once::new();

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Installs the SIGINT, SIGTERM, and SIGHUP handler. Safe to call more than once.
pub fn install_signal_handlers() -> anyhow::Result<()> {
    let mut result = Ok(());
    INSTALL.call_once(|| {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        match signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) {
            Ok(mut signals) => {
                std::thread::spawn(move || {
                    if let Some(signal) = signals.forever().next() {
                        shutdown();
                        std::process::exit(128 + signal);
                    }
                });
            }
            Err(e) => result = Err(e).context("cannot install signal handlers"),
        }
    });
    result
}

/// Kills every live group and removes every registered dir. New spawns fail afterwards.
pub fn shutdown() {
    let (groups, dirs) = {
        let mut state = state();
        state.shutting_down = true;
        (
            std::mem::take(&mut state.groups)
                .into_iter()
                .collect::<Vec<_>>(),
            std::mem::take(&mut state.dirs),
        )
    };
    for &group in &groups {
        kill_group(group, libc::SIGTERM);
    }
    if !groups.is_empty() {
        std::thread::sleep(Duration::from_millis(300));
    }
    for &group in &groups {
        kill_group(group, libc::SIGKILL);
    }
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Registers a dir that shutdown removes.
pub fn register_dir(path: &Path) {
    state().dirs.push(path.to_path_buf());
}

pub fn unregister_dir(path: &Path) {
    state().dirs.retain(|d| d != path);
}

fn kill_group(group: i32, signal: i32) {
    // SAFETY: kill has no memory-safety preconditions; a negative pid targets the group.
    unsafe {
        libc::kill(-group, signal);
    }
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
    /// Peak resident set size of the process or its largest waited-for descendant.
    pub max_rss_bytes: u64,
    pub stdout: String,
    pub stderr: String,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Runs `cmd` in a new process group with stdin closed, capturing output into files under
/// `out_dir`. On timeout, or once the leader exits, the whole group is killed.
pub fn run(
    mut cmd: Command,
    timeout: Option<Duration>,
    out_dir: &Path,
) -> anyhow::Result<RunOutput> {
    std::fs::create_dir_all(out_dir)?;
    let stamp = unique_stamp();
    let stdout_path = out_dir.join(format!("{stamp}.out"));
    let stderr_path = out_dir.join(format!("{stamp}.err"));
    cmd.stdin(Stdio::null())
        .stdout(File::create(&stdout_path)?)
        .stderr(File::create(&stderr_path)?)
        .process_group(0);
    let shown = format!("{:?}", cmd.get_program());
    let started = Instant::now();
    let pid = {
        let mut state = state();
        if state.shutting_down {
            bail!("titer is shutting down");
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("cannot start {shown}"))?;
        let pid = child.id() as i32;
        state.groups.insert(pid);
        pid
    };
    let deadline = timeout.map(|t| started + t);
    let mut timed_out = false;
    let mut sleep = Duration::from_millis(2);
    let (status, rusage) = loop {
        if let Some((status, rusage)) = wait4(pid, libc::WNOHANG)? {
            break (status, rusage);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            kill_group(pid, libc::SIGKILL);
            let waited = wait4(pid, 0)?.expect("blocking wait4 returns a status");
            break waited;
        }
        std::thread::sleep(sleep);
        sleep = (sleep * 2).min(Duration::from_millis(25));
    };
    // Children the leader left behind, such as runner workers, die with the group.
    kill_group(pid, libc::SIGKILL);
    {
        let mut state = state();
        if state.shutting_down {
            // The signal thread is cleaning up and will exit the process with the signal's
            // code. Returning would let the caller exit first with the child's code.
            drop(state);
            loop {
                std::thread::park();
            }
        }
        state.groups.remove(&pid);
    }
    let duration = started.elapsed();
    let stdout = read_lossy(&stdout_path);
    let stderr = read_lossy(&stderr_path);
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
    let (exit_code, signal) = if libc::WIFEXITED(status) {
        (Some(libc::WEXITSTATUS(status)), None)
    } else if libc::WIFSIGNALED(status) {
        (None, Some(libc::WTERMSIG(status)))
    } else {
        (None, None)
    };
    Ok(RunOutput {
        exit_code,
        signal,
        timed_out,
        duration,
        max_rss_bytes: max_rss_bytes(&rusage),
        stdout,
        stderr,
    })
}

fn wait4(pid: i32, flags: i32) -> anyhow::Result<Option<(i32, libc::rusage)>> {
    let mut status = 0;
    // SAFETY: rusage is plain old data, so a zeroed value is valid; wait4 writes to the
    // pointers we pass, which live for the whole call.
    let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe { libc::wait4(pid, &mut status, flags, &mut rusage) };
        if rc == pid {
            return Ok(Some((status, rusage)));
        }
        if rc == 0 {
            return Ok(None);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err).context("wait4 failed");
        }
    }
}

fn max_rss_bytes(rusage: &libc::rusage) -> u64 {
    let raw = rusage.ru_maxrss.max(0) as u64;
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

fn read_lossy(path: &Path) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn unique_stamp() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("run{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Runs `cmd` to completion in its own process group with piped output, like
/// `Command::output`, so a signal kills it too. Used for git, whose filters and hooks can
/// start their own children.
pub fn output(mut cmd: Command) -> std::io::Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let child = {
        let mut state = state();
        if state.shutting_down {
            return Err(std::io::Error::other("titer is shutting down"));
        }
        let child = cmd.spawn()?;
        state.groups.insert(child.id() as i32);
        child
    };
    let pid = child.id() as i32;
    let result = child.wait_with_output();
    kill_group(pid, libc::SIGKILL);
    let mut state = state();
    if state.shutting_down {
        drop(state);
        loop {
            std::thread::park();
        }
    }
    state.groups.remove(&pid);
    result
}

/// True while a process with this pid exists.
pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn captures_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(sh("echo hi; echo err >&2; exit 3"), None, dir.path()).unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "err\n");
        assert!(!out.timed_out);
        assert!(out.max_rss_bytes > 0);
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pids");
        let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let out = run(sh(&script), Some(Duration::from_millis(500)), dir.path()).unwrap();
        assert!(out.timed_out);
        assert!(out.duration < Duration::from_secs(5));
        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(grandchild) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(grandchild), "grandchild {grandchild} survived");
    }

    #[test]
    fn leftover_children_die_when_the_leader_exits() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pids");
        let script = format!("sleep 300 & echo $! > {}", pidfile.display());
        let out = run(sh(&script), None, dir.path()).unwrap();
        assert!(out.success());
        let orphan: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(orphan) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(orphan), "orphan {orphan} survived");
    }
}
