//! Python: tests, functions, and the other structure titer reads from Python sources.

use std::cell::RefCell;

use tree_sitter::Node;

use super::{FunctionDef, PythonStyle, TestDef, children, end_line_of, line_of, text};

/// The function definition inside a possibly decorated definition, and the outer node whose
/// span includes the decorators.
pub(crate) fn unwrap_decorated(node: Node) -> (Node, Node) {
    if node.kind() == "decorated_definition"
        && let Some(def) = node.child_by_field_name("definition")
    {
        return (def, node);
    }
    (node, node)
}

fn name_of<'a>(node: Node, source: &'a str) -> &'a str {
    node.child_by_field_name("name")
        .map(|n| text(n, source))
        .unwrap_or_default()
}

/// The last dotted segment of each base class.
fn base_names<'a>(class: Node, source: &'a str) -> Vec<&'a str> {
    class
        .child_by_field_name("superclasses")
        .map(|b| {
            children(b)
                .into_iter()
                .filter(|n| matches!(n.kind(), "identifier" | "attribute"))
                .map(|n| text(n, source).rsplit('.').next().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

/// A base that is, or by naming convention derives from, `unittest.TestCase`.
fn is_test_base(name: &str) -> bool {
    let test_word = name
        .strip_prefix("Test")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_uppercase()));
    test_word || name.contains("TestCase") || name.ends_with("Test") || name.ends_with("Tests")
}

fn defines_init(class: Node, source: &str) -> bool {
    class.child_by_field_name("body").is_some_and(|body| {
        children(body).into_iter().any(|c| {
            let def = unwrap_decorated(c).0;
            def.kind() == "function_definition" && name_of(def, source) == "__init__"
        })
    })
}

fn is_test_class(class: Node, source: &str, style: PythonStyle) -> bool {
    let unittest_class = base_names(class, source).into_iter().any(is_test_base);
    match style {
        // pytest collects unittest classes, and Test* classes without __init__.
        PythonStyle::Pytest => {
            unittest_class
                || (name_of(class, source).starts_with("Test") && !defines_init(class, source))
        }
        PythonStyle::Unittest => unittest_class,
    }
}

pub fn find_tests(root: Node, source: &str, style: PythonStyle) -> Vec<TestDef> {
    let module = Module::new(root, source);
    let mut out = Vec::new();
    for child in children(root) {
        let (def, outer) = unwrap_decorated(child);
        match def.kind() {
            "function_definition" if style == PythonStyle::Pytest => {
                let name = name_of(def, source);
                if name.starts_with("test") {
                    out.push(test_def(Vec::new(), name.to_string(), def, outer));
                }
            }
            "class_definition" => {
                let Some(i) = module.classes.iter().position(|c| c.id() == def.id()) else {
                    continue;
                };
                if module.shadowed(i) || !module.is_test_class(i, style) {
                    continue;
                }
                // The class's own and inherited methods come from its resolution order;
                // the walk adds only the nested test classes.
                module.class_tests(i, &mut out);
                let mut nested = Vec::new();
                let mut suite = vec![name_of(def, source).to_string()];
                walk_tests(def, source, style, &mut suite, &mut nested);
                out.extend(nested.into_iter().filter(|t| t.suite.len() > 1));
            }
            _ => {}
        }
    }
    out
}

fn test_def(suite: Vec<String>, name: String, def: Node, outer: Node) -> TestDef {
    TestDef {
        suite,
        name,
        line: line_of(outer),
        end_line: end_line_of(outer),
        report_line: line_of(def),
        skip: false,
        parametrized: false,
        start_byte: outer.start_byte(),
        end_byte: outer.end_byte(),
    }
}

/// The top-level classes of a test file, so a class's bases can be followed to the classes
/// the same file defines.
struct Module<'a> {
    source: &'a str,
    classes: Vec<Node<'a>>,
    mro: RefCell<Vec<Option<Vec<Base>>>>,
    unittest: RefCell<Vec<Option<bool>>>,
}

/// A base in a resolution order: a class this file defines, or any other by its text.
#[derive(Clone, Debug, PartialEq)]
enum Base {
    Local(usize),
    Other(String),
}

