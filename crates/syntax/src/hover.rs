//! LSP hover: show a reconstructed signature (fenced `java`) plus the symbol's
//! Javadoc. Works on declarations and on references that resolve to a
//! declaration in an open file.

use ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use tree_sitter::{Node, Tree};

use crate::external::{ExternalMemberKind, SymbolSource};
use crate::imports::Imports;
use crate::model::{named_children, TypeTable};
use crate::resolve::{self, render_type_ref, Ctx, FactsCache, HierMember, Resolved, ResolvedType};
use crate::signature::{javadoc, param_count_in_label, param_labels, signature};
use crate::{node_text, LineIndex, OpenDoc};

/// What to render: an in-project declaration (signature + Javadoc from
/// source), or a pre-rendered external signature (bytecode carries no Javadoc).
enum Target<'t> {
    /// Declaration node + source, plus an inherited-Javadoc fallback used
    /// for `{@inheritDoc}` or doc-less overrides.
    InProject(Node<'t>, &'t str, Option<String>),
    /// A pre-rendered external signature plus optional Javadoc.
    External(String, Option<String>),
}

impl Target<'_> {
    fn with_signature(self, signature: String) -> Self {
        let doc = match self {
            Self::InProject(node, source, inherited) => javadoc(node, source).or(inherited),
            Self::External(_, doc) => doc,
        };
        Self::External(signature, doc)
    }
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
    let facts = FactsCache::default();
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
        docs,
        facts: &facts,
    };

    let name_node = identifier_at(doc.tree, cursor)?;
    let value = match resolve_target(name_node, &ctx)? {
        Target::InProject(node, source, inherited_doc) => {
            let sig = signature(node, source)?;
            let mut value = format!("```java\n{sig}\n```");
            if let Some(doc_text) = javadoc(node, source).or(inherited_doc) {
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

    // Cursor on the `new Foo(...)` type name: show the best-matching
    // constructor instead of falling through to the plain class declaration.
    if let Some(call) = enclosing_object_creation(name_node) {
        return constructor_target(call, ctx);
    }

    let name = node_text(name_node, ctx.doc.source);

    // A `var` local (declaration name or later use) renders its *inferred*
    // type — `var w = new Widget()` hovers as `Widget w`, not `var w`.
    if let Some(target) = local_var_target(name, name_node.start_byte(), ctx) {
        return Some(target);
    }

    if let Some(parent) = name_node.parent() {
        if is_decl_name(parent, name_node) {
            // An overriding method declared without its own doc (or with
            // only `{@inheritDoc}`) inherits the supertype's.
            let inherited = (parent.kind() == "method_declaration"
                && javadoc(parent, ctx.doc.source).is_none())
            .then(|| inherited_member_doc(parent, ctx.current, ctx, name))
            .flatten();
            return Some(Target::InProject(parent, ctx.doc.source, inherited));
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
                        ty: ResolvedType::InProject {
                            decl: resolve::enclosing_typedecl(name_node, ctx.table, ctx.current)?,
                            args: Vec::new(),
                        },
                        static_only: false,
                    },
                };
                let target = member_target(&resolved, ctx, name)?;
                return Some(
                    match crate::call::contextual_method_call_signature(parent, ctx) {
                        Some(signature) => target.with_signature(signature),
                        None => target,
                    },
                );
            }
            "method_reference"
                if named_children(parent)
                    .last()
                    .is_some_and(|node| node.id() == name_node.id()) =>
            {
                let signature = crate::call::method_reference_signature(parent, ctx)?;
                return Some(Target::External(signature, None));
            }
            // Mid-edit `recv.member` (no trailing `;`) parses as a scoped path; if
            // the cursor is on the trailing segment, resolve it as a member of the
            // prefix's type.
            "scoped_type_identifier" | "scoped_identifier" => {
                let segments = named_children(parent);
                if segments.len() >= 2 && segments.last() == Some(&name_node) {
                    if let Some(resolved) = resolve::resolve_receiver_type(segments[0], ctx) {
                        if let Some(target) = member_target(&resolved, ctx, name) {
                            return Some(target);
                        }
                        // Not a member of the prefix — may instead be a nested
                        // class (`Map.Entry`); fall through to the whole-path
                        // lookup below.
                    }
                    // Treat the whole dotted path as a fully-qualified type
                    // name, including nested classes via `$`-substitution.
                    let path = node_text(parent, ctx.doc.source)
                        .split_whitespace()
                        .collect::<String>();
                    if let Some(fqn) = resolve::import_path_to_fqn(&path, ctx) {
                        return external_type_target(&fqn, ctx);
                    }
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
        return Some(Target::InProject(binding.decl_node, binding.source, None));
    }
    if let Some(td) =
        ctx.table
            .resolve_type_name_node(name_node, ctx.doc.source, ctx.current, ctx.imports)
    {
        return Some(Target::InProject(td.node, td.source, None));
    }
    // Falls through to an external (JDK/dependency) class resolved via
    // imports or `java.lang`.
    let fqn = resolve::resolve_simple_to_fqn(name, ctx)?;
    external_type_target(&fqn, ctx)
}

/// Hover for an external type itself: `fqn<TypeParams>` plus its
/// type-level Javadoc. The `class`/`interface` keyword isn't shown, since
/// it isn't modeled at signature level.
fn external_type_target<'t>(fqn: &str, ctx: &Ctx<'_, 't>) -> Option<Target<'t>> {
    let class = ctx.symbols.class(fqn)?;
    let params = if class.type_params.is_empty() {
        String::new()
    } else {
        format!("<{}>", class.type_params.join(", "))
    };
    Some(Target::External(
        format!("{fqn}{params}"),
        ctx.symbols.doc(fqn, None),
    ))
}

/// Hover for a `var` local or Java 21 pattern binding, whose declaration
/// has no signature of its own. Renders as the resolved type plus name
/// (`Widget w`), fenced, no Javadoc; `None` otherwise so the caller falls
/// through to normal handling.
fn local_var_target<'t>(name: &str, byte: usize, ctx: &Ctx<'_, 't>) -> Option<Target<'t>> {
    let ty = resolve::inferred_var_type_display(name, byte, ctx)
        .or_else(|| resolve::inferred_lambda_parameter_type_display(name, byte, ctx))
        .or_else(|| resolve::pattern_binding_type_display(name, byte, ctx))?;
    Some(Target::External(format!("{ty} {name}"), None))
}

fn inproject_target<'t>(resolved: &Resolved<'t>) -> Option<Target<'t>> {
    match &resolved.ty {
        ResolvedType::InProject { decl: td, .. } => {
            Some(Target::InProject(td.node, td.source, None))
        }
        ResolvedType::External { .. }
        | ResolvedType::Primitive(_)
        | ResolvedType::Void
        | ResolvedType::Null
        | ResolvedType::Array { .. } => None,
    }
}

