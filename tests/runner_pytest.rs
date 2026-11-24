mod common;

use common::{Fixture, fixture_python};
use titer::config::Config;
use titer::deadline::Deadline;
use titer::runner::{CaseResult, FailureKind, RunRequest, Runner, Selection, TestCase};

const APP: &str = "def get(d):\n    return d[\"exclude\"]\n";

const TESTS: &str = r#"import pytest
from pkg.app import get


class TestX:
    class TestNested:
        def test_n(self):
            assert 1 == 1

    def test_ok(self):
        assert get({"exclude": 1}) == 1

    def test_assert(self):
        assert get({"exclude": 1}) == 2

    def test_exc(self):
        get({})


def test_skip():
    pytest.skip("not today")


@pytest.mark.parametrize("x", [1, 2])
def test_param(x):
    assert x == 1
"#;

const BROKEN: &str = "from pkg.app import missing_name\n\n\ndef test_b():\n    pass\n";

fn runner(cwd: &str) -> Runner {
    let toml = format!(
        "[[runner]]\nkind = \"pytest\"\ncwd = \"{cwd}\"\ncommand = [\"{}\", \"-m\", \"pytest\"]\n",
        fixture_python()
    );
    Runner::new(Config::parse(&toml).unwrap().runners.remove(0))
}

fn case(runner: &Runner, file: &str, suite: &[&str], name: &str) -> TestCase {
    let suite: Vec<String> = suite.iter().map(|s| s.to_string()).collect();
    TestCase {
        runner: runner.name().to_string(),
        file: file.to_string(),
        line: 1,
        end_line: 1,
        id: runner.test_id(file, &suite, name, 1),
        suite,
        name: name.to_string(),
        skip: false,
    }
}

fn repo(prefix: &str) -> Fixture {
    let repo = Fixture::new();
    repo.write(&format!("{prefix}pkg/__init__.py"), "")
        .write(&format!("{prefix}pkg/app.py"), APP)
        .write(&format!("{prefix}tests/test_a.py"), TESTS)
        .write(&format!("{prefix}tests/test_b.py"), BROKEN);
    repo
}

#[test]
fn reports_green_red_skipped_and_not_run_per_test() {
    let repo = repo("");
    let runner = runner(".");
    let cases = vec![
        case(
            &runner,
            "tests/test_a.py",
            &["TestX", "TestNested"],
            "test_n",
        ),
        case(&runner, "tests/test_a.py", &["TestX"], "test_ok"),
        case(&runner, "tests/test_a.py", &["TestX"], "test_assert"),
        case(&runner, "tests/test_a.py", &["TestX"], "test_exc"),
        case(&runner, "tests/test_a.py", &[], "test_skip"),
        case(&runner, "tests/test_a.py", &[], "test_param"),
        case(&runner, "tests/test_a.py", &[], "test_missing"),
        case(&runner, "tests/test_b.py", &[], "test_b"),
        case(&runner, "tests/test_gone.py", &[], "test_gone"),
    ];
    let scratch = repo.scratch().join("run");

    let run = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(&cases),
            fail_fast: false,
            timeout: None,
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();

    let results: Vec<CaseResult> = cases.iter().map(|c| run.result_for(c, ".")).collect();
    assert_eq!(results[0], CaseResult::Passed);
    assert_eq!(results[1], CaseResult::Passed);
    let CaseResult::Failed(assertion) = &results[2] else {
        panic!("{:?}", results[2])
    };
    assert_eq!(assertion.kind, FailureKind::Assertion);
    assert_eq!(assertion.exception_type.as_deref(), Some("AssertionError"));
    assert!(
        assertion.message.contains("assert 1 == 2"),
        "{}",
        assertion.message
    );
    let CaseResult::Failed(exception) = &results[3] else {
        panic!("{:?}", results[3])
    };
    assert_eq!(exception.kind, FailureKind::Exception);
    assert_eq!(exception.exception_type.as_deref(), Some("KeyError"));
    assert!(
        exception.message.contains("'exclude'"),
        "{}",
        exception.message
    );
    assert_eq!(results[4], CaseResult::Skipped);
    assert!(matches!(&results[5], CaseResult::Failed(f) if f.kind == FailureKind::Assertion));
    assert_eq!(results[6], CaseResult::NotRun);
    let CaseResult::Failed(import) = &results[7] else {
        panic!("{:?}", results[7])
    };
    assert_eq!(import.exception_type.as_deref(), Some("ImportError"));
    assert!(
        import.message.contains("missing_name"),
        "{}",
        import.message
    );
    assert_eq!(run.executed(), 6);
    assert!(run.runner_error.is_none());
}

