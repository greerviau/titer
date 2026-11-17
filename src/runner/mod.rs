//! Runner adapters: select tests, run them through the repo's own command, and parse results.

pub mod pytest;
pub mod unittest;
pub mod vitest;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::{RunnerConfig, RunnerKind};
use crate::process::{self, RunOutput};

/// One selectable test, found by test detection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TestCase {
    /// The runner's name from `titer.toml`.
    pub runner: String,
    /// Repo-relative path.
    pub file: String,
    /// 1-based line where the test starts.
    pub line: u32,
    pub end_line: u32,
    /// Enclosing classes or `describe` titles, outermost first.
    pub suite: Vec<String>,
    pub name: String,
    /// The runner-native selector: a pytest node id, a unittest dotted id, or `file:line`.
    pub id: String,
    /// Statically marked skipped, such as `it.skip`. vitest runs such a test when it is
    /// selected by line, so its result is skipped whatever the runner reports.
    pub skip: bool,
}

impl TestCase {
    /// The test name with its suite, joined by " > ".
    pub fn display_name(&self) -> String {
        let mut parts = self.suite.clone();
        parts.push(self.name.clone());
        parts.join(" > ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The test's own assertion failed.
    Assertion,
    /// Any other exception.
    Exception,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    /// The exception class, such as `KeyError` or `TypeError`, when the runner reports it.
    pub exception_type: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed(Failure),
    Skipped,
}

/// One executed or skipped test as the runner reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestOutcome {
    /// The runner-native id, when the runner reports one that matches [`TestCase::id`].
    pub id: Option<String>,
    /// The path as the runner reported it (repo-, cwd-, or rootdir-relative, or absolute), or
    /// cwd-relative with a leading `./`.
    pub file: String,
    pub suite: Vec<String>,
    /// The test name without parameters.
    pub name: String,
    /// 1-based line, when the runner reports it.
    pub line: Option<u32>,
    pub status: Status,
}

/// A failure that stopped a whole file from loading, such as an import error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    /// The path as the runner reported it, or cwd-relative with a leading `./`.
    pub file: String,
    pub failure: Failure,
}

#[derive(Debug, Clone)]
pub struct TestRun {
    pub outcomes: Vec<TestOutcome>,
    pub file_errors: Vec<FileError>,
    pub timed_out: bool,
    pub duration: Duration,
    pub max_rss_bytes: u64,
    /// Set when the runner itself failed, for example on a usage error, and gave no results.
    pub runner_error: Option<String>,
}

