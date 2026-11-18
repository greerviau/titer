//! The CRAP tier: `CRAP = comp^2 * (1 - cov)^3 + comp` for each changed function, from
//! tree-sitter complexity and a coverage file. Informational unless `[gate] fail_on_crap`.

use std::collections::BTreeMap;

use anyhow::Context;
use tree_sitter::Node;

use crate::detect::ChangedFunction;
use crate::lang::{FunctionDef, Lang};
use crate::report::CrapScore;
use crate::xml;

/// Line coverage per file: line number to whether any test ran it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Coverage {
    pub files: BTreeMap<String, BTreeMap<u32, bool>>,
}

impl Coverage {
    /// Parses lcov, Cobertura XML, or coverage.py JSON, by content.
    pub fn parse(text: &str) -> anyhow::Result<Coverage> {
        let trimmed = text.trim_start();
        if trimmed.starts_with('<') {
            return Ok(parse_cobertura(text));
        }
        if trimmed.starts_with('{') {
            return parse_coverage_py(text);
        }
        if text.lines().any(|l| l.starts_with("SF:")) {
            return Ok(parse_lcov(text));
        }
        anyhow::bail!("unknown coverage format; expected lcov, Cobertura XML, or coverage.py JSON")
    }

    /// The coverage of a repo-relative file. Coverage tools write paths relative to their
    /// own root or absolute, so the longest path that ends with, or is a suffix of, the
    /// repo-relative path wins.
    pub fn for_file(&self, repo_rel: &str) -> Option<&BTreeMap<u32, bool>> {
        if let Some((_, lines)) = self
            .files
            .iter()
            .find(|(path, _)| path.trim_start_matches("./") == repo_rel)
        {
            return Some(lines);
        }
        self.files
            .iter()
            .filter(|(path, _)| {
                let path = path.trim_start_matches("./");
                path == repo_rel
                    || path.ends_with(&format!("/{repo_rel}"))
                    || repo_rel.ends_with(&format!("/{path}"))
            })
            .max_by_key(|(path, _)| path.len())
            .map(|(_, lines)| lines)
    }
}

fn parse_lcov(text: &str) -> Coverage {
    let mut coverage = Coverage::default();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            current = Some(path.trim().to_string());
        } else if let Some(data) = line.strip_prefix("DA:") {
            let mut parts = data.split(',');
            let (Some(n), Some(hits), Some(file)) = (parts.next(), parts.next(), current.as_ref())
            else {
                continue;
            };
            if let (Ok(n), Ok(hits)) = (n.trim().parse::<u32>(), hits.trim().parse::<u64>()) {
                let lines = coverage.files.entry(file.clone()).or_default();
                let entry = lines.entry(n).or_insert(false);
                *entry |= hits > 0;
            }
        } else if line.starts_with("end_of_record") {
            current = None;
        }
    }
    coverage
}

fn parse_cobertura(text: &str) -> Coverage {
    let mut coverage = Coverage::default();
    let mut sources: Vec<String> = Vec::new();
    let mut in_source = false;
    let mut current: Option<String> = None;
    for event in xml::events(text) {
        match event {
            xml::Event::Start { name, attrs, .. } => match name.as_str() {
                "source" => in_source = true,
                "class" => current = xml::attr(&attrs, "filename").map(str::to_string),
                "line" => {
                    let (Some(file), Some(n), Some(hits)) = (
                        current.as_ref(),
                        xml::attr(&attrs, "number").and_then(|n| n.parse::<u32>().ok()),
                        xml::attr(&attrs, "hits").and_then(|h| h.parse::<u64>().ok()),
                    ) else {
                        continue;
                    };
                    let lines = coverage.files.entry(file.clone()).or_default();
                    *lines.entry(n).or_insert(false) |= hits > 0;
                }
                _ => {}
            },
            xml::Event::Text(t) if in_source => sources.push(t.trim().to_string()),
            xml::Event::End(name) if name == "source" => in_source = false,
            xml::Event::End(name) if name == "class" => current = None,
            _ => {}
        }
    }
    // Class filenames are relative to a <source> dir; keep both forms so either can match.
    if let Some(source) = sources.iter().find(|s| !s.is_empty()) {
        let joined: Vec<(String, BTreeMap<u32, bool>)> = coverage
            .files
            .iter()
            .filter(|(f, _)| !f.starts_with('/'))
            .map(|(f, l)| (format!("{}/{f}", source.trim_end_matches('/')), l.clone()))
            .collect();
        coverage.files.extend(joined);
    }
    coverage
}

fn parse_coverage_py(text: &str) -> anyhow::Result<Coverage> {
    let json: serde_json::Value =
        serde_json::from_str(text).context("coverage JSON does not parse")?;
    let files = json["files"]
        .as_object()
        .context("coverage JSON has no \"files\" object; expected coverage.py's `coverage json`")?;
    let mut coverage = Coverage::default();
    for (path, data) in files {
        let mut lines = BTreeMap::new();
        for (key, covered) in [("executed_lines", true), ("missing_lines", false)] {
            for n in data[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_u64())
            {
                lines.insert(n as u32, covered);
            }
        }
        coverage.files.insert(path.clone(), lines);
    }
    Ok(coverage)
}

