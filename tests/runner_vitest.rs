mod common;

use common::{Fixture, fixture_node_modules};
use titer::config::Config;
use titer::runner::{CaseResult, FailureKind, RunRequest, Runner, Selection, TestCase, TestRun};

const MATH: &str = r#"export function add(a: number, b: number): number {
  return a + b;
}

export function get(d: Record<string, number>): number {
  if (!("x" in d)) throw new TypeError("no x");
  return d.x;
}
"#;

// Lines matter: tests select by file:line.
const TESTS: &str = r#"import { describe, it, test, expect } from "vitest";
import { add, get } from "./math";

describe("outer", () => {
  describe("inner", () => {
    it("works", () => {
      expect(add(1, 2)).toBe(3);
    });
    it("works", () => {
      expect(add(1, 2)).toBe(4);
    });
  });
  it("works", () => {
    expect(add(2, 2)).toBe(4);
  });
  it.skip("skipped", () => {});
  test("throws", () => {
    get({});
  });
});
"#;

const BROKEN: &str = "import { it, expect } from \"vitest\";\nimport { nothere } from \"./nothere\";\n\nit(\"b\", () => {\n  expect(nothere).toBe(1);\n});\n";

fn fixture() -> Fixture {
    let repo = Fixture::new();
    repo.write("web/package.json", "{\"type\": \"module\"}\n")
        .write("web/src/math.ts", MATH)
        .write("web/src/math.test.ts", TESTS)
        .write("web/src/broken.test.ts", BROKEN);
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("web/node_modules"))
        .unwrap();
    repo
}

fn runner() -> Runner {
    let vitest = fixture_node_modules().join(".bin/vitest");
    let toml = format!(
        "[[runner]]\nkind = \"vitest\"\ncwd = \"web\"\ncommand = [\"{}\", \"run\"]\n",
        vitest.display()
    );
    Runner::new(Config::parse(&toml).unwrap().runners.remove(0))
}

fn case(runner: &Runner, file: &str, suite: &[&str], name: &str, line: u32) -> TestCase {
    let suite: Vec<String> = suite.iter().map(|s| s.to_string()).collect();
    TestCase {
        runner: runner.name().to_string(),
        file: file.to_string(),
        line,
        end_line: line,
        id: runner.test_id(file, &suite, name, line),
        suite,
        name: name.to_string(),
        skip: false,
    }
}

fn run(repo: &Fixture, runner: &Runner, selection: Selection, fail_fast: bool) -> TestRun {
    let scratch = repo.scratch().join("run");
    runner
        .run(&RunRequest {
            worktree: repo.path(),
            selection,
            fail_fast,
            timeout: None,
            deadline: titer::deadline::Deadline::NONE,
            scratch: &scratch,
        })
        .unwrap()
}

const FILE: &str = "web/src/math.test.ts";

#[test]
fn file_line_selects_exactly_one_of_two_tests_with_the_same_full_name() {
    let repo = fixture();
    let runner = runner();
    let second = case(&runner, FILE, &["outer", "inner"], "works", 9);
    assert_eq!(second.id, "src/math.test.ts:9");

    let run = run(
        &repo,
        &runner,
        Selection::Tests(std::slice::from_ref(&second)),
        false,
    );

    assert_eq!(run.executed(), 1, "{run:?}");
    let CaseResult::Failed(failure) = run.result_for(&second, "web") else {
        panic!("{run:?}")
    };
    assert_eq!(failure.kind, FailureKind::Assertion);
    assert_eq!(failure.exception_type.as_deref(), Some("AssertionError"));
    assert!(
        failure.message.contains("expected 3 to be 4"),
        "{}",
        failure.message
    );
    let first = case(&runner, FILE, &["outer", "inner"], "works", 6);
    assert_eq!(run.result_for(&first, "web"), CaseResult::NotRun);
}

#[test]
fn reports_green_red_skipped_file_errors_and_bad_lines() {
    let repo = fixture();
    let runner = runner();
    let mut skipped = case(&runner, FILE, &["outer"], "skipped", 16);
    skipped.skip = true;
    let cases = vec![
        case(&runner, FILE, &["outer", "inner"], "works", 6),
        case(&runner, FILE, &["outer"], "works", 13),
        case(&runner, FILE, &["outer"], "throws", 17),
        skipped,
        case(&runner, FILE, &["outer"], "gone", 3),
        case(&runner, "web/src/broken.test.ts", &[], "b", 4),
        case(&runner, "web/src/missing.test.ts", &[], "m", 1),
    ];

    let run = run(&repo, &runner, Selection::Tests(&cases), false);

    let results: Vec<CaseResult> = cases.iter().map(|c| run.result_for(c, "web")).collect();
    assert_eq!(results[0], CaseResult::Passed, "{run:?}");
    assert_eq!(results[1], CaseResult::Passed);
    let CaseResult::Failed(exception) = &results[2] else {
        panic!("{:?}", results[2])
    };
    assert_eq!(exception.kind, FailureKind::Exception);
    assert_eq!(exception.exception_type.as_deref(), Some("TypeError"));
    assert_eq!(exception.message, "no x");
    assert_eq!(results[3], CaseResult::Skipped);
    assert_eq!(results[4], CaseResult::NotRun);
    let CaseResult::Failed(import) = &results[5] else {
        panic!("{:?}", results[5])
    };
    assert!(import.message.contains("./nothere"), "{}", import.message);
    assert_eq!(results[6], CaseResult::NotRun);
    assert!(run.runner_error.is_none());
}

