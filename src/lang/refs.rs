//! Table-driven tests: which tests read a declaration that a change edits outside every
//! test span, such as a new case in a table that a loop or `parametrize` runs. Names are
//! matched by text within the scope that binds them; shadowing and Rust module paths are
//! not resolved.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use tree_sitter::{Node, Tree};

use super::{Lang, TestDef, end_line_of, line_of, text};

/// What a binding holds, which decides the uses that can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Value,
    /// A function, which a pytest test also reads as a fixture through a parameter.
    Function,
    /// A Python `self.name` or `cls.name` assignment, read only as an attribute.
    Attribute,
}

/// How a name appears where it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    Name,
    /// A Python parameter, which names a fixture and reads nothing else.
    Parameter,
    /// A Python `self.name` or `cls.name`.
    Attribute,
}

fn can_read(kind: Kind, used: Use) -> bool {
    match kind {
        Kind::Attribute => used == Use::Attribute,
        Kind::Value => used != Use::Parameter,
        Kind::Function => true,
    }
}

/// A name a declaration binds, and the byte range it is visible in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Binding {
    name: String,
    kind: Kind,
    scope: (usize, usize),
}

struct Declaration {
    bindings: Vec<Binding>,
    start_byte: usize,
    end_byte: usize,
    line: u32,
    end_line: u32,
    /// Lines that belong to a nested declaration instead, such as each method of a Rust
    /// `impl`, so an edit there changes the method and not the whole type.
    holes: Vec<(u32, u32)>,
}

impl Declaration {
    fn holds_line(&self, line: u32) -> bool {
        (self.line..=self.end_line).contains(&line)
            && !self.holes.iter().any(|(s, e)| (*s..=*e).contains(&line))
    }
}

struct Index {
    lang: Lang,
    tree: Tree,
    declarations: Vec<Declaration>,
    references: HashMap<String, Vec<(usize, Use)>>,
}

impl Index {
    /// `rust_source` limits declarations to a Rust source file's test code, so that the
    /// code under test never counts as test data.
    fn new(lang: Lang, source: &str, rust_source: bool) -> Option<Index> {
        let tree = lang.parse(source)?;
        let nodes = preorder(tree.root_node());
        let mut declarations = Vec::new();
        let mut declaring: HashSet<usize> = HashSet::new();
        for node in &nodes {
            if let Some(d) = declaration(lang, *node, source, &mut declaring) {
                declarations.push(d);
            }
        }
        if rust_source {
            let ranges = super::rust::test_ranges(source);
            declarations.retain(|d| {
                ranges
                    .iter()
                    .any(|(s, e)| *s <= d.start_byte && d.end_byte <= *e)
            });
        }
        let mut references: HashMap<String, Vec<(usize, Use)>> = HashMap::new();
        for node in &nodes {
            if declaring.contains(&node.start_byte()) && is_name(node.kind()) {
                continue;
            }
            if let Some(used) = reference(lang, *node, source) {
                references
                    .entry(text(*node, source).to_string())
                    .or_default()
                    .push((node.start_byte(), used));
            }
        }
        Some(Index {
            lang,
            tree,
            declarations,
            references,
        })
    }

    /// Whether `range` reads `binding` at a place the binding is visible.
    fn reads(&self, binding: &Binding, range: (usize, usize)) -> bool {
        self.references.get(&binding.name).is_some_and(|at| {
            at.iter().any(|(b, used)| {
                range.0 <= *b
                    && *b < range.1
                    && binding.scope.0 <= *b
                    && *b < binding.scope.1
                    && can_read(binding.kind, *used)
            })
        })
    }

    /// The bindings of the declarations at `seeds`, and of every declaration that reads
    /// one of them, such as a table that spreads another.
    fn propagate(&self, seeds: impl IntoIterator<Item = usize>) -> BTreeSet<Binding> {
        let mut done = vec![false; self.declarations.len()];
        let mut changed = BTreeSet::new();
        let mut queue = VecDeque::new();
        let mark = |i: usize, changed: &mut BTreeSet<Binding>, queue: &mut VecDeque<Binding>| {
            for b in &self.declarations[i].bindings {
                if changed.insert(b.clone()) {
                    queue.push_back(b.clone());
                }
            }
        };
        for i in seeds {
            if !std::mem::replace(&mut done[i], true) {
                mark(i, &mut changed, &mut queue);
            }
        }
        while let Some(binding) = queue.pop_front() {
            let Some(at) = self.references.get(&binding.name) else {
                continue;
            };
            for (byte, used) in at {
                let visible = binding.scope.0 <= *byte && *byte < binding.scope.1;
                if !visible || !can_read(binding.kind, *used) {
                    continue;
                }
                for (i, d) in self.declarations.iter().enumerate() {
                    if !done[i] && d.start_byte <= *byte && *byte < d.end_byte {
                        done[i] = true;
                        mark(i, &mut changed, &mut queue);
                    }
                }
            }
        }
        changed
    }

