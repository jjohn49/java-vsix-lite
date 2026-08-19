//! Cursor-relative resolution: enclosing type, in-scope bindings, and
//! receiver-expression → type. All best-effort and `Option`-returning — an
//! unresolved query yields `None`, never a panic, even on tree-sitter
//! ERROR/MISSING recovery nodes.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::external::{ExternalMember, ExternalMemberKind, SymbolSource};
use crate::imports::Imports;
use crate::model::{
    base_type_name, named_children, DeclSite, Member, MemberKind, TypeDecl, TypeTable,
};
use crate::{node_text, OpenDoc};

/// Depth cap for receiver/path resolution — real receiver chains are a handful
/// deep; this bounds stack use on pathological nesting without affecting any
/// legitimate code.
const MAX_RESOLVE_DEPTH: usize = 64;

/// Everything resolution needs: the cursor's document, the in-project type table,
/// the file's imports, and the external symbol source (JDK/deps).
pub(crate) struct Ctx<'a, 't> {
    pub doc: &'a OpenDoc<'t>,
    /// Index of `doc` in the `&[OpenDoc]` slice `table` was built from — needed
    /// to stamp a [`DeclSite`] on bindings/types declared in `doc` itself (e.g.
    /// the enclosing type via `this`/`super`), whose site isn't otherwise
    /// recorded on the node.
    pub current: usize,
    pub table: &'a TypeTable<'t>,
    pub imports: &'a Imports,
    pub symbols: &'a dyn SymbolSource,
}

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
    /// Index (into the `&[OpenDoc]` slice `collect_bindings` was called with) of
    /// the document this binding is declared in. A field binding can name a
    /// different document than the usage site (inherited from a supertype
    /// declared elsewhere); locals/params/for-vars are always the current doc.
    pub doc: usize,
}

impl<'t> Binding<'t> {
    /// Where this binding is declared.
    pub(crate) fn decl_site(&self) -> Option<DeclSite> {
        let name = binding_name_node(self.decl_node)?;
        Some(DeclSite::new(self.doc, name, self.decl_node))
    }
}

/// The name-identifier node of a binding's declaration node. Most binding decl
/// nodes carry a `name` field (`variable_declarator`, `formal_parameter`,
/// `enhanced_for_statement`, and the field-origin `Member` node kinds); a bare
/// lambda parameter's decl node *is* its name (`identifier`); a varargs
/// parameter's name lives on its nested `variable_declarator`.
fn binding_name_node(decl_node: Node) -> Option<Node> {
    match decl_node.kind() {
        "identifier" => Some(decl_node),
        "spread_parameter" => crate::signature::spread_param_name(decl_node),
        _ => decl_node.child_by_field_name("name"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BindingKind {
    Local,
    Param,
    ForVar,
    Field,
}

/// A receiver type: declared in an open document, an external (JDK/dependency)
/// type named by its FQN with any use-site type arguments (e.g. `["String"]`
/// for `ArrayList<String>`), or an array (M7 — whose members are the synthetic
/// `length`/`clone()` plus `java.lang.Object`'s, never the element's).
pub(crate) enum ResolvedType<'t> {
    InProject(TypeDecl<'t>),
    External {
        fqn: String,
        args: Vec<String>,
    },
    Array {
        /// The declared display text (`String[]`), kept for rendering
        /// `clone()`'s signature and for element-type resolution on
        /// `arr[i].` receivers.
        display: String,
    },
}

/// A resolved receiver type plus whether the access is static (the receiver was
/// a bare type name, e.g. `Math.`), which filters member completion.
pub(crate) struct Resolved<'t> {
    pub ty: ResolvedType<'t>,
    pub static_only: bool,
}

fn instance(ty: ResolvedType) -> Resolved {
    Resolved {
        ty,
        static_only: false,
    }
}

/// The smallest named node at a byte offset, or the root as a fallback.
pub(crate) fn node_at<'t>(tree: &'t Tree, byte: usize) -> Node<'t> {
    let root = tree.root_node();
    let byte = byte.min(root.end_byte());
    root.named_descendant_for_byte_range(byte, byte)
        .unwrap_or(root)
}

/// Whether a tree-sitter node kind is one of Java's five type-declaration
/// shapes. `pub(crate)` (M4.4) so `rename.rs` can walk past a nested type's
/// immediate declaration to find its *outer* enclosing type, the same way
/// [`enclosing_type_node`] finds the innermost one.
pub(crate) fn is_type_decl(kind: &str) -> bool {
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

/// The [`TypeDecl`] for the type enclosing `node`, declared in document `doc`.
pub(crate) fn enclosing_typedecl<'t>(
    node: Node<'t>,
    source: &'t str,
    doc: usize,
) -> Option<TypeDecl<'t>> {
    TypeDecl::from_node(enclosing_type_node(node)?, source, doc)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// If the cursor is in a member-access position (`recv.` or `recv.partial`),
/// return the receiver expression node. The decision is text-anchored: skip any
/// partially-typed member name and surrounding whitespace; if the preceding
/// non-space byte is `.`, it is member access.
pub(crate) fn member_receiver<'t>(tree: &'t Tree, source: &str, cursor: usize) -> Option<Node<'t>> {
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
pub(crate) fn resolve_receiver_type<'t>(recv: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Resolved<'t>> {
    resolve_receiver_depth(recv, ctx, 0)
}

