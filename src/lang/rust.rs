//! Rust: tests, functions, and test-only items.
//!
//! Rust keeps unit tests in the source file they test, inside `#[cfg(test)]` items. titer
//! treats those items as test code: their lines are never mutated, and prove splices head's
//! test items into base's source file.

use tree_sitter::Node;

use super::{FunctionDef, Lang, TestDef, children, end_line_of, line_of, text};

/// The attributes before an item, nearest last, and where the first one starts. Comments
/// between attributes are skipped; a comment before the first attribute is not part of the
/// item.
fn attributes<'t>(item: Node<'t>) -> (Vec<Node<'t>>, Node<'t>) {
    let mut attrs = Vec::new();
    let mut first = item;
    let mut prev = item.prev_named_sibling();
    while let Some(node) = prev {
        match node.kind() {
            "attribute_item" => {
                attrs.push(node);
                first = node;
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        prev = node.prev_named_sibling();
    }
    attrs.reverse();
    (attrs, first)
}

/// An attribute's path, such as `test`, `tokio::test`, or `cfg`, and its whole text.
fn attribute_parts<'a>(attr_item: Node, source: &'a str) -> Option<(&'a str, &'a str)> {
    let attr = children(attr_item)
        .into_iter()
        .find(|n| n.kind() == "attribute")?;
    let path = children(attr).into_iter().next()?;
    Some((text(path, source), text(attr, source)))
}

/// `#[test]`, a framework's `#[tokio::test]`-style attribute, or a parametrizing macro whose
/// cases run as tests.
fn is_test_attribute(path: &str) -> bool {
    let last = path.rsplit("::").next().unwrap_or(path);
    last == "test" || matches!(last, "rstest" | "test_case" | "quickcheck")
}

fn is_cfg_test(attr: &str) -> bool {
    let compact: String = attr.chars().filter(|c| !c.is_whitespace()).collect();
    compact == "cfg(test)"
}

/// Whether an item is test-only: marked `#[cfg(test)]` or a test function.
fn is_test_item(item: Node, source: &str) -> bool {
    attributes(item).0.into_iter().any(|a| {
        attribute_parts(a, source)
            .is_some_and(|(path, whole)| is_cfg_test(whole) || is_test_attribute(path))
    })
}

fn name_of<'a>(node: Node, source: &'a str) -> &'a str {
    node.child_by_field_name("name")
        .map(|n| text(n, source))
        .unwrap_or_default()
}

pub fn find_tests(root: Node, source: &str) -> Vec<TestDef> {
    let mut out = Vec::new();
    walk_tests(root, source, &mut Vec::new(), &mut out);
    out
}

fn walk_tests(container: Node, source: &str, suite: &mut Vec<String>, out: &mut Vec<TestDef>) {
    for child in children(container) {
        match child.kind() {
            "function_item" => {
                let (attrs, first) = attributes(child);
                let paths: Vec<&str> = attrs
                    .iter()
                    .filter_map(|a| attribute_parts(*a, source).map(|(p, _)| p))
                    .collect();
                if !paths.iter().any(|p| is_test_attribute(p)) {
                    continue;
                }
                out.push(TestDef {
                    suite: suite.clone(),
                    name: name_of(child, source).to_string(),
                    line: line_of(first),
                    end_line: end_line_of(child),
                    report_line: line_of(child),
                    skip: paths.contains(&"ignore"),
                    start_byte: first.start_byte(),
                    end_byte: child.end_byte(),
                });
            }
            "mod_item" => {
                if let Some(body) = child.child_by_field_name("body") {
                    suite.push(name_of(child, source).to_string());
                    walk_tests(body, source, suite, out);
                    suite.pop();
                }
            }
            _ => {}
        }
    }
}

pub fn find_functions(
    node: Node,
    source: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<FunctionDef>,
) {
    for child in children(node) {
        match child.kind() {
            "function_item" => {
                let name = name_of(child, source).to_string();
                let first = attributes(child).1;
                let mut qualified = scope.clone();
                qualified.push(name.clone());
                out.push(FunctionDef {
                    name: qualified.join("."),
                    line: line_of(first),
                    end_line: end_line_of(child),
                    start_byte: first.start_byte(),
                    end_byte: child.end_byte(),
                });
                scope.push(name);
                find_functions(child, source, scope, out);
                scope.pop();
            }
            "impl_item" => {
                let name = child
                    .child_by_field_name("type")
                    .map(|t| impl_name(t, source))
                    .unwrap_or_else(|| "<impl>".to_string());
                scope.push(name);
                find_functions(child, source, scope, out);
                scope.pop();
            }
            "trait_item" | "mod_item" => {
                scope.push(name_of(child, source).to_string());
                find_functions(child, source, scope, out);
                scope.pop();
            }
            _ => find_functions(child, source, scope, out),
        }
    }
}

