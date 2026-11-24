//! The static import check: does a changed test import a module or name that exists only in
//! head, or use a member that only head defines on an imported object, class, or namespace?
//! Such a test cannot prove anything on base. vitest does not type-check, so a missing export
//! or member arrives as `undefined` and can fail an assertion that proves nothing; this check
//! runs before the base run's result is read.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

use crate::config::{RunnerConfig, RunnerKind};
use crate::lang::{Lang, children, line_of, text};

/// The files of head and of the base worktree, and a way to read them.
pub struct Trees<'a> {
    pub head: &'a HashSet<String>,
    /// Base files plus head's test paths overlaid onto the base worktree.
    pub base: &'a HashSet<String>,
    pub read_head: &'a dyn Fn(&str) -> Option<String>,
    pub read_base: &'a dyn Fn(&str) -> Option<String>,
    /// tsconfig `paths` aliases: (prefix, replacement dirs), both repo-relative.
    pub aliases: &'a [(String, Vec<String>)],
}

/// A module or name that a test file imports and that base lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingImport {
    /// The binding the test file uses, or None when the whole module is missing.
    pub local: Option<String>,
    pub module: String,
    pub line: u32,
    pub message: String,
    /// For a member that base lacks on an imported value, the lines that use it. None finds
    /// uses by the binding's name instead.
    pub uses: Option<Vec<u32>>,
}

pub fn missing_imports(
    runner: &RunnerConfig,
    file: &str,
    source: &str,
    trees: &Trees,
) -> Vec<MissingImport> {
    let Some(lang) = Lang::of(file) else {
        return Vec::new();
    };
    let Some(tree) = lang.parse(source) else {
        return Vec::new();
    };
    match lang {
        Lang::Python => python(runner, file, tree.root_node(), source, trees),
        Lang::TypeScript | Lang::Tsx => typescript(file, tree.root_node(), source, trees),
        // The compiler reports a name that only head has; prove reads its errors instead.
        Lang::Rust => Vec::new(),
    }
}

/// Which of the file's missing imports a test in `span` depends on. A Python file that
/// fails to import fails every test in it. A TypeScript binding that the file uses outside
/// any test reaches every test.
pub fn affecting<'m>(
    lang: Lang,
    source: &str,
    missing: &'m [MissingImport],
    span: (u32, u32),
    test_spans: &[(u32, u32)],
) -> Option<&'m MissingImport> {
    if lang.is_python() {
        return missing.first();
    }
    let lines: Vec<&str> = source.lines().collect();
    let in_any_test = |line: u32| test_spans.iter().any(|(s, e)| (*s..=*e).contains(&line));
    missing.iter().find(|m| {
        if let Some(uses) = &m.uses {
            return uses
                .iter()
                .any(|l| (span.0..=span.1).contains(l) || !in_any_test(*l));
        }
        let Some(local) = &m.local else {
            return true;
        };
        let word = Regex::new(&format!(r"(^|[^\w$]){}($|[^\w$])", regex::escape(local)))
            .expect("escaped identifier");
        lines.iter().enumerate().any(|(i, l)| {
            let line = i as u32 + 1;
            line != m.line
                && word.is_match(l)
                && ((span.0..=span.1).contains(&line) || !in_any_test(line))
                && !l.trim_start().starts_with("import ")
        })
    })
}

// ---------------------------------------------------------------------------------------
// Python

