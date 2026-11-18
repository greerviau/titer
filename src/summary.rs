//! Human-facing renderings of a report for CI: a Markdown summary for a job summary or a pull
//! request comment, and GitHub workflow commands that annotate lines in the diff.

use std::fmt::Write as _;

use crate::report::{
    Finding, FindingKind, GateStatus, HeadStatus, Mutant, MutantOutcome, Report, Tier, Verdict,
};

/// The marker that identifies titer's pull request comment, so the Action updates one comment.
pub const COMMENT_MARKER: &str = "<!-- titer-report -->";

/// Items listed before the rest move into a collapsed section.
const SHOWN: usize = 10;

/// A Markdown summary. With `link_base`, such as
/// `https://github.com/owner/repo/blob/<sha>`, file locations link to that commit. With
/// `commit`, the heading names the commit checked, and a hidden marker records it so the
/// Action can tell one commit's comment from another's.
pub fn markdown(report: &Report, link_base: Option<&str>, commit: Option<&str>) -> String {
    let link = |file: &str, line: u32| location(file, line, link_base);
    let mut out = String::new();
    let _ = writeln!(out, "{COMMENT_MARKER}");
    let commit = commit.filter(|c| c.chars().all(|ch| ch.is_ascii_hexdigit()) && !c.is_empty());
    if let Some(sha) = commit {
        let _ = writeln!(out, "<!-- titer-commit: {sha} -->");
    }
    let (title, sentence) = headline(report);
    match commit {
        Some(sha) => {
            let short = &sha[..sha.len().min(7)];
            let _ = writeln!(out, "### titer: {title} for `{short}`\n\n{sentence}\n");
        }
        None => {
            let _ = writeln!(out, "### titer: {title}\n\n{sentence}\n");
        }
    }

    if report.gate.status == GateStatus::Error {
        for reason in &report.gate.reasons {
            let _ = writeln!(out, "> {}", one_line(reason));
        }
        out.push('\n');
    }
    for line in tally(report) {
        let _ = writeln!(out, "- {line}");
    }

    let red: Vec<_> = report
        .tests
        .iter()
        .filter(|t| t.head == HeadStatus::Red)
        .collect();
    if !red.is_empty() {
        let _ = writeln!(
            out,
            "\n**Fails on head.** Fix these tests first; nothing else ran.\n"
        );
        for t in red {
            let detail = t.message.as_deref().map(one_line).unwrap_or_default();
            let _ = writeln!(
                out,
                "- {} {} {}",
                link(&t.file, t.line),
                escape(&t.name),
                escape(&detail)
            );
        }
    }

    let survivors: Vec<&Mutant> = report
        .mutants
        .iter()
        .filter(|m| m.outcome == MutantOutcome::Survived)
        .collect();
    let unexecuted: Vec<&Mutant> = report
        .mutants
        .iter()
        .filter(|m| m.outcome == MutantOutcome::NotExecuted)
        .collect();
    if !survivors.is_empty() {
        let _ = writeln!(
            out,
            "\n**Survivors.** These edits to changed lines pass every changed test. Each is a missing test or noise, such as an equivalent edit; triage before you act.\n"
        );
        list(&mut out, &survivors, |m| {
            format!(
                "{}: {} to {}",
                link(&m.file, m.line),
                code(&m.original),
                code(&m.replacement)
            )
        });
    }
    if !unexecuted.is_empty() {
        let lines: Vec<(&str, u32)> = unexecuted
            .iter()
            .map(|m| (m.file.as_str(), m.line))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let _ = writeln!(
            out,
            "\n**Not executed.** No passing changed test runs these changed lines.\n"
        );
        list(&mut out, &lines, |(file, line)| link(file, *line));
    }
    if !report.findings.is_empty() {
        let _ = writeln!(
            out,
            "\n**Weakened tests.** Explain each one in the PR, or revert it.\n"
        );
        let findings: Vec<&Finding> = report.findings.iter().collect();
        list(&mut out, &findings, |f| {
            let test = f
                .test
                .as_deref()
                .map(|t| format!(" {}", code(t)))
                .unwrap_or_default();
            // A deleted test's line is in base, so it gets no link to head.
            let place = if f.kind == FindingKind::TestDeleted {
                format!("{} (in base)", escape(&format!("{}:{}", f.file, f.line)))
            } else {
                link(&f.file, f.line)
            };
            format!(
                "{place} {}{test}: {}",
                kind_name(f.kind),
                escape(&f.message)
            )
        });
    }

    if !report.tests.is_empty() {
        let _ = writeln!(
            out,
            "\n<details><summary>Changed tests ({})</summary>\n",
            report.tests.len()
        );
        let _ = writeln!(out, "| Result on base | Test | Where |\n|---|---|---|");
        for t in &report.tests {
            let verdict = match (t.head, t.verdict) {
                (HeadStatus::Red, _) => "fails on head".to_string(),
                (_, Some(v)) => verdict_name(v).to_string(),
                (_, None) => "-".to_string(),
            };
            let _ = writeln!(
                out,
                "| {verdict} | {} | {} |",
                escape(&t.name),
                link(&t.file, t.line)
            );
        }
        out.push_str("\n</details>\n");
    }
    if !report.mutants.is_empty() {
        let _ = writeln!(
            out,
            "\n<details><summary>All mutants ({})</summary>\n",
            report.mutants.len()
        );
        let _ = writeln!(out, "| Outcome | Where | Edit |\n|---|---|---|");
        for m in &report.mutants {
            let _ = writeln!(
                out,
                "| {} | {} | {} to {} |",
                outcome_name(m.outcome),
                link(&m.file, m.line),
                cell(&code(&m.original)),
                cell(&code(&m.replacement))
            );
        }
        out.push_str("\n</details>\n");
    }
    let flagged: Vec<_> = report.crap.iter().filter(|c| c.flagged).collect();
    if !flagged.is_empty() {
        let _ = writeln!(
            out,
            "\n<details><summary>Complex, poorly covered functions ({})</summary>\n",
            flagged.len()
        );
        let _ = writeln!(out, "| CRAP | Function | Where |\n|---|---|---|");
        for c in flagged {
            let _ = writeln!(
                out,
                "| {:.1} | {} | {} |",
                c.score.unwrap_or_default(),
                cell(&code(&c.function)),
                link(&c.file, c.line)
            );
        }
        out.push_str("\n</details>\n");
    }
    for w in &report.warnings {
        let _ = writeln!(out, "\n> Note: {}", one_line(w));
    }
    let base = report
        .base_sha
        .as_deref()
        .map(|s| &s[..s.len().min(7)])
        .unwrap_or("?");
    let _ = writeln!(
        out,
        "\n<sub>titer {} against `{base}` in {:.0} s. Proof is a changed test that fails on the base code; a killed mutant is an edit a changed test catches.</sub>",
        report.titer_version,
        report.timing.total_ms as f64 / 1000.0
    );
    out
}

