//! Test helpers that `init` adds to a runner's `support_globs`: vitest setup files, and
//! files that only test paths import and that look like test code. Without them titer
//! mutates the helpers as source and leaves them off base.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use tree_sitter::Node;

use crate::config::{RunnerConfig, RunnerKind};
use crate::lang::{Lang, children, text};
use crate::runner::cwd_relative;

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

/// The runner's test helpers that its globs miss.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Helpers {
    /// Globs relative to the runner's cwd, each matching one helper.
    pub globs: Vec<String>,
    /// Repo-relative helpers whose path no glob can spell, such as one with a backslash.
    pub unwritable: Vec<String>,
}

/// Finds the runner's test helpers. `files` are the repo's files; `read` reads one by
/// repo-relative path. Every guess errs toward source: a file that a non-test file
/// anywhere in the repo imports, that a package exports, or that a test is named after
/// stays source, so titer keeps mutating it.
pub fn helpers(
    runner: &RunnerConfig,
    files: &HashSet<String>,
    read: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Helpers> {
    let wanted = |path: &str| match runner.kind {
        RunnerKind::Vitest => Lang::of(path).is_some_and(Lang::is_ts),
        RunnerKind::Pytest | RunnerKind::Unittest => Lang::of(path).is_some_and(Lang::is_python),
        // Rust helpers live in `tests/` or `#[cfg(test)]` items, which titer already knows.
        RunnerKind::Cargo => false,
    };
    let in_runner = |f: &str| runner.cwd == "." || f.starts_with(&format!("{}/", runner.cwd));
    let tests = crate::detect::glob_set(&runner.test_globs)?;
    let support = crate::detect::glob_set(&runner.support_globs)?;
    let lang_files: Vec<&String> = files.iter().filter(|f| wanted(f)).collect();
    let mut test_paths: BTreeSet<String> = lang_files
        .iter()
        .filter(|f| in_runner(f))
        .filter(|f| {
            let rel = cwd_relative(f, &runner.cwd);
            tests.is_match(&rel) || support.is_match(&rel)
        })
        .map(|f| f.to_string())
        .collect();
    let mut found: BTreeSet<String> = BTreeSet::new();
    if runner.kind == RunnerKind::Vitest {
        for path in setup_files(runner, files, read) {
            if in_runner(&path) && test_paths.insert(path.clone()) {
                found.insert(path);
            }
        }
    }

    let aliases = crate::imports::ts_aliases(&runner.cwd, read);
    let mut graph = Graph::default();
    for file in lang_files.iter().filter(|f| in_runner(f)) {
        graph.add(runner, file, read, files, &aliases);
    }
    // Imported files that read as test code. Whether only test paths reach them comes next.
    let entries = entry_points(runner, files, read);
    let mut candidates: BTreeSet<String> = graph
        .importers
        .iter()
        .filter(|(target, by)| {
            in_runner(target)
                && wanted(target)
                && !test_paths.contains(*target)
                && !entries.contains(*target)
                && looks_like_helper(runner.kind, target, graph.external.get(*target))
                && !by.iter().any(|t| {
                    tests.is_match(cwd_relative(t, &runner.cwd))
                        && is_named_after(runner.kind, t, target)
                })
        })
        .map(|(target, _)| target.clone())
        .collect();
    // An importer in another package also keeps a file source. Parsing every file in a big
    // monorepo is slow, so only files that mention a candidate's module name are parsed.
    let names: Vec<String> = candidates.iter().map(|p| module_name(p)).collect();
    if !names.is_empty() {
        for file in lang_files.iter().filter(|f| !in_runner(f)) {
            let Some(source) = read(file) else { continue };
            if names.iter().any(|n| source.contains(n.as_str())) {
                graph.add_source(runner, file, &source, files, &aliases);
            }
        }
    }
    // Drop every candidate that a file outside the test paths and the other candidates
    // imports, until none does. Helpers that import each other survive together.
    loop {
        let dropped: Vec<String> = candidates
            .iter()
            .filter(|c| {
                graph.importers[*c]
                    .iter()
                    .any(|i| !test_paths.contains(i) && !candidates.contains(i))
            })
            .cloned()
            .collect();
        if dropped.is_empty() {
            break;
        }
        for c in dropped {
            candidates.remove(&c);
        }
    }
    // Keep the candidates that a test path reaches through other candidates.
    let mut frontier: Vec<String> = test_paths.iter().cloned().collect();
    while let Some(file) = frontier.pop() {
        for target in graph.imports.get(&file).into_iter().flatten() {
            if candidates.remove(target) {
                found.insert(target.clone());
                frontier.push(target.clone());
            }
        }
    }

    let mut out = Helpers::default();
    for file in found {
        if file.contains('\\') {
            out.unwritable.push(file);
        } else {
            out.globs
                .push(globset::escape(&cwd_relative(&file, &runner.cwd)));
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Graph {
    imports: BTreeMap<String, Vec<String>>,
    importers: BTreeMap<String, Vec<String>>,
    external: BTreeMap<String, Vec<String>>,
}

impl Graph {
    fn add(
        &mut self,
        runner: &RunnerConfig,
        file: &str,
        read: &dyn Fn(&str) -> Option<String>,
        files: &HashSet<String>,
        aliases: &[(String, Vec<String>)],
    ) {
        if let Some(source) = read(file) {
            self.add_source(runner, file, &source, files, aliases);
        }
    }

    fn add_source(
        &mut self,
        runner: &RunnerConfig,
        file: &str,
        source: &str,
        files: &HashSet<String>,
        aliases: &[(String, Vec<String>)],
    ) {
        let imports = crate::imports::file_imports(runner, file, source, files, aliases);
        for target in &imports.local {
            if target != file {
                self.importers
                    .entry(target.clone())
                    .or_default()
                    .push(file.to_string());
            }
        }
        self.imports.insert(file.to_string(), imports.local);
        self.external.insert(file.to_string(), imports.external);
    }
}

/// The name other files import a module by: its stem, or its dir for an index file.
fn module_name(path: &str) -> String {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    match stem {
        "index" | "__init__" => parts.next().unwrap_or(stem).to_string(),
        _ => stem.to_string(),
    }
}

/// The local files that the runner's vitest config names in `setupFiles` or `globalSetup`,
/// resolved against the config's `root` the way vitest resolves them. Only the config
/// vitest loads counts, and only code: a commented-out entry is not a setup file.
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
    let mut configs: Vec<String> = Vec::new();
    match config_flag(&runner.command) {
        Some(path) => configs.push(normalize(&join(dir, &path))),
        None => {
            // vitest loads the first of these that exists.
            let default = ["vitest.config", "vite.config"].iter().find_map(|name| {
                CONFIG_EXTENSIONS
                    .iter()
                    .map(|e| join(dir, &format!("{name}.{e}")))
                    .find(|p| files.contains(p))
            });
            configs.extend(default);
        }
    }
    configs.extend(
        CONFIG_EXTENSIONS
            .iter()
            .map(|e| join(dir, &format!("vitest.workspace.{e}")))
            .filter(|p| files.contains(p)),
    );
    let mut out = Vec::new();
    for config in configs {
        let Some(source) = read(&config) else {
            continue;
        };
        let Some(tree) = Lang::of(&config).and_then(|l| l.parse(&source)) else {
            continue;
        };
        let config_dir = match config.rsplit_once('/') {
            Some((d, _)) => d.to_string(),
            None => String::new(),
        };
        let pairs = pairs(tree.root_node());
        let root = pairs
            .iter()
            .find(|(k, _)| key(*k, &source) == "root")
            .and_then(|(_, v)| string(*v, &source))
            .map_or(config_dir.clone(), |r| normalize(&join(&config_dir, &r)));
        for (k, value) in &pairs {
            if !matches!(key(*k, &source).as_str(), "setupFiles" | "globalSetup") {
                continue;
            }
            let values = if value.kind() == "array" {
                children(*value)
            } else {
                vec![*value]
            };
            for spec in values.into_iter().filter_map(|v| string(v, &source)) {
                if let Some(found) = resolve(&root, &spec, files)
                    && !out.contains(&found)
                {
                    out.push(found);
                }
            }
        }
    }
    out
}

/// The `--config`/`-c` path in a vitest command, relative to its cwd.
fn config_flag(command: &[String]) -> Option<String> {
    let mut args = command.iter();
    while let Some(arg) = args.next() {
        if arg == "--config" || arg == "-c" {
            return args.next().cloned();
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            return Some(path.to_string());
        }
    }
    None
}

fn pairs(root: Node) -> Vec<(Node, Node)> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "pair"
            && let (Some(k), Some(v)) = (
                node.child_by_field_name("key"),
                node.child_by_field_name("value"),
            )
        {
            out.push((k, v));
        }
        stack.extend(children(node).into_iter().rev());
    }
    out
}

fn key(node: Node, source: &str) -> String {
    string(node, source).unwrap_or_else(|| text(node, source).to_string())
}

/// A string literal's value; None for any other node, or a template with substitutions.
fn string(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "string" => Some(crate::lang::ts::title(node, source)),
        "template_string"
            if !children(node)
                .iter()
                .any(|c| c.kind() == "template_substitution") =>
        {
            Some(crate::lang::ts::title(node, source))
        }
        _ => None,
    }
}

