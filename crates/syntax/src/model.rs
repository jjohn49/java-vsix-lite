//! Parse-tree declarations shared by resolution, completion, and hover.
//! [`TypeTable`] indexes types while [`TypeDecl`] exposes their members.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use jvl_types::TypeId;

use crate::imports::Imports;
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

/// Where a symbol is declared: which open document, and the byte ranges of its
/// name identifier (for go-to/rename) and its enclosing declaration.
///
/// External (JDK/jar) symbols have no `DeclSite`; callers get `None` instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeclSite {
    pub doc: usize,
    pub name_range: std::ops::Range<usize>,
    pub full_range: std::ops::Range<usize>,
}

impl DeclSite {
    /// Build a `DeclSite` from a name node and its enclosing declaration (`full`).
    /// Callers pass `None` up rather than fabricate a range when no name node exists.
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
    /// Where this member is declared, or `None` if `node` has no `name` field.
    /// Resolution never panics on an unexpected node shape.
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
    /// Raw type nodes of `extends`/`implements` clauses (see [`super_type_nodes`]).
    /// Never erased to a simple name: same-simple-name supertypes in different
    /// packages must not collide.
    pub super_nodes: Vec<Node<'t>>,
    /// The type declaration node.
    pub node: Node<'t>,
    pub source: &'t str,
    /// Index (into the `&[OpenDoc]` slice given to [`TypeTable::build`]) of the
    /// document this type is declared in.
    pub doc: usize,
    /// Binary name `pkg.Outer$Inner` for top-level and member types; `None`
    /// for local/anonymous classes (identity = `TypeId::Local`).
    pub binary_name: Option<String>,
    #[allow(dead_code)] // reserved for future type comparisons
    pub type_id: TypeId,
}

impl<'t> TypeDecl<'t> {
    /// `package` is the declaring document's own package (`Imports::package`),
    /// used to compute the binary name.
    pub(crate) fn from_node(
        node: Node<'t>,
        source: &'t str,
        doc: usize,
        package: Option<&str>,
    ) -> Option<TypeDecl<'t>> {
        let kind = TypeKind::from_kind(node.kind())?;
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))?;
        let super_nodes = super_type_nodes(node);
        let (binary_name, type_id) = binary_identity(node, name, source, doc, package);
        Some(TypeDecl {
            name,
            kind,
            super_nodes,
            node,
            source,
            doc,
            binary_name,
            type_id,
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

    /// This type's directly-declared constructors (never inherited).
    /// Not part of the [`Member`] model since only signature help needs them,
    /// for `new Foo(...)` calls.
    pub(crate) fn constructors(&self) -> Vec<Node<'t>> {
        let mut out = Vec::new();
        if let Some(body) = self.body() {
            collect_constructors(body, &mut out);
        }
        out
    }

    /// This record's compact canonical constructor (`Point { ... }`), if declared.
    /// A distinct node kind from `constructor_declaration`, so not included in
    /// [`TypeDecl::constructors`].
    pub(crate) fn compact_constructor(&self) -> Option<Node<'t>> {
        self.body().and_then(|body| {
            named_children(body)
                .into_iter()
                .find(|c| c.kind() == "compact_constructor_declaration")
        })
    }

    /// Erased supertype simple names, for **display only** (hover/hierarchy
    /// rendering). Semantic consumers must use [`TypeTable::source_supers`]
    /// instead; a bare simple name collides across packages.
    pub(crate) fn super_simple_names(&self) -> Vec<&'t str> {
        self.super_nodes
            .iter()
            .filter_map(|n| base_type_name(*n, self.source))
            .collect()
    }
}

/// Ancestor-walk `node` to compute its binary identity: a binary name for a
/// top-level/member type, or `TypeId::Local` for a local/anonymous class, which
/// has no binary name stable outside the request that parsed it.
fn binary_identity<'t>(
    node: Node<'t>,
    name: &'t str,
    source: &'t str,
    doc: usize,
    package: Option<&str>,
) -> (Option<String>, TypeId) {
    let mut names = vec![name];
    let mut anc = node.parent();
    while let Some(a) = anc {
        match a.kind() {
            "program" => break,
            "block" | "method_declaration" | "constructor_declaration" | "lambda_expression" => {
                return (
                    None,
                    TypeId::Local {
                        document: doc,
                        declaration: node.id(),
                    },
                );
            }
            "object_creation_expression"
                if named_children(a)
                    .into_iter()
                    .any(|c| c.kind() == "class_body") =>
            {
                return (
                    None,
                    TypeId::Local {
                        document: doc,
                        declaration: node.id(),
                    },
                );
            }
            _ => {
                if TypeKind::from_kind(a.kind()).is_some() {
                    if let Some(n) = a.child_by_field_name("name") {
                        names.push(node_text(n, source));
                    }
                }
            }
        }
        anc = a.parent();
    }
    names.reverse();
    let qualified = names.join("$");
    let binary = match package {
        Some(p) if !p.is_empty() => format!("{p}.{qualified}"),
        _ => qualified,
    };
    (Some(binary.clone()), TypeId::Named(binary))
}

