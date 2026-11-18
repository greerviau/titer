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
