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
    assert_eq!(
        package,
        format!(
            "{{\n  \"name\": \"web\",\n  \"type\": \"module\",\n  \"devDependencies\": {{\n    \"@greerviau/titer\": \"{VERSION}\",\n    \"vitest\": \"5.1.0\"\n  }}\n}}\n"
        ),
        "the pin goes in sorted order"
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
    assert!(run.contains("hard deadline"), "{run}");
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
        )
        // A pytest keyword in a unittest project still leaves it a unittest project.
        .write(
            "svc/pyproject.toml",
            "[project]\nname = \"svc\"\nkeywords = [\"pytest\"]\n",
        )
        .write(
            "svc/tests/test_svc.py",
            "import unittest\n\n\nclass T(unittest.TestCase):\n    def test_x(self):\n        pass\n",
        );
    repo.commit_all("base");

    let output = repo.titer(&["init", "--dry-run"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    // A pytest plugin such as pytest-cov needs pytest, so it counts too.
    assert_eq!(stdout.matches("kind = \"pytest\"").count(), 2, "{stdout}");
    assert!(stdout.contains("cwd = \"api\""), "{stdout}");
    assert!(stdout.contains("cwd = \"lib\""), "{stdout}");
    assert_eq!(stdout.matches("kind = \"unittest\"").count(), 1, "{stdout}");
    assert!(stdout.contains("cwd = \"svc\""), "{stdout}");
    assert!(stdout.contains("kind = \"cargo\""), "{stdout}");
}

#[test]
fn init_excludes_tool_configs_so_a_config_change_still_checks() {
    let repo = Fixture::new();
    repo.write(".gitignore", "node_modules\n")
        .write("clients/web/package.json", "{\n  \"name\": \"web\",\n  \"type\": \"module\",\n  \"devDependencies\": {\n    \"vitest\": \"5.1.0\"\n  }\n}\n")
        .write("clients/web/vite.config.ts", "import { defineConfig } from \"vitest/config\";\n\nexport default defineConfig({\n  define: { HEADER: JSON.stringify(\"1\") },\n});\n")
        .write("clients/web/scripts/build.mjs", "console.log(\"build\");\n")
        .write("clients/web/src/half.ts", "export const half = (x: number) => x / 2;\n")
        .write("clients/web/src/half.test.ts", "import { it, expect } from \"vitest\";\nimport { half } from \"./half\";\n\nit(\"halves\", () => {\n  expect(half(4)).toBe(2);\n});\n");
    std::os::unix::fs::symlink(
        fixture_node_modules(),
        repo.path().join("clients/web/node_modules"),
    )
    .unwrap();
    repo.commit_all("base");

    let init = repo.titer(&["init"]);

    assert_eq!(
        init.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let config = std::fs::read_to_string(repo.path().join("titer.toml")).unwrap();
    assert!(
        config.contains("[mutate]\n") && config.contains("exclude_globs = ["),
        "{config}"
    );
    assert!(config.contains("\"clients/web/{vite,vitest,"), "{config}");
    let stdout = String::from_utf8_lossy(&init.stdout);
    assert!(stdout.contains("\"clients/web/scripts/**\""), "{stdout}");

    repo.commit_all("opt in to titer");
    repo.git(&["checkout", "-q", "-b", "change"]);
    repo.write("clients/web/vite.config.ts", "import { defineConfig } from \"vitest/config\";\n\nexport default defineConfig({\n  define: { HEADER: JSON.stringify(2 + 3) },\n});\n");
    // A change to the config alone makes no mutants and still checks.
    let config_only = repo.titer(&["check", "--json", "--only", "mutate"]);
    let report = stdout_json(&config_only);
    assert_eq!(config_only.status.code(), Some(0), "{report:#}");
    assert_eq!(report["mutants"], serde_json::json!([]), "{report:#}");

    repo.write(
        "clients/web/src/half.ts",
        "export const half = (x: number) => x * 0.5;\n",
    );
    let check = repo.titer(&["check", "--json", "--only", "mutate"]);
    let report = stdout_json(&check);
    assert_ne!(report["gate"]["status"], "error", "{report:#}");
    let files: Vec<&str> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["file"].as_str().unwrap())
        .collect();
    assert!(!files.is_empty(), "{report:#}");
    assert!(
        files.iter().all(|f| *f == "clients/web/src/half.ts"),
        "{report:#}"
    );
}

#[test]
fn init_writes_the_skill_once_through_an_in_repo_skills_symlink() {
    let repo = monorepo();
    std::fs::create_dir_all(repo.path().join(".claude/skills")).unwrap();
    std::fs::create_dir_all(repo.path().join(".agents")).unwrap();
    std::os::unix::fs::symlink("../.claude/skills", repo.path().join(".agents/skills")).unwrap();

    let dry = repo.titer(&["init", "--dry-run"]);
    let output = repo.titer(&["init"]);

    assert_eq!(
        dry.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let stdout = String::from_utf8_lossy(&dry.stdout);
    assert_eq!(stdout.matches("SKILL.md").count(), 1, "{stdout}");
    assert!(stdout.contains(".claude/skills/titer/SKILL.md"), "{stdout}");
    assert_eq!(output.status.code(), Some(0));
    assert!(repo.path().join(".agents/skills").is_symlink());
    let text = std::fs::read_to_string(repo.path().join(".agents/skills/titer/SKILL.md")).unwrap();
    assert!(text.contains(&format!("<!-- titer-version: {VERSION} -->")));
}

#[test]
fn init_refuses_a_dangling_skill_link_through_an_outside_symlink_and_into_git() {
    let repo = monorepo();
    let outside = repo.scratch().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    // `hop` leads outside; `.agents/skills` dangles through it.
    std::os::unix::fs::symlink(&outside, repo.path().join("hop")).unwrap();
    std::fs::create_dir_all(repo.path().join(".agents")).unwrap();
    std::os::unix::fs::symlink("../hop/missing", repo.path().join(".agents/skills")).unwrap();

    let escaped = repo.titer(&["init"]);

    assert_eq!(escaped.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&escaped.stderr).contains("symlink"));
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());

    std::fs::remove_file(repo.path().join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink("../.git/hooks", repo.path().join(".agents/skills")).unwrap();

    let into_git = repo.titer(&["init"]);

    assert_eq!(into_git.status.code(), Some(2));
    assert!(!repo.path().join(".git/hooks/titer").exists());
    assert!(!repo.path().join("titer.toml").exists());
}

#[test]
fn init_refuses_skill_links_into_a_separate_git_dir_through_a_file_or_onto_a_dir() {
    // Git's files live in `git-data` inside the repo, not in `.git`.
    let repo = monorepo();
    let git_data = repo.path().join("git-data");
    std::fs::rename(repo.path().join(".git"), &git_data).unwrap();
    std::fs::write(repo.path().join(".git"), "gitdir: git-data\n").unwrap();
    std::fs::write(repo.path().join("notes.md"), "keep\n").unwrap();
    std::fs::write(repo.path().join("blocker"), "a file\n").unwrap();
    let skill_link = |target: &str| {
        let dir = repo.path().join(".agents/skills/titer");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("SKILL.md");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(target, &link).unwrap();
    };
    let head = std::fs::read_to_string(git_data.join("HEAD")).unwrap();

    for target in [
        "../../../git-data/HEAD",
        "../../../blocker/../notes.md",
        "../../..",
    ] {
        skill_link(target);

        let output = repo.titer(&["init"]);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{target}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!repo.path().join("titer.toml").exists(), "{target}");
    }
    assert_eq!(
        std::fs::read_to_string(git_data.join("HEAD")).unwrap(),
        head
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("notes.md")).unwrap(),
        "keep\n"
    );
}

