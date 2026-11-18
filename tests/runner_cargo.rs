mod common;

use std::time::Duration;

use common::Fixture;
use titer::config::Config;
use titer::runner::{CaseResult, FailureKind, RunRequest, Runner, Selection, TestCase, TestRun};

const CARGO_TOML: &str =
    "[package]\nname = \"demo-calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

const LIB: &str = r#"pub mod math;

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    fn adds_wrong() {
        assert_eq!(add(1, 2), 4);
    }

    #[test]
    fn unwraps() {
        let none: Option<i32> = None;
        none.unwrap();
    }

    #[test]
    #[ignore]
    fn ignored() {}

    #[test]
    fn crashes() {
        std::process::abort();
    }

    #[test]
    fn spins() {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
"#;

const MATH: &str = r#"pub fn mul(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod tests {
    #[test]
    fn muls() {
        assert_eq!(super::mul(2, 3), 6);
    }

    #[test]
    fn muls_zero() {
        assert_eq!(super::mul(0, 3), 0);
    }
}
"#;

const API: &str = r#"#[test]
fn adds() {
    assert_eq!(demo_calc::add(2, 2), 4);
}

mod nested {
    #[test]
    fn subtracts() {
        assert_eq!(demo_calc::add(2, -2), 0);
    }
}
"#;

fn fixture() -> Fixture {
    let repo = Fixture::new();
    repo.write("rs/Cargo.toml", CARGO_TOML)
        .write("rs/src/lib.rs", LIB)
        .write("rs/src/math.rs", MATH)
        .write("rs/tests/api.rs", API);
    repo
}

fn runner() -> Runner {
    let toml = "[[runner]]\nkind = \"cargo\"\ncwd = \"rs\"\ncommand = [\"cargo\", \"test\"]\n";
    Runner::new(Config::parse(toml).unwrap().runners.remove(0))
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

fn run(
    repo: &Fixture,
    runner: &Runner,
    selection: Selection,
    timeout: Option<Duration>,
) -> TestRun {
    let scratch = repo.scratch().join("run");
    runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection,
            fail_fast: false,
            timeout,
            scratch: &scratch,
        })
        .unwrap()
}

#[test]
fn ids_name_the_file_and_the_path_inside_it() {
    let runner = runner();
    assert_eq!(
        case(&runner, "rs/src/math.rs", &["tests"], "muls").id,
        "src/math.rs::tests::muls"
    );
    assert_eq!(
        case(&runner, "rs/tests/api.rs", &[], "adds").id,
        "tests/api.rs::adds"
    );
}

#[test]
fn selects_unit_and_integration_tests_and_classifies_each_result() {
    let repo = fixture();
    let runner = runner();
    let lib = "rs/src/lib.rs";
    let adds = case(&runner, lib, &["tests"], "adds");
    let wrong = case(&runner, lib, &["tests"], "adds_wrong");
    let unwraps = case(&runner, lib, &["tests"], "unwraps");
    let ignored = case(&runner, lib, &["tests"], "ignored");
    let muls = case(&runner, "rs/src/math.rs", &["tests"], "muls");
    let api_adds = case(&runner, "rs/tests/api.rs", &[], "adds");
    let subtracts = case(&runner, "rs/tests/api.rs", &["nested"], "subtracts");
    let unselected = case(&runner, "rs/src/math.rs", &["tests"], "muls_zero");
    let cases = vec![
        adds.clone(),
        wrong.clone(),
        unwraps.clone(),
        ignored.clone(),
        muls.clone(),
        api_adds.clone(),
        subtracts.clone(),
    ];

    let run = run(&repo, &runner, Selection::Tests(&cases), None);

    assert!(run.runner_error.is_none(), "{run:?}");
    assert_eq!(run.result_for(&adds, "rs"), CaseResult::Passed);
    let CaseResult::Failed(f) = run.result_for(&wrong, "rs") else {
        panic!("{run:?}")
    };
    assert_eq!(f.kind, FailureKind::Assertion);
    assert!(f.message.contains("left == right"), "{}", f.message);
    assert!(f.message.contains("right: 4"), "{}", f.message);
    let CaseResult::Failed(f) = run.result_for(&unwraps, "rs") else {
        panic!("{run:?}")
    };
    assert_eq!(f.kind, FailureKind::Exception);
    assert!(f.message.contains("Option::unwrap()"), "{}", f.message);
    assert_eq!(run.result_for(&ignored, "rs"), CaseResult::Skipped);
    assert_eq!(run.result_for(&muls, "rs"), CaseResult::Passed);
    assert_eq!(run.result_for(&api_adds, "rs"), CaseResult::Passed);
    assert_eq!(run.result_for(&subtracts, "rs"), CaseResult::Passed);
    assert_eq!(run.result_for(&unselected, "rs"), CaseResult::NotRun);
}

