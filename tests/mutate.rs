mod common;

use common::{Fixture, assert_schema_valid, fixture_node_modules, fixture_python, stdout_json};
use serde_json::Value;

const CLAMP_BASE: &str = "def clamp(x, hi):\n    return x\n";
const CLAMP_HEAD: &str = "def clamp(x, hi):\n    if x > hi:\n        return hi\n    return x\n";
const CLAMP_TEST: &str = "from app import clamp\n\n\ndef test_clamps_high_values():\n    assert clamp(5, 3) == 3\n\n\ndef test_keeps_low_values():\n    assert clamp(1, 3) == 1\n";

fn repo(budget: &str) -> Fixture {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"pytest\"\ncommand = [\"{}\", \"-m\", \"pytest\"]\n\n[mutate]\nbudget = \"{budget}\"\n",
            fixture_python()
        ),
    )
    .write("app.py", CLAMP_BASE)
    .write("other.py", "def untouched():\n    return 1\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo
}

fn outcomes(report: &Value) -> Vec<(u64, String, String, String)> {
    report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["line"].as_u64().unwrap(),
                m["operator"].as_str().unwrap().to_string(),
                m["replacement"].as_str().unwrap().to_string(),
                m["outcome"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn mutants_on_changed_lines_are_killed_or_survive() {
    let repo = repo("120s");
    repo.write("app.py", CLAMP_HEAD)
        .write("test_app.py", CLAMP_TEST);
    let before = repo.checkout_state();

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--workers", "2"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let mut got = outcomes(&report);
    got.sort();
    assert_eq!(
        got,
        vec![
            (
                2,
                "negate_if".into(),
                "not (x > hi)".into(),
                "killed".into()
            ),
            (2, "swap_comparison".into(), ">=".into(), "survived".into()),
            (
                3,
                "return_none".into(),
                "return None".into(),
                "killed".into()
            ),
        ]
    );
    let killed = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["outcome"] == "killed")
        .unwrap();
    assert!(
        killed["killed_by"]
            .as_str()
            .unwrap()
            .starts_with("test_app.py::"),
        "{killed:#}"
    );
    assert_eq!(report["timing"]["workers"], 2);
    assert_eq!(repo.checkout_state(), before);
    assert!(repo.leftover_tmp().is_empty());
}

#[test]
fn modules_the_tests_never_load_are_not_executed() {
    let repo = repo("120s");
    repo.write("app.py", CLAMP_HEAD)
        .write("test_app.py", CLAMP_TEST)
        .write("other.py", "def untouched():\n    return 2\n");

    let output = repo.titer(&["check", "--json", "--only", "mutate"]);

    let report = stdout_json(&output);
    let other: Vec<&Value> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["file"] == "other.py")
        .collect();
    assert_eq!(other.len(), 2, "{report:#}");
    assert!(
        other.iter().all(|m| m["outcome"] == "not_executed"),
        "{report:#}"
    );
    assert!(report["warnings"][0].as_str().unwrap().contains("other.py"));
}

#[test]
fn a_canary_that_survives_everywhere_exits_2() {
    let repo = repo("120s");
    repo.write("test_app.py", "def test_unrelated():\n    assert True\n")
        .write("other.py", "def untouched():\n    return 2\n");

    let output = repo.titer(&["check", "--json", "--only", "mutate"]);

    assert_eq!(output.status.code(), Some(2));
    let report = stdout_json(&output);
    assert!(
        report["gate"]["reasons"][0]
            .as_str()
            .unwrap()
            .contains("canary"),
        "{report:#}"
    );
}

