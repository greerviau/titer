//! Table-driven tests: which tests read a declaration that a change edits outside every
//! test span, such as a new case in a table that a loop or `parametrize` runs.

use std::collections::{BTreeSet, HashMap};

use tree_sitter::Node;

use super::{Lang, TestDef, children, end_line_of, line_of, text};

/// A name a declaration binds, and the byte range of the block it is visible in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Binding {
    name: String,
    scope: (usize, usize),
}

struct Declaration {
    names: Vec<String>,
    start_byte: usize,
    end_byte: usize,
    line: u32,
    end_line: u32,
    scope: (usize, usize),
}

/// Indexes into `tests` of the tests that read a changed declaration. A declaration is
/// changed when a changed line outside every test span falls inside it, or when it reads
/// another changed declaration. A test reads a declaration that is visible where the test
/// is when its own span names it, or when an enclosing construct does outside the
/// statements it holds, such as the iterable of a `for` loop or the receiver of a
/// `forEach`. `test_code`, when given, limits declarations to those byte ranges, so that a
/// Rust source file's own code never counts as test data. Shadowing is not resolved, so a
/// name bound again in an inner block still counts.
pub fn tests_reading_changed_data(
    lang: Lang,
    source: &str,
    tests: &[TestDef],
    added: &BTreeSet<u32>,
    test_code: Option<&[(usize, usize)]>,
) -> BTreeSet<usize> {
    let outside: Vec<u32> = added
        .iter()
        .copied()
        .filter(|l| !tests.iter().any(|t| (t.line..=t.end_line).contains(l)))
        .collect();
    if outside.is_empty() {
        return BTreeSet::new();
    }
    let Some(tree) = lang.parse(source) else {
        return BTreeSet::new();
    };
    let root = tree.root_node();
    let mut declarations = Vec::new();
    collect_declarations(lang, root, source, &mut declarations);
    declarations.retain(|d| {
        test_code.is_none_or(|ranges| {
            ranges
                .iter()
                .any(|(s, e)| *s <= d.start_byte && d.end_byte <= *e)
        })
    });
    let mut references: HashMap<String, Vec<usize>> = HashMap::new();
    collect_references(lang, root, source, &mut references);
    let reads = |name: &str, scope: (usize, usize), range: (usize, usize)| {
        references.get(name).is_some_and(|at| {
            at.iter()
                .any(|b| range.0 <= *b && *b < range.1 && scope.0 <= *b && *b < scope.1)
        })
    };

    let mut changed: BTreeSet<Binding> = BTreeSet::new();
    let mut done = vec![false; declarations.len()];
    for (i, d) in declarations.iter().enumerate() {
        if outside.iter().any(|l| (d.line..=d.end_line).contains(l)) {
            done[i] = true;
            changed.extend(d.names.iter().map(|name| Binding {
                name: name.clone(),
                scope: d.scope,
            }));
        }
    }
    if changed.is_empty() {
        return BTreeSet::new();
    }
    // A declaration built from a changed one, such as a table that spreads another, also
    // changes.
    loop {
        let mut grew = false;
        for (i, d) in declarations.iter().enumerate() {
            if done[i]
                || !changed
                    .iter()
                    .any(|b| reads(&b.name, b.scope, (d.start_byte, d.end_byte)))
            {
                continue;
            }
            done[i] = true;
            grew = true;
            changed.extend(d.names.iter().map(|name| Binding {
                name: name.clone(),
                scope: d.scope,
            }));
        }
        if !grew {
            break;
        }
    }

    let mut out = BTreeSet::new();
    for (i, test) in tests.iter().enumerate() {
        let span = (test.start_byte, test.end_byte);
        let visible: Vec<&Binding> = changed
            .iter()
            .filter(|b| b.scope.0 <= span.0 && span.1 <= b.scope.1)
            .collect();
        if visible.is_empty() {
            continue;
        }
        let in_span = visible.iter().any(|b| reads(&b.name, b.scope, span));
        let in_context = || {
            context_ranges(lang, root, span)
                .into_iter()
                .any(|range| visible.iter().any(|b| reads(&b.name, b.scope, range)))
        };
        if in_span || in_context() {
            out.insert(i);
        }
    }
    out
}

/// Kinds whose children are separate statements or items. A name in one of them says
/// nothing about the others, so the walk up from a test skips their contents.
fn is_block(lang: Lang, kind: &str) -> bool {
    match lang {
        Lang::Python => matches!(kind, "module" | "block"),
        Lang::TypeScript | Lang::Tsx => {
            matches!(kind, "program" | "statement_block" | "class_body")
        }
        Lang::Rust => matches!(kind, "source_file" | "declaration_list" | "block"),
    }
}

