//! Which compiled targets hold a source file, and the file's module paths in each.
//!
//! Packages, targets, and workspace members come from `cargo metadata`, so titer agrees
//! with cargo about what exists. Modules come from the `mod` declarations reachable from
//! each target's root, including `#[path]` attributes. A file that several targets or
//! several declarations reach has a module path for each. A file reached only through
//! `#[cfg(test)]` modules is test code throughout.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use tree_sitter::Node;

use crate::lang::rust::{attribute_parts, attributes, is_test_item, name_of};
use crate::lang::{Lang, children};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Lib,
    Bin(String),
    Test(String),
    Example(String),
}

/// A compiled target of one package.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target {
    pub package: String,
    pub kind: Kind,
    /// The target's root file, relative to the runner's cwd.
    pub root: String,
}

impl Target {
    /// The cargo arguments that select this target and nothing else.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["-p".to_string(), self.package.clone()];
        match &self.kind {
            Kind::Lib => args.push("--lib".to_string()),
            Kind::Bin(name) => args.extend(["--bin".to_string(), name.clone()]),
            Kind::Test(name) => args.extend(["--test".to_string(), name.clone()]),
            Kind::Example(name) => args.extend(["--example".to_string(), name.clone()]),
        }
        args
    }
}

/// One way a file is compiled: as `module` of `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRef {
    pub target: Target,
    pub module: Vec<String>,
    /// The declaration path passes through a `#[cfg(test)]` or test-only module.
    pub test_only: bool,
}

/// The packages under one cwd, with each file's module references.
#[derive(Debug, Default)]
pub struct Index {
    /// Cwd-relative file to its references.
    files: HashMap<String, Vec<ModuleRef>>,
    /// Cwd-relative dirs of the workspace members' manifests.
    pub members: Vec<String>,
}

impl Index {
    pub fn refs(&self, rel: &str) -> &[ModuleRef] {
        self.files.get(rel).map_or(&[], Vec::as_slice)
    }
}

/// The index for `cwd`, built once per dir: worktrees do not change their module layout
/// between runs. `command` is the runner's command; its prefix before `test` runs cargo.
pub fn index(command: &[String], cwd: &Path) -> Arc<Index> {
    static CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<Index>>>> = LazyLock::new(Default::default);
    if let Some(index) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(cwd) {
        return index.clone();
    }
    let index = Arc::new(build(command, cwd));
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cwd.to_path_buf(), index.clone());
    index
}

