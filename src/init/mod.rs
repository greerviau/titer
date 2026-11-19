//! `titer init`: opt a repo in. It detects runners, writes `titer.toml`, pins titer as a dev
//! dependency, and writes the repo skill where Claude Code and Codex find it.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::config::{self, Config};
use crate::git::Git;

const SKILL: &str = include_str!("SKILL.md");
pub const SKILL_PATHS: [&str; 2] = [
    ".agents/skills/titer/SKILL.md",
    ".claude/skills/titer/SKILL.md",
];
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// npm refuses the unscoped name `titer` as too close to existing packages.
const NPM_PACKAGE: &str = "@greerviau/titer";

/// Dirs never searched for runners.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "dist",
    "build",
    "target",
    ".tox",
    ".nox",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "coverage",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedRunner {
    pub name: String,
    pub kind: &'static str,
    /// Repo-relative dir, `.` for the root.
    pub cwd: String,
    pub command: Vec<String>,
    pub start_dir: Option<String>,
}

/// Every file `init` writes, with its new contents.
#[derive(Debug, Default)]
pub struct Plan {
    pub writes: Vec<(PathBuf, String)>,
    pub notes: Vec<String>,
}

pub fn run(cwd: &Path, dry_run: bool, update: bool) -> anyhow::Result<Plan> {
    let git = Git::discover(cwd)?;
    let root = git.root().to_path_buf();
    let config_path = root.join(config::FILE_NAME);
    let mut plan = Plan::default();
    if update {
        if !config_path.is_file() {
            bail!("no {} here; run `titer init` first", config::FILE_NAME);
        }
        let config = Config::load(&root)?;
        let dirs: Vec<String> = config.runners.iter().map(|r| r.cwd.clone()).collect();
        pin_dependencies(&root, &dirs, &mut plan)?;
    } else {
        if config_path.is_file() {
            bail!(
                "{} already exists; run `titer init --update` to refresh the skill and the pinned version",
                config::FILE_NAME
            );
        }
        let runners = detect_runners(&root)?;
        if runners.is_empty() {
            bail!(
                "found no pytest, unittest, vitest, or cargo project; write titer.toml by hand (see docs/configuration.md)"
            );
        }
        let text = render_config(&runners);
        Config::parse(&text).context("the generated titer.toml does not parse")?;
        plan.writes.push((config_path, text));
        let dirs: Vec<String> = runners.iter().map(|r| r.cwd.clone()).collect();
        pin_dependencies(&root, &dirs, &mut plan)?;
    }
    let skill = SKILL
        .replace("{version}", VERSION)
        .replace("{run}", &run_commands(&root, &plan));
    for path in SKILL_PATHS {
        plan.writes.push((root.join(path), skill.clone()));
    }
    for (path, _) in &plan.writes {
        refuse_symlinks(&root, path)?;
    }
    if !dry_run {
        for (path, contents) in &plan.writes {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, contents)
                .with_context(|| format!("cannot write {}", path.display()))?;
        }
    }
    Ok(plan)
}

/// Fails when any dir on the way to `path` inside the repo is a symlink, so `init` never
/// writes into a directory shared with other repos or the user's own config.
fn refuse_symlinks(root: &Path, path: &Path) -> anyhow::Result<()> {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut current = root.to_path_buf();
    for part in rel.components() {
        current.push(part);
        if current.is_symlink() {
            bail!(
                "refusing to write {}: {} is a symlink, and titer writes only inside the repo",
                path.display(),
                current.display()
            );
        }
    }
    Ok(())
}

