//! Declaration model extracted from parse trees.
//!
//! [`TypeTable`] indexes the type declarations across all open documents by
//! **simple name**; [`TypeDecl`] describes one type and yields its [`Member`]s on
//! demand. This is the shared substrate the resolver, completion, and hover all
//! build on. Everything borrows the parse trees (`'t`) and runs synchronously
//! while the server holds the documents lock — no allocation of source text.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::{node_text, OpenDoc};

/// Depth cap for inheritance walks — far beyond any real `extends`/`implements`
/// chain, but bounds stack use on pathological (e.g. cyclic-after-edit) input.
const MAX_SUPER_DEPTH: usize = 64;

/// Collect a node's named children into a `Vec` so callers don't juggle the
/// tree-sitter cursor borrow.
pub(crate) fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// Collect a node's children (named and anonymous) into a `Vec`.
pub(crate) fn children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// Where a symbol is declared: which open document (an index into the
/// `&[OpenDoc]` slice given to [`TypeTable::build`]) and byte ranges within that
/// document — the declaring **name** identifier (what a client should jump the
/// cursor to / highlight for rename) and the enclosing declaration (whatever
/// node the owning [`TypeDecl`]/[`Member`]/`Binding` already holds — cheap to
/// carry alongside since no extra tree walk is needed to produce it).
///
/// Built for symbols that live in an open document; external (JDK/jar) symbols
/// have no `DeclSite` — callers get `None` for those instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeclSite {
    pub doc: usize,
    pub name_range: std::ops::Range<usize>,
    pub full_range: std::ops::Range<usize>,
}

impl DeclSite {
    /// Build a `DeclSite` from a name node and the declaration node it belongs
    /// to (`full`), both required to exist — callers pass `None` up when a name
    /// node can't be found rather than fabricating a range.
    pub(crate) fn new(doc: usize, name: Node, full: Node) -> DeclSite {
        DeclSite {
            doc,
            name_range: name.byte_range(),
            full_range: full.byte_range(),
        }
    }
}

/// Kind of a Java type declaration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TypeKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

impl TypeKind {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            TypeKind::Class => "class",
            TypeKind::Interface => "interface",
            TypeKind::Enum => "enum",
            TypeKind::Record => "record",
            TypeKind::Annotation => "@interface",
        }
    }

    pub(crate) fn from_kind(kind: &str) -> Option<TypeKind> {
        Some(match kind {
            "class_declaration" => TypeKind::Class,
            "interface_declaration" => TypeKind::Interface,
            "enum_declaration" => TypeKind::Enum,
            "record_declaration" => TypeKind::Record,
            "annotation_type_declaration" => TypeKind::Annotation,
            _ => return None,
        })
    }

    /// The named child kind holding this type's members.
    fn body_kind(self) -> &'static str {
        match self {
            TypeKind::Class | TypeKind::Record => "class_body",
            TypeKind::Interface => "interface_body",
            TypeKind::Enum => "enum_body",
            TypeKind::Annotation => "annotation_type_body",
        }
    }
}

/// What sort of member a [`Member`] is (drives the completion item kind).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MemberKind {
    Method,
    Field,
    EnumConstant,
    NestedType(TypeKind),
}

/// One member of a type: a method, field, enum constant, or nested type.
#[derive(Clone, Copy)]
pub(crate) struct Member<'t> {
    pub name: &'t str,
    pub kind: MemberKind,
    /// The declaration node to render/inspect: `method_declaration`,
    /// `variable_declarator` (for fields), `enum_constant`, `formal_parameter`
    /// (record component), or a nested type declaration.
    pub node: Node<'t>,
    pub is_static: bool,
    /// Source text of the document this member came from (members can be
    /// inherited from a type declared in a different open file).
    pub source: &'t str,
    /// Index (into the `&[OpenDoc]` slice given to [`TypeTable::build`]) of the
    /// document this member's declaring [`TypeDecl`] came from.
    pub doc: usize,
}

impl<'t> Member<'t> {
    /// Where this member is declared, or `None` if `node` unexpectedly has no
    /// `name` field (never true for the node kinds [`Member`] is built from,
    /// but resolution never panics on a shape it didn't expect).
    pub(crate) fn decl_site(&self) -> Option<DeclSite> {
        let name = self.node.child_by_field_name("name")?;
        Some(DeclSite::new(self.doc, name, self.node))
    }
}

