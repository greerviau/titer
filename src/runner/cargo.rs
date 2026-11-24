//! cargo test: build each selected target, run it with libtest's name filters, and parse
//! libtest's plain output.
//!
//! A test id is the file and the path inside it, `src/math.rs::tests::muls`. The file maps
//! to the targets that compile it (`--lib`, `--bin`, `--test`, or `--example` of a
//! package) and to its module paths in each, which together give libtest's names, such as
//! `math::tests::muls`. titer lists each target's tests and selects exact names: the
//! test's own name, or, for a test that a macro such as `rstest` expands, each case under
//! its name. A target that does not build or list leaves the run incomplete.
//!
//! The build runs before the tests, with its own generous timeout. The run's timeout and
//! duration cover the test binaries only: each worktree's first build is cold, and counting
//! it would stretch every mutant's timeout. A build that fails yields compile errors and
//! no results; a compile error is never a test failure.
//!
//! Selected tests run on one thread, so libtest prints each test's name when it starts and
//! a crash, such as an abort or a stack overflow, fails the test that was running.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::{
    CompileError, Failure, FailureKind, FileError, RunRequest, Runner, Selection, Status, TestCase,
    TestOutcome, TestRun, cwd_relative, output_tail,
};
use crate::lang::{self, Lang, PythonStyle, TestDef};
use crate::process::RunOutput;
use targets::Target;

pub mod targets;

/// How long a build may take. Builds do not hang the way tests can, and a cold build of a
/// large workspace can take many minutes.
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `path::module::test`, with `path` relative to the runner's cwd.
pub fn test_id(rel: &str, suite: &[String], name: &str) -> String {
    let mut parts = vec![rel.to_string()];
    parts.extend(suite.iter().cloned());
    parts.push(name.to_string());
    parts.join("::")
}

/// A libtest name to select, the case it stands for, and whether a macro expands it into
/// a module of cases.
struct Wanted<'a> {
    case: Option<&'a TestCase>,
    name: String,
    parametrized: bool,
}

/// One target and the tests wanted from it, or the whole suite.
struct Group<'a> {
    target: Option<Target>,
    wanted: Vec<Wanted<'a>>,
}

fn groups<'a>(runner: &Runner, cwd: &Path, selection: &Selection<'a>) -> Vec<Group<'a>> {
    let rel = |f: &str| cwd_relative(f, &runner.config.cwd);
    let index = targets::index(cwd);
    let mut by_target: BTreeMap<Target, Vec<Wanted<'a>>> = BTreeMap::new();
    let mut defs: BTreeMap<String, Vec<TestDef>> = BTreeMap::new();
    let mut tests_in = |file: &str| -> Vec<TestDef> {
        defs.entry(file.to_string())
            .or_insert_with(|| {
                let source = std::fs::read_to_string(cwd.join(file)).unwrap_or_default();
                lang::find_tests(Lang::Rust, &source, PythonStyle::Pytest)
            })
            .clone()
    };
    let mut add = |file: &str, case: Option<&'a TestCase>, def: &TestDef| {
        for r in index.refs(file) {
            // Cargo skips a target the command's features leave out, and a module path
            // that two files can fill gives their tests one name.
            if !index.builds(&r.target, &runner.config.command)
                || index.ambiguous(&r.target, &r.module)
            {
                continue;
            }
            let mut path = r.module.clone();
            path.extend(def.suite.iter().cloned());
            path.push(def.name.clone());
            by_target.entry(r.target.clone()).or_default().push(Wanted {
                case,
                name: path.join("::"),
                parametrized: def.parametrized,
            });
        }
    };
    match selection {
        Selection::Tests(cases) => {
            for case in cases.iter() {
                let file = rel(&case.file);
                let def = tests_in(&file)
                    .into_iter()
                    .find(|d| d.name == case.name && d.suite == case.suite);
                if let Some(def) = def {
                    add(&file, Some(case), &def);
                }
            }
        }
        // A file's own tests, not the rest of its target.
        Selection::Files(files) => {
            for file in files.iter() {
                let file = rel(file);
                for def in tests_in(&file) {
                    add(&file, None, &def);
                }
            }
        }
        Selection::Suite => {
            return vec![Group {
                target: None,
                wanted: Vec::new(),
            }];
        }
    }
    by_target
        .into_iter()
        .map(|(target, wanted)| Group {
            target: Some(target),
            wanted,
        })
        .collect()
}

