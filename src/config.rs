//! `titer.toml`: the per-repo opt-in and configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::report::Operator;

pub const FILE_NAME: &str = "titer.toml";

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The default `--base` ref. Without it, titer uses the remote default branch.
    pub base: Option<String>,
    #[serde(rename = "runner", default)]
    pub runners: Vec<RunnerConfig>,
    #[serde(default)]
    pub gate: GateConfig,
    #[serde(default)]
    pub mutate: MutateConfig,
    #[serde(default)]
    pub crap: CrapConfig,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    Pytest,
    Unittest,
    Vitest,
    Cargo,
}

impl RunnerKind {
    pub fn name(self) -> &'static str {
        match self {
            RunnerKind::Pytest => "pytest",
            RunnerKind::Unittest => "unittest",
            RunnerKind::Vitest => "vitest",
            RunnerKind::Cargo => "cargo",
        }
    }

    fn default_test_globs(self) -> Vec<String> {
        let globs: &[&str] = match self {
            RunnerKind::Pytest => &["**/test_*.py", "**/*_test.py"],
            RunnerKind::Unittest => &["**/test*.py"],
            RunnerKind::Vitest => &["**/*.{test,spec}.{ts,tsx,mts,cts,js,jsx,mjs,cjs}"],
            // Integration tests, and unit tests split out of a source file into `tests.rs`.
            // Unit tests inside source files are found by their attributes instead.
            RunnerKind::Cargo => &["**/tests/**/*.rs", "**/tests.rs"],
        };
        globs.iter().map(|g| g.to_string()).collect()
    }

    fn default_support_globs(self) -> Vec<String> {
        let globs: &[&str] = match self {
            RunnerKind::Pytest | RunnerKind::Unittest => &["**/conftest.py", "tests/**", "test/**"],
            RunnerKind::Vitest => &[
                "**/__tests__/**",
                "**/__mocks__/**",
                "**/__snapshots__/**",
                "**/__fixtures__/**",
                "**/test-utils/**",
                "**/test-setup.*",
                "**/setupTests.*",
                "**/vitest.setup.*",
                "test/**",
                "tests/**",
            ],
            RunnerKind::Cargo => &["**/tests/**", "**/testdata/**", "**/snapshots/**"],
        };
        globs.iter().map(|g| g.to_string()).collect()
    }

    fn default_deps(self) -> Vec<String> {
        let deps: &[&str] = match self {
            RunnerKind::Pytest | RunnerKind::Unittest => &[".venv", "venv"],
            RunnerKind::Vitest => &["node_modules"],
            // A shared target dir would let one worktree's build overwrite another's
            // binaries, so each worktree builds its own.
            RunnerKind::Cargo => &[],
        };
        deps.iter().map(|d| d.to_string()).collect()
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawRunner {
    name: Option<String>,
    kind: RunnerKind,
    #[serde(default = "dot")]
    cwd: String,
    command: Vec<String>,
    test_globs: Option<Vec<String>>,
    support_globs: Option<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    deps: Option<Vec<String>>,
    start_dir: Option<String>,
}

fn dot() -> String {
    ".".to_string()
}

/// One test runner. A monorepo lists one per package.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(try_from = "RawRunner")]
pub struct RunnerConfig {
    /// Unique name; defaults to the kind.
    pub name: String,
    pub kind: RunnerKind,
    /// Repo-relative directory the command runs in. Globs and paths below are relative to it.
    pub cwd: String,
    /// The repo's own command prefix, for example `["uv", "run", "--no-sync", "python", "-m", "pytest"]`
    /// or `["cargo", "test"]`.
    pub command: Vec<String>,
    /// Files that contain tests.
    pub test_globs: Vec<String>,
    /// Other test-only files, such as fixtures and helpers. Never mutated; copied onto base with the tests.
    pub support_globs: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Ignored dependency dirs symlinked from the checkout into each worktree.
    pub deps: Vec<String>,
    /// unittest only: the top-level dir that dotted ids are relative to.
    pub start_dir: String,
}

impl TryFrom<RawRunner> for RunnerConfig {
    type Error = String;

    fn try_from(raw: RawRunner) -> Result<Self, String> {
        if raw.command.is_empty() {
            return Err("runner command must not be empty".to_string());
        }
        if raw.start_dir.is_some() && raw.kind != RunnerKind::Unittest {
            return Err("start_dir applies only to unittest runners".to_string());
        }
        if raw.kind == RunnerKind::Unittest && raw.command.iter().any(|a| a == "discover") {
            return Err(
                "a unittest command is the prefix that accepts test names, such as \
                 [\"python\", \"-m\", \"unittest\"]; drop `discover` and set start_dir"
                    .to_string(),
            );
        }
        let cwd = normalize_rel(&raw.cwd)?;
        Ok(RunnerConfig {
            name: raw.name.unwrap_or_else(|| raw.kind.name().to_string()),
            kind: raw.kind,
            cwd,
            command: raw.command,
            test_globs: raw
                .test_globs
                .unwrap_or_else(|| raw.kind.default_test_globs()),
            support_globs: raw
                .support_globs
                .unwrap_or_else(|| raw.kind.default_support_globs()),
            env: raw.env,
            deps: raw.deps.unwrap_or_else(|| raw.kind.default_deps()),
            start_dir: normalize_rel(raw.start_dir.as_deref().unwrap_or("."))?,
        })
    }
}

/// Normalizes a repo-relative path: no leading `./`, no trailing slash, `.` for the root.
fn normalize_rel(path: &str) -> Result<String, String> {
    if Path::new(path).is_absolute() {
        return Err(format!("path must be relative to the repo: {path}"));
    }
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(format!("path must stay inside the repo: {path}"));
    }
    Ok(if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    })
}

