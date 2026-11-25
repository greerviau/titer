//! Per-test coverage: which changed tests run each line of a source file, so each mutant
//! runs first against only the changed tests that cover it, and a survivor whose code no
//! changed test runs is not executed without a sentinel run.
//!
//! pytest and unittest: the green changed tests run once more with the repo's own command
//! and coverage.py's `.pth` file, which starts coverage in every Python process when
//! `COVERAGE_PROCESS_START` names a config file. That config turns on `test_function`
//! dynamic contexts. vitest has no per-test coverage, so the changed tests run once
//! together with the repo's coverage provider and, to attribute statements, once each
//! alone. Cargo has none.
//!
//! The data says a line ran outside any changed test, such as at import or in a fixture,
//! without naming a test, and says nothing about a line it does not measure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config::RunnerKind;
use crate::runner::{CaseResult, RunRequest, Runner, Selection, TestCase, TestRun};

/// What coverage says about a mutant's code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cover {
    /// The data cannot say.
    Unknown,
    /// The changed tests never ran it.
    NotRun,
    /// The changed tests ran it. The ids name the tests that did, when only changed tests
    /// ran it and the data attributes it to them.
    Ran(Option<BTreeSet<String>>),
}

/// When coverage runs must stop: a run starts only if `expected` fits before `deadline`,
/// and none outlasts it.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub deadline: crate::deadline::Deadline,
    pub expected: Duration,
    pub timeout: Duration,
}

/// The outcome of collecting coverage for one runner.
#[derive(Debug, Clone)]
pub enum Collected {
    /// No coverage tool ran, or the runner has none.
    Missing,
    Ready(TestCoverage),
    /// Data exists but does not match the tests that ran, so titer does not trust it.
    Unusable(String),
}

/// Per-test coverage of one runner's changed tests, keyed by repo-relative path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TestCoverage {
    files: BTreeMap<String, FileCover>,
}

#[derive(Debug, Clone, PartialEq)]
enum FileCover {
    /// coverage.py: each measured line, by whether only changed tests ran it.
    Lines(BTreeMap<u32, LineCover>),
    /// istanbul JSON, as vitest writes it: statements, whether the run of every changed
    /// test ran each, and which changed tests ran each when they ran alone.
    Statements {
        statements: Vec<Statement>,
        /// Every changed test ran alone and each report counted every statement.
        attributed: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum LineCover {
    /// Code outside any changed test ran it, such as an import or a fixture.
    Shared,
    Tests(BTreeSet<String>),
}

#[derive(Debug, Clone, PartialEq)]
struct Statement {
    /// 1-based line and 0-based UTF-16 column, as istanbul reports them.
    start: (u32, u32),
    /// The end; no column means the end of the line.
    end: (u32, Option<u32>),
    /// The run of every changed test together ran it.
    ran: bool,
    /// The changed tests that ran it when each ran alone.
    tests: BTreeSet<String>,
}

impl TestCoverage {
    /// What the data says about a mutant that starts at `line` and `column` (0-based, in
    /// UTF-16 units) and ends on `end_line`.
    pub fn cover(&self, file: &str, line: u32, column: u32, end_line: u32) -> Cover {
        match self.files.get(file) {
            None => Cover::Unknown,
            Some(FileCover::Lines(lines)) => {
                // A continuation line belongs to a statement that starts on an earlier line,
                // which the range may not include.
                if !lines.contains_key(&line) {
                    return Cover::Unknown;
                }
                let mut tests = BTreeSet::new();
                let mut shared = false;
                for cover in lines.range(line..=end_line.max(line)).map(|(_, c)| c) {
                    match cover {
                        LineCover::Shared => shared = true,
                        LineCover::Tests(t) => tests.extend(t.iter().cloned()),
                    }
                }
                match (shared, tests.is_empty()) {
                    (true, _) => Cover::Ran(None),
                    (false, true) => Cover::NotRun,
                    (false, false) => Cover::Ran(Some(tests)),
                }
            }
            Some(FileCover::Statements {
                statements,
                attributed,
            }) => {
                // Statement ranges say nothing about which parts of a multi-line edit ran.
                if end_line != line {
                    return Cover::Unknown;
                }
                let at = (line, column);
                let contains = |s: &&Statement| {
                    s.start <= at
                        && (s.end.0 > line
                            || (s.end.0 == line && s.end.1.is_none_or(|c| column < c)))
                };
                // The innermost statement: one that never ran never ran anything inside it.
                match statements.iter().filter(contains).max_by_key(|s| s.start) {
                    None => Cover::Unknown,
                    Some(s) if !s.ran => Cover::NotRun,
                    // Alone, a test can miss code that it reaches after another test ran.
                    Some(s) if *attributed && !s.tests.is_empty() => {
                        Cover::Ran(Some(s.tests.clone()))
                    }
                    Some(_) => Cover::Ran(None),
                }
            }
        }
    }
}

/// The repo-relative path of a path a coverage tool reported, which is absolute under one
/// of `roots` or relative to the runner's cwd.
fn repo_path(reported: &str, roots: &[PathBuf], cwd: &str) -> Option<String> {
    let path = Path::new(reported);
    if path.is_absolute() {
        return roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok())
            .map(|rel| rel.to_string_lossy().into_owned());
    }
    let rel = reported.trim_start_matches("./");
    Some(if cwd == "." {
        rel.to_string()
    } else {
        format!("{cwd}/{rel}")
    })
}

/// The worktree root as given and as the OS resolves it, since tools report either.
fn roots(worktree: &Path) -> Vec<PathBuf> {
    let mut roots = vec![worktree.to_path_buf()];
    roots.extend(worktree.canonicalize().ok().filter(|c| c != worktree));
    roots
}

/// The changed tests that a coverage.py `test_function` context names. The context is the
/// test's module and qualified name, such as `tests.test_a.TestX.test_y`, and the module
/// is the file's dotted path or a suffix of it, by how the runner imported it.
fn tests_for_context<'a>(context: &str, cases: &'a [TestCase]) -> Vec<&'a TestCase> {
    let parts: Vec<&str> = context.split('.').collect();
    cases
        .iter()
        .filter(|case| {
            let tail: Vec<&str> = case
                .suite
                .iter()
                .map(String::as_str)
                .chain([case.name.as_str()])
                .collect();
            if parts.len() <= tail.len() || !parts.ends_with(&tail) {
                return false;
            }
            let module = &parts[..parts.len() - tail.len()];
            let file: Vec<&str> = case.file.trim_end_matches(".py").split('/').collect();
            file.ends_with(module)
        })
        .collect()
}