/// Walk a type body, pushing each declared constructor (descends into
/// `enum_body_declarations` like [`collect_body_members`] does for fields).
fn collect_constructors<'t>(body: Node<'t>, out: &mut Vec<Node<'t>>) {
    for child in named_children(body) {
        match child.kind() {
            "constructor_declaration" => out.push(child),
            "enum_body_declarations" => collect_constructors(child, out),
            _ => {}
        }
    }
}

/// The raw type nodes of a declaration's `extends`/`implements` clauses.
/// `pub(crate)` for go-to-implementation, which needs the node itself (not just
/// the erased simple name) to tell a fully-qualified supertype reference apart
/// from an unqualified one.
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

/// The base simple name of a type *reference*: same as [`base_type_name`] for a
/// real type node, or the identifier's own text for a bare type-name reference
/// like `Foo` in `Foo.method()` (grammar gives `identifier`, not `type_identifier`).
pub(crate) fn type_ref_simple_name<'t>(node: Node<'t>, source: &'t str) -> Option<&'t str> {
    if node.kind() == "identifier" {
        Some(node_text(node, source))
    } else {
        base_type_name(node, source)
    }
}

/// The import/package context of one open document, aligned by index with
/// the `&[OpenDoc]` slice [`TypeTable::build`] was called with.
pub(crate) struct DocContext {
    pub imports: Imports,
    pub package: Option<String>,
}

/// Index of every type declaration across open documents, keyed by qualified
/// binary identity, never a bare simple name — same-simple-name types in
/// different packages must never collide.
pub(crate) struct TypeTable<'t> {
    decls: Vec<TypeDecl<'t>>,
    /// Binary name -> decl indices. More than one entry means the name is
    /// declared more than once across the given documents — ambiguous.
    by_binary: HashMap<String, Vec<usize>>,
    /// Simple name -> decl indices: every declaration sharing that name, not a
    /// resolved winner (used for candidate lists: completion, import offers).
    by_simple: HashMap<&'t str, Vec<usize>>,
    /// (doc, node.id()) -> decl index — for a caller that already holds the
    /// declaration node (an enclosing-type lookup, a completion resolve key).
    by_node: HashMap<(usize, usize), usize>,
    /// Index-aligned with the `&[OpenDoc]` slice `build` was called with.
    docs: Vec<DocContext>,
}

impl<'t> TypeTable<'t> {
    /// Build the table, scanning `docs[current]` first so current-file types
    /// win simple-name candidate ordering (binary-name identity never
    /// collides regardless of scan order).
    pub(crate) fn build(docs: &[OpenDoc<'t>], current: usize) -> TypeTable<'t> {
        let mut t = TypeTable {
            decls: Vec::new(),
            by_binary: HashMap::new(),
            by_simple: HashMap::new(),
            by_node: HashMap::new(),
            docs: Vec::new(),
        };
        for doc in docs {
            let imports = Imports::parse(doc.tree, doc.source);
            t.docs.push(DocContext {
                package: imports.package().map(str::to_string),
                imports,
            });
        }
        let order = std::iter::once(current).chain((0..docs.len()).filter(|&i| i != current));
        for i in order {
            let Some(doc) = docs.get(i) else { continue };
            let package = t.docs[i].package.clone();
            let mut stack = vec![doc.tree.root_node()];
            while let Some(node) = stack.pop() {
                if let Some(decl) = TypeDecl::from_node(node, doc.source, i, package.as_deref()) {
                    let idx = t.decls.len();
                    if let Some(b) = &decl.binary_name {
                        t.by_binary.entry(b.clone()).or_default().push(idx);
                    }
                    t.by_simple.entry(decl.name).or_default().push(idx);
                    t.by_node.insert((i, node.id()), idx);
                    t.decls.push(decl);
                }
                stack.extend(children(node));
            }
        }
        t
    }

    /// Exact binary-name lookup; `None` when absent OR declared more than
    /// once across the given documents (ambiguous — never guess).
    pub(crate) fn get_named(&self, binary: &str) -> Option<&TypeDecl<'t>> {
        match self.by_binary.get(binary).map(Vec::as_slice) {
            Some([i]) => Some(&self.decls[*i]),
            _ => None,
        }
    }

    /// Whether `binary` is declared more than once across the given
    /// documents.
    #[allow(dead_code)] // reserved for future type comparisons
    pub(crate) fn is_duplicate(&self, binary: &str) -> bool {
        self.by_binary.get(binary).is_some_and(|v| v.len() > 1)
    }

    /// Every declaration sharing simple name `simple`, in scan order
    /// (current document first) — a candidate list, not a resolved winner.
    pub(crate) fn candidates<'a>(&'a self, simple: &str) -> impl Iterator<Item = &'a TypeDecl<'t>> {
        self.by_simple
            .get(simple)
            .into_iter()
            .flatten()
            .map(move |i| &self.decls[*i])
    }

