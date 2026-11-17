//! TypeScript, TSX, and JavaScript: tests, functions, and other structure.

use tree_sitter::Node;

use super::{FunctionDef, TestDef, children, end_line_of, line_of, text};

/// What a call's callee says about it: `describe.skip(...)`, `it.each([...])(...)`.
#[derive(Debug)]
struct Callee {
    root: String,
    modifiers: Vec<String>,
}

fn callee(node: Node, source: &str) -> Option<Callee> {
    match node.kind() {
        "identifier" => Some(Callee {
            root: text(node, source).to_string(),
            modifiers: Vec::new(),
        }),
        "member_expression" => {
            let mut inner = callee(node.child_by_field_name("object")?, source)?;
            let property = node.child_by_field_name("property")?;
            inner.modifiers.push(text(property, source).to_string());
            Some(inner)
        }
        // A curried call such as `it.each(table)` or `test.skipIf(cond)`.
        "call_expression" => callee(node.child_by_field_name("function")?, source),
        _ => None,
    }
}

const TEST_ROOTS: &[&str] = &["it", "test", "xit", "xtest", "fit"];
const SUITE_ROOTS: &[&str] = &["describe", "suite", "xdescribe", "fdescribe", "context"];

fn is_static_skip(c: &Callee) -> bool {
    c.root.starts_with('x') || c.modifiers.iter().any(|m| m == "skip" || m == "todo")
}

/// The test or suite title: the string's contents, or the expression's source.
pub(crate) fn title(node: Node, source: &str) -> String {
    match node.kind() {
        "string" => {
            let raw = text(node, source);
            raw.get(1..raw.len().saturating_sub(1))
                .unwrap_or_default()
                .replace("\\'", "'")
                .replace("\\\"", "\"")
        }
        "template_string" => {
            let raw = text(node, source);
            raw.trim_start_matches('`')
                .trim_end_matches('`')
                .to_string()
        }
        _ => text(node, source).to_string(),
    }
}

pub fn find_tests(root: Node, source: &str) -> Vec<TestDef> {
    let mut out = Vec::new();
    walk_tests(root, source, &mut Vec::new(), false, &mut out);
    out
}

fn walk_tests(
    node: Node,
    source: &str,
    suite: &mut Vec<String>,
    skipped: bool,
    out: &mut Vec<TestDef>,
) {
    if node.kind() == "call_expression"
        && let Some(func) = node.child_by_field_name("function")
        && let Some(c) = callee(func, source)
        && let Some(args) = node.child_by_field_name("arguments")
    {
        let first = children(args).into_iter().next();
        let is_test = TEST_ROOTS.contains(&c.root.as_str());
        let is_suite = SUITE_ROOTS.contains(&c.root.as_str());
        if (is_test || is_suite)
            && let Some(first) = first
        {
            let name = title(first, source);
            let skip = skipped || is_static_skip(&c);
            if is_test {
                out.push(TestDef {
                    suite: suite.clone(),
                    name,
                    line: line_of(node),
                    end_line: end_line_of(node),
                    // vitest reports a test at the opening paren of its outermost call.
                    report_line: line_of(args),
                    skip,
                });
                return;
            }
            suite.push(name);
            for child in children(args) {
                walk_tests(child, source, suite, skip, out);
            }
            suite.pop();
            return;
        }
    }
    for child in children(node) {
        walk_tests(child, source, suite, skipped, out);
    }
}

const FUNCTION_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_expression",
    "function",
    "generator_function",
    "arrow_function",
    "method_definition",
];

/// The name a function goes by: its own name, or the variable, key, or field it is
/// assigned to.
fn function_name(node: Node, source: &str) -> String {
    if let Some(name) = node.child_by_field_name("name") {
        return text(name, source).to_string();
    }
    if let Some(parent) = node.parent() {
        let named = match parent.kind() {
            "variable_declarator" => parent.child_by_field_name("name"),
            "pair" => parent.child_by_field_name("key"),
            "assignment_expression" => parent.child_by_field_name("left"),
            "public_field_definition" | "field_definition" => parent
                .child_by_field_name("name")
                .or_else(|| parent.child_by_field_name("property")),
            _ => None,
        };
        if let Some(named) = named {
            return text(named, source).to_string();
        }
    }
    format!("<anonymous:{}>", line_of(node))
}

pub fn find_functions(
    node: Node,
    source: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<FunctionDef>,
) {
    for child in children(node) {
        let kind = child.kind();
        if FUNCTION_KINDS.contains(&kind) {
            let name = function_name(child, source);
            let mut qualified = scope.clone();
            qualified.push(name.clone());
            out.push(FunctionDef {
                name: qualified.join("."),
                line: line_of(child),
                end_line: end_line_of(child),
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
            });
            scope.push(name);
            find_functions(child, source, scope, out);
            scope.pop();
        } else if matches!(
            kind,
            "class_declaration" | "class" | "abstract_class_declaration"
        ) {
            let name = child
                .child_by_field_name("name")
                .map(|n| text(n, source).to_string())
                .unwrap_or_else(|| "<class>".to_string());
            scope.push(name);
            find_functions(child, source, scope, out);
            scope.pop();
        } else {
            find_functions(child, source, scope, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::PythonStyle;
    use super::super::{Lang, find_functions, find_tests};

    const SOURCE: &str = r#"import { describe, it, test, expect } from "vitest";
import { add } from "./math";

describe("outer", () => {
  describe('inner', () => {
    it("works", () => {
      expect(add(1, 2)).toBe(3);
    });
    it.skip("later", () => {});
  });
  test.each([[1, 2]])("adds %i", (a, b) => {
    expect(add(a, b)).toBe(3);
  });
  it(`template`, async () => {});
  it
    .skipIf(false)("multi", () => {});
});

describe.skip("off", () => {
  it("inherits skip", () => {});
});

it.todo("someday");
"#;

    #[test]
    fn finds_tests_with_describe_titles_and_static_skips() {
        let tests: Vec<String> = find_tests(Lang::TypeScript, SOURCE, PythonStyle::Pytest)
            .into_iter()
            .map(|t| {
                let mut parts = t.suite.clone();
                parts.push(t.name.clone());
                format!(
                    "{} {}-{} at {}{}",
                    parts.join(" > "),
                    t.line,
                    t.end_line,
                    t.report_line,
                    if t.skip { " skip" } else { "" }
                )
            })
            .collect();
        assert_eq!(
            tests,
            vec![
                "outer > inner > works 6-8 at 6",
                "outer > inner > later 9-9 at 9 skip",
                "outer > adds %i 11-13 at 11",
                "outer > template 14-14 at 14",
                "outer > multi 15-16 at 16",
                "off > inherits skip 20-20 at 20 skip",
                "someday 23-23 at 23 skip",
            ]
        );
    }

    #[test]
    fn finds_named_functions_methods_and_arrows() {
        let source = r#"export function add(a: number, b: number) {
  return a + b;
}
export const mul = (a: number, b: number) => a * b;
class Box {
  open() {
    const inner = function () { return 1; };
    return inner();
  }
  handler = () => {};
}
const api = { get: () => 1 };
export default function () {}
"#;
        let names: Vec<String> = find_functions(Lang::Tsx, source)
            .into_iter()
            .map(|f| format!("{} {}-{}", f.name, f.line, f.end_line))
            .collect();
        assert_eq!(
            names,
            vec![
                "add 1-3",
                "mul 4-4",
                "Box.open 6-9",
                "Box.open.inner 7-7",
                "Box.handler 10-10",
                "get 12-12",
                "<anonymous:13> 13-13",
            ]
        );
    }
}
