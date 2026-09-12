//! Cursor-relative resolution: enclosing type, in-scope bindings, and
//! receiver-expression → type. All best-effort and `Option`-returning — an
//! unresolved query yields `None`, never a panic, even on tree-sitter
//! ERROR/MISSING recovery nodes.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tree_sitter::{Node, Tree};

use jvl_types::{Access, ClassKind, ClassMetadata, PrimitiveType, TypeId, TypeRef, TypeVariableId};

use crate::external::{ExternalMember, ExternalMemberKind, SymbolSource};
use crate::imports::Imports;
use crate::model::{named_children, DeclSite, Member, MemberKind, TypeDecl, TypeKind, TypeTable};
use crate::{node_text, OpenDoc};

/// Depth cap for receiver/path resolution — real receiver chains are a handful
/// deep; this bounds stack use on pathological nesting without affecting any
/// legitimate code.
const MAX_RESOLVE_DEPTH: usize = 64;

/// Everything resolution needs: the cursor's document, the in-project type table,
/// the file's imports, and the external symbol source (JDK/deps).
pub(crate) struct Ctx<'a, 't> {
    pub doc: &'a OpenDoc<'t>,
    /// Index of `doc` in the `&[OpenDoc]` slice `table` was built from.
    /// Needed to stamp a [`DeclSite`] on bindings/types declared in `doc`
    /// itself (e.g. the enclosing type via `this`/`super`).
    pub current: usize,
    pub table: &'a TypeTable<'t>,
    pub imports: &'a Imports,
    pub symbols: &'a dyn SymbolSource,
    /// The same `&[OpenDoc]` slice `table` was built from. Used by
    /// [`Ctx::for_document`] to resolve a member's or supertype's declared
    /// type against the document that actually declares it.
    pub docs: &'a [OpenDoc<'t>],
    /// Per-request memo of [`class_facts`], shared across every `Ctx` view
    /// from [`Ctx::for_document`] so a class reached through several
    /// hierarchy steps is only ever extracted once.
    pub facts: &'a FactsCache,
}

impl<'a, 't> Ctx<'a, 't> {
    /// Views the same table/symbols/facts from another document's own
    /// imports/package — for resolving a member's or supertype's declared
    /// type where it's actually declared. `None` only when `doc` is out of
    /// range for `table`/`docs`.
    pub(crate) fn for_document(&self, doc: usize) -> Option<Ctx<'a, 't>> {
        let dc = self.table.doc_context(doc)?;
        Some(Ctx {
            doc: self.docs.get(doc)?,
            current: doc,
            table: self.table,
            imports: &dc.imports,
            symbols: self.symbols,
            docs: self.docs,
            facts: self.facts,
        })
    }
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
    /// Index into the `&[OpenDoc]` slice of the document this binding is
    /// declared in. An inherited field may point elsewhere; locals/params/
    /// for-vars are always the current doc.
    pub doc: usize,
}

impl<'t> Binding<'t> {
    /// Where this binding is declared.
    pub(crate) fn decl_site(&self) -> Option<DeclSite> {
        let name = binding_name_node(self.decl_node)?;
        Some(DeclSite::new(self.doc, name, self.decl_node))
    }
}

/// The name-identifier node of a binding's declaration node. Most decl nodes
/// carry a `name` field; a bare lambda parameter's decl node *is* its name,
/// and a varargs parameter's name lives on its nested `variable_declarator`.
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

/// A conservatively resolved Java value type. Reference types keep structured
/// [`TypeRef`]s, never rendered strings, so [`assignable_refs`] can compare
/// them directly; primitives, `void`, `null`, and arrays stay distinct so
/// callers never reconstruct value semantics from text.
pub(crate) enum ResolvedType<'t> {
    InProject {
        decl: TypeDecl<'t>,
        args: Vec<TypeRef>,
    },
    External {
        fqn: String,
        args: Vec<TypeRef>,
    },
    Primitive(PrimitiveType),
    Void,
    Null,
    Array {
        /// The array's element type, one level peeled (`String[][]`'s
        /// element is itself `Array(String)`).
        element: TypeRef,
    },
}

impl<'t> ResolvedType<'t> {
    /// This resolved type's identity as a structured [`TypeRef`] — the sole
    /// bridge into the subtype engine ([`assignable_refs`]/[`is_subtype`]),
    /// which never touches `TypeDecl`/`ExternalClass` directly.
    pub(crate) fn type_ref(&self) -> TypeRef {
        match self {
            ResolvedType::InProject { decl, args } => TypeRef::Named {
                id: decl.type_id.clone(),
                args: args.clone(),
            },
            ResolvedType::External { fqn, args } => TypeRef::Named {
                id: TypeId::named(fqn),
                args: args.clone(),
            },
            ResolvedType::Primitive(p) => TypeRef::Primitive(*p),
            ResolvedType::Void => TypeRef::Void,
            ResolvedType::Null => TypeRef::Null,
            ResolvedType::Array { element } => TypeRef::Array(Box::new(element.clone())),
        }
    }

    /// Structured `TypeRef` -> resolved type: looks up a `Named` declaration
    /// in `ctx.table` first, then falls back to the external symbol source,
    /// never guessing across an ambiguous in-project name. `None` for a
    /// type variable/wildcard/unknown — none name a concrete receiver.
    pub(crate) fn from_type_ref(ty: &TypeRef, ctx: &Ctx<'_, 't>) -> Option<ResolvedType<'t>> {
        Some(match ty {
            TypeRef::Primitive(p) => ResolvedType::Primitive(*p),
            TypeRef::Void => ResolvedType::Void,
            TypeRef::Null => ResolvedType::Null,
            TypeRef::Array(e) => ResolvedType::Array {
                element: (**e).clone(),
            },
            TypeRef::Named {
                id: TypeId::Named(b),
                args,
            } => match ctx.table.get_named(b) {
                Some(d) => ResolvedType::InProject {
                    decl: d.clone(),
                    args: args.clone(),
                },
                None if ctx.table.is_duplicate(b) => return None,
                None => {
                    ctx.symbols.class(b)?;
                    ResolvedType::External {
                        fqn: b.clone(),
                        args: args.clone(),
                    }
                }
            },
            TypeRef::Named {
                id:
                    TypeId::Local {
                        document,
                        declaration,
                    },
                args,
            } => ResolvedType::InProject {
                decl: ctx.table.by_node(*document, *declaration)?.clone(),
                args: args.clone(),
            },
            TypeRef::Variable(_) | TypeRef::Wildcard { .. } | TypeRef::Unknown => return None,
        })
    }
}

/// Render a structured [`TypeRef`] as it would read in a signature: simple
/// name, `<args>` for generics, `[]` per array level, `?`/`? extends X`/
/// `? super X` for wildcards. Display only — never fed back into the
/// subtype engine.
pub(crate) fn render_type_ref(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Primitive(p) => p.name().to_string(),
        TypeRef::Void => "void".to_string(),
        TypeRef::Null => "null".to_string(),
        TypeRef::Named { id, args } => {
            let simple = match id {
                TypeId::Named(b) => b
                    .rsplit('.')
                    .next()
                    .and_then(|s| s.rsplit('$').next())
                    .unwrap_or(b)
                    .to_string(),
                TypeId::Local { .. } => "?".to_string(),
            };
            if args.is_empty() {
                simple
            } else {
                let rendered: Vec<String> = args.iter().map(render_type_ref).collect();
                format!("{simple}<{}>", rendered.join(", "))
            }
        }
        TypeRef::Array(element) => format!("{}[]", render_type_ref(element)),
        TypeRef::Wildcard { upper: Some(u), .. } => format!("? extends {}", render_type_ref(u)),
        TypeRef::Wildcard { lower: Some(l), .. } => format!("? super {}", render_type_ref(l)),
        TypeRef::Wildcard { .. } | TypeRef::Variable(_) | TypeRef::Unknown => "?".to_string(),
    }
}

/// Render a structured type for a source-aware UI surface. Unlike
/// [`render_type_ref`], this preserves class type-variable names when their
/// declaration is available in the current project or symbol source.
pub(crate) fn render_type_ref_in(ty: &TypeRef, ctx: &Ctx<'_, '_>) -> String {
    render_type_ref_with(ty, &|variable| type_variable_name(variable, ctx))
}

fn render_type_ref_with(
    ty: &TypeRef,
    variable_name: &dyn Fn(&TypeVariableId) -> Option<String>,
) -> String {
    match ty {
        TypeRef::Primitive(p) => p.name().to_string(),
        TypeRef::Void => "void".to_string(),
        TypeRef::Null => "null".to_string(),
        TypeRef::Named { id, args } => {
            let simple = match id {
                TypeId::Named(b) => b
                    .rsplit('.')
                    .next()
                    .and_then(|s| s.rsplit('$').next())
                    .unwrap_or(b)
                    .to_string(),
                TypeId::Local { .. } => "?".to_string(),
            };
            if args.is_empty() {
                simple
            } else {
                let rendered: Vec<String> = args
                    .iter()
                    .map(|arg| render_type_ref_with(arg, variable_name))
                    .collect();
                format!("{simple}<{}>", rendered.join(", "))
            }
        }
        TypeRef::Array(element) => {
            format!("{}[]", render_type_ref_with(element, variable_name))
        }
        TypeRef::Variable(variable) => variable_name(variable).unwrap_or_else(|| "?".to_string()),
        TypeRef::Wildcard { upper: Some(u), .. } => {
            format!("? extends {}", render_type_ref_with(u, variable_name))
        }
        TypeRef::Wildcard { lower: Some(l), .. } => {
            format!("? super {}", render_type_ref_with(l, variable_name))
        }
        TypeRef::Wildcard { .. } | TypeRef::Unknown => "?".to_string(),
    }
}

fn type_variable_name(variable: &TypeVariableId, ctx: &Ctx<'_, '_>) -> Option<String> {
    if let Some((class_owner, _)) = variable.owner.split_once('#') {
        let decl = ctx.table.get_named(class_owner)?;
        for member in decl.own_members() {
            if !matches!(
                member.node.kind(),
                "method_declaration" | "constructor_declaration"
            ) {
                continue;
            }
            let name = node_text(member.node.child_by_field_name("name")?, member.source);
            let params = member
                .node
                .child_by_field_name("parameters")
                .map(|node| node_text(node, member.source))
                .unwrap_or("()");
            let source_owner = format!("{class_owner}#{name}{params}");
            let constructor_owner = format!("{class_owner}#<init>{params}");
            if variable.owner == source_owner || variable.owner == constructor_owner {
                return type_parameter_name(
                    member.node.child_by_field_name("type_parameters")?,
                    variable.index,
                    member.source,
                );
            }
        }
        return None;
    }
    if let Some(decl) = ctx.table.get_named(&variable.owner) {
        return type_parameter_name(
            decl.node.child_by_field_name("type_parameters")?,
            variable.index,
            decl.source,
        );
    }
    ctx.symbols
        .class(&variable.owner)?
        .type_params
        .get(variable.index)
        .cloned()
}

fn type_parameter_name(params: Node, index: usize, source: &str) -> Option<String> {
    named_children(params)
        .into_iter()
        .filter(|node| node.kind() == "type_parameter")
        .nth(index)
        .and_then(|param| {
            named_children(param)
                .into_iter()
                .find(|node| node.kind() == "type_identifier")
        })
        .map(|name| node_text(name, source).to_string())
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
/// shapes. `pub(crate)`: `rename.rs` uses it to walk past a nested type's
/// own declaration to find its *outer* enclosing type.
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

/// The [`TypeDecl`] for the type enclosing `node`, declared in document
/// `doc` — looked up directly in `table` (never re-parsed), so it always
/// carries the same qualified identity `table` indexed it under.
pub(crate) fn enclosing_typedecl<'t>(
    node: Node<'t>,
    table: &TypeTable<'t>,
    doc: usize,
) -> Option<TypeDecl<'t>> {
    table.by_node(doc, enclosing_type_node(node)?.id()).cloned()
}

/// Binary names of every type declaration enclosing `node`, in `ctx`'s own
/// document, innermost first. Used by constructor accessibility (visible
/// from any nesting level of its own top-level type) and enclosing-instance
/// checks.
pub(crate) fn enclosing_binary_names(node: Node, ctx: &Ctx<'_, '_>) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        let Some(type_node) = enclosing_type_node(n) else {
            break;
        };
        if let Some(td) = ctx.table.by_node(ctx.current, type_node.id()) {
            if let Some(b) = &td.binary_name {
                out.push(b.clone());
            }
        }
        cur = type_node.parent();
    }
    out
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

#[derive(Clone, Copy)]
enum MethodLookup {
    First,
    Unique,
}

/// Resolve a receiver expression using the historical first-name method lookup
/// used by completion and hover.
pub(crate) fn resolve_receiver_type<'t>(recv: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Resolved<'t>> {
    resolve_receiver_depth(recv, ctx, 0, MethodLookup::First)
}

/// The receiver of a call being semantically checked: same as
/// [`resolve_receiver_type`] but with argument-aware (`Unique`) lookup, so an
/// unknown generic call in the chain stays unknown instead of being guessed.
pub(crate) fn resolve_checked_receiver<'t>(
    recv: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
    resolve_receiver_depth(recv, ctx, 0, MethodLookup::Unique)
}

/// Resolve the value type of an expression for conservative semantic checks.
/// An overloaded same-name call is unknown because this layer does not perform
/// Java overload selection.
pub(crate) fn resolve_expression_type<'t>(
    expression: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let resolved = resolve_receiver_depth(expression, ctx, 0, MethodLookup::Unique)?;
    (!resolved.static_only).then_some(resolved.ty)
}

