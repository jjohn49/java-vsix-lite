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

/// One of Java's eight primitive value types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PrimitiveType {
    Boolean,
    Byte,
    Short,
    Int,
    Long,
    Char,
    Float,
    Double,
}

/// A conservatively resolved Java value type. Reference types retain their
/// source/classpath identity; primitives, `void`, `null`, and arrays remain
/// distinct so callers never have to reconstruct value semantics from text.
pub(crate) enum ResolvedType<'t> {
    InProject(TypeDecl<'t>),
    External {
        fqn: String,
        args: Vec<String>,
    },
    Primitive(PrimitiveType),
    Void,
    Null,
    Array {
        /// The declared display text (`String[]`).
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
/// shapes. `pub(crate)` so `rename.rs` can walk past a nested type's
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
    match recv.kind() {
        "this" => enclosing_typedecl(recv, ctx.doc.source, ctx.current)
            .map(|td| instance(ResolvedType::InProject(td))),
        "super" => {
            let td = enclosing_typedecl(recv, ctx.doc.source, ctx.current)?;
            let sup = td.supers.first()?;
            resolve_super(sup, ctx).map(instance)
        }
        "identifier" | "type_identifier" => resolve_name_depth(
            node_text(recv, ctx.doc.source),
            recv.start_byte(),
            ctx,
            depth + 1,
            method_lookup,
        ),
        "method_invocation" => {
            let name = recv.child_by_field_name("name")?;
            let recv_ty = match recv.child_by_field_name("object") {
                Some(obj) => resolve_receiver_depth(obj, ctx, depth + 1, method_lookup)?,
                None => instance(ResolvedType::InProject(enclosing_typedecl(
                    recv,
                    ctx.doc.source,
                    ctx.current,
                )?)),
            };
            let member = find_method(
                &recv_ty,
                ctx,
                node_text(name, ctx.doc.source),
                method_lookup,
            )?;
            member_result_type(&member, &recv_ty, ctx, method_lookup)
        }
        "cast_expression" => {
            let ty = recv.child_by_field_name("type")?;
            resolve_type_node(ty, ctx.doc.source, ctx).map(instance)
        }
        "array_access" => {
            let arr = recv.child_by_field_name("array")?;
            let a = resolve_receiver_depth(arr, ctx, depth + 1, method_lookup)?;
            match &a.ty {
                ResolvedType::Array { display } => array_element_type(display, ctx),
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
            if let Some(obj_ty) =
                resolve_receiver_depth(obj, ctx, depth + 1, method_lookup)
            {
                return resolve_member_segment(
                    &obj_ty,
                    node_text(field, ctx.doc.source),
                    ctx,
                    method_lookup,
                );
            }
            resolve_scoped_path(recv, ctx, method_lookup)
        }
        "object_creation_expression" => resolve_object_creation_type(recv, ctx).map(instance),
        "scoped_type_identifier" | "scoped_identifier" => {
            resolve_scoped_path(recv, ctx, method_lookup)
        }
        "parenthesized_expression" => resolve_receiver_depth(
            recv.named_child(0)?,
            ctx,
            depth + 1,
            method_lookup,
        ),
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
                .filter(|member| {
                    member.name() == name && MemberNamespace::Method.matches(member)
                });
            let member = matches.next()?;
            matches.next().is_none().then_some(member)
        }
    }
}

/// Whether `actual` can be returned where `expected` is declared. `None`
/// means this conservative layer cannot prove either answer.
pub(crate) fn is_assignable<'t>(
    actual: &ResolvedType<'t>,
    expected: &ResolvedType<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<bool> {
    match (actual, expected) {
        (
            ResolvedType::Null,
            ResolvedType::InProject(_)
            | ResolvedType::External { .. }
            | ResolvedType::Array { .. },
        ) => Some(true),
        (ResolvedType::Null, ResolvedType::Null) => Some(true),
        (ResolvedType::Null, _) | (_, ResolvedType::Null) => Some(false),
        (ResolvedType::Void, ResolvedType::Void) => Some(true),
        (ResolvedType::Void, _) | (_, ResolvedType::Void) => Some(false),
        (ResolvedType::Primitive(actual), ResolvedType::Primitive(expected)) => {
            primitive_assignable(*actual, *expected, true)
        }
        (
            ResolvedType::Primitive(actual),
            ResolvedType::External {
                fqn: expected, ..
            },
        ) => match boxed_primitive(expected) {
            Some(expected) if *actual == expected => Some(true),
            Some(expected) => primitive_assignable(*actual, expected, true).map(|_| false),
            None => external_subtype(primitive_box_fqn(*actual), expected, ctx.symbols),
        },
        (
            ResolvedType::External { fqn: actual, .. },
            ResolvedType::Primitive(expected),
        ) => match boxed_primitive(actual) {
            Some(actual) => primitive_assignable(actual, *expected, false),
            None => Some(false),
        },
        (ResolvedType::Primitive(_), _) | (_, ResolvedType::Primitive(_)) => Some(false),
        (
            ResolvedType::Array {
                display: actual,
            },
            ResolvedType::Array {
                display: expected,
            },
        ) => (actual == expected).then_some(true),
        (ResolvedType::Array { .. }, _) | (_, ResolvedType::Array { .. }) => None,
        (ResolvedType::InProject(actual), ResolvedType::InProject(expected)) => {
            if actual.doc != ctx.current || expected.doc != ctx.current {
                return None;
            }
            if actual.node.id() == expected.node.id() {
                Some(true)
            } else {
                None
            }
        }
        (ResolvedType::InProject(_), _) | (_, ResolvedType::InProject(_)) => None,
        (
            ResolvedType::External {
                fqn: actual,
                args: actual_args,
            },
            ResolvedType::External {
                fqn: expected,
                args: expected_args,
            },
        ) => {
            if type_args_unknown(actual_args) || type_args_unknown(expected_args) {
                return None;
            }
            if actual == expected {
                return if actual_args == expected_args {
                    Some(true)
                } else {
                    None
                };
            }
            external_subtype(actual, expected, ctx.symbols)
        }
    }
}

fn primitive_assignable(
    actual: PrimitiveType,
    expected: PrimitiveType,
    constant_narrowing_unknown: bool,
) -> Option<bool> {
    if actual == expected {
        return Some(true);
    }
    let widening = matches!(
        (actual, expected),
        (PrimitiveType::Byte, PrimitiveType::Short)
            | (PrimitiveType::Byte, PrimitiveType::Int)
            | (PrimitiveType::Byte, PrimitiveType::Long)
            | (PrimitiveType::Byte, PrimitiveType::Float)
            | (PrimitiveType::Byte, PrimitiveType::Double)
            | (PrimitiveType::Short, PrimitiveType::Int)
            | (PrimitiveType::Short, PrimitiveType::Long)
            | (PrimitiveType::Short, PrimitiveType::Float)
            | (PrimitiveType::Short, PrimitiveType::Double)
            | (PrimitiveType::Char, PrimitiveType::Int)
            | (PrimitiveType::Char, PrimitiveType::Long)
            | (PrimitiveType::Char, PrimitiveType::Float)
            | (PrimitiveType::Char, PrimitiveType::Double)
            | (PrimitiveType::Int, PrimitiveType::Long)
            | (PrimitiveType::Int, PrimitiveType::Float)
            | (PrimitiveType::Int, PrimitiveType::Double)
            | (PrimitiveType::Long, PrimitiveType::Float)
            | (PrimitiveType::Long, PrimitiveType::Double)
            | (PrimitiveType::Float, PrimitiveType::Double)
    );
    if widening {
        return Some(true);
    }
    if constant_narrowing_unknown
        && matches!(
            actual,
            PrimitiveType::Byte
                | PrimitiveType::Short
                | PrimitiveType::Char
                | PrimitiveType::Int
        )
        && matches!(
            expected,
            PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char
        )
    {
        return None;
    }
    Some(false)
}

fn boxed_primitive(fqn: &str) -> Option<PrimitiveType> {
    Some(match fqn {
        "java.lang.Boolean" => PrimitiveType::Boolean,
        "java.lang.Byte" => PrimitiveType::Byte,
        "java.lang.Short" => PrimitiveType::Short,
        "java.lang.Integer" => PrimitiveType::Int,
        "java.lang.Long" => PrimitiveType::Long,
        "java.lang.Character" => PrimitiveType::Char,
        "java.lang.Float" => PrimitiveType::Float,
        "java.lang.Double" => PrimitiveType::Double,
        _ => return None,
    })
}

fn primitive_box_fqn(primitive: PrimitiveType) -> &'static str {
    match primitive {
        PrimitiveType::Boolean => "java.lang.Boolean",
        PrimitiveType::Byte => "java.lang.Byte",
        PrimitiveType::Short => "java.lang.Short",
        PrimitiveType::Int => "java.lang.Integer",
        PrimitiveType::Long => "java.lang.Long",
        PrimitiveType::Char => "java.lang.Character",
        PrimitiveType::Float => "java.lang.Float",
        PrimitiveType::Double => "java.lang.Double",
    }
}

