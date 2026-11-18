//! Python: tests, functions, and the other structure titer reads from Python sources.

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
    let mut out = Vec::new();
    walk_tests(root, source, style, &mut Vec::new(), &mut out);
    out
}

fn walk_tests(
    container: Node,
    source: &str,
    style: PythonStyle,
    suite: &mut Vec<String>,
    out: &mut Vec<TestDef>,
) {
    let body = if container.kind() == "module" {
        Some(container)
    } else {
        container.child_by_field_name("body")
    };
    let Some(body) = body else { return };
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
