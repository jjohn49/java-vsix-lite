//! Conservative semantic diagnostics: resolvable member accesses, method
//! returns, variable/field initializers, unreachable statements, and unused
//! code. Resolution/analysis must be complete before a diagnostic is emitted,
//! so unknown project/classpath types, overloads, and recovery regions stay
//! silent.

use ls_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString};
use tree_sitter::Node;

use crate::external::SymbolSource;
use crate::imports::Imports;
use crate::model::TypeTable;
use crate::resolve::{self, Ctx, ResolvedType};
use crate::{diagnostic, node_text, LineIndex, OpenDoc, MAX_DIAGNOSTICS};

/// Stable LSP code for a proven incompatible method return.
pub const INCOMPATIBLE_RETURN_CODE: &str = "jvl.incompatibleReturn";

/// Stable LSP code for a proven incompatible variable/field initializer.
pub const INCOMPATIBLE_ASSIGNMENT_CODE: &str = "jvl.incompatibleAssignment";

/// Stable LSP code for a statement that can never execute.
pub const UNREACHABLE_CODE: &str = "jvl.unreachable";

/// Stable LSP code for unused locals, parameters, and private members.
const UNUSED_CODE: &str = "jvl.unused";

/// Immediate semantic diagnostics for `docs[current]`.
///
/// Return, initializer, and unreachable checks always run.
/// `unresolved_members` gates only the unresolved-member rule and `unused`
/// only the unused-code rule (both default-on init options, opt-out only).
pub fn semantic_diagnostics(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    symbols: &dyn SymbolSource,
    unresolved_members: bool,
    unused: bool,
) -> Vec<Diagnostic> {
    let Some(doc) = docs.get(current) else {
        return Vec::new();
    };
    let table = TypeTable::build(docs, current);
    let imports = Imports::parse(doc.tree, doc.source);
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
    };

    let mut out = Vec::new();
    let root = doc.tree.root_node();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }

        match node.kind() {
            "return_statement" => check_return(node, &ctx, index, &mut out),
            "local_variable_declaration" => {
                check_initializers(node, &ctx, index, &mut out);
                if unused {
                    check_unused_locals(node, doc.source, index, &mut out);
                }
            }
            "field_declaration" => {
                check_initializers(node, &ctx, index, &mut out);
                if unused {
                    check_unused_private_fields(node, root, doc.source, index, &mut out);
                }
            }
            "block" => check_unreachable(node, index, &mut out),
            "method_declaration" if unused => {
                check_unused_method(node, root, doc.source, index, &mut out)
            }
            "constructor_declaration" if unused => {
                check_unused_parameters(node, doc.source, index, &mut out)
            }
            "field_access" if unresolved_members => {
                check_member(node, "field", "field", &ctx, index, &mut out)
            }
            // Only qualified calls (`recv.name()`); unqualified calls may be
            // inherited or statically imported.
            "method_invocation"
                if unresolved_members && node.child_by_field_name("object").is_some() =>
            {
                check_member(node, "name", "method", &ctx, index, &mut out)
            }
            _ => {}
        }
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }

        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out
}

fn check_member(
    node: Node,
    member_field: &str,
    kind_word: &str,
    ctx: &Ctx,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let (Some(object), Some(member_node)) = (
        node.child_by_field_name("object"),
        node.child_by_field_name(member_field),
    ) else {
        return;
    };
    if member_node.kind() != "identifier" {
        return; // mid-edit / missing member
    }
    let member = node_text(member_node, ctx.doc.source);

    // A type-cased segment after a dot is a *nested type* or static-member
    // reference (`Map.Entry`, `Map.Entry::getKey`, `Outer.Inner`), not an
    // instance member — those live in the type namespace this member check
    // doesn't model, so never flag them (conservative-by-design). camelCase
    // members and SCREAMING_CASE constants (no lowercase) stay checked.
    if member_field == "field" && crate::looks_like_type_name(member) {
        return;
    }

    // Receiver type must resolve; otherwise we cannot know its members.
    let Some(resolved) = resolve::resolve_receiver_type(object, ctx) else {
        return;
    };
    // A receiver that resolves to `java.lang.Object` is almost always the
    // erased fallback of generic inference we couldn't fully carry through a
    // chain (`list.stream().findFirst().orElseThrow()`, a raw type variable),
    // not a genuine `Object`-typed value. Flagging members on it produces a
    // flood of false positives on ordinary generic code, so stay silent —
    // consistent with this module's conservative-by-design policy.
    if matches!(&resolved.ty, ResolvedType::External { fqn, .. } if fqn == "java.lang.Object") {
        return;
    }

    let (names, complete) = resolve::member_names(&resolved, ctx);
    if complete && !names.contains(member) {
        out.push(diagnostic(
            index.range(member_node),
            format!("Cannot resolve {kind_word} '{member}'"),
        ));
    }
}