#[test]
fn a_selection_that_matches_nothing_executes_zero_tests() {
    let repo = fixture();
    let runner = runner();
    let cases = vec![case(&runner, FILE, &["outer"], "gone", 2)];

    let run = run(&repo, &runner, Selection::Tests(&cases), false);

    assert_eq!(run.executed(), 0);
    assert_eq!(run.result_for(&cases[0], "web"), CaseResult::NotRun);
}

#[test]
fn bail_stops_after_the_first_failure() {
    let repo = fixture();
    let runner = runner();
    let cases = vec![
        case(&runner, FILE, &["outer", "inner"], "works", 9),
        case(&runner, FILE, &["outer"], "throws", 17),
    ];

    let run = run(&repo, &runner, Selection::Tests(&cases), true);

    assert!(matches!(
        run.result_for(&cases[0], "web"),
        CaseResult::Failed(_)
    ));
    assert_eq!(run.result_for(&cases[1], "web"), CaseResult::NotRun);
}

#[test]
fn files_and_suite_select_whole_files() {
    let repo = fixture();
    let runner = runner();
    let files = vec![FILE.to_string()];

    let by_file = run(&repo, &runner, Selection::Files(&files), false);
    let suite = run(&repo, &runner, Selection::Suite, false);

    // Five tests, one of them skipped.
    assert_eq!(by_file.executed(), 4, "{by_file:?}");
    assert_eq!(suite.executed(), 4);
    assert_eq!(suite.file_errors.len(), 1);
}

#[test]
fn a_file_error_never_lands_on_a_test_in_another_file_with_the_same_name() {
    let repo = fixture();
    // web/x.test.ts is a path suffix of web/src/x.test.ts, which fails to load.
    repo.write("web/x.test.ts", &TESTS.replace("./math", "./src/math"))
        .write("web/src/x.test.ts", BROKEN);
    let runner = runner();
    let cases = vec![
        case(&runner, "web/x.test.ts", &["outer"], "works", 13),
        case(&runner, "web/src/x.test.ts", &[], "b", 4),
    ];

    let run = run(&repo, &runner, Selection::Tests(&cases), false);

    assert_eq!(
        run.result_for(&cases[0], "web"),
        CaseResult::Passed,
        "{run:?}"
    );
    assert!(matches!(
        run.result_for(&cases[1], "web"),
        CaseResult::Failed(_)
    ));
}

const EACH: &str = r#"import { it, expect } from "vitest";

it.each([
  ["a", 1],
  ["b", 2],
])("maps %s", (name, n) => {
  expect(n).toBeLessThan(2);
});
"#;

#[test]
fn an_each_table_is_one_test_selected_by_its_call_line() {
    let repo = fixture();
    repo.write("web/src/each.test.ts", EACH);
    let runner = runner();
    // The title is formatted at run time, so only the line identifies the test.
    let table = case(&runner, "web/src/each.test.ts", &[], "maps %s", 6);

    let run = run(
        &repo,
        &runner,
        Selection::Tests(std::slice::from_ref(&table)),
        false,
    );

    assert_eq!(run.executed(), 2, "{run:?}");
    assert!(matches!(
        run.result_for(&table, "web"),
        CaseResult::Failed(_)
    ));
}

#[test]
fn a_throwing_after_all_hook_is_a_file_error() {
    let repo = fixture();
    repo.write(
        "web/src/hook.test.ts",
        "import { it, expect, afterAll } from \"vitest\";\n\nafterAll(() => {\n  throw new Error(\"hook\");\n});\n\nit(\"passes\", () => {\n  expect(1).toBe(1);\n});\n",
    );
    let runner = runner();
    let test = case(&runner, "web/src/hook.test.ts", &[], "passes", 7);

    let run = run(
        &repo,
        &runner,
        Selection::Tests(std::slice::from_ref(&test)),
        false,
    );

    assert_eq!(run.result_for(&test, "web"), CaseResult::Passed, "{run:?}");
    assert_eq!(run.file_errors.len(), 1, "{run:?}");
}

#[test]
fn an_unhandled_error_outside_tests_fails_the_selected_file() {
    let repo = fixture();
    repo.write(
        "web/src/unhandled.test.ts",
        "import { it, expect } from \"vitest\";\n\nit(\"passes\", () => {\n  setTimeout(() => { throw new Error(\"late\"); }, 0);\n  expect(1).toBe(1);\n});\n\nit(\"waits\", async () => {\n  await new Promise((r) => setTimeout(r, 50));\n});\n",
    );
    let runner = runner();
    let test = case(&runner, "web/src/unhandled.test.ts", &[], "passes", 3);
    let wait = case(&runner, "web/src/unhandled.test.ts", &[], "waits", 8);

    let run = run(&repo, &runner, Selection::Tests(&[test, wait]), false);

    assert_eq!(run.file_errors.len(), 1, "{run:?}");
    assert!(
        run.file_errors[0].failure.message.contains("unhandled"),
        "{run:?}"
    );
}
