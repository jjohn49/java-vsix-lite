//! Purely syntactic Java diagnostics for files, packages, duplicates, and modifiers.
//! Checks with parse errors stay silent; one tree pass performs all rules.

use std::collections::HashSet;

use ls_types::Diagnostic;
use tree_sitter::{Node, Tree};

use crate::model::{
    children, has_modifier, modifiers_node, named_children, MemberKind, TypeDecl, TypeKind,
};
use crate::{diagnostic, node_text, LineIndex, MAX_DIAGNOSTICS};

/// Run structural checks for one document.
/// Optional filename and expected-package inputs independently enable location checks.
pub fn structural_diagnostics(
    tree: &Tree,
    source: &str,
    index: &LineIndex,
    filename: Option<&str>,
    expected_package: Option<&str>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let root = tree.root_node();

    let top_level: Vec<Node> = named_children(root)
        .into_iter()
        .filter(|n| TypeKind::from_kind(n.kind()).is_some())
        .collect();

    check_filename(&top_level, source, filename, index, &mut out);
    check_duplicate_top_level(&top_level, source, index, &mut out);
    check_package(root, source, expected_package, index, &mut out);

    // Rule (e)/(f): duplicate members and illegal modifiers, checked for
    // every type/method/constructor/field via one stack walk.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => {
                check_repeated_modifiers(node, index, &mut out);
                if node.kind() == "class_declaration" {
                    check_modifier_combo(node, "abstract", "final", index, &mut out);
                }
                if node.kind() == "class_declaration" || node.kind() == "interface_declaration" {
                    check_modifier_combo(node, "sealed", "non-sealed", index, &mut out);
                }
                check_duplicate_members(node, source, index, &mut out);
            }
            "method_declaration" => {
                check_repeated_modifiers(node, index, &mut out);
                check_modifier_combo(node, "abstract", "final", index, &mut out);
                check_modifier_combo(node, "abstract", "private", index, &mut out);
                check_method_body_shape(node, source, index, &mut out);
                if !node.has_error() {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let label = format!("method {}", node_text(name_node, source));
                        check_duplicate_params(node, &label, source, index, &mut out);
                    }
                }
            }
            "constructor_declaration" => {
                check_repeated_modifiers(node, index, &mut out);
                if !node.has_error() {
                    if let Some((enclosing, _)) = enclosing_type(node) {
                        if !enclosing.has_error() {
                            if let Some(name_node) = enclosing.child_by_field_name("name") {
                                let label = format!("constructor {}", node_text(name_node, source));
                                check_duplicate_params(node, &label, source, index, &mut out);
                            }
                        }
                    }
                }
            }
            "field_declaration" | "constant_declaration" => {
                check_repeated_modifiers(node, index, &mut out);
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }

    out.truncate(MAX_DIAGNOSTICS);
    out
}

/// The type declaration a node belongs to.
///
/// An anonymous class has no `*_declaration` node — it is a bare `class_body`
/// hanging off an `object_creation_expression` — so a naive walk to the
/// nearest declaration escapes it and lands on the enclosing type. That made
/// every method of `return new CloseableIterator<T>() { ... };` inside a
/// `static` interface method look like an interface method with a body. An
/// anonymous class is always a class, so its members may have bodies.
fn enclosing_type(node: Node) -> Option<(Node, TypeKind)> {
    let mut cur = node.parent();
    while let Some(p) = cur {
        if p.kind() == "class_body"
            && p.parent()
                .is_some_and(|g| g.kind() == "object_creation_expression")
        {
            return Some((p, TypeKind::Class));
        }
        if let Some(kind) = TypeKind::from_kind(p.kind()) {
            return Some((p, kind));
        }
        cur = p.parent();
    }
    None
}

/// Rule (f): a modifier keyword repeated on one declaration. javac's
/// message is the terse `"repeated modifier"`, pointed at the repeat.
fn check_repeated_modifiers(decl: Node, index: &LineIndex, out: &mut Vec<Diagnostic>) {
    if decl.has_error() {
        return; // the whole declaration is the relevant region, not just `modifiers`
    }
    let Some(mods) = modifiers_node(decl) else {
        return;
    };
    let mut seen: HashSet<&str> = HashSet::new();
    for tok in children(mods) {
        if tok.is_named() {
            continue; // an annotation modifier (`@Override`), not a keyword
        }
        if !seen.insert(tok.kind()) {
            out.push(diagnostic(
                index.range(tok),
                "repeated modifier".to_string(),
            ));
        }
    }
}