#[test]
fn a_selection_that_matches_nothing_executes_zero_tests() {
    let repo = repo("");
    let runner = runner(".");
    let cases = vec![case(&runner, "tests/test_a.py", &[], "test_nope")];
    let scratch = repo.scratch().join("run");

    let run = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(&cases),
            fail_fast: false,
            timeout: None,
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();

    assert_eq!(run.executed(), 0);
    assert_eq!(run.result_for(&cases[0], "."), CaseResult::NotRun);
}

#[test]
fn only_skips_count_as_zero_executed() {
    let repo = repo("");
    let runner = runner(".");
    let cases = vec![case(&runner, "tests/test_a.py", &[], "test_skip")];
    let scratch = repo.scratch().join("run");

    let run = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(&cases),
            fail_fast: false,
            timeout: None,
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();

    assert_eq!(run.executed(), 0);
    assert_eq!(run.result_for(&cases[0], "."), CaseResult::Skipped);
}

#[test]
fn runs_in_a_subdirectory_cwd_and_stops_at_the_first_failure() {
    let repo = repo("api/");
    let runner = runner("api");
    let cases = vec![
        case(&runner, "api/tests/test_a.py", &["TestX"], "test_assert"),
        case(&runner, "api/tests/test_a.py", &["TestX"], "test_exc"),
    ];
    let scratch = repo.scratch().join("run");

    let run = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(&cases),
            fail_fast: true,
            timeout: None,
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();

    assert!(matches!(
        run.result_for(&cases[0], "api"),
        CaseResult::Failed(_)
    ));
    assert_eq!(run.result_for(&cases[1], "api"), CaseResult::NotRun);
}

#[test]
fn whole_files_and_the_suite_select_every_test() {
    let repo = repo("");
    let runner = runner(".");
    let scratch = repo.scratch().join("run");
    let files = vec!["tests/test_a.py".to_string()];

    let by_file = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Files(&files),
            fail_fast: false,
            timeout: None,
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();
    let suite = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Suite,
            fail_fast: false,
            timeout: None,
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();

    assert_eq!(by_file.executed(), 6);
    assert_eq!(suite.executed(), 6);
    assert_eq!(suite.file_errors.len(), 1);
}

#[test]
fn a_conftest_that_fails_to_load_fails_the_selected_tests() {
    let repo = repo("");
    repo.write(
        "tests/conftest.py",
        "raise RuntimeError(\"titer canary\")\n",
    );
    let runner = runner(".");
    let cases = vec![case(&runner, "tests/test_a.py", &["TestX"], "test_ok")];
    let scratch = repo.scratch().join("run");

    let run = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(&cases),
            fail_fast: true,
            timeout: None,
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();

    let CaseResult::Failed(failure) = run.result_for(&cases[0], ".") else {
        panic!("{run:?}")
    };
    assert_eq!(failure.exception_type.as_deref(), Some("RuntimeError"));
}

#[test]
fn retries_share_one_timeout() {
    let repo = repo("");
    // Every attempt loads the conftest, so each takes over 1 s. The first fits the timeout
    // and drops the missing test; a retry with a fresh timeout would fit too.
    repo.write("tests/conftest.py", "import time\n\ntime.sleep(1)\n");
    let runner = runner(".");
    let cases = vec![
        case(&runner, "tests/test_a.py", &["TestX"], "test_ok"),
        case(&runner, "tests/test_a.py", &[], "test_nope"),
    ];
    let scratch = repo.scratch().join("run");
    let started = std::time::Instant::now();

    let run = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(&cases),
            fail_fast: false,
            timeout: Some(std::time::Duration::from_millis(2000)),
            deadline: Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap();

    let elapsed = started.elapsed();
    assert!(run.timed_out, "{run:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(2800),
        "the retry restarted the timeout: {elapsed:?}"
    );
}
