//! Agent-written mutants from `--mutants FILE`: JSON lines that each replace a line range of
//! a source file. They run like generated mutants and are tagged `source: agent`.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

use crate::check::Context;
use crate::detect::{Classifier, Role, read_head};
use crate::report::{Mutant, MutantOutcome, MutantSource, Operator};

use super::Job;

/// One line of the mutants file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    /// Repo-relative source file.
    file: String,
    /// First line to replace, 1-based.
    line: u32,
    /// Last line to replace; defaults to `line`.
    end_line: Option<u32>,
    /// The text that replaces the whole lines, without a trailing newline.
    replacement: String,
    /// An optional id for the report; defaults to file:line:agent:N.
    id: Option<String>,
}

/// Parses the mutants file. Lines that name no source file or a range outside it become
/// warnings; a line that is not valid JSON is an error.
/// Agent mutants with their jobs, and warnings for the lines that were skipped.
pub struct Loaded {
    pub mutants: Vec<(Mutant, Job)>,
    pub warnings: Vec<String>,
}

pub fn load(ctx: &Context, path: &Path) -> anyhow::Result<Loaded> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read --mutants {}", path.display()))?;
    let classifier = Classifier::new(&ctx.runners)?;
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let spec: Spec = serde_json::from_str(raw)
            .with_context(|| format!("--mutants {} line {}", path.display(), n + 1))?;
        let Role::Source { runner, .. } = classifier.role(&spec.file) else {
            warnings.push(format!(
                "agent mutant on line {} skipped: {} is not a source file of any runner",
                n + 1,
                spec.file
            ));
            continue;
        };
        let original = match read_head(&ctx.session, &spec.file) {
            Ok(text) => text,
            Err(_) => {
                warnings.push(format!(
                    "agent mutant on line {} skipped: {} is not in head",
                    n + 1,
                    spec.file
                ));
                continue;
            }
        };
        let end = spec.end_line.unwrap_or(spec.line);
        let lines: Vec<&str> = original.split_inclusive('\n').collect();
        if spec.line == 0 || end < spec.line || end as usize > lines.len() {
            warnings.push(format!(
                "agent mutant on line {} skipped: lines {}-{end} are outside {}",
                n + 1,
                spec.line,
                spec.file
            ));
            continue;
        }
        let (start, stop) = (spec.line as usize - 1, end as usize);
        let replaced: String = lines[start..stop].concat();
        let newline = if replaced.ends_with('\n') { "\n" } else { "" };
        let mutated = format!(
            "{}{}{newline}{}",
            lines[..start].concat(),
            spec.replacement,
            lines[stop..].concat()
        );
        let unchanged_from_base =
            super::BaseHunks::load(&ctx.session, &spec.file)?.contains(spec.line, &replaced);
        let mutant = Mutant {
            id: spec
                .id
                .unwrap_or_else(|| format!("{}:{}:agent:{}", spec.file, spec.line, n + 1)),
            runner: ctx.runners[runner].name().to_string(),
            file: spec.file.clone(),
            line: spec.line,
            end_line: end,
            operator: Operator::Agent,
            source: MutantSource::Agent,
            original: replaced.trim_end_matches('\n').to_string(),
            replacement: spec.replacement,
            outcome: MutantOutcome::NotRun,
            killed_by: None,
            duration_ms: None,
            unchanged_from_base,
        };
        out.push((
            mutant,
            Job {
                runner,
                file: spec.file,
                original,
                mutated,
            },
        ));
    }
    Ok(Loaded {
        mutants: out,
        warnings,
    })
}