#[test]
fn init_refuses_dir_links_into_git_files_and_hardlinked_skills() {
    let repo = monorepo();
    let git_data = repo.path().join("git-data");
    std::fs::rename(repo.path().join(".git"), &git_data).unwrap();
    std::fs::write(repo.path().join(".git"), "gitdir: git-data\n").unwrap();
    std::fs::create_dir_all(repo.path().join("sub/.git")).unwrap();
    std::fs::create_dir_all(repo.path().join(".agents")).unwrap();
    let link = repo.path().join(".agents/skills");

    for target in ["../git-data", "../sub/.git"] {
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(target, &link).unwrap();

        let output = repo.titer(&["init"]);

        assert_eq!(output.status.code(), Some(2), "{target}");
        assert!(!repo.path().join("titer.toml").exists(), "{target}");
    }
    assert!(!git_data.join("titer").exists());
    assert!(!repo.path().join("sub/.git/titer").exists());

    // A skill hardlinked to a file outside the repo is not overwritten.
    std::fs::remove_file(&link).unwrap();
    let outside = repo.scratch().join("shared-skill.md");
    std::fs::write(&outside, "shared\n").unwrap();
    std::fs::create_dir_all(repo.path().join(".agents/skills/titer")).unwrap();
    std::fs::hard_link(&outside, repo.path().join(".agents/skills/titer/SKILL.md")).unwrap();

    let output = repo.titer(&["init"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "shared\n");
}

#[test]
fn init_refuses_a_skill_destination_that_is_not_a_regular_file() {
    let repo = monorepo();
    std::fs::create_dir_all(repo.path().join(".claude/skills/titer")).unwrap();
    let fifo = repo.path().join(".claude/skills/titer/SKILL.md");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());

    let output = repo.titer(&["init"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a regular file"));
    assert!(!repo.path().join("titer.toml").exists());
}

#[test]
fn init_refuses_a_file_on_the_skill_path_and_a_git_dir_at_the_skill_dir() {
    let repo = monorepo();
    std::fs::create_dir_all(repo.path().join(".claude")).unwrap();
    std::fs::write(repo.path().join(".claude/skills"), "a file\n").unwrap();

    let dry = repo.titer(&["init", "--dry-run"]);
    let real = repo.titer(&["init"]);

    assert_eq!(dry.status.code(), Some(2));
    assert_eq!(real.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&real.stderr).contains("not a directory"));
    assert!(!repo.path().join("titer.toml").exists());

    // Git's own dir moved to where the skill goes.
    std::fs::remove_file(repo.path().join(".claude/skills")).unwrap();
    std::fs::rename(repo.path().join(".git"), repo.path().join(".claude/skills")).unwrap();
    std::fs::write(repo.path().join(".git"), "gitdir: .claude/skills\n").unwrap();

    let into_git = repo.titer(&["init"]);

    assert_eq!(into_git.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&into_git.stderr).contains("git's files"));
    assert!(!repo.path().join(".claude/skills/titer").exists());
}
