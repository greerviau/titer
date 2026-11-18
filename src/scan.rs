//! The scan tier: static findings for weakened tests. It compares each changed test file in
//! base with head, test by test, and never runs anything.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::detect::{Classifier, Role, python_style};
use crate::diff::FileStatus;
use crate::lang::{self, Lang, TestDef};
use crate::report::{Finding, FindingKind};
use crate::runner::Runner;
use crate::session::Session;

pub fn scan(session: &Session, runners: &[Runner]) -> anyhow::Result<Vec<Finding>> {
    let classifier = Classifier::in_checkout(runners, session.git.root())?;
    let mut findings = Vec::new();
    for change in &session.changes.files {
        let path = change.path();
        if is_snapshot(path) {
            // A rename with identical contents, such as moving a test and its snapshot
            // together, changes nothing.
            let edited = !change.added.is_empty() || !change.removed.is_empty();
            if change.status == FileStatus::Deleted
                || (change.status != FileStatus::Added && edited)
            {
                let line = change
                    .added
                    .iter()
                    .chain(&change.removed)
                    .next()
                    .copied()
                    .unwrap_or(1);
                let verb = if change.status == FileStatus::Deleted {
                    "deleted"
                } else {
                    "changed"
                };
                findings.push(Finding {
                    kind: FindingKind::SnapshotChanged,
                    file: path.to_string(),
                    line,
                    test: None,
                    message: format!("snapshot file {verb}; check that the new output is intended"),
                });
            }
            continue;
        }
        let old_path = change.old_path.as_deref();
        let new_path = change.new_path.as_deref();
        // A test file renamed to a name the runner does not collect loses its tests. A Rust
        // source file holds its own unit tests.
        let is_test = |p: &str| {
            matches!(
                classifier.role(p),
                Role::Test { .. }
                    | Role::Source {
                        lang: Lang::Rust,
                        ..
                    }
            )
        };
        let new_is_test = new_path.is_some_and(is_test);
        let role_path = if new_is_test { new_path } else { old_path }.unwrap_or_default();
        let runner = match classifier.role(role_path) {
            Role::Test { runner }
            | Role::Source {
                runner,
                lang: Lang::Rust,
            } => runner,
            _ => continue,
        };
        let new_path = new_path.filter(|_| new_is_test);
        let Some(lang) = Lang::of(role_path) else {
            continue;
        };
        let style = python_style(runners[runner].config.kind);
        let base = match old_path {
            Some(p) => read(session, &session.base_sha, p)?,
            None => String::new(),
        };
        let head = match new_path {
            Some(p) => read(session, &session.head_tree, p)?,
            None => String::new(),
        };
        findings.extend(compare(lang, style, path, &base, &head));
    }
    Ok(findings)
}

fn read(session: &Session, tree: &str, path: &str) -> anyhow::Result<String> {
    Ok(session
        .git
        .read_blob(tree, path)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default())
}

pub fn is_snapshot(path: &str) -> bool {
    path.contains("__snapshots__/") || path.ends_with(".snap") || path.ends_with(".ambr")
}

/// A test with the text of its span.
struct Test<'a> {
    def: TestDef,
    text: &'a str,
}

fn tests_of<'a>(
    lang: Lang,
    source: &'a str,
    style: lang::PythonStyle,
) -> BTreeMap<String, Test<'a>> {
    let mut out = BTreeMap::new();
    for def in lang::find_tests(lang, source, style) {
        let mut key = def.suite.clone();
        key.push(def.name.clone());
        let mut key = key.join(" > ");
        // Two tests with one name: keep both, distinguished by order.
        while out.contains_key(&key) {
            key.push('\'');
        }
        let text = &source[def.start_byte..def.end_byte];
        out.insert(key, Test { def, text });
    }
    out
}