/// Decision points in the function, not counting nested functions, plus one.
pub fn complexity(lang: Lang, source: &str, function: &FunctionDef) -> u32 {
    let Some(tree) = lang.parse(source) else {
        return 1;
    };
    let Some(mut node) = tree
        .root_node()
        .descendant_for_byte_range(function.start_byte, function.end_byte)
    else {
        return 1;
    };
    while node.start_byte() > function.start_byte || node.end_byte() < function.end_byte {
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    1 + decisions(lang, node, source, true)
}

const PY_FUNCTIONS: &[&str] = &["function_definition", "lambda", "class_definition"];
const TS_FUNCTIONS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_expression",
    "function",
    "generator_function",
    "arrow_function",
    "method_definition",
    "class_declaration",
    "class",
];

const RS_FUNCTIONS: &[&str] = &[
    "function_item",
    "closure_expression",
    "impl_item",
    "trait_item",
    "mod_item",
];

fn decisions(lang: Lang, node: Node, source: &str, top: bool) -> u32 {
    let kind = node.kind();
    let nested = match lang {
        Lang::Python => PY_FUNCTIONS,
        Lang::TypeScript | Lang::Tsx => TS_FUNCTIONS,
        Lang::Rust => RS_FUNCTIONS,
    };
    // The decorated definition wraps the function itself; only deeper functions are nested.
    let wrapped = node
        .parent()
        .is_some_and(|p| p.kind() == "decorated_definition");
    if !top && (kind == "decorated_definition" || (nested.contains(&kind) && !wrapped)) {
        return 0;
    }
    let own = if lang == Lang::Rust {
        match kind {
            "if_expression" | "for_expression" | "while_expression" | "match_arm" => 1,
            "binary_expression" => node
                .child_by_field_name("operator")
                .is_some_and(|op| matches!(&source[op.byte_range()], "&&" | "||"))
                as u32,
            _ => 0,
        }
    } else if lang.is_python() {
        matches!(
            kind,
            "if_statement"
                | "elif_clause"
                | "for_statement"
                | "while_statement"
                | "except_clause"
                | "conditional_expression"
                | "boolean_operator"
                | "for_in_clause"
                | "if_clause"
                | "case_clause"
        ) as u32
    } else {
        match kind {
            "if_statement" | "for_statement" | "for_in_statement" | "while_statement"
            | "do_statement" | "switch_case" | "catch_clause" | "ternary_expression" => 1,
            "binary_expression" => node
                .child_by_field_name("operator")
                .is_some_and(|op| matches!(&source[op.byte_range()], "&&" | "||" | "??"))
                as u32,
            _ => 0,
        }
    };
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    own + children
        .into_iter()
        .map(|c| decisions(lang, c, source, false))
        .sum::<u32>()
}

/// The covered fraction of the function's coverable lines, or None without data for them.
pub fn coverage_of(lines: Option<&BTreeMap<u32, bool>>, function: &FunctionDef) -> Option<f64> {
    let lines = lines?;
    let coverable: Vec<bool> = lines
        .range(function.line..=function.end_line)
        .map(|(_, covered)| *covered)
        .collect();
    if coverable.is_empty() {
        return None;
    }
    Some(coverable.iter().filter(|c| **c).count() as f64 / coverable.len() as f64)
}

pub fn score(complexity: u32, coverage: f64) -> f64 {
    let comp = complexity as f64;
    comp * comp * (1.0 - coverage).powi(3) + comp
}