/// The type an `impl` block is for, without generic arguments: `Calc` for
/// `impl<T> Calc<T>`.
fn impl_name(ty: Node, source: &str) -> String {
    match ty.kind() {
        "generic_type" => ty
            .child_by_field_name("type")
            .map_or_else(|| text(ty, source).to_string(), |t| impl_name(t, source)),
        _ => text(ty, source).to_string(),
    }
}

/// An outermost test-only item: one marked `#[cfg(test)]` or with a test attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestItem {
    /// The names of the inline modules that contain the item, outermost first, or None
    /// when another item, such as an `impl`, contains it.
    pub container: Option<Vec<String>>,
    /// The span from the item's first attribute to its end.
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: u32,
    pub end_line: u32,
}

pub fn test_items(root: Node, source: &str) -> Vec<TestItem> {
    let mut out = Vec::new();
    walk_items(root, source, Some(&mut Vec::new()), &mut out);
    out
}

fn walk_items(
    node: Node,
    source: &str,
    mut container: Option<&mut Vec<String>>,
    out: &mut Vec<TestItem>,
) {
    for child in children(node) {
        let kind = child.kind();
        if matches!(kind, "attribute_item" | "line_comment" | "block_comment") {
            continue;
        }
        if is_test_item(child, source) {
            let first = attributes(child).1;
            out.push(TestItem {
                container: container.as_deref().cloned(),
                start_byte: first.start_byte(),
                end_byte: child.end_byte(),
                line: line_of(first),
                end_line: end_line_of(child),
            });
            continue;
        }
        if kind == "mod_item" {
            if let Some(body) = child.child_by_field_name("body") {
                // A module inside an impl or function is not a container prove can find.
                match container.as_deref_mut() {
                    Some(path) => {
                        path.push(name_of(child, source).to_string());
                        walk_items(body, source, Some(path), out);
                        path.pop();
                    }
                    None => walk_items(body, source, None, out),
                }
            }
        } else {
            walk_items(child, source, None, out);
        }
    }
}

/// Whether the file starts with an inner `#![cfg(test)]`, which makes all of it test-only.
fn is_test_only_file(root: Node, source: &str) -> bool {
    children(root)
        .into_iter()
        .take_while(|n| {
            matches!(
                n.kind(),
                "inner_attribute_item" | "line_comment" | "block_comment"
            )
        })
        .filter(|n| n.kind() == "inner_attribute_item")
        .any(|n| {
            children(n)
                .into_iter()
                .find(|a| a.kind() == "attribute")
                .is_some_and(|a| is_cfg_test(text(a, source)))
        })
}

/// The 1-based line ranges of test-only code in `source`. A file with an inner
/// `#![cfg(test)]` is test-only throughout.
pub fn test_lines(source: &str) -> Vec<(u32, u32)> {
    let Some(tree) = Lang::Rust.parse(source) else {
        return Vec::new();
    };
    let root = tree.root_node();
    if is_test_only_file(root, source) {
        return vec![(1, end_line_of(root).max(1))];
    }
    test_items(root, source)
        .into_iter()
        .map(|i| (i.line, i.end_line))
        .collect()
}

/// Base's source file with its test items replaced by head's, each placed at the end of the
/// inline module that holds it in head. Returns None when the test items are the same.
pub fn splice_tests(base: &str, head: &str) -> Option<String> {
    let base_tree = Lang::Rust.parse(base)?;
    let head_tree = Lang::Rust.parse(head)?;
    let base_items: Vec<TestItem> = test_items(base_tree.root_node(), base)
        .into_iter()
        .filter(|i| i.container.is_some())
        .collect();
    let head_items: Vec<TestItem> = test_items(head_tree.root_node(), head)
        .into_iter()
        .filter(|i| i.container.is_some())
        .collect();
    let texts = |items: &[TestItem], source: &str| -> Vec<String> {
        items
            .iter()
            .map(|i| source[i.start_byte..i.end_byte].to_string())
            .collect()
    };
    if texts(&base_items, base) == texts(&head_items, head) {
        return None;
    }
    // Byte offsets in base: where each item is removed, and where each is inserted. An
    // item moves with the indentation before it.
    let mut edits: Vec<(usize, usize, String)> = base_items
        .iter()
        .map(|i| (indent_start(base, i.start_byte), i.end_byte, String::new()))
        .collect();
    for item in &head_items {
        let path = item.container.as_deref().unwrap_or_default();
        // A test item whose module base lacks could not compile on base anyway.
        let Some(at) = container_end(base_tree.root_node(), base, path) else {
            continue;
        };
        let mut insert = String::new();
        if at > 0 && !base[..at].ends_with('\n') {
            insert.push('\n');
        }
        insert.push_str(&head[indent_start(head, item.start_byte)..item.end_byte]);
        insert.push('\n');
        edits.push((at, at, insert));
    }
    // Apply from the end, keeping head's order among insertions at one offset.
    edits.sort_by_key(|(start, _, _)| *start);
    let mut out = base.to_string();
    for (start, end, replacement) in edits.into_iter().rev() {
        out.replace_range(start..end, &replacement);
    }
    Some(out)
}