/// Findings for one test file, from its base and head contents.
pub fn compare(
    lang: Lang,
    style: lang::PythonStyle,
    file: &str,
    base: &str,
    head: &str,
) -> Vec<Finding> {
    let before = tests_of(lang, base, style);
    let after = tests_of(lang, head, style);
    let mut findings = Vec::new();
    for (key, old) in &before {
        if !after.contains_key(key) {
            findings.push(Finding {
                kind: FindingKind::TestDeleted,
                file: file.to_string(),
                line: old.def.line,
                test: Some(key.clone()),
                message: "the test exists in base and not in head".to_string(),
            });
        }
    }
    for (key, new) in &after {
        let old = before.get(key);
        let at = |kind, message: String| Finding {
            kind,
            file: file.to_string(),
            line: new.def.line,
            test: Some(key.clone()),
            message,
        };
        let new_markers = markers(lang, new);
        let old_markers = old.map(|o| markers(lang, o)).unwrap_or_default();
        let added: Vec<&String> = new_markers.difference(&old_markers).collect();
        if !added.is_empty() {
            let list: Vec<&str> = added.iter().map(|s| s.as_str()).collect();
            findings.push(at(
                FindingKind::SkipAdded,
                format!("adds {}", list.join(", ")),
            ));
        }
        // An unchanged test can still gain a skip, through an enclosing describe.skip.
        let Some(old) = old.filter(|o| o.text != new.text) else {
            continue;
        };
        let (old_count, new_count) = (assertions(lang, old.text), assertions(lang, new.text));
        if new_count < old_count {
            findings.push(at(
                FindingKind::AssertionRemoved,
                format!("assertions drop from {old_count} to {new_count}"),
            ));
        }
        let (old_m, new_m) = (matchers(lang, old.text), matchers(lang, new.text));
        let strong = |m: &BTreeMap<String, usize>| {
            m.iter()
                .filter(|(k, _)| is_strong(k))
                .map(|(_, n)| n)
                .sum::<usize>()
        };
        let weak = |m: &BTreeMap<String, usize>| {
            m.iter()
                .filter(|(k, _)| is_weak(k))
                .map(|(_, n)| n)
                .sum::<usize>()
        };
        if strong(&new_m) < strong(&old_m) && weak(&new_m) > weak(&old_m) {
            let lost: Vec<&str> = old_m
                .keys()
                .filter(|k| is_strong(k) && new_m.get(*k).copied().unwrap_or(0) < old_m[*k])
                .map(String::as_str)
                .collect();
            let gained: Vec<&str> = new_m
                .keys()
                .filter(|k| is_weak(k) && new_m[*k] > old_m.get(*k).copied().unwrap_or(0))
                .map(String::as_str)
                .collect();
            findings.push(at(
                FindingKind::MatcherLoosened,
                format!("replaces {} with {}", lost.join(", "), gained.join(", ")),
            ));
        }
        if inline_snapshots(lang, old.text) != inline_snapshots(lang, new.text) {
            findings.push(at(
                FindingKind::SnapshotChanged,
                "an inline snapshot changed; check that the new output is intended".to_string(),
            ));
        }
    }
    for (pattern, what) in file_markers(lang) {
        let (was, now) = (
            pattern.find_iter(base).count(),
            pattern.find_iter(head).count(),
        );
        if now > was {
            let line = pattern
                .find_iter(head)
                .last()
                .map_or(1, |m| head[..m.start()].matches('\n').count() as u32 + 1);
            findings.push(Finding {
                kind: FindingKind::SkipAdded,
                file: file.to_string(),
                line,
                test: None,
                message: format!("adds {what}"),
            });
        }
    }
    findings.sort_by_key(|f| (f.line, f.kind as u8));
    findings
}

static PY_PYTESTMARK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^pytestmark\s*=.*\b(skip|skipif|xfail)\b").unwrap());
static TS_SUITE_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(describe|suite)\s*\.\s*only\b").unwrap());

/// Markers that act on a whole file or suite rather than on one test.
fn file_markers(lang: Lang) -> Vec<(&'static Regex, &'static str)> {
    match lang {
        Lang::Python => vec![(&PY_PYTESTMARK, "a module-level pytestmark skip or xfail")],
        Lang::TypeScript | Lang::Tsx => vec![(
            &TS_SUITE_ONLY,
            "describe.only, which skips every other suite in the file",
        )],
        Lang::Rust => Vec::new(),
    }
}

static PY_SKIP_DECORATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*@\S*\b(skip|skipif|skipIf|skipUnless|xfail|expectedFailure)\b").unwrap()
});
static PY_SKIP_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(pytest\.skip|self\.skipTest|pytest\.xfail)\(").unwrap());
static TS_MODIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\w$]+((?:\s*\.\s*[\w$]+)*)").unwrap());