/// The byte ranges around the test at `span` that its enclosing constructs hold beside it,
/// such as the header of a `for` loop or the callee and other arguments of a call that the
/// test is passed into. A block's other statements are left out.
fn context_ranges(lang: Lang, root: Node, span: (usize, usize)) -> Vec<(usize, usize)> {
    let Some(mut node) = root.descendant_for_byte_range(span.0, span.1) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    loop {
        let on_path = node;
        let Some(parent) = on_path.parent() else {
            break;
        };
        node = parent;
        if is_block(lang, parent.kind()) {
            continue;
        }
        let name = parent.child_by_field_name("name").map(|n| n.id());
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            let holds_test = child.start_byte() <= span.0 && span.1 <= child.end_byte();
            if holds_test || Some(child.id()) == name || child.id() == on_path.id() {
                continue;
            }
            out.push((child.start_byte(), child.end_byte()));
        }
    }
    out
}

/// The innermost block that holds `node`, which is where the names it binds are visible.
fn scope_of(lang: Lang, node: Node) -> (usize, usize) {
    let mut up = node.parent();
    while let Some(n) = up {
        if is_block(lang, n.kind()) {
            return (n.start_byte(), n.end_byte());
        }
        up = n.parent();
    }
    (0, usize::MAX)
}

fn collect_declarations(lang: Lang, node: Node, source: &str, out: &mut Vec<Declaration>) {
    let names = declared_names(lang, node, source);
    if !names.is_empty() {
        out.push(Declaration {
            names,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            line: line_of(node),
            end_line: end_line_of(node),
            scope: scope_of(lang, node),
        });
    }
    for child in children(node) {
        collect_declarations(lang, child, source, out);
    }
}

/// The names `node` binds, when it is a declaration whose value tests can read. Imports
/// are left out: a test that a moved import touches has not changed what it checks.
fn declared_names(lang: Lang, node: Node, source: &str) -> Vec<String> {
    let name_field = || {
        node.child_by_field_name("name")
            .map(|n| vec![text(n, source).to_string()])
            .unwrap_or_default()
    };
    match (lang, node.kind()) {
        (Lang::TypeScript | Lang::Tsx, "lexical_declaration" | "variable_declaration") => {
            let mut names = Vec::new();
            for declarator in children(node) {
                if let Some(pattern) = declarator.child_by_field_name("name") {
                    pattern_names(pattern, source, &mut names);
                }
            }
            names
        }
        (
            Lang::TypeScript | Lang::Tsx,
            "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration"
            | "enum_declaration",
        ) => name_field(),
        (Lang::Python, "assignment" | "augmented_assignment") => {
            let mut names = Vec::new();
            if let Some(left) = node.child_by_field_name("left") {
                pattern_names(left, source, &mut names);
            }
            names
        }
        (Lang::Python, "function_definition" | "class_definition") => name_field(),
        (Lang::Python, "decorated_definition") => node
            .child_by_field_name("definition")
            .and_then(|d| d.child_by_field_name("name"))
            .map(|n| vec![text(n, source).to_string()])
            .unwrap_or_default(),
        (
            Lang::Rust,
            "const_item" | "static_item" | "function_item" | "struct_item" | "enum_item"
            | "union_item" | "type_item" | "macro_definition" | "trait_item",
        ) => name_field(),
        // A changed method changes what its type does for every test that uses the type.
        (Lang::Rust, "impl_item") => node
            .child_by_field_name("type")
            .map(|t| {
                let t = t.child_by_field_name("type").unwrap_or(t);
                vec![text(t, source).to_string()]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The identifiers a binding pattern binds, such as each name in `const [a, { b }] = x` or
/// `a, b = x`. Attribute and subscript targets bind nothing new.
fn pattern_names(node: Node, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            out.push(text(node, source).to_string());
        }
        "attribute" | "subscript" | "member_expression" | "subscript_expression" => {}
        // In `{ key: value }` only the value side binds.
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                pattern_names(value, source, out);
            }
        }
        _ => {
            for child in children(node) {
                pattern_names(child, source, out);
            }
        }
    }
}

/// Every place a name is read, by the byte where it starts.
fn collect_references(lang: Lang, node: Node, source: &str, out: &mut HashMap<String, Vec<usize>>) {
    let kind = node.kind();
    let is_ref = matches!(
        kind,
        "identifier" | "type_identifier" | "shorthand_property_identifier"
    ) && !(lang.is_python() && is_python_non_reference(node, source));
    if is_ref {
        out.entry(text(node, source).to_string())
            .or_default()
            .push(node.start_byte());
    }
    for child in children(node) {
        collect_references(lang, child, source, out);
    }
}

