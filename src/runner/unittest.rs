//! unittest: select by dotted id under the configured start dir, parse `-v` output.
//!
//! The start dir goes first on PYTHONPATH, as `unittest discover -s` puts it first on
//! sys.path, so test modules that import sibling modules (`from test_login import ...`) load
//! the same way they do under discovery.

use std::sync::LazyLock;

use regex::Regex;

use super::{
    Failure, FailureKind, FileError, RunRequest, Runner, Selection, Status, TestOutcome, TestRun,
    cwd_relative, output_tail,
};

/// The dotted module for a cwd-relative file under `start_dir`, such as `pkg.test_a`.
pub fn module_name(rel: &str, start_dir: &str) -> String {
    let under = if start_dir == "." {
        rel
    } else {
        rel.strip_prefix(start_dir)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(rel)
    };
    under.trim_end_matches(".py").replace('/', ".")
}

/// `module.Class.test_name`.
pub fn test_id(rel: &str, start_dir: &str, suite: &[String], name: &str) -> String {
    let mut parts = vec![module_name(rel, start_dir)];
    parts.extend(suite.iter().cloned());
    parts.push(name.to_string());
    parts.join(".")
}

fn module_path(module: &str, start_dir: &str) -> String {
    let path = format!("{}.py", module.replace('.', "/"));
    if start_dir == "." {
        path
    } else {
        format!("{start_dir}/{path}")
    }
}

pub fn run(runner: &Runner, req: &RunRequest) -> anyhow::Result<TestRun> {
    let config = &runner.config;
    // `discover` must be the first argument, so flags go after the selection.
    let mut args: Vec<String> = match req.selection {
        Selection::Tests(cases) => cases.iter().map(|c| c.id.clone()).collect(),
        Selection::Files(files) => files
            .iter()
            .map(|f| module_name(&cwd_relative(f, &config.cwd), &config.start_dir))
            .collect(),
        Selection::Suite => vec![
            "discover".to_string(),
            "-s".to_string(),
            config.start_dir.clone(),
            "-t".to_string(),
            config.start_dir.clone(),
        ],
    };
    let flags_at = if matches!(req.selection, Selection::Suite) {
        args.len()
    } else {
        0
    };
    let mut flags = vec!["-v".to_string()];
    if req.fail_fast {
        flags.push("-f".to_string());
    }
    args.splice(flags_at..flags_at, flags);
    let mut cmd = runner.command(req.worktree, &args);
    let start = runner.cwd_in(req.worktree).join(&config.start_dir);
    let mut python_path = start.into_os_string();
    let inherited = config
        .env
        .get("PYTHONPATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PYTHONPATH"))
        .filter(|p| !p.is_empty());
    if let Some(rest) = inherited {
        python_path.push(":");
        python_path.push(rest);
    }
    cmd.env("PYTHONPATH", python_path);
    let out = runner.exec(cmd, req)?;
    let parsed = parse(&out.stderr, &config.start_dir);
    let runner_error = (parsed.outcomes.is_empty()
        && parsed.file_errors.is_empty()
        && !out.timed_out
        && !out.stderr.contains("\nRan ")
        && !out.success())
    .then(|| output_tail(&out));
    Ok(TestRun {
        outcomes: parsed.outcomes,
        file_errors: parsed.file_errors,
        timed_out: out.timed_out,
        duration: out.duration,
        max_rss_bytes: out.max_rss_bytes,
        runner_error,
    })
}

#[derive(Debug, Default)]
struct Parsed {
    outcomes: Vec<TestOutcome>,
    file_errors: Vec<FileError>,
}

/// `name (dotted.id)` with an optional subtest suffix and an optional ` ... status`.
static HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\w+) \(([\w.]+)\)( \(.*\))?(?: \.\.\. ?(.*))?$").unwrap());
static STATUS_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" \.\.\. (.+)$").unwrap());

fn known_status(text: &str) -> Option<&str> {
    let known = matches!(
        text,
        "ok" | "FAIL" | "ERROR" | "expected failure" | "unexpected success"
    ) || text.starts_with("skipped");
    known.then_some(text)
}
static SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(FAIL|ERROR): (\w+) \(([\w.]+)\)( \(.*\))?$").unwrap());
static IMPORT_FAILURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Failed to import test module: ([\w.]+)").unwrap());

/// The dotted id from a header's name and parenthesized part. Python 3.11+ prints the full
/// id; older versions print only `module.Class`.
fn full_id(name: &str, paren: &str) -> String {
    if paren.rsplit('.').next() == Some(name) {
        paren.to_string()
    } else {
        format!("{paren}.{name}")
    }
}

