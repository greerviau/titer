//! Rust: tests, functions, and test-only items.
//!
//! Rust keeps unit tests in the source file they test, inside `#[cfg(test)]` items. titer
//! treats those items as test code: their lines are never mutated, and prove splices head's
//! test items into base's source file.

use std::collections::BTreeMap;

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

/// Whether an attribute marks a test: a test attribute, or `cfg_attr(condition, ...)` over
/// one, which makes the function a test when the build enables the condition.
fn marks_test(path: &str, whole: &str) -> bool {
    if is_test_attribute(path) {
        return true;
    }
    let compact: String = whole.chars().filter(|c| !c.is_whitespace()).collect();
    cfg_attr_marks_test(&compact)
}

fn cfg_attr_marks_test(attr: &str) -> bool {
    let Some(args) = call_args(attr, "cfg_attr") else {
        return false;
    };
    split_top_level(args).iter().skip(1).any(|a| {
        let path = a.split('(').next().unwrap_or_default();
        is_test_attribute(path) || cfg_attr_marks_test(a)
    })
}

/// The text inside `name(...)`, with whitespace already removed.
fn call_args<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')
}

/// `a,b(c,d),e` split at commas outside parentheses and brackets.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

/// Whether a `cfg(...)` holds only in test builds: `cfg(test)`, `all(...)` with such a
/// predicate, or `any(...)` of only such predicates.
fn is_cfg_test(attr: &str) -> bool {
    let compact: String = attr.chars().filter(|c| !c.is_whitespace()).collect();
    call_args(&compact, "cfg").is_some_and(requires_test)
}

fn requires_test(predicate: &str) -> bool {
    if predicate == "test" {
        return true;
    }
    if let Some(args) = call_args(predicate, "all") {
        return split_top_level(args).into_iter().any(requires_test);
    }
    if let Some(args) = call_args(predicate, "any") {
        let parts = split_top_level(args);
        return !parts.is_empty() && parts.into_iter().all(requires_test);
    }
    false
}

