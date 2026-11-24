mod common;

use common::{Fixture, assert_schema_valid, fixture_node_modules, stdout_json};

const CONFIG_TS: &str =
    "[[runner]]\nkind = \"vitest\"\ncommand = [\"node_modules/.bin/vitest\", \"run\"]\n";

const BASE: &str = r#"import { describe, it, expect } from "vitest";
import { add } from "./math";

describe("add", () => {
  it("adds", () => {
    expect(add(1, 2)).toBe(3);
    expect(add(2, 2)).toBe(4);
  });
  it("loosens", () => {
    expect(add(1, 1)).toEqual(2);
  });
  it("goes away", () => {
    expect(add(0, 0)).toBe(0);
  });
  it("skips", () => {
    expect(add(5, 5)).toBe(10);
  });
  it("untouched", () => {
    expect(add(3, 3)).toBe(6);
  });
});
"#;

const HEAD: &str = r#"import { describe, it, expect } from "vitest";
import { add } from "./math";

describe("add", () => {
  it("adds", () => {
    expect(add(1, 2)).toBe(3);
  });
  it("loosens", () => {
    expect(add(1, 1)).toBeTruthy();
  });
  it.skip("skips", () => {
    expect(add(5, 5)).toBe(10);
  });
  it("untouched", () => {
    expect(add(3, 3)).toBe(6);
  });
});
"#;

#[test]
fn scan_reports_each_weakened_test_and_nothing_for_unchanged_ones() {
    let repo = Fixture::new();
    repo.write("titer.toml", CONFIG_TS)
        .write(".gitignore", "node_modules\n")
        .write(
            "src/math.ts",
            "export const add = (a: number, b: number) => a + b;\n",
        )
        .write("src/math.test.ts", BASE)
        .write(
            "src/other.test.ts",
            "import { it, expect } from \"vitest\";\nit(\"x\", () => { expect(1).toBe(1); });\n",
        )
        .write(
            "src/__snapshots__/math.test.ts.snap",
            "exports[`a`] = `1`;\n",
        );
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("node_modules")).unwrap();
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "weaken"]);
    repo.write("src/math.test.ts", HEAD)
        .write("src/__snapshots__/math.test.ts.snap", "exports[`a`] = `2`;\n")
        .write("src/other.test.ts", "import { it, expect } from \"vitest\";\nit(\"x\", () => { expect(1).toBe(1); });\n// note\n");

    let output = repo.titer(&["check", "--json", "--only", "scan"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let findings: Vec<(String, String, String)> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["kind"].as_str().unwrap().to_string(),
                f["file"].as_str().unwrap().to_string(),
                f["test"].as_str().unwrap_or("-").to_string(),
            )
        })
        .collect();
    assert_eq!(
        findings,
        vec![
            (
                "snapshot_changed".into(),
                "src/__snapshots__/math.test.ts.snap".into(),
                "-".into()
            ),
            (
                "assertion_removed".into(),
                "src/math.test.ts".into(),
                "add > adds".into()
            ),
            (
                "matcher_loosened".into(),
                "src/math.test.ts".into(),
                "add > loosens".into()
            ),
            (
                "skip_added".into(),
                "src/math.test.ts".into(),
                "add > skips".into()
            ),
            (
                "test_deleted".into(),
                "src/math.test.ts".into(),
                "add > goes away".into()
            ),
        ]
    );
}

const PY_BASE: &str = r#"import unittest


class TestOffset(unittest.TestCase):
    def test_offset(self):
        self.assertEqual(f(1), 2)
        self.assertTrue(f(2))

    def test_quoted(self):
        self.assertEqual(f(1), 2)
        self.assertEqual(f(2), 3)
"#;

const PY_HEAD: &str = r#"import unittest


class TestOffset(unittest.TestCase):
    def test_offset(self):
        self.assertTrue(f(1))
        self.assertEqual(f(2), 3)

    def test_quoted(self):
        self.assertEqual(f(1), 2)
        text = "self.assertEqual(f(2), 3)"
"#;

#[test]
fn scan_compares_each_assertion_and_ignores_assertion_like_strings() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        "[[runner]]\nkind = \"pytest\"\ncommand = [\"python3\", \"-m\", \"pytest\"]\n",
    )
    .write("test_a.py", PY_BASE);
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "weaken"]);
    repo.write("test_a.py", PY_HEAD);

    let output = repo.titer(&["check", "--json", "--only", "scan"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    let findings: Vec<(String, String)> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["kind"].as_str().unwrap().to_string(),
                f["test"].as_str().unwrap_or("-").to_string(),
            )
        })
        .collect();
    assert_eq!(
        findings,
        vec![
            ("matcher_loosened".into(), "TestOffset > test_offset".into()),
            (
                "assertion_removed".into(),
                "TestOffset > test_quoted".into()
            ),
        ],
        "{report:#}"
    );
}

const DUP_BASE: &str = r#"import { describe, it, expect } from "vitest";

describe("dup", () => {
  it("same", () => {
    expect(1).toBe(1);
    expect(2).toBe(2);
  });
  it.skip("same", () => {
    expect(3).toBe(3);
  });
});
"#;

const DUP_HEAD: &str = r#"import { describe, it, expect } from "vitest";

describe("dup", () => {
  it.skip("same", () => {
    expect(3).toBe(3);
  });
  it("same", () => {
    expect(1).toBe(1);
    expect(2).toBe(2);
  });
});
"#;

#[test]
fn scan_reports_nothing_for_reordered_tests_that_share_a_title() {
    let repo = Fixture::new();
    repo.write("titer.toml", CONFIG_TS)
        .write(".gitignore", "node_modules\n")
        .write("src/dup.test.ts", DUP_BASE);
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("node_modules")).unwrap();
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "reorder"]);
    repo.write("src/dup.test.ts", DUP_HEAD);

    let output = repo.titer(&["check", "--json", "--only", "scan"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_eq!(report["findings"], serde_json::json!([]), "{report:#}");
}