/// The exact libtest names to run: each wanted name that the target lists, and for a test
/// that a macro expands, each case under its name.
fn resolve<'a>(wanted: &[Wanted<'a>], listed: &[String]) -> BTreeMap<String, Option<&'a TestCase>> {
    let mut out = BTreeMap::new();
    for w in wanted {
        if listed.contains(&w.name) {
            out.insert(w.name.clone(), w.case);
        } else if w.parametrized {
            let prefix = format!("{}::", w.name);
            for l in listed.iter().filter(|l| l.starts_with(&prefix)) {
                out.insert(l.clone(), w.case);
            }
        }
    }
    out
}

/// Exact names in batches that stay well under the OS limit on argument size.
fn batches(names: Vec<String>) -> Vec<Vec<String>> {
    const LIMIT: usize = 64 * 1024;
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    let mut size = 0;
    for name in names {
        if size + name.len() > LIMIT && !out.last().is_some_and(Vec::is_empty) {
            out.push(Vec::new());
            size = 0;
        }
        size += name.len() + 1;
        out.last_mut().expect("out is never empty").push(name);
    }
    out
}

/// Builds the group's target, or every test target for the suite. Returns false when the
/// build did not succeed, after recording why.
fn build(
    runner: &Runner,
    req: &RunRequest,
    target_args: &[String],
    run: &mut TestRun,
) -> anyhow::Result<bool> {
    let mut args = vec!["--no-run".to_string(), "--message-format=short".to_string()];
    args.extend(target_args.iter().cloned());
    let build = exec(runner, req, &args, Some(BUILD_TIMEOUT))?;
    if build.timed_out {
        run.timed_out = true;
        run.past_deadline |= build.past_deadline;
        run.incomplete = true;
        return Ok(false);
    }
    if build.success() {
        return Ok(true);
    }
    let errors = compile_errors(&build.stderr);
    let summary = if errors.is_empty() {
        output_tail(&build)
    } else {
        ERROR_LINE
            .find_iter(&build.stderr)
            .take(10)
            .map(|m| m.as_str().trim())
            .collect::<Vec<_>>()
            .join("\n")
    };
    run.runner_error = Some(format!("the build failed:\n{summary}"));
    run.compile_errors.extend(errors);
    run.incomplete = true;
    Ok(false)
}

/// The test names that the target's binary lists, or why it could not list them: None when
/// the check deadline stopped the listing.
fn list(
    runner: &Runner,
    req: &RunRequest,
    target_args: &[String],
) -> anyhow::Result<Result<Vec<String>, Option<String>>> {
    let mut args = target_args.to_vec();
    args.extend(["--", "--list", "--format", "terse"].map(String::from));
    let out = exec(runner, req, &args, Some(BUILD_TIMEOUT))?;
    if out.past_deadline {
        return Ok(Err(None));
    }
    if out.timed_out || !out.success() {
        return Ok(Err(Some(format!(
            "cannot list the tests of {}:\n{}",
            target_args.join(" "),
            output_tail(&out)
        ))));
    }
    Ok(Ok(out
        .stdout
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .map(str::to_string)
        .collect()))
}

