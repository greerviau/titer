//! Test helpers that `init` adds to a runner's `support_globs`: vitest setup files, and
//! files that only test paths import and that look like test code. Without them titer
//! mutates the helpers as source and leaves them off base.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::{RunnerConfig, RunnerKind};
use crate::lang::Lang;
use crate::runner::cwd_relative;

/// vitest configs in a runner's dir that can set `setupFiles` or `globalSetup`.
const VITEST_CONFIGS: &[&str] = &["vitest.config", "vite.config", "vitest.workspace"];
const CONFIG_EXTENSIONS: &[&str] = &["ts", "mts", "cts", "js", "mjs", "cjs"];

/// Packages a test helper imports and source code does not.
const TS_TEST_PACKAGES: &[&str] = &["vitest", "@vitest/", "@testing-library/", "msw", "jest"];
const PY_TEST_MODULES: &[&str] = &["pytest", "_pytest", "unittest", "mock", "hypothesis"];

/// File stems and dir names that conventionally hold test helpers.
const HELPER_NAMES: &[&str] = &[
    "testing",
    "testutils",
    "test_utils",
    "test-utils",
    "testhelpers",
    "test_helpers",
    "test-helpers",
    "test_support",
    "test-support",
    "mock",
    "mocks",
    "fixture",
    "fixtures",
    "factories",
    "fakes",
    "stubs",
];

/// Repo-relative paths, relative to the runner's cwd, of the runner's test helpers that its
/// globs miss. `files` are the repo's files; `read` reads one by repo-relative path.
pub fn helpers(
    runner: &RunnerConfig,
    files: &HashSet<String>,
    read: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Vec<String>> {
    let wanted = |path: &str| match runner.kind {
        RunnerKind::Vitest => Lang::of(path).is_some_and(Lang::is_ts),
        RunnerKind::Pytest | RunnerKind::Unittest => Lang::of(path).is_some_and(Lang::is_python),
        // Rust helpers live in `tests/` or `#[cfg(test)]` items, which titer already knows.
        RunnerKind::Cargo => false,
    };
    let tests = crate::detect::glob_set(&runner.test_globs)?;
    let support = crate::detect::glob_set(&runner.support_globs)?;
    let in_runner: Vec<&String> = files
        .iter()
        .filter(|f| runner.cwd == "." || f.starts_with(&format!("{}/", runner.cwd)))
        .filter(|f| wanted(f))
        .collect();
    let is_test_file = |f: &str| tests.is_match(cwd_relative(f, &runner.cwd));
    let mut test_paths: BTreeSet<String> = in_runner
        .iter()
        .filter(|f| {
            let rel = cwd_relative(f, &runner.cwd);
            tests.is_match(&rel) || support.is_match(&rel)
        })
        .map(|f| f.to_string())
        .collect();
    let mut found: BTreeSet<String> = BTreeSet::new();
    if runner.kind == RunnerKind::Vitest {
        for path in setup_files(runner, files, read) {
            if test_paths.insert(path.clone()) {
                found.insert(path);
            }
        }
    }

    let aliases = crate::imports::ts_aliases(&runner.cwd, read);
    let mut importers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut external: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in &in_runner {
        let Some(source) = read(file) else { continue };
        let imports = crate::imports::file_imports(runner, file, &source, files, &aliases);
        for target in imports.local {
            if target != **file {
                importers.entry(target).or_default().push(file.to_string());
            }
        }
        external.insert(file.to_string(), imports.external);
    }
    // A helper can import another helper, so repeat until nothing new joins the test paths.
    loop {
        let new: Vec<String> = importers
            .iter()
            .filter(|(target, _)| !test_paths.contains(*target) && wanted(target))
            .filter(|(_, by)| by.iter().all(|f| test_paths.contains(f)))
            .filter(|(target, _)| {
                looks_like_helper(runner.kind, target, external.get(*target))
                    && !is_subject(
                        runner.kind,
                        target,
                        in_runner.iter().copied(),
                        &is_test_file,
                    )
            })
            .map(|(target, _)| target.clone())
            .collect();
        if new.is_empty() {
            break;
        }
        for path in new {
            test_paths.insert(path.clone());
            found.insert(path);
        }
    }
    Ok(found
        .into_iter()
        .map(|f| cwd_relative(&f, &runner.cwd))
        .filter(|f| !super::has_glob_syntax(f))
        .collect())
}

static SETUP_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b(?:setupFiles|globalSetup)\s*:\s*(\[[^\]]*\]|"[^"]*"|'[^']*'|`[^`]*`)"#)
        .unwrap()
});
static STRING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""([^"]*)"|'([^']*)'|`([^`]*)`"#).unwrap());