/// A setup path as a repo file: relative to the vitest root, with or without its
/// extension. A leading `/` is root-relative, as in vite.
fn resolve(root: &str, spec: &str, files: &HashSet<String>) -> Option<String> {
    let rel = spec.strip_prefix('/').unwrap_or(spec);
    if rel.is_empty() {
        return None;
    }
    let base = normalize(&join(root, rel));
    std::iter::once(base.clone())
        .chain(CONFIG_EXTENSIONS.iter().map(|e| format!("{base}.{e}")))
        .chain(["tsx", "jsx"].iter().map(|e| format!("{base}.{e}")))
        .find(|c| files.contains(c))
}

/// Files a package publishes: package.json's `main`, `module`, `browser`, `bin`, and
/// `exports` targets for vitest, and pyproject.toml's `[project.scripts]` modules for
/// Python. Only tests import such a file inside the repo, yet it is source.
fn entry_points(
    runner: &RunnerConfig,
    files: &HashSet<String>,
    read: &dyn Fn(&str) -> Option<String>,
) -> HashSet<String> {
    let dir = if runner.cwd == "." {
        ""
    } else {
        runner.cwd.as_str()
    };
    let mut out = HashSet::new();
    match runner.kind {
        RunnerKind::Vitest => {
            let Some(json) = read(&join(dir, "package.json"))
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            else {
                return out;
            };
            let mut targets = Vec::new();
            for field in ["main", "module", "browser", "bin", "exports"] {
                strings(&json[field], &mut targets);
            }
            for target in targets {
                let base = normalize(&join(dir, target.strip_prefix("./").unwrap_or(&target)));
                let stem = base.rsplit_once('.').map_or(base.as_str(), |(s, _)| s);
                out.extend(
                    std::iter::once(base.clone())
                        .chain(CONFIG_EXTENSIONS.iter().map(|e| format!("{stem}.{e}")))
                        .chain(["tsx", "jsx"].iter().map(|e| format!("{stem}.{e}")))
                        .chain(
                            CONFIG_EXTENSIONS
                                .iter()
                                .map(|e| format!("{base}/index.{e}")),
                        )
                        .filter(|c| files.contains(c)),
                );
            }
        }
        RunnerKind::Pytest | RunnerKind::Unittest => {
            let Some(doc) =
                read(&join(dir, "pyproject.toml")).and_then(|t| t.parse::<toml::Table>().ok())
            else {
                return out;
            };
            let scripts = doc
                .get("project")
                .and_then(|p| p.get("scripts"))
                .and_then(|s| s.as_table());
            for target in scripts.into_iter().flat_map(|s| s.values()) {
                let Some(module) = target.as_str().and_then(|t| t.split(':').next()) else {
                    continue;
                };
                let rel = module.trim().replace('.', "/");
                for root in [dir.to_string(), join(dir, "src")] {
                    let base = join(&root, &rel);
                    out.extend(
                        [format!("{base}.py"), join(&base, "__init__.py")]
                            .into_iter()
                            .filter(|c| files.contains(c)),
                    );
                }
            }
        }
        RunnerKind::Cargo => {}
    }
    out
}