/// The start of the line that `at` is on, when only whitespace precedes `at` there.
fn indent_start(source: &str, at: usize) -> usize {
    let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
    if source[line_start..at]
        .chars()
        .all(|c| c == ' ' || c == '\t')
    {
        line_start
    } else {
        at
    }
}

/// The byte offset just inside the end of the inline module at `path`: the end of the file
/// for the root, the closing brace of a module body otherwise.
fn container_end(root: Node, source: &str, path: &[String]) -> Option<usize> {
    let mut node = root;
    for name in path {
        let module = children(node).into_iter().find(|c| {
            c.kind() == "mod_item"
                && name_of(*c, source) == name
                && !is_test_item(*c, source)
                && c.child_by_field_name("body").is_some()
        })?;
        node = module.child_by_field_name("body")?;
    }
    if node.kind() == "source_file" {
        Some(source.len())
    } else {
        Some(node.end_byte() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PythonStyle, find_functions, find_tests};
    use super::*;

    const SOURCE: &str = r#"//! A crate.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub struct Calc;

impl Calc {
    pub fn mul(&self, a: i32) -> i32 {
        let sq = |x: i32| x * x;
        sq(a)
    }
}

trait Shape {
    fn area(&self) -> f64 {
        0.0
    }
}

mod inner {
    pub fn helper() {}

    #[cfg(test)]
    mod tests {
        #[test]
        fn nested() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> i32 {
        1
    }

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }

    /// Slow.
    #[test]
    #[ignore = "slow"]
    fn later() {}

    #[tokio::test]
    async fn async_one() {}

    mod deeper {
        #[test]
        fn deep() {}
    }
}

#[test]
fn top_level() {}
"#;

    fn summary(tests: &[TestDef]) -> Vec<String> {
        tests
            .iter()
            .map(|t| {
                let mut parts = t.suite.clone();
                parts.push(t.name.clone());
                format!(
                    "{} {}-{} fn {}{}",
                    parts.join("::"),
                    t.line,
                    t.end_line,
                    t.report_line,
                    if t.skip { " skip" } else { "" }
                )
            })
            .collect()
    }

    #[test]
    fn finds_test_functions_in_modules_with_attributes_and_ignores() {
        let tests = find_tests(Lang::Rust, SOURCE, PythonStyle::Pytest);
        assert_eq!(
            summary(&tests),
            vec![
                "inner::tests::nested 26-27 fn 27",
                "tests::adds 39-42 fn 40",
                "tests::later 45-47 fn 47 skip",
                "tests::async_one 49-50 fn 50",
                "tests::deeper::deep 53-54 fn 54",
                "top_level 58-59 fn 59",
            ]
        );
    }

    #[test]
    fn finds_functions_qualified_by_module_impl_and_trait() {
        let names: Vec<String> = find_functions(Lang::Rust, SOURCE)
            .into_iter()
            .map(|f| format!("{} {}-{}", f.name, f.line, f.end_line))
            .collect();
        assert_eq!(
            names,
            vec![
                "add 2-4",
                "Calc.mul 9-12",
                "Shape.area 16-18",
                "inner.helper 22-22",
                "inner.tests.nested 26-27",
                "tests.fixture 35-37",
                "tests.adds 39-42",
                "tests.later 45-47",
                "tests.async_one 49-50",
                "tests.deeper.deep 53-54",
                "top_level 58-59",
            ]
        );
    }

    #[test]
    fn test_lines_cover_cfg_test_items_and_bare_tests() {
        assert_eq!(test_lines(SOURCE), vec![(24, 28), (31, 56), (58, 59)]);
        assert_eq!(test_lines("#![cfg(test)]\nfn helper() {}\n"), vec![(1, 2)]);
        assert_eq!(test_lines("fn f() {}\n"), vec![]);
    }

    #[test]
    fn splices_head_tests_into_base_source() {
        let base = r#"pub fn add(a: i32, b: i32) -> i32 {
    a - b
}

mod inner {
    pub fn helper() {}

    #[cfg(test)]
    mod tests {
        #[test]
        fn old_nested() {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn old() {}
}
"#;
        let head = r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn sub(a: i32, b: i32) -> i32 {
    a - b
}

#[cfg(test)]
mod tests {
    #[test]
    fn new() {}
}

mod inner {
    pub fn helper() {}

    #[cfg(test)]
    mod tests {
        #[test]
        fn new_nested() {}
    }
}

mod gone {
    #[cfg(test)]
    mod tests {}
}
"#;
        let spliced = splice_tests(base, head).unwrap();
        assert_eq!(
            spliced,
            r#"pub fn add(a: i32, b: i32) -> i32 {
    a - b
}

mod inner {
    pub fn helper() {}


    #[cfg(test)]
    mod tests {
        #[test]
        fn new_nested() {}
    }
}


#[cfg(test)]
mod tests {
    #[test]
    fn new() {}
}
"#
        );
        assert_eq!(splice_tests(head, head), None);
    }
}
