//! The mutate tier: mutate changed source lines and run the selected tests against each
//! mutant, one process per mutant, in parallel worktrees.

pub mod agent;
pub mod operators;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::bail;

use crate::check::Context;
use crate::detect::{self, glob_set};
use crate::diff::FileChange;
use crate::lang::Lang;
use crate::prove::HeadRun;
use crate::report::{Mutant, MutantOutcome, MutantSource};
use crate::runner::{CaseResult, RunRequest, Runner, Selection, Status, TestCase, TestRun};
use crate::session::Session;
use crate::worktree::Worktree;

use operators::{CallFilter, Candidate, CodeNode};

/// What each runner's mutants run: the head-green changed tests, or the whole suite when the
/// change has no changed tests for that runner.
#[derive(Debug, Clone)]
pub enum Plan {
    Tests(Vec<TestCase>),
    /// Whole files, for the other-tests rerun of survivors.
    Files(Vec<String>),
    Suite,
    /// The runner has changed tests but none is green on head.
    Nothing,
}

#[derive(Debug, Clone)]
pub struct RunnerPlan {
    pub plan: Plan,
    /// How long the selection took on head; mutants time out at 3x this plus 10 s.
    pub head_duration: Duration,
    pub peak_rss: u64,
}

impl RunnerPlan {
    pub fn timeout(&self) -> Duration {
        self.head_duration * 3 + Duration::from_secs(10)
    }
}

/// A mutant to run, with its source file before and after the edit.
#[derive(Debug, Clone)]
pub struct Job {
    pub runner: usize,
    pub file: String,
    pub original: String,
    pub mutated: String,
}

/// The outcome of one job.
#[derive(Debug, Clone)]
pub struct JobResult {
    pub outcome: MutantOutcome,
    pub killed_by: Option<String>,
    pub duration: Option<Duration>,
    /// The mutated code did not compile. Such a mutant has no result, but a compile-error
    /// canary that fails the build shows that the build compiles its file.
    pub compile_failed: bool,
}

pub struct Mutation {
    pub mutants: Vec<Mutant>,
    pub workers: u32,
    pub warnings: Vec<String>,
}

/// Builds each runner's plan, running the suite on head where a runner has no changed tests.
pub fn plan(
    ctx: &Context,
    heads: &[HeadRun],
    head: &Worktree,
    runners: &[usize],
) -> anyhow::Result<BTreeMap<usize, RunnerPlan>> {
    let mut plans = BTreeMap::new();
    let mut runners = runners.to_vec();
    runners.sort();
    runners.dedup();
    for runner in runners {
        if let Some(h) = heads.iter().find(|h| h.runner == runner) {
            let green = h.green();
            plans.insert(
                runner,
                RunnerPlan {
                    plan: if green.is_empty() {
                        Plan::Nothing
                    } else {
                        Plan::Tests(green)
                    },
                    head_duration: h.run.duration,
                    peak_rss: h.run.max_rss_bytes,
                },
            );
            continue;
        }
        let r = &ctx.runners[runner];
        let run = r.run(&RunRequest {
            worktree: &head.path,
            selection: Selection::Suite,
            fail_fast: false,
            timeout: Some(ctx.budget.max(Duration::from_secs(300))),
            scratch: &ctx.session.run_dir.join("logs"),
        })?;
        let failed: Vec<String> = run
            .outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Failed(_)))
            .map(|o| o.name.clone())
            .chain(run.file_errors.iter().map(|e| e.file.clone()))
            .collect();
        if run.timed_out || !failed.is_empty() || run.runner_error.is_some() {
            bail!(
                "head is not green: the change has no changed tests for runner {:?}, and its suite fails on head{}",
                r.name(),
                if failed.is_empty() {
                    String::new()
                } else {
                    format!(": {}", failed.join(", "))
                }
            );
        }
        plans.insert(
            runner,
            RunnerPlan {
                plan: if run.executed() == 0 {
                    Plan::Nothing
                } else {
                    Plan::Suite
                },
                head_duration: run.duration,
                peak_rss: run.max_rss_bytes,
            },
        );
    }
    Ok(plans)
}

