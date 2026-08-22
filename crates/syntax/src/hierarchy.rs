//! M8e: call- and type-hierarchy primitives. The server composes these with
//! its existing bounded scans: incoming calls = the M4.3 reference scan with
//! each hit grouped under [`enclosing_callable`]; subtypes = the M4.6
//! implementation scan with each hit wrapped by [`type_decl_at_byte`];
//! outgoing calls = [`outgoing_call_sites`] resolved through the
//! go-to-definition ladder. All byte ranges are relative to the document
//! they were computed from.

use std::ops::Range;

use tree_sitter::Node;

use crate::model::{named_children, TypeDecl, TypeKind, TypeTable};
use crate::{node_text, LineIndex, OpenDoc};

/// What a call-hierarchy participant is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallableKind {
    Method,
    Constructor,
    /// A reference that sits outside any method/constructor (a field
    /// initializer, say) groups under its enclosing type instead.
    Type,
}

/// A method/constructor (or fallback type) declaration, as a call-hierarchy
/// item wants it: name, kind, the name identifier's range, and the whole
/// declaration's range.
#[derive(Clone, Debug)]
pub struct CallableInfo {
    pub name: String,
    pub kind: CallableKind,
    pub name_range: Range<usize>,
    pub decl_range: Range<usize>,
    /// Rendered signature, for the item's `detail`.
    pub detail: Option<String>,
}

fn callable_info(node: Node, source: &str, kind: CallableKind) -> Option<CallableInfo> {
    let name_node = node.child_by_field_name("name")?;
    Some(CallableInfo {
        name: node_text(name_node, source).to_string(),
        kind,
        name_range: name_node.start_byte()..name_node.end_byte(),
        decl_range: node.start_byte()..node.end_byte(),
        detail: crate::signature::signature(node, source),
    })
}

/// The innermost method/constructor declaration containing `byte`, else the
/// innermost type declaration (kind [`CallableKind::Type`]). `None` outside
/// any declaration.
pub fn enclosing_callable(doc: &OpenDoc, byte: usize) -> Option<CallableInfo> {
    let mut current = doc
        .tree
        .root_node()
        .named_descendant_for_byte_range(byte, byte)?;
    loop {
        match current.kind() {
            "method_declaration" => {
                return callable_info(current, doc.source, CallableKind::Method)
            }
            "constructor_declaration" => {
                return callable_info(current, doc.source, CallableKind::Constructor)
            }
            kind if TypeKind::from_kind(kind).is_some() => {
                return callable_info(current, doc.source, CallableKind::Type)
            }
            _ => current = current.parent()?,
        }
    }
}

/// The method/constructor declaration whose **name identifier** starts at
/// `byte` — the "is the cursor's target actually a callable?" gate for
/// `prepareCallHierarchy`. `None` for fields, types, locals, anything else.
pub fn callable_decl_at_name(doc: &OpenDoc, byte: usize) -> Option<CallableInfo> {
    let (node, decl) = callable_name_and_decl(doc, byte)?;
    let kind = match decl.kind() {
        "method_declaration" => CallableKind::Method,
        _ => CallableKind::Constructor,
    };
    let _ = node;
    callable_info(decl, doc.source, kind)
}

fn callable_name_and_decl<'t>(doc: &OpenDoc<'t>, byte: usize) -> Option<(Node<'t>, Node<'t>)> {
    let node = doc
        .tree
        .root_node()
        .named_descendant_for_byte_range(byte, byte)
        .filter(|n| n.kind() == "identifier")?;
    let decl = node.parent().filter(|p| {
        matches!(p.kind(), "method_declaration" | "constructor_declaration")
            && p.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
    })?;
    Some((node, decl))
}

/// One call made from inside a callable: the callee's name and the name
/// node's byte range (the position a definition lookup resolves).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
    pub name: String,
    pub name_range: Range<usize>,
}

/// Every call site inside the method/constructor whose name identifier is at
/// `byte`: plain and chained method invocations, plus `new Foo(...)`
/// constructor calls (whose "name" is the constructed type).
pub fn outgoing_call_sites(doc: &OpenDoc, byte: usize) -> Vec<CallSite> {
    let Some((_, decl)) = callable_name_and_decl(doc, byte) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_call_sites(decl, doc.source, &mut out);
    out
}