fn resolve_receiver_depth<'t>(
    recv: Node<'t>,
    ctx: &Ctx<'_, 't>,
    depth: usize,
    method_lookup: MethodLookup,
) -> Option<Resolved<'t>> {
    if depth > MAX_RESOLVE_DEPTH {
        return None;
    }
    let _nesting = ctx.facts.enter()?;
    match recv.kind() {
        "this" => enclosing_typedecl(recv, ctx.table, ctx.current).map(|td| {
            instance(ResolvedType::InProject {
                decl: td,
                args: Vec::new(),
            })
        }),
        "super" => {
            let td = enclosing_typedecl(recv, ctx.table, ctx.current)?;
            let sup_node = *td.super_nodes.first()?;
            resolve_type_node(sup_node, td.source, ctx).map(instance)
        }
        "identifier" | "type_identifier" => resolve_name_depth(
            node_text(recv, ctx.doc.source),
            recv.start_byte(),
            ctx,
            depth + 1,
            method_lookup,
        ),
        "method_invocation" => match method_lookup {
            // The arity-aware applicability engine — the only mode
            // semantic checks (`resolve_expression_type`) use.
            MethodLookup::Unique => match crate::call::resolve_method_call(recv, ctx) {
                crate::call::CallResolution::Selected { result, .. } => {
                    ResolvedType::from_type_ref(&result, ctx).map(instance)
                }
                _ => None,
            },
            // Completion/hover's historical first-name lookup: ignores
            // argument types entirely, so any name match resolves.
            MethodLookup::First => {
                let name = recv.child_by_field_name("name")?;
                let recv_ty = match recv.child_by_field_name("object") {
                    Some(obj) => resolve_receiver_depth(obj, ctx, depth + 1, method_lookup)?,
                    None => instance(ResolvedType::InProject {
                        decl: enclosing_typedecl(recv, ctx.table, ctx.current)?,
                        args: Vec::new(),
                    }),
                };
                let member = find_method(
                    &recv_ty,
                    ctx,
                    node_text(name, ctx.doc.source),
                    method_lookup,
                )?;
                member_result_type(&member, &recv_ty, ctx, method_lookup)
            }
        },
        "cast_expression" => {
            let ty = recv.child_by_field_name("type")?;
            resolve_type_node(ty, ctx.doc.source, ctx).map(instance)
        }
        "array_access" => {
            let arr = recv.child_by_field_name("array")?;
            let a = resolve_receiver_depth(arr, ctx, depth + 1, method_lookup)?;
            match &a.ty {
                ResolvedType::Array { element } => array_element_type(element, ctx),
                _ => None,
            }
        }
        "true" | "false" => Some(instance(ResolvedType::Primitive(PrimitiveType::Boolean))),
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => {
            let text = node_text(recv, ctx.doc.source);
            let ty = if text.ends_with('l') || text.ends_with('L') {
                PrimitiveType::Long
            } else {
                PrimitiveType::Int
            };
            Some(instance(ResolvedType::Primitive(ty)))
        }
        "decimal_floating_point_literal" | "hex_floating_point_literal" => {
            let text = node_text(recv, ctx.doc.source);
            let ty = if text.ends_with('f') || text.ends_with('F') {
                PrimitiveType::Float
            } else {
                PrimitiveType::Double
            };
            Some(instance(ResolvedType::Primitive(ty)))
        }
        "character_literal" => Some(instance(ResolvedType::Primitive(PrimitiveType::Char))),
        "string_literal" | "text_block" => Some(instance(ResolvedType::External {
            fqn: "java.lang.String".to_string(),
            args: Vec::new(),
        })),
        "null_literal" => Some(instance(ResolvedType::Null)),
        "field_access" => {
            let obj = recv.child_by_field_name("object")?;
            let field = recv.child_by_field_name("field")?;
            if let Some(obj_ty) = resolve_receiver_depth(obj, ctx, depth + 1, method_lookup) {
                return resolve_member_segment(
                    &obj_ty,
                    node_text(field, ctx.doc.source),
                    ctx,
                    method_lookup,
                );
            }
            resolve_scoped_path(recv, ctx, method_lookup)
        }
        "object_creation_expression" => match method_lookup {
            // The created type comes from actual constructor selection
            // (arity, diamond inference), not just the written name; an
            // inapplicable `new` resolves to nothing.
            MethodLookup::Unique => match crate::call::resolve_constructor_call(recv, ctx) {
                Ok(crate::call::CallResolution::Selected { result, .. }) => {
                    ResolvedType::from_type_ref(&result, ctx).map(instance)
                }
                _ => None,
            },
            // Completion/hover's lookup: the written type name alone,
            // regardless of constructor applicability, so a chain off
            // `new Foo(...)` keeps resolving.
            MethodLookup::First => resolve_object_creation_type(recv, ctx).map(instance),
        },
        "scoped_type_identifier" | "scoped_identifier" => {
            resolve_scoped_path(recv, ctx, method_lookup)
        }
        "parenthesized_expression" => {
            resolve_receiver_depth(recv.named_child(0)?, ctx, depth + 1, method_lookup)
        }
        // A ternary's value is whichever branch's type the other widens to
        // (full JLS 15.25 numeric-promotion/lub is out of scope). Two
        // unrelated branches, neither assignable to the other, stay unknown
        // rather than guessing a common supertype.
        "ternary_expression" => {
            let consequence = recv.child_by_field_name("consequence")?;
            let alternative = recv.child_by_field_name("alternative")?;
            let c = resolve_receiver_depth(consequence, ctx, depth + 1, method_lookup)?;
            let a = resolve_receiver_depth(alternative, ctx, depth + 1, method_lookup)?;
            if assignable_refs(&c.ty.type_ref(), &a.ty.type_ref(), ctx) == Some(true) {
                Some(a)
            } else if assignable_refs(&a.ty.type_ref(), &c.ty.type_ref(), ctx) == Some(true) {
                Some(c)
            } else {
                None
            }
        }
        // An assignment expression's own value is the (possibly narrowed)
        // type of its target — resolve `left` the same way any other
        // receiver would be.
        "assignment_expression" => {
            let left = recv.child_by_field_name("left")?;
            resolve_receiver_depth(left, ctx, depth + 1, method_lookup)
        }
        // Only `+` with a `String` operand is typed (concatenation);
        // comparison/logical operators always yield `boolean`. Other
        // operators (arithmetic/bitwise) are out of scope here.
        "binary_expression" => {
            let operator = node_text(recv.child_by_field_name("operator")?, ctx.doc.source);
            match operator {
                "+" => {
                    let left = recv.child_by_field_name("left")?;
                    let right = recv.child_by_field_name("right")?;
                    let is_string = |n: Node<'t>| {
                        resolve_receiver_depth(n, ctx, depth + 1, method_lookup).is_some_and(|r| {
                            matches!(&r.ty, ResolvedType::External { fqn, .. } if fqn == "java.lang.String")
                        })
                    };
                    if is_string(left) || is_string(right) {
                        Some(instance(ResolvedType::External {
                            fqn: "java.lang.String".to_string(),
                            args: Vec::new(),
                        }))
                    } else {
                        None
                    }
                }
                "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => {
                    Some(instance(ResolvedType::Primitive(PrimitiveType::Boolean)))
                }
                _ => None,
            }
        }
        // `x instanceof Type` (and its pattern form) is always `boolean`, a
        // distinct node kind from `binary_expression` in this grammar.
        "instanceof_expression" => Some(instance(ResolvedType::Primitive(PrimitiveType::Boolean))),
        // `!` is the only `unary_expression` operator typed here; `+`/`-`/`~`
        // need numeric-promotion typing (out of scope). Increment/decrement
        // are a distinct `update_expression` node, never reaching here.
        "unary_expression" => {
            let operator = node_text(recv.child_by_field_name("operator")?, ctx.doc.source);
            (operator == "!").then(|| instance(ResolvedType::Primitive(PrimitiveType::Boolean)))
        }
        _ => None,
    }
}

fn find_method<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    name: &str,
    lookup: MethodLookup,
) -> Option<HierMember<'t>> {
    match lookup {
        MethodLookup::First => {
            find_member_hier_of_kind(resolved, ctx, name, MemberNamespace::Method)
        }
        MethodLookup::Unique => {
            let mut matches = collect_members(resolved, ctx)
                .into_iter()
                .filter(|member| member.name() == name && MemberNamespace::Method.matches(member));
            let member = matches.next()?;
            matches.next().is_none().then_some(member)
        }
    }
}

/// Depth cap for the subtype engine's hierarchy walk, distinct from
/// [`MAX_RESOLVE_DEPTH`] since the two walks are unrelated.
const MAX_SUBTYPE_DEPTH: usize = 64;

/// Per-request memo of [`class_facts`] results — the subtype engine calls it
/// repeatedly for the same handful of classes, so each is extracted once.
/// One instance per top-level request; never persisted, so no invalidation
/// is needed.
#[derive(Default)]
pub(crate) struct FactsCache {
    facts: RefCell<HashMap<TypeId, Option<Rc<ClassFacts>>>>,
    /// Current nesting of semantic resolutions in this request. Shared
    /// across `resolve_receiver_depth` ↔ `call::resolve_method_call`
    /// re-entry, which restarts the per-call `depth` counter.
    nesting: std::cell::Cell<u32>,
}

/// Ceiling on nested expression resolutions. Real code stays below 20;
/// pathological nesting hits the cap and resolves to Unknown (silent).
const MAX_NESTING: u32 = 64;

/// RAII token: decrements the nesting level when dropped.
pub(crate) struct NestingGuard<'a>(&'a std::cell::Cell<u32>);

impl Drop for NestingGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl FactsCache {
    /// Enter one nesting level; `None` when the cap is reached (caller must
    /// return "unknown"). Hold the guard for the whole resolution.
    pub(crate) fn enter(&self) -> Option<NestingGuard<'_>> {
        let n = self.nesting.get();
        if n >= MAX_NESTING {
            return None;
        }
        self.nesting.set(n + 1);
        Some(NestingGuard(&self.nesting))
    }
}

/// The metadata + member set the subtype/applicability engines need for one
/// class-like type, in-project or external, addressed by [`TypeId`] alone —
/// never re-derived from a `TypeDecl`/`ExternalClass` at each hierarchy step.
pub(crate) struct ClassFacts {
    pub meta: ClassMetadata,
    #[allow(dead_code)] // consumed by the method/constructor-applicability engine
    pub members: Vec<ExternalMember>,
}

/// Facts for `id`, memoized in `ctx.facts` for the lifetime of this request.
/// `None` when `id` is unresolvable, ambiguous (a duplicated in-project
/// binary name), or the source that would supply it carries no structured
/// metadata (a test stub, a synthetic lombok/array class) — every caller
/// must treat that identically to "unknown", never guess.
pub(crate) fn class_facts(id: &TypeId, ctx: &Ctx<'_, '_>) -> Option<Rc<ClassFacts>> {
    if let Some(hit) = ctx.facts.facts.borrow().get(id) {
        return hit.clone();
    }
    let computed = compute_class_facts(id, ctx).map(Rc::new);
    ctx.facts
        .facts
        .borrow_mut()
        .insert(id.clone(), computed.clone());
    computed
}

fn compute_class_facts(id: &TypeId, ctx: &Ctx<'_, '_>) -> Option<ClassFacts> {
    let source_decl = match id {
        TypeId::Named(b) => {
            if ctx.table.is_duplicate(b) {
                return None;
            }
            ctx.table.get_named(b)
        }
        TypeId::Local {
            document,
            declaration,
        } => ctx.table.by_node(*document, *declaration),
    };
    if let Some(d) = source_decl {
        let dctx = ctx.for_document(d.doc)?;
        let pick = |cands: &[String]| -> Option<String> {
            cands
                .iter()
                .find(|c| ctx.table.get_named(c).is_some() || ctx.symbols.class(c).is_some())
                .cloned()
        };
        let ext = crate::srcclass::to_external_class(d, d.source, dctx.imports, &pick);
        return Some(ClassFacts {
            meta: ext.metadata?,
            members: ext.members,
        });
    }
    let b = id.as_named()?;
    if b == "java.lang.Object" && ctx.symbols.class(b).is_none() {
        // Intrinsic terminal: every reference type is an Object even with no
        // JDK on the classpath. No members are invented — only the
        // subtype-proof-relevant metadata (an empty, complete hierarchy).
        return Some(ClassFacts {
            meta: ClassMetadata {
                id: id.clone(),
                kind: ClassKind::Class,
                access: Access::Public,
                is_abstract: false,
                is_static: true,
                enclosing_class: None,
                type_parameters: Vec::new(),
                supertypes: Vec::new(),
                hierarchy_complete: true,
                constructors_complete: false,
            },
            members: Vec::new(),
        });
    }
    let ext = ctx.symbols.class(b)?;
    Some(ClassFacts {
        meta: ext.metadata?,
        members: ext.members,
    })
}

/// Whether `actual` can be assigned/returned where `expected` is declared.
/// `None` means this conservative layer cannot prove either answer — never a
/// wrong guess.
pub(crate) fn is_assignable<'t>(
    actual: &ResolvedType<'t>,
    expected: &ResolvedType<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<bool> {
    assignable_refs(&actual.type_ref(), &expected.type_ref(), ctx)
}

/// The structured subtype/assignability engine: JLS 5.2/5.1.2 conversions
/// (identity, widening, boxing/unboxing) plus [`is_subtype`]'s hierarchy walk
/// for reference types, covering in-project, external, and mixed
/// hierarchies alike through [`class_facts`].
pub(crate) fn assignable_refs(
    actual: &TypeRef,
    expected: &TypeRef,
    ctx: &Ctx<'_, '_>,
) -> Option<bool> {
    use TypeRef::*;
    match (actual, expected) {
        (Unknown, _) | (_, Unknown) => None,
        (Null, e) if e.is_reference() => Some(true),
        (Null, _) | (_, Null) => Some(false),
        (Void, Void) => Some(true),
        (Void, _) | (_, Void) => Some(false),
        // Variable/wildcard cases are handled before any primitive/array
        // catch-all below so neither side's "is a wildcard/variable"
        // question is ever pre-empted by a broader arm.
        (Variable(v), e) => {
            let bounds = variable_bounds(v, ctx)?;
            let bounds = if bounds.is_empty() {
                vec![TypeRef::named("java.lang.Object")]
            } else {
                bounds
            };
            any_proved(bounds.iter().map(|b| assignable_refs(b, e, ctx)))
        }
        (_, Variable(_)) => None,
        (Wildcard { upper, .. }, e) => match upper {
            Some(u) => assignable_refs(u, e, ctx),
            None => assignable_refs(&TypeRef::named("java.lang.Object"), e, ctx),
        },
        (_, Wildcard { .. }) => None,
        (Primitive(a), Primitive(e)) => {
            if a == e || a.widens_to(*e) {
                Some(true)
            } else {
                constant_narrowing(*a, *e)
            }
        }
        // Boxing to a different primitive's box type is never a combined
        // box+widen — decided by primitive rules alone (including JLS 5.2's
        // constant-narrowing-then-boxing allowance). An in-project/local
        // class name is never a primitive's box type, so it's provably
        // false without any classpath lookup.
        (
            Primitive(_),
            Named {
                id: TypeId::Local { .. },
                ..
            },
        ) => Some(false),
        (
            Primitive(_),
            Named {
                id: TypeId::Named(eb),
                ..
            },
        ) if ctx.table.get_named(eb).is_some() => Some(false),
        (
            Primitive(a),
            Named {
                id: TypeId::Named(eb),
                ..
            },
        ) => match PrimitiveType::from_box_fqn(eb) {
            Some(ep) if *a == ep => Some(true),
            Some(ep) => constant_narrowing(*a, ep),
            None => assignable_refs(&TypeRef::named(a.box_fqn()), expected, ctx),
        },
        (Primitive(_), _) => Some(false),
        (
            Named {
                id: TypeId::Named(b),
                ..
            },
            Primitive(e),
        ) => PrimitiveType::from_box_fqn(b)
            .map(|p| p == *e || p.widens_to(*e))
            .or(Some(false)),
        (_, Primitive(_)) => Some(false),
        (Array(a), Array(e)) => match (&**a, &**e) {
            (Primitive(x), Primitive(y)) => Some(x == y),
            (Primitive(_), _) | (_, Primitive(_)) => Some(false),
            (x, y) => assignable_refs(x, y, ctx),
        },
        (
            Array(_),
            Named {
                id: TypeId::Named(b),
                ..
            },
        ) => Some(matches!(
            b.as_str(),
            "java.lang.Object" | "java.lang.Cloneable" | "java.io.Serializable"
        )),
        (Array(_), _) | (_, Array(_)) => Some(false),
        (
            Named { .. },
            Named {
                id: eid,
                args: eargs,
            },
        ) => is_subtype(actual, eid, eargs, ctx, 0, &mut HashSet::new()),
    }
}