    /// The declaration at a specific (document, node) pair — for a caller
    /// that already holds the declaration node itself.
    pub(crate) fn by_node(&self, doc: usize, node_id: usize) -> Option<&TypeDecl<'t>> {
        self.by_node.get(&(doc, node_id)).map(|i| &self.decls[*i])
    }

    /// The import/package context of document `doc`.
    pub(crate) fn doc_context(&self, doc: usize) -> Option<&DocContext> {
        self.docs.get(doc)
    }

    /// Every indexed type declaration (used for in-scope type-name completion).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &TypeDecl<'t>> {
        self.decls.iter()
    }

    /// Java 6.4/7.5 resolution order: (1) lexically enclosing declarations,
    /// (2) explicit single import, (3) same package, (4) fully-qualified/dotted
    /// text, (5) on-demand (wildcard) imports + `java.lang`. Ambiguous or absent
    /// resolves to `None` — never guessed.
    pub(crate) fn resolve_type_name_node(
        &self,
        type_node: Node<'t>,
        source: &'t str,
        doc: usize,
        imports: &Imports,
    ) -> Option<&TypeDecl<'t>> {
        let simple = type_ref_simple_name(type_node, source)?;
        let dotted = crate::resolve::dotted_type_name(type_node, source);
        self.resolve_type_name_at(type_node, simple, dotted, doc, imports)
    }

    /// Resolve `simple` at `anchor`'s lexical position.
    /// This handles nested generic arguments whose name differs from the anchor.
    pub(crate) fn resolve_type_name_at(
        &self,
        anchor: Node<'t>,
        simple: &str,
        dotted: Option<String>,
        doc: usize,
        imports: &Imports,
    ) -> Option<&TypeDecl<'t>> {
        // (1) lexical: walk up from `anchor` through enclosing type declarations.
        let mut anc = anchor.parent();
        while let Some(a) = anc {
            if TypeKind::from_kind(a.kind()).is_some() {
                if let Some(d) = self.by_node(doc, a.id()) {
                    if dotted.is_none() && d.name == simple {
                        return Some(d);
                    }
                    if let Some(b) = &d.binary_name {
                        if let Some(m) = self.get_named(&format!("{b}${simple}")) {
                            if dotted.is_none() {
                                return Some(m);
                            }
                        }
                    }
                }
            }
            anc = a.parent();
        }
        if let Some(dotted) = dotted {
            // (4) qualified: `p.q.Outer.Inner` -> try `p.q.Outer$Inner`,
            // `p.q$Outer$Inner`, … until one is registered. Bypasses imports
            // entirely.
            let mut candidate = dotted;
            for _ in 0..8 {
                if let Some(d) = self.get_named(&candidate) {
                    return Some(d);
                }
                let dot = candidate.rfind('.')?;
                candidate.replace_range(dot..dot + 1, "$");
            }
            return None;
        }
        // (2) explicit import
        if let Some(path) = imports.single_import(simple) {
            let binary =
                crate::resolve::import_path_to_binary(path, |b| self.by_binary.contains_key(b))?;
            return self.get_named(&binary);
        }
        // (3) same package
        let same_pkg = match imports.package() {
            Some(p) => format!("{p}.{simple}"),
            None => simple.to_string(),
        };
        if self.by_binary.contains_key(&same_pkg) {
            return self.get_named(&same_pkg);
        }
        // (5) on-demand: every wildcard package + java.lang; more than one
        // hit is ambiguous.
        let mut hits = imports
            .wildcard_packages()
            .map(|w| format!("{w}.{simple}"))
            .chain(std::iter::once(format!("java.lang.{simple}")))
            .filter(|c| self.by_binary.contains_key(c));
        let first = hits.next()?;
        if hits.next().is_some() {
            return None;
        }
        self.get_named(&first)
    }

    /// Resolve each of `decl`'s [`TypeDecl::super_nodes`] through `decl`'s own
    /// document's imports/package, never the caller's, so a supertype is never
    /// mis-resolved by whoever happens to be asking.
    pub(crate) fn source_supers(&self, decl: &TypeDecl<'t>) -> Vec<&TypeDecl<'t>> {
        let Some(ctx) = self.doc_context(decl.doc) else {
            return Vec::new();
        };
        decl.super_nodes
            .iter()
            .filter_map(|n| self.resolve_type_name_node(*n, decl.source, decl.doc, &ctx.imports))
            .collect()
    }

    /// Find the nearest member through resolvable supertypes.
    /// The walk is cycle- and depth-guarded.
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
        for sup in self.source_supers(decl) {
            if let Some(m) = self.find_member_rec(sup, name, visited, depth + 1) {
                return Some(m);
            }
        }
        None
    }

    /// Collect own and inherited members, nearest override first.
    /// `static_only` retains static members and nested types.
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
        for sup in self.source_supers(decl) {
            self.collect_inherited(sup, out, seen_sig, visited, depth + 1);
        }
    }
}