    /// Indexes into `tests` of the tests that read one of `changed`.
    fn readers(&self, tests: &[TestDef], changed: &BTreeSet<Binding>) -> BTreeSet<usize> {
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
            let in_span = visible.iter().any(|b| self.reads(b, span));
            let in_context = || {
                context_ranges(self.lang, self.tree.root_node(), span)
                    .into_iter()
                    .any(|range| visible.iter().any(|b| self.reads(b, range)))
            };
            if in_span || in_context() {
                out.insert(i);
            }
        }
        out
    }
}

/// Indexes into `tests` of the tests that read a changed declaration. A declaration is
/// changed when a changed line outside every test span falls inside it, or when it reads
/// another changed declaration. A test reads a declaration that is visible where the test
/// is when its own span names it, or when an enclosing construct names it beside the test,
/// such as the iterable of a `for` loop or the receiver of a `forEach`. With
/// `rust_source`, only test code counts as data, and a line it shares with other code
/// does not count.
pub fn tests_reading_changed_data(
    lang: Lang,
    source: &str,
    tests: &[TestDef],
    added: &BTreeSet<u32>,
    rust_source: bool,
) -> BTreeSet<usize> {
    let mut outside: Vec<u32> = added
        .iter()
        .copied()
        .filter(|l| !tests.iter().any(|t| (t.line..=t.end_line).contains(l)))
        .collect();
    if rust_source {
        let test_lines = super::rust::test_lines(source);
        outside.retain(|l| test_lines.iter().any(|(s, e)| (*s..=*e).contains(l)));
    }
    if outside.is_empty() {
        return BTreeSet::new();
    }
    let Some(index) = Index::new(lang, source, rust_source) else {
        return BTreeSet::new();
    };
    let seeds: Vec<usize> = (0..index.declarations.len())
        .filter(|i| {
            outside
                .iter()
                .any(|l| index.declarations[*i].holds_line(*l))
        })
        .collect();
    let changed = index.propagate(seeds);
    index.readers(tests, &changed)
}

/// The tests that read the test data at `line`, such as a table whose row does not
/// compile against base, and the text of each declaration that holds the line. With
/// `rust_source`, only test code counts as data.
pub fn readers_of_data_at(
    lang: Lang,
    source: &str,
    tests: &[TestDef],
    line: u32,
    rust_source: bool,
) -> (BTreeSet<usize>, Vec<String>) {
    let Some(index) = Index::new(lang, source, rust_source) else {
        return (BTreeSet::new(), Vec::new());
    };
    let seeds: Vec<usize> = (0..index.declarations.len())
        .filter(|i| index.declarations[*i].holds_line(line))
        .collect();
    let texts = seeds
        .iter()
        .map(|i| {
            let d = &index.declarations[*i];
            source[d.start_byte..d.end_byte].to_string()
        })
        .collect();
    let changed = index.propagate(seeds);
    (index.readers(tests, &changed), texts)
}

/// `source` without the declarations whose text is one of `texts`, together with the
/// attributes in front of a Rust item. With `rust_source`, only test code is removed.
pub fn without_declarations(
    lang: Lang,
    source: &str,
    texts: &[String],
    rust_source: bool,
) -> String {
    let Some(index) = Index::new(lang, source, rust_source) else {
        return source.to_string();
    };
    let root = index.tree.root_node();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for d in &index.declarations {
        if !texts.iter().any(|t| *t == source[d.start_byte..d.end_byte]) {
            continue;
        }
        let mut start = d.start_byte;
        if let Some(node) = root.descendant_for_byte_range(d.start_byte, d.end_byte) {
            let mut prev = node.prev_named_sibling();
            while let Some(p) = prev.filter(|p| p.kind() == "attribute_item") {
                start = p.start_byte();
                prev = p.prev_named_sibling();
            }
        }
        spans.push((start, d.end_byte));
    }
    spans.sort();
    let mut out = source.to_string();
    let mut last_start = usize::MAX;
    for (start, end) in spans.into_iter().rev() {
        // A span inside one already removed went with it.
        if end > last_start {
            continue;
        }
        out.replace_range(start..end, "");
        last_start = start;
    }
    out
}

