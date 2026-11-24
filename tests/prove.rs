mod common;

use common::{Fixture, assert_schema_valid, fixture_node_modules, fixture_python, stdout_json};
use serde_json::Value;

const APP_BASE: &str = r#"def parse_tz(name):
    return name


def lookup(d):
    return d["exclude"]


def double(x):
    return x * 2


def total(xs):
    return sum(xs)
"#;

// Head fixes three bugs and adds a new function.
const APP_HEAD: &str = r#"def parse_tz(name):
    if name not in ("UTC", "EST"):
        raise ValueError("bad tz")
    return name


def lookup(d):
    return d.get("exclude", [])


def double(x):
    return x * 2


def total(xs):
    return sum(xs) if xs else 0


def fresh():
    return 1
"#;

const TESTS_HEAD: &str = r#"import pytest

from app import double, lookup, parse_tz, total


def test_rejects_bad_tz():
    with pytest.raises(ValueError):
        parse_tz("Mars/Base")


def test_missing_key_is_empty():
    assert lookup({}) == []


def test_double_still_works():
    assert double(2) == 4


@pytest.mark.skip(reason="needs postgres")
def test_needs_db():
    assert total([]) == 0
"#;

const NEW_TESTS: &str = r#"from app import fresh


def test_fresh():
    assert fresh() == 1
"#;

fn pytest_repo() -> Fixture {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"pytest\"\ncommand = [\"{}\", \"-m\", \"pytest\"]\n",
            fixture_python()
        ),
    )
    .write("app.py", APP_BASE)
    .write("uv.lock", "version = 1\n")
    .write(
        "test_app.py",
        "from app import double\n\n\ndef test_double():\n    assert double(1) == 2\n",
    );
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("app.py", APP_HEAD)
        .write("test_app.py", TESTS_HEAD)
        .write("test_fresh.py", NEW_TESTS);
    repo
}

fn verdicts(report: &Value) -> Vec<(String, String, String)> {
    report["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["head"].as_str().unwrap().to_string(),
                t["verdict"].as_str().unwrap_or("-").to_string(),
            )
        })
        .collect()
}

#[test]
fn prove_classifies_each_changed_pytest_test() {
    let repo = pytest_repo();
    let before = repo.checkout_state();

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    assert_eq!(
        verdicts(&report),
        vec![
            (
                "test_rejects_bad_tz".into(),
                "green".into(),
                "assertion".into()
            ),
            (
                "test_missing_key_is_empty".into(),
                "green".into(),
                "exception".into()
            ),
            (
                "test_double_still_works".into(),
                "green".into(),
                "passes_on_base".into()
            ),
            ("test_needs_db".into(), "skipped".into(), "skipped".into()),
            ("test_fresh".into(), "green".into(), "missing_symbol".into()),
        ]
    );
    let tests = report["tests"].as_array().unwrap();
    assert_eq!(tests[0]["exception_type"], "Failed");
    assert_eq!(tests[1]["exception_type"], "KeyError");
    assert_eq!(tests[1]["message"], "KeyError: 'exclude'");
    assert!(
        tests[4]["message"].as_str().unwrap().contains("fresh"),
        "{report:#}"
    );
    assert_eq!(repo.checkout_state(), before);
    assert!(repo.leftover_tmp().is_empty());
}