fn check_return<'t>(
    return_statement: Node<'t>,
    ctx: &Ctx<'_, 't>,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(method) = nearest_method(return_statement) else {
        return;
    };
    if method.has_error() {
        return;
    }
    let Some(return_type) = method.child_by_field_name("type") else {
        return;
    };
    let expression = {
        let mut cursor = return_statement.walk();
        let expression = return_statement
            .named_children(&mut cursor)
            .find(|child| !matches!(child.kind(), "line_comment" | "block_comment"));
        expression
    };
    let returns_void = node_text(return_type, ctx.doc.source).trim() == "void";

    match (returns_void, expression) {
        (true, Some(_)) => out.push(coded_diagnostic(
            index.range(return_statement),
            INCOMPATIBLE_RETURN_CODE,
            "incompatible types: unexpected return value".to_string(),
        )),
        (false, None) => out.push(coded_diagnostic(
            index.range(return_statement),
            INCOMPATIBLE_RETURN_CODE,
            "incompatible types: missing return value".to_string(),
        )),
        (true, None) => {}
        (false, Some(expression)) => {
            let Some(expected) = resolve::resolve_type_node(return_type, ctx.doc.source, ctx)
            else {
                return;
            };
            let Some(actual) = resolve::resolve_expression_type(expression, ctx) else {
                return;
            };
            if resolve::is_assignable(&actual, &expected, ctx) == Some(false) {
                out.push(coded_diagnostic(
                    index.range(expression),
                    INCOMPATIBLE_RETURN_CODE,
                    format!(
                        "incompatible types: {} cannot be converted to {}",
                        resolve::type_display(&actual),
                        resolve::type_display(&expected)
                    ),
                ));
            }
        }
    }
}

fn nearest_method<'t>(return_statement: Node<'t>) -> Option<Node<'t>> {
    let mut ancestor = return_statement.parent();
    while let Some(node) = ancestor {
        match node.kind() {
            "method_declaration" => return Some(node),
            "lambda_expression" | "constructor_declaration" | "compact_constructor_declaration" => {
                return None
            }
            _ => ancestor = node.parent(),
        }
    }
    None
}

/// [`diagnostic`] with a stable LSP `code` attached.
fn coded_diagnostic(range: ls_types::Range, code: &str, message: String) -> Diagnostic {
    let mut diagnostic = diagnostic(range, message);
    diagnostic.code = Some(NumberOrString::String(code.to_string()));
    diagnostic
}

/// A `jvl.unused` warning: WARNING severity plus the Unnecessary tag so
/// clients fade the range instead of squiggling it.
fn unused_diagnostic(range: ls_types::Range, message: String) -> Diagnostic {
    let mut diagnostic = coded_diagnostic(range, UNUSED_CODE, message);
    diagnostic.severity = Some(DiagnosticSeverity::WARNING);
    diagnostic.tags = Some(vec![DiagnosticTag::UNNECESSARY]);
    diagnostic
}

/// Incompatible-initializer rule: for each declarator with a `value` inside a
/// local variable or field declaration, flag a proven mismatch between the
/// declared type and the initializer's type. Unknown resolution (`None`)
/// stays silent — only `Some(false)` flags — and recovery inside the
/// declaration mutes it (`has_error` propagates from ERROR/MISSING
/// descendants). `var` has no declared type to check.
fn check_initializers<'t>(
    declaration: Node<'t>,
    ctx: &Ctx<'_, 't>,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if declaration.has_error() {
        return;
    }
    let Some(type_node) = declaration.child_by_field_name("type") else {
        return;
    };
    if node_text(type_node, ctx.doc.source) == "var" {
        return;
    }
    let Some(expected) = resolve::resolve_type_node(type_node, ctx.doc.source, ctx) else {
        return;
    };
    let mut cursor = declaration.walk();
    for declarator in declaration.children_by_field_name("declarator", &mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        let Some(value) = declarator.child_by_field_name("value") else {
            continue;
        };
        let Some(actual) = resolve::resolve_expression_type(value, ctx) else {
            continue;
        };
        if resolve::is_assignable(&actual, &expected, ctx) == Some(false) {
            out.push(coded_diagnostic(
                index.range(value),
                INCOMPATIBLE_ASSIGNMENT_CODE,
                format!(
                    "incompatible types: {} cannot be converted to {}",
                    resolve::type_display(&actual),
                    resolve::type_display(&expected)
                ),
            ));
        }
    }
}

/// Unreachable-statement rule: only plain `block` nodes are inspected (switch
/// case groups are a different grammar node and stay silent, conservatively).
/// A `return`/`throw`/`break`/`continue` statement followed by a later named
/// non-comment sibling marks the first such sibling dead. One diagnostic per
/// block; recovery anywhere inside the block mutes it.
fn check_unreachable(block: Node, index: &LineIndex, out: &mut Vec<Diagnostic>) {
    if block.has_error() {
        return;
    }
    let mut cursor = block.walk();
    let mut terminated = false;
    for statement in block.named_children(&mut cursor) {
        if matches!(statement.kind(), "line_comment" | "block_comment") {
            continue;
        }
        if terminated {
            out.push(coded_diagnostic(
                index.range(statement),
                UNREACHABLE_CODE,
                "unreachable statement".to_string(),
            ));
            return;
        }
        terminated = matches!(
            statement.kind(),
            "return_statement" | "throw_statement" | "break_statement" | "continue_statement"
        );
    }
}

