//! `titer check`: compose the tiers into one report.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::detect::{self, Detection};
use crate::gate;
use crate::mutate;
use crate::process;
use crate::prove::{self, HeadRun};
use crate::report::{Gate, Report, Tier};
use crate::runner::Runner;
use crate::scan;
use crate::session::Session;
use crate::worktree::Worktree;

#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub base: Option<String>,
    pub tiers: Vec<Tier>,
    pub budget: Option<Duration>,
    pub workers: Option<u32>,
    pub mutants: Option<PathBuf>,
    pub coverage: Option<PathBuf>,
}

/// Everything the tiers share during one check.
pub struct Context {
    pub session: Session,
    pub runners: Vec<Runner>,
    pub detection: Detection,
    pub options: CheckOptions,
    pub started: Instant,
    pub budget: Duration,
}

impl Context {
    pub fn wants(&self, tier: Tier) -> bool {
        self.options.tiers.contains(&tier)
    }

    pub fn remaining(&self) -> Duration {
        self.budget.saturating_sub(self.started.elapsed())
    }
}

/// Runs a check from `cwd`, which must be inside a git checkout that has opted in.
pub fn run(cwd: &Path, options: &CheckOptions) -> anyhow::Result<Report> {
    let started = Instant::now();
    process::install_signal_handlers()?;
    let session = Session::open(cwd, options.base.as_deref())?;
    let runners: Vec<Runner> = session
        .config
        .runners
        .iter()
        .cloned()
        .map(Runner::new)
        .collect();
    let detection = detect::detect(&session, &runners)?;
    let budget = options.budget.unwrap_or(session.config.mutate.budget);
    let ctx = Context {
        session,
        runners,
        detection,
        options: options.clone(),
        started,
        budget,
    };
    let mut report = Report::new();
    report.base_ref = Some(ctx.session.base_ref.clone());
    report.base_sha = Some(ctx.session.base_sha.clone());
    report.head_tree = Some(ctx.session.head_tree.clone());
    report.tiers = options.tiers.clone();
    report.not_analyzed = ctx.detection.not_analyzed.clone();
    report
        .warnings
        .extend(crate::init::stamp_warning(ctx.session.git.root()));

    if ctx.wants(Tier::Scan) {
        let tier_started = Instant::now();
        report.findings = scan::scan(&ctx.session, &ctx.runners)?;
        report.timing.scan_ms = Some(elapsed_ms(tier_started));
    }
    if ctx.wants(Tier::Prove) || ctx.wants(Tier::Mutate) {
        let Some((heads, head)) = head_tier(&ctx, &mut report)? else {
            report.timing.total_ms = elapsed_ms(started);
            return Ok(report);
        };
        if ctx.wants(Tier::Prove) {
            let tier_started = Instant::now();
            let base = Worktree::create(
                &ctx.session.git,
                &ctx.session.base_sha,
                &ctx.session.run_dir.join("base"),
                &ctx.session.config.runners,
            )?;
            let paths: Vec<&str> = ctx
                .detection
                .test_paths
                .iter()
                .map(|(_, p)| p.as_str())
                .collect();
            base.overlay(&ctx.session.git, &ctx.session.head_tree, &paths)?;
            prove::splice_rust_sources(&ctx.session, &ctx.detection, &base)?;
            report.tests = prove::prove(&ctx.session, &ctx.runners, &ctx.detection, &heads, &base)?;
            report.timing.prove_ms = Some(elapsed_ms(tier_started));
        }
        if ctx.wants(Tier::Mutate) {
            let tier_started = Instant::now();
            let agent = match &ctx.options.mutants {
                Some(path) => mutate::agent::load(&ctx, path)?,
                None => mutate::agent::Loaded {
                    mutants: Vec::new(),
                    warnings: Vec::new(),
                },
            };
            report.warnings.extend(agent.warnings);
            match mutate::run(&ctx, &heads, head, agent.mutants) {
                Ok(mutation) => {
                    report.mutants = mutation.mutants;
                    report.warnings.extend(mutation.warnings);
                    report.timing.workers = Some(mutation.workers);
                }
                Err(e) => report.gate = Gate::error(format!("{e:#}")),
            }
            report.timing.mutate_ms = Some(elapsed_ms(tier_started));
        }
    }
    if ctx.wants(Tier::Crap) && report.gate.status != crate::report::GateStatus::Error {
        let tier_started = Instant::now();
        crap_tier(&ctx, &mut report)?;
        report.timing.crap_ms = Some(elapsed_ms(tier_started));
    }
    let touches_source = !ctx.detection.sources.is_empty();
    gate::evaluate(&mut report, &ctx.session.config.gate, touches_source);
    report.timing.total_ms = elapsed_ms(started);
    Ok(report)
}

