//! The mutate tier: mutate changed source lines and run the selected tests against each
//! mutant, one process per mutant, in parallel worktrees.

pub mod operators;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::bail;

use crate::check::Context;
use crate::detect::{self, glob_set};
use crate::lang::Lang;
use crate::prove::HeadRun;
use crate::report::{Mutant, MutantOutcome, MutantSource};
use crate::runner::{CaseResult, RunRequest, Runner, Selection, Status, TestCase, TestRun};
use crate::worktree::Worktree;

use operators::{CallFilter, Candidate};

/// What each runner's mutants run: the head-green changed tests, or the whole suite when the
/// change has no changed tests for that runner.
#[derive(Debug, Clone)]
pub enum Plan {
    Tests(Vec<TestCase>),
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
) -> anyhow::Result<BTreeMap<usize, RunnerPlan>> {
    let mut plans = BTreeMap::new();
    let runners_with_sources: Vec<usize> = {
        let mut r: Vec<usize> = ctx.detection.sources.iter().map(|s| s.runner).collect();
        r.sort();
        r.dedup();
        r
    };
    for runner in runners_with_sources {
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
        for cand in operators::generate(
            source.lang,
            &text,
            &source.changed,
            &calls,
            &config.exclude_operators,
        ) {
            let mutated = cand.apply(&text);
            all.push((
                mutant_from(
                    &ctx.runners[source.runner],
                    &source.file,
                    &cand,
                    MutantSource::Generated,
                ),
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
                    let fits = deadline.is_none_or(|d| Instant::now() + plan.head_duration <= d);
                    let (result, healthy) = if fits {
                        run_job(ctx, worktree, plan, job)
                    } else {
                        let skipped = JobResult {
                            outcome: MutantOutcome::NotRun,
                            killed_by: None,
                            duration: None,
                        };
                        (skipped, true)
                    };
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
            })
        })
        .collect()
}

/// Runs one job and restores the file. The flag is false when the original could not be
/// restored, so the worktree must not take more jobs.
fn run_job(ctx: &Context, worktree: &Worktree, plan: &RunnerPlan, job: &Job) -> (JobResult, bool) {
    let runner = &ctx.runners[job.runner];
    let selection = match &plan.plan {
        Plan::Tests(cases) => Selection::Tests(cases),
        Plan::Suite => Selection::Suite,
        Plan::Nothing => {
            let nothing = JobResult {
                outcome: MutantOutcome::NoResult,
                killed_by: None,
                duration: None,
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
    }
}

/// Killed when a selected test fails or the mutant breaks a selected file's import; survived
/// when a selected test passed and none failed; no result when nothing executed.
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
            if results.iter().any(|(_, r)| *r == CaseResult::Passed) {
                return (MutantOutcome::Survived, None);
            }
        }
        Plan::Suite => {
            if let Some(o) = run
                .outcomes
                .iter()
                .find(|o| matches!(o.status, Status::Failed(_)))
            {
                let mut name = o.suite.clone();
                name.push(o.name.clone());
                return (
                    MutantOutcome::Killed,
                    Some(format!("{} {}", o.file, name.join("::"))),
                );
            }
            if let Some(e) = run.file_errors.first() {
                return (MutantOutcome::Killed, Some(e.file.clone()));
            }
            if run.executed() > 0 {
                return (MutantOutcome::Survived, None);
            }
        }
        Plan::Nothing => {}
    }
    (MutantOutcome::NoResult, None)
}

/// Runs a canary in every mutated module. Returns the modules whose canary survived, or an
/// error when every canary survived, which means the worktrees do not load the code.
pub fn canaries(
    ctx: &Context,
    workers: &[Worktree],
    plans: &BTreeMap<usize, RunnerPlan>,
    files: &[(usize, String, String)],
) -> anyhow::Result<Vec<String>> {
    let jobs: Vec<Job> = files
        .iter()
        .filter(|(runner, _, _)| !matches!(plans[runner].plan, Plan::Nothing))
        .map(|(runner, file, original)| Job {
            runner: *runner,
            file: file.clone(),
            original: original.clone(),
            mutated: operators::canary(
                Lang::of(file).expect("source files have a language"),
                original,
            ),
        })
        .collect();
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let results = run_jobs(ctx, workers, plans, &jobs, None);
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
             An editable install or a PYTHONPATH that points at the checkout can cause this.",
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
        return Ok(Mutation {
            mutants: Vec::new(),
            workers: 0,
            warnings,
        });
    }
    let plans = plan(ctx, heads, &head)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
