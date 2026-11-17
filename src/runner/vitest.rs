//! vitest: select by `file:line`, parse `--reporter=json` with NO_COLOR set and ANSI codes
//! stripped anyway.
//!
//! vitest reports every test in a selected file. Tests outside the selection come back as
//! "skipped", and tests that `--bail` cut off come back as "pending", so only selected lines
//! count. vitest also runs an `it.skip` that is selected by line, so test detection marks
//! static skips on the [`TestCase`](super::TestCase).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{
    Failure, FailureKind, FileError, RunRequest, Runner, Selection, Status, TestOutcome, TestRun,
    cwd_relative, output_tail, same_file,
};

/// `path:line`, with `path` relative to the runner's cwd.
pub fn test_id(rel: &str, line: u32) -> String {
    format!("{rel}:{line}")
}

pub fn run(runner: &Runner, req: &RunRequest) -> anyhow::Result<TestRun> {
    let mut selectors: Vec<String> = match req.selection {
        Selection::Tests(cases) => cases.iter().map(|c| c.id.clone()).collect(),
        Selection::Files(files) => files
            .iter()
            .map(|f| cwd_relative(f, &runner.config.cwd))
            .collect(),
        Selection::Suite => Vec::new(),
    };
    selectors.dedup();
    let selected_lines: Option<Vec<(String, u32)>> = match req.selection {
        Selection::Tests(_) => Some(selectors.iter().filter_map(|s| split_selector(s)).collect()),
        _ => None,
    };
    let mut attempt = 0;
    loop {
        attempt += 1;
        let report = req.scratch.join(format!("vitest-{}.json", unique()));
        let mut args = vec![
            "--reporter=json".to_string(),
            format!("--outputFile={}", report.display()),
            "--includeTaskLocation".to_string(),
            "--coverage.enabled=false".to_string(),
            "--passWithNoTests".to_string(),
        ];
        if req.fail_fast {
            args.push("--bail=1".to_string());
        }
        args.extend(selectors.iter().cloned());
        let cmd = runner.command(req.worktree, &args);
        let out = runner.exec(cmd, req)?;
        let json = std::fs::read_to_string(&report).unwrap_or_default();
        let _ = std::fs::remove_file(&report);
        let text = strip_ansi(&format!("{}\n{}", out.stdout, out.stderr));

        // A selected line with no test, or a missing file, fails the whole run or file.
        // Drop those selectors and run the rest again.
        let cwd = runner.cwd_in(req.worktree);
        let mut roots = vec![cwd.clone()];
        roots.extend(cwd.canonicalize().ok());
        let parsed = parse(&json, selected_lines.as_deref(), &roots);
        let mut bad: Vec<String> = parsed.bad_lines.clone();
        for caps in MISSING_FILE.captures_iter(&text) {
            let missing = &caps[1];
            bad.extend(
                selectors
                    .iter()
                    .filter(|s| {
                        s.as_str() == missing
                            || split_selector(s).is_some_and(|(file, _)| file == missing)
                    })
                    .cloned(),
            );
        }
        bad.retain(|b| selectors.contains(b));
        if !bad.is_empty() && attempt <= selectors.len() {
            selectors.retain(|s| !bad.contains(s));
            if !selectors.is_empty() {
                continue;
            }
            return Ok(empty_run(&out));
        }
        let runner_error =
            (json.trim().is_empty() && !out.timed_out && !out.success()).then(|| output_tail(&out));
        return Ok(TestRun {
            outcomes: parsed.outcomes,
            file_errors: parsed.file_errors,
            timed_out: out.timed_out,
            duration: out.duration,
            max_rss_bytes: out.max_rss_bytes,
            runner_error,
        });
    }
}

fn empty_run(out: &crate::process::RunOutput) -> TestRun {
    TestRun {
        outcomes: Vec::new(),
        file_errors: Vec::new(),
        timed_out: out.timed_out,
        duration: out.duration,
        max_rss_bytes: out.max_rss_bytes,
        runner_error: None,
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn split_selector(selector: &str) -> Option<(String, u32)> {
    let (file, line) = selector.rsplit_once(':')?;
    Some((file.to_string(), line.parse().ok()?))
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());
static MISSING_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Couldn't find file (\S+?)\.? Note").unwrap());
static NO_TEST_AT_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"No test found in (\S+) in lines? (\d+(?:, \d+)*)").unwrap());
static ERROR_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z_$][\w$]*(?:Error|Exception))(?::|$)").unwrap());

pub fn strip_ansi(text: &str) -> String {
    ANSI.replace_all(text, "").into_owned()
}

#[derive(Debug, Default)]
struct Parsed {
    outcomes: Vec<TestOutcome>,
    file_errors: Vec<FileError>,
    /// `file:line` selectors that vitest reported as having no test.
    bad_lines: Vec<String>,
}

/// `path` relative to the runner cwd and marked exact (`./`), when it lies under one of
/// `roots`; otherwise unchanged.
fn relative_to(path: &str, roots: &[std::path::PathBuf]) -> String {
    roots
        .iter()
        .find_map(|root| {
            std::path::Path::new(path)
                .strip_prefix(root)
                .ok()
                .map(|rel| format!("./{}", rel.display()))
        })
        .unwrap_or_else(|| path.to_string())
}