#[test]
fn a_changed_test_that_fails_on_head_exits_2() {
    let repo = pytest_repo();
    repo.write("test_red.py", "def test_red():\n    assert 1 == 2\n");

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    assert_eq!(output.status.code(), Some(2));
    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(report["gate"]["status"], "error");
    assert!(
        report["gate"]["reasons"][0]
            .as_str()
            .unwrap()
            .contains("test_red.py::test_red")
    );
    let red: Vec<&Value> = report["tests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["head"] == "red")
        .collect();
    assert_eq!(red.len(), 1);
    assert_eq!(red[0]["exception_type"], "AssertionError");
}

const STORE_BASE: &str = "export function read(key: string): string | null {\n  return null;\n}\n";
const STORE_HEAD: &str = "export function read(key: string): string | null {\n  return key === \"a\" ? \"1\" : null;\n}\n\nexport function readBool(key: string): boolean {\n  return read(key) === \"1\";\n}\n";

// readBool is missing on base and arrives as undefined; vitest does not type-check, so only
// the static import check keeps "missing export" from looking like proof.
const STORE_TEST: &str = r#"import { describe, it, expect } from "vitest";
import { read, readBool } from "./store";

describe("store", () => {
  it("reads a", () => {
    expect(read("a")).toBe("1");
  });
  it("reads bools", () => {
    expect(readBool("a")).toBe(true);
  });
});
"#;

#[test]
fn prove_catches_a_missing_export_before_the_base_run() {
    let repo = Fixture::new();
    let vitest = fixture_node_modules().join(".bin/vitest");
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"vitest\"\ncommand = [\"{}\", \"run\"]\n",
            vitest.display()
        ),
    )
    .write(".gitignore", "node_modules\n")
    .write("package.json", "{\"type\": \"module\"}\n")
    .write("src/store.ts", STORE_BASE);
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("node_modules")).unwrap();
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/store.ts", STORE_HEAD)
        .write("src/store.test.ts", STORE_TEST);

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert_eq!(
        verdicts(&report),
        vec![
            ("store > reads a".into(), "green".into(), "assertion".into()),
            (
                "store > reads bools".into(),
                "green".into(),
                "missing_symbol".into()
            ),
        ]
    );
}

#[test]
fn a_changed_test_in_a_mixin_runs_in_each_test_case_that_inherits_it() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"unittest\"\ncommand = [\"{}\", \"-m\", \"unittest\"]\nstart_dir = \"tests\"\n",
            fixture_python()
        ),
    )
    .write("app.py", "def double(x):\n    return x\n")
    .write(
        "tests/test_app.py",
        "import unittest\n\nfrom app import double\n\n\nclass Checks:\n    def test_double(self):\n        pass\n\n\nclass FastTest(Checks, unittest.TestCase):\n    pass\n\n\nclass Slow(Checks, unittest.TestCase):\n    pass\n\n\nclass Other(unittest.TestCase):\n    def test_other(self):\n        pass\n",
    );
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("app.py", "def double(x):\n    return x * 2\n").write(
        "tests/test_app.py",
        "import unittest\n\nfrom app import double\n\n\nclass Checks:\n    def test_double(self):\n        self.assertEqual(double(self.n), 2 * self.n)\n\n\nclass FastTest(Checks, unittest.TestCase):\n    n = 2\n\n\nclass Slow(Checks, unittest.TestCase):\n    n = 0\n\n\nclass Other(unittest.TestCase):\n    def test_other(self):\n        pass\n",
    );

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0), "{report:#}");
    let mut tests: Vec<(String, String)> = report["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["id"].as_str().unwrap().to_string(),
                t["verdict"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    tests.sort();
    assert_eq!(
        tests,
        vec![
            // Each class's own result: double(0) is 0 on base too.
            ("test_app.FastTest.test_double".into(), "assertion".into()),
            ("test_app.Slow.test_double".into(), "passes_on_base".into()),
        ],
        "{report:#}"
    );
}

#[test]
fn two_vitest_tests_on_one_line_are_reported_as_sharing_a_selector() {
    let repo = Fixture::new();
    let vitest = fixture_node_modules().join(".bin/vitest");
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"vitest\"\ncommand = [\"{}\", \"run\"]\n",
            vitest.display()
        ),
    )
    .write(".gitignore", "node_modules\n")
    .write("package.json", "{\"type\": \"module\"}\n")
    .write("src/f.ts", "export const f = () => 1;\nexport const g = () => 1;\n")
    .write(
        "src/f.test.ts",
        "import { it, expect } from \"vitest\";\nimport { f, g } from \"./f\";\n\nit(\"a\", () => { expect(f()).toBe(1); }); it(\"b\", () => { expect(g()).toBe(1); });\n",
    );
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("node_modules")).unwrap();
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/f.ts", "export const f = () => 1;\nexport const g = () => 2;\n")
        .write(
            "src/f.test.ts",
            "import { it, expect } from \"vitest\";\nimport { f, g } from \"./f\";\n\nit(\"a\", () => { expect(f()).toBe(1); }); it(\"b\", () => { expect(g()).toBe(2); });\n",
        );

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    let warnings: Vec<&str> = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("src/f.test.ts:4") && w.contains("\"a\"") && w.contains("\"b\"")),
        "{report:#}"
    );
}