/// Prove the substituted subtype path: `Some(false)` requires every path to
/// be complete, while any unresolved hierarchy returns `None`.
fn is_subtype(
    actual: &TypeRef,
    target: &TypeId,
    target_args: &[TypeRef],
    ctx: &Ctx<'_, '_>,
    depth: usize,
    seen: &mut HashSet<TypeRef>,
) -> Option<bool> {
    let TypeRef::Named { id, args } = actual else {
        return None;
    };
    if depth > MAX_SUBTYPE_DEPTH || !seen.insert(actual.clone()) {
        return None;
    }
    // `seen` guards only the current path (cyclic `extends`), never a
    // diamond — two supertypes converging on a common ancestor must each
    // independently re-walk it. Removed on every exit so a sibling branch
    // isn't starved by an earlier visit.
    let result = (|| {
        if id == target {
            return type_args_contain(args, target_args, ctx);
        }
        if target.as_named() == Some("java.lang.Object") {
            return Some(true);
        }
        // A "not a subtype" answer needs a real target class; an opaque or
        // unresolvable name can never be proven unrelated.
        if depth == 0 && class_facts(target, ctx).is_none() {
            return None;
        }
        let facts = class_facts(id, ctx)?;
        let env: Vec<(TypeVariableId, TypeRef)> = facts
            .meta
            .type_parameters
            .iter()
            .map(|p| p.id.clone())
            .zip(args.iter().cloned())
            .collect();
        let raw = args.is_empty() && !facts.meta.type_parameters.is_empty();
        let mut complete = facts.meta.hierarchy_complete;
        for sup in &facts.meta.supertypes {
            let sup = if raw {
                erase(sup)
            } else {
                sup.substitute(&env)
            };
            match is_subtype(&sup, target, target_args, ctx, depth + 1, seen) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => complete = false,
            }
        }
        if complete {
            Some(false)
        } else {
            None
        }
    })();
    seen.remove(actual);
    result
}

/// JLS 4.5.1 expected-side containment: `?` contains anything,
/// `? extends B` accepts a concrete/sub-wildcard upper bounded by `B`, and
/// `? super B` accepts a concrete/super-wildcard lower-bounding `B`.
/// Raw expected accepts unconditionally; raw actual into parameterized
/// expected is unknown.
fn type_args_contain(actual: &[TypeRef], expected: &[TypeRef], ctx: &Ctx<'_, '_>) -> Option<bool> {
    if expected.is_empty() {
        return Some(true);
    }
    if actual.is_empty() || actual.len() != expected.len() {
        return None;
    }
    let mut unknown = false;
    for (actual, expected) in actual.iter().zip(expected) {
        let contains = match (actual, expected) {
            (
                _,
                TypeRef::Wildcard {
                    upper: None,
                    lower: None,
                },
            ) => Some(true),
            (TypeRef::Unknown, _) | (_, TypeRef::Unknown) => None,
            (TypeRef::Variable(_), _) | (_, TypeRef::Variable(_)) => None,
            (
                TypeRef::Wildcard {
                    upper: actual_upper,
                    lower: None,
                },
                TypeRef::Wildcard {
                    upper: Some(expected_upper),
                    lower: None,
                },
            ) => match actual_upper {
                Some(actual_upper) => assignable_refs(actual_upper, expected_upper, ctx),
                None => assignable_refs(&TypeRef::named("java.lang.Object"), expected_upper, ctx),
            },
            (
                TypeRef::Wildcard {
                    lower: Some(actual_lower),
                    ..
                },
                TypeRef::Wildcard {
                    lower: Some(expected_lower),
                    ..
                },
            ) => assignable_refs(expected_lower, actual_lower, ctx),
            (
                TypeRef::Wildcard { .. },
                TypeRef::Wildcard {
                    upper: Some(_),
                    lower: None,
                },
            )
            | (
                TypeRef::Wildcard { .. },
                TypeRef::Wildcard {
                    lower: Some(_),
                    upper: None,
                },
            ) => None,
            (
                actual,
                TypeRef::Wildcard {
                    upper: Some(expected_upper),
                    lower: None,
                },
            ) => assignable_refs(actual, expected_upper, ctx),
            (
                actual,
                TypeRef::Wildcard {
                    lower: Some(expected_lower),
                    upper: None,
                },
            ) => assignable_refs(expected_lower, actual, ctx),
            (TypeRef::Wildcard { .. }, _) => Some(false),
            (actual, expected) => Some(actual == expected),
        };
        match contains {
            Some(false) => return Some(false),
            None => unknown = true,
            Some(true) => {}
        }
    }
    if unknown {
        None
    } else {
        Some(true)
    }
}

/// Erase a `TypeRef` to its raw form (drop generic arguments at every
/// level). `pub(crate)`: shared by the method/constructor-applicability
/// engine.
pub(crate) fn erase(t: &TypeRef) -> TypeRef {
    match t {
        TypeRef::Named { id, .. } => TypeRef::Named {
            id: id.clone(),
            args: Vec::new(),
        },
        TypeRef::Array(e) => TypeRef::Array(Box::new(erase(e))),
        other => other.clone(),
    }
}

/// `Some(true)` on the first proved path; `Some(false)` only when every path
/// is proved false; `None` when at least one path is unknown and none is
/// proved true.
fn any_proved(results: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut unknown = false;
    for r in results {
        match r {
            Some(true) => return Some(true),
            Some(false) => {}
            None => unknown = true,
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

/// The bounds of type variable `v` — its declaring class's own
/// `type_parameters` entry. Method/constructor-owned variables aren't
/// looked up this way (their owner string doesn't name a class), so any
/// reference to one stays `None` (unknown) here — conservative, never wrong.
fn variable_bounds(v: &TypeVariableId, ctx: &Ctx<'_, '_>) -> Option<Vec<TypeRef>> {
    let facts = class_facts(&TypeId::Named(v.owner.clone()), ctx)?;
    facts
        .meta
        .type_parameters
        .get(v.index)
        .map(|p| p.bounds.clone())
}

/// JLS 5.2: an `int`-family constant may still narrow-fit a smaller target
/// (`byte b = 5;`, even boxed `Byte b = 5;`), so that direction stays
/// unknown rather than a wrong `false`. Every other primitive narrowing is
/// proven incompatible.
fn constant_narrowing(actual: PrimitiveType, expected: PrimitiveType) -> Option<bool> {
    if matches!(
        actual,
        PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char | PrimitiveType::Int
    ) && matches!(
        expected,
        PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char
    ) {
        None
    } else {
        Some(false)
    }
}

/// Resolve a simple name at a position: a scope binding gives an instance type;
/// otherwise a bare type name gives static access (in-project or external).
pub(crate) fn resolve_name_to_type<'t>(
    name: &str,
    byte: usize,
    ctx: &Ctx<'_, 't>,
) -> Option<Resolved<'t>> {
    resolve_name_depth(name, byte, ctx, 0, MethodLookup::First)
}

fn resolve_name_depth<'t>(
    name: &str,
    byte: usize,
    ctx: &Ctx<'_, 't>,
    depth: usize,
    method_lookup: MethodLookup,
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
        if matches!(binding.kind, BindingKind::Param) {
            if let Some(inferred) =
                crate::call::inferred_lambda_parameter_type(binding.decl_node, ctx)
            {
                return receiver_type_ref(&inferred, ctx, depth + 1).map(instance);
            }
        }
        // A `var` (or typeless) binding infers its type from the
        // declarator's initializer, resolved like any receiver expression.
        // Depth-capped since ERROR-recovery trees can produce
        // self-referential shapes; the result is always an instance
        // regardless of the initializer's static-ness.
        let value = binding.decl_node.child_by_field_name("value")?;
        // A `var` in an enhanced-for binds the *element* type of the iterable,
        // not the iterable's own type: `for (var s : List<String>)` → `s` is a
        // `String`. Resolving `value` directly would give `List` and mis-flag
        // every member access on the loop variable.
        if binding.decl_node.kind() == "enhanced_for_statement" {
            let iterable = resolve_receiver_depth(value, ctx, depth + 1, method_lookup)?;
            return iterable_element_type(&iterable.ty, ctx).map(instance);
        }
        return resolve_receiver_depth(value, ctx, depth + 1, method_lookup)
            .map(|r| instance(r.ty));
    }
    if let Some(td) = ctx.table.resolve_type_name_node(
        node_at(ctx.doc.tree, byte),
        ctx.doc.source,
        ctx.current,
        ctx.imports,
    ) {
        return Some(Resolved {
            ty: ResolvedType::InProject {
                decl: td.clone(),
                args: Vec::new(),
            },
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

/// Display name for a resolved type, as it would read in a signature
/// (`ArrayList<String>`, `Widget`, `String[]`). Used to render an inferred
/// `var` type on hover.
pub(crate) fn type_display(ty: &ResolvedType) -> String {
    match ty {
        ResolvedType::InProject { decl, .. } => decl.name.to_string(),
        ResolvedType::External { fqn, args } => {
            let simple = fqn.rsplit('.').next().unwrap_or(fqn);
            if args.is_empty() {
                simple.to_string()
            } else {
                let rendered: Vec<String> = args.iter().map(render_type_ref).collect();
                format!("{simple}<{}>", rendered.join(", "))
            }
        }
        ResolvedType::Primitive(primitive) => primitive.name().to_string(),
        ResolvedType::Void => "void".to_string(),
        ResolvedType::Null => "null".to_string(),
        ResolvedType::Array { element } => format!("{}[]", render_type_ref(element)),
    }
}

/// The display string of a `var` local's *inferred* type (from its
/// initializer); `None` for an explicitly-typed binding or failed
/// inference. Mirrors [`resolve_name_depth`]'s `var` inference; used to
/// render hover on a `var` local.
pub(crate) fn inferred_var_type_display(name: &str, byte: usize, ctx: &Ctx) -> Option<String> {
    let binding = lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        byte,
        name,
        ctx.table,
        ctx.current,
    )?;
    let type_node = binding.type_node?;
    if node_text(type_node, binding.source) != "var" {
        return None;
    }
    let value = binding.decl_node.child_by_field_name("value")?;
    // A complete initializer needs the same overload selection used by
    // semantic checks; the historical first-name hover lookup drops generic
    // method inference (`var xs = List.of(value)` became `List<?>`). Keep the
    // permissive lookup only as a fallback for half-typed code.
    let resolved = resolve_receiver_depth(value, ctx, 0, MethodLookup::Unique)
        .or_else(|| resolve_receiver_depth(value, ctx, 0, MethodLookup::First))?;
    let display = match &resolved.ty {
        ResolvedType::InProject { decl, args } if !args.is_empty() => format!(
            "{}<{}>",
            decl.name,
            args.iter()
                .map(|arg| render_type_ref_in(arg, ctx))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ResolvedType::External { fqn, args } if !args.is_empty() => format!(
            "{}<{}>",
            fqn.rsplit('.').next().unwrap_or(fqn),
            args.iter()
                .map(|arg| render_type_ref_in(arg, ctx))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => type_display(&resolved.ty),
    };
    Some(display)
}

pub(crate) fn inferred_lambda_parameter_type_display(
    name: &str,
    byte: usize,
    ctx: &Ctx,
) -> Option<String> {
    let binding = lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        byte,
        name,
        ctx.table,
        ctx.current,
    )?;
    if !matches!(binding.kind, BindingKind::Param)
        || binding
            .type_node
            .is_some_and(|node| node_text(node, binding.source) != "var")
    {
        return None;
    }
    let inferred = crate::call::inferred_lambda_parameter_type(binding.decl_node, ctx)?;
    Some(render_type_ref_in(&inferred, ctx))
}

/// If the binding is a Java 21 pattern binding (`case Type name`, a
/// record-deconstruction component, or `instanceof Type name`), the display
/// string of its declared type; `None` for any other binding. A type that
/// doesn't resolve to a class falls back to its written text.
pub(crate) fn pattern_binding_type_display(name: &str, byte: usize, ctx: &Ctx) -> Option<String> {
    let binding = lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        byte,
        name,
        ctx.table,
        ctx.current,
    )?;
    if !matches!(
        binding.decl_node.kind(),
        "type_pattern" | "record_pattern_component" | "instanceof_expression"
    ) {
        return None;
    }
    let type_node = binding.type_node?;
    let display = resolve_type_node(type_node, binding.source, ctx)
        .map(|ty| type_display(&ty))
        .unwrap_or_else(|| node_text(type_node, binding.source).to_string());
    Some(display)
}

/// Resolve a `new Type(...)` expression to the receiver type it constructs,
/// same as any other declared-type resolution. Shared by receiver
/// resolution, hover, and constructor signature help, so all three agree on
/// what `new Foo` refers to.
pub(crate) fn resolve_object_creation_type<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let ty = call.child_by_field_name("type")?;
    resolve_type_node(ty, ctx.doc.source, ctx)
}

/// Resolve a declared-type node to a receiver type: in-project if the given
/// documents declare it (JLS 6.4/7.5 qualified resolution — see
/// [`crate::model::TypeTable::resolve_type_name_node`]), else an external
/// FQN via imports/symbol source.
pub(crate) fn resolve_type_node<'t>(
    type_node: Node<'t>,
    source: &'t str,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let vars = vars_in_scope(type_node, ctx);
    let resolve_named = |name: &str, dotted: bool| named_resolver(name, dotted, type_node, ctx);
    let ty = crate::typeref::lower_type_node(type_node, source, &vars, &resolve_named);
    ResolvedType::from_type_ref(&ty, ctx)
}