fn headline(report: &Report) -> (&'static str, String) {
    let proven = report
        .tests
        .iter()
        .filter(|t| t.verdict.is_some_and(Verdict::is_proof))
        .count();
    let killed = detected(report);
    let reasons = |skip_unproven: bool| -> Vec<String> {
        report
            .gate
            .reasons
            .iter()
            .filter(|r| !(skip_unproven && r.starts_with("unproven")))
            .map(|r| one_line(r))
            .collect()
    };
    match report.gate.status {
        GateStatus::Error => (
            "could not run",
            "titer could not finish the check.".to_string(),
        ),
        GateStatus::Fail
            if report
                .gate
                .reasons
                .iter()
                .any(|r| r.starts_with("unproven")) =>
        {
            let mut sentence = "No changed test fails on the base code, and no changed test catches an edit to the changed lines. The tests may not cover this change.".to_string();
            let others = reasons(true);
            if !others.is_empty() {
                sentence.push_str(&format!(" The gate also failed on: {}.", others.join("; ")));
            }
            ("unproven", sentence)
        }
        GateStatus::Fail => (
            "failed",
            format!("The gate failed on: {}.", reasons(false).join("; ")),
        ),
        GateStatus::Pass if proven > 0 => (
            "proven",
            format!(
                "{proven} changed test{} fail{} on the base code, so {} would have caught the bug.",
                plural(proven),
                if proven == 1 { "s" } else { "" },
                if proven == 1 { "it" } else { "they" }
            ),
        ),
        GateStatus::Pass if killed > 0 => (
            "proven by mutation",
            format!(
                "No changed test fails on the base code, but the tests catch {killed} edit{} to the changed lines.",
                plural(killed)
            ),
        ),
        GateStatus::Pass
            if !report.tiers.contains(&Tier::Prove) && !report.tiers.contains(&Tier::Mutate) =>
        {
            let tiers: Vec<&str> = report.tiers.iter().map(|t| t.name()).collect();
            (
                "pass",
                format!(
                    "Only {} ran, so nothing here says whether the tests prove the change.",
                    tiers.join(" and ")
                ),
            )
        }
        GateStatus::Pass => (
            "pass",
            "Nothing in this change needs proof from tests.".to_string(),
        ),
    }
}