fn resolve_receiver_depth<'t>(
    recv: Node<'t>,
    ctx: &Ctx<'_, 't>,
    depth: usize,
) -> Option<Resolved<'t>> {
    if depth > MAX_RESOLVE_DEPTH {
        return None;
    }
    match recv.kind() {
        "this" => enclosing_typedecl(recv, ctx.doc.source, ctx.current)
            .map(|td| instance(ResolvedType::InProject(td))),
        "super" => {
            let td = enclosing_typedecl(recv, ctx.doc.source, ctx.current)?;
            let sup = td.supers.first()?;
            resolve_super(sup, ctx).map(instance)
        }
        // `type_identifier` is how tree-sitter parses a bare name in the common
        // mid-edit shape `recv.partial` (an ERROR / scoped_type_identifier), so
        // it must resolve like `identifier` — instance var or static type name.
        "identifier" | "type_identifier" => resolve_name_depth(
            node_text(recv, ctx.doc.source),
            recv.start_byte(),
            ctx,
            depth + 1,
        ),
        // M7: a method call's receiver type is the called member's result
        // type — the arm that makes `xs.stream().`, `s.trim().`, and every
        // builder chain resolve. No object means an unqualified call on the
        // enclosing type.
        "method_invocation" => {
            let name = recv.child_by_field_name("name")?;
            let recv_ty = match recv.child_by_field_name("object") {
                Some(obj) => resolve_receiver_depth(obj, ctx, depth + 1)?,
                None => instance(ResolvedType::InProject(enclosing_typedecl(
                    recv,
                    ctx.doc.source,
                    ctx.current,
                )?)),
            };
            let member = find_member_hier_of_kind(
                &recv_ty,
                ctx,
                node_text(name, ctx.doc.source),
                MemberNamespace::Method,
            )?;
            member_result_type(&member, &recv_ty, ctx)
        }
        // M7: `((Type) expr).` exposes the cast-to type's members.
        "cast_expression" => {
            let ty = recv.child_by_field_name("type")?;
            resolve_type_node(ty, ctx.doc.source, ctx).map(instance)
        }
        // M7: `arr[i].` exposes the array's element type.
        "array_access" => {
            let arr = recv.child_by_field_name("array")?;
            let a = resolve_receiver_depth(arr, ctx, depth + 1)?;
            match &a.ty {
                ResolvedType::Array { display } => array_element_type(display, ctx),
                _ => None,
            }
        }
        // A string literal receiver (`"".length()`) is always `java.lang.String`.
        "string_literal" => {
            let fqn = "java.lang.String".to_string();
            ctx.symbols.class(&fqn).is_some().then(|| {
                instance(ResolvedType::External {
                    fqn,
                    args: Vec::new(),
                })
            })
        }
        "field_access" => {
            let obj = recv.child_by_field_name("object")?;
            let field = recv.child_by_field_name("field")?;
            // M7: one shared segment step handles in-project fields, external
            // fields (`System.out.` → `java.io.PrintStream`), and nested
            // types alike.
            if let Some(obj_ty) = resolve_receiver_depth(obj, ctx, depth + 1) {
                return resolve_member_segment(&obj_ty, node_text(field, ctx.doc.source), ctx);
            }
            // `java.util.List` in expression position parses as *nested
            // field_access*, whose head (`java`) resolves to nothing — fall
            // back to treating the whole dotted shape as a scoped path
            // (longest-prefix fully-qualified type + segment walk).
            resolve_scoped_path(recv, ctx)
        }
        "object_creation_expression" => resolve_object_creation_type(recv, ctx).map(instance),
        "scoped_type_identifier" | "scoped_identifier" => resolve_scoped_path(recv, ctx),
        "parenthesized_expression" => resolve_receiver_depth(recv.named_child(0)?, ctx, depth + 1),
        _ => None,
    }
}

/// Resolve a simple name at a position: a scope binding gives an instance type;
/// otherwise a bare type name gives static access (in-project or external).
pub(crate) fn resolve_name_to_type<'t>(
    name: &str,
    byte: usize,
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
    resolve_name_depth(name, byte, ctx, 0)
}

fn resolve_name_depth<'t>(
    name: &str,
    byte: usize,
    ctx: &Ctx<'_, 't>,
    depth: usize,
) -> Option<Resolved<'t>> {
    if depth > MAX_RESOLVE_DEPTH {
        return None;
    }
    if let Some(binding) = lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        byte,
        name,
        ctx.table,
        ctx.current,
    ) {
        if let Some(type_node) = binding.type_node {
            if node_text(type_node, binding.source) != "var" {
                return resolve_type_node(type_node, binding.source, ctx).map(instance);
            }
        }
        // M7: a `var` (or typeless) binding infers its type from the
        // declarator's initializer, resolved like any receiver expression
        // (`var v = new ArrayList<String>()`, `var t = s.trim()`, …).
        // Depth-capped: ERROR-recovery trees can produce self-referential
        // shapes legal Java can't. The result is always an instance —
        // whatever static-ness the initializer expression had does not
        // transfer to the value it produced.
        let value = binding.decl_node.child_by_field_name("value")?;
        return resolve_receiver_depth(value, ctx, depth + 1).map(|r| instance(r.ty));
    }
    if let Some(td) = ctx.table.get(name) {
        return Some(Resolved {
            ty: ResolvedType::InProject(td.clone()),
            static_only: true,
        });
    }
    let fqn = resolve_simple_to_fqn(name, ctx)?;
    Some(Resolved {
        ty: ResolvedType::External {
            fqn,
            args: Vec::new(),
        },
        static_only: true,
    })
}

/// Resolve a `new Type(...)` expression's type reference to the receiver type
/// it constructs — in-project or external, same as any other declared-type
/// resolution. Shared by ordinary receiver resolution (`new Foo().x`),
/// hover on the type name inside `new Foo(...)`, and constructor signature
/// help, so all three agree on what `new Foo` refers to.
pub(crate) fn resolve_object_creation_type<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let ty = call.child_by_field_name("type")?;
    resolve_type_node(ty, ctx.doc.source, ctx)
}

/// Resolve a declared-type node to a receiver type: in-project if the open docs
/// declare it, else an external FQN (fully-qualified use, or a simple name
/// resolved through imports + the symbol source).
fn resolve_type_node<'t>(
    type_node: Node<'t>,
    source: &'t str,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    // M7: an array's members are `length`/`clone()`/Object's — never the
    // element type's (the old behavior offered `String`'s members on a
    // `String[]` receiver).
    if type_node.kind() == "array_type" {
        return Some(ResolvedType::Array {
            display: node_text(type_node, source).to_string(),
        });
    }
    let args = extract_type_args(type_node, source);
    if let Some(fqn) = dotted_type_name(type_node, source) {
        let simple = fqn.rsplit('.').next().unwrap_or(&fqn);
        if let Some(td) = ctx.table.get(simple) {
            return Some(ResolvedType::InProject(td.clone()));
        }
        return ctx
            .symbols
            .class(&fqn)
            .is_some()
            .then_some(ResolvedType::External { fqn, args });
    }
    let simple = base_type_name(type_node, source)?;
    if let Some(td) = ctx.table.get(simple) {
        return Some(ResolvedType::InProject(td.clone()));
    }
    let fqn = resolve_simple_to_fqn(simple, ctx)?;
    Some(ResolvedType::External { fqn, args })
}

/// Type arguments of a declared type (`ArrayList<String>` → `["String"]`),
/// erased to simple names; wildcards render as `?`. Empty for raw/non-generic.
fn extract_type_args(type_node: Node, source: &str) -> Vec<String> {
    if type_node.kind() != "generic_type" {
        return Vec::new();
    }
    let Some(targs) = named_children(type_node)
        .into_iter()
        .find(|c| c.kind() == "type_arguments")
    else {
        return Vec::new();
    };
    named_children(targs)
        .into_iter()
        .filter(|a| a.kind() != "annotation" && a.kind() != "marker_annotation")
        .map(|arg| match arg.kind() {
            "wildcard" => "?".to_string(),
            _ => base_type_name(arg, source)
                .map(str::to_string)
                .unwrap_or_else(|| "?".to_string()),
        })
        .collect()
}

