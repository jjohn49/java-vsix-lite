//! LSP hover: show a reconstructed signature (fenced `java`) plus the symbol's
//! Javadoc. Works on declarations and on references that resolve to a
//! declaration in an open file.

use ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use tree_sitter::{Node, Tree};

use crate::external::SymbolSource;
use crate::imports::Imports;
use crate::model::{named_children, TypeTable};
use crate::resolve::{self, Ctx, HierMember, Resolved, ResolvedType};
use crate::signature::{javadoc, signature};
use crate::{node_text, LineIndex, OpenDoc};

/// What to render: an in-project declaration node (signature + Javadoc from the
/// tree), or an external member's pre-rendered signature (no Javadoc — JDK/jar
/// bytecode carries none).
enum Target<'t> {
    InProject(Node<'t>, &'t str),
    /// A pre-rendered external signature plus optional Javadoc.
    External(String, Option<String>),
}

/// Build a hover for the identifier under the cursor, or `None` if there is none
/// or it doesn't resolve to a renderable declaration.
pub fn hover(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    symbols: &dyn SymbolSource,
) -> Option<Hover> {
    let doc = docs.get(current)?;
    let cursor = index.offset(pos);
    let table = TypeTable::build(docs, current);
    let imports = Imports::parse(doc.tree, doc.source);
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
    };

    let name_node = identifier_at(doc.tree, cursor)?;
    let value = match resolve_target(name_node, &ctx)? {
        Target::InProject(node, source) => {
            let sig = signature(node, source)?;
            let mut value = format!("```java\n{sig}\n```");
            if let Some(doc_text) = javadoc(node, source) {
                value.push_str("\n\n");
                value.push_str(&doc_text);
            }
            value
        }
        Target::External(sig, doc) => {
            let mut value = format!("```java\n{sig}\n```");
            if let Some(doc_text) = doc {
                value.push_str("\n\n");
                value.push_str(&doc_text);
            }
            value
        }
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(index.range(name_node)),
    })
}

/// The identifier-like node at the cursor, if any. Shared with the
/// goto-definition facade (`definition.rs`), which resolves the same
/// identifier/type-name/`this`/`super` shapes hover does.
pub(crate) fn identifier_at<'t>(tree: &'t Tree, cursor: usize) -> Option<Node<'t>> {
    let node = resolve::node_at(tree, cursor);
    matches!(
        node.kind(),
        "identifier" | "type_identifier" | "this" | "super"
    )
    .then_some(node)
}

/// Resolve the identifier node to what hover should render. Handles declaration
/// names, member accesses (in-project or external), calls, and plain references.
fn resolve_target<'t>(name_node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Target<'t>> {
    if matches!(name_node.kind(), "this" | "super") {
        let resolved = resolve::resolve_receiver_type(name_node, ctx)?;
        return inproject_target(&resolved);
    }

    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        if is_decl_name(parent, name_node) {
            return Some(Target::InProject(parent, ctx.doc.source));
        }
        match parent.kind() {
            "field_access" if field_is(parent, "field", name_node) => {
                let object = parent.child_by_field_name("object")?;
                let resolved = resolve::resolve_receiver_type(object, ctx)?;
                return member_target(&resolved, ctx, name);
            }
            "method_invocation" if field_is(parent, "name", name_node) => {
                let resolved = match parent.child_by_field_name("object") {
                    Some(object) => resolve::resolve_receiver_type(object, ctx)?,
                    None => Resolved {
                        ty: ResolvedType::InProject(resolve::enclosing_typedecl(
                            name_node,
                            ctx.doc.source,
                            ctx.current,
                        )?),
                        static_only: false,
                    },
                };
                return member_target(&resolved, ctx, name);
            }
            // Mid-edit `recv.member` (no trailing `;`) parses as a scoped path; if
            // the cursor is on the trailing segment, resolve it as a member of the
            // prefix's type.
            "scoped_type_identifier" | "scoped_identifier" => {
                let segments = named_children(parent);
                if segments.len() >= 2 && segments.last() == Some(&name_node) {
                    let resolved = resolve::resolve_receiver_type(segments[0], ctx)?;
                    return member_target(&resolved, ctx, name);
                }
            }
            _ => {}
        }
    }

    // Plain reference: a local/param/field, then an in-project type name.
    if let Some(binding) = resolve::lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        name_node.start_byte(),
        name,
        ctx.table,
        ctx.current,
    ) {
        return Some(Target::InProject(binding.decl_node, binding.source));
    }
    ctx.table
        .get(name)
        .map(|td| Target::InProject(td.node, td.source))
}

fn inproject_target<'t>(resolved: &Resolved<'t>) -> Option<Target<'t>> {
    match &resolved.ty {
        ResolvedType::InProject(td) => Some(Target::InProject(td.node, td.source)),
        ResolvedType::External { .. } => None,
    }
}

fn member_target<'t>(resolved: &Resolved<'t>, ctx: &Ctx<'_, 't>, name: &str) -> Option<Target<'t>> {
    match resolve::find_member_hier(resolved, ctx, name)? {
        HierMember::InProject(m) => Some(Target::InProject(m.node, m.source)),
        HierMember::External(m) => {
            // Javadoc only when the receiver itself is external (we have its FQN).
            let doc = match &resolved.ty {
                ResolvedType::External { fqn, .. } => ctx.symbols.doc(fqn, Some(name)),
                ResolvedType::InProject(_) => None,
            };
            Some(Target::External(m.signature, doc))
        }
    }
}