/// Every named node under `root`, parents before children. Iterative, since a long
/// expression can nest deeper than the stack allows.
fn preorder(root: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_named() {
            out.push(node);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
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
/// test is passed into. Other statements and branches are left out, since they do not
/// decide whether the test is registered.
fn context_ranges(lang: Lang, root: Node, span: (usize, usize)) -> Vec<(usize, usize)> {
    let Some(mut node) = root.descendant_for_byte_range(span.0, span.1) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Some(parent) = node.parent() {
        let on_path = node;
        node = parent;
        if is_block(lang, parent.kind()) {
            continue;
        }
        let name = parent.child_by_field_name("name").map(|n| n.id());
        let mut cursor = parent.walk();
        for child in parent.named_children(&mut cursor) {
            let kind = child.kind();
            let holds_test = child.start_byte() <= span.0 && span.1 <= child.end_byte();
            let branch =
                is_block(lang, kind) || kind.ends_with("_clause") || kind.ends_with("_statement");
            if holds_test || branch || Some(child.id()) == name || child.id() == on_path.id() {
                continue;
            }
            out.push((child.start_byte(), child.end_byte()));
        }
    }
    out
}

const TS_FUNCTIONS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_expression",
    "function",
    "generator_function",
    "arrow_function",
    "method_definition",
];

/// The byte range where a name that `node` binds is visible: the innermost block for most
/// bindings, the function body for a JavaScript `var`, the function, class, or module for
/// Python, and the enclosing items for a Rust method.
fn scope_of(lang: Lang, node: Node) -> (usize, usize) {
    let var = lang.is_ts()
        && node
            .parent()
            .is_some_and(|p| p.kind() == "variable_declaration");
    let mut up = node.parent();
    while let Some(n) = up {
        let parent = n.parent().map(|p| p.kind()).unwrap_or_default();
        let scope = match (lang, n.kind()) {
            (Lang::Python, "module") => true,
            (Lang::Python, "block") => {
                matches!(parent, "function_definition" | "class_definition")
            }
            (Lang::TypeScript | Lang::Tsx, "statement_block") if var => {
                TS_FUNCTIONS.contains(&parent)
            }
            (Lang::Rust, "declaration_list") => !matches!(parent, "impl_item" | "trait_item"),
            (_, kind) => is_block(lang, kind),
        };
        if scope {
            return (n.start_byte(), n.end_byte());
        }
        up = n.parent();
    }
    (0, usize::MAX)
}

fn is_name(kind: &str) -> bool {
    matches!(
        kind,
        "identifier" | "type_identifier" | "shorthand_property_identifier_pattern"
    )
}

/// The declaration `node` is, when it binds names whose values tests can read. Imports
/// are left out: a test that a moved import touches has not changed what it checks. The
/// start of each identifier that names a binding goes into `declaring`.
fn declaration<'a>(
    lang: Lang,
    node: Node<'a>,
    source: &str,
    declaring: &mut HashSet<usize>,
) -> Option<Declaration> {
    let mut names: Vec<Node<'a>> = Vec::new();
    let mut kind = Kind::Value;
    let mut scope = None;
    let mut holes = Vec::new();
    let name_field = |names: &mut Vec<Node<'a>>| names.extend(node.child_by_field_name("name"));
    match (lang, node.kind()) {
        (Lang::TypeScript | Lang::Tsx, "variable_declarator") => {
            if let Some(pattern) = node.child_by_field_name("name") {
                pattern_names(pattern, &mut names);
            }
        }
        (
            Lang::TypeScript | Lang::Tsx,
            "function_declaration" | "generator_function_declaration",
        ) => {
            kind = Kind::Function;
            name_field(&mut names);
        }
        (
            Lang::TypeScript | Lang::Tsx,
            "class_declaration" | "abstract_class_declaration" | "enum_declaration",
        ) => name_field(&mut names),
        (Lang::Python, "assignment" | "augmented_assignment") => {
            let left = node.child_by_field_name("left")?;
            if left.kind() == "attribute" {
                // `self.cases = [...]` in `setUp` binds data for the class's tests.
                let object = left.child_by_field_name("object")?;
                if !matches!(text(object, source), "self" | "cls") {
                    return None;
                }
                let class = ancestor(node, "class_definition")?;
                let body = class.child_by_field_name("body")?;
                names.extend(left.child_by_field_name("attribute"));
                kind = Kind::Attribute;
                scope = Some((body.start_byte(), body.end_byte()));
            } else {
                pattern_names(left, &mut names);
            }
        }
        (Lang::Python, "function_definition") => {
            kind = Kind::Function;
            name_field(&mut names);
        }
        (Lang::Python, "class_definition") => name_field(&mut names),
        (Lang::Python, "decorated_definition") => {
            let definition = node.child_by_field_name("definition")?;
            if definition.kind() == "function_definition" {
                kind = Kind::Function;
            }
            names.extend(definition.child_by_field_name("name"));
        }
        (
            Lang::Rust,
            "const_item" | "static_item" | "function_item" | "struct_item" | "enum_item"
            | "union_item" | "type_item" | "macro_definition" | "trait_item",
        ) => name_field(&mut names),
        // An edit to an `impl` outside its methods, such as to an associated const,
        // changes the type for every test that uses it.
        (Lang::Rust, "impl_item") => {
            let ty = node.child_by_field_name("type")?;
            names.push(ty.child_by_field_name("type").unwrap_or(ty));
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                holes = body
                    .named_children(&mut cursor)
                    .filter(|c| c.kind() == "function_item")
                    .map(|c| (line_of(c), end_line_of(c)))
                    .collect();
            }
        }
        _ => return None,
    }
    if names.is_empty() {
        return None;
    }
    let scope = scope.unwrap_or_else(|| scope_of(lang, node));
    let bindings = names
        .iter()
        .map(|n| {
            declaring.insert(n.start_byte());
            Binding {
                name: text(*n, source).to_string(),
                kind,
                scope,
            }
        })
        .collect();
    Some(Declaration {
        bindings,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        line: line_of(node),
        end_line: end_line_of(node),
        holes,
    })
}

