//! Source parsing with tree-sitter: Python, TypeScript, and TSX (which also covers
//! JavaScript and JSX).

pub mod python;
pub mod ts;

use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Python,
    TypeScript,
    /// TSX, also used for JavaScript and JSX files.
    Tsx,
}

impl Lang {
    /// The language of a path by extension, or None when titer does not parse it.
    pub fn of(path: &str) -> Option<Lang> {
        let ext = path.rsplit_once('.').map(|(_, e)| e)?;
        match ext {
            "py" => Some(Lang::Python),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Lang::Tsx),
            _ => None,
        }
    }

    pub fn is_python(self) -> bool {
        self == Lang::Python
    }

    pub fn parse(self, source: &str) -> Option<Tree> {
        let mut parser = Parser::new();
        let language: tree_sitter::Language = match self {
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        };
        parser.set_language(&language).ok()?;
        parser.parse(source, None)
    }
}

/// A test function, method, or `it`/`test` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestDef {
    /// Enclosing classes or `describe` titles, outermost first.
    pub suite: Vec<String>,
    pub name: String,
    /// 1-based first line, including decorators.
    pub line: u32,
    pub end_line: u32,
    /// The line the runner reports for the test: the `def` line in Python, the call's first
    /// line in TypeScript.
    pub report_line: u32,
    /// Statically skipped: `it.skip`, `it.todo`, `xit`, or inside `describe.skip`.
    pub skip: bool,
    /// The byte span of the test, including decorators.
    pub start_byte: usize,
    pub end_byte: usize,
}

/// A function, method, or named arrow function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    /// The name qualified by enclosing classes and functions, joined with `.`.
    pub name: String,
    pub line: u32,
    pub end_line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
}

impl FunctionDef {
    pub fn contains_line(&self, line: u32) -> bool {
        (self.line..=self.end_line).contains(&line)
    }
}

/// Which test framework conventions to apply when finding Python tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonStyle {
    Pytest,
    Unittest,
}

pub fn find_tests(lang: Lang, source: &str, style: PythonStyle) -> Vec<TestDef> {
    let Some(tree) = lang.parse(source) else {
        return Vec::new();
    };
    match lang {
        Lang::Python => python::find_tests(tree.root_node(), source, style),
        Lang::TypeScript | Lang::Tsx => ts::find_tests(tree.root_node(), source),
    }
}

pub fn find_functions(lang: Lang, source: &str) -> Vec<FunctionDef> {
    let Some(tree) = lang.parse(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match lang {
        Lang::Python => python::find_functions(tree.root_node(), source, &mut Vec::new(), &mut out),
        Lang::TypeScript | Lang::Tsx => {
            ts::find_functions(tree.root_node(), source, &mut Vec::new(), &mut out)
        }
    }
    out
}

/// The innermost function that contains each line, by index into `functions`.
pub fn innermost_function(functions: &[FunctionDef], line: u32) -> Option<usize> {
    functions
        .iter()
        .enumerate()
        .filter(|(_, f)| f.contains_line(line))
        .min_by_key(|(_, f)| f.end_byte - f.start_byte)
        .map(|(i, _)| i)
}

pub(crate) fn text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

pub(crate) fn line_of(node: Node) -> u32 {
    node.start_position().row as u32 + 1
}

pub(crate) fn end_line_of(node: Node) -> u32 {
    let end = node.end_position();
    // A node that ends at column 0 ends on the previous line.
    if end.column == 0 && end.row > node.start_position().row {
        end.row as u32
    } else {
        end.row as u32 + 1
    }
}

pub(crate) fn children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_by_extension() {
        assert_eq!(Lang::of("a/b.py"), Some(Lang::Python));
        assert_eq!(Lang::of("a.ts"), Some(Lang::TypeScript));
        assert_eq!(Lang::of("a.tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::of("a.js"), Some(Lang::Tsx));
        assert_eq!(Lang::of("a.rs"), None);
        assert_eq!(Lang::of("Makefile"), None);
    }
}