/// Whether `name_node` is the `name` field of a renderable declaration `parent`.
/// Shared with `definition.rs` (see [`identifier_at`]).
pub(crate) fn is_decl_name(parent: Node, name_node: Node) -> bool {
    let renderable = matches!(
        parent.kind(),
        "method_declaration"
            | "constructor_declaration"
            | "variable_declarator"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "formal_parameter"
            | "spread_parameter"
            | "catch_formal_parameter"
            | "enhanced_for_statement"
            | "enum_constant"
    );
    renderable && parent.child_by_field_name("name") == Some(name_node)
}

/// Shared with `definition.rs` (see [`identifier_at`]).
pub(crate) fn field_is(parent: Node, field: &str, name_node: Node) -> bool {
    parent.child_by_field_name(field) == Some(name_node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

    /// A `SymbolSource` that resolves exactly one class, for external hover tests.
    struct OneClass {
        fqn: &'static str,
        members: Vec<ExternalMember>,
    }

    impl SymbolSource for OneClass {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            (fqn == self.fqn).then(|| ExternalClass {
                supers: Vec::new(),
                type_params: Vec::new(),
                members: self
                    .members
                    .iter()
                    .map(|m| ExternalMember {
                        name: m.name.clone(),
                        kind: m.kind,
                        signature: m.signature.clone(),
                        template: m.template.clone(),
                        is_static: m.is_static,
                    })
                    .collect(),
            })
        }
    }

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    /// Hover with the cursor one byte into the first occurrence of `marker`
    /// (markers start with the identifier of interest).
    fn hover_text(src: &str, marker: &str) -> Option<String> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        // Cursor on the first byte of the identifier the marker begins with.
        let at = src.find(marker).expect("marker present");
        let h = hover(&docs, 0, &index, index.position(at), &NoSymbols)?;
        match h.contents {
            HoverContents::Markup(m) => Some(m.value),
            _ => None,
        }
    }

    #[test]
    fn hover_on_method_call_shows_signature_and_javadoc() {
        let src = "class C { /** Adds two numbers. */ int add(int a, int b) { return a + b; }\n\
                   void m() { add(1, 2); } }\n";
        let text = hover_text(src, "add(1").expect("hover");
        assert!(text.contains("int add(int a, int b)"), "{text}");
        assert!(text.contains("Adds two numbers."), "{text}");
    }

    #[test]
    fn hover_on_declaration_name() {
        let src = "class C { /** Adds. */ int add(int a, int b) { return a + b; } }\n";
        let text = hover_text(src, "add(int").expect("hover");
        assert!(text.contains("int add(int a, int b)"), "{text}");
        assert!(text.contains("Adds."), "{text}");
    }

    #[test]
    fn hover_on_field_reference() {
        let src = "class C { /** the user's name */ String name;\n\
                   void m() { name.length(); } }\n";
        let text = hover_text(src, "name.").expect("hover");
        assert!(text.contains("String name"), "{text}");
        assert!(text.contains("the user's name"), "{text}");
    }

    #[test]
    fn hover_on_local_reference() {
        let src = "class C { void m() { int count = 0; count++; } }\n";
        let text = hover_text(src, "count+").expect("hover");
        assert!(text.contains("int count"), "{text}");
    }

    #[test]
    fn hover_on_type_identifier() {
        let src = "class Widget {}\nclass C { Widget w; }\n";
        let text = hover_text(src, "Widget w").expect("hover");
        assert!(text.contains("class Widget"), "{text}");
    }

    #[test]
    fn hover_signature_is_fenced_java() {
        let src = "class C { int x; }\n";
        let text = hover_text(src, "x;").expect("hover");
        assert!(text.starts_with("```java\n"), "{text}");
    }

    #[test]
    fn hover_cross_file_member() {
        let lib = "class Gadget { /** ticks */ int tick() { return 0; } }\n";
        let use_src = "class C { void m() { Gadget g; g.tick(); } }\n";
        let lib_tree = tree(lib);
        let use_tree = tree(use_src);
        let docs = [
            OpenDoc {
                source: use_src,
                tree: &use_tree,
            },
            OpenDoc {
                source: lib,
                tree: &lib_tree,
            },
        ];
        let index = LineIndex::new(use_src, PositionEncoding::Utf16);
        let at = use_src.find("tick(").unwrap();
        let h = hover(&docs, 0, &index, index.position(at), &NoSymbols).expect("hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("int tick()"), "{}", m.value);
        assert!(m.value.contains("ticks"), "{}", m.value);
    }

    #[test]
    fn hover_on_unterminated_scoped_member() {
        // `Helper.S` with no trailing `;` parses as a scoped_type_identifier;
        // hover on the trailing segment must still resolve the member.
        let src = "class Helper { /** the constant */ static int S = 1; }\n\
                   class C { void m() { Helper.S } }\n";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("Helper.S").unwrap() + "Helper.".len(); // cursor on `S`
        let h = hover(&docs, 0, &index, index.position(at), &NoSymbols).expect("hover on S");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("static int S"), "{}", m.value);
        assert!(m.value.contains("the constant"), "{}", m.value);
    }

    #[test]
    fn hover_on_non_identifier_is_none() {
        // Cursor on the opening brace -> nothing to show.
        let src = "class C { }\n";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find('{').unwrap();
        assert!(hover(&docs, 0, &index, index.position(at), &NoSymbols).is_none());
    }

    #[test]
    fn hover_on_external_member() {
        let src = "import java.util.List;\nclass C { void m() { List xs; xs.size(); } }\n";
        let symbols = OneClass {
            fqn: "java.util.List",
            members: vec![ExternalMember {
                name: "size".to_string(),
                kind: ExternalMemberKind::Method,
                signature: "int size()".to_string(),
                template: None,
                is_static: false,
            }],
        };
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("size(").unwrap(); // cursor on `size`
        let h = hover(&docs, 0, &index, index.position(at), &symbols).expect("hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("int size()"), "{}", m.value);
    }
}
