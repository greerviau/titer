//! The prove tier: run the changed tests on head, then on base code with head's test paths
//! overlaid, and classify each changed test.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::config::RunnerKind;
use crate::detect::{Detection, read_head};
use crate::imports::{self, Trees};
use crate::lang::{self, Lang, PythonStyle};
use crate::report::{HeadStatus, TestResult, Verdict};
use crate::runner::{
    CaseResult, CompileError, Failure, FailureKind, RunRequest, Runner, Selection, TestCase,
    TestRun, same_file,
};
use crate::session::Session;
use crate::worktree::Worktree;

/// The changed tests of one runner and their results on head.
#[derive(Debug, Clone)]
pub struct HeadRun {
    pub runner: usize,
    pub cases: Vec<TestCase>,
    pub results: Vec<CaseResult>,
    pub run: TestRun,
}

impl HeadRun {
    pub fn status(&self, i: usize) -> HeadStatus {
        match &self.results[i] {
            CaseResult::Passed => HeadStatus::Green,
            CaseResult::Failed(_) => HeadStatus::Red,
            CaseResult::Skipped => HeadStatus::Skipped,
            CaseResult::NotRun => HeadStatus::NoResult,
        }
    }

    /// The changed tests that pass on head: the selection for base and for mutants.
    pub fn green(&self) -> Vec<TestCase> {
        self.cases
            .iter()
            .enumerate()
            .filter(|(i, _)| self.status(*i) == HeadStatus::Green)
            .map(|(_, c)| c.clone())
            .collect()
    }

    pub fn red(&self) -> Vec<&TestCase> {
        self.cases
            .iter()
            .enumerate()
            .filter(|(i, _)| self.status(*i) == HeadStatus::Red)
            .map(|(_, c)| c)
            .collect()
    }
}

/// Runs each runner's changed tests on head.
pub fn run_head(
    session: &Session,
    runners: &[Runner],
    detection: &Detection,
    head: &Worktree,
    timeout: Duration,
) -> anyhow::Result<Vec<HeadRun>> {
    let mut out = Vec::new();
    for (i, runner) in runners.iter().enumerate() {
        let cases = detection.tests_for(runner.name());
        if cases.is_empty() {
            continue;
        }
        let run = runner.run(&RunRequest {
            worktree: &head.path,
            selection: Selection::Tests(&cases),
            fail_fast: false,
            timeout: Some(timeout),
            scratch: &session.run_dir.join("logs"),
        })?;
        let results = cases
            .iter()
            .map(|c| {
                if run.timed_out {
                    return CaseResult::Failed(Failure {
                        kind: FailureKind::Exception,
                        exception_type: None,
                        message: format!("the head run timed out after {}s", timeout.as_secs()),
                    });
                }
                match run.result_for(c, &runner.config.cwd) {
                    CaseResult::NotRun if run.runner_error.is_some() => {
                        CaseResult::Failed(Failure {
                            kind: FailureKind::Exception,
                            exception_type: None,
                            message: run.runner_error.clone().unwrap_or_default(),
                        })
                    }
                    other => other,
                }
            })
            .collect();
        out.push(HeadRun {
            runner: i,
            cases,
            results,
            run,
        });
    }
    Ok(out)
}

