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

/// Base's source file with its test items replaced by head's. Returns None when that
/// leaves base's items as they were.
///
/// Rust items can come in any order, except that a `macro_rules!` macro is visible only
/// after its definition. So each module is rebuilt in head's item order: an item both
/// sides have keeps base's text (with its own modules merged the same way), an item only
/// base has stays after the base item before it, a test item comes from head, and a
/// non-test item only head has is left out. A module only head has keeps just its test
/// items, so they compile on base only if they use nothing new. Items match by kind, name
/// (or text, for an unnamed item such as an item macro), and occurrence. Comments between
/// items are not kept.
pub fn splice_tests(base: &str, head: &str) -> Option<String> {
    let base_tree = Lang::Rust.parse(base)?;
    let head_tree = Lang::Rust.parse(head)?;
    let rebuild = |other: &str, other_tree: &tree_sitter::Tree| {
        let root = base_tree.root_node();
        let entries = entries(root, base);
        let prefix = entries
            .first()
            .map_or(base, |e| &base[..indent_start(base, e.start)]);
        let body = merge(Some((root, base)), (other_tree.root_node(), other));
        format!("{prefix}{body}")
    };
    let spliced = rebuild(head, &head_tree);
    (spliced != rebuild(base, &base_tree)).then_some(spliced)
}

/// The items of one module, merged as `splice_tests` describes, one per paragraph.
fn merge(base: Option<(Node, &str)>, head: (Node, &str)) -> String {
    let (head_node, head_src) = head;
    let head_entries = entries(head_node, head_src);
    let base_entries = base.map(|(n, src)| entries(n, src)).unwrap_or_default();
    let base_src = base.map_or("", |(_, src)| src);
    let in_head = |e: &Entry| head_entries.iter().any(|h| !h.test && h.key == e.key);
    let mut out: Vec<String> = Vec::new();
    let mut emitted = vec![false; base_entries.len()];
    let mut next = 0;
    let base_only = |k: usize| !base_entries[k].test && !in_head(&base_entries[k]);
    for h in &head_entries {
        if h.test {
            out.push(head_src[indent_start(head_src, h.start)..h.end].to_string());
            continue;
        }
        let common = base_entries.iter().position(|b| !b.test && b.key == h.key);
        match common {
            Some(j) => {
                for k in next..j {
                    if base_only(k) && !emitted[k] {
                        out.push(entry_text(&base_entries[k], base_src));
                        emitted[k] = true;
                    }
                }
                let (base_node, _) = base.expect("a common item has a base");
                out.push(merged_item(
                    &base_entries[j],
                    base_node,
                    base_src,
                    h,
                    head_node,
                    head_src,
                ));
                emitted[j] = true;
                next = next.max(j + 1);
            }
            None => {
                if let Some(shell) = shell(h, head_node, head_src) {
                    out.push(shell);
                }
            }
        }
    }
    for (k, e) in base_entries.iter().enumerate() {
        if base_only(k) && !emitted[k] {
            out.push(entry_text(e, base_src));
        }
    }
    let mut text = out.join("\n\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

fn entry_text(e: &Entry, source: &str) -> String {
    source[indent_start(source, e.start)..e.end].to_string()
}

/// The inline module `entry` of `container`, when it has a body.
fn module_body<'t>(container: Node<'t>, source: &str, entry: &Entry) -> Option<Node<'t>> {
    children(container)
        .into_iter()
        .find(|c| c.end_byte() == entry.end && c.kind() == "mod_item")
        .and_then(|m| m.child_by_field_name("body"))
        .filter(|_| source.len() >= entry.end)
}

/// An item both sides have: base's text, with an inline module's items merged.
fn merged_item(
    b: &Entry,
    base_container: Node,
    base_src: &str,
    h: &Entry,
    head_container: Node,
    head_src: &str,
) -> String {
    let base_body = module_body(base_container, base_src, b);
    let head_body = module_body(head_container, head_src, h);
    let (Some(base_body), Some(head_body)) = (base_body, head_body) else {
        return entry_text(b, base_src);
    };
    let open = base_body.start_byte() + 1;
    let inner_entries = entries(base_body, base_src);
    // Inner attributes and comments before the module's first item stay.
    let lead_end = inner_entries
        .first()
        .map_or(open, |e| indent_start(base_src, e.start));
    let merged = merge(Some((base_body, base_src)), (head_body, head_src));
    let close = base_body.end_byte() - 1;
    let indent = &base_src[indent_start(base_src, close)..close];
    format!(
        "{}{}{merged}{indent}}}",
        &base_src[indent_start(base_src, b.start)..open],
        if base_src[open..lead_end].contains('\n') {
            base_src[open..lead_end].to_string()
        } else {
            format!("{}\n", &base_src[open..lead_end])
        },
    )
}