/// Iterative pre-order walk — NOT native recursion. A method body can nest
/// arbitrarily deep (`((((…))))`) from untrusted project source, and Rust
/// cannot catch a stack overflow (it aborts the whole server); every other
/// whole-subtree walk in this crate uses a work-stack for the same reason.
/// Children are pushed reversed so the pop order stays pre-order (left to
/// right), matching what callers/tests expect.
fn collect_call_sites(root: Node, source: &str, out: &mut Vec<CallSite>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "method_invocation" => {
                if let Some(name) = node.child_by_field_name("name") {
                    out.push(CallSite {
                        name: node_text(name, source).to_string(),
                        name_range: name.start_byte()..name.end_byte(),
                    });
                }
            }
            "object_creation_expression" => {
                if let Some(ty) = node.child_by_field_name("type") {
                    if let Some(base) = crate::model::base_type_name(ty, source) {
                        out.push(CallSite {
                            name: base.to_string(),
                            name_range: ty.start_byte()..ty.end_byte(),
                        });
                    }
                }
            }
            _ => {}
        }
        for child in named_children(node).into_iter().rev() {
            stack.push(child);
        }
    }
}

/// What a type-hierarchy participant is (mirrors the source-level
/// declaration kinds; the server maps these onto LSP `SymbolKind`s).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeInfoKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

/// One supertype reference of a type declaration: the simple name as
/// written, plus the candidate FQNs the declaring file's imports/package
/// give it (resolution-priority order), for locating it in the workspace.
#[derive(Clone, Debug)]
pub struct SuperRef {
    pub simple: String,
    pub candidates: Vec<String>,
}

/// A located type declaration, as a type-hierarchy item wants it.
#[derive(Clone, Debug)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeInfoKind,
    pub name_range: Range<usize>,
    pub decl_range: Range<usize>,
    pub supers: Vec<SuperRef>,
}

fn type_info_of(td: &TypeDecl, doc: &OpenDoc) -> Option<TypeInfo> {
    let name_node = td.node.child_by_field_name("name")?;
    let imports = crate::imports::Imports::parse(doc.tree, doc.source);
    let kind = match td.kind {
        TypeKind::Class => TypeInfoKind::Class,
        TypeKind::Interface => TypeInfoKind::Interface,
        TypeKind::Enum => TypeInfoKind::Enum,
        TypeKind::Record => TypeInfoKind::Record,
        TypeKind::Annotation => TypeInfoKind::Annotation,
    };
    Some(TypeInfo {
        name: td.name.to_string(),
        kind,
        name_range: name_node.start_byte()..name_node.end_byte(),
        decl_range: td.node.start_byte()..td.node.end_byte(),
        supers: td
            .supers
            .iter()
            .map(|s| SuperRef {
                simple: s.to_string(),
                candidates: imports.candidates(s),
            })
            .collect(),
    })
}

/// The in-project type the identifier under the cursor names (its own
/// declaration or any reference resolvable through the open documents),
/// with the index of the document declaring it.
pub fn type_decl_at(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: ls_types::Position,
) -> Option<(usize, TypeInfo)> {
    let doc = docs.get(current)?;
    let byte = index.offset(pos);
    let node = doc
        .tree
        .root_node()
        .named_descendant_for_byte_range(byte, byte)
        .filter(|n| matches!(n.kind(), "identifier" | "type_identifier"))?;
    let name = node_text(node, doc.source);
    let table = TypeTable::build(docs, current);
    let td = table.get(name)?;
    let decl_doc = docs.get(td.doc)?;
    Some((td.doc, type_info_of(td, decl_doc)?))
}

/// The top-level (or table-visible) type named `name` declared in `doc`.
pub fn type_info_in(doc: &OpenDoc, name: &str) -> Option<TypeInfo> {
    let docs = [OpenDoc {
        source: doc.source,
        tree: doc.tree,
    }];
    let table = TypeTable::build(&docs, 0);
    let td = table.get(name)?;
    type_info_of(td, doc)
}