/// The skill's run instructions: one command for one check of the whole repo, from the
/// first manifest that pins titer. Every pin runs the same check, so listing each one only
/// invites an agent to run it several times.
fn run_commands(root: &Path, plan: &Plan) -> String {
    let pins: Vec<(&str, String)> = plan
        .writes
        .iter()
        .filter_map(|(path, _)| {
            let command = match path.file_name()?.to_str()? {
                "package.json" => "npx titer check --json",
                "pyproject.toml" => "uv run titer check --json",
                _ => return None,
            };
            Some((command, rel_dir(root, path.parent()?)))
        })
        .collect();
    let Some((command, dir)) = pins.first() else {
        return "- `titer check --json` from the repo root".to_string();
    };
    let mut text = match dir.as_str() {
        "." => format!("- `{command}` from the repo root"),
        d => format!("- `{command}` from `{d}/`, where titer is pinned"),
    };
    if pins.len() > 1 {
        let others: Vec<String> = pins[1..].iter().map(|(_, d)| format!("`{d}/`")).collect();
        text.push_str(&format!(
            "\n\ntiter is also pinned in {}. Each pin runs the same check of the whole repo, so run it once, from any one of them.",
            others.join(", ")
        ));
    } else if dir != "." {
        text.push_str(" (it still checks the whole repo)");
    }
    text
}

/// The titer version stamped in each copy of the repo skill; None for a missing copy.
pub fn skill_versions(root: &Path) -> Vec<Option<String>> {
    SKILL_PATHS
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(root.join(p)).ok()?;
            let start = text.find("<!-- titer-version: ")? + "<!-- titer-version: ".len();
            let end = text[start..].find(" -->")? + start;
            Some(text[start..end].to_string())
        })
        .collect()
}

/// The warning `check` adds when a copy of the skill is missing or was written by another
/// titer version. A repo with no skill at all gets no warning.
pub fn stamp_warning(root: &Path) -> Option<String> {
    let versions = skill_versions(root);
    if versions.iter().all(Option::is_none) {
        return None;
    }
    let stale: Vec<String> = SKILL_PATHS
        .iter()
        .zip(&versions)
        .filter(|(_, v)| v.as_deref() != Some(VERSION))
        .map(|(p, v)| match v {
            Some(v) => format!("{p} (titer {v})"),
            None => format!("{p} (missing)"),
        })
        .collect();
    (!stale.is_empty()).then(|| {
        format!(
            "the repo skill is out of date for titer {VERSION}: {}; run `titer init --update`",
            stale.join(", ")
        )
    })
}

pub fn detect_runners(root: &Path) -> anyhow::Result<Vec<DetectedRunner>> {
    let mut dirs = Vec::new();
    collect_dirs(root, root, 0, &mut dirs);
    let mut runners: Vec<DetectedRunner> = Vec::new();
    for dir in dirs {
        let rel = rel_dir(root, &dir);
        if let Some(r) = detect_vitest(&dir, &rel) {
            runners.push(r);
        }
        // One cargo runner covers a workspace and its members; dirs come parents first. A
        // package that no workspace above it lists gets its own runner.
        let member = runners
            .iter()
            .filter(|r| r.kind == "cargo")
            .any(|r| workspace_member(&root.join(&r.cwd), &r.cwd, &rel));
        if !member && let Some(r) = detect_cargo(&dir, &rel) {
            runners.push(r);
        }
        if let Some(r) = detect_python(&dir, &rel) {
            runners.push(r);
        }
    }
    if runners.len() > 1 {
        // Short names (the dir's last component) when they are unique, else the full path.
        let short: Vec<String> = runners
            .iter()
            .map(|r| {
                if r.cwd == "." {
                    "root".to_string()
                } else {
                    r.cwd.rsplit('/').next().unwrap_or(&r.cwd).to_string()
                }
            })
            .collect();
        let unique = short
            .iter()
            .enumerate()
            .all(|(i, n)| !short[..i].contains(n));
        for (r, name) in runners.iter_mut().zip(short) {
            r.name = if unique {
                name
            } else {
                let base = if r.cwd == "." {
                    "root".to_string()
                } else {
                    r.cwd.replace('/', "-")
                };
                format!("{base}-{}", r.kind)
            };
        }
    }
    Ok(runners)
}

fn collect_dirs(root: &Path, dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    out.push(dir.to_path_buf());
    if depth >= 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !SKIP_DIRS.contains(&name.as_ref()) && !name.starts_with('.')
        })
        .map(|e| e.path())
        .collect();
    subdirs.sort();
    for sub in subdirs {
        let _ = root;
        collect_dirs(root, &sub, depth + 1, out);
    }
}

fn rel_dir(root: &Path, dir: &Path) -> String {
    let rel = dir
        .strip_prefix(root)
        .unwrap_or(dir)
        .to_string_lossy()
        .into_owned();
    if rel.is_empty() { ".".to_string() } else { rel }
}