fn parse(json: &str, selected: Option<&[(String, u32)]>, roots: &[std::path::PathBuf]) -> Parsed {
    let mut parsed = Parsed::default();
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return parsed;
    };
    let empty = Vec::new();
    for file in root["testResults"].as_array().unwrap_or(&empty) {
        let path = relative_to(file["name"].as_str().unwrap_or_default(), roots);
        let message = strip_ansi(file["message"].as_str().unwrap_or_default());
        let assertions = file["assertionResults"].as_array().unwrap_or(&empty);
        let line_errors: Vec<_> = NO_TEST_AT_LINE.captures_iter(&message).collect();
        if !line_errors.is_empty() {
            for caps in line_errors {
                for line in caps[2].split(", ") {
                    parsed.bad_lines.push(format!("{}:{line}", &caps[1]));
                }
            }
            continue;
        }
        let ran_any = assertions
            .iter()
            .any(|a| matches!(a["status"].as_str(), Some("passed" | "failed")));
        if file["status"] == "failed" && !message.trim().is_empty() && !ran_any {
            parsed.file_errors.push(FileError {
                file: path.clone(),
                failure: failure_from(&message),
            });
            continue;
        }
        for a in assertions {
            let line = a["location"]["line"].as_u64().map(|l| l as u32);
            if let Some(selected) = selected {
                let picked = line.is_some_and(|l| {
                    selected
                        .iter()
                        .any(|(f, sl)| *sl == l && same_file(&path, f))
                });
                if !picked {
                    continue;
                }
            }
            let status = match a["status"].as_str() {
                Some("passed") => Status::Passed,
                Some("failed") => {
                    let first = a["failureMessages"]
                        .as_array()
                        .and_then(|m| m.first())
                        .and_then(Value::as_str)
                        .unwrap_or("failed");
                    Status::Failed(failure_from(&strip_ansi(first)))
                }
                Some("skipped" | "todo" | "disabled") => Status::Skipped,
                // "pending": cut off by --bail before it ran.
                _ => continue,
            };
            parsed.outcomes.push(TestOutcome {
                id: None,
                file: path.clone(),
                suite: a["ancestorTitles"]
                    .as_array()
                    .unwrap_or(&empty)
                    .iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect(),
                name: a["title"].as_str().unwrap_or_default().to_string(),
                line,
                status,
            });
        }
    }
    parsed
}

/// Assertion failures from vitest's `expect`, chai, and Testing Library queries. A query
/// that finds no element is the test's own check failing, not the code under test raising.
fn is_assertion(error_type: &str) -> bool {
    matches!(
        error_type,
        "AssertionError" | "JestAssertionError" | "TestingLibraryElementError"
    )
}

/// Matchers that throw a plain `Error`, such as jest-dom's `toBeDisabled`, and Testing
/// Library queries that find no element or too many.
static ASSERTION_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\w+: )?(?:expect\(|Unable to find (?:an? )?(?:element|label|role|accessible element)|Found multiple elements)")
        .unwrap()
});

fn failure_from(message: &str) -> Failure {
    let first = message.lines().next().unwrap_or_default().trim();
    let exception_type = ERROR_TYPE.captures(first).map(|c| c[1].to_string());
    let kind = match exception_type.as_deref() {
        Some(t) if is_assertion(t) => FailureKind::Assertion,
        Some("Error") | None if ASSERTION_TEXT.is_match(first) => FailureKind::Assertion,
        _ => FailureKind::Exception,
    };
    let detail: Vec<&str> = message
        .lines()
        .take_while(|l| !l.trim_start().starts_with("at "))
        .take(6)
        .collect();
    let mut text = detail.join("\n").trim().to_string();
    if let Some(t) = &exception_type {
        if let Some(rest) = text.strip_prefix(t.as_str()) {
            text = rest.trim_start_matches(':').trim_start().to_string();
        }
    }
    Failure {
        kind,
        exception_type,
        message: text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_failure_messages() {
        let f = failure_from(
            "AssertionError: expected 3 to be 4 // Object.is equality\n    at /w/a.test.ts:10:25",
        );
        assert_eq!(f.kind, FailureKind::Assertion);
        assert_eq!(f.exception_type.as_deref(), Some("AssertionError"));
        assert_eq!(f.message, "expected 3 to be 4 // Object.is equality");
        let f = failure_from(
            "Error: expect(element).toBeDisabled()\n\nReceived element is not disabled:",
        );
        assert_eq!(f.kind, FailureKind::Assertion);
        let f = failure_from("Error: Unable to find a label with the text of: /^R/");
        assert_eq!(f.kind, FailureKind::Assertion);
        let f = failure_from("TypeError: no x\n    at get (/w/math.ts:5:26)");
        assert_eq!(f.kind, FailureKind::Exception);
        assert_eq!(f.exception_type.as_deref(), Some("TypeError"));
        let f = failure_from("Cannot find module './nothere' imported from /w/b.test.ts");
        assert_eq!(f.kind, FailureKind::Exception);
        assert_eq!(f.exception_type, None);
        assert!(f.message.starts_with("Cannot find module"));
    }

    #[test]
    fn strips_ansi_codes() {
        assert_eq!(
            strip_ansi("\x1b[31mAssertionError\x1b[39m: x"),
            "AssertionError: x"
        );
    }
}