/// A supertype simple name → in-project decl or external FQN (type args of a
/// parameterized super are not tracked yet — members render erased).
fn resolve_super<'t>(simple: &str, ctx: &Ctx<'_, 't>) -> Option<ResolvedType<'t>> {
    if let Some(td) = ctx.table.get(simple) {
        return Some(ResolvedType::InProject(td.clone()));
    }
    resolve_simple_to_fqn(simple, ctx).map(|fqn| ResolvedType::External {
        fqn,
        args: Vec::new(),
    })
}

/// First import candidate FQN that the symbol source can actually resolve.
/// M7: an import path (`java.util.Map.Entry`) to the binary FQN
/// (`java.util.Map$Entry`) — replace trailing dots with `$` until the symbol
/// source recognizes the name. Shared by import completion and (M7.5)
/// hover-on-import.
pub(crate) fn import_path_to_fqn(path: &str, ctx: &Ctx) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut candidate = path.to_string();
    for _ in 0..8 {
        if ctx.symbols.class(&candidate).is_some() {
            return Some(candidate);
        }
        let dot = candidate.rfind('.')?;
        candidate.replace_range(dot..dot + 1, "$");
    }
    None
}

/// `pub(crate)` (M7.5): also used by hover's inherited-Javadoc walk, which
/// resolves a supertype simple name to ask the symbol source for the
/// super's member doc.
pub(crate) fn resolve_simple_to_fqn(simple: &str, ctx: &Ctx) -> Option<String> {
    ctx.imports
        .candidates(simple)
        .into_iter()
        .find(|fqn| ctx.symbols.class(fqn).is_some())
}

/// The full dotted name of a fully-qualified type node (`java.util.List`), or
/// `None` for a simple (unqualified) type.
///
/// `pub(crate)` (M4.6): also used by `implementation.rs`'s per-supertype
/// confirm, which must match a fully-qualified `extends`/`implements` entry
/// against the target's real FQN rather than through the scanned file's
/// imports (a qualified reference bypasses imports entirely).
pub(crate) fn dotted_type_name(type_node: Node, source: &str) -> Option<String> {
    match type_node.kind() {
        "scoped_type_identifier" => {
            let parts = flatten_scoped(type_node, source);
            (parts.len() >= 2).then(|| parts.join("."))
        }
        "generic_type" => named_children(type_node)
            .into_iter()
            .next()
            .and_then(|n| dotted_type_name(n, source)),
        "annotated_type" => named_children(type_node)
            .into_iter()
            .find_map(|n| dotted_type_name(n, source)),
        "array_type" => type_node
            .child_by_field_name("element")
            .and_then(|e| dotted_type_name(e, source)),
        _ => None,
    }
}

/// The declared-type node of a field/record-component declarator.
fn field_type_node<'t>(declarator: Node<'t>) -> Option<Node<'t>> {
    if declarator.kind() == "formal_parameter" {
        return declarator.child_by_field_name("type");
    }
    declarator.parent()?.child_by_field_name("type")
}

/// Resolve a dotted path (`a.b.c`), M7-style: walk it segment by segment from
/// a resolvable head (binding → in-project type → imported/`java.lang`
/// external type), stepping through fields, nested types, and enum constants
/// in either world; failing that, try the longest prefix of the path as a
/// fully-qualified external type (`java.util.List`) and walk any remaining
/// segments from there.
fn resolve_scoped_path<'t>(node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Resolved<'t>> {
    let names = flatten_scoped(node, ctx.doc.source);
    if let Some(resolved) = names.split_first().and_then(|(first, rest)| {
        let head = resolve_name_to_type(first, node.start_byte(), ctx)?;
        walk_segments(head, rest, ctx)
    }) {
        return Some(resolved);
    }
    for k in (1..=names.len()).rev() {
        let fqn = names[..k].join(".");
        if ctx.symbols.class(&fqn).is_some() {
            let head = Resolved {
                ty: ResolvedType::External {
                    fqn,
                    args: Vec::new(),
                },
                static_only: true,
            };
            return walk_segments(head, &names[k..], ctx);
        }
    }
    None
}

fn walk_segments<'t>(
    head: Resolved<'t>,
    segments: &[&str],
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
    let mut current = head;
    for segment in segments {
        current = resolve_member_segment(&current, segment, ctx)?;
    }
    Some(current)
}

/// One dotted step off a resolved receiver: a nested type (static context
/// continues), or a field / enum constant (whose declared type the walk
/// re-resolves as an instance). Methods never appear in a dotted path
/// without parens, so the Field namespace is the only member lookup.
fn resolve_member_segment<'t>(
    current: &Resolved<'t>,
    name: &str,
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
    if current.static_only {
        if let ResolvedType::External { fqn, .. } = &current.ty {
            // `Map.Entry` — a nested class continues the static context.
            let nested = format!("{fqn}${name}");
            if ctx.symbols.class(&nested).is_some() {
                return Some(Resolved {
                    ty: ResolvedType::External {
                        fqn: nested,
                        args: Vec::new(),
                    },
                    static_only: true,
                });
            }
        }
    }
    let members = collect_members(current, ctx);
    // An in-project nested type (`Outer.Inner`) also continues the static
    // context — it lives in the member list rather than the symbol source.
    if let Some(td) = members.iter().find_map(|m| match m {
        HierMember::InProject(m)
            if m.name == name && matches!(m.kind, MemberKind::NestedType(_)) =>
        {
            TypeDecl::from_node(m.node, m.source, m.doc)
        }
        _ => None,
    }) {
        return Some(Resolved {
            ty: ResolvedType::InProject(td),
            static_only: true,
        });
    }
    let member = members
        .into_iter()
        .find(|m| m.name() == name && MemberNamespace::Field.matches(m))?;
    member_result_type(&member, current, ctx)
}

/// M7: the type a member access *evaluates to* — a method call's return type
/// or a field/enum-constant's declared type — which becomes the next
/// receiver in a chain. `None` for primitives/void/arrays-of-unknown and
/// whatever else can't be re-resolved (the chain just stops, never errors).
fn member_result_type<'t>(
    member: &HierMember<'t>,
    recv: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
    match member {
        HierMember::InProject(m) => {
            let ty_node = match m.kind {
                MemberKind::Method => m.node.child_by_field_name("type")?,
                MemberKind::Field => field_type_node(m.node)?,
                // An enum constant's type is its declaring enum.
                MemberKind::EnumConstant => {
                    let td = TypeDecl::from_node(enclosing_type_node(m.node)?, m.source, m.doc)?;
                    return Some(instance(ResolvedType::InProject(td)));
                }
                MemberKind::NestedType(_) => return None, // handled as a segment, not a value
            };
            resolve_type_node(ty_node, m.source, ctx).map(instance)
        }
        HierMember::External(m) => external_result_type(m, recv, ctx).map(instance),
    }
}