/// Map a declared type node's base name to a binary name:
/// [`crate::model::TypeTable::resolve_type_name_at`] first (lexical scope,
/// explicit import, package, qualified text, on-demand imports — the one
/// JLS-ordered rule), then the external/import-based fallback.
///
/// Keyed by `name`, not `type_node`'s own base, so the same resolver also
/// serves each type argument nested inside `type_node`.
fn named_resolver(name: &str, dotted: bool, type_node: Node, ctx: &Ctx<'_, '_>) -> Option<String> {
    let simple = if dotted {
        name.rsplit('.').next().unwrap_or(name)
    } else {
        name
    };
    if let Some(td) = ctx.table.resolve_type_name_at(
        type_node,
        simple,
        dotted.then(|| name.to_string()),
        ctx.current,
        ctx.imports,
    ) {
        return td.binary_name.clone();
    }
    if dotted {
        return ctx.symbols.class(name).is_some().then(|| name.to_string());
    }
    resolve_simple_to_fqn(simple, ctx)
}

/// Type variables in scope at `type_node`, innermost last. Owner strings
/// must match the ones srcclass.rs builds, or variable identity breaks.
fn vars_in_scope(type_node: Node, ctx: &Ctx<'_, '_>) -> Vec<(String, TypeVariableId)> {
    let mut levels: Vec<Node> = Vec::new();
    let mut anc = type_node.parent();
    while let Some(a) = anc {
        if TypeKind::from_kind(a.kind()).is_some()
            || matches!(a.kind(), "method_declaration" | "constructor_declaration")
        {
            levels.push(a);
        }
        anc = a.parent();
    }
    let mut vars: Vec<(String, TypeVariableId)> = Vec::new();
    for level in levels.into_iter().rev() {
        let resolve_named = |name: &str, dotted: bool| named_resolver(name, dotted, type_node, ctx);
        if TypeKind::from_kind(level.kind()).is_some() {
            let Some(td) = ctx.table.by_node(ctx.current, level.id()) else {
                continue;
            };
            let Some(binary) = &td.binary_name else {
                continue;
            };
            let (names, _) = crate::typeref::lower_type_parameters(
                level.child_by_field_name("type_parameters"),
                ctx.doc.source,
                binary,
                &[],
                &resolve_named,
            );
            vars.extend(names);
        } else {
            let Some(owner_td) = enclosing_typedecl(level, ctx.table, ctx.current) else {
                continue;
            };
            let Some(binary) = &owner_td.binary_name else {
                continue;
            };
            let Some(name_node) = level.child_by_field_name("name") else {
                continue;
            };
            let name = node_text(name_node, ctx.doc.source);
            let owner = match level.child_by_field_name("parameters") {
                Some(p) => format!("{binary}#{name}{}", node_text(p, ctx.doc.source)),
                None => format!("{binary}#{name}()"),
            };
            let (names, _) = crate::typeref::lower_type_parameters(
                level.child_by_field_name("type_parameters"),
                ctx.doc.source,
                &owner,
                &[],
                &resolve_named,
            );
            vars.extend(names);
        }
    }
    vars
}

/// Replace trailing dots with `$` until `exists` accepts the candidate, or
/// attempts run out. Shared core of [`import_path_to_fqn`] and
/// [`crate::model::TypeTable::resolve_type_name_node`]'s explicit-import
/// step.
pub(crate) fn import_path_to_binary(path: &str, exists: impl Fn(&str) -> bool) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut candidate = path.to_string();
    for _ in 0..8 {
        if exists(&candidate) {
            return Some(candidate);
        }
        let dot = candidate.rfind('.')?;
        candidate.replace_range(dot..dot + 1, "$");
    }
    None
}

/// An import path (`java.util.Map.Entry`) to the binary FQN the symbol
/// source recognizes (`java.util.Map$Entry`), by replacing trailing dots
/// with `$`. Shared by import completion and hover-on-import.
pub(crate) fn import_path_to_fqn(path: &str, ctx: &Ctx) -> Option<String> {
    import_path_to_binary(path, |b| ctx.symbols.class(b).is_some())
}

/// Resolve a simple type name to an external FQN — the first import/
/// package/wildcard/`java.lang` candidate the classpath symbol source
/// recognizes.
pub(crate) fn resolve_simple_to_fqn(simple: &str, ctx: &Ctx) -> Option<String> {
    ctx.imports
        .candidates(simple)
        .into_iter()
        .find(|fqn| ctx.symbols.class(fqn).is_some())
}

/// A bare simple name -> the open-document declaration it names, used when
/// no real type-reference node drives full lexical resolution. Prefers the
/// current document, else the candidate this document's imports/package/
/// wildcards resolve `simple` to; not a substitute for
/// [`crate::model::TypeTable::resolve_type_name_node`], which also honors
/// lexical scoping.
pub(crate) fn resolve_simple_in_project<'t>(
    simple: &str,
    ctx: &Ctx<'_, 't>,
) -> Option<TypeDecl<'t>> {
    if let Some(td) = ctx
        .table
        .candidates(simple)
        .find(|td| td.doc == ctx.current)
    {
        return Some(td.clone());
    }
    let candidate_fqns = ctx.imports.candidates(simple);
    ctx.table
        .candidates(simple)
        .find(|td| {
            td.binary_name
                .as_deref()
                .is_some_and(|b| candidate_fqns.iter().any(|c| c == b))
        })
        .cloned()
}

/// The full dotted name of a fully-qualified type node (`java.util.List`),
/// or `None` for a simple type. `pub(crate)`: used by `implementation.rs`
/// to match a qualified `extends`/`implements` entry against the target's
/// real FQN, bypassing imports.
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

/// Resolve a dotted path (`a.b.c`) segment by segment from a resolvable
/// head (binding → in-project type → imported/`java.lang` type), stepping
/// through fields, nested types, and enum constants. Failing that, try the
/// longest prefix as a fully-qualified external type and walk any
/// remaining segments from there.
fn resolve_scoped_path<'t>(
    node: Node<'t>,
    ctx: &Ctx<'_, 't>,
    method_lookup: MethodLookup,
) -> Option<Resolved<'t>> {
    let names = flatten_scoped(node, ctx.doc.source);
    if let Some(resolved) = names.split_first().and_then(|(first, rest)| {
        let head = resolve_name_to_type(first, node.start_byte(), ctx)?;
        walk_segments(head, rest, ctx, method_lookup)
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
            return walk_segments(head, &names[k..], ctx, method_lookup);
        }
    }
    None
}

fn walk_segments<'t>(
    head: Resolved<'t>,
    segments: &[&str],
    ctx: &Ctx<'_, 't>,
    method_lookup: MethodLookup,
) -> Option<Resolved<'t>> {
    let mut current = head;
    for segment in segments {
        current = resolve_member_segment(&current, segment, ctx, method_lookup)?;
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
    method_lookup: MethodLookup,
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
            ctx.table.by_node(m.doc, m.node.id()).cloned()
        }
        _ => None,
    }) {
        return Some(Resolved {
            ty: ResolvedType::InProject {
                decl: td,
                args: Vec::new(),
            },
            static_only: true,
        });
    }
    let member = members
        .into_iter()
        .find(|m| m.name() == name && MemberNamespace::Field.matches(m))?;
    member_result_type(&member, current, ctx, method_lookup)
}

/// Resolve a member's result as the next receiver in a chain.
/// In-project declarations use their own document imports; unresolved results stop quietly.
fn member_result_type<'t>(
    member: &HierMember<'t>,
    recv: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    method_lookup: MethodLookup,
) -> Option<Resolved<'t>> {
    match member {
        HierMember::InProject(m) => {
            let (ty_node, kind) = match m.kind {
                MemberKind::Method => (
                    m.node.child_by_field_name("type")?,
                    ExternalMemberKind::Method,
                ),
                MemberKind::Field => (field_type_node(m.node)?, ExternalMemberKind::Field),
                // An enum constant's type is its declaring enum.
                MemberKind::EnumConstant => {
                    let td = ctx
                        .table
                        .by_node(m.doc, enclosing_type_node(m.node)?.id())?
                        .clone();
                    return Some(instance(ResolvedType::InProject {
                        decl: td,
                        args: Vec::new(),
                    }));
                }
                MemberKind::NestedType(_) => return None, // handled as a segment, not a value
            };
            // A generic member's declared type (`T item`, `T first()`) only
            // means something through the receiver's type arguments; the
            // source node alone names a variable no chain can continue from.
            if let Some(declaring) = enclosing_type_node(m.node)
                .and_then(|type_node| ctx.table.by_node(m.doc, type_node.id()))
            {
                if let Some(ty) = crate::call::member_type_through(
                    &recv.ty.type_ref(),
                    &declaring.type_id,
                    m.name,
                    kind,
                    ctx,
                ) {
                    return receiver_type_ref(&ty, ctx, 0).map(instance);
                }
            }
            let dctx = ctx.for_document(m.doc)?;
            resolve_type_node(ty_node, m.source, &dctx).map(instance)
        }
        HierMember::External(m) => external_result_type(m, recv, ctx, method_lookup).map(instance),
    }
}

/// An external member's result type. Prefers the generic `ret_display`
/// template substituted with the receiver's use-site type arguments
/// (`List<String>.get(int)` chains as `String`); first-name lookup may fall
/// back to the erased `ret_fqn`, but unique lookup must not infer a value
/// type from erasure.
fn external_result_type<'t>(
    m: &ExternalMember,
    recv: &Resolved<'_>,
    ctx: &Ctx<'_, 't>,
    method_lookup: MethodLookup,
) -> Option<ResolvedType<'t>> {
    if let Some(meta) = &m.metadata {
        if let Some(resolved) = receiver_type_ref(&meta.result, ctx, 0) {
            return Some(resolved);
        }
    }
    if let Some(display) = &m.ret_display {
        let (args, type_params) = match &recv.ty {
            ResolvedType::External { fqn, args } => {
                let params = ctx
                    .symbols
                    .class(fqn)
                    .map(|c| c.type_params)
                    .unwrap_or_default();
                let args: Vec<String> = args
                    .iter()
                    .map(|arg| render_type_ref_in(arg, ctx))
                    .collect();
                (args, params)
            }
            _ => (Vec::new(), Vec::new()),
        };
        if matches!(method_lookup, MethodLookup::Unique)
            && template_has_missing_arg(display, args.len())
        {
            return None;
        }
        let substituted = substitute_template(display, &args, &type_params);
        // A type variable the receiver didn't pin down isn't a concrete
        // value type — semantic resolution stays unknown rather than
        // inferring from its erased bound. Historical receiver resolution
        // may still use the declared result to keep completion/hover chains
        // alive.
        if display_looks_unresolved(&substituted) && matches!(method_lookup, MethodLookup::Unique) {
            return None;
        }
        if let Some(resolved) = resolve_display_type(&substituted, m.ret_fqn.as_deref(), ctx) {
            return Some(resolved);
        }
        if matches!(method_lookup, MethodLookup::Unique) {
            return None;
        }
    }
    let fqn = m.ret_fqn.clone()?;
    Some(ResolvedType::External {
        fqn,
        args: Vec::new(),
    })
}

fn receiver_type_ref<'t>(
    ty: &TypeRef,
    ctx: &Ctx<'_, 't>,
    depth: usize,
) -> Option<ResolvedType<'t>> {
    if depth > MAX_RESOLVE_DEPTH {
        return None;
    }
    match ty {
        TypeRef::Variable(variable) => {
            let bounds = variable_bounds(variable, ctx)?;
            let bound = bounds
                .into_iter()
                .find(|bound| !bound.contains_unknown())
                .unwrap_or_else(|| TypeRef::named("java.lang.Object"));
            receiver_type_ref(&bound, ctx, depth + 1)
        }
        TypeRef::Wildcard {
            upper: Some(upper), ..
        } => receiver_type_ref(upper, ctx, depth + 1),
        TypeRef::Wildcard { .. } => {
            receiver_type_ref(&TypeRef::named("java.lang.Object"), ctx, depth + 1)
        }
        TypeRef::Unknown => None,
        other => ResolvedType::from_type_ref(other, ctx),
    }
}

/// Whether a rendered display string still looks unresolved — a leftover
/// `{i}` placeholder, or a bare single uppercase letter (`T`/`E`/`K`) rather
/// than a concrete type. Used only by [`external_result_type`]'s display
/// chain; semantic proofs use [`jvl_types::TypeRef::contains_unknown`]
/// instead.
fn display_looks_unresolved(s: &str) -> bool {
    s.contains('?')
        || s.contains('{')
        || s.contains('}')
        || s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
            .any(|part| part.len() == 1 && part.as_bytes()[0].is_ascii_uppercase())
}

fn template_has_missing_arg(template: &str, arg_count: usize) -> bool {
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('}') else {
            return true;
        };
        if after_open[..close]
            .parse::<usize>()
            .map_or(true, |index| index >= arg_count)
        {
            return true;
        }
        rest = &after_open[close + 1..];
    }
    false
}

/// A display-string type argument -> best-effort `TypeRef` for this
/// display-oriented chain only: `?` as a wildcard, a primitive by name, an
/// array suffix peeled recursively, otherwise an opaque `Named` wrapping the
/// raw text. That wrapping is never a real binary name and must never be
/// looked up via `class_facts` or fed into the subtype engine.
fn opaque_type_ref(s: &str) -> TypeRef {
    let s = s.trim();
    if s == "?" {
        return TypeRef::Wildcard {
            upper: None,
            lower: None,
        };
    }
    if let Some(element) = s.strip_suffix("[]") {
        return TypeRef::Array(Box::new(opaque_type_ref(element)));
    }
    if let Some(p) = PrimitiveType::from_name(s) {
        return TypeRef::Primitive(p);
    }
    TypeRef::named(s)
}

/// Resolve a rendered display type (`Stream<String>`, `String`, `MyType`)
/// back to a receiver type: in-project table first, then the erased FQN
/// when its simple name matches the display's base, then the file's
/// imports/`java.lang` for the type-variable case.
fn resolve_display_type<'t>(
    display: &str,
    erased_fqn: Option<&str>,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let display = display.trim();
    if let Some(element) = display.strip_suffix("[]") {
        return Some(ResolvedType::Array {
            element: opaque_type_ref(element.trim_end()),
        });
    }
    if display == "void" {
        return Some(ResolvedType::Void);
    }
    if let Some(primitive) = PrimitiveType::from_name(display) {
        return Some(ResolvedType::Primitive(primitive));
    }
    let (base, raw_args) = parse_display_type(display)?;
    let args: Vec<TypeRef> = raw_args.iter().map(|a| opaque_type_ref(a)).collect();
    if base.contains('.') && ctx.symbols.class(base).is_some() {
        return Some(ResolvedType::External {
            fqn: base.to_string(),
            args,
        });
    }
    let simple = base.rsplit('.').next().unwrap_or(base);
    if let Some(td) = resolve_simple_in_project(simple, ctx) {
        return Some(ResolvedType::InProject { decl: td, args });
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

/// Split a rendered reference type into its base name and top-level type
/// arguments: `Map<String, List<Integer>>` → (`Map`, [`String`,
/// `List<Integer>`]). Arrays are handled before this parser.
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

/// The two members every Java array has beyond `java.lang.Object`'s —
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
            ret_display: Some("int".to_string()),
            metadata: None,
        },
        ExternalMember {
            name: "clone".to_string(),
            kind: ExternalMemberKind::Method,
            signature: format!("{display} clone()"),
            template: None,
            is_static: false,
            ret_fqn: None,
            ret_display: Some(display.to_string()),
            metadata: None,
        },
    ]
}