/// Rule (f): a forbidden pair of modifier keywords on one declaration.
/// javac's message is `"illegal combination of modifiers: {first} and
/// {second}"`, naming them in source order; pointed at the declaration's
/// name rather than javac's caret column.
fn check_modifier_combo(
    decl: Node,
    a: &str,
    b: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if decl.has_error() {
        return; // the whole declaration is the relevant region, not just `modifiers`
    }
    let Some(mods) = modifiers_node(decl) else {
        return;
    };
    let keywords: Vec<&str> = children(mods)
        .into_iter()
        .filter(|c| !c.is_named())
        .map(|c| c.kind())
        .collect();
    let pos_a = keywords.iter().position(|&k| k == a);
    let pos_b = keywords.iter().position(|&k| k == b);
    let (Some(pa), Some(pb)) = (pos_a, pos_b) else {
        return;
    };
    let (first, second) = if pa < pb { (a, b) } else { (b, a) };
    let Some(name_node) = decl.child_by_field_name("name") else {
        return;
    };
    out.push(diagnostic(
        index.range(name_node),
        format!("illegal combination of modifiers: {first} and {second}"),
    ));
}

/// Rule (f): a method body's legality given its modifiers and enclosing
/// type — two distinct javac messages:
/// - Interface method with a body but no `default`/`static`/`private`:
///   `"interface abstract methods cannot have body"`.
/// - Explicit `abstract` method (class/enum/record) with a body:
///   `"abstract methods cannot have a body"`.
fn check_method_body_shape(
    method: Node,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if method.has_error() {
        return;
    }
    let Some(body) = method.child_by_field_name("body") else {
        return; // no body -> nothing illegal about its shape
    };
    let Some((enclosing, kind)) = enclosing_type(method) else {
        return;
    };
    if enclosing.has_error() {
        return;
    }
    if kind == TypeKind::Interface {
        let allowed = has_modifier(method, source, "default")
            || has_modifier(method, source, "static")
            || has_modifier(method, source, "private");
        if !allowed {
            out.push(diagnostic(
                index.range(body),
                "interface abstract methods cannot have body".to_string(),
            ));
        }
    } else if has_modifier(method, source, "abstract") {
        if let Some(name_node) = method.child_by_field_name("name") {
            out.push(diagnostic(
                index.range(name_node),
                "abstract methods cannot have a body".to_string(),
            ));
        }
    }
}

/// Rule (e): duplicate same-name fields and duplicate method signatures
/// among `ty`'s own directly-declared members; nested types are checked
/// separately when the stack walk visits them.
fn check_duplicate_members(ty: Node, source: &str, index: &LineIndex, out: &mut Vec<Diagnostic>) {
    let Some(decl) = TypeDecl::from_node(ty, source, 0, None) else {
        return;
    };
    let owner_kw = member_owner_keyword(decl.kind);
    let owner_name = decl.name;
    let members = decl.own_members();

    // Only genuine body-declared fields (`variable_declarator` nodes);
    // record components are shaped as `formal_parameter` and excluded.
    let mut seen_fields: HashSet<&str> = HashSet::new();
    for m in members
        .iter()
        .filter(|m| m.kind == MemberKind::Field && m.node.kind() == "variable_declarator")
    {
        // A malformed initializer's `ERROR` node is a sibling of the
        // `variable_declarator`, not a descendant — check the parent too.
        let field_ok = m.node.parent().map(|p| !p.has_error()).unwrap_or(true);
        if m.node.has_error() || !field_ok {
            continue;
        }
        let Some(name_node) = m.node.child_by_field_name("name") else {
            continue;
        };
        if !seen_fields.insert(m.name) {
            out.push(diagnostic(
                index.range(name_node),
                format!(
                    "variable {} is already defined in {owner_kw} {owner_name}",
                    m.name
                ),
            ));
        }
    }

    // Methods: duplicate signatures by exact textual parameter-type
    // match. Varargs or non-plain parameters silence the comparison for
    // that method, staying conservative on ambiguous shapes.
    let mut seen_methods: HashSet<(&str, Vec<&str>)> = HashSet::new();
    for m in members
        .iter()
        .filter(|m| m.kind == MemberKind::Method && m.node.kind() == "method_declaration")
    {
        if m.node.has_error() {
            continue;
        }
        let Some(types) = param_types_text(m.node, source) else {
            continue;
        };
        let key = (m.name, types.clone());
        if !seen_methods.insert(key) {
            let Some(name_node) = m.node.child_by_field_name("name") else {
                continue;
            };
            out.push(diagnostic(
                index.range(name_node),
                format!(
                    "method {}({}) is already defined in {owner_kw} {owner_name}",
                    m.name,
                    types.join(",")
                ),
            ));
        }
    }
}