#[test]
fn a_crash_fails_the_test_that_was_running() {
    let repo = fixture();
    let runner = runner();
    let adds = case(&runner, "rs/src/lib.rs", &["tests"], "adds");
    let crashes = case(&runner, "rs/src/lib.rs", &["tests"], "crashes");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(&[adds.clone(), crashes.clone()]),
        None,
    );

    assert_eq!(run.result_for(&adds, "rs"), CaseResult::Passed, "{run:?}");
    let CaseResult::Failed(f) = run.result_for(&crashes, "rs") else {
        panic!("{run:?}")
    };
    assert_eq!(f.kind, FailureKind::Exception);
    assert!(f.message.contains("SIGABRT"), "{}", f.message);
}

#[test]
fn a_compile_error_reports_its_location_and_runs_nothing() {
    let repo = fixture();
    repo.write(
        "rs/src/math.rs",
        &MATH.replace("super::mul(2, 3)", "super::div(2, 3)"),
    );
    let runner = runner();
    let muls = case(&runner, "rs/src/math.rs", &["tests"], "muls");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(std::slice::from_ref(&muls)),
        None,
    );

    assert_eq!(run.executed(), 0);
    assert_eq!(run.result_for(&muls, "rs"), CaseResult::NotRun);
    assert_eq!(run.compile_errors.len(), 1, "{run:?}");
    let error = &run.compile_errors[0];
    assert!(error.file.ends_with("src/math.rs"), "{error:?}");
    assert_eq!(error.line, 9);
    assert!(
        error.message.contains("cannot find function `div`"),
        "{error:?}"
    );
    assert!(
        run.runner_error.as_deref().unwrap().contains("E0425"),
        "{run:?}"
    );
}

#[test]
fn file_selection_runs_whole_targets_or_one_module() {
    let repo = fixture();
    let runner = runner();

    let files = vec!["rs/tests/api.rs".to_string(), "rs/src/math.rs".to_string()];
    let run = run(&repo, &runner, Selection::Files(&files), None);

    let mut names: Vec<String> = run
        .outcomes
        .iter()
        .map(|o| format!("{} {}", o.file, o.name))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "./src/lib.rs math::tests::muls",
            "./src/lib.rs math::tests::muls_zero",
            "./tests/api.rs adds",
            "./tests/api.rs nested::subtracts",
        ]
    );
}

#[test]
fn the_suite_runs_every_target() {
    let repo = Fixture::new();
    repo.write("rs/Cargo.toml", CARGO_TOML)
        .write(
            "rs/src/lib.rs",
            "pub mod math;\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .write("rs/src/math.rs", MATH)
        .write("rs/tests/api.rs", API);
    let runner = runner();

    let run = run(&repo, &runner, Selection::Suite, None);

    assert!(run.runner_error.is_none(), "{run:?}");
    let mut names: Vec<String> = run
        .outcomes
        .iter()
        .map(|o| format!("{} {}", o.file, o.name))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "src/lib.rs math::tests::muls",
            "src/lib.rs math::tests::muls_zero",
            "tests/api.rs adds",
            "tests/api.rs nested::subtracts",
        ]
    );
}

#[test]
fn the_timeout_limits_the_tests_and_not_the_build() {
    let repo = fixture();
    let runner = runner();
    let spins = case(&runner, "rs/src/lib.rs", &["tests"], "spins");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(std::slice::from_ref(&spins)),
        Some(Duration::from_secs(2)),
    );

    assert!(run.timed_out, "{run:?}");
    assert!(run.compile_errors.is_empty(), "{run:?}");
    assert!(run.duration < Duration::from_secs(10), "{run:?}");
}
