//! The gate: the pass, fail, or error decision and its exit code.
//!
//! By default the gate fails only on an unproven change. That is a change that touches
//! supported source lines, where no changed test is proven and the selected tests kill none of
//! the mutants. Survivors, passes on base, scan findings, and CRAP fail the gate only when
//! `[gate]` asks for it.

use crate::config::GateConfig;
use crate::report::{Gate, GateStatus, MutantOutcome, Report, Tier};

pub fn evaluate(report: &mut Report, config: &GateConfig, touches_source: bool) {
    if report.gate.status == GateStatus::Error {
        return;
    }
    let ran = |tier| report.tiers.contains(&tier);
    let mut failures = Vec::new();
    let mut notes = Vec::new();

    let proven = report
        .tests
        .iter()
        .filter(|t| t.verdict.is_some_and(|v| v.is_proof()))
        .count();
    let evaluated: Vec<_> = report
        .mutants
        .iter()
        .filter(|m| !matches!(m.outcome, MutantOutcome::NotRun | MutantOutcome::NoResult))
        .collect();
    let detected = evaluated
        .iter()
        .filter(|m| matches!(m.outcome, MutantOutcome::Killed | MutantOutcome::TimedOut))
        .count();
    if ran(Tier::Prove) {
        notes.push(format!(
            "{proven} of {} changed test(s) proven",
            report.tests.len()
        ));
    }
    if ran(Tier::Mutate) {
        notes.push(format!(
            "{detected} of {} mutant(s) killed by the selected tests",
            report.mutants.len()
        ));
    }

    if touches_source && (ran(Tier::Prove) || ran(Tier::Mutate)) {
        let unproven_by_prove = !ran(Tier::Prove) || proven == 0;
        let unproven_by_mutate = !ran(Tier::Mutate) || (!evaluated.is_empty() && detected == 0);
        let mutate_blocks = ran(Tier::Mutate) && evaluated.is_empty();
        if unproven_by_prove && unproven_by_mutate && !mutate_blocks {
            failures.push(
                "unproven change: no changed test fails on base, and the selected tests kill no mutant of the changed lines"
                    .to_string(),
            );
        }
    }
    if config.fail_on_survivors {
        let survivors = report
            .mutants
            .iter()
            .filter(|m| m.outcome == MutantOutcome::Survived)
            .count();
        if survivors > 0 {
            failures.push(format!(
                "{survivors} mutant(s) survived (gate.fail_on_survivors)"
            ));
        }
    }
    if config.fail_on_passes_on_base {
        let passes = report
            .tests
            .iter()
            .filter(|t| t.verdict == Some(crate::report::Verdict::PassesOnBase))
            .count();
        if passes > 0 {
            failures.push(format!(
                "{passes} changed test(s) pass on base (gate.fail_on_passes_on_base)"
            ));
        }
    }
    if config.fail_on_scan && !report.findings.is_empty() {
        failures.push(format!(
            "{} scan finding(s) (gate.fail_on_scan)",
            report.findings.len()
        ));
    }
    if config.fail_on_crap {
        let flagged = report.crap.iter().filter(|c| c.flagged).count();
        if flagged > 0 {
            failures.push(format!(
                "{flagged} function(s) above the CRAP threshold (gate.fail_on_crap)"
            ));
        }
    }
    report.gate = if failures.is_empty() {
        let mut gate = Gate::pass();
        gate.reasons = notes;
        gate
    } else {
        Gate::fail(failures)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{HeadStatus, Mutant, MutantSource, Operator, TestResult, Verdict};

    fn test(verdict: Verdict) -> TestResult {
        TestResult {
            id: "t".into(),
            runner: "r".into(),
            file: "t.py".into(),
            line: 1,
            name: "t".into(),
            head: HeadStatus::Green,
            verdict: Some(verdict),
            exception_type: None,
            message: None,
        }
    }

    fn mutant(outcome: MutantOutcome) -> Mutant {
        Mutant {
            id: "m".into(),
            runner: "r".into(),
            file: "a.py".into(),
            line: 1,
            end_line: 1,
            operator: Operator::FlipBool,
            source: MutantSource::Generated,
            original: "True".into(),
            replacement: "False".into(),
            outcome,
            killed_by: None,
            duration_ms: None,
        }
    }

    fn report(tests: Vec<TestResult>, mutants: Vec<MutantOutcome>) -> Report {
        let mut r = Report::new();
        r.tiers = vec![Tier::Prove, Tier::Mutate];
        r.tests = tests;
        r.mutants = mutants.into_iter().map(mutant).collect();
        r
    }

    fn status(mut r: Report, config: &GateConfig, touches: bool) -> GateStatus {
        evaluate(&mut r, config, touches);
        r.gate.status
    }

    #[test]
    fn fails_only_when_nothing_proves_the_change() {
        let c = GateConfig::default();
        use MutantOutcome::*;
        let cases = [
            (
                vec![test(Verdict::Assertion)],
                vec![Survived],
                true,
                GateStatus::Pass,
            ),
            (
                vec![test(Verdict::PassesOnBase)],
                vec![Killed],
                true,
                GateStatus::Pass,
            ),
            (
                vec![test(Verdict::PassesOnBase)],
                vec![TimedOut],
                true,
                GateStatus::Pass,
            ),
            (
                vec![test(Verdict::PassesOnBase)],
                vec![Survived, NotExecuted],
                true,
                GateStatus::Fail,
            ),
            (
                vec![test(Verdict::MissingSymbol)],
                vec![CaughtByOther],
                true,
                GateStatus::Fail,
            ),
            (vec![], vec![Survived], true, GateStatus::Fail),
            (
                vec![test(Verdict::PassesOnBase)],
                vec![],
                true,
                GateStatus::Pass,
            ),
            (
                vec![test(Verdict::PassesOnBase)],
                vec![NotRun, NoResult],
                true,
                GateStatus::Pass,
            ),
            (
                vec![test(Verdict::PassesOnBase)],
                vec![Survived],
                false,
                GateStatus::Pass,
            ),
        ];
        for (tests, mutants, touches, expected) in cases {
            let label = format!(
                "{:?} {:?} {touches}",
                tests.iter().map(|t| t.verdict).collect::<Vec<_>>(),
                mutants
            );
            assert_eq!(
                status(report(tests, mutants), &c, touches),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn options_add_failures() {
        let c = GateConfig {
            fail_on_survivors: true,
            fail_on_passes_on_base: true,
            fail_on_scan: false,
            fail_on_crap: false,
        };
        let mut r = report(
            vec![test(Verdict::Assertion), test(Verdict::PassesOnBase)],
            vec![MutantOutcome::Killed, MutantOutcome::Survived],
        );
        evaluate(&mut r, &c, true);
        assert_eq!(r.gate.status, GateStatus::Fail);
        assert_eq!(r.gate.exit_code, 1);
        assert_eq!(r.gate.reasons.len(), 2);
    }

    #[test]
    fn only_prove_judges_by_proof_alone() {
        let mut r = report(vec![test(Verdict::PassesOnBase)], vec![]);
        r.tiers = vec![Tier::Prove];
        evaluate(&mut r, &GateConfig::default(), true);
        assert_eq!(r.gate.status, GateStatus::Fail);
        let mut r = report(vec![], vec![]);
        r.tiers = vec![Tier::Scan];
        evaluate(&mut r, &GateConfig::default(), true);
        assert_eq!(r.gate.status, GateStatus::Pass);
    }
}