/// M7: an external member's result type. Prefers the generic `ret_display`
/// template substituted with the receiver's use-site type arguments (so
/// `List<String>.get(int)` chains as `String`, `stream()` as
/// `Stream<String>`); falls back to the erased `ret_fqn`.
fn external_result_type<'t>(
    m: &ExternalMember,
    recv: &Resolved<'_>,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    if let Some(display) = &m.ret_display {
        let (args, type_params) = match &recv.ty {
            ResolvedType::External { fqn, args } => {
                let params = ctx
                    .symbols
                    .class(fqn)
                    .map(|c| c.type_params)
                    .unwrap_or_default();
                (args.clone(), params)
            }
            _ => (Vec::new(), Vec::new()),
        };
        let substituted = substitute_template(display, &args, &type_params);
        // An unfilled placeholder means the substitution had nothing to say
        // (raw receiver, malformed template) — fall through to erasure.
        if !substituted.contains('{') {
            if let Some(resolved) = resolve_display_type(&substituted, m.ret_fqn.as_deref(), ctx) {
                return Some(resolved);
            }
        }
    }
    let fqn = m.ret_fqn.clone()?;
    Some(ResolvedType::External {
        fqn,
        args: Vec::new(),
    })
}

/// M7: resolve a rendered display type (`Stream<String>`, `String`,
/// `MyType`) back to a receiver type: the in-project table first, then the
/// erased FQN when its simple name agrees with the display's base (the
/// common generic-class case), then the file's imports/`java.lang` (the
/// type-variable case, where erasure and display genuinely differ).
fn resolve_display_type<'t>(
    display: &str,
    erased_fqn: Option<&str>,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let (base, args) = parse_display_type(display)?;
    let simple = base.rsplit('.').next().unwrap_or(base);
    if let Some(td) = ctx.table.get(simple) {
        return Some(ResolvedType::InProject(td.clone()));
    }
    if let Some(fqn) = erased_fqn {
        let erased_simple = fqn.rsplit(['.', '$']).next().unwrap_or(fqn);
        if erased_simple == simple {
            return Some(ResolvedType::External {
                fqn: fqn.to_string(),
                args,
            });
        }
    }
    resolve_simple_to_fqn(simple, ctx).map(|fqn| ResolvedType::External { fqn, args })
}

/// M7: split a rendered type into its base name and top-level type
/// arguments: `Map<String, List<Integer>>` → `("Map", ["String",
/// "List<Integer>"])`. Arrays and malformed shapes yield `None` (chains
/// don't continue through them).
fn parse_display_type(s: &str) -> Option<(&str, Vec<String>)> {
    let s = s.trim();
    if s.is_empty() || s.ends_with("[]") {
        return None;
    }
    let Some(lt) = s.find('<') else {
        return Some((s, Vec::new()));
    };
    let base = &s[..lt];
    let inner = s.strip_suffix('>')?.get(lt + 1..)?;
    let mut args = Vec::new();
    let mut nesting = 0usize;
    let mut start = 0usize;
    for (i, c) in inner.char_indices() {
        match c {
            '<' => nesting += 1,
            '>' => nesting = nesting.checked_sub(1)?,
            ',' if nesting == 0 => {
                args.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    args.push(inner[start..].trim().to_string());
    Some((base, args))
}

/// M7: the two members every Java array has beyond `java.lang.Object`'s —
/// the `length` field and the covariant `clone()`.
fn array_members(display: &str) -> [ExternalMember; 2] {
    [
        ExternalMember {
            name: "length".to_string(),
            kind: ExternalMemberKind::Field,
            signature: "int length".to_string(),
            template: None,
            is_static: false,
            ret_fqn: None,
            ret_display: None,
        },
        ExternalMember {
            name: "clone".to_string(),
            kind: ExternalMemberKind::Method,
            signature: format!("{display} clone()"),
            template: None,
            is_static: false,
            ret_fqn: None,
            ret_display: Some(display.to_string()),
        },
    ]
}

/// M7: the element type of an array receiver (`arr[i].`), resolved from the
/// array's declared display text. Multi-dimensional arrays peel one level.
fn array_element_type<'t>(display: &str, ctx: &Ctx<'_, 't>) -> Option<Resolved<'t>> {
    let element = display.trim().strip_suffix("[]")?.trim_end();
    if element.ends_with(']') {
        return Some(instance(ResolvedType::Array {
            display: element.to_string(),
        }));
    }
    let base = element.split('<').next().unwrap_or(element).trim();
    let simple = base.rsplit('.').next().unwrap_or(base);
    if let Some(td) = ctx.table.get(simple) {
        return Some(instance(ResolvedType::InProject(td.clone())));
    }
    resolve_simple_to_fqn(simple, ctx).map(|fqn| {
        instance(ResolvedType::External {
            fqn,
            args: Vec::new(),
        })
    })
}

/// A member of a resolved type — declared in an open document or external.
pub(crate) enum HierMember<'t> {
    InProject(Member<'t>),
    External(ExternalMember),
}

impl HierMember<'_> {
    pub(crate) fn name(&self) -> &str {
        match self {
            HierMember::InProject(m) => m.name,
            HierMember::External(m) => &m.name,
        }
    }

    /// Where this member is declared, or `None` for an external (JDK/dependency)
    /// member — those aren't backed by an open document in this task.
    pub(crate) fn decl_site(&self) -> Option<DeclSite> {
        match self {
            HierMember::InProject(m) => m.decl_site(),
            HierMember::External(_) => None,
        }
    }
}

#[derive(Default)]
struct MemberAcc<'t> {
    out: Vec<HierMember<'t>>,
    seen: HashSet<String>,
    visited_node: HashSet<usize>,
    visited_fqn: HashSet<String>,
}

/// All members of a resolved type, own and inherited, across the in-project ↔
/// external boundary. Deduplicated by rendered signature (override hides the
/// inherited copy; overloads survive); cycle- and depth-guarded.
pub(crate) fn collect_members<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
) -> Vec<HierMember<'t>> {
    let mut acc = MemberAcc::default();
    walk_members(&resolved.ty, ctx, resolved.static_only, &mut acc, 0);
    acc.out
}

/// The member named `name` on a resolved type or any supertype.
pub(crate) fn find_member_hier<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    name: &str,
) -> Option<HierMember<'t>> {
    collect_members(resolved, ctx)
        .into_iter()
        .find(|m| m.name() == name)
}