/// The suite's arguments: every member of a workspace.
fn suite_args(cwd: &Path) -> Vec<String> {
    let workspace = std::fs::read_to_string(cwd.join("Cargo.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .is_some_and(|m| m.contains_key("workspace"));
    if workspace {
        vec!["--workspace".to_string()]
    } else {
        Vec::new()
    }
}

pub fn run(runner: &Runner, req: &RunRequest) -> anyhow::Result<TestRun> {
    let cwd = runner.cwd_in(req.worktree);
    let mut run = TestRun {
        outcomes: Vec::new(),
        file_errors: Vec::new(),
        timed_out: false,
        past_deadline: false,
        duration: Duration::ZERO,
        max_rss_bytes: 0,
        runner_error: None,
        compile_errors: Vec::new(),
        incomplete: false,
    };
    let sequential = matches!(req.selection, Selection::Tests(_));
    'groups: for group in groups(runner, &cwd, &req.selection) {
        let target_args = match &group.target {
            Some(t) => t.args(),
            None => suite_args(&cwd),
        };
        if !build(runner, req, &target_args, &mut run)? {
            if run.timed_out {
                break;
            }
            continue;
        }
        let selected = match &group.target {
            Some(_) => {
                let listed = match list(runner, req, &target_args)? {
                    Ok(listed) => listed,
                    Err(Some(why)) => {
                        run.runner_error = Some(why);
                        run.incomplete = true;
                        continue;
                    }
                    Err(None) => {
                        run.timed_out = true;
                        run.past_deadline = true;
                        run.incomplete = true;
                        break;
                    }
                };
                let selected = resolve(&group.wanted, &listed);
                if selected.is_empty() {
                    continue;
                }
                Some(selected)
            }
            None => None,
        };
        let names: Vec<Vec<String>> = match &selected {
            Some(s) => batches(s.keys().cloned().collect()),
            None => vec![Vec::new()],
        };
        let mut failed = false;
        for batch in names {
            let remaining = req.timeout.map(|t| t.saturating_sub(run.duration));
            if remaining.is_some_and(|r| r.is_zero()) {
                run.timed_out = true;
                break 'groups;
            }
            let mut args = target_args.clone();
            args.extend(["--".to_string(), "--color".to_string(), "never".to_string()]);
            if selected.is_some() {
                args.push("--exact".to_string());
                if sequential {
                    args.push("--test-threads=1".to_string());
                }
                args.extend(batch);
            }
            let out = exec(runner, req, &args, remaining)?;
            run.duration += out.duration;
            run.max_rss_bytes = run.max_rss_bytes.max(out.max_rss_bytes);
            failed |= read_output(
                &out,
                &group,
                selected.as_ref(),
                &runner.config.cwd,
                &mut run,
            );
            if out.timed_out {
                run.timed_out = true;
                run.past_deadline |= out.past_deadline;
                break 'groups;
            }
        }
        if failed && req.fail_fast {
            break;
        }
    }
    Ok(run)
}

/// Adds one invocation's results to the run. Returns whether any test failed or crashed.
/// `selected` maps the exact names a target run selected to their cases; None for the
/// suite.
fn read_output(
    out: &RunOutput,
    group: &Group,
    selected: Option<&BTreeMap<String, Option<&TestCase>>>,
    cwd: &str,
    run: &mut TestRun,
) -> bool {
    let blocks = parse(&out.stdout);
    let files = binary_files(&out.stderr);
    let case_of = |name: &str| selected.and_then(|s| s.get(name).copied().flatten());
    let mut failed = false;
    for (i, block) in blocks.iter().enumerate() {
        let file = match &group.target {
            Some(t) => format!("./{}", t.root),
            None => files.get(i).cloned().unwrap_or_default(),
        };
        if !block.consistent() {
            run.incomplete = true;
            run.runner_error = Some(format!(
                "the test output of {file} disagrees with its own summary, for example because a test printed to stdout; titer reads no result from it"
            ));
            continue;
        }
        for (name, status) in &block.results {
            if selected.is_some_and(|s| !s.contains_key(name)) {
                continue;
            }
            failed |= matches!(status, Status::Failed(_));
            run.outcomes
                .push(outcome(case_of(name), &file, name, status.clone(), cwd));
        }
        if block.finished || out.timed_out {
            continue;
        }
        failed = true;
        let failure = Failure {
            kind: FailureKind::Exception,
            exception_type: None,
            message: format!("the test binary crashed: {}", crash_reason(out)),
        };
        match &block.running {
            Some(name) => run.outcomes.push(outcome(
                case_of(name),
                &file,
                name,
                Status::Failed(failure),
                cwd,
            )),
            None => run.file_errors.push(FileError { file, failure }),
        }
    }
    if blocks.is_empty() && !out.success() && !out.timed_out {
        run.runner_error = Some(output_tail(out));
        run.incomplete = true;
    }
    failed
}

fn exec(
    runner: &Runner,
    req: &RunRequest,
    args: &[String],
    timeout: Option<Duration>,
) -> anyhow::Result<RunOutput> {
    let mut cmd = runner.command(req.worktree, args);
    cmd.env("CARGO_TERM_COLOR", "never");
    // Each worktree builds into its own dir, whatever the environment, the runner's `env`,
    // or a cargo config says: a shared dir would let one worktree's mutant overwrite
    // another's binaries, and a dir in the checkout would be a write to the checkout.
    let cache = req.worktree.join(".titer-cache");
    let target = cache.join("cargo-target");
    // A symlinked dir, such as one linked in from the checkout as a dependency dir, would
    // lead the build out of the worktree.
    for dir in [&cache, &target] {
        if dir.is_symlink() {
            anyhow::bail!(
                "refusing to build into {}: it is a symlink out of the worktree",
                dir.display()
            );
        }
    }
    // Cargo writes the lockfile, and a symlinked one can point into the checkout.
    let cwd = runner.cwd_in(req.worktree);
    for dir in cwd.ancestors().take_while(|d| d.starts_with(req.worktree)) {
        let lock = dir.join("Cargo.lock");
        if lock.is_symlink() {
            anyhow::bail!(
                "refusing to run cargo: {} is a symlink, and cargo may rewrite it",
                lock.display()
            );
        }
    }
    cmd.env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_BUILD_DIR", &target);
    if !runner.config.env.contains_key("RUST_BACKTRACE") {
        cmd.env("RUST_BACKTRACE", "0");
    }
    super::run_limited(cmd, req.deadline.limit(timeout), req.scratch)
}

/// The outcome for a reported libtest name. A selected case's outcome carries its id, so
/// tests with one name in two targets stay apart.
fn outcome(
    case: Option<&TestCase>,
    file: &str,
    name: &str,
    status: Status,
    cwd: &str,
) -> TestOutcome {
    match case {
        Some(case) => TestOutcome {
            id: Some(case.id.clone()),
            file: format!("./{}", cwd_relative(&case.file, cwd)),
            suite: case.suite.clone(),
            name: case.name.clone(),
            line: None,
            status,
        },
        None => TestOutcome {
            id: None,
            file: file.to_string(),
            suite: Vec::new(),
            name: name.to_string(),
            line: None,
            status,
        },
    }
}

/// The source of each test binary that cargo ran, in order: `src/lib.rs` for
/// `Running unittests src/lib.rs (...)`, `tests/api.rs` for `Running tests/api.rs (...)`,
/// and the crate name for doc tests.
fn binary_files(stderr: &str) -> Vec<String> {
    RUNNING
        .captures_iter(stderr)
        .map(|c| match (c.get(1), c.get(2)) {
            (Some(path), _) => path.as_str().to_string(),
            (None, Some(krate)) => format!("doctests of {}", krate.as_str()),
            _ => String::new(),
        })
        .collect()
}

static RUNNING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:Running (?:unittests )?(\S+) \(|Doc-tests (\S+)\s*$)").unwrap()
});
static ERROR_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(\S.*?):(\d+):(\d+): error(?:\[(E\d+)\])?: (.*)$").unwrap());
static TEST_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^test (.+?)(?: - should panic)? \.\.\. ?(ok|FAILED|ignored.*)?$").unwrap()
});
static SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^---- (.+?) std(?:out|err) ----$").unwrap());
static PANIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^thread '[^']*'(?: \(\d+\))? panicked at (?:'(.*)', \S+|\S+:\d+:\d+:)$").unwrap()
});
static SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored;").unwrap()
});
static SIGNAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\((signal: \d+[^)]*|exit status: \d+)\)").unwrap());