/// A CRAP score per changed function.
pub fn scores(
    functions: &[ChangedFunction],
    read: &dyn Fn(&str) -> Option<String>,
    coverage: Option<&Coverage>,
    threshold: f64,
) -> Vec<CrapScore> {
    let mut out = Vec::new();
    for changed in functions {
        let Some(lang) = Lang::of(&changed.file) else {
            continue;
        };
        let Some(source) = read(&changed.file) else {
            continue;
        };
        let complexity = complexity(lang, &source, &changed.function);
        let cov = coverage.and_then(|c| coverage_of(c.for_file(&changed.file), &changed.function));
        let raw = cov.map(|c| score(complexity, c));
        let score = raw.map(|s| (s * 100.0).round() / 100.0);
        out.push(CrapScore {
            file: changed.file.clone(),
            function: changed.function.name.clone(),
            line: changed.function.line,
            complexity,
            coverage: cov.map(|c| (c * 1000.0).round() / 1000.0),
            score,
            flagged: raw.is_some_and(|s| s > threshold),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::find_functions;

    const PY: &str = r#"def classify(x, items):
    if x < 0 and x != -1:
        return "neg"
    elif x == 0:
        return "zero"
    for i in items:
        if i:
            pass
    try:
        y = 1 if x else 2
    except ValueError:
        pass
    def inner():
        if True:
            return 1
    return [i for i in items if i]
"#;

    #[test]
    fn python_complexity_counts_decisions_outside_nested_functions() {
        let functions = find_functions(Lang::Python, PY);
        // if, and, elif, for, if, conditional, except, comprehension for and if: 9, plus 1.
        assert_eq!(complexity(Lang::Python, PY, &functions[0]), 10);
        assert_eq!(complexity(Lang::Python, PY, &functions[1]), 2);
    }

    #[test]
    fn typescript_complexity_counts_logical_operators_and_cases() {
        let ts = "export function f(x: number, y?: string) {\n  switch (x) {\n    case 1: return y ?? 'a';\n    case 2: return x > 1 && y ? 'b' : 'c';\n    default: return '';\n  }\n}\n";
        let functions = find_functions(Lang::TypeScript, ts);
        // case, case, ??, &&, ternary: 5, plus 1.
        assert_eq!(complexity(Lang::TypeScript, ts, &functions[0]), 6);
    }

    #[test]
    fn rust_complexity_counts_branches_arms_and_logical_operators() {
        let rs = r#"fn f(x: i32, xs: &[i32]) -> i32 {
    if x > 0 && x < 10 {
        for y in xs {
            match y {
                0 => {}
                1 => {}
                _ => {}
            }
        }
    } else if let Some(z) = xs.first() {
        let _ = z;
    }
    let g = |z: i32| if z > 0 { 1 } else { 2 };
    while x > 0 || x < -1 {}
    g(x)
}
"#;
        let functions = find_functions(Lang::Rust, rs);
        assert_eq!(complexity(Lang::Rust, rs, &functions[0]), 10);
        let attributed = "#[inline]\npub fn g(x: bool) -> i32 {\n    if x { 1 } else { 2 }\n}\n";
        let functions = find_functions(Lang::Rust, attributed);
        assert_eq!(functions[0].line, 1);
        assert_eq!(complexity(Lang::Rust, attributed, &functions[0]), 2);
    }

    #[test]
    fn decorated_nested_functions_are_their_own() {
        let src = "def outer():\n    @cache\n    def inner(x):\n        if x:\n            return 1\n    return inner\n";
        let functions = find_functions(Lang::Python, src);
        assert_eq!(complexity(Lang::Python, src, &functions[0]), 1);
        assert_eq!(complexity(Lang::Python, src, &functions[1]), 2);
    }

    #[test]
    fn an_exact_path_beats_a_longer_suffix_match() {
        let lcov = "SF:src/app.py\nDA:1,1\nend_of_record\nSF:packages/other/src/app.py\nDA:1,0\nend_of_record\n";
        let coverage = Coverage::parse(lcov).unwrap();
        assert!(coverage.for_file("src/app.py").unwrap()[&1]);
    }

    #[test]
    fn the_threshold_applies_before_rounding() {
        let src = "def f(x):\n    return x\n";
        let function = find_functions(Lang::Python, src).remove(0);
        let changed = vec![ChangedFunction {
            runner: 0,
            file: "a.py".into(),
            function,
        }];
        let coverage = Coverage::parse("SF:a.py\nDA:1,0\nDA:2,0\nend_of_record\n").unwrap();
        let read = |_: &str| Some(src.to_string());
        // Complexity 1 and no coverage give a CRAP of exactly 2.
        assert!(!scores(&changed, &read, Some(&coverage), 2.0)[0].flagged);
        assert!(scores(&changed, &read, Some(&coverage), 1.9999)[0].flagged);
    }

    #[test]
    fn scores_match_the_formula() {
        assert_eq!(score(1, 1.0), 1.0);
        assert_eq!(score(10, 0.0), 110.0);
        assert!((score(5, 0.5) - (25.0 * 0.125 + 5.0)).abs() < 1e-9);
    }

    #[test]
    fn parses_each_coverage_format_into_the_same_lines() {
        let lcov = "TN:\nSF:/work/repo/src/app.py\nDA:1,1\nDA:2,0\nDA:3,4\nend_of_record\n";
        let cobertura = r#"<?xml version="1.0" ?><coverage><sources><source>/work/repo</source></sources><packages><package><classes><class filename="src/app.py"><lines><line number="1" hits="1"/><line number="2" hits="0"/><line number="3" hits="4"/></lines></class></classes></package></packages></coverage>"#;
        let coverage_py = r#"{"meta": {}, "files": {"src/app.py": {"executed_lines": [1, 3], "missing_lines": [2], "excluded_lines": []}}}"#;
        let expected: BTreeMap<u32, bool> =
            [(1, true), (2, false), (3, true)].into_iter().collect();
        for text in [lcov, cobertura, coverage_py] {
            let coverage = Coverage::parse(text).unwrap();
            assert_eq!(coverage.for_file("src/app.py"), Some(&expected), "{text}");
        }
    }
}
