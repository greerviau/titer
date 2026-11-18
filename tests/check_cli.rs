mod common;

use common::{Fixture, assert_schema_valid, stdout_json};

const PYTEST_CONFIG: &str = r#"
[[runner]]
kind = "pytest"
command = ["python3", "-m", "pytest"]
"#;

#[test]
fn check_json_emits_a_schema_valid_empty_report() {
    let repo = Fixture::new();
    repo.write("titer.toml", PYTEST_CONFIG)
        .write("app.py", "def add(a, b):\n    return a + b\n");
    repo.commit_all("init");

    let output = repo.titer(&["check", "--json"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(report["gate"]["status"], "pass");
    assert_eq!(report["schema_version"], "2");
    assert_eq!(
        report["tiers"],
        serde_json::json!(["scan", "prove", "mutate", "crap"])
    );
}

#[test]
fn check_refuses_to_run_in_a_repo_that_has_not_opted_in() {
    let repo = Fixture::new();
    repo.write("app.py", "x = 1\n");
    repo.commit_all("init");

    let output = repo.titer(&["check", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("titer init"), "stderr: {stderr}");
    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(report["gate"]["status"], "error");
    assert_eq!(report["gate"]["exit_code"], 2);
}

#[test]
fn check_reports_config_errors_with_exit_2() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        "[[runner]]\nkind = \"jest\"\ncommand = [\"x\"]\n",
    );
    repo.commit_all("init");

    let output = repo.titer(&["check"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid titer.toml"), "stderr: {stderr}");
}

#[test]
fn only_restricts_the_tiers_in_the_report() {
    let repo = Fixture::new();
    repo.write("titer.toml", PYTEST_CONFIG);
    repo.commit_all("init");

    let output = repo.titer(&["check", "--json", "--only", "scan,prove"]);

    assert_eq!(
        stdout_json(&output)["tiers"],
        serde_json::json!(["scan", "prove"])
    );
}

#[test]
fn published_schema_matches_the_report_types() {
    let committed =
        std::fs::read_to_string(common::repo_root().join("schema/report.schema.json")).unwrap();
    let committed: serde_json::Value = serde_json::from_str(&committed).unwrap();
    assert_eq!(
        committed,
        titer::report::schema(),
        "schema/report.schema.json is stale; regenerate it with `cargo run -- schema > schema/report.schema.json`"
    );
}
