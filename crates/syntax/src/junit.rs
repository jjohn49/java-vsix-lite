//! Static JUnit 4/5 test discovery: classify annotated test methods without
//! executing anything, distinguishing real `org.junit.*` annotations from
//! same-named annotations of other packages via the file's imports. This
//! powers Test Explorer discovery in the safe (no-execution) tier.

use ls_types::Range;
use tree_sitter::{Node, Tree};

use crate::imports::Imports;
use crate::model::{has_modifier, modifiers_node, named_children};
use crate::{lombok, node_text, LineIndex};

/// JUnit annotation family a test method belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    /// `org.junit.jupiter.*` (JUnit 5) annotations.
    JUnit5,
    /// `org.junit.Test` (JUnit 4).
    JUnit4,
}

/// One discovered test method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestMethod {
    pub name: String,
    /// Range of the method's name identifier (gutter anchor).
    pub range: Range,
    pub kind: TestKind,
}

/// One class holding at least one discovered test method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestClass {
    /// Binary FQN (`pkg.Outer$Inner`) — the name the JUnit Platform launcher
    /// selects classes by.
    pub binary_fqn: String,
    /// The class's own simple name.
    pub name: String,
    /// Range of the class's name identifier.
    pub range: Range,
    pub methods: Vec<TestMethod>,
}

/// JUnit 5 method annotations that mark a method as executable by the
/// platform. JUnit 4's only marker is `Test`.
const TEST_ANNOTATIONS: [&str; 5] = [
    "Test",
    "ParameterizedTest",
    "RepeatedTest",
    "TestFactory",
    "TestTemplate",
];

/// Discover JUnit 4/5 test classes in one parsed document, in document order.
/// Nested classes produce their own entries (`@Nested` needs no special
/// handling: a class is a [`TestClass`] iff it has ≥1 classified method).
pub fn discover_tests(tree: &Tree, source: &str, index: &LineIndex) -> Vec<TestClass> {
    let imports = Imports::parse(tree, source);
    let mut out = Vec::new();
    collect_classes(tree.root_node(), source, index, &imports, &mut out);
    out
}

fn collect_classes(
    node: Node,
    source: &str,
    index: &LineIndex,
    imports: &Imports,
    out: &mut Vec<TestClass>,
) {
    // Only concrete classes host runnable tests; interfaces, annotation
    // types, enums, and records are skipped as containers but still walked
    // for nested class declarations.
    if node.kind() == "class_declaration" {
        if let Some(class) = test_class(node, source, index, imports) {
            out.push(class);
        }
    }
    for child in named_children(node) {
        collect_classes(child, source, index, imports, out);
    }
}

fn test_class(
    class: Node,
    source: &str,
    index: &LineIndex,
    imports: &Imports,
) -> Option<TestClass> {
    let name_node = class.child_by_field_name("name")?;
    let body = class.child_by_field_name("body")?;
    let methods: Vec<TestMethod> = named_children(body)
        .into_iter()
        .filter(|member| member.kind() == "method_declaration")
        .filter_map(|method| test_method(method, source, index, imports))
        .collect();
    if methods.is_empty() {
        return None;
    }
    Some(TestClass {
        binary_fqn: lombok::binary_fqn_of(class, source)?,
        name: node_text(name_node, source).to_string(),
        range: index.range(name_node),
        methods,
    })
}

fn test_method(
    method: Node,
    source: &str,
    index: &LineIndex,
    imports: &Imports,
) -> Option<TestMethod> {
    // Recovery inside the declaration means the annotation/method pairing
    // can't be trusted; static/abstract methods are never JUnit tests.
    if method.has_error()
        || has_modifier(method, source, "static")
        || has_modifier(method, source, "abstract")
    {
        return None;
    }
    let modifiers = modifiers_node(method)?;
    let kind = named_children(modifiers)
        .into_iter()
        .filter(|child| matches!(child.kind(), "annotation" | "marker_annotation"))
        .filter_map(|annotation| annotation.child_by_field_name("name"))
        .find_map(|name| classify_annotation(node_text(name, source), imports))?;
    let name_node = method.child_by_field_name("name")?;
    Some(TestMethod {
        name: node_text(name_node, source).to_string(),
        range: index.range(name_node),
        kind,
    })
}

/// Classify one annotation name against the JUnit families. A dotted name is
/// classified by its literal text; a simple name resolves through the file's
/// imports — an explicit single-type import binds the name exclusively
/// (Java resolution: it shadows wildcards), otherwise every candidate
/// (same package, wildcards, `java.lang`) is considered.
fn classify_annotation(name: &str, imports: &Imports) -> Option<TestKind> {
    if name.contains('.') {
        return classify_fqn(name);
    }
    if !TEST_ANNOTATIONS.contains(&name) {
        return None;
    }
    if let Some(fqn) = imports.single_import(name) {
        return classify_fqn(fqn);
    }
    imports
        .candidates(name)
        .iter()
        .find_map(|candidate| classify_fqn(candidate))
}

