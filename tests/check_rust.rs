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

// The import of double does not resolve on base, so setting aside the test that calls
// double must also drop that name from the import.
const API: &str = r#"use demo::{clamp, double};

#[test]
fn clamps_through_the_api() {
    assert_eq!(clamp(9, 3), 3);
}

#[test]
fn doubles_through_the_api() {
    assert_eq!(double(3), 6);
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
            "tests/api.rs::doubles_through_the_api green missing_symbol",
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
    // A target dir in the checkout must not draw the build into the checkout.
    let target = repo.path().join("target");
    let config = format!(
        "[[runner]]\nkind = \"cargo\"\ncommand = [\"cargo\", \"test\"]\nenv = {{ CARGO_TARGET_DIR = \"{}\" }}\n\n[mutate]\nbudget = \"300s\"\n",
        target.display()
    );
    repo.write("titer.toml", &config);
    repo.git(&["commit", "-qam", "target dir"]);
    // Line 22 asserts inside a test; an agent may not mutate test code.
    let mutants = repo.scratch().join("mutants.jsonl");
    std::fs::write(
        &mutants,
        "{\"file\": \"src/lib.rs\", \"line\": 22, \"replacement\": \"        assert_eq!(clamp(5, 3), 4);\"}\n",
    )
    .unwrap();
    let before = repo.checkout_state();

    let output = repo.titer(&[
        "check",
        "--json",
        "--only",
        "mutate",
        "--workers",
        "2",
        "--mutants",
        mutants.to_str().unwrap(),
    ]);

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
    assert!(
        report["warnings"][0]
            .as_str()
            .unwrap()
            .contains("lines 22-22 of src/lib.rs hold test code"),
        "{report:#}"
    );
    assert!(!target.exists());
    assert_eq!(repo.checkout_state(), before);
}

const LIMITS_BASE: &str = "pub const LIMIT: i32 = 2;\n";
const LIMITS_HEAD: &str = r#"pub const LIMIT: i32 = 3;

#[cfg(test)]
mod tests {
    #[test]
    fn limit_is_positive() {
        assert!(super::LIMIT > 0);
    }
}
"#;
const MAIN_BASE: &str = "const _: () = assert!(demo::LIMIT == 2);\n\nfn main() {}\n";
const MAIN_HEAD: &str = r#"const _: () = assert!(demo::LIMIT == 3);

fn main() {}

#[cfg(test)]
mod tests {
    #[test]
    fn starts() {}
}
"#;

#[test]
fn a_mutant_that_breaks_one_selected_target_has_no_result() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        "[[runner]]\nkind = \"cargo\"\ncommand = [\"cargo\", \"test\"]\n\n[mutate]\nbudget = \"300s\"\n",
    )
    .write(".gitignore", "target\n")
    .write("Cargo.toml", CARGO_TOML)
    .write("src/lib.rs", LIMITS_BASE)
    .write("src/main.rs", MAIN_BASE);
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/lib.rs", LIMITS_HEAD)
        .write("src/main.rs", MAIN_HEAD);

    let output = repo.titer(&["check", "--json", "--only", "mutate", "--workers", "1"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(
        strings(
            &report,
            "mutants",
            &["file", "line", "replacement", "outcome"]
        ),
        vec!["src/lib.rs 1 4 no_result", "src/main.rs 1 4 no_result",],
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

const TABLE_BASE: &str = r#"pub fn clamp(x: i32, _hi: i32) -> i32 {
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[(i32, i32)] = &[
        (1, 1),
    ];

    #[test]
    fn clamps_each_case() {
        for (x, out) in CASES {
            assert_eq!(clamp(*x, 3), *out);
        }
    }

    #[test]
    fn keeps_zero() {
        assert_eq!(clamp(0, 3), 0);
    }
}
"#;

#[test]
fn a_new_row_in_a_test_table_makes_the_test_that_reads_it_changed() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        "[[runner]]\nkind = \"cargo\"\ncommand = [\"cargo\", \"test\"]\n",
    )
    .write(".gitignore", "target\n")
    .write("Cargo.toml", CARGO_TOML)
    .write("src/lib.rs", TABLE_BASE);
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write(
        "src/lib.rs",
        &TABLE_BASE
            .replacen("    x\n}", "    x.min(hi)\n}", 1)
            .replacen("_hi: i32", "hi: i32", 1)
            .replacen("        (1, 1),\n", "        (1, 1),\n        (5, 3),\n", 1),
    );

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_eq!(
        strings(&report, "tests", &["id", "head", "verdict"]),
        vec!["src/lib.rs::tests::clamps_each_case green assertion"],
        "{report:#}"
    );
}

const TABLE_API_BASE: &str = r#"pub fn clamp(x: i32, _hi: i32) -> i32 {
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[(i32, i32)] = &[
        (1, 1),
    ];

    #[test]
    fn rows() {
        for (x, out) in CASES {
            assert_eq!(*x, *out);
        }
    }
}
"#;

// Head fixes clamp, adds twice, puts a call to twice in the table, and adds a test of
// clamp. On base the table does not compile, which must not stop the test of clamp.
const TABLE_API_HEAD: &str = r#"pub fn clamp(x: i32, hi: i32) -> i32 {
    x.min(hi)
}

pub const fn twice(x: i32) -> i32 {
    x * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[(i32, i32)] = &[
        (1, 1),
        (twice(1), 2),
    ];

    #[test]
    fn rows() {
        for (x, out) in CASES {
            assert_eq!(*x, *out);
        }
    }

    #[test]
    fn independent() {
        assert_eq!(clamp(5, 3), 3);
    }
}
"#;

#[test]
fn a_table_that_does_not_compile_on_base_sets_aside_only_its_readers() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        "[[runner]]\nkind = \"cargo\"\ncommand = [\"cargo\", \"test\"]\n",
    )
    .write(".gitignore", "target\n")
    .write("Cargo.toml", CARGO_TOML)
    .write("src/lib.rs", TABLE_API_BASE);
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/lib.rs", TABLE_API_HEAD);

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_eq!(
        strings(&report, "tests", &["id", "head", "verdict"]),
        vec![
            "src/lib.rs::tests::rows green missing_symbol",
            "src/lib.rs::tests::independent green assertion",
        ],
        "{report:#}"
    );
    assert!(
        report["tests"][0]["message"]
            .as_str()
            .unwrap()
            .contains("twice"),
        "{report:#}"
    );
}