/// A single type declaration, located in some open document.
#[derive(Clone)]
pub(crate) struct TypeDecl<'t> {
    pub name: &'t str,
    pub kind: TypeKind,
    /// Simple names of supertypes (`extends` + `implements`/`permits` excluded).
    pub supers: Vec<&'t str>,
    /// The type declaration node.
    pub node: Node<'t>,
    pub source: &'t str,
    /// Index (into the `&[OpenDoc]` slice given to [`TypeTable::build`]) of the
    /// document this type is declared in.
    pub doc: usize,
}

impl<'t> TypeDecl<'t> {
    /// Build a `TypeDecl` from a type declaration node, or `None` if `node` is
    /// not a type declaration.
    pub(crate) fn from_node(node: Node<'t>, source: &'t str, doc: usize) -> Option<TypeDecl<'t>> {
        let kind = TypeKind::from_kind(node.kind())?;
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let supers = collect_supers(node, source);
        Some(TypeDecl {
            name,
            kind,
            supers,
            node,
            source,
            doc,
        })
    }

    /// Where this type is declared.
    pub(crate) fn decl_site(&self) -> Option<DeclSite> {
        let name = self.node.child_by_field_name("name")?;
        Some(DeclSite::new(self.doc, name, self.node))
    }

    /// The body node containing this type's members.
    fn body(&self) -> Option<Node<'t>> {
        named_children(self.node)
            .into_iter()
            .find(|c| c.kind() == self.kind.body_kind())
    }

    /// This type's directly-declared members (no inheritance).
    pub(crate) fn own_members(&self) -> Vec<Member<'t>> {
        let mut out = Vec::new();
        // Record components behave as fields/accessors.
        if self.kind == TypeKind::Record {
            if let Some(params) = self.node.child_by_field_name("parameters") {
                for p in named_children(params) {
                    if p.kind() == "formal_parameter" {
                        if let Some(name) = p.child_by_field_name("name") {
                            out.push(Member {
                                name: node_text(name, self.source),
                                kind: MemberKind::Field,
                                node: p,
                                is_static: false,
                                source: self.source,
                                doc: self.doc,
                            });
                        }
                    }
                }
            }
        }
        if let Some(body) = self.body() {
            collect_body_members(body, self.source, self.doc, &mut out);
        }
        out
    }

    /// This type's directly-declared constructors (never inherited — Java
    /// constructors aren't members of the [`Member`]/`MemberKind` model since
    /// completion/hover never need to list them; signature help does, for
    /// `new Foo(...)` calls).
    pub(crate) fn constructors(&self) -> Vec<Node<'t>> {
        let mut out = Vec::new();
        if let Some(body) = self.body() {
            collect_constructors(body, &mut out);
        }
        out
    }
}

/// Walk a type body, pushing each declared constructor (descending into the
/// `enum_body_declarations` wrapper the same way [`collect_body_members`]
/// does for methods/fields).
fn collect_constructors<'t>(body: Node<'t>, out: &mut Vec<Node<'t>>) {
    for child in named_children(body) {
        match child.kind() {
            "constructor_declaration" => out.push(child),
            "enum_body_declarations" => collect_constructors(child, out),
            _ => {}
        }
    }
}

/// Extract supertype simple names from `extends`/`implements` clauses.
fn collect_supers<'t>(node: Node<'t>, source: &'t str) -> Vec<&'t str> {
    super_type_nodes(node)
        .into_iter()
        .filter_map(|ty| base_type_name(ty, source))
        .collect()
}