fn type_args_unknown(args: &[String]) -> bool {
    args.iter().any(|arg| {
        (arg.contains('?') || arg.contains('{') || arg.contains('}'))
            || arg
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
                .any(|part| part.len() == 1 && part.as_bytes()[0].is_ascii_uppercase())
    })
}

fn external_subtype(
    actual: &str,
    expected: &str,
    symbols: &dyn SymbolSource,
) -> Option<bool> {
    symbols.class(expected)?;
    let mut path = HashSet::new();
    external_subtype_walk(actual, expected, symbols, &mut path, 0)
}

fn external_subtype_walk(
    actual: &str,
    expected: &str,
    symbols: &dyn SymbolSource,
    path: &mut HashSet<String>,
    depth: usize,
) -> Option<bool> {
    if actual == expected {
        return Some(true);
    }
    if depth > MAX_RESOLVE_DEPTH || !path.insert(actual.to_string()) {
        return None;
    }
    let result = match symbols.class(actual) {
        None => None,
        Some(class) => {
            let mut complete = true;
            let mut found = false;
            for supertype in class.supers {
                match external_subtype_walk(
                    &supertype,
                    expected,
                    symbols,
                    path,
                    depth + 1,
                ) {
                    Some(true) => {
                        found = true;
                        break;
                    }
                    Some(false) => {}
                    None => complete = false,
                }
            }
            if found {
                Some(true)
            } else if complete {
                Some(false)
            } else {
                None
            }
        }
    };
    path.remove(actual);
    result
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
        // A `var` (or typeless) binding infers its type from the
        // declarator's initializer, resolved like any receiver expression
        // (`var v = new ArrayList<String>()`, `var t = s.trim()`, …).
        // Depth-capped: ERROR-recovery trees can produce self-referential
        // shapes legal Java can't. The result is always an instance —
        // whatever static-ness the initializer expression had does not
        // transfer to the value it produced.
        let value = binding.decl_node.child_by_field_name("value")?;
        // A `var` in an enhanced-for binds the *element* type of the iterable,
        // not the iterable's own type: `for (var s : List<String>)` → `s` is a
        // `String`. Resolving `value` directly would give `List` and mis-flag
        // every member access on the loop variable.
        if binding.decl_node.kind() == "enhanced_for_statement" {
            let iterable =
                resolve_receiver_depth(value, ctx, depth + 1, method_lookup)?;
            return iterable_element_type(&iterable.ty, ctx).map(instance);
        }
        return resolve_receiver_depth(value, ctx, depth + 1, method_lookup)
            .map(|r| instance(r.ty));
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

fn primitive_name(primitive: PrimitiveType) -> &'static str {
    match primitive {
        PrimitiveType::Boolean => "boolean",
        PrimitiveType::Byte => "byte",
        PrimitiveType::Short => "short",
        PrimitiveType::Int => "int",
        PrimitiveType::Long => "long",
        PrimitiveType::Char => "char",
        PrimitiveType::Float => "float",
        PrimitiveType::Double => "double",
    }
}