/// The compile errors in cargo's `--message-format=short` output.
fn compile_errors(stderr: &str) -> Vec<CompileError> {
    ERROR_LINE
        .captures_iter(stderr)
        .map(|c| CompileError {
            file: c[1].to_string(),
            line: c[2].parse().unwrap_or(0),
            message: match c.get(4) {
                Some(code) => format!("error[{}]: {}", code.as_str(), &c[5]),
                None => format!("error: {}", &c[5]),
            },
        })
        .collect()
}

fn crash_reason(out: &RunOutput) -> String {
    SIGNAL
        .captures(&out.stderr)
        .map(|c| c[1].to_string())
        .or_else(|| out.signal.map(|s| format!("signal: {s}")))
        .unwrap_or_else(|| "the binary exited without a test result".to_string())
}

/// One test binary's output.
#[derive(Debug, Default)]
struct Block {
    results: Vec<(String, Status)>,
    /// A test that started and never finished.
    running: Option<String>,
    /// The binary printed its `test result:` line.
    finished: bool,
    /// The passed, failed, and ignored counts of that line.
    summary: Option<(usize, usize, usize)>,
}

impl Block {
    /// Whether the per-test lines agree with the binary's own counts. A test that prints
    /// to stdout can garble or forge result lines, and then no line is trusted.
    fn consistent(&self) -> bool {
        let Some((passed, failed, ignored)) = self.summary else {
            return !self.finished;
        };
        let count = |f: fn(&Status) -> bool| self.results.iter().filter(|(_, s)| f(s)).count();
        count(|s| *s == Status::Passed) == passed
            && count(|s| matches!(s, Status::Failed(_))) == failed
            && count(|s| *s == Status::Skipped) == ignored
            && self.results.iter().all(|(n, _)| !n.contains(" ... "))
    }
}

