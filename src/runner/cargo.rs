//! cargo test: build each selected target, run it with libtest's name filters, and parse
//! libtest's plain output.
//!
//! A test id is the file and the path inside it, `src/math.rs::tests::muls`. The file maps
//! to its package and target (`--lib`, `--bin`, or `--test`) and to its module path inside
//! the target, which together give libtest's name, `math::tests::muls`. Filters match by
//! substring, so a filter also selects tests that a parametrizing macro such as `rstest`
//! generates under the test's name; results are read back by exact name or that prefix.
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
use crate::process::RunOutput;

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Lib,
    Bin(String),
    Test(String),
}

/// The compiled target that holds a file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Target {
    package: Option<String>,
    kind: Kind,
    /// The target's root file, relative to the runner's cwd.
    root: String,
}

impl Target {
    fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(package) = &self.package {
            args.extend(["-p".to_string(), package.clone()]);
        }
        match &self.kind {
            Kind::Lib => args.push("--lib".to_string()),
            Kind::Bin(name) => args.extend(["--bin".to_string(), name.clone()]),
            Kind::Test(name) => args.extend(["--test".to_string(), name.clone()]),
        }
        args
    }
}

/// The target and module path of a cwd-relative file, from cargo's default layout. Files
/// outside it, such as a module that a `#[path]` attribute names, have no target.
fn target_of(cwd: &Path, rel: &str) -> Option<(Target, Vec<String>)> {
    let (package_dir, package) = package_of(cwd, rel)?;
    let in_package = if package_dir.is_empty() {
        rel
    } else {
        rel.strip_prefix(&format!("{package_dir}/"))?
    };
    let dir = cwd.join(&package_dir);
    let root = |file: &str| {
        if package_dir.is_empty() {
            file.to_string()
        } else {
            format!("{package_dir}/{file}")
        }
    };
    let target = |kind: Kind, file: &str| Target {
        package: package.clone(),
        kind,
        root: root(file),
    };
    let stem = |f: &str| f.strip_suffix(".rs").map(str::to_string);
    let parts: Vec<&str> = in_package.split('/').collect();
    let package_name = package.clone().unwrap_or_default();
    Some(match parts.as_slice() {
        ["src", "lib.rs"] => (target(Kind::Lib, "src/lib.rs"), Vec::new()),
        ["src", "main.rs"] => (target(Kind::Bin(package_name), "src/main.rs"), Vec::new()),
        ["src", "bin", file] => {
            let name = stem(file)?;
            let root = format!("src/bin/{file}");
            (target(Kind::Bin(name), &root), Vec::new())
        }
        ["src", "bin", name, rest @ ..] => {
            let root = format!("src/bin/{name}/main.rs");
            let module = if rest == ["main.rs"] {
                Vec::new()
            } else {
                module_path(rest)
            };
            (target(Kind::Bin(name.to_string()), &root), module)
        }
        ["src", rest @ ..] => {
            let kind = if dir.join("src/lib.rs").is_file() {
                (Kind::Lib, "src/lib.rs")
            } else if dir.join("src/main.rs").is_file() {
                (Kind::Bin(package_name), "src/main.rs")
            } else {
                return None;
            };
            (target(kind.0, kind.1), module_path(rest))
        }
        ["tests", file] => {
            let name = stem(file)?;
            (
                target(Kind::Test(name), &format!("tests/{file}")),
                Vec::new(),
            )
        }
        ["tests", name, rest @ ..] => {
            let root = format!("tests/{name}/main.rs");
            if !dir.join(&root).is_file() {
                return None;
            }
            let module = if rest == ["main.rs"] {
                Vec::new()
            } else {
                module_path(rest)
            };
            (target(Kind::Test(name.to_string()), &root), module)
        }
        _ => return None,
    })
}

/// `["a", "b.rs"]` and `["a", "b", "mod.rs"]` are module `a::b`.
fn module_path(parts: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
    if out.last().is_some_and(|l| l == "mod.rs") {
        out.pop();
    } else if let Some(last) = out.last_mut() {
        *last = last.trim_end_matches(".rs").to_string();
    }
    out
}

/// The nearest dir at or above `rel`'s dir, inside the cwd, with a `[package]` manifest:
/// its cwd-relative path and the package name.
fn package_of(cwd: &Path, rel: &str) -> Option<(String, Option<String>)> {
    let mut dir = Path::new(rel).parent();
    while let Some(d) = dir {
        if let Ok(text) = std::fs::read_to_string(cwd.join(d).join("Cargo.toml"))
            && let Ok(manifest) = text.parse::<toml::Table>()
            && let Some(package) = manifest.get("package")
        {
            let name = package
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::to_string);
            return Some((d.to_string_lossy().into_owned(), name));
        }
        dir = d.parent();
    }
    None
}

/// One cargo invocation: a target, or the whole configured suite, and its filters.
struct Group<'a> {
    target: Option<Target>,
    /// libtest name filters; empty runs every test in the target.
    filters: Vec<String>,
    /// Selected cases and their libtest names.
    cases: Vec<(&'a TestCase, String)>,
}

