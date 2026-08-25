//! Conservative semantic diagnostics for resolvable member accesses and method
//! returns. Resolution must be complete before a diagnostic is emitted, so
//! unknown project/classpath types, overloads, and recovery regions stay silent.

use ls_types::{Diagnostic, NumberOrString};
use tree_sitter::Node;

use crate::external::SymbolSource;
use crate::imports::Imports;
use crate::model::TypeTable;
use crate::resolve::{self, Ctx, ResolvedType};
use crate::{diagnostic, node_text, LineIndex, OpenDoc, MAX_DIAGNOSTICS};

/// Stable LSP code for a proven incompatible method return.
pub const INCOMPATIBLE_RETURN_CODE: &str = "jvl.incompatibleReturn";

/// Immediate semantic diagnostics for `docs[current]`.
///
/// Return checks always run. `unresolved_members` gates only the existing
/// unresolved-member rule.
pub fn semantic_diagnostics(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    symbols: &dyn SymbolSource,
    unresolved_members: bool,
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
    let mut stack = vec![doc.tree.root_node()];
    while let Some(node) = stack.pop() {
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }

        match node.kind() {
            "return_statement" => check_return(node, &ctx, index, &mut out),
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
    if return_statement.has_error() || method.has_error() {
        return;
    }
    let Some(return_type) = method.child_by_field_name("type") else {
        return;
    };
    let expression = return_statement.named_child(0);
    let returns_void = node_text(return_type, ctx.doc.source).trim() == "void";

    match (returns_void, expression) {
        (true, Some(_)) => out.push(incompatible_return(
            index.range(return_statement),
            "incompatible types: unexpected return value".to_string(),
        )),
        (false, None) => out.push(incompatible_return(
            index.range(return_statement),
            "incompatible types: missing return value".to_string(),
        )),
        (true, None) => {}
        (false, Some(expression)) => {
            let Some(expected) = resolve::resolve_type_node(return_type, ctx.doc.source, ctx) else {
                return;
            };
            let Some(actual) = resolve::resolve_expression_type(expression, ctx) else {
                return;
            };
            if resolve::is_assignable(&actual, &expected, ctx) == Some(false) {
                out.push(incompatible_return(
                    index.range(expression),
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
            "lambda_expression"
            | "constructor_declaration"
            | "compact_constructor_declaration" => return None,
            _ => ancestor = node.parent(),
        }
    }
    None
}

fn incompatible_return(range: ls_types::Range, message: String) -> Diagnostic {
    let mut diagnostic = diagnostic(range, message);
    diagnostic.code = Some(NumberOrString::String(
        INCOMPATIBLE_RETURN_CODE.to_string(),
    ));
    diagnostic
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use ls_types::{DiagnosticSeverity, NumberOrString, Range};

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

    fn semantic(src: &str, symbols: &dyn SymbolSource, unresolved_members: bool) -> Vec<Diagnostic> {
        semantic_for_sources(&[src], 0, symbols, unresolved_members)
    }

    fn semantic_for_sources(
        sources: &[&str],
        current: usize,
        symbols: &dyn SymbolSource,
        unresolved_members: bool,
    ) -> Vec<Diagnostic> {
        let mut parser = new_parser();
        let trees: Vec<_> = sources
            .iter()
            .map(|src| parse(&mut parser, src, None).unwrap())
            .collect();
        let docs: Vec<_> = sources
            .iter()
            .zip(&trees)
            .map(|(source, tree)| OpenDoc {
                source: *source,
                tree,
            })
            .collect();
        let index = LineIndex::new(sources[current], PositionEncoding::Utf16);
        semantic_diagnostics(&docs, current, &index, symbols, unresolved_members)
    }

    fn diags(src: &str, symbols: &dyn SymbolSource) -> Vec<String> {
        semantic(src, symbols, true)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    fn return_messages(src: &str, symbols: &dyn SymbolSource) -> Vec<String> {
        semantic(src, symbols, false)
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
        let diagnostics = semantic(src, &NoSymbols, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String(
                "jvl.incompatibleReturn".to_string()
            ))
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
            (
                "java.lang.Integer",
                vec!["java.lang.Number"],
                Vec::new(),
            ),
            (
                "java.lang.Number",
                vec!["java.lang.Object"],
                Vec::new(),
            ),
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
        let diagnostics = semantic(src, &NoSymbols, false);

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
                Some(NumberOrString::String(
                    "jvl.incompatibleReturn".to_string()
                ))
            );
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        }
    }

    #[test]
    fn return_checks_run_when_member_checks_are_disabled() {
        let src = "class C { boolean m() { this.nope(); return 1; } }\n";
        let diagnostics = semantic(src, &ObjectAware(vec![]), false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String(
                "jvl.incompatibleReturn".to_string()
            ))
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

        let diagnostics = semantic(&src, &NoSymbols, false);
        assert_eq!(diagnostics.len(), crate::MAX_DIAGNOSTICS);
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.code
                == Some(NumberOrString::String(
                    "jvl.incompatibleReturn".to_string(),
                ))
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
        assert!(has_recovery(src, true), "fixture must contain a MISSING node");
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn error_recovery_silences_return_check() {
        let src = "class C { boolean m() { return 1 ???; } }\n";
        assert!(has_recovery(src, false), "fixture must contain an ERROR node");
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
            semantic_for_sources(&sources, 1, &NoSymbols, false).is_empty(),
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
        let src =
            "import a.Base; import a.Child; class C { Base m() { return new Child(); } }\n";
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
}
