//! The prove tier: run the changed tests on head, then on base code with head's test paths
//! overlaid, and classify each changed test.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::detect::{Detection, read_head};
use crate::imports::{self, Trees};
use crate::lang::{self, Lang};
use crate::report::{HeadStatus, TestResult, Verdict};
use crate::runner::{
    CaseResult, Failure, FailureKind, RunRequest, Runner, Selection, TestCase, TestRun,
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
        let statics = static_verdicts(session, runner, head, &trees)?;
        let green = head.green();
        let base_run = if green.is_empty() {
            None
        } else {
            let timeout = head.run.duration * 3 + Duration::from_secs(10);
            Some(runner.run(&RunRequest {
                worktree: &base.path,
                selection: Selection::Tests(&green),
                fail_fast: false,
                timeout: Some(timeout),
                scratch: &session.run_dir.join("logs"),
            })?)
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