/// Skip, xfail, only, todo, and ignore markers on a test.
fn markers(lang: Lang, test: &Test) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if lang == Lang::Rust {
        if test.def.skip {
            out.insert("#[ignore]".to_string());
        }
        return out;
    }
    if lang.is_python() {
        for caps in PY_SKIP_DECORATOR.captures_iter(test.text) {
            out.insert(caps[1].to_lowercase().replace("expectedfailure", "xfail"));
        }
        for caps in PY_SKIP_CALL.captures_iter(test.text) {
            out.insert(
                if caps[1].ends_with("xfail") {
                    "xfail"
                } else {
                    "skip"
                }
                .to_string(),
            );
        }
        return out;
    }
    if let Some(caps) = TS_MODIFIER.captures(test.text) {
        for m in caps[1].split('.').map(str::trim).filter(|m| !m.is_empty()) {
            if matches!(m, "only" | "skip" | "todo" | "fails" | "skipIf" | "runIf") {
                out.insert(m.to_string());
            }
        }
    }
    if test.def.skip && !out.contains("skip") && !out.contains("todo") {
        // Skipped through an enclosing describe.skip or an x-prefixed call.
        out.insert("skip".to_string());
    }
    out
}

static PY_ASSERT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*assert\b|\.assert\w*\(|\bassert_\w+\(|\bpytest\.(raises|warns)\(|\bself\.fail\(",
    )
    .unwrap()
});
static TS_ASSERT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bexpect(\.soft)?\(|\bassert(\.\w+)?\(|\bexpectTypeOf\(").unwrap()
});

static RS_ASSERT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:debug_|prop_)?assert(?:_\w+)?!|#\[should_panic").unwrap());

fn assertions(lang: Lang, text: &str) -> usize {
    let text = code_of(lang, text);
    let re = match lang {
        Lang::Python => &PY_ASSERT,
        Lang::TypeScript | Lang::Tsx => &TS_ASSERT,
        Lang::Rust => &RS_ASSERT,
    };
    re.find_iter(&text).count()
}

static PY_ASSERT_METHOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(assert\w*)\(").unwrap());
static PY_PLAIN_ASSERT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*assert\s+(.*)$").unwrap());
static PY_COMPARISON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"==|!=|<=|>=|<|>|\bin\b|\bis\b").unwrap());
static RS_ASSERT_MACRO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)\b(?:debug_|prop_)?(assert(?:_\w+)?)!\s*[(\[{](.*)$").unwrap()
});
static RS_COMPARISON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"==|!=|<=|>=|\s<\s|\s>\s").unwrap());
static TS_MATCHER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(?:not\.|resolves\.|rejects\.)*(to[A-Z]\w*)\(").unwrap());

/// Matcher names and how often each occurs.
fn matchers(lang: Lang, text: &str) -> BTreeMap<String, usize> {
    let text = code_of(lang, text);
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    if lang.is_python() {
        for caps in PY_ASSERT_METHOD.captures_iter(&text) {
            *out.entry(caps[1].to_string()).or_default() += 1;
        }
        for caps in PY_PLAIN_ASSERT.captures_iter(&text) {
            let kind = if PY_COMPARISON.is_match(&caps[1]) {
                "assert_compare"
            } else {
                "assert_bare"
            };
            *out.entry(kind.to_string()).or_default() += 1;
        }
    } else if lang == Lang::Rust {
        for caps in RS_ASSERT_MACRO.captures_iter(&text) {
            let kind = match &caps[1] {
                "assert" if RS_COMPARISON.is_match(&caps[2]) => "assert_compare",
                "assert" => "assert_bare",
                other => other,
            };
            *out.entry(kind.to_string()).or_default() += 1;
        }
    } else {
        for caps in TS_MATCHER.captures_iter(&text) {
            *out.entry(caps[1].to_string()).or_default() += 1;
        }
    }
    out
}

/// Matchers that pin a value.
fn is_strong(matcher: &str) -> bool {
    matches!(
        matcher,
        "toBe"
            | "toEqual"
            | "toStrictEqual"
            | "toMatchObject"
            | "toHaveLength"
            | "toHaveBeenCalledWith"
            | "toHaveBeenCalledTimes"
            | "toHaveBeenLastCalledWith"
            | "toHaveBeenNthCalledWith"
            | "toContain"
            | "toContainEqual"
            | "toMatch"
            | "toHaveProperty"
            | "toBeCloseTo"
            | "toHaveTextContent"
            | "toHaveValue"
            | "toHaveAttribute"
            | "toThrowError"
            | "toMatchSnapshot"
            | "toMatchInlineSnapshot"
            | "assertEqual"
            | "assertEquals"
            | "assertNotEqual"
            | "assertIs"
            | "assertIsNot"
            | "assertIn"
            | "assertNotIn"
            | "assertDictEqual"
            | "assertListEqual"
            | "assertTupleEqual"
            | "assertSetEqual"
            | "assertSequenceEqual"
            | "assertCountEqual"
            | "assertAlmostEqual"
            | "assertRaisesRegex"
            | "assertRegex"
            | "assertGreater"
            | "assertLess"
            | "assertGreaterEqual"
            | "assertLessEqual"
            | "assert_called_with"
            | "assert_called_once_with"
            | "assert_any_call"
            | "assert_has_calls"
            | "assert_compare"
            | "assert_eq"
            | "assert_ne"
            | "assert_matches"
            | "assert_snapshot"
            | "assert_debug_snapshot"
            | "assert_display_snapshot"
            | "assert_json_snapshot"
            | "assert_yaml_snapshot"
    )
}

/// Matchers that accept almost any value.
fn is_weak(matcher: &str) -> bool {
    matches!(
        matcher,
        "toBeTruthy"
            | "toBeFalsy"
            | "toBeDefined"
            | "toHaveBeenCalled"
            | "toBeInTheDocument"
            | "toThrow"
            | "assertTrue"
            | "assertFalse"
            | "assertIsNotNone"
            | "assertIsNone"
            | "assertRaises"
            | "assert_called"
            | "assert_called_once"
            | "assert_bare"
    )
}

static INLINE_SNAPSHOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)toMatchInlineSnapshot\(\s*(`(?:[^`\\]|\\.)*`|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')?"#,
    )
    .unwrap()
});

/// insta's inline snapshots: `@"..."`, or a raw string such as `@r#"..."#`.
static RS_INLINE_SNAPSHOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r##"(?s)@(r#*".*?"#*|"(?:[^"\\]|\\.)*")"##).unwrap());

