//! pytest: select by node id, parse `--junitxml`.

use std::sync::LazyLock;

use regex::Regex;

use crate::xml;

use super::{
    Failure, FailureKind, FileError, RunRequest, Runner, Selection, Status, TestOutcome, TestRun,
    cwd_relative, output_tail, same_file,
};

/// `path::Class::Nested::test_name`, with `path` relative to the runner's cwd.
pub fn node_id(rel: &str, suite: &[String], name: &str) -> String {
    let mut id = rel.to_string();
    for part in suite.iter().map(String::as_str).chain([name]) {
        id.push_str("::");
        id.push_str(part);
    }
    id
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
    let mut file_errors: Vec<FileError> = Vec::new();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let junit = req.scratch.join(format!("junit-{}.xml", unique()));
        let mut args = vec![
            "-p".to_string(),
            "no:cacheprovider".to_string(),
            "-o".to_string(),
            "junit_family=xunit1".to_string(),
            format!("--junitxml={}", junit.display()),
            "-q".to_string(),
            "--continue-on-collection-errors".to_string(),
        ];
        if req.fail_fast {
            args.push("-x".to_string());
        }
        args.extend(selectors.iter().cloned());
        let cmd = runner.command(req.worktree, &args);
        let out = runner.exec(cmd, req)?;
        let xml = std::fs::read_to_string(&junit).unwrap_or_default();
        let _ = std::fs::remove_file(&junit);
        let parsed = parse_junit(&xml);
        file_errors.extend(parsed.file_errors.iter().cloned());

        // pytest aborts the whole run when a selected node id is missing or its file fails to
        // import. Drop those selectors and run the rest again.
        let text = format!("{}\n{}", out.stdout, out.stderr);
        let bad = unresolved_selectors(&text, &selectors);
        if out.exit_code == Some(4) && !bad.is_empty() && attempt <= selectors.len() {
            selectors.retain(|s| !bad.contains(s));
            if !selectors.is_empty() {
                continue;
            }
        }
        // A conftest that fails to load stops pytest before it writes the junit file, and
        // every selected test fails with it.
        if parsed.outcomes.is_empty()
            && file_errors.is_empty()
            && !out.timed_out
            && text.contains("while loading conftest")
        {
            let failure = failure_from("", &text);
            let files: Vec<String> = match req.selection {
                Selection::Tests(cases) => cases
                    .iter()
                    .map(|c| cwd_relative(&c.file, &runner.config.cwd))
                    .collect(),
                Selection::Files(files) => files
                    .iter()
                    .map(|f| cwd_relative(f, &runner.config.cwd))
                    .collect(),
                Selection::Suite => vec!["conftest.py".to_string()],
            };
            for file in files {
                file_errors.push(FileError {
                    file: format!("./{file}"),
                    failure: failure.clone(),
                });
            }
        }
        let runner_error = (parsed.outcomes.is_empty()
            && file_errors.is_empty()
            && !out.timed_out
            && !matches!(out.exit_code, Some(0 | 1 | 4 | 5)))
        .then(|| output_tail(&out));
        return Ok(TestRun {
            outcomes: parsed.outcomes,
            file_errors,
            timed_out: out.timed_out,
            duration: out.duration,
            max_rss_bytes: out.max_rss_bytes,
            runner_error,
            compile_errors: Vec::new(),
        });
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

static UNRESOLVED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^ERROR: (not found:|found no collectors for|file or directory not found:) (\S+)$",
    )
    .unwrap()
});

/// Selectors that pytest reported as not found or uncollectable.
fn unresolved_selectors(text: &str, selectors: &[String]) -> Vec<String> {
    let mut bad = Vec::new();
    for caps in UNRESOLVED.captures_iter(text) {
        // A missing file names only its first selector, but every selector in it is bad.
        let whole_file = caps[1].starts_with("file or directory");
        let reported = &caps[2];
        for selector in selectors {
            let matches = if whole_file {
                let path = |s: &str| s.split_once("::").map_or(s, |(p, _)| p).to_string();
                same_file(&path(reported), &path(selector))
            } else {
                same_selector(reported, selector)
            };
            if matches && !bad.contains(selector) {
                bad.push(selector.clone());
            }
        }
    }
    bad
}

fn same_selector(reported: &str, selector: &str) -> bool {
    let (rep_path, rep_rest) = reported.split_once("::").unwrap_or((reported, ""));
    let (sel_path, sel_rest) = selector.split_once("::").unwrap_or((selector, ""));
    rep_rest == sel_rest && same_file(rep_path, sel_path)
}

#[derive(Debug, Default)]
struct Parsed {
    outcomes: Vec<TestOutcome>,
    file_errors: Vec<FileError>,
}

#[derive(Debug, Default)]
struct Case {
    classname: String,
    name: String,
    file: String,
    result: Option<(String, String, String)>,
}