fn tally(report: &Report) -> Vec<String> {
    let mut out = Vec::new();
    let ran = |t| report.tiers.contains(&t);
    if ran(Tier::Prove) && !report.tests.is_empty() {
        let count = |v: Verdict| report.tests.iter().filter(|t| t.verdict == Some(v)).count();
        let proof = count(Verdict::Assertion) + count(Verdict::Exception);
        let mut parts = vec![format!("{proof} of {} fail on base", report.tests.len())];
        for (v, label) in [
            (Verdict::PassesOnBase, "pass on base"),
            (Verdict::MissingSymbol, "only miss a new name on base"),
            (Verdict::Skipped, "skipped"),
            (Verdict::NoResult, "did not run"),
        ] {
            let n = count(v);
            if n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
        out.push(format!("Changed tests: {}.", parts.join(", ")));
    }
    if ran(Tier::Mutate) && !report.mutants.is_empty() {
        let n = |o: MutantOutcome| report.mutants.iter().filter(|m| m.outcome == o).count();
        let mut parts = vec![format!(
            "{} of {} caught",
            detected(report),
            report.mutants.len()
        )];
        for (o, label) in [
            (MutantOutcome::Survived, "survived"),
            (MutantOutcome::NotExecuted, "on lines no test runs"),
            (MutantOutcome::CaughtByOther, "caught only by older tests"),
            (MutantOutcome::NotRun, "past the time budget"),
            (MutantOutcome::NoResult, "with no result"),
        ] {
            if n(o) > 0 {
                parts.push(format!("{} {label}", n(o)));
            }
        }
        out.push(format!("Mutants: {}.", parts.join(", ")));
    }
    if ran(Tier::Scan) && !report.findings.is_empty() {
        out.push(format!("Weakened tests: {}.", report.findings.len()));
    }
    out
}

fn detected(report: &Report) -> usize {
    report
        .mutants
        .iter()
        .filter(|m| matches!(m.outcome, MutantOutcome::Killed | MutantOutcome::TimedOut))
        .count()
}

fn list<T>(out: &mut String, items: &[T], line: impl Fn(&T) -> String) {
    for item in items.iter().take(SHOWN) {
        let _ = writeln!(out, "- {}", line(item));
    }
    if items.len() > SHOWN {
        let _ = writeln!(
            out,
            "\n<details><summary>{} more</summary>\n",
            items.len() - SHOWN
        );
        for item in &items[SHOWN..] {
            let _ = writeln!(out, "- {}", line(item));
        }
        out.push_str("\n</details>\n");
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Assertion => "fails (assertion)",
        Verdict::Exception => "fails (exception)",
        Verdict::MissingSymbol => "missing a new name",
        Verdict::PassesOnBase => "passes",
        Verdict::Skipped => "skipped",
        Verdict::NoResult => "did not run",
        Verdict::Timeout => "timed out",
    }
}

fn outcome_name(o: MutantOutcome) -> &'static str {
    match o {
        MutantOutcome::Killed => "caught",
        MutantOutcome::TimedOut => "caught (timeout)",
        MutantOutcome::Survived => "survived",
        MutantOutcome::NotExecuted => "not executed",
        MutantOutcome::CaughtByOther => "caught by older tests",
        MutantOutcome::NoResult => "no result",
        MutantOutcome::NotRun => "not run",
    }
}

fn kind_name(kind: crate::report::FindingKind) -> &'static str {
    use crate::report::FindingKind::*;
    match kind {
        TestDeleted => "test deleted",
        AssertionRemoved => "assertion removed",
        SkipAdded => "skip added",
        MatcherLoosened => "matcher loosened",
        SnapshotChanged => "snapshot changed",
    }
}

/// The first line, as Markdown-safe text.
fn one_line(text: &str) -> String {
    escape(text.lines().next().unwrap_or_default().trim())
}

/// Text with Markdown's inline syntax and table pipes escaped, on one line.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '#' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The first line of source, at most 60 characters.
fn plain(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 60 {
        format!("{}...", line.chars().take(57).collect::<String>())
    } else {
        line.to_string()
    }
}