fn ancestor<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut up = node.parent();
    while let Some(n) = up {
        if n.kind() == kind {
            return Some(n);
        }
        up = n.parent();
    }
    None
}

/// The identifiers a binding pattern binds, such as each name in `const [a, { b }] = x` or
/// `a, b = x`. Attribute and subscript targets bind nothing new, and in `{ key: value }`
/// only the value side binds.
fn pattern_names<'a>(pattern: Node<'a>, out: &mut Vec<Node<'a>>) {
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => out.push(node),
            "attribute" | "subscript" | "member_expression" | "subscript_expression" => {}
            "pair_pattern" => stack.extend(node.child_by_field_name("value")),
            _ => {
                let mut cursor = node.walk();
                let children: Vec<Node> = node.named_children(&mut cursor).collect();
                stack.extend(children.into_iter().rev());
            }
        }
    }
}

/// How `node` reads a name, when it is one that can name a declaration.
fn reference(lang: Lang, node: Node, source: &str) -> Option<Use> {
    let kind = node.kind();
    match lang {
        Lang::TypeScript | Lang::Tsx => matches!(
            kind,
            "identifier" | "type_identifier" | "shorthand_property_identifier"
        )
        .then_some(Use::Name),
        // A method call names its method with a field identifier.
        Lang::Rust => matches!(kind, "identifier" | "type_identifier" | "field_identifier")
            .then_some(Use::Name),
        Lang::Python => (kind == "identifier")
            .then(|| python_use(node, source))
            .flatten(),
    }
}