#[test]
fn a_test_that_catches_the_import_error_still_runs_its_mutants() {
    let repo = repo("120s");
    // An optional fast path: the test falls back when the module fails to import, which
    // also swallows the canary, though the tests load the module and catch real mutants.
    // A future import must stay first, so the canary goes after it.
    repo.write(
        "app.py",
        &format!("from __future__ import annotations\n\n{CLAMP_HEAD}"),
    )
    .write(
        "test_app.py",
        &CLAMP_TEST.replace(
            "from app import clamp\n",
            "try:\n    from app import clamp\nexcept Exception:\n    def clamp(x, hi):\n        return min(x, hi)\n",
        ),
    );

    let output = repo.titer(&["check", "--json", "--only", "mutate"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let mut got = outcomes(&report);
    got.sort();
    assert_eq!(
        got.iter().map(|o| o.3.as_str()).collect::<Vec<_>>(),
        ["killed", "survived", "killed"],
        "{report:#}"
    );
}

#[test]
fn a_vitest_test_that_catches_the_import_error_still_runs_its_mutants() {
    let repo = Fixture::new();
    repo.write(".gitignore", "node_modules\n")
        .write(
            "titer.toml",
            "[[runner]]\nkind = \"vitest\"\ncommand = [\"npx\", \"--no-install\", \"vitest\", \"run\"]\n",
        )
        .write("package.json", "{\n  \"type\": \"module\"\n}\n")
        .write("src/half.ts", "export const half = (x: number) => x;\n");
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("node_modules")).unwrap();
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/half.ts", "export const half = (x: number) => x / 2;\n")
        .write(
            "src/half.test.ts",
            "import { it, expect } from \"vitest\";\n\nlet half: (x: number) => number;\ntry {\n  ({ half } = await import(\"./half\"));\n} catch {\n  half = (x) => x / 2;\n}\n\nit(\"halves\", () => {\n  expect(half(4)).toBe(2);\n});\n",
        );

    let output = repo.titer(&["check", "--json", "--only", "mutate"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let mutants = report["mutants"].as_array().unwrap();
    assert!(!mutants.is_empty(), "{report:#}");
    assert!(
        mutants.iter().all(|m| m["outcome"] == "killed"),
        "{report:#}"
    );
}

#[test]
fn survivors_on_an_elif_the_tests_never_reach_are_not_executed() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"pytest\"\ncommand = [\"{}\", \"-m\", \"pytest\"]\n",
            fixture_python()
        ),
    )
    .write(
        "app.py",
        "def f(x):\n    if x > 1:\n        return 2\n    return x\n",
    )
    .write(
        "test_app.py",
        "from app import f\n\n\ndef test_big():\n    assert f(2) == 2\n",
    );
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    // The test takes the first branch, so it never evaluates the new elif.
    repo.write(
        "app.py",
        "def f(x):\n    if x > 1:\n        return 2\n    elif x < 0:\n        return 0\n    return x\n",
    );

    let output = repo.titer(&["check", "--json", "--only", "mutate"]);

    let report = stdout_json(&output);
    let elif: Vec<(u64, String, String, String)> =
        outcomes(&report).into_iter().filter(|o| o.0 == 4).collect();
    assert!(!elif.is_empty(), "{report:#}");
    assert!(
        elif.iter().all(|o| o.3 == "not_executed"),
        "{elif:?}\n{report:#}"
    );

    // f(0) evaluates the elif, and no mutant of it changes the result, so they survive.
    repo.write(
        "test_app.py",
        // A test that stubs sys.exit must not hide the probe.
        "import sys\n\nfrom app import f\n\n\ndef test_big():\n    assert f(2) == 2\n\n\ndef test_zero(monkeypatch):\n    monkeypatch.setattr(sys, \"exit\", lambda *a: None)\n    assert f(0) == 0\n",
    );
    let report = stdout_json(&repo.titer(&["check", "--json", "--only", "mutate"]));
    let elif: Vec<(u64, String, String, String)> =
        outcomes(&report).into_iter().filter(|o| o.0 == 4).collect();
    assert!(!elif.is_empty(), "{report:#}");
    assert!(
        elif.iter().all(|o| o.3 == "survived"),
        "{elif:?}\n{report:#}"
    );
}

#[test]
fn without_changed_tests_the_suite_runs_against_mutants() {
    let repo = repo("120s");
    repo.write("test_app.py", CLAMP_TEST);
    repo.commit_all("tests first");
    repo.write("app.py", CLAMP_HEAD);
    repo.git(&["checkout", "-q", "-b", "fix2"]);

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--base", "HEAD"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let killed = outcomes(&report).iter().filter(|o| o.3 == "killed").count();
    assert_eq!(killed, 2, "{report:#}");
}