/// Whether an item is test-only: marked `#[cfg(test)]` or a test function.
pub(crate) fn is_test_item(item: Node, source: &str) -> bool {
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
                    parametrized: paths.iter().any(|p| {
                        let last = p.rsplit("::").next().unwrap_or(p);
                        matches!(last, "rstest" | "test_case" | "quickcheck")
                    }),
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

/// A step into an inline module: its name, and how many non-test modules of that name
/// come before it in its container. Two `#[cfg]` variants of one module stay apart.
pub type Step = (String, usize);

/// An outermost test-only item: one marked `#[cfg(test)]` or with a test attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestItem {
    /// The inline modules that contain the item, outermost first, or None when another
    /// item, such as an `impl`, contains it.
    pub container: Option<Vec<Step>>,
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
    mut container: Option<&mut Vec<Step>>,
    out: &mut Vec<TestItem>,
) {
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
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
                let name = name_of(child, source).to_string();
                let count = seen.entry(name.clone()).or_default();
                let step = (name, *count);
                *count += 1;
                // A module inside an impl or function is not a container prove can find.
                match container.as_deref_mut() {
                    Some(path) => {
                        path.push(step);
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
/// Rust items can come in any order, except that a `macro_rules!` macro is visible only
/// after its definition. So each head test item goes right after its anchor: the nearest
/// non-test item before it in head that base also has, matched by kind, name (or text,
/// for an unnamed item such as an item macro), and occurrence. A base test item with the
/// same key and anchor is replaced where it stands. A module that base lacks becomes an
/// empty shell around head's test items, which then compile on base only if they use
/// nothing new. Base's test items that head lacks go away.
///
/// Returns None when splicing leaves base unchanged.
pub fn splice_tests(base: &str, head: &str) -> Option<String> {
    let base_tree = Lang::Rust.parse(base)?;
    let head_tree = Lang::Rust.parse(head)?;
    let mut paths: Vec<Vec<Step>> = test_items(base_tree.root_node(), base)
        .into_iter()
        .chain(test_items(head_tree.root_node(), head))
        .filter_map(|i| i.container)
        .collect();
    paths.sort();
    paths.dedup();
    let mut edits: Vec<Edit> = Vec::new();
    // Head's test items in modules base lacks, by the deepest module base has and the
    // first missing step.
    let mut shells: BTreeMap<(Vec<Step>, String), Vec<ShellItem>> = BTreeMap::new();
    for path in &paths {
        let Some(head_node) = container(head_tree.root_node(), head, path) else {
            // Only base has the module: its test items go.
            if let Some(base_node) = container(base_tree.root_node(), base, path) {
                for e in entries(base_node, base).iter().filter(|e| e.test) {
                    edits.push(Edit::remove(
                        indent_start(base, e.start),
                        line_end(base, e.end),
                    ));
                }
            }
            continue;
        };
        let head_entries = entries(head_node, head);
        let Some(base_node) = container(base_tree.root_node(), base, path) else {
            let have = (0..path.len())
                .rev()
                .find(|n| container(base_tree.root_node(), base, &path[..*n]).is_some())
                .unwrap_or(0);
            let rest: Vec<String> = path[have + 1..].iter().map(|(n, _)| n.clone()).collect();
            let key = (path[..have].to_vec(), path[have].0.clone());
            for e in head_entries.iter().filter(|e| e.test) {
                shells.entry(key.clone()).or_default().push((
                    rest.clone(),
                    head[indent_start(head, e.start)..e.end].to_string(),
                ));
            }
            continue;
        };
        let base_entries = entries(base_node, base);
        // The base index of the nearest non-test head item before head entry `i` that
        // base also has.
        let anchor = |i: usize| -> Option<usize> {
            head_entries[..i]
                .iter()
                .rev()
                .filter(|e| !e.test)
                .find_map(|e| base_entries.iter().position(|t| !t.test && t.key == e.key))
        };
        let mut kept: Vec<usize> = Vec::new();
        // Where the last head test item went, by anchor, so the next one with that anchor
        // follows it.
        let mut cursor: Option<(Option<usize>, usize)> = None;
        for (i, h) in head_entries.iter().enumerate().filter(|(_, h)| h.test) {
            let text = &head[h.start..h.end];
            let at = anchor(i);
            let same = base_entries.iter().enumerate().position(|(b, e)| {
                e.test
                    && e.key.is_some()
                    && e.key == h.key
                    && !kept.contains(&b)
                    && base_anchor(&base_entries, &head_entries, b) == at
            });
            if let Some(b) = same {
                kept.push(b);
                edits.push(Edit::replace(
                    base_entries[b].start,
                    base_entries[b].end,
                    text,
                ));
                cursor = Some((at, base_entries[b].end));
                continue;
            }
            let indent = &head[indent_start(head, h.start)..h.start];
            let after = match cursor {
                Some((a, end)) if a == at => Some(end),
                _ => at.map(|a| base_entries[a].end),
            };
            match after {
                Some(end) => {
                    edits.push(Edit::insert(end, format!("\n{indent}{text}")));
                    cursor = Some((at, end));
                }
                None => {
                    // Before every item base has; past the indentation of the first one, or
                    // inside an empty container.
                    let (start, lead) = match base_entries.first() {
                        Some(first) => (indent_start(base, first.start), String::new()),
                        None => {
                            let end = container_end(base_node, base);
                            let nl = if base[..end].ends_with('\n') {
                                ""
                            } else {
                                "\n"
                            };
                            (end, nl.to_string())
                        }
                    };
                    edits.push(Edit::insert(start, format!("{lead}{indent}{text}\n")));
                }
            }
        }
        for (b, e) in base_entries.iter().enumerate() {
            if e.test && !kept.contains(&b) {
                edits.push(Edit::remove(
                    indent_start(base, e.start),
                    line_end(base, e.end),
                ));
            }
        }
    }
    for ((have, first), items) in shells {
        let Some(node) = container(base_tree.root_node(), base, &have) else {
            continue;
        };
        let at = container_end(node, base);
        let lead = if base[..at].ends_with('\n') { "" } else { "\n" };
        edits.push(Edit::insert(at, format!("{lead}{}", shell(&first, &items))));
    }
    edits.sort_by_key(|e| (e.start, e.end > e.start));
    let mut out = base.to_string();
    for e in edits.into_iter().rev() {
        out.replace_range(e.start..e.end, &e.text);
    }
    (out != base).then_some(out)
}

/// Just past the newline that ends the line `at` is on, when only whitespace follows `at`
/// there; else `at`.
fn line_end(source: &str, at: usize) -> usize {
    match source[at..].find('\n') {
        Some(i) if source[at..at + i].trim().is_empty() => at + i + 1,
        None if source[at..].trim().is_empty() => source.len(),
        _ => at,
    }
}

/// The anchor of base entry `b`, as an index into base's entries: the nearest non-test
/// entry before it that head also has.
fn base_anchor(base: &[Entry], head: &[Entry], b: usize) -> Option<usize> {
    base[..b].iter().enumerate().rev().find_map(|(i, e)| {
        (!e.test && e.key.is_some() && head.iter().any(|h| !h.test && h.key == e.key)).then_some(i)
    })
}

/// A test item for a shell module: the module names below the shell, and its text.
type ShellItem = (Vec<String>, String);

/// `mod name { ... }` around test items, with nested shells for deeper modules.
fn shell(name: &str, items: &[ShellItem]) -> String {
    let mut body = String::new();
    let mut nested: BTreeMap<String, Vec<ShellItem>> = BTreeMap::new();
    for (rest, text) in items {
        match rest.split_first() {
            None => {
                body.push_str(text);
                body.push('\n');
            }
            Some((first, deeper)) => nested
                .entry(first.clone())
                .or_default()
                .push((deeper.to_vec(), text.clone())),
        }
    }
    for (inner, items) in nested {
        body.push_str(&shell(&inner, &items));
    }
    format!("mod {name} {{\n{body}}}\n")
}

/// One edit of base's bytes.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

impl Edit {
    fn insert(at: usize, text: String) -> Edit {
        Edit {
            start: at,
            end: at,
            text,
        }
    }

    fn remove(start: usize, end: usize) -> Edit {
        Edit {
            start,
            end,
            text: String::new(),
        }
    }

    fn replace(start: usize, end: usize, text: &str) -> Edit {
        Edit {
            start,
            end,
            text: text.to_string(),
        }
    }
}

/// The last path segment a use tree imports: `n` for `demo::api::n` and for
/// `demo::n as m`.
fn imported(node: Node, source: &str) -> String {
    let path = match node.kind() {
        "use_as_clause" => node.child_by_field_name("path").unwrap_or(node),
        _ => node,
    };
    let t = text(path, source);
    t.rsplit("::").next().unwrap_or(t).to_string()
}

/// A use tree without the imports of `names`, or None when nothing is left of it.
fn kept_tree(node: Node, source: &str, names: &[&str]) -> Option<String> {
    match node.kind() {
        "scoped_use_list" | "use_list" => {
            let list = if node.kind() == "use_list" {
                node
            } else {
                node.child_by_field_name("list")?
            };
            let items = children(list);
            let kept: Vec<String> = items
                .iter()
                .filter_map(|i| kept_tree(*i, source, names))
                .collect();
            if kept.is_empty() {
                return None;
            }
            let unchanged = kept.len() == items.len()
                && kept.iter().zip(&items).all(|(k, i)| k == text(*i, source));
            if unchanged {
                return Some(text(node, source).to_string());
            }
            let inner = format!("{{{}}}", kept.join(", "));
            Some(match node.child_by_field_name("path") {
                Some(path) => format!("{}::{inner}", text(path, source)),
                None => inner,
            })
        }
        "use_wildcard" => Some(text(node, source).to_string()),
        _ => (!names.contains(&imported(node, source).as_str()))
            .then(|| text(node, source).to_string()),
    }
}

fn use_declarations<'t>(root: Node<'t>) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "use_declaration" {
            out.push(node);
        } else {
            stack.extend(children(node));
        }
    }
    out
}