/// The prove tier proper: one TestResult per changed test.
pub fn prove(
    session: &Session,
    runners: &[Runner],
    detection: &Detection,
    heads: &[HeadRun],
    base: &Worktree,
) -> anyhow::Result<Vec<TestResult>> {
    let head_files = session.git.list_files(&session.head_tree)?;
    let mut base_files = session.git.list_files(&session.base_sha)?;
    for (_, path) in &detection.test_paths {
        base_files.insert(path.clone());
    }
    let read_head = |p: &str| {
        session
            .git
            .read_blob(&session.head_tree, p)
            .ok()
            .flatten()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let read_base = |p: &str| std::fs::read_to_string(base.path.join(p)).ok();

    let mut results = Vec::new();
    for head in heads {
        let runner = &runners[head.runner];
        let aliases = imports::ts_aliases(&runner.config.cwd, &read_head);
        let trees = Trees {
            head: &head_files,
            base: &base_files,
            read_head: &read_head,
            read_base: &read_base,
            aliases: &aliases,
        };
        let mut statics = static_verdicts(session, runner, head, &trees)?;
        let green = head.green();
        let base_run = if green.is_empty() {
            None
        } else {
            Some(run_base(
                session,
                runner,
                detection,
                head,
                green,
                base,
                &mut statics,
            )?)
        };
        for (i, case) in head.cases.iter().enumerate() {
            let status = head.status(i);
            let mut result = TestResult {
                id: case.id.clone(),
                runner: runner.name().to_string(),
                file: case.file.clone(),
                line: case.line,
                name: case.display_name(),
                head: status,
                verdict: None,
                exception_type: None,
                message: None,
            };
            match status {
                HeadStatus::Red => {
                    if let CaseResult::Failed(f) = &head.results[i] {
                        result.exception_type = f.exception_type.clone();
                        result.message = Some(f.message.clone());
                    }
                }
                HeadStatus::Skipped => result.verdict = Some(Verdict::Skipped),
                HeadStatus::NoResult => {
                    result.verdict = Some(Verdict::NoResult);
                    result.message = Some("the selection executed no test on head".to_string());
                }
                HeadStatus::Green => {
                    if let Some(message) = &statics[i] {
                        result.verdict = Some(Verdict::MissingSymbol);
                        result.message = Some(message.clone());
                    } else if let Some(run) = &base_run {
                        let (verdict, failure) = classify_base(run, case, &runner.config.cwd);
                        result.verdict = Some(verdict);
                        if let Some(f) = failure {
                            result.exception_type = f.exception_type;
                            result.message = Some(f.message);
                        }
                    }
                }
            }
            results.push(result);
        }
    }
    Ok(results)
}

/// Runs the head-green changed tests on base. A Rust test that does not compile against
/// base stops its whole target from building, so each round sets aside the tests that the
/// compile errors fall in as missing symbols, and builds again without them.
fn run_base(
    session: &Session,
    runner: &Runner,
    detection: &Detection,
    head: &HeadRun,
    mut selected: Vec<TestCase>,
    base: &Worktree,
    statics: &mut [Option<String>],
) -> anyhow::Result<TestRun> {
    let timeout = head.run.duration * 3 + Duration::from_secs(10);
    let mut set_aside: Vec<TestCase> = Vec::new();
    loop {
        let run = runner.run(&RunRequest {
            worktree: &base.path,
            selection: Selection::Tests(&selected),
            fail_fast: false,
            timeout: Some(timeout),
            scratch: &session.run_dir.join("logs"),
        })?;
        if runner.config.kind != RunnerKind::Cargo || run.compile_errors.is_empty() {
            return Ok(run);
        }
        let blamed = blame(&run.compile_errors, &selected, base, session)?;
        if blamed.is_empty() {
            return Ok(run);
        }
        for (case, message) in &blamed {
            if let Some(i) = head.cases.iter().position(|c| c == case) {
                statics[i] = Some(message.clone());
            }
            set_aside.push(case.clone());
        }
        selected.retain(|c| !set_aside.contains(c));
        let mut files: Vec<&str> = blamed.iter().map(|(c, _)| c.file.as_str()).collect();
        files.sort();
        files.dedup();
        for file in files {
            let head_text = read_head(session, file)?;
            let without = remove_tests(&head_text, file, &set_aside);
            write_base_copy(session, detection, base, file, &without)?;
        }
        if selected.is_empty() {
            return Ok(run);
        }
    }
}

/// The selected tests that compile errors fall in, each with its first error. An error
/// inside a test blames that test. An error outside every test, such as in a `use`, blames
/// the tests in its file that name what it reports missing.
fn blame(
    errors: &[CompileError],
    selected: &[TestCase],
    base: &Worktree,
    session: &Session,
) -> anyhow::Result<Vec<(TestCase, String)>> {
    let mut out: Vec<(TestCase, String)> = Vec::new();
    let mut files: Vec<&str> = selected.iter().map(|c| c.file.as_str()).collect();
    files.sort();
    files.dedup();
    for file in files {
        let errors: Vec<&CompileError> =
            errors.iter().filter(|e| same_file(&e.file, file)).collect();
        if errors.is_empty() {
            continue;
        }
        let compiled = std::fs::read_to_string(base.path.join(file)).unwrap_or_default();
        let defs = lang::find_tests(Lang::Rust, &compiled, PythonStyle::Pytest);
        let head_text = read_head(session, file)?;
        let head_defs = lang::find_tests(Lang::Rust, &head_text, PythonStyle::Pytest);
        for case in selected.iter().filter(|c| c.file == file) {
            let inside = errors.iter().find(|e| {
                defs.iter().any(|d| {
                    (d.line..=d.end_line).contains(&e.line)
                        && d.name == case.name
                        && d.suite == case.suite
                })
            });
            let named = || {
                let text = head_defs
                    .iter()
                    .find(|d| d.report_line == case.line)
                    .map_or("", |d| &head_text[d.start_byte..d.end_byte]);
                errors.iter().find(|e| {
                    let outside = !defs.iter().any(|d| (d.line..=d.end_line).contains(&e.line));
                    outside && missing_name(&e.message).is_some_and(|n| mentions(text, &n))
                })
            };
            if let Some(error) = inside.or_else(named) {
                out.push((case.clone(), error.message.clone()));
            }
        }
    }
    Ok(out)
}

static BACKTICKED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());