fn primitive_type(name: &str) -> Option<PrimitiveType> {
    Some(match name.trim() {
        "boolean" => PrimitiveType::Boolean,
        "byte" => PrimitiveType::Byte,
        "short" => PrimitiveType::Short,
        "int" => PrimitiveType::Int,
        "long" => PrimitiveType::Long,
        "char" => PrimitiveType::Char,
        "float" => PrimitiveType::Float,
        "double" => PrimitiveType::Double,
        _ => return None,
    })
}

/// Display name for a resolved type, as it would read in a signature
/// (`ArrayList<String>`, an in-project `Widget`, `String[]`). External FQNs are
/// shown by their simple name plus any use-site type arguments. Used to render
/// an inferred `var` type on hover.
pub(crate) fn type_display(ty: &ResolvedType) -> String {
    match ty {
        ResolvedType::InProject(td) => td.name.to_string(),
        ResolvedType::External { fqn, args } => {
            let simple = fqn.rsplit('.').next().unwrap_or(fqn);
            if args.is_empty() {
                simple.to_string()
            } else {
                format!("{simple}<{}>", args.join(", "))
            }
        }
        ResolvedType::Primitive(primitive) => primitive_name(*primitive).to_string(),
        ResolvedType::Void => "void".to_string(),
        ResolvedType::Null => "null".to_string(),
        ResolvedType::Array { display } => display.clone(),
    }
}

