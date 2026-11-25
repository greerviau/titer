//! Per-test coverage selects the changed tests that each mutant runs.

mod common;

use std::path::Path;

use common::{
    Fixture, assert_schema_valid, fixture_node_modules, fixture_node_modules_with_coverage,
    fixture_python, fixture_python_with_coverage, stdout_json,
};
use serde_json::Value;

const APP_BASE: &str = "def cap(x):\n    return x\n\n\ndef double(x):\n    return x\n";
const APP_HEAD: &str = "def cap(x):\n    if x > 3:\n        return 0\n    return x\n\n\ndef double(x):\n    return x * 2\n\n\ndef never(x):\n    return x - 1\n\n\ndef risky(x):\n    return x + 1\n";

/// test_double runs first and logs the two lines of cap that it reads from app.py, so the
/// log shows every mutant of cap that test_double ran against. test_cap kills every mutant
/// of cap, so none reaches the rerun against the whole test file. test_risky swallows every
/// exception, even the sentinel's SystemExit, so a sentinel cannot show that it runs risky.
fn pytest_tests(log: &Path) -> String {
    format!(
        "import os\n\nfrom app import cap, double, risky\n\n\ndef test_double():\n    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), \"app.py\")) as src:\n        lines = src.read().splitlines()\n    with open({log:?}, \"a\") as out:\n        out.write(lines[1] + \"|\" + lines[2] + \"\\n\")\n    assert double(2) == 4\n\n\ndef test_cap():\n    assert cap(4) == 0\n    assert cap(3) == 3\n\n\ndef test_risky():\n    try:\n        risky(1)\n    except BaseException:\n        pass\n",
        log = log.display().to_string()
    )
}

fn python_repo(python: &str, kind: &str) -> Fixture {
    let repo = Fixture::new();
    let extra = if kind == "unittest" {
        "start_dir = \"tests\"\n"
    } else {
        ""
    };
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"{kind}\"\ncommand = [\"{python}\", \"-m\", \"{kind}\"]\n{extra}"
        ),
    )
    .write("app.py", APP_BASE);
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("app.py", APP_HEAD);
    repo
}

/// (function, operator, outcome, ran), sorted, where ran says whether the mutant ran.
fn outcomes(report: &Value) -> Vec<(String, String, String, bool)> {
    let functions = [(1, "cap"), (7, "double"), (11, "never"), (15, "risky")];
    let mut got: Vec<(String, String, String, bool)> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let line = m["line"].as_u64().unwrap();
            let function = functions
                .iter()
                .rev()
                .find(|(start, _)| *start <= line)
                .unwrap()
                .1;
            (
                function.to_string(),
                m["operator"].as_str().unwrap().to_string(),
                m["outcome"].as_str().unwrap().to_string(),
                !m["duration_ms"].is_null(),
            )
        })
        .collect();
    got.sort();
    got
}

fn by_outcome(report: &Value) -> Vec<(String, String)> {
    let mut got: Vec<(String, String)> = outcomes(report)
        .into_iter()
        .map(|(f, _, o, _)| (f, o))
        .collect();
    got.sort();
    got.dedup();
    got
}

const CAP_LINES: &str = "    if x > 3:|        return 0";

/// Asserts that the non-covering test ran against fewer mutants of cap than there are,
/// where without coverage it runs against all of them. A narrowed run that a loaded machine
/// times out or leaves without a result runs again with every changed test, so a few may
/// still show up.
fn assert_narrowed(lines: &[String], head: &str, report: &Value) {
    let cap = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| (2..=3).contains(&m["line"].as_u64().unwrap()))
        .count();
    let mutated = lines.iter().filter(|l| *l != head).count();
    assert!(!lines.is_empty() && cap > 0, "{report:#}");
    assert!(
        mutated < cap,
        "the non-covering test ran against {mutated} of {cap} mutants of cap: {lines:#?}\n{report:#}"
    );
}