/// How a Python identifier reads a name. An attribute counts only on `self` or `cls`, and
/// a keyword argument's name reads nothing.
fn python_use(node: Node, source: &str) -> Option<Use> {
    let Some(parent) = node.parent() else {
        return Some(Use::Name);
    };
    let is_field = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|n| n.id() == node.id())
    };
    match parent.kind() {
        "attribute" if is_field("attribute") => parent
            .child_by_field_name("object")
            .is_some_and(|o| matches!(text(o, source), "self" | "cls"))
            .then_some(Use::Attribute),
        "keyword_argument" if is_field("name") => None,
        "parameters" | "lambda_parameters" => Some(Use::Parameter),
        "default_parameter" | "typed_default_parameter" if is_field("name") => Some(Use::Parameter),
        "typed_parameter" if parent.named_child(0).is_some_and(|n| n.id() == node.id()) => {
            Some(Use::Parameter)
        }
        _ => Some(Use::Name),
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
        tests_reading_changed_data(lang, source, &tests, &added, false)
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
        let tests = find_tests(Lang::Rust, RUST, PythonStyle::Pytest);
        let names = |lines: &[u32]| -> Vec<String> {
            let added: BTreeSet<u32> = lines.iter().copied().collect();
            tests_reading_changed_data(Lang::Rust, RUST, &tests, &added, true)
                .into_iter()
                .map(|i| tests[i].name.clone())
                .collect()
        };
        assert_eq!(names(&[10]), vec!["table"]);
        assert_eq!(names(&[17]), vec!["probe"]);
        // The code under test is not test data.
        assert!(names(&[2]).is_empty());
    }

    #[test]
    fn each_declarator_and_branch_stands_alone() {
        let source = r#"const rows = [
  1,
], other = [2];
test.each(rows)("rows %i", () => {});
test.each(other)("other %i", () => {});
if (process.env.X) {
  rows.forEach((n) => it(`case ${n}`, () => {}));
} else {
  it("fallback", () => {});
}
"#;
        assert_eq!(
            reading(Lang::TypeScript, source, &[2]),
            vec!["rows %i", "case ${n}"]
        );
    }

    #[test]
    fn var_and_python_bindings_reach_past_their_block() {
        let ts =
            "if (true) {\n  var rows = [\n    1,\n  ];\n}\ntest.each(rows)(\"r\", () => {});\n";
        assert_eq!(reading(Lang::TypeScript, ts, &[3]), vec!["r"]);
        let py = r#"import sys

if sys.platform:
    CASES = [
        1,
    ]


@pytest.mark.parametrize("x", CASES)
def test_cases(x):
    pass
"#;
        assert_eq!(reading(Lang::Python, py, &[5]), vec!["test_cases"]);
    }

    #[test]
    fn python_parameters_read_only_fixtures_and_set_up_data_counts() {
        let params = r#"cases = [
    1,
]


@pytest.mark.parametrize("cases", [1])
def test_other(cases):
    pass
"#;
        assert!(reading(Lang::Python, params, &[2]).is_empty());
        let set_up = r#"class TestRows:
    def setUp(self):
        self.cases = [
            1,
        ]

    def test_rows(self):
        for c in self.cases:
            pass

    def test_other(self):
        pass
"#;
        assert_eq!(reading(Lang::Python, set_up, &[4]), vec!["test_rows"]);
    }

    #[test]
    fn a_rust_method_edit_changes_only_its_callers_and_mixed_lines_are_not_data() {
        let source = r#"pub fn f() -> i32 { 1 } #[cfg(test)] const SHARED: &[i32] = &[1];

#[cfg(test)]
mod tests {
    struct Probe;

    impl Probe {
        fn first(&self) -> i32 {
            1
        }
        fn second(&self) -> i32 {
            2
        }
    }

    #[test]
    fn probe_first() {
        assert_eq!(Probe.first(), 1);
    }

    #[test]
    fn probe_second() {
        assert_eq!(Probe.second(), 2);
    }

    #[test]
    fn shared() {
        assert_eq!(super::SHARED.len(), 1);
    }
}
"#;
        let tests = find_tests(Lang::Rust, source, PythonStyle::Pytest);
        let names = |lines: &[u32]| -> Vec<String> {
            let added: BTreeSet<u32> = lines.iter().copied().collect();
            tests_reading_changed_data(Lang::Rust, source, &tests, &added, true)
                .into_iter()
                .map(|i| tests[i].name.clone())
                .collect()
        };
        assert_eq!(names(&[12]), vec!["probe_second"]);
        assert!(names(&[1]).is_empty());
    }

    #[test]
    fn a_deeply_nested_initializer_does_not_overflow_the_stack() {
        let source = format!(
            "X = (\n    {}\n)\n\n\ndef test_x():\n    assert X\n",
            vec!["1"; 20_000].join(" + ")
        );
        let worker = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || reading(Lang::Python, &source, &[2]))
            .unwrap();
        assert_eq!(worker.join().unwrap(), vec!["test_x"]);
    }
}