/// The innermost type declaration containing `byte` — wraps an
/// implementation-scan hit (an implementor's name range) back into a full
/// [`TypeInfo`].
pub fn type_decl_at_byte(doc: &OpenDoc, byte: usize) -> Option<TypeInfo> {
    let mut current = doc
        .tree
        .root_node()
        .named_descendant_for_byte_range(byte, byte)?;
    loop {
        if TypeKind::from_kind(current.kind()).is_some() {
            let td = TypeDecl::from_node(current, doc.source, 0)?;
            return type_info_of(&td, doc);
        }
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse, PositionEncoding};

    fn doc_of(src: &str) -> (tree_sitter::Tree, &str) {
        (parse(&mut new_parser(), src, None).expect("parse"), src)
    }

    #[test]
    fn enclosing_callable_finds_method_ctor_and_type_fallback() {
        let src = "class C {\n\
                   \u{20}   int f = init();\n\
                   \u{20}   C() { helper(); }\n\
                   \u{20}   void m() { helper(); }\n\
                   }\n";
        let (tree, source) = doc_of(src);
        let doc = OpenDoc {
            source,
            tree: &tree,
        };

        let in_m = src.rfind("helper").unwrap();
        let info = enclosing_callable(&doc, in_m).expect("method");
        assert_eq!((info.name.as_str(), info.kind), ("m", CallableKind::Method));

        let in_ctor = src.find("helper").unwrap();
        let info = enclosing_callable(&doc, in_ctor).expect("ctor");
        assert_eq!(
            (info.name.as_str(), info.kind),
            ("C", CallableKind::Constructor)
        );

        let in_field = src.find("init").unwrap();
        let info = enclosing_callable(&doc, in_field).expect("type fallback");
        assert_eq!((info.name.as_str(), info.kind), ("C", CallableKind::Type));
    }

    #[test]
    fn callable_decl_gate_accepts_method_names_only() {
        let src = "class C { int field; void run() {} }\n";
        let (tree, source) = doc_of(src);
        let doc = OpenDoc {
            source,
            tree: &tree,
        };
        let at_run = src.find("run").unwrap();
        let info = callable_decl_at_name(&doc, at_run).expect("method decl");
        assert_eq!(info.name, "run");
        assert_eq!(info.detail.as_deref(), Some("void run()"));
        // A field name is not a callable.
        assert!(callable_decl_at_name(&doc, src.find("field").unwrap()).is_none());
        // A call site is not a declaration either.
        let src = "class C { void a() { b(); } void b() {} }\n";
        let (tree, source) = doc_of(src);
        let doc = OpenDoc {
            source,
            tree: &tree,
        };
        assert!(callable_decl_at_name(&doc, src.find("b()").unwrap()).is_none());
    }

    #[test]
    fn outgoing_call_sites_cover_chains_and_constructors() {
        let src = "class C { void m() { helper(); obj.chain().next(); new Widget(1); } }\n";
        let (tree, source) = doc_of(src);
        let doc = OpenDoc {
            source,
            tree: &tree,
        };
        let sites = outgoing_call_sites(&doc, src.find("m()").unwrap());
        let names: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["helper", "next", "chain", "Widget"]);
    }

    #[test]
    fn type_info_reports_supers_with_import_candidates() {
        let src = "package demo;\nimport other.Base;\n\
                   class Impl extends Base implements Runnable {}\n";
        let (tree, source) = doc_of(src);
        let doc = OpenDoc {
            source,
            tree: &tree,
        };
        let info = type_info_in(&doc, "Impl").expect("Impl");
        assert_eq!(info.kind, TypeInfoKind::Class);
        assert_eq!(info.supers.len(), 2);
        let base = &info.supers[0];
        assert_eq!(base.simple, "Base");
        assert_eq!(
            base.candidates,
            vec![
                "other.Base".to_string(),
                "demo.Base".to_string(),
                "java.lang.Base".to_string()
            ]
        );
        // And the byte-anchored lookup wraps a hit back into the same type.
        let at_impl = src.find("extends").unwrap();
        let wrapped = type_decl_at_byte(&doc, at_impl).expect("enclosing type");
        assert_eq!(wrapped.name, "Impl");
    }

    #[test]
    fn type_decl_at_resolves_cursor_references_through_open_docs() {
        let src = "class Impl extends Base {}\nclass Base {}\n";
        let (tree, source) = doc_of(src);
        let docs = [OpenDoc {
            source,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        // Cursor on the `Base` *reference* resolves to the Base declaration.
        let at = index.position(src.find("Base {}").unwrap() - "Base {}\nclass ".len() + 100);
        let _ = at; // positions computed below instead, clearer:
        let at_ref = index.position(src.find("extends Base").unwrap() + "extends ".len());
        let (doc_idx, info) = type_decl_at(&docs, 0, &index, at_ref).expect("resolves");
        assert_eq!(doc_idx, 0);
        assert_eq!(info.name, "Base");
        assert_eq!(info.name_range.start, src.rfind("Base").unwrap());
    }
}