#[test]
fn mutants_past_the_budget_are_not_run() {
    let repo = repo("1ms");
    repo.write("app.py", CLAMP_HEAD)
        .write("test_app.py", CLAMP_TEST);

    let output = repo.titer(&["check", "--json", "--only", "mutate"]);

    let report = stdout_json(&output);
    assert!(
        outcomes(&report).iter().all(|o| o.3 == "not_run"),
        "{report:#}"
    );
    // With only mutate run, mutants that never ran leave the change unproven.
    assert_eq!(output.status.code(), Some(1), "{report:#}");
}

const LABEL_BASE: &str = "def label(x):\n    if x < 0:\n        return \"neg\"\n    return \"pos\"\n\n\ndef shout(s):\n    return s.upper()\n";
const LABEL_HEAD: &str = "def label(x):\n    if x <= -1:\n        return \"neg\"\n    return \"pos\"\n\n\ndef shout(s):\n    return s.upper() + \"!\"\n";
const LABEL_TEST_BASE: &str =
    "from app import label\n\n\ndef test_minus_one_is_neg():\n    assert label(-1) == \"neg\"\n";

#[test]
fn survivors_split_into_not_executed_and_caught_by_other_tests() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"pytest\"\ncommand = [\"{}\", \"-m\", \"pytest\"]\n",
            fixture_python()
        ),
    )
    .write("app.py", LABEL_BASE)
    .write("test_app.py", LABEL_TEST_BASE);
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    // The new test is weak: -5 stays negative under most mutants of line 2. The older test
    // in the same file pins the -1 boundary. Nothing calls shout.
    repo.write("app.py", LABEL_HEAD).write(
        "test_app.py",
        &format!(
            "{LABEL_TEST_BASE}\n\ndef test_minus_five_is_neg():\n    assert label(-5) == \"neg\"\n"
        ),
    );

    let output = repo.titer(&["check", "--json", "--only", "mutate"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let mut got = outcomes(&report);
    got.sort();
    assert_eq!(
        got,
        vec![
            (
                2,
                "increment_int".into(),
                "2".into(),
                "caught_by_other".into()
            ),
            (
                2,
                "negate_if".into(),
                "not (x <= -1)".into(),
                "killed".into()
            ),
            (
                2,
                "swap_comparison".into(),
                "<".into(),
                "caught_by_other".into()
            ),
            (
                8,
                "return_none".into(),
                "return None".into(),
                "not_executed".into()
            ),
            (
                8,
                "swap_arithmetic".into(),
                "-".into(),
                "not_executed".into()
            ),
        ]
    );
    let caught = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["outcome"] == "caught_by_other")
        .unwrap();
    assert_eq!(
        caught["killed_by"], "test_app.py::test_minus_one_is_neg",
        "{caught:#}"
    );
}

