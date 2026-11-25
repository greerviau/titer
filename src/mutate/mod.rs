//! The mutate tier: mutate changed source lines and run the selected tests against each
//! mutant, one process per mutant, in parallel worktrees.

pub mod agent;
pub mod operators;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;

use crate::check::Context;
use crate::config::RunnerKind;
use crate::coverage::{Collected, Cover, TestCoverage};
use crate::deadline::Deadline;
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
    /// Per-test coverage of the plan's tests, when the runner's tooling records it.
    pub coverage: Option<TestCoverage>,
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
    /// The changed tests that coverage shows run the mutant's code. None runs the plan.
    pub tests: Option<Vec<TestCase>>,
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
                    coverage: None,
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
            // Head must be green before any mutant means anything, so this head run is not
            // cut short at the deadline.
            deadline: Deadline::NONE,
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
                coverage: None,
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
                    tests: None,
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

/// Runs `jobs` across the workers' worktrees, skipping jobs the deadline cannot fit and
/// killing a job still running at the deadline. Results come back in job order; a skipped or
/// killed job is `NotRun`.
pub fn run_jobs(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    jobs: &[Job],
    deadline: Deadline,
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
                    let (result, healthy) = if deadline.fits(expected) {
                        run_job(ctx, worktree, plan, job, deadline)
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

fn mean(observed: &Mutex<BTreeMap<usize, (Duration, u32)>>, runner: usize) -> Duration {
    let observed = observed.lock().unwrap_or_else(|e| e.into_inner());
    match observed.get(&runner) {
        Some((total, n)) if *n > 0 => *total / *n,
        _ => Duration::ZERO,
    }
}

/// Runs one job and restores the file. The flag is false when the original could not be
/// restored, so the worktree must not take more jobs.
fn run_job(
    ctx: &Context,
    worktree: &Worktree,
    plan: &RunnerPlan,
    job: &Job,
    deadline: Deadline,
) -> (JobResult, bool) {
    let runner = &ctx.runners[job.runner];
    let covering = job.tests.clone().map(Plan::Tests);
    let tests = covering.as_ref().unwrap_or(&plan.plan);
    let selection = match tests {
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
        deadline,
        scratch: &ctx
            .session
            .run_dir
            .join(&format!("logs-{}", worktree_name(worktree))),
    });
    if let Err(e) = worktree.restore(&job.file, job.original.as_bytes()) {
        let mut result = error_result(e.context("cannot restore the mutated file"));
        if run.as_ref().is_ok_and(|r| r.past_deadline) {
            result.outcome = MutantOutcome::NotRun;
        }
        return (result, false);
    }
    let run = match run {
        Ok(run) => run,
        Err(e) => return (error_result(e), true),
    };
    let (outcome, killed_by) = classify(&run, tests, &runner.config.cwd);
    let result = JobResult {
        outcome,
        killed_by,
        // A run the deadline cut short has no result, and no duration either.
        duration: (!run.past_deadline).then_some(run.duration),
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
    if run.past_deadline {
        return (MutantOutcome::NotRun, None);
    }
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

/// The modules whose canary did not confirm that the worktree loads them.
#[derive(Debug, Default)]
pub struct Canaries {
    /// The canary survived: the selected tests never load the module.
    pub unloaded: Vec<String>,
    /// The deadline left no time for the canary, so the module's mutants are not run.
    pub not_run: Vec<String>,
}

/// Runs a canary in every mutated module before the deadline. Returns the modules whose
/// canary survived or did not run, or an error when every canary survived, which means the
/// worktrees do not load the code. A Rust canary is a compile error, so a build that fails
/// counts as killed.
pub fn canaries(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    files: &[(usize, String, String)],
) -> anyhow::Result<Canaries> {
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
            tests: None,
        })
        .collect();
    if jobs.is_empty() {
        return Ok(Canaries::default());
    }
    let mut results = run_jobs(ctx, workers, plans, &jobs, ctx.deadline());
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
        .filter(|(_, r)| {
            !matches!(
                r.outcome,
                MutantOutcome::Killed | MutantOutcome::Survived | MutantOutcome::NotRun
            )
        })
        .map(|(j, _)| j.file.as_str())
        .collect();
    if !unconfirmed.is_empty() {
        bail!(
            "the canary neither failed nor passed the selected tests in {}: titer cannot confirm that the worktree loads the code it mutates",
            unconfirmed.join(", ")
        );
    }
    let with = |outcome: MutantOutcome| -> Vec<String> {
        jobs.iter()
            .zip(&results)
            .filter(|(_, r)| r.outcome == outcome)
            .map(|(j, _)| j.file.clone())
            .collect()
    };
    let survived = with(MutantOutcome::Survived);
    if survived.len() == jobs.len() {
        bail!(
            "the canary survived in every mutated module ({}): the worktree is not loading the code titer mutates. \
             An editable install, a PYTHONPATH that points at the checkout, or a Cargo path dependency outside the repo can cause this.",
            survived.join(", ")
        );
    }
    Ok(Canaries {
        unloaded: survived,
        not_run: with(MutantOutcome::NotRun),
    })
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
    let mut pairs = operators::budget_order(pairs, |(m, _)| (m.file.clone(), m.line));
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
    let mut plans = plan(ctx, heads, &head, &runners)?;
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
    let deadline = ctx.deadline();
    collect_coverage(ctx, heads, &worktrees, &mut plans, deadline, &mut warnings);
    let canaries = canaries(ctx, &worktrees, &plans, &modules)?;
    let unloaded = canaries.unloaded;
    if !unloaded.is_empty() {
        warnings.push(format!(
            "the selected tests never load {}; its mutants are reported as not executed",
            unloaded.join(", ")
        ));
    }

    // Mutants of a module whose canary did not run keep their not-run outcome.
    let (loaded, skipped): (Vec<usize>, Vec<usize>) = (0..pairs.len())
        .filter(|i| !canaries.not_run.contains(&pairs[*i].1.file))
        .partition(|i| !unloaded.contains(&pairs[*i].1.file));
    // Coverage that names the tests that run a mutant's code narrows its run to them.
    // A mutant whose code the changed tests never run still runs every changed test: a
    // process that coverage missed, or an edit that changes how unexecuted code compiles,
    // such as removing a `yield`, can still kill it.
    let covers: Vec<Cover> = pairs.iter().map(|(m, j)| cover_of(&plans, m, j)).collect();
    let to_run = loaded;
    for i in &to_run {
        pairs[*i].1.tests = narrowed(&plans, &pairs[*i].1, &covers[*i]);
    }
    validate_narrowed(ctx, &worktrees, &plans, &mut pairs, &to_run, deadline);
    let jobs: Vec<Job> = to_run.iter().map(|i| pairs[*i].1.clone()).collect();
    let results = run_jobs(ctx, &worktrees, &plans, &jobs, deadline);
    let mut mutants: Vec<Mutant> = pairs.iter().map(|(m, _)| m.clone()).collect();
    for (i, result) in to_run.iter().zip(results) {
        let m = &mut mutants[*i];
        m.outcome = result.outcome;
        m.killed_by = result.killed_by;
        m.duration_ms = result.duration.map(|d| d.as_millis() as u64);
    }
    // A narrowed run that kills nothing, or times out, may have left out a test that kills
    // the mutant, for example one whose effect on shared state the coverage does not show,
    // so it runs again with every changed test.
    let confirm: Vec<usize> = to_run
        .iter()
        .copied()
        .filter(|i| {
            pairs[*i].1.tests.is_some()
                && matches!(
                    mutants[*i].outcome,
                    MutantOutcome::Survived | MutantOutcome::NoResult | MutantOutcome::TimedOut
                )
        })
        .collect();
    let jobs: Vec<Job> = confirm
        .iter()
        .map(|i| Job {
            tests: None,
            ..pairs[*i].1.clone()
        })
        .collect();
    for (i, result) in confirm
        .iter()
        .zip(run_jobs(ctx, &worktrees, &plans, &jobs, deadline))
    {
        // A confirmation the budget did not reach leaves the mutant not run, since the
        // narrowed run alone cannot show that it survives.
        let m = &mut mutants[*i];
        m.outcome = result.outcome;
        m.killed_by = result.killed_by;
        m.duration_ms = result.duration.map(|d| d.as_millis() as u64);
    }
    for i in skipped {
        mutants[i].outcome = MutantOutcome::NotExecuted;
    }
    // A survivor that the changed tests never run is not executed. Where a sentinel could
    // not probe its line, it first reruns against the whole changed test files, as it would
    // without coverage, so a kill there is still caught by other tests.
    for (i, m) in mutants.iter_mut().enumerate() {
        if m.outcome == MutantOutcome::Survived
            && covers[i] == Cover::NotRun
            && probeable(&pairs[i].1, m)
        {
            m.outcome = MutantOutcome::NotExecuted;
        }
    }
    follow_up(
        ctx,
        &worktrees,
        &plans,
        &pairs,
        &covers,
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

/// What the runner's coverage says about a mutant's code. An agent edit can change code
/// past where it starts, so coverage cannot speak for it.
fn cover_of(plans: &BTreeMap<usize, RunnerPlan>, mutant: &Mutant, job: &Job) -> Cover {
    let plan = &plans[&job.runner];
    let (Plan::Tests(_), Some(coverage)) = (&plan.plan, &plan.coverage) else {
        return Cover::Unknown;
    };
    if mutant.source == MutantSource::Agent {
        return Cover::Unknown;
    }
    let column = edit_column(&job.original, &job.mutated, mutant.line);
    coverage.cover(&job.file, mutant.line, column, mutant.end_line)
}

/// The plan's tests that coverage names as running the mutant's code, when they are fewer
/// than the plan's.
fn narrowed(
    plans: &BTreeMap<usize, RunnerPlan>,
    job: &Job,
    cover: &Cover,
) -> Option<Vec<TestCase>> {
    let (Plan::Tests(cases), Cover::Ran(Some(ids))) = (&plans[&job.runner].plan, cover) else {
        return None;
    };
    let tests: Vec<TestCase> = cases
        .iter()
        .filter(|c| ids.contains(&c.id))
        .cloned()
        .collect();
    // A covering test outside the plan, such as one skipped on head, leaves the data unsure.
    (tests.len() == ids.len() && tests.len() < cases.len()).then_some(tests)
}

/// Runs each distinct narrowed selection once on unmutated head. A selection that is not
/// green there, for example because a test needs state that a left-out test module sets,
/// cannot kill anything, so its mutants run every changed test instead.
fn validate_narrowed(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    pairs: &mut [(Mutant, Job)],
    to_run: &[usize],
    deadline: Deadline,
) {
    let mut selections: Vec<(usize, Vec<TestCase>)> = Vec::new();
    let mut jobs = Vec::new();
    for i in to_run {
        let job = &pairs[*i].1;
        let Some(tests) = &job.tests else { continue };
        if selections
            .iter()
            .any(|(r, t)| *r == job.runner && t == tests)
        {
            continue;
        }
        selections.push((job.runner, tests.clone()));
        jobs.push(Job {
            mutated: job.original.clone(),
            ..job.clone()
        });
    }
    let results = run_jobs(ctx, workers, plans, &jobs, deadline);
    for ((runner, tests), result) in selections.iter().zip(results) {
        if result.outcome == MutantOutcome::Survived {
            continue;
        }
        for i in to_run {
            let job = &mut pairs[*i].1;
            if job.runner == *runner && job.tests.as_ref() == Some(tests) {
                job.tests = None;
            }
        }
    }
}

/// The 0-based UTF-16 column on `line` where `mutated` first differs from `original`, as
/// istanbul counts columns. 0 when the edit starts on another line.
fn edit_column(original: &str, mutated: &str, line: u32) -> u32 {
    let at = original
        .bytes()
        .zip(mutated.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or(original.len().min(mutated.len()));
    let mut at = at;
    while !original.is_char_boundary(at) {
        at -= 1;
    }
    let before = &original[..at];
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    if before.matches('\n').count() as u32 + 1 != line {
        return 0;
    }
    before[start..].encode_utf16().count() as u32
}

/// Whether a sentinel can probe the mutant's line.
fn probeable(job: &Job, mutant: &Mutant) -> bool {
    mutant.end_line == mutant.line
        && Lang::of(&job.file)
            .and_then(|lang| operators::sentinel(lang, &job.original, mutant.line))
            .is_some()
}

/// Collects coverage for runners whose mutants run changed tests. The head run stays
/// uninstrumented and decides which tests are green; coverage comes from runs of its own.
fn collect_coverage(
    ctx: &Context,
    heads: &[HeadRun],
    worktrees: &[Worktree],
    plans: &mut BTreeMap<usize, RunnerPlan>,
    deadline: Deadline,
    warnings: &mut Vec<String>,
) {
    let paths: Vec<PathBuf> = worktrees.iter().map(|w| w.path.clone()).collect();
    for (runner, plan) in plans.iter_mut() {
        let r = &ctx.runners[*runner];
        let Plan::Tests(cases) = &plan.plan else {
            continue;
        };
        let dir = ctx.session.run_dir.join(&format!("coverage-{runner}"));
        let budget = crate::coverage::Budget {
            deadline,
            expected: plan.head_duration,
            timeout: plan.timeout(),
        };
        let collected = match r.config.kind {
            RunnerKind::Pytest | RunnerKind::Unittest => {
                match heads.iter().find(|h| h.runner == *runner) {
                    Some(h) => crate::coverage::collect_python(
                        r,
                        &worktrees[0].path,
                        &dir,
                        cases,
                        &h.run,
                        budget,
                    ),
                    None => Collected::Missing,
                }
            }
            RunnerKind::Vitest
                if crate::coverage::vitest_provider_installed(r, &worktrees[0].path) =>
            {
                crate::coverage::collect_vitest(r, &paths, &dir, cases, budget, warnings)
            }
            _ => Collected::Missing,
        };
        match collected {
            Collected::Ready(c) => plan.coverage = Some(c),
            Collected::Unusable(why) => warnings.push(format!("runner {:?}: {why}", r.name())),
            Collected::Missing => {}
        }
    }
}

/// Survivor follow-up. First the sentinel: a raise before the statement that encloses each
/// surviving line, or inside an `elif` or `else if` condition. If the selected tests still
/// pass, no green selected test runs that line, and its survivors are not executed. Then the
/// other tests: the remaining survivors run against the whole changed test files, and a kill
/// there is caught by other tests. A survivor whose sentinel or other-tests run the deadline
/// does not reach is not run: without that run, titer cannot say what kind of survivor it is.
#[allow(clippy::too_many_arguments)]
fn follow_up(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    pairs: &[(Mutant, Job)],
    covers: &[Cover],
    mutants: &mut [Mutant],
    deadline: Deadline,
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
        // Coverage already showed that the changed tests run a covered mutant's code, and a
        // sentinel that a test swallows would say otherwise.
        if mutants[i].end_line != mutants[i].line || covers[i] != Cover::Unknown {
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
                tests: None,
            });
            job_lines.push((file.clone(), *line));
        }
    }
    let results = run_jobs(ctx, workers, plans, &jobs, deadline);
    for ((file, line), result) in job_lines.iter().zip(&results) {
        let outcome = match result.outcome {
            MutantOutcome::Survived => MutantOutcome::NotExecuted,
            MutantOutcome::NotRun => MutantOutcome::NotRun,
            _ => continue,
        };
        for (m, cover) in mutants.iter_mut().zip(covers) {
            if &m.file == file
                && m.line == *line
                && m.end_line == *line
                && m.outcome == MutantOutcome::Survived
                && *cover == Cover::Unknown
            {
                if outcome == MutantOutcome::NotRun {
                    not_run(m);
                } else {
                    m.outcome = outcome;
                }
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
        if files.is_empty() {
            continue;
        }
        let skip = |mutants: &mut [Mutant]| {
            for i in &remaining {
                if pairs[*i].1.runner == *runner {
                    not_run(&mut mutants[*i]);
                }
            }
        };
        if !deadline.fits(plan.head_duration) {
            skip(mutants);
            continue;
        }
        let r = &ctx.runners[*runner];
        let head = r.run(&RunRequest {
            worktree: &workers[0].path,
            selection: Selection::Files(&files),
            fail_fast: false,
            timeout: Some(plan.timeout()),
            deadline,
            scratch: &ctx.session.run_dir.join("logs"),
        })?;
        if head.past_deadline {
            skip(mutants);
            continue;
        }
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
                coverage: None,
            },
        );
    }
    let rerun: Vec<usize> = remaining
        .into_iter()
        .filter(|i| file_plans.contains_key(&pairs[*i].1.runner))
        .collect();
    let jobs: Vec<Job> = rerun
        .iter()
        .map(|i| Job {
            tests: None,
            ..pairs[*i].1.clone()
        })
        .collect();
    let results = run_jobs(ctx, workers, &file_plans, &jobs, deadline);
    for (i, result) in rerun.iter().zip(results) {
        match result.outcome {
            MutantOutcome::Killed => {
                mutants[*i].outcome = MutantOutcome::CaughtByOther;
                mutants[*i].killed_by = result.killed_by;
            }
            MutantOutcome::NotRun => not_run(&mut mutants[*i]),
            _ => {}
        }
    }
    for (m, cover) in mutants.iter_mut().zip(covers) {
        if m.outcome == MutantOutcome::Survived && *cover == Cover::NotRun {
            m.outcome = MutantOutcome::NotExecuted;
        }
    }
    Ok(())
}

/// Reports a survivor as not run when the deadline cut its follow-up short.
fn not_run(m: &mut Mutant) {
    m.outcome = MutantOutcome::NotRun;
    m.killed_by = None;
    m.duration_ms = None;
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
    fn edit_columns_count_utf16_units_from_the_line_start() {
        assert_eq!(edit_column("a\n  x > 1\n", "a\n  x >= 1\n", 2), 5);
        assert_eq!(
            edit_column("\u{00e9} = x > 1\n", "\u{00e9} = x >= 1\n", 1),
            7
        );
        assert_eq!(edit_column("a\nb\n", "a\nc\n", 1), 0);
        // An astral character is two UTF-16 units.
        assert_eq!(edit_column("\u{1F600} x > 1\n", "\u{1F600} x >= 1\n", 1), 6);
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
