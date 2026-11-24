//! Changed-test and source-function detection: which files, tests, and functions a change
//! touches.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use globset::{GlobSet, GlobSetBuilder};

use crate::config::{RunnerConfig, RunnerKind};
use crate::diff::FileStatus;
use crate::lang::{self, FunctionDef, Lang, PythonStyle};
use crate::runner::{Runner, TestCase, cwd_relative};
use crate::session::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Matches a runner's `test_globs`.
    Test { runner: usize },
    /// Matches a runner's `support_globs`.
    Support { runner: usize },
    /// A supported language under a runner that runs it.
    Source { runner: usize, lang: Lang },
    /// Any other file.
    NotAnalyzed,
}

/// Assigns repo-relative paths to runners and roles.
pub struct Classifier {
    runners: Vec<(RunnerConfig, GlobSet, GlobSet)>,
    /// The checkout, when the classifier may read it to learn which files cargo compiles
    /// as tests.
    root: Option<PathBuf>,
}

pub fn glob_set(globs: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        builder.add(
            globset::GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid glob {glob:?}"))?,
        );
    }
    Ok(builder.build()?)
}

fn runs_lang(kind: RunnerKind, lang: Lang) -> bool {
    match kind {
        RunnerKind::Pytest | RunnerKind::Unittest => lang.is_python(),
        RunnerKind::Vitest => lang.is_ts(),
        RunnerKind::Cargo => lang == Lang::Rust,
    }
}

fn under(path: &str, cwd: &str) -> bool {
    cwd == "." || path.starts_with(&format!("{cwd}/"))
}