/// Runs the changed tests on head. Returns None, with the gate set to error, when a changed
/// test fails on head.
fn head_tier(
    ctx: &Context,
    report: &mut Report,
) -> anyhow::Result<Option<(Vec<HeadRun>, Worktree)>> {
    let head = Worktree::create(
        &ctx.session.git,
        &ctx.session.head_tree,
        &ctx.session.run_dir.join("head"),
        &ctx.session.config.runners,
    )?;
    // The budget limits mutants, not the head run that everything else needs.
    let head_timeout = ctx.budget.max(Duration::from_secs(300));
    let heads = prove::run_head(
        &ctx.session,
        &ctx.runners,
        &ctx.detection,
        &head,
        head_timeout,
    )?;
    let red: Vec<String> = heads
        .iter()
        .flat_map(|h| h.red().into_iter().map(|c| c.id.clone()))
        .collect();
    if red.is_empty() {
        return Ok(Some((heads, head)));
    }
    report.tests = heads
        .iter()
        .flat_map(|h| {
            h.cases.iter().enumerate().map(|(i, c)| {
                let (exception_type, message) = match &h.results[i] {
                    crate::runner::CaseResult::Failed(f) => {
                        (f.exception_type.clone(), Some(f.message.clone()))
                    }
                    _ => (None, None),
                };
                crate::report::TestResult {
                    id: c.id.clone(),
                    runner: c.runner.clone(),
                    file: c.file.clone(),
                    line: c.line,
                    name: c.display_name(),
                    head: h.status(i),
                    verdict: None,
                    exception_type,
                    message,
                }
            })
        })
        .collect();
    report.gate = Gate::error(format!(
        "head is not green: {} changed test(s) fail on head: {}",
        red.len(),
        red.join(", ")
    ));
    Ok(None)
}

/// Scores changed functions from `--coverage`, or from the configured coverage command run
/// in a head worktree.
fn crap_tier(ctx: &Context, report: &mut Report) -> anyhow::Result<()> {
    if ctx.detection.functions.is_empty() {
        return Ok(());
    }
    let config = &ctx.session.config.crap;
    let text = if let Some(path) = &ctx.options.coverage {
        Some(
            std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read --coverage {}: {e}", path.display()))?,
        )
    } else if let (Some(command), Some(file)) = (&config.coverage_command, &config.coverage_file) {
        let worktree = Worktree::create(
            &ctx.session.git,
            &ctx.session.head_tree,
            &ctx.session.run_dir.join("coverage"),
            &ctx.session.config.runners,
        )?;
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..])
            .current_dir(&worktree.path)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("NO_COLOR", "1");
        // CRAP is informational, so a coverage command that cannot run is a warning.
        match process::run(
            cmd,
            Some(ctx.budget.max(Duration::from_secs(300))),
            &ctx.session.run_dir.join("logs"),
        ) {
            Ok(out) if out.success() => {}
            Ok(out) => report.warnings.push(format!(
                "the coverage command exited with {:?}; CRAP scores may lack coverage",
                out.exit_code
            )),
            Err(e) => report
                .warnings
                .push(format!("cannot run the coverage command: {e:#}")),
        }
        std::fs::read_to_string(worktree.path.join(file)).ok()
    } else {
        None
    };
    let coverage = match text.as_deref().map(crate::crap::Coverage::parse) {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => {
            report.warnings.push(format!("cannot read coverage: {e:#}"));
            None
        }
        // Without configured coverage, CRAP reports complexity alone; a null score says why.
        None => None,
    };
    let read = |p: &str| crate::detect::read_head(&ctx.session, p).ok();
    report.crap = crate::crap::scores(
        &ctx.detection.functions,
        &read,
        coverage.as_ref(),
        config.threshold,
    );
    Ok(())
}

fn elapsed_ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// Runs `argv` in a head worktree, the way titer runs tests. A debugging aid.
pub fn exec(cwd: &Path, base: Option<&str>, argv: &[String]) -> anyhow::Result<i32> {
    process::install_signal_handlers()?;
    let session = Session::open(cwd, base)?;
    let head = Worktree::create(
        &session.git,
        &session.head_tree,
        &session.run_dir.join("head"),
        &session.config.runners,
    )?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(&head.path);
    let out = process::run(cmd, None, &session.run_dir.join("logs"))?;
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    Ok(out.exit_code.unwrap_or(128 + out.signal.unwrap_or(0)))
}
