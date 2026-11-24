//! The scan tier: static findings for weakened tests. It compares each changed test file in
//! base with head, test by test, and never runs anything.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

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

/// A test with the text of its span and the assertions inside it.
struct Test<'a> {
    def: TestDef,
    text: &'a str,
    assertions: Vec<Assertion<'a>>,
}

/// A test's suite path and name, joined with ` > `. Tests that share a key are told apart
/// by matching, not by the key.
fn key_of(def: &TestDef) -> String {
    let mut key = def.suite.clone();
    key.push(def.name.clone());
    key.join(" > ")
}

fn tests_of<'a>(lang: Lang, source: &'a str, style: lang::PythonStyle) -> Vec<Test<'a>> {
    let all = assertions_in(lang, source);
    lang::find_tests(lang, source, style)
        .into_iter()
        .map(|def| {
            let span = def.start_byte..def.end_byte;
            let assertions = all
                .iter()
                .filter(|a| span.contains(&a.start))
                .cloned()
                .collect();
            Test {
                text: &source[span],
                def,
                assertions,
            }
        })
        .collect()
}

/// Pairs each head test with the base test it continues, by index. Tests with distinct keys
/// pair by key. Among tests that share a key, a head test first takes a base test with the
/// same text and skip state, so reordering unchanged tests pairs each with itself, and the
/// rest pair in order. The skip state can come from an enclosing suite outside the text. Returns the head index paired with each base index.
fn pair_tests(before: &[Test], after: &[Test]) -> Vec<Option<usize>> {
    let keys: Vec<String> = before.iter().map(|t| key_of(&t.def)).collect();
    let mut paired: Vec<Option<usize>> = vec![None; before.len()];
    let mut unpaired: Vec<usize> = Vec::new();
    for (j, new) in after.iter().enumerate() {
        let key = key_of(&new.def);
        let same = (0..before.len()).find(|&i| {
            paired[i].is_none()
                && keys[i] == key
                && before[i].text == new.text
                && before[i].def.skip == new.def.skip
        });
        match same {
            Some(i) => paired[i] = Some(j),
            None => unpaired.push(j),
        }
    }
    for j in unpaired {
        let key = key_of(&after[j].def);
        if let Some(i) = (0..before.len()).find(|&i| paired[i].is_none() && keys[i] == key) {
            paired[i] = Some(j);
        }
    }
    paired
}

/// The key each test is reported under: its key, with a `'` for each earlier test in the
/// same file that shares it.
fn report_keys(tests: &[Test]) -> Vec<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    tests
        .iter()
        .map(|t| {
            let key = key_of(&t.def);
            let n = seen.entry(key.clone()).or_default();
            let out = format!("{key}{}", "'".repeat(*n));
            *n += 1;
            out
        })
        .collect()
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
    let paired = pair_tests(&before, &after);
    let mut old_of: Vec<Option<&Test>> = vec![None; after.len()];
    for (i, j) in paired.iter().enumerate() {
        if let Some(j) = j {
            old_of[*j] = Some(&before[i]);
        }
    }
    let mut findings = Vec::new();
    for ((old, key), pair) in before.iter().zip(report_keys(&before)).zip(&paired) {
        if pair.is_none() {
            findings.push(Finding {
                kind: FindingKind::TestDeleted,
                file: file.to_string(),
                line: old.def.line,
                test: Some(key),
                message: "the test exists in base and not in head".to_string(),
            });
        }
    }
    for ((new, key), old) in after.iter().zip(report_keys(&after)).zip(old_of) {
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
        let (old_count, new_count) = (old.assertions.len(), new.assertions.len());
        if new_count < old_count {
            findings.push(at(
                FindingKind::AssertionRemoved,
                format!("assertions drop from {old_count} to {new_count}"),
            ));
        }
        let (lost, gained) = loosened(&old.assertions, &new.assertions);
        if !lost.is_empty() {
            let lost: Vec<&str> = lost.into_iter().collect();
            let gained: Vec<&str> = gained.into_iter().collect();
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
        if test.def.skip || RS_CONDITIONAL_IGNORE.is_match(test.text) {
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

/// `#[cfg_attr(condition, ignore)]`, which ignores the test where the condition holds.
static RS_CONDITIONAL_IGNORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\s*\[\s*cfg_attr\s*\([^\]]*\bignore\b").unwrap());
static RS_ASSERT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:debug_|prop_)?(assert(?:_\w+)?)$").unwrap());
/// `expect`, `expect.soft`, `expect.poll`, `expectTypeOf`, `assert`, and `assert.*`, also
/// through a namespace such as `vitest.expect`.
static TS_ASSERT_CALLEE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[\w$]+\.)*(?:expect(?:\.soft|\.poll)?|expectTypeOf|assert(?:\.[\w$]+)?)$")
        .unwrap()
});
static TS_MATCHER_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^to[A-Z]\w*$").unwrap());