fn classify_fqn(fqn: &str) -> Option<TestKind> {
    let simple = fqn.rsplit('.').next().unwrap_or(fqn);
    if !TEST_ANNOTATIONS.contains(&simple) {
        return None;
    }
    if fqn.starts_with("org.junit.jupiter.") {
        Some(TestKind::JUnit5)
    } else if fqn == "org.junit.Test" {
        Some(TestKind::JUnit4)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse, PositionEncoding};

    fn discovered(src: &str) -> Vec<TestClass> {
        let tree = parse(&mut new_parser(), src, None).unwrap();
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        discover_tests(&tree, src, &index)
    }

    fn flat(src: &str) -> Vec<(String, Vec<(String, TestKind)>)> {
        discovered(src)
            .into_iter()
            .map(|class| {
                (
                    class.binary_fqn,
                    class
                        .methods
                        .into_iter()
                        .map(|m| (m.name, m.kind))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn junit5_explicit_import_is_discovered() {
        let src = "package demo;
import org.junit.jupiter.api.Test;
class CalcTest {
    @Test
    void adds() { }
}\n";
        assert_eq!(
            flat(src),
            [(
                "demo.CalcTest".to_string(),
                vec![("adds".to_string(), TestKind::JUnit5)]
            )]
        );
    }

    #[test]
    fn junit5_wildcard_import_is_discovered() {
        let src = "package demo;
import org.junit.jupiter.api.*;
class CalcTest {
    @Test
    void adds() { }
    @ParameterizedTest
    void eachCase(int value) { }
}\n";
        assert_eq!(
            flat(src),
            [(
                "demo.CalcTest".to_string(),
                vec![
                    ("adds".to_string(), TestKind::JUnit5),
                    ("eachCase".to_string(), TestKind::JUnit5),
                ]
            )]
        );
    }

    #[test]
    fn junit4_import_is_discovered() {
        let src = "package demo;
import org.junit.Test;
public class LegacyTest {
    @Test
    public void adds() { }
}\n";
        assert_eq!(
            flat(src),
            [(
                "demo.LegacyTest".to_string(),
                vec![("adds".to_string(), TestKind::JUnit4)]
            )]
        );
    }

    #[test]
    fn fully_qualified_annotation_is_discovered() {
        let src = "package demo;
class QualifiedTest {
    @org.junit.jupiter.api.Test
    void adds() { }
    @org.junit.Test
    public void legacy() { }
}\n";
        assert_eq!(
            flat(src),
            [(
                "demo.QualifiedTest".to_string(),
                vec![
                    ("adds".to_string(), TestKind::JUnit5),
                    ("legacy".to_string(), TestKind::JUnit4),
                ]
            )]
        );
    }

    #[test]
    fn unrelated_test_annotation_is_not_discovered() {
        let src = "package demo;
import com.acme.Test;
class DecoyTest {
    @Test
    void adds() { }
}\n";
        assert!(discovered(src).is_empty());
    }

    #[test]
    fn nested_class_gets_binary_fqn() {
        let src = "package demo;
import org.junit.jupiter.api.Nested;
import org.junit.jupiter.api.Test;
class OuterTest {
    @Test
    void top() { }
    @Nested
    class Inner {
        @Test
        void nested() { }
    }
}\n";
        assert_eq!(
            flat(src),
            [
                (
                    "demo.OuterTest".to_string(),
                    vec![("top".to_string(), TestKind::JUnit5)]
                ),
                (
                    "demo.OuterTest$Inner".to_string(),
                    vec![("nested".to_string(), TestKind::JUnit5)]
                ),
            ]
        );
    }

    #[test]
    fn non_test_file_yields_nothing() {
        let src = "package demo;\nclass Calc { int add(int a, int b) { return a + b; } }\n";
        assert!(discovered(src).is_empty());
    }

    #[test]
    fn damaged_method_is_skipped() {
        let src = "package demo;
import org.junit.jupiter.api.Test;
class BrokenTest {
    @Test
    void broken( { }
    @Test
    void intact() { }
}\n";
        let classes = discovered(src);
        // Only the intact method may survive; the damaged one must never
        // appear (recovery could pair the annotation with the wrong node).
        for class in &classes {
            assert!(
                class.methods.iter().all(|m| m.name == "intact"),
                "{classes:?}"
            );
        }
    }

    #[test]
    fn static_and_abstract_methods_are_skipped() {
        let src = "package demo;
import org.junit.jupiter.api.Test;
abstract class BaseTest {
    @Test
    static void helper() { }
    @Test
    abstract void contract();
    @Test
    void real() { }
}\n";
        assert_eq!(
            flat(src),
            [(
                "demo.BaseTest".to_string(),
                vec![("real".to_string(), TestKind::JUnit5)]
            )]
        );
    }

    #[test]
    fn ranges_cover_name_identifiers() {
        let src = "package demo;
import org.junit.jupiter.api.Test;
class CalcTest {
    @Test
    void adds() { }
}\n";
        let classes = discovered(src);
        assert_eq!(classes.len(), 1);
        let class = &classes[0];
        assert_eq!(class.range.start.line, 2);
        assert_eq!(class.methods[0].range.start.line, 4);
    }
}