impl TestRun {
    /// Tests that ran to a pass or a failure. Skipped tests do not count.
    pub fn executed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| !matches!(o.status, Status::Skipped))
            .count()
    }

    pub fn first_failure(&self) -> Option<(&TestOutcome, &Failure)> {
        self.outcomes.iter().find_map(|o| match &o.status {
            Status::Failed(f) => Some((o, f)),
            _ => None,
        })
    }

    /// The result for one selected test. Parametrized outcomes aggregate: any failure fails
    /// the test, and it passes only if some instance passed and none failed.
    pub fn result_for(&self, case: &TestCase, cwd: &str) -> CaseResult {
        if case.skip {
            return CaseResult::Skipped;
        }
        let rel = cwd_relative(&case.file, cwd);
        let mut matched: Vec<&TestOutcome> = self
            .outcomes
            .iter()
            .filter(|o| match &o.id {
                Some(id) => id == &case.id,
                None => o.name == case.name && o.suite == case.suite && same_file(&o.file, &rel),
            })
            .collect();
        // When the runner reports lines (vitest), the line picks the test, so two tests with
        // the same name in one file stay apart.
        if matched.iter().all(|o| o.line.is_some()) {
            matched.retain(|o| o.line == Some(case.line));
        }
        if let Some(failure) = matched.iter().find_map(|o| match &o.status {
            Status::Failed(f) => Some(f),
            _ => None,
        }) {
            return CaseResult::Failed(failure.clone());
        }
        if matched.iter().any(|o| o.status == Status::Passed) {
            return CaseResult::Passed;
        }
        if !matched.is_empty() {
            return CaseResult::Skipped;
        }
        if let Some(error) = self.file_errors.iter().find(|e| same_file(&e.file, &rel)) {
            return CaseResult::Failed(error.failure.clone());
        }
        CaseResult::NotRun
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseResult {
    Passed,
    Failed(Failure),
    Skipped,
    /// The selection did not execute this test.
    NotRun,
}

/// Which tests a run selects.
#[derive(Debug, Clone, Copy)]
pub enum Selection<'a> {
    Tests(&'a [TestCase]),
    /// Whole files, as repo-relative paths.
    Files(&'a [String]),
    /// The runner's whole configured suite.
    Suite,
}

#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    /// The worktree root.
    pub worktree: &'a Path,
    pub selection: Selection<'a>,
    /// Stop at the first failure.
    pub fail_fast: bool,
    pub timeout: Option<Duration>,
    /// Where the run writes its output and report files.
    pub scratch: &'a Path,
}

/// A runner from `titer.toml` with its adapter.
#[derive(Debug, Clone)]
pub struct Runner {
    pub config: RunnerConfig,
}

impl Runner {
    pub fn new(config: RunnerConfig) -> Runner {
        Runner { config }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// The selector id for a test: what the runner's command line accepts.
    pub fn test_id(&self, file: &str, suite: &[String], name: &str, line: u32) -> String {
        let rel = cwd_relative(file, &self.config.cwd);
        match self.config.kind {
            RunnerKind::Pytest => pytest::node_id(&rel, suite, name),
            RunnerKind::Unittest => unittest::test_id(&rel, &self.config.start_dir, suite, name),
            RunnerKind::Vitest => vitest::test_id(&rel, line),
        }
    }

    /// Runs the selection and parses the results.
    pub fn run(&self, req: &RunRequest) -> anyhow::Result<TestRun> {
        match self.config.kind {
            RunnerKind::Pytest => pytest::run(self, req),
            RunnerKind::Unittest => unittest::run(self, req),
            RunnerKind::Vitest => vitest::run(self, req),
        }
    }

    /// The repo's command, run in the runner's cwd inside the worktree, with `args` appended.
    pub(crate) fn command(&self, worktree: &Path, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.config.command[0]);
        cmd.args(&self.config.command[1..])
            .args(args)
            .current_dir(self.cwd_in(worktree))
            // Runners must not cache bytecode: a mutant and its original can share a size
            // and an mtime second, and a stale .pyc would run the wrong code.
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("NO_COLOR", "1")
            .env_remove("FORCE_COLOR")
            .envs(&self.config.env);
        cmd
    }

    pub fn cwd_in(&self, worktree: &Path) -> PathBuf {
        if self.config.cwd == "." {
            worktree.to_path_buf()
        } else {
            worktree.join(&self.config.cwd)
        }
    }

    pub(crate) fn exec(&self, cmd: Command, req: &RunRequest) -> anyhow::Result<RunOutput> {
        process::run(cmd, req.timeout, req.scratch)
    }
}

/// `file` relative to the runner's `cwd`. Both are repo-relative.
pub fn cwd_relative(file: &str, cwd: &str) -> String {
    if cwd == "." {
        return file.to_string();
    }
    file.strip_prefix(cwd)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(file)
        .to_string()
}

/// True when two paths name the same file. A reported path that starts with `./` is
/// cwd-relative and must match exactly. Other reported paths may be relative to another
/// root, such as pytest's rootdir, or absolute, so they match by whole-component suffix.
pub fn same_file(reported: &str, expected: &str) -> bool {
    let expected = expected.trim_start_matches("./");
    if let Some(exact) = reported.strip_prefix("./") {
        return exact == expected;
    }
    reported == expected
        || reported.ends_with(&format!("/{expected}"))
        || expected.ends_with(&format!("/{reported}"))
}

/// The tail of a runner's output, for error messages.
pub(crate) fn output_tail(out: &RunOutput) -> String {
    let text = format!("{}\n{}", out.stdout, out.stderr);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(15);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_allows_different_roots() {
        assert!(same_file("tests/test_a.py", "tests/test_a.py"));
        assert!(same_file("api/tests/test_a.py", "tests/test_a.py"));
        assert!(same_file("/abs/repo/tests/test_a.py", "tests/test_a.py"));
        assert!(!same_file("tests/test_ab.py", "tests/test_a.py"));
        assert!(!same_file("xtests/test_a.py", "tests/test_a.py"));
        assert!(same_file("./src/a.test.ts", "src/a.test.ts"));
        assert!(!same_file("./src/sub/a.test.ts", "a.test.ts"));
    }

    #[test]
    fn cwd_relative_strips_the_runner_dir() {
        assert_eq!(cwd_relative("api/tests/t.py", "api"), "tests/t.py");
        assert_eq!(cwd_relative("tests/t.py", "."), "tests/t.py");
    }
}