/// The raw type NODES of a declaration's `extends`/`implements` clauses —
/// the same nodes whose base simple names [`collect_supers`] erases into
/// [`TypeDecl::supers`]. `pub(crate)` for go-to-implementation, whose
/// per-supertype confirm needs the node itself (not just the erased simple
/// name) to tell a fully-qualified supertype reference
/// (`implements com.example.Foo` — confirmed against the target's real FQN,
/// bypassing imports) apart from an unqualified one (`implements Foo` —
/// confirmed through the scanned file's import/package context).
pub(crate) fn super_type_nodes<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    for child in named_children(node) {
        match child.kind() {
            // `extends Base` (class) -> superclass(type); `extends A, B` (interface)
            // -> (extends_interfaces (type_list ...)).
            "superclass" | "extends_interfaces" | "super_interfaces" => {
                for ty in named_children(child) {
                    if ty.kind() == "type_list" {
                        out.extend(named_children(ty));
                    } else {
                        out.push(ty);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Walk a type body, pushing each declared member.
fn collect_body_members<'t>(
    body: Node<'t>,
    source: &'t str,
    doc: usize,
    out: &mut Vec<Member<'t>>,
) {
    for child in named_children(body) {
        match child.kind() {
            // `constant_declaration` is how `interface`/`@interface` bodies hold
            // fields; such constants are implicitly `static`.
            "field_declaration" | "constant_declaration" => {
                let is_static =
                    child.kind() == "constant_declaration" || has_modifier(child, source, "static");
                for declarator in named_children(child) {
                    if declarator.kind() == "variable_declarator" {
                        if let Some(name) = declarator.child_by_field_name("name") {
                            out.push(Member {
                                name: node_text(name, source),
                                kind: MemberKind::Field,
                                node: declarator,
                                is_static,
                                source,
                                doc,
                            });
                        }
                    }
                }
            }
            // Methods, and annotation elements (`int value();`), which are
            // method-shaped (type + name, no parameters).
            "method_declaration" | "annotation_type_element_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    out.push(Member {
                        name: node_text(name, source),
                        kind: MemberKind::Method,
                        node: child,
                        is_static: has_modifier(child, source, "static"),
                        source,
                        doc,
                    });
                }
            }
            "enum_constant" => {
                if let Some(name) = child.child_by_field_name("name") {
                    out.push(Member {
                        name: node_text(name, source),
                        kind: MemberKind::EnumConstant,
                        node: child,
                        is_static: true,
                        source,
                        doc,
                    });
                }
            }
            // Methods/fields inside an enum live under this wrapper.
            "enum_body_declarations" => collect_body_members(child, source, doc, out),
            _ => {
                if let Some(kind) = TypeKind::from_kind(child.kind()) {
                    if let Some(name) = child.child_by_field_name("name") {
                        out.push(Member {
                            name: node_text(name, source),
                            kind: MemberKind::NestedType(kind),
                            node: child,
                            is_static: has_modifier(child, source, "static"),
                            source,
                            doc,
                        });
                    }
                }
            }
        }
    }
}

/// The `modifiers` child node of a declaration, if any.
pub(crate) fn modifiers_node<'t>(decl: Node<'t>) -> Option<Node<'t>> {
    named_children(decl)
        .into_iter()
        .find(|c| c.kind() == "modifiers")
}

/// Whether a declaration carries a given modifier keyword (e.g. `static`).
pub(crate) fn has_modifier(decl: Node, source: &str, keyword: &str) -> bool {
    modifiers_node(decl)
        .map(|m| children(m).iter().any(|c| node_text(*c, source) == keyword))
        .unwrap_or(false)
}

/// The base **simple name** of a type node, erasing generics, scopes, and array
/// dimensions. Returns `None` for primitives, `void`, and `var`.
pub(crate) fn base_type_name<'t>(ty: Node<'t>, source: &'t str) -> Option<&'t str> {
    match ty.kind() {
        "type_identifier" => Some(node_text(ty, source)),
        // `List<Integer>` -> first child is the (possibly scoped) type name.
        "generic_type" => named_children(ty)
            .into_iter()
            .next()
            .and_then(|n| base_type_name(n, source)),
        // `a.b.C` -> last `type_identifier`.
        "scoped_type_identifier" => named_children(ty)
            .into_iter()
            .rev()
            .find(|n| n.kind() == "type_identifier")
            .map(|n| node_text(n, source)),
        "array_type" => ty
            .child_by_field_name("element")
            .and_then(|el| base_type_name(el, source)),
        "annotated_type" => named_children(ty)
            .into_iter()
            .find_map(|n| base_type_name(n, source)),
        _ => None,
    }
}

