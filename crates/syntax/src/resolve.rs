//! Cursor-relative resolution: enclosing type, in-scope bindings, and
//! receiver-expression → type. All best-effort and `Option`-returning — an
//! unresolved query yields `None`, never a panic, even on tree-sitter
//! ERROR/MISSING recovery nodes.

use tree_sitter::{Node, Tree};

use crate::model::{base_type_name, named_children, Member, MemberKind, TypeDecl, TypeTable};
use crate::{node_text, OpenDoc};

/// A binding visible at the cursor (local, parameter, for-variable, or field).
#[derive(Clone)]
pub(crate) struct Binding<'t> {
    pub name: &'t str,
    pub kind: BindingKind,
    /// Declared type node, or `None` for `var` / inferred lambda params.
    pub type_node: Option<Node<'t>>,
    /// Declaration node, used to render hover signatures.
    pub decl_node: Node<'t>,
    pub source: &'t str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BindingKind {
    Local,
    Param,
    ForVar,
    Field,
}

/// A resolved receiver type plus whether the access is static (the receiver was
/// a bare type name, e.g. `Math.`), which filters member completion.
pub(crate) struct Resolved<'t> {
    pub decl: TypeDecl<'t>,
    pub static_only: bool,
}

fn instance(decl: TypeDecl) -> Resolved {
    Resolved {
        decl,
        static_only: false,
    }
}

/// The smallest named node at a byte offset, or the root as a fallback.
pub(crate) fn node_at<'t>(tree: &'t Tree, byte: usize) -> Node<'t> {
    let root = tree.root_node();
    let byte = byte.min(root.end_byte());
    root.named_descendant_for_byte_range(byte, byte).unwrap_or(root)
}

fn is_type_decl(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

/// Innermost type declaration node enclosing `node`.
pub(crate) fn enclosing_type_node<'t>(node: Node<'t>) -> Option<Node<'t>> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if is_type_decl(n.kind()) {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// The [`TypeDecl`] for the type enclosing `node`.
pub(crate) fn enclosing_typedecl<'t>(node: Node<'t>, source: &'t str) -> Option<TypeDecl<'t>> {
    TypeDecl::from_node(enclosing_type_node(node)?, source)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// If the cursor is in a member-access position (`recv.` or `recv.partial`),
/// return the receiver expression node. The decision is text-anchored: skip any
/// partially-typed member name and surrounding whitespace; if the preceding
/// non-space byte is `.`, it is member access.
pub(crate) fn member_receiver<'t>(
    tree: &'t Tree,
    source: &str,
    cursor: usize,
) -> Option<Node<'t>> {
    let bytes = source.as_bytes();
    let mut i = cursor.min(bytes.len());
    while i > 0 && is_ident_byte(bytes[i - 1]) {
        i -= 1;
    }
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'.' {
        return None;
    }
    receiver_ending_at(tree, i - 1)
}

/// The largest expression node whose `end_byte == dot`.
fn receiver_ending_at<'t>(tree: &'t Tree, dot: usize) -> Option<Node<'t>> {
    if dot == 0 {
        return None;
    }
    let root = tree.root_node();
    let mut node = root.named_descendant_for_byte_range(dot - 1, dot)?;
    while let Some(parent) = node.parent() {
        if parent.end_byte() == dot {
            node = parent;
        } else {
            break;
        }
    }
    Some(node)
}

/// Resolve a receiver expression node to the type whose members it exposes.
pub(crate) fn resolve_receiver_type<'t>(
    recv: Node<'t>,
    doc: &OpenDoc<'t>,
    table: &TypeTable<'t>,
) -> Option<Resolved<'t>> {
    match recv.kind() {
        "this" => enclosing_typedecl(recv, doc.source).map(instance),
        "super" => {
            let td = enclosing_typedecl(recv, doc.source)?;
            let sup = td.supers.first()?;
            table.get(sup).cloned().map(instance)
        }
        "identifier" => resolve_name_to_type(node_text(recv, doc.source), recv.start_byte(), doc, table),
        "field_access" => {
            let obj = recv.child_by_field_name("object")?;
            let field = recv.child_by_field_name("field")?;
            let obj_ty = resolve_receiver_type(obj, doc, table)?;
            let member = table.find_member(&obj_ty.decl, node_text(field, doc.source))?;
            member_type_decl(&member, table).map(instance)
        }
        "object_creation_expression" => {
            let ty = recv.child_by_field_name("type")?;
            let base = base_type_name(ty, doc.source)?;
            table.get(base).cloned().map(instance)
        }
        "scoped_type_identifier" | "scoped_identifier" => resolve_scoped_path(recv, doc, table),
        "parenthesized_expression" => {
            resolve_receiver_type(recv.named_child(0)?, doc, table)
        }
        _ => None,
    }
}

/// Resolve a simple name at a position: a scope binding gives an instance type;
/// otherwise a bare type name gives static access.
pub(crate) fn resolve_name_to_type<'t>(
    name: &str,
    byte: usize,
    doc: &OpenDoc<'t>,
    table: &TypeTable<'t>,
) -> Option<Resolved<'t>> {
    if let Some(binding) = lookup_binding(doc.tree, doc.source, byte, name, table) {
        let ty = binding.type_node?;
        let base = base_type_name(ty, binding.source)?;
        return Some(instance(table.get(base)?.clone()));
    }
    table.get(name).map(|td| Resolved {
        decl: td.clone(),
        static_only: true,
    })
}

/// The declared type of a field member, resolved to a [`TypeDecl`]. Methods have
/// no resolvable result yet (return-type inference is deferred).
fn member_type_decl<'t>(member: &Member<'t>, table: &TypeTable<'t>) -> Option<TypeDecl<'t>> {
    if member.kind != MemberKind::Field {
        return None;
    }
    let ty = field_type_node(member.node)?;
    let base = base_type_name(ty, member.source)?;
    table.get(base).cloned()
}