/// Opt-in gate rules. By default the gate fails only on unproven changes.
#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    #[serde(default)]
    pub fail_on_survivors: bool,
    #[serde(default)]
    pub fail_on_passes_on_base: bool,
    #[serde(default)]
    pub fail_on_scan: bool,
    #[serde(default)]
    pub fail_on_crap: bool,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MutateConfig {
    /// The time budget for a whole check, for example "120s".
    #[serde(default = "default_budget", with = "humantime_serde_compat")]
    pub budget: Duration,
    #[serde(default)]
    pub exclude_operators: Vec<Operator>,
    /// Repo-relative globs of source files never mutated.
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    /// Callee names whose calls are never removed, in addition to logging calls.
    #[serde(default)]
    pub exclude_calls: Vec<String>,
}

impl Default for MutateConfig {
    fn default() -> Self {
        MutateConfig {
            budget: default_budget(),
            exclude_operators: Vec::new(),
            exclude_globs: Vec::new(),
            exclude_calls: Vec::new(),
        }
    }
}

pub const DEFAULT_BUDGET: Duration = Duration::from_secs(120);

fn default_budget() -> Duration {
    DEFAULT_BUDGET
}

mod humantime_serde_compat {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(d)?;
        humantime::parse_duration(&text).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CrapConfig {
    /// A command that writes a coverage file, run from the repo root.
    pub coverage_command: Option<Vec<String>>,
    /// The repo-relative coverage file the command writes: lcov, cobertura XML, or coverage.py JSON.
    pub coverage_file: Option<String>,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}

impl Default for CrapConfig {
    fn default() -> Self {
        CrapConfig {
            coverage_command: None,
            coverage_file: None,
            threshold: default_threshold(),
        }
    }
}

fn default_threshold() -> f64 {
    30.0
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no {FILE_NAME} in {0}; this repo has not opted in to titer. Run `titer init` to opt in."
    )]
    Missing(PathBuf),
    #[error("cannot read {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("invalid {FILE_NAME}: {0}")]
    Invalid(String),
}