/// The keyword javac uses in "already defined in {kw} {name}" messages.
/// `@interface` reports as `"class"` here, unlike the rule-(a) filename
/// message where it reports as `"interface"` — a real javac
/// inconsistency, not a bug here.
fn member_owner_keyword(kind: TypeKind) -> &'static str {
    match kind {
        TypeKind::Class | TypeKind::Annotation => "class",
        TypeKind::Interface => "interface",
        TypeKind::Enum => "enum",
        TypeKind::Record => "record",
    }
}

/// The rendered type text of each plain (non-vararg) parameter in
/// `decl`'s parameter list, in order. `None` if any parameter isn't a
/// plain `formal_parameter`, or the list has a parse error.
fn param_types_text<'t>(decl: Node<'t>, source: &'t str) -> Option<Vec<&'t str>> {
    let params = decl.child_by_field_name("parameters")?;
    if params.has_error() {
        return None;
    }
    let mut out = Vec::new();
    for p in named_children(params) {
        if p.kind() != "formal_parameter" {
            return None;
        }
        let ty = p.child_by_field_name("type")?;
        out.push(node_text(ty, source));
    }
    Some(out)
}

/// Rule (e): duplicate formal parameter names within one method/constructor
/// declaration — javac's exact wording is `"variable {name} is already
/// defined in {owner_label}"` (e.g. `"method m"` or `"constructor Foo"`).
fn check_duplicate_params(
    decl: Node,
    owner_label: &str,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(params) = decl.child_by_field_name("parameters") else {
        return;
    };
    if params.has_error() {
        return;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for p in named_children(params) {
        if p.has_error() {
            continue;
        }
        if p.kind() != "formal_parameter" && p.kind() != "spread_parameter" {
            continue;
        }
        let Some(name_node) = p.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if !seen.insert(name) {
            out.push(diagnostic(
                index.range(name_node),
                format!("variable {name} is already defined in {owner_label}"),
            ));
        }
    }
}

/// Rule (a)/(b) (JLS §7.6): every public top-level type whose name
/// doesn't match `filename` is an error. Checking every type, not just
/// the first, also catches rule (b) ("more than one public type per
/// file") for free, since at most one can match the file's stem.
fn check_filename(
    top_level: &[Node],
    source: &str,
    filename: Option<&str>,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(filename) = filename else { return };
    let Some(stem) = filename.strip_suffix(".java") else {
        return;
    };
    for &ty in top_level {
        if ty.has_error() {
            continue; // ambiguous region — stay silent
        }
        let Some(kind) = TypeKind::from_kind(ty.kind()) else {
            continue;
        };
        if !has_modifier(ty, source, "public") {
            continue; // legal: only public top-level types are constrained
        }
        let Some(name_node) = ty.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if name == stem {
            continue;
        }
        let keyword = filename_keyword(kind);
        out.push(diagnostic(
            index.range(name_node),
            format!("{keyword} {name} is public, should be declared in a file named {name}.java"),
        ));
    }
}

/// The keyword javac uses in the rule-(a) "is public, should be
/// declared..." message. `record` reports as `"class"`; `@interface`
/// reports as `"interface"`.
fn filename_keyword(kind: TypeKind) -> &'static str {
    match kind {
        TypeKind::Class | TypeKind::Record => "class",
        TypeKind::Interface | TypeKind::Annotation => "interface",
        TypeKind::Enum => "enum",
    }
}