/// If the binding named `name` visible at `byte` is a `var` local, the display
/// string of its *inferred* type (from the declarator's initializer); `None`
/// for an explicitly-typed binding — render its declaration normally — or when
/// inference fails. Mirrors the `var` inference in [`resolve_name_depth`], but
/// stays silent unless the declared type is literally `var` so it never
/// overrides an explicit type. Used to render hover on a `var` local.
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
    let resolved = resolve_receiver_depth(value, ctx, 0, MethodLookup::First)?;
    Some(type_display(&resolved.ty))
}

/// If the binding named `name` visible at `byte` is a Java 21 pattern binding
/// (`case Type name`, a record-deconstruction component, or `instanceof Type
/// name`), the display string of its declared type; `None` for any other
/// binding (rendered from its declaration node instead). A type that doesn't
/// resolve to a class (a primitive like `int`) falls back to its written text.
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
pub(crate) fn resolve_type_node<'t>(
    type_node: Node<'t>,
    source: &'t str,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let display = node_text(type_node, source).trim();
    if display == "void" {
        return Some(ResolvedType::Void);
    }
    if let Some(primitive) = primitive_type(display) {
        return Some(ResolvedType::Primitive(primitive));
    }
    // An array's members are `length`/`clone()`/Object's — never the
    // element type's (an earlier behavior offered `String`'s members on a
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
/// An import path (`java.util.Map.Entry`) to the binary FQN
/// (`java.util.Map$Entry`) — replace trailing dots with `$` until the symbol
/// source recognizes the name. Shared by import completion and
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

/// `pub(crate)`: also used by hover's inherited-Javadoc walk, which
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
/// `pub(crate)`: also used by `implementation.rs`'s per-supertype
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

/// Resolve a dotted path (`a.b.c`): walk it segment by segment from
/// a resolvable head (binding → in-project type → imported/`java.lang`
/// external type), stepping through fields, nested types, and enum constants
/// in either world; failing that, try the longest prefix of the path as a
/// fully-qualified external type (`java.util.List`) and walk any remaining
/// segments from there.
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
    member_result_type(&member, current, ctx, method_lookup)
}

/// The type a member access *evaluates to* — a method call's return type
/// or a field/enum-constant's declared type — which becomes the next
/// receiver in a chain. `None` for primitives/void/arrays-of-unknown and
/// whatever else can't be re-resolved (the chain just stops, never errors).
fn member_result_type<'t>(
    member: &HierMember<'t>,
    recv: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    method_lookup: MethodLookup,
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
        HierMember::External(m) => {
            external_result_type(m, recv, ctx, method_lookup).map(instance)
        }
    }
}