/// `source` without the imports of `names`: a `use` left with nothing goes away, and a
/// `use` list keeps the rest.
pub fn remove_imports(source: &str, names: &[&str]) -> String {
    if names.is_empty() {
        return source.to_string();
    }
    let Some(tree) = Lang::Rust.parse(source) else {
        return source.to_string();
    };
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for decl in use_declarations(tree.root_node()) {
        let Some(arg) = decl.child_by_field_name("argument") else {
            continue;
        };
        match kept_tree(arg, source, names) {
            None => edits.push((
                indent_start(source, decl.start_byte()),
                line_end(source, decl.end_byte()),
                String::new(),
            )),
            Some(kept) if kept != text(arg, source) => {
                edits.push((arg.start_byte(), arg.end_byte(), kept));
            }
            Some(_) => {}
        }
    }
    edits.sort_by_key(|e| e.0);
    let mut out = source.to_string();
    for (start, end, text) in edits.into_iter().rev() {
        out.replace_range(start..end, &text);
    }
    out
}

/// The names a file's code can use for the imported `name`: the name itself, and each
/// alias a `use ... as` gives it.
pub fn local_names(source: &str, name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    let Some(tree) = Lang::Rust.parse(source) else {
        return out;
    };
    let mut stack = use_declarations(tree.root_node());
    while let Some(node) = stack.pop() {
        if node.kind() == "use_as_clause"
            && imported(node, source) == name
            && let Some(alias) = node.child_by_field_name("alias")
        {
            let alias = text(alias, source).to_string();
            if !out.contains(&alias) {
                out.push(alias);
            }
        }
        stack.extend(children(node));
    }
    out
}