/// An inline code span that shows the source exactly: its fence is longer than any run of
/// backticks inside it.
fn code(text: &str) -> String {
    let text = plain(text);
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// A Markdown fragment made safe for a table cell, where `|` ends the cell even in code.
fn cell(markdown: &str) -> String {
    markdown.replace('|', "\\|")
}

/// A location, linked to `link_base` when there is one. The label is escaped text and the
/// URL is percent-encoded, so spaces and other characters in paths survive.
fn location(file: &str, line: u32, link_base: Option<&str>) -> String {
    let label = escape(&format!("{file}:{line}"));
    match link_base {
        Some(base) => {
            let path: Vec<String> = file.split('/').map(percent_encode).collect();
            format!(
                "[{label}]({}/{}#L{line})",
                base.trim_end_matches('/'),
                path.join("/")
            )
        }
        None => label,
    }
}

fn percent_encode(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// GitHub workflow commands that annotate the diff: errors for tests that fail on head,
/// warnings for survivors and weakened tests, notices for lines no test runs. GitHub shows at
/// most 10 of each level per step, so each level is capped at 10, most important first.
pub fn annotations(report: &Report) -> String {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut notices = Vec::new();
    for t in report.tests.iter().filter(|t| t.head == HeadStatus::Red) {
        let detail = t
            .message
            .as_deref()
            .map(|m| m.lines().next().unwrap_or_default())
            .unwrap_or("fails");
        errors.push(command(
            "error",
            &t.file,
            t.line,
            "titer: test fails on head",
            &format!("{} fails on head: {detail}", t.name),
        ));
    }
    for m in report
        .mutants
        .iter()
        .filter(|m| m.outcome == MutantOutcome::Survived)
    {
        warnings.push(command(
            "warning",
            &m.file,
            m.line,
            "titer: survivor",
            &format!(
                "Changing \"{}\" to \"{}\" here fails no changed test. Add a test if the behavior matters; leave it if the edit is equivalent.",
                plain(&m.original),
                plain(&m.replacement)
            ),
        ));
    }
    // A deleted test's line is in base and cannot be annotated on the head diff.
    for f in report
        .findings
        .iter()
        .filter(|f| f.kind != FindingKind::TestDeleted)
    {
        warnings.push(command(
            "warning",
            &f.file,
            f.line,
            &format!("titer: {}", kind_name(f.kind)),
            &f.message,
        ));
    }
    let mut seen = Vec::new();
    for m in report
        .mutants
        .iter()
        .filter(|m| m.outcome == MutantOutcome::NotExecuted)
    {
        if seen.contains(&(&m.file, m.line)) {
            continue;
        }
        seen.push((&m.file, m.line));
        notices.push(command(
            "notice",
            &m.file,
            m.line,
            "titer: not executed",
            "No passing changed test runs this changed line.",
        ));
    }
    let mut out = String::new();
    for level in [errors, warnings, notices] {
        for line in level.into_iter().take(SHOWN) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn command(level: &str, file: &str, line: u32, title: &str, message: &str) -> String {
    let prop = |s: &str| {
        s.replace('%', "%25")
            .replace('\r', "%0D")
            .replace('\n', "%0A")
            .replace(':', "%3A")
            .replace(',', "%2C")
    };
    let data = message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A");
    format!(
        "::{level} file={},line={line},title={}::{data}",
        prop(file),
        prop(title)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{MutantSource, Operator, TestResult};

    fn report() -> Report {
        let mut r = Report::new();
        r.tiers = Tier::ALL.to_vec();
        r.base_sha = Some("0123456789abcdef".into());
        r.tests = vec![TestResult {
            id: "t".into(),
            runner: "api".into(),
            file: "tests/test_app.py".into(),
            line: 4,
            name: "test_high".into(),
            head: HeadStatus::Green,
            verdict: Some(Verdict::Assertion),
            exception_type: Some("AssertionError".into()),
            message: Some("3 != 5".into()),
        }];
        let mutant = |line, outcome| Mutant {
            id: "m".into(),
            runner: "api".into(),
            file: "app.py".into(),
            line,
            end_line: line,
            operator: Operator::SwapComparison,
            source: MutantSource::Generated,
            original: ">".into(),
            replacement: ">=".into(),
            outcome,
            killed_by: None,
            duration_ms: None,
        };
        r.mutants = vec![
            mutant(9, MutantOutcome::NotExecuted),
            mutant(2, MutantOutcome::Killed),
            mutant(2, MutantOutcome::Survived),
            mutant(9, MutantOutcome::NotExecuted),
        ];
        r.findings = vec![Finding {
            kind: FindingKind::SkipAdded,
            file: "tests/test_app.py".into(),
            line: 10,
            test: Some("test_low".into()),
            message: "adds skip".into(),
        }];
        r
    }

    #[test]
    fn markdown_leads_with_the_verdict_and_links_locations() {
        let text = markdown(&report(), Some("https://github.com/o/r/blob/abc"), None);
        assert!(text.starts_with(COMMENT_MARKER), "{text}");
        assert!(text.contains("### titer: proven\n\n1 changed test fails on the base code, so it would have caught the bug."), "{text}");
        assert!(
            text.contains("- Changed tests: 1 of 1 fail on base."),
            "{text}"
        );
        assert!(
            text.contains("- Mutants: 1 of 4 caught, 1 survived, 2 on lines no test runs."),
            "{text}"
        );
        assert!(
            text.contains("- [app.py:2](https://github.com/o/r/blob/abc/app.py#L2): `>` to `>=`"),
            "{text}"
        );
        assert_eq!(text.matches("- [app.py:9]").count(), 1, "{text}");
        assert!(text.contains("skip added `test_low`: adds skip"), "{text}");
        assert!(text.contains("against `0123456`"), "{text}");
    }

    #[test]
    fn annotations_mark_survivors_findings_and_unexecuted_lines() {
        let text = annotations(&report());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(
            lines[0].starts_with(
                "::warning file=app.py,line=2,title=titer%3A survivor::Changing \">\" to \">=\" here"
            ),
            "{text}"
        );
        assert!(
            lines[1].starts_with(
                "::warning file=tests/test_app.py,line=10,title=titer%3A skip added::adds skip"
            ),
            "{text}"
        );
        assert!(
            lines[2].starts_with("::notice file=app.py,line=9,"),
            "{text}"
        );
    }

    #[test]
    fn an_unproven_change_says_so_plainly() {
        let mut r = report();
        r.tests[0].verdict = Some(Verdict::PassesOnBase);
        r.gate = crate::report::Gate::fail(vec!["unproven change: ...".into()]);
        let text = markdown(&r, None, None);
        assert!(
            text.contains("### titer: unproven\n\nNo changed test fails on the base code"),
            "{text}"
        );
        assert!(text.contains("app.py:2"), "{text}");
    }

    #[test]
    fn the_heading_and_a_hidden_marker_name_the_commit() {
        let text = markdown(&report(), None, Some("4b986aa0123456789"));
        assert!(text.starts_with("<!-- titer-report -->\n<!-- titer-commit: 4b986aa0123456789 -->\n### titer: proven for `4b986aa`\n"), "{text}");
        let unsafe_commit = markdown(&report(), None, Some("--> <b>"));
        assert!(
            !unsafe_commit.contains("titer-commit") && !unsafe_commit.contains("<b>"),
            "{unsafe_commit}"
        );
    }

    #[test]
    fn odd_paths_and_source_render_exactly() {
        let mut r = report();
        r.mutants[2].file = "sub dir/a|b`.ts".into();
        r.mutants[2].original = "a || `x`".into();
        let text = markdown(&r, Some("https://h/blob/s"), None);
        assert!(
            text.contains("- [sub dir/a\\|b\\`.ts:2](https://h/blob/s/sub%20dir/a%7Cb%60.ts#L2): `` a || `x` `` to `>=`"),
            "{text}"
        );
        assert!(text.contains("| survived | [sub dir/a\\|b\\`.ts:2](https://h/blob/s/sub%20dir/a%7Cb%60.ts#L2) | `` a \\|\\| `x` `` to `>=` |"), "{text}");
    }

    #[test]
    fn deleted_tests_point_at_base_and_get_no_annotation() {
        let mut r = report();
        r.findings[0].kind = FindingKind::TestDeleted;
        let text = markdown(&r, Some("https://h/blob/s"), None);
        assert!(
            text.contains("- tests/test\\_app.py:10 (in base) test deleted"),
            "{text}"
        );
        assert!(!annotations(&r).contains("test deleted"));
    }

    #[test]
    fn verdicts_name_every_gate_failure_and_partial_runs() {
        let mut r = report();
        r.gate = crate::report::Gate::fail(vec![
            "unproven change: x".into(),
            "1 scan finding(s) (gate.fail_on_scan)".into(),
        ]);
        assert!(
            markdown(&r, None, None)
                .contains("The gate also failed on: 1 scan finding(s) (gate.fail\\_on\\_scan).")
        );
        let mut scan_only = Report::new();
        scan_only.tiers = vec![Tier::Scan];
        assert!(
            markdown(&scan_only, None, None).contains(
                "Only scan ran, so nothing here says whether the tests prove the change."
            )
        );
    }
}