/// An external member's result type. Prefers the generic `ret_display`
/// template substituted with the receiver's use-site type arguments (so
/// `List<String>.get(int)` chains as `String`, `stream()` as
/// `Stream<String>`). First-name receiver lookup may fall back to the erased
/// `ret_fqn`; unique semantic lookup must not infer a value type from erasure.
fn external_result_type<'t>(
    m: &ExternalMember,
    recv: &Resolved<'_>,
    ctx: &Ctx<'_, 't>,
    method_lookup: MethodLookup,
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
        if matches!(method_lookup, MethodLookup::Unique)
            && template_has_missing_arg(display, args.len())
        {
            return None;
        }
        let substituted = substitute_template(display, &args, &type_params);
        // A type variable the receiver did not pin down is not a concrete
        // value type. Semantic resolution must stay unknown so diagnostics do
        // not infer from its erased bound; historical receiver resolution may
        // still use the declared result to keep completion/hover chains alive.
        if (substituted.contains('{')
            || type_args_unknown(std::slice::from_ref(&substituted)))
            && matches!(method_lookup, MethodLookup::Unique)
        {
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


/// Resolve a rendered display type (`Stream<String>`, `String`,
/// `MyType`) back to a receiver type: the in-project table first, then the
/// erased FQN when its simple name agrees with the display's base (the
/// common generic-class case), then the file's imports/`java.lang` (the
/// type-variable case, where erasure and display genuinely differ).
fn resolve_display_type<'t>(
    display: &str,
    erased_fqn: Option<&str>,
    ctx: &Ctx<'_, 't>,
) -> Option<ResolvedType<'t>> {
    let display = display.trim();
    if display.ends_with("[]") {
        return Some(ResolvedType::Array {
            display: display.to_string(),
        });
    }
    if display == "void" {
        return Some(ResolvedType::Void);
    }
    if let Some(primitive) = primitive_type(display) {
        return Some(ResolvedType::Primitive(primitive));
    }
    let (base, args) = parse_display_type(display)?;
    if base.contains('.') && ctx.symbols.class(base).is_some() {
        return Some(ResolvedType::External {
            fqn: base.to_string(),
            args,
        });
    }
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

/// The element type of an array receiver (`arr[i].`), resolved from the
/// array's declared display text. Multi-dimensional arrays peel one level.
fn array_element_type<'t>(display: &str, ctx: &Ctx<'_, 't>) -> Option<Resolved<'t>> {
    let element = display.trim().strip_suffix("[]")?.trim_end();
    resolve_display_type(element, None, ctx).map(instance)
}

/// The element type produced by iterating `ty` — an array's element
/// (`String[]` → `String`) or a generic collection's first type argument
/// (`List<Foo>`/`Set<Foo>`/`Iterable<Foo>` → `Foo`). Returns `None` (never the
/// iterable's own type) when it can't be determined — a raw collection, an
/// unparameterized project type, an unresolvable argument — so an enhanced-for
/// `var` with an unknown element resolves to nothing rather than to the
/// collection itself (which would mis-resolve every access on the loop var).
fn iterable_element_type<'t>(ty: &ResolvedType<'t>, ctx: &Ctx<'_, 't>) -> Option<ResolvedType<'t>> {
    match ty {
        ResolvedType::Array { display } => array_element_type(display, ctx).map(|r| r.ty),
        ResolvedType::External { args, .. } => {
            let first = args.first()?.trim();
            if first.is_empty() || first.contains('?') {
                return None;
            }
            resolve_display_type(first, None, ctx)
        }
        // A project collection type's element isn't tracked (no generics from
        // source); non-reference values are not iterable here.
        ResolvedType::InProject(_)
        | ResolvedType::Primitive(_)
        | ResolvedType::Void
        | ResolvedType::Null => None,
    }
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
/// shift underneath them.
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
            // Lombok-generated accessors join the class's declared
            // members. Synthesized, not declared — no AST node to point at —
            // so they travel as External members; their result types resolve
            // through `ret_display`/`ret_fqn` like any bytecode member's.
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
    if matches!(
        &resolved.ty,
        ResolvedType::InProject(_)
            | ResolvedType::External { .. }
            | ResolvedType::Array { .. }
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
        ResolvedType::Primitive(_) | ResolvedType::Void | ResolvedType::Null => {}
    }
}

/// The signature to show for an external member: its generic template
/// substituted with the use-site type arguments when present, otherwise the
/// erased signature. `pub(crate)`: also used directly by hover and
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