/// The name a compile error reports missing: the last path segment of its first quoted
/// name, `mul` for "unresolved import `demo::mul`".
fn missing_name(message: &str) -> Option<String> {
    let quoted = BACKTICKED.captures(message)?.get(1)?.as_str();
    let last = quoted.rsplit("::").next().unwrap_or(quoted);
    let name: String = last
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn mentions(text: &str, name: &str) -> bool {
    Regex::new(&format!(r"\b{}\b", regex::escape(name))).is_ok_and(|re| re.is_match(text))
}

/// `source` without the tests in `cases` that belong to `file`.
fn remove_tests(source: &str, file: &str, cases: &[TestCase]) -> String {
    let mut spans: Vec<(usize, usize)> = lang::find_tests(Lang::Rust, source, PythonStyle::Pytest)
        .into_iter()
        .filter(|d| {
            cases
                .iter()
                .any(|c| c.file == file && c.name == d.name && c.suite == d.suite)
        })
        .map(|d| (d.start_byte, d.end_byte))
        .collect();
    spans.sort();
    let mut out = source.to_string();
    for (start, end) in spans.into_iter().rev() {
        out.replace_range(start..end, "");
    }
    out
}

/// Writes head's tests for `file` into the base worktree: the whole file for a test path,
/// or base's source with head's test items spliced in for a Rust source file.
fn write_base_copy(
    session: &Session,
    detection: &Detection,
    base: &Worktree,
    file: &str,
    head_text: &str,
) -> anyhow::Result<()> {
    if !detection.rust_sources.iter().any(|f| f == file) {
        return base.write(file, head_text.as_bytes());
    }
    let Some(original) = session.git.read_blob(&session.base_sha, file)? else {
        return Ok(());
    };
    let original = String::from_utf8_lossy(&original);
    let spliced =
        lang::rust::splice_tests(&original, head_text).unwrap_or_else(|| original.to_string());
    base.write(file, spliced.as_bytes())
}

/// Splices head's test items into base's copy of each changed Rust source file. Base's
/// source code stays, so the tests run against it.
pub fn splice_rust_sources(
    session: &Session,
    detection: &Detection,
    base: &Worktree,
) -> anyhow::Result<()> {
    for file in &detection.rust_sources {
        let head_text = read_head(session, file)?;
        write_base_copy(session, detection, base, file, &head_text)?;
    }
    Ok(())
}

/// The static import check's message for each changed test, when it imports a name that
/// only head has.
fn static_verdicts(
    session: &Session,
    runner: &Runner,
    head: &HeadRun,
    trees: &Trees,
) -> anyhow::Result<Vec<Option<String>>> {
    let mut out = vec![None; head.cases.len()];
    let mut files: Vec<&str> = head.cases.iter().map(|c| c.file.as_str()).collect();
    files.dedup();
    for file in files {
        let Some(lang) = Lang::of(file) else { continue };
        let source = read_head(session, file)?;
        let missing = imports::missing_imports(&runner.config, file, &source, trees);
        if missing.is_empty() {
            continue;
        }
        let style = crate::detect::python_style(runner.config.kind);
        let spans: Vec<(u32, u32)> = lang::find_tests(lang, &source, style)
            .iter()
            .map(|t| (t.line, t.end_line))
            .collect();
        for (i, case) in head.cases.iter().enumerate() {
            if case.file != file {
                continue;
            }
            let span = spans
                .iter()
                .copied()
                .find(|(s, e)| (*s..=*e).contains(&case.line))
                .unwrap_or((case.line, case.end_line));
            if let Some(m) = imports::affecting(lang, &source, &missing, span, &spans) {
                out[i] = Some(m.message.clone());
            }
        }
    }
    Ok(out)
}

fn classify_base(run: &TestRun, case: &TestCase, cwd: &str) -> (Verdict, Option<Failure>) {
    match run.result_for(case, cwd) {
        CaseResult::Passed => (Verdict::PassesOnBase, None),
        CaseResult::Skipped => (Verdict::Skipped, None),
        CaseResult::NotRun if run.timed_out => (
            Verdict::Timeout,
            Some(Failure {
                kind: FailureKind::Exception,
                exception_type: None,
                message: "the base run timed out".to_string(),
            }),
        ),
        CaseResult::NotRun => (
            Verdict::NoResult,
            Some(Failure {
                kind: FailureKind::Exception,
                exception_type: None,
                message: run
                    .runner_error
                    .clone()
                    .unwrap_or_else(|| "the selection executed no test on base".to_string()),
            }),
        ),
        CaseResult::Failed(f) => (classify_failure(&f), Some(f)),
    }
}

static MISSING_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^module\s'[^']+'\shas\sno\sattribute
        | unexpected\skeyword\sargument
        | Cannot\sfind\smodule
        | Failed\sto\s(resolve|load)\s(import|url)
        | does\snot\sprovide\san\sexport\snamed
        | is\snot\sexported\sby
        | Cannot\sfind\spackage",
    )
    .unwrap()
});

