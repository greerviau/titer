mod common;

use common::{Fixture, assert_schema_valid, stdout_json};
use serde_json::Value;

const CARGO_TOML: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

const LIB_BASE: &str = r#"pub fn clamp(x: i32, _hi: i32) -> i32 {
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_low_values() {
        assert_eq!(clamp(1, 3), 1);
    }

    #[test]
    fn removed_later() {
        assert_eq!(clamp(0, 3), 0);
    }
}
"#;

// Head fixes clamp, adds double, and tests both in the same test module, so on base the
// module does not compile until the test of double is set aside.
const LIB_HEAD: &str = r#"pub fn clamp(x: i32, hi: i32) -> i32 {
    if x > hi {
        return hi;
    }
    x
}

pub fn double(x: i32) -> i32 {
    x * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_low_values() {
        assert_eq!(clamp(1, 3), 1);
    }

    #[test]
    fn clamps_high_values() {
        assert_eq!(clamp(5, 3), 3);
    }

    #[test]
    fn doubles() {
        assert_eq!(double(2), 4);
    }
}
"#;

const API: &str = r#"#[test]
fn clamps_through_the_api() {
    assert_eq!(demo::clamp(9, 3), 3);
}
"#;

fn repo() -> Fixture {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        "[[runner]]\nkind = \"cargo\"\ncommand = [\"cargo\", \"test\"]\n\n[mutate]\nbudget = \"300s\"\n",
    )
    .write(".gitignore", "target\n")
    .write("Cargo.toml", CARGO_TOML)
    .write("src/lib.rs", LIB_BASE);
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/lib.rs", LIB_HEAD)
        .write("tests/api.rs", API);
    repo
}

fn strings(report: &Value, key: &str, fields: &[&str]) -> Vec<String> {
    report[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            fields
                .iter()
                .map(|f| match &t[*f] {
                    Value::String(s) => s.clone(),
                    Value::Null => "-".to_string(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[test]
fn prove_splices_head_tests_into_base_and_sets_aside_tests_that_cannot_compile() {
    let repo = repo();
    let before = repo.checkout_state();

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_eq!(
        strings(&report, "tests", &["id", "head", "verdict"]),
        vec![
            "src/lib.rs::tests::clamps_high_values green assertion",
            "src/lib.rs::tests::doubles green missing_symbol",
            "tests/api.rs::clamps_through_the_api green assertion",
        ],
        "{report:#}"
    );
    let tests = report["tests"].as_array().unwrap();
    assert!(
        tests[0]["message"].as_str().unwrap().contains("right: 3"),
        "{report:#}"
    );
    assert!(
        tests[1]["message"]
            .as_str()
            .unwrap()
            .contains("cannot find function `double`"),
        "{report:#}"
    );
    assert_eq!(repo.checkout_state(), before);
    assert!(repo.leftover_tmp().is_empty());
}

#[test]
fn mutate_skips_test_code_and_treats_the_compile_error_canary_as_killed() {
    let repo = repo();

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--workers", "2"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let mut got = strings(
        &report,
        "mutants",
        &["line", "operator", "replacement", "outcome"],
    );
    got.sort();
    assert_eq!(
        got,
        vec![
            "2 negate_if !(x > hi) killed",
            "2 swap_comparison >= survived",
            "9 increment_int 3 killed",
            "9 swap_arithmetic / killed",
        ],
        "{report:#}"
    );
}

#[test]
fn scan_reads_tests_inside_rust_source_files() {
    let repo = repo();

    let output = repo.titer(&["check", "--json", "--only", "scan"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(
        strings(&report, "findings", &["kind", "file", "test"]),
        vec!["test_deleted src/lib.rs tests > removed_later"],
        "{report:#}"
    );
}