fn parse(stdout: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut sections: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut section: Option<String> = None;
    for line in stdout.lines() {
        if line.starts_with("running ") && line.ends_with(" tests") || line == "running 1 test" {
            finish_block(blocks.last_mut(), &mut sections);
            blocks.push(Block::default());
            section = None;
            continue;
        }
        let Some(block) = blocks.last_mut() else {
            continue;
        };
        if let Some(caps) = SECTION.captures(line) {
            section = Some(caps[1].to_string());
            continue;
        }
        if line == "failures:" || line.starts_with("test result: ") {
            section = None;
            if line.starts_with("test result: ") {
                block.finished = true;
                block.summary = SUMMARY.captures(line).map(|c| {
                    let n = |i: usize| c[i].parse().unwrap_or(usize::MAX);
                    (n(1), n(2), n(3))
                });
            }
            continue;
        }
        if let Some(name) = &section {
            sections.entry(name.clone()).or_default().push(line);
            continue;
        }
        if let Some(caps) = TEST_LINE.captures(line) {
            let name = caps[1].to_string();
            block.running = None;
            match caps.get(2).map(|m| m.as_str()) {
                Some("ok") => block.results.push((name, Status::Passed)),
                Some("FAILED") => block.results.push((name, Status::Failed(placeholder()))),
                Some(_) => block.results.push((name, Status::Skipped)),
                None => block.running = Some(name),
            }
        }
    }
    finish_block(blocks.last_mut(), &mut sections);
    blocks
}

fn placeholder() -> Failure {
    Failure {
        kind: FailureKind::Exception,
        exception_type: None,
        message: String::new(),
    }
}

/// Fills each failure from its captured output section.
fn finish_block(block: Option<&mut Block>, sections: &mut BTreeMap<String, Vec<&str>>) {
    if let Some(block) = block {
        for (name, status) in block.results.iter_mut() {
            if let Status::Failed(f) = status {
                *f = failure_from(sections.get(name.as_str()).map_or(&[][..], |s| s));
            }
        }
    }
    sections.clear();
}

