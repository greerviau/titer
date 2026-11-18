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

const NAMES: &str = r#"pub mod math;

#[cfg(test)]
mod tests {
    #[test]
    fn checks() {}

    mod checks {
        #[test]
        fn unrelated() {
            panic!("not the selected test");
        }
    }

    // An inactive test, and a module of the same name whose tests are not its cases.
    #[cfg(any())]
    #[test]
    fn inactive() {}

    mod inactive {
        #[test]
        fn unrelated() {
            panic!("not the selected test");
        }
    }

    // `fn param` as the source spells it, and the module of cases rstest makes of it. The
    // cfg keeps the unresolvable attribute out of the build.
    #[cfg(any())]
    #[rstest]
    fn param() {}

    mod param {
        #[test]
        fn case_1() {}

        #[test]
        fn case_2() {
            assert_eq!(1, 2);
        }
    }
}
"#;

#[test]
fn a_test_and_a_module_of_one_name_stay_apart_and_macro_cases_aggregate() {
    let repo = fixture();
    repo.write("rs/src/lib.rs", NAMES);
    let runner = runner();
    let checks = case(&runner, "rs/src/lib.rs", &["tests"], "checks");
    let param = case(&runner, "rs/src/lib.rs", &["tests"], "param");
    let inactive = case(&runner, "rs/src/lib.rs", &["tests"], "inactive");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(&[checks.clone(), param.clone(), inactive.clone()]),
        None,
    );

    assert_eq!(run.result_for(&checks, "rs"), CaseResult::Passed, "{run:?}");
    assert!(
        !run.outcomes.iter().any(|o| o.name.contains("unrelated")),
        "{run:?}"
    );
    let CaseResult::Failed(f) = run.result_for(&param, "rs") else {
        panic!("{run:?}")
    };
    assert_eq!(f.kind, FailureKind::Assertion);
    assert_eq!(run.result_for(&inactive, "rs"), CaseResult::NotRun);
    assert_eq!(run.executed(), 3, "{run:?}");
}

#[test]
fn file_selection_runs_only_the_tests_in_that_file() {
    let repo = fixture();
    repo.write("rs/src/lib.rs", NAMES);
    let runner = runner();

    // The target's root file: its own tests, not the tests of the modules it declares.
    let files = vec!["rs/src/lib.rs".to_string()];
    let run = run(&repo, &runner, Selection::Files(&files), None);

    let mut names: Vec<String> = run.outcomes.iter().map(|o| o.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "tests::checks",
            "tests::checks::unrelated",
            "tests::inactive::unrelated",
            "tests::param::case_1",
            "tests::param::case_2"
        ]
    );
}

#[test]
fn the_suite_of_a_workspace_runs_every_member() {
    let repo = Fixture::new();
    repo.write(
        "rs/Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n\n[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .write("rs/src/lib.rs", "#[test]\nfn in_root() {}\n");
    for member in ["a", "b"] {
        repo.write(
            &format!("rs/{member}/Cargo.toml"),
            &format!("[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .write(
            &format!("rs/{member}/src/lib.rs"),
            &format!("#[test]\nfn in_{member}() {{}}\n"),
        );
    }
    let runner = runner();

    let run = run(&repo, &runner, Selection::Suite, None);

    let mut names: Vec<String> = run.outcomes.iter().map(|o| o.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["in_a", "in_b", "in_root"], "{run:?}");
}

#[test]
fn a_target_that_fails_to_link_makes_the_run_incomplete() {
    let repo = fixture();
    repo.write(
        "rs/src/main.rs",
        "#[link(name = \"titer_no_such_library\")]\nunsafe extern \"C\" {\n    fn titer_missing();\n}\n\nfn main() {\n    unsafe { titer_missing() }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn starts() {}\n}\n",
    );
    let runner = runner();
    let adds = case(&runner, "rs/src/lib.rs", &["tests"], "adds");
    let starts = case(&runner, "rs/src/main.rs", &["tests"], "starts");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(&[adds.clone(), starts.clone()]),
        None,
    );

    assert_eq!(run.result_for(&adds, "rs"), CaseResult::Passed, "{run:?}");
    assert_eq!(run.result_for(&starts, "rs"), CaseResult::NotRun);
    assert!(run.incomplete, "{run:?}");
}

#[test]
fn a_symlinked_build_dir_is_refused() {
    let repo = fixture();
    let outside = repo.scratch().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, repo.path().join(".titer-cache")).unwrap();
    let runner = runner();
    let adds = case(&runner, "rs/src/lib.rs", &["tests"], "adds");
    let scratch = repo.scratch().join("run");

    let err = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(std::slice::from_ref(&adds)),
            fail_fast: false,
            timeout: None,
            scratch: &scratch,
        })
        .unwrap_err();

    assert!(err.to_string().contains("symlink"), "{err:#}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn a_test_that_prints_a_result_line_yields_no_result() {
    let repo = fixture();
    repo.write(
        "rs/src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn forged() {\n        use std::io::Write;\n        let mut out = std::io::stdout();\n        out.write_all(b\"test tests::forged ... FAILED\\n\").unwrap();\n        out.flush().unwrap();\n    }\n}\n",
    );
    let runner = runner();
    let forged = case(&runner, "rs/src/lib.rs", &["tests"], "forged");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(std::slice::from_ref(&forged)),
        None,
    );

    assert!(
        !matches!(run.result_for(&forged, "rs"), CaseResult::Failed(_)),
        "{run:?}"
    );
    assert!(run.incomplete, "{run:?}");
}

#[test]
fn a_symlinked_lockfile_is_refused() {
    let repo = fixture();
    let outside = repo.scratch().join("workspace.lock");
    std::fs::write(&outside, "").unwrap();
    std::os::unix::fs::symlink(&outside, repo.path().join("rs/Cargo.lock")).unwrap();
    let runner = runner();
    let adds = case(&runner, "rs/src/lib.rs", &["tests"], "adds");
    let scratch = repo.scratch().join("run");

    let err = runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection: Selection::Tests(std::slice::from_ref(&adds)),
            fail_fast: false,
            timeout: None,
            scratch: &scratch,
        })
        .unwrap_err();

    assert!(err.to_string().contains("Cargo.lock"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "");
}

#[test]
fn a_target_the_features_leave_out_is_not_run() {
    let repo = fixture();
    repo.write(
        "rs/Cargo.toml",
        &format!(
            "{CARGO_TOML}\n[features]\nnightly = []\n\n[[test]]\nname = \"gated\"\nrequired-features = [\"nightly\"]\n"
        ),
    )
    .write("rs/tests/gated.rs", "#[test]\nfn gated() {}\n");
    let runner = runner();
    let gated = case(&runner, "rs/tests/gated.rs", &[], "gated");
    let api = case(&runner, "rs/tests/api.rs", &[], "adds");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(&[gated.clone(), api.clone()]),
        None,
    );

    assert_eq!(run.result_for(&gated, "rs"), CaseResult::NotRun, "{run:?}");
    assert_eq!(run.result_for(&api, "rs"), CaseResult::Passed, "{run:?}");
    assert!(run.runner_error.is_none(), "{run:?}");
}
