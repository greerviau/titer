mod common;

use common::Fixture;
use titer::detect::detect;
use titer::runner::Runner;
use titer::session::Session;

const CONFIG: &str = r#"
[[runner]]
name = "api"
kind = "pytest"
cwd = "api"
command = ["python3", "-m", "pytest"]

[[runner]]
name = "web"
kind = "vitest"
cwd = "web"
command = ["npx", "vitest", "run"]
"#;

const PY_BASE: &str = r#"def keep(x):
    return x


def fix(x):
    return x + 1
"#;

const PY_HEAD: &str = r#"def keep(x):
    return x


def fix(x):
    if x < 0:
        return 0
    return x + 1
"#;

const PY_TEST_BASE: &str = r#"from app import fix, keep


def test_keep():
    assert keep(1) == 1


class TestFix:
    def test_positive(self):
        assert fix(1) == 2
"#;

const PY_TEST_HEAD: &str = r#"from app import fix, keep


def test_keep():
    assert keep(1) == 1


class TestFix:
    def test_positive(self):
        assert fix(1) == 2

    def test_negative(self):
        assert fix(-5) == 0
"#;

const TS_BASE: &str = "export function clamp(n: number): number {\n  return n;\n}\n";
const TS_HEAD: &str = "export function clamp(n: number): number {\n  return n > 10 ? 10 : n;\n}\n";

const TS_TEST: &str = r#"import { describe, it, expect } from "vitest";
import { clamp } from "./clamp";

describe("clamp", () => {
  it("keeps small numbers", () => {
    expect(clamp(1)).toBe(1);
  });
  it.skip("later", () => {
    expect(clamp(11)).toBe(10);
  });
});
"#;

#[test]
fn maps_changed_lines_to_tests_and_functions() {
    let repo = Fixture::new();
    repo.write("titer.toml", CONFIG)
        .write("api/app.py", PY_BASE)
        .write("api/tests/test_app.py", PY_TEST_BASE)
        .write("web/src/clamp.ts", TS_BASE)
        .write("README.md", "hi\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "feat"]);
    repo.write("api/app.py", PY_HEAD)
        .write("api/tests/test_app.py", PY_TEST_HEAD)
        .write("web/src/clamp.ts", TS_HEAD)
        .write("web/src/clamp.test.ts", TS_TEST)
        .write("README.md", "hello\n");

    let session = Session::open(repo.path(), None).unwrap();
    let runners: Vec<Runner> = session
        .config
        .runners
        .iter()
        .cloned()
        .map(Runner::new)
        .collect();
    let detection = detect(&session, &runners).unwrap();

    let tests: Vec<(String, String, u32, bool)> = detection
        .tests
        .iter()
        .map(|t| (t.runner.clone(), t.id.clone(), t.line, t.skip))
        .collect();
    assert_eq!(
        tests,
        vec![
            (
                "api".into(),
                "tests/test_app.py::TestFix::test_negative".into(),
                12,
                false
            ),
            ("web".into(), "src/clamp.test.ts:5".into(), 5, false),
            ("web".into(), "src/clamp.test.ts:8".into(), 8, true),
        ]
    );
    let functions: Vec<(String, String)> = detection
        .functions
        .iter()
        .map(|f| (f.file.clone(), f.function.name.clone()))
        .collect();
    assert_eq!(
        functions,
        vec![
            ("api/app.py".into(), "fix".into()),
            ("web/src/clamp.ts".into(), "clamp".into()),
        ]
    );
    let sources: Vec<(&str, Vec<u32>)> = detection
        .sources
        .iter()
        .map(|s| (s.file.as_str(), s.changed.iter().copied().collect()))
        .collect();
    assert_eq!(
        sources,
        vec![("api/app.py", vec![6, 7]), ("web/src/clamp.ts", vec![2])]
    );
    assert_eq!(detection.not_analyzed, vec!["README.md".to_string()]);
}