fn python(
    runner: &RunnerConfig,
    file: &str,
    root: Node,
    source: &str,
    trees: &Trees,
) -> Vec<MissingImport> {
    let dir = parent(file);
    let cwd = if runner.cwd == "." {
        String::new()
    } else {
        runner.cwd.clone()
    };
    let mut roots = vec![cwd.clone(), join(&cwd, "src"), dir.clone()];
    if runner.kind == RunnerKind::Unittest {
        roots.insert(1, join(&cwd, &runner.start_dir));
    }
    let mut out = Vec::new();
    for node in children(root) {
        match node.kind() {
            "import_from_statement" => {
                let Some(module_node) = node.child_by_field_name("module_name") else {
                    continue;
                };
                let resolved = if module_node.kind() == "relative_import" {
                    resolve_relative_py(text(module_node, source), &dir, trees.head)
                } else {
                    resolve_py(text(module_node, source), &roots, trees.head)
                };
                let Some((module_path, package_dir)) = resolved else {
                    continue;
                };
                let module = text(module_node, source).to_string();
                if !trees.base.contains(&module_path) {
                    out.push(MissingImport {
                        local: None,
                        module: module.clone(),
                        line: line_of(node),
                        message: format!("imports module {module}, which base lacks"),
                        uses: None,
                    });
                    continue;
                }
                let head_names = (trees.read_head)(&module_path).map(|s| py_top_level(&s));
                let base_names = (trees.read_base)(&module_path).map(|s| py_top_level(&s));
                let mut cursor = node.walk();
                for name_node in node.children_by_field_name("name", &mut cursor) {
                    let (name, _) = imported_name(name_node, source);
                    let submodule = package_dir.as_ref().and_then(|d| {
                        let file = format!("{}.py", join(d, &name));
                        let pkg = join(&join(d, &name), "__init__.py");
                        [file, pkg].into_iter().find(|p| trees.head.contains(p))
                    });
                    let missing = match submodule {
                        Some(path) => !trees.base.contains(&path),
                        None => match (&head_names, &base_names) {
                            (Some(head), Some(base)) => {
                                head.names.contains(&name)
                                    && !base.names.contains(&name)
                                    && !base.dynamic
                            }
                            _ => false,
                        },
                    };
                    if missing {
                        out.push(MissingImport {
                            local: Some(name.clone()),
                            module: module.clone(),
                            line: line_of(node),
                            message: format!("imports {name} from {module}, which base lacks"),
                            uses: None,
                        });
                    }
                }
            }
            "import_statement" => {
                let mut cursor = node.walk();
                for name_node in node.children_by_field_name("name", &mut cursor) {
                    let dotted = match name_node.kind() {
                        "aliased_import" => name_node
                            .child_by_field_name("name")
                            .map(|n| text(n, source))
                            .unwrap_or_default(),
                        _ => text(name_node, source),
                    };
                    if let Some((path, _)) = resolve_py(dotted, &roots, trees.head)
                        && !trees.base.contains(&path)
                    {
                        out.push(MissingImport {
                            local: None,
                            module: dotted.to_string(),
                            line: line_of(node),
                            message: format!("imports module {dotted}, which base lacks"),
                            uses: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn imported_name(node: Node, source: &str) -> (String, String) {
    if node.kind() == "aliased_import" {
        let name = node
            .child_by_field_name("name")
            .map(|n| text(n, source).to_string())
            .unwrap_or_default();
        let alias = node
            .child_by_field_name("alias")
            .map(|n| text(n, source).to_string())
            .unwrap_or_else(|| name.clone());
        (name, alias)
    } else {
        let name = text(node, source).to_string();
        (name.clone(), name)
    }
}

/// The module file and, for a package, its directory.
fn resolve_py(
    dotted: &str,
    roots: &[String],
    files: &HashSet<String>,
) -> Option<(String, Option<String>)> {
    let rel = dotted.replace('.', "/");
    for root in roots {
        let base = join(root, &rel);
        let module = format!("{base}.py");
        if files.contains(&module) {
            return Some((module, None));
        }
        let package = join(&base, "__init__.py");
        if files.contains(&package) {
            return Some((package, Some(base)));
        }
    }
    None
}

fn resolve_relative_py(
    spec: &str,
    dir: &str,
    files: &HashSet<String>,
) -> Option<(String, Option<String>)> {
    let dots = spec.chars().take_while(|c| *c == '.').count();
    let mut base = dir.to_string();
    for _ in 1..dots {
        base = parent(&base);
    }
    let rest = &spec[dots..];
    if rest.is_empty() {
        let package = join(&base, "__init__.py");
        return files.contains(&package).then_some((package, Some(base)));
    }
    resolve_py(rest, &[base], files)
}

struct PyNames {
    names: HashSet<String>,
    /// The module defines `__getattr__` or star-imports, so absent names may still resolve.
    dynamic: bool,
}

fn py_top_level(source: &str) -> PyNames {
    let mut names = HashSet::new();
    let mut dynamic = false;
    if let Some(tree) = Lang::Python.parse(source) {
        collect_py_names(tree.root_node(), source, &mut names, &mut dynamic);
    }
    dynamic |= names.contains("__getattr__");
    PyNames { names, dynamic }
}

fn collect_py_names(block: Node, source: &str, names: &mut HashSet<String>, dynamic: &mut bool) {
    for node in children(block) {
        let def = crate::lang::python::unwrap_decorated(node).0;
        match def.kind() {
            "function_definition" | "class_definition" => {
                if let Some(n) = def.child_by_field_name("name") {
                    names.insert(text(n, source).to_string());
                }
            }
            "expression_statement" => {
                for child in children(def) {
                    if matches!(child.kind(), "assignment" | "augmented_assignment")
                        && let Some(left) = child.child_by_field_name("left")
                    {
                        collect_identifiers(left, source, names);
                    }
                }
            }
            "import_from_statement" => {
                if children(def).iter().any(|c| c.kind() == "wildcard_import") {
                    *dynamic = true;
                }
                let mut cursor = def.walk();
                for n in def.children_by_field_name("name", &mut cursor) {
                    names.insert(imported_name(n, source).1);
                }
            }
            "import_statement" => {
                let mut cursor = def.walk();
                for n in def.children_by_field_name("name", &mut cursor) {
                    let (name, alias) = imported_name(n, source);
                    let bound = if n.kind() == "aliased_import" {
                        alias
                    } else {
                        name.split('.').next().unwrap_or_default().to_string()
                    };
                    names.insert(bound);
                }
            }
            "if_statement" | "try_statement" | "with_statement" => {
                for child in children(def) {
                    if child.kind() == "block" {
                        collect_py_names(child, source, names, dynamic);
                    } else {
                        for grandchild in children(child) {
                            if grandchild.kind() == "block" {
                                collect_py_names(grandchild, source, names, dynamic);
                            }
                        }
                    }
                }
            }
            "type_alias_statement" => {
                if let Some(n) = def.child_by_field_name("left") {
                    collect_identifiers(n, source, names);
                }
            }
            _ => {}
        }
    }
}

fn collect_identifiers(node: Node, source: &str, names: &mut HashSet<String>) {
    if node.kind() == "identifier" {
        names.insert(text(node, source).to_string());
        return;
    }
    if matches!(node.kind(), "attribute" | "subscript") {
        return;
    }
    for child in children(node) {
        collect_identifiers(child, source, names);
    }
}

// ---------------------------------------------------------------------------------------
// TypeScript

const TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

fn typescript(file: &str, root: Node, source: &str, trees: &Trees) -> Vec<MissingImport> {
    let dir = parent(file);
    let mut out = Vec::new();
    for node in children(root) {
        if node.kind() != "import_statement" {
            continue;
        }
        if is_type_only(node, source) {
            continue;
        }
        let Some(spec_node) = node.child_by_field_name("source") else {
            continue;
        };
        let spec = crate::lang::ts::title(spec_node, source);
        let Some(path) = resolve_ts(&spec, &dir, trees) else {
            continue;
        };
        if !trees.base.contains(&path) {
            out.push(MissingImport {
                local: None,
                module: spec.clone(),
                line: line_of(node),
                message: format!("imports module {spec}, which base lacks"),
                uses: None,
            });
            continue;
        }
        let (Some(head), Some(base)) = ((trees.read_head)(&path), (trees.read_base)(&path)) else {
            continue;
        };
        let head_exports = ts_exports(&head);
        let base_exports = ts_exports(&base);
        let mut head_members = None;
        let mut base_members = None;
        let mocked = is_mocked(root, source, &spec, &path, &dir, trees);
        for (imported, local) in ts_bindings(node, source) {
            let (own, instance): (Vec<String>, Vec<String>) = match &imported {
                None => {
                    if base_exports.star {
                        continue;
                    }
                    let own = head_exports
                        .names
                        .iter()
                        .filter(|n| {
                            !base_exports.names.contains(*n) && !head_exports.types.contains(*n)
                        })
                        .cloned()
                        .collect();
                    (own, Vec::new())
                }
                Some(imported) if !head_exports.names.contains(imported) => continue,
                Some(imported) if !base_exports.names.contains(imported) => {
                    if !base_exports.star {
                        out.push(MissingImport {
                            local: Some(local),
                            module: spec.clone(),
                            line: line_of(node),
                            message: format!(
                                "imports {imported} from {spec}, which base does not export"
                            ),
                            uses: None,
                        });
                    }
                    continue;
                }
                Some(imported) => {
                    let head = head_members.get_or_insert_with(|| ts_members(&head));
                    let base = base_members.get_or_insert_with(|| ts_members(&base));
                    let (Some(h), Some(b)) = (head.get(imported), base.get(imported)) else {
                        continue;
                    };
                    if h.class != b.class {
                        continue;
                    }
                    let own_inherited: &[&str] = if h.class { FUNCTION_INHERITED } else { &[] };
                    let own = h
                        .own
                        .difference(&b.own)
                        .filter(|m| !own_inherited.contains(&m.as_str()))
                        .cloned()
                        .collect();
                    let instance = h.instance.difference(&b.instance).cloned().collect();
                    (own, instance)
                }
            };
            let keep = |v: Vec<String>| -> HashSet<String> {
                v.into_iter()
                    .filter(|m| !OBJECT_INHERITED.contains(&m.as_str()))
                    .collect()
            };
            let (own, instance) = (keep(own), keep(instance));
            if (own.is_empty() && instance.is_empty()) || mocked {
                continue;
            }
            let verb = if imported.is_none() {
                "export"
            } else {
                "define"
            };
            for (member, uses) in member_uses(root, source, &local, &own, &instance) {
                out.push(MissingImport {
                    local: Some(local.clone()),
                    module: spec.clone(),
                    line: line_of(node),
                    message: format!("uses {member} from {spec}, which base does not {verb}"),
                    uses: Some(uses),
                });
            }
        }
    }
    out
}

/// Members every object has without defining them; base resolves them anyway.
const OBJECT_INHERITED: &[&str] = &[
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
    "__proto__",
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
];

/// Members every class has as a function.
const FUNCTION_INHERITED: &[&str] = &[
    "apply",
    "arguments",
    "bind",
    "call",
    "caller",
    "length",
    "name",
    "prototype",
];

/// Whether the test file mocks the module with `vi.mock` or `vi.doMock`, which replaces its
/// members with whatever the factory returns.
fn is_mocked(root: Node, source: &str, spec: &str, path: &str, dir: &str, trees: &Trees) -> bool {
    descendants(root).into_iter().any(|n| {
        if n.kind() != "call_expression" {
            return false;
        }
        let Some(callee) = n.child_by_field_name("function") else {
            return false;
        };
        let is_mock = callee.kind() == "member_expression"
            && callee
                .child_by_field_name("property")
                .is_some_and(|p| matches!(text(p, source), "mock" | "doMock"));
        if !is_mock {
            return false;
        }
        let first = n
            .child_by_field_name("arguments")
            .and_then(|a| children(a).into_iter().find(|c| c.kind() != "comment"));
        match first {
            Some(arg) if arg.kind() == "string" => {
                let target = crate::lang::ts::title(arg, source);
                target == spec || resolve_ts(&target, dir, trees).as_deref() == Some(path)
            }
            _ => true,
        }
    })
}

/// The lines in the test file that read each missing member from the binding `local`, keyed
/// by how the message names the member. `own` holds members of the value itself, read as
/// `local.m`, `local["m"]`, or `const { m } = local`; `instance` holds members of instances,
/// read as `new local().m` or through a `const` bound once to `new local()`. A binding that
/// the file rebinds, passes to a call other than `expect`, or assigns members on is left out,
/// since the test can then supply the member itself.
fn member_uses(
    root: Node,
    source: &str,
    local: &str,
    own: &HashSet<String>,
    instance: &HashSet<String>,
) -> Vec<(String, Vec<u32>)> {
    if bindings(root, source, local) > 0 || escapes(root, source, local) {
        return Vec::new();
    }
    let mut uses: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let receiver = |n: Option<Node>, name: &str| {
        n.map(unparen)
            .is_some_and(|n| n.kind() == "identifier" && text(n, source) == name)
    };
    let mut instances: Vec<(String, Node)> = Vec::new();
    for node in descendants(root) {
        match node.kind() {
            "member_expression" | "subscript_expression" => {
                if receiver(node.child_by_field_name("object"), local)
                    && let Some((member, line)) = accessed(node, source)
                    && own.contains(&member)
                {
                    uses.entry(format!("{local}.{member}"))
                        .or_default()
                        .push(line);
                }
            }
            "variable_declarator" => {
                let (Some(name), Some(value)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("value"),
                ) else {
                    continue;
                };
                if name.kind() == "object_pattern" && receiver(Some(value), local) {
                    for key in pattern_keys(name, source) {
                        if own.contains(&key) {
                            uses.entry(format!("{local}.{key}"))
                                .or_default()
                                .push(line_of(node));
                        }
                    }
                }
            }
            "new_expression" if receiver(node.child_by_field_name("constructor"), local) => {
                let mut up = node;
                while let Some(p) = up.parent()
                    && p.kind() == "parenthesized_expression"
                {
                    up = p;
                }
                let Some(parent) = up.parent() else {
                    continue;
                };
                match parent.kind() {
                    "member_expression" | "subscript_expression"
                        if parent.child_by_field_name("object") == Some(up) =>
                    {
                        if let Some((member, line)) = accessed(parent, source)
                            && instance.contains(&member)
                        {
                            uses.entry(format!("{local}.prototype.{member}"))
                                .or_default()
                                .push(line);
                        }
                    }
                    "variable_declarator" => {
                        if let Some(name) = parent.child_by_field_name("name")
                            && name.kind() == "identifier"
                            && parent.parent().is_some_and(|d| {
                                d.kind() == "lexical_declaration"
                                    && text(d, source).starts_with("const")
                            })
                            && let Some(scope) = ancestor(parent, &["statement_block", "program"])
                        {
                            instances.push((text(name, source).to_string(), scope));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for (name, scope) in instances {
        if bindings(root, source, &name) != 1 || escapes(root, source, &name) {
            continue;
        }
        for node in descendants(scope) {
            if matches!(node.kind(), "member_expression" | "subscript_expression")
                && receiver(node.child_by_field_name("object"), &name)
                && let Some((member, line)) = accessed(node, source)
                && instance.contains(&member)
            {
                uses.entry(format!("{local}.prototype.{member}"))
                    .or_default()
                    .push(line);
            }
        }
    }
    uses.into_iter().collect()
}

fn unparen(mut node: Node) -> Node {
    while node.kind() == "parenthesized_expression" {
        match children(node).into_iter().find(|c| c.kind() != "comment") {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// How many times the file binds `name` outside its imports: declarations, parameters,
/// catch parameters, function and class names, and assignments.
fn bindings(root: Node, source: &str, name: &str) -> usize {
    descendants(root)
        .into_iter()
        .filter(|n| {
            matches!(
                n.kind(),
                "identifier" | "shorthand_property_identifier_pattern" | "type_identifier"
            ) && text(*n, source) == name
                && ancestor(*n, &["import_statement"]).is_none()
                && is_binding(*n)
        })
        .count()
}

fn is_binding(node: Node) -> bool {
    if node.kind() == "shorthand_property_identifier_pattern" {
        return true;
    }
    let mut cur = node;
    while let Some(p) = cur.parent() {
        let field = |f: &str| p.child_by_field_name(f) == Some(cur);
        match p.kind() {
            "array_pattern" | "object_pattern" | "rest_pattern" => {}
            "pair_pattern" if field("value") => {}
            "assignment_pattern" | "object_assignment_pattern" if field("left") => {}
            "variable_declarator" => return field("name"),
            "required_parameter" | "optional_parameter" => return field("pattern"),
            "arrow_function" => return field("parameter"),
            "catch_clause" => return field("parameter"),
            "assignment_expression" | "augmented_assignment_expression" => return field("left"),
            "for_in_statement" => return field("left"),
            "function_declaration"
            | "function_expression"
            | "generator_function_declaration"
            | "class_declaration"
            | "class"
            | "abstract_class_declaration" => {
                return field("name");
            }
            _ => return false,
        }
        cur = p;
    }
    false
}

/// Whether the file passes `name` to a call other than `expect`, aliases it, or assigns
/// members on it.
fn escapes(root: Node, source: &str, name: &str) -> bool {
    descendants(root).into_iter().any(|n| {
        if n.kind() == "shorthand_property_identifier" && text(n, source) == name {
            return true;
        }
        if n.kind() != "identifier" || text(n, source) != name {
            return false;
        }
        if ancestor(n, &["import_statement"]).is_some() {
            return false;
        }
        let (up, parent) = outer(n);
        let Some(parent) = parent else {
            return false;
        };
        match parent.kind() {
            "arguments" => !parent
                .parent()
                .and_then(|c| c.child_by_field_name("function"))
                .is_some_and(|f| is_expect(text(f, source))),
            "member_expression" | "subscript_expression"
                if parent.child_by_field_name("object") == Some(up) =>
            {
                is_assigned(parent)
            }
            kind => aliases(parent, kind, up),
        }
    })
}

/// `expect(...)` and its matchers only read the values they get.
fn is_expect(callee: &str) -> bool {
    callee == "expect" || callee.starts_with("expect(") || callee.starts_with("expect.")
}

/// The node with its enclosing parentheses, and the parent outside them.
fn outer(node: Node) -> (Node, Option<Node>) {
    let mut up = node;
    while let Some(p) = up.parent()
        && p.kind() == "parenthesized_expression"
    {
        up = p;
    }
    (up, up.parent())
}

/// Whether `parent` hands the value `up` on to a place where other code can add members.
fn aliases(parent: Node, kind: &str, up: Node) -> bool {
    match kind {
        "spread_element" | "array" | "return_statement" => true,
        "pair" => parent.child_by_field_name("value") == Some(up),
        "variable_declarator" => {
            parent.child_by_field_name("value") == Some(up)
                && parent
                    .child_by_field_name("name")
                    .is_some_and(|n| n.kind() != "object_pattern")
        }
        "arrow_function" => parent.child_by_field_name("body") == Some(up),
        "assignment_expression" | "augmented_assignment_expression" => true,
        _ => false,
    }
}

/// The member a `member_expression` or `subscript_expression` reads, when it is a fixed
/// name, and the line it is on.
fn accessed(node: Node, source: &str) -> Option<(String, u32)> {
    if node.kind() == "member_expression" {
        let prop = node.child_by_field_name("property")?;
        (prop.kind() == "property_identifier")
            .then(|| (text(prop, source).to_string(), line_of(prop)))
    } else {
        let index = node.child_by_field_name("index")?;
        (index.kind() == "string" && !text(index, source).contains('\\'))
            .then(|| (crate::lang::ts::title(index, source), line_of(index)))
    }
}

fn pattern_keys(pattern: Node, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in children(pattern) {
        match part.kind() {
            "shorthand_property_identifier_pattern" => out.push(text(part, source).to_string()),
            "pair_pattern" | "object_assignment_pattern" => {
                let key = part
                    .child_by_field_name("key")
                    .or_else(|| part.child_by_field_name("left"));
                if let Some(k) = key
                    && matches!(
                        k.kind(),
                        "property_identifier" | "shorthand_property_identifier_pattern"
                    )
                {
                    out.push(text(k, source).to_string());
                }
            }
            _ => {}
        }
    }
    out
}

fn ancestor<'t>(node: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if kinds.contains(&n.kind()) {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

fn descendants(node: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        out.push(n);
        stack.extend(children(n).into_iter().rev());
    }
    out
}

fn has_token(node: Node, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == token)
}

/// The members of an exported object literal or class. A class's static members are `own`;
/// its prototype and instance members are `instance`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Members {
    class: bool,
    own: HashSet<String>,
    instance: HashSet<String>,
}

/// The members of each exported object literal or class whose full set of members the
/// module shows. A value that is not an object or class, a `let` or `var` binding, a class
/// that extends another or has decorators, and a value with spread, computed, `__proto__`, or
/// index members are left out, as is a value the module passes to a call, aliases, or assigns
/// a computed member on, since any of those can add members that the source does not name.
fn ts_members(source: &str) -> HashMap<String, Members> {
    let mut out = HashMap::new();
    let Some(tree) = parse_ts(source) else {
        return out;
    };
    let root = tree.root_node();
    let mut locals: HashMap<String, Node> = HashMap::new();
    // (export name, local binding or None for an inline value, inline value)
    let mut exported: Vec<(String, Option<String>, Option<Node>)> = Vec::new();
    for node in children(root) {
        let is_export = node.kind() == "export_statement";
        let decl = if is_export {
            node.child_by_field_name("declaration")
        } else {
            Some(node)
        };
        let is_default = is_export && has_token(node, "default");
        let decorated = is_export && children(node).iter().any(|c| c.kind() == "decorator");
        if let Some(decl) = decl {
            match decl.kind() {
                "lexical_declaration" if text(decl, source).starts_with("const") => {
                    for d in children(decl) {
                        if let (Some(n), Some(v)) = (
                            d.child_by_field_name("name"),
                            d.child_by_field_name("value"),
                        ) && n.kind() == "identifier"
                        {
                            let name = text(n, source).to_string();
                            locals.insert(name.clone(), v);
                            if is_export {
                                exported.push((name.clone(), Some(name), None));
                            }
                        }
                    }
                }
                "class_declaration" | "abstract_class_declaration" | "class" if !decorated => {
                    let name = decl
                        .child_by_field_name("name")
                        .map(|n| text(n, source).to_string());
                    if let Some(name) = &name {
                        locals.insert(name.clone(), decl);
                    }
                    if is_default {
                        exported.push(("default".to_string(), name, Some(decl)));
                    } else if is_export && let Some(name) = name {
                        exported.push((name.clone(), Some(name), None));
                    }
                }
                _ => {}
            }
        }
        if !is_export || node.child_by_field_name("source").is_some() {
            continue;
        }
        if let Some(value) = node.child_by_field_name("value") {
            if value.kind() == "identifier" {
                let name = text(value, source).to_string();
                exported.push(("default".to_string(), Some(name), None));
            } else {
                exported.push(("default".to_string(), None, Some(value)));
            }
        }
        for clause in children(node) {
            if clause.kind() != "export_clause" {
                continue;
            }
            for spec in children(clause) {
                let Some(name) = spec.child_by_field_name("name") else {
                    continue;
                };
                let local = text(name, source).to_string();
                let alias = spec
                    .child_by_field_name("alias")
                    .map(|a| crate::lang::ts::title(a, source))
                    .unwrap_or_else(|| local.clone());
                exported.push((alias, Some(local), None));
            }
        }
    }
    for (name, local, inline) in exported {
        let Some(value) = inline.or_else(|| local.as_ref().and_then(|l| locals.get(l).copied()))
        else {
            continue;
        };
        let Some(mut members) = declared_members(value, source) else {
            continue;
        };
        if !added_by_this(value, source, &mut members) {
            continue;
        }
        if let Some(local) = &local
            && !added_members(root, source, local, &mut members)
        {
            continue;
        }
        out.insert(name, members);
    }
    out
}

/// The members an object literal or class body names, or None when it is neither or when
/// members can come from somewhere the body does not name.
fn declared_members(value: Node, source: &str) -> Option<Members> {
    let mut value = value;
    while matches!(
        value.kind(),
        "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression"
    ) {
        value = children(value)
            .into_iter()
            .find(|c| c.kind() != "comment")?;
    }
    let mut out = Members::default();
    match value.kind() {
        "object" => {
            for part in children(value) {
                let key = match part.kind() {
                    "pair" => part.child_by_field_name("key")?,
                    "method_definition" => part.child_by_field_name("name")?,
                    "shorthand_property_identifier" => part,
                    "comment" => continue,
                    _ => return None,
                };
                let name = member_name(key, source)?;
                if name == "__proto__" {
                    return None;
                }
                out.own.insert(name);
            }
        }
        "class" | "class_declaration" | "abstract_class_declaration" => {
            out.class = true;
            let heritage = children(value)
                .into_iter()
                .filter(|c| c.kind() == "class_heritage");
            if heritage
                .flat_map(children)
                .any(|c| c.kind() == "extends_clause")
                || children(value).iter().any(|c| c.kind() == "decorator")
            {
                return None;
            }
            let body = value.child_by_field_name("body")?;
            if descendants(body).iter().any(|c| c.kind() == "decorator") {
                return None;
            }
            for part in children(body) {
                match part.kind() {
                    "method_definition" | "public_field_definition" => {
                        if has_token(part, "declare") {
                            continue;
                        }
                        let key = part.child_by_field_name("name")?;
                        if key.kind() == "private_property_identifier" {
                            continue;
                        }
                        let name = member_name(key, source)?;
                        if has_token(part, "static") {
                            out.own.insert(name);
                        } else if name == "constructor" && part.kind() == "method_definition" {
                            out.instance.extend(parameter_properties(part, source));
                        } else {
                            out.instance.insert(name);
                        }
                    }
                    "index_signature" => return None,
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

/// A property key as the runtime spells it, or None when this code cannot be sure of the
/// spelling: an escape sequence, a computed key, or a number that is not a plain integer.
fn member_name(key: Node, source: &str) -> Option<String> {
    match key.kind() {
        "property_identifier" | "shorthand_property_identifier" => {
            let raw = text(key, source);
            (!raw.contains('\\')).then(|| raw.to_string())
        }
        "string" => {
            let raw = text(key, source);
            (!raw.contains('\\')).then(|| crate::lang::ts::title(key, source))
        }
        "number" => canonical_integer(text(key, source)),
        _ => None,
    }
}

fn canonical_integer(raw: &str) -> Option<String> {
    let raw = raw.replace('_', "");
    let lower = raw.to_ascii_lowercase();
    let radix = [("0x", 16), ("0o", 8), ("0b", 2)]
        .into_iter()
        .find_map(|(p, r)| lower.strip_prefix(p).map(|d| (d.to_string(), r)));
    let value: f64 = match radix {
        Some((digits, r)) => u64::from_str_radix(&digits, r).ok()? as f64,
        None => lower.parse().ok()?,
    };
    (value.is_finite() && value.fract() == 0.0 && value < 1e15).then(|| format!("{}", value as u64))
}

/// `constructor(private x: T)` and `readonly x` parameters, which define instance members.
fn parameter_properties(constructor: Node, source: &str) -> Vec<String> {
    let Some(params) = constructor.child_by_field_name("parameters") else {
        return Vec::new();
    };
    children(params)
        .into_iter()
        .filter(|p| {
            children(*p)
                .iter()
                .any(|c| matches!(c.kind(), "accessibility_modifier" | "override_modifier"))
                || text(*p, source).starts_with("readonly ")
        })
        .filter_map(|p| p.child_by_field_name("pattern"))
        .filter(|p| p.kind() == "identifier")
        .map(|p| text(p, source).to_string())
        .collect()
}

/// Adds the members that `this.m = ...` assigns inside the value: a class's static methods
/// and static blocks add to `own`, its other methods and fields to `instance`, and a nested
/// `function` whose `this` is unknown to both. Returns false when `this` escapes or gets a
/// computed member.
fn added_by_this(value: Node, source: &str, members: &mut Members) -> bool {
    for node in descendants(value) {
        if node.kind() != "this" {
            continue;
        }
        let (up, Some(parent)) = outer(node) else {
            continue;
        };
        let target = match parent.kind() {
            "member_expression" | "subscript_expression"
                if parent.child_by_field_name("object") == Some(up) =>
            {
                parent
            }
            "arguments" => return false,
            kind if aliases(parent, kind, up) => return false,
            _ => continue,
        };
        if !is_assigned(target) {
            continue;
        }
        let Some((member, _)) = accessed(target, source) else {
            return false;
        };
        let holder = ancestor(
            node,
            &[
                "method_definition",
                "public_field_definition",
                "class_static_block",
                "function_declaration",
                "function_expression",
                "generator_function",
                "generator_function_declaration",
            ],
        );
        match holder {
            _ if !members.class => {
                members.own.insert(member);
            }
            Some(h) if h.kind() == "class_static_block" || has_token(h, "static") => {
                members.own.insert(member);
            }
            Some(h) if matches!(h.kind(), "method_definition" | "public_field_definition") => {
                members.instance.insert(member);
            }
            _ => {
                members.own.insert(member.clone());
                members.instance.insert(member);
            }
        }
    }
    true
}

/// Adds the members that `owner.m = ...` and `owner.prototype.m = ...` assign anywhere in the
/// module. Returns false when `owner` or `owner.prototype` escapes where members can be added
/// unseen: as a call argument, an alias, a reassignment, or a computed-member target.
fn added_members(scope: Node, source: &str, owner: &str, members: &mut Members) -> bool {
    for node in descendants(scope) {
        if node.kind() == "shorthand_property_identifier" && text(node, source) == owner {
            return false;
        }
        if node.kind() != "identifier" || text(node, source) != owner {
            continue;
        }
        let (up, Some(parent)) = outer(node) else {
            continue;
        };
        let target = match parent.kind() {
            "member_expression" | "subscript_expression"
                if parent.child_by_field_name("object") == Some(up) =>
            {
                parent
            }
            "arguments" => return false,
            kind if aliases(parent, kind, up) => return false,
            _ => continue,
        };
        let mut instance = false;
        let mut target = target;
        if accessed(target, source).is_some_and(|(m, _)| m == "prototype") {
            let (pup, pparent) = outer(target);
            match pparent {
                Some(p)
                    if matches!(p.kind(), "member_expression" | "subscript_expression")
                        && p.child_by_field_name("object") == Some(pup) =>
                {
                    target = p;
                    instance = true;
                }
                Some(p) if p.kind() == "arguments" || aliases(p, p.kind(), pup) => return false,
                _ => continue,
            }
        }
        if !is_assigned(target) {
            continue;
        }
        let Some((member, _)) = accessed(target, source) else {
            return false;
        };
        if instance {
            members.instance.insert(member);
        } else {
            members.own.insert(member);
        }
    }
    true
}

fn is_assigned(target: Node) -> bool {
    let (up, parent) = outer(target);
    parent.is_some_and(|p| {
        matches!(
            p.kind(),
            "assignment_expression" | "augmented_assignment_expression"
        ) && p.child_by_field_name("left") == Some(up)
    })
}

fn parse_ts(source: &str) -> Option<tree_sitter::Tree> {
    let lang = if source.contains("</") || source.contains("/>") {
        Lang::Tsx
    } else {
        Lang::TypeScript
    };
    lang.parse(source)
}

fn is_type_only(node: Node, source: &str) -> bool {
    let header = text(node, source);
    header.starts_with("import type ") || header.starts_with("import typeof ")
}

/// (imported name, local binding) pairs; the default import is named `default`, and a
/// namespace import has no imported name.
fn ts_bindings(node: Node, source: &str) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    for child in children(node) {
        if child.kind() != "import_clause" {
            continue;
        }
        for part in children(child) {
            match part.kind() {
                "identifier" => {
                    out.push((Some("default".to_string()), text(part, source).to_string()))
                }
                "namespace_import" => {
                    if let Some(n) = children(part)
                        .into_iter()
                        .find(|n| n.kind() == "identifier")
                    {
                        out.push((None, text(n, source).to_string()));
                    }
                }
                "named_imports" => {
                    for spec in children(part) {
                        if spec.kind() != "import_specifier"
                            || text(spec, source).starts_with("type ")
                        {
                            continue;
                        }
                        let Some(name) = spec.child_by_field_name("name") else {
                            continue;
                        };
                        let name = crate::lang::ts::title(name, source);
                        let local = spec
                            .child_by_field_name("alias")
                            .map(|a| text(a, source).to_string())
                            .unwrap_or_else(|| name.clone());
                        out.push((Some(name), local));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

struct TsExports {
    names: HashSet<String>,
    /// Exported names that are only types, which a namespace object does not hold.
    types: HashSet<String>,
    /// `export * from`: absent names may still be exported.
    star: bool,
}

fn ts_exports(source: &str) -> TsExports {
    let mut names = HashSet::new();
    let mut types = HashSet::new();
    let mut star = false;
    let Some(tree) = parse_ts(source) else {
        return TsExports {
            names,
            types,
            star: true,
        };
    };
    for node in children(tree.root_node()) {
        if node.kind() != "export_statement" {
            continue;
        }
        if has_token(node, "default") {
            names.insert("default".to_string());
        }
        let type_only = has_token(node, "type");
        if let Some(decl) = node.child_by_field_name("declaration") {
            match decl.kind() {
                "lexical_declaration" | "variable_declaration" => {
                    for d in children(decl) {
                        if let Some(n) = d.child_by_field_name("name") {
                            let mut found = HashSet::new();
                            collect_ts_identifiers(n, source, &mut found);
                            names.extend(found);
                        }
                    }
                }
                kind => {
                    if let Some(n) = decl.child_by_field_name("name") {
                        let name = text(n, source).to_string();
                        if matches!(
                            kind,
                            "interface_declaration"
                                | "type_alias_declaration"
                                | "ambient_declaration"
                        ) {
                            types.insert(name.clone());
                        }
                        names.insert(name);
                    }
                }
            }
        }
        let mut clause_found = false;
        for child in children(node) {
            if child.kind() == "export_clause" {
                clause_found = true;
                for spec in children(child) {
                    if spec.kind() != "export_specifier" {
                        continue;
                    }
                    let exported = spec
                        .child_by_field_name("alias")
                        .or_else(|| spec.child_by_field_name("name"))
                        .map(|n| crate::lang::ts::title(n, source));
                    if let Some(name) = exported {
                        if type_only || text(spec, source).starts_with("type ") {
                            types.insert(name.clone());
                        }
                        names.insert(name);
                    }
                }
            } else if child.kind() == "namespace_export" {
                clause_found = true;
                if let Some(n) = children(child).into_iter().next() {
                    names.insert(text(n, source).to_string());
                }
            }
        }
        if !clause_found && has_token(node, "*") {
            star = true;
        }
    }
    TsExports { names, types, star }
}

fn collect_ts_identifiers(node: Node, source: &str, out: &mut HashSet<String>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            out.insert(text(node, source).to_string());
        }
        _ => {
            for child in children(node) {
                collect_ts_identifiers(child, source, out);
            }
        }
    }
}

fn resolve_ts(spec: &str, dir: &str, trees: &Trees) -> Option<String> {
    let mut bases: Vec<String> = Vec::new();
    if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
        bases.push(normalize(&join(dir, spec)));
    } else {
        for (prefix, targets) in trees.aliases {
            if let Some(rest) = spec.strip_prefix(prefix.as_str()) {
                for target in targets {
                    bases.push(normalize(&format!("{target}{rest}")));
                }
            }
        }
    }
    for base in bases {
        let stem = TS_EXTENSIONS
            .iter()
            .find_map(|ext| base.strip_suffix(&format!(".{ext}")))
            .unwrap_or(&base)
            .to_string();
        let mut candidates = vec![base.clone()];
        for ext in TS_EXTENSIONS {
            candidates.push(format!("{stem}.{ext}"));
        }
        for ext in TS_EXTENSIONS {
            candidates.push(format!("{base}/index.{ext}"));
        }
        if let Some(found) = candidates.into_iter().find(|c| trees.head.contains(c)) {
            return Some(found);
        }
    }
    None
}

static JSONC_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)("(?:[^"\\]|\\.)*")|//[^\n]*|/\*.*?\*/"#).unwrap());
static TRAILING_COMMA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r",(\s*[}\]])").unwrap());

/// tsconfig `compilerOptions.paths` from the runner's `tsconfig.json` and
/// `tsconfig.app.json`, as repo-relative (prefix, dirs) pairs. Only `prefix/*` patterns.
pub fn ts_aliases(cwd: &str, read: &dyn Fn(&str) -> Option<String>) -> Vec<(String, Vec<String>)> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in ["tsconfig.json", "tsconfig.app.json"] {
        let path = join(if cwd == "." { "" } else { cwd }, name);
        let Some(raw) = read(&path) else { continue };
        let stripped = JSONC_COMMENT.replace_all(&raw, |c: &regex::Captures| {
            c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default()
        });
        let cleaned = TRAILING_COMMA.replace_all(&stripped, "$1");
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&cleaned) else {
            continue;
        };
        let options = &json["compilerOptions"];
        let base_url = options["baseUrl"].as_str().unwrap_or(".");
        let base = normalize(&join(parent(&path).as_str(), base_url));
        let Some(paths) = options["paths"].as_object() else {
            continue;
        };
        for (pattern, targets) in paths {
            let Some(prefix) = pattern.strip_suffix('*') else {
                continue;
            };
            let dirs = targets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str()?.strip_suffix('*'))
                .map(|t| normalize(&join(&base, t)))
                .map(|t| {
                    if t.is_empty() || t.ends_with('/') {
                        t
                    } else {
                        format!("{t}/")
                    }
                })
                .collect();
            out.insert(prefix.to_string(), dirs);
        }
    }
    out.into_iter().collect()
}

// ---------------------------------------------------------------------------------------
// Paths

fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() || dir == "." {
        rest.to_string()
    } else if rest.is_empty() {
        dir.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// Resolves `.` and `..` segments. Keeps a trailing slash.
fn normalize(path: &str) -> String {
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    if trailing && !joined.is_empty() {
        format!("{joined}/")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn set(files: &[&str]) -> HashSet<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    fn runner(toml: &str) -> RunnerConfig {
        Config::parse(toml).unwrap().runners.remove(0)
    }

    #[test]
    fn python_flags_new_modules_and_new_names() {
        let head_files = set(&[
            "api/app/__init__.py",
            "api/app/core.py",
            "api/app/fresh.py",
            "api/tests/test_x.py",
        ]);
        let base_files = set(&[
            "api/app/__init__.py",
            "api/app/core.py",
            "api/tests/test_x.py",
        ]);
        let read_head = |p: &str| match p {
            "api/app/core.py" => Some("def old(): pass\ndef new(): pass\nCONST = 1\n".to_string()),
            _ => Some(String::new()),
        };
        let read_base = |p: &str| match p {
            "api/app/core.py" => Some("def old(): pass\n".to_string()),
            _ => Some(String::new()),
        };
        let trees = Trees {
            head: &head_files,
            base: &base_files,
            read_head: &read_head,
            read_base: &read_base,
            aliases: &[],
        };
        let r = runner(
            "[[runner]]\nkind = \"unittest\"\ncwd = \"api\"\nstart_dir = \"tests\"\ncommand = [\"python\", \"-m\", \"unittest\"]\n",
        );
        let source = "import os\nfrom app.core import old, new as renamed, CONST\nfrom app import fresh\nimport app.fresh\nfrom requests import get\n";

        let missing = missing_imports(&r, "api/tests/test_x.py", source, &trees);

        let summary: Vec<(Option<String>, String)> = missing
            .iter()
            .map(|m| (m.local.clone(), m.module.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (Some("new".into()), "app.core".into()),
                (Some("CONST".into()), "app.core".into()),
                (Some("fresh".into()), "app".into()),
                (None, "app.fresh".into()),
            ]
        );
    }

    #[test]
    fn typescript_flags_new_exports_modules_and_defaults_but_not_types() {
        let head_files = set(&[
            "web/src/lib/a.ts",
            "web/src/lib/stored.ts",
            "web/src/a.test.ts",
            "web/src/Page.tsx",
        ]);
        let base_files = set(&["web/src/lib/a.ts", "web/src/a.test.ts", "web/src/Page.tsx"]);
        let read_head = |p: &str| {
            match p {
            "web/src/lib/a.ts" => Some("export function old() {}\nexport const fresh = 1, other = 2;\nexport type T = number;\nexport { helper as aliased };\n".to_string()),
            "web/src/Page.tsx" => Some("export default function Page() { return <div />; }\n".to_string()),
            _ => Some(String::new()),
        }
        };
        let read_base = |p: &str| match p {
            "web/src/lib/a.ts" => {
                Some("export function old() {}\nexport const other = 2;\n".to_string())
            }
            "web/src/Page.tsx" => Some("export function Page() { return <div />; }\n".to_string()),
            _ => Some(String::new()),
        };
        let aliases = vec![("@/".to_string(), vec!["web/src/".to_string()])];
        let trees = Trees {
            head: &head_files,
            base: &base_files,
            read_head: &read_head,
            read_base: &read_base,
            aliases: &aliases,
        };
        let r = runner("[[runner]]\nkind = \"vitest\"\ncwd = \"web\"\ncommand = [\"vitest\"]\n");
        let source = "import { old, fresh, type T, aliased as a } from './lib/a';\nimport type { Q } from './lib/a';\nimport { readStored } from '@/lib/stored';\nimport Page from './Page';\nimport { x } from 'vitest';\n";

        let missing = missing_imports(&r, "web/src/a.test.ts", source, &trees);

        let summary: Vec<(Option<String>, String)> = missing
            .iter()
            .map(|m| (m.local.clone(), m.module.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (Some("fresh".into()), "./lib/a".into()),
                (Some("a".into()), "./lib/a".into()),
                (None, "@/lib/stored".into()),
                (Some("Page".into()), "./Page".into()),
            ]
        );
    }

    #[test]
    fn affecting_follows_references_in_typescript() {
        let source = "import { fresh } from './a';\n\nit('uses', () => {\n  fresh();\n});\nit('ignores', () => {\n  other();\n});\n";
        let missing = vec![MissingImport {
            local: Some("fresh".into()),
            module: "./a".into(),
            line: 1,
            message: "m".into(),
            uses: None,
        }];
        let spans = [(3, 5), (6, 8)];
        assert!(affecting(Lang::TypeScript, source, &missing, (3, 5), &spans).is_some());
        assert!(affecting(Lang::TypeScript, source, &missing, (6, 8), &spans).is_none());
        let helper = format!("{source}function h() {{ return fresh; }}\n");
        assert!(affecting(Lang::TypeScript, &helper, &missing, (6, 8), &spans).is_some());
    }

    fn members(source: &str) -> Vec<(String, bool, Vec<String>, Vec<String>)> {
        let sorted = |set: HashSet<String>| {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            v
        };
        let mut out: Vec<_> = ts_members(source)
            .into_iter()
            .map(|(k, m)| (k, m.class, sorted(m.own), sorted(m.instance)))
            .collect();
        out.sort();
        out
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_members_of_exported_objects_and_classes() {
        let source = r#"
const inner = { a: 1, "b-c": 2, 0x10: 3, d, e() { this.x = 1; }, get f() { return 1; } };
export { inner as renamed };
export const obj = { g: 1 } as const;
export class C implements Shape {
  static s() { this.y = 1; }
  static { this.z = 1; }
  field = 1;
  declare ghost: number;
  #hidden = 2;
  constructor(private p: number, readonly q: number, r: number) {
    this.t = r;
  }
  m() {}
}
C.u = 1;
C.prototype.v = function () {};
export abstract class A { static make() {} abstract run(): void; }
export /* comment */ default { w: 1 };
"#;
        assert_eq!(
            members(source),
            vec![
                ("A".into(), true, names(&["make"]), names(&[])),
                (
                    "C".into(),
                    true,
                    names(&["s", "u", "y", "z"]),
                    names(&["field", "m", "p", "q", "t", "v"])
                ),
                ("default".into(), false, names(&["w"]), names(&[])),
                ("obj".into(), false, names(&["g"]), names(&[])),
                (
                    "renamed".into(),
                    false,
                    names(&["16", "a", "b-c", "d", "e", "f", "x"]),
                    names(&[])
                ),
            ]
        );
    }

    #[test]
    fn leaves_out_values_whose_members_can_come_from_elsewhere() {
        let source = r#"
const extra = { a: 1 };
export const spread = { ...extra, b: 1 };
const key = "k";
export const computed = { [key]: 1 };
export const proto = { __proto__: extra, b: 1 };
export const escaped = { "\u006e"() {} };
export const float = { 1.5: 1 };
class Base {}
export class Child extends Base {}
@sealed export class Decorated {}
export let mutable = { a: 1 };
export const made = make();
export const assigned = { a: 1 };
Object.assign((assigned), extra);
export const aliased = { a: 1 };
const alias = aliased;
export const listed = { a: 1 };
const all = { listed };
export const indexed = { a: 1 };
indexed[key] = 2;
export class Defined { m() {} }
Object.defineProperty(Defined.prototype, "n", { value: 1 });
export class Indexed { [k: string]: unknown; }
export class SelfIndexed { constructor() { this[key] = 1; } }
export class SelfAssigned { constructor() { Object.assign(this, extra); } }
export function f() {}
export { other } from "./other";
"#;
        assert_eq!(members(source), vec![]);
    }

    fn api_trees<'a>(
        files: &'a HashSet<String>,
        read_head: &'a dyn Fn(&str) -> Option<String>,
        read_base: &'a dyn Fn(&str) -> Option<String>,
    ) -> Trees<'a> {
        Trees {
            head: files,
            base: files,
            read_head,
            read_base,
            aliases: &[],
        }
    }

    fn member_messages(head: &str, base: &str, test: &str) -> Vec<(String, Option<Vec<u32>>)> {
        let files = set(&["web/api.ts", "web/api.test.ts"]);
        let (head, base) = (head.to_string(), base.to_string());
        let read_head = move |p: &str| (p == "web/api.ts").then(|| head.clone());
        let read_base = move |p: &str| (p == "web/api.ts").then(|| base.clone());
        let trees = api_trees(&files, &read_head, &read_base);
        let r = runner("[[runner]]\nkind = \"vitest\"\ncwd = \"web\"\ncommand = [\"vitest\"]\n");
        missing_imports(&r, "web/api.test.ts", test, &trees)
            .into_iter()
            .map(|m| (m.message, m.uses))
            .collect()
    }

    #[test]
    fn typescript_flags_members_that_only_head_defines() {
        let head = "export const api = { get() {}, post() {}, toString() {}, call() {} };\nexport const open = { ...x, a: 1 };\nexport class S { static make() {} m() {} n() {} }\nexport function ping() {}\nexport interface Shape {}\n";
        let base = "export const api = { get() {} };\nexport const open = { ...x };\nexport class S { m() {} }\n";
        let test = r#"import { api, open, S } from "./api";
import * as mod from "./api";
it("a", () => {
  api.get();
  (api).post();
  api["post"];
  api.toString();
  api.call();
  open.a;
  const { post } = api;
  new S().n();
  const s = new S();
  s.n();
  S.make();
  S.n;
  mod.ping();
  expect(mod.Shape).toBeUndefined();
  expect(s).toBeInstanceOf(S);
});
"#;
        assert_eq!(
            member_messages(head, base, test),
            vec![
                (
                    "uses api.call from ./api, which base does not define".into(),
                    Some(vec![8])
                ),
                (
                    "uses api.post from ./api, which base does not define".into(),
                    Some(vec![5, 6, 10])
                ),
                (
                    "uses S.make from ./api, which base does not define".into(),
                    Some(vec![14])
                ),
                (
                    "uses S.prototype.n from ./api, which base does not define".into(),
                    Some(vec![11, 13])
                ),
                (
                    "uses mod.ping from ./api, which base does not export".into(),
                    Some(vec![16])
                ),
            ]
        );
    }

    #[test]
    fn typescript_skips_members_the_test_file_can_supply_or_shadow() {
        let head = "export const api = { get() {}, n() {} };\nexport class C { n() {} }\n";
        let base = "export const api = { get() {} };\nexport class C {}\n";
        let cases = [
            "import { api } from './api';\nfunction check(api) { api.n(); }\n",
            "import { api } from './api';\nit('a', () => { const api = { n() {} }; api.n(); });\n",
            "import { api } from './api';\napi.n = () => 1;\nit('a', () => { api.n(); });\n",
            "import { api } from './api';\nit('a', () => { vi.spyOn(api, 'n'); api.n(); });\n",
            "import { api } from './api';\nvi.mock('./api', () => ({ api: { n() {} } }));\nit('a', () => { api.n(); });\n",
            "import { C } from './api';\nit('a', () => { let c = new C(); c = { n() {} }; c.n(); });\n",
            "import { C } from './api';\nit('a', () => { const c = new C(); c.n = () => 1; c.n(); });\n",
        ];
        for test in cases {
            assert_eq!(member_messages(head, base, test), vec![], "{test}");
        }
    }

    #[test]
    fn typescript_keeps_existing_export_checks_beside_namespaces() {
        let head = "const f = () => 1;\nexport { f as \"*\" };\nexport function n() {}\n";
        let base = "export /* forwarding */ * from './common';\n";
        let test = "import { \"*\" as f } from './api';\nimport * as mod from './api';\nit('a', () => { f(); mod.n(); });\n";
        assert_eq!(member_messages(head, base, test), vec![]);
        let base = "export const other = 1;\n";
        assert_eq!(
            member_messages(head, base, test),
            vec![
                (
                    "imports * from ./api, which base does not export".into(),
                    None
                ),
                (
                    "uses mod.n from ./api, which base does not export".into(),
                    Some(vec![3])
                ),
            ]
        );
    }

    #[test]
    fn reads_tsconfig_path_aliases() {
        let read = |p: &str| {
            match p {
            "web/tsconfig.app.json" => Some("{\n  // comment\n  \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"./src/*\"], }, },\n}".to_string()),
            _ => None,
        }
        };
        assert_eq!(
            ts_aliases("web", &read),
            vec![("@/".to_string(), vec!["web/src/".to_string()])]
        );
    }
}