/// What a class body binds a name to last: a method, or something else, such as
/// `test_x = None`, which hides an inherited test.
#[derive(Clone, Copy)]
enum Binding<'a> {
    Method(Node<'a>, Node<'a>),
    Other,
}

impl<'a> Module<'a> {
    fn new(root: Node<'a>, source: &'a str) -> Module<'a> {
        let classes: Vec<Node> = children(root)
            .into_iter()
            .map(|c| unwrap_decorated(c).0)
            .filter(|c| c.kind() == "class_definition")
            .collect();
        let n = classes.len();
        Module {
            source,
            classes,
            mro: RefCell::new(vec![None; n]),
            unittest: RefCell::new(vec![None; n]),
        }
    }

    fn name(&self, i: usize) -> &'a str {
        name_of(self.classes[i], self.source)
    }

    /// A later class of the same name rebinds it, so the runner never sees this one.
    fn shadowed(&self, i: usize) -> bool {
        self.classes[i + 1..]
            .iter()
            .any(|c| name_of(*c, self.source) == self.name(i))
    }

    /// The class's bases in order. A plain name, or a subscript of one such as
    /// `Mixin[int]`, is the last class of that name defined before this one.
    fn bases(&self, i: usize) -> Vec<Base> {
        let Some(list) = self.classes[i].child_by_field_name("superclasses") else {
            return Vec::new();
        };
        children(list)
            .into_iter()
            .filter_map(|n| {
                let n = if n.kind() == "subscript" {
                    n.child_by_field_name("value")?
                } else {
                    n
                };
                let name = text(n, self.source);
                match n.kind() {
                    "identifier" => Some(
                        (0..i)
                            .rev()
                            .find(|j| self.name(*j) == name)
                            .map_or_else(|| Base::Other(name.to_string()), Base::Local),
                    ),
                    "attribute" => Some(Base::Other(name.to_string())),
                    _ => None,
                }
            })
            .collect()
    }

    /// Whether the class derives from `unittest.TestCase`: through a same-file base that
    /// does, or through another base whose name says so.
    fn is_unittest(&self, i: usize) -> bool {
        if let Some(known) = self.unittest.borrow()[i] {
            return known;
        }
        let found = self.bases(i).into_iter().any(|b| match b {
            Base::Local(j) => self.is_unittest(j),
            Base::Other(name) => is_test_base(name.rsplit('.').next().unwrap_or_default()),
        });
        self.unittest.borrow_mut()[i] = Some(found);
        found
    }

    fn is_test_class(&self, i: usize, style: PythonStyle) -> bool {
        let unittest = self.is_unittest(i);
        match style {
            // pytest collects unittest classes, and Test* classes without __init__.
            PythonStyle::Pytest => {
                unittest
                    || (self.name(i).starts_with("Test")
                        && !self.mro(i).iter().any(|b| {
                            matches!(b, Base::Local(c) if defines_init(self.classes[*c], self.source))
                        }))
            }
            PythonStyle::Unittest => unittest,
        }
    }

    /// The C3 method resolution order, as Python computes it, with bases from other files
    /// as opaque entries so they still constrain the order of this file's classes.
    fn mro(&self, i: usize) -> Vec<Base> {
        if let Some(known) = &self.mro.borrow()[i] {
            return known.clone();
        }
        let bases = self.bases(i);
        let mut seqs: Vec<Vec<Base>> = bases
            .iter()
            .map(|b| match b {
                Base::Local(j) => self.mro(*j),
                other => vec![other.clone()],
            })
            .collect();
        seqs.push(bases);
        let mut out = vec![Base::Local(i)];
        loop {
            seqs.retain(|s| !s.is_empty());
            let Some(head) = seqs
                .iter()
                .map(|s| s[0].clone())
                .find(|h| seqs.iter().all(|s| !s[1..].contains(h)))
            else {
                break;
            };
            for s in &mut seqs {
                if s[0] == head {
                    s.remove(0);
                }
            }
            out.push(head);
        }
        // Python rejects an inconsistent order; keep the rest in declaration order.
        for b in seqs.into_iter().flatten() {
            if !out.contains(&b) {
                out.push(b);
            }
        }
        self.mro.borrow_mut()[i] = Some(out.clone());
        out
    }

    /// The tests the runner collects under the class: for each `test*` name, the first
    /// class in the resolution order that binds it, when it binds it to a method. An
    /// inherited method gets a test here with the span of the method it inherits.
    fn class_tests(&self, i: usize, out: &mut Vec<TestDef>) {
        let mut bound: Vec<&str> = Vec::new();
        for base in self.mro(i) {
            let Base::Local(c) = base else { continue };
            for (name, binding) in self.bindings(c) {
                if bound.contains(&name) {
                    continue;
                }
                bound.push(name);
                if let Binding::Method(def, outer) = binding {
                    out.push(test_def(
                        vec![self.name(i).to_string()],
                        name.to_string(),
                        def,
                        outer,
                    ));
                }
            }
        }
    }

    /// The `test*` names a class body binds, each with its last binding, in the order of
    /// first appearance. A bare annotation binds nothing.
    fn bindings(&self, i: usize) -> Vec<(&'a str, Binding<'a>)> {
        let Some(body) = self.classes[i].child_by_field_name("body") else {
            return Vec::new();
        };
        let mut out: Vec<(&str, Binding)> = Vec::new();
        let mut bind = |name: &'a str, b: Binding<'a>| {
            if !name.starts_with("test") {
                return;
            }
            match out.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = b,
                None => out.push((name, b)),
            }
        };
        for child in children(body) {
            let (def, outer) = unwrap_decorated(child);
            if def.kind() == "function_definition" {
                bind(name_of(def, self.source), Binding::Method(def, outer));
            } else if def.kind() == "expression_statement" {
                for assign in children(def).into_iter().filter(|n| {
                    n.kind() == "assignment" && n.child_by_field_name("right").is_some()
                }) {
                    if let Some(left) = assign.child_by_field_name("left")
                        && left.kind() == "identifier"
                    {
                        bind(text(left, self.source), Binding::Other);
                    }
                }
            }
        }
        out
    }
}