/// Generated mutants for every changed source line, in budget order.
pub fn generate(ctx: &Context) -> anyhow::Result<Vec<(Mutant, Job)>> {
    let config = &ctx.session.config.mutate;
    let excluded_files = glob_set(&config.exclude_globs)?;
    let calls = CallFilter {
        excluded: config.exclude_calls.clone(),
    };
    let mut all = Vec::new();
    for source in &ctx.detection.sources {
        if excluded_files.is_match(&source.file) {
            continue;
        }
        let text = detect::read_head(&ctx.session, &source.file)?;
        let base = BaseHunks::load(&ctx.session, &source.file, source.lang)?;
        // Test code that shares a changed line with source code is not a target.
        let tests = if source.lang == Lang::Rust {
            crate::lang::rust::test_ranges(&text)
        } else {
            Vec::new()
        };
        for cand in operators::generate(
            source.lang,
            &text,
            &source.changed,
            &calls,
            &config.exclude_operators,
        ) {
            if tests
                .iter()
                .any(|(s, e)| cand.start_byte < *e && *s < cand.end_byte)
            {
                continue;
            }
            let mutated = cand.apply(&text);
            let mut mutant = mutant_from(
                &ctx.runners[source.runner],
                &source.file,
                &cand,
                MutantSource::Generated,
            );
            mutant.unchanged_from_base = base.contains(cand.line, &cand.context);
            all.push((
                mutant,
                Job {
                    runner: source.runner,
                    file: source.file.clone(),
                    original: text.clone(),
                    mutated,
                },
            ));
        }
    }
    Ok(operators::budget_order(all, |(m, _)| {
        (m.file.clone(), m.line)
    }))
}

pub fn mutant_from(runner: &Runner, file: &str, cand: &Candidate, source: MutantSource) -> Mutant {
    let op = serde_json::to_value(cand.operator)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    Mutant {
        id: format!("{file}:{}:{}:{op}", cand.line, cand.column),
        runner: runner.name().to_string(),
        file: file.to_string(),
        line: cand.line,
        end_line: cand.end_line,
        operator: cand.operator,
        source,
        original: cand.original.clone(),
        replacement: cand.replacement.clone(),
        outcome: MutantOutcome::NotRun,
        killed_by: None,
        duration_ms: None,
        unchanged_from_base: false,
    }
}

/// The base side of one source file's hunks, for telling whether a mutant's code is new.
pub struct BaseHunks {
    change: Option<FileChange>,
    base: String,
    tree: Option<tree_sitter::Tree>,
}

impl BaseHunks {
    pub fn load(session: &Session, file: &str, lang: Lang) -> anyhow::Result<BaseHunks> {
        let change = session.changes.get(file).cloned();
        let base = match change.as_ref().and_then(|c| c.old_path.as_deref()) {
            Some(old) => session
                .git
                .read_blob(&session.base_sha, old)?
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default(),
            None => String::new(),
        };
        Ok(BaseHunks::new(change, lang, base))
    }

    pub fn new(change: Option<FileChange>, lang: Lang, base: String) -> BaseHunks {
        let tree = lang.parse(&base);
        BaseHunks { change, base, tree }
    }

    /// Whether a node in the base lines that the hunk of head `line` replaces has the same
    /// kind, parent kind, and tokens as `context`. Then the change only moved or reformatted
    /// that code.
    pub fn contains(&self, line: u32, context: &CodeNode) -> bool {
        let (Some(change), Some(tree)) = (&self.change, &self.tree) else {
            return false;
        };
        let replaced = change.replaced(line);
        let (Some(&first), Some(&last)) = (replaced.first(), replaced.last()) else {
            return false;
        };
        let mut found = false;
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            let start = node.start_position().row as u32 + 1;
            let end = node.end_position().row as u32 + 1;
            if end < first || start > last {
                continue;
            }
            if start >= first && end <= last && CodeNode::of(node, &self.base) == *context {
                found = true;
                break;
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        found
    }
}

/// Mutation workers: the smaller of cores minus 1 and 70% of available memory over the
/// head run's peak RSS, at least 1.
pub fn worker_count(peak_rss: u64) -> u32 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(2);
    let by_cores = cores.saturating_sub(1).max(1);
    let by_memory = match (available_memory(), peak_rss) {
        (Some(avail), rss) if rss > 0 => (avail as f64 * 0.7 / rss as f64) as u64,
        _ => by_cores,
    };
    by_cores.min(by_memory).max(1) as u32
}