/// The element type of an array receiver (`arr[i].`), resolved from the
/// array's structured element type. Multi-dimensional arrays peel one level
/// (the element of `String[][]` is itself `Array(String)`).
fn array_element_type<'t>(element: &TypeRef, ctx: &Ctx<'_, 't>) -> Option<Resolved<'t>> {
    ResolvedType::from_type_ref(element, ctx).map(instance)
}

/// The element type produced by iterating `ty`: an array's element, or a
/// generic collection's first type argument (`List<Foo>`/`Set<Foo>`/
/// `Iterable<Foo>` → `Foo`). Returns `None`, never the iterable's own type,
/// when it can't be determined — so an unknown-element enhanced-for `var`
/// resolves to nothing rather than mis-resolving every loop-var access.
fn iterable_element_type<'t>(ty: &ResolvedType<'t>, ctx: &Ctx<'_, 't>) -> Option<ResolvedType<'t>> {
    match ty {
        ResolvedType::Array { element } => array_element_type(element, ctx).map(|r| r.ty),
        ResolvedType::External { args, .. } => {
            let first = args.first()?;
            if matches!(first, TypeRef::Wildcard { .. } | TypeRef::Unknown) {
                return None;
            }
            // Try the structured `TypeRef` first; fall back to the
            // display-string pipeline for a use-site argument that only
            // ever existed as rendered text (see `opaque_type_ref`).
            ResolvedType::from_type_ref(first, ctx)
                .or_else(|| resolve_display_type(&render_type_ref(first), None, ctx))
        }
        ResolvedType::InProject { .. } => {
            let elem = iterable_argument(&ty.type_ref(), ctx, 0, &mut HashSet::new())?;
            if !matches!(elem, TypeRef::Named { .. } | TypeRef::Array(_)) || elem.contains_unknown()
            {
                return None;
            }
            ResolvedType::from_type_ref(&elem, ctx)
        }
        ResolvedType::Primitive(_) | ResolvedType::Void | ResolvedType::Null => None,
    }
}

/// The `T` in the `java.lang.Iterable<T>` (or `java.util.Collection<T>`)
/// supertype reachable from `ty`, with class type variables substituted
/// through each `extends`/`implements` hop. `None` when the hierarchy is
/// incomplete, raw, or never reaches `Iterable`.
fn iterable_argument(
    ty: &TypeRef,
    ctx: &Ctx<'_, '_>,
    depth: usize,
    seen: &mut HashSet<TypeRef>,
) -> Option<TypeRef> {
    let TypeRef::Named { id, args } = ty else {
        return None;
    };
    if depth > MAX_SUBTYPE_DEPTH || !seen.insert(ty.clone()) {
        return None;
    }
    if matches!(
        id.as_named(),
        Some("java.lang.Iterable") | Some("java.util.Collection")
    ) {
        return args.first().cloned();
    }
    let facts = class_facts(id, ctx)?;
    if !facts.meta.hierarchy_complete {
        return None;
    }
    if args.is_empty() && !facts.meta.type_parameters.is_empty() {
        return None; // raw use: element type is erased
    }
    let env: Vec<(TypeVariableId, TypeRef)> = facts
        .meta
        .type_parameters
        .iter()
        .map(|p| p.id.clone())
        .zip(args.iter().cloned())
        .collect();
    facts
        .meta
        .supertypes
        .iter()
        .find_map(|sup| iterable_argument(&sup.substitute(&env), ctx, depth + 1, seen))
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

/// Which Java member namespace a reference occupies (JLS §6.5): fields and
/// methods live in separate namespaces, so `recv.foo()` can only mean a
/// method and `recv.foo` can only mean a field. [`find_member_hier`] is
/// namespace-blind and fine for display-oriented lookups; reference
/// confirmation must use [`find_member_hier_of_kind`] instead to avoid
/// conflating a field with a same-named method.
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
/// type or any supertype. A kind-aware variant of [`find_member_hier`], kept
/// separate since hover/completion depend on that one's name-only semantics.
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
        ResolvedType::InProject { decl: td, .. } => {
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
            // Lombok-generated accessors join the class's declared members.
            // They're synthesized (no AST node), so they travel as External
            // members and resolve through `ret_display`/`ret_fqn` like a
            // bytecode member.
            if crate::lombok::file_uses_lombok(td.node, td.source) {
                for sm in crate::lombok::synthesize(td.node, td.source) {
                    if static_only && !sm.is_static {
                        continue;
                    }
                    let m = sm.into_external();
                    if acc.seen.insert(m.signature.clone()) {
                        acc.out.push(HierMember::External(m));
                    }
                }
            }
            if let Some(dctx) = ctx.for_document(td.doc) {
                for sup_node in &td.super_nodes {
                    if let Some(resolved) = resolve_type_node(*sup_node, td.source, &dctx) {
                        walk_members(&resolved, ctx, static_only, acc, depth + 1);
                    }
                }
            }
            // An in-project enum implicitly extends `java.lang.Enum` — the
            // source of `name()`, `ordinal()`, `compareTo()`, etc. (its own
            // `supers` list holds only explicit interfaces).
            if td.kind == crate::model::TypeKind::Enum {
                walk_members(
                    &ResolvedType::External {
                        fqn: "java.lang.Enum".to_string(),
                        args: Vec::new(),
                    },
                    ctx,
                    static_only,
                    acc,
                    depth + 1,
                );
            }
        }
        // Arrays expose exactly `length`, `clone()`, and Object's members.
        ResolvedType::Array { element } => {
            let display = format!("{}[]", render_type_ref(element));
            for m in array_members(&display) {
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
            let arg_strings: Vec<String> = args
                .iter()
                .map(|arg| render_type_ref_in(arg, ctx))
                .collect();
            let env: Vec<(TypeVariableId, TypeRef)> = class
                .metadata
                .as_ref()
                .filter(|meta| meta.type_parameters.len() == args.len())
                .map(|meta| {
                    meta.type_parameters
                        .iter()
                        .map(|param| param.id.clone())
                        .zip(args.iter().cloned())
                        .collect()
                })
                .unwrap_or_default();
            let structured_supers = class.metadata.as_ref().and_then(|meta| {
                meta.hierarchy_complete.then(|| {
                    meta.supertypes
                        .iter()
                        .map(|supertype| supertype.substitute(&env))
                        .collect::<Vec<_>>()
                })
            });
            for mut member in class.members {
                if member.kind == ExternalMemberKind::Constructor
                    || (static_only && !member.is_static)
                {
                    continue;
                }
                let erased_signature = member.signature.clone();
                if !acc.seen.insert(erased_signature) {
                    continue;
                }
                let signature = display_signature(&member, &arg_strings, &class.type_params);
                if let Some(meta) = &mut member.metadata {
                    meta.parameters = meta.parameters.as_ref().map(|parameters| {
                        parameters
                            .iter()
                            .map(|parameter| parameter.substitute(&env))
                            .collect()
                    });
                    meta.result = meta.result.substitute(&env);
                    for parameter in &mut meta.type_parameters {
                        parameter.bounds = parameter
                            .bounds
                            .iter()
                            .map(|bound| bound.substitute(&env))
                            .collect();
                    }
                }
                member.signature = signature;
                acc.out.push(HierMember::External(member));
            }
            if let Some(supertypes) = structured_supers {
                for supertype in supertypes {
                    if let Some(resolved) = ResolvedType::from_type_ref(&supertype, ctx) {
                        walk_members(&resolved, ctx, static_only, acc, depth + 1);
                    }
                }
                return;
            }
            let super_type_args = ctx.symbols.super_type_args(fqn);
            let super_type_args = if super_type_args.len() == class.supers.len() {
                super_type_args
            } else {
                vec![Vec::new(); class.supers.len()]
            };
            for (supertype, super_args) in class.supers.into_iter().zip(super_type_args) {
                let mapped_args: Vec<TypeRef> = super_args
                    .iter()
                    .map(|raw| {
                        opaque_type_ref(&substitute_template(raw, &arg_strings, &class.type_params))
                    })
                    .collect();
                walk_members(
                    &ResolvedType::External {
                        fqn: supertype,
                        args: mapped_args,
                    },
                    ctx,
                    static_only,
                    acc,
                    depth + 1,
                );
            }
        }
        ResolvedType::Primitive(_) | ResolvedType::Void | ResolvedType::Null => {}
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

/// Collect member names and whether the full hierarchy resolved.
/// Diagnostics stay silent for incomplete hierarchies that might contain the member.
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
    if matches!(
        &resolved.ty,
        ResolvedType::InProject { .. } | ResolvedType::External { .. } | ResolvedType::Array { .. }
    ) {
        match ctx.symbols.class("java.lang.Object") {
            Some(object) => names.extend(object.members.into_iter().map(|m| m.name)),
            None => complete = false,
        }
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
        ResolvedType::InProject { decl: td, .. } => {
            if !visited_node.insert(td.node.id()) {
                return;
            }
            for m in td.own_members() {
                names.insert(m.name.to_string());
            }
            match ctx.for_document(td.doc) {
                Some(dctx) => {
                    for sup_node in &td.super_nodes {
                        match resolve_type_node(*sup_node, td.source, &dctx) {
                            Some(resolved) => diag_walk(
                                &resolved,
                                ctx,
                                names,
                                complete,
                                visited_node,
                                visited_fqn,
                                depth + 1,
                            ),
                            None => *complete = false, // unknown supertype — give up flagging
                        }
                    }
                }
                None => {
                    if !td.super_nodes.is_empty() {
                        *complete = false;
                    }
                }
            }
            // An in-project enum implicitly extends `java.lang.Enum`
            // (`name()`, `ordinal()`, `compareTo()`, …) — see the matching
            // walk in `walk_members`.
            if td.kind == crate::model::TypeKind::Enum {
                diag_walk(
                    &ResolvedType::External {
                        fqn: "java.lang.Enum".to_string(),
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
        // An array's complete member set is `length` + `clone` (plus
        // Object's, appended globally by `member_names`).
        ResolvedType::Array { element } => {
            for m in array_members(&format!("{}[]", render_type_ref(element))) {
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
        ResolvedType::Primitive(_) | ResolvedType::Void | ResolvedType::Null => {}
    }
}

/// The signature to show for an external member: its generic template
/// substituted with use-site type arguments, or the erased signature
/// otherwise. `pub(crate)`: hover and signature help's constructor lookups
/// bypass [`collect_members`] but still want this substitution.
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
            // Java 21 pattern bindings visible in a switch rule's guard and
            // body (`case Type name`, record deconstruction components).
            "switch_rule" => push_pattern_bindings(n, source, doc, &mut out),
            // `x instanceof Type name` binds `name` in the enclosing condition
            // and its guarded branch — scan the condition subtree from each
            // condition-bearing statement/expression on the way up.
            "if_statement" | "while_statement" | "do_statement" | "ternary_expression" => {
                if let Some(cond) = n.child_by_field_name("condition") {
                    push_instanceof_bindings(cond, source, doc, &mut out);
                }
            }
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "lambda_expression" => push_params(n, source, doc, &mut out),
            k if is_type_decl(k) && enclosing_type.is_none() => {
                enclosing_type = Some(n);
            }
            _ => {}
        }
        node = n.parent();
    }
    if include_fields {
        if let Some(type_node) = enclosing_type {
            if let Some(td) = table.by_node(doc, type_node.id()) {
                push_fields(td, table, &mut out);
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

/// The (name, declared-type) parts of a `type_pattern` (`String s`) or a
/// `record_pattern_component` (`int x`): the sole `identifier` child is the
/// binding name; the first non-`identifier` named child is the type (a
/// `type_identifier`/`generic_type`/…, or a primitive `integral_type`).
/// `None` for a component that nests another pattern instead of binding.
fn pattern_binding_parts<'t>(node: Node<'t>) -> Option<(Node<'t>, Option<Node<'t>>)> {
    let name = named_children(node)
        .into_iter()
        .find(|c| c.kind() == "identifier")?;
    let type_node = named_children(node)
        .into_iter()
        .find(|c| c.kind() != "identifier");
    Some((name, type_node))
}

/// Collect the pattern bindings a `switch_rule` introduces — walking only its
/// `switch_label` subtree (where the patterns are declared; they're in scope
/// for the whole rule), so a nested switch in the body doesn't leak its own.
fn push_pattern_bindings<'t>(
    rule: Node<'t>,
    source: &'t str,
    doc: usize,
    out: &mut Vec<Binding<'t>>,
) {
    let mut stack: Vec<Node<'t>> = named_children(rule)
        .into_iter()
        .filter(|c| c.kind() == "switch_label")
        .collect();
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "type_pattern" | "record_pattern_component") {
            if let Some((name, type_node)) = pattern_binding_parts(n) {
                out.push(Binding {
                    name: node_text(name, source),
                    kind: BindingKind::Local,
                    type_node,
                    decl_node: n,
                    source,
                    doc,
                });
            }
        }
        for c in named_children(n) {
            stack.push(c);
        }
    }
}

/// Collect every binding introduced within an `instanceof` condition
/// subtree (`f instanceof String s && s.length() > 0` nests the pattern in a
/// `binary_expression`, so the whole subtree is walked): a plain `Type name`
/// binding on the `instanceof_expression` itself, plus every
/// `record_pattern_component` reachable through a `pattern: record_pattern`
/// — including components nested arbitrarily deep inside further
/// `record_pattern`s (`Line(Point(int x, int y), Point b)` binds `x`, `y`,
/// and `b`, all in scope for the guarded branch).
fn push_instanceof_bindings<'t>(
    cond: Node<'t>,
    source: &'t str,
    doc: usize,
    out: &mut Vec<Binding<'t>>,
) {
    let mut stack = vec![cond];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "instanceof_expression" => {
                if let Some(name) = n.child_by_field_name("name") {
                    out.push(Binding {
                        name: node_text(name, source),
                        kind: BindingKind::Local,
                        type_node: n.child_by_field_name("right"),
                        decl_node: n,
                        source,
                        doc,
                    });
                }
            }
            "record_pattern_component" => {
                if let Some((name, type_node)) = pattern_binding_parts(n) {
                    out.push(Binding {
                        name: node_text(name, source),
                        kind: BindingKind::Local,
                        type_node,
                        decl_node: n,
                        source,
                        doc,
                    });
                }
            }
            _ => {}
        }
        for c in named_children(n) {
            stack.push(c);
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

    /// A usage of a local variable resolves to a `DeclSite` in the same
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

    /// A type used in doc B but declared in doc A resolves to a
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
        let td = table.get_named("Foo").expect("Foo indexed");
        let site = td.decl_site().expect("decl site");
        assert_eq!(site.doc, 1);
        let expected = doc_a.find("Foo").unwrap();
        assert_eq!(site.name_range, expected..expected + "Foo".len());
    }

    /// Resolving a member inherited from a supertype declared in another
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
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols: &NoSymbols,
            docs: &docs,
            facts: &facts,
        };
        let td_b = table.get_named("B").expect("B indexed").clone();
        let resolved = Resolved {
            ty: ResolvedType::InProject {
                decl: td_b,
                args: Vec::new(),
            },
            static_only: false,
        };
        let member = find_member_hier(&resolved, &ctx, "methodFromA").expect("member found");
        let site = member.decl_site().expect("decl site");
        assert_eq!(site.doc, 1);
        let expected = doc_a.find("methodFromA").unwrap();
        assert_eq!(site.name_range, expected..expected + "methodFromA".len());
    }

    /// A member resolved from an external `SymbolSource` (JDK/jar) has no
    /// `DeclSite` — no open document backs it.
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
                        metadata: None,
                    }],
                    metadata: None,
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
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols: &StubSymbols,
            docs: &docs,
            facts: &facts,
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

