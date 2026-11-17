mod common;

use std::time::{Duration, Instant};

use common::{Fixture, stdout_json};

const CONFIG: &str = "[[runner]]\nkind = \"pytest\"\ncommand = [\"python3\", \"-m\", \"pytest\"]\n";

/// A repo on branch `feat` with a commit beyond `main`, plus staged, unstaged, untracked, and
/// ignored files, a stash entry, a linked worktree, and a `.venv` dependency dir.
fn busy_repo() -> Fixture {
    let repo = Fixture::new();
    repo.write("titer.toml", CONFIG)
        .write(".gitignore", ".venv/\n*.log\n")
        .write("app.py", "def f():\n    return 1\n")
        .write("other.py", "x = 1\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "feat"]);
    repo.write("app.py", "def f():\n    return 2\n");
    repo.commit_all("feat");
    repo.write("stashed.py", "s = 1\n");
    repo.git(&["stash", "push", "-q", "-u", "-m", "keep me"]);
    repo.write("other.py", "x = 2\n");
    repo.git(&["add", "other.py"]);
    repo.write("other.py", "x = 3\n")
        .write("new_test.py", "def test_new():\n    assert True\n")
        .write("debug.log", "ignored\n")
        .write(".venv/lib/marker.txt", "dep\n");
    let linked = repo.scratch().join("linked");
    repo.git(&["worktree", "add", "-q", linked.to_str().unwrap(), "main"]);
    repo
}

#[test]
fn check_snapshots_head_with_untracked_files_and_leaves_the_checkout_unchanged() {
    let repo = busy_repo();
    let before = repo.checkout_state();

    let output = repo.titer(&["check", "--json"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(repo.checkout_state(), before);
    assert!(
        repo.leftover_tmp().is_empty(),
        "left {:?}",
        repo.leftover_tmp()
    );
    let report = stdout_json(&output);
    let merge_base = repo.git(&["merge-base", "HEAD", "main"]);
    assert_eq!(report["base_ref"], "main");
    assert_eq!(report["base_sha"], merge_base.trim());
    let tree = report["head_tree"].as_str().unwrap();
    let files = repo.git(&["ls-tree", "-r", "--name-only", tree]);
    let mut files: Vec<&str> = files.lines().collect();
    files.sort();
    assert_eq!(
        files,
        vec![
            ".gitignore",
            "app.py",
            "new_test.py",
            "other.py",
            "titer.toml"
        ]
    );
    assert_eq!(repo.git(&["show", &format!("{tree}:other.py")]), "x = 3\n");
}

#[test]
fn check_fails_with_exit_2_on_an_unknown_base() {
    let repo = busy_repo();

    let output = repo.titer(&["check", "--json", "--base", "nope"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown base ref \"nope\""),
        "stderr: {stderr}"
    );
}

#[test]
fn worktrees_hold_head_and_link_dependency_dirs() {
    let repo = busy_repo();

    let output = repo.titer(&["exec", "--", "sh", "-c", "cat other.py new_test.py .venv/lib/marker.txt; test ! -e debug.log && test -L .venv && test ! -e .git && echo ok"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        "x = 3\ndef test_new():\n    assert True\ndep\nok\n",
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo.leftover_tmp().is_empty());
}

#[test]
fn sigint_kills_child_processes_and_removes_worktrees() {
    let repo = busy_repo();
    let before = repo.checkout_state();
    let pidfile = repo.scratch().join("pids");
    let script = format!(
        "echo $$ > {p}; sleep 300 & echo $! >> {p}; wait",
        p = pidfile.display()
    );
    let mut child = repo
        .titer_cmd()
        .args(["exec", "--", "sh", "-c", &script])
        .spawn()
        .unwrap();
    let pids = wait_for_pids(&pidfile, 2);

    // SAFETY: signalling our own child process.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(130));
    for pid in pids {
        assert!(wait_until_dead(pid), "process {pid} survived SIGINT");
    }
    assert!(
        repo.leftover_tmp().is_empty(),
        "left {:?}",
        repo.leftover_tmp()
    );
    assert_eq!(repo.checkout_state(), before);
}

#[test]
fn a_new_run_removes_dirs_left_by_a_killed_run() {
    let repo = busy_repo();
    let stale = repo.titer_tmp().join("run-999999999-1");
    std::fs::create_dir_all(stale.join("head")).unwrap();

    let output = repo.titer(&["check", "--json"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        repo.leftover_tmp().is_empty(),
        "left {:?}",
        repo.leftover_tmp()
    );
}

fn wait_for_pids(pidfile: &std::path::Path, count: usize) -> Vec<i32> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(pidfile) {
            let pids: Vec<i32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
            if pids.len() >= count {
                return pids;
            }
        }
        assert!(Instant::now() < deadline, "child never started");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until_dead(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        // SAFETY: signal 0 only checks for existence.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}
