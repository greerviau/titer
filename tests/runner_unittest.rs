mod common;

use common::{Fixture, fixture_python};
use titer::config::Config;
use titer::runner::{CaseResult, FailureKind, RunRequest, Runner, Selection, TestCase};

const LOGIN: &str = r#"import unittest

from app.core import get


def make_user():
    return {"exclude": 1}


class LoginTests(unittest.TestCase):
    def test_ok(self):
        """A docstring line."""
        self.assertEqual(get(make_user()), 1)

    def test_assert(self):
        self.assertEqual(get(make_user()), 2)

    def test_exc(self):
        get({})

    @unittest.skip("not today")
    def test_skip(self):
        pass
"#;

// Imports a sibling test module by its bare name, which works only with the start dir on
// sys.path, as `unittest discover -s tests` arranges.
const SIGNUP: &str = r#"import unittest

from test_login import make_user
from app.core import get


class SignupTests(unittest.TestCase):
    def test_signup(self):
        self.assertEqual(get(make_user()), 1)
"#;

const BROKEN: &str = "import unittest\n\nfrom app.core import nothere\n\n\nclass B(unittest.TestCase):\n    def test_b(self):\n        pass\n";

fn fixture() -> Fixture {
    let repo = Fixture::new();
    repo.write("api/app/__init__.py", "")
        .write(
            "api/app/core.py",
            "def get(d):\n    return d[\"exclude\"]\n",
        )
        .write("api/tests/test_login.py", LOGIN)
        .write("api/tests/test_signup.py", SIGNUP)
        .write("api/tests/test_broken.py", BROKEN);
    repo
}

fn runner() -> Runner {
    let toml = format!(
        "[[runner]]\nkind = \"unittest\"\ncwd = \"api\"\nstart_dir = \"tests\"\ncommand = [\"{}\", \"-m\", \"unittest\"]\n",
        fixture_python()
    );
    Runner::new(Config::parse(&toml).unwrap().runners.remove(0))
}

fn case(runner: &Runner, file: &str, suite: &str, name: &str) -> TestCase {
    let suite = vec![suite.to_string()];
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

fn run(
    repo: &Fixture,
    runner: &Runner,
    selection: Selection,
    fail_fast: bool,
) -> titer::runner::TestRun {
    let scratch = repo.scratch().join("run");
    runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection,
            fail_fast,
            timeout: None,
            scratch: &scratch,
        })
        .unwrap()
}

#[test]
fn sibling_module_imports_load_and_results_are_per_test() {
    let repo = fixture();
    let runner = runner();
    let cases = vec![
        case(&runner, "api/tests/test_login.py", "LoginTests", "test_ok"),
        case(
            &runner,
            "api/tests/test_login.py",
            "LoginTests",
            "test_assert",
        ),
        case(&runner, "api/tests/test_login.py", "LoginTests", "test_exc"),
        case(
            &runner,
            "api/tests/test_login.py",
            "LoginTests",
            "test_skip",
        ),
        case(
            &runner,
            "api/tests/test_login.py",
            "LoginTests",
            "test_missing",
        ),
        case(
            &runner,
            "api/tests/test_signup.py",
            "SignupTests",
            "test_signup",
        ),
        case(&runner, "api/tests/test_broken.py", "B", "test_b"),
    ];
    assert_eq!(cases[5].id, "test_signup.SignupTests.test_signup");

    let run = run(&repo, &runner, Selection::Tests(&cases), false);

    let results: Vec<CaseResult> = cases.iter().map(|c| run.result_for(c, "api")).collect();
    assert_eq!(results[0], CaseResult::Passed);
    let CaseResult::Failed(assertion) = &results[1] else {
        panic!("{:?}", results[1])
    };
    assert_eq!(assertion.kind, FailureKind::Assertion);
    assert_eq!(assertion.exception_type.as_deref(), Some("AssertionError"));
    assert_eq!(assertion.message, "1 != 2");
    let CaseResult::Failed(exception) = &results[2] else {
        panic!("{:?}", results[2])
    };
    assert_eq!(exception.kind, FailureKind::Exception);
    assert_eq!(exception.exception_type.as_deref(), Some("KeyError"));
    assert_eq!(exception.message, "'exclude'");
    assert_eq!(results[3], CaseResult::Skipped);
    assert_eq!(results[4], CaseResult::NotRun);
    assert_eq!(
        results[5],
        CaseResult::Passed,
        "sibling import failed: {run:?}"
    );
    let CaseResult::Failed(import) = &results[6] else {
        panic!("{:?}", results[6])
    };
    assert_eq!(import.exception_type.as_deref(), Some("ImportError"));
    assert!(import.message.contains("nothere"), "{}", import.message);
    assert_eq!(run.executed(), 4);
}