fn build(command: &[String], cwd: &Path) -> Index {
    let mut index = Index::default();
    let Some(meta) = metadata(command, cwd) else {
        return index;
    };
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let rel = |path: &str| -> Option<String> {
        let path = Path::new(path);
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        path.strip_prefix(&cwd)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    };
    let members: Vec<&str> = meta["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str())
        .collect();
    let empty = Vec::new();
    for package in meta["packages"].as_array().unwrap_or(&empty) {
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        let Some(manifest) = package["manifest_path"].as_str() else {
            continue;
        };
        let Some(manifest_rel) = rel(manifest) else {
            continue;
        };
        let package_dir = Path::new(&manifest_rel)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if package["id"]
            .as_str()
            .is_some_and(|id| members.contains(&id))
        {
            index.members.push(package_dir.clone());
        }
        let no_harness = no_harness(&cwd.join(&manifest_rel));
        for target in package["targets"].as_array().unwrap_or(&empty) {
            if target["test"].as_bool() == Some(false) {
                continue;
            }
            let target_name = target["name"].as_str().unwrap_or_default().to_string();
            let kinds: Vec<&str> = target["kind"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| k.as_str())
                .collect();
            let kind = if kinds.iter().any(|k| {
                matches!(
                    *k,
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            }) {
                Kind::Lib
            } else if kinds.contains(&"bin") {
                Kind::Bin(target_name.clone())
            } else if kinds.contains(&"test") {
                Kind::Test(target_name.clone())
            } else if kinds.contains(&"example") {
                Kind::Example(target_name.clone())
            } else {
                continue;
            };
            if no_harness.contains(&target_name) && !matches!(kind, Kind::Lib) {
                continue;
            }
            let Some(root) = target["src_path"].as_str().and_then(rel) else {
                continue;
            };
            let target = Target {
                package: name.to_string(),
                kind,
                root: root.clone(),
            };
            let mut seen = Vec::new();
            walk_module(&cwd, &root, Vec::new(), true, false, &mut seen);
            for (file, module, test_only) in seen {
                let refs = index.files.entry(file).or_default();
                let r = ModuleRef {
                    target: target.clone(),
                    module,
                    test_only,
                };
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
    }
    index
}

/// `cargo metadata` for the workspace in `cwd`, run with the command's prefix before
/// `test`. It reads manifests only and writes nothing.
fn metadata(command: &[String], cwd: &Path) -> Option<serde_json::Value> {
    let at = command.iter().position(|a| a == "test")?;
    let mut cmd = std::process::Command::new(&command[0]);
    cmd.args(&command[1..at])
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(cwd);
    let out = crate::process::output(cmd).ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// The targets a manifest marks `harness = false`: titer cannot select or read their tests.
fn no_harness(manifest: &Path) -> Vec<String> {
    let Some(table) = std::fs::read_to_string(manifest)
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
    else {
        return Vec::new();
    };
    ["bin", "test", "example"]
        .iter()
        .flat_map(|section| table.get(*section).and_then(|s| s.as_array()).cloned())
        .flatten()
        .filter(|t| t.get("harness").and_then(|h| h.as_bool()) == Some(false))
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect()
}

static PATH_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r##"^path\s*=\s*(?:"((?:[^"\\]|\\.)*)"|r(#*)"(.*)"#*)$"##).unwrap()
});

fn path_attribute(whole: &str) -> Option<String> {
    let caps = PATH_ATTR.captures(whole.trim())?;
    caps.get(1)
        .or_else(|| caps.get(3))
        .map(|m| m.as_str().to_string())
}

/// Records `file`, a cwd-relative module file, and every module file it declares. A mod-rs
/// file (a root, `mod.rs`, or a `#[path]` file) declares children in its own dir; another
/// file `a.rs` declares them in `a/`. Each (file, module path) is visited once.
fn walk_module(
    cwd: &Path,
    file: &str,
    module: Vec<String>,
    mod_rs: bool,
    test_only: bool,
    seen: &mut Vec<(String, Vec<String>, bool)>,
) {
    if seen.iter().any(|(f, m, _)| f == file && *m == module) || seen.len() > 100_000 {
        return;
    }
    seen.push((file.to_string(), module.clone(), test_only));
    let Ok(source) = std::fs::read_to_string(cwd.join(file)) else {
        return;
    };
    let Some(tree) = Lang::Rust.parse(&source) else {
        return;
    };
    let parent = Path::new(file)
        .parent()
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let children_dir = if mod_rs {
        parent.clone()
    } else {
        parent.join(Path::new(file).file_stem().unwrap_or_default())
    };
    let scope = Scope {
        children_dir: &children_dir,
        path_base: &parent,
        module: &module,
        test_only,
    };
    declarations(cwd, tree.root_node(), &source, &scope, seen);
}

struct Scope<'a> {
    /// Where a plain `mod x;` looks.
    children_dir: &'a Path,
    /// What a `#[path]` is relative to.
    path_base: &'a Path,
    module: &'a [String],
    test_only: bool,
}

fn declarations(
    cwd: &Path,
    node: Node,
    source: &str,
    scope: &Scope,
    seen: &mut Vec<(String, Vec<String>, bool)>,
) {
    for child in children(node) {
        if child.kind() != "mod_item" {
            continue;
        }
        let name = name_of(child, source).to_string();
        let mut path = scope.module.to_vec();
        path.push(name.clone());
        let test_only = scope.test_only || is_test_item(child, source);
        let path_attr = attributes(child)
            .0
            .into_iter()
            .find_map(|a| attribute_parts(a, source).and_then(|(_, w)| path_attribute(w)));
        if let Some(body) = child.child_by_field_name("body") {
            let inner = match &path_attr {
                Some(p) => scope.path_base.join(p),
                None => scope.children_dir.join(&name),
            };
            let inner_scope = Scope {
                children_dir: &inner,
                path_base: &inner,
                module: &path,
                test_only,
            };
            declarations(cwd, body, source, &inner_scope, seen);
            continue;
        }
        let (file, mod_rs) = match path_attr {
            Some(p) => (normalize(&scope.path_base.join(p)), true),
            None => {
                let flat = normalize(&scope.children_dir.join(format!("{name}.rs")));
                if flat.as_ref().is_some_and(|f| cwd.join(f).is_file()) {
                    (flat, false)
                } else {
                    (
                        normalize(&scope.children_dir.join(&name).join("mod.rs")),
                        true,
                    )
                }
            }
        };
        if let Some(file) = file {
            walk_module(cwd, &file, path, mod_rs, test_only, seen);
        }
    }
}

/// A cwd-relative path with `.` and `..` resolved, or None when it leaves the cwd.
fn normalize(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn summary(index: &Index, rel: &str) -> Vec<String> {
        index
            .refs(rel)
            .iter()
            .map(|r| {
                format!(
                    "{:?} {} [{}]{}",
                    r.target.kind,
                    r.target.root,
                    r.module.join("::"),
                    if r.test_only { " test" } else { "" }
                )
            })
            .collect()
    }

    #[test]
    fn maps_files_to_the_targets_and_modules_that_declare_them() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
        );
        write(
            d,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"./src/lib.rs\"\n\n[[test]]\nname = \"custom\"\npath = \"tests/my.rs\"\nharness = false\n\n[[example]]\nname = \"ex\"\ntest = true\n",
        );
        write(
            d,
            "app/src/lib.rs",
            "pub mod math;\nmod inner {\n    mod deep;\n}\n#[path = r#\"odd/name.rs\"#]\nmod renamed;\n#[path = \"../../shared.rs\"]\nmod shared;\n#[cfg(test)]\nmod unit;\n#[cfg(any())]\nmod disabled;\n#[path = \"disabled.rs\"]\nmod enabled;\n#[path = \"../tests/core.rs\"]\npub mod core;\n",
        );
        write(d, "app/src/math.rs", "mod ops;\n");
        write(d, "app/src/math/ops.rs", "");
        write(d, "app/src/inner/deep.rs", "");
        write(d, "app/src/odd/name.rs", "");
        write(d, "shared.rs", "");
        write(d, "app/src/unit.rs", "mod helpers;\n");
        write(d, "app/src/unit/helpers.rs", "");
        write(d, "app/src/disabled.rs", "");
        write(d, "app/tests/core.rs", "");
        write(d, "app/src/main.rs", "mod cli;\n");
        write(d, "app/src/cli.rs", "");
        write(d, "app/tests/a.rs", "mod common;\n");
        write(d, "app/tests/b.rs", "mod common;\n");
        write(d, "app/tests/common/mod.rs", "");
        write(d, "app/tests/my.rs", "");
        write(d, "app/examples/ex.rs", "");
        write(d, "app/src/orphan.rs", "");
        let command = vec!["cargo".to_string(), "test".to_string()];

        let index = build(&command, d);

        assert_eq!(index.members, vec!["app"]);
        assert_eq!(
            summary(&index, "app/src/math/ops.rs"),
            vec!["Lib app/src/lib.rs [math::ops]"]
        );
        assert_eq!(
            summary(&index, "app/src/inner/deep.rs"),
            vec!["Lib app/src/lib.rs [inner::deep]"]
        );
        assert_eq!(
            summary(&index, "app/src/odd/name.rs"),
            vec!["Lib app/src/lib.rs [renamed]"]
        );
        assert_eq!(
            summary(&index, "shared.rs"),
            vec!["Lib app/src/lib.rs [shared]"]
        );
        assert_eq!(
            summary(&index, "app/src/unit/helpers.rs"),
            vec!["Lib app/src/lib.rs [unit::helpers] test"]
        );
        assert_eq!(
            summary(&index, "app/src/disabled.rs"),
            vec![
                "Lib app/src/lib.rs [disabled]",
                "Lib app/src/lib.rs [enabled]"
            ]
        );
        // Production code under tests/ is also its own test target, as cargo sees it.
        assert_eq!(
            summary(&index, "app/tests/core.rs"),
            vec![
                "Lib app/src/lib.rs [core]",
                "Test(\"core\") app/tests/core.rs []"
            ]
        );
        assert_eq!(
            summary(&index, "app/src/cli.rs"),
            vec!["Bin(\"app\") app/src/main.rs [cli]"]
        );
        assert_eq!(
            summary(&index, "app/tests/common/mod.rs"),
            vec![
                "Test(\"a\") app/tests/a.rs [common]",
                "Test(\"b\") app/tests/b.rs [common]"
            ]
        );
        assert_eq!(
            summary(&index, "app/examples/ex.rs"),
            vec!["Example(\"ex\") app/examples/ex.rs []"]
        );
        assert!(summary(&index, "app/tests/my.rs").is_empty());
        assert!(summary(&index, "app/src/orphan.rs").is_empty());
    }
}