/// One assertion call, statement, macro, or `#[should_panic]`.
#[derive(Clone, Debug)]
struct Assertion<'a> {
    /// Where it starts in the file, to place it in a test's span.
    start: usize,
    /// The matcher it checks with, such as `toEqual`, `assertTrue`, or `assert_eq`, or the
    /// kinds `assert_compare` and `assert_bare` for a plain assert.
    matcher: Option<String>,
    /// The value under test, such as the argument to `expect`, used to pair an assertion
    /// in base with its version in head.
    subject: &'a str,
    text: &'a str,
}

/// Every assertion in a file, from its syntax tree, so assertion-like text in a string or
/// comment does not count.
fn assertions_in(lang: Lang, source: &str) -> Vec<Assertion<'_>> {
    let Some(tree) = lang.parse(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let found = match lang {
            Lang::Python => python_assertion(node, source),
            Lang::TypeScript | Lang::Tsx => ts_assertion(node, source),
            Lang::Rust => {
                if node.kind() == "token_tree" {
                    out.extend(rust_token_tree_assertions(node, source));
                }
                rust_assertion(node, source)
            }
        };
        out.extend(found);
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out.sort_by_key(|a| a.start);
    out
}

fn assertion<'a>(
    node: Node,
    source: &'a str,
    matcher: Option<String>,
    subject: &'a str,
) -> Assertion<'a> {
    Assertion {
        start: node.start_byte(),
        matcher,
        subject: subject.trim(),
        text: lang::text(node, source),
    }
}

fn first_argument<'a>(call: Node, source: &'a str) -> &'a str {
    call.child_by_field_name("arguments")
        .and_then(|args| lang::children(args).into_iter().next())
        .map_or("", |a| lang::text(a, source))
}

fn has_descendant(node: Node, kind: &str) -> bool {
    node.kind() == kind
        || lang::children(node)
            .into_iter()
            .any(|c| has_descendant(c, kind))
}

/// An `assert` statement, an `assert*` method call, an `assert_*` function call,
/// `pytest.raises`, `pytest.warns`, or `self.fail`.
fn python_assertion<'a>(node: Node, source: &'a str) -> Option<Assertion<'a>> {
    match node.kind() {
        "assert_statement" => {
            let checked = lang::children(node).into_iter().next()?;
            let (kind, subject) = if checked.kind() == "comparison_operator" {
                let left = lang::children(checked)
                    .into_iter()
                    .next()
                    .unwrap_or(checked);
                ("assert_compare", lang::text(left, source))
            } else if has_descendant(checked, "comparison_operator") {
                ("assert_compare", lang::text(checked, source))
            } else {
                ("assert_bare", lang::text(checked, source))
            };
            Some(assertion(node, source, Some(kind.to_string()), subject))
        }
        "call" => {
            let function = node.child_by_field_name("function")?;
            let whole = lang::text(function, source);
            let name = match function.kind() {
                "attribute" => lang::text(function.child_by_field_name("attribute")?, source),
                "identifier" => whole,
                _ => return None,
            };
            let counts = (function.kind() == "attribute" && name.starts_with("assert"))
                || name.starts_with("assert_")
                || matches!(whole, "pytest.raises" | "pytest.warns" | "self.fail");
            // A mock assertion checks its receiver; `self.assert*` checks its first argument.
            let receiver = function
                .child_by_field_name("object")
                .map(|o| lang::text(o, source))
                .filter(|o| *o != "self" && name.starts_with("assert_"));
            let subject = receiver.unwrap_or_else(|| first_argument(node, source));
            counts.then(|| assertion(node, source, Some(name.to_string()), subject))
        }
        _ => None,
    }
}