fn walk_tests(
    container: Node,
    source: &str,
    style: PythonStyle,
    suite: &mut Vec<String>,
    out: &mut Vec<TestDef>,
) {
    let Some(body) = container.child_by_field_name("body") else {
        return;
    };
    let in_class = !suite.is_empty();
    for child in children(body) {
        let (def, outer) = unwrap_decorated(child);
        match def.kind() {
            "function_definition" => {
                let name = name_of(def, source);
                let collected =
                    name.starts_with("test") && (in_class || style == PythonStyle::Pytest);
                if collected {
                    out.push(TestDef {
                        suite: suite.clone(),
                        name: name.to_string(),
                        line: line_of(outer),
                        end_line: end_line_of(outer),
                        report_line: line_of(def),
                        skip: false,
                        parametrized: false,
                        start_byte: outer.start_byte(),
                        end_byte: outer.end_byte(),
                    });
                }
            }
            "class_definition" if is_test_class(def, source, style) => {
                suite.push(name_of(def, source).to_string());
                walk_tests(def, source, style, suite, out);
                suite.pop();
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
        let (def, outer) = unwrap_decorated(child);
        match def.kind() {
            "function_definition" => {
                let name = name_of(def, source).to_string();
                let mut qualified = scope.clone();
                qualified.push(name.clone());
                out.push(FunctionDef {
                    name: qualified.join("."),
                    line: line_of(outer),
                    end_line: end_line_of(outer),
                    start_byte: outer.start_byte(),
                    end_byte: outer.end_byte(),
                });
                scope.push(name);
                find_functions(def, source, scope, out);
                scope.pop();
            }
            "class_definition" => {
                scope.push(name_of(def, source).to_string());
                find_functions(def, source, scope, out);
                scope.pop();
            }
            _ => find_functions(child, source, scope, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Lang, find_functions, find_tests};
    use super::*;

    const SOURCE: &str = r#"import pytest
import unittest


def helper():
    return 1


def test_top():
    assert helper() == 1


@pytest.mark.parametrize("x", [1])
async def test_decorated(x):
    assert x


class TestGroup:
    def test_a(self):
        def inner():
            return 2
        assert inner() == 2

    class TestNested:
        def test_b(self):
            pass

    def not_a_test(self):
        pass


class LoginTests(unittest.TestCase):
    def setUp(self):
        pass

    def test_login(self):
        self.assertTrue(True)


class Plain:
    def test_ignored(self):
        pass


class TestWithInit:
    def __init__(self):
        pass

    def test_never_collected(self):
        pass


class TestOuter:
    class Helper:
        def test_in_helper(self):
            pass


class Bogus(Testable):
    def test_bogus(self):
        pass
"#;

    fn summary(tests: &[TestDef]) -> Vec<String> {
        tests
            .iter()
            .map(|t| {
                let mut parts = t.suite.clone();
                parts.push(t.name.clone());
                format!(
                    "{} {}-{} def {}",
                    parts.join("::"),
                    t.line,
                    t.end_line,
                    t.report_line
                )
            })
            .collect()
    }

    #[test]
    fn finds_pytest_tests_with_decorators_and_nested_classes() {
        let tests = find_tests(Lang::Python, SOURCE, PythonStyle::Pytest);
        assert_eq!(
            summary(&tests),
            vec![
                "test_top 9-10 def 9",
                "test_decorated 13-15 def 14",
                "TestGroup::test_a 19-22 def 19",
                "TestGroup::TestNested::test_b 25-26 def 25",
                "LoginTests::test_login 36-37 def 36",
            ]
        );
    }

    #[test]
    fn finds_unittest_methods_only_in_test_case_classes() {
        let tests = find_tests(Lang::Python, SOURCE, PythonStyle::Unittest);
        assert_eq!(summary(&tests), vec!["LoginTests::test_login 36-37 def 36"]);
    }

    #[test]
    fn test_cases_inherit_mixin_tests_unless_they_override_them() {
        let source = "import unittest\n\n\nclass Base:\n    def test_a(self):\n        pass\n\n\nclass Checks(Base):\n    def test_b(self):\n        pass\n\n\nclass ApiTest(Checks, unittest.TestCase):\n    def test_b(self):\n        pass\n\n\nclass TestChild(ApiTest):\n    pass\n";
        let unittest = find_tests(Lang::Python, source, PythonStyle::Unittest);
        assert_eq!(
            summary(&unittest),
            vec![
                "ApiTest::test_b 15-16 def 15",
                "ApiTest::test_a 5-6 def 5",
                "TestChild::test_b 15-16 def 15",
                "TestChild::test_a 5-6 def 5",
            ]
        );
        // pytest also collects the Test* class that inherits from the TestCase.
        let pytest = find_tests(Lang::Python, source, PythonStyle::Pytest);
        assert_eq!(summary(&pytest), summary(&unittest));
    }

    #[test]
    fn inherited_tests_follow_same_file_ancestry_and_the_c3_order() {
        let names = |source: &str, style| -> Vec<String> {
            find_tests(Lang::Python, source, style)
                .iter()
                .map(|t| format!("{}::{} {}", t.suite.join("::"), t.name, t.line))
                .collect()
        };
        // TestCase ancestry through a same-file base whose name does not say so.
        let chain = "import unittest\n\nclass Checks:\n    def test_x(self):\n        pass\n\nclass Base(Checks, unittest.TestCase):\n    pass\n\nclass Concrete(Base):\n    pass\n";
        assert_eq!(
            names(chain, PythonStyle::Unittest),
            ["Base::test_x 4", "Concrete::test_x 4"]
        );
        // A Test-named mixin without TestCase does not make its subclass a unittest class.
        let mixin = "class TestSupport:\n    def test_x(self):\n        pass\n\nclass Support(TestSupport):\n    pass\n";
        assert!(names(mixin, PythonStyle::Unittest).is_empty());
        // C3: Left's base Checks comes before Right.
        let diamond = "import unittest\n\nclass Checks:\n    def test_x(self):\n        pass\n\nclass Left(Checks):\n    pass\n\nclass Right:\n    def test_x(self):\n        pass\n\nclass Concrete(Left, Right, unittest.TestCase):\n    pass\n";
        assert_eq!(
            names(diamond, PythonStyle::Unittest),
            ["Concrete::test_x 4"]
        );
        // An assignment hides the inherited test; a later definition of a name wins.
        let hidden = "import unittest\n\nclass Checks:\n    def test_x(self):\n        pass\n\nclass Checks:\n    def test_y(self):\n        pass\n\nclass A(Checks, unittest.TestCase):\n    test_y = None\n\nclass B(Checks, unittest.TestCase):\n    pass\n";
        assert_eq!(names(hidden, PythonStyle::Unittest), ["B::test_y 8"]);
        // An imported base still orders this file's classes: Right comes before Ancestor.
        let external = "import unittest\nfrom lib import Shared\n\nclass Ancestor:\n    def test_conflict(self):\n        pass\n\nclass Left(Shared, Ancestor):\n    pass\n\nclass Right(Shared):\n    def test_conflict(self):\n        pass\n\nclass Concrete(Left, Right, unittest.TestCase):\n    pass\n";
        assert_eq!(
            names(external, PythonStyle::Unittest),
            ["Concrete::test_conflict 12"]
        );
        // A subscripted base is the class it subscripts.
        let generic = "import unittest\n\nclass Mixin:\n    def test_generic(self):\n        pass\n\nclass Concrete(Mixin[int], unittest.TestCase):\n    pass\n";
        assert_eq!(
            names(generic, PythonStyle::Unittest),
            ["Concrete::test_generic 4"]
        );
        // The last binding in a class body wins, and a bare annotation binds nothing.
        let rebound = "import unittest\n\nclass A(unittest.TestCase):\n    def test_x(self):\n        pass\n    test_x = None\n    test_y = None\n    def test_y(self):\n        pass\n    test_z: int\n    def test_z(self):\n        pass\n";
        assert_eq!(
            names(rebound, PythonStyle::Unittest),
            ["A::test_y 8", "A::test_z 11"]
        );
        // pytest skips a Test class that inherits __init__.
        let init = "class Base:\n    def __init__(self):\n        pass\n\n    def test_x(self):\n        pass\n\nclass TestConcrete(Base):\n    pass\n";
        assert!(names(init, PythonStyle::Pytest).is_empty());
    }

    #[test]
    fn deep_inheritance_graphs_resolve_without_repeating_work() {
        // Each class inherits from the two before it, which doubles the paths per level.
        let mut source = String::from(
            "import unittest\n\nclass C0(unittest.TestCase):\n    def test_0(self):\n        pass\n\nclass C1(C0):\n    pass\n",
        );
        for i in 2..60 {
            source.push_str(&format!(
                "\nclass C{i}(C{}, C{}):\n    pass\n",
                i - 1,
                i - 2
            ));
        }
        let tests = find_tests(Lang::Python, &source, PythonStyle::Unittest);
        assert_eq!(tests.len(), 60);
    }

    #[test]
    fn finds_qualified_functions() {
        let names: Vec<String> = find_functions(Lang::Python, SOURCE)
            .into_iter()
            .map(|f| format!("{} {}-{}", f.name, f.line, f.end_line))
            .collect();
        assert_eq!(
            names,
            vec![
                "helper 5-6",
                "test_top 9-10",
                "test_decorated 13-15",
                "TestGroup.test_a 19-22",
                "TestGroup.test_a.inner 20-21",
                "TestGroup.TestNested.test_b 25-26",
                "TestGroup.not_a_test 28-29",
                "LoginTests.setUp 33-34",
                "LoginTests.test_login 36-37",
                "Plain.test_ignored 41-42",
                "TestWithInit.__init__ 46-47",
                "TestWithInit.test_never_collected 49-50",
                "TestOuter.Helper.test_in_helper 55-56",
                "Bogus.test_bogus 60-61",
            ]
        );
    }
}
