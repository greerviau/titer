//! Which compiled targets hold a source file, and the file's module path in each. Targets
//! come from the package manifest and cargo's automatic discovery; modules come from the
//! `mod` declarations reachable from each target's root, including `#[path]` attributes.
//! A module file that several targets declare, such as `tests/common/mod.rs`, belongs to
//! each of them.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

use crate::lang::rust::{attribute_parts, attributes, name_of};
use crate::lang::{Lang, children};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Lib,
    Bin(String),
    Test(String),
}

/// A compiled target of one package.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target {
    pub package: Option<String>,
    pub kind: Kind,
    /// The target's root file, relative to the runner's cwd.
    pub root: String,
}

impl Target {
    /// The cargo arguments that select this target and nothing else.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(package) = &self.package {
            args.extend(["-p".to_string(), package.clone()]);
        }
        match &self.kind {
            Kind::Lib => args.push("--lib".to_string()),
            Kind::Bin(name) => args.extend(["--bin".to_string(), name.clone()]),
            Kind::Test(name) => args.extend(["--test".to_string(), name.clone()]),
        }
        args
    }
}

/// The targets that compile the cwd-relative file `rel`, each with the file's module path.
pub fn targets_of(cwd: &Path, rel: &str) -> Vec<(Target, Vec<String>)> {
    let Some((package_dir, manifest)) = package_of(cwd, rel) else {
        return Vec::new();
    };
    let in_package = if package_dir.is_empty() {
        rel.to_string()
    } else {
        match rel.strip_prefix(&format!("{package_dir}/")) {
            Some(r) => r.to_string(),
            None => return Vec::new(),
        }
    };
    let dir = cwd.join(&package_dir);
    let package = manifest
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string);
    let mut out = Vec::new();
    for (kind, root) in roots(&dir, &manifest, package.as_deref().unwrap_or_default()) {
        let mut modules = HashMap::new();
        walk_module(&dir, &root, Vec::new(), true, &mut modules);
        if let Some(module) = modules.remove(&in_package) {
            let root = if package_dir.is_empty() {
                root
            } else {
                format!("{package_dir}/{root}")
            };
            out.push((
                Target {
                    package: package.clone(),
                    kind,
                    root,
                },
                module,
            ));
        }
    }
    out
}

/// The nearest dir at or above `rel`'s dir, inside the cwd, with a `[package]` manifest:
/// its cwd-relative path and the manifest.
fn package_of(cwd: &Path, rel: &str) -> Option<(String, toml::Table)> {
    let mut dir = Path::new(rel).parent();
    while let Some(d) = dir {
        if let Ok(text) = std::fs::read_to_string(cwd.join(d).join("Cargo.toml"))
            && let Ok(manifest) = text.parse::<toml::Table>()
            && manifest.contains_key("package")
        {
            return Some((d.to_string_lossy().into_owned(), manifest));
        }
        dir = d.parent();
    }
    None
}

/// The package's targets that libtest runs, with root files relative to the package dir:
/// explicit `[lib]`, `[[bin]]`, and `[[test]]` entries, then cargo's automatic ones.
/// Targets with `test = false` or `harness = false` are left out, since titer cannot select
/// or read their tests.
fn roots(dir: &Path, manifest: &toml::Table, package: &str) -> Vec<(Kind, String)> {
    let mut out: Vec<(Kind, String)> = Vec::new();
    let flag = |t: &toml::Value, key: &str| t.get(key).and_then(|v| v.as_bool());
    let runs =
        |t: &toml::Value| flag(t, "test") != Some(false) && flag(t, "harness") != Some(false);
    let exists = |p: &str| dir.join(p).is_file();
    let disabled = |key: &str| {
        manifest
            .get("package")
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_bool())
            == Some(false)
    };

    let lib = manifest.get("lib");
    let lib_path = lib
        .and_then(|l| l.get("path"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .or_else(|| exists("src/lib.rs").then(|| "src/lib.rs".to_string()));
    if let Some(path) = lib_path
        && lib.is_none_or(runs)
    {
        out.push((Kind::Lib, path));
    }

    let mut explicit =
        |section: &str, make: fn(String) -> Kind, default: &dyn Fn(&str) -> Option<String>| {
            let mut named = Vec::new();
            for entry in manifest
                .get(section)
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
            {
                let Some(name) = entry.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                named.push(name.to_string());
                let path = entry
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(str::to_string)
                    .or_else(|| default(name));
                if let Some(path) = path
                    && runs(entry)
                {
                    out.push((make(name.to_string()), path));
                }
            }
            named
        };
    let bin_default = |name: &str| {
        [
            if name == package {
                "src/main.rs".to_string()
            } else {
                String::new()
            },
            format!("src/bin/{name}.rs"),
            format!("src/bin/{name}/main.rs"),
        ]
        .into_iter()
        .find(|p| !p.is_empty() && exists(p))
    };
    let named_bins = explicit("bin", Kind::Bin, &bin_default);
    let test_default = |name: &str| {
        [format!("tests/{name}.rs"), format!("tests/{name}/main.rs")]
            .into_iter()
            .find(|p| exists(p))
    };
    let named_tests = explicit("test", Kind::Test, &test_default);

    if !disabled("autobins") {
        let mut auto = Vec::new();
        if exists("src/main.rs") {
            auto.push((package.to_string(), "src/main.rs".to_string()));
        }
        auto.extend(discovered(dir, "src/bin"));
        for (name, path) in auto {
            if !named_bins.contains(&name) && !out.iter().any(|(_, p)| *p == path) {
                out.push((Kind::Bin(name), path));
            }
        }
    }
    if !disabled("autotests") {
        for (name, path) in discovered(dir, "tests") {
            if !named_tests.contains(&name) && !out.iter().any(|(_, p)| *p == path) {
                out.push((Kind::Test(name), path));
            }
        }
    }
    out
}

/// Targets cargo discovers in `sub`: `<name>.rs` and `<name>/main.rs`.
fn discovered(dir: &Path, sub: &str) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir.join(sub)) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            if path.is_file() {
                let stem = name.strip_suffix(".rs")?;
                Some((stem.to_string(), format!("{sub}/{name}")))
            } else if path.join("main.rs").is_file() {
                Some((name.clone(), format!("{sub}/{name}/main.rs")))
            } else {
                None
            }
        })
        .collect();
    out.sort();
    out
}