fn parse(stderr: &str, start_dir: &str) -> Parsed {
    let lines: Vec<&str> = stderr.lines().collect();
    // Results in report order, keyed by full id. Subtests fold into their test.
    let mut order: Vec<String> = Vec::new();
    let mut statuses: std::collections::HashMap<String, String> = Default::default();
    let mut pending: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("=====") || line.starts_with("Ran ") {
            break;
        }
        if let Some(caps) = HEADER.captures(line) {
            let id = full_id(&caps[1], &caps[2]);
            if !statuses.contains_key(&id) && !order.contains(&id) {
                order.push(id.clone());
            }
            match caps
                .get(4)
                .map(|m| m.as_str().trim())
                .and_then(known_status)
            {
                Some(status) => {
                    record(&mut statuses, &id, status, caps.get(3).is_some());
                    pending = None;
                }
                // A subtest parent, a docstring on the next line, or log output in between.
                None => pending = Some(id),
            }
        } else if let Some(id) = pending.as_ref() {
            let status = known_status(line.trim()).or_else(|| {
                STATUS_TAIL
                    .captures(line)
                    .and_then(|c| known_status(c.get(1)?.as_str()))
            });
            if let Some(status) = status {
                record(&mut statuses, id, status, false);
                pending = None;
            }
        }
        i += 1;
    }
    let failures = sections(&lines);
    let mut parsed = Parsed::default();
    for id in order {
        let status = statuses.get(&id).map(String::as_str).unwrap_or("ok");
        if id.starts_with("unittest.loader._FailedTest.") {
            if let Some((_, failure, body)) = failures.iter().find(|(fid, _, _)| fid == &id) {
                if let Some(caps) = IMPORT_FAILURE.captures(body) {
                    parsed.file_errors.push(FileError {
                        file: module_path(&caps[1], start_dir),
                        failure: failure.clone(),
                    });
                }
            }
            // A selected name that does not exist executes nothing.
            continue;
        }
        let status = match status {
            "ok" => Status::Passed,
            s if s.starts_with("skipped") || s == "expected failure" => Status::Skipped,
            "unexpected success" => Status::Failed(Failure {
                kind: FailureKind::Assertion,
                exception_type: None,
                message: "unexpected success".to_string(),
            }),
            _ => Status::Failed(
                failures
                    .iter()
                    .find(|(fid, _, _)| fid == &id)
                    .map(|(_, f, _)| f.clone())
                    .unwrap_or_else(|| Failure {
                        kind: if status == "FAIL" {
                            FailureKind::Assertion
                        } else {
                            FailureKind::Exception
                        },
                        exception_type: None,
                        message: status.to_string(),
                    }),
            ),
        };
        let mut parts: Vec<String> = id.split('.').map(str::to_string).collect();
        let name = parts.pop().unwrap_or_default();
        parsed.outcomes.push(TestOutcome {
            id: Some(id),
            file: String::new(),
            suite: parts,
            name,
            line: None,
            status,
        });
    }
    parsed
}

/// Folds a status into a test's result. A failure always wins, so one failing subtest fails
/// the test; a subtest's pass never overrides the test's own status.
fn record(
    statuses: &mut std::collections::HashMap<String, String>,
    id: &str,
    status: &str,
    subtest: bool,
) {
    let failing = |s: &str| matches!(s, "FAIL" | "ERROR");
    let entry = statuses
        .entry(id.to_string())
        .or_insert_with(|| status.to_string());
    if failing(status) || (!subtest && !failing(entry)) {
        *entry = status.to_string();
    }
}

/// The FAIL and ERROR sections after the progress lines, as (full id, failure, body). The first
/// section for a test wins.
fn sections(lines: &[&str]) -> Vec<(String, Failure, String)> {
    let mut out: Vec<(String, Failure, String)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(caps) = SECTION.captures(lines[i]) else {
            i += 1;
            continue;
        };
        let id = full_id(&caps[2], &caps[3]);
        let fail = &caps[1] == "FAIL";
        i += 1;
        if lines.get(i).is_some_and(|l| l.starts_with("-----")) {
            i += 1;
        }
        let body_start = i;
        while i < lines.len()
            && !lines[i].starts_with("=====")
            && !(lines[i].starts_with("-----")
                && lines.get(i + 1).is_some_and(|l| l.starts_with("Ran ")))
        {
            i += 1;
        }
        let body = &lines[body_start..i];
        if out.iter().any(|(existing, _, _)| existing == &id) {
            continue;
        }
        out.push((id, failure_from(body, fail), body.join("\n")));
    }
    out
}

static EXCEPTION_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z_][\w.]*)(?::\s?(.*))?$").unwrap());