/// Classifies a base failure. A name or module that only head has is a missing symbol, not
/// proof; any other exception counts as the code under test failing.
pub fn classify_failure(failure: &Failure) -> Verdict {
    let short = failure
        .exception_type
        .as_deref()
        .map(|t| t.rsplit('.').next().unwrap_or(t));
    let missing = matches!(short, Some("ImportError" | "ModuleNotFoundError"))
        || (matches!(
            short,
            Some("AttributeError" | "TypeError" | "SyntaxError" | "Error") | None
        ) && MISSING_MESSAGE.is_match(&failure.message));
    if missing {
        Verdict::MissingSymbol
    } else if failure.kind == FailureKind::Assertion {
        Verdict::Assertion
    } else {
        Verdict::Exception
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(kind: FailureKind, ty: Option<&str>, message: &str) -> Failure {
        Failure {
            kind,
            exception_type: ty.map(str::to_string),
            message: message.to_string(),
        }
    }

    #[test]
    fn classifies_base_failures() {
        use FailureKind::*;
        let cases = [
            (
                failure(Assertion, Some("AssertionError"), "1 != 2"),
                Verdict::Assertion,
            ),
            (
                failure(Exception, Some("KeyError"), "'exclude'"),
                Verdict::Exception,
            ),
            (
                failure(
                    Exception,
                    Some("pytz.exceptions.UnknownTimeZoneError"),
                    "'X'",
                ),
                Verdict::Exception,
            ),
            (
                failure(Exception, Some("ImportError"), "cannot import name 'x'"),
                Verdict::MissingSymbol,
            ),
            (
                failure(
                    Exception,
                    Some("ModuleNotFoundError"),
                    "No module named 'x'",
                ),
                Verdict::MissingSymbol,
            ),
            (
                failure(
                    Exception,
                    Some("AttributeError"),
                    "module 'app.core' has no attribute 'x'",
                ),
                Verdict::MissingSymbol,
            ),
            (
                failure(
                    Exception,
                    Some("AttributeError"),
                    "'NoneType' object has no attribute 'x'",
                ),
                Verdict::Exception,
            ),
            (
                failure(
                    Exception,
                    Some("TypeError"),
                    "f() got an unexpected keyword argument 'tz'",
                ),
                Verdict::MissingSymbol,
            ),
            (
                failure(Exception, Some("TypeError"), "unsupported operand"),
                Verdict::Exception,
            ),
            (
                failure(
                    Exception,
                    None,
                    "Cannot find module './storedBoolean' imported from x",
                ),
                Verdict::MissingSymbol,
            ),
            (
                failure(
                    Exception,
                    Some("Error"),
                    "Failed to resolve import \"./x\" from \"a.ts\"",
                ),
                Verdict::MissingSymbol,
            ),
            (
                failure(
                    Exception,
                    Some("SyntaxError"),
                    "The requested module './a' does not provide an export named 'b'",
                ),
                Verdict::MissingSymbol,
            ),
            (
                failure(
                    Exception,
                    Some("TypeError"),
                    "Cannot read properties of undefined",
                ),
                Verdict::Exception,
            ),
        ];
        for (f, expected) in cases {
            assert_eq!(classify_failure(&f), expected, "{f:?}");
        }
    }
}