/// Mapping a parameterized supertype's type arguments through
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
            metadata: None,
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
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
            docs: &docs,
            facts: &facts,
        };
        let resolved = Resolved {
            ty: ResolvedType::External {
                fqn: fqn.to_string(),
                args: args.iter().map(|a| TypeRef::named(a)).collect(),
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
                    metadata: None,
                }),
                "test.AbstractList" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["E".to_string()],
                    members: vec![method("get", "Object get(int)", "{0} get(int)")],
                    metadata: None,
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
                    metadata: None,
                }),
                "test.B" => Some(ExternalClass {
                    supers: vec!["test.A".to_string()],
                    type_params: vec!["T".to_string()],
                    members: Vec::new(),
                    metadata: None,
                }),
                "test.A" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("id", "Object id(Object)", "{0} id({0})")],
                    metadata: None,
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
                    metadata: None,
                }),
                "test.Base" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["K".to_string(), "V".to_string()],
                    members: vec![method("first", "Object first()", "{0} first()")],
                    metadata: None,
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
                    metadata: None,
                }),
                "test.Box" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("unwrap", "Object unwrap()", "{0} unwrap()")],
                    metadata: None,
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
                    metadata: None,
                }),
                "test.Generic" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("get", "Object get()", "{0} get()")],
                    metadata: None,
                }),
                _ => None,
            }
        }
        // No override: defaults to `Vec::new()` for every fqn — "nothing
        // tracked", exactly like a raw (unparameterized) supertype use.
    }

    #[test]
    fn raw_supertype_falls_back_to_erased_rendering_without_panicking() {
        // No type args flow through an untracked supertype, so the
        // inherited member keeps its erased signature — no panic, no
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
                    metadata: None,
                }),
                "test.Base" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["T".to_string()],
                    members: vec![method("head", "Object head()", "{0} head()")],
                    metadata: None,
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

