//! Conservative semantic diagnostics: resolvable member accesses, method
//! returns, variable/field initializers, reassignments, unreachable
//! statements, and unused
//! code. Resolution/analysis must be complete before a diagnostic is emitted,
//! so unknown project/classpath types, overloads, and recovery regions stay
//! silent.

use std::collections::{HashMap, HashSet};

use ls_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag, Location,
    NumberOrString, Uri,
};
use tree_sitter::Node;

use crate::external::SymbolSource;
use crate::imports::Imports;
use crate::model::{has_modifier, named_children, TypeKind, TypeTable};
use crate::resolve::{self, Ctx, FactsCache, ResolvedType};
use crate::{diagnostic, node_text, LineIndex, OpenDoc, MAX_DIAGNOSTICS};

/// Stable LSP code for a proven incompatible method return.
pub const INCOMPATIBLE_RETURN_CODE: &str = "jvl.incompatibleReturn";

/// Stable LSP code for a proven incompatible variable/field initializer or
/// reassignment.
pub const INCOMPATIBLE_ASSIGNMENT_CODE: &str = "jvl.incompatibleAssignment";

/// Stable LSP code for a statement that can never execute.
pub const UNREACHABLE_CODE: &str = "jvl.unreachable";

/// Stable LSP code for a provably undeclared variable/field reference.
pub const CANNOT_FIND_SYMBOL_CODE: &str = "jvl.cannotFindSymbol";

/// Stable LSP code for unused locals, parameters, and private members.
const UNUSED_CODE: &str = "jvl.unused";

/// Stable LSP code for a proven no-applicable-overload method call.
pub const INVALID_INVOCATION_CODE: &str = "jvl.invalidInvocation";

/// Stable LSP code for a proven uninstantiable/inaccessible constructor call.
pub const INVALID_INSTANTIATION_CODE: &str = "jvl.invalidInstantiation";

/// Immediate semantic diagnostics for `docs[current]`.
///
/// Return, initializer, and unreachable checks always run.
/// `unresolved_members` gates only the unresolved-member rule and `unused`
/// only the unused-code rule (both default-on init options, opt-out only).
pub fn semantic_diagnostics(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    uri: &Uri,
    symbols: &dyn SymbolSource,
    unresolved_members: bool,
    unused: bool,
) -> Vec<Diagnostic> {
    let Some(doc) = docs.get(current) else {
        return Vec::new();
    };
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

    let mut out = Vec::new();
    let root = doc.tree.root_node();
    let name_counts = unused.then(|| identifier_counts(root, doc.source));
    // Needs a clean parse (recovery can reattach identifiers anywhere) and
    // static-import info, since static imports can bind any bare name.
    let static_imports =
        (unresolved_members && !root.has_error()).then(|| StaticImports::parse(root, doc.source));
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }

        match node.kind() {
            "return_statement" => check_return(node, &ctx, index, uri, &mut out),
            "assignment_expression" => check_assignment(node, &ctx, index, uri, &mut out),
            "local_variable_declaration" => {
                check_initializers(node, &ctx, index, &mut out);
                if unused {
                    check_unused_locals(node, doc.source, index, &mut out);
                }
            }
            "field_declaration" => {
                check_initializers(node, &ctx, index, &mut out);
                if let Some(name_counts) = &name_counts {
                    check_unused_private_fields(
                        node,
                        root,
                        name_counts,
                        doc.source,
                        index,
                        &mut out,
                    );
                }
            }
            "block" => check_unreachable(node, index, &mut out),
            "method_declaration" => {
                if let Some(name_counts) = &name_counts {
                    check_unused_method(node, root, name_counts, doc.source, index, &mut out);
                }
            }
            "constructor_declaration" if unused => {
                check_unused_parameters(node, doc.source, index, &mut out)
            }
            "field_access" if unresolved_members => {
                check_member(node, "field", "field", &ctx, index, &mut out)
            }
            // Qualified calls check member existence first; applicability
            // runs only if that found nothing. Unqualified calls have no
            // receiver, so applicability is the only check.
            "method_invocation" if unresolved_members => {
                let before = out.len();
                if node.child_by_field_name("object").is_some() {
                    check_member(node, "name", "method", &ctx, index, &mut out);
                }
                if out.len() == before {
                    check_invocation(node, &ctx, index, &mut out);
                }
            }
            "object_creation_expression" if unresolved_members => {
                check_instantiation(node, &ctx, index, &mut out)
            }
            "identifier" => {
                if let Some(static_imports) = &static_imports {
                    check_unresolved_identifier(node, &ctx, static_imports, index, &mut out);
                }
            }
            _ => {}
        }
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }

        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out
}

fn check_member(
    node: Node,
    member_field: &str,
    kind_word: &str,
    ctx: &Ctx,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let (Some(object), Some(member_node)) = (
        node.child_by_field_name("object"),
        node.child_by_field_name(member_field),
    ) else {
        return;
    };
    if member_node.kind() != "identifier" {
        return; // mid-edit / missing member
    }
    let member = node_text(member_node, ctx.doc.source);

    // A type-cased segment (`Map.Entry`) is a nested type or static-member
    // reference, not an instance member — never flag it. camelCase members
    // and SCREAMING_CASE constants still get checked.
    if member_field == "field" && crate::looks_like_type_name(member) {
        return;
    }

    // Receiver type must resolve; otherwise we cannot know its members.
    let Some(resolved) = resolve::resolve_receiver_type(object, ctx) else {
        return;
    };
    // `java.lang.Object` here is almost always an erased generic-inference
    // fallback, not a genuine `Object` value, so flagging its members would
    // flood ordinary generic code with false positives.
    if matches!(&resolved.ty, ResolvedType::External { fqn, .. } if fqn == "java.lang.Object") {
        return;
    }

    let (names, complete) = resolve::member_names(&resolved, ctx);
    if complete && !names.contains(member) {
        out.push(diagnostic(
            index.range(member_node),
            format!("Cannot resolve {kind_word} '{member}'"),
        ));
    }
}

/// Argument-applicability check for a call whose member existence
/// [`check_member`] already resolved. Malformed argument lists stay silent,
/// but a missing `name`/`arguments` field needs its own guard here.
fn check_invocation<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if call.has_error() || call.parent().is_some_and(|p| p.has_error()) {
        return;
    }
    let (Some(name), Some(args)) = (
        call.child_by_field_name("name"),
        call.child_by_field_name("arguments"),
    ) else {
        return;
    };
    match crate::call::resolve_method_call(call, ctx) {
        crate::call::CallResolution::NoApplicable { candidates } => out.push(coded_diagnostic(
            index.range(args),
            INVALID_INVOCATION_CODE,
            format!(
                "no applicable method '{}' for argument types ({}); {candidates} candidate(s) considered",
                node_text(name, ctx.doc.source),
                argument_display(args, ctx)
            ),
        )),
        crate::call::CallResolution::Ambiguous => out.push(coded_diagnostic(
            index.range(name),
            INVALID_INVOCATION_CODE,
            format!("ambiguous method call '{}'", node_text(name, ctx.doc.source)),
        )),
        crate::call::CallResolution::Selected { .. } | crate::call::CallResolution::Unknown => {}
    }
}

/// Instantiability + argument-applicability check for `new Type(args)`.
fn check_instantiation<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if call.has_error() || call.parent().is_some_and(|p| p.has_error()) {
        return;
    }
    let (Some(ty), Some(args)) = (
        call.child_by_field_name("type"),
        call.child_by_field_name("arguments"),
    ) else {
        return;
    };
    let shown = node_text(ty, ctx.doc.source)
        .trim_end_matches("<>")
        .to_string();
    use crate::call::{CallResolution as R, InstantiationError as E};
    match crate::call::resolve_constructor_call(call, ctx) {
        Ok(R::NoApplicable { candidates }) => out.push(coded_diagnostic(
            index.range(args),
            INVALID_INSTANTIATION_CODE,
            format!(
                "no applicable constructor '{shown}' for argument types ({}); {candidates} candidate(s) considered",
                argument_display(args, ctx)
            ),
        )),
        Ok(R::Ambiguous) => out.push(coded_diagnostic(
            index.range(ty),
            INVALID_INSTANTIATION_CODE,
            format!("ambiguous constructor call '{shown}'"),
        )),
        Err(E::Abstract(kind)) => out.push(coded_diagnostic(
            index.range(ty),
            INVALID_INSTANTIATION_CODE,
            format!(
                "cannot instantiate {} '{shown}'",
                match kind {
                    jvl_types::ClassKind::Interface => "interface",
                    jvl_types::ClassKind::Annotation => "annotation",
                    jvl_types::ClassKind::Enum => "enum",
                    _ => "abstract class",
                }
            ),
        )),
        Err(E::Inaccessible) => out.push(coded_diagnostic(
            index.range(ty),
            INVALID_INSTANTIATION_CODE,
            format!("constructor '{shown}' is not accessible"),
        )),
        Err(E::NeedsEnclosingInstance) => out.push(coded_diagnostic(
            index.range(ty),
            INVALID_INSTANTIATION_CODE,
            format!("enclosing instance required for '{shown}'"),
        )),
        Ok(R::Selected { .. }) | Ok(R::Unknown) => {}
    }
}

