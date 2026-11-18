//! Mutation operators: tree-sitter edits on changed lines of a source file.

use std::collections::BTreeSet;

use tree_sitter::Node;

use crate::lang::{Lang, children, line_of, text};
use crate::report::Operator;

/// One mutant: replace `start_byte..end_byte` with `replacement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub line: u32,
    pub end_line: u32,
    pub column: u32,
    pub operator: Operator,
    pub start_byte: usize,
    pub end_byte: usize,
    pub original: String,
    pub replacement: String,
    /// The code the edit's meaning depends on, such as the whole comparison for an operator
    /// swap. A mutant whose context is unchanged from base may predate the change.
    pub context: CodeNode,
}

/// A syntax node, in a form that can be compared with nodes of the base file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeNode {
    pub kind: String,
    pub parent: Option<String>,
    /// The texts of the node's leaf tokens. Whitespace between tokens is formatting and is
    /// left out; whitespace inside a token, such as a string literal, is kept.
    pub leaves: Vec<String>,
}

impl CodeNode {
    pub fn of(node: Node, source: &str) -> CodeNode {
        CodeNode {
            kind: node.kind().to_string(),
            parent: node.parent().map(|p| p.kind().to_string()),
            leaves: leaves(node, source),
        }
    }
}

fn leaves(node: Node, source: &str) -> Vec<String> {
    if node.child_count() == 0 {
        let t = text(node, source);
        return if t.is_empty() {
            Vec::new()
        } else {
            vec![t.to_string()]
        };
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .flat_map(|c| leaves(c, source))
        .collect()
}

impl Candidate {
    pub fn apply(&self, source: &str) -> String {
        let mut out = String::with_capacity(source.len() + self.replacement.len());
        out.push_str(&source[..self.start_byte]);
        out.push_str(&self.replacement);
        out.push_str(&source[self.end_byte..]);
        out
    }
}

/// Which calls the remove-call operator leaves alone.
#[derive(Debug, Clone, Default)]
pub struct CallFilter {
    /// Callee names from `[mutate] exclude_calls`, matched against the whole callee or its
    /// last segment.
    pub excluded: Vec<String>,
}

const LOGGER_ROOTS: &[&str] = &[
    "log", "logger", "logging", "console", "_log", "_logger", "LOG", "LOGGER", "print", "warnings",
];
const LOG_METHODS: &[&str] = &[
    "debug",
    "info",
    "warning",
    "warn",
    "error",
    "exception",
    "critical",
    "trace",
    "log",
    "fatal",
];

impl CallFilter {
    /// Logging calls and configured callees are never removed.
    pub fn skips(&self, callee: &str) -> bool {
        let callee = callee.trim();
        let root = callee
            .split(['.', '(', '['])
            .next()
            .unwrap_or(callee)
            .trim_start_matches("self.")
            .trim_start_matches("this.");
        let last = callee.rsplit('.').next().unwrap_or(callee);
        if LOGGER_ROOTS.contains(&root) {
            return true;
        }
        let receiver_is_logger = callee
            .rsplit_once('.')
            .is_some_and(|(recv, _)| recv.to_lowercase().contains("log"));
        if receiver_is_logger && LOG_METHODS.contains(&last) {
            return true;
        }
        self.excluded.iter().any(|e| e == callee || e == last)
    }
}

/// Generates mutants for the changed lines of `source`, in source order.
pub fn generate(
    lang: Lang,
    source: &str,
    changed: &BTreeSet<u32>,
    calls: &CallFilter,
    excluded: &[Operator],
) -> Vec<Candidate> {
    let Some(tree) = lang.parse(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut ctx = Gen {
        lang,
        source,
        changed,
        calls,
        out: &mut out,
    };
    ctx.walk(tree.root_node());
    out.retain(|c| !excluded.contains(&c.operator));
    out.sort_by_key(|c| (c.start_byte, c.end_byte));
    out.dedup_by(|a, b| {
        a.start_byte == b.start_byte && a.end_byte == b.end_byte && a.replacement == b.replacement
    });
    // Keep only mutants that still parse.
    let had_errors = tree.root_node().has_error();
    out.retain(|c| {
        had_errors
            || lang
                .parse(&c.apply(source))
                .is_some_and(|t| !t.root_node().has_error())
    });
    out
}

struct Gen<'a> {
    lang: Lang,
    source: &'a str,
    changed: &'a BTreeSet<u32>,
    calls: &'a CallFilter,
    out: &'a mut Vec<Candidate>,
}

impl Gen<'_> {
    fn push(&mut self, node: Node, operator: Operator, replacement: impl Into<String>) {
        let replacement = replacement.into();
        let original = text(node, self.source).to_string();
        if original == replacement {
            return;
        }
        self.out.push(Candidate {
            line: line_of(node),
            end_line: crate::lang::end_line_of(node),
            column: node.start_position().column as u32 + 1,
            operator,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            original,
            replacement,
            context: CodeNode::of(context(node, operator), self.source),
        });
    }

    fn on_changed_line(&self, node: Node) -> bool {
        self.changed.contains(&line_of(node))
    }

    fn walk(&mut self, node: Node) {
        match self.lang {
            Lang::Python => self.python(node),
            Lang::TypeScript | Lang::Tsx => self.typescript(node),
            Lang::Rust => {
                // Attributes are configuration, not behavior.
                if node.kind() == "attribute_item" || node.kind() == "inner_attribute_item" {
                    return;
                }
                self.rust(node);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child);
        }
    }

    fn python(&mut self, node: Node) {
        match node.kind() {
            "comparison_operator" => {
                let mut cursor = node.walk();
                let ops: Vec<Node> = node
                    .children_by_field_name("operators", &mut cursor)
                    .collect();
                for op in ops {
                    if !self.on_changed_line(op) {
                        continue;
                    }
                    let swapped = match text(op, self.source) {
                        "<" => "<=",
                        "<=" => "<",
                        ">" => ">=",
                        ">=" => ">",
                        "==" => "!=",
                        "!=" => "==",
                        "in" => "not in",
                        "not in" => "in",
                        "is" => "is not",
                        "is not" => "is",
                        _ => continue,
                    };
                    self.push(op, Operator::SwapComparison, swapped);
                }
            }
            "boolean_operator" => {
                if let Some(op) = node.child_by_field_name("operator")
                    && self.on_changed_line(op)
                {
                    let swapped = if text(op, self.source) == "and" {
                        "or"
                    } else {
                        "and"
                    };
                    self.push(op, Operator::SwapBoolean, swapped);
                }
            }
            "binary_operator" => {
                if let Some(op) = node.child_by_field_name("operator")
                    && self.on_changed_line(op)
                    && let Some(swapped) = swap_arithmetic(text(op, self.source))
                {
                    self.push(op, Operator::SwapArithmetic, swapped);
                }
            }
            "true" | "false" if self.on_changed_line(node) => {
                let flipped = if text(node, self.source) == "True" {
                    "False"
                } else {
                    "True"
                };
                self.push(node, Operator::FlipBool, flipped);
            }
            "integer" if self.on_changed_line(node) => self.increment(node),
            "not_operator" if self.on_changed_line(node) => {
                if let Some(arg) = node.child_by_field_name("argument") {
                    self.push(
                        node,
                        Operator::RemoveNot,
                        text(arg, self.source).to_string(),
                    );
                }
            }
            "return_statement" if self.on_changed_line(node) => {
                if let Some(value) = children(node).into_iter().next()
                    && text(value, self.source) != "None"
                {
                    self.push(node, Operator::ReturnNone, "return None");
                }
            }
            "expression_statement" if self.on_changed_line(node) => {
                let call = children(node)
                    .into_iter()
                    .next()
                    .and_then(|e| match e.kind() {
                        "call" => Some(e),
                        "await" => children(e)
                            .into_iter()
                            .next()
                            .filter(|c| c.kind() == "call"),
                        _ => None,
                    });
                if let Some(call) = call
                    && let Some(func) = call.child_by_field_name("function")
                    && !self.calls.skips(text(func, self.source))
                {
                    self.push(node, Operator::RemoveCall, "pass");
                }
            }
            "raise_statement" if self.on_changed_line(node) => {
                self.push(node, Operator::RemoveRaise, "pass");
            }
            "if_statement" | "elif_clause" => {
                if let Some(cond) = node.child_by_field_name("condition")
                    && self.on_changed_line(cond)
                {
                    let negated = format!("not ({})", text(cond, self.source));
                    self.push(cond, Operator::NegateIf, negated);
                }
            }
            _ => {}
        }
    }

    fn typescript(&mut self, node: Node) {
        match node.kind() {
            "binary_expression" => {
                let Some(op) = node.child_by_field_name("operator") else {
                    return;
                };
                if !self.on_changed_line(op) {
                    return;
                }
                let token = text(op, self.source);
                let (operator, swapped) = match token {
                    "<" => (Operator::SwapComparison, "<="),
                    "<=" => (Operator::SwapComparison, "<"),
                    ">" => (Operator::SwapComparison, ">="),
                    ">=" => (Operator::SwapComparison, ">"),
                    "===" => (Operator::SwapComparison, "!=="),
                    "!==" => (Operator::SwapComparison, "==="),
                    "==" => (Operator::SwapComparison, "!="),
                    "!=" => (Operator::SwapComparison, "=="),
                    "&&" => (Operator::SwapBoolean, "||"),
                    "||" => (Operator::SwapBoolean, "&&"),
                    other => match swap_arithmetic(other) {
                        Some(s) => (Operator::SwapArithmetic, s),
                        None => return,
                    },
                };
                self.push(op, operator, swapped);
            }
            "true" | "false" if self.on_changed_line(node) => {
                let flipped = if text(node, self.source) == "true" {
                    "false"
                } else {
                    "true"
                };
                self.push(node, Operator::FlipBool, flipped);
            }
            "number" if self.on_changed_line(node) => self.increment(node),
            "unary_expression" if self.on_changed_line(node) => {
                if let Some(op) = node.child_by_field_name("operator")
                    && text(op, self.source) == "!"
                    && let Some(arg) = node.child_by_field_name("argument")
                {
                    self.push(
                        node,
                        Operator::RemoveNot,
                        text(arg, self.source).to_string(),
                    );
                }
            }
            "return_statement" if self.on_changed_line(node) => {
                if let Some(value) = children(node).into_iter().next()
                    && !matches!(text(value, self.source), "undefined" | "null")
                {
                    self.push(node, Operator::ReturnNone, "return undefined;");
                }
            }
            "expression_statement" if self.on_changed_line(node) => {
                let call = children(node)
                    .into_iter()
                    .next()
                    .and_then(|e| match e.kind() {
                        "call_expression" => Some(e),
                        "await_expression" => children(e)
                            .into_iter()
                            .next()
                            .filter(|c| c.kind() == "call_expression"),
                        _ => None,
                    });
                if let Some(call) = call
                    && let Some(func) = call.child_by_field_name("function")
                    && !self.calls.skips(text(func, self.source))
                {
                    self.push(node, Operator::RemoveCall, ";");
                }
            }
            "throw_statement" if self.on_changed_line(node) => {
                self.push(node, Operator::RemoveRaise, ";");
            }
            "if_statement" => {
                if let Some(cond) = node.child_by_field_name("condition")
                    && self.on_changed_line(cond)
                {
                    let inner = text(cond, self.source);
                    let inner = inner
                        .strip_prefix('(')
                        .and_then(|i| i.strip_suffix(')'))
                        .unwrap_or(inner);
                    self.push(cond, Operator::NegateIf, format!("(!({inner}))"));
                }
            }
            "jsx_attribute" if self.on_changed_line(node) => {
                let parts = children(node);
                if let Some(name) = parts.first() {
                    let name = text(*name, self.source);
                    if name != "key" {
                        self.push(
                            node,
                            Operator::JsxAttributeUndefined,
                            format!("{name}={{undefined}}"),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn rust(&mut self, node: Node) {
        match node.kind() {
            "binary_expression" => {
                let Some(op) = node.child_by_field_name("operator") else {
                    return;
                };
                if !self.on_changed_line(op) {
                    return;
                }
                let (operator, swapped) = match text(op, self.source) {
                    "<" => (Operator::SwapComparison, "<="),
                    "<=" => (Operator::SwapComparison, "<"),
                    ">" => (Operator::SwapComparison, ">="),
                    ">=" => (Operator::SwapComparison, ">"),
                    "==" => (Operator::SwapComparison, "!="),
                    "!=" => (Operator::SwapComparison, "=="),
                    "&&" => (Operator::SwapBoolean, "||"),
                    "||" => (Operator::SwapBoolean, "&&"),
                    other => match swap_arithmetic(other) {
                        Some(s) => (Operator::SwapArithmetic, s),
                        None => return,
                    },
                };
                self.push(op, operator, swapped);
            }
            "boolean_literal" if self.on_changed_line(node) => {
                let flipped = if text(node, self.source) == "true" {
                    "false"
                } else {
                    "true"
                };
                self.push(node, Operator::FlipBool, flipped);
            }
            "integer_literal" if self.on_changed_line(node) => {
                // A tuple index or an array length changes a type, which rarely compiles.
                let sized = node.parent().is_some_and(|p| match p.kind() {
                    "field_expression" | "array_type" => true,
                    "array_expression" => p
                        .child_by_field_name("length")
                        .is_some_and(|l| l.id() == node.id()),
                    _ => false,
                });
                if !sized {
                    self.increment(node);
                }
            }
            "unary_expression" if self.on_changed_line(node) => {
                let parts: Vec<Node> = {
                    let mut cursor = node.walk();
                    node.children(&mut cursor).collect()
                };
                if let [op, arg] = parts.as_slice()
                    && text(*op, self.source) == "!"
                {
                    self.push(
                        node,
                        Operator::RemoveNot,
                        text(*arg, self.source).to_string(),
                    );
                }
            }
            "expression_statement" if self.on_changed_line(node) => {
                // Only a statement that ends in `;` can go without changing a block's value.
                if !text(node, self.source).ends_with(';') {
                    return;
                }
                let Some(expr) = children(node).into_iter().next() else {
                    return;
                };
                let call = match expr.kind() {
                    "call_expression" => Some(expr),
                    "try_expression" | "await_expression" => children(expr)
                        .into_iter()
                        .next()
                        .filter(|c| c.kind() == "call_expression"),
                    _ => None,
                };
                if let Some(call) = call
                    && let Some(func) = call.child_by_field_name("function")
                    && !self.calls.skips(text(func, self.source))
                {
                    self.push(node, Operator::RemoveCall, ";");
                    return;
                }
                if is_rust_raise(expr, self.source) {
                    self.push(node, Operator::RemoveRaise, ";");
                }
            }
            "if_expression" => {
                if let Some(cond) = node.child_by_field_name("condition")
                    && self.on_changed_line(cond)
                    && !has_let(cond)
                {
                    let negated = format!("!({})", text(cond, self.source));
                    self.push(cond, Operator::NegateIf, negated);
                }
            }
            _ => {}
        }
    }

    fn increment(&mut self, node: Node) {
        let raw = text(node, self.source);
        let digits = raw.replace('_', "");
        if let Some(n) = digits.parse::<u64>().ok().and_then(|n| n.checked_add(1)) {
            self.push(node, Operator::IncrementInt, n.to_string());
        }
    }
}

/// Rust's raise: `panic!`-like macros, and `return Err(...)`.
fn is_rust_raise(expr: Node, source: &str) -> bool {
    match expr.kind() {
        "macro_invocation" => expr.child_by_field_name("macro").is_some_and(|m| {
            matches!(
                text(m, source),
                "panic" | "unreachable" | "todo" | "unimplemented"
            )
        }),
        "return_expression" => children(expr).into_iter().next().is_some_and(|value| {
            value.kind() == "call_expression"
                && value
                    .child_by_field_name("function")
                    .is_some_and(|f| text(f, source) == "Err")
        }),
        _ => false,
    }
}

/// A condition with a `let` pattern, which cannot be negated.
fn has_let(cond: Node) -> bool {
    matches!(cond.kind(), "let_condition" | "let_chain") || children(cond).into_iter().any(has_let)
}

/// Parents that only wrap a literal, such as the `-` of `-1` or a call's argument list.
const WRAPPERS: &[&str] = &[
    "argument_list",
    "arguments",
    "parenthesized_expression",
    "unary_operator",
    "unary_expression",
];

/// The node a mutant's meaning depends on. An operator token means nothing without its
/// operands, and a literal nothing without what uses it, so each widens to that; other
/// mutants replace a whole construct already. A context matches base only with the same
/// parent kind, which ties a negated condition to its `if`.
fn context(node: Node, operator: Operator) -> Node {
    let parent = node.parent().unwrap_or(node);
    match operator {
        Operator::SwapComparison | Operator::SwapBoolean | Operator::SwapArithmetic => parent,
        Operator::FlipBool | Operator::IncrementInt => {
            let mut user = parent;
            while WRAPPERS.contains(&user.kind())
                && let Some(next) = user.parent()
            {
                user = next;
            }
            user
        }
        _ => node,
    }
}

fn swap_arithmetic(op: &str) -> Option<&'static str> {
    Some(match op {
        "+" => "-",
        "-" => "+",
        "*" => "/",
        "/" => "*",
        "//" => "/",
        "%" => "*",
        "**" => "*",
        _ => return None,
    })
}

/// Orders mutants so the first pass covers one mutant per changed line, then the rest.
pub fn budget_order<T>(items: Vec<T>, key: impl Fn(&T) -> (String, u32)) -> Vec<T> {
    let mut by_line: std::collections::BTreeMap<(String, u32), Vec<T>> = Default::default();
    for item in items {
        by_line.entry(key(&item)).or_default().push(item);
    }
    let mut queues: Vec<std::collections::VecDeque<T>> =
        by_line.into_values().map(Into::into).collect();
    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for queue in queues.iter_mut() {
            if let Some(item) = queue.pop_front() {
                out.push(item);
                progressed = true;
            }
        }
        if !progressed {
            return out;
        }
    }
}

/// The canary: a raise or throw at the top of a module. Rust runs no code when a module
/// loads, so its canary is a compile error at the end of the file instead: it fails the
/// build only when the build compiles that file. The end of the file keeps it after any
/// inner attributes.
pub fn canary(lang: Lang, source: &str) -> String {
    match lang {
        Lang::Python => format!("raise RuntimeError(\"titer canary\")\n{source}"),
        Lang::TypeScript | Lang::Tsx => format!("throw new Error(\"titer canary\");\n{source}"),
        Lang::Rust => format!("{source}\ncompile_error!(\"titer canary\");\n"),
    }
}

/// Inserts a raise or throw before the statement that encloses `line`, so a run that stays
/// green shows that no selected test executes that statement. Returns None when no
/// statement encloses the line or the result does not parse.
pub fn sentinel(lang: Lang, source: &str, line: u32) -> Option<String> {
    let tree = lang.parse(source)?;
    let row = line.checked_sub(1)? as usize;
    let text_line = source.lines().nth(row)?;
    let column = text_line.len() - text_line.trim_start().len();
    let point = tree_sitter::Point { row, column };
    let mut node = tree.root_node().descendant_for_point_range(point, point)?;
    let containers: &[&str] = match lang {
        Lang::Python => &["block", "module"],
        Lang::TypeScript | Lang::Tsx => &[
            "statement_block",
            "program",
            "switch_case",
            "switch_default",
        ],
        Lang::Rust => &["block"],
    };
    loop {
        let parent = node.parent()?;
        if containers.contains(&parent.kind()) {
            break;
        }
        node = parent;
    }
    let start = node.start_byte();
    let indent = &source[source[..start].rfind('\n').map_or(0, |i| i + 1)..start];
    if !indent.chars().all(char::is_whitespace) {
        // Another statement shares the line; a sentinel there would not isolate this one.
        return None;
    }
    let raise = match lang {
        // SystemExit is not an Exception, so `except Exception` in the code under test
        // cannot swallow the sentinel; pytest and unittest both report it as a failure.
        Lang::Python => "raise SystemExit(\"titer sentinel\")",
        Lang::TypeScript | Lang::Tsx => "throw new Error(\"titer sentinel\");",
        // A panic can be caught or expected by `#[should_panic]`, and an abort cannot. The
        // `if true` keeps the code after it reachable, so no unreachable-code lint fires.
        Lang::Rust => "if true { std::process::abort(); }",
    };
    let out = format!("{}{raise}\n{indent}{}", &source[..start], &source[start..]);
    let had_errors = tree.root_node().has_error();
    let parses = lang.parse(&out).is_some_and(|t| !t.root_node().has_error());
    (had_errors || parses).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(range: std::ops::RangeInclusive<u32>) -> BTreeSet<u32> {
        range.collect()
    }

    fn summary(cands: &[Candidate]) -> Vec<String> {
        cands
            .iter()
            .map(|c| {
                format!(
                    "{} {:?} {} -> {}",
                    c.line,
                    c.operator,
                    c.original.lines().next().unwrap(),
                    c.replacement
                )
            })
            .collect()
    }

    #[test]
    fn python_operators_cover_the_spike_set() {
        let source = r#"import logging

log = logging.getLogger(__name__)


def f(xs, flag):
    if not flag and len(xs) <= 3:
        raise ValueError("bad")
    log.info("checked")
    save(xs)
    total = sum(xs) + 1
    return total is None or True


def g():
    return 2
"#;
        let cands = generate(
            Lang::Python,
            source,
            &lines(7..=12),
            &CallFilter::default(),
            &[],
        );
        assert_eq!(
            summary(&cands),
            vec![
                "7 RemoveNot not flag -> flag",
                "7 NegateIf not flag and len(xs) <= 3 -> not (not flag and len(xs) <= 3)",
                "7 SwapBoolean and -> or",
                "7 SwapComparison <= -> <",
                "7 IncrementInt 3 -> 4",
                "8 RemoveRaise raise ValueError(\"bad\") -> pass",
                "10 RemoveCall save(xs) -> pass",
                "11 SwapArithmetic + -> -",
                "11 IncrementInt 1 -> 2",
                "12 ReturnNone return total is None or True -> return None",
                "12 SwapComparison is -> is not",
                "12 SwapBoolean or -> and",
                "12 FlipBool True -> False",
            ]
        );
    }

    #[test]
    fn typescript_operators_include_jsx_attributes() {
        let source = r#"export function Button({ busy, label }: Props) {
  if (busy === true && label.length > 0) {
    throw new Error("busy");
  }
  console.log("render");
  track(label);
  return <button disabled={!busy} aria-label="go" key="k">{label}</button>;
}
"#;
        let cands = generate(
            Lang::Tsx,
            source,
            &lines(2..=7),
            &CallFilter::default(),
            &[Operator::ReturnNone],
        );
        assert_eq!(
            summary(&cands),
            vec![
                "2 NegateIf (busy === true && label.length > 0) -> (!(busy === true && label.length > 0))",
                "2 SwapComparison === -> !==",
                "2 FlipBool true -> false",
                "2 SwapBoolean && -> ||",
                "2 SwapComparison > -> >=",
                "2 IncrementInt 0 -> 1",
                "3 RemoveRaise throw new Error(\"busy\"); -> ;",
                "6 RemoveCall track(label); -> ;",
                "7 JsxAttributeUndefined disabled={!busy} -> disabled={undefined}",
                "7 RemoveNot !busy -> busy",
                "7 JsxAttributeUndefined aria-label=\"go\" -> aria-label={undefined}",
            ]
        );
    }

    #[test]
    fn rust_operators_leave_macros_attributes_and_let_conditions_alone() {
        let source = r#"pub fn f(xs: &[i32], ok: bool) -> Result<i32, E> {
    if !ok && xs.len() <= 3 {
        return Err(E::Short);
    }
    if let Some(x) = xs.first() {
        log::info!("first {}", x);
    }
    save(xs);
    self.store.put(xs)?;
    panic!("unreachable");
    let total = xs.iter().sum::<i32>() + 1 - xs.0;
    let flag = true;
    #[cfg(feature = "x")]
    let y = [0u8; 4];
    Ok(total * 2)
}
"#;
        let cands = generate(
            Lang::Rust,
            source,
            &lines(2..=15),
            &CallFilter::default(),
            &[],
        );
        assert_eq!(
            summary(&cands),
            vec![
                "2 RemoveNot !ok -> ok",
                "2 NegateIf !ok && xs.len() <= 3 -> !(!ok && xs.len() <= 3)",
                "2 SwapBoolean && -> ||",
                "2 SwapComparison <= -> <",
                "2 IncrementInt 3 -> 4",
                "3 RemoveRaise return Err(E::Short); -> ;",
                "8 RemoveCall save(xs); -> ;",
                "9 RemoveCall self.store.put(xs)?; -> ;",
                "10 RemoveRaise panic!(\"unreachable\"); -> ;",
                "11 SwapArithmetic + -> -",
                "11 IncrementInt 1 -> 2",
                "11 SwapArithmetic - -> +",
                "12 FlipBool true -> false",
                "15 SwapArithmetic * -> /",
                "15 IncrementInt 2 -> 3",
            ]
        );
    }

    #[test]
    fn rust_canary_and_sentinel() {
        assert_eq!(
            canary(Lang::Rust, "#![allow(dead_code)]\nfn f() {}\n"),
            "#![allow(dead_code)]\nfn f() {}\n\ncompile_error!(\"titer canary\");\n"
        );
        let source = "fn f(x: i32) -> i32 {\n    let y = match x {\n        0 => 1,\n        _ => x + 1,\n    };\n    y\n}\n";
        assert_eq!(
            sentinel(Lang::Rust, source, 4).unwrap(),
            "fn f(x: i32) -> i32 {\n    if true { std::process::abort(); }\n    let y = match x {\n        0 => 1,\n        _ => x + 1,\n    };\n    y\n}\n"
        );
        assert!(
            sentinel(Lang::Rust, source, 6)
                .unwrap()
                .contains("    if true { std::process::abort(); }\n    y\n")
        );
    }

    #[test]
    fn mutants_carry_the_code_their_meaning_depends_on() {
        let source =
            "def f(xs, flag):\n    if not flag and len(xs) <= 3:\n        save(-1, True)\n";
        let cands = generate(
            Lang::Python,
            source,
            &lines(2..=3),
            &CallFilter::default(),
            &[],
        );
        let contexts: Vec<String> = cands
            .iter()
            .map(|c| {
                format!(
                    "{:?} {} in {}: {}",
                    c.operator,
                    c.context.kind,
                    c.context.parent.as_deref().unwrap_or("-"),
                    c.context.leaves.join(" ")
                )
            })
            .collect();
        assert_eq!(
            contexts,
            vec![
                "RemoveNot not_operator in boolean_operator: not flag",
                "NegateIf boolean_operator in if_statement: not flag and len ( xs ) <= 3",
                "SwapBoolean boolean_operator in if_statement: not flag and len ( xs ) <= 3",
                "SwapComparison comparison_operator in boolean_operator: len ( xs ) <= 3",
                "IncrementInt comparison_operator in boolean_operator: len ( xs ) <= 3",
                "RemoveCall expression_statement in block: save ( - 1 , True )",
                "IncrementInt call in expression_statement: save ( - 1 , True )",
                "FlipBool call in expression_statement: save ( - 1 , True )",
            ]
        );

        let tsx = "const a = <p lang={lang} className={LARGE}>{text}</p>;\n";
        let cands = generate(Lang::Tsx, tsx, &lines(1..=1), &CallFilter::default(), &[]);
        assert_eq!(cands[1].context.leaves.join(""), "className={LARGE}");
    }

    #[test]
    fn unchanged_lines_get_no_mutants() {
        let source = "def f(x):\n    return x + 1\n";
        assert!(
            generate(
                Lang::Python,
                source,
                &lines(1..=1),
                &CallFilter::default(),
                &[]
            )
            .is_empty()
        );
    }

    #[test]
    fn sentinels_go_before_the_enclosing_statement() {
        let py = "def f(x):\n    if x > 1:\n        return 2\n    elif x < 0:\n        return 3\n    return x\n";
        assert_eq!(
            sentinel(Lang::Python, py, 3).unwrap(),
            "def f(x):\n    if x > 1:\n        raise SystemExit(\"titer sentinel\")\n        return 2\n    elif x < 0:\n        return 3\n    return x\n"
        );
        // An elif condition belongs to the whole if statement.
        assert!(
            sentinel(Lang::Python, py, 4)
                .unwrap()
                .contains("    raise SystemExit(\"titer sentinel\")\n    if x > 1:")
        );
        let ts = "export function f(x: number) {\n  const y = x > 1\n    ? 2\n    : 3;\n  return <a href={y} />;\n}\n";
        assert!(
            sentinel(Lang::Tsx, ts, 3)
                .unwrap()
                .contains("  throw new Error(\"titer sentinel\");\n  const y")
        );
        assert!(
            sentinel(Lang::Tsx, ts, 5)
                .unwrap()
                .contains("  throw new Error(\"titer sentinel\");\n  return")
        );
    }

    #[test]
    fn budget_order_takes_one_per_line_first() {
        let items = vec![("a", 1, 'x'), ("a", 1, 'y'), ("a", 2, 'z'), ("b", 1, 'w')];
        let ordered = budget_order(items, |i| (i.0.to_string(), i.1));
        let tags: String = ordered.iter().map(|i| i.2).collect();
        assert_eq!(tags, "xzwy");
    }

    #[test]
    fn logging_and_configured_calls_are_never_removed() {
        let filter = CallFilter {
            excluded: vec!["metrics.incr".into(), "flush".into()],
        };
        for callee in [
            "console.log",
            "logger.info",
            "self.log.warning",
            "logging.debug",
            "print",
            "metrics.incr",
            "cache.flush",
        ] {
            assert!(filter.skips(callee), "{callee}");
        }
        for callee in ["save", "self.store.put", "catalog.error_page"] {
            assert!(!filter.skips(callee), "{callee}");
        }
    }
}