/// An item's kind, its name (or its text, for an unnamed item), and how many items with
/// that kind and name come before it in its container.
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
                _ => n.child_by_field_name("name").map(|t| text(t, source)),
            }
            .unwrap_or_else(|| text(n, source));
            let k = (n.kind().to_string(), name.to_string());
            let count = seen.entry(k.clone()).or_default();
            *count += 1;
            Entry {
                key: Some((k.0, k.1, *count - 1)),
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
fn container<'t>(root: Node<'t>, source: &str, path: &[Step]) -> Option<Node<'t>> {
    let mut node = root;
    for (name, occurrence) in path {
        let module = children(node)
            .into_iter()
            .filter(|c| {
                c.kind() == "mod_item"
                    && name_of(*c, source) == name
                    && !is_test_item(*c, source)
                    && c.child_by_field_name("body").is_some()
            })
            .nth(*occurrence)?;
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
            "use demo::{clamp, other as o};\nuse std::fmt;\n"
        );
        assert_eq!(
            remove_imports("use demo::{clamp, double};\n", &["double", "clamp"]),
            ""
        );
        assert_eq!(
            remove_imports("use demo::{n, x, y};\n", &["x", "y"]),
            "use demo::{n};\n"
        );
        assert_eq!(
            remove_imports("use demo::{api::{new_api, n}, m};\n", &["new_api"]),
            "use demo::{api::{n}, m};\n"
        );
        assert_eq!(
            remove_imports("use demo::new_api as renamed;\nfn f() {}\n", &["new_api"]),
            "fn f() {}\n"
        );
        assert_eq!(
            local_names(
                "use demo::new_api as renamed;\nuse demo::{x::new_api};\n",
                "new_api"
            ),
            vec!["new_api", "renamed"]
        );
    }

    #[test]
    fn conditional_test_attributes_count() {
        let source = "#[cfg_attr(feature = \"checks\", test)]\nfn gated() {}\n\n#[cfg_attr(feature = \"x\", derive(Debug))]\nstruct S;\n";
        let tests = find_tests(Lang::Rust, source, PythonStyle::Pytest);
        assert_eq!(summary(&tests), vec!["gated 1-2 fn 2"]);
        assert_eq!(test_lines(source), vec![(1, 2)]);
        let nested = "#[cfg_attr(all(), cfg_attr(all(), test))]\nfn deep() {}\n";
        assert_eq!(
            summary(&find_tests(Lang::Rust, nested, PythonStyle::Pytest)),
            vec!["deep 1-2 fn 2"]
        );
        let gated = "#[cfg(all(test, unix))]\nfn helper() -> i32 {\n    1\n}\n#[cfg(any(test, unix))]\nfn real() {}\n#[cfg(not(test))]\nfn prod() {}\n";
        assert_eq!(test_lines(gated), vec![(1, 4)]);
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
        // Head's root tests follow `add`, the item before them in head that base has; the
        // module base lacks becomes a shell.
        assert_eq!(
            spliced,
            r#"pub fn add(a: i32, b: i32) -> i32 {
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
"#
        );
        assert_eq!(splice_tests(head, head), None);
    }

    #[test]
    fn splices_handle_renames_moves_shells_and_twin_modules() {
        // A renamed first test: the removal and the insertion share an offset.
        let base = "#[test]\nfn old() {}\n\npub fn f() {}\n";
        let head = "#[test]\nfn new() {}\n\npub fn f() {}\n";
        assert_eq!(
            splice_tests(base, head).unwrap(),
            "#[test]\nfn new() {}\n\npub fn f() {}\n"
        );

        // A test that moved past a second macro definition moves on base too.
        let base = "macro_rules! v { () => { 1 }; }\n#[test]\nfn t() { assert_eq!(v!(), 2); }\nmacro_rules! v { () => { 2 }; }\n";
        let head = "macro_rules! v { () => { 1 }; }\nmacro_rules! v { () => { 2 }; }\n#[test]\nfn t() { assert_eq!(v!(), 2); }\n";
        assert_eq!(
            splice_tests(base, head).unwrap(),
            "macro_rules! v { () => { 1 }; }\nmacro_rules! v { () => { 2 }; }\n#[test]\nfn t() { assert_eq!(v!(), 2); }\n"
        );

        // A new test after an item macro follows it on base.
        let base = "make_want!();\npub fn f() {}\n";
        let head = "make_want!();\n#[test]\nfn uses_want() {}\npub fn f() {}\n";
        assert_eq!(
            splice_tests(base, head).unwrap(),
            "make_want!();\n#[test]\nfn uses_want() {}\npub fn f() {}\n"
        );

        // The same test moved between modules is a change.
        let base = "mod a {\n    #[test]\n    fn t() {}\n}\nmod b {}\n";
        let head = "mod a {}\nmod b {\n    #[test]\n    fn t() {}\n}\n";
        assert_eq!(
            splice_tests(base, head).unwrap(),
            "mod a {\n}\nmod b {\n    #[test]\n    fn t() {}\n}\n"
        );

        // A test in the second of two same-named modules goes into the second.
        let base = "#[cfg(unix)]\nmod os {}\n#[cfg(windows)]\nmod os {}\n";
        let head =
            "#[cfg(unix)]\nmod os {}\n#[cfg(windows)]\nmod os {\n    #[test]\n    fn t() {}\n}\n";
        assert_eq!(
            splice_tests(base, head).unwrap(),
            "#[cfg(unix)]\nmod os {}\n#[cfg(windows)]\nmod os {\n    #[test]\n    fn t() {}\n}\n"
        );

        // A module base lacks becomes an empty shell around head's tests.
        let base = "pub fn n() -> i32 {\n    1\n}\n";
        let head = "pub fn n() -> i32 {\n    1\n}\n\nmod fresh {\n    pub fn new() {}\n\n    #[cfg(test)]\n    mod tests {\n        #[test]\n        fn calls_n() {\n            assert_eq!(crate::n(), 1);\n        }\n    }\n}\n";
        assert_eq!(
            splice_tests(base, head).unwrap(),
            "pub fn n() -> i32 {\n    1\n}\nmod fresh {\n    #[cfg(test)]\n    mod tests {\n        #[test]\n        fn calls_n() {\n            assert_eq!(crate::n(), 1);\n        }\n    }\n}\n"
        );
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