/// Which Java member namespace a reference occupies. Java resolves fields
/// and methods in *separate* namespaces (JLS §6.5): `recv.foo()` can only
/// mean a method; `recv.foo` / a bare `foo` in expression position can only
/// mean a field (or enum constant). [`find_member_hier`] is namespace-blind
/// (first name match wins), which is fine for hover/completion's
/// display-oriented lookups; reference confirmation must not conflate a
/// field with a same-named method, so it goes through
/// [`find_member_hier_of_kind`] instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MemberNamespace {
    Method,
    Field,
}

impl MemberNamespace {
    fn matches(self, member: &HierMember) -> bool {
        match member {
            HierMember::InProject(m) => match self {
                MemberNamespace::Method => matches!(m.kind, MemberKind::Method),
                MemberNamespace::Field => {
                    matches!(m.kind, MemberKind::Field | MemberKind::EnumConstant)
                }
            },
            HierMember::External(m) => match self {
                MemberNamespace::Method => m.kind == ExternalMemberKind::Method,
                MemberNamespace::Field => m.kind == ExternalMemberKind::Field,
            },
        }
    }
}

/// The member named `name` in the given [`MemberNamespace`], on a resolved
/// type or any supertype. A kind-aware variant of [`find_member_hier`] — a
/// deliberately separate function rather than a behavior change to that one,
/// which hover/completion consume and whose name-only semantics must not
/// shift underneath them (M4.3 fix round 1).
pub(crate) fn find_member_hier_of_kind<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    name: &str,
    namespace: MemberNamespace,
) -> Option<HierMember<'t>> {
    collect_members(resolved, ctx)
        .into_iter()
        .find(|m| m.name() == name && namespace.matches(m))
}

fn walk_members<'t>(
    ty: &ResolvedType<'t>,
    ctx: &Ctx<'_, 't>,
    static_only: bool,
    acc: &mut MemberAcc<'t>,
    depth: usize,
) {
    if depth > MAX_RESOLVE_DEPTH {
        return;
    }
    match ty {
        ResolvedType::InProject(td) => {
            if !acc.visited_node.insert(td.node.id()) {
                return;
            }
            for m in td.own_members() {
                if static_only && !(m.is_static || matches!(m.kind, MemberKind::NestedType(_))) {
                    continue;
                }
                let sig = crate::signature::signature(m.node, m.source)
                    .unwrap_or_else(|| m.name.to_string());
                if acc.seen.insert(sig) {
                    acc.out.push(HierMember::InProject(m));
                }
            }
            for sup in &td.supers {
                if let Some(sd) = ctx.table.get(sup) {
                    walk_members(
                        &ResolvedType::InProject(sd.clone()),
                        ctx,
                        static_only,
                        acc,
                        depth + 1,
                    );
                } else if let Some(fqn) = resolve_simple_to_fqn(sup, ctx) {
                    walk_members(
                        &ResolvedType::External {
                            fqn,
                            args: Vec::new(),
                        },
                        ctx,
                        static_only,
                        acc,
                        depth + 1,
                    );
                }
            }
        }
        // M7: arrays expose exactly `length`, `clone()`, and Object's members.
        ResolvedType::Array { display } => {
            for m in array_members(display) {
                if static_only {
                    continue; // arrays have no static members
                }
                if acc.seen.insert(m.signature.clone()) {
                    acc.out.push(HierMember::External(m));
                }
            }
            walk_members(
                &ResolvedType::External {
                    fqn: "java.lang.Object".to_string(),
                    args: Vec::new(),
                },
                ctx,
                static_only,
                acc,
                depth + 1,
            );
        }
        ResolvedType::External { fqn, args } => {
            if !acc.visited_fqn.insert(fqn.clone()) {
                return;
            }
            let Some(class) = ctx.symbols.class(fqn) else {
                return;
            };
            for m in class.members {
                // Constructors are never ordinary members — same as an
                // in-project type's constructors, which `own_members()`
                // never lists either (see `TypeDecl::constructors`). A
                // dedicated lookup (hover on `new Foo(...)`, constructor
                // signature help) fetches them straight from `SymbolSource`
                // instead.
                if m.kind == ExternalMemberKind::Constructor {
                    continue;
                }
                if static_only && !m.is_static {
                    continue;
                }
                // Dedup on the erased signature (stable across declarations);
                // display the generic signature substituted with the use-site
                // type arguments (e.g. `add({0})` + `[String]` → `add(String)`).
                if acc.seen.insert(m.signature.clone()) {
                    let signature = display_signature(&m, args, &class.type_params);
                    acc.out
                        .push(HierMember::External(ExternalMember { signature, ..m }));
                }
            }
            // Map this instantiation's type arguments through each supertype's
            // own type-argument list (index-aligned with `class.supers`, same
            // convention as `ClassInfo::super_type_args`) so an inherited
            // member substitutes with the *use-site* concrete types rather
            // than the supertype's raw type variables — e.g. `ArrayList<E>
            // extends AbstractList<E>` with `args = ["String"]` maps
            // `AbstractList`'s `["{0}"]` entry to `["String"]`. A raw
            // (unparameterized) supertype, or one whose arguments aren't
            // tracked (length mismatch against `supers`), degrades to no args
            // — today's behavior.
            let super_type_args = ctx.symbols.super_type_args(fqn);
            let super_type_args = if super_type_args.len() == class.supers.len() {
                super_type_args
            } else {
                vec![Vec::new(); class.supers.len()]
            };
            for (sup, sup_args) in class.supers.into_iter().zip(super_type_args) {
                // Each raw arg string uses the same `{i}` placeholder
                // convention as a member template, over the *current* class's
                // `type_params` — so the same substitution helper composes
                // directly, nested generics (`List<{0}>`) included.
                let mapped_args: Vec<String> = sup_args
                    .iter()
                    .map(|raw| substitute_template(raw, args, &class.type_params))
                    .collect();
                walk_members(
                    &ResolvedType::External {
                        fqn: sup,
                        args: mapped_args,
                    },
                    ctx,
                    static_only,
                    acc,
                    depth + 1,
                );
            }
        }
    }
}

fn flatten_scoped<'t>(node: Node<'t>, source: &'t str) -> Vec<&'t str> {
    fn rec<'t>(n: Node<'t>, source: &'t str, out: &mut Vec<&'t str>, depth: usize) {
        if depth > MAX_RESOLVE_DEPTH || out.len() > MAX_RESOLVE_DEPTH {
            return;
        }
        match n.kind() {
            "type_identifier" | "identifier" => out.push(node_text(n, source)),
            _ => {
                for c in named_children(n) {
                    rec(c, source, out, depth + 1);
                }
            }
        }
    }
    let mut out = Vec::new();
    rec(node, source, &mut out, 0);
    out
}