const API_BASE: &str = r#"export const api = {
  get(key: string): string {
    return key;
  },
};

export class Store {
  static open(): Store {
    return new Store();
  }
  read(): number {
    return 0;
  }
}

export const limits = { max: 1 };
"#;

// Head adds a method to an exported object, a static and an instance method to an exported
// class, a property to an exported object, and a new export reached through a namespace.
const API_HEAD: &str = r#"export const api = {
  get(key: string): string {
    return key.toUpperCase();
  },
  post(key: string): string {
    return `posted ${key}`;
  },
};

export class Store {
  static open(): Store {
    return new Store();
  }
  static create(): Store {
    return new Store();
  }
  read(): number {
    return 1;
  }
  size(): number {
    return 2;
  }
}

export const limits = { max: 1, min: 0 };

export function ping(): number {
  return 1;
}
"#;

const API_TEST: &str = r#"import { describe, it, expect } from "vitest";
import { api, Store, limits } from "./api";
import * as mod from "./api";

describe("api", () => {
  it("gets", () => {
    expect(api.get("a")).toBe("A");
  });
  it("posts", () => {
    expect(api.post("a")).toBe("posted a");
  });
  it("reads", () => {
    expect(Store.open().read()).toBe(1);
  });
  it("creates", () => {
    expect(Store.create()).toBeInstanceOf(Store);
  });
  it("sizes", () => {
    const store = new Store();
    expect(store.size()).toBe(2);
  });
  it("has a min", () => {
    expect(limits.min).toBe(0);
  });
  it("pings", () => {
    expect(mod.ping()).toBe(1);
  });
});
"#;

#[test]
fn prove_catches_a_new_member_on_an_imported_object() {
    let repo = Fixture::new();
    let vitest = fixture_node_modules().join(".bin/vitest");
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"vitest\"\ncommand = [\"{}\", \"run\"]\n",
            vitest.display()
        ),
    )
    .write(".gitignore", "node_modules\n")
    .write("package.json", "{\"type\": \"module\"}\n")
    .write("src/api.ts", API_BASE);
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("node_modules")).unwrap();
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "fix"]);
    repo.write("src/api.ts", API_HEAD)
        .write("src/api.test.ts", API_TEST);

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    let missing = |name: &str| {
        (
            format!("api > {name}"),
            "green".into(),
            "missing_symbol".into(),
        )
    };
    assert_eq!(
        verdicts(&report),
        vec![
            ("api > gets".into(), "green".into(), "assertion".into()),
            missing("posts"),
            ("api > reads".into(), "green".into(), "assertion".into()),
            missing("creates"),
            missing("sizes"),
            missing("has a min"),
            missing("pings"),
        ],
        "{report:#}"
    );
    let posts = report["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "api > posts")
        .unwrap();
    assert_eq!(
        posts["message"],
        "uses api.post from ./api, which base does not define"
    );
}

fn warnings(report: &Value) -> Vec<String> {
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_changed_lockfile_warns_that_base_runs_with_heads_dependencies() {
    let repo = pytest_repo();
    repo.write("uv.lock", "version = 2\n");

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert_schema_valid(&report);
    assert!(
        warnings(&report)
            .iter()
            .any(|w| w.contains("uv.lock") && w.contains("base runs with head's dependencies")),
        "{report:#}"
    );
}

#[test]
fn an_untouched_lockfile_gives_no_dependency_warning() {
    let repo = pytest_repo();

    let output = repo.titer(&["check", "--json", "--only", "prove"]);

    let report = stdout_json(&output);
    assert!(
        !warnings(&report)
            .iter()
            .any(|w| w.contains("head's dependencies")),
        "{report:#}"
    );
}