fn detect_vitest(dir: &Path, rel: &str) -> Option<DetectedRunner> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let has = |section: &str| {
        json[section]
            .as_object()
            .is_some_and(|m| m.contains_key("vitest"))
    };
    if !has("devDependencies") && !has("dependencies") {
        return None;
    }
    Some(DetectedRunner {
        name: "vitest".to_string(),
        kind: "vitest",
        cwd: rel.to_string(),
        command: vec![
            "npx".into(),
            "--no-install".into(),
            "vitest".into(),
            "run".into(),
        ],
        start_dir: None,
    })
}

/// Whether cargo counts the package at repo-relative `rel` as a member of the workspace
/// rooted at `workspace_dir` (repo-relative `workspace_rel`), listed or joined as a path
/// dependency. `cargo metadata` decides; it reads the manifests and writes nothing.
fn workspace_member(workspace_dir: &Path, workspace_rel: &str, rel: &str) -> bool {
    let inner = if workspace_rel == "." {
        Some(rel)
    } else {
        rel.strip_prefix(&format!("{workspace_rel}/"))
    };
    let Some(inner) = inner else {
        return false;
    };
    crate::runner::cargo::targets::index(workspace_dir)
        .members
        .iter()
        .any(|m| m == inner)
}

fn detect_cargo(dir: &Path, rel: &str) -> Option<DetectedRunner> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let manifest: toml::Table = text.parse().ok()?;
    if !manifest.contains_key("package") && !manifest.contains_key("workspace") {
        return None;
    }
    Some(DetectedRunner {
        name: "cargo".to_string(),
        kind: "cargo",
        cwd: rel.to_string(),
        command: vec!["cargo".into(), "test".into()],
        start_dir: None,
    })
}

fn detect_python(dir: &Path, rel: &str) -> Option<DetectedRunner> {
    let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap_or_default();
    let is_project =
        !pyproject.is_empty() || dir.join("setup.py").is_file() || dir.join("setup.cfg").is_file();
    if !is_project {
        return None;
    }
    let pytest = pyproject_uses_pytest(&pyproject)
        || dir.join("pytest.ini").is_file()
        || dir.join("conftest.py").is_file()
        || dir.join("tests/conftest.py").is_file()
        || std::fs::read_to_string(dir.join("setup.cfg"))
            .is_ok_and(|s| s.contains("[tool:pytest]"));
    let tests_dir = ["tests", "test"].into_iter().find(|t| dir.join(t).is_dir());
    let unittest = !pytest && tests_dir.is_some_and(|t| uses_unittest(&dir.join(t)));
    if !pytest && !unittest {
        return None;
    }
    let module = if pytest { "pytest" } else { "unittest" };
    let mut command: Vec<String> = if dir.join("uv.lock").is_file() {
        vec![
            "uv".into(),
            "run".into(),
            "--no-sync".into(),
            "python".into(),
        ]
    } else if dir.join(".venv/bin/python").exists() {
        vec![".venv/bin/python".into()]
    } else {
        vec!["python3".into()]
    };
    command.extend(["-m".to_string(), module.to_string()]);
    Some(DetectedRunner {
        name: module.to_string(),
        kind: module,
        cwd: rel.to_string(),
        command,
        start_dir: unittest.then(|| tests_dir.unwrap_or(".").to_string()),
    })
}