fn member_target<'t>(resolved: &Resolved<'t>, ctx: &Ctx<'_, 't>, name: &str) -> Option<Target<'t>> {
    match resolve::find_member_hier(resolved, ctx, name)? {
        HierMember::InProject(m) => {
            let inherited = javadoc(m.node, m.source)
                .is_none()
                .then(|| inherited_member_doc(m.node, m.doc, ctx, name))
                .flatten();
            Some(Target::InProject(m.node, m.source, inherited))
        }
        HierMember::External(m) => {
            // Javadoc only when the receiver itself is external (we have its FQN).
            let doc = match &resolved.ty {
                ResolvedType::External { fqn, .. } => ctx.symbols.doc(fqn, Some(name)),
                ResolvedType::InProject { .. }
                | ResolvedType::Primitive(_)
                | ResolvedType::Void
                | ResolvedType::Null
                | ResolvedType::Array { .. } => None,
            };
            Some(Target::External(m.signature, doc))
        }
    }
}

/// The Javadoc a member would inherit: walks the declaring type's
/// supertype chain for a same-named member's doc, first hit wins.
/// Depth-capped so a hostile hierarchy can't stall a hover.
fn inherited_member_doc(
    member_node: Node,
    member_doc: usize,
    ctx: &Ctx,
    name: &str,
) -> Option<String> {
    let type_node = resolve::enclosing_type_node(member_node)?;
    let td = ctx.table.by_node(member_doc, type_node.id())?;
    let dctx = ctx.for_document(td.doc)?;
    let mut queue: Vec<ResolvedType> = td
        .super_nodes
        .iter()
        .filter_map(|n| resolve::resolve_type_node(*n, td.source, &dctx))
        .collect();
    let mut budget = 32usize;
    while let Some(resolved) = queue.pop() {
        if budget == 0 {
            return None;
        }
        budget -= 1;
        match resolved {
            ResolvedType::InProject { decl: sd, .. } => {
                if let Some(m) = sd.own_members().into_iter().find(|m| m.name == name) {
                    if let Some(doc) = javadoc(m.node, m.source) {
                        return Some(doc);
                    }
                }
                if let Some(sdctx) = ctx.for_document(sd.doc) {
                    queue.extend(
                        sd.super_nodes
                            .iter()
                            .filter_map(|n| resolve::resolve_type_node(*n, sd.source, &sdctx)),
                    );
                }
            }
            ResolvedType::External { fqn, .. } => {
                if let Some(doc) = ctx.symbols.doc(&fqn, Some(name)) {
                    return Some(doc);
                }
            }
            _ => {}
        }
    }
    None
}