fn logged(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn with_coverage_each_pytest_mutant_runs_only_the_changed_tests_that_cover_its_line() {
    let repo = python_repo(&fixture_python_with_coverage(), "pytest");
    let log = repo.scratch().join("double.log");
    repo.write("test_app.py", &pytest_tests(&log));

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--budget", "600s"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_narrowed(&logged(&log), CAP_LINES, &report);
    // Coverage shows that no changed test runs never, so its survivors are not executed
    // without a sentinel, and that test_risky runs risky, so its survivors are not mistaken
    // for not executed.
    assert_eq!(
        by_outcome(&report),
        vec![
            ("cap".into(), "killed".into()),
            ("double".into(), "killed".into()),
            ("never".into(), "not_executed".into()),
            ("risky".into(), "survived".into()),
        ],
        "{report:#}"
    );
    assert!(repo.leftover_tmp().is_empty());
}

#[test]
fn without_coverage_every_pytest_mutant_runs_every_changed_test() {
    let repo = python_repo(&fixture_python(), "pytest");
    let log = repo.scratch().join("double.log");
    repo.write("test_app.py", &pytest_tests(&log));

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--budget", "600s"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert!(logged(&log).iter().any(|l| l != CAP_LINES), "{report:#}");
    // The sentinel finds never's survivors, and test_risky catches the sentinel in risky.
    assert_eq!(
        by_outcome(&report),
        vec![
            ("cap".into(), "killed".into()),
            ("double".into(), "killed".into()),
            ("never".into(), "not_executed".into()),
            ("risky".into(), "not_executed".into()),
        ],
        "{report:#}"
    );
}

#[test]
fn with_coverage_each_unittest_mutant_runs_only_the_changed_tests_that_cover_its_line() {
    let repo = python_repo(&fixture_python_with_coverage(), "unittest");
    let log = repo.scratch().join("double.log");
    let tests = format!(
        "import os\nimport sys\nimport unittest\n\nsys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))\n\nfrom app import cap, double, risky\n\n\nclass TestApp(unittest.TestCase):\n    def test_a_double(self):\n        with open(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), \"app.py\")) as src:\n            lines = src.read().splitlines()\n        with open({log:?}, \"a\") as out:\n            out.write(lines[1] + \"|\" + lines[2] + \"\\n\")\n        self.assertEqual(double(2), 4)\n\n    def test_b_cap(self):\n        self.assertEqual(cap(4), 0)\n        self.assertEqual(cap(3), 3)\n\n    def test_c_risky(self):\n        try:\n            risky(1)\n        except BaseException:\n            pass\n",
        log = log.display().to_string()
    );
    repo.write("tests/test_app.py", &tests);

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--budget", "600s"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_narrowed(&logged(&log), CAP_LINES, &report);
    assert_eq!(
        by_outcome(&report),
        vec![
            ("cap".into(), "killed".into()),
            ("double".into(), "killed".into()),
            ("never".into(), "not_executed".into()),
            ("risky".into(), "survived".into()),
        ],
        "{report:#}"
    );
}

const TS_BASE: &str = "export function cap(x: number) {\n  return x;\n}\n\nexport function double(x: number) {\n  return x;\n}\n";
const TS_HEAD: &str = "export function cap(x: number) {\n  if (x > 3) {\n    return 0;\n  }\n  return x;\n}\n\nexport function double(x: number) {\n  return x * 2;\n}\n\nexport function never(x: number) {\n  return x - 1;\n}\n";

fn vitest_repo(node_modules: &Path, log: &Path) -> Fixture {
    let repo = Fixture::new();
    repo.write(".gitignore", "node_modules\n")
        .write(
            "titer.toml",
            "[[runner]]\nkind = \"vitest\"\ncommand = [\"npx\", \"--no-install\", \"vitest\", \"run\"]\n",
        )
        .write("package.json", "{\n  \"type\": \"module\"\n}\n")
        .write("src/app.ts", TS_BASE);
    std::os::unix::fs::symlink(node_modules, repo.path().join("node_modules")).unwrap();
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/app.ts", TS_HEAD).write(
        "src/app.test.ts",
        &format!(
            "import {{ appendFileSync, readFileSync }} from \"node:fs\";\nimport {{ it, expect }} from \"vitest\";\nimport {{ cap, double }} from \"./app\";\n\nit(\"doubles\", () => {{\n  const lines = readFileSync(new URL(\"./app.ts\", import.meta.url), \"utf8\").split(\"\\n\");\n  appendFileSync({log:?}, lines[1] + \"|\" + lines[2] + \"\\n\");\n  expect(double(2)).toBe(4);\n}});\n\nit(\"caps\", () => {{\n  expect(cap(4)).toBe(0);\n  expect(cap(3)).toBe(3);\n}});\n",
            log = log.display().to_string()
        ),
    );
    repo
}

const TS_CAP_LINES: &str = "  if (x > 3) {|    return 0;";

#[test]
fn with_coverage_each_vitest_mutant_runs_only_the_changed_tests_that_cover_its_line() {
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("double.log");
    let repo = vitest_repo(&fixture_node_modules_with_coverage(), &log);

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--budget", "600s"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_narrowed(&logged(&log), TS_CAP_LINES, &report);
    let never: Vec<&Value> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["line"] == 13)
        .collect();
    assert!(!never.is_empty(), "{report:#}");
    assert!(
        never.iter().all(|m| m["outcome"] == "not_executed"),
        "{report:#}"
    );
}

