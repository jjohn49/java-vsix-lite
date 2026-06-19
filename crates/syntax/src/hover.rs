//! LSP hover: show a reconstructed signature (fenced `java`) plus the symbol's
//! Javadoc. Works on declarations and on references that resolve to a
//! declaration in an open file.

use ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use tree_sitter::{Node, Tree};

use crate::model::{named_children, TypeTable};
use crate::resolve;
use crate::signature::{javadoc, signature};
use crate::{node_text, LineIndex, OpenDoc};

/// Build a hover for the identifier under the cursor, or `None` if there is none
/// or it doesn't resolve to a renderable declaration.
pub fn hover(docs: &[OpenDoc], current: usize, index: &LineIndex, pos: Position) -> Option<Hover> {
    let doc = docs.get(current)?;
    let cursor = index.offset(pos);
    let table = TypeTable::build(docs, current);

    let name_node = identifier_at(doc.tree, cursor)?;
    let (decl_node, decl_source) = resolve_declaration(name_node, doc, &table)?;
    let sig = signature(decl_node, decl_source)?;

    let mut value = format!("```java\n{sig}\n```");
    if let Some(doc_text) = javadoc(decl_node, decl_source) {
        value.push_str("\n\n");
        value.push_str(&doc_text);
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(index.range(name_node)),
    })
}

/// The identifier-like node at the cursor, if any.
fn identifier_at<'t>(tree: &'t Tree, cursor: usize) -> Option<Node<'t>> {
    let node = resolve::node_at(tree, cursor);
    matches!(node.kind(), "identifier" | "type_identifier" | "this" | "super").then_some(node)
}

/// Resolve the identifier node to the declaration to render and the source it
/// lives in. Handles declaration names, member accesses, calls, and plain
/// references (locals/params/fields/types).
fn resolve_declaration<'t>(
    name_node: Node<'t>,
    doc: &OpenDoc<'t>,
    table: &TypeTable<'t>,
) -> Option<(Node<'t>, &'t str)> {
    if matches!(name_node.kind(), "this" | "super") {
        let resolved = resolve::resolve_receiver_type(name_node, doc, table)?;
        return Some((resolved.decl.node, resolved.decl.source));
    }

    let name = node_text(name_node, doc.source);

    if let Some(parent) = name_node.parent() {
        if is_decl_name(parent, name_node) {
            return Some((parent, doc.source));
        }
        match parent.kind() {
            "field_access" if field_is(parent, "field", name_node) => {
                let object = parent.child_by_field_name("object")?;
                let resolved = resolve::resolve_receiver_type(object, doc, table)?;
                let member = table.find_member(&resolved.decl, name)?;
                return Some((member.node, member.source));
            }
            "method_invocation" if field_is(parent, "name", name_node) => {
                let decl = match parent.child_by_field_name("object") {
                    Some(object) => {
                        let resolved = resolve::resolve_receiver_type(object, doc, table)?;
                        table.find_member(&resolved.decl, name)?
                    }
                    None => {
                        let td = resolve::enclosing_typedecl(name_node, doc.source)?;
                        table.find_member(&td, name)?
                    }
                };
                return Some((decl.node, decl.source));
            }
            // Mid-edit `recv.member` (no trailing `;`) parses as a scoped path; if
            // the cursor is on the trailing segment, resolve it as a member of the
            // prefix's type.
            "scoped_type_identifier" | "scoped_identifier" => {
                let segments = named_children(parent);
                if segments.len() >= 2 && segments.last() == Some(&name_node) {
                    let resolved = resolve::resolve_receiver_type(segments[0], doc, table)?;
                    let member = table.find_member(&resolved.decl, name)?;
                    return Some((member.node, member.source));
                }
            }
            _ => {}
        }
    }

    // Plain reference: a local/param/field, then a type name.
    if let Some(binding) =
        resolve::lookup_binding(doc.tree, doc.source, name_node.start_byte(), name, table)
    {
        return Some((binding.decl_node, binding.source));
    }
    table.get(name).map(|td| (td.node, td.source))
}

/// Whether `name_node` is the `name` field of a renderable declaration `parent`.
fn is_decl_name(parent: Node, name_node: Node) -> bool {
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

fn field_is(parent: Node, field: &str, name_node: Node) -> bool {
    parent.child_by_field_name(field) == Some(name_node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

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
        let h = hover(&docs, 0, &index, index.position(at))?;
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
        let h = hover(&docs, 0, &index, index.position(at)).expect("hover");
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
        let h = hover(&docs, 0, &index, index.position(at)).expect("hover on S");
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
        assert!(hover(&docs, 0, &index, index.position(at)).is_none());
    }
}
