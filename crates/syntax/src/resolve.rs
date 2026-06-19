//! Cursor-relative resolution: enclosing type, in-scope bindings, and
//! receiver-expression → type. All best-effort and `Option`-returning — an
//! unresolved query yields `None`, never a panic, even on tree-sitter
//! ERROR/MISSING recovery nodes.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::external::{ExternalMember, SymbolSource};
use crate::imports::Imports;
use crate::model::{base_type_name, named_children, Member, MemberKind, TypeDecl, TypeTable};
use crate::{node_text, OpenDoc};

/// Depth cap for receiver/path resolution — real receiver chains are a handful
/// deep; this bounds stack use on pathological nesting without affecting any
/// legitimate code.
const MAX_RESOLVE_DEPTH: usize = 64;

/// Everything resolution needs: the cursor's document, the in-project type table,
/// the file's imports, and the external symbol source (JDK/deps).
pub(crate) struct Ctx<'a, 't> {
    pub doc: &'a OpenDoc<'t>,
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BindingKind {
    Local,
    Param,
    ForVar,
    Field,
}

/// A receiver type: either declared in an open document or an external
/// (JDK/dependency) type named by its FQN, with any type arguments from the use
/// site (e.g. `["String"]` for `ArrayList<String>`).
pub(crate) enum ResolvedType<'t> {
    InProject(TypeDecl<'t>),
    External { fqn: String, args: Vec<String> },
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
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
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
        "this" => enclosing_typedecl(recv, ctx.doc.source).map(|td| instance(ResolvedType::InProject(td))),
        "super" => {
            let td = enclosing_typedecl(recv, ctx.doc.source)?;
            let sup = td.supers.first()?;
            resolve_super(sup, ctx).map(instance)
        }
        // `type_identifier` is how tree-sitter parses a bare name in the common
        // mid-edit shape `recv.partial` (an ERROR / scoped_type_identifier), so
        // it must resolve like `identifier` — instance var or static type name.
        "identifier" | "type_identifier" => {
            resolve_name_to_type(node_text(recv, ctx.doc.source), recv.start_byte(), ctx)
        }
        "field_access" => {
            let obj = recv.child_by_field_name("object")?;
            let field = recv.child_by_field_name("field")?;
            let obj_ty = resolve_receiver_depth(obj, ctx, depth + 1)?;
            // Only an in-project object exposes a field whose declared type we can
            // re-resolve (external field-type chaining is deferred).
            let ResolvedType::InProject(td) = &obj_ty.ty else {
                return None;
            };
            let member = ctx.table.find_member(td, node_text(field, ctx.doc.source))?;
            let type_node = field_type_node(member.node)?;
            resolve_type_node(type_node, member.source, ctx).map(instance)
        }
        "object_creation_expression" => {
            let ty = recv.child_by_field_name("type")?;
            resolve_type_node(ty, ctx.doc.source, ctx).map(instance)
        }
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
    if let Some(binding) = lookup_binding(ctx.doc.tree, ctx.doc.source, byte, name, ctx.table) {
        let type_node = binding.type_node?;
        return resolve_type_node(type_node, binding.source, ctx).map(instance);
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

/// Resolve a declared-type node to a receiver type: in-project if the open docs
/// declare it, else an external FQN (fully-qualified use, or a simple name
/// resolved through imports + the symbol source).
fn resolve_type_node<'t>(
    type_node: Node<'t>,
    source: &'t str,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
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
    resolve_simple_to_fqn(simple, ctx).map(|fqn| ResolvedType::External { fqn, args: Vec::new() })
}

/// First import candidate FQN that the symbol source can actually resolve.
fn resolve_simple_to_fqn(simple: &str, ctx: &Ctx) -> Option<String> {
    ctx.imports
        .candidates(simple)
        .into_iter()
        .find(|fqn| ctx.symbols.class(fqn).is_some())
}

/// The full dotted name of a fully-qualified type node (`java.util.List`), or
/// `None` for a simple (unqualified) type.
fn dotted_type_name(type_node: Node, source: &str) -> Option<String> {
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

/// Resolve a dotted path (`a.b.c`). Prefer an in-project var.field chain; failing
/// that, treat the whole dotted name as a fully-qualified external type (static).
fn resolve_scoped_path<'t>(node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Resolved<'t>> {
    let names = flatten_scoped(node, ctx.doc.source);
    if let Some(resolved) = resolve_inproject_chain(&names, node.start_byte(), ctx) {
        return Some(resolved);
    }
    let fqn = names.join(".");
    if ctx.symbols.class(&fqn).is_some() {
        Some(Resolved {
            ty: ResolvedType::External {
                fqn,
                args: Vec::new(),
            },
            static_only: true,
        })
    } else {
        None
    }
}

fn resolve_inproject_chain<'t>(
    names: &[&str],
    first_byte: usize,
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
    let (first, rest) = names.split_first()?;
    let mut current = resolve_name_to_type(first, first_byte, ctx)?;
    for segment in rest {
        let ResolvedType::InProject(td) = &current.ty else {
            return None;
        };
        let member = ctx.table.find_member(td, segment)?;
        let type_node = field_type_node(member.node)?;
        current = instance(resolve_type_node(type_node, member.source, ctx)?);
    }
    Some(current)
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
                    walk_members(&ResolvedType::InProject(sd.clone()), ctx, static_only, acc, depth + 1);
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
        ResolvedType::External { fqn, args } => {
            if !acc.visited_fqn.insert(fqn.clone()) {
                return;
            }
            let Some(class) = ctx.symbols.class(fqn) else {
                return;
            };
            for m in class.members {
                if static_only && !m.is_static {
                    continue;
                }
                // Dedup on the erased signature (stable across declarations);
                // display the generic signature substituted with the use-site
                // type arguments (e.g. `add({0})` + `[String]` → `add(String)`).
                if acc.seen.insert(m.signature.clone()) {
                    let signature = display_signature(&m, args, &class.type_params);
                    acc.out.push(HierMember::External(ExternalMember { signature, ..m }));
                }
            }
            // Supertype members render erased (parameterized-super args untracked).
            for sup in class.supers {
                walk_members(
                    &ResolvedType::External {
                        fqn: sup,
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
                        &ResolvedType::External { fqn, args: Vec::new() },
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
        ResolvedType::External { fqn, .. } => {
            if !visited_fqn.insert(fqn.clone()) {
                return;
            }
            match ctx.symbols.class(fqn) {
                Some(class) => {
                    for m in &class.members {
                        names.insert(m.name.clone());
                    }
                    for sup in class.supers {
                        diag_walk(
                            &ResolvedType::External { fqn: sup, args: Vec::new() },
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
/// erased signature.
fn display_signature(member: &ExternalMember, args: &[String], type_params: &[String]) -> String {
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
    if include_fields {
        if let Some(type_node) = enclosing_type {
            if let Some(td) = TypeDecl::from_node(type_node, source) {
                push_fields(&td, table, &mut out);
            }
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
                match p.kind() {
                    "formal_parameter" => {
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
    collect_bindings(tree, source, byte, table, true)
        .into_iter()
        .find(|b| b.name == name)
}