#[test]
fn without_coverage_every_vitest_mutant_runs_every_changed_test() {
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("double.log");
    let repo = vitest_repo(&fixture_node_modules(), &log);

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--budget", "600s"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert!(logged(&log).iter().any(|l| l != TS_CAP_LINES), "{report:#}");
    let never: Vec<&Value> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["line"] == 13)
        .collect();
    assert!(!never.is_empty(), "{report:#}");
    assert!(
        never.iter().all(|m| m["outcome"] == "not_executed"),
        "{report:#}"
    );
}

const TARGET_BASE: &str = "def target(value):\n    if value:\n        return 1\n    return 3\n";
const TARGET_HEAD: &str = "def target(value):\n    if value:\n        return 2\n    return 3\n";

/// Outcomes of the mutants of `target`'s changed return, with and without coverage.py.
fn target_outcomes(tests: &[(&str, &str)]) -> [(Vec<String>, Value); 2] {
    changed_outcomes(TARGET_BASE, TARGET_HEAD, tests)
}

/// Outcomes of the mutants of a change from `base` to `head` in app.py, with and without
/// coverage.py.
fn changed_outcomes(base: &str, head: &str, tests: &[(&str, &str)]) -> [(Vec<String>, Value); 2] {
    [fixture_python_with_coverage(), fixture_python()].map(|python| {
        let repo = python_repo(&python, "pytest");
        repo.write("app.py", base);
        repo.git(&["commit", "-qam", "target"]);
        repo.write("app.py", head);
        for (path, text) in tests {
            repo.write(path, text);
        }
        let output = repo.titer(&[
            "check", "--json", "--only", "mutate", "--base", "HEAD", "--budget", "600s",
        ]);
        let report = stdout_json(&output);
        let mut got: Vec<String> = report["mutants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["outcome"].as_str().unwrap().to_string())
            .collect();
        got.sort();
        (got, report)
    })
}

#[test]
fn a_narrowed_selection_that_is_not_green_on_head_does_not_kill() {
    // test_a passes only after test_setup's module has loaded, so the tests that cover the
    // line fail without it.
    let [(with, report), (without, _)] = target_outcomes(&[
        (
            "test_a.py",
            "import os\nfrom app import target\n\n\ndef test_result():\n    assert target(True) in (None, 2, 3)\n    assert os.environ[\"TITER_FIXTURE_SETTING\"] == \"1\"\n",
        ),
        (
            "test_setup.py",
            "import os\nos.environ[\"TITER_FIXTURE_SETTING\"] = \"1\"\n\n\ndef test_setup():\n    assert True\n",
        ),
    ]);
    assert_eq!(with, ["survived", "survived"], "{report:#}");
    assert_eq!(with, without);
}

#[test]
fn a_test_that_coverage_misattributes_still_kills() {
    // The fixture calls a helper named like a test, so coverage attributes the line to it.
    let [(with, report), (without, _)] = target_outcomes(&[(
        "test_app.py",
        "import pytest\nfrom app import target\n\n\ndef test_probe(value=False):\n    return target(value) if value else None\n\n\n@pytest.fixture\ndef result():\n    return test_probe(True)\n\n\ndef test_result(result):\n    assert result == 2\n",
    )]);
    assert_eq!(with, ["killed", "killed"], "{report:#}");
    assert_eq!(with, without);
}

#[test]
fn a_forked_child_counts_as_running_the_line() {
    // Only the child runs the changed line, and nothing checks what it returns. The
    // sentinel kills the child, which the parent ignores, so without coverage the line
    // looks unexecuted. Coverage saves the child's data and shows that it ran.
    let [(with, report), (without, _)] = target_outcomes(&[(
        "test_app.py",
        "import multiprocessing\nfrom app import target\n\n\ndef child():\n    target(True)\n\n\ndef test_worker():\n    proc = multiprocessing.get_context(\"fork\").Process(target=child)\n    proc.start()\n    proc.join(5)\n    assert target(False) == 3\n",
    )]);
    assert_eq!(with, ["survived", "survived"], "{report:#}");
    assert_eq!(without, ["not_executed", "not_executed"]);
}

#[test]
fn a_test_that_passes_only_with_the_coverage_variables_is_red_on_head() {
    let [(with, report), (without, _)] = target_outcomes(&[(
        "test_app.py",
        "import os\nfrom app import target\n\n\ndef test_result():\n    assert os.getenv(\"COVERAGE_PROCESS_START\")\n    assert target(True) == 2\n",
    )]);
    assert_eq!(report["gate"]["status"], "error", "{report:#}");
    assert_eq!(with, without);
}

#[test]
fn a_test_that_fails_only_under_coverage_runs_without_it() {
    let [(with, report), (without, _)] = target_outcomes(&[(
        "test_app.py",
        "import sys\nfrom app import target\n\n\ndef test_result():\n    assert sys.gettrace() is None\n    assert target(True) == 2\n",
    )]);
    assert_eq!(with, ["killed", "killed"], "{report:#}");
    assert_eq!(with, without);
    assert!(
        report["warnings"][0]
            .as_str()
            .unwrap()
            .contains("has another result under coverage"),
        "{report:#}"
    );
}

#[test]
fn a_line_a_fixture_runs_is_not_mistaken_for_not_executed() {
    // The fixture swallows the sentinel's SystemExit, but coverage saw the line run.
    let [(with, report), (without, _)] = target_outcomes(&[(
        "test_app.py",
        "import pytest\nfrom app import target\n\n\n@pytest.fixture\ndef result():\n    try:\n        target(True)\n    except BaseException:\n        pass\n\n\ndef test_result(result):\n    assert True\n",
    )]);
    assert_eq!(with, ["survived", "survived"], "{report:#}");
    assert_eq!(without, ["not_executed", "not_executed"]);
}

#[test]
fn a_vitest_test_that_reaches_code_only_after_another_still_kills() {
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("unused.log");
    let repo = vitest_repo(&fixture_node_modules_with_coverage(), &log);
    repo.write(
        "src/app.test.ts",
        "import { it, expect } from \"vitest\";\n\nlet ready = false;\nit(\"first\", async () => {\n  await import(\"./app\");\n  ready = true;\n});\nit(\"second\", async () => {\n  if (ready) {\n    const { double } = await import(\"./app\");\n    expect(double(2)).toBe(4);\n  }\n});\n",
    );

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--budget", "600s"]);

    let report = stdout_json(&output);
    let double: Vec<&Value> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["line"] == 9)
        .collect();
    assert!(!double.is_empty(), "{report:#}");
    // A loaded machine can time a mutant out instead; either way the tests detect it.
    assert!(
        double
            .iter()
            .all(|m| m["outcome"] == "killed" || m["outcome"] == "timed_out"),
        "{report:#}"
    );
}