/// Render each argument expression's resolved type (`?` when unresolvable)
/// for a "no applicable overload" message.
fn argument_display(args: Node, ctx: &Ctx<'_, '_>) -> String {
    named_children(args)
        .into_iter()
        .map(|a| {
            resolve::resolve_expression_type(a, ctx)
                .map(|t| resolve::type_display(&t))
                .unwrap_or_else(|| "?".into())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Static-import context for the unresolved-identifier rule: a static
/// import can bind any bare name (a constant), so an identifier matching a
/// static single import — or any file with a static wildcard import — must
/// stay silent.
struct StaticImports {
    names: HashSet<String>,
    wildcard: bool,
}

impl StaticImports {
    fn parse(root: Node, source: &str) -> StaticImports {
        let mut names = HashSet::new();
        let mut wildcard = false;
        for child in named_children(root) {
            if child.kind() != "import_declaration" {
                continue;
            }
            let Some(path) = crate::imports::dotted_path(node_text(child, source), "import") else {
                continue;
            };
            // `dotted_path` collapses whitespace, so a static import arrives
            // as `statica.b.C.member`; peel the keyword off here.
            let Some(path) = path.strip_prefix("static") else {
                continue;
            };
            if path.ends_with(".*") {
                wildcard = true;
            } else if let Some(simple) = path.rsplit('.').next() {
                names.insert(simple.to_string());
            }
        }
        StaticImports { names, wildcard }
    }

    fn may_bind(&self, name: &str) -> bool {
        self.wildcard || self.names.contains(name)
    }
}

/// Whether `id` occupies a position where only a variable/field *value* is
/// legal Java. Receiver/member/type/label/declaration positions are
/// excluded — a class or member name would be valid there, and the type
/// namespace is not this rule's to judge.
fn is_variable_only_position(id: Node) -> bool {
    let Some(parent) = id.parent() else {
        return false;
    };
    let in_field = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|n| n.id() == id.id())
    };
    match parent.kind() {
        "argument_list"
        | "binary_expression"
        | "unary_expression"
        | "update_expression"
        | "parenthesized_expression"
        | "array_initializer"
        | "array_access"
        | "assignment_expression"
        | "ternary_expression"
        | "return_statement"
        | "throw_statement"
        | "yield_statement" => true,
        "variable_declarator" | "enhanced_for_statement" | "cast_expression" => in_field("value"),
        "instanceof_expression" => in_field("left"),
        "lambda_expression" => in_field("body"),
        _ => false,
    }
}

/// The name bound by a `catch` clause's parameter, if any.
fn catch_param_name<'t>(catch_clause: Node<'t>, source: &'t str) -> Option<&'t str> {
    let parameter = named_children(catch_clause)
        .into_iter()
        .find(|c| c.kind() == "catch_formal_parameter")?;
    if let Some(name) = parameter.child_by_field_name("name") {
        return Some(node_text(name, source));
    }
    // Fallback for grammar variants without a `name` field: the last direct
    // identifier child is the binding.
    named_children(parameter)
        .into_iter()
        .rev()
        .find(|c| c.kind() == "identifier")
        .map(|n| node_text(n, source))
}

/// Whether a try-with-resources statement binds `name` as a resource.
fn binds_resource(try_statement: Node, name: &str, source: &str) -> bool {
    named_children(try_statement)
        .into_iter()
        .filter(|c| c.kind() == "resource_specification")
        .flat_map(named_children)
        .filter(|c| c.kind() == "resource")
        .filter_map(|resource| resource.child_by_field_name("name"))
        .any(|n| node_text(n, source) == name)
}

/// Unresolved-identifier rule (`jvl.cannotFindSymbol`): flags a bare
/// identifier with no local/parameter/pattern/binding, static import, or
/// resolvable member match. Anything unprovable stays silent.
fn check_unresolved_identifier<'t>(
    id: Node<'t>,
    ctx: &Ctx<'_, 't>,
    static_imports: &StaticImports,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if !is_variable_only_position(id) {
        return;
    }
    let name = node_text(id, ctx.doc.source);
    if name == "_" || static_imports.may_bind(name) {
        return;
    }
    // Same position-aware lookup hover/completion use, so scoping matches
    // Java (a local is invisible before its declaration or outside its block).
    if resolve::lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        id.start_byte(),
        name,
        ctx.table,
        ctx.current,
    )
    .is_some()
    {
        return;
    }
    // Bindings the shared lookup doesn't model, plus contexts where absence
    // can't be proven — one walk up the ancestor chain.
    let mut enclosing_types: Vec<Node<'t>> = Vec::new();
    let mut node = id;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "catch_clause" => {
                if catch_param_name(parent, ctx.doc.source) == Some(name) {
                    return;
                }
            }
            "try_with_resources_statement" => {
                if binds_resource(parent, name, ctx.doc.source) {
                    return;
                }
            }
            // An anonymous class body inherits fields from its created
            // type; proving absence there isn't worth the complexity.
            "object_creation_expression"
                if named_children(parent)
                    .iter()
                    .any(|c| c.kind() == "class_body") =>
            {
                return;
            }
            _ => {}
        }
        if TypeKind::from_kind(parent.kind()).is_some() {
            enclosing_types.push(parent);
        }
        node = parent;
    }
    if enclosing_types.is_empty() {
        return;
    }
    // Every enclosing type's full hierarchy must be enumerable before
    // absence is proven — an unresolvable supertype could declare the
    // field. Method names count too, so a same-named method stays silent.
    for type_node in enclosing_types {
        let Some(td) = ctx.table.by_node(ctx.current, type_node.id()).cloned() else {
            return;
        };
        let resolved = resolve::Resolved {
            ty: ResolvedType::InProject {
                decl: td,
                args: Vec::new(),
            },
            static_only: false,
        };
        let (names, complete) = resolve::member_names(&resolved, ctx);
        if !complete || names.contains(name) {
            return;
        }
    }
    out.push(coded_diagnostic(
        index.range(id),
        CANNOT_FIND_SYMBOL_CODE,
        format!("cannot find symbol: variable '{name}'"),
    ));
}

fn check_return<'t>(
    return_statement: Node<'t>,
    ctx: &Ctx<'_, 't>,
    index: &LineIndex,
    uri: &Uri,
    out: &mut Vec<Diagnostic>,
) {
    let Some(method) = nearest_method(return_statement) else {
        return;
    };
    if method.has_error() {
        return;
    }
    let Some(return_type) = method.child_by_field_name("type") else {
        return;
    };
    let expression = {
        let mut cursor = return_statement.walk();
        let expression = return_statement
            .named_children(&mut cursor)
            .find(|child| !matches!(child.kind(), "line_comment" | "block_comment"));
        expression
    };
    let returns_void = node_text(return_type, ctx.doc.source).trim() == "void";
    // Presentation metadata: a clickable link to the declared return type.
    let related = method.child_by_field_name("name").map(|name| {
        declared_here(
            uri,
            index.range(return_type),
            format!(
                "method '{}' declared to return '{}' here",
                node_text(name, ctx.doc.source),
                node_text(return_type, ctx.doc.source).trim()
            ),
        )
    });

    let mut push = |diagnostic: Diagnostic| {
        let mut diagnostic = diagnostic;
        diagnostic.related_information = related.clone();
        out.push(diagnostic);
    };
    match (returns_void, expression) {
        (true, Some(_)) => push(coded_diagnostic(
            index.range(return_statement),
            INCOMPATIBLE_RETURN_CODE,
            "incompatible types: unexpected return value".to_string(),
        )),
        (false, None) => push(coded_diagnostic(
            index.range(return_statement),
            INCOMPATIBLE_RETURN_CODE,
            "incompatible types: missing return value".to_string(),
        )),
        (true, None) => {}
        (false, Some(expression)) => {
            let Some(expected) = resolve::resolve_type_node(return_type, ctx.doc.source, ctx)
            else {
                return;
            };
            let Some(actual) = resolve::resolve_expression_type(expression, ctx) else {
                return;
            };
            if resolve::is_assignable(&actual, &expected, ctx) == Some(false) {
                push(coded_diagnostic(
                    index.range(expression),
                    INCOMPATIBLE_RETURN_CODE,
                    format!(
                        "incompatible types: {} cannot be converted to {}",
                        resolve::type_display(&actual),
                        resolve::type_display(&expected)
                    ),
                ));
            }
        }
    }
}

/// A one-entry `related_information` list pointing at a same-document
/// declaration site.
fn declared_here(
    uri: &Uri,
    range: ls_types::Range,
    message: String,
) -> Vec<DiagnosticRelatedInformation> {
    vec![DiagnosticRelatedInformation {
        location: Location {
            uri: uri.clone(),
            range,
        },
        message,
    }]
}

fn nearest_method<'t>(return_statement: Node<'t>) -> Option<Node<'t>> {
    let mut ancestor = return_statement.parent();
    while let Some(node) = ancestor {
        match node.kind() {
            "method_declaration" => return Some(node),
            "lambda_expression" | "constructor_declaration" | "compact_constructor_declaration" => {
                return None
            }
            _ => ancestor = node.parent(),
        }
    }
    None
}

/// [`diagnostic`] with a stable LSP `code` attached.
fn coded_diagnostic(range: ls_types::Range, code: &str, message: String) -> Diagnostic {
    let mut diagnostic = diagnostic(range, message);
    diagnostic.code = Some(NumberOrString::String(code.to_string()));
    diagnostic
}

/// A `jvl.unused` warning: WARNING severity plus the Unnecessary tag so
/// clients fade the range instead of squiggling it.
fn unused_diagnostic(range: ls_types::Range, message: String) -> Diagnostic {
    let mut diagnostic = coded_diagnostic(range, UNUSED_CODE, message);
    diagnostic.severity = Some(DiagnosticSeverity::WARNING);
    diagnostic.tags = Some(vec![DiagnosticTag::UNNECESSARY]);
    diagnostic
}

/// Incompatible-initializer rule: for each declarator with a `value` inside a
/// local variable or field declaration, flag a proven mismatch between the
/// declared type and the initializer's type. Unknown resolution (`None`)
/// stays silent — only `Some(false)` flags — and recovery inside the
/// declaration mutes it (`has_error` propagates from ERROR/MISSING
/// descendants). `var` has no declared type to check.
fn check_initializers<'t>(
    declaration: Node<'t>,
    ctx: &Ctx<'_, 't>,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if declaration.has_error() {
        return;
    }
    let Some(type_node) = declaration.child_by_field_name("type") else {
        return;
    };
    if node_text(type_node, ctx.doc.source) == "var" {
        return;
    }
    let Some(expected) = resolve::resolve_type_node(type_node, ctx.doc.source, ctx) else {
        return;
    };
    let mut cursor = declaration.walk();
    for declarator in declaration.children_by_field_name("declarator", &mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        let Some(value) = declarator.child_by_field_name("value") else {
            continue;
        };
        let Some(actual) = resolve::resolve_expression_type(value, ctx) else {
            continue;
        };
        if resolve::is_assignable(&actual, &expected, ctx) == Some(false) {
            out.push(coded_diagnostic(
                index.range(value),
                INCOMPATIBLE_ASSIGNMENT_CODE,
                format!(
                    "incompatible types: {} cannot be converted to {}",
                    resolve::type_display(&actual),
                    resolve::type_display(&expected)
                ),
            ));
        }
    }
}

/// Incompatible-reassignment rule: for a plain `=` assignment whose LHS and
/// RHS types both resolve, flag a proven mismatch. Compound operators
/// (`+=` …) carry implicit-cast semantics and stay silent, as do unknown
/// types (`None` from resolution) and recovery regions.
fn check_assignment<'t>(
    assignment: Node<'t>,
    ctx: &Ctx<'_, 't>,
    index: &LineIndex,
    uri: &Uri,
    out: &mut Vec<Diagnostic>,
) {
    // A MISSING `;` after the expression lands on the enclosing statement,
    // not the assignment node itself, so recovery is checked one level up.
    if assignment.has_error() || assignment.parent().is_some_and(|parent| parent.has_error()) {
        return;
    }
    let (Some(operator), Some(left), Some(right)) = (
        assignment.child_by_field_name("operator"),
        assignment.child_by_field_name("left"),
        assignment.child_by_field_name("right"),
    ) else {
        return;
    };
    if node_text(operator, ctx.doc.source) != "=" {
        return;
    }
    let Some(expected) = resolve::resolve_expression_type(left, ctx) else {
        return;
    };
    let Some(actual) = resolve::resolve_expression_type(right, ctx) else {
        return;
    };
    if resolve::is_assignable(&actual, &expected, ctx) == Some(false) {
        let mut diagnostic = coded_diagnostic(
            index.range(right),
            INCOMPATIBLE_ASSIGNMENT_CODE,
            format!(
                "incompatible types: {} cannot be converted to {}",
                resolve::type_display(&actual),
                resolve::type_display(&expected)
            ),
        );
        // Presentation metadata only: on a lookup miss the diagnostic ships
        // without the link (silence over wrong links).
        if let Some((name_node, type_node)) = lhs_declaration(assignment, left, ctx.doc.source) {
            diagnostic.related_information = Some(declared_here(
                uri,
                index.range(name_node),
                format!(
                    "'{}' declared as '{}' here",
                    node_text(name_node, ctx.doc.source),
                    node_text(type_node, ctx.doc.source).trim()
                ),
            ));
        }
        out.push(diagnostic);
    }
}

/// The declaration `name`/`type` nodes for a flagged assignment's LHS:
/// nearest local/parameter declaration, else the enclosing class's field.
/// `this.f` searches fields only, so a same-named local can't win.
fn lhs_declaration<'t>(
    assignment: Node<'t>,
    left: Node<'t>,
    source: &str,
) -> Option<(Node<'t>, Node<'t>)> {
    let (name, fields_only) = match left.kind() {
        "identifier" => (node_text(left, source), false),
        "field_access" => {
            let object = left.child_by_field_name("object")?;
            let field = left.child_by_field_name("field")?;
            if object.kind() != "this" || field.kind() != "identifier" {
                return None;
            }
            (node_text(field, source), true)
        }
        _ => return None,
    };
    let at = assignment.start_byte();
    let mut current = assignment;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "block" | "constructor_body" if !fields_only => {
                // Nearest preceding declaration in this block wins.
                let mut nearest = None;
                for statement in named_children(parent) {
                    if statement.start_byte() >= at {
                        break;
                    }
                    if statement.kind() != "local_variable_declaration" {
                        continue;
                    }
                    let Some(ty) = statement.child_by_field_name("type") else {
                        continue;
                    };
                    let mut cursor = statement.walk();
                    for declarator in statement.children_by_field_name("declarator", &mut cursor) {
                        if let Some(name_node) = declarator.child_by_field_name("name") {
                            if node_text(name_node, source) == name {
                                nearest = Some((name_node, ty));
                            }
                        }
                    }
                }
                if nearest.is_some() {
                    return nearest;
                }
            }
            "method_declaration" | "constructor_declaration" if !fields_only => {
                if let Some(parameters) = parent.child_by_field_name("parameters") {
                    for parameter in named_children(parameters) {
                        if parameter.kind() != "formal_parameter" {
                            continue;
                        }
                        if let (Some(name_node), Some(ty)) = (
                            parameter.child_by_field_name("name"),
                            parameter.child_by_field_name("type"),
                        ) {
                            if node_text(name_node, source) == name {
                                return Some((name_node, ty));
                            }
                        }
                    }
                }
            }
            "class_body" => {
                for member in named_children(parent) {
                    if member.kind() != "field_declaration" {
                        continue;
                    }
                    let Some(ty) = member.child_by_field_name("type") else {
                        continue;
                    };
                    for declarator in named_children(member) {
                        if declarator.kind() != "variable_declarator" {
                            continue;
                        }
                        if let Some(name_node) = declarator.child_by_field_name("name") {
                            if node_text(name_node, source) == name {
                                return Some((name_node, ty));
                            }
                        }
                    }
                }
                // Stop at the nearest class — an outer class's same-named
                // field would be a wrong link.
                return None;
            }
            _ => {}
        }
        current = parent;
    }
    None
}