/// A non-test inline module only head has, reduced to its test items and the modules
/// that hold more of them; None when it holds none.
fn shell(h: &Entry, head_container: Node, head_src: &str) -> Option<String> {
    let body = module_body(head_container, head_src, h)?;
    let merged = merge(None, (body, head_src));
    if merged.is_empty() {
        return None;
    }
    let module = children(head_container)
        .into_iter()
        .find(|c| c.end_byte() == h.end)?;
    let name = name_of(module, head_src);
    let close = body.end_byte() - 1;
    let indent = &head_src[indent_start(head_src, close)..close];
    let lead = &head_src[indent_start(head_src, h.start)..h.start];
    Some(format!("{lead}mod {name} {{\n{merged}{indent}}}"))
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

/// `prefix::path`, without whitespace or a leading `::`.
fn join_path(prefix: &str, path: &str) -> String {
    let path: String = path.chars().filter(|c| !c.is_whitespace()).collect();
    let full = if prefix.is_empty() {
        path
    } else {
        format!("{prefix}::{path}")
    };
    full.trim_start_matches("::").to_string()
}

/// The full path a leaf of a use tree imports, without its alias: `demo::api::n` for
/// `n` inside `use demo::api::{n}`, and `demo::n` for `demo::n as m`.
fn imported_path(node: Node, source: &str, prefix: &str) -> String {
    let path = match node.kind() {
        "use_as_clause" => node.child_by_field_name("path").unwrap_or(node),
        _ => node,
    };
    join_path(prefix, text(path, source))
}

/// A use tree without the imports of `paths`, or None when nothing is left of it.
/// `prefix` is the path the enclosing lists add.
fn kept_tree(node: Node, source: &str, prefix: &str, paths: &[&str]) -> Option<String> {
    match node.kind() {
        "scoped_use_list" | "use_list" => {
            let (list, inner_prefix) = if node.kind() == "use_list" {
                (node, prefix.to_string())
            } else {
                let path = node
                    .child_by_field_name("path")
                    .map_or(String::new(), |p| join_path(prefix, text(p, source)));
                (node.child_by_field_name("list")?, path)
            };
            let items = children(list);
            let kept: Vec<String> = items
                .iter()
                .filter_map(|i| kept_tree(*i, source, &inner_prefix, paths))
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
        _ => (!paths.contains(&imported_path(node, source, prefix).as_str()))
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

/// `source` without the imports of `paths`, such as `demo::new_api`: a `use` left with
/// nothing goes away, and a `use` list keeps the rest. An import of another path that
/// ends in the same name stays.
pub fn remove_imports(source: &str, paths: &[&str]) -> String {
    if paths.is_empty() {
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
        match kept_tree(arg, source, "", paths) {
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

/// The names a file's code uses for an import of `path`: each alias a `use ... as` gives
/// it, and its last segment where it is imported without one.
pub fn local_names(source: &str, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(tree) = Lang::Rust.parse(source) else {
        return out;
    };
    let mut stack: Vec<(Node, String)> = use_declarations(tree.root_node())
        .into_iter()
        .filter_map(|d| {
            d.child_by_field_name("argument")
                .map(|a| (a, String::new()))
        })
        .collect();
    while let Some((node, prefix)) = stack.pop() {
        match node.kind() {
            "scoped_use_list" => {
                let inner = node
                    .child_by_field_name("path")
                    .map_or(prefix.clone(), |p| join_path(&prefix, text(p, source)));
                if let Some(list) = node.child_by_field_name("list") {
                    stack.extend(children(list).into_iter().map(|c| (c, inner.clone())));
                }
            }
            "use_list" => stack.extend(children(node).into_iter().map(|c| (c, prefix.clone()))),
            _ if imported_path(node, source, &prefix) == path => {
                let local = match node.child_by_field_name("alias") {
                    Some(alias) => text(alias, source).to_string(),
                    None => path.rsplit("::").next().unwrap_or(path).to_string(),
                };
                if !out.contains(&local) {
                    out.push(local);
                }
            }
            _ => {}
        }
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
    fn removes_only_the_imports_of_the_paths_base_lacks() {
        let source = "use demo::{clamp, double, other as o};\nuse demo::double;\nuse demo::{double};\nuse std::fmt;\nuse demo::fixed::double as call;\n";
        assert_eq!(
            remove_imports(source, &["demo::double"]),
            "use demo::{clamp, other as o};\nuse std::fmt;\nuse demo::fixed::double as call;\n"
        );
        assert_eq!(
            remove_imports(
                "use demo::{clamp, double};\n",
                &["demo::double", "demo::clamp"]
            ),
            ""
        );
        assert_eq!(
            remove_imports("use demo::{n, x, y};\n", &["demo::x", "demo::y"]),
            "use demo::{n};\n"
        );
        assert_eq!(
            remove_imports(
                "use demo::{api::{new_api, n}, m};\n",
                &["demo::api::new_api"]
            ),
            "use demo::{api::{n}, m};\n"
        );
        assert_eq!(
            remove_imports(
                "use demo::new_api as renamed;\nfn f() {}\n",
                &["demo::new_api"]
            ),
            "fn f() {}\n"
        );
        assert_eq!(
            local_names(
                "use demo::new_api as renamed;\nuse demo::{x::new_api};\nuse demo::{new_api};\n",
                "demo::new_api"
            ),
            vec!["renamed", "new_api"]
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
        let cases = [
            // A renamed test.
            (
                "#[test]\nfn old() {}\n\npub fn f() {}\n",
                "#[test]\nfn new() {}\n\npub fn f() {}\n",
                "#[test]\nfn new() {}\n\npub fn f() {}\n",
            ),
            // A test that moved past a second macro definition moves on base too.
            (
                "macro_rules! v { () => { 1 }; }\n#[test]\nfn t() { assert_eq!(v!(), 2); }\nmacro_rules! v { () => { 2 }; }\n",
                "macro_rules! v { () => { 1 }; }\nmacro_rules! v { () => { 2 }; }\n#[test]\nfn t() { assert_eq!(v!(), 2); }\n",
                "macro_rules! v { () => { 1 }; }\n\nmacro_rules! v { () => { 2 }; }\n\n#[test]\nfn t() { assert_eq!(v!(), 2); }\n",
            ),
            // Test-only macros come from head, in head's order.
            (
                "#[cfg(test)]\nmacro_rules! v { () => { 0 }; }\n#[test]\nfn t() { assert_eq!(v!(), 1); }\n#[cfg(test)]\nmacro_rules! v { () => { 1 }; }\n",
                "#[cfg(test)]\nmacro_rules! v { () => { 0 }; }\n#[cfg(test)]\nmacro_rules! v { () => { 1 }; }\n#[test]\nfn t() { assert_eq!(v!(), 1); }\n",
                "#[cfg(test)]\nmacro_rules! v { () => { 0 }; }\n\n#[cfg(test)]\nmacro_rules! v { () => { 1 }; }\n\n#[test]\nfn t() { assert_eq!(v!(), 1); }\n",
            ),
            // A new test after an item macro follows it.
            (
                "make_want!();\npub fn f() {}\n",
                "make_want!();\n#[test]\nfn uses_want() {}\npub fn f() {}\n",
                "make_want!();\n\n#[test]\nfn uses_want() {}\n\npub fn f() {}\n",
            ),
            // The same test moved between modules.
            (
                "mod a {\n    #[test]\n    fn t() {}\n}\nmod b {}\n",
                "mod a {}\nmod b {\n    #[test]\n    fn t() {}\n}\n",
                "mod a {\n}\n\nmod b {\n    #[test]\n    fn t() {}\n}\n",
            ),
            // A test in the second of two same-named modules.
            (
                "#[cfg(unix)]\nmod os {}\n#[cfg(windows)]\nmod os {}\n",
                "#[cfg(unix)]\nmod os {}\n#[cfg(windows)]\nmod os {\n    #[test]\n    fn t() {}\n}\n",
                "#[cfg(unix)]\nmod os {\n}\n\n#[cfg(windows)]\nmod os {\n    #[test]\n    fn t() {}\n}\n",
            ),
            // A module base lacks keeps only its tests.
            (
                "pub fn n() -> i32 {\n    1\n}\n",
                "pub fn n() -> i32 {\n    1\n}\n\nmod fresh {\n    pub fn new() {}\n\n    #[cfg(test)]\n    mod tests {\n        #[test]\n        fn calls_n() {\n            assert_eq!(crate::n(), 1);\n        }\n    }\n}\n",
                "pub fn n() -> i32 {\n    1\n}\n\nmod fresh {\n    #[cfg(test)]\n    mod tests {\n        #[test]\n        fn calls_n() {\n            assert_eq!(crate::n(), 1);\n        }\n    }\n}\n",
            ),
        ];
        for (base, head, expected) in cases {
            assert_eq!(splice_tests(base, head).unwrap(), expected, "{head}");
        }
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
