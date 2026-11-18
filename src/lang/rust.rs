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
pub(crate) fn attributes<'t>(item: Node<'t>) -> (Vec<Node<'t>>, Node<'t>) {
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
pub(crate) fn attribute_parts<'a>(attr_item: Node, source: &'a str) -> Option<(&'a str, &'a str)> {
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

/// Whether an attribute marks a test: a test attribute, or `cfg_attr(condition, test)`,
/// which makes the function a test when the build enables the condition.
fn marks_test(path: &str, whole: &str) -> bool {
    if is_test_attribute(path) {
        return true;
    }
    if path != "cfg_attr" {
        return false;
    }
    let Some(args) = whole
        .strip_prefix("cfg_attr")
        .map(str::trim)
        .and_then(|a| a.strip_prefix('('))
        .and_then(|a| a.strip_suffix(')'))
    else {
        return false;
    };
    // The attributes after the condition, split at top-level commas.
    let mut depth = 0;
    let mut parts = vec![String::new()];
    for c in args.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(String::new());
                continue;
            }
            _ => {}
        }
        parts.last_mut().expect("parts is never empty").push(c);
    }
    parts
        .iter()
        .skip(1)
        .any(|p| is_test_attribute(p.trim().split('(').next().unwrap_or_default().trim()))
}

fn is_cfg_test(attr: &str) -> bool {
    let compact: String = attr.chars().filter(|c| !c.is_whitespace()).collect();
    compact == "cfg(test)"
}

/// Whether an item is test-only: marked `#[cfg(test)]` or a test function.
fn is_test_item(item: Node, source: &str) -> bool {
    attributes(item).0.into_iter().any(|a| {
        attribute_parts(a, source)
            .is_some_and(|(path, whole)| is_cfg_test(whole) || marks_test(path, whole))
    })
}

pub(crate) fn name_of<'a>(node: Node, source: &'a str) -> &'a str {
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
                let parts: Vec<(&str, &str)> = attrs
                    .iter()
                    .filter_map(|a| attribute_parts(*a, source))
                    .collect();
                if !parts.iter().any(|(p, w)| marks_test(p, w)) {
                    continue;
                }
                let paths: Vec<&str> = parts.iter().map(|(p, _)| *p).collect();
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
                // The lines include the attributes; the bytes are the function node itself,
                // which no node spans together with its attributes.
                out.push(FunctionDef {
                    name: qualified.join("."),
                    line: line_of(first),
                    end_line: end_line_of(child),
                    start_byte: child.start_byte(),
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

/// The byte ranges of test-only code in `source`. A file with an inner `#![cfg(test)]` is
/// test-only throughout.
pub fn test_ranges(source: &str) -> Vec<(usize, usize)> {
    let Some(tree) = Lang::Rust.parse(source) else {
        return Vec::new();
    };
    let root = tree.root_node();
    if is_test_only_file(root, source) {
        return vec![(0, source.len())];
    }
    test_items(root, source)
        .into_iter()
        .map(|i| (i.start_byte, i.end_byte))
        .collect()
}

/// Whether the file starts with an inner `#![cfg(test)]`.
pub fn is_test_only(source: &str) -> bool {
    Lang::Rust
        .parse(source)
        .is_some_and(|t| is_test_only_file(t.root_node(), source))
}

fn line_at(source: &str, byte: usize) -> u32 {
    source[..byte].matches('\n').count() as u32 + 1
}

/// The 1-based lines that test-only code touches, including lines it shares with other
/// code.
pub fn test_spans(source: &str) -> Vec<(u32, u32)> {
    test_ranges(source)
        .into_iter()
        .map(|(start, end)| {
            (
                line_at(source, start),
                line_at(source, end.max(start + 1) - 1),
            )
        })
        .collect()
}

/// The 1-based lines that test-only code covers entirely. A line that test code shares
/// with other code is left out, so that code stays a mutation target.
pub fn test_lines(source: &str) -> Vec<(u32, u32)> {
    let blank = |s: &str| s.chars().all(char::is_whitespace);
    test_ranges(source)
        .into_iter()
        .filter_map(|(start, end)| {
            let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
            let line_end = source[end..].find('\n').map_or(source.len(), |i| end + i);
            let mut first = line_at(source, start);
            let mut last = line_at(source, end.max(start + 1) - 1);
            if !blank(&source[line_start..start]) {
                first += 1;
            }
            if !blank(&source[end..line_end]) {
                last = last.saturating_sub(1);
            }
            (first <= last).then_some((first, last))
        })
        .collect()
}

/// Base's source file with its test items replaced by head's. Returns None when the test
/// items are the same.
///
/// A head test item replaces base's item of the same kind and name where base's stands. A
/// new one goes right after the nearest item before it in head that base also has, so it
/// sees the same `macro_rules!` definitions and items it sees in head. Base's test items
/// that head lacks go away, and so do head's in a module base lacks: they could not compile
/// on base anyway.
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
    let mut paths: Vec<Vec<String>> = base_items
        .iter()
        .chain(&head_items)
        .filter_map(|i| i.container.clone())
        .collect();
    paths.sort();
    paths.dedup();
    // (start, end, replacement) in base's bytes.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for path in paths {
        let Some(base_node) = container(base_tree.root_node(), base, &path) else {
            continue;
        };
        let base_entries = entries(base_node, base);
        let head_entries = container(head_tree.root_node(), head, &path)
            .map(|n| entries(n, head))
            .unwrap_or_default();
        let base_index = |key: &Key| {
            base_entries
                .iter()
                .position(|e| e.key.as_ref() == Some(key))
        };
        for e in base_entries.iter().filter(|e| e.test) {
            let kept = e.key.as_ref().is_some_and(|k| {
                head_entries
                    .iter()
                    .any(|h| h.test && h.key.as_ref() == Some(k))
            });
            if !kept {
                edits.push((e.start, e.end, String::new()));
            }
        }
        for (i, h) in head_entries.iter().enumerate().filter(|(_, h)| h.test) {
            let text = &head[h.start..h.end];
            if let Some(b) = h.key.as_ref().and_then(base_index)
                && base_entries[b].test
            {
                edits.push((base_entries[b].start, base_entries[b].end, text.to_string()));
                continue;
            }
            let anchor = head_entries[..i]
                .iter()
                .rev()
                .find_map(|p| p.key.as_ref().and_then(base_index));
            let indent = &head[indent_start(head, h.start)..h.start];
            match anchor {
                Some(b) => {
                    let at = base_entries[b].end;
                    edits.push((at, at, format!("\n{indent}{text}")));
                }
                None => {
                    let (at, after) = match base_entries.first() {
                        Some(first) => (
                            first.start,
                            &base[indent_start(base, first.start)..first.start],
                        ),
                        None => (container_end(base_node, base), ""),
                    };
                    edits.push((at, at, format!("{text}\n{after}")));
                }
            }
        }
    }
    // Apply from the end; at one offset, the insertions after an item come before its
    // removal, and keep head's order among themselves.
    edits.sort_by_key(|(start, _, _)| *start);
    let mut out = base.to_string();
    for (start, end, replacement) in edits.into_iter().rev() {
        out.replace_range(start..end, &replacement);
    }
    Some(out)
}

