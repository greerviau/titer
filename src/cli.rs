//! Command-line interface.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::check::{self, CheckOptions};
use crate::exit;
use crate::report::{GateStatus, Report, Tier};

#[derive(Parser, Debug)]
#[command(
    name = "titer",
    version,
    about = "Prove that a change's tests test the change."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Opt this repo in: write titer.toml, pin titer as a dev dependency, and write the repo skill.
    Init {
        /// Print the config and file list without writing.
        #[arg(long)]
        dry_run: bool,
        /// Rewrite the repo skill for this titer version.
        #[arg(long)]
        update: bool,
    },
    /// Check that the tests in the change against the base ref prove the change.
    Check(CheckArgs),
    /// Print the report's JSON Schema.
    #[command(hide = true)]
    Schema,
    /// Run a command in a head worktree, the way titer runs tests.
    #[command(hide = true)]
    Exec {
        #[arg(long)]
        base: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
}

#[derive(clap::Args, Debug)]
struct CheckArgs {
    /// The base ref. Defaults to `base` in titer.toml, then the remote default branch.
    #[arg(long)]
    base: Option<String>,
    /// Run only these tiers, comma separated.
    #[arg(long, value_delimiter = ',', value_parser = parse_tier)]
    only: Option<Vec<Tier>>,
    /// Print the JSON report on stdout.
    #[arg(long)]
    json: bool,
    /// Time budget for the whole check, for example 120s or 3m.
    #[arg(long, value_parser = humantime::parse_duration)]
    budget: Option<Duration>,
    /// Mutation workers. Defaults to a value sized from cores and memory.
    #[arg(long)]
    workers: Option<u32>,
    /// Agent-written mutants as JSON lines.
    #[arg(long)]
    mutants: Option<PathBuf>,
    /// A coverage file for CRAP: lcov, cobertura XML, or coverage.py JSON.
    #[arg(long)]
    coverage: Option<PathBuf>,
    /// Also write a Markdown summary to this file, for a CI job summary.
    #[arg(long)]
    markdown: Option<PathBuf>,
}

fn parse_tier(text: &str) -> Result<Tier, String> {
    Tier::ALL
        .into_iter()
        .find(|t| t.name() == text)
        .ok_or_else(|| format!("unknown tier {text:?}; expected scan, prove, mutate, or crap"))
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Init { dry_run, update } => run_init(dry_run, update),
        Command::Check(args) => run_check(args),
        Command::Exec { base, argv } => run_exec(base.as_deref(), &argv),
        Command::Schema => {
            let schema = crate::report::schema();
            println!(
                "{}",
                serde_json::to_string_pretty(&schema).expect("schema serializes")
            );
            exit::PASS
        }
    };
    ExitCode::from(code)
}

fn run_init(dry_run: bool, update: bool) -> u8 {
    let result = std::env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| crate::init::run(&cwd, dry_run, update));
    let plan = match result {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("titer: {e:#}");
            return exit::ERROR;
        }
    };
    let verb = if dry_run { "would write" } else { "wrote" };
    for (path, contents) in &plan.writes {
        println!("{verb} {}", path.display());
        if dry_run && path.ends_with(crate::config::FILE_NAME) {
            println!("{contents}");
        }
    }
    for note in &plan.notes {
        println!("next: {note}");
    }
    if !dry_run {
        println!("next: review and commit these files");
    }
    exit::PASS
}

fn run_exec(base: Option<&str>, argv: &[String]) -> u8 {
    let result = std::env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| check::exec(&cwd, base, argv));
    match result {
        Ok(code) => u8::try_from(code).unwrap_or(exit::ERROR),
        Err(e) => {
            eprintln!("titer: {e:#}");
            exit::ERROR
        }
    }
}

fn run_check(args: CheckArgs) -> u8 {
    let options = CheckOptions {
        base: args.base,
        tiers: args.only.unwrap_or_else(|| Tier::ALL.to_vec()),
        budget: args.budget,
        workers: args.workers,
        mutants: args.mutants,
        coverage: args.coverage,
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("titer: cannot read the current directory: {e}");
            return exit::ERROR;
        }
    };
    let report = match check::run(&cwd, &options) {
        Ok(report) => report,
        Err(e) => Report::error(format!("{e:#}")),
    };
    if report.gate.status == GateStatus::Error {
        for reason in &report.gate.reasons {
            eprintln!("titer: {reason}");
        }
    }
    if let Some(path) = &args.markdown
        && let Err(e) = std::fs::write(path, render_markdown(&report))
    {
        eprintln!("titer: cannot write {}: {e}", path.display());
        return exit::ERROR;
    }
    let mut stdout = std::io::stdout().lock();
    let written = if args.json {
        serde_json::to_writer_pretty(&mut stdout, &report)
            .map_err(std::io::Error::from)
            .and_then(|()| writeln!(stdout))
    } else {
        stdout.write_all(render_text(&report).as_bytes())
    };
    if let Err(e) = written {
        eprintln!("titer: cannot write the report: {e}");
        return exit::ERROR;
    }
    report.gate.exit_code
}