/// A call that `TS_ASSERT_CALLEE` names, with the matcher chained onto it, such as `toEqual` in `expect(x).not.toEqual(1)`.
fn ts_assertion<'a>(node: Node, source: &'a str) -> Option<Assertion<'a>> {
    if node.kind() != "call_expression" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    let callee: String = lang::text(function, source)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if !TS_ASSERT_CALLEE.is_match(&callee) {
        return None;
    }
    let mut matcher = None;
    let mut current = node;
    while let Some(parent) = current.parent() {
        let continues = match parent.kind() {
            "member_expression" => {
                parent.child_by_field_name("object").map(|o| o.id()) == Some(current.id())
            }
            "call_expression" => {
                parent.child_by_field_name("function").map(|f| f.id()) == Some(current.id())
            }
            "parenthesized_expression" => true,
            _ => false,
        };
        if !continues {
            break;
        }
        if parent.kind() == "call_expression" && current.kind() == "member_expression" {
            let name = current
                .child_by_field_name("property")
                .map_or("", |p| lang::text(p, source));
            if TS_MATCHER_NAME.is_match(name) {
                matcher = Some(name.to_string());
                current = parent;
                break;
            }
        }
        current = parent;
    }
    let mut found = assertion(node, source, matcher, first_argument(node, source));
    // The whole chain, so a changed matcher or expected value changes the text.
    found.text = lang::text(current, source);
    Some(found)
}

/// An `assert*!` macro, or a `#[should_panic]` attribute.
fn rust_assertion<'a>(node: Node, source: &'a str) -> Option<Assertion<'a>> {
    match node.kind() {
        "macro_invocation" => {
            let path = lang::text(node.child_by_field_name("macro")?, source);
            let name = path.rsplit("::").next().unwrap_or(path).trim();
            let args = lang::children(node)
                .into_iter()
                .find(|c| c.kind() == "token_tree")?;
            rust_macro_assertion(node, name, args, source)
        }
        "attribute_item" => {
            let (path, _) = lang::rust::attribute_parts(node, source)?;
            (path == "should_panic").then(|| assertion(node, source, None, ""))
        }
        _ => None,
    }
}

/// Assertions inside another macro's tokens, such as a test body inside `proptest!`, which
/// the parser leaves as a token tree: a name, `!`, and a token tree.
fn rust_token_tree_assertions<'a>(tree: Node, source: &'a str) -> Vec<Assertion<'a>> {
    let tokens = tokens_of(tree);
    tokens
        .windows(3)
        .filter(|w| {
            w[0].kind() == "identifier" && w[1].kind() == "!" && w[2].kind() == "token_tree"
        })
        .filter_map(|w| rust_macro_assertion(w[0], lang::text(w[0], source), w[2], source))
        .collect()
}

fn rust_macro_assertion<'a>(
    start: Node,
    name: &str,
    args: Node,
    source: &'a str,
) -> Option<Assertion<'a>> {
    let caps = RS_ASSERT_NAME.captures(name)?;
    let tokens = tokens_of(args);
    // The direct children are the delimiters, the top-level tokens, and nested groups, so a
    // top-level `,` ends the first argument.
    let open = tokens.first()?;
    let close = tokens.last()?;
    let end = tokens[1..]
        .iter()
        .find(|t| t.kind() == ",")
        .unwrap_or(close)
        .start_byte();
    let subject = source.get(open.end_byte()..end).unwrap_or_default();
    let compares = compares(&tokens, source);
    let matcher = match &caps[1] {
        "assert" if compares => "assert_compare",
        "assert" => "assert_bare",
        other => other,
    };
    let mut found = assertion(start, source, Some(matcher.to_string()), subject);
    found.text = &source[start.start_byte()..args.end_byte()];
    Some(found)
}

/// A token tree's children without comments.
fn tokens_of(tree: Node) -> Vec<Node> {
    let mut cursor = tree.walk();
    tree.children(&mut cursor)
        .filter(|t| !matches!(t.kind(), "line_comment" | "block_comment"))
        .collect()
}

/// Whether the tokens hold a comparison, also inside nested groups such as `((a == b))`.
fn compares(tokens: &[Node], source: &str) -> bool {
    tokens.iter().any(|t| match t.kind() {
        "token_tree" => compares(&tokens_of(*t), source),
        "==" | "!=" | "<=" | ">=" => true,
        // `<` and `>` also delimit generics; spaced ones compare.
        "<" | ">" => {
            let spaced = |i: usize| {
                source
                    .as_bytes()
                    .get(i)
                    .is_some_and(u8::is_ascii_whitespace)
            };
            t.start_byte() > 0 && spaced(t.start_byte() - 1) && spaced(t.end_byte())
        }
        _ => false,
    })
}