/// Available memory in bytes: MemAvailable on Linux, free plus inactive pages on macOS.
pub fn available_memory() -> Option<u64> {
    if cfg!(target_os = "linux") {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = info.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        return Some(kb * 1024);
    }
    let out = std::process::Command::new("vm_stat").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let page = text
        .lines()
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    let pages = |label: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(label))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().trim_end_matches('.').parse().ok())
            .unwrap_or(0)
    };
    Some((pages("Pages free") + pages("Pages inactive") + pages("Pages speculative")) * page)
}

/// Runs `jobs` across the workers' worktrees, skipping jobs the deadline cannot fit.
/// Results come back in job order; a skipped job is `NotRun`.
pub fn run_jobs(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    jobs: &[Job],
    deadline: Option<Instant>,
) -> Vec<JobResult> {
    let queue: Mutex<VecDeque<usize>> = Mutex::new((0..jobs.len()).collect());
    let results: Mutex<Vec<Option<JobResult>>> = Mutex::new(vec![None; jobs.len()]);
    // Total duration and count of finished runs, per runner.
    let observed: Mutex<BTreeMap<usize, (Duration, u32)>> = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for worktree in workers {
            scope.spawn(|| {
                loop {
                    let Some(i) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
                    else {
                        return;
                    };
                    let job = &jobs[i];
                    let plan = &plans[&job.runner];
                    // Parallel runs are slower than the lone head run, so admit a job by
                    // the longer of the head run and the mean mutant run so far.
                    let expected = plan.head_duration.max(mean(&observed, job.runner));
                    let fits = deadline.is_none_or(|d| Instant::now() + expected <= d);
                    let (result, healthy) = if fits {
                        run_job(ctx, worktree, plan, job)
                    } else {
                        let skipped = JobResult {
                            outcome: MutantOutcome::NotRun,
                            killed_by: None,
                            duration: None,
                            compile_failed: false,
                        };
                        (skipped, true)
                    };
                    if let Some(d) = result.duration {
                        let mut observed = observed.lock().unwrap_or_else(|e| e.into_inner());
                        let entry = observed.entry(job.runner).or_insert((Duration::ZERO, 0));
                        entry.0 += d;
                        entry.1 += 1;
                    }
                    results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(result);
                    if !healthy {
                        // The worktree may still hold a mutant; later jobs must not run in it.
                        return;
                    }
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|r| {
            r.unwrap_or(JobResult {
                outcome: MutantOutcome::NotRun,
                killed_by: None,
                duration: None,
                compile_failed: false,
            })
        })
        .collect()
}

/// Runs one job and restores the file. The flag is false when the original could not be
/// restored, so the worktree must not take more jobs.
fn mean(observed: &Mutex<BTreeMap<usize, (Duration, u32)>>, runner: usize) -> Duration {
    let observed = observed.lock().unwrap_or_else(|e| e.into_inner());
    match observed.get(&runner) {
        Some((total, n)) if *n > 0 => *total / *n,
        _ => Duration::ZERO,
    }
}

fn run_job(ctx: &Context, worktree: &Worktree, plan: &RunnerPlan, job: &Job) -> (JobResult, bool) {
    let runner = &ctx.runners[job.runner];
    let selection = match &plan.plan {
        Plan::Tests(cases) => Selection::Tests(cases),
        Plan::Files(files) => Selection::Files(files),
        Plan::Suite => Selection::Suite,
        Plan::Nothing => {
            let nothing = JobResult {
                outcome: MutantOutcome::NoResult,
                killed_by: None,
                duration: None,
                compile_failed: false,
            };
            return (nothing, true);
        }
    };
    if let Err(e) = worktree.write(&job.file, job.mutated.as_bytes()) {
        let restored = worktree.restore(&job.file, job.original.as_bytes()).is_ok();
        return (error_result(e), restored);
    }
    let run = runner.run(&RunRequest {
        worktree: &worktree.path,
        selection,
        fail_fast: true,
        timeout: Some(plan.timeout()),
        scratch: &ctx
            .session
            .run_dir
            .join(&format!("logs-{}", worktree_name(worktree))),
    });
    if let Err(e) = worktree.restore(&job.file, job.original.as_bytes()) {
        return (
            error_result(e.context("cannot restore the mutated file")),
            false,
        );
    }
    let run = match run {
        Ok(run) => run,
        Err(e) => return (error_result(e), true),
    };
    let (outcome, killed_by) = classify(&run, &plan.plan, &runner.config.cwd);
    let result = JobResult {
        outcome,
        killed_by,
        duration: Some(run.duration),
        compile_failed: !run.compile_errors.is_empty(),
    };
    (result, true)
}

fn worktree_name(worktree: &Worktree) -> String {
    worktree
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn error_result(e: anyhow::Error) -> JobResult {
    eprintln!("titer: a mutant run failed: {e:#}");
    JobResult {
        outcome: MutantOutcome::NoResult,
        killed_by: None,
        duration: None,
        compile_failed: false,
    }
}

/// Killed when a selected test fails or the mutant breaks a selected file's import; survived
/// when every selected test ran and passed and every selected target built; no result
/// otherwise. A target that does not build ran none of its tests, so a pass elsewhere is
/// not a survivor.
pub fn classify(run: &TestRun, plan: &Plan, cwd: &str) -> (MutantOutcome, Option<String>) {
    if run.timed_out {
        return (MutantOutcome::TimedOut, None);
    }
    match plan {
        Plan::Tests(cases) => {
            let results: Vec<(&TestCase, CaseResult)> =
                cases.iter().map(|c| (c, run.result_for(c, cwd))).collect();
            if let Some((case, _)) = results
                .iter()
                .find(|(_, r)| matches!(r, CaseResult::Failed(_)))
            {
                return (MutantOutcome::Killed, Some(case.id.clone()));
            }
            // A file-level failure in a selected file, such as a throwing afterAll or a failed
            // setUpClass, fails those tests even when their bodies passed.
            let rel = |c: &TestCase| crate::runner::cwd_relative(&c.file, cwd);
            if let Some(e) = run.file_errors.iter().find(|e| {
                cases
                    .iter()
                    .any(|c| crate::runner::same_file(&e.file, &rel(c)))
            }) {
                return (
                    MutantOutcome::Killed,
                    Some(e.file.trim_start_matches("./").to_string()),
                );
            }
            // Every selected test must run and pass: a mutant that hides a test, for
            // example by turning off its cfg, has not shown that the tests miss it.
            if results.iter().all(|(_, r)| *r == CaseResult::Passed) && !run.incomplete {
                return (MutantOutcome::Survived, None);
            }
        }
        Plan::Suite | Plan::Files(_) => {
            if let Some(o) = run
                .outcomes
                .iter()
                .find(|o| matches!(o.status, Status::Failed(_)))
            {
                // The runner's own id when it reports one (unittest), else file::suite::name.
                let killer = o.id.clone().unwrap_or_else(|| {
                    let mut parts = vec![o.file.trim_start_matches("./").to_string()];
                    parts.extend(o.suite.iter().cloned());
                    parts.push(o.name.clone());
                    parts.join("::")
                });
                return (MutantOutcome::Killed, Some(killer));
            }
            if let Some(e) = run.file_errors.first() {
                return (MutantOutcome::Killed, Some(e.file.clone()));
            }
            if run.executed() > 0 && !run.incomplete {
                return (MutantOutcome::Survived, None);
            }
        }
        Plan::Nothing => {}
    }
    (MutantOutcome::NoResult, None)
}

/// Runs a canary in every mutated module. Returns the modules whose canary survived, or an
/// error when every canary survived, which means the worktrees do not load the code. A
/// Rust canary is a compile error, so a build that fails counts as killed.
pub fn canaries(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    files: &[(usize, String, String)],
) -> anyhow::Result<Vec<String>> {
    let files: Vec<&(usize, String, String)> = files
        .iter()
        .filter(|(runner, _, _)| !matches!(plans[runner].plan, Plan::Nothing))
        .collect();
    let markers: Vec<PathBuf> = (0..files.len())
        .map(|i| ctx.session.run_dir.join(&format!("canary-{i}.loaded")))
        .collect();
    let jobs: Vec<Job> = files
        .iter()
        .zip(&markers)
        .map(|((runner, file, original), marker)| Job {
            runner: *runner,
            file: file.clone(),
            original: original.clone(),
            mutated: operators::canary(
                Lang::of(file).expect("source files have a language"),
                original,
                marker,
            ),
        })
        .collect();
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let mut results = run_jobs(ctx, workers, plans, &jobs, None);
    for r in results.iter_mut().filter(|r| r.compile_failed) {
        r.outcome = MutantOutcome::Killed;
    }
    // A canary that survived after its marker appeared ran and was caught by a test, so the
    // worktree does load the module.
    for (r, marker) in results.iter_mut().zip(&markers) {
        if r.outcome == MutantOutcome::Survived && marker.exists() {
            r.outcome = MutantOutcome::Killed;
        }
    }
    let unconfirmed: Vec<&str> = jobs
        .iter()
        .zip(&results)
        .filter(|(_, r)| !matches!(r.outcome, MutantOutcome::Killed | MutantOutcome::Survived))
        .map(|(j, _)| j.file.as_str())
        .collect();
    if !unconfirmed.is_empty() {
        bail!(
            "the canary neither failed nor passed the selected tests in {}: titer cannot confirm that the worktree loads the code it mutates",
            unconfirmed.join(", ")
        );
    }
    let survived: Vec<String> = jobs
        .iter()
        .zip(&results)
        .filter(|(_, r)| r.outcome == MutantOutcome::Survived)
        .map(|(j, _)| j.file.clone())
        .collect();
    if survived.len() == jobs.len() {
        bail!(
            "the canary survived in every mutated module ({}): the worktree is not loading the code titer mutates. \
             An editable install, a PYTHONPATH that points at the checkout, or a Cargo path dependency outside the repo can cause this.",
            survived.join(", ")
        );
    }
    Ok(survived)
}

/// The mutate tier: plans, canaries, then mutants in budget order until the deadline.
pub fn run(
    ctx: &Context,
    heads: &[HeadRun],
    head: Worktree,
    extra: Vec<(Mutant, Job)>,
) -> anyhow::Result<Mutation> {
    let mut warnings = Vec::new();
    let mut pairs = generate(ctx)?;
    pairs.extend(extra);
    let pairs = operators::budget_order(pairs, |(m, _)| (m.file.clone(), m.line));
    if pairs.is_empty() {
        // Still confirm that head is green where the suite stands in for changed tests.
        let runners: Vec<usize> = ctx.detection.sources.iter().map(|s| s.runner).collect();
        plan(ctx, heads, &head, &runners)?;
        return Ok(Mutation {
            mutants: Vec::new(),
            workers: 0,
            warnings,
        });
    }
    let runners: Vec<usize> = pairs.iter().map(|(_, j)| j.runner).collect();
    let plans = plan(ctx, heads, &head, &runners)?;
    let peak = plans.values().map(|p| p.peak_rss).max().unwrap_or(0);
    let workers = ctx
        .options
        .workers
        .unwrap_or_else(|| worker_count(peak))
        .clamp(1, pairs.len().max(1) as u32);
    let mut worktrees = vec![head];
    worktrees.extend(Worktree::create_many(
        &ctx.session.git,
        &ctx.session.head_tree,
        ctx.session.run_dir.path(),
        "worker",
        workers as usize - 1,
        &ctx.session.config.runners,
    )?);

    let mut modules: Vec<(usize, String, String)> = Vec::new();
    for (_, job) in &pairs {
        if !modules.iter().any(|(_, f, _)| f == &job.file) {
            modules.push((job.runner, job.file.clone(), job.original.clone()));
        }
    }
    let unloaded = canaries(ctx, &worktrees, &plans, &modules)?;
    if !unloaded.is_empty() {
        warnings.push(format!(
            "the selected tests never load {}; its mutants are reported as not executed",
            unloaded.join(", ")
        ));
    }

    let (to_run, skipped): (Vec<usize>, Vec<usize>) =
        (0..pairs.len()).partition(|i| !unloaded.contains(&pairs[*i].1.file));
    let jobs: Vec<Job> = to_run.iter().map(|i| pairs[*i].1.clone()).collect();
    let deadline = ctx.started + ctx.budget;
    let results = run_jobs(ctx, &worktrees, &plans, &jobs, Some(deadline));
    let mut mutants: Vec<Mutant> = pairs.iter().map(|(m, _)| m.clone()).collect();
    for (i, result) in to_run.iter().zip(results) {
        let m = &mut mutants[*i];
        m.outcome = result.outcome;
        m.killed_by = result.killed_by;
        m.duration_ms = result.duration.map(|d| d.as_millis() as u64);
    }
    for i in skipped {
        mutants[i].outcome = MutantOutcome::NotExecuted;
    }
    follow_up(
        ctx,
        &worktrees,
        &plans,
        &pairs,
        &mut mutants,
        deadline,
        &mut warnings,
    )?;
    for (runner, plan) in &plans {
        if matches!(plan.plan, Plan::Nothing) {
            warnings.push(format!(
                "runner {:?} has no green selected test on head, so its mutants have no result",
                ctx.runners[*runner].name()
            ));
        }
    }
    Ok(Mutation {
        mutants,
        workers,
        warnings,
    })
}

/// Survivor follow-up. First the sentinel: a raise before the statement that encloses each
/// surviving line, or inside an `elif` or `else if` condition. If the selected tests still pass, no green selected test runs that line,
/// and its survivors are not executed. Then the other tests: the remaining survivors run
/// against the whole changed test files, and a kill there is caught by other tests.
fn follow_up(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    pairs: &[(Mutant, Job)],
    mutants: &mut [Mutant],
    deadline: Instant,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let survivors = |mutants: &[Mutant]| -> Vec<usize> {
        (0..mutants.len())
            .filter(|i| mutants[*i].outcome == MutantOutcome::Survived)
            .collect()
    };
    // One sentinel per surviving line.
    let mut lines: Vec<(usize, String, u32)> = Vec::new();
    // A sentinel probes one line, so a mutant that spans lines, such as a multi-line agent
    // mutant, skips this step and stays a survivor.
    for i in survivors(mutants) {
        if mutants[i].end_line != mutants[i].line {
            continue;
        }
        let key = (pairs[i].1.runner, mutants[i].file.clone(), mutants[i].line);
        if !lines.contains(&key) {
            lines.push(key);
        }
    }
    let mut jobs = Vec::new();
    let mut job_lines = Vec::new();
    for (runner, file, line) in &lines {
        let original = &pairs
            .iter()
            .find(|(m, _)| &m.file == file)
            .expect("a survivor has a job")
            .1
            .original;
        let Some(lang) = Lang::of(file) else { continue };
        if let Some(mutated) = operators::sentinel(lang, original, *line) {
            jobs.push(Job {
                runner: *runner,
                file: file.clone(),
                original: original.clone(),
                mutated,
            });
            job_lines.push((file.clone(), *line));
        }
    }
    let results = run_jobs(ctx, workers, plans, &jobs, Some(deadline));
    for ((file, line), result) in job_lines.iter().zip(&results) {
        if result.outcome != MutantOutcome::Survived {
            continue;
        }
        for m in mutants.iter_mut() {
            if &m.file == file
                && m.line == *line
                && m.end_line == *line
                && m.outcome == MutantOutcome::Survived
            {
                m.outcome = MutantOutcome::NotExecuted;
            }
        }
    }

    // The other tests in the changed test files, for runners that ran only changed tests.
    let remaining = survivors(mutants);
    let mut file_plans: BTreeMap<usize, RunnerPlan> = BTreeMap::new();
    for (runner, plan) in plans {
        if !matches!(plan.plan, Plan::Tests(_))
            || !remaining.iter().any(|i| pairs[*i].1.runner == *runner)
        {
            continue;
        }
        let files: Vec<String> = ctx
            .detection
            .test_files
            .iter()
            .filter(|(r, _)| r == runner)
            .map(|(_, f)| f.clone())
            .collect();
        if files.is_empty() || Instant::now() >= deadline {
            continue;
        }
        let r = &ctx.runners[*runner];
        let head = r.run(&RunRequest {
            worktree: &workers[0].path,
            selection: Selection::Files(&files),
            fail_fast: false,
            timeout: Some(plan.timeout()),
            scratch: &ctx.session.run_dir.join("logs"),
        })?;
        let red = head.timed_out
            || head.runner_error.is_some()
            || !head.file_errors.is_empty()
            || head
                .outcomes
                .iter()
                .any(|o| matches!(o.status, Status::Failed(_)));
        if red {
            warnings.push(format!(
                "the changed test files of runner {:?} are not green on head, so survivors were not rerun against their other tests",
                r.name()
            ));
            continue;
        }
        file_plans.insert(
            *runner,
            RunnerPlan {
                plan: Plan::Files(files),
                head_duration: head.duration,
                peak_rss: head.max_rss_bytes,
            },
        );
    }
    let rerun: Vec<usize> = remaining
        .into_iter()
        .filter(|i| file_plans.contains_key(&pairs[*i].1.runner))
        .collect();
    let jobs: Vec<Job> = rerun.iter().map(|i| pairs[*i].1.clone()).collect();
    let results = run_jobs(ctx, workers, &file_plans, &jobs, Some(deadline));
    for (i, result) in rerun.iter().zip(results) {
        if result.outcome == MutantOutcome::Killed {
            mutants[*i].outcome = MutantOutcome::CaughtByOther;
            mutants[*i].killed_by = result.killed_by;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(base: &str, head: &str, lang: Lang) -> Vec<(u32, String, bool)> {
        let patch = format!(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n{}",
            hunks(base, head)
        );
        let change = crate::diff::ChangeSet::parse(&patch).files.pop();
        let changed = change.as_ref().unwrap().added.clone();
        let hunks = BaseHunks::new(change, lang, base.to_string());
        operators::generate(lang, head, &changed, &CallFilter::default(), &[])
            .into_iter()
            .map(|c| {
                (
                    c.line,
                    c.original.clone(),
                    hunks.contains(c.line, &c.context),
                )
            })
            .collect()
    }

    /// One hunk that replaces every base line with every head line: enough for these tests.
    fn hunks(base: &str, head: &str) -> String {
        let (b, h) = (base.lines().count(), head.lines().count());
        let mut out = format!("@@ -1,{b} +1,{h} @@\n");
        for l in base.lines() {
            out.push_str(&format!("-{l}\n"));
        }
        for l in head.lines() {
            out.push_str(&format!("+{l}\n"));
        }
        out
    }

    #[test]
    fn code_the_change_only_reformats_matches_base() {
        let base = "const a = <p className={LARGE}>{t}</p>;\n";
        let head = "const a = (\n  <p lang={lang} className={LARGE}>\n    {t}\n  </p>\n);\n";
        assert_eq!(
            matches(base, head, Lang::Tsx),
            vec![
                (2, "lang={lang}".into(), false),
                (2, "className={LARGE}".into(), true)
            ]
        );
        let base = "def f(x, hi):\n    if x > hi: return hi\n";
        let head = "def f(x, hi):\n    if x > hi:\n        return hi\n";
        assert!(
            matches(base, head, Lang::Python).iter().all(|m| m.2),
            "{:?}",
            matches(base, head, Lang::Python)
        );
    }

    #[test]
    fn comments_larger_expressions_and_literals_do_not_match() {
        let cases = [
            (
                "def f(x):\n    # return x > 2\n    pass\n",
                "def f(x):\n    return x > 2\n",
            ),
            (
                "def f(x):\n    return x > 2 + 3\n",
                "def f(x):\n    return x > 2\n",
            ),
            (
                "def f(x):\n    return x > 'a b'\n",
                "def f(x):\n    return x > 'a  b'\n",
            ),
            (
                "def f(x, y):\n    if y and x:\n        pass\n",
                "def f(x, y):\n    if x:\n        pass\n",
            ),
        ];
        for (base, head) in cases {
            let got = matches(base, head, Lang::Python);
            assert!(!got.is_empty(), "{head}");
            assert!(
                got.iter().filter(|m| m.0 > 1).all(|m| !m.2),
                "{head}: {got:?}"
            );
        }
    }

    #[test]
    fn workers_are_bounded_by_cores_and_memory() {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(2);
        assert_eq!(worker_count(0), cores.saturating_sub(1).max(1));
        assert_eq!(worker_count(u64::MAX / 2), 1);
        if let Some(avail) = available_memory() {
            let expected = ((avail as f64 * 0.7) / (avail as f64 / 2.0)) as u32;
            assert_eq!(
                worker_count(avail / 2),
                expected.min(cores.saturating_sub(1)).max(1)
            );
        }
    }
}