/// Whether `name` occurs as an `identifier` anywhere under `scope` other than
/// at the declaration's own name node. Purely name-occurrence based — field
/// accesses, method invocations, and `::name` method references all carry
/// `identifier` nodes — so shadowing can only cause silence, never a false
/// warning.
fn name_used(scope: Node, name_node: Node, name: &str, source: &str) -> bool {
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier"
            && node.id() != name_node.id()
            && node_text(node, source) == name
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    false
}

/// The body a local's usage scan covers: the enclosing method or constructor
/// body, a `static` initializer's block, or a bare instance initializer block
/// (a `block` directly under `class_body`). A lambda-local resolves to the
/// enclosing method body — a superset scope can only cause silence.
fn local_scope<'t>(declaration: Node<'t>) -> Option<Node<'t>> {
    let mut node = declaration;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "method_declaration"
            | "constructor_declaration"
            | "static_initializer"
            | "class_body" => {
                return matches!(node.kind(), "block" | "constructor_body").then_some(node);
            }
            _ => node = parent,
        }
    }
    None
}

/// The declaration's `modifiers` child, if present.
fn find_modifiers<'t>(declaration: Node<'t>) -> Option<Node<'t>> {
    let mut cursor = declaration.walk();
    let found = declaration
        .named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers");
    found
}

/// Whether the declaration's modifier list contains the bare `modifier`
/// keyword (`private`, `static`, …).
fn has_modifier(declaration: Node, source: &str, modifier: &str) -> bool {
    let Some(modifiers) = find_modifiers(declaration) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    let found = modifiers
        .children(&mut cursor)
        .any(|m| node_text(m, source) == modifier);
    found
}

/// Whether the declaration carries any annotation (marker or full form).
fn has_annotation(declaration: Node) -> bool {
    let Some(modifiers) = find_modifiers(declaration) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    let found = modifiers
        .children(&mut cursor)
        .any(|m| matches!(m.kind(), "annotation" | "marker_annotation"));
    found
}

/// Whether the declaration carries `@Override` specifically.
fn has_override(declaration: Node, source: &str) -> bool {
    let Some(modifiers) = find_modifiers(declaration) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    let found = modifiers.children(&mut cursor).any(|m| {
        matches!(m.kind(), "annotation" | "marker_annotation")
            && m.child_by_field_name("name")
                .map(|name| node_text(name, source) == "Override")
                .unwrap_or(false)
    });
    found
}

/// Unused-local rule: a declarator name that never occurs as an identifier
/// elsewhere in the enclosing method (or initializer block) body is dead.
/// Recovery anywhere in that scan scope mutes the check — occurrences inside
/// a damaged region can't be trusted.
fn check_unused_locals(
    declaration: Node,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(scope) = local_scope(declaration) else {
        return;
    };
    if scope.has_error() {
        return;
    }
    let mut cursor = declaration.walk();
    for declarator in declaration.children_by_field_name("declarator", &mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if !name_used(scope, name_node, name, source) {
            out.push(unused_diagnostic(
                index.range(name_node),
                format!("unused local variable '{name}'"),
            ));
        }
    }
}

/// Unused-private-field rule: the usage scan is the whole file (identifiers,
/// field accesses, and `::name` references all count), so recovery anywhere
/// in the file mutes it. Annotated members and `serialVersionUID` (a
/// reflective serialization contract) are exempt.
fn check_unused_private_fields(
    field: Node,
    root: Node,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if root.has_error() {
        return;
    }
    if !has_modifier(field, source, "private") || has_annotation(field) {
        return;
    }
    let mut cursor = field.walk();
    for declarator in field.children_by_field_name("declarator", &mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if name == "serialVersionUID" {
            continue;
        }
        if !name_used(root, name_node, name, source) {
            out.push(unused_diagnostic(
                index.range(name_node),
                format!("unused private field '{name}'"),
            ));
        }
    }
}

/// Unused checks for a `method_declaration`: an unreferenced private method
/// (any annotation exempts it — `@Override` included), and unused parameters
/// on `private` or `static` methods with bodies. `@Override`-annotated
/// methods and `main` keep their parameters — the signature is an inherited
/// or external contract.
fn check_unused_method(
    method: Node,
    root: Node,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = method.child_by_field_name("name") else {
        return;
    };
    let name = node_text(name_node, source);
    let is_private = has_modifier(method, source, "private");

    if is_private
        && !has_annotation(method)
        && !root.has_error()
        && !name_used(root, name_node, name, source)
    {
        out.push(unused_diagnostic(
            index.range(name_node),
            format!("unused private method '{name}'"),
        ));
    }

    if (is_private || has_modifier(method, source, "static"))
        && !has_override(method, source)
        && name != "main"
    {
        check_unused_parameters(method, source, index, out);
    }
}