/// Rule (d) (JLS §7.6): two top-level types sharing a name is always an
/// error, regardless of kind — javac's message is literally `"duplicate
/// class: {name}"` even when one or both are interfaces/enums/records.
fn check_duplicate_top_level(
    top_level: &[Node],
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let mut seen: HashSet<&str> = HashSet::new();
    for &ty in top_level {
        if ty.has_error() {
            continue;
        }
        let Some(name_node) = ty.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if !seen.insert(name) {
            out.push(diagnostic(
                index.range(name_node),
                format!("duplicate class: {name}"),
            ));
        }
    }
}

/// Rule (c): the file's declared package vs. what its location under a
/// discovered source root implies. Uses IDE-style (Eclipse/IntelliJ)
/// wording, not javac's — a batch `javac` invocation never errors on
/// this at all.
fn check_package(
    root: Node,
    source: &str,
    expected_package: Option<&str>,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(expected) = expected_package else {
        return;
    };
    let mut cursor = root.walk();
    let decls: Vec<Node> = root
        .children(&mut cursor)
        .filter(|c| c.kind() == "package_declaration")
        .collect();
    if decls.len() != 1 {
        // 2+ package declarations is ambiguous for this check: stay
        // silent rather than guess which one is authoritative.
        if decls.is_empty() {
            // A compilation unit that declares no type cannot be in the
            // wrong place: an empty file is legal Java (JLS 7.3) and javac
            // accepts it, as does a file holding only comments or imports.
            // cbioportal ships a 0-byte `DataAccessTokenConfig.java`.
            let declares_a_type = named_children(root)
                .iter()
                .any(|child| TypeKind::from_kind(child.kind()).is_some());
            // No node to scope an error-guard to for an absent
            // declaration, so fall back to checking the whole file.
            if declares_a_type && !expected.is_empty() && !root.has_error() {
                out.push(diagnostic(
                    index.range(root.child(0).unwrap_or(root)),
                    format!(
                        "The declared package \"\" does not match the expected package \"{expected}\""
                    ),
                ));
            }
            return;
        }
        return;
    }
    let decl = decls[0];
    if decl.has_error() {
        return;
    }
    let Some(actual) = package_path(decl, source) else {
        return;
    };
    if actual != expected {
        out.push(diagnostic(
            index.range(decl),
            format!(
                "The declared package \"{actual}\" does not match the expected package \"{expected}\""
            ),
        ));
    }
}