impl Config {
    /// Loads `titer.toml` from the repo root.
    pub fn load(repo_root: &Path) -> Result<Config, ConfigError> {
        let path = repo_root.join(FILE_NAME);
        if !path.is_file() {
            return Err(ConfigError::Missing(repo_root.to_path_buf()));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| ConfigError::Read(path, e))?;
        let config = Config::parse(&text)?;
        for runner in &config.runners {
            if !repo_root.join(&runner.cwd).is_dir() {
                return Err(ConfigError::Invalid(format!(
                    "runner {:?} cwd {:?} is not a directory",
                    runner.name, runner.cwd
                )));
            }
        }
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let config: Config =
            toml::from_str(text).map_err(|e| ConfigError::Invalid(e.message().to_string()))?;
        if config.runners.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[runner]] is required".to_string(),
            ));
        }
        let mut names = std::collections::HashSet::new();
        for runner in &config.runners {
            if !names.insert(runner.name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate runner name {:?}; set a unique `name` on each runner",
                    runner.name
                )));
            }
        }
        if config
            .crap
            .coverage_command
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            return Err(ConfigError::Invalid(
                "crap.coverage_command must not be empty".to_string(),
            ));
        }
        if config.crap.coverage_command.is_some() && config.crap.coverage_file.is_none() {
            return Err(ConfigError::Invalid(
                "crap.coverage_command needs crap.coverage_file".to_string(),
            ));
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_monorepo_config_with_defaults() {
        let config = Config::parse(
            r#"
            [[runner]]
            name = "api"
            kind = "unittest"
            cwd = "./api/"
            command = ["uv", "run", "--no-sync", "python", "-m", "unittest"]
            start_dir = "tests"

            [[runner]]
            name = "web"
            kind = "vitest"
            cwd = "web"
            command = ["npx", "vitest", "run"]
            env = { TZ = "UTC" }

            [gate]
            fail_on_survivors = true

            [mutate]
            budget = "90s"
            exclude_operators = ["remove_call"]
            "#,
        )
        .unwrap();
        assert_eq!(config.runners.len(), 2);
        let api = &config.runners[0];
        assert_eq!(api.cwd, "api");
        assert_eq!(api.start_dir, "tests");
        assert_eq!(api.deps, vec![".venv", "venv"]);
        let web = &config.runners[1];
        assert_eq!(web.kind, RunnerKind::Vitest);
        assert_eq!(web.env["TZ"], "UTC");
        assert!(config.gate.fail_on_survivors);
        assert!(!config.gate.fail_on_scan);
        assert_eq!(config.mutate.budget, Duration::from_secs(90));
        assert_eq!(config.mutate.exclude_operators, vec![Operator::RemoveCall]);
        assert_eq!(config.crap.threshold, 30.0);
    }

    #[test]
    fn cargo_runners_default_to_rust_test_paths_and_no_deps() {
        let config =
            Config::parse("[[runner]]\nkind = \"cargo\"\ncommand = [\"cargo\", \"test\"]\n")
                .unwrap();
        let cargo = &config.runners[0];
        assert_eq!(cargo.name, "cargo");
        assert_eq!(cargo.kind, RunnerKind::Cargo);
        assert_eq!(cargo.test_globs, vec!["**/tests/**/*.rs", "**/tests.rs"]);
        assert_eq!(
            cargo.support_globs,
            vec!["**/tests/**", "**/testdata/**", "**/snapshots/**"]
        );
        assert!(cargo.deps.is_empty());
    }

    #[test]
    fn rejects_configs_that_cannot_run() {
        let cases = [
            ("", "at least one [[runner]]"),
            (
                "[[runner]]\nkind = \"pytest\"\ncommand = []",
                "command must not be empty",
            ),
            (
                "[[runner]]\nkind = \"jest\"\ncommand = [\"x\"]",
                "unknown variant",
            ),
            (
                "[[runner]]\nkind = \"pytest\"\ncommand = [\"x\"]\ncwd = \"../up\"",
                "inside the repo",
            ),
            (
                "[[runner]]\nkind = \"pytest\"\ncommand = [\"x\"]\n[[runner]]\nkind = \"pytest\"\ncommand = [\"y\"]",
                "duplicate runner name",
            ),
            (
                "[[runner]]\nkind = \"pytest\"\ncommand = [\"x\"]\nstart_dir = \"t\"",
                "only to unittest",
            ),
            (
                "[[runner]]\nkind = \"unittest\"\ncommand = [\"python\", \"-m\", \"unittest\", \"discover\"]",
                "drop `discover`",
            ),
            (
                "[[runner]]\nkind = \"pytest\"\ncommand = [\"x\"]\n[mutate]\nbudget = \"soon\"",
                "",
            ),
        ];
        for (text, expected) in cases {
            let err = Config::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text:?} gave {err:?}");
        }
    }
}
