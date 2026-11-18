mod common;

use common::{Fixture, assert_schema_valid, fixture_python, stdout_json};

const BASE: &str = "def clamp(x, hi):\n    return x\n";
const HEAD: &str = "def clamp(x, hi):\n    if x > hi:\n        return hi\n    return x\n";

fn repo(gate: &str) -> Fixture {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"pytest\"\ncommand = [\"{}\", \"-m\", \"pytest\"]\n{gate}",
            fixture_python()
        ),
    )
    .write("app.py", BASE)
    .write(
        "test_old.py",
        "from app import clamp\n\n\ndef test_low():\n    assert clamp(1, 3) == 1\n",
    );
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("app.py", HEAD);
    repo
}

fn check(repo: &Fixture) -> (i32, serde_json::Value) {
    let output = repo.titer(&["check", "--json"]);
    let report = stdout_json(&output);
    assert_schema_valid(&report);
    (output.status.code().unwrap(), report)
}

#[test]
fn a_proven_change_passes_with_exit_0() {
    let repo = repo("");
    repo.write(
        "test_new.py",
        "from app import clamp\n\n\ndef test_high():\n    assert clamp(5, 3) == 3\n",
    );

    let (code, report) = check(&repo);

    assert_eq!(code, 0, "{report:#}");
    assert_eq!(report["gate"]["status"], "pass");
    assert_eq!(report["tests"][0]["verdict"], "assertion");
}

#[test]
fn an_unproven_change_fails_with_exit_1() {
    let repo = repo("");
    // Passes on base and never calls clamp, so no mutant of the changed lines can fail it.
    repo.write(
        "test_new.py",
        "import app\n\n\ndef test_exists():\n    assert callable(app.clamp)\n",
    );

    let (code, report) = check(&repo);

    assert_eq!(code, 1, "{report:#}");
    assert_eq!(report["gate"]["status"], "fail");
    assert!(
        report["gate"]["reasons"][0]
            .as_str()
            .unwrap()
            .starts_with("unproven change")
    );
    assert_eq!(report["tests"][0]["verdict"], "passes_on_base");
}

#[test]
fn a_change_whose_tests_fail_on_head_exits_2() {
    let repo = repo("");
    repo.write(
        "test_new.py",
        "from app import clamp\n\n\ndef test_wrong():\n    assert clamp(5, 3) == 5\n",
    );

    let (code, report) = check(&repo);

    assert_eq!(code, 2, "{report:#}");
    assert_eq!(report["gate"]["status"], "error");
}

#[test]
fn gate_options_fail_on_scan_findings() {
    let repo = repo("\n[gate]\nfail_on_scan = true\n");
    repo.write("test_new.py", "from app import clamp\n\n\ndef test_high():\n    assert clamp(5, 3) == 3\n")
        .write("test_old.py", "import pytest\n\nfrom app import clamp\n\n\n@pytest.mark.skip\ndef test_low():\n    assert clamp(1, 3) == 1\n");

    let (code, report) = check(&repo);

    assert_eq!(code, 1, "{report:#}");
    assert!(
        report["gate"]["reasons"][0]
            .as_str()
            .unwrap()
            .contains("fail_on_scan")
    );
    assert_eq!(report["findings"][0]["kind"], "skip_added");
}

#[test]
fn text_output_summarizes_the_gate() {
    let repo = repo("");
    repo.write(
        "test_new.py",
        "from app import clamp\n\n\ndef test_high():\n    assert clamp(5, 3) == 3\n",
    );

    let output = repo.titer(&["check"]);

    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("titer gate: pass\n"), "{text}");
    assert!(text.contains("Assertion"), "{text}");
}

#[test]
fn a_red_suite_exits_2_even_when_the_change_yields_no_mutants() {
    let repo = repo("");
    // Neither a docstring nor an import has mutants, and the import breaks test_old on head.
    repo.write(
        "app.py",
        "\"\"\"Clamping.\"\"\"\nimport missing_module\n\n\ndef clamp(x, hi):\n    return x\n",
    );

    let output = repo.titer(&["check", "--json"]);

    let report = stdout_json(&output);
    assert_eq!(report["mutants"], serde_json::json!([]), "{report:#}");
    assert_eq!(output.status.code(), Some(2), "{report:#}");
    assert!(
        report["gate"]["reasons"][0]
            .as_str()
            .unwrap()
            .contains("head is not green")
    );
}

#[test]
fn markdown_writes_a_job_summary() {
    let repo = repo("");
    repo.write(
        "test_new.py",
        "from app import clamp\n\n\ndef test_high():\n    assert clamp(5, 3) == 3\n",
    );
    let summary = repo.scratch().join("summary.md");

    let output = repo.titer(&["check", "--json", "--markdown", summary.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(0));
    let text = std::fs::read_to_string(&summary).unwrap();
    assert!(text.contains("### titer: proven\n"), "{text}");
    assert!(
        text.contains("| fails (assertion) | test\\_high | test\\_new.py:4 |"),
        "{text}"
    );
}
