mod common;

use common::{Fixture, fixture_node_modules, fixture_python, stdout_json};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A monorepo with a pytest API and a vitest web app, each with its dependencies linked in.
fn monorepo() -> Fixture {
    let repo = Fixture::new();
    repo.write(".gitignore", "node_modules\n.venv\n")
        .write("api/pyproject.toml", "[project]\nname = \"api\"\nversion = \"0.1.0\"\n\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n")
        .write("api/app.py", "def double(x):\n    return x * 2\n")
        .write("api/tests/test_app.py", "from app import double\n\n\ndef test_double():\n    assert double(2) == 4\n")
        .write("web/package.json", "{\n  \"name\": \"web\",\n  \"type\": \"module\",\n  \"devDependencies\": {\n    \"vitest\": \"5.1.0\"\n  }\n}\n")
        .write("web/src/half.ts", "export const half = (x: number) => x / 2;\n")
        .write("web/src/half.test.ts", "import { it, expect } from \"vitest\";\nimport { half } from \"./half\";\n\nit(\"halves\", () => {\n  expect(half(4)).toBe(2);\n});\n");
    let python = std::path::Path::new(&fixture_python())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(python, repo.path().join("api/.venv")).unwrap();
    std::os::unix::fs::symlink(fixture_node_modules(), repo.path().join("web/node_modules"))
        .unwrap();
    repo.commit_all("base");
    repo
}

#[test]
fn dry_run_prints_the_plan_and_writes_nothing() {
    let repo = monorepo();
    let before = repo.checkout_state();

    let output = repo.titer(&["init", "--dry-run"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("would write"), "{stdout}");
    assert!(stdout.contains("kind = \"pytest\""), "{stdout}");
    assert!(stdout.contains("kind = \"vitest\""), "{stdout}");
    assert_eq!(repo.checkout_state(), before);
    assert!(!repo.path().join("titer.toml").exists());
}

#[test]
fn init_writes_a_config_that_check_runs_with_and_the_skill_for_both_harnesses() {
    let repo = monorepo();

    let output = repo.titer(&["init"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = std::fs::read_to_string(repo.path().join("titer.toml")).unwrap();
    assert!(
        config.contains("name = \"api\"") && config.contains("cwd = \"api\""),
        "{config}"
    );
    assert!(
        config.contains("command = [\".venv/bin/python\", \"-m\", \"pytest\"]"),
        "{config}"
    );
    assert!(
        config.contains("command = [\"npx\", \"--no-install\", \"vitest\", \"run\"]"),
        "{config}"
    );
    let pyproject = std::fs::read_to_string(repo.path().join("api/pyproject.toml")).unwrap();
    assert!(
        pyproject.contains(&format!("dev = [\"titer=={VERSION}\"]")),
        "{pyproject}"
    );
    let package = std::fs::read_to_string(repo.path().join("web/package.json")).unwrap();
    assert!(
        package.contains(&format!("\"titer\": \"{VERSION}\"")),
        "{package}"
    );
    for skill in [
        ".agents/skills/titer/SKILL.md",
        ".claude/skills/titer/SKILL.md",
    ] {
        let text = std::fs::read_to_string(repo.path().join(skill)).unwrap();
        assert!(text.starts_with("---\nname: titer\n"), "{skill}");
        assert!(
            text.contains(&format!("<!-- titer-version: {VERSION} -->")),
            "{skill}"
        );
    }

    // The developer commits the result; then check runs on a change.
    repo.commit_all("opt in to titer");
    repo.git(&["checkout", "-q", "-b", "change"]);
    repo.write("api/app.py", "def double(x):\n    return x + x\n")
        .write("api/tests/test_app.py", "from app import double\n\n\ndef test_double():\n    assert double(2) == 4\n\n\ndef test_double_three():\n    assert double(3) == 6\n")
        .write("web/src/half.ts", "export const half = (x: number) => x * 0.5;\n");
    let check = repo.titer(&["check", "--json"]);
    let report = stdout_json(&check);
    assert_ne!(report["gate"]["status"], "error", "{report:#}");
    let runners: Vec<&str> = report["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["runner"].as_str().unwrap())
        .collect();
    assert_eq!(runners, vec!["api"], "{report:#}");
    assert!(
        report["mutants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["runner"] == "web"),
        "{report:#}"
    );
    assert!(
        report["warnings"].as_array().unwrap().is_empty(),
        "{report:#}"
    );
}

#[test]
fn a_second_init_refuses_and_update_restamps_the_skill() {
    let repo = monorepo();
    assert_eq!(repo.titer(&["init"]).status.code(), Some(0));
    let skill = repo.path().join(".claude/skills/titer/SKILL.md");
    let old = std::fs::read_to_string(&skill)
        .unwrap()
        .replace(VERSION, "0.0.1");
    std::fs::write(&skill, old).unwrap();
    std::fs::remove_file(repo.path().join(".agents/skills/titer/SKILL.md")).unwrap();
    repo.commit_all("init");

    let again = repo.titer(&["init"]);
    let warned = repo.titer(&["check", "--json", "--only", "scan"]);
    let update = repo.titer(&["init", "--update"]);

    assert_eq!(again.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&again.stderr).contains("init --update"));
    assert!(
        stdout_json(&warned)["warnings"][0]
            .as_str()
            .unwrap()
            .contains("titer 0.0.1")
    );
    assert_eq!(update.status.code(), Some(0));
    for path in [
        ".agents/skills/titer/SKILL.md",
        ".claude/skills/titer/SKILL.md",
    ] {
        let text = std::fs::read_to_string(repo.path().join(path)).unwrap();
        assert!(
            text.contains(&format!("<!-- titer-version: {VERSION} -->")),
            "{path}"
        );
    }
}