/// Check unreachable statements in plain blocks only.
/// Recovery silences the rule, and each block emits at most one warning.
fn check_unreachable(block: Node, index: &LineIndex, out: &mut Vec<Diagnostic>) {
    if block.has_error() {
        return;
    }
    let mut cursor = block.walk();
    let mut terminated = false;
    for statement in block.named_children(&mut cursor) {
        if matches!(statement.kind(), "line_comment" | "block_comment") {
            continue;
        }
        if terminated {
            out.push(coded_diagnostic(
                index.range(statement),
                UNREACHABLE_CODE,
                "unreachable statement".to_string(),
            ));
            return;
        }
        terminated = !can_complete_normally(statement);
    }
}

/// Conservative subset of JLS §14.22 normal-completion analysis. Only direct
/// abrupt statements, blocks, and `try` forms are modeled; every other shape
/// returns `true` so unsupported control flow can only suppress a warning.
fn can_complete_normally(statement: Node) -> bool {
    match statement.kind() {
        "return_statement" | "throw_statement" | "break_statement" | "continue_statement" => false,
        "block" | "constructor_body" => block_can_complete_normally(statement),
        "try_statement" | "try_with_resources_statement" => try_can_complete_normally(statement),
        _ => true,
    }
}

fn block_can_complete_normally(block: Node) -> bool {
    let mut cursor = block.walk();
    for statement in block
        .named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
    {
        if !can_complete_normally(statement) {
            return false;
        }
    }
    true
}

/// A `try` completes normally iff its body or any catch does, and its
/// `finally` (if present) also does. `finally` is still walked separately
/// by [`check_unreachable`], even with a pending return/throw.
fn try_can_complete_normally(statement: Node) -> bool {
    let Some(body) = statement.child_by_field_name("body") else {
        return true;
    };
    let mut body_or_catch = block_can_complete_normally(body);
    let mut finally_can_complete = true;
    let mut cursor = statement.walk();
    for child in statement.named_children(&mut cursor) {
        match child.kind() {
            "catch_clause" => {
                if let Some(body) = child.child_by_field_name("body") {
                    body_or_catch |= block_can_complete_normally(body);
                }
            }
            "finally_clause" => {
                let mut finally_cursor = child.walk();
                let finally_block = child
                    .named_children(&mut finally_cursor)
                    .find(|node| node.kind() == "block");
                if let Some(block) = finally_block {
                    finally_can_complete = block_can_complete_normally(block);
                }
            }
            _ => {}
        }
    }
    body_or_catch && finally_can_complete
}

/// Counts identifier spellings file-wide for the unused-private-member check.
/// A count above one means another occurrence exists; shadowing can only
/// raise the count, never cause a false unused warning.
fn identifier_counts<'a>(root: Node, source: &'a str) -> HashMap<&'a str, usize> {
    let mut counts = HashMap::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" {
            *counts.entry(node_text(node, source)).or_default() += 1;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    counts
}

/// Whether `name` occurs as an `identifier` anywhere under `scope`, other
/// than at its own declaration node. Purely occurrence-based, so shadowing
/// can only cause silence, never a false warning.
fn name_used(scope: Node, name_node: Node, name: &str, source: &str) -> bool {
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier"
            && node.id() != name_node.id()
            && node_text(node, source) == name
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    false
}

/// The body a local's usage scan covers: the enclosing method/constructor
/// body, or a static/instance initializer block. A lambda-local resolves to
/// the enclosing method body, a superset scope that can only cause silence.
fn local_scope<'t>(declaration: Node<'t>) -> Option<Node<'t>> {
    let mut node = declaration;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "method_declaration"
            | "constructor_declaration"
            | "static_initializer"
            | "class_body" => {
                return matches!(node.kind(), "block" | "constructor_body").then_some(node);
            }
            _ => node = parent,
        }
    }
    None
}

/// Whether the declaration carries any annotation (marker or full form).
fn has_annotation(declaration: Node) -> bool {
    let Some(modifiers) = crate::model::modifiers_node(declaration) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    let found = modifiers
        .children(&mut cursor)
        .any(|m| matches!(m.kind(), "annotation" | "marker_annotation"));
    found
}

/// Whether the declaration carries `@Override` specifically.
fn has_override(declaration: Node, source: &str) -> bool {
    let Some(modifiers) = crate::model::modifiers_node(declaration) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    let found = modifiers.children(&mut cursor).any(|m| {
        matches!(m.kind(), "annotation" | "marker_annotation")
            && m.child_by_field_name("name")
                .and_then(|name| node_text(name, source).rsplit('.').next())
                == Some("Override")
    });
    found
}

/// Unused-local rule: a declarator name with no other identifier occurrence
/// in the enclosing method (or initializer block) is dead. Recovery anywhere
/// in that scope mutes the check.
fn check_unused_locals(
    declaration: Node,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(scope) = local_scope(declaration) else {
        return;
    };
    if scope.has_error() {
        return;
    }
    let mut cursor = declaration.walk();
    for declarator in declaration.children_by_field_name("declarator", &mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if !name_used(scope, name_node, name, source) {
            out.push(unused_diagnostic(
                index.range(name_node),
                format!("unused local variable '{name}'"),
            ));
        }
    }
}

/// Unused-private-field rule: the usage scan covers the whole file, so
/// recovery anywhere in it mutes the check. Annotated members and
/// `serialVersionUID` (a reflective serialization contract) are exempt.
fn check_unused_private_fields(
    field: Node,
    root: Node,
    name_counts: &HashMap<&str, usize>,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if root.has_error() {
        return;
    }
    if !has_modifier(field, source, "private") || has_annotation(field) {
        return;
    }
    let mut cursor = field.walk();
    for declarator in field.children_by_field_name("declarator", &mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if name == "serialVersionUID" {
            continue;
        }
        if name_counts.get(name).copied().unwrap_or(0) == 1 {
            out.push(unused_diagnostic(
                index.range(name_node),
                format!("unused private field '{name}'"),
            ));
        }
    }
}

/// Unused checks for a `method_declaration`: an unreferenced private method
/// (any annotation exempts it), and unused parameters on `private`/`static`
/// methods. `@Override` and `main` keep their parameters as external contracts.
fn check_unused_method(
    method: Node,
    root: Node,
    name_counts: &HashMap<&str, usize>,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = method.child_by_field_name("name") else {
        return;
    };
    let name = node_text(name_node, source);
    let is_private = has_modifier(method, source, "private");

    if is_private
        && !has_annotation(method)
        && !root.has_error()
        && name_counts.get(name).copied().unwrap_or(0) == 1
    {
        out.push(unused_diagnostic(
            index.range(name_node),
            format!("unused private method '{name}'"),
        ));
    }

    if (is_private || has_modifier(method, source, "static"))
        && !has_override(method, source)
        && name != "main"
    {
        check_unused_parameters(method, source, index, out);
    }
}