/// Index of every type declaration across the open documents, by simple name.
pub(crate) struct TypeTable<'t> {
    by_name: HashMap<&'t str, TypeDecl<'t>>,
}

impl<'t> TypeTable<'t> {
    /// Build the table, scanning `docs[current]` first so current-file types win
    /// simple-name collisions.
    pub(crate) fn build(docs: &[OpenDoc<'t>], current: usize) -> TypeTable<'t> {
        let mut by_name = HashMap::new();
        let order = std::iter::once(current).chain((0..docs.len()).filter(|&i| i != current));
        for i in order {
            let Some(doc) = docs.get(i) else { continue };
            collect_type_decls(doc.tree.root_node(), doc.source, i, &mut by_name);
        }
        TypeTable { by_name }
    }

    pub(crate) fn get(&self, simple_name: &str) -> Option<&TypeDecl<'t>> {
        self.by_name.get(simple_name)
    }

    /// Every indexed type declaration (used for in-scope type-name completion).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &TypeDecl<'t>> {
        self.by_name.values()
    }

    /// Member named `name` on `decl` or any in-table supertype. Nearest-first, so
    /// an override wins over the inherited copy; stops at the first match without
    /// rendering signatures. Cycle- and depth-guarded.
    pub(crate) fn find_member(&self, decl: &TypeDecl<'t>, name: &str) -> Option<Member<'t>> {
        let mut visited = HashSet::new();
        self.find_member_rec(decl, name, &mut visited, 0)
    }

    fn find_member_rec(
        &self,
        decl: &TypeDecl<'t>,
        name: &str,
        visited: &mut HashSet<usize>,
        depth: usize,
    ) -> Option<Member<'t>> {
        if depth > MAX_SUPER_DEPTH || !visited.insert(decl.node.id()) {
            return None;
        }
        if let Some(m) = decl.own_members().into_iter().find(|m| m.name == name) {
            return Some(m);
        }
        for sup in &decl.supers {
            if let Some(super_decl) = self.get(sup) {
                if let Some(m) = self.find_member_rec(super_decl, name, visited, depth + 1) {
                    return Some(m);
                }
            }
        }
        None
    }

    /// All members of `decl` plus inherited members from in-table supertypes.
    /// When `static_only`, keeps only static members and nested types. Overrides
    /// (same rendered signature) collapse to the nearest declaration; overloads
    /// survive.
    pub(crate) fn all_members(&self, decl: &TypeDecl<'t>, static_only: bool) -> Vec<Member<'t>> {
        let mut out = Vec::new();
        let mut seen_sig = HashSet::new();
        let mut visited = HashSet::new();
        self.collect_inherited(decl, &mut out, &mut seen_sig, &mut visited, 0);
        if static_only {
            out.retain(|m| m.is_static || matches!(m.kind, MemberKind::NestedType(_)));
        }
        out
    }

    fn collect_inherited(
        &self,
        decl: &TypeDecl<'t>,
        out: &mut Vec<Member<'t>>,
        seen_sig: &mut HashSet<String>,
        visited: &mut HashSet<usize>,
        depth: usize,
    ) {
        // Guard against cyclic `extends` (by node id) and pathological depth.
        if depth > MAX_SUPER_DEPTH || !visited.insert(decl.node.id()) {
            return;
        }
        for m in decl.own_members() {
            let sig =
                crate::signature::signature(m.node, m.source).unwrap_or_else(|| m.name.to_string());
            if seen_sig.insert(sig) {
                out.push(m);
            }
        }
        for sup in &decl.supers {
            if let Some(super_decl) = self.get(sup) {
                self.collect_inherited(super_decl, out, seen_sig, visited, depth + 1);
            }
        }
    }
}

/// DFS the tree, registering every type declaration (top-level and nested) under
/// its simple name; first registration wins.
fn collect_type_decls<'t>(
    root: Node<'t>,
    source: &'t str,
    doc: usize,
    by_name: &mut HashMap<&'t str, TypeDecl<'t>>,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let Some(decl) = TypeDecl::from_node(node, source, doc) {
            by_name.entry(decl.name).or_insert(decl);
        }
        stack.extend(children(node));
    }
}