fn failure_from(body: &[&str], fail: bool) -> Failure {
    // The exception line is the first unindented line after the last traceback frame.
    let last_frame = body.iter().rposition(|l| l.starts_with("  File \""));
    let from = last_frame.map(|i| i + 1).unwrap_or(0);
    let exception_at = body[from..]
        .iter()
        .position(|l| !l.starts_with(' ') && !l.trim().is_empty())
        .map(|p| p + from);
    let (exception_type, message) = match exception_at.and_then(|i| {
        EXCEPTION_LINE.captures(body[i]).map(|c| {
            (
                i,
                c[1].to_string(),
                c.get(2).map(|m| m.as_str().to_string()),
            )
        })
    }) {
        Some((i, ty, first)) => {
            let mut message = first.unwrap_or_default();
            for extra in body[i + 1..].iter().take(5) {
                if extra.trim().is_empty() {
                    break;
                }
                message.push('\n');
                message.push_str(extra);
            }
            (Some(ty), message)
        }
        None => (None, body.join("\n").trim().to_string()),
    };
    let kind = if fail {
        FailureKind::Assertion
    } else {
        FailureKind::Exception
    };
    // A file that fails to import reports "ImportError: Failed to import test module" first
    // and the real error after the traceback.
    Failure {
        kind,
        exception_type,
        message: if message.is_empty() {
            body.join("\n")
        } else {
            message
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "\
test_ok (test_login.LoginTests.test_ok)
Docstring first line. ... ok
test_log (test_login.LoginTests.test_log) ... WARNING:root:noise
ok
test_assert (test_login.LoginTests.test_assert) ... FAIL
test_exc (test_login.LoginTests.test_exc) ... ERROR
test_skip (test_login.LoginTests.test_skip) ... skipped 'nope'
test_sub (test_login.LoginTests.test_sub) ...
  test_sub (test_login.LoginTests.test_sub) (i=1) ... FAIL
test_old (test_login.OldStyle) ... ok
test_broken (unittest.loader._FailedTest.test_broken) ... ERROR
test_nope (unittest.loader._FailedTest.test_nope) ... ERROR

======================================================================
ERROR: test_exc (test_login.LoginTests.test_exc)
----------------------------------------------------------------------
Traceback (most recent call last):
  File \"/w/tests/test_login.py\", line 14, in test_exc
    get({})
    ~~~^^^^
  File \"/w/app/core.py\", line 2, in get
    return d[\"exclude\"]
           ~^^^^^^^^^^^
KeyError: 'exclude'

======================================================================
ERROR: test_broken (unittest.loader._FailedTest.test_broken)
----------------------------------------------------------------------
ImportError: Failed to import test module: test_broken
Traceback (most recent call last):
  File \"/usr/lib/python3.14/unittest/loader.py\", line 137, in loadTestsFromName
    module = __import__(module_name)
  File \"/w/tests/test_broken.py\", line 2, in <module>
    from app.core import nothere
ImportError: cannot import name 'nothere' from 'app.core' (/w/app/core.py)


======================================================================
ERROR: test_nope (unittest.loader._FailedTest.test_nope)
----------------------------------------------------------------------
AttributeError: type object 'LoginTests' has no attribute 'test_nope'

======================================================================
FAIL: test_assert (test_login.LoginTests.test_assert)
----------------------------------------------------------------------
Traceback (most recent call last):
  File \"/w/tests/test_login.py\", line 12, in test_assert
    self.assertEqual(get(make_user()), 2)
AssertionError: 1 != 2

======================================================================
FAIL: test_sub (test_login.LoginTests.test_sub) (i=1)
----------------------------------------------------------------------
Traceback (most recent call last):
  File \"/w/tests/test_login.py\", line 21, in test_sub
    self.assertEqual(i, 0)
AssertionError: 1 != 0

----------------------------------------------------------------------
Ran 9 tests in 0.003s

FAILED (failures=2, errors=3, skipped=1)
";

    #[test]
    fn parses_statuses_failures_and_import_errors() {
        let parsed = parse(OUTPUT, "tests");
        let summary: Vec<(String, String)> = parsed
            .outcomes
            .iter()
            .map(|o| {
                let status = match &o.status {
                    Status::Passed => "pass".to_string(),
                    Status::Skipped => "skip".to_string(),
                    Status::Failed(f) => format!(
                        "{:?} {} {}",
                        f.kind,
                        f.exception_type.clone().unwrap_or_default(),
                        f.message
                    ),
                };
                (o.id.clone().unwrap(), status)
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("test_login.LoginTests.test_ok".into(), "pass".into()),
                ("test_login.LoginTests.test_log".into(), "pass".into()),
                (
                    "test_login.LoginTests.test_assert".into(),
                    "Assertion AssertionError 1 != 2".into()
                ),
                (
                    "test_login.LoginTests.test_exc".into(),
                    "Exception KeyError 'exclude'".into()
                ),
                ("test_login.LoginTests.test_skip".into(), "skip".into()),
                (
                    "test_login.LoginTests.test_sub".into(),
                    "Assertion AssertionError 1 != 0".into()
                ),
                ("test_login.OldStyle.test_old".into(), "pass".into()),
            ]
        );
        assert_eq!(parsed.file_errors.len(), 1);
        assert_eq!(parsed.file_errors[0].file, "tests/test_broken.py");
        assert_eq!(
            parsed.file_errors[0].failure.exception_type.as_deref(),
            Some("ImportError")
        );
        assert!(parsed.file_errors[0].failure.message.contains("nothere"));
    }

    #[test]
    fn ids_and_modules_follow_the_start_dir() {
        assert_eq!(
            test_id("tests/pkg/test_a.py", "tests", &["A".into()], "test_x"),
            "pkg.test_a.A.test_x"
        );
        assert_eq!(module_name("test_a.py", "."), "test_a");
        assert_eq!(module_path("pkg.test_a", "tests"), "tests/pkg/test_a.py");
    }
}