/// A failure from a test's captured output: the panic message, or what a test that returned
/// an error printed.
fn failure_from(lines: &[&str]) -> Failure {
    let mut message: Vec<&str> = Vec::new();
    let mut in_panic = false;
    for line in lines {
        if let Some(caps) = PANIC.captures(line) {
            message.clear();
            if let Some(inline) = caps.get(1) {
                message.push(inline.as_str());
            } else {
                in_panic = true;
            }
            continue;
        }
        if in_panic {
            if line.starts_with("note: run with") || line.starts_with("stack backtrace:") {
                in_panic = false;
                continue;
            }
            message.push(line);
        }
    }
    if message.is_empty() {
        message = lines
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty() && !l.starts_with("note: run with"))
            .collect();
    }
    while message.last().is_some_and(|l| l.trim().is_empty()) {
        message.pop();
    }
    let text = message
        .iter()
        .take(6)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let first = text.trim_start();
    let kind = if first.starts_with("assertion")
        || first.contains("snapshot assertion")
        || first.contains("did not panic as expected")
        || first.contains("panic did not contain expected string")
    {
        FailureKind::Assertion
    } else {
        FailureKind::Exception
    };
    Failure {
        kind,
        exception_type: None,
        message: text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_sections_and_an_unfinished_test() {
        let stdout = "\nrunning 4 tests\ntest tests::a ... ok\ntest tests::b ... FAILED\ntest tests::c ... ignored, slow\ntest tests::sp - should panic ... FAILED\n\nfailures:\n\n---- tests::b stdout ----\n\nthread 'tests::b' (12) panicked at src/lib.rs:6:25:\nassertion `left == right` failed\n  left: 3\n right: 4\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n---- tests::sp stdout ----\nnote: test did not panic as expected at src/lib.rs:10:32\n\nfailures:\n    tests::b\n    tests::sp\n\ntest result: FAILED. 1 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n\nrunning 2 tests\ntest x ... ok\ntest y ... ";
        let blocks = parse(stdout);
        assert_eq!(blocks.len(), 2);
        let first = &blocks[0];
        assert!(first.finished);
        assert_eq!(first.results[0], ("tests::a".to_string(), Status::Passed));
        let Status::Failed(b) = &first.results[1].1 else {
            panic!("{first:?}")
        };
        assert_eq!(b.kind, FailureKind::Assertion);
        assert_eq!(
            b.message,
            "assertion `left == right` failed\n  left: 3\n right: 4"
        );
        assert_eq!(first.results[2].1, Status::Skipped);
        let Status::Failed(sp) = &first.results[3].1 else {
            panic!("{first:?}")
        };
        assert_eq!(sp.kind, FailureKind::Assertion);
        let second = &blocks[1];
        assert!(!second.finished);
        assert_eq!(second.running.as_deref(), Some("y"));
    }

    #[test]
    fn output_that_disagrees_with_the_summary_is_not_trusted() {
        // A test that prints a result line of its own while it runs.
        let forged = "\nrunning 1 test\ntest works ... test works ... FAILED\nok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let blocks = parse(forged);
        assert!(blocks[0].finished);
        assert!(!blocks[0].consistent());
        let honest = "\nrunning 2 tests\ntest a ... ok\ntest b ... ignored\n\ntest result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        assert!(parse(honest)[0].consistent());
    }

    #[test]
    fn classifies_old_style_panics_and_returned_errors() {
        let f = failure_from(&[
            "thread 'a' panicked at 'index out of bounds: the len is 0 but the index is 1', src/lib.rs:3:5",
        ]);
        assert_eq!(f.kind, FailureKind::Exception);
        assert!(
            f.message.starts_with("index out of bounds"),
            "{}",
            f.message
        );
        let f = failure_from(&["Error: \"boom\""]);
        assert_eq!(f.kind, FailureKind::Exception);
        assert_eq!(f.message, "Error: \"boom\"");
    }

    #[test]
    fn reads_short_compile_errors_and_binary_sources() {
        let stderr = "   Compiling x v0.1.0 (/w)\nsrc/math.rs:3:51: error[E0425]: cannot find function `div` in module `super`: not found in `super`\nsrc/lib.rs:1:1: warning: unused\nerror: could not compile `x` (lib test) due to 1 previous error\n";
        assert_eq!(
            compile_errors(stderr),
            vec![CompileError {
                file: "src/math.rs".to_string(),
                line: 3,
                message: "error[E0425]: cannot find function `div` in module `super`: not found in `super`".to_string(),
            }]
        );
        let stderr = "     Running unittests src/lib.rs (target/debug/deps/x-1)\n     Running tests/api.rs (target/debug/deps/api-2)\n   Doc-tests demo_calc\n";
        assert_eq!(
            binary_files(stderr),
            vec!["src/lib.rs", "tests/api.rs", "doctests of demo_calc"]
        );
    }
}