/// Collect every member name of a resolved type (own + inherited, any kind),
/// and whether the **entire** supertype hierarchy was resolvable. Used by
/// unresolved-member diagnostics, which must stay silent unless the answer is
/// complete (an unknown supertype could declare the member). `java.lang.Object`
/// members are always included, since they are callable on any reference type.
pub(crate) fn member_names(resolved: &Resolved<'_>, ctx: &Ctx<'_, '_>) -> (HashSet<String>, bool) {
    let mut names = HashSet::new();
    let mut complete = true;
    let mut visited_node = HashSet::new();
    let mut visited_fqn = HashSet::new();
    diag_walk(
        &resolved.ty,
        ctx,
        &mut names,
        &mut complete,
        &mut visited_node,
        &mut visited_fqn,
        0,
    );
    match ctx.symbols.class("java.lang.Object") {
        Some(object) => names.extend(object.members.into_iter().map(|m| m.name)),
        None => complete = false, // can't confirm Object's members → never flag
    }
    (names, complete)
}

#[allow(clippy::too_many_arguments)]
fn diag_walk(
    ty: &ResolvedType<'_>,
    ctx: &Ctx<'_, '_>,
    names: &mut HashSet<String>,
    complete: &mut bool,
    visited_node: &mut HashSet<usize>,
    visited_fqn: &mut HashSet<String>,
    depth: usize,
) {
    if depth > MAX_RESOLVE_DEPTH {
        *complete = false;
        return;
    }
    match ty {
        ResolvedType::InProject(td) => {
            if !visited_node.insert(td.node.id()) {
                return;
            }
            for m in td.own_members() {
                names.insert(m.name.to_string());
            }
            for sup in &td.supers {
                if let Some(sd) = ctx.table.get(sup) {
                    diag_walk(
                        &ResolvedType::InProject(sd.clone()),
                        ctx,
                        names,
                        complete,
                        visited_node,
                        visited_fqn,
                        depth + 1,
                    );
                } else if let Some(fqn) = resolve_simple_to_fqn(sup, ctx) {
                    diag_walk(
                        &ResolvedType::External {
                            fqn,
                            args: Vec::new(),
                        },
                        ctx,
                        names,
                        complete,
                        visited_node,
                        visited_fqn,
                        depth + 1,
                    );
                } else {
                    *complete = false; // unknown supertype — give up flagging
                }
            }
        }
        // M7: an array's complete member set is `length` + `clone` (plus
        // Object's, appended globally by `member_names`).
        ResolvedType::Array { display } => {
            for m in array_members(display) {
                names.insert(m.name);
            }
        }
        ResolvedType::External { fqn, .. } => {
            if !visited_fqn.insert(fqn.clone()) {
                return;
            }
            match ctx.symbols.class(fqn) {
                Some(class) => {
                    for m in &class.members {
                        // A constructor's name (the class's own simple name)
                        // is never a valid `recv.member` name — same
                        // exclusion as the ordinary member walk.
                        if m.kind != ExternalMemberKind::Constructor {
                            names.insert(m.name.clone());
                        }
                    }
                    for sup in class.supers {
                        diag_walk(
                            &ResolvedType::External {
                                fqn: sup,
                                args: Vec::new(),
                            },
                            ctx,
                            names,
                            complete,
                            visited_node,
                            visited_fqn,
                            depth + 1,
                        );
                    }
                }
                None => *complete = false,
            }
        }
    }
}

/// The signature to show for an external member: its generic template
/// substituted with the use-site type arguments when present, otherwise the
/// erased signature. `pub(crate)` (M6.3): also used directly by hover and
/// signature help's dedicated constructor lookups, which bypass
/// [`collect_members`] (constructors are filtered out of the ordinary member
/// walk — see [`walk_members`]) but still want the same use-site generic
/// substitution.
pub(crate) fn display_signature(
    member: &ExternalMember,
    args: &[String],
    type_params: &[String],
) -> String {
    match &member.template {
        Some(template) if !args.is_empty() => substitute_template(template, args, type_params),
        _ => member.signature.clone(),
    }
}

/// Replace `{i}` placeholders with the i-th type argument (falling back to the
/// type-parameter name, then `?`).
fn substitute_template(template: &str, args: &[String], type_params: &[String]) -> String {
    let mut out = template.to_string();
    for i in 0..type_params.len().max(args.len()) {
        let replacement = args
            .get(i)
            .or_else(|| type_params.get(i))
            .map(String::as_str)
            .unwrap_or("?");
        out = out.replace(&format!("{{{i}}}"), replacement);
    }
    out
}

/// All bindings visible at `cursor`, innermost first (so a name lookup finds the
/// shadowing declaration). `include_fields` adds the enclosing type's fields
/// (own + inherited); callers that already enumerate members separately pass
/// `false` to avoid recomputing them.
pub(crate) fn collect_bindings<'t>(
    tree: &'t Tree,
    source: &'t str,
    cursor: usize,
    table: &TypeTable<'t>,
    include_fields: bool,
    doc: usize,
) -> Vec<Binding<'t>> {
    let mut out = Vec::new();
    let mut node = Some(node_at(tree, cursor));
    let mut enclosing_type: Option<Node<'t>> = None;
    while let Some(n) = node {
        match n.kind() {
            "block" | "constructor_body" | "switch_block" => {
                for child in named_children(n) {
                    if child.start_byte() < cursor && child.kind() == "local_variable_declaration" {
                        push_locals(child, source, doc, &mut out);
                    }
                }
            }
            "for_statement" => {
                if let Some(init) = n.child_by_field_name("init") {
                    if init.kind() == "local_variable_declaration" {
                        push_locals(init, source, doc, &mut out);
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
                        doc,
                    });
                }
            }
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "lambda_expression" => push_params(n, source, doc, &mut out),
            k if is_type_decl(k) => {
                if enclosing_type.is_none() {
                    enclosing_type = Some(n);
                }
            }
            _ => {}
        }
        node = n.parent();
    }
    if include_fields {
        if let Some(type_node) = enclosing_type {
            if let Some(td) = TypeDecl::from_node(type_node, source, doc) {
                push_fields(&td, table, &mut out);
            }
        }
    }
    out
}

fn push_locals<'t>(decl: Node<'t>, source: &'t str, doc: usize, out: &mut Vec<Binding<'t>>) {
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
                    doc,
                });
            }
        }
    }
}