impl Classifier {
    /// A classifier from paths and globs alone.
    pub fn new(runners: &[Runner]) -> anyhow::Result<Classifier> {
        let runners = runners
            .iter()
            .map(|r| {
                Ok((
                    r.config.clone(),
                    glob_set(&r.config.test_globs)?,
                    glob_set(&r.config.support_globs)?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Classifier {
            runners,
            root: None,
        })
    }

    /// A classifier that also reads the checkout at `root`, which holds head, for cargo's
    /// module layout. It never writes there.
    pub fn in_checkout(runners: &[Runner], root: &Path) -> anyhow::Result<Classifier> {
        let mut classifier = Classifier::new(runners)?;
        classifier.root = Some(root.to_path_buf());
        Ok(classifier)
    }

    /// The runners whose cwd contains `path`, deepest first.
    fn containing(&self, path: &str) -> Vec<usize> {
        let mut out: Vec<usize> = (0..self.runners.len())
            .filter(|i| under(path, &self.runners[*i].0.cwd))
            .collect();
        let depth = |i: &usize| {
            let cwd = &self.runners[*i].0.cwd;
            if cwd == "." { 0 } else { cwd.len() }
        };
        out.sort_by_key(|i| std::cmp::Reverse(depth(i)));
        out
    }

    /// For a cargo runner: whether cargo compiles the file only as test code, or None when
    /// titer cannot tell from the checkout.
    fn cargo_test_code(&self, config: &RunnerConfig, rel: &str) -> Option<bool> {
        let root = self.root.as_ref()?;
        let index = crate::runner::cargo::targets::index(&root.join(&config.cwd));
        let refs = index.refs(rel);
        if refs.is_empty() {
            return None;
        }
        Some(refs.iter().all(|r| {
            r.test_only || matches!(r.target.kind, crate::runner::cargo::targets::Kind::Test(_))
        }))
    }

    pub fn role(&self, path: &str) -> Role {
        let lang = Lang::of(path);
        let runners = self.containing(path);
        for &i in &runners {
            let (config, tests, _) = &self.runners[i];
            if !lang.is_some_and(|l| runs_lang(config.kind, l)) {
                continue;
            }
            let rel = cwd_relative(path, &config.cwd);
            if config.kind == RunnerKind::Cargo {
                // What cargo compiles decides; without that, a file under `src/` is source,
                // even in a `tests` module dir, since it can hold code that base must keep.
                let test = self
                    .cargo_test_code(config, &rel)
                    .unwrap_or_else(|| tests.is_match(&rel) && !rel.split('/').any(|p| p == "src"));
                if test {
                    return Role::Test { runner: i };
                }
                continue;
            }
            if tests.is_match(&rel) {
                return Role::Test { runner: i };
            }
        }
        for &i in &runners {
            let (config, _, support) = &self.runners[i];
            let rel = cwd_relative(path, &config.cwd);
            let cargo_rust = config.kind == RunnerKind::Cargo && lang == Some(Lang::Rust);
            if support.is_match(&rel) && !cargo_rust {
                return Role::Support { runner: i };
            }
        }
        let Some(lang) = lang else {
            return Role::NotAnalyzed;
        };
        // The runner with the deepest cwd that contains the file and runs its language.
        runners
            .into_iter()
            .find(|i| runs_lang(self.runners[*i].0.kind, lang))
            .map_or(Role::NotAnalyzed, |runner| Role::Source { runner, lang })
    }

    /// True for test and support files of any runner.
    pub fn is_test_path(&self, path: &str) -> bool {
        matches!(self.role(path), Role::Test { .. } | Role::Support { .. })
    }
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub runner: usize,
    /// Repo-relative path in head.
    pub file: String,
    pub lang: Lang,
    pub changed: BTreeSet<u32>,
}

#[derive(Debug, Clone)]
pub struct ChangedFunction {
    pub runner: usize,
    pub file: String,
    pub function: FunctionDef,
}

#[derive(Debug, Default)]
pub struct Detection {
    /// Changed tests, in file then line order.
    pub tests: Vec<TestCase>,
    /// Test and support files that changed and exist in head, with their runner.
    pub test_paths: Vec<(usize, String)>,
    /// Test files (not support files) that changed and exist in head, and Rust source files
    /// whose changed tests they hold.
    pub test_files: Vec<(usize, String)>,
    /// Rust source files that changed and exist in head. They hold their own unit tests,
    /// so prove splices head's test items into base's copy of each.
    pub rust_sources: Vec<String>,
    pub sources: Vec<SourceFile>,
    pub functions: Vec<ChangedFunction>,
    /// Changed files titer does not analyze.
    pub not_analyzed: Vec<String>,
    /// Changed tests that the runner cannot select apart from other tests.
    pub warnings: Vec<String>,
    /// Changed lockfiles under a runner that links dependency dirs from the checkout.
    pub lockfiles: Vec<String>,
}

impl Detection {
    pub fn tests_for(&self, runner: &str) -> Vec<TestCase> {
        self.tests
            .iter()
            .filter(|t| t.runner == runner)
            .cloned()
            .collect()
    }
}

pub fn python_style(kind: RunnerKind) -> PythonStyle {
    match kind {
        RunnerKind::Unittest => PythonStyle::Unittest,
        _ => PythonStyle::Pytest,
    }
}

pub fn detect(session: &Session, runners: &[Runner]) -> anyhow::Result<Detection> {
    let classifier = Classifier::in_checkout(runners, session.git.root())?;
    let mut detection = Detection::default();
    for change in &session.changes.files {
        let paths = [change.old_path.as_deref(), change.new_path.as_deref()];
        if paths
            .into_iter()
            .flatten()
            .any(|p| is_linked_lockfile(runners, p))
        {
            detection.lockfiles.push(change.path().to_string());
        }
        if change.status == FileStatus::Deleted {
            continue;
        }
        let path = change.path().to_string();
        if change.binary {
            detection.not_analyzed.push(path);
            continue;
        }
        match classifier.role(&path) {
            Role::Test { runner } => {
                detection.test_paths.push((runner, path.clone()));
                detection.test_files.push((runner, path.clone()));
                let source = read_head(session, &path)?;
                let lang = Lang::of(&path).expect("test files have a language");
                let found = changed_tests(&runners[runner], &path, lang, &source, &change.added);
                detection.tests.extend(found.tests);
                detection.warnings.extend(found.warnings);
            }
            Role::Support { runner } => detection.test_paths.push((runner, path)),
            Role::Source { runner, lang } => {
                if change.added.is_empty() {
                    continue;
                }
                let source = read_head(session, &path)?;
                let mut changed = change.added.clone();
                let mut test_ranges = Vec::new();
                if lang == Lang::Rust {
                    detection.rust_sources.push(path.clone());
                    let found =
                        changed_tests(&runners[runner], &path, lang, &source, &change.added);
                    if !found.tests.is_empty() {
                        detection.test_files.push((runner, path.clone()));
                    }
                    detection.tests.extend(found.tests);
                    detection.warnings.extend(found.warnings);
                    // Test code is never a mutant's target and never makes a change touch
                    // source. A line it shares with other code still counts.
                    let test_lines = lang::rust::test_lines(&source);
                    test_ranges = lang::rust::test_ranges(&source);
                    changed.retain(|l| !test_lines.iter().any(|(s, e)| (*s..=*e).contains(l)));
                }
                if changed.is_empty() {
                    continue;
                }
                let mut functions = lang::find_functions(lang, &source);
                functions.retain(|f| {
                    !test_ranges
                        .iter()
                        .any(|(s, e)| (*s..*e).contains(&f.start_byte))
                });
                let mut owners: BTreeSet<usize> = BTreeSet::new();
                for line in &changed {
                    owners.extend(lang::innermost_function(&functions, *line));
                }
                for i in owners {
                    detection.functions.push(ChangedFunction {
                        runner,
                        file: path.clone(),
                        function: functions[i].clone(),
                    });
                }
                detection.sources.push(SourceFile {
                    runner,
                    file: path,
                    lang,
                    changed,
                });
            }
            Role::NotAnalyzed => detection.not_analyzed.push(path),
        }
    }
    Ok(detection)
}

/// Whether `path` pins the dependencies in a dir that worktrees link from the checkout.
/// Base then runs with head's dependencies, so a change to it can skew results on base.
/// Cargo runners link nothing, so base builds from its own Cargo.lock.
fn is_linked_lockfile(runners: &[Runner], path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lockfile = matches!(
        name,
        "uv.lock" | "poetry.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock"
    ) || (name.starts_with("requirements") && name.ends_with(".txt"));
    lockfile
        && runners
            .iter()
            .any(|r| !r.config.deps.is_empty() && under(path, &r.config.cwd))
}

struct ChangedTests {
    tests: Vec<TestCase>,
    warnings: Vec<String>,
}

/// The tests in `source` whose spans intersect a changed line. A changed test whose id it
/// shares with another test, such as two vitest calls on one line, gets a warning: the
/// runner selects them together and titer reports their combined result for each.
fn changed_tests(
    runner: &Runner,
    path: &str,
    lang: Lang,
    source: &str,
    added: &BTreeSet<u32>,
) -> ChangedTests {
    let all: Vec<(String, lang::TestDef)> =
        lang::find_tests(lang, source, python_style(runner.config.kind))
            .into_iter()
            .map(|def| {
                (
                    runner.test_id(path, &def.suite, &def.name, def.report_line),
                    def,
                )
            })
            .collect();
    let changed = |def: &lang::TestDef| added.iter().any(|l| (def.line..=def.end_line).contains(l));
    // Groups in the order their first test appears, so warnings follow the source.
    let mut by_id: Vec<(&str, Vec<&lang::TestDef>)> = Vec::new();
    for (id, def) in &all {
        match by_id.iter_mut().find(|(i, _)| *i == id.as_str()) {
            Some((_, defs)) => defs.push(def),
            None => by_id.push((id.as_str(), vec![def])),
        }
    }
    let mut warnings = Vec::new();
    for (id, defs) in by_id {
        if defs.len() < 2 || !defs.iter().any(|d| changed(d)) {
            continue;
        }
        let same_name = defs
            .iter()
            .all(|d| (&d.suite, &d.name) == (&defs[0].suite, &defs[0].name));
        if same_name {
            // Python rebinds the name, so only the last definition runs. Rust twins differ
            // by #[cfg], so at most one compiles, and neither case shares a run.
            if lang.is_python() {
                warnings.push(format!(
                    "test {:?} is defined {} times in {path}; only the last definition runs",
                    full_name(defs[0]),
                    defs.len()
                ));
            }
            continue;
        }
        let names: Vec<String> = defs.iter().map(|d| format!("{:?}", full_name(d))).collect();
        warnings.push(format!(
            "tests {} share the selector {id}, so titer runs them together and reports their combined result for each",
            names.join(" and ")
        ));
    }
    let tests = all
        .into_iter()
        .filter(|(_, def)| changed(def))
        .map(|(id, def)| TestCase {
            runner: runner.name().to_string(),
            file: path.to_string(),
            line: def.report_line,
            end_line: def.end_line,
            id,
            suite: def.suite,
            name: def.name,
            skip: def.skip,
        })
        .collect();
    ChangedTests { tests, warnings }
}

/// A test's suite and name, as a person reads them.
fn full_name(def: &lang::TestDef) -> String {
    let mut parts = def.suite.clone();
    parts.push(def.name.clone());
    parts.join(" > ")
}

pub fn read_head(session: &Session, path: &str) -> anyhow::Result<String> {
    let bytes = session
        .git
        .read_blob(&session.head_tree, path)?
        .with_context(|| format!("{path} is missing from head"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn classifier(toml: &str) -> Classifier {
        let runners: Vec<Runner> = Config::parse(toml)
            .unwrap()
            .runners
            .into_iter()
            .map(Runner::new)
            .collect();
        Classifier::new(&runners).unwrap()
    }

    #[test]
    fn lockfiles_count_only_under_a_runner_that_links_dependency_dirs() {
        let runners: Vec<Runner> = Config::parse(
            r#"
            [[runner]]
            kind = "pytest"
            cwd = "api"
            command = ["python", "-m", "pytest"]

            [[runner]]
            kind = "cargo"
            command = ["cargo", "test"]
            "#,
        )
        .unwrap()
        .runners
        .into_iter()
        .map(Runner::new)
        .collect();
        for path in [
            "api/uv.lock",
            "api/requirements-dev.txt",
            "api/pkg/poetry.lock",
            "api/package-lock.json",
            "api/pnpm-lock.yaml",
            "api/yarn.lock",
        ] {
            assert!(is_linked_lockfile(&runners, path), "{path}");
        }
        for path in ["uv.lock", "Cargo.lock", "api/Cargo.lock", "api/notes.txt"] {
            assert!(!is_linked_lockfile(&runners, path), "{path}");
        }
    }

    #[test]
    fn assigns_files_to_runners_by_cwd_globs_and_language() {
        let c = classifier(
            r#"
            [[runner]]
            name = "api"
            kind = "unittest"
            cwd = "api"
            command = ["python", "-m", "unittest"]
            start_dir = "tests"

            [[runner]]
            name = "web"
            kind = "vitest"
            cwd = "web"
            command = ["npx", "vitest", "run"]
            "#,
        );
        assert_eq!(c.role("api/tests/test_login.py"), Role::Test { runner: 0 });
        assert_eq!(c.role("api/tests/helpers.py"), Role::Support { runner: 0 });
        assert_eq!(
            c.role("api/app/core.py"),
            Role::Source {
                runner: 0,
                lang: Lang::Python
            }
        );
        assert_eq!(c.role("web/src/a.test.tsx"), Role::Test { runner: 1 });
        assert_eq!(
            c.role("web/src/__tests__/util.ts"),
            Role::Support { runner: 1 }
        );
        assert_eq!(
            c.role("web/src/App.tsx"),
            Role::Source {
                runner: 1,
                lang: Lang::Tsx
            }
        );
        assert_eq!(c.role("web/scripts/build.py"), Role::NotAnalyzed);
        assert_eq!(c.role("scripts/deploy.py"), Role::NotAnalyzed);
        assert_eq!(c.role("api/README.md"), Role::NotAnalyzed);
    }

    #[test]
    fn cargo_test_files_are_integration_tests_outside_src() {
        let c = classifier("[[runner]]\nkind = \"cargo\"\ncommand = [\"cargo\", \"test\"]\n");
        assert_eq!(c.role("tests/api.rs"), Role::Test { runner: 0 });
        assert_eq!(
            c.role("crates/a/tests/common/mod.rs"),
            Role::Test { runner: 0 }
        );
        assert_eq!(
            c.role("tests/fixtures/data.json"),
            Role::Support { runner: 0 }
        );
        for source in [
            "src/tests.rs",
            "src/tests/mod.rs",
            "crates/a/src/x/tests/y.rs",
        ] {
            assert_eq!(
                c.role(source),
                Role::Source {
                    runner: 0,
                    lang: Lang::Rust
                },
                "{source}"
            );
        }
    }

    #[test]
    fn root_test_globs_match_files_at_any_depth() {
        let c = classifier("[[runner]]\nkind = \"pytest\"\ncommand = [\"pytest\"]\n");
        assert_eq!(c.role("test_a.py"), Role::Test { runner: 0 });
        assert_eq!(c.role("pkg/sub/test_a.py"), Role::Test { runner: 0 });
        assert_eq!(c.role("pkg/conftest.py"), Role::Support { runner: 0 });
        assert_eq!(
            c.role("pkg/a.py"),
            Role::Source {
                runner: 0,
                lang: Lang::Python
            }
        );
    }
}