/// Whether pyproject.toml configures pytest or depends on it or a pytest plugin. Other
/// mentions, such as a `pytest` keyword, do not count.
fn pyproject_uses_pytest(text: &str) -> bool {
    let Ok(doc) = text.parse::<toml::Table>() else {
        return false;
    };
    let tool = doc.get("tool").and_then(|t| t.as_table());
    if tool.is_some_and(|t| t.contains_key("pytest")) {
        return true;
    }
    let mut lists: Vec<&toml::Value> = Vec::new();
    if let Some(project) = doc.get("project").and_then(|p| p.as_table()) {
        lists.extend(project.get("dependencies"));
        if let Some(extras) = project
            .get("optional-dependencies")
            .and_then(|e| e.as_table())
        {
            lists.extend(extras.values());
        }
    }
    if let Some(groups) = doc.get("dependency-groups").and_then(|g| g.as_table()) {
        lists.extend(groups.values());
    }
    if let Some(uv) = tool.and_then(|t| t.get("uv")).and_then(|u| u.as_table()) {
        lists.extend(uv.get("dev-dependencies"));
    }
    let requirement = lists
        .iter()
        .filter_map(|l| l.as_array())
        .flatten()
        .filter_map(|v| v.as_str())
        .map(|s| {
            s.split(['=', '<', '>', '~', '!', ' ', '[', ';'])
                .next()
                .unwrap_or_default()
        });
    // Poetry lists dependencies as table keys.
    let poetry = tool
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.as_table())
        .into_iter()
        .flat_map(|p| {
            let groups = p
                .get("group")
                .and_then(|g| g.as_table())
                .into_iter()
                .flat_map(|g| g.values())
                .filter_map(|g| g.get("dependencies"));
            ["dependencies", "dev-dependencies"]
                .into_iter()
                .filter_map(|k| p.get(k))
                .chain(groups)
                .filter_map(|d| d.as_table())
                .flat_map(|d| d.keys().map(String::as_str))
                .collect::<Vec<_>>()
        });
    requirement.chain(poetry).any(|name| {
        let name = normalize_name(name);
        name == "pytest" || name.starts_with("pytest-")
    })
}

fn uses_unittest(tests: &Path) -> bool {
    std::fs::read_dir(tests)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("test")
                && name.ends_with(".py")
                && std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("unittest"))
        })
}

pub fn render_config(runners: &[DetectedRunner]) -> String {
    let mut out = String::from(
        "# titer configuration. See https://github.com/greerviau/titer for every option.\n\n",
    );
    for r in runners {
        out.push_str("[[runner]]\n");
        if runners.len() > 1 {
            out.push_str(&format!("name = {}\n", quote(&r.name)));
        }
        out.push_str(&format!("kind = {}\n", quote(r.kind)));
        if r.cwd != "." {
            out.push_str(&format!("cwd = {}\n", quote(&r.cwd)));
        }
        let command: Vec<String> = r.command.iter().map(|c| quote(c)).collect();
        out.push_str(&format!("command = [{}]\n", command.join(", ")));
        if let Some(start) = &r.start_dir {
            out.push_str(&format!("start_dir = {}\n", quote(start)));
        }
        out.push('\n');
    }
    out.push_str("[gate]\n# fail_on_survivors = false\n# fail_on_passes_on_base = false\n# fail_on_scan = false\n# fail_on_crap = false\n");
    out
}

fn quote(text: &str) -> String {
    toml_edit::Value::from(text).to_string().trim().to_string()
}

/// Pins titer in the root manifest when there is one, otherwise in each runner dir's
/// manifest: package.json devDependencies, or pyproject.toml's dev dependency group.
fn pin_dependencies(root: &Path, dirs: &[String], plan: &mut Plan) -> anyhow::Result<()> {
    let root_manifest =
        root.join("package.json").is_file() || root.join("pyproject.toml").is_file();
    let mut targets: Vec<PathBuf> = if root_manifest {
        vec![root.to_path_buf()]
    } else {
        dirs.iter().map(|d| root.join(d)).collect()
    };
    let mut seen = Vec::new();
    targets.retain(|t| {
        let new = !seen.contains(t);
        seen.push(t.clone());
        new
    });
    for dir in targets {
        let package = dir.join("package.json");
        let pyproject = dir.join("pyproject.toml");
        if package.is_file() {
            let text = std::fs::read_to_string(&package)?;
            plan.writes.push((package.clone(), pin_npm(&text)?));
            plan.notes.push(format!(
                "run `npm install` in {} to install the pinned titer",
                rel_dir(root, &dir)
            ));
        } else if pyproject.is_file() {
            let text = std::fs::read_to_string(&pyproject)?;
            plan.writes.push((pyproject.clone(), pin_python(&text)?));
            plan.notes.push(format!(
                "run `uv lock` in {} to lock the pinned titer",
                rel_dir(root, &dir)
            ));
        }
    }
    Ok(())
}