/// The inline snapshot arguments, read as whole string or template literals, outside
/// comments.
fn inline_snapshots(lang: Lang, text: &str) -> Vec<String> {
    let text = strip_comments(lang, text);
    let re = if lang == Lang::Rust {
        &RS_INLINE_SNAPSHOT
    } else {
        &INLINE_SNAPSHOT
    };
    re.captures_iter(&text)
        .map(|c| c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default())
        .collect()
}

/// The text that assertions and matchers are counted in: comments dropped, and for Rust
/// the contents of string literals too, so a quoted `assert!` does not count.
fn code_of(lang: Lang, text: &str) -> String {
    if lang == Lang::Rust {
        strip_rust_nodes(
            text,
            &["line_comment", "block_comment"],
            &["string_literal", "raw_string_literal"],
        )
    } else {
        strip_comments(lang, text)
    }
}

/// Drops Rust comments as the parser finds them, so `//` inside a string stays.
fn strip_rust_comments(text: &str) -> String {
    strip_rust_nodes(text, &["line_comment", "block_comment"], &[])
}

/// `text` without the nodes of the `dropped` kinds, and with the nodes of the `emptied`
/// kinds replaced by an empty string literal.
fn strip_rust_nodes(text: &str, dropped: &[&str], emptied: &[&str]) -> String {
    let Some(tree) = Lang::Rust.parse(text) else {
        return TS_COMMENT.replace_all(text, "").into_owned();
    };
    let mut cuts: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if dropped.contains(&node.kind()) {
            cuts.push((node.byte_range(), ""));
            continue;
        }
        if emptied.contains(&node.kind()) {
            cuts.push((node.byte_range(), "\"\""));
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    cuts.sort_by_key(|(r, _)| r.start);
    let mut out = text.to_string();
    for (range, with) in cuts.into_iter().rev() {
        out.replace_range(range, with);
    }
    out
}

static PY_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)#.*$").unwrap());
static TS_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?ms)//.*?$|/\*.*?\*/").unwrap());