/// Unused-parameter scan for a method/constructor with a body: a parameter
/// never referenced in the body is dead. Bodyless signatures stay silent;
/// constructors are always eligible since they can't be overridden.
fn check_unused_parameters(
    callable: Node,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    let Some(body) = callable.child_by_field_name("body") else {
        return;
    };
    if body.has_error() {
        return;
    }
    let Some(parameters) = callable.child_by_field_name("parameters") else {
        return;
    };
    if parameters.has_error() {
        return;
    }
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if out.len() >= MAX_DIAGNOSTICS {
            return;
        }
        if parameter.kind() != "formal_parameter" {
            continue;
        }
        let Some(name_node) = parameter.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        if !name_used(body, name_node, name, source) {
            out.push(unused_diagnostic(
                index.range(name_node),
                format!("unused parameter '{name}'"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use ls_types::{DiagnosticSeverity, DiagnosticTag, NumberOrString, Range};

    /// Resolves `java.lang.Object` (so the conservative policy can flag) plus any
    /// extra classes provided.
    struct ObjectAware(Vec<(&'static str, Vec<&'static str>, Vec<&'static str>)>);

    impl SymbolSource for ObjectAware {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            let make = |supers: &[&str], members: &[&str]| ExternalClass {
                supers: supers.iter().map(|s| s.to_string()).collect(),
                type_params: Vec::new(),
                members: members
                    .iter()
                    .map(|n| ExternalMember {
                        name: n.to_string(),
                        kind: ExternalMemberKind::Method,
                        signature: format!("{n}()"),
                        template: None,
                        is_static: false,
                        ret_fqn: None,
                        ret_display: None,
                        metadata: None,
                    })
                    .collect(),
                metadata: Some(jvl_types::ClassMetadata {
                    id: jvl_types::TypeId::named(fqn),
                    kind: jvl_types::ClassKind::Class,
                    access: jvl_types::Access::Public,
                    is_abstract: false,
                    is_static: true,
                    enclosing_class: None,
                    type_parameters: Vec::new(),
                    supertypes: supers
                        .iter()
                        .map(|s| jvl_types::TypeRef::named(s))
                        .collect(),
                    hierarchy_complete: true,
                    constructors_complete: true,
                }),
            };
            if fqn == "java.lang.Object" {
                return Some(make(&[], &["toString", "equals", "hashCode", "getClass"]));
            }
            self.0
                .iter()
                .find(|(f, _, _)| *f == fqn)
                .map(|(_, supers, members)| make(supers, members))
        }
    }

    fn semantic(
        src: &str,
        symbols: &dyn SymbolSource,
        unresolved_members: bool,
        unused: bool,
    ) -> Vec<Diagnostic> {
        semantic_for_sources(&[src], 0, symbols, unresolved_members, unused)
    }

    fn semantic_for_sources(
        sources: &[&str],
        current: usize,
        symbols: &dyn SymbolSource,
        unresolved_members: bool,
        unused: bool,
    ) -> Vec<Diagnostic> {
        let mut parser = new_parser();
        let trees: Vec<_> = sources
            .iter()
            .map(|src| parse(&mut parser, src, None).unwrap())
            .collect();
        let docs: Vec<_> = sources
            .iter()
            .zip(&trees)
            .map(|(source, tree)| OpenDoc { source, tree })
            .collect();
        let index = LineIndex::new(sources[current], PositionEncoding::Utf16);
        let uri: Uri = "file:///Test.java".parse().expect("test uri");
        semantic_diagnostics(
            &docs,
            current,
            &index,
            &uri,
            symbols,
            unresolved_members,
            unused,
        )
    }

    fn diags(src: &str, symbols: &dyn SymbolSource) -> Vec<String> {
        semantic(src, symbols, true, false)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    fn return_messages(src: &str, symbols: &dyn SymbolSource) -> Vec<String> {
        semantic(src, symbols, false, false)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    fn range_of(src: &str, needle: &str) -> Range {
        let start = src.find(needle).expect("range marker present");
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        Range {
            start: index.position(start),
            end: index.position(start + needle.len()),
        }
    }

    fn has_recovery(src: &str, missing: bool) -> bool {
        let tree = parse(&mut new_parser(), src, None).unwrap();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if (missing && node.is_missing()) || (!missing && node.kind() == "ERROR") {
                return true;
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        false
    }

    #[test]
    fn return_diagnostic_has_exact_contract() {
        let src = "class C { boolean m() { return 1; } }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(
            diagnostic.message,
            "incompatible types: int cannot be converted to boolean"
        );
        assert_eq!(diagnostic.range, range_of(src, "1"));
        // Clickable declaration link: the declared return type, same document.
        let related = diagnostic
            .related_information
            .as_ref()
            .expect("related information present");
        assert_eq!(related.len(), 1, "{related:?}");
        assert_eq!(
            related[0].message,
            "method 'm' declared to return 'boolean' here"
        );
        assert_eq!(related[0].location.uri.as_str(), "file:///Test.java");
        assert_eq!(related[0].location.range, range_of(src, "boolean"));
    }

    #[test]
    fn primitive_returns_are_checked_conservatively() {
        let valid = "class C {
            int exact() { return 1; }
            long widened() { return 1; }
            double floating() { return 1.0f; }
            boolean truth() { return true; }
            char character() { return 'x'; }
        }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class C { boolean m() { return 1; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: int cannot be converted to boolean"]
        );
    }

    #[test]
    fn reference_returns_are_checked_conservatively() {
        let valid = "class Box {} class C { Box m(Box box) { return box; } }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class Box {} class C { Box m() { return 1; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: int cannot be converted to Box"]
        );
    }

    #[test]
    fn boxed_returns_support_unboxing_and_reject_incompatible_types() {
        let symbols = ObjectAware(vec![
            ("java.lang.Integer", vec!["java.lang.Number"], Vec::new()),
            ("java.lang.Number", vec!["java.lang.Object"], Vec::new()),
        ]);
        let valid = "class C {
            Integer boxed() { return 1; }
            long widened(Integer value) { return value; }
        }\n";
        assert!(return_messages(valid, &symbols).is_empty());

        let invalid = "class C { boolean m(Integer value) { return value; } }\n";
        assert_eq!(
            return_messages(invalid, &symbols),
            ["incompatible types: Integer cannot be converted to boolean"]
        );
    }

    #[test]
    fn array_returns_are_checked_conservatively() {
        let valid = "class C { int[] m(int[] values) { return values; } }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class C { int[] m() { return 1; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: int cannot be converted to int[]"]
        );
    }

    #[test]
    fn null_returns_follow_reference_rules() {
        let valid = "class Box {} class C { Box m() { return null; } }\n";
        assert!(return_messages(valid, &NoSymbols).is_empty());

        let invalid = "class C { int m() { return null; } }\n";
        assert_eq!(
            return_messages(invalid, &NoSymbols),
            ["incompatible types: null cannot be converted to int"]
        );
    }

    #[test]
    fn missing_and_unexpected_return_values_use_statement_ranges() {
        let src = "class C {
            void unexpected() { return 1; }
            int missing() { return; }
        }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        let unexpected = diagnostics
            .iter()
            .find(|d| d.message.ends_with("unexpected return value"))
            .expect("unexpected-value diagnostic");
        assert_eq!(
            unexpected.message,
            "incompatible types: unexpected return value"
        );
        assert_eq!(unexpected.range, range_of(src, "return 1;"));
        let missing = diagnostics
            .iter()
            .find(|d| d.message.ends_with("missing return value"))
            .expect("missing-value diagnostic");
        assert_eq!(missing.message, "incompatible types: missing return value");
        assert_eq!(missing.range, range_of(src, "return;"));
        for diagnostic in diagnostics {
            assert_eq!(
                diagnostic.code,
                Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
            );
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        }
    }

    #[test]
    fn comment_only_void_return_is_bare() {
        let src = "class C { void m() { return /* no value */; } }\n";

        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn comment_only_non_void_return_is_missing_value() {
        let src = "class C { int m() { return /* no value */; } }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: missing return value"
        );
        assert_eq!(
            diagnostics[0].range,
            range_of(src, "return /* no value */;")
        );
    }

    #[test]
    fn comment_before_return_expression_is_skipped() {
        let src = "class C { boolean m() { return /* value */ 1; } }\n";
        let diagnostics = semantic(src, &NoSymbols, false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: int cannot be converted to boolean"
        );
        assert_eq!(diagnostics[0].range, range_of(src, "1"));
    }

    #[test]
    fn return_checks_run_when_member_checks_are_disabled() {
        let src = "class C { boolean m() { this.nope(); return 1; } }\n";
        let diagnostics = semantic(src, &ObjectAware(vec![]), false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
        );
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: int cannot be converted to boolean"
        );
    }

    #[test]
    fn semantic_diagnostics_respects_global_cap() {
        let methods = (0..crate::MAX_DIAGNOSTICS + 7)
            .map(|i| format!("boolean m{i}() {{ return {i}; }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("class C {{\n{methods}\n}}\n");

        let diagnostics = semantic(&src, &NoSymbols, false, false);
        assert_eq!(diagnostics.len(), crate::MAX_DIAGNOSTICS);
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.code == Some(NumberOrString::String("jvl.incompatibleReturn".to_string()))
        }));
    }

    #[test]
    fn constructors_are_ignored() {
        let src = "class C { C() { return 1; } }\n";
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn nested_method_uses_nearest_callable() {
        let src = "class C {
            int outer() {
                class Local { boolean inner() { return 1; } }
                return 1;
            }
        }\n";

        assert_eq!(
            return_messages(src, &NoSymbols),
            ["incompatible types: int cannot be converted to boolean"]
        );
    }

    #[test]
    fn lambda_nested_returns_are_ignored() {
        let src = "class C {
            boolean outer() {
                Runnable task = () -> { return 1; };
                return true;
            }
        }\n";

        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn missing_recovery_silences_return_check() {
        let src = "class C { boolean m() { return 1 } }\n";
        assert!(
            has_recovery(src, true),
            "fixture must contain a MISSING node"
        );
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn error_recovery_silences_return_check() {
        let src = "class C { boolean m() { return 1 ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn overload_resolution_selects_by_arity_and_proves_return_mismatch() {
        // Arity distinguishes the zero-arg overload from `pick(int)`; its
        // return type (`int`) is incompatible with `boolean`.
        let src = "class C {
            boolean m() { return pick(); }
            int pick() { return 1; }
            int pick(int value) { return value; }
        }\n";

        let messages = return_messages(src, &NoSymbols);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("int cannot be converted to boolean"),
            "{messages:?}"
        );
    }

    #[test]
    fn same_arity_overload_with_unknown_argument_is_silent() {
        // Same-arity overloads plus an unresolvable argument type must stay
        // unknown rather than guess a return type.
        let src = "class C {
            boolean m() { return pick(new Missing()); }
            int pick(Foo a) { return 1; }
            int pick(Bar a) { return 2; }
        }
        class Foo {}
        class Bar {}
        ";

        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    /// `List.add(E)` re-declared over `Collection.add(E)` is one override,
    /// not two competing overloads, so `list.add(x)` is never ambiguous.
    #[test]
    fn redeclared_inherited_generic_method_is_one_override() {
        let src = "package p;
        interface Coll<E> { boolean add(E e); }
        interface Lst<E> extends Coll<E> { boolean add(E e); boolean add(int i, E e); }
        class Item {}
        class C { void m(Lst<Item> l) { l.add(new Item()); } }
        ";
        assert!(
            diags(src, &NoSymbols).is_empty(),
            "{:?}",
            diags(src, &NoSymbols)
        );
    }

    /// A chained call whose receiver is an unknown generic-method result
    /// must stay unknown. Mirrors JDK `Optional<T>`: `map` is generic
    /// (result `Opt<U>` by display only), `orElse(T)` is not.
    #[test]
    fn call_on_unknown_generic_result_is_silent() {
        use jvl_types::{
            Access, ClassKind, ClassMetadata, MemberMetadata, TypeId, TypeParameter, TypeRef,
            TypeVariableId,
        };
        struct OptLike;
        impl SymbolSource for OptLike {
            fn class(&self, fqn: &str) -> Option<ExternalClass> {
                let object = |members: &[&str]| ExternalClass {
                    supers: Vec::new(),
                    type_params: Vec::new(),
                    members: members
                        .iter()
                        .map(|n| ExternalMember {
                            name: n.to_string(),
                            kind: ExternalMemberKind::Method,
                            signature: format!("{n}()"),
                            template: None,
                            is_static: false,
                            ret_fqn: None,
                            ret_display: None,
                            metadata: None,
                        })
                        .collect(),
                    metadata: Some(ClassMetadata {
                        id: TypeId::named(fqn),
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
                };
                if fqn == "java.lang.Object" {
                    return Some(object(&["toString"]));
                }
                if fqn == "java.lang.String" {
                    let mut s = object(&[]);
                    s.supers = vec!["java.lang.Object".to_string()];
                    s.metadata.as_mut().unwrap().supertypes =
                        vec![TypeRef::named("java.lang.Object")];
                    return Some(s);
                }
                if fqn != "q.Opt" {
                    return None;
                }
                let t = TypeVariableId {
                    owner: "q.Opt".into(),
                    index: 0,
                };
                let u = TypeVariableId {
                    owner: "q.Opt#map(Object)".into(),
                    index: 0,
                };
                let member = |name: &str,
                              params: Vec<TypeRef>,
                              result: TypeRef,
                              tps: Vec<TypeParameter>,
                              disp: &str| {
                    ExternalMember {
                        name: name.to_string(),
                        kind: ExternalMemberKind::Method,
                        signature: format!("{name}()"),
                        template: None,
                        is_static: false,
                        ret_fqn: Some("q.Opt".to_string()),
                        ret_display: Some(disp.to_string()),
                        metadata: Some(MemberMetadata {
                            declaring_class: TypeId::named("q.Opt"),
                            access: Access::Public,
                            is_static: false,
                            is_abstract: false,
                            parameters: Some(params),
                            result,
                            type_parameters: tps,
                            is_varargs: false,
                        }),
                    }
                };
                Some(ExternalClass {
                    supers: vec!["java.lang.Object".to_string()],
                    type_params: vec!["T".to_string()],
                    members: vec![
                        member(
                            "map",
                            vec![TypeRef::named("java.lang.Object")],
                            TypeRef::named_with("q.Opt", vec![TypeRef::Variable(u.clone())]),
                            vec![TypeParameter {
                                id: u,
                                bounds: Vec::new(),
                            }],
                            "Opt<U>",
                        ),
                        member(
                            "orElse",
                            vec![TypeRef::Variable(t.clone())],
                            TypeRef::Variable(t.clone()),
                            Vec::new(),
                            "{0}",
                        ),
                    ],
                    metadata: Some(ClassMetadata {
                        id: TypeId::named("q.Opt"),
                        kind: ClassKind::Class,
                        access: Access::Public,
                        is_abstract: false,
                        is_static: true,
                        enclosing_class: None,
                        type_parameters: vec![TypeParameter {
                            id: t,
                            bounds: Vec::new(),
                        }],
                        supertypes: vec![TypeRef::named("java.lang.Object")],
                        hierarchy_complete: true,
                        constructors_complete: true,
                    }),
                })
            }
        }
        let src = "import q.Opt;
        class Dog {}
        class C { void m(Opt<Dog> o) { o.map(x -> x).orElse(\"none\"); } }
        ";
        assert!(
            diags(src, &OptLike).is_empty(),
            "{:?}",
            diags(src, &OptLike)
        );
    }

    #[test]
    fn unresolved_type_variable_return_is_silent() {
        let src = "class C { <T> T m() { return \"bad\"; } }\n";
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn constant_narrowing_return_is_silent() {
        let src = "class C { byte m() { return 1; } }\n";
        assert!(return_messages(src, &NoSymbols).is_empty());
    }

    #[test]
    fn incomplete_external_hierarchy_return_is_silent() {
        let src = "import a.Base; import a.Child; class C { Base m() { return new Child(); } }\n";
        let symbols = ObjectAware(vec![
            ("a.Base", vec!["java.lang.Object"], Vec::new()),
            ("a.Child", vec!["a.Missing"], Vec::new()),
        ]);

        assert!(return_messages(src, &symbols).is_empty());
    }

    #[test]
    fn flags_unknown_member_on_in_project_type() {
        let src = "class Box { int width; }\nclass C { void m() { Box b; b.nope(); } }\n";
        let msgs = diags(src, &ObjectAware(vec![]));
        assert!(msgs.iter().any(|m| m.contains("nope")), "{msgs:?}");
    }

    #[test]
    fn does_not_flag_real_or_inherited_object_members() {
        let src = "class Box { int width; }\n\
                   class C { void m() { Box b; b.width = b.hashCode(); b.toString(); } }\n";
        // width is real; hashCode/toString are inherited from Object.
        assert!(diags(src, &ObjectAware(vec![])).is_empty());
    }

    /// Array receivers know their complete member set (`length`/`clone`
    /// plus Object's), so real members stay silent and bogus ones flagged.
    #[test]
    fn array_members_diagnose_correctly() {
        let src = "class C { void m(int[] a) { int n = a.length; a.clone(); a.toString(); } }\n";
        assert!(diags(src, &ObjectAware(vec![])).is_empty());
        let src = "class C { void m(int[] a) { a.missingNo(); } }\n";
        let msgs = diags(src, &ObjectAware(vec![]));
        assert!(msgs.iter().any(|m| m.contains("missingNo")), "{msgs:?}");
    }

    #[test]
    fn stays_silent_without_object_resolution() {
        // No symbol source can resolve Object -> hierarchy incomplete -> no flags.
        let src = "class Box { int width; }\nclass C { void m() { Box b; b.nope(); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn stays_silent_when_a_supertype_is_unknown() {
        // Box extends an external type the source can't resolve -> incomplete.
        let src = "import x.Base;\n\
                   class Box extends Base { }\n\
                   class C { void m() { Box b; b.nope(); } }\n";
        assert!(
            diags(src, &ObjectAware(vec![])).is_empty(),
            "unknown super must mute"
        );
    }

    #[test]
    fn stays_silent_on_unresolved_receiver() {
        let src = "class C { void m() { mystery().nope(); } }\n";
        assert!(diags(src, &ObjectAware(vec![])).is_empty());
    }

    /// A nested-type reference in a method reference (`Map.Entry::getKey`)
    /// must not be flagged as a missing field of the receiver.
    #[test]
    fn does_not_flag_nested_type_in_method_reference() {
        let src = "import java.util.Map;\nclass C { void m() { Object r = Map.Entry.class; } }\n";
        let symbols = ObjectAware(vec![(
            "java.util.Map",
            vec!["java.lang.Object"],
            vec!["get"],
        )]);
        let msgs = diags(src, &symbols);
        assert!(
            !msgs.iter().any(|m| m.contains("Entry")),
            "nested type falsely flagged: {msgs:?}"
        );
    }

    /// A receiver erased to `java.lang.Object` (a generic-inference
    /// fallback) is never flagged, or ordinary generic chains would flood
    /// with false positives.
    #[test]
    fn does_not_flag_members_on_object_receiver() {
        let src = "class C { void m(Object o) { o.definitelyNotAMethod(); } }\n";
        assert!(
            diags(src, &ObjectAware(vec![])).is_empty(),
            "Object receiver must never be flagged"
        );
    }

    /// An enhanced-for `var` binds the element type, so member checks run
    /// against the element (`Box`), not the array/collection (`Box[]`).
    #[test]
    fn enhanced_for_var_checks_element_type() {
        // A real element member is not flagged (proves `b` is `Box`, not `Box[]`).
        let ok = "class Box { int width; }\n\
                  class C { void m(Box[] boxes) { for (var b : boxes) { int w = b.width; } } }\n";
        assert!(
            diags(ok, &ObjectAware(vec![])).is_empty(),
            "element field must resolve: {:?}",
            diags(ok, &ObjectAware(vec![]))
        );
        // A bogus element member is flagged.
        let bad = "class Box { int width; }\n\
                   class C { void m(Box[] boxes) { for (var b : boxes) { b.nope(); } } }\n";
        assert!(
            diags(bad, &ObjectAware(vec![]))
                .iter()
                .any(|m| m.contains("nope")),
            "bogus element member should flag"
        );
    }

    /// An in-project enum's `name()`/`ordinal()` (from the implicit
    /// `java.lang.Enum` super) and its constants resolve — no false flags.
    #[test]
    fn enum_name_ordinal_and_constants_resolve() {
        let src = "enum E { A, B; }\n\
                   class C { void u(E e) { e.name(); e.ordinal(); E x = E.A; } }\n";
        let syms = ObjectAware(vec![(
            "java.lang.Enum",
            vec!["java.lang.Object"],
            vec!["name", "ordinal"],
        )]);
        assert!(
            diags(src, &syms).is_empty(),
            "enum members must resolve: {:?}",
            diags(src, &syms)
        );
    }

    #[test]
    fn flags_unknown_member_on_external_type() {
        let src = "import a.Widget;\nclass C { void m() { Widget w; w.spin(); w.nope(); } }\n";
        let symbols = ObjectAware(vec![("a.Widget", vec!["java.lang.Object"], vec!["spin"])]);
        let msgs = diags(src, &symbols);
        assert!(msgs.iter().any(|m| m.contains("nope")), "{msgs:?}");
        assert!(
            !msgs.iter().any(|m| m.contains("spin")),
            "spin is real: {msgs:?}"
        );
    }

    // ---- Rule: unresolved identifiers (`jvl.cannotFindSymbol`) --------------

    #[test]
    fn undeclared_variable_has_exact_contract() {
        assert_eq!(CANNOT_FIND_SYMBOL_CODE, "jvl.cannotFindSymbol");
        let symbols = ObjectAware(Vec::new());
        let src = "class C { void m() { use(x); } void use(int v) {} }\n";
        let diagnostics = semantic(src, &symbols, true, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String("jvl.cannotFindSymbol".to_string()))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.message, "cannot find symbol: variable 'x'");
        assert_eq!(diagnostic.range, range_of(src, "x"));
    }

    #[test]
    fn declared_bindings_stay_silent() {
        let symbols = ObjectAware(Vec::new());
        let src = "class C {
            int field;
            void m(int param) {
                int local = 1;
                use(field);
                use(param);
                use(local);
                for (int i = 0; i < 3; i++) {
                    use(i);
                }
                for (int n : new int[0]) {
                    use(n);
                }
                Runnable r = () -> use(local);
            }
            void use(int v) {}
        }\n";
        let diagnostics = semantic(src, &symbols, true, false);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn pattern_catch_and_resource_bindings_stay_silent() {
        let symbols = ObjectAware(Vec::new());
        let src = "class C {
            void m(Object o) {
                if (o instanceof String s) {
                    use(s);
                }
                try (AutoCloseable res = open()) {
                    use(res);
                } catch (Exception e) {
                    use(e);
                }
            }
            void use(Object v) {}
            AutoCloseable open() { return null; }
        }\n";
        let diagnostics = semantic(src, &symbols, true, false);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn out_of_scope_and_use_before_declaration_are_flagged() {
        let symbols = ObjectAware(Vec::new());
        // A block-scoped local referenced after its block, and an
        // assignment before the declaration statement — both javac errors.
        let out_of_scope =
            "class C { void m() { { int x = 1; use(x); } use(x); } void use(int v) {} }\n";
        let messages: Vec<String> = semantic(out_of_scope, &symbols, true, false)
            .into_iter()
            .map(|d| d.message)
            .collect();
        assert_eq!(messages, ["cannot find symbol: variable 'x'"]);

        let before_decl = "class C { void m() { y = 1; int y = 2; use(y); } void use(int v) {} }\n";
        let messages: Vec<String> = semantic(before_decl, &symbols, true, false)
            .into_iter()
            .map(|d| d.message)
            .collect();
        assert_eq!(messages, ["cannot find symbol: variable 'y'"]);
    }

    #[test]
    fn unresolvable_supertype_silences_the_identifier_check() {
        // `Unknown` could declare the field — absence is unprovable.
        let symbols = ObjectAware(Vec::new());
        let src = "class C extends Unknown { void m() { use(x); } void use(int v) {} }\n";
        let diagnostics = semantic(src, &symbols, true, false);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn incomplete_object_hierarchy_silences_the_identifier_check() {
        // With no symbol source even `java.lang.Object` is unknown, so no
        // hierarchy is ever complete — the rule must stay silent.
        let src = "class C { void m() { use(x); } void use(int v) {} }\n";
        let diagnostics = semantic(src, &NoSymbols, true, false);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn inherited_field_from_open_document_stays_silent() {
        let symbols = ObjectAware(Vec::new());
        let base = "class Base { int shared; }\n";
        let current = "class C extends Base { void m() { use(shared); } void use(int v) {} }\n";
        let diagnostics = semantic_for_sources(&[current, base], 0, &symbols, true, false);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn static_imports_silence_matching_names() {
        let symbols = ObjectAware(Vec::new());
        let single = "import static java.lang.Math.PI;
        class C { double m() { return PI; } }\n";
        assert!(semantic(single, &symbols, true, false).is_empty());

        // A static wildcard can bind any name — everything stays silent.
        let wildcard = "import static java.lang.Math.*;
        class C { double m() { return E; } }\n";
        assert!(semantic(wildcard, &symbols, true, false).is_empty());
    }

    #[test]
    fn class_name_receivers_are_not_flagged() {
        // `Foo` sits in receiver position, where a type name is legal — the
        // identifier rule never judges the type namespace.
        let symbols = ObjectAware(Vec::new());
        let src = "class C { void m() { Foo.bar(); } }\n";
        let diagnostics = semantic(src, &symbols, true, false);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn recovery_silences_the_identifier_check() {
        let symbols = ObjectAware(Vec::new());
        let src = "class C { void m() { use(x) } void use(int v) {} }\n";
        assert!(
            has_recovery(src, true) || has_recovery(src, false),
            "fixture must exercise parse recovery"
        );
        assert!(semantic(src, &symbols, true, false).is_empty());
    }

    #[test]
    fn anonymous_class_bodies_silence_the_identifier_check() {
        // The created type could contribute inherited fields; unprovable.
        let symbols = ObjectAware(Vec::new());
        let src = "class C {
            Runnable r = new Runnable() {
                public void run() {
                    use(x);
                }
            };
            void use(int v) {}
        }\n";
        let diagnostics = semantic(src, &symbols, true, false);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn record_components_and_enum_constants_stay_silent() {
        let symbols = ObjectAware(Vec::new());
        let record = "record R(int size) { int doubled() { return size * 2; } }\n";
        assert!(semantic(record, &symbols, true, false).is_empty());

        let with_enum = "enum E { A, B; int m() { return use(A); } int use(E e) { return 0; } }\n";
        assert!(semantic(with_enum, &symbols, true, false).is_empty());
    }

    // ---- Hygiene rules (initializers, unreachable, unused) ------------------

    /// Hygiene-rule diagnostics with member checks off. `unused` gates only
    /// the unused-code rule; initializer/unreachable checks always run.
    fn hygiene(src: &str, unused: bool) -> Vec<Diagnostic> {
        semantic(src, &NoSymbols, false, unused)
    }

    fn hygiene_messages(src: &str, unused: bool) -> Vec<String> {
        hygiene(src, unused)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    // ---- Rule 1: incompatible initializers (`jvl.incompatibleAssignment`) ----

    #[test]
    fn incompatible_local_initializer_has_exact_contract() {
        assert_eq!(INCOMPATIBLE_ASSIGNMENT_CODE, "jvl.incompatibleAssignment");
        let src = "class C { void m() { boolean flag = 1; } }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String(
                INCOMPATIBLE_ASSIGNMENT_CODE.to_string()
            ))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(
            diagnostic.message,
            "incompatible types: int cannot be converted to boolean"
        );
        // The range covers the initializer expression, not the declarator.
        assert_eq!(diagnostic.range, range_of(src, "1"));
    }

    #[test]
    fn incompatible_field_initializer_is_flagged() {
        let src = "class Box {} class C { Box box = 1; }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: int cannot be converted to Box"
        );
        assert_eq!(diagnostics[0].range, range_of(src, "1"));
    }

    #[test]
    fn compatible_initializers_are_silent() {
        let src = "class Box {} class C {
            int exact = 1;
            long widened = 1;
            double floating = 1.0f;
            boolean truth = true;
            char letter = 'x';
            int[] numbers = null;
            void m(Box box) { Box other = box; }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn constant_narrowing_initializer_is_silent() {
        // `byte small = 1;` is legal Java — assignment conversion narrows
        // constants.
        let src = "class C { byte small = 1; }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn unknown_initializer_types_are_silent() {
        // Flag only on proven `Some(false)`: an unknown declared type or an
        // unresolvable initializer (`None`) must stay silent.
        let src = "class C {
            void m() {
                Mystery thing = source();
                int number = source();
            }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn var_initializers_are_skipped() {
        // `var` has no declared type to check; the rule must skip it rather
        // than resolve `var` as a type name.
        let src = "class C { void m() { var flag = 1; } }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn null_initializer_follows_reference_rules() {
        let valid = "class Box {} class C { Box box = null; }\n";
        assert!(hygiene_messages(valid, false).is_empty());

        let invalid = "class C { int number = null; }\n";
        assert_eq!(
            hygiene_messages(invalid, false),
            ["incompatible types: null cannot be converted to int"]
        );
    }

    #[test]
    fn missing_recovery_silences_initializer_check() {
        let src = "class C { void m() { boolean flag = 1 } }\n";
        assert!(
            has_recovery(src, true),
            "fixture must contain a MISSING node"
        );
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn error_recovery_silences_initializer_check() {
        let src = "class C { void m() { boolean flag = 1 ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(hygiene_messages(src, false).is_empty());
    }

    // ---- Rule 1b: incompatible reassignments (`jvl.incompatibleAssignment`) --

    #[test]
    fn incompatible_reassignment_has_exact_contract() {
        // `java.lang.Integer` (int's box) must be known for the layer to
        // prove int is NOT convertible to String.
        let symbols = ObjectAware(vec![
            (
                "java.lang.String",
                vec!["java.lang.Object"],
                vec!["valueOf"],
            ),
            ("java.lang.Integer", vec!["java.lang.Number"], Vec::new()),
            ("java.lang.Number", vec!["java.lang.Object"], Vec::new()),
        ]);
        let src = "class C { void m() { String jack = String.valueOf(10); jack = 10; } }\n";
        let diagnostics = semantic(src, &symbols, false, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String(
                "jvl.incompatibleAssignment".to_string()
            ))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostic.message,
            "incompatible types: int cannot be converted to String"
        );
        // The range covers the assigned value (the second `10`), not the LHS.
        let start = src.rfind("10").expect("assigned value present");
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        assert_eq!(
            diagnostic.range,
            Range {
                start: index.position(start),
                end: index.position(start + 2),
            }
        );
        // Declaration link: the declarator's name node, same document.
        let related = diagnostic
            .related_information
            .as_ref()
            .expect("related information present");
        assert_eq!(related.len(), 1, "{related:?}");
        assert_eq!(related[0].message, "'jack' declared as 'String' here");
        assert_eq!(related[0].location.uri.as_str(), "file:///Test.java");
        assert_eq!(related[0].location.range, range_of(src, "jack"));
    }

    #[test]
    fn incompatible_reassignment_to_project_type_is_flagged() {
        let src = "class Box {} class C { void m() { Box b = new Box(); b = 1; } }\n";
        assert_eq!(
            hygiene_messages(src, false),
            ["incompatible types: int cannot be converted to Box"]
        );
    }

    #[test]
    fn compatible_reassignments_are_silent() {
        let src = "class C { void m() { int x = 1; x = 2; } }\n";
        assert!(hygiene_messages(src, false).is_empty());

        let widening = "class C { void m() { long l; l = 1; } }\n";
        assert!(hygiene_messages(widening, false).is_empty());
    }

    #[test]
    fn compound_assignments_are_silent() {
        // Compound operators have implicit-cast semantics (`byte b; b += 1;`
        // is legal Java), so the rule only inspects plain `=`.
        let src = "class C { void m() { byte b = 1; b += 1; int i = 1; i += 2; } }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn constant_narrowing_reassignment_is_silent() {
        // `byte b; b = 1;` is legal Java — assignment conversion narrows
        // constants.
        let src = "class C { void m() { byte b; b = 1; } }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn unknown_reassignment_types_are_silent() {
        // Flag only on proven `Some(false)`: an undeclared LHS or an
        // unresolvable RHS (`None`) must stay silent.
        let src = "class C {
            void m() {
                mystery = 10;
                int x;
                x = unknownCall();
            }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn null_reassignment_follows_reference_rules() {
        let valid = "class Box {} class C { void m() { Box b = new Box(); b = null; } }\n";
        assert!(hygiene_messages(valid, false).is_empty());

        let invalid = "class C { void m() { int i; i = null; } }\n";
        assert_eq!(
            hygiene_messages(invalid, false),
            ["incompatible types: null cannot be converted to int"]
        );
    }

    #[test]
    fn recovery_silences_reassignment_check() {
        let src = "class C { void m() { boolean flag; flag = 1 } }\n";
        assert!(
            has_recovery(src, true),
            "fixture must exercise MISSING recovery"
        );
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn field_reassignment_through_this_is_checked() {
        let src = "class C { int f; void m() { this.f = true; } }\n";
        let diagnostics = hygiene(src, false);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "incompatible types: boolean cannot be converted to int"
        );
        // `this.f` links to the field declarator, never a same-named local.
        let related = diagnostics[0]
            .related_information
            .as_ref()
            .expect("related information present");
        assert_eq!(related[0].message, "'f' declared as 'int' here");
        let f_at = src.find("f;").expect("field declarator present");
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        assert_eq!(
            related[0].location.range,
            Range {
                start: index.position(f_at),
                end: index.position(f_at + 1),
            }
        );
    }

    #[test]
    fn hygiene_diagnostics_respect_global_cap() {
        let fields = (0..crate::MAX_DIAGNOSTICS + 7)
            .map(|i| format!("boolean f{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("class C {{\n{fields}\n}}\n");

        let diagnostics = hygiene(&src, false);
        assert_eq!(diagnostics.len(), crate::MAX_DIAGNOSTICS);
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.code
                == Some(NumberOrString::String(
                    INCOMPATIBLE_ASSIGNMENT_CODE.to_string(),
                ))
        }));
    }

    // ---- Rule 2: unreachable statements (`jvl.unreachable`) -----------------

    #[test]
    fn unreachable_statement_has_exact_contract() {
        assert_eq!(UNREACHABLE_CODE, "jvl.unreachable");
        let src = "class C { int m() { return 1; return 2; } }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String(UNREACHABLE_CODE.to_string()))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(diagnostic.message, "unreachable statement");
        // The range covers the first dead statement.
        assert_eq!(diagnostic.range, range_of(src, "return 2;"));
    }

    #[test]
    fn unreachable_after_throw_break_and_continue() {
        for terminator in ["throw new RuntimeException();", "break;", "continue;"] {
            let src = format!(
                "class C {{ void m() {{ while (true) {{ {terminator} int dead = 0; }} }} }}\n"
            );
            assert_eq!(
                hygiene_messages(&src, false),
                ["unreachable statement"],
                "{terminator}"
            );
        }
    }

    #[test]
    fn unreachable_emits_once_per_block() {
        let src = "class C { void m() { return; int first = 0; int second = 0; } }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range, range_of(src, "int first = 0;"));
    }

    #[test]
    fn unreachable_is_per_block_not_per_file() {
        let src = "class C {
            void a() { return; int deadA = 0; int alsoDeadA = 0; }
            void b() { return; int deadB = 0; }
        }\n";
        let diagnostics = hygiene(src, false);

        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        let ranges: Vec<_> = diagnostics.iter().map(|d| d.range).collect();
        assert!(ranges.contains(&range_of(src, "int deadA = 0;")));
        assert!(ranges.contains(&range_of(src, "int deadB = 0;")));
    }

    #[test]
    fn trailing_comments_are_not_unreachable() {
        let block = "class C { void m() { return; /* done */ } }\n";
        assert!(hygiene_messages(block, false).is_empty());

        let line = "class C { void m() { return; // done\n } }\n";
        assert!(hygiene_messages(line, false).is_empty());
    }

    #[test]
    fn switch_case_groups_are_never_flagged() {
        // A statement after `break;` inside the same case group is genuinely
        // dead, but the rule is conservative and inspects only plain blocks.
        let src = "class C {
            void m(int v) {
                switch (v) { case 1: break; m(v); default: return; }
            }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn statement_after_conditional_return_is_reachable() {
        // The `return` terminates only the `if` block; the outer statement
        // after the `if` is reachable and must not be flagged.
        let src = "class C { void m(boolean flag) { if (flag) { return; } int after = 0; } }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn recovery_silences_unreachable_check() {
        let src = "class C { void m() { return; int dead = 0 ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn finally_runs_after_return_without_becoming_unreachable() {
        let src = "class C {
            void m() {
                try { return; }
                finally { cleanup(); }
            }
            void cleanup() { }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn following_statement_is_unreachable_when_try_and_catches_return() {
        let src = "class C {
            int m() {
                try { return 1; }
                catch (Exception e) { return 2; }
                finally { cleanup(); }
                return 3;
            }
            void cleanup() { }
        }\n";
        let diagnostics = hygiene(src, false);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range, range_of(src, "return 3;"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn normal_catch_keeps_statement_after_try_reachable() {
        let src = "class C {
            void m() {
                try { return; }
                catch (Exception e) { cleanup(); }
                finally { cleanup(); }
                cleanup();
            }
            void cleanup() { }
        }\n";
        assert!(hygiene_messages(src, false).is_empty());
    }

    #[test]
    fn abrupt_finally_makes_following_statement_unreachable() {
        let src = "class C {
            void m() {
                try { cleanup(); }
                finally { return; }
                after();
            }
            void cleanup() { }
            void after() { }
        }\n";
        let diagnostics = hygiene(src, false);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range, range_of(src, "after();"));
    }

    #[test]
    fn statements_after_returns_inside_try_catch_and_finally_are_unreachable() {
        let src = "class C {
            void a() { try { return; cleanup(); } finally { cleanup(); } }
            void b() { try { cleanup(); } catch (Exception e) { return; cleanup(); } }
            void c() { try { cleanup(); } finally { return; cleanup(); } }
            void cleanup() { }
        }\n";
        let diagnostics = hygiene(src, false);
        assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
        assert!(diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)));
    }

    #[test]
    fn nested_try_completion_propagates_through_finally() {
        let src = "class C {
            void m() {
                try {
                    try { return; }
                    finally { cleanup(); }
                } finally { cleanup(); }
                cleanup();
            }
            void cleanup() { }
        }\n";
        let diagnostics = hygiene(src, false);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    #[test]
    fn dead_code_after_return_in_try_preserves_abrupt_completion() {
        let src = "class C {
            void m() {
                try { return; dead(); }
                finally { cleanup(); }
                after();
            }
            void dead() { } void cleanup() { } void after() { }
        }\n";
        assert_eq!(
            hygiene_messages(src, false),
            ["unreachable statement", "unreachable statement"]
        );
    }

    #[test]
    fn dead_code_after_return_in_catch_preserves_abrupt_completion() {
        let src = "class C {
            void m() {
                try { throw new RuntimeException(); }
                catch (Exception e) { return; dead(); }
                finally { cleanup(); }
                after();
            }
            void dead() { } void cleanup() { } void after() { }
        }\n";
        assert_eq!(
            hygiene_messages(src, false),
            ["unreachable statement", "unreachable statement"]
        );
    }

    #[test]
    fn dead_code_after_return_in_finally_preserves_abrupt_completion() {
        let src = "class C {
            void m() {
                try { cleanup(); }
                finally { return; dead(); }
                after();
            }
            void dead() { } void cleanup() { } void after() { }
        }\n";
        assert_eq!(
            hygiene_messages(src, false),
            ["unreachable statement", "unreachable statement"]
        );
    }

    #[test]
    fn dead_code_in_try_with_resources_preserves_abrupt_completion() {
        let src = "class C {
            void m() {
                try (var resource = resource()) { return; dead(); }
                finally { cleanup(); }
                after();
            }
            AutoCloseable resource() { return null; }
            void dead() { } void cleanup() { } void after() { }
        }\n";
        assert_eq!(
            hygiene_messages(src, false),
            ["unreachable statement", "unreachable statement"]
        );
    }

    #[test]
    fn recovery_in_try_catch_finally_stays_silent() {
        let src = "class C {
            void m() {
                try { return; }
                catch (Exception e) { return ???; }
                finally { cleanup(); }
                cleanup();
            }
            void cleanup() { }
        }\n";
        assert!(has_recovery(src, false));
        assert!(hygiene_messages(src, false).is_empty());
    }

    // ---- Rule 3: unused code (`jvl.unused`, gated by `unused`) --------------

    #[test]
    fn unused_local_has_exact_contract() {
        let src = "class C { void m() { int count = 0; } }\n";
        let diagnostics = hygiene(src, true);

        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.code,
            Some(NumberOrString::String("jvl.unused".to_string()))
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diagnostic.source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(diagnostic.message, "unused local variable 'count'");
        assert_eq!(diagnostic.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
        // The range covers the declarator name.
        assert_eq!(diagnostic.range, range_of(src, "count"));
    }

    #[test]
    fn used_local_is_silent() {
        let src = "class C { void m() { int count = 0; log(count); } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn unused_initializer_block_local_is_flagged() {
        let src = "class C { { int temp = 0; } }\n";
        assert_eq!(
            hygiene_messages(src, true),
            ["unused local variable 'temp'"]
        );
    }

    #[test]
    fn unused_parameters_flag_constructors_private_and_static_methods() {
        let constructor = "class C { C(int seed) { } }\n";
        assert_eq!(
            hygiene_messages(constructor, true),
            ["unused parameter 'seed'"]
        );

        let private = "class C { private void log(int level) { } void m() { log(1); } }\n";
        assert_eq!(
            hygiene_messages(private, true),
            ["unused parameter 'level'"]
        );

        let is_static = "class C { static void tick(int beat) { } }\n";
        assert_eq!(
            hygiene_messages(is_static, true),
            ["unused parameter 'beat'"]
        );
    }

    #[test]
    fn public_instance_method_parameters_are_silent() {
        // Non-private instance methods can be overridden or fulfill an
        // interface; their parameters are part of a wider contract.
        let src = "class C { public void on(int event) { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn bodyless_method_parameters_are_silent() {
        // `native`: private-and-static but bodyless; only methods WITH
        // bodies are checked.
        let src =
            "class C { private static native void poke(int handle); void m() { poke(1); } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn override_annotated_method_parameters_are_silent() {
        // Syntactically `@Override` parses on a private method; the rule must
        // skip the whole method (the annotation also mutes the member rule).
        let src = "class C { @Override private void log(int level) { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn qualified_override_annotated_method_parameters_are_silent() {
        let src = "class C { @java.lang.Override private void log(int level) { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn main_method_parameters_are_silent() {
        let src = "class C { public static void main(String[] args) { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn catch_parameters_are_silent() {
        let src = "class C { void m() { try { m(); } catch (Exception e) { } } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn lambda_parameters_are_silent() {
        let src = "class C {
            void m() {
                java.util.function.IntConsumer sink = value -> { };
                sink.accept(1);
            }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn unused_private_field_and_method_are_flagged() {
        let src = "class C {
            private int hidden = 1;
            private void helper() { }
        }\n";
        let mut messages = hygiene_messages(src, true);
        messages.sort();
        assert_eq!(
            messages,
            [
                "unused private field 'hidden'",
                "unused private method 'helper'"
            ]
        );
    }

    #[test]
    fn referenced_private_members_are_silent() {
        let src = "class C {
            private int width = 1;
            private int grow() { return width + 1; }
            int m() { return grow(); }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn method_reference_counts_as_use() {
        let src = "class C {
            private void helper() { }
            Runnable m() { return this::helper; }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn annotated_members_are_silent() {
        let src = "class C { @Deprecated private int legacy = 1; }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn serial_version_uid_is_silent() {
        let src = "class C { private static final long serialVersionUID = 1L; }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn private_constructor_is_silent() {
        let src = "class Util { private Util() { } }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn shadowing_only_ever_silences() {
        // The local `size` shadows the field, so name-occurrence detection
        // can't tell them apart — the truly-unused field stays silent.
        let src = "class C {
            private int size = 1;
            void m() {
                int size = 2;
                log(size);
            }
        }\n";
        assert!(hygiene_messages(src, true).is_empty());
    }

    #[test]
    fn unused_option_gates_only_the_unused_rule() {
        let src = "class C {
            private int hidden = 1;
            void m() { boolean bad = 1; int dead = 0; log(bad); }
        }\n";
        // Disabled: the unused warnings disappear, but the always-on
        // initializer rule still fires.
        assert_eq!(
            hygiene_messages(src, false),
            ["incompatible types: int cannot be converted to boolean"]
        );
        // Enabled (the default): the warnings join the error.
        let mut messages = hygiene_messages(src, true);
        messages.sort();
        assert_eq!(
            messages,
            [
                "incompatible types: int cannot be converted to boolean",
                "unused local variable 'dead'",
                "unused private field 'hidden'",
            ]
        );
    }

    #[test]
    fn recovery_silences_unused_check() {
        let src = "class C { void m() { int dead = 0; ???; } }\n";
        assert!(
            has_recovery(src, false),
            "fixture must contain an ERROR node"
        );
        assert!(hygiene_messages(src, true).is_empty());
    }

    // ---- Method/constructor applicability -----------------------

    fn codes(src: &str, symbols: &dyn SymbolSource) -> Vec<Option<String>> {
        semantic(src, symbols, true, false)
            .into_iter()
            .map(|d| match d.code {
                Some(NumberOrString::String(s)) => Some(s),
                _ => None,
            })
            .collect()
    }

    fn has_code(src: &str, symbols: &dyn SymbolSource, code: &str) -> bool {
        codes(src, symbols)
            .iter()
            .any(|c| c.as_deref() == Some(code))
    }

    #[test]
    fn constructor_argument_mismatch_is_invalid_instantiation() {
        let src = "package demo; class User { User(User copy) {} } class Order {} \
                   class C { void m() { new User(new Order()); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INSTANTIATION_CODE));
    }

    #[test]
    fn no_declared_constructor_uses_the_synthesized_default() {
        let src = "class User {} class C { void m() { new User(); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn declaring_any_constructor_removes_the_synthesized_default() {
        let src = "package demo; class User { User(User copy) {} } \
                   class C { void m() { new User(1); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INSTANTIATION_CODE));
    }

    #[test]
    fn cannot_instantiate_interface_directly() {
        let src = "interface Named {} class C { void m() { new Named(); } }\n";
        let msgs = diags(src, &NoSymbols);
        assert!(
            msgs.iter()
                .any(|m| m.contains("cannot instantiate interface 'Named'")),
            "{msgs:?}"
        );
    }

    #[test]
    fn anonymous_interface_implementation_is_clean() {
        let src = "interface Named {} class C { void m() { new Named() {}; } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn cannot_instantiate_abstract_class_directly() {
        let src = "abstract class Shape {} class C { void m() { new Shape(); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INSTANTIATION_CODE));
    }

    #[test]
    fn anonymous_abstract_class_subclass_is_clean() {
        let src = "abstract class Shape {} class C { void m() { new Shape() {}; } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn cannot_instantiate_enum_directly() {
        let src = "enum Color { A } class C { void m() { new Color(); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INSTANTIATION_CODE));
    }

    #[test]
    fn unqualified_inner_class_creation_needs_an_enclosing_instance() {
        let src = "class Outer { class Inner {} } \
                   class C { void m() { new Outer.Inner(); } }\n";
        let msgs = diags(src, &NoSymbols);
        assert!(
            msgs.iter()
                .any(|m| m.contains("enclosing instance required")),
            "{msgs:?}"
        );
    }

    #[test]
    fn qualified_inner_class_creation_is_clean() {
        let src = "class Outer { class Inner {} } \
                   class C { void m() { new Outer().new Inner(); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn static_nested_class_creation_needs_no_enclosing_instance() {
        let src = "class Outer { static class Inner {} } \
                   class C { void m() { new Outer.Inner(); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn private_constructor_from_another_top_level_class_is_inaccessible() {
        let src = "class P { private P() {} } class C { void m() { new P(); } }\n";
        let msgs = diags(src, &NoSymbols);
        assert!(
            msgs.iter().any(|m| m.contains("not accessible")),
            "{msgs:?}"
        );
    }

    #[test]
    fn private_constructor_from_the_same_top_level_nest_is_clean() {
        let src = "class P { private P() {} class Inner { void m() { new P(); } } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn package_private_constructor_across_packages_is_inaccessible() {
        let provider = "package a; public class P { P() {} }\n";
        let consumer = "package b; import a.P; class C { void m() { new P(); } }\n";
        let diagnostics = semantic_for_sources(&[provider, consumer], 1, &NoSymbols, true, false);
        assert!(
            diagnostics.iter().any(|d| d.code
                == Some(NumberOrString::String(
                    INVALID_INSTANTIATION_CODE.to_string()
                ))),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn package_private_constructor_in_the_same_package_is_clean() {
        let provider = "package a; public class P { P() {} }\n";
        let consumer = "package a; class C { void m() { new P(); } }\n";
        assert!(semantic_for_sources(&[provider, consumer], 1, &NoSymbols, true, false).is_empty());
    }

    #[test]
    fn protected_constructor_across_packages_is_silent() {
        let provider = "package a; public class P { protected P() {} }\n";
        let consumer = "package b; import a.P; class C { void m() { new P(); } }\n";
        assert!(semantic_for_sources(&[provider, consumer], 1, &NoSymbols, true, false).is_empty());
    }

    #[test]
    fn record_canonical_constructor_checks_argument_types() {
        let src = "record R(int a) {} class C { void m() { new R(\"x\"); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INSTANTIATION_CODE));
    }

    #[test]
    fn record_canonical_constructor_accepts_matching_arguments() {
        let src = "record R(int a) {} class C { void m() { new R(1); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn overload_selection_proves_the_selected_returns_return_mismatch() {
        let src = "package demo; class User {} class Order {} \
                   class C { User pick(User u) { return u; } Order pick(Order o) { return o; } \
                   void m() { Order o = pick(new User()); } }\n";
        assert!(has_code(src, &NoSymbols, INCOMPATIBLE_ASSIGNMENT_CODE));
    }

    #[test]
    fn no_overload_accepts_a_primitive_argument() {
        let src = "package demo; class User {} class Order {} \
                   class C { User pick(User u) { return u; } Order pick(Order o) { return o; } \
                   void m() { pick(1); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INVOCATION_CODE));
    }

    #[test]
    fn strict_phase_widening_is_preferred_over_boxing() {
        let src = "class C { long f(long a) { return a; } boolean f(Integer a) { return false; } \
                   void m() { long x = f(1); } }\n";
        let symbols = ObjectAware(vec![("java.lang.Integer", Vec::new(), Vec::new())]);
        assert!(diags(src, &symbols).is_empty());
    }

    #[test]
    fn fixed_arity_overload_beats_varargs() {
        let src = "class C { void g(int a, int b) {} void g(int... xs) {} \
                   void m() { g(1, 2); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn ambiguous_call_between_unrelated_reference_overloads() {
        let src = "package demo; class User {} class Order {} \
                   class C { void h(User u) {} void h(Order o) {} void m() { h(null); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INVOCATION_CODE));
    }

    #[test]
    fn lambda_argument_is_checked_against_function_arity() {
        let clean = "interface Fn { void run(); } \
                     class C { void h(Fn f) {} void m() { h(() -> {}); } }\n";
        assert!(diags(clean, &NoSymbols).is_empty());

        let wrong = "interface Fn { void run(); } \
                     class C { void h(Fn f) {} void m() { h(value -> {}); } }\n";
        assert!(has_code(wrong, &NoSymbols, INVALID_INVOCATION_CODE));
    }

    /// `ExecutorService.submit(() -> { work(); })` in spring-petclinic was
    /// reported ambiguous while javac compiled it. A block body returning no
    /// value is void-compatible only (JLS 15.27.2), so the `Callable`-shaped
    /// overload is not applicable and there is nothing to be ambiguous with.
    #[test]
    fn void_block_lambda_selects_the_void_overload() {
        let src = "interface Task<T> { T call(); } interface Job { void run(); } \
                   class C { void work() {} <T> void submit(Task<T> t) {} void submit(Job j) {} \
                   void m() { submit(() -> { work(); }); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    /// A value-returning block is value-compatible only, so the mirror case
    /// must exclude the `Runnable`-shaped overload instead.
    #[test]
    fn value_block_lambda_selects_the_value_overload() {
        let src = "interface Task<T> { T call(); } interface Job { void run(); } \
                   class C { <T> void submit(Task<T> t) {} void submit(Job j) {} \
                   void m() { submit(() -> { return 1; }); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    /// An expression body is only value-compatible if it actually produces a
    /// value: `() -> work()` on a `void work()` fits `Runnable` alone. Typing
    /// the expression is what separates this from the case above.
    #[test]
    fn void_expression_lambda_selects_the_void_overload() {
        let src = "interface Task<T> { T call(); } interface Job { void run(); } \
                   class C { void work() {} <T> void submit(Task<T> t) {} void submit(Job j) {} \
                   void m() { submit(() -> work()); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    /// A body that always throws is congruent with *both* descriptors, so
    /// applicability cannot decide. javac resolves it by the most-specific
    /// rule — a value result beats `void` — rather than calling it ambiguous.
    #[test]
    fn always_throwing_lambda_prefers_the_value_returning_overload() {
        let src = "interface Task<T> { T call(); } interface Job { void run(); } \
                   class Boom extends RuntimeException {} \
                   class C { <T> void submit(Task<T> t) {} void submit(Job j) {} \
                   void m() { submit(() -> { throw new Boom(); }); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    /// The narrowing must stay narrow: two overloads whose descriptors are
    /// both `void` cannot be separated by the lambda rule, and javac reports
    /// `reference to k is ambiguous` here.
    #[test]
    fn lambda_between_two_void_overloads_stays_ambiguous() {
        let src = "interface G1 { void g(); } interface G2 { void g(); } \
                   class C { void k(G1 g) {} void k(G2 g) {} void m() { k(() -> {}); } }\n";
        assert!(has_code(src, &NoSymbols, INVALID_INVOCATION_CODE));
    }

    /// When the body's own call cannot be resolved — in cbioportal its
    /// arguments come from an enclosing lambda's inferred parameter — the
    /// shape is not knowable. Treating that as "congruent with neither"
    /// leaves the overload set tied and reports a false ambiguity, so an
    /// unresolvable statement expression is treated as congruent with both
    /// and the most-specific rule decides.
    #[test]
    fn unresolvable_expression_lambda_still_selects_an_overload() {
        let src = "interface Task<T> { T call(); } interface Job { void run(); } \
                   class C { <T> void submit(Task<T> t) {} void submit(Job j) {} \
                   void m(Mystery m) { submit(() -> m.compute()); } }\n";
        assert!(!has_code(src, &NoSymbols, INVALID_INVOCATION_CODE));
    }

    #[test]
    fn constructor_method_reference_is_applicable_to_function_target() {
        let src = "interface Factory<T, R> { R make(T value); } \
                   class Animal { Animal(String name) {} } \
                   class C { void use(Factory<String, Animal> factory) {} \
                   void m() { use(Animal::new); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn incomplete_receiver_hierarchy_silences_invocation_checks() {
        let src = "class Foo extends Missing { void bar(int x) {} } \
                   class C { void m() { new Foo().bar(1, 2); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn unresolvable_argument_never_produces_an_invocation_error() {
        let src = "class C { void h(int a) {} void m() { h(new Missing()); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn diamond_constructor_infers_class_type_argument_for_assignment_checks() {
        let src = "class User {} class Order {} \
                   class Box<T> { Box(T v) {} T get() { return null; } } \
                   class C { void m() { Box<User> b = new Box<>(new User()); Order o = b.get(); } }\n";
        assert!(has_code(src, &NoSymbols, INCOMPATIBLE_ASSIGNMENT_CODE));
    }

    #[test]
    fn diamond_constructor_infers_var_local_type() {
        let src = "class User {} \
                   class Box<T> { Box(T v) {} T get() { return null; } } \
                   class C { void m() { var b = new Box<>(new User()); User u = b.get(); } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn explicit_non_diamond_type_argument_is_honored_over_inference() {
        let src = "package demo; class User {} class Order {} \
                   class Box<T> { Box(T v) {} T get() { return null; } } \
                   class C { void m() { Box<User> b = new Box<Order>(new Order()); } }\n";
        assert!(has_code(src, &NoSymbols, INCOMPATIBLE_ASSIGNMENT_CODE));
    }

    #[test]
    fn malformed_constructor_call_recovery_stays_silent() {
        let src = "class User {}\nclass C { void m() { new User(\n } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn check_instantiation_respects_global_cap() {
        let calls = (0..crate::MAX_DIAGNOSTICS + 7)
            .map(|i| format!("void m{i}() {{ new Widget(1); }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("class Widget {{ Widget() {{}} }}\nclass C {{\n{calls}\n}}\n");
        let diagnostics = semantic(&src, &NoSymbols, true, false);
        assert_eq!(diagnostics.len(), crate::MAX_DIAGNOSTICS);
        assert!(diagnostics.iter().all(|d| d.code
            == Some(NumberOrString::String(
                INVALID_INSTANTIATION_CODE.to_string()
            ))));
    }

    #[test]
    fn provider_with_broken_constructor_header_silences_instantiation_checks() {
        let provider = "package a; public class P { public P(int a, { } }";
        let consumer = "package a; class C { void m() { new P(\"x\"); } }";
        assert!(semantic_for_sources(&[provider, consumer], 1, &NoSymbols, true, false).is_empty());
    }

    #[test]
    fn provider_with_broken_extends_silences_assignment_checks() {
        let provider = "package a; public class Dog extends { }";
        let consumer = "package a; class Animal {} class C { Animal a = new Dog(); }";
        assert!(semantic_for_sources(&[provider, consumer], 1, &NoSymbols, true, false).is_empty());
    }

    /// The nesting cap makes resolution recursion depth a constant,
    /// independent of how deeply the source nests calls. Measured: without
    /// the cap, 120 nested calls overflow a 1 MiB stack; with it the same
    /// input completes. 40 levels is past the cap (64 nesting slots = 32
    /// source levels, two slots per level), so the engine must go silent
    /// rather than keep descending.
    #[test]
    fn nested_call_arguments_past_the_cap_are_silent() {
        let mut expr = "1".to_string();
        for _ in 0..40 {
            expr = format!("id({expr})");
        }
        let src = format!(
            "class Box {{}} class C {{ int id(int x) {{ return x; }} void m() {{ Box b = {expr}; }} }}\n"
        );
        assert!(diags(&src, &NoSymbols).is_empty()); // silent, and it returned
    }

    #[test]
    fn shallow_nested_call_arguments_are_still_proven() {
        // 10 levels is far under the cap: the mismatch must still be flagged.
        let mut expr = "1".to_string();
        for _ in 0..10 {
            expr = format!("id({expr})");
        }
        let src = format!(
            "class Box {{}} class C {{ int id(int x) {{ return x; }} void m() {{ Box b = {expr}; }} }}\n"
        );
        assert!(has_code(&src, &NoSymbols, INCOMPATIBLE_ASSIGNMENT_CODE));
    }

    /// A `package` declaration is required for these: `Imports::candidates`
    /// never offers the bare simple name, so an unpackaged project class
    /// can't be resolved from another type's `implements` clause.
    #[test]
    fn enhanced_for_over_project_iterable_infers_element() {
        // `Bag implements Iterable<Item>`: `var i` is Item, so `i.size` (int)
        // resolves and assigning it to an Item is proven incompatible.
        let symbols = ObjectAware(vec![("java.lang.Iterable", vec![], vec![])]);
        let src = "package p;\n\
                   class Item { int size; }\n\
                   class Bag implements Iterable<Item> {}\n\
                   class C { void m(Bag bag) { for (var i : bag) { Item x = i.size; } } }\n";
        assert!(has_code(src, &symbols, INCOMPATIBLE_ASSIGNMENT_CODE));
    }

    #[test]
    fn enhanced_for_over_project_iterable_subclass_infers_element() {
        // Two hops with substitution: `Sack extends Bag<Item>`, `Bag<T> implements Iterable<T>`.
        let symbols = ObjectAware(vec![("java.lang.Iterable", vec![], vec![])]);
        let src = "package p;\n\
                   class Item { int size; }\n\
                   class Bag<T> implements Iterable<T> {}\n\
                   class Sack extends Bag<Item> {}\n\
                   class C { void m(Sack sack) { for (var i : sack) { Item x = i.size; } } }\n";
        assert!(has_code(src, &symbols, INCOMPATIBLE_ASSIGNMENT_CODE));
    }

    #[test]
    fn enhanced_for_over_raw_project_iterable_is_silent() {
        let symbols = ObjectAware(vec![("java.lang.Iterable", vec![], vec![])]);
        let src = "package p;\n\
                   class Item { int size; }\n\
                   class Bag<T> implements Iterable<T> {}\n\
                   class C { void m(Bag bag) { for (var i : bag) { Item x = i.size; } } }\n";
        assert!(diags(src, &symbols).is_empty());
    }

    #[test]
    fn enhanced_for_over_wildcard_project_iterable_is_silent() {
        let symbols = ObjectAware(vec![("java.lang.Iterable", vec![], vec![])]);
        let src = "package p;\n\
                   class Item { int size; }\n\
                   class Bag<T> implements Iterable<T> {}\n\
                   class C { void m(Bag<? extends Item> bag) { for (var i : bag) { Item x = i.size; } } }\n";
        assert!(diags(src, &symbols).is_empty());
    }

    #[test]
    fn enhanced_for_over_incomplete_hierarchy_is_silent() {
        let src = "package p;\n\
                   class Item { int size; }\n\
                   class Bag extends Missing {}\n\
                   class C { void m(Bag bag) { for (var i : bag) { Item x = i.size; } } }\n";
        assert!(diags(src, &NoSymbols).is_empty());
    }

    #[test]
    fn enhanced_for_over_non_iterable_project_type_is_silent() {
        let symbols = ObjectAware(vec![("java.lang.Iterable", vec![], vec![])]);
        let src = "package p;\n\
                   class Item { int size; }\n\
                   class Bag {}\n\
                   class C { void m(Bag bag) { for (var i : bag) { Item x = i.size; } } }\n";
        assert!(diags(src, &symbols).is_empty());
    }

    /// A functional interface nested in the calling class is in scope by
    /// simple name (JLS 6.5.5.1), but its binary name is `p.C$Fn`, which no
    /// file-level import candidate can produce. Without nesting-aware
    /// resolution the whole lambda check silently degrades to Unknown.
    #[test]
    fn nested_functional_interface_still_checks_lambda_arity() {
        let ok = "package p; class C { interface Fn { void run(); } \
                  void h(Fn f) {} void m() { h(() -> {}); } }\n";
        assert!(
            diags(ok, &NoSymbols).is_empty(),
            "{:?}",
            diags(ok, &NoSymbols)
        );
        let wrong = "package p; class C { interface Fn { void run(); } \
                     void h(Fn f) {} void m() { h(value -> {}); } }\n";
        assert!(has_code(wrong, &NoSymbols, INVALID_INVOCATION_CODE));
    }

    /// A sibling type in the *unnamed* package is named by its simple name,
    /// which is also its binary name.
    #[test]
    fn default_package_sibling_type_resolves_for_lambda_targets() {
        let wrong = "interface Fn { void run(); } \
                     class C { void h(Fn f) {} void m() { h(value -> {}); } }\n";
        assert!(has_code(wrong, &NoSymbols, INVALID_INVOCATION_CODE));
    }
}