#[test]
fn a_mutant_of_a_line_no_test_runs_still_runs() {
    // Removing the yield on a line no test reaches turns the generator into a function.
    let [(with, report), (without, _)] = changed_outcomes(
        "def gen(value):\n    if value:\n        return (yield 1)\n    return 3\n",
        "def gen(value):\n    if value:\n        return (yield 2)\n    return 3\n",
        &[(
            "test_app.py",
            "import types\nfrom app import gen\n\n\ndef test_is_a_generator():\n    assert isinstance(gen(False), types.GeneratorType)\n",
        )],
    );
    assert!(with.contains(&"killed".to_string()), "{report:#}");
    assert_eq!(with, without);
}

#[test]
fn an_uncovered_line_a_sentinel_cannot_probe_still_reruns_against_the_whole_file() {
    // The changed test never reaches the else branch, which shares its line with `}`, so
    // no sentinel can probe it. An unchanged test in the same file does reach it.
    let tests = |changed: u32| {
        format!(
            "import {{ it, expect }} from \"vitest\";\nimport {{ target }} from \"./app\";\n\nit(\"changed\", () => {{\n  expect(target(true)).toBe({changed});\n}});\n\nit(\"other\", () => {{\n  expect(target(false)).toBe(2);\n}});\n"
        )
    };
    let app = |amount: u32| {
        format!(
            "export function target(value: boolean) {{\n  if (value) {{\n    return 3;\n  }} else {{ return {amount}; }}\n}}\n"
        )
    };
    let run = |node_modules: &Path| {
        let repo = Fixture::new();
        repo.write(".gitignore", "node_modules\n")
            .write(
                "titer.toml",
                "[[runner]]\nkind = \"vitest\"\ncommand = [\"npx\", \"--no-install\", \"vitest\", \"run\"]\n",
            )
            .write("package.json", "{\n  \"type\": \"module\"\n}\n")
            .write("app.ts", &app(1))
            .write("app.test.ts", &tests(2));
        std::os::unix::fs::symlink(node_modules, repo.path().join("node_modules")).unwrap();
        repo.commit_all("base");
        repo.git(&["checkout", "-q", "-b", "fix"]);
        repo.write("app.ts", &app(2))
            .write("app.test.ts", &tests(3));
        let report =
            stdout_json(&repo.titer(&["check", "--json", "--only", "mutate", "--budget", "600s"]));
        let mut got: Vec<String> = report["mutants"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["line"] == 4)
            .map(|m| m["outcome"].as_str().unwrap().to_string())
            .collect();
        got.sort();
        (got, report)
    };
    let (with, report) = run(&fixture_node_modules_with_coverage());
    let (without, _) = run(&fixture_node_modules());
    assert!(!with.is_empty(), "{report:#}");
    assert!(with.iter().all(|o| o == "caught_by_other"), "{report:#}");
    assert_eq!(with, without);
}

#[test]
fn a_parameter_that_skips_only_under_coverage_makes_coverage_unusable() {
    let [(with, report), (without, _)] = target_outcomes(&[(
        "test_app.py",
        "import sys\nimport pytest\nfrom app import target\n\n\n@pytest.mark.parametrize(\"value\", [False, True])\ndef test_result(value):\n    if value and sys.gettrace() is not None:\n        pytest.skip(\"traced\")\n    target(value)\n",
    )]);
    assert_eq!(with, without, "{report:#}");
    assert!(
        report["warnings"][0]
            .as_str()
            .unwrap()
            .contains("has another result under coverage"),
        "{report:#}"
    );
}