/// Pairs each base assertion with its version in head, and returns the strong matchers
/// that paired with weak ones and those weak ones. Assertions pair by identical text, then
/// by subject, then in order, so weakening one assertion while strengthening another still
/// counts.
fn loosened<'a>(
    old: &'a [Assertion],
    new: &'a [Assertion],
) -> (BTreeSet<&'a str>, BTreeSet<&'a str>) {
    let mut taken = vec![false; old.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut rest: Vec<usize> = (0..new.len()).collect();
    let same = |round: usize, o: &Assertion, n: &Assertion| match round {
        0 => o.text == n.text,
        1 => !o.subject.is_empty() && o.subject == n.subject,
        _ => true,
    };
    for round in 0..3 {
        rest.retain(|&j| {
            match (0..old.len()).find(|&i| !taken[i] && same(round, &old[i], &new[j])) {
                Some(i) => {
                    taken[i] = true;
                    if round > 0 {
                        pairs.push((i, j));
                    }
                    false
                }
                None => true,
            }
        });
    }
    let (mut lost, mut gained) = (BTreeSet::new(), BTreeSet::new());
    for (i, j) in pairs {
        if let (Some(was), Some(now)) = (&old[i].matcher, &new[j].matcher)
            && is_strong(was)
            && is_weak(now)
        {
            lost.insert(was.as_str());
            gained.insert(now.as_str());
        }
    }
    (lost, gained)
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

/// Drops Rust comments as the parser finds them, so `//` inside a string stays.
fn strip_rust_comments(text: &str) -> String {
    let Some(tree) = Lang::Rust.parse(text) else {
        return TS_COMMENT.replace_all(text, "").into_owned();
    };
    let mut cuts: Vec<std::ops::Range<usize>> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "line_comment" | "block_comment") {
            cuts.push(node.byte_range());
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    cuts.sort_by_key(|r| r.start);
    let mut out = text.to_string();
    for range in cuts.into_iter().rev() {
        out.replace_range(range, "");
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
    #[cfg_attr(unix, ignore = "flaky")]
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
        assert_eq!(assertions_in(Lang::Rust, head).len(), 2);
        assert!(compare(Lang::Rust, PythonStyle::Pytest, "tests/a.rs", base, head).is_empty());
        let quoted = "#[test]\nfn t() {\n    let _ = \"assert!(true)\"; assert_eq!(f(2), 2);\n}\n";
        assert_eq!(assertions_in(Lang::Rust, quoted).len(), 1);
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
    fn reordered_duplicate_titles_match_by_text_then_position() {
        let base = r#"describe("s", () => {
  it("dup", () => { expect(a()).toBe(1); expect(b()).toBe(2); });
  it.skip("dup", () => { expect(c()).toBe(3); });
});
"#;
        let reordered = r#"describe("s", () => {
  it.skip("dup", () => { expect(c()).toBe(3); });
  it("dup", () => { expect(a()).toBe(1); expect(b()).toBe(2); });
});
"#;
        let weakened = r#"describe("s", () => {
  it.skip("dup", () => { expect(c()).toBe(3); });
  it("dup", () => { expect(a()).toBe(1); });
});
"#;
        let run = |head| {
            compare(
                Lang::TypeScript,
                PythonStyle::Pytest,
                "a.test.ts",
                base,
                head,
            )
        };
        assert_eq!(kinds(&run(reordered)), vec![]);
        let findings = run(weakened);
        assert_eq!(
            kinds(&findings),
            vec![(FindingKind::AssertionRemoved, "s > dup'".into())]
        );
        assert_eq!(findings[0].line, 3);
    }

    #[test]
    fn offsetting_matcher_changes_still_loosen_an_assertion() {
        let ts = compare(
            Lang::TypeScript,
            PythonStyle::Pytest,
            "a.test.ts",
            "it(\"t\", () => {\n  expect(a()).toEqual(1);\n  expect(b()).toBeTruthy();\n});\n",
            "it(\"t\", () => {\n  expect(a()).toBeTruthy();\n  expect(b()).toEqual(2);\n});\n",
        );
        assert_eq!(kinds(&ts), vec![(FindingKind::MatcherLoosened, "t".into())]);
        assert_eq!(ts[0].message, "replaces toEqual with toBeTruthy");
        let py = compare(
            Lang::Python,
            PythonStyle::Pytest,
            "test_a.py",
            "def test_t(self):\n    self.assertEqual(a(), 1)\n    self.assertTrue(b())\n",
            "def test_t(self):\n    self.assertTrue(a())\n    self.assertEqual(b(), 2)\n",
        );
        assert_eq!(
            kinds(&py),
            vec![(FindingKind::MatcherLoosened, "test_t".into())]
        );
        let swapped = compare(
            Lang::TypeScript,
            PythonStyle::Pytest,
            "a.test.ts",
            "it(\"t\", () => {\n  expect(a()).toEqual(1);\n  expect(b()).toBeTruthy();\n});\n",
            "it(\"t\", () => {\n  expect(b()).toBeTruthy();\n  expect(a()).toEqual(1);\n});\n",
        );
        assert_eq!(kinds(&swapped), vec![]);
    }

    #[test]
    fn assertion_like_strings_are_not_assertions() {
        let py = compare(
            Lang::Python,
            PythonStyle::Pytest,
            "test_a.py",
            "def test_t(self):\n    self.assertEqual(f(1), 2)\n    self.assertEqual(f(2), 3)\n",
            "def test_t(self):\n    self.assertEqual(f(1), 2)\n    text = \"self.assertEqual(1, 1)\"\n",
        );
        assert_eq!(
            kinds(&py),
            vec![(FindingKind::AssertionRemoved, "test_t".into())]
        );
        let ts = compare(
            Lang::TypeScript,
            PythonStyle::Pytest,
            "a.test.ts",
            "it(\"t\", () => {\n  expect(f(1)).toBe(2);\n  expect(f(2)).toBe(3);\n});\n",
            "it(\"t\", () => {\n  expect(f(1)).toBe(2);\n  const s = \"expect(f(2)).toBe(3)\";\n});\n",
        );
        assert_eq!(
            kinds(&ts),
            vec![(FindingKind::AssertionRemoved, "t".into())]
        );
    }

    #[test]
    fn identical_tests_in_reordered_duplicate_suites_keep_their_skip_context() {
        let base = "describe(\"dup\", () => {\n  it(\"same\", () => { expect(1).toBe(1); });\n});\ndescribe.skip(\"dup\", () => {\n  it(\"same\", () => { expect(1).toBe(1); });\n});\n";
        let head = "describe.skip(\"dup\", () => {\n  it(\"same\", () => { expect(1).toBe(1); });\n});\ndescribe(\"dup\", () => {\n  it(\"same\", () => { expect(1).toBe(1); });\n});\n";
        let findings = compare(
            Lang::TypeScript,
            PythonStyle::Pytest,
            "a.test.ts",
            base,
            head,
        );
        assert_eq!(kinds(&findings), vec![]);
    }

    #[test]
    fn assertions_are_found_in_every_form() {
        let ts = |body: &str| {
            assertions_in(
                Lang::TypeScript,
                &format!("it(\"t\", async () => {{\n{body}\n}});\n"),
            )
            .into_iter()
            .map(|a| a.matcher.unwrap_or_default())
            .collect::<Vec<_>>()
        };
        assert_eq!(ts("vitest.expect(value).toBe(1);"), vec!["toBe"]);
        assert_eq!(ts("(expect(value)).toEqual(1);"), vec!["toEqual"]);
        assert_eq!(
            ts("await expect.poll(() => value).toEqual(1);"),
            vec!["toEqual"]
        );
        let rs = |body: &str| {
            assertions_in(Lang::Rust, &format!("#[test]\nfn t() {{\n{body}\n}}\n"))
                .into_iter()
                .map(|a| a.matcher.unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rs("let _ = vec![assert_eq! /* c */ (1, 1)];"),
            vec!["assert_eq"]
        );
        assert_eq!(rs("std:: assert_eq!(1, 1);"), vec!["assert_eq"]);
        assert_eq!(rs("assert!((value == 1));"), vec!["assert_compare"]);
    }

    #[test]
    fn mock_assertions_pair_by_their_receiver() {
        let py = compare(
            Lang::Python,
            PythonStyle::Pytest,
            "test_a.py",
            "def test_t():\n    a.assert_called_once_with(1)\n    b.assert_called_once()\n",
            "def test_t():\n    b.assert_called_once_with(1)\n    a.assert_called_once()\n",
        );
        assert_eq!(
            kinds(&py),
            vec![(FindingKind::MatcherLoosened, "test_t".into())]
        );
    }

    #[test]
    fn snapshot_files_are_recognized() {
        assert!(is_snapshot("src/__snapshots__/a.test.ts.snap"));
        assert!(is_snapshot("tests/__snapshots__/test_a.ambr"));
        assert!(!is_snapshot("src/snapshot.ts"));
    }
}
