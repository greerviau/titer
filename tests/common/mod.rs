//! Shared helpers for integration tests: fixture git repos and the titer binary.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A throwaway git repo built inside a test.
pub struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Fixture {
        let dir = tempfile::Builder::new()
            .prefix("titer-fixture-")
            .tempdir()
            .expect("create fixture dir");
        let fixture = Fixture { dir };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn write(&self, rel: &str, contents: &str) -> &Fixture {
        let path = self.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        self
    }

    pub fn remove(&self, rel: &str) -> &Fixture {
        std::fs::remove_file(self.path().join(rel)).unwrap();
        self
    }

    /// Runs git in the fixture and returns stdout. Panics on failure.
    pub fn git(&self, args: &[&str]) -> String {
        let output = git_command(self.path()).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    /// Runs the titer binary in the fixture.
    pub fn titer(&self, args: &[&str]) -> Output {
        titer_command(self.path()).args(args).output().unwrap()
    }
}

/// Git variables that would point fixture commands at another repo or index, for example
/// when the tests run inside a git hook.
const GIT_REPO_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// A git command isolated from the user's and system config and from any outer repo.
pub fn git_command(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    for var in GIT_REPO_VARS {
        cmd.env_remove(var);
    }
    cmd.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.com");
    cmd
}

pub fn titer_command(cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_titer"));
    for var in GIT_REPO_VARS {
        cmd.env_remove(var);
    }
    cmd.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1");
    cmd
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Panics unless `report` is valid against the published schema.
pub fn assert_schema_valid(report: &serde_json::Value) {
    let text = std::fs::read_to_string(repo_root().join("schema/report.schema.json")).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&text).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(report)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "report violates schema: {errors:#?}\n{report:#}"
    );
}
