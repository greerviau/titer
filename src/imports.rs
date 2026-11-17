//! The static import check: does a changed test import a module or name that exists only in
//! head? Such a test cannot prove anything on base. vitest does not type-check, so a missing
//! export arrives as `undefined` and can fail an assertion that proves nothing; this check
//! runs before the base run's result is read.

use std::collections::{BTreeMap, HashSet};
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
            });
            continue;
        }
        let (Some(head), Some(base)) = ((trees.read_head)(&path), (trees.read_base)(&path)) else {
            continue;
        };
        let head_exports = ts_exports(&head);
        let base_exports = ts_exports(&base);
        if base_exports.star {
            continue;
        }
        for (imported, local) in ts_bindings(node, source) {
            if head_exports.names.contains(&imported) && !base_exports.names.contains(&imported) {
                out.push(MissingImport {
                    local: Some(local),
                    module: spec.clone(),
                    line: line_of(node),
                    message: format!("imports {imported} from {spec}, which base does not export"),
                });
            }
        }
    }
    out
}

fn is_type_only(node: Node, source: &str) -> bool {
    let header = text(node, source);
    header.starts_with("import type ") || header.starts_with("import typeof ")
}

/// (imported name, local binding) pairs; the default import is named `default`.
fn ts_bindings(node: Node, source: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for child in children(node) {
        if child.kind() != "import_clause" {
            continue;
        }
        for part in children(child) {
            match part.kind() {
                "identifier" => out.push(("default".to_string(), text(part, source).to_string())),
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
                        out.push((name, local));
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
    /// `export * from`: absent names may still be exported.
    star: bool,
}

fn ts_exports(source: &str) -> TsExports {
    let mut names = HashSet::new();
    let mut star = false;
    let lang = if source.contains("</") || source.contains("/>") {
        Lang::Tsx
    } else {
        Lang::TypeScript
    };
    let Some(tree) = lang.parse(source) else {
        return TsExports { names, star: true };
    };
    for node in children(tree.root_node()) {
        if node.kind() != "export_statement" {
            continue;
        }
        let raw = text(node, source);
        if raw.starts_with("export default") {
            names.insert("default".to_string());
        }
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
                _ => {
                    if let Some(n) = decl.child_by_field_name("name") {
                        names.insert(text(n, source).to_string());
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
                    names.extend(exported);
                }
            } else if child.kind() == "namespace_export" {
                clause_found = true;
                if let Some(n) = children(child).into_iter().next() {
                    names.insert(text(n, source).to_string());
                }
            }
        }
        if !clause_found && raw.starts_with("export *") {
            star = true;
        }
    }
    TsExports { names, star }
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
        }];
        let spans = [(3, 5), (6, 8)];
        assert!(affecting(Lang::TypeScript, source, &missing, (3, 5), &spans).is_some());
        assert!(affecting(Lang::TypeScript, source, &missing, (6, 8), &spans).is_none());
        let helper = format!("{source}function h() {{ return fresh; }}\n");
        assert!(affecting(Lang::TypeScript, &helper, &missing, (6, 8), &spans).is_some());
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