#[cfg(test)]
mod value_type_tests {
    use super::*;
    use crate::external::{
        ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols, SymbolSource,
    };
    use crate::{new_parser, parse};
    use jvl_types::TypeParameter;

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn returned_expression(tree: &Tree) -> Node<'_> {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "return_statement" {
                return node.named_child(0).expect("return value");
            }
            stack.extend(named_children(node));
        }
        panic!("return statement");
    }

    fn expression_display(src: &str, symbols: &dyn SymbolSource) -> Option<String> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree, src);
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
            docs: &docs,
            facts: &facts,
        };
        resolve_expression_type(returned_expression(&tree), &ctx).map(|ty| type_display(&ty))
    }

    fn receiver_display(src: &str, symbols: &dyn SymbolSource) -> Option<String> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree, src);
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
            docs: &docs,
            facts: &facts,
        };
        resolve_receiver_type(returned_expression(&tree), &ctx)
            .map(|resolved| type_display(&resolved.ty))
    }

    fn primitive(ty: PrimitiveType) -> ResolvedType<'static> {
        ResolvedType::Primitive(ty)
    }

    /// `"?"` becomes a bare wildcard; everything else an opaque `Named` (see
    /// [`opaque_type_ref`]) — good enough identity for these hand-authored
    /// fixture args, which only ever get compared for equality/containment.
    fn external(fqn: &str, args: &[&str]) -> ResolvedType<'static> {
        ResolvedType::External {
            fqn: fqn.to_string(),
            args: args.iter().map(|arg| opaque_type_ref(arg)).collect(),
        }
    }

    fn array(display: &str) -> ResolvedType<'static> {
        let element = display
            .strip_suffix("[]")
            .expect("array display ends with []");
        ResolvedType::Array {
            element: opaque_type_ref(element),
        }
    }

    fn assign(
        actual: ResolvedType<'static>,
        expected: ResolvedType<'static>,
        symbols: &dyn SymbolSource,
    ) -> Option<bool> {
        let src = "class Current {}";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree, src);
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
            docs: &docs,
            facts: &facts,
        };
        is_assignable(&actual, &expected, &ctx)
    }

    fn in_project_assign(
        sources: &[&str],
        current: usize,
        actual: &str,
        expected: &str,
    ) -> Option<bool> {
        let trees: Vec<Tree> = sources.iter().map(|source| tree(source)).collect();
        let docs: Vec<OpenDoc<'_>> = sources
            .iter()
            .zip(&trees)
            .map(|(source, tree)| OpenDoc { source, tree })
            .collect();
        let table = TypeTable::build(&docs, current);
        let imports = Imports::parse(docs[current].tree, docs[current].source);
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[current],
            current,
            table: &table,
            imports: &imports,
            symbols: &NoSymbols,
            docs: &docs,
            facts: &facts,
        };
        let actual = ResolvedType::InProject {
            decl: table.get_named(actual).expect("actual type").clone(),
            args: Vec::new(),
        };
        let expected = ResolvedType::InProject {
            decl: table.get_named(expected).expect("expected type").clone(),
            args: Vec::new(),
        };
        is_assignable(&actual, &expected, &ctx)
    }

    fn result_member(
        name: &str,
        signature: &str,
        ret_fqn: Option<&str>,
        ret_display: &str,
    ) -> ExternalMember {
        ExternalMember {
            name: name.to_string(),
            kind: ExternalMemberKind::Method,
            signature: signature.to_string(),
            template: None,
            is_static: false,
            ret_fqn: ret_fqn.map(str::to_string),
            ret_display: Some(ret_display.to_string()),
            metadata: None,
        }
    }

    struct ValueSymbols;

    impl SymbolSource for ValueSymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            // Structured metadata is attached only to members whose result
            // type the Unique-lookup engine must resolve (`size`/`clear`/
            // `values`). `pick`/`unknown`/`static_unknown`/`unknown_result`
            // stay `metadata: None` on purpose — tests below assert Unique
            // lookup stays unknown for those overload/generic cases.
            let structured =
                |name: &str, sig: &str, display: &str, result: TypeRef| ExternalMember {
                    metadata: Some(jvl_types::MemberMetadata {
                        declaring_class: TypeId::named("test.Values"),
                        access: Access::Public,
                        is_static: false,
                        is_abstract: false,
                        parameters: Some(Vec::new()),
                        result,
                        type_parameters: Vec::new(),
                        is_varargs: false,
                    }),
                    ..result_member(name, sig, None, display)
                };
            (fqn == "test.Values").then(|| ExternalClass {
                supers: Vec::new(),
                type_params: vec!["T".to_string()],
                metadata: Some(ClassMetadata {
                    id: TypeId::named("test.Values"),
                    kind: ClassKind::Class,
                    access: Access::Public,
                    is_abstract: false,
                    is_static: true,
                    enclosing_class: None,
                    type_parameters: Vec::new(),
                    supertypes: Vec::new(),
                    hierarchy_complete: true,
                    constructors_complete: true,
                }),
                members: vec![
                    structured(
                        "size",
                        "int size()",
                        "int",
                        TypeRef::Primitive(PrimitiveType::Int),
                    ),
                    structured("clear", "void clear()", "void", TypeRef::Void),
                    structured(
                        "values",
                        "Object[] values()",
                        "Object[]",
                        TypeRef::Array(Box::new(TypeRef::named("java.lang.Object"))),
                    ),
                    result_member("pick", "String pick()", Some("java.lang.String"), "String"),
                    result_member(
                        "pick",
                        "String pick(int)",
                        Some("java.lang.String"),
                        "String",
                    ),
                    result_member(
                        "unknown",
                        "Object unknown()",
                        Some("java.lang.Object"),
                        "{0}",
                    ),
                    ExternalMember {
                        is_static: true,
                        ..result_member(
                            "static_unknown",
                            "static <T> Object static_unknown()",
                            Some("java.lang.Object"),
                            "{0}",
                        )
                    },
                    result_member(
                        "unknown_result",
                        "<TResult> TResult unknown_result()",
                        Some("java.lang.Object"),
                        "TResult",
                    ),
                ],
            })
        }
    }

    fn value_call(method: &str) -> String {
        format!("import test.Values; class C {{ Object m(Values v) {{ return v.{method}(); }} }}")
    }

    fn static_value_call(method: &str) -> String {
        format!("import test.Values; class C {{ Object m() {{ return Values.{method}(); }} }}")
    }

    #[test]
    fn expression_resolution_preserves_all_literal_types_without_classpath_symbols() {
        let cases = [
            ("true", "boolean"),
            ("1", "int"),
            ("1L", "long"),
            ("1.0f", "float"),
            ("1.0", "double"),
            ("'x'", "char"),
            ("\"text\"", "String"),
            ("\"\"\"\ntext\n\"\"\"", "String"),
            ("null", "null"),
        ];
        for (literal, expected) in cases {
            let src = format!("class C {{ Object m() {{ return {literal}; }} }}");
            assert_eq!(
                expression_display(&src, &NoSymbols).as_deref(),
                Some(expected),
                "literal {literal}"
            );
        }
    }

    #[test]
    fn expression_resolution_preserves_all_declared_primitive_types() {
        for primitive in [
            "boolean", "byte", "short", "int", "long", "char", "float", "double",
        ] {
            let src = format!("class C {{ {primitive} m({primitive} value) {{ return value; }} }}");
            assert_eq!(
                expression_display(&src, &NoSymbols).as_deref(),
                Some(primitive),
                "declared {primitive}"
            );
        }
    }

    #[test]
    fn external_member_results_preserve_primitive_void_and_array_types() {
        for (method, expected) in [("size", "int"), ("clear", "void"), ("values", "Object[]")] {
            assert_eq!(
                expression_display(&value_call(method), &ValueSymbols).as_deref(),
                Some(expected),
                "member {method}"
            );
        }
    }

    #[test]
    fn array_access_preserves_primitive_element_type() {
        let src = "class C { int m(int[] values) { return values[0]; } }";
        assert_eq!(expression_display(src, &NoSymbols).as_deref(), Some("int"));
    }

    #[test]
    fn ambiguous_overload_is_unknown_only_for_value_resolution() {
        let src = value_call("pick");
        assert_eq!(expression_display(&src, &ValueSymbols), None);
        assert_eq!(
            receiver_display(&src, &ValueSymbols).as_deref(),
            Some("String"),
            "completion/hover receiver lookup must retain first-name behavior"
        );
    }

    #[test]
    fn unresolved_generic_member_result_stays_unknown() {
        assert_eq!(
            expression_display(&value_call("unknown"), &ValueSymbols),
            None
        );
    }

    #[test]
    fn first_lookup_retains_erased_raw_and_static_generic_results() {
        for (src, receiver_kind) in [
            (value_call("unknown"), "raw"),
            (static_value_call("static_unknown"), "static"),
        ] {
            assert_eq!(
                receiver_display(&src, &ValueSymbols).as_deref(),
                Some("Object"),
                "{receiver_kind} generic receiver must retain erased First lookup"
            );
            assert_eq!(
                expression_display(&src, &ValueSymbols),
                None,
                "{receiver_kind} generic result must remain unknown for Unique lookup"
            );
        }
    }

    #[test]
    fn unresolved_multi_character_method_type_variable_stays_unknown() {
        assert_eq!(
            expression_display(&value_call("unknown_result"), &ValueSymbols),
            None
        );
    }

    #[test]
    fn unresolved_declared_type_variable_stays_unknown() {
        let src = "class C<T> { T m(T value) { return value; } }";
        assert_eq!(expression_display(src, &NoSymbols), None);
    }

    #[test]
    fn primitive_identity_and_widening_are_assignable() {
        let cases = [
            (PrimitiveType::Boolean, PrimitiveType::Boolean),
            (PrimitiveType::Byte, PrimitiveType::Short),
            (PrimitiveType::Byte, PrimitiveType::Double),
            (PrimitiveType::Short, PrimitiveType::Int),
            (PrimitiveType::Char, PrimitiveType::Int),
            (PrimitiveType::Int, PrimitiveType::Long),
            (PrimitiveType::Long, PrimitiveType::Float),
            (PrimitiveType::Float, PrimitiveType::Double),
        ];
        for (actual, expected) in cases {
            assert_eq!(
                assign(primitive(actual), primitive(expected), &NoSymbols),
                Some(true),
                "{actual:?} to {expected:?}"
            );
        }
    }

    #[test]
    fn unsupported_primitive_narrowing_is_incompatible() {
        for (actual, expected) in [
            (PrimitiveType::Long, PrimitiveType::Int),
            (PrimitiveType::Double, PrimitiveType::Float),
            (PrimitiveType::Float, PrimitiveType::Long),
        ] {
            assert_eq!(
                assign(primitive(actual), primitive(expected), &NoSymbols),
                Some(false),
                "{actual:?} to {expected:?}"
            );
        }
    }

    #[test]
    fn all_eight_boxing_and_unboxing_pairs_are_assignable() {
        let mappings = [
            (PrimitiveType::Boolean, "java.lang.Boolean"),
            (PrimitiveType::Byte, "java.lang.Byte"),
            (PrimitiveType::Short, "java.lang.Short"),
            (PrimitiveType::Int, "java.lang.Integer"),
            (PrimitiveType::Long, "java.lang.Long"),
            (PrimitiveType::Char, "java.lang.Character"),
            (PrimitiveType::Float, "java.lang.Float"),
            (PrimitiveType::Double, "java.lang.Double"),
        ];
        for (primitive_type, wrapper) in mappings {
            assert_eq!(
                assign(
                    primitive(primitive_type),
                    external(wrapper, &[]),
                    &NoSymbols
                ),
                Some(true),
                "boxing {primitive_type:?}"
            );
            assert_eq!(
                assign(
                    external(wrapper, &[]),
                    primitive(primitive_type),
                    &NoSymbols
                ),
                Some(true),
                "unboxing {primitive_type:?}"
            );
        }
    }

    #[test]
    fn unboxing_followed_by_primitive_widening_is_assignable() {
        for (wrapper, expected) in [
            ("java.lang.Byte", PrimitiveType::Double),
            ("java.lang.Integer", PrimitiveType::Long),
            ("java.lang.Character", PrimitiveType::Double),
        ] {
            assert_eq!(
                assign(external(wrapper, &[]), primitive(expected), &NoSymbols),
                Some(true),
                "{wrapper} to {expected:?}"
            );
        }
    }

    #[test]
    fn int_constant_narrowing_stays_unknown_for_primitives_and_boxes() {
        for expected in [
            primitive(PrimitiveType::Byte),
            primitive(PrimitiveType::Short),
            primitive(PrimitiveType::Char),
            external("java.lang.Byte", &[]),
            external("java.lang.Short", &[]),
            external("java.lang.Character", &[]),
        ] {
            assert_eq!(
                assign(primitive(PrimitiveType::Int), expected, &NoSymbols),
                None
            );
        }
    }

    #[test]
    fn potential_constant_narrowing_is_unknown_for_primitive_and_boxed_targets() {
        fn boxed_target(primitive: PrimitiveType) -> &'static str {
            match primitive {
                PrimitiveType::Byte => "java.lang.Byte",
                PrimitiveType::Short => "java.lang.Short",
                PrimitiveType::Char => "java.lang.Character",
                _ => unreachable!("only constant-narrowing targets are used"),
            }
        }

        for (actual, expected) in [
            (PrimitiveType::Byte, PrimitiveType::Char),
            (PrimitiveType::Short, PrimitiveType::Byte),
            (PrimitiveType::Short, PrimitiveType::Char),
            (PrimitiveType::Char, PrimitiveType::Byte),
            (PrimitiveType::Char, PrimitiveType::Short),
            (PrimitiveType::Int, PrimitiveType::Byte),
            (PrimitiveType::Int, PrimitiveType::Short),
            (PrimitiveType::Int, PrimitiveType::Char),
        ] {
            assert_eq!(
                assign(primitive(actual), primitive(expected), &NoSymbols),
                None,
                "{actual:?} to {expected:?}"
            );
            assert_eq!(
                assign(
                    primitive(actual),
                    external(boxed_target(expected), &[]),
                    &NoSymbols,
                ),
                None,
                "{actual:?} to boxed {expected:?}"
            );
        }

        for (actual, expected) in [
            (PrimitiveType::Long, PrimitiveType::Byte),
            (PrimitiveType::Float, PrimitiveType::Short),
            (PrimitiveType::Double, PrimitiveType::Char),
        ] {
            assert_eq!(
                assign(primitive(actual), primitive(expected), &NoSymbols),
                Some(false),
                "{actual:?} to {expected:?}"
            );
            assert_eq!(
                assign(
                    primitive(actual),
                    external(boxed_target(expected), &[]),
                    &NoSymbols,
                ),
                Some(false),
                "{actual:?} to boxed {expected:?}"
            );
        }
    }

    #[test]
    fn primitive_reference_mismatches_are_proven_incompatible() {
        assert_eq!(
            assign(
                primitive(PrimitiveType::Int),
                external("java.lang.String", &[]),
                &BoxingHierarchySymbols,
            ),
            Some(false)
        );
        assert_eq!(
            assign(
                external("java.lang.String", &[]),
                primitive(PrimitiveType::Int),
                &BoxingHierarchySymbols,
            ),
            Some(false)
        );
    }

    #[test]
    fn null_is_assignable_only_to_references_and_arrays() {
        assert_eq!(
            assign(
                ResolvedType::Null,
                external("java.lang.String", &[]),
                &NoSymbols,
            ),
            Some(true)
        );
        assert_eq!(
            assign(ResolvedType::Null, array("String[]"), &NoSymbols),
            Some(true)
        );
        assert_eq!(
            assign(
                ResolvedType::Null,
                primitive(PrimitiveType::Int),
                &NoSymbols,
            ),
            Some(false)
        );
    }

    #[test]
    fn arrays_are_assignable_only_when_provably_related() {
        assert_eq!(
            assign(array("Object[]"), array("Object[]"), &NoSymbols),
            Some(true)
        );
        assert_eq!(
            assign(array("int[]"), array("long[]"), &NoSymbols),
            Some(false),
            "primitive arrays are invariant, never widen"
        );
        assert_eq!(
            assign(
                array("java.lang.String[]"),
                array("java.lang.Object[]"),
                &NoSymbols
            ),
            Some(true),
            "reference arrays are covariant and are now provable via the structured element type"
        );
    }

    /// A minimal but *real* `ClassMetadata`-carrying fixture: `own_fqn`'s
    /// own type parameters (identified by `(own_fqn, index)`, never by
    /// letter) plus direct, unparameterized supertypes. Every
    /// hierarchy-walk test here links supertypes by raw reference only;
    /// use-site argument flow is exercised by `srcclass.rs`'s/
    /// `class_info.rs`'s own tests.
    fn class_with_supertypes(
        own_fqn: &str,
        supertypes: Vec<TypeRef>,
        type_params: &[&str],
    ) -> ExternalClass {
        let type_parameters: Vec<TypeParameter> = type_params
            .iter()
            .enumerate()
            .map(|(index, _)| TypeParameter {
                id: TypeVariableId {
                    owner: own_fqn.to_string(),
                    index,
                },
                bounds: Vec::new(),
            })
            .collect();
        ExternalClass {
            supers: supertypes.iter().map(render_type_ref).collect(),
            type_params: type_params.iter().map(|value| value.to_string()).collect(),
            members: Vec::new(),
            metadata: Some(ClassMetadata {
                id: TypeId::named(own_fqn),
                kind: ClassKind::Class,
                access: Access::Public,
                is_abstract: false,
                is_static: true,
                enclosing_class: None,
                type_parameters,
                supertypes,
                hierarchy_complete: true,
                constructors_complete: true,
            }),
        }
    }

    fn class(own_fqn: &str, supers: &[&str], type_params: &[&str]) -> ExternalClass {
        class_with_supertypes(
            own_fqn,
            supers.iter().map(|s| TypeRef::named(s)).collect(),
            type_params,
        )
    }

    struct BoxingHierarchySymbols;

    impl SymbolSource for BoxingHierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                // The real JDK declaration is `Integer implements
                // Comparable<Integer>` — a parameterized supertype — so the
                // fixture must carry that argument for the
                // `Comparable<Integer>` case below to be provable.
                "java.lang.Integer" | "java.lang.Long" => Some(class_with_supertypes(
                    fqn,
                    vec![
                        TypeRef::named("java.lang.Number"),
                        TypeRef::named_with("java.lang.Comparable", vec![TypeRef::named(fqn)]),
                    ],
                    &[],
                )),
                "java.lang.Number" => Some(class(
                    fqn,
                    &["java.lang.Object", "java.io.Serializable"],
                    &[],
                )),
                "java.lang.String" => Some(class(
                    fqn,
                    &[
                        "java.lang.Object",
                        "java.lang.Comparable",
                        "java.io.Serializable",
                    ],
                    &[],
                )),
                "java.lang.Comparable" => Some(class(fqn, &[], &["T"])),
                "java.io.Serializable" | "java.lang.Object" => Some(class(fqn, &[], &[])),
                _ => None,
            }
        }
    }

    #[test]
    fn boxing_followed_by_widening_reference_is_assignable() {
        for (actual, expected) in [
            (PrimitiveType::Int, external("java.lang.Object", &[])),
            (PrimitiveType::Int, external("java.lang.Number", &[])),
            (
                PrimitiveType::Int,
                external("java.lang.Comparable", &["java.lang.Integer"]),
            ),
            (PrimitiveType::Long, external("java.io.Serializable", &[])),
        ] {
            assert_eq!(
                assign(primitive(actual), expected, &BoxingHierarchySymbols),
                Some(true),
                "{actual:?} boxing plus widening reference"
            );
        }

        assert_eq!(
            assign(
                primitive(PrimitiveType::Int),
                external("java.lang.String", &[]),
                &BoxingHierarchySymbols,
            ),
            Some(false),
            "the complete Integer hierarchy proves String incompatible"
        );
    }

    struct HierarchySymbols;

    impl SymbolSource for HierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.Leaf" => Some(class(fqn, &["test.Middle"], &["T"])),
                "test.Middle" => Some(class(fqn, &["test.Root", "test.Marker"], &[])),
                "test.Root" | "test.Marker" | "test.Other" => {
                    Some(class(fqn, &["java.lang.Object"], &["T"]))
                }
                "java.util.List" => Some(class(fqn, &["java.lang.Object"], &["E"])),
                "java.lang.Object" => Some(class(fqn, &[], &[])),
                _ => None,
            }
        }
    }

    #[test]
    fn complete_external_class_and_interface_hierarchy_proves_both_answers() {
        assert_eq!(
            assign(
                external("test.Leaf", &[]),
                external("test.Root", &[]),
                &HierarchySymbols,
            ),
            Some(true)
        );
        assert_eq!(
            assign(
                external("test.Leaf", &[]),
                external("test.Marker", &[]),
                &HierarchySymbols,
            ),
            Some(true)
        );
        assert_eq!(
            assign(
                external("test.Leaf", &[]),
                external("test.Other", &[]),
                &HierarchySymbols,
            ),
            Some(false)
        );
    }

    #[test]
    fn parameterized_external_assignability_is_conservative() {
        assert_eq!(
            assign(
                external("java.util.List", &["String"]),
                external("java.util.List", &["String"]),
                &HierarchySymbols,
            ),
            Some(true)
        );
        assert_eq!(
            assign(
                external("java.util.List", &["String"]),
                external("java.util.List", &["Integer"]),
                &HierarchySymbols,
            ),
            Some(false),
            "invariant generics: differing concrete arguments are now a proved mismatch"
        );
        assert_eq!(
            assign(
                external("java.util.List", &["?"]),
                external("java.util.List", &["?"]),
                &HierarchySymbols,
            ),
            Some(true),
            "a bare wildcard target contains anything, including another bare wildcard"
        );
        assert_eq!(
            assign(
                external("test.Leaf", &["String"]),
                external("test.Root", &["Integer"]),
                &HierarchySymbols,
            ),
            None,
            "a raw (untracked-args) intermediate link into a parameterized target is an unchecked conversion, not a proof"
        );
    }

    struct IncompleteHierarchySymbols;

    impl SymbolSource for IncompleteHierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.Broken" => Some(class(fqn, &["test.Missing"], &[])),
                "test.Other" => Some(class(fqn, &["java.lang.Object"], &[])),
                "java.lang.Object" => Some(class(fqn, &[], &[])),
                _ => None,
            }
        }
    }

    struct CyclicHierarchySymbols;

    impl SymbolSource for CyclicHierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.A" => Some(class(fqn, &["test.B"], &[])),
                "test.B" => Some(class(fqn, &["test.A"], &[])),
                "test.Other" => Some(class(fqn, &["java.lang.Object"], &[])),
                "java.lang.Object" => Some(class(fqn, &[], &[])),
                _ => None,
            }
        }
    }

    struct DeepHierarchySymbols;

    impl SymbolSource for DeepHierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            if let Some(depth) = fqn
                .strip_prefix("test.Depth")
                .and_then(|value| value.parse::<usize>().ok())
            {
                return Some(if depth < 70 {
                    let next = format!("test.Depth{}", depth + 1);
                    class_with_supertypes(fqn, vec![TypeRef::named(&next)], &[])
                } else {
                    class(fqn, &["java.lang.Object"], &[])
                });
            }
            match fqn {
                "test.Other" => Some(class(fqn, &["java.lang.Object"], &[])),
                "java.lang.Object" => Some(class(fqn, &[], &[])),
                _ => None,
            }
        }
    }

    #[test]
    fn incomplete_cyclic_and_depth_exhausted_hierarchies_stay_unknown() {
        for (actual, symbols) in [
            (
                "test.Broken",
                &IncompleteHierarchySymbols as &dyn SymbolSource,
            ),
            ("test.A", &CyclicHierarchySymbols as &dyn SymbolSource),
            ("test.Depth0", &DeepHierarchySymbols as &dyn SymbolSource),
        ] {
            assert_eq!(
                assign(external(actual, &[]), external("test.Other", &[]), symbols,),
                None,
                "{actual}"
            );
        }
    }

    /// `InProject` identity is proved through qualified `TypeId`s, not node
    /// identity plus `ctx.current`. Two different declarations sharing only
    /// an implicit `Object` ancestor prove `Some(false)`; the same
    /// declaration reached via another open document proves `Some(true)`.
    #[test]
    fn is_assignable_proves_identity_and_inequality_regardless_of_document() {
        assert_eq!(
            in_project_assign(&["class A {} class B {}"], 0, "A", "A"),
            Some(true)
        );
        assert_eq!(
            in_project_assign(&["class A {} class B {}"], 0, "A", "B"),
            Some(false),
            "two unrelated classes with only an implicit Object ancestor are now provably incompatible"
        );
        assert_eq!(
            in_project_assign(
                &["class Current {}", "class Foreign {}"],
                0,
                "Foreign",
                "Foreign"
            ),
            Some(true),
            "the same declaration, reached via another open document, is now provably identical"
        );
    }

    /// The `type` field of the sole `method_declaration` in `tree`.
    fn method_return_type_node(tree: &Tree) -> Node<'_> {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "method_declaration" {
                return node.child_by_field_name("type").expect("return type");
            }
            stack.extend(named_children(node));
        }
        panic!("method declaration not found");
    }

    /// The `type` field of the sole `object_creation_expression` in `tree`.
    fn object_creation_type_node(tree: &Tree) -> Node<'_> {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "object_creation_expression" {
                return node.child_by_field_name("type").expect("creation type");
            }
            stack.extend(named_children(node));
        }
        panic!("object creation expression not found");
    }

    /// The `type` field of the sole `field_declaration` in `tree`.
    fn field_declaration_type_node(tree: &Tree) -> Node<'_> {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "field_declaration" {
                return node.child_by_field_name("type").expect("field type");
            }
            stack.extend(named_children(node));
        }
        panic!("field declaration not found");
    }

    /// `import a.User;` resolves the declared return type to `a.User`; a
    /// fully-qualified `new b.User()` resolves to the distinct `b.User`
    /// declaration in the other package.
    #[test]
    fn qualified_identity_distinguishes_same_simple_name_across_packages() {
        let doc_a = "package a; public class User {}\n";
        let doc_b = "package b; public class User {}\n";
        let doc_c = "package c; import a.User; class C { User f() { return new b.User(); } }\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let tree_c = tree(doc_c);
        let docs = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
        ];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree_c, doc_c);

        let declared = table
            .resolve_type_name_node(method_return_type_node(&tree_c), doc_c, 0, &imports)
            .expect("declared return type resolves through the explicit import");
        assert_eq!(declared.binary_name.as_deref(), Some("a.User"));

        let actual = table
            .resolve_type_name_node(object_creation_type_node(&tree_c), doc_c, 0, &imports)
            .expect("dotted `b.User` resolves via qualified nesting");
        assert_eq!(actual.binary_name.as_deref(), Some("b.User"));
        assert_ne!(
            declared.node.id(),
            actual.node.id(),
            "a.User and b.User must be distinct declarations"
        );
    }

    /// The same scenario but `return new a.User();` — the actual and
    /// declared return types resolve to the SAME declaration.
    #[test]
    fn qualified_identity_resolves_same_binary_name_to_identical_declaration() {
        let doc_a = "package a; public class User {}\n";
        let doc_b = "package b; public class User {}\n";
        let doc_c = "package c; import a.User; class C { User f() { return new a.User(); } }\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let tree_c = tree(doc_c);
        let docs = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
        ];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree_c, doc_c);

        let declared = table
            .resolve_type_name_node(method_return_type_node(&tree_c), doc_c, 0, &imports)
            .expect("declared return type resolves");
        let actual = table
            .resolve_type_name_node(object_creation_type_node(&tree_c), doc_c, 0, &imports)
            .expect("dotted actual type resolves");
        assert_eq!(declared.node.id(), actual.node.id());
        assert_eq!(declared.binary_name.as_deref(), Some("a.User"));
    }

    /// A member's declared type resolves through its own declaring
    /// document's imports, never the caller's: `Api` (package `p`) imports
    /// `x.Result`; the caller (package `q`) imports a distinct `y.Result`.
    /// Resolving `Api::r`'s return type via `Ctx::for_document` must land on
    /// `x.Result`.
    #[test]
    fn member_return_type_resolves_in_declaring_documents_imports() {
        let doc_x = "package x; public class Result {}\n";
        let doc_y = "package y; public class Result {}\n";
        let doc_a = "package p; import x.Result; class Api { Result r() { return null; } }\n";
        let doc_b = "package q; import y.Result; import p.Api; class C { }\n";
        let tree_x = tree(doc_x);
        let tree_y = tree(doc_y);
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
            OpenDoc {
                source: doc_x,
                tree: &tree_x,
            },
            OpenDoc {
                source: doc_y,
                tree: &tree_y,
            },
        ];
        let current = 0;
        let table = TypeTable::build(&docs, current);
        let imports = Imports::parse(&tree_b, doc_b);
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[current],
            current,
            table: &table,
            imports: &imports,
            symbols: &NoSymbols,
            docs: &docs,
            facts: &facts,
        };

        let api = table.get_named("p.Api").expect("Api indexed");
        let dctx = ctx
            .for_document(api.doc)
            .expect("Api's own document context");
        let resolved = resolve_type_node(method_return_type_node(&tree_a), api.source, &dctx)
            .expect("resolves through the declaring document's own imports");
        match resolved {
            ResolvedType::InProject { decl: td, .. } => {
                assert_eq!(td.binary_name.as_deref(), Some("x.Result"))
            }
            _ => panic!("expected an in-project resolution"),
        }
    }

    /// Two wildcard imports both offering `User` (in different packages,
    /// neither an explicit single-type import, no same-package candidate)
    /// leave a bare `User` reference ambiguous — never guessed.
    #[test]
    fn wildcard_ambiguity_is_unresolved() {
        let doc_a = "package a; public class User {}\n";
        let doc_b = "package b; public class User {}\n";
        let doc_c = "package c; import a.*; import b.*; class C { User u; }\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let tree_c = tree(doc_c);
        let docs = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
        ];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree_c, doc_c);
        assert!(table
            .resolve_type_name_node(field_declaration_type_node(&tree_c), doc_c, 0, &imports)
            .is_none());
    }

    /// Opening an unrelated third `User` (package `d`, not imported/
    /// wildcarded) must not change either resolution from the qualified-
    /// identity scenario above.
    #[test]
    fn unrelated_open_document_does_not_affect_qualified_resolution() {
        let doc_a = "package a; public class User {}\n";
        let doc_b = "package b; public class User {}\n";
        let doc_d = "package d; public class User {}\n";
        let doc_c = "package c; import a.User; class C { User f() { return new b.User(); } }\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let tree_d = tree(doc_d);
        let tree_c = tree(doc_c);
        let docs = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_d,
                tree: &tree_d,
            },
        ];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&tree_c, doc_c);

        let declared = table
            .resolve_type_name_node(method_return_type_node(&tree_c), doc_c, 0, &imports)
            .expect("declared return type resolves");
        assert_eq!(declared.binary_name.as_deref(), Some("a.User"));
        let actual = table
            .resolve_type_name_node(object_creation_type_node(&tree_c), doc_c, 0, &imports)
            .expect("dotted actual type resolves");
        assert_eq!(actual.binary_name.as_deref(), Some("b.User"));
    }

    /// An assignability table built from one small in-project corpus
    /// (`Animal`/`Dog`/`Cat`/`Named`/`Order`/`User`/`Box<T>`/`UserBox`) plus
    /// a few externally-fixtured cases needing a hierarchy no snippet can
    /// express. `assign_in` resolves two bare simple names to `TypeRef`s and
    /// runs them through [`assignable_refs`] directly, one level below
    /// `is_assignable`'s `ResolvedType` wrapping.
    const ASSIGN_CORPUS: &str = "\
        package demo;\n\
        class Animal {}\n\
        class Dog extends Animal {}\n\
        class Cat extends Animal {}\n\
        interface Named {}\n\
        class Order {}\n\
        class User implements Named {}\n\
        class Box<T> { T get() { return null; } }\n\
        class UserBox extends Box<User> {}\n\
        class Missing2 {}\n\
        class Broken extends Missing {}\n\
        class Cyc1 extends Cyc2 {}\n\
        class Cyc2 extends Cyc1 {}\n\
        class A {}\n\
    ";

    fn assign_in(source: &str, actual: TypeRef, expected: TypeRef) -> Option<bool> {
        let t = tree(source);
        let docs = [OpenDoc { source, tree: &t }];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&t, source);
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols: &NoSymbols,
            docs: &docs,
            facts: &facts,
        };
        assignable_refs(&actual, &expected, &ctx)
    }

    #[test]
    fn assign_dog_to_animal_is_true() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named("demo.Dog"),
                TypeRef::named("demo.Animal")
            ),
            Some(true)
        );
    }

    #[test]
    fn assign_animal_to_dog_is_false() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named("demo.Animal"),
                TypeRef::named("demo.Dog")
            ),
            Some(false)
        );
    }

    #[test]
    fn assign_cat_to_dog_is_false() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named("demo.Cat"),
                TypeRef::named("demo.Dog")
            ),
            Some(false)
        );
    }

    #[test]
    fn assign_order_to_user_is_false() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named("demo.Order"),
                TypeRef::named("demo.User")
            ),
            Some(false)
        );
    }

    #[test]
    fn assign_user_to_named_is_true() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named("demo.User"),
                TypeRef::named("demo.Named")
            ),
            Some(true)
        );
    }

    #[test]
    fn assign_boxed_user_to_boxed_order_is_false() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.User")]),
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.Order")]),
            ),
            Some(false),
            "invariant generics: User <: Named does not make Box<User> a Box<Named>"
        );
    }

    #[test]
    fn assign_boxed_user_to_boxed_named_is_false() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.User")]),
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.Named")]),
            ),
            Some(false)
        );
    }

    #[test]
    fn assign_user_box_to_boxed_user_is_true() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named("demo.UserBox"),
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.User")]),
            ),
            Some(true)
        );
    }

    #[test]
    fn assign_boxed_dog_to_boxed_bounded_wildcard_animal_is_true() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.Dog")]),
                TypeRef::named_with(
                    "demo.Box",
                    vec![TypeRef::Wildcard {
                        upper: Some(Box::new(TypeRef::named("demo.Animal"))),
                        lower: None,
                    }],
                ),
            ),
            Some(true)
        );
    }

    #[test]
    fn bounded_wildcard_containment_respects_bound_direction() {
        let box_of = |argument| TypeRef::named_with("demo.Box", vec![argument]);
        let extends_dog = TypeRef::Wildcard {
            upper: Some(Box::new(TypeRef::named("demo.Dog"))),
            lower: None,
        };
        let super_animal = TypeRef::Wildcard {
            upper: None,
            lower: Some(Box::new(TypeRef::named("demo.Animal"))),
        };
        let super_dog = TypeRef::Wildcard {
            upper: None,
            lower: Some(Box::new(TypeRef::named("demo.Dog"))),
        };
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                box_of(TypeRef::named("demo.Animal")),
                box_of(super_dog),
            ),
            Some(true)
        );
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                box_of(TypeRef::named("demo.Animal")),
                box_of(extends_dog),
            ),
            Some(false)
        );
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                box_of(TypeRef::named("demo.Dog")),
                box_of(super_animal),
            ),
            Some(false)
        );
    }

    #[test]
    fn assign_dog_array_to_animal_array_is_true() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::Array(Box::new(TypeRef::named("demo.Dog"))),
                TypeRef::Array(Box::new(TypeRef::named("demo.Animal"))),
            ),
            Some(true)
        );
    }

    #[test]
    fn assign_int_array_to_long_array_is_false() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::Array(Box::new(TypeRef::Primitive(PrimitiveType::Int))),
                TypeRef::Array(Box::new(TypeRef::Primitive(PrimitiveType::Long))),
            ),
            Some(false),
            "primitive arrays are invariant, never widen"
        );
    }

    #[test]
    fn assign_class_with_unresolvable_supertype_to_order_is_unknown() {
        assert_eq!(
            assign_in(ASSIGN_CORPUS, TypeRef::named("demo.Broken"), TypeRef::named("demo.Order")),
            None,
            "an unresolvable `extends Missing` makes the hierarchy incomplete, never a guessed false"
        );
    }

    #[test]
    fn assign_cyclic_hierarchy_is_unknown_without_hanging() {
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named("demo.Cyc1"),
                TypeRef::named("demo.Order")
            ),
            None,
            "the depth/seen-set guard must stop a cyclic `extends` without a stack overflow"
        );
    }

    #[test]
    fn class_named_a_as_a_type_argument_does_not_misfire_as_a_variable() {
        // A class literally named `A` must never be mistaken for a type
        // variable identified by that same letter.
        assert_eq!(
            assign_in(
                ASSIGN_CORPUS,
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.A")]),
                TypeRef::named_with("demo.Box", vec![TypeRef::named("demo.A")]),
            ),
            Some(true)
        );
    }

    struct GSymbols;

    impl SymbolSource for GSymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            (fqn == "test.G").then(|| {
                let mut c = class("test.G", &[], &["T"]);
                if let Some(meta) = &mut c.metadata {
                    meta.type_parameters[0].bounds = vec![TypeRef::named("java.lang.Number")];
                }
                c
            })
        }
    }

    #[test]
    fn declared_type_variable_is_assignable_to_its_own_bound() {
        // `class G<T extends Number> { Animal f(T t){ return t; } }` —
        // proved one level below `resolve_type_node`, without a full
        // source round-trip.
        let v = TypeRef::Variable(TypeVariableId {
            owner: "test.G".to_string(),
            index: 0,
        });
        assert_eq!(
            assign_in("class C {}", v, TypeRef::named("java.lang.Number")),
            None,
            "NoSymbols alone cannot see test.G's bound"
        );
        let src = "class C {}";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let table = TypeTable::build(&docs, 0);
        let imports = Imports::parse(&t, src);
        let facts = FactsCache::default();
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols: &GSymbols,
            docs: &docs,
            facts: &facts,
        };
        let v = TypeRef::Variable(TypeVariableId {
            owner: "test.G".to_string(),
            index: 0,
        });
        assert_eq!(
            assignable_refs(&v, &TypeRef::named("java.lang.Number"), &ctx),
            Some(true)
        );
    }

    #[test]
    fn ternary_expression_types_as_the_wider_provable_branch() {
        let src = "package demo; class Animal {} class Dog extends Animal {} \
                   class C { Object m(boolean b) { return b ? new Dog() : new Animal(); } }";
        assert_eq!(
            expression_display(src, &NoSymbols).as_deref(),
            Some("Animal")
        );
    }

    #[test]
    fn ternary_expression_with_unrelated_branches_is_unknown() {
        let src = "package demo; class Animal {} class Dog extends Animal {} class Cat extends Animal {} \
                   class C { Object m(boolean b) { return b ? new Dog() : new Cat(); } }";
        assert_eq!(expression_display(src, &NoSymbols), None);
    }

    #[test]
    fn assignment_expression_types_as_its_targets_type() {
        let src = "class C { int m() { int x = 1; return (x = 2); } }";
        assert_eq!(expression_display(src, &NoSymbols).as_deref(), Some("int"));
    }

    #[test]
    fn string_concatenation_types_as_string() {
        let src = "class C { String m() { return \"a\" + 1; } }";
        assert_eq!(
            expression_display(src, &NoSymbols).as_deref(),
            Some("String")
        );
    }

    #[test]
    fn comparison_binary_expression_types_as_boolean() {
        let src = "class C { boolean m() { return 1 == 2; } }";
        assert_eq!(
            expression_display(src, &NoSymbols).as_deref(),
            Some("boolean")
        );
    }

    #[test]
    fn arithmetic_binary_expression_is_unknown() {
        let src = "class C { int m() { return 1 + 2; } }";
        assert_eq!(expression_display(src, &NoSymbols), None);
    }

    #[test]
    fn logical_not_unary_expression_types_as_boolean() {
        let src = "class C { boolean m() { return !true; } }";
        assert_eq!(
            expression_display(src, &NoSymbols).as_deref(),
            Some("boolean")
        );
    }

    #[test]
    fn arithmetic_unary_expression_is_unknown() {
        let src = "class C { int m() { return -1; } }";
        assert_eq!(expression_display(src, &NoSymbols), None);
    }
}