/// The dotted package path text of a `package_declaration` node — its
/// child identifier/scoped-identifier, taken verbatim from source.
fn package_path<'t>(decl: Node<'t>, source: &'t str) -> Option<&'t str> {
    let mut cursor = decl.walk();
    let path_node = decl
        .children(&mut cursor)
        .find(|c| c.kind() == "identifier" || c.kind() == "scoped_identifier")?;
    Some(node_text(path_node, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse, PositionEncoding};

    fn diags(src: &str, filename: Option<&str>, expected_package: Option<&str>) -> Vec<String> {
        let tree = parse(&mut new_parser(), src, None).unwrap();
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        structural_diagnostics(&tree, src, &index, filename, expected_package)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    // ---- rule (a)/(b): public top-level type name vs filename ----

    #[test]
    fn filename_matching_public_type_name_is_silent() {
        let msgs = diags("public class Foo {}\n", Some("Foo.java"), None);
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn filename_mismatched_public_class_is_flagged_with_javac_wording() {
        let msgs = diags(
            "public class MavenDemo3 {}\n",
            Some("MavenDemo2.java"),
            None,
        );
        assert_eq!(
            msgs,
            vec![
                "class MavenDemo3 is public, should be declared in a file named MavenDemo3.java"
                    .to_string()
            ]
        );
    }

    #[test]
    fn filename_mismatched_public_interface_uses_interface_keyword() {
        let msgs = diags("public interface Foo {}\n", Some("Bar.java"), None);
        assert_eq!(
            msgs,
            vec![
                "interface Foo is public, should be declared in a file named Foo.java".to_string()
            ]
        );
    }

    #[test]
    fn filename_mismatched_public_enum_uses_enum_keyword() {
        let msgs = diags("public enum Foo { A, B }\n", Some("Bar.java"), None);
        assert_eq!(
            msgs,
            vec!["enum Foo is public, should be declared in a file named Foo.java".to_string()]
        );
    }

    #[test]
    fn filename_mismatched_public_record_uses_class_keyword() {
        let msgs = diags("public record Foo(int x) {}\n", Some("Bar.java"), None);
        assert_eq!(
            msgs,
            vec!["class Foo is public, should be declared in a file named Foo.java".to_string()]
        );
    }

    #[test]
    fn filename_mismatched_public_annotation_type_uses_interface_keyword() {
        let msgs = diags("public @interface Foo {}\n", Some("Bar.java"), None);
        assert_eq!(
            msgs,
            vec![
                "interface Foo is public, should be declared in a file named Foo.java".to_string()
            ]
        );
    }

    #[test]
    fn non_public_top_level_type_with_different_name_is_legal() {
        let msgs = diags("class Foo {}\n", Some("Bar.java"), None);
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn missing_filename_input_is_silent() {
        let msgs = diags("public class MavenDemo3 {}\n", None, None);
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn second_mismatched_public_type_is_flagged_first_matching_is_not() {
        let msgs = diags(
            "public class Only {}\npublic class Second {}\n",
            Some("Only.java"),
            None,
        );
        assert_eq!(
            msgs,
            vec![
                "class Second is public, should be declared in a file named Second.java"
                    .to_string()
            ]
        );
    }

    #[test]
    fn filename_check_stays_silent_when_the_type_itself_has_a_parse_error() {
        // The malformed body means the whole `class_declaration` carries
        // the error, even though name/modifiers are intact.
        let msgs = diags(
            "public class Wrong {\n  int x = ;\n}\n",
            Some("Other.java"),
            None,
        );
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    // ---- rule (d): duplicate top-level type names ----

    #[test]
    fn duplicate_top_level_class_names_flags_the_second() {
        let msgs = diags("class Dup {}\nclass Dup {}\n", None, None);
        assert_eq!(msgs, vec!["duplicate class: Dup".to_string()]);
    }

    #[test]
    fn duplicate_top_level_across_kinds_still_flags() {
        let msgs = diags("class Dup {}\ninterface Dup {}\n", None, None);
        assert_eq!(msgs, vec!["duplicate class: Dup".to_string()]);
    }

    #[test]
    fn distinct_top_level_names_are_silent() {
        let msgs = diags("class A {}\nclass B {}\n", None, None);
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn duplicate_top_level_stays_silent_when_a_declaration_has_a_parse_error() {
        let msgs = diags("class Dup {\n  int x = ;\n}\nclass Dup {}\n", None, None);
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    // ---- rule (c): package vs. expected directory ----

    #[test]
    fn package_matching_expected_is_silent() {
        let msgs = diags("package com.x;\nclass C {}\n", None, Some("com.x"));
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn package_mismatching_expected_is_flagged() {
        let msgs = diags("package com.x;\nclass C {}\n", None, Some("com.y"));
        assert_eq!(
            msgs,
            vec![
                "The declared package \"com.x\" does not match the expected package \"com.y\""
                    .to_string()
            ]
        );
    }

    #[test]
    fn package_check_skips_when_expected_package_is_none() {
        let msgs = diags("package com.x;\nclass C {}\n", None, None);
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn missing_package_declaration_matching_default_expectation_is_silent() {
        let msgs = diags("class C {}\n", None, Some(""));
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn missing_package_declaration_when_one_is_expected_is_flagged() {
        let msgs = diags("class C {}\n", None, Some("com.x"));
        assert_eq!(
            msgs,
            vec![
                "The declared package \"\" does not match the expected package \"com.x\""
                    .to_string()
            ]
        );
    }

    #[test]
    fn package_check_stays_silent_when_the_declaration_has_a_parse_error() {
        // A malformed package declaration (missing semicolon) carries the
        // error onto the `package_declaration` node itself.
        let src = "package com x\nclass C {}\n";
        let msgs = diags(src, None, Some("com.y"));
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    fn structural(src: &str) -> Vec<String> {
        diags(src, None, None)
    }

    // ---- rule (e1): duplicate fields ----

    #[test]
    fn duplicate_field_names_flags_the_second() {
        let msgs = structural("class DupField {\n    int x;\n    int x;\n}\n");
        assert_eq!(
            msgs,
            vec!["variable x is already defined in class DupField".to_string()]
        );
    }

    #[test]
    fn duplicate_field_in_interface_uses_interface_keyword() {
        let msgs = structural("interface DupConst {\n    int X = 1;\n    int X = 2;\n}\n");
        assert_eq!(
            msgs,
            vec!["variable X is already defined in interface DupConst".to_string()]
        );
    }

    #[test]
    fn field_and_method_sharing_a_name_is_not_a_duplicate() {
        let msgs = structural("class C {\n    int x;\n    void x() {}\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn distinct_field_names_are_silent() {
        let msgs = structural("class C {\n    int x;\n    int y;\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn duplicate_field_stays_silent_when_one_declarator_has_a_parse_error() {
        // The missing initializer's `ERROR` node is a sibling under the
        // `field_declaration`, not inside the `variable_declarator`.
        let msgs = structural("class C {\n    int x = ;\n    int x;\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    // ---- rule (e2): duplicate method signatures ----

    #[test]
    fn duplicate_no_arg_method_flags_the_second() {
        let msgs = structural("class DupNoArg {\n    void m() {}\n    void m() {}\n}\n");
        assert_eq!(
            msgs,
            vec!["method m() is already defined in class DupNoArg".to_string()]
        );
    }

    #[test]
    fn duplicate_multi_arg_generic_method_renders_types_verbatim() {
        let msgs = structural(
            "import java.util.List;\nclass DupMulti {\n    \
             void m(List<String> a, int b) {}\n    void m(List<String> a, int b) {}\n}\n",
        );
        assert_eq!(
            msgs,
            vec!["method m(List<String>,int) is already defined in class DupMulti".to_string()]
        );
    }

    #[test]
    fn duplicate_method_in_interface_uses_interface_keyword() {
        let msgs =
            structural("interface DupIfaceMethod {\n    void m(int a);\n    void m(int a);\n}\n");
        assert_eq!(
            msgs,
            vec!["method m(int) is already defined in interface DupIfaceMethod".to_string()]
        );
    }

    #[test]
    fn overload_with_different_param_types_is_silent() {
        let msgs = structural("class C {\n    void m(int a) {}\n    void m(String a) {}\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn overload_with_different_arity_is_silent() {
        let msgs = structural("class C {\n    void m(int a) {}\n    void m(int a, int b) {}\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn varargs_method_is_never_compared_conservatively() {
        // Same signature modulo varargs shape: varargs mutes the
        // comparison rather than risk a false match.
        let msgs = structural("class C {\n    void m(int... a) {}\n    void m(int... a) {}\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn duplicate_method_stays_silent_when_one_declaration_has_a_parse_error() {
        // The missing semicolon leaves an `ERROR` node inside the first
        // method's `block`, so its `method_declaration` carries the error.
        let msgs = structural("class C {\n    void m(int a) { return }\n    void m(int a) {}\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    // ---- rule (e3): duplicate formal parameter names ----

    #[test]
    fn duplicate_method_parameter_name_is_flagged() {
        let msgs = structural("class DupParam {\n    void m(int a, int a) {}\n}\n");
        assert_eq!(
            msgs,
            vec!["variable a is already defined in method m".to_string()]
        );
    }

    #[test]
    fn duplicate_constructor_parameter_name_uses_constructor_wording() {
        let msgs = structural("class DupCtorParam {\n    DupCtorParam(int a, int a) {}\n}\n");
        assert_eq!(
            msgs,
            vec!["variable a is already defined in constructor DupCtorParam".to_string()]
        );
    }

    #[test]
    fn distinct_parameter_names_are_silent() {
        let msgs = structural("class C {\n    void m(int a, int b) {}\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn duplicate_parameter_stays_silent_when_the_parameter_list_has_a_parse_error() {
        let msgs = structural("class C {\n    void m(int a, ) {}\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    // ---- rule (f): illegal modifier combinations ----

    #[test]
    fn abstract_final_method_is_flagged() {
        let msgs = structural("abstract class AbsFinal {\n    abstract final void m();\n}\n");
        assert_eq!(
            msgs,
            vec!["illegal combination of modifiers: abstract and final".to_string()]
        );
    }

    #[test]
    fn abstract_private_method_is_flagged() {
        let msgs = structural("abstract class AbsPriv {\n    abstract private void m();\n}\n");
        assert_eq!(
            msgs,
            vec!["illegal combination of modifiers: abstract and private".to_string()]
        );
    }

    #[test]
    fn abstract_final_class_is_flagged() {
        let msgs = structural("abstract final class AbsFinalClass {\n}\n");
        assert_eq!(
            msgs,
            vec!["illegal combination of modifiers: abstract and final".to_string()]
        );
    }

    #[test]
    fn sealed_non_sealed_class_is_flagged() {
        let msgs = structural(
            "sealed non-sealed class SealedNs permits Foo {\n}\nclass Foo extends SealedNs {}\n",
        );
        assert_eq!(
            msgs,
            vec!["illegal combination of modifiers: sealed and non-sealed".to_string()]
        );
    }

    #[test]
    fn plain_abstract_method_is_silent() {
        let msgs = structural("abstract class A {\n    abstract void m();\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn modifier_combo_stays_silent_when_the_declaration_has_a_parse_error() {
        // The missing semicolon puts an `ERROR` node inside the method's
        // `block`, so the whole `method_declaration` carries the error.
        let msgs =
            structural("abstract class C {\n    abstract final void m(int a) { return }\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn duplicate_modifier_keyword_is_flagged() {
        let msgs = structural("public public class DupMod {\n}\n");
        assert_eq!(msgs, vec!["repeated modifier".to_string()]);
    }

    #[test]
    fn repeated_modifier_stays_silent_when_the_declaration_has_a_parse_error() {
        // The malformed field initializer means the whole
        // `class_declaration` carries the error.
        let msgs = structural("public public class DupMod {\n    int x = ;\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn distinct_modifiers_are_silent() {
        let msgs = structural("public final class C {\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn interface_method_with_body_and_no_default_static_private_is_flagged() {
        let msgs = structural("interface IfaceBody {\n    void m() { }\n}\n");
        assert_eq!(
            msgs,
            vec!["interface abstract methods cannot have body".to_string()]
        );
    }

    /// An anonymous class inside a `static` interface method is a *class*,
    /// so its methods may have bodies. The enclosing-type walk used to skip
    /// the anonymous `class_body` — it has no `*_declaration` node — and
    /// land on the interface, flagging every method of
    /// `return new CloseableIterator<T>() { ... };`.
    #[test]
    fn anonymous_class_in_an_interface_method_may_have_method_bodies() {
        let msgs = structural(
            "interface Iter {\n  static Iter empty() {\n    return new Iter() {\n      public void run() { }\n    };\n  }\n  void run();\n}\n",
        );
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    /// A compilation unit that declares no type cannot be in the wrong
    /// package. An empty file is legal Java (JLS 7.3) and javac accepts it;
    /// cbioportal ships a 0-byte one.
    #[test]
    fn typeless_compilation_unit_has_no_package_mismatch() {
        assert!(diags("", Some("Empty.java"), Some("demo")).is_empty());
        assert!(diags("// only a comment\n", Some("C.java"), Some("demo")).is_empty());
    }

    /// A file that does declare a type still has to sit where its package
    /// says, so the narrowing above must not silence the real case.
    #[test]
    fn missing_package_declaration_is_flagged_when_a_type_is_declared() {
        let msgs = diags("class A {}\n", Some("A.java"), Some("demo"));
        assert_eq!(
            msgs,
            vec![
                "The declared package \"\" does not match the expected package \"demo\""
                    .to_string()
            ]
        );
    }

    #[test]
    fn method_body_shape_stays_silent_when_the_declaration_has_a_parse_error() {
        // The missing semicolon leaves an `ERROR` node inside the
        // method's `block`, so the check must stay silent.
        let msgs = structural("interface I {\n    void m() { return }\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn interface_default_method_with_body_is_silent() {
        let msgs = structural("interface I {\n    default void m() { }\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn interface_static_method_with_body_is_silent() {
        let msgs = structural("interface I {\n    static void m() { }\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn interface_private_method_with_body_is_silent() {
        let msgs = structural("interface I {\n    private void m() { }\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn interface_method_without_body_is_silent() {
        let msgs = structural("interface I {\n    void m();\n}\n");
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn abstract_method_with_body_in_a_class_is_flagged() {
        let msgs = structural("abstract class AbsBody {\n    abstract void m() { }\n}\n");
        assert_eq!(
            msgs,
            vec!["abstract methods cannot have a body".to_string()]
        );
    }
}