#[test]
fn agent_mutants_run_like_generated_ones_and_are_tagged() {
    let repo = repo("120s");
    repo.write("app.py", CLAMP_HEAD)
        .write("test_app.py", CLAMP_TEST);
    let mutants = repo.scratch().join("mutants.jsonl");
    std::fs::write(
        &mutants,
        concat!(
            "{\"file\": \"app.py\", \"line\": 3, \"replacement\": \"        return x\", \"id\": \"keep-x\"}\n",
            "{\"file\": \"app.py\", \"line\": 2, \"end_line\": 3, \"replacement\": \"    if x >= hi:\\n        return hi\"}\n",
            "{\"file\": \"README.md\", \"line\": 1, \"replacement\": \"x\"}\n",
        ),
    )
    .unwrap();

    let output = repo.titer(&[
        "check",
        "--json",
        "--only",
        "mutate",
        "--mutants",
        mutants.to_str().unwrap(),
    ]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let agent: Vec<(String, String, String)> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["source"] == "agent")
        .map(|m| {
            (
                m["id"].as_str().unwrap().to_string(),
                m["operator"].as_str().unwrap().to_string(),
                m["outcome"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        agent,
        vec![
            ("keep-x".into(), "agent".into(), "killed".into()),
            ("app.py:2:agent:2".into(), "agent".into(), "survived".into()),
        ]
    );
    let warnings = report["warnings"].to_string();
    assert!(
        warnings.contains("README.md is not a source file"),
        "{warnings}"
    );
}

#[test]
fn a_malformed_mutants_file_exits_2() {
    let repo = repo("120s");
    repo.write("app.py", CLAMP_HEAD)
        .write("test_app.py", CLAMP_TEST);
    let mutants = repo.scratch().join("mutants.jsonl");
    std::fs::write(&mutants, "{not json}\n").unwrap();

    let output = repo.titer(&[
        "check",
        "--json",
        "--only",
        "mutate",
        "--mutants",
        mutants.to_str().unwrap(),
    ]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--mutants"));
}

#[test]
fn mutants_of_code_the_change_only_reformats_are_marked_unchanged_from_base() {
    let repo = repo("120s");
    repo.write(
        "app.py",
        "def clamp(x, hi):\n    if x > hi: return hi\n    return x\n",
    );
    repo.commit_all("one-line if");
    // Line 2 only splits the base line; line 4 is new behavior.
    repo.write(
        "app.py",
        "def clamp(x, hi):\n    if x > hi:\n        return hi\n    return max(x, 0)\n",
    )
    .write(
        "test_app.py",
        &format!("{CLAMP_TEST}\n\ndef test_floors_at_zero():\n    assert clamp(-1, 3) == 0\n"),
    );
    let summary = repo.scratch().join("summary.md");

    let output = repo.titer(&[
        "check",
        "--json",
        "--only",
        "mutate",
        "--base",
        "HEAD",
        "--markdown",
        summary.to_str().unwrap(),
    ]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    let mut got: Vec<(u64, String, bool)> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["line"].as_u64().unwrap(),
                m["operator"].as_str().unwrap().to_string(),
                m["unchanged_from_base"].as_bool().unwrap(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (2, "negate_if".into(), true),
            (2, "swap_comparison".into(), true),
            (3, "return_none".into(), true),
            (4, "increment_int".into(), false),
            (4, "return_none".into(), false),
        ],
        "{report:#}"
    );
    let text = std::fs::read_to_string(&summary).unwrap();
    assert!(
        text.contains("app.py:2: `>` to `>=` (this code is also in the base lines the change replaces, so it may predate the change)"),
        "{text}"
    );
}

const PAUSE_BASE: &str = "import time\n\n\ndef pause(x):\n    return x\n";
const PAUSE_HEAD: &str =
    "import time\n\n\ndef pause(x):\n    if x < 0:\n        time.sleep(60)\n    return x\n";
const PAUSE_TEST: &str = "from app import pause\n\n\ndef test_pause():\n    assert pause(1) == 1\n";

#[test]
fn the_budget_kills_a_running_mutant_and_its_follow_up() {
    let repo = repo("8s");
    repo.write("app.py", PAUSE_BASE);
    repo.commit_all("pause");
    repo.write("app.py", PAUSE_HEAD)
        .write("test_app.py", PAUSE_TEST);

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--workers", "1"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    let got = outcomes(&report);
    // Negating the condition sleeps for 60 s, past its own 10 s timeout and the 8 s budget.
    let hang = got
        .iter()
        .find(|o| o.1 == "negate_if")
        .unwrap_or_else(|| panic!("{report:#}"));
    assert_eq!(hang.3, "not_run", "{report:#}");
    // The deadline passed during the hang, so no survivor had its sentinel or other-tests
    // run, and none may be reported as a survivor.
    assert!(
        got.iter().all(|o| o.3 != "survived" && o.3 != "timed_out"),
        "{report:#}"
    );
    let not_run = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["outcome"] == "not_run");
    assert!(
        not_run.into_iter().all(|m| m["duration_ms"].is_null()),
        "{report:#}"
    );
    // The report's total counts from the start of the check, as the budget does.
    let total = report["timing"]["total_ms"].as_u64().unwrap();
    assert!(
        total < 10_000,
        "the check overran its 8 s budget: {total} ms"
    );
    assert!(repo.leftover_tmp().is_empty(), "{:?}", repo.leftover_tmp());
}