fn push_params<'t>(node: Node<'t>, source: &'t str, doc: usize, out: &mut Vec<Binding<'t>>) {
    let Some(params) = node.child_by_field_name("parameters") else {
        return;
    };
    match params.kind() {
        "formal_parameters" => {
            for p in named_children(params) {
                match p.kind() {
                    "formal_parameter" => {
                        if let Some(name) = p.child_by_field_name("name") {
                            out.push(Binding {
                                name: node_text(name, source),
                                kind: BindingKind::Param,
                                type_node: p.child_by_field_name("type"),
                                decl_node: p,
                                source,
                                doc,
                            });
                        }
                    }
                    // Varargs: name + type live off the field accessors.
                    "spread_parameter" => {
                        if let Some(name) = crate::signature::spread_param_name(p) {
                            let type_node = named_children(p)
                                .into_iter()
                                .find(|c| !matches!(c.kind(), "modifiers" | "variable_declarator"));
                            out.push(Binding {
                                name: node_text(name, source),
                                kind: BindingKind::Param,
                                type_node,
                                decl_node: p,
                                doc,
                                source,
                            });
                        }
                    }
                    _ => {}
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
                        doc,
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
            doc,
        }),
        _ => {}
    }
}

/// Bind each of `td`'s fields (own + inherited). A field's `doc` is the
/// declaring [`Member`]'s own document — not necessarily `td`'s — since fields
/// can be inherited from a supertype declared in a different open file.
fn push_fields<'t>(td: &TypeDecl<'t>, table: &TypeTable<'t>, out: &mut Vec<Binding<'t>>) {
    for m in table.all_members(td, false) {
        if matches!(m.kind, MemberKind::Field | MemberKind::EnumConstant) {
            out.push(Binding {
                name: m.name,
                kind: BindingKind::Field,
                type_node: field_type_node(m.node),
                decl_node: m.node,
                doc: m.doc,
                source: m.source,
            });
        }
    }
}

/// Find the innermost binding named `name` visible at `byte`, in document `doc`.
pub(crate) fn lookup_binding<'t>(
    tree: &'t Tree,
    source: &'t str,
    byte: usize,
    name: &str,
    table: &TypeTable<'t>,
    doc: usize,
) -> Option<Binding<'t>> {
    collect_bindings(tree, source, byte, table, true, doc)
        .into_iter()
        .find(|b| b.name == name)
}

#[cfg(test)]
mod decl_site_tests {
    use super::*;
    use crate::external::{
        ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols, SymbolSource,
    };
    use crate::{new_parser, parse};

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    /// M4.0: a usage of a local variable resolves to a `DeclSite` in the same
    /// document, at the byte range of the declaring identifier.
    #[test]
    fn binding_decl_site_points_at_declaring_identifier() {
        let src = "class C { void m() { int count = 0; count++; } }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let table = TypeTable::build(&docs, 0);
        let byte = src.find("count++").unwrap();
        let binding = lookup_binding(&t, src, byte, "count", &table, 0).expect("binding found");
        let site = binding.decl_site().expect("decl site");
        assert_eq!(site.doc, 0);
        let expected = src.find("count = 0").unwrap();
        assert_eq!(site.name_range, expected..expected + "count".len());
    }

    /// M4.0: a type used in doc B but declared in doc A resolves to a
    /// `DeclSite` naming A's index and the byte range of `Foo`'s name.
    #[test]
    fn cross_file_type_decl_site_points_at_declaring_document() {
        let doc_a = "class Foo {}\n";
        let doc_b = "class B { Foo f; }\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let docs = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let table = TypeTable::build(&docs, 0);
        let td = table.get("Foo").expect("Foo indexed");
        let site = td.decl_site().expect("decl site");
        assert_eq!(site.doc, 1);
        let expected = doc_a.find("Foo").unwrap();
        assert_eq!(site.name_range, expected..expected + "Foo".len());
    }

    /// M4.0: resolving a member inherited from a supertype declared in another
    /// open document yields a `DeclSite` in that other document.
    #[test]
    fn inherited_member_decl_site_points_at_supertype_document() {
        let doc_a = "class A { void methodFromA() {} }\n";
        let doc_b = "class B extends A { void m() { B b = new B(); b.methodFromA(); } }\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let docs = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree_b, doc_b);
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols: &NoSymbols,
        };
        let td_b = table.get("B").expect("B indexed").clone();
        let resolved = Resolved {
            ty: ResolvedType::InProject(td_b),
            static_only: false,
        };
        let member = find_member_hier(&resolved, &ctx, "methodFromA").expect("member found");
        let site = member.decl_site().expect("decl site");
        assert_eq!(site.doc, 1);
        let expected = doc_a.find("methodFromA").unwrap();
        assert_eq!(site.name_range, expected..expected + "methodFromA".len());
    }

    /// M4.0: a member resolved from an external `SymbolSource` (JDK/jar) has no
    /// `DeclSite` in this task — no open document backs it.
    #[test]
    fn external_member_decl_site_is_absent() {
        struct StubSymbols;
        impl SymbolSource for StubSymbols {
            fn class(&self, fqn: &str) -> Option<ExternalClass> {
                (fqn == "java.lang.String").then(|| ExternalClass {
                    supers: Vec::new(),
                    type_params: Vec::new(),
                    members: vec![ExternalMember {
                        name: "length".to_string(),
                        kind: ExternalMemberKind::Method,
                        signature: "int length()".to_string(),
                        template: None,
                        is_static: false,
                        ret_fqn: None,
                        ret_display: None,
                    }],
                })
            }
        }

        let src = "class C {}\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&t, src);
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols: &StubSymbols,
        };
        let resolved = Resolved {
            ty: ResolvedType::External {
                fqn: "java.lang.String".to_string(),
                args: Vec::new(),
            },
            static_only: false,
        };
        let member = find_member_hier(&resolved, &ctx, "length").expect("member found");
        assert!(member.decl_site().is_none());
    }
}

