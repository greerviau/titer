//! The JSON report that `titer check --json` prints, and its JSON Schema.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::exit;

/// Bumped on any breaking change to the report shape.
pub const SCHEMA_VERSION: &str = "2";

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub schema_version: String,
    pub titer_version: String,
    /// The `--base` ref as given or resolved from the remote default branch.
    pub base_ref: Option<String>,
    /// The merge-base of head and the base ref.
    pub base_sha: Option<String>,
    /// The tree object of the working-tree snapshot.
    pub head_tree: Option<String>,
    pub gate: Gate,
    /// The tiers that ran.
    pub tiers: Vec<Tier>,
    pub warnings: Vec<String>,
    /// Changed files in languages titer does not parse.
    pub not_analyzed: Vec<String>,
    pub findings: Vec<Finding>,
    pub tests: Vec<TestResult>,
    pub mutants: Vec<Mutant>,
    pub crap: Vec<CrapScore>,
    pub timing: Timing,
}

impl Report {
    pub fn new() -> Self {
        Report {
            schema_version: SCHEMA_VERSION.to_string(),
            titer_version: env!("CARGO_PKG_VERSION").to_string(),
            base_ref: None,
            base_sha: None,
            head_tree: None,
            gate: Gate::pass(),
            tiers: Vec::new(),
            warnings: Vec::new(),
            not_analyzed: Vec::new(),
            findings: Vec::new(),
            tests: Vec::new(),
            mutants: Vec::new(),
            crap: Vec::new(),
            timing: Timing::default(),
        }
    }

    /// A report for a check that could not run.
    pub fn error(reason: impl Into<String>) -> Self {
        let mut report = Report::new();
        report.gate = Gate::error(reason);
        report
    }
}

impl Default for Report {
    fn default() -> Self {
        Report::new()
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Scan,
    Prove,
    Mutate,
    Crap,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Scan, Tier::Prove, Tier::Mutate, Tier::Crap];

    pub fn name(self) -> &'static str {
        match self {
            Tier::Scan => "scan",
            Tier::Prove => "prove",
            Tier::Mutate => "mutate",
            Tier::Crap => "crap",
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    Fail,
    Error,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub status: GateStatus,
    /// The process exit code: 0 pass, 1 fail, 2 error.
    pub exit_code: u8,
    pub reasons: Vec<String>,
}

impl Gate {
    pub fn pass() -> Self {
        Gate {
            status: GateStatus::Pass,
            exit_code: exit::PASS,
            reasons: Vec::new(),
        }
    }

    pub fn fail(reasons: Vec<String>) -> Self {
        Gate {
            status: GateStatus::Fail,
            exit_code: exit::FAIL,
            reasons,
        }
    }

    pub fn error(reason: impl Into<String>) -> Self {
        Gate {
            status: GateStatus::Error,
            exit_code: exit::ERROR,
            reasons: vec![reason.into()],
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    TestDeleted,
    AssertionRemoved,
    SkipAdded,
    MatcherLoosened,
    SnapshotChanged,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub kind: FindingKind,
    /// Repo-relative path.
    pub file: String,
    /// 1-based line in head, or in base when the line no longer exists.
    pub line: u32,
    /// The test the finding is about, when there is one.
    pub test: Option<String>,
    pub message: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HeadStatus {
    Green,
    Red,
    Skipped,
    /// The selection executed nothing.
    NoResult,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Fails an assertion on base. Proof.
    Assertion,
    /// Raises another exception from the code under test on base. Proof.
    Exception,
    /// Fails on base because a name exists only in head, or does not compile against base. Not proof.
    MissingSymbol,
    /// Green on base. Informational.
    PassesOnBase,
    /// Skipped on head or on base. Not proof.
    Skipped,
    /// Zero tests executed. Not proof.
    NoResult,
    /// Timed out on base. Not proof.
    Timeout,
}

impl Verdict {
    pub fn is_proof(self) -> bool {
        matches!(self, Verdict::Assertion | Verdict::Exception)
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TestResult {
    /// The runner-native id: a pytest node id, a unittest dotted id, `file:line` for vitest, or `file::path::to::test` for cargo.
    pub id: String,
    pub runner: String,
    /// Repo-relative path.
    pub file: String,
    pub line: u32,
    /// The test name, with enclosing classes or `describe` titles joined by " > ".
    pub name: String,
    pub head: HeadStatus,
    /// Absent when the test is not green on head.
    pub verdict: Option<Verdict>,
    pub exception_type: Option<String>,
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    SwapComparison,
    SwapBoolean,
    SwapArithmetic,
    FlipBool,
    IncrementInt,
    RemoveNot,
    ReturnNone,
    RemoveCall,
    RemoveRaise,
    NegateIf,
    JsxAttributeUndefined,
    /// A mutant from `--mutants`.
    Agent,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MutantSource {
    Generated,
    Agent,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MutantOutcome {
    /// A selected test failed with an assertion or exception.
    Killed,
    /// The selected tests passed and the line runs under them.
    Survived,
    /// A survivor whose line no green selected test runs (the sentinel survived).
    NotExecuted,
    /// A survivor that other tests in the changed test files kill.
    CaughtByOther,
    TimedOut,
    /// Zero tests executed or every test skipped.
    NoResult,
    /// The budget ran out before the mutant's run, its canary, or its survivor follow-up
    /// finished.
    NotRun,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mutant {
    pub id: String,
    pub runner: String,
    /// Repo-relative path.
    pub file: String,
    pub line: u32,
    pub end_line: u32,
    pub operator: Operator,
    pub source: MutantSource,
    pub original: String,
    pub replacement: String,
    pub outcome: MutantOutcome,
    /// The test that killed the mutant, when known.
    pub killed_by: Option<String>,
    pub duration_ms: Option<u64>,
    /// The mutated code also appears in the base lines that this hunk replaces, as a syntax
    /// node with the same kind, parent kind, and tokens. The change may only have moved or
    /// reformatted it, so a survivor here may predate the change. Always false for agent
    /// mutants.
    pub unchanged_from_base: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CrapScore {
    /// Repo-relative path.
    pub file: String,
    pub function: String,
    pub line: u32,
    pub complexity: u32,
    /// Covered fraction of the function's coverable lines, 0 to 1. Absent without coverage data.
    pub coverage: Option<f64>,
    pub score: Option<f64>,
    /// The score is above the configured threshold.
    pub flagged: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    pub total_ms: u64,
    pub scan_ms: Option<u64>,
    pub prove_ms: Option<u64>,
    pub mutate_ms: Option<u64>,
    pub crap_ms: Option<u64>,
    /// Mutation workers used.
    pub workers: Option<u32>,
}

/// The JSON Schema for [`Report`], as published in `schema/report.schema.json`.
pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Report)).expect("schema serializes")
}
