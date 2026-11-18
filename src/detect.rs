//! Changed-test and source-function detection: which files, tests, and functions a change
//! touches.

use std::collections::BTreeSet;

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
        Ok(Classifier { runners })
    }

    pub fn role(&self, path: &str) -> Role {
        let lang = Lang::of(path);
        for (i, (config, tests, _)) in self.runners.iter().enumerate() {
            let rel = cwd_relative(path, &config.cwd);
            // A cargo file under `src/` is source, even in a `tests` module dir: it can hold
            // code that base must keep.
            let in_src = config.kind == RunnerKind::Cargo && rel.split('/').any(|p| p == "src");
            if under(path, &config.cwd)
                && lang.is_some_and(|l| runs_lang(config.kind, l))
                && tests.is_match(&rel)
                && !in_src
            {
                return Role::Test { runner: i };
            }
        }
        for (i, (config, _, support)) in self.runners.iter().enumerate() {
            let rel = cwd_relative(path, &config.cwd);
            let rust_in_src = config.kind == RunnerKind::Cargo
                && lang == Some(Lang::Rust)
                && rel.split('/').any(|p| p == "src");
            if under(path, &config.cwd) && support.is_match(&rel) && !rust_in_src {
                return Role::Support { runner: i };
            }
        }
        let Some(lang) = lang else {
            return Role::NotAnalyzed;
        };
        // The runner with the deepest cwd that contains the file and runs its language.
        self.runners
            .iter()
            .enumerate()
            .filter(|(_, (config, _, _))| under(path, &config.cwd) && runs_lang(config.kind, lang))
            .max_by_key(|(_, (config, _, _))| {
                if config.cwd == "." {
                    0
                } else {
                    config.cwd.len()
                }
            })
            .map_or(Role::NotAnalyzed, |(runner, _)| Role::Source {
                runner,
                lang,
            })
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
    let classifier = Classifier::new(runners)?;
    let mut detection = Detection::default();
    for change in &session.changes.files {
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
                detection.tests.extend(found);
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
                    if !found.is_empty() {
                        detection.test_files.push((runner, path.clone()));
                    }
                    detection.tests.extend(found);
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

/// The tests in `source` whose spans intersect a changed line.
fn changed_tests(
    runner: &Runner,
    path: &str,
    lang: Lang,
    source: &str,
    added: &BTreeSet<u32>,
) -> Vec<TestCase> {
    lang::find_tests(lang, source, python_style(runner.config.kind))
        .into_iter()
        .filter(|def| added.iter().any(|l| (def.line..=def.end_line).contains(l)))
        .map(|def| TestCase {
            runner: runner.name().to_string(),
            file: path.to_string(),
            line: def.report_line,
            end_line: def.end_line,
            id: runner.test_id(path, &def.suite, &def.name, def.report_line),
            suite: def.suite,
            name: def.name,
            skip: def.skip,
        })
        .collect()
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
