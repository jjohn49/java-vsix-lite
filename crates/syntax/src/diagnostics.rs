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

    // Receiver type must resolve; otherwise we cannot know its members.
    let Some(resolved) = resolve::resolve_receiver_type(object, ctx) else {
        return;
    };
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
        let docs = [OpenDoc { source: src, tree: &tree }];
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
        assert!(diags(src, &ObjectAware(vec![])).is_empty(), "unknown super must mute");
    }

    #[test]
    fn stays_silent_on_unresolved_receiver() {
        let src = "class C { void m() { mystery().nope(); } }\n";
        assert!(diags(src, &ObjectAware(vec![])).is_empty());
    }

    #[test]
    fn flags_unknown_member_on_external_type() {
        let src = "import a.Widget;\nclass C { void m() { Widget w; w.spin(); w.nope(); } }\n";
        let symbols = ObjectAware(vec![("a.Widget", vec!["java.lang.Object"], vec!["spin"])]);
        let msgs = diags(src, &symbols);
        assert!(msgs.iter().any(|m| m.contains("nope")), "{msgs:?}");
        assert!(!msgs.iter().any(|m| m.contains("spin")), "spin is real: {msgs:?}");
    }
}