/// M5 (5.3b): mapping a parameterized supertype's type arguments through
/// `SymbolSource::super_type_args` so an inherited external member's template
/// substitutes with the *use-site* concrete types, not the raw class's own
/// type variables.
#[cfg(test)]
mod super_type_args_tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, SymbolSource};
    use crate::{new_parser, parse};

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn method(name: &str, signature: &str, template: &str) -> ExternalMember {
        ExternalMember {
            name: name.to_string(),
            kind: ExternalMemberKind::Method,
            signature: signature.to_string(),
            template: Some(template.to_string()),
            is_static: false,
            ret_fqn: None,
            ret_display: None,
        }
    }

    fn find(fqn: &str, args: Vec<String>, symbols: &dyn SymbolSource, name: &str) -> String {
        let src = "class C {}\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&t, src);
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
        };
        let resolved = Resolved {
            ty: ResolvedType::External {
                fqn: fqn.to_string(),
                args,
            },
            static_only: false,
        };
        let member = find_member_hier(&resolved, &ctx, name).expect("member found");
        match member {
            HierMember::External(m) => m.signature,
            HierMember::InProject(_) => panic!("expected external member"),
        }
    }

    /// `ArrayList<E> extends AbstractList<E>`; `AbstractList` declares
    /// `E get(int)`. `ArrayList<String>` → inherited `get` renders `String
    /// get(int)`, not erased/`E`.
    struct ArrayListStub;
    impl SymbolSource for ArrayListStub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.ArrayList" => Some(ExternalClass {
                    supers: vec!["test.AbstractList".to_string()],
                    type_params: vec!["E".to_string()],
                    members: Vec::new(),
                }),
                "test.AbstractList" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["E".to_string()],
                    members: vec![method("get", "Object get(int)", "{0} get(int)")],
                }),
                _ => None,
            }
        }
        fn super_type_args(&self, fqn: &str) -> Vec<Vec<String>> {
            match fqn {
                "test.ArrayList" => vec![vec!["{0}".to_string()]],
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn inherited_member_substitutes_through_direct_supertype_type_arg() {
        let sig = find(
            "test.ArrayList",
            vec!["String".to_string()],
            &ArrayListStub,
            "get",
        );
        assert_eq!(sig, "String get(int)");
    }

    /// Two-hop: `class C<T> extends B<T>`, `B<T> extends A<T>`; `A` declares
    /// `T id(T)`. `C<Integer>` → `Integer id(Integer)`.
    struct TwoHopStub;
    impl SymbolSource for TwoHopStub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.C" => Some(ExternalClass {
                    supers: vec!["test.B".to_string()],
                    type_params: vec!["T".to_string()],
                    members: Vec::new(),
                }),
                "test.B" => Some(ExternalClass {
                    supers: vec!["test.A".to_string()],
                    type_params: vec!["T".to_string()],
                    members: Vec::new(),
                }),
                "test.A" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("id", "Object id(Object)", "{0} id({0})")],
                }),
                _ => None,
            }
        }
        fn super_type_args(&self, fqn: &str) -> Vec<Vec<String>> {
            match fqn {
                "test.C" | "test.B" => vec![vec!["{0}".to_string()]],
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn inherited_member_substitutes_through_two_hop_hierarchy() {
        let sig = find("test.C", vec!["Integer".to_string()], &TwoHopStub, "id");
        assert_eq!(sig, "Integer id(Integer)");
    }

    /// Re-ordered args: `class M<K,V> extends Base<V,K>`; `Base` declares
    /// `K first()` (Base's own `K` = `M`'s `V`). `M<String,Integer>` →
    /// `Integer first()`.
    struct ReorderedStub;
    impl SymbolSource for ReorderedStub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.M" => Some(ExternalClass {
                    supers: vec!["test.Base".to_string()],
                    type_params: vec!["K".to_string(), "V".to_string()],
                    members: Vec::new(),
                }),
                "test.Base" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["K".to_string(), "V".to_string()],
                    members: vec![method("first", "Object first()", "{0} first()")],
                }),
                _ => None,
            }
        }
        fn super_type_args(&self, fqn: &str) -> Vec<Vec<String>> {
            match fqn {
                // Base<V, K> — first arg is M's V ({1}), second is M's K ({0}).
                "test.M" => vec![vec!["{1}".to_string(), "{0}".to_string()]],
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn inherited_member_substitutes_through_reordered_type_args() {
        let sig = find(
            "test.M",
            vec!["String".to_string(), "Integer".to_string()],
            &ReorderedStub,
            "first",
        );
        assert_eq!(sig, "Integer first()");
    }

    /// Concrete supertype args: `class S extends Box<String>`; `Box` declares
    /// `T unwrap()` → `String unwrap()`.
    struct ConcreteStub;
    impl SymbolSource for ConcreteStub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.S" => Some(ExternalClass {
                    supers: vec!["test.Box".to_string()],
                    type_params: Vec::new(),
                    members: Vec::new(),
                }),
                "test.Box" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("unwrap", "Object unwrap()", "{0} unwrap()")],
                }),
                _ => None,
            }
        }
        fn super_type_args(&self, fqn: &str) -> Vec<Vec<String>> {
            match fqn {
                "test.S" => vec![vec!["String".to_string()]],
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn inherited_member_substitutes_through_concrete_supertype_arg() {
        let sig = find("test.S", Vec::new(), &ConcreteStub, "unwrap");
        assert_eq!(sig, "String unwrap()");
    }

    /// Raw supertype (no `super_type_args` tracked for it) → inherited member
    /// renders as today: erased/var-name, no panic.
    struct RawStub;
    impl SymbolSource for RawStub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.RawUser" => Some(ExternalClass {
                    supers: vec!["test.Generic".to_string()],
                    type_params: Vec::new(),
                    members: Vec::new(),
                }),
                "test.Generic" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("get", "Object get()", "{0} get()")],
                }),
                _ => None,
            }
        }
        // No override: defaults to `Vec::new()` for every fqn — "nothing
        // tracked", exactly like a raw (unparameterized) supertype use.
    }

    #[test]
    fn raw_supertype_falls_back_to_erased_rendering_without_panicking() {
        // No type args flow through an untracked supertype (`RawStub` never
        // overrides `super_type_args`), so the inherited member keeps
        // rendering its today's-behavior erased signature — no panic, no
        // spurious substitution.
        let sig = find("test.RawUser", Vec::new(), &RawStub, "get");
        assert_eq!(sig, "Object get()");
    }

    /// Nested arg: `class C<T> extends Base<List<T>>` — the placeholder is
    /// *embedded* inside a larger rendered supertype-arg string
    /// (`super_type_args = [["List<{0}>"]]`), so substitution must rewrite
    /// inside the string, not just match whole-arg placeholders. `Base`
    /// declares `T head()`; `C<String>` → `List<String> head()`.
    struct NestedStub;
    impl SymbolSource for NestedStub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.C" => Some(ExternalClass {
                    supers: vec!["test.Base".to_string()],
                    type_params: vec!["T".to_string()],
                    members: Vec::new(),
                }),
                "test.Base" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("head", "Object head()", "{0} head()")],
                }),
                _ => None,
            }
        }
        fn super_type_args(&self, fqn: &str) -> Vec<Vec<String>> {
            match fqn {
                "test.C" => vec![vec!["List<{0}>".to_string()]],
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn inherited_member_substitutes_inside_nested_supertype_arg() {
        let sig = find("test.C", vec!["String".to_string()], &NestedStub, "head");
        assert_eq!(sig, "List<String> head()");
    }
}