/// The local files that the runner's vitest config names in `setupFiles` or `globalSetup`.
/// vitest resolves them against the config's dir, which is the runner's cwd.
fn setup_files(
    runner: &RunnerConfig,
    files: &HashSet<String>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let dir = if runner.cwd == "." {
        ""
    } else {
        runner.cwd.as_str()
    };
    let mut out = Vec::new();
    for name in VITEST_CONFIGS {
        for ext in CONFIG_EXTENSIONS {
            let path = join(dir, &format!("{name}.{ext}"));
            let Some(text) = read(&path) else { continue };
            for value in SETUP_KEY.captures_iter(&text) {
                for literal in STRING.captures_iter(&value[1]) {
                    let spec = (1..=3)
                        .find_map(|i| literal.get(i))
                        .map_or("", |m| m.as_str());
                    if let Some(found) = resolve(dir, spec, files)
                        && !out.contains(&found)
                    {
                        out.push(found);
                    }
                }
            }
        }
    }
    out
}

/// A setup path as a repo file: relative to the config dir, with or without its extension.
fn resolve(dir: &str, spec: &str, files: &HashSet<String>) -> Option<String> {
    let rel = spec.strip_prefix("./").unwrap_or(spec);
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with("..") {
        return None;
    }
    let base = join(dir, rel);
    std::iter::once(base.clone())
        .chain(CONFIG_EXTENSIONS.iter().map(|e| format!("{base}.{e}")))
        .chain(["tsx", "jsx"].iter().map(|e| format!("{base}.{e}")))
        .find(|c| files.contains(c))
}

/// Whether a file reads as test code: it imports a test framework or a testing library, or
/// its name or a dir on its path is a conventional helper name. A module that only tests
/// import but that looks like neither is a public entry point, which titer should mutate.
fn looks_like_helper(kind: RunnerKind, path: &str, external: Option<&Vec<String>>) -> bool {
    let imports_framework = external.into_iter().flatten().any(|spec| match kind {
        RunnerKind::Vitest => TS_TEST_PACKAGES.iter().any(|p| match p.strip_suffix('/') {
            Some(scope) => spec.starts_with(scope) && spec[scope.len()..].starts_with('/'),
            None => spec == *p || spec.starts_with(&format!("{p}/")),
        }),
        _ => {
            let top = spec.split('.').next().unwrap_or(spec);
            PY_TEST_MODULES.contains(&top)
        }
    });
    let mut parts: Vec<&str> = path.split('/').collect();
    let file = parts.pop().unwrap_or_default();
    let stem = file.split('.').next().unwrap_or(file).to_ascii_lowercase();
    // `api.mock.ts` and `user.fixtures.ts` name the helper in a middle segment.
    let segments: Vec<String> = file.split('.').map(str::to_ascii_lowercase).collect();
    imports_framework
        || HELPER_NAMES.contains(&stem.as_str())
        || segments[1..]
            .iter()
            .any(|s| HELPER_NAMES.contains(&s.as_str()))
        || parts
            .iter()
            .any(|d| HELPER_NAMES.contains(&d.to_ascii_lowercase().as_str()))
}