/// Parses `coverage json --show-contexts` output for the changed tests `cases`.
fn parse_coverage_py(
    text: &str,
    cases: &[TestCase],
    roots: &[PathBuf],
    cwd: &str,
) -> Result<TestCoverage, String> {
    let json: Value =
        serde_json::from_str(text).map_err(|e| format!("coverage JSON does not parse: {e}"))?;
    let files = json["files"]
        .as_object()
        .ok_or("coverage JSON has no \"files\" object")?;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut coverage = TestCoverage::default();
    for (reported, data) in files {
        let Some(file) = repo_path(reported, roots, cwd) else {
            continue;
        };
        let mut lines = BTreeMap::new();
        for n in data["missing_lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
        {
            lines.insert(n as u32, LineCover::Tests(BTreeSet::new()));
        }
        let contexts = data["contexts"].as_object();
        for n in data["executed_lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
        {
            let labels: Vec<&str> = contexts
                .and_then(|c| c.get(&n.to_string()))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let mut tests = BTreeSet::new();
            let mut shared = labels.is_empty();
            for label in labels {
                let matched = tests_for_context(label, cases);
                // An empty label is code outside any test. A label that names no changed
                // test means the mapping failed, so the line may matter to any of them.
                if matched.is_empty() {
                    shared = true;
                }
                for case in matched {
                    seen.insert(case.id.as_str());
                    tests.insert(case.id.clone());
                }
            }
            lines.insert(
                n as u32,
                if shared {
                    LineCover::Shared
                } else {
                    LineCover::Tests(tests)
                },
            );
        }
        coverage.files.insert(file, FileCover::Lines(lines));
    }
    // Every test that passed ran at least its own body, so a test with no context means
    // the data is incomplete, for example because another coverage plugin took over.
    if let Some(case) = cases.iter().find(|c| !seen.contains(c.id.as_str())) {
        return Err(format!(
            "the coverage data has no context for {}, so mutants run every changed test",
            case.id
        ));
    }
    Ok(coverage)
}

/// Reads the istanbul `coverage-final.json` of the run of every changed test. A file whose
/// report lacks a statement's counter is left out, so its mutants are unknown.
fn parse_istanbul(text: &str, roots: &[PathBuf], cwd: &str) -> Result<TestCoverage, String> {
    let mut coverage = TestCoverage::default();
    for (file, statements) in istanbul_statements(text, roots, cwd)? {
        let Some(statements) = statements else {
            continue;
        };
        let statements = statements
            .into_iter()
            .map(|(start, end, ran)| Statement {
                start,
                end,
                ran,
                tests: BTreeSet::new(),
            })
            .collect();
        coverage.files.insert(
            file,
            FileCover::Statements {
                statements,
                attributed: true,
            },
        );
    }
    Ok(coverage)
}

/// Adds one test's own istanbul report to `coverage`. A file the report lacks, a statement
/// it does not count, or a statement the combined run does not have leaves that file
/// unattributed.
fn attribute_istanbul(
    coverage: &mut TestCoverage,
    text: &str,
    test: &str,
    roots: &[PathBuf],
    cwd: &str,
) -> Result<(), String> {
    let mut reported = istanbul_statements(text, roots, cwd)?;
    for (file, cover) in coverage.files.iter_mut() {
        let FileCover::Statements {
            statements,
            attributed,
        } = cover
        else {
            continue;
        };
        // A file the test never loaded ran nothing for it.
        let Some(entry) = reported.remove(file) else {
            continue;
        };
        let Some(alone) = entry else {
            *attributed = false;
            continue;
        };
        for (start, end, ran) in alone {
            match statements
                .iter_mut()
                .find(|s| s.start == start && s.end == end)
            {
                Some(s) if ran => {
                    s.tests.insert(test.to_string());
                }
                Some(_) => {}
                None => *attributed = false,
            }
        }
    }
    Ok(())
}

type Span = ((u32, u32), (u32, Option<u32>), bool);

/// Each file's statements and whether they ran, keyed by repo-relative path. None for a
/// file with a statement and no numeric counter for it.
fn istanbul_statements(
    text: &str,
    roots: &[PathBuf],
    cwd: &str,
) -> Result<BTreeMap<String, Option<Vec<Span>>>, String> {
    let json: Value =
        serde_json::from_str(text).map_err(|e| format!("coverage JSON does not parse: {e}"))?;
    let files = json.as_object().ok_or("coverage JSON is not an object")?;
    let mut out = BTreeMap::new();
    for (reported, data) in files {
        let Some(file) = repo_path(reported, roots, cwd) else {
            continue;
        };
        let (Some(map), Some(counts)) = (data["statementMap"].as_object(), data["s"].as_object())
        else {
            out.insert(file, None);
            continue;
        };
        let pos = |p: &Value| -> Option<(u32, Option<u32>)> {
            Some((
                p["line"].as_u64()? as u32,
                p["column"].as_u64().map(|c| c as u32),
            ))
        };
        let mut spans = Some(Vec::new());
        for (id, range) in map {
            let (Some((start_line, Some(start_col))), Some(end), Some(count)) = (
                pos(&range["start"]),
                pos(&range["end"]),
                counts.get(id).and_then(Value::as_u64),
            ) else {
                spans = None;
                break;
            };
            if let Some(spans) = spans.as_mut() {
                spans.push(((start_line, start_col), end, count > 0));
            }
        }
        // The arms of a ternary and the operands of `&&`, `||`, and `??` are narrower than
        // their statement and have their own counters. An `if` branch's locations span the
        // whole statement, so they say nothing about the condition.
        if let (Some(spans), Some(branches), Some(counts)) = (
            spans.as_mut(),
            data["branchMap"].as_object(),
            data["b"].as_object(),
        ) {
            for (id, branch) in branches {
                if !matches!(branch["type"].as_str(), Some("cond-expr" | "binary-expr")) {
                    continue;
                }
                let arms = branch["locations"].as_array().into_iter().flatten();
                let hits = counts.get(id).and_then(Value::as_array);
                for (k, arm) in arms.enumerate() {
                    let (Some((line, Some(col))), Some(end), Some(count)) = (
                        pos(&arm["start"]),
                        pos(&arm["end"]),
                        hits.and_then(|h| h.get(k)).and_then(Value::as_u64),
                    ) else {
                        continue;
                    };
                    spans.push(((line, col), end, count > 0));
                }
            }
        }
        out.insert(file, spans);
    }
    Ok(out)
}

/// The coverage.py settings for the head run, written into `dir`, and the environment that
/// makes the runner's own command load them.
pub fn python_env(dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
    std::fs::create_dir_all(dir)?;
    let rc = dir.join("coveragerc");
    let data = dir.join("data");
    // Parallel data files keep a subprocess from overwriting the test process's data, and
    // the patches save the data of forked children and of processes that call `os._exit`.
    std::fs::write(
        &rc,
        format!(
            "[run]\ndynamic_context = test_function\nparallel = true\npatch = fork, _exit, subprocess\ndata_file = {}\n",
            data.display()
        ),
    )?;
    Ok(vec![
        (
            "COVERAGE_PROCESS_START".to_string(),
            rc.display().to_string(),
        ),
        ("COVERAGE_FILE".to_string(), data.display().to_string()),
    ])
}

/// The argv prefix that runs the Python the runner's command runs, such as
/// `[".venv/bin/python"]` for `.venv/bin/python -m pytest` or `["uv", "run", "python"]` for
/// `uv run pytest`. None when the command does not show it.
fn python_argv(
    command: &[String],
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<Vec<String>> {
    let module = command
        .windows(2)
        .position(|w| w[0] == "-m" && matches!(w[1].as_str(), "pytest" | "py.test" | "unittest"));
    if let Some(i) = module {
        return (i > 0).then(|| command[..i].to_vec());
    }
    let script = command.iter().position(|a| {
        matches!(
            Path::new(a).file_name().and_then(|n| n.to_str()),
            Some("pytest" | "py.test")
        )
    })?;
    if script > 0 {
        // A wrapper such as `uv run` or `poetry run` runs `python` the same way.
        let mut argv = command[..script].to_vec();
        argv.push("python".to_string());
        return Some(argv);
    }
    // An entry-point script names its interpreter on its first line.
    let text = std::fs::read_to_string(resolve(&command[0])?).ok()?;
    let shebang = text.lines().next()?.strip_prefix("#!")?.trim();
    let mut words = shebang.split_whitespace();
    let first = words.next()?;
    if Path::new(first).file_name().is_some_and(|n| n == "env") {
        return words
            .find(|w| !w.starts_with('-'))
            .map(|w| vec![w.to_string()]);
    }
    Some(vec![first.to_string()])
}

/// The argv prefix that runs the runner's Python in `worktree`.
fn python_for(runner: &Runner, worktree: &Path) -> Option<Vec<String>> {
    let cwd = runner.cwd_in(worktree);
    let resolve = |program: &str| -> Option<PathBuf> {
        if program.contains('/') {
            return Some(cwd.join(program));
        }
        let path = runner
            .config
            .env
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok())?;
        std::env::split_paths(&path)
            .map(|d| d.join(program))
            .find(|p| p.is_file())
    };
    python_argv(&runner.config.command, &resolve)
}

/// True when the runner's Python environment has a coverage.py that starts itself from
/// `COVERAGE_PROCESS_START` and saves forked children: 7.13 or later. titer reads the
/// environment's installed packages rather than running the command, so a check without
/// coverage.py costs nothing. Without one, titer sets no coverage variables at all.
fn python_supported(runner: &Runner, worktree: &Path) -> bool {
    python_prefixes(runner, worktree)
        .iter()
        .any(|prefix| has_coverage(prefix))
}

/// The environment dirs that the runner's Python may be in: the venv of an interpreter the
/// command names, or for `uv run` the `.venv` beside or above the runner's cwd.
fn python_prefixes(runner: &Runner, worktree: &Path) -> Vec<PathBuf> {
    let Some(argv) = python_for(runner, worktree) else {
        return Vec::new();
    };
    let cwd = runner.cwd_in(worktree);
    let program = Path::new(&argv[0]);
    if program.file_name().is_some_and(|n| n == "uv") {
        if let Some(env) = runner
            .config
            .env
            .get("UV_PROJECT_ENVIRONMENT")
            .cloned()
            .or_else(|| std::env::var("UV_PROJECT_ENVIRONMENT").ok())
        {
            return vec![cwd.join(env)];
        }
        return cwd
            .ancestors()
            .take_while(|d| d.starts_with(worktree))
            .map(|d| d.join(".venv"))
            .collect();
    }
    if argv.len() != 1 {
        return Vec::new();
    }
    let interpreter = if program.components().count() > 1 {
        cwd.join(program)
    } else {
        let path = runner
            .config
            .env
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        match std::env::split_paths(&path)
            .map(|d| d.join(program))
            .find(|p| p.is_file())
        {
            Some(p) => p,
            None => return Vec::new(),
        }
    };
    // `<prefix>/bin/python`, without resolving the venv's symlink to the base interpreter.
    interpreter
        .parent()
        .and_then(Path::parent)
        .map(|p| vec![p.to_path_buf()])
        .unwrap_or_default()
}

/// True when `prefix` has coverage.py 7.13 or later installed.
fn has_coverage(prefix: &Path) -> bool {
    let libs = ["lib", "lib64"]
        .iter()
        .filter_map(|l| std::fs::read_dir(prefix.join(l)).ok())
        .flatten()
        .flatten()
        .map(|e| e.path().join("site-packages"));
    libs.filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let version = name
                .strip_prefix("coverage-")?
                .strip_suffix(".dist-info")?
                .to_string();
            let mut parts = version.split('.').map(|p| p.parse::<u32>().ok());
            Some((parts.next()??, parts.next()??))
        })
        .any(|v| v >= (7, 13))
}