fn strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
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
    // `api.mock.ts` and `user.fixtures.ts` name the helper in a later segment.
    let segments: Vec<String> = file.split('.').map(str::to_ascii_lowercase).collect();
    imports_framework
        || segments[..segments.len().saturating_sub(1).max(1)]
            .iter()
            .any(|s| HELPER_NAMES.contains(&s.as_str()))
        || parts
            .iter()
            .any(|d| HELPER_NAMES.contains(&d.to_ascii_lowercase().as_str()))
}

/// Whether test file `test`, which imports `path`, is named after it, such as
/// `testing.test.ts` or `test_testing.py`. Then `path` is the code under test.
fn is_named_after(kind: RunnerKind, test: &str, path: &str) -> bool {
    let name = module_name(path);
    let file = test.rsplit('/').next().unwrap_or(test);
    match kind {
        RunnerKind::Vitest => file.split('.').next() == Some(name.as_str()),
        _ => {
            let stem = file.trim_end_matches(".py");
            stem == format!("test_{name}") || stem == format!("{name}_test")
        }
    }
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() || dir == "." {
        rest.to_string()
    } else if rest.is_empty() {
        dir.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// Resolves `.` and `..` segments.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// The scripts that run a dir's tests: package.json's `test` script and `scripts/test*.sh`,
/// in each runner's dir and every dir above it. The repo runs its suite through them, so any
/// env they set may be one the suite needs.
pub fn test_scripts(root: &Path, runner_dirs: &[String]) -> Vec<String> {
    let mut dirs: Vec<String> = vec![".".to_string()];
    for dir in runner_dirs.iter().filter(|d| *d != ".") {
        let mut prefix = String::new();
        for part in dir.split('/') {
            prefix = join(&prefix, part);
            if !dirs.contains(&prefix) {
                dirs.push(prefix.clone());
            }
        }
    }
    let mut out = Vec::new();
    for dir in &dirs {
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

    fn vitest(cwd: &str) -> RunnerConfig {
        crate::config::Config::parse(&format!(
            "[[runner]]\nkind = \"vitest\"\ncwd = \"{cwd}\"\ncommand = [\"npx\", \"vitest\", \"run\"]\n"
        ))
        .unwrap()
        .runners
        .remove(0)
    }

    fn repo(entries: &[(&str, &str)]) -> (HashSet<String>, BTreeMap<String, String>) {
        let map: BTreeMap<String, String> = entries
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect();
        (map.keys().cloned().collect(), map)
    }

    fn found(runner: &RunnerConfig, entries: &[(&str, &str)]) -> Vec<String> {
        let (files, map) = repo(entries);
        let read = |p: &str| map.get(p).cloned();
        helpers(runner, &files, &read).unwrap().globs
    }

    const USES_VI: &str = "import { vi } from \"vitest\";\nexport const spy = () => vi.fn();\n";

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
    fn setup_files_come_from_the_loaded_config_code_and_its_root() {
        let (files, map) = repo(&[
            (
                "web/vitest.config.ts",
                "export default {\n  root: './app',\n  test: {\n    // setupFiles: ['./money.ts'],\n    setupFiles: ['./setup.ts', \"dom\", \"@testing-library/jest-dom/vitest\"],\n    globalSetup: `/global`,\n  },\n};\n",
            ),
            (
                "web/vite.config.ts",
                "export default { test: { setupFiles: ['./app/unused.ts'] } };\n",
            ),
            ("web/app/setup.ts", ""),
            ("web/app/dom.js", ""),
            ("web/app/global.ts", ""),
            ("web/app/money.ts", ""),
            ("web/app/unused.ts", ""),
        ]);
        let read = |p: &str| map.get(p).cloned();
        assert_eq!(
            setup_files(&vitest("web"), &files, &read),
            ["web/app/setup.ts", "web/app/dom.js", "web/app/global.ts"]
        );
    }

    #[test]
    fn setup_files_follow_a_config_flag_in_the_command() {
        let mut runner = vitest("web");
        runner
            .command
            .extend(["--config".into(), "vitest.unit.ts".into()]);
        let (files, map) = repo(&[
            (
                "web/vitest.unit.ts",
                "export default { test: { setupFiles: 'unit.ts' } };\n",
            ),
            (
                "web/vitest.config.ts",
                "export default { test: { setupFiles: 'other.ts' } };\n",
            ),
            ("web/unit.ts", ""),
            ("web/other.ts", ""),
        ]);
        let read = |p: &str| map.get(p).cloned();
        assert_eq!(setup_files(&runner, &files, &read), ["web/unit.ts"]);
    }

    #[test]
    fn a_helper_that_any_non_test_file_in_the_repo_imports_stays_source() {
        let test = "import { spy } from \"./fixtures\";\n";
        // Another package's source imports it.
        assert_eq!(
            found(
                &vitest("apps/web"),
                &[
                    ("apps/web/src/fixtures.ts", USES_VI),
                    ("apps/web/src/a.test.ts", test),
                    (
                        "apps/worker/src/main.ts",
                        "import { spy } from \"../../web/src/fixtures\";\n"
                    ),
                ]
            ),
            Vec::<String>::new()
        );
        // CommonJS source requires it.
        assert_eq!(
            found(
                &vitest("."),
                &[
                    ("fixtures.cjs", "module.exports = require(\"vitest\");\n"),
                    ("server.cjs", "const f = require(\"./fixtures.cjs\");\n"),
                    ("a.test.js", "import f from \"./fixtures.cjs\";\n"),
                ]
            ),
            Vec::<String>::new()
        );
        // The package publishes it.
        assert_eq!(
            found(
                &vitest("."),
                &[
                    (
                        "package.json",
                        "{\"exports\": {\".\": \"./src/factories/index.js\"}}"
                    ),
                    ("src/factories/index.js", "export const make = () => 1;\n"),
                    (
                        "integration.test.js",
                        "import { make } from \"./src/factories/index.js\";\n"
                    ),
                ]
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_namespace_package_submodule_that_source_imports_stays_source() {
        let pytest = crate::config::Config::parse(
            "[[runner]]\nkind = \"pytest\"\ncommand = [\"python\", \"-m\", \"pytest\"]\n",
        )
        .unwrap()
        .runners
        .remove(0);
        let (files, map) = repo(&[
            ("api/fixtures.py", "import pytest\nprice = 1\n"),
            ("api/cli.py", "from api import fixtures\n"),
            ("tests/test_x.py", "from api.fixtures import price\n"),
        ]);
        let read = |p: &str| map.get(p).cloned();
        assert_eq!(helpers(&pytest, &files, &read).unwrap(), Helpers::default());

        // `import mocks.backend` runs `mocks/__init__.py` too.
        let (files, map) = repo(&[
            ("mocks/__init__.py", "import pytest\n"),
            ("mocks/backend.py", "def make():\n    return 1\n"),
            ("tests/test_x.py", "import mocks.backend\n"),
        ]);
        let read = |p: &str| map.get(p).cloned();
        assert_eq!(
            helpers(&pytest, &files, &read).unwrap().globs,
            ["mocks/__init__.py", "mocks/backend.py"]
        );
    }

    #[test]
    fn helpers_stay_inside_the_runner_and_cycles_and_names_resolve() {
        let found = found(
            &vitest("apps/web"),
            &[
                ("shared/mocks/other.ts", USES_VI),
                (
                    "apps/web/src/a.ts",
                    "import { b } from \"./b.fixture\";\nimport { vi } from \"vitest\";\nexport const a = 1;\n",
                ),
                (
                    "apps/web/src/b.fixture.ts",
                    "import { a } from \"./a\";\nexport const b = 2;\n",
                ),
                ("apps/web/src/a/testing.ts", USES_VI),
                ("apps/web/src/b/testing.ts", "export const t = 1;\n"),
                (
                    "apps/web/src/b/testing.test.ts",
                    "import { t } from \"./testing\";\n",
                ),
                ("apps/web/src/[web]/mock.ts", ""),
                (
                    "apps/web/src/x.test.ts",
                    "import \"../../../shared/mocks/other\";\nimport { a } from \"./a\";\nimport \"./a/testing\";\nimport \"./[web]/mock\";\n",
                ),
            ],
        );
        assert_eq!(
            found,
            [
                "src/[[]web[]]/mock.ts",
                "src/a.ts",
                "src/a/testing.ts",
                "src/b.fixture.ts"
            ]
        );
        let globs = crate::detect::glob_set(&[found[0].clone()]).unwrap();
        assert!(globs.is_match("src/[web]/mock.ts"));
    }

    #[test]
    fn test_scripts_include_dirs_between_the_root_and_a_runner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("apps/scripts")).unwrap();
        std::fs::create_dir_all(root.join("apps/web")).unwrap();
        std::fs::write(
            root.join("apps/package.json"),
            "{\"scripts\": {\"test\": \"X=1 vitest\"}}",
        )
        .unwrap();
        std::fs::write(root.join("apps/scripts/test-web.sh"), "").unwrap();
        assert_eq!(
            test_scripts(root, &["apps/web".to_string()]),
            [
                "`apps/package.json` test (`X=1 vitest`)",
                "`apps/scripts/test-web.sh`"
            ]
        );
    }
}
