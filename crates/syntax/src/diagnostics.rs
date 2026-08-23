//! Conservative semantic diagnostics: flag `receiver.member` where the receiver
//! type resolves **completely** but declares no such member. Stays silent
//! whenever resolution is incomplete (unresolved receiver, unknown supertype,
//! generics, `var`), so valid code is never falsely flagged.

use ls_types::Diagnostic;
use tree_sitter::Node;

use crate::external::SymbolSource;
use crate::imports::Imports;
use crate::model::TypeTable;
use crate::resolve::{self, Ctx};
use crate::{diagnostic, node_text, LineIndex, OpenDoc};

/// Diagnostics for member accesses on resolvable receivers in `docs[current]`.
pub fn member_diagnostics(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    symbols: &dyn SymbolSource,
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
        match node.kind() {
            "field_access" => check(node, "field", "field", &ctx, index, &mut out),
            // Only qualified calls (`recv.name()`); unqualified calls may be
            // inherited or statically imported.
            "method_invocation" if node.child_by_field_name("object").is_some() => {
                check(node, "name", "method", &ctx, index, &mut out)
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out
}

fn check(
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
    if matches!(&resolved.ty, resolve::ResolvedType::External { fqn, .. } if fqn == "java.lang.Object")
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};

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

    fn diags(src: &str, symbols: &dyn SymbolSource) -> Vec<String> {
        let tree = parse(&mut new_parser(), src, None).unwrap();
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        member_diagnostics(&docs, 0, &index, symbols)
            .into_iter()
            .map(|d| d.message)
            .collect()
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