/// A short human-readable summary of a report.
pub fn render_text(report: &Report) -> String {
    use std::fmt::Write as _;

    use crate::report::MutantOutcome;

    let mut out = String::new();
    let status = match report.gate.status {
        GateStatus::Pass => "pass",
        GateStatus::Fail => "fail",
        GateStatus::Error => "error",
    };
    let _ = writeln!(out, "titer gate: {status}");
    for reason in &report.gate.reasons {
        let _ = writeln!(out, "  - {reason}");
    }
    if !report.tests.is_empty() {
        let _ = writeln!(out, "\nchanged tests:");
        for t in &report.tests {
            let verdict = t
                .verdict
                .map(|v| format!("{v:?}"))
                .unwrap_or_else(|| format!("head {:?}", t.head));
            let _ = writeln!(out, "  {verdict:<14} {}  ({}:{})", t.name, t.file, t.line);
        }
    }
    let notable: Vec<_> = report
        .mutants
        .iter()
        .filter(|m| {
            matches!(
                m.outcome,
                MutantOutcome::Survived | MutantOutcome::NotExecuted
            )
        })
        .collect();
    if !notable.is_empty() {
        let _ = writeln!(out, "\nsurvivors:");
        for m in notable {
            let _ = writeln!(
                out,
                "  {:<13} {}:{}  {} -> {}",
                format!("{:?}", m.outcome),
                m.file,
                m.line,
                m.original.lines().next().unwrap_or_default().trim(),
                m.replacement.lines().next().unwrap_or_default().trim()
            );
        }
    }
    if !report.findings.is_empty() {
        let _ = writeln!(out, "\nscan findings:");
        for f in &report.findings {
            let _ = writeln!(out, "  {:?} {}:{}  {}", f.kind, f.file, f.line, f.message);
        }
    }
    for c in report.crap.iter().filter(|c| c.flagged) {
        let _ = writeln!(
            out,
            "crap {:.0}: {} ({}:{})",
            c.score.unwrap_or_default(),
            c.function,
            c.file,
            c.line
        );
    }
    for warning in &report.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

/// A Markdown summary of a report, for a CI job summary.
pub fn render_markdown(report: &Report) -> String {
    use std::fmt::Write as _;

    use crate::report::MutantOutcome;

    let mut out = String::new();
    let status = match report.gate.status {
        GateStatus::Pass => "pass",
        GateStatus::Fail => "fail",
        GateStatus::Error => "error",
    };
    let _ = writeln!(out, "## titer: {status}\n");
    for reason in &report.gate.reasons {
        let _ = writeln!(out, "- {}", escape_md(reason));
    }
    if !report.tests.is_empty() {
        let _ = writeln!(
            out,
            "\n### Changed tests\n\n| Verdict | Test | File |\n|---|---|---|"
        );
        for t in &report.tests {
            let verdict = t
                .verdict
                .map(|v| format!("{v:?}"))
                .unwrap_or_else(|| format!("head {:?}", t.head));
            let _ = writeln!(
                out,
                "| {verdict} | {} | `{}:{}` |",
                escape_md(&t.name),
                t.file,
                t.line
            );
        }
    }
    if !report.mutants.is_empty() {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for m in &report.mutants {
            *counts.entry(format!("{:?}", m.outcome)).or_default() += 1;
        }
        let summary: Vec<String> = counts.iter().map(|(k, n)| format!("{k} {n}")).collect();
        let _ = writeln!(out, "\n### Mutants\n\n{}\n", summary.join(", "));
        let notable: Vec<_> = report
            .mutants
            .iter()
            .filter(|m| {
                matches!(
                    m.outcome,
                    MutantOutcome::Survived | MutantOutcome::NotExecuted
                )
            })
            .collect();
        if !notable.is_empty() {
            let _ = writeln!(out, "| Outcome | Line | Change |\n|---|---|---|");
            for m in notable {
                let _ = writeln!(
                    out,
                    "| {:?} | `{}:{}` | `{}` to `{}` |",
                    m.outcome,
                    m.file,
                    m.line,
                    escape_md(m.original.lines().next().unwrap_or_default().trim()),
                    escape_md(m.replacement.lines().next().unwrap_or_default().trim())
                );
            }
        }
    }
    if !report.findings.is_empty() {
        let _ = writeln!(
            out,
            "\n### Scan findings\n\n| Kind | Where | Detail |\n|---|---|---|"
        );
        for f in &report.findings {
            let _ = writeln!(
                out,
                "| {:?} | `{}:{}` | {} |",
                f.kind,
                f.file,
                f.line,
                escape_md(&f.message)
            );
        }
    }
    let flagged: Vec<_> = report.crap.iter().filter(|c| c.flagged).collect();
    if !flagged.is_empty() {
        let _ = writeln!(
            out,
            "\n### CRAP above the threshold\n\n| Score | Function |\n|---|---|"
        );
        for c in flagged {
            let _ = writeln!(
                out,
                "| {:.1} | `{}` (`{}:{}`) |",
                c.score.unwrap_or_default(),
                c.function,
                c.file,
                c.line
            );
        }
    }
    for w in &report.warnings {
        let _ = writeln!(out, "\n> warning: {}", escape_md(w));
    }
    out
}

fn escape_md(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}