pub fn pin_npm(text: &str) -> anyhow::Result<String> {
    let mut json: serde_json::Value =
        serde_json::from_str(text).context("package.json is not valid JSON")?;
    let object = json
        .as_object_mut()
        .context("package.json is not an object")?;
    let dev = object
        .entry("devDependencies")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    dev.as_object_mut()
        .context("devDependencies is not an object")?
        .insert(
            NPM_PACKAGE.to_string(),
            serde_json::Value::String(VERSION.to_string()),
        );
    let indent = detect_indent(text);
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(&json, &mut ser)?;
    Ok(format!("{}\n", String::from_utf8(out)?))
}

fn detect_indent(text: &str) -> String {
    text.lines()
        .find_map(|l| {
            let trimmed = l.trim_start();
            (trimmed.len() < l.len() && trimmed.starts_with('"'))
                .then(|| l[..l.len() - trimmed.len()].to_string())
        })
        .unwrap_or_else(|| "  ".to_string())
}

/// A package name after PEP 503 normalization, so Titer, titer, and ti_ter compare equal.
fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .split(['-', '_', '.'])
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn pin_python(text: &str) -> anyhow::Result<String> {
    let mut doc: toml_edit::DocumentMut =
        text.parse().context("pyproject.toml is not valid TOML")?;
    let groups = doc
        .entry("dependency-groups")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .context("dependency-groups is not a table")?;
    let dev = groups
        .entry("dev")
        .or_insert(toml_edit::Item::Value(toml_edit::Value::Array(
            toml_edit::Array::new(),
        )));
    let array = dev
        .as_array_mut()
        .context("dependency-groups.dev is not an array")?;
    array.retain(|v| {
        v.as_str().is_none_or(|s| {
            !s.split(['=', '<', '>', '~', '!', ' ', '[', ';'])
                .next()
                .is_some_and(|n| normalize_name(n) == "titer")
        })
    });
    array.push(format!("titer=={VERSION}"));
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_npm_dev_dependencies_and_keeps_key_order() {
        let text = "{\n    \"name\": \"web\",\n    \"devDependencies\": {\n        \"vitest\": \"^5.1.0\"\n    },\n    \"scripts\": {}\n}\n";
        let out = pin_npm(text).unwrap();
        assert_eq!(
            out,
            format!(
                "{{\n    \"name\": \"web\",\n    \"devDependencies\": {{\n        \"vitest\": \"^5.1.0\",\n        \"@greerviau/titer\": \"{VERSION}\"\n    }},\n    \"scripts\": {{}}\n}}\n"
            )
        );
    }

    #[test]
    fn pyproject_uses_pytest_reads_config_and_dependency_tables() {
        for yes in [
            "[tool.pytest.ini_options]\n",
            "[project]\ndependencies = [\"pytest>=8; python_version>'3.8'\"]\n",
            "[tool.uv]\ndev-dependencies = [\"pytest_xdist\"]\n",
            "[tool.poetry.dev-dependencies]\npytest = \"^8\"\n",
            "[tool.poetry.group.test.dependencies]\nPytest = \"^8\"\n",
        ] {
            assert!(pyproject_uses_pytest(yes), "{yes}");
        }
        for no in [
            "[project]\nkeywords = [\"pytest\"]\n",
            "[project]\ndependencies = [\"pytestish\"]\n",
            "[tool.poetry.dependencies]\npython = \"^3.12\"\n",
            "not toml [",
        ] {
            assert!(!pyproject_uses_pytest(no), "{no}");
        }
    }

    #[test]
    fn pins_python_dev_group_and_replaces_an_old_pin() {
        let text = "[project]\nname = \"api\"\n\n[dependency-groups]\ndev = [\"pytest>=8\", \"Titer==0.0.1\"]\n";
        let out = pin_python(text).unwrap();
        assert_eq!(
            out,
            format!(
                "[project]\nname = \"api\"\n\n[dependency-groups]\ndev = [\"pytest>=8\", \"titer=={VERSION}\"]\n"
            )
        );
        let added = pin_python("[project]\nname = \"api\"\n").unwrap();
        assert!(
            added.contains(&format!(
                "[dependency-groups]\ndev = [\"titer=={VERSION}\"]"
            )),
            "{added}"
        );
    }
}