/// Drops comments, so a commented-out assertion does not count. Approximate: a `#` or `//`
/// inside a string also starts a comment here.
fn strip_comments(lang: Lang, text: &str) -> String {
    if lang == Lang::Rust {
        return strip_rust_comments(text);
    }
    let re = if lang.is_python() {
        &PY_COMMENT
    } else {
        &TS_COMMENT
    };
    re.replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::PythonStyle;

    fn kinds(findings: &[Finding]) -> Vec<(FindingKind, String)> {
        findings
            .iter()
            .map(|f| (f.kind, f.test.clone().unwrap_or_default()))
            .collect()
    }

    const PY_BASE: &str = r#"import pytest


def test_kept():
    assert 1 == 1


def test_removed():
    assert True


def test_assertions():
    assert f(1) == 2
    assert f(2) == 3


def test_loosened(self):
    self.assertEqual(f(1), 2)


def test_skip_me():
    assert f(1) == 2


def test_unchanged_but_moved():
    assert f(3) == 4
"#;

    const PY_HEAD: &str = r#"import pytest


def test_kept():
    assert 1 == 1


def test_assertions():
    assert f(1) == 2
    # assert f(2) == 3


def test_loosened(self):
    self.assertTrue(f(1))


@pytest.mark.skip(reason="flaky")
def test_skip_me():
    assert f(1) == 2


def test_unchanged_but_moved():
    assert f(3) == 4


@pytest.mark.xfail
def test_new_and_marked():
    assert f(9) == 9
"#;

    const RS_BASE: &str = r#"pub fn f(x: i32) -> i32 {
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept() {
        assert_eq!(f(1), 1);
    }

    #[test]
    fn removed() {
        assert!(true);
    }

    #[test]
    fn assertions() {
        assert_eq!(f(1), 1);
        assert_eq!(f(2), 2);
    }

    #[test]
    fn loosened() {
        assert_eq!(f(1), 1);
    }

    #[test]
    fn ignore_me() {
        assert_eq!(f(1), 1);
    }

    #[test]
    fn snap() {
        insta::assert_snapshot!(f(1), @"1");
    }
}
"#;

    const RS_HEAD: &str = r#"pub fn f(x: i32) -> i32 {
    x + 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept() {
        assert_eq!(f(1), 1);
    }

    #[test]
    fn assertions() {
        assert_eq!(f(1), 1);
        // assert_eq!(f(2), 2);
    }

    #[test]
    fn loosened() {
        assert!(f(1).is_positive());
    }

    #[test]
    #[ignore = "flaky"]
    fn ignore_me() {
        assert_eq!(f(1), 1);
    }

    #[test]
    fn snap() {
        insta::assert_snapshot!(f(1), @"2");
    }
}
"#;

    #[test]
    fn rust_findings_read_tests_inside_source_files() {
        let findings = compare(
            Lang::Rust,
            PythonStyle::Pytest,
            "src/lib.rs",
            RS_BASE,
            RS_HEAD,
        );
        let mut got = kinds(&findings);
        got.sort_by_key(|(k, t)| (*k as u8, t.clone()));
        let mut expected = vec![
            (FindingKind::TestDeleted, "tests > removed".to_string()),
            (
                FindingKind::AssertionRemoved,
                "tests > assertions".to_string(),
            ),
            (FindingKind::MatcherLoosened, "tests > loosened".to_string()),
            (FindingKind::SkipAdded, "tests > ignore_me".to_string()),
            (FindingKind::SnapshotChanged, "tests > snap".to_string()),
        ];
        expected.sort_by_key(|(k, t)| (*k as u8, t.clone()));
        assert_eq!(got, expected, "{findings:#?}");
    }

    #[test]
    fn rust_comments_come_from_the_parser_so_urls_in_strings_stay() {
        let base = "#[test]\nfn t() {\n    assert_eq!(f(1), 1); assert_eq!(f(2), 2);\n}\n";
        let head = "#[test]\nfn t() {\n    let s = \"http://example\"; assert_eq!(f(1), 1); assert_eq!(f(2), 2);\n    // assert_eq!(f(3), 3);\n}\n";
        assert_eq!(assertions(Lang::Rust, head), 2);
        assert!(compare(Lang::Rust, PythonStyle::Pytest, "tests/a.rs", base, head).is_empty());
        let quoted = "#[test]\nfn t() {\n    let _ = \"assert!(true)\"; assert_eq!(f(2), 2);\n}\n";
        assert_eq!(assertions(Lang::Rust, quoted), 1);
    }

    #[test]
    fn python_findings_cover_each_kind_and_skip_unchanged_tests() {
        let findings = compare(
            Lang::Python,
            PythonStyle::Pytest,
            "test_a.py",
            PY_BASE,
            PY_HEAD,
        );
        assert_eq!(
            kinds(&findings),
            vec![
                (FindingKind::TestDeleted, "test_removed".into()),
                (FindingKind::AssertionRemoved, "test_assertions".into()),
                (FindingKind::MatcherLoosened, "test_loosened".into()),
                (FindingKind::SkipAdded, "test_skip_me".into()),
                (FindingKind::SkipAdded, "test_new_and_marked".into()),
            ]
        );
        assert_eq!(findings[2].message, "replaces assertEqual with assertTrue");
    }

    #[test]
    fn typescript_findings_cover_only_skip_todo_and_inline_snapshots() {
        let base = r#"describe("s", () => {
  it("only", () => { expect(a()).toEqual(1); });
  it("snap", () => { expect(a()).toMatchInlineSnapshot(`1`); });
  it("loose", () => { expect(a()).toBe(1); });
  it("same", () => { expect(a()).toBe(1); });
});
"#;
        let head = r#"describe("s", () => {
  it.only("only", () => { expect(a()).toEqual(1); });
  it("snap", () => { expect(a()).toMatchInlineSnapshot(`2`); });
  it("loose", () => { expect(a()).toBeTruthy(); });
  it("same", () => { expect(a()).toBe(1); });
  it.todo("later");
});
"#;
        let findings = compare(
            Lang::TypeScript,
            PythonStyle::Pytest,
            "a.test.ts",
            base,
            head,
        );
        assert_eq!(
            kinds(&findings),
            vec![
                (FindingKind::SkipAdded, "s > only".into()),
                (FindingKind::SnapshotChanged, "s > snap".into()),
                (FindingKind::MatcherLoosened, "s > loose".into()),
                (FindingKind::SkipAdded, "s > later".into()),
            ]
        );
    }

    #[test]
    fn skips_from_enclosing_suites_and_files_are_findings() {
        let base = "describe(\"s\", () => {\n  it(\"a\", () => { expect(1).toBe(1); });\n});\ndescribe(\"t\", () => {\n  it(\"b\", () => { expect(1).toBe(1); }); it(\"c\", () => { expect(2).toBe(2); });\n});\n";
        let head = "describe.skip(\"s\", () => {\n  it(\"a\", () => { expect(1).toBe(1); });\n});\ndescribe.only(\"t\", () => {\n  it(\"b\", () => { expect(1).toBe(1); }); it\n    .only(\"c\", () => {});\n});\n";
        let findings = compare(
            Lang::TypeScript,
            PythonStyle::Pytest,
            "a.test.ts",
            base,
            head,
        );
        assert_eq!(
            kinds(&findings),
            vec![
                (FindingKind::SkipAdded, "s > a".into()),
                (FindingKind::SkipAdded, "".into()),
                (FindingKind::AssertionRemoved, "t > c".into()),
                (FindingKind::SkipAdded, "t > c".into()),
            ]
        );
        let py = compare(
            Lang::Python,
            PythonStyle::Pytest,
            "test_a.py",
            "def test_a():\n    assert 1\n",
            "import pytest\npytestmark = pytest.mark.skip(reason=\"later\")\n\ndef test_a():\n    assert 1\n",
        );
        assert_eq!(kinds(&py), vec![(FindingKind::SkipAdded, "".into())]);
    }

    #[test]
    fn inline_snapshots_compare_whole_literals_outside_comments() {
        let base = "it(\"s\", () => {\n  expect(a()).toMatchInlineSnapshot(`a() first`);\n});\n";
        let changed =
            "it(\"s\", () => {\n  expect(a()).toMatchInlineSnapshot(`a() second`);\n});\n";
        let commented = "it(\"s\", () => {\n  /* toMatchInlineSnapshot(\"no\") */\n  expect(a()).toMatchInlineSnapshot(`a() first`);\n});\n";
        let kinds_of = |head| {
            kinds(&compare(
                Lang::TypeScript,
                PythonStyle::Pytest,
                "a.test.ts",
                base,
                head,
            ))
        };
        assert_eq!(
            kinds_of(changed),
            vec![(FindingKind::SnapshotChanged, "s".into())]
        );
        assert_eq!(kinds_of(commented), vec![]);
    }

    #[test]
    fn snapshot_files_are_recognized() {
        assert!(is_snapshot("src/__snapshots__/a.test.ts.snap"));
        assert!(is_snapshot("tests/__snapshots__/test_a.ambr"));
        assert!(!is_snapshot("src/snapshot.ts"));
    }
}