/// Unused-parameter scan for a method or constructor WITH a body: a formal
/// parameter whose name never occurs in the body is dead. Bodyless
/// (`abstract`/`native`) signatures are external contracts and stay silent;
/// catch and lambda parameters are different grammar nodes and never reach
/// here. Constructors are always eligible — they cannot be overridden.
fn check_unused_parameters(
    callable: Node,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(body) = callable.child_by_field_name("body") else {
        return;
    };
    if body.has_error() {
        return;
    }
    let Some(parameters) = callable.child_by_field_name("parameters") else {
        return;
    };
    if parameters.has_error() {
        return;
    }
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        if parameter.kind() != "formal_parameter" {
            continue;
        }
        let Some(name_node) = parameter.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if !name_used(body, name_node, name, source) {
            out.push(unused_diagnostic(
                index.range(name_node),
                format!("unused parameter '{name}'"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use ls_types::{DiagnosticSeverity, DiagnosticTag, NumberOrString, Range};

    /// Resolves `java.lang.Object` (so the conservative policy can flag) plus any
    /// extra classes provided.
    struct ObjectAware(Vec<(&'static str, Vec<&'static str>, Vec<&'static str>)>);

    impl SymbolSource for ObjectAware {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            let make = |supers: &[&str], members: &[&str]| ExternalClass {
                supers: supers.iter().map(|s| s.to_string()).collect(),
                type_params: Vec::new(),
                members: members
                    .iter()
                    .map(|n| ExternalMember {
                        name: n.to_string(),
                        kind: ExternalMemberKind::Method,
                        signature: format!("{n}()"),
                        template: None,
                        is_static: false,
                        ret_fqn: None,
                        ret_display: None,
                    })
                    .collect(),
            };
            if fqn == "java.lang.Object" {
                return Some(make(&[], &["toString", "equals", "hashCode", "getClass"]));
            }
            self.0
                .iter()
                .find(|(f, _, _)| *f == fqn)
                .map(|(_, supers, members)| make(supers, members))
        }
    }

    fn semantic(
        src: &str,
        symbols: &dyn SymbolSource,
        unresolved_members: bool,
        unused: bool,
    ) -> Vec<Diagnostic> {
        semantic_for_sources(&[src], 0, symbols, unresolved_members, unused)
    }

    fn semantic_for_sources(
        sources: &[&str],
        current: usize,
        symbols: &dyn SymbolSource,
        unresolved_members: bool,
        unused: bool,
    ) -> Vec<Diagnostic> {
        let mut parser = new_parser();
        let trees: Vec<_> = sources
            .iter()
            .map(|src| parse(&mut parser, src, None).unwrap())
            .collect();
        let docs: Vec<_> = sources
            .iter()
            .zip(&trees)
            .map(|(source, tree)| OpenDoc { source, tree })
            .collect();
        let index = LineIndex::new(sources[current], PositionEncoding::Utf16);
        semantic_diagnostics(&docs, current, &index, symbols, unresolved_members, unused)
    }

    fn diags(src: &str, symbols: &dyn SymbolSource) -> Vec<String> {
        semantic(src, symbols, true, false)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    fn return_messages(src: &str, symbols: &dyn SymbolSource) -> Vec<String> {
        semantic(src, symbols, false, false)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    fn range_of(src: &str, needle: &str) -> Range {
        let start = src.find(needle).expect("range marker present");
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        Range {
            start: index.position(start),
            end: index.position(start + needle.len()),
        }
    }

    fn has_recovery(src: &str, missing: bool) -> bool {
        let tree = parse(&mut new_parser(), src, None).unwrap();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if (missing && node.is_missing()) || (!missing && node.kind() == "ERROR") {
                return true;
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        false
    }

    #[test]
    fn return_diagnostic_has_exact_contract() {
        let src = "class C { boolean m() { return 1; } }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(
            diagnostic.message,
            "incompatible types: int cannot be converted to boolean"
        );
        assert_eq!(diagnostic.range, range_of(src, "1"));
    }

    #[test]
    fn primitive_returns_are_checked_conservatively() {
        let valid = "class C {
            int exact() { return 1; }
            long widened() { return 1; }
            double floating() { return 1.0f; }
            boolean truth() { return true; }
            char character() { return 'x'; }
        }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class C { boolean m() { return 1; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: int cannot be converted to boolean"]
        );
    }

    #[test]
    fn reference_returns_are_checked_conservatively() {
        let valid = "class Box {} class C { Box m(Box box) { return box; } }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class Box {} class C { Box m() { return 1; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: int cannot be converted to Box"]
        );
    }

    #[test]
    fn boxed_returns_support_unboxing_and_reject_incompatible_types() {
        let symbols = ObjectAware(vec![
            ("java.lang.Integer", vec!["java.lang.Number"], Vec::new()),
            ("java.lang.Number", vec!["java.lang.Object"], Vec::new()),
        ]);
        let valid = "class C {
            Integer boxed() { return 1; }
            long widened(Integer value) { return value; }
        }\n";
        assert!(return_messages(valid, &symbols).is_empty());

        let invalid = "class C { boolean m(Integer value) { return value; } }\n";
        assert_eq!(
            return_messages(invalid, &symbols),
            ["incompatible types: Integer cannot be converted to boolean"]
        );
    }

    #[test]
    fn array_returns_are_checked_conservatively() {
        let valid = "class C { int[] m(int[] values) { return values; } }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class C { int[] m() { return 1; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: int cannot be converted to int[]"]
        );
    }

    #[test]
    fn null_returns_follow_reference_rules() {
        let valid = "class Box {} class C { Box m() { return null; } }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class C { int m() { return null; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: null cannot be converted to int"]
        );
    }

    #[test]
    fn missing_and_unexpected_return_values_use_statement_ranges() {
        let src = "class C {
            void unexpected() { return 1; }
            int missing() { return; }
        }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        let unexpected = diagnostics
            .iter()
            .find(|d| d.message.ends_with("unexpected return value"))
            .expect("unexpected-value diagnostic");
        assert_eq!(
            unexpected.message,
            "incompatible types: unexpected return value"
        );
        assert_eq!(unexpected.range, range_of(src, "return 1;"));
        let missing = diagnostics
            .iter()
            .find(|d| d.message.ends_with("missing return value"))
            .expect("missing-value diagnostic");
        assert_eq!(missing.message, "incompatible types: missing return value");
        assert_eq!(missing.range, range_of(src, "return;"));
        for diagnostic in diagnostics {
            assert_eq!(
                diagnostic.code,
                Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
            );
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        }
    }

    #[test]
    fn comment_only_void_return_is_bare() {
        let src = "class C { void m() { return /* no value */; } }\n";

        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn comment_only_non_void_return_is_missing_value() {
        let src = "class C { int m() { return /* no value */; } }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: missing return value"
        );
        assert_eq!(
            diagnostics[0].range,
            range_of(src, "return /* no value */;")
        );
    }

    #[test]
    fn comment_before_return_expression_is_skipped() {
        let src = "class C { boolean m() { return /* value */ 1; } }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: int cannot be converted to boolean"
        );
        assert_eq!(diagnostics[0].range, range_of(src, "1"));
    }

    #[test]
    fn return_checks_run_when_member_checks_are_disabled() {
        let src = "class C { boolean m() { this.nope(); return 1; } }\n";
        let diagnostics = semantic(src, &ObjectAware(vec![]), false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
        );
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: int cannot be converted to boolean"
        );
    }

    #[test]
    fn semantic_diagnostics_respects_global_cap() {
        let methods = (0..crate::MAX_DIAGNOSTICS + 7)
            .map(|i| format!("boolean m{i}() {{ return {i}; }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("class C {{\n{methods}\n}}\n");

        let diagnostics = semantic(&src, &NoSymbols, false, false);
        assert_eq!(diagnostics.len(), crate::MAX_DIAGNOSTICS);
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.code == Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
        }));
    }

    #[test]
    fn constructors_are_ignored() {
        let src = "class C { C() { return 1; } }\n";
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn nested_method_uses_nearest_callable() {
        let src = "class C {
            int outer() {
                class Local { boolean inner() { return 1; } }
                return 1;
            }
        }\n";

        assert_eq!(
            return_messages(src, &NoSymbols),
            ["incompatible types: int cannot be converted to boolean"]
        );
    }

    #[test]
    fn lambda_nested_returns_are_ignored() {
        let src = "class C {
            boolean outer() {
                Runnable task = () -> { return 1; };
                return true;
            }
        }\n";

        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn missing_recovery_silences_return_check() {
        let src = "class C { boolean m() { return 1 } }\n";
        assert!(
            has_recovery(src, true),
            "fixture must contain a MISSING node"
        );
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn error_recovery_silences_return_check() {
        let src = "class C { boolean m() { return 1 ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn ambiguous_overload_return_is_silent() {
        let src = "class C {
            boolean m() { return pick(); }
            int pick() { return 1; }
            int pick(int value) { return value; }
        }\n";

        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn unresolved_type_variable_return_is_silent() {
        let src = "class C { <T> T m() { return \"bad\"; } }\n";
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn cross_document_project_return_is_silent() {
        let sources = [
            "package p; class Other {}\n",
            "package p; class C { Other m() { return new C(); } }\n",
        ];

        assert!(
            semantic_for_sources(&sources, 1, &NoSymbols, false, false).is_empty(),
            "types declared in another open document remain unknown"
        );
    }

    #[test]
    fn constant_narrowing_return_is_silent() {
        let src = "class C { byte m() { return 1; } }\n";
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn incomplete_external_hierarchy_return_is_silent() {
        let src = "import a.Base; import a.Child; class C { Base m() { return new Child(); } }\n";
        let symbols = ObjectAware(vec![
            ("a.Base", vec!["java.lang.Object"], Vec::new()),
            ("a.Child", vec!["a.Missing"], Vec::new()),
        ]);

        assert!(return_messages(src, &symbols).is_empty());
    }

    #[test]
    fn flags_unknown_member_on_in_project_type() {
        let src = "class Box { int width; }\nclass C { void m() { Box b; b.nope(); } }\n";
        let msgs = diags(src, &ObjectAware(vec![]));
        assert!(msgs.iter().any(|m| m.contains("nope")), "{msgs:?}");
    }

    #[test]
    fn does_not_flag_real_or_inherited_object_members() {
        let src = "class Box { int width; }\n\
                   class C { void m() { Box b; b.width = b.hashCode(); b.toString(); } }\n";
        // width is real; hashCode/toString are inherited from Object.
        assert!(diags(src, &ObjectAware(vec![])).is_empty());
    }

    /// Array receivers know their complete member set — `length`/`clone`
    /// plus Object's — so real members stay silent and bogus ones are
    /// flagged (an earlier resolver treated `a` as its *element* type).
    #[test]
    fn array_members_diagnose_correctly() {
        let src = "class C { void m(int[] a) { int n = a.length; a.clone(); a.toString(); } }\n";
        assert!(diags(src, &ObjectAware(vec![])).is_empty());
        let src = "class C { void m(int[] a) { a.missingNo(); } }\n";
        let msgs = diags(src, &ObjectAware(vec![]));
        assert!(msgs.iter().any(|m| m.contains("missingNo")), "{msgs:?}");
    }

    #[test]
    fn stays_silent_without_object_resolution() {
        // No symbol source can resolve Object -> hierarchy incomplete -> no flags.
        let src = "class Box { int width; }\nclass C { void m() { Box b; b.nope(); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn stays_silent_when_a_supertype_is_unknown() {
        // Box extends an external type the source can't resolve -> incomplete.
        let src = "import x.Base;\n\
                   class Box extends Base { }\n\
                   class C { void m() { Box b; b.nope(); } }\n";
        assert!(
            diags(src, &ObjectAware(vec![])).is_empty(),
            "unknown super must mute"
        );
    }

    #[test]
    fn stays_silent_on_unresolved_receiver() {
        let src = "class C { void m() { mystery().nope(); } }\n";
        assert!(diags(src, &ObjectAware(vec![])).is_empty());
    }

    /// A nested-type reference in a method reference (`Map.Entry::getKey`)
    /// must not be flagged as a missing field of the receiver.
    #[test]
    fn does_not_flag_nested_type_in_method_reference() {
        let src = "import java.util.Map;\nclass C { void m() { Object r = Map.Entry.class; } }\n";
        let symbols = ObjectAware(vec![(
            "java.util.Map",
            vec!["java.lang.Object"],
            vec!["get"],
        )]);
        let msgs = diags(src, &symbols);
        assert!(
            !msgs.iter().any(|m| m.contains("Entry")),
            "nested type falsely flagged: {msgs:?}"
        );
    }

    /// A receiver that erased to `java.lang.Object` (the fallback of
    /// generic inference we couldn't carry through a chain) is never flagged —
    /// otherwise ordinary `list.stream().findFirst().orElseThrow().foo()`
    /// floods with false positives.
    #[test]
    fn does_not_flag_members_on_object_receiver() {
        let src = "class C { void m(Object o) { o.definitelyNotAMethod(); } }\n";
        assert!(
            diags(src, &ObjectAware(vec![])).is_empty(),
            "Object receiver must never be flagged"
        );
    }

    /// An enhanced-for `var` binds the element type, so member checks run
    /// against the element (`Box`), not the array/collection (`Box[]`).
    #[test]
    fn enhanced_for_var_checks_element_type() {
        // A real element member is not flagged (proves `b` is `Box`, not `Box[]`).
        let ok = "class Box { int width; }\n\
                  class C { void m(Box[] boxes) { for (var b : boxes) { int w = b.width; } } }\n";
        assert!(
            diags(ok, &ObjectAware(vec![])).is_empty(),
            "element field must resolve: {:?}",
            diags(ok, &ObjectAware(vec![]))
        );
        // A bogus element member is flagged.
        let bad = "class Box { int width; }\n\
                   class C { void m(Box[] boxes) { for (var b : boxes) { b.nope(); } } }\n";
        assert!(
            diags(bad, &ObjectAware(vec![]))
                .iter()
                .any(|m| m.contains("nope")),
            "bogus element member should flag"
        );
    }

    /// An in-project enum's `name()`/`ordinal()` (from the implicit
    /// `java.lang.Enum` super) and its constants resolve — no false flags.
    #[test]
    fn enum_name_ordinal_and_constants_resolve() {
        let src = "enum E { A, B; }\n\
                   class C { void u(E e) { e.name(); e.ordinal(); E x = E.A; } }\n";
        let syms = ObjectAware(vec![(
            "java.lang.Enum",
            vec!["java.lang.Object"],
            vec!["name", "ordinal"],
        )]);
        assert!(
            diags(src, &syms).is_empty(),
            "enum members must resolve: {:?}",
            diags(src, &syms)
        );
    }

    #[test]
    fn flags_unknown_member_on_external_type() {
        let src = "import a.Widget;\nclass C { void m() { Widget w; w.spin(); w.nope(); } }\n";
        let symbols = ObjectAware(vec![("a.Widget", vec!["java.lang.Object"], vec!["spin"])]);
        let msgs = diags(src, &symbols);
        assert!(msgs.iter().any(|m| m.contains("nope")), "{msgs:?}");
        assert!(
            !msgs.iter().any(|m| m.contains("spin")),
            "spin is real: {msgs:?}"
        );
    }

    // ---- Hygiene rules (initializers, unreachable, unused) ------------------

    /// Hygiene-rule diagnostics: member checks off (gated and tested
    /// independently above); `unused` gates only the unused-code rule —
    /// initializer and unreachable checks always run.
    fn hygiene(src: &str, unused: bool) -> Vec<Diagnostic> {
        semantic(src, &NoSymbols, false, unused)
    }

    fn hygiene_messages(src: &str, unused: bool) -> Vec<String> {
        hygiene(src, unused)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    // ---- Rule 1: incompatible initializers (`jvl.incompatibleAssignment`) ----

    #[test]
    fn incompatible_local_initializer_has_exact_contract() {
        assert_eq!(INCOMPATIBLE_ASSIGNMENT_CODE, "jvl.incompatibleAssignment");
        let src = "class C { void m() { boolean flag = 1; } }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String(
                INCOMPATIBLE_ASSIGNMENT_CODE.to_string()
            ))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(
            diagnostic.message,
            "incompatible types: int cannot be converted to boolean"
        );
        // The range covers the initializer expression, not the declarator.
        assert_eq!(diagnostic.range, range_of(src, "1"));
    }

    #[test]
    fn incompatible_field_initializer_is_flagged() {
        let src = "class Box {} class C { Box box = 1; }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: int cannot be converted to Box"
        );
        assert_eq!(diagnostics[0].range, range_of(src, "1"));
    }

    #[test]
    fn compatible_initializers_are_silent() {
        let src = "class Box {} class C {
            int exact = 1;
            long widened = 1;
            double floating = 1.0f;
            boolean truth = true;
            char letter = 'x';
            int[] numbers = null;
            void m(Box box) { Box other = box; }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn constant_narrowing_initializer_is_silent() {
        // `byte small = 1;` is legal Java (assignment conversion narrows
        // constants), exactly like the return-check's narrowing silence.
        let src = "class C { byte small = 1; }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn unknown_initializer_types_are_silent() {
        // Flag only on proven `Some(false)`: an unknown declared type or an
        // unresolvable initializer (`None`) must stay silent.
        let src = "class C {
            void m() {
                Mystery thing = source();
                int number = source();
            }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn var_initializers_are_skipped() {
        // `var` has no declared type to check; the rule must skip it rather
        // than resolve `var` as a type name.
        let src = "class C { void m() { var flag = 1; } }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn null_initializer_follows_reference_rules() {
        let valid = "class Box {} class C { Box box = null; }\n";
        assert!(hygiene_messages(valid, false).is_empty());

        let invalid = "class C { int number = null; }\n";
        assert_eq!(
            hygiene_messages(invalid, false),
            ["incompatible types: null cannot be converted to int"]
        );
    }

    #[test]
    fn missing_recovery_silences_initializer_check() {
        let src = "class C { void m() { boolean flag = 1 } }\n";
        assert!(
            has_recovery(src, true),
            "fixture must contain a MISSING node"
        );
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn error_recovery_silences_initializer_check() {
        let src = "class C { void m() { boolean flag = 1 ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn hygiene_diagnostics_respect_global_cap() {
        let fields = (0..crate::MAX_DIAGNOSTICS + 7)
            .map(|i| format!("boolean f{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("class C {{\n{fields}\n}}\n");

        let diagnostics = hygiene(&src, false);
        assert_eq!(diagnostics.len(), crate::MAX_DIAGNOSTICS);
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.code
                == Some(NumberOrString::String(
                    INCOMPATIBLE_ASSIGNMENT_CODE.to_string(),
                ))
        }));
    }

    // ---- Rule 2: unreachable statements (`jvl.unreachable`) -----------------

    #[test]
    fn unreachable_statement_has_exact_contract() {
        assert_eq!(UNREACHABLE_CODE, "jvl.unreachable");
        let src = "class C { int m() { return 1; return 2; } }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String(UNREACHABLE_CODE.to_string()))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(diagnostic.message, "unreachable statement");
        // The range covers the first dead statement.
        assert_eq!(diagnostic.range, range_of(src, "return 2;"));
    }

    #[test]
    fn unreachable_after_throw_break_and_continue() {
        for terminator in ["throw new RuntimeException();", "break;", "continue;"] {
            let src = format!(
                "class C {{ void m() {{ while (true) {{ {terminator} int dead = 0; }} }} }}\n"
            );
            assert_eq!(
                hygiene_messages(&src, false),
                ["unreachable statement"],
                "{terminator}"
            );
        }
    }

    #[test]
    fn unreachable_emits_once_per_block() {
        let src = "class C { void m() { return; int first = 0; int second = 0; } }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range, range_of(src, "int first = 0;"));
    }

    #[test]
    fn unreachable_is_per_block_not_per_file() {
        let src = "class C {
            void a() { return; int deadA = 0; int alsoDeadA = 0; }
            void b() { return; int deadB = 0; }
        }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        let ranges: Vec<_> = diagnostics.iter().map(|d| d.range).collect();
        assert!(ranges.contains(&range_of(src, "int deadA = 0;")));
        assert!(ranges.contains(&range_of(src, "int deadB = 0;")));
    }

    #[test]
    fn trailing_comments_are_not_unreachable() {
        let block = "class C { void m() { return; /* done */ } }\n";
        assert!(hygiene_messages(block, false).is_empty());

        let line = "class C { void m() { return; // done\n } }\n";
        assert!(hygiene_messages(line, false).is_empty());
    }

    #[test]
    fn switch_case_groups_are_never_flagged() {
        // A statement after `break;` inside the same case group is genuinely
        // dead, but the rule is conservative and inspects only plain blocks.
        let src = "class C {
            void m(int v) {
                switch (v) { case 1: break; m(v); default: return; }
            }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn statement_after_conditional_return_is_reachable() {
        // The `return` terminates only the `if` block; the outer statement
        // after the `if` is reachable and must not be flagged.
        let src = "class C { void m(boolean flag) { if (flag) { return; } int after = 0; } }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn recovery_silences_unreachable_check() {
        let src = "class C { void m() { return; int dead = 0 ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(hygiene_messages(src, false).is_empty());
    }

    // ---- Rule 3: unused code (`jvl.unused`, gated by `unused`) --------------

    #[test]
    fn unused_local_has_exact_contract() {
        let src = "class C { void m() { int count = 0; } }\n";
        let diagnostics = hygiene(src, true);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String("jvl.unused".to_string()))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(diagnostic.message, "unused local variable 'count'");
        assert_eq!(diagnostic.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
        // The range covers the declarator name.
        assert_eq!(diagnostic.range, range_of(src, "count"));
    }

    #[test]
    fn used_local_is_silent() {
        let src = "class C { void m() { int count = 0; log(count); } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn unused_initializer_block_local_is_flagged() {
        let src = "class C { { int temp = 0; } }\n";
        assert_eq!(
            hygiene_messages(src, true),
            ["unused local variable 'temp'"]
        );
    }

    #[test]
    fn unused_parameters_flag_constructors_private_and_static_methods() {
        let constructor = "class C { C(int seed) { } }\n";
        assert_eq!(
            hygiene_messages(constructor, true),
            ["unused parameter 'seed'"]
        );

        let private = "class C { private void log(int level) { } void m() { log(1); } }\n";
        assert_eq!(
            hygiene_messages(private, true),
            ["unused parameter 'level'"]
        );

        let is_static = "class C { static void tick(int beat) { } }\n";
        assert_eq!(
            hygiene_messages(is_static, true),
            ["unused parameter 'beat'"]
        );
    }

    #[test]
    fn public_instance_method_parameters_are_silent() {
        // Non-private instance methods can be overridden or fulfill an
        // interface; their parameters are part of a wider contract.
        let src = "class C { public void on(int event) { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn bodyless_method_parameters_are_silent() {
        // `native`: private-and-static but without a body — only methods WITH
        // bodies are checked (`poke` itself is referenced so the member rule
        // stays out of the way).
        let src =
            "class C { private static native void poke(int handle); void m() { poke(1); } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn override_annotated_method_parameters_are_silent() {
        // Syntactically `@Override` parses on a private method; the rule must
        // skip the whole method (the annotation also mutes the member rule).
        let src = "class C { @Override private void log(int level) { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn main_method_parameters_are_silent() {
        let src = "class C { public static void main(String[] args) { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn catch_parameters_are_silent() {
        let src = "class C { void m() { try { m(); } catch (Exception e) { } } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn lambda_parameters_are_silent() {
        let src = "class C {
            void m() {
                java.util.function.IntConsumer sink = value -> { };
                sink.accept(1);
            }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn unused_private_field_and_method_are_flagged() {
        let src = "class C {
            private int hidden = 1;
            private void helper() { }
        }\n";
        let mut messages = hygiene_messages(src, true);
        messages.sort();
        assert_eq!(
            messages,
            [
                "unused private field 'hidden'",
                "unused private method 'helper'"
            ]
        );
    }

    #[test]
    fn referenced_private_members_are_silent() {
        let src = "class C {
            private int width = 1;
            private int grow() { return width + 1; }
            int m() { return grow(); }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn method_reference_counts_as_use() {
        let src = "class C {
            private void helper() { }
            Runnable m() { return this::helper; }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn annotated_members_are_silent() {
        let src = "class C { @Deprecated private int legacy = 1; }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn serial_version_uid_is_silent() {
        let src = "class C { private static final long serialVersionUID = 1L; }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn private_constructor_is_silent() {
        let src = "class Util { private Util() { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn shadowing_only_ever_silences() {
        // The local `size` shadows the field, so every `size` use in `m`
        // binds to the local. Name-occurrence detection cannot tell them
        // apart, so the truly-unused FIELD must stay silent — shadowing may
        // only ever cause silence, never a false warning.
        let src = "class C {
            private int size = 1;
            void m() {
                int size = 2;
                log(size);
            }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn unused_option_gates_only_the_unused_rule() {
        let src = "class C {
            private int hidden = 1;
            void m() { boolean bad = 1; int dead = 0; log(bad); }
        }\n";
        // Disabled: the unused warnings disappear, but the always-on
        // initializer rule still fires.
        assert_eq!(
            hygiene_messages(src, false),
            ["incompatible types: int cannot be converted to boolean"]
        );
        // Enabled (the default): the warnings join the error.
        let mut messages = hygiene_messages(src, true);
        messages.sort();
        assert_eq!(
            messages,
            [
                "incompatible types: int cannot be converted to boolean",
                "unused local variable 'dead'",
                "unused private field 'hidden'",
            ]
        );
    }

    #[test]
    fn recovery_silences_unused_check() {
        let src = "class C { void m() { int dead = 0; ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(hygiene_messages(src, true).is_empty());
    }
}