/// True when coverage.py started and wrote data into `dir`.
fn python_started(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("data."))
}

/// Runs the green changed tests `cases` once more with coverage.py, then combines and reads
/// its data. The uninstrumented head run stays the one that decides which tests are green,
/// so a test whose result tracing or the coverage variables change cannot make a mutant
/// look killed.
pub fn collect_python(
    runner: &Runner,
    worktree: &Path,
    dir: &Path,
    cases: &[TestCase],
    head: &TestRun,
    budget: Budget,
) -> Collected {
    let logs = dir.join("logs");
    if cases.is_empty()
        || !budget.deadline.fits(budget.expected)
        || !python_supported(runner, worktree)
    {
        return Collected::Missing;
    }
    let mut measured = runner.clone();
    match python_env(dir) {
        Ok(env) => measured.config.env.extend(env),
        Err(e) => return Collected::Unusable(format!("cannot set up coverage.py: {e:#}")),
    }
    let run = measured.run(&RunRequest {
        worktree,
        selection: Selection::Tests(cases),
        fail_fast: false,
        timeout: Some(budget.timeout),
        deadline: budget.deadline,
        scratch: &logs,
    });
    let run = match run {
        Ok(run) => run,
        Err(e) => return Collected::Unusable(format!("cannot run the coverage run: {e:#}")),
    };
    if !python_started(dir) {
        return Collected::Missing;
    }
    // Each parameter must end the same way as on head, so one that skips only under
    // coverage cannot hide behind another that passes.
    let statuses = |run: &TestRun, case: &TestCase| {
        let mut statuses: Vec<String> = run
            .outcomes_for(case, &runner.config.cwd)
            .iter()
            .map(|o| format!("{:?}", std::mem::discriminant(&o.status)))
            .collect();
        statuses.sort();
        statuses
    };
    if let Some(case) = cases.iter().find(|c| {
        run.result_for(c, &runner.config.cwd) != CaseResult::Passed
            || statuses(&run, c) != statuses(head, c)
    }) {
        return Collected::Unusable(format!(
            "{} has another result under coverage, so mutants run every changed test",
            case.id
        ));
    }
    let Some(python) = python_for(runner, worktree) else {
        return Collected::Missing;
    };
    let rc = dir.join("coveragerc");
    let json = dir.join("coverage.json");
    for args in [
        vec!["combine".to_string(), "-q".to_string()],
        vec![
            "json".to_string(),
            "-q".to_string(),
            "--show-contexts".to_string(),
            "-o".to_string(),
            json.display().to_string(),
        ],
    ] {
        let mut argv = python.clone();
        argv.extend(["-m".to_string(), "coverage".to_string()]);
        argv.extend(args);
        argv.push(format!("--rcfile={}", rc.display()));
        let mut cmd = runner.program(worktree, &argv);
        // COVERAGE_FILE beats the config's data_file, so an inherited one would point
        // coverage.py at data outside the run dir.
        cmd.env_remove("COVERAGE_PROCESS_START")
            .env("COVERAGE_FILE", dir.join("data"));
        match crate::runner::run_limited(cmd, budget.deadline.limit(Some(budget.timeout)), &logs) {
            Ok(out) if out.past_deadline => return Collected::Missing,
            Ok(out) if out.success() => {}
            Ok(out) => {
                return Collected::Unusable(format!(
                    "coverage.py could not read its data, so mutants run every changed test: {}",
                    crate::runner::output_tail(&out)
                ));
            }
            Err(e) => return Collected::Unusable(format!("cannot run coverage.py: {e:#}")),
        }
    }
    let text = std::fs::read_to_string(&json).unwrap_or_default();
    match parse_coverage_py(&text, cases, &roots(worktree), &runner.config.cwd) {
        Ok(c) => Collected::Ready(c),
        Err(e) => Collected::Unusable(e),
    }
}

