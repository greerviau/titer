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
        package.contains(&format!("\"@greerviau/titer\": \"{VERSION}\"")),
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
    // An old skill: another version stamp, and text that predates `--only scan,prove`.
    let old = std::fs::read_to_string(&skill)
        .unwrap()
        .replace(VERSION, "0.0.1")
        .replace("`--only scan,prove`", "nothing");
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
        assert!(text.contains("`--only scan,prove`"), "{path}");
    }
}

#[test]
fn init_never_writes_through_a_symlinked_skill_dir() {
    let repo = monorepo();
    let shared = repo.scratch().join("shared-skills");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(repo.path().join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(&shared, repo.path().join(".agents/skills/titer")).unwrap();

    let output = repo.titer(&["init"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("symlink"));
    assert!(std::fs::read_dir(&shared).unwrap().next().is_none());
    assert!(!repo.path().join("titer.toml").exists());
}

#[test]
fn runner_names_stay_unique_and_the_skill_runs_from_the_pinned_dir() {
    let repo = Fixture::new();
    repo.write(
        "api/app/pyproject.toml",
        "[project]\nname = \"a\"\nversion = \"0.1.0\"\n\n[tool.pytest.ini_options]\n",
    )
    .write(
        "web/app/package.json",
        "{\n  \"devDependencies\": {\n    \"vitest\": \"5.1.0\"\n  }\n}\n",
    )
    .write(
        "web/app/src/x.test.ts",
        "import { it } from \"vitest\";\nit(\"x\", () => {});\n",
    );
    repo.commit_all("base");

    let output = repo.titer(&["init"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = std::fs::read_to_string(repo.path().join("titer.toml")).unwrap();
    assert!(
        config.contains("name = \"api-app-pytest\"")
            && config.contains("name = \"web-app-vitest\""),
        "{config}"
    );
    let skill = std::fs::read_to_string(repo.path().join(".claude/skills/titer/SKILL.md")).unwrap();
    let run = skill_section(&skill, "## Run");
    assert_eq!(run.matches("titer check --json").count(), 1, "{run}");
    assert!(
        run.contains("`uv run titer check --json` from `api/app/`"),
        "{run}"
    );
    assert!(run.contains("`web/app/`"), "{run}");
    assert!(run.contains("same check"), "{run}");
    let description = skill.lines().nth(2).unwrap();
    assert!(!description.contains("titer check"), "{description}");
}

/// The text of one `## ` section of the skill, without the next section.
fn skill_section<'a>(skill: &'a str, heading: &str) -> &'a str {
    let start = skill
        .find(heading)
        .unwrap_or_else(|| panic!("no {heading}: {skill}"));
    let rest = &skill[start + heading.len()..];
    &rest[..rest.find("\n## ").unwrap_or(rest.len())]
}

#[test]
fn the_skill_says_what_budget_limits_and_when_crap_has_scores() {
    let repo = monorepo();

    assert_eq!(repo.titer(&["init"]).status.code(), Some(0));

    let skill = std::fs::read_to_string(repo.path().join(".claude/skills/titer/SKILL.md")).unwrap();
    let run = skill_section(&skill, "## Run");
    assert!(run.contains("`--budget"), "{run}");
    assert!(run.contains("limits mutation only"), "{run}");
    assert!(run.contains("from the start of the check"), "{run}");
    assert!(run.contains("`--only scan,prove`"), "{run}");
    assert!(!run.contains("makes it shorter"), "{run}");
    let report = skill_section(&skill, "## Read the report");
    assert!(report.contains("`--coverage"), "{report}");
    assert!(report.contains("`coverage_file`"), "{report}");
    assert!(report.contains("complexity only"), "{report}");
}

#[test]
fn init_detects_a_cargo_workspace_once_at_its_root() {
    let repo = Fixture::new();
    repo.write(".gitignore", "target\n")
        .write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n")
        .write(
            "crates/calc/Cargo.toml",
            "[package]\nname = \"calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .write(
            "crates/calc/src/lib.rs",
            "pub fn one() -> i32 {\n    1\n}\n",
        );
    repo.commit_all("base");

    let output = repo.titer(&["init", "--dry-run"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("[[runner]]").count(), 1, "{stdout}");
    assert!(stdout.contains("kind = \"cargo\""), "{stdout}");
    assert!(
        stdout.contains("command = [\"cargo\", \"test\"]"),
        "{stdout}"
    );
    assert!(!stdout.contains("cwd ="), "{stdout}");
}

#[test]
fn init_gives_a_nested_crate_outside_the_workspace_its_own_runner() {
    let repo = Fixture::new();
    repo.write(".gitignore", "target\n")
        .write(
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nhelper = { path = \"helper\" }\n\n[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n",
        )
        .write("src/lib.rs", "")
        // A path dependency joins the workspace without a members entry.
        .write(
            "helper/Cargo.toml",
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .write("helper/src/lib.rs", "")
        .write(
            "crates/member/Cargo.toml",
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .write("crates/member/src/lib.rs", "")
        .write(
            "crates/skip/Cargo.toml",
            "[package]\nname = \"skip\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .write("crates/skip/src/lib.rs", "")
        .write(
            "fuzz/Cargo.toml",
            "[package]\nname = \"fuzz\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .write("fuzz/src/lib.rs", "");
    repo.commit_all("base");

    let output = repo.titer(&["init", "--dry-run"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert_eq!(stdout.matches("kind = \"cargo\"").count(), 3, "{stdout}");
    assert!(stdout.contains("cwd = \"crates/skip\""), "{stdout}");
    assert!(stdout.contains("cwd = \"fuzz\""), "{stdout}");
    assert!(!stdout.contains("cwd = \"crates/member\""), "{stdout}");
    assert!(!stdout.contains("cwd = \"helper\""), "{stdout}");
}

#[test]
fn update_pins_each_manifest_once_whatever_the_runner_order() {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        "[[runner]]\nname = \"a\"\nkind = \"pytest\"\ncwd = \"api\"\ncommand = [\"x\"]\n\n[[runner]]\nname = \"w\"\nkind = \"vitest\"\ncwd = \"web\"\ncommand = [\"x\"]\n\n[[runner]]\nname = \"b\"\nkind = \"unittest\"\ncwd = \"api\"\ncommand = [\"x\"]\n",
    )
    .write("api/pyproject.toml", "[project]\nname = \"api\"\n")
    .write("web/package.json", "{\n  \"name\": \"web\"\n}\n");
    repo.commit_all("base");

    let output = repo.titer(&["init", "--update", "--dry-run"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert_eq!(stdout.matches("api/pyproject.toml").count(), 1, "{stdout}");
}

#[test]
fn init_detects_pytest_from_dependencies_and_not_from_keywords() {
    let repo = Fixture::new();
    repo.write(".gitignore", "target\n")
        .write(
            "Cargo.toml",
            "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .write("src/lib.rs", "")
        // A Rust tool packaged for PyPI that mentions pytest only as a keyword.
        .write(
            "pyproject.toml",
            "[project]\nname = \"tool\"\nkeywords = [\"testing\", \"pytest\"]\ndescription = \"works with \\\"pytest\\\"\"\n",
        )
        .write(
            "api/pyproject.toml",
            "[project]\nname = \"api\"\n\n[dependency-groups]\ndev = [\"PyTest>=8\"]\n",
        )
        .write(
            "lib/pyproject.toml",
            "[project]\nname = \"lib\"\n\n[project.optional-dependencies]\ntest = [\"pytest-cov\"]\n",
        );
    repo.commit_all("base");

    let output = repo.titer(&["init", "--dry-run"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    // A pytest plugin such as pytest-cov needs pytest, so it counts too.
    assert_eq!(stdout.matches("kind = \"pytest\"").count(), 2, "{stdout}");
    assert!(stdout.contains("cwd = \"api\""), "{stdout}");
    assert!(stdout.contains("cwd = \"lib\""), "{stdout}");
    assert!(stdout.contains("kind = \"cargo\""), "{stdout}");
}