/// The declared-type node of a field/record-component declarator.
fn field_type_node<'t>(declarator: Node<'t>) -> Option<Node<'t>> {
    if declarator.kind() == "formal_parameter" {
        return declarator.child_by_field_name("type");
    }
    declarator.parent()?.child_by_field_name("type")
}

/// Resolve a dotted path (`a.b.c`) parsed as a scoped identifier: the first
/// segment is a name in scope, each subsequent segment a field of the prior type.
fn resolve_scoped_path<'t>(
    node: Node<'t>,
    doc: &OpenDoc<'t>,
    table: &TypeTable<'t>,
) -> Option<Resolved<'t>> {
    let names = flatten_scoped(node, doc.source);
    let (first, rest) = names.split_first()?;
    let mut current = resolve_name_to_type(first, node.start_byte(), doc, table)?;
    for segment in rest {
        let member = table.find_member(&current.decl, segment)?;
        current = instance(member_type_decl(&member, table)?);
    }
    Some(current)
}

fn flatten_scoped<'t>(node: Node<'t>, source: &'t str) -> Vec<&'t str> {
    fn rec<'t>(n: Node<'t>, source: &'t str, out: &mut Vec<&'t str>) {
        match n.kind() {
            "type_identifier" | "identifier" => out.push(node_text(n, source)),
            _ => {
                for c in named_children(n) {
                    rec(c, source, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    rec(node, source, &mut out);
    out
}

/// All bindings visible at `cursor`, innermost first (so a name lookup finds the
/// shadowing declaration).
pub(crate) fn collect_bindings<'t>(
    tree: &'t Tree,
    source: &'t str,
    cursor: usize,
    table: &TypeTable<'t>,
) -> Vec<Binding<'t>> {
    let mut out = Vec::new();
    let mut node = Some(node_at(tree, cursor));
    let mut enclosing_type: Option<Node<'t>> = None;
    while let Some(n) = node {
        match n.kind() {
            "block" | "constructor_body" | "switch_block" => {
                for child in named_children(n) {
                    if child.start_byte() < cursor && child.kind() == "local_variable_declaration"
                    {
                        push_locals(child, source, &mut out);
                    }
                }
            }
            "for_statement" => {
                if let Some(init) = n.child_by_field_name("init") {
                    if init.kind() == "local_variable_declaration" {
                        push_locals(init, source, &mut out);
                    }
                }
            }
            "enhanced_for_statement" => {
                if let Some(name) = n.child_by_field_name("name") {
                    out.push(Binding {
                        name: node_text(name, source),
                        kind: BindingKind::ForVar,
                        type_node: n.child_by_field_name("type"),
                        decl_node: n,
                        source,
                    });
                }
            }
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "lambda_expression" => push_params(n, source, &mut out),
            k if is_type_decl(k) => {
                if enclosing_type.is_none() {
                    enclosing_type = Some(n);
                }
            }
            _ => {}
        }
        node = n.parent();
    }
    if let Some(type_node) = enclosing_type {
        if let Some(td) = TypeDecl::from_node(type_node, source) {
            push_fields(&td, table, &mut out);
        }
    }
    out
}

fn push_locals<'t>(decl: Node<'t>, source: &'t str, out: &mut Vec<Binding<'t>>) {
    let ty = decl.child_by_field_name("type");
    for declarator in named_children(decl) {
        if declarator.kind() == "variable_declarator" {
            if let Some(name) = declarator.child_by_field_name("name") {
                out.push(Binding {
                    name: node_text(name, source),
                    kind: BindingKind::Local,
                    type_node: ty,
                    decl_node: declarator,
                    source,
                });
            }
        }
    }
}

fn push_params<'t>(node: Node<'t>, source: &'t str, out: &mut Vec<Binding<'t>>) {
    let Some(params) = node.child_by_field_name("parameters") else {
        return;
    };
    match params.kind() {
        "formal_parameters" => {
            for p in named_children(params) {
                if matches!(p.kind(), "formal_parameter" | "spread_parameter") {
                    if let Some(name) = p.child_by_field_name("name") {
                        out.push(Binding {
                            name: node_text(name, source),
                            kind: BindingKind::Param,
                            type_node: p.child_by_field_name("type"),
                            decl_node: p,
                            source,
                        });
                    }
                }
            }
        }
        "inferred_parameters" => {
            for p in named_children(params) {
                if p.kind() == "identifier" {
                    out.push(Binding {
                        name: node_text(p, source),
                        kind: BindingKind::Param,
                        type_node: None,
                        decl_node: p,
                        source,
                    });
                }
            }
        }
        "identifier" => out.push(Binding {
            name: node_text(params, source),
            kind: BindingKind::Param,
            type_node: None,
            decl_node: params,
            source,
        }),
        _ => {}
    }
}

fn push_fields<'t>(td: &TypeDecl<'t>, table: &TypeTable<'t>, out: &mut Vec<Binding<'t>>) {
    for m in table.all_members(td, false) {
        if matches!(m.kind, MemberKind::Field | MemberKind::EnumConstant) {
            out.push(Binding {
                name: m.name,
                kind: BindingKind::Field,
                type_node: field_type_node(m.node),
                decl_node: m.node,
                source: m.source,
            });
        }
    }
}

/// Find the innermost binding named `name` visible at `byte`.
pub(crate) fn lookup_binding<'t>(
    tree: &'t Tree,
    source: &'t str,
    byte: usize,
    name: &str,
    table: &TypeTable<'t>,
) -> Option<Binding<'t>> {
    collect_bindings(tree, source, byte, table)
        .into_iter()
        .find(|b| b.name == name)
}