fn parse_junit(xml: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut current: Option<Case> = None;
    // The result element whose text we are reading: (tag, message).
    let mut open: Option<(String, String)> = None;
    let mut text = String::new();
    for event in xml::events(xml) {
        match event {
            xml::Event::Start { name, attrs, empty } if name == "testcase" => {
                if let Some(case) = current.take() {
                    finish(case, &mut parsed);
                }
                let get = |k| xml::attr(&attrs, k).unwrap_or_default().to_string();
                let case = Case {
                    classname: get("classname"),
                    name: get("name"),
                    file: get("file"),
                    result: None,
                };
                if empty {
                    finish(case, &mut parsed);
                } else {
                    current = Some(case);
                }
            }
            xml::Event::Start { name, attrs, empty }
                if matches!(name.as_str(), "failure" | "error" | "skipped") =>
            {
                let message = xml::attr(&attrs, "message").unwrap_or_default().to_string();
                if empty {
                    if let Some(case) = current.as_mut() {
                        case.result.get_or_insert((name, message, String::new()));
                    }
                } else {
                    open = Some((name, message));
                    text.clear();
                }
            }
            xml::Event::Text(t) if open.is_some() => text.push_str(&t),
            xml::Event::End(name) if matches!(name.as_str(), "failure" | "error" | "skipped") => {
                if let (Some((tag, message)), Some(case)) = (open.take(), current.as_mut()) {
                    case.result
                        .get_or_insert((tag, message, std::mem::take(&mut text)));
                }
            }
            xml::Event::End(name) if name == "testcase" => {
                if let Some(case) = current.take() {
                    finish(case, &mut parsed);
                }
            }
            _ => {}
        }
    }
    if let Some(case) = current.take() {
        finish(case, &mut parsed);
    }
    parsed
}

fn finish(case: Case, parsed: &mut Parsed) {
    let file = if case.file.is_empty() {
        format!("{}.py", case.name.replace('.', "/"))
    } else {
        case.file.clone()
    };
    if case.classname.is_empty()
        && let Some((tag, message, text)) = &case.result
        && tag == "error"
        && message == "collection failure"
    {
        parsed.file_errors.push(FileError {
            file,
            failure: failure_from(message, text),
        });
        return;
    }
    let status = match &case.result {
        None => Status::Passed,
        Some((tag, _, _)) if tag == "skipped" => Status::Skipped,
        Some((_, message, text)) => Status::Failed(failure_from(message, text)),
    };
    let name = match case.name.find('[') {
        Some(i) => case.name[..i].to_string(),
        None => case.name.clone(),
    };
    parsed.outcomes.push(TestOutcome {
        id: None,
        suite: suite_from(&case.classname, &file),
        file,
        name,
        line: None,
        status,
    });
}

/// The classes in `classname` after the module, such as `["TestX", "TestNested"]` for
/// `tests.test_a.TestX.TestNested` in `tests/test_a.py`.
fn suite_from(classname: &str, file: &str) -> Vec<String> {
    let stem = file
        .rsplit('/')
        .next()
        .unwrap_or(file)
        .trim_end_matches(".py");
    let parts: Vec<&str> = classname.split('.').collect();
    match parts.iter().rposition(|p| *p == stem) {
        Some(i) => parts[i + 1..].iter().map(|p| p.to_string()).collect(),
        None => Vec::new(),
    }
}

static LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\S.*:\d+: ([A-Za-z_][\w.]*)$").unwrap());
static TYPED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z_][\w.]*(?:Error|Exception|Exit|Failed|Warning|Interrupt|Iteration)):")
        .unwrap()
});

fn failure_from(message: &str, text: &str) -> Failure {
    let lines: Vec<&str> = text.lines().collect();
    let e_lines: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("E ").map(str::trim))
        .collect();
    let exception_type = lines
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| LOCATION.captures(l))
        .map(|c| c[1].to_string())
        .or_else(|| TYPED.captures(message).map(|c| c[1].to_string()))
        .or_else(|| {
            e_lines
                .iter()
                .rev()
                .find_map(|l| TYPED.captures(l).map(|c| c[1].to_string()))
        });
    let detail = if e_lines.is_empty() {
        message.to_string()
    } else {
        e_lines
            .iter()
            .take(6)
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let kind = match exception_type.as_deref() {
        Some(t) if is_assertion(t) => FailureKind::Assertion,
        None if message.starts_with("assert ") => FailureKind::Assertion,
        _ => FailureKind::Exception,
    };
    Failure {
        kind,
        exception_type,
        message: detail,
    }
}

fn is_assertion(exception_type: &str) -> bool {
    let short = exception_type.rsplit('.').next().unwrap_or(exception_type);
    matches!(short, "AssertionError" | "Failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_join_the_suite() {
        assert_eq!(
            node_id("tests/t.py", &["A".into(), "B".into()], "test_x"),
            "tests/t.py::A::B::test_x"
        );
        assert_eq!(node_id("t.py", &[], "test_x"), "t.py::test_x");
    }

    #[test]
    fn suite_comes_from_the_classname_after_the_module() {
        assert_eq!(
            suite_from("tests.test_a.TestX.TestN", "tests/test_a.py"),
            vec!["TestX", "TestN"]
        );
        assert_eq!(suite_from("test_a", "test_a.py"), Vec::<String>::new());
    }

    #[test]
    fn unresolved_selectors_match_absolute_reports() {
        let text = "ERROR: not found: /w/api/tests/test_a.py::nope\n(no match)\nERROR: found no collectors for /w/api/tests/test_b.py::test_b\n";
        let selectors = vec![
            "tests/test_a.py::nope".to_string(),
            "tests/test_b.py::test_b".to_string(),
            "tests/test_c.py::test_c".to_string(),
        ];
        assert_eq!(
            unresolved_selectors(text, &selectors),
            selectors[..2].to_vec()
        );
    }
}