/// `source` without the imports of `names`: a `use` of one of them goes away, and one of
/// them leaves a `use` list. Only `use` declarations at the top of a list are read.
pub fn remove_imports(source: &str, names: &[&str]) -> String {
    if names.is_empty() {
        return source.to_string();
    }
    let Some(tree) = Lang::Rust.parse(source) else {
        return source.to_string();
    };
    let last = |n: Node| -> String {
        let path = match n.kind() {
            "use_as_clause" => n.child_by_field_name("path").unwrap_or(n),
            _ => n,
        };
        let t = text(path, source);
        t.rsplit("::").next().unwrap_or(t).to_string()
    };
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() != "use_declaration" {
            stack.extend(children(node));
            continue;
        }
        let Some(arg) = node.child_by_field_name("argument") else {
            continue;
        };
        match arg.kind() {
            "identifier" | "scoped_identifier" | "use_as_clause" => {
                if names.contains(&last(arg).as_str()) {
                    cuts.push((indent_start(source, node.start_byte()), node.end_byte()));
                }
            }
            "scoped_use_list" => {
                let Some(list) = arg.child_by_field_name("list") else {
                    continue;
                };
                let items = children(list);
                let gone: Vec<Node> = items
                    .iter()
                    .copied()
                    .filter(|i| i.kind() != "use_list" && names.contains(&last(*i).as_str()))
                    .collect();
                if gone.is_empty() {
                    continue;
                }
                if gone.len() == items.len() {
                    cuts.push((indent_start(source, node.start_byte()), node.end_byte()));
                    continue;
                }
                for item in gone {
                    // The item and the comma after it, or else the comma before it.
                    let next = item.next_sibling().filter(|n| n.kind() == ",");
                    let prev = item.prev_sibling().filter(|n| n.kind() == ",");
                    let (start, end) = match (next, prev) {
                        (Some(comma), _) => {
                            let after = comma
                                .next_sibling()
                                .map_or(comma.end_byte(), |n| n.start_byte());
                            (item.start_byte(), after)
                        }
                        (None, Some(comma)) => (comma.start_byte(), item.end_byte()),
                        (None, None) => (item.start_byte(), item.end_byte()),
                    };
                    cuts.push((start, end));
                }
            }
            _ => {}
        }
    }
    cuts.sort();
    let mut out = source.to_string();
    for (start, end) in cuts.into_iter().rev() {
        out.replace_range(start..end, "");
    }
    out
}