/// True when a vitest coverage provider is installed where the runner's cwd resolves
/// packages, so a coverage run will not fail to start.
pub fn vitest_provider_installed(runner: &Runner, worktree: &Path) -> bool {
    let cwd = runner.cwd_in(worktree);
    cwd.ancestors()
        .take_while(|d| d.starts_with(worktree))
        .any(|d| {
            ["coverage-v8", "coverage-istanbul"]
                .iter()
                .any(|p| d.join("node_modules/@vitest").join(p).exists())
        })
}

/// Runs the changed tests together with vitest's coverage, which shows the code they never
/// run, and each alone, which shows the tests that run each statement. The runs spread
/// across `worktrees` and stop at the budget.
pub fn collect_vitest(
    runner: &Runner,
    worktrees: &[PathBuf],
    dir: &Path,
    cases: &[TestCase],
    budget: Budget,
    warnings: &mut Vec<String>,
) -> Collected {
    let timeout = budget.timeout;
    if runner.config.kind != RunnerKind::Vitest || cases.is_empty() || worktrees.is_empty() {
        return Collected::Missing;
    }
    // Run 0 selects every case; run i selects case i - 1 alone.
    let alone = if cases.len() > 1 { cases.len() } else { 0 };
    let queue = std::sync::Mutex::new((0..=alone).rev().collect::<Vec<usize>>());
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for worktree in worktrees {
            scope.spawn(|| {
                loop {
                    let Some(i) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
                        return;
                    };
                    // Coverage runs share the mutants' budget and stop where a mutant would.
                    if !budget.deadline.fits(budget.expected) {
                        return;
                    }
                    let selected = if i == 0 { cases } else { &cases[i - 1..i] };
                    let reports = dir.join(format!("run-{i}"));
                    let req = RunRequest {
                        worktree,
                        selection: Selection::Tests(selected),
                        fail_fast: false,
                        timeout: Some(timeout),
                        deadline: budget.deadline,
                        scratch: &dir.join("logs"),
                    };
                    let outcome = crate::runner::vitest::run_with_coverage(runner, &req, &reports)
                        .map_err(|e| format!("{e:#}"))
                        .and_then(|run| {
                            if let Some(case) = selected.iter().find(|c| {
                                run.result_for(c, &runner.config.cwd) != CaseResult::Passed
                            }) {
                                return Err(format!(
                                    "{} did not pass in its coverage run",
                                    case.id
                                ));
                            }
                            std::fs::read_to_string(reports.join("coverage-final.json"))
                                .map_err(|e| format!("a coverage run wrote no report: {e}"))
                        });
                    results.lock().unwrap_or_else(|e| e.into_inner()).push((
                        i,
                        worktree.clone(),
                        outcome,
                    ));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by_key(|(i, _, _)| *i);
    let unusable = |e: String| {
        Collected::Unusable(format!(
            "vitest coverage is unusable, so mutants run every changed test: {e}"
        ))
    };
    // Attribution needs every test's own run.
    let attribute = results.len() == alone + 1;
    if alone > 0 && !attribute {
        warnings.push(
            "the budget ran out before every changed test ran alone with vitest coverage, so mutants run every changed test"
                .to_string(),
        );
    }
    let mut runs = results.into_iter();
    let Some((0, worktree, combined)) = runs.next() else {
        return Collected::Missing;
    };
    let mut coverage = match combined
        .and_then(|text| parse_istanbul(&text, &roots(&worktree), &runner.config.cwd))
    {
        Ok(c) => c,
        Err(e) => return unusable(e),
    };
    let one = cases.len() == 1;
    let mut attributed = one || attribute;
    if one {
        // The run of every changed test is the one test's own run.
        for cover in coverage.files.values_mut() {
            if let FileCover::Statements { statements, .. } = cover {
                for s in statements.iter_mut().filter(|s| s.ran) {
                    s.tests.insert(cases[0].id.clone());
                }
            }
        }
    }
    for (i, worktree, outcome) in runs.filter(|_| attribute) {
        let added = outcome.and_then(|text| {
            attribute_istanbul(
                &mut coverage,
                &text,
                &cases[i - 1].id,
                &roots(&worktree),
                &runner.config.cwd,
            )
        });
        if let Err(e) = added {
            attributed = false;
            warnings.push(format!(
                "vitest coverage cannot attribute statements to tests, so mutants run every changed test: {e}"
            ));
            break;
        }
    }
    if !attributed {
        for cover in coverage.files.values_mut() {
            if let FileCover::Statements { attributed, .. } = cover {
                *attributed = false;
            }
        }
    }
    Collected::Ready(coverage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(file: &str, suite: &[&str], name: &str, id: &str) -> TestCase {
        TestCase {
            runner: "r".into(),
            file: file.into(),
            line: 1,
            end_line: 2,
            suite: suite.iter().map(|s| s.to_string()).collect(),
            name: name.into(),
            id: id.into(),
            skip: false,
        }
    }

    fn ran(ids: &[&str]) -> Cover {
        Cover::Ran(Some(ids.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn contexts_name_tests_by_any_module_suffix_and_qualified_name() {
        let cases = [
            case("tests/test_m.py", &[], "test_f", "f"),
            case("tests/test_m.py", &["TestG"], "test_g", "g"),
            case("tests/other/test_m.py", &[], "test_f", "other_f"),
        ];
        let names = |ctx: &str| -> Vec<&str> {
            tests_for_context(ctx, &cases)
                .iter()
                .map(|c| c.id.as_str())
                .collect()
        };
        // pytest's rootdir import uses the basename; a package import, the dotted path.
        assert_eq!(names("test_m.test_f"), ["f", "other_f"]);
        assert_eq!(names("tests.test_m.test_f"), ["f"]);
        assert_eq!(names("tests.test_m.TestG.test_g"), ["g"]);
        assert!(names("test_m.test_g").is_empty());
        assert!(names("x.test_m.test_f").is_empty());
        assert!(names("").is_empty());
    }

    const COVERAGE_PY: &str = r#"{"files": {
        "pkg/m.py": {"executed_lines": [1, 3, 4, 5, 9], "missing_lines": [6, 12],
            "contexts": {"1": [""], "3": [""], "4": ["test_m.test_f", "test_m.test_p"],
                "5": ["test_m.test_f"], "9": ["", "test_m.test_g"]}},
        "/wt/api/tests/test_m.py": {"executed_lines": [2, 4], "missing_lines": [],
            "contexts": {"2": ["test_m.test_f", "test_m.test_p"], "4": ["test_m.test_g"]}},
        "/elsewhere/lib.py": {"executed_lines": [1], "missing_lines": [], "contexts": {"1": ["x"]}}
    }}"#;

    fn python_cases() -> Vec<TestCase> {
        vec![
            case("api/tests/test_m.py", &[], "test_f", "f"),
            case("api/tests/test_m.py", &[], "test_p", "p"),
            case("api/tests/test_m.py", &[], "test_g", "g"),
        ]
    }

    #[test]
    fn coverage_py_lines_select_the_tests_that_ran_them() {
        let coverage =
            parse_coverage_py(COVERAGE_PY, &python_cases(), &[PathBuf::from("/wt")], "api")
                .unwrap();
        let cover = |line, end| coverage.cover("api/pkg/m.py", line, 0, end);
        assert_eq!(cover(4, 4), ran(&["f", "p"]));
        assert_eq!(cover(5, 5), ran(&["f"]));
        assert_eq!(cover(6, 6), Cover::NotRun);
        assert_eq!(cover(12, 12), Cover::NotRun);
        assert_eq!(cover(4, 6), ran(&["f", "p"]));
        // Import-time and fixture code ran, but may matter to every test.
        assert_eq!(cover(1, 1), Cover::Ran(None));
        assert_eq!(cover(9, 9), Cover::Ran(None));
        assert_eq!(cover(3, 4), Cover::Ran(None));
        // A continuation line, and a file the data does not have.
        assert_eq!(cover(7, 7), Cover::Unknown);
        assert_eq!(coverage.cover("api/pkg/n.py", 1, 0, 1), Cover::Unknown);
        assert!(coverage.files.contains_key("api/tests/test_m.py"));
        assert_eq!(coverage.files.len(), 2);
    }

    #[test]
    fn coverage_py_data_without_every_test_is_unusable() {
        let mut cases = python_cases();
        cases.push(case("api/tests/test_m.py", &[], "test_h", "h"));
        let err =
            parse_coverage_py(COVERAGE_PY, &cases, &[PathBuf::from("/wt")], "api").unwrap_err();
        assert!(err.contains("h"), "{err}");
    }

    /// Line 3 is `  if (x > 3) { return 3; }`, lines 6 to 8 an arrow function in a
    /// declaration, line 10 a statement no test runs, and line 15 one that runs only after
    /// another test ran.
    fn istanbul(counts: [u32; 7]) -> String {
        let spans = [
            (3, 2, 3, "null"),
            (3, 16, 3, "25"),
            (6, 0, 8, "null"),
            (7, 2, 7, "null"),
            (10, 2, 10, "9"),
            (12, 2, 13, "4"),
            (15, 2, 15, "9"),
        ];
        let map: Vec<String> = spans
            .iter()
            .enumerate()
            .map(|(i, (l, c, el, ec))| {
                format!(r#""{i}": {{"start": {{"line": {l}, "column": {c}}}, "end": {{"line": {el}, "column": {ec}}}}}"#)
            })
            .collect();
        let s: Vec<String> = counts
            .iter()
            .enumerate()
            .map(|(i, n)| format!(r#""{i}": {n}"#))
            .collect();
        format!(
            r#"{{"/wt/src/m.ts": {{"statementMap": {{{}}}, "s": {{{}}}}}}}"#,
            map.join(", "),
            s.join(", ")
        )
    }

    #[test]
    fn istanbul_statements_select_by_the_innermost_statement() {
        let roots = [PathBuf::from("/wt")];
        let mut coverage = parse_istanbul(&istanbul([1, 1, 1, 1, 0, 1, 1]), &roots, ".").unwrap();
        attribute_istanbul(
            &mut coverage,
            &istanbul([1, 0, 1, 0, 0, 1, 0]),
            "a",
            &roots,
            ".",
        )
        .unwrap();
        attribute_istanbul(
            &mut coverage,
            &istanbul([1, 1, 1, 1, 0, 0, 0]),
            "b",
            &roots,
            ".",
        )
        .unwrap();
        let cover = |line, column| coverage.cover("src/m.ts", line, column, line);
        // The condition ran in both; the return only in b.
        assert_eq!(cover(3, 6), ran(&["a", "b"]));
        assert_eq!(cover(3, 18), ran(&["b"]));
        assert_eq!(cover(7, 4), ran(&["b"]));
        assert_eq!(cover(6, 10), ran(&["a", "b"]));
        assert_eq!(cover(10, 3), Cover::NotRun);
        assert_eq!(cover(13, 2), ran(&["a"]));
        // Alone, neither test reaches line 15, but together they do.
        assert_eq!(cover(15, 3), Cover::Ran(None));
        // Outside every statement, past a statement's end, or over several lines.
        assert_eq!(cover(1, 0), Cover::Unknown);
        assert_eq!(cover(10, 9), Cover::Unknown);
        assert_eq!(coverage.cover("src/m.ts", 12, 3, 13), Cover::Unknown);
    }

    #[test]
    fn istanbul_reports_without_counters_say_nothing() {
        let roots = [PathBuf::from("/wt")];
        let empty = istanbul([0; 7]).replace(
            r#""s": {"0": 0, "1": 0, "2": 0, "3": 0, "4": 0, "5": 0, "6": 0}"#,
            r#""s": {}"#,
        );
        assert!(empty.contains(r#""s": {}"#));
        let coverage = parse_istanbul(&empty, &roots, ".").unwrap();
        assert_eq!(coverage.cover("src/m.ts", 10, 3, 10), Cover::Unknown);
        // A test's own report without counters leaves the file unattributed.
        let mut coverage = parse_istanbul(&istanbul([1, 1, 1, 1, 0, 1, 1]), &roots, ".").unwrap();
        attribute_istanbul(&mut coverage, &empty, "a", &roots, ".").unwrap();
        assert_eq!(coverage.cover("src/m.ts", 3, 18, 3), Cover::Ran(None));
        assert_eq!(coverage.cover("src/m.ts", 10, 3, 10), Cover::NotRun);
    }

    #[test]
    fn coverage_py_counts_only_from_7_13_in_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path().join("lib/python3.12/site-packages");
        std::fs::create_dir_all(site.join("coverage-7.10.4.dist-info")).unwrap();
        assert!(!has_coverage(dir.path()));
        std::fs::create_dir_all(site.join("coverage-7.17.5.dist-info")).unwrap();
        assert!(has_coverage(dir.path()));
        assert!(!has_coverage(&dir.path().join("missing")));
    }

    #[test]
    fn an_untaken_ternary_arm_did_not_run() {
        let report = r#"{"/wt/m.ts": {
            "statementMap": {"0": {"start": {"line": 1, "column": 0}, "end": {"line": 3, "column": null}}},
            "s": {"0": 1},
            "branchMap": {
                "0": {"type": "cond-expr", "locations": [
                    {"start": {"line": 2, "column": 4}, "end": {"line": 2, "column": 9}},
                    {"start": {"line": 3, "column": 4}, "end": {"line": 3, "column": 9}}]},
                "1": {"type": "if", "locations": [
                    {"start": {"line": 1, "column": 0}, "end": {"line": 3, "column": null}}]}
            },
            "b": {"0": [1, 0], "1": [0]}
        }}"#;
        let coverage = parse_istanbul(report, &[PathBuf::from("/wt")], ".").unwrap();
        assert_eq!(coverage.cover("m.ts", 2, 5, 2), Cover::Ran(None));
        assert_eq!(coverage.cover("m.ts", 3, 5, 3), Cover::NotRun);
        // An `if` branch's range is the whole statement, so its counter is ignored.
        assert_eq!(coverage.cover("m.ts", 1, 2, 1), Cover::Ran(None));
    }

    #[test]
    fn the_python_comes_from_the_command() {
        let none = |_: &str| None;
        let argv = |c: &[&str]| -> Vec<String> { c.iter().map(|s| s.to_string()).collect() };
        assert_eq!(
            python_argv(&argv(&[".venv/bin/python", "-m", "pytest", "-x"]), &none),
            Some(argv(&[".venv/bin/python"]))
        );
        assert_eq!(
            python_argv(&argv(&["uv", "run", "python", "-m", "unittest"]), &none),
            Some(argv(&["uv", "run", "python"]))
        );
        assert_eq!(
            python_argv(&argv(&["uv", "run", "--no-sync", "pytest"]), &none),
            Some(argv(&["uv", "run", "--no-sync", "python"]))
        );
        assert_eq!(python_argv(&argv(&["make", "test"]), &none), None);
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("pytest");
        std::fs::write(&script, "#!/opt/venv/bin/python3\nimport sys\n").unwrap();
        let resolve = |_: &str| Some(script.clone());
        assert_eq!(
            python_argv(&argv(&["pytest"]), &resolve),
            Some(argv(&["/opt/venv/bin/python3"]))
        );
        std::fs::write(&script, "#!/usr/bin/env -S python3\n").unwrap();
        assert_eq!(
            python_argv(&argv(&["pytest"]), &resolve),
            Some(argv(&["python3"]))
        );
    }
}