static PATH_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^path\s*=\s*"([^"]*)"$"#).unwrap());

/// Records `file`, a package-relative module file, and every module file it declares. A
/// mod-rs file (a root, `mod.rs`, or a `#[path]` file) declares children in its own dir;
/// another file `a.rs` declares them in `a/`.
fn walk_module(
    dir: &Path,
    file: &str,
    module: Vec<String>,
    mod_rs: bool,
    out: &mut HashMap<String, Vec<String>>,
) {
    if out.contains_key(file) {
        return;
    }
    out.insert(file.to_string(), module.clone());
    let Ok(source) = std::fs::read_to_string(dir.join(file)) else {
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
    declarations(
        dir,
        tree.root_node(),
        &source,
        &children_dir,
        &parent,
        &module,
        out,
    );
}

/// Follows the `mod` declarations in `node`. `children_dir` is where a plain `mod x;`
/// looks; `path_base` is what a `#[path]` is relative to.
fn declarations(
    dir: &Path,
    node: Node,
    source: &str,
    children_dir: &Path,
    path_base: &Path,
    module: &[String],
    out: &mut HashMap<String, Vec<String>>,
) {
    for child in children(node) {
        if child.kind() != "mod_item" {
            continue;
        }
        let name = name_of(child, source).to_string();
        let mut path = module.to_vec();
        path.push(name.clone());
        let path_attr = attributes(child).0.into_iter().find_map(|a| {
            let (_, whole) = attribute_parts(a, source)?;
            PATH_ATTR.captures(whole).map(|c| c[1].to_string())
        });
        if let Some(body) = child.child_by_field_name("body") {
            let inner = match &path_attr {
                Some(p) => path_base.join(p),
                None => children_dir.join(&name),
            };
            declarations(dir, body, source, &inner, &inner, &path, out);
            continue;
        }
        let (file, mod_rs) = match path_attr {
            Some(p) => (normalize(&path_base.join(p)), true),
            None => {
                let flat = normalize(&children_dir.join(format!("{name}.rs")));
                if dir.join(&flat).is_file() {
                    (flat, false)
                } else {
                    (normalize(&children_dir.join(&name).join("mod.rs")), true)
                }
            }
        };
        walk_module(dir, &file, path, mod_rs, out);
    }
}

/// A relative path with `.` and `..` resolved, as a `/`-separated string.
fn normalize(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    PathBuf::from(parts.join("/"))
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn summary(cwd: &Path, rel: &str) -> Vec<String> {
        targets_of(cwd, rel)
            .into_iter()
            .map(|(t, m)| format!("{:?} {} [{}]", t.kind, t.root, m.join("::")))
            .collect()
    }

    #[test]
    fn maps_files_to_the_targets_that_declare_them() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"custom\"\nharness = false\n",
        );
        write(
            d,
            "app/src/lib.rs",
            "pub mod math;\nmod inner {\n    mod deep;\n}\n#[path = \"odd/name.rs\"]\nmod renamed;\n",
        );
        write(d, "app/src/math.rs", "mod ops;\n");
        write(d, "app/src/math/ops.rs", "");
        write(d, "app/src/inner/deep.rs", "");
        write(d, "app/src/odd/name.rs", "");
        write(d, "app/src/main.rs", "mod cli;\n");
        write(d, "app/src/cli.rs", "");
        write(d, "app/src/bin/tool/main.rs", "mod args;\n");
        write(d, "app/src/bin/tool/args.rs", "");
        write(d, "app/tests/a.rs", "mod common;\n");
        write(d, "app/tests/b.rs", "mod common;\n");
        write(d, "app/tests/common/mod.rs", "");
        write(d, "app/tests/custom.rs", "");
        write(d, "app/src/orphan.rs", "");

        assert_eq!(
            summary(d, "app/src/math/ops.rs"),
            vec!["Lib app/src/lib.rs [math::ops]"]
        );
        assert_eq!(
            summary(d, "app/src/inner/deep.rs"),
            vec!["Lib app/src/lib.rs [inner::deep]"]
        );
        assert_eq!(
            summary(d, "app/src/odd/name.rs"),
            vec!["Lib app/src/lib.rs [renamed]"]
        );
        assert_eq!(
            summary(d, "app/src/cli.rs"),
            vec!["Bin(\"app\") app/src/main.rs [cli]"]
        );
        assert_eq!(
            summary(d, "app/src/bin/tool/args.rs"),
            vec!["Bin(\"tool\") app/src/bin/tool/main.rs [args]"]
        );
        assert_eq!(
            summary(d, "app/tests/common/mod.rs"),
            vec![
                "Test(\"a\") app/tests/a.rs [common]",
                "Test(\"b\") app/tests/b.rs [common]"
            ]
        );
        assert!(summary(d, "app/tests/custom.rs").is_empty());
        assert!(summary(d, "app/src/orphan.rs").is_empty());
    }
}