/// Walks up from `name_node` through type-node wrappers (`generic_type`,
/// `scoped_type_identifier`, `annotated_type`) to the enclosing `new`
/// expression, if `name_node` is part of its `type` field; `None` otherwise.
fn enclosing_object_creation(name_node: Node) -> Option<Node> {
    let mut node = name_node;
    loop {
        let parent = node.parent()?;
        match parent.kind() {
            "object_creation_expression" if parent.child_by_field_name("type") == Some(node) => {
                return Some(parent);
            }
            "generic_type" | "scoped_type_identifier" | "annotated_type" => node = parent,
            _ => return None,
        }
    }
}

/// Number of arguments written at a call site (`new Foo(1, 2)` -> 2),
/// counting only the object-creation's own `argument_list`, not a nested
/// call's.
fn call_arg_count(call: Node) -> usize {
    named_children(call)
        .into_iter()
        .find(|c| c.kind() == "argument_list")
        .map(|args| {
            named_children(args)
                .into_iter()
                .filter(|c| !matches!(c.kind(), "line_comment" | "block_comment"))
                .count()
        })
        .unwrap_or(0)
}

/// Hover for the type name in `new Foo(...)`: the best-matching
/// constructor's signature + Javadoc, chosen by argument-count arity
/// (ties favor the first declared, like `signature_help`'s heuristic).
/// Falls back to the first constructor, or a synthesized `Foo()` plus
/// class-level Javadoc when none is declared.
fn constructor_target<'t>(call: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Target<'t>> {
    let resolved_ty = resolve::resolve_object_creation_type(call, ctx)?;
    let arg_count = call_arg_count(call);
    match resolved_ty {
        // Only class types can have constructors.
        ResolvedType::Primitive(_)
        | ResolvedType::Void
        | ResolvedType::Null
        | ResolvedType::Array { .. } => None,
        ResolvedType::InProject { decl: td, .. } => {
            let ctors = td.constructors();
            let (sig, ctor_doc) = if ctors.is_empty() {
                (format!("{}()", td.name), None)
            } else {
                let chosen = ctors
                    .iter()
                    .find(|&&n| param_labels(n, td.source).len() == arg_count)
                    .copied()
                    .unwrap_or(ctors[0]);
                (signature(chosen, td.source)?, javadoc(chosen, td.source))
            };
            // No Javadoc of its own falls back to the class-level Javadoc
            // (also the only doc a synthesized default constructor can show).
            let doc = ctor_doc.or_else(|| javadoc(td.node, td.source));
            Some(Target::External(sig, doc))
        }
        ResolvedType::External { fqn, args } => {
            let class = ctx.symbols.class(&fqn)?;
            let simple = fqn
                .rsplit('.')
                .next()
                .and_then(|s| s.rsplit('$').next())
                .unwrap_or(&fqn)
                .to_string();
            let ctor_members: Vec<_> = class
                .members
                .into_iter()
                .filter(|m| m.kind == ExternalMemberKind::Constructor)
                .collect();
            if ctor_members.is_empty() {
                let sig = format!("{simple}()");
                let doc = ctx.symbols.doc(&fqn, None);
                return Some(Target::External(sig, doc));
            }
            let chosen = ctor_members
                .iter()
                .find(|m| param_count_in_label(&m.signature) == arg_count)
                .unwrap_or(&ctor_members[0]);
            let arg_strings: Vec<String> = args.iter().map(render_type_ref).collect();
            let sig = resolve::display_signature(chosen, &arg_strings, &class.type_params);
            // External constructor Javadoc is keyed by the class's simple
            // name (source archives have no `<init>`); falls back to the
            // class-level doc when there's none.
            let doc = ctx
                .symbols
                .doc(&fqn, Some(&simple))
                .or_else(|| ctx.symbols.doc(&fqn, None));
            Some(Target::External(sig, doc))
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
#[path = "hover_tests.rs"]
mod tests;