fn groups<'a>(runner: &Runner, cwd: &Path, selection: &Selection<'a>) -> Vec<Group<'a>> {
    let rel = |f: &str| cwd_relative(f, &runner.config.cwd);
    match selection {
        Selection::Tests(cases) => {
            let mut by_target: BTreeMap<Target, Vec<(&TestCase, String)>> = BTreeMap::new();
            for case in cases.iter() {
                let Some((target, module)) = target_of(cwd, &rel(&case.file)) else {
                    continue;
                };
                let mut path = module;
                path.extend(case.suite.iter().cloned());
                path.push(case.name.clone());
                by_target
                    .entry(target)
                    .or_default()
                    .push((case, path.join("::")));
            }
            by_target
                .into_iter()
                .map(|(target, cases)| {
                    let mut filters: Vec<String> = cases.iter().map(|(_, n)| n.clone()).collect();
                    filters.dedup();
                    Group {
                        target: Some(target),
                        filters,
                        cases,
                    }
                })
                .collect()
        }
        Selection::Files(files) => {
            let mut by_target: BTreeMap<Target, Option<Vec<String>>> = BTreeMap::new();
            for file in files.iter() {
                let Some((target, module)) = target_of(cwd, &rel(file)) else {
                    continue;
                };
                let entry = by_target.entry(target).or_insert_with(|| Some(Vec::new()));
                match (entry.as_mut(), module.is_empty()) {
                    // The target's root file selects the whole target.
                    (_, true) => *entry = None,
                    (Some(filters), false) => filters.push(format!("{}::", module.join("::"))),
                    (None, false) => {}
                }
            }
            by_target
                .into_iter()
                .map(|(target, filters)| Group {
                    target: Some(target),
                    filters: filters.unwrap_or_default(),
                    cases: Vec::new(),
                })
                .collect()
        }
        Selection::Suite => vec![Group {
            target: None,
            filters: Vec::new(),
            cases: Vec::new(),
        }],
    }
}

pub fn run(runner: &Runner, req: &RunRequest) -> anyhow::Result<TestRun> {
    let cwd = runner.cwd_in(req.worktree);
    let mut run = TestRun {
        outcomes: Vec::new(),
        file_errors: Vec::new(),
        timed_out: false,
        duration: Duration::ZERO,
        max_rss_bytes: 0,
        runner_error: None,
        compile_errors: Vec::new(),
    };
    let sequential = matches!(req.selection, Selection::Tests(_));
    for group in groups(runner, &cwd, &req.selection) {
        let target_args = group.target.as_ref().map(Target::args).unwrap_or_default();

        let mut args = vec!["--no-run".to_string(), "--message-format=short".to_string()];
        args.extend(target_args.iter().cloned());
        let build = exec(runner, req, &args, Some(BUILD_TIMEOUT))?;
        if build.timed_out {
            run.timed_out = true;
            break;
        }
        if !build.success() {
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
            continue;
        }

        let remaining = req.timeout.map(|t| t.saturating_sub(run.duration));
        if remaining.is_some_and(|r| r.is_zero()) {
            run.timed_out = true;
            break;
        }
        let mut args = target_args;
        args.extend(["--".to_string(), "--color".to_string(), "never".to_string()]);
        if sequential {
            args.push("--test-threads=1".to_string());
        }
        args.extend(group.filters.iter().cloned());
        let out = exec(runner, req, &args, remaining)?;
        run.duration += out.duration;
        run.max_rss_bytes = run.max_rss_bytes.max(out.max_rss_bytes);

        let blocks = parse(&out.stdout);
        let files = binary_files(&out.stderr);
        let mut failed = false;
        for (i, block) in blocks.iter().enumerate() {
            let file = match &group.target {
                Some(t) => format!("./{}", t.root),
                None => files.get(i).cloned().unwrap_or_default(),
            };
            for (name, status) in &block.results {
                failed |= matches!(status, Status::Failed(_));
                run.outcomes.push(outcome(
                    &group,
                    &file,
                    name,
                    status.clone(),
                    &runner.config.cwd,
                ));
            }
            if block.finished || out.timed_out {
                continue;
            }
            failed = true;
            let failure = Failure {
                kind: FailureKind::Exception,
                exception_type: None,
                message: format!("the test binary crashed: {}", crash_reason(&out)),
            };
            match &block.running {
                Some(name) => run.outcomes.push(outcome(
                    &group,
                    &file,
                    name,
                    Status::Failed(failure),
                    &runner.config.cwd,
                )),
                None => run.file_errors.push(FileError { file, failure }),
            }
        }
        if blocks.is_empty() && !out.success() && !out.timed_out {
            run.runner_error = Some(output_tail(&out));
        }
        if out.timed_out {
            run.timed_out = true;
            break;
        }
        if failed && req.fail_fast {
            break;
        }
    }
    Ok(run)
}

fn exec(
    runner: &Runner,
    req: &RunRequest,
    args: &[String],
    timeout: Option<Duration>,
) -> anyhow::Result<RunOutput> {
    let mut cmd = runner.command(req.worktree, args);
    cmd.env("CARGO_TERM_COLOR", "never");
    // Each worktree builds into its own dir: a target dir shared through the environment
    // would let one worktree's mutant overwrite another's binaries.
    for (key, value) in [
        (
            "CARGO_TARGET_DIR",
            req.worktree
                .join(".titer-cache/cargo-target")
                .into_os_string(),
        ),
        ("RUST_BACKTRACE", "0".into()),
    ] {
        if !runner.config.env.contains_key(key) {
            cmd.env(key, value);
        }
    }
    crate::process::run(cmd, timeout, req.scratch)
}

/// The outcome for a reported libtest name. A selected case's outcome carries its id, so
/// tests with one name in two targets stay apart.
fn outcome(group: &Group, file: &str, name: &str, status: Status, cwd: &str) -> TestOutcome {
    let case = group.cases.iter().find(|(_, n)| {
        name == n
            || name
                .strip_prefix(n.as_str())
                .is_some_and(|r| r.starts_with("::"))
    });
    match case {
        Some((case, _)) => TestOutcome {
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
    fn module_paths_follow_the_file_layout() {
        assert_eq!(module_path(&["math.rs"]), vec!["math"]);
        assert_eq!(module_path(&["a", "mod.rs"]), vec!["a"]);
        assert_eq!(module_path(&["a", "b.rs"]), vec!["a", "b"]);
    }

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