/// A Python identifier that names an attribute or a keyword argument rather than reading a
/// variable. `self.cases` and `cls.cases` still count, since a test method reads class-level
/// data that way.
fn is_python_non_reference(node: Node, source: &str) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let is_field = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|n| n.id() == node.id())
    };
    match parent.kind() {
        "attribute" if is_field("attribute") => !parent
            .child_by_field_name("object")
            .is_some_and(|o| matches!(text(o, source), "self" | "cls")),
        "keyword_argument" => is_field("name"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PythonStyle, find_tests};
    use super::*;

    /// The names of the tests that read changed data when `lines` change.
    fn reading(lang: Lang, source: &str, lines: &[u32]) -> Vec<String> {
        let tests = find_tests(lang, source, PythonStyle::Pytest);
        let added: BTreeSet<u32> = lines.iter().copied().collect();
        tests_reading_changed_data(lang, source, &tests, &added, None)
            .into_iter()
            .map(|i| tests[i].name.clone())
            .collect()
    }

    const UFO: &str = r#"import { describe, expect, test } from "vitest";

describe("withBase", () => {
  const tests = [
    { base: "/", input: "/", out: "/" },
  ];

  for (const t of tests) {
    test(t.input, () => {
      expect(t.out).toBe(t.out);
    });
  }

  test("plain", () => {});
});

describe("withoutBase", () => {
  const tests = [
    { base: "/", input: "/", out: "/" },
  ];

  for (const t of tests) {
    test(t.base, () => {});
  }
});
"#;

    #[test]
    fn a_table_row_changes_only_the_loop_test_in_its_own_describe() {
        assert_eq!(reading(Lang::TypeScript, UFO, &[5]), vec!["t.input"]);
        assert_eq!(reading(Lang::TypeScript, UFO, &[19]), vec!["t.base"]);
    }

    #[test]
    fn a_line_inside_a_test_or_in_an_unread_declaration_adds_nothing() {
        assert!(reading(Lang::TypeScript, UFO, &[10]).is_empty());
        let source = "const unused = 1;\nconst cases = [1];\ntest.each(cases)(\"x\", () => {});\n";
        assert!(reading(Lang::TypeScript, source, &[1]).is_empty());
        assert!(
            reading(
                Lang::TypeScript,
                "import { a } from \"./a\";\ntest(\"x\", () => a());\n",
                &[1]
            )
            .is_empty()
        );
    }

    #[test]
    fn each_tables_for_each_receivers_and_derived_tables_count() {
        let source = r#"const base = [1];
const cases = [...base, 2];
test.each(cases)("each %i", (n) => {});
cases.forEach((n) => {
  it(`loop ${n}`, () => {});
});
describe.each(cases)("suite %i", (n) => {
  it("inside", () => {});
});
function check(n) {
  return n > 0;
}
it("calls", () => { check(1); });
it("other", () => {});
"#;
        assert_eq!(
            reading(Lang::TypeScript, source, &[1]),
            vec!["each %i", "loop ${n}", "inside"]
        );
        assert_eq!(reading(Lang::TypeScript, source, &[11]), vec!["calls"]);
    }

    #[test]
    fn python_tables_fixtures_and_class_data_count() {
        let source = r#"import pytest

CASES = [
    (1, 1),
]


@pytest.fixture
def client():
    return 1


@pytest.mark.parametrize("x,out", CASES)
def test_cases(x, out):
    assert x == out


def test_client(client):
    assert client


def test_keyword():
    f(CASES=1)


class TestThing:
    rows = [1]

    def test_rows(self):
        assert self.rows
"#;
        assert_eq!(reading(Lang::Python, source, &[4]), vec!["test_cases"]);
        assert_eq!(reading(Lang::Python, source, &[10]), vec!["test_client"]);
        assert_eq!(reading(Lang::Python, source, &[27]), vec!["test_rows"]);
    }

    const RUST: &str = r#"pub fn clamp(x: i32) -> i32 {
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[i32] = &[
        1,
    ];

    struct Probe;

    impl Probe {
        fn run(&self) -> i32 {
            1
        }
    }

    #[test]
    fn table() {
        for x in CASES {
            assert_eq!(clamp(*x), *x);
        }
    }

    #[test]
    fn probe() {
        assert_eq!(Probe.run(), 1);
    }

    #[test]
    fn plain() {
        assert_eq!(clamp(1), 1);
    }
}
"#;

    #[test]
    fn rust_tables_and_helper_types_in_test_code_count() {
        let ranges = super::super::rust::test_ranges(RUST);
        let tests = find_tests(Lang::Rust, RUST, PythonStyle::Pytest);
        let names = |lines: &[u32]| -> Vec<String> {
            let added: BTreeSet<u32> = lines.iter().copied().collect();
            tests_reading_changed_data(Lang::Rust, RUST, &tests, &added, Some(&ranges))
                .into_iter()
                .map(|i| tests[i].name.clone())
                .collect()
        };
        assert_eq!(names(&[10]), vec!["table"]);
        assert_eq!(names(&[17]), vec!["probe"]);
        // The code under test is not test data.
        assert!(names(&[2]).is_empty());
    }
}