/// An item's kind, name, and how many items of that kind and name come before it in its
/// container.
type Key = (String, String, usize);

/// One item of a container, from its first attribute to its end.
struct Entry {
    key: Option<Key>,
    test: bool,
    start: usize,
    end: usize,
}

fn entries(container: Node, source: &str) -> Vec<Entry> {
    let mut seen: std::collections::HashMap<(String, String), usize> = Default::default();
    children(container)
        .into_iter()
        .filter(|n| {
            !matches!(
                n.kind(),
                "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment"
            )
        })
        .map(|n| {
            let name = match n.kind() {
                "impl_item" => n.child_by_field_name("type").map(|t| text(t, source)),
                "use_declaration" => Some(text(n, source)),
                _ => n.child_by_field_name("name").map(|t| text(t, source)),
            };
            let key = name.map(|name| {
                let k = (n.kind().to_string(), name.to_string());
                let count = seen.entry(k.clone()).or_default();
                *count += 1;
                (k.0, k.1, *count - 1)
            });
            Entry {
                key,
                test: is_test_item(n, source),
                start: attributes(n).1.start_byte(),
                end: n.end_byte(),
            }
        })
        .collect()
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

/// The body of the inline module at `path`: the file itself for the root.
fn container<'t>(root: Node<'t>, source: &str, path: &[String]) -> Option<Node<'t>> {
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
    Some(node)
}

/// The byte offset just inside the end of a container: the end of the file, or a module
/// body's closing brace.
fn container_end(node: Node, source: &str) -> usize {
    if node.kind() == "source_file" {
        source.len()
    } else {
        node.end_byte() - 1
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
        // Production code that shares a line with a test keeps that line.
        let shared = "fn f() -> i32 { 2 } #[test] fn t() {\n    assert_eq!(f(), 2);\n} fn g() {}\n";
        assert_eq!(test_lines(shared), vec![(2, 2)]);
        let ranges = test_ranges(shared);
        assert_eq!(
            &shared[ranges[0].0..ranges[0].1],
            "#[test] fn t() {\n    assert_eq!(f(), 2);\n}"
        );
        assert_eq!(test_spans(shared), vec![(1, 3)]);
    }

    #[test]
    fn removes_imports_of_names_base_lacks() {
        let source = "use demo::{clamp, double, other as o};\nuse demo::double;\nuse demo::{double};\nuse std::fmt;\n";
        assert_eq!(
            remove_imports(source, &["double"]),
            "use demo::{clamp, other as o};\n\n\nuse std::fmt;\n"
        );
        assert_eq!(
            remove_imports("use demo::{clamp, double};\n", &["double", "clamp"]),
            "\n"
        );
    }

    #[test]
    fn conditional_test_attributes_count() {
        let source = "#[cfg_attr(feature = \"checks\", test)]\nfn gated() {}\n\n#[cfg_attr(feature = \"x\", derive(Debug))]\nstruct S;\n";
        let tests = find_tests(Lang::Rust, source, PythonStyle::Pytest);
        assert_eq!(summary(&tests), vec!["gated 1-2 fn 2"]);
        assert_eq!(test_lines(source), vec![(1, 2)]);
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

    #[test]
    fn splices_keep_test_items_where_macros_and_items_around_them_are() {
        let base = r#"#![allow(unused)]
macro_rules! value {
    () => { 1 };
}

#[test]
fn uses_first() {
    assert_eq!(value!(), 1);
}

macro_rules! value {
    () => { 2 };
}

pub fn f() {}
"#;
        let head = r#"#![allow(unused)]
#[test]
fn leading() {}

macro_rules! value {
    () => { 1 };
}

#[test]
fn uses_first() {
    assert_eq!(value!(), 1, "the first definition");
}

#[test]
fn also_first() {
    assert_eq!(value!(), 1);
}

macro_rules! value {
    () => { 2 };
}

pub fn f() {}
"#;
        assert_eq!(
            splice_tests(base, head).unwrap(),
            r#"#![allow(unused)]
#[test]
fn leading() {}
macro_rules! value {
    () => { 1 };
}

#[test]
fn uses_first() {
    assert_eq!(value!(), 1, "the first definition");
}
#[test]
fn also_first() {
    assert_eq!(value!(), 1);
}

macro_rules! value {
    () => { 2 };
}

pub fn f() {}
"#
        );
    }
}