/// Whether a test file is named after `path`, such as `testing.test.ts` or
/// `test_testing.py`, so `path` is the code under test.
fn is_subject<'a>(
    kind: RunnerKind,
    path: &str,
    files: impl Iterator<Item = &'a String>,
    is_test_file: &dyn Fn(&str) -> bool,
) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name);
    files.filter(|f| is_test_file(f)).any(|f| {
        let test = f.rsplit('/').next().unwrap_or(f);
        match kind {
            RunnerKind::Vitest => {
                test.split('.').next() == Some(stem)
                    && (test.contains(".test.") || test.contains(".spec."))
            }
            _ => {
                let test = test.trim_end_matches(".py");
                test == format!("test_{stem}") || test == format!("{stem}_test")
            }
        }
    })
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// The scripts that run a dir's tests: package.json's `test` script and `scripts/test*.sh`.
/// The repo runs its suite through them, so any env they set may be one the suite needs.
pub fn test_scripts(root: &Path, dirs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for dir in dirs {
        if seen.contains(dir) {
            continue;
        }
        seen.push(dir.clone());
        let base = if dir == "." {
            root.to_path_buf()
        } else {
            root.join(dir)
        };
        let prefix = if dir == "." {
            String::new()
        } else {
            format!("{dir}/")
        };
        if let Ok(text) = std::fs::read_to_string(base.join("package.json"))
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(test) = json["scripts"]["test"].as_str()
        {
            out.push(format!("`{prefix}package.json` test (`{test}`)"));
        }
        let mut scripts: Vec<String> = std::fs::read_dir(base.join("scripts"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("test") && n.ends_with(".sh"))
            .collect();
        scripts.sort();
        out.extend(
            scripts
                .into_iter()
                .map(|n| format!("`{prefix}scripts/{n}`")),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_names_match_stems_middle_segments_and_dirs() {
        for (path, yes) in [
            ("src/testing.ts", true),
            ("src/mocks/handlers.ts", true),
            ("src/api.mock.ts", true),
            ("src/user.fixtures.ts", true),
            ("src/half.ts", false),
            ("src/testingLibrary.ts", false),
            ("src/main.ts", false),
        ] {
            assert_eq!(
                looks_like_helper(RunnerKind::Vitest, path, None),
                yes,
                "{path}"
            );
        }
        let vitest = vec!["vitest".to_string()];
        assert!(looks_like_helper(
            RunnerKind::Vitest,
            "src/spy.ts",
            Some(&vitest)
        ));
        let lib = vec!["@testing-library/react".to_string()];
        assert!(looks_like_helper(
            RunnerKind::Vitest,
            "src/render.tsx",
            Some(&lib)
        ));
        let other = vec!["vitesse".to_string(), "jestful".to_string()];
        assert!(!looks_like_helper(
            RunnerKind::Vitest,
            "src/x.ts",
            Some(&other)
        ));
        let py = vec!["unittest.mock".to_string()];
        assert!(looks_like_helper(
            RunnerKind::Pytest,
            "pkg/fakes_.py",
            Some(&py)
        ));
        let pyother = vec!["pytesting".to_string()];
        assert!(!looks_like_helper(
            RunnerKind::Pytest,
            "pkg/x.py",
            Some(&pyother)
        ));
    }

    #[test]
    fn setup_files_resolve_lists_strings_and_extensionless_paths() {
        let runner = crate::config::Config::parse(
            "[[runner]]\nkind = \"vitest\"\ncwd = \"web\"\ncommand = [\"npx\"]\n",
        )
        .unwrap()
        .runners
        .remove(0);
        let files: HashSet<String> = [
            "web/vitest.config.ts",
            "web/src/setup.ts",
            "web/src/global.ts",
            "web/src/dom.js",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let read = |p: &str| {
            (p == "web/vitest.config.ts").then(|| {
                "export default {\n  test: {\n    setupFiles: ['./src/setup.ts', \"src/dom\", \"@testing-library/jest-dom/vitest\"],\n    globalSetup: `./src/global`,\n  },\n};\n".to_string()
            })
        };
        assert_eq!(
            setup_files(&runner, &files, &read),
            ["web/src/setup.ts", "web/src/dom.js", "web/src/global.ts"]
        );
    }
}
