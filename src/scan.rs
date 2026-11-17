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
    let classifier = Classifier::new(runners)?;
    let mut findings = Vec::new();
    for change in &session.changes.files {
        let path = change.path();
        if is_snapshot(path) {
            if change.status != FileStatus::Added {
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
        let role_path = new_path.or(old_path).unwrap_or_default();
        let Role::Test { runner } = classifier.role(role_path) else {
            continue;
        };
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
    let offsets = line_offsets(source);
    let mut out = BTreeMap::new();
    for def in lang::find_tests(lang, source, style) {
        let start = offsets[(def.line - 1) as usize];
        let end = offsets
            .get(def.end_line as usize)
            .copied()
            .unwrap_or(source.len());
        let mut key = def.suite.clone();
        key.push(def.name.clone());
        let mut key = key.join(" > ");
        // Two tests with one name: keep both, distinguished by order.
        while out.contains_key(&key) {
            key.push('\'');
        }
        out.insert(
            key,
            Test {
                def,
                text: &source[start..end],
            },
        );
    }
    out
}

fn line_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(source.match_indices('\n').map(|(i, _)| i + 1));
    offsets
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
        if old.is_some_and(|o| o.text == new.text) {
            continue;
        }
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
        let Some(old) = old else { continue };
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
        if new.text.contains("toMatchInlineSnapshot(")
            && old.text != new.text
            && inline_snapshots(old.text) != inline_snapshots(new.text)
        {
            findings.push(at(
                FindingKind::SnapshotChanged,
                "an inline snapshot changed; check that the new output is intended".to_string(),
            ));
        }
    }
    findings.sort_by_key(|f| (f.line, f.kind as u8));
    findings
}

static PY_SKIP_DECORATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*@\S*\b(skip|skipif|skipIf|skipUnless|xfail|expectedFailure)\b").unwrap()
});
static PY_SKIP_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(pytest\.skip|self\.skipTest|pytest\.xfail)\(").unwrap());
static TS_MODIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\w$]+((?:\.[\w$]+)*)").unwrap());

/// Skip, xfail, only, and todo markers on a test.
fn markers(lang: Lang, test: &Test) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
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
        for m in caps[1].split('.').filter(|m| !m.is_empty()) {
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

fn assertions(lang: Lang, text: &str) -> usize {
    let text = strip_comments(lang, text);
    if lang.is_python() {
        PY_ASSERT.find_iter(&text).count()
    } else {
        TS_ASSERT.find_iter(&text).count()
    }
}

static PY_ASSERT_METHOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(assert\w*)\(").unwrap());
static PY_PLAIN_ASSERT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*assert\s+(.*)$").unwrap());
static PY_COMPARISON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"==|!=|<=|>=|<|>|\bin\b|\bis\b").unwrap());
static TS_MATCHER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(?:not\.|resolves\.|rejects\.)*(to[A-Z]\w*)\(").unwrap());

/// Matcher names and how often each occurs.
fn matchers(lang: Lang, text: &str) -> BTreeMap<String, usize> {
    let text = strip_comments(lang, text);
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

static INLINE_SNAPSHOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)toMatchInlineSnapshot\((.*?)\)\s*;?").unwrap());

fn inline_snapshots(text: &str) -> Vec<String> {
    INLINE_SNAPSHOT
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

static PY_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)#.*$").unwrap());
static TS_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?ms)//.*?$|/\*.*?\*/").unwrap());

/// Drops comments, so a commented-out assertion does not count. Approximate: a `#` or `//`
/// inside a string also starts a comment here.
fn strip_comments(lang: Lang, text: &str) -> String {
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
    fn snapshot_files_are_recognized() {
        assert!(is_snapshot("src/__snapshots__/a.test.ts.snap"));
        assert!(is_snapshot("tests/__snapshots__/test_a.ambr"));
        assert!(!is_snapshot("src/snapshot.ts"));
    }
}