#[test]
fn a_selection_that_matches_nothing_executes_zero_tests() {
    let repo = fixture();
    let runner = runner();
    let cases = vec![case(
        &runner,
        "api/tests/test_login.py",
        "LoginTests",
        "test_nope",
    )];

    let run = run(&repo, &runner, Selection::Tests(&cases), false);

    assert_eq!(run.executed(), 0);
    assert_eq!(run.result_for(&cases[0], "api"), CaseResult::NotRun);
    assert!(run.runner_error.is_none());
}

#[test]
fn fail_fast_stops_after_the_first_failure() {
    let repo = fixture();
    let runner = runner();
    let cases = vec![
        case(
            &runner,
            "api/tests/test_login.py",
            "LoginTests",
            "test_assert",
        ),
        case(&runner, "api/tests/test_login.py", "LoginTests", "test_exc"),
    ];

    let run = run(&repo, &runner, Selection::Tests(&cases), true);

    assert!(matches!(
        run.result_for(&cases[0], "api"),
        CaseResult::Failed(_)
    ));
    assert_eq!(run.result_for(&cases[1], "api"), CaseResult::NotRun);
}

#[test]
fn files_and_suite_select_whole_modules() {
    let repo = fixture();
    let runner = runner();
    let files = vec![
        "api/tests/test_signup.py".to_string(),
        "api/tests/test_login.py".to_string(),
    ];

    let by_file = run(&repo, &runner, Selection::Files(&files), false);
    let suite = run(&repo, &runner, Selection::Suite, false);

    assert_eq!(by_file.executed(), 4);
    assert_eq!(suite.executed(), 4);
    assert_eq!(suite.file_errors.len(), 1);
}

#[test]
fn a_non_import_error_while_loading_a_module_fails_its_tests() {
    let repo = fixture();
    // Raises TypeError at import time, which unittest's loader does not catch.
    repo.write(
        "api/app/core.py",
        "LIMIT = max(None, None)\n\ndef get(d):\n    return d[\"exclude\"]\n",
    );
    let runner = runner();
    let cases = vec![case(
        &runner,
        "api/tests/test_login.py",
        "LoginTests",
        "test_ok",
    )];

    let run = run(&repo, &runner, Selection::Tests(&cases), false);

    let CaseResult::Failed(failure) = run.result_for(&cases[0], "api") else {
        panic!("{run:?}")
    };
    assert_eq!(failure.exception_type.as_deref(), Some("TypeError"));
    assert_eq!(failure.kind, FailureKind::Exception);
}

#[test]
fn a_failing_set_up_class_fails_the_tests_of_its_module() {
    let repo = fixture();
    repo.write(
        "api/tests/test_cls.py",
        "import unittest\n\n\nclass A(unittest.TestCase):\n    @classmethod\n    def setUpClass(cls):\n        raise ValueError(\"boom\")\n\n    def test_a(self):\n        pass\n",
    );
    let runner = runner();
    let cases = vec![case(&runner, "api/tests/test_cls.py", "A", "test_a")];

    let run = run(&repo, &runner, Selection::Tests(&cases), false);

    let CaseResult::Failed(failure) = run.result_for(&cases[0], "api") else {
        panic!("{run:?}")
    };
    assert_eq!(failure.exception_type.as_deref(), Some("ValueError"));
}