/// Collect every `instanceof Type name` binding within a condition subtree
/// (`f instanceof String s && s.length() > 0` nests the pattern in a
/// `binary_expression`, so the whole subtree is walked).
fn push_instanceof_bindings<'t>(
    cond: Node<'t>,
    source: &'t str,
    doc: usize,
    out: &mut Vec<Binding<'t>>,
) {
    let mut stack = vec![cond];
    while let Some(n) = stack.pop() {
        if n.kind() == "instanceof_expression" {
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
        let td = table.get("Foo").expect("Foo indexed");
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

#[cfg(test)]
mod value_type_tests {
    use super::*;
    use crate::external::{
        ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols, SymbolSource,
    };
    use crate::{new_parser, parse};

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
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
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
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
        };
        resolve_receiver_type(returned_expression(&tree), &ctx)
            .map(|resolved| type_display(&resolved.ty))
    }

    fn primitive(ty: PrimitiveType) -> ResolvedType<'static> {
        ResolvedType::Primitive(ty)
    }

    fn external(fqn: &str, args: &[&str]) -> ResolvedType<'static> {
        ResolvedType::External {
            fqn: fqn.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
        }
    }

    fn array(display: &str) -> ResolvedType<'static> {
        ResolvedType::Array {
            display: display.to_string(),
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
        let ctx = Ctx {
            doc: &docs[0],
            current: 0,
            table: &table,
            imports: &imports,
            symbols,
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
            .map(|(source, tree)| OpenDoc {
                source: *source,
                tree,
            })
            .collect();
        let table = TypeTable::build(&docs, current);
        let imports = Imports::parse(docs[current].tree, docs[current].source);
        let ctx = Ctx {
            doc: &docs[current],
            current,
            table: &table,
            imports: &imports,
            symbols: &NoSymbols,
        };
        let actual = ResolvedType::InProject(table.get(actual).expect("actual type").clone());
        let expected = ResolvedType::InProject(table.get(expected).expect("expected type").clone());
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
        }
    }

    struct ValueSymbols;

    impl SymbolSource for ValueSymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            (fqn == "test.Values").then(|| ExternalClass {
                supers: Vec::new(),
                type_params: vec!["T".to_string()],
                members: vec![
                    result_member("size", "int size()", None, "int"),
                    result_member("clear", "void clear()", None, "void"),
                    result_member("values", "Object[] values()", None, "Object[]"),
                    result_member(
                        "pick",
                        "String pick()",
                        Some("java.lang.String"),
                        "String",
                    ),
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
        format!(
            "import test.Values; class C {{ Object m(Values v) {{ return v.{method}(); }} }}"
        )
    }

    fn static_value_call(method: &str) -> String {
        format!(
            "import test.Values; class C {{ Object m() {{ return Values.{method}(); }} }}"
        )
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
            let src =
                format!("class C {{ {primitive} m({primitive} value) {{ return value; }} }}");
            assert_eq!(
                expression_display(&src, &NoSymbols).as_deref(),
                Some(primitive),
                "declared {primitive}"
            );
        }
    }

    #[test]
    fn external_member_results_preserve_primitive_void_and_array_types() {
        for (method, expected) in [
            ("size", "int"),
            ("clear", "void"),
            ("values", "Object[]"),
        ] {
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
        assert_eq!(
            expression_display(src, &NoSymbols).as_deref(),
            Some("int")
        );
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
                assign(primitive(primitive_type), external(wrapper, &[]), &NoSymbols),
                Some(true),
                "boxing {primitive_type:?}"
            );
            assert_eq!(
                assign(external(wrapper, &[]), primitive(primitive_type), &NoSymbols),
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
    fn arrays_are_assignable_only_when_their_displays_match_exactly() {
        assert_eq!(
            assign(array("Object[]"), array("Object[]"), &NoSymbols),
            Some(true)
        );
        assert_eq!(assign(array("int[]"), array("long[]"), &NoSymbols), None);
        assert_eq!(
            assign(array("String[]"), array("Object[]"), &NoSymbols),
            None,
            "array covariance is outside the conservative native proof"
        );
    }

    fn class(supers: &[&str], type_params: &[&str]) -> ExternalClass {
        ExternalClass {
            supers: supers.iter().map(|value| value.to_string()).collect(),
            type_params: type_params
                .iter()
                .map(|value| value.to_string())
                .collect(),
            members: Vec::new(),
        }
    }

    struct BoxingHierarchySymbols;

    impl SymbolSource for BoxingHierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "java.lang.Integer" | "java.lang.Long" => Some(class(
                    &["java.lang.Number", "java.lang.Comparable"],
                    &[],
                )),
                "java.lang.Number" => Some(class(
                    &["java.lang.Object", "java.io.Serializable"],
                    &[],
                )),
                "java.lang.String" => Some(class(
                    &[
                        "java.lang.Object",
                        "java.lang.Comparable",
                        "java.io.Serializable",
                    ],
                    &[],
                )),
                "java.lang.Comparable" => Some(class(&[], &["T"])),
                "java.io.Serializable" | "java.lang.Object" => Some(class(&[], &[])),
                _ => None,
            }
        }
    }

    #[test]
    fn boxing_followed_by_widening_reference_is_assignable() {
        for (actual, expected) in [
            (
                PrimitiveType::Int,
                external("java.lang.Object", &[]),
            ),
            (
                PrimitiveType::Int,
                external("java.lang.Number", &[]),
            ),
            (
                PrimitiveType::Int,
                external("java.lang.Comparable", &["java.lang.Integer"]),
            ),
            (
                PrimitiveType::Long,
                external("java.io.Serializable", &[]),
            ),
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
                "test.Leaf" => Some(class(&["test.Middle"], &["T"])),
                "test.Middle" => Some(class(&["test.Root", "test.Marker"], &[])),
                "test.Root" | "test.Marker" | "test.Other" => {
                    Some(class(&["java.lang.Object"], &["T"]))
                }
                "java.util.List" => Some(class(&["java.lang.Object"], &["E"])),
                "java.lang.Object" => Some(class(&[], &[])),
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
            None
        );
        assert_eq!(
            assign(
                external("java.util.List", &["?"]),
                external("java.util.List", &["?"]),
                &HierarchySymbols,
            ),
            None,
            "wildcard equality is not a proof"
        );
        assert_eq!(
            assign(
                external("test.Leaf", &["String"]),
                external("test.Root", &["Integer"]),
                &HierarchySymbols,
            ),
            Some(true),
            "different raw types use only the erased hierarchy"
        );
    }

    struct IncompleteHierarchySymbols;

    impl SymbolSource for IncompleteHierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.Broken" => Some(class(&["test.Missing"], &[])),
                "test.Other" => Some(class(&["java.lang.Object"], &[])),
                "java.lang.Object" => Some(class(&[], &[])),
                _ => None,
            }
        }
    }

    struct CyclicHierarchySymbols;

    impl SymbolSource for CyclicHierarchySymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "test.A" => Some(class(&["test.B"], &[])),
                "test.B" => Some(class(&["test.A"], &[])),
                "test.Other" => Some(class(&["java.lang.Object"], &[])),
                "java.lang.Object" => Some(class(&[], &[])),
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
                    ExternalClass {
                        supers: vec![format!("test.Depth{}", depth + 1)],
                        type_params: Vec::new(),
                        members: Vec::new(),
                    }
                } else {
                    class(&["java.lang.Object"], &[])
                });
            }
            match fqn {
                "test.Other" => Some(class(&["java.lang.Object"], &[])),
                "java.lang.Object" => Some(class(&[], &[])),
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
            (
                "test.Depth0",
                &DeepHierarchySymbols as &dyn SymbolSource,
            ),
        ] {
            assert_eq!(
                assign(
                    external(actual, &[]),
                    external("test.Other", &[]),
                    symbols,
                ),
                None,
                "{actual}"
            );
        }
    }

    #[test]
    fn only_current_document_project_type_identity_is_proven() {
        assert_eq!(
            in_project_assign(&["class A {} class B {}"], 0, "A", "A"),
            Some(true)
        );
        assert_eq!(
            in_project_assign(&["class A {} class B {}"], 0, "A", "B"),
            None
        );
        assert_eq!(
            in_project_assign(&["class Current {}", "class Foreign {}"], 0, "Foreign", "Foreign"),
            None,
            "another open document must not influence current diagnostics"
        );
    }
}
