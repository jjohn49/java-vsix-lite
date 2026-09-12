//! Method and constructor applicability: JLS 15.12.2's three-phase overload
//! selection (strict, loose/boxing, varargs), plus constructor
//! instantiability (abstract/interface/enum, enclosing instance,
//! accessibility) and diamond (`<>`) type-argument inference.
//! Unresolvable input always yields `None`/[`CallResolution::Unknown`],
//! never a guess.

use std::collections::HashSet;

use tree_sitter::Node;

use jvl_types::{
    Access, ClassKind, MemberMetadata, TypeId, TypeParameter, TypeRef, TypeVariableId,
};

use crate::external::ExternalMemberKind;
use crate::model::named_children;
use crate::node_text;
use crate::resolve::{
    assignable_refs, class_facts, erase, render_type_ref_in, resolve_expression_type, ClassFacts,
    Ctx,
};

/// The outcome of resolving a method call's target overload.
pub(crate) enum CallResolution {
    /// Exactly one applicable, most-specific overload.
    Selected {
        result: TypeRef,
        /// Declaring class and access level, unused for now but kept for
        /// a future accessibility-aware consumer. Method accessibility is
        /// already enforced upstream; constructors are checked separately
        /// in this file.
        #[allow(dead_code)]
        declaring: TypeId,
        #[allow(dead_code)]
        access: Access,
    },
    /// Every candidate was excluded in all three phases — a proven error.
    NoApplicable { candidates: usize },
    /// More than one maximally-specific applicable overload — a proven error.
    Ambiguous,
    /// Insufficient information (unresolved receiver/argument/hierarchy, a
    /// generic candidate this layer can't fully check, …) — never guessed.
    Unknown,
}

/// One method/constructor candidate: its declared metadata plus the
/// use-site substitution for any class type variables its signature
/// mentions. For a diamond constructor call this comes from
/// [`infer_env_from_args`], not the receiver's (not-yet-known) type
/// arguments.
struct Candidate {
    meta: MemberMetadata,
    env: Vec<(TypeVariableId, TypeRef)>,
    /// The member's declared return type as written, kept only so hover can
    /// show `String` where the structured type lowered to `Unknown` (the
    /// class isn't on the classpath). Never used for any type decision.
    result_display: Option<String>,
    /// Erased declaration label, used only to recover source parameter names
    /// for contextual hover rendering.
    declared_signature: Option<String>,
}

#[derive(Clone)]
enum Argument<'t> {
    Value(TypeRef),
    Lambda(Node<'t>),
    MethodReference(Node<'t>),
}

fn arguments<'t>(call: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Vec<Argument<'t>>> {
    let args = call.child_by_field_name("arguments")?;
    if args.has_error() {
        return None;
    }
    named_children(args)
        .into_iter()
        .filter(|arg| !matches!(arg.kind(), "line_comment" | "block_comment"))
        .map(|arg| match arg.kind() {
            "lambda_expression" => Some(Argument::Lambda(arg)),
            "method_reference" => Some(Argument::MethodReference(arg)),
            _ => resolve_expression_type(arg, ctx)
                .map(|resolved| Argument::Value(resolved.type_ref())),
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Strict,
    Loose,
    Varargs,
}

/// Selection result, borrowing the winning [`Candidate`] instead of
/// rebuilding a [`CallResolution`] — callers that need the candidate
/// itself (diamond inference, accessibility) avoid re-running selection.
enum Selection<'c> {
    Selected(&'c Candidate, Phase),
    NoApplicable(usize),
    Ambiguous,
    Unknown,
}

/// JLS 15.12.2 phases 1-3 with each candidate's own substitution.
fn select_candidate<'c, 't>(
    candidates: &'c [Candidate],
    args: &[Argument<'t>],
    ctx: &Ctx<'_, 't>,
) -> Selection<'c> {
    if candidates.is_empty() {
        return Selection::Unknown;
    }
    for phase in [Phase::Strict, Phase::Loose, Phase::Varargs] {
        let mut applicable = Vec::new();
        let mut unknown = false;
        for candidate in candidates {
            match applicable_in(candidate, args, phase, ctx) {
                Some(true) => applicable.push(candidate),
                Some(false) => {}
                None => unknown = true,
            }
        }
        if applicable.is_empty() {
            if unknown {
                return Selection::Unknown;
            }
            continue;
        }
        if unknown {
            return Selection::Unknown;
        }
        return match most_specific(&applicable, args, ctx) {
            Some(candidate) => Selection::Selected(candidate, phase),
            None => Selection::Ambiguous,
        };
    }
    Selection::NoApplicable(candidates.len())
}

fn select<'t>(
    candidates: Vec<Candidate>,
    args: &[Argument<'t>],
    ctx: &Ctx<'_, 't>,
) -> CallResolution {
    match select_candidate(&candidates, args, ctx) {
        Selection::Selected(candidate, _) => CallResolution::Selected {
            result: candidate.meta.result.substitute(&candidate.env),
            declaring: candidate.meta.declaring_class.clone(),
            access: candidate.meta.access,
        },
        Selection::NoApplicable(count) => CallResolution::NoApplicable { candidates: count },
        Selection::Ambiguous => CallResolution::Ambiguous,
        Selection::Unknown => CallResolution::Unknown,
    }
}

fn applicable_in<'t>(
    candidate: &Candidate,
    args: &[Argument<'t>],
    phase: Phase,
    ctx: &Ctx<'_, 't>,
) -> Option<bool> {
    let params: Vec<TypeRef> = candidate
        .meta
        .parameters
        .as_ref()?
        .iter()
        .map(|parameter| parameter.substitute(&candidate.env))
        .collect();
    if params.iter().any(TypeRef::contains_unknown) {
        return if arity_excluded(candidate, args.len()) {
            Some(false)
        } else {
            None
        };
    }
    match phase {
        Phase::Strict | Phase::Loose => {
            if params.len() != args.len() {
                return Some(false);
            }
            all_proved(
                params.iter().zip(args).map(|(parameter, argument)| {
                    argument_compatible(argument, parameter, phase, ctx)
                }),
            )
        }
        Phase::Varargs => {
            if !candidate.meta.is_varargs || args.len() + 1 < params.len() {
                return Some(false);
            }
            let (fixed, last) = params.split_at(params.len() - 1);
            let TypeRef::Array(element) = &last[0] else {
                return None;
            };
            let mut checks: Vec<Option<bool>> = fixed
                .iter()
                .zip(args)
                .map(|(parameter, argument)| {
                    argument_compatible(argument, parameter, Phase::Loose, ctx)
                })
                .collect();
            checks.extend(
                args[fixed.len()..]
                    .iter()
                    .map(|argument| argument_compatible(argument, element, Phase::Loose, ctx)),
            );
            all_proved(checks.into_iter())
        }
    }
}

fn argument_compatible<'t>(
    argument: &Argument<'t>,
    parameter: &TypeRef,
    phase: Phase,
    ctx: &Ctx<'_, 't>,
) -> Option<bool> {
    match argument {
        Argument::Value(value) => convertible(value, parameter, phase, ctx),
        Argument::Lambda(lambda) => lambda_compatible(*lambda, parameter, ctx),
        Argument::MethodReference(reference) => {
            method_reference_resolution(*reference, parameter, ctx, ReferenceMode::Proof)
                .map(|_| true)
        }
    }
}

fn arity_excluded(c: &Candidate, n: usize) -> bool {
    match &c.meta.parameters {
        Some(p) if c.meta.is_varargs => n + 1 < p.len(),
        Some(p) => p.len() != n,
        None => false,
    }
}

/// Invocation-context conversion (JLS 5.3): no constant narrowing in any
/// phase, and no boxing/unboxing in the strict phase.
fn convertible(arg: &TypeRef, param: &TypeRef, phase: Phase, ctx: &Ctx<'_, '_>) -> Option<bool> {
    match (arg, param) {
        (TypeRef::Primitive(a), TypeRef::Primitive(p)) => Some(a == p || a.widens_to(*p)),
        (TypeRef::Primitive(_), _) | (_, TypeRef::Primitive(_)) if phase == Phase::Strict => {
            Some(false)
        }
        _ => assignable_refs(arg, param, ctx),
    }
}

fn all_proved(results: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut unknown = false;
    for r in results {
        match r {
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

/// When the subtyping pass cannot separate two candidates, JLS 15.12.2.5's
/// functional-interface clause can: for a lambda argument that is
/// value-compatible, a descriptor returning a value is more specific than
/// one returning `void`. This is what makes `ExecutorService.submit(() -> {
/// throw ...; })` pick `Callable` rather than being ambiguous - that body is
/// congruent with both `Callable` and `Runnable`, so applicability alone
/// leaves a tie that only this rule breaks.
fn narrow_by_lambda_result<'c, 't>(
    applicable: &[&'c Candidate],
    args: &[Argument<'t>],
    ctx: &Ctx<'_, 't>,
) -> Vec<&'c Candidate> {
    let mut kept: Vec<&'c Candidate> = applicable.to_vec();
    for (index, arg) in args.iter().enumerate() {
        let Argument::Lambda(lambda) = arg else {
            continue;
        };
        // Only a value-compatible lambda triggers the rule; a void-only body
        // has already eliminated the value-returning candidates.
        if !matches!(
            lambda_body_shape(*lambda, ctx),
            BodyShape::Either | BodyShape::ValueOnly
        ) {
            continue;
        }
        let result_is_void = |candidate: &Candidate| -> Option<bool> {
            let parameters = candidate.meta.parameters.clone()?;
            let parameter = parameters.get(index)?.substitute(&candidate.env);
            Some(matches!(
                functional_descriptor(&parameter, ctx)?.result,
                TypeRef::Void
            ))
        };
        // Unknown descriptors abstain rather than being discarded: dropping
        // one would silently narrow to a candidate that never won.
        let value_returning: Vec<&'c Candidate> = kept
            .iter()
            .copied()
            .filter(|c| result_is_void(c) == Some(false))
            .collect();
        let void_returning = kept
            .iter()
            .filter(|c| result_is_void(c) == Some(true))
            .count();
        if !value_returning.is_empty() && value_returning.len() + void_returning == kept.len() {
            kept = value_returning;
        }
    }
    kept
}

/// `m1` is more specific than `m2` when each of `m1`'s parameters is a
/// subtype of the corresponding parameter of `m2` (JLS 15.12.2.5, fixed
/// arity only). Exactly one candidate dominating every other wins; zero
/// or more than one is ambiguous.
fn most_specific<'c, 't>(
    applicable: &[&'c Candidate],
    args: &[Argument<'t>],
    ctx: &Ctx<'_, 't>,
) -> Option<&'c Candidate> {
    if applicable.len() == 1 {
        return Some(applicable[0]);
    }
    let params = |c: &Candidate| -> Vec<TypeRef> {
        c.meta
            .parameters
            .clone()
            .unwrap_or_default()
            .iter()
            .map(|p| p.substitute(&c.env))
            .collect()
    };
    let subtyping_winner = |set: &[&'c Candidate]| -> Option<&'c Candidate> {
        let mut best: Option<&'c Candidate> = None;
        'outer: for m1 in set {
            for m2 in set {
                if std::ptr::eq(*m1, *m2) {
                    continue;
                }
                let (p1, p2) = (params(m1), params(m2));
                if p1.len() != p2.len() {
                    return None;
                }
                let dominated =
                    all_proved(p1.iter().zip(&p2).map(|(a, b)| assignable_refs(a, b, ctx)));
                if dominated != Some(true) {
                    continue 'outer;
                }
            }
            if best.is_some() {
                return None;
            }
            best = Some(m1);
        }
        best
    };
    if let Some(best) = subtyping_winner(applicable) {
        return Some(best);
    }
    // Unrelated functional interfaces (`Callable` vs `Runnable`) are not
    // subtypes of each other, so subtyping can never separate them. Apply
    // the lambda clause and, if it narrowed anything, decide on that set.
    let narrowed = narrow_by_lambda_result(applicable, args, ctx);
    if narrowed.len() < applicable.len() {
        return if narrowed.len() == 1 {
            Some(narrowed[0])
        } else {
            subtyping_winner(&narrowed)
        };
    }
    None
}

/// Method candidates named `name` over `recv`'s hierarchy, own type first
/// then supertypes, with overridden methods collapsed to their nearest
/// declaration (overloads still survive). Returns `None` if the hierarchy
/// is incomplete or a matching member lacks structured metadata, rather
/// than risk missing an overload we can't see.
fn method_candidates(
    recv: &TypeRef,
    name: &str,
    static_only: bool,
    ctx: &Ctx<'_, '_>,
) -> Option<Vec<Candidate>> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen_sigs: HashSet<Vec<TypeRef>> = HashSet::new();
    let mut stack = vec![(recv.clone(), 0usize)];
    let mut visited = HashSet::new();
    while let Some((ty, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(ty.clone()) {
            continue;
        }
        let TypeRef::Named { id, args } = &ty else {
            return None;
        };
        let facts = class_facts(id, ctx)?;
        if !facts.meta.hierarchy_complete {
            return None;
        }
        let env: Vec<(TypeVariableId, TypeRef)> = facts
            .meta
            .type_parameters
            .iter()
            .map(|p| p.id.clone())
            .zip(args.iter().cloned())
            .collect();
        for m in &facts.members {
            if m.name != name || m.kind != ExternalMemberKind::Method {
                continue;
            }
            let Some(meta) = &m.metadata else {
                return None;
            };
            if static_only && !meta.is_static {
                continue;
            }
            // Override key: the signature as seen through the receiver's
            // type arguments, with any leftover type variable erased to
            // Object, so `List.add(E)` and `Collection.add(E)` collapse.
            let erased: Vec<TypeRef> = meta
                .parameters
                .as_ref()?
                .iter()
                .map(|p| erase_to_object(&erase(&p.substitute(&env))))
                .collect();
            if seen_sigs.insert(erased) {
                out.push(Candidate {
                    meta: meta.clone(),
                    env: env.clone(),
                    result_display: m.ret_display.clone(),
                    declared_signature: Some(m.signature.clone()),
                });
            }
        }
        for s in &facts.meta.supertypes {
            stack.push((s.substitute(&env), depth + 1));
        }
        // If nothing has matched `name` yet and `java.lang.Object` itself
        // is unresolvable (no JDK on the classpath), stay unknown rather
        // than conclude there's no such overload.
        if out.is_empty()
            && id.as_named() == Some("java.lang.Object")
            && facts.members.is_empty()
            && ctx.symbols.class("java.lang.Object").is_none()
        {
            return None;
        }
    }
    Some(out)
}

/// Replace every type variable in `t` with `java.lang.Object` (a bound-erased
/// approximation good enough for override identity).
fn erase_to_object(t: &TypeRef) -> TypeRef {
    match t {
        TypeRef::Variable(_) => TypeRef::named("java.lang.Object"),
        TypeRef::Named { id, args } => TypeRef::Named {
            id: id.clone(),
            args: args.iter().map(erase_to_object).collect(),
        },
        TypeRef::Array(e) => TypeRef::Array(Box::new(erase_to_object(e))),
        other => other.clone(),
    }
}

#[derive(Clone)]
struct FunctionDescriptor {
    parameters: Vec<TypeRef>,
    result: TypeRef,
}

fn ground_target_type(ty: &TypeRef) -> TypeRef {
    match ty {
        TypeRef::Named { id, args } => TypeRef::Named {
            id: id.clone(),
            args: args.iter().map(ground_target_type).collect(),
        },
        TypeRef::Array(element) => TypeRef::Array(Box::new(ground_target_type(element))),
        TypeRef::Wildcard {
            lower: Some(lower), ..
        } => ground_target_type(lower),
        TypeRef::Wildcard {
            upper: Some(upper), ..
        } => ground_target_type(upper),
        TypeRef::Wildcard { .. } => TypeRef::named("java.lang.Object"),
        other => other.clone(),
    }
}

fn type_environment(
    parameters: &[TypeParameter],
    args: &[TypeRef],
) -> Option<Vec<(TypeVariableId, TypeRef)>> {
    if parameters.len() == args.len() {
        return Some(
            parameters
                .iter()
                .map(|parameter| parameter.id.clone())
                .zip(args.iter().cloned())
                .collect(),
        );
    }
    if !args.is_empty() {
        return None;
    }
    Some(
        parameters
            .iter()
            .map(|parameter| {
                let erased = parameter
                    .bounds
                    .first()
                    .map(erase)
                    .unwrap_or_else(|| TypeRef::named("java.lang.Object"));
                (parameter.id.clone(), erased)
            })
            .collect(),
    )
}

fn is_object_method(name: &str, parameters: &[TypeRef]) -> bool {
    matches!((name, parameters), ("hashCode" | "toString", []))
        || (name == "equals"
            && matches!(
                parameters,
                [TypeRef::Named {
                    id: TypeId::Named(fqn),
                    args
                }] if fqn == "java.lang.Object" && args.is_empty()
            ))
}

fn functional_descriptor(target: &TypeRef, ctx: &Ctx<'_, '_>) -> Option<FunctionDescriptor> {
    let target = ground_target_type(target);
    let TypeRef::Named { id, .. } = &target else {
        return None;
    };
    if class_facts(id, ctx)?.meta.kind != ClassKind::Interface {
        return None;
    }
    let mut stack = vec![(target, 0usize)];
    let mut visited = HashSet::new();
    let mut seen = HashSet::new();
    let mut abstract_methods = Vec::new();
    while let Some((current, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(current.clone()) {
            continue;
        }
        let TypeRef::Named { id, args } = current else {
            return None;
        };
        let facts = class_facts(&id, ctx)?;
        if !facts.meta.hierarchy_complete {
            return None;
        }
        let env = type_environment(&facts.meta.type_parameters, &args)?;
        for member in &facts.members {
            if member.kind != ExternalMemberKind::Method || member.is_static {
                continue;
            }
            let meta = member.metadata.as_ref()?;
            if meta.access == Access::Private {
                continue;
            }
            let parameters: Vec<TypeRef> = meta
                .parameters
                .as_ref()?
                .iter()
                .map(|parameter| ground_target_type(&parameter.substitute(&env)))
                .collect();
            if is_object_method(&member.name, &parameters) {
                continue;
            }
            let key = (
                member.name.clone(),
                parameters
                    .iter()
                    .map(|parameter| erase_to_object(&erase(parameter)))
                    .collect::<Vec<_>>(),
            );
            if !seen.insert(key) {
                continue;
            }
            if meta.is_abstract {
                if !meta.type_parameters.is_empty() {
                    return None;
                }
                abstract_methods.push(FunctionDescriptor {
                    parameters,
                    result: ground_target_type(&meta.result.substitute(&env)),
                });
            }
        }
        stack.extend(
            facts
                .meta
                .supertypes
                .iter()
                .map(|supertype| (supertype.substitute(&env), depth + 1)),
        );
    }
    match abstract_methods.as_slice() {
        [descriptor] => Some(descriptor.clone()),
        _ => None,
    }
}

fn lambda_parameters(lambda: Node) -> Vec<Node> {
    let Some(parameters) = lambda.child_by_field_name("parameters") else {
        return Vec::new();
    };
    match parameters.kind() {
        "identifier" => vec![parameters],
        "inferred_parameters" => named_children(parameters)
            .into_iter()
            .filter(|node| node.kind() == "identifier")
            .collect(),
        "formal_parameters" => named_children(parameters)
            .into_iter()
            .filter(|node| matches!(node.kind(), "formal_parameter" | "spread_parameter"))
            .collect(),
        _ => Vec::new(),
    }
}

/// JLS §15.27.2 body compatibility. A lambda is congruent with a function
/// type only if its body agrees with that descriptor's *result*, not just
/// its parameters: `() -> { work(); }` is void-compatible only, so it fits
/// `Runnable` and not `Callable<T>`. Without this distinction every
/// `ExecutorService.submit(() -> { ... })` looks like it matches both
/// overloads and gets reported ambiguous, which javac does not do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BodyShape {
    /// No `return` yields a value and the body can complete normally.
    VoidOnly,
    /// Some `return` yields a value.
    ValueOnly,
    /// Both: a statement-expression body, or a block that always throws.
    Either,
    /// Not modeled. Imposes no constraint, so an unrecognised shape can only
    /// fail to narrow an overload set - never wrongly reject a candidate.
    Unknown,
}

/// `return`s belonging to *this* lambda. A `return` inside a nested lambda
/// or an anonymous/local class body belongs to that body instead, so those
/// subtrees are not descended into.
fn collect_lambda_returns(block: Node, has_value: &mut bool, has_bare: &mut bool) {
    let mut stack = vec![block];
    while let Some(node) = stack.pop() {
        for child in named_children(node) {
            match child.kind() {
                "lambda_expression" | "class_body" => continue,
                "return_statement" => {
                    if named_children(child)
                        .iter()
                        .any(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
                    {
                        *has_value = true;
                    } else {
                        *has_bare = true;
                    }
                }
                _ => stack.push(child),
            }
        }
    }
}

/// `while (true)` / `for (;;)`. Such a loop cannot complete normally unless
/// it breaks, and this deliberately ignores `break`: claiming a loop does
/// not complete yields [`BodyShape::Either`], which constrains nothing.
fn is_unconditional_loop(statement: Node, source: &str) -> bool {
    match statement.child_by_field_name("condition") {
        None => statement.kind() == "for_statement",
        Some(condition) => {
            let text = node_text(condition, source);
            text.trim()
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim()
                == "true"
        }
    }
}

/// Deliberately narrower than [`crate::diagnostics`]'s unreachable-code
/// analysis, and biased the opposite way: anything unmodeled is reported as
/// completing normally, because here a wrong "cannot complete" would make a
/// value-returning lambda look value-incompatible and reject a real overload.
fn lambda_block_completes_normally(block: Node, source: &str) -> bool {
    named_children(block)
        .into_iter()
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
        .all(|statement| match statement.kind() {
            "return_statement" | "throw_statement" => false,
            "block" => lambda_block_completes_normally(statement, source),
            "while_statement" | "for_statement" => !is_unconditional_loop(statement, source),
            _ => true,
        })
}

fn lambda_body_shape<'t>(lambda: Node<'t>, ctx: &Ctx<'_, 't>) -> BodyShape {
    let Some(body) = lambda.child_by_field_name("body") else {
        return BodyShape::Unknown;
    };
    if body.kind() != "block" {
        return match body.kind() {
            // A statement expression may be evaluated purely for effect, so
            // it can satisfy a void descriptor -- but only if it actually
            // produces a value can it satisfy a non-void one. `() -> work()`
            // on a `void work()` is void-compatible *only*, which is why the
            // expression's own type has to be resolved rather than assumed.
            "method_invocation" => match resolve_expression_type(body, ctx) {
                Some(resolved) => match resolved.type_ref() {
                    TypeRef::Void => BodyShape::VoidOnly,
                    TypeRef::Unknown => BodyShape::Unknown,
                    _ => BodyShape::Either,
                },
                None => BodyShape::Unknown,
            },
            // These always produce a value and may also stand alone.
            "object_creation_expression" | "assignment_expression" | "update_expression" => {
                BodyShape::Either
            }
            // Not a statement expression: it can only produce a value.
            _ => BodyShape::ValueOnly,
        };
    }
    let mut has_value = false;
    let mut has_bare = false;
    collect_lambda_returns(body, &mut has_value, &mut has_bare);
    match (has_value, has_bare) {
        // Mixing `return x;` and `return;` does not compile; say nothing.
        (true, true) => BodyShape::Unknown,
        (true, false) => BodyShape::ValueOnly,
        (false, true) => BodyShape::VoidOnly,
        // No `return` at all is always void-compatible, and additionally
        // value-compatible when the body cannot complete normally
        // (`{ throw ...; }`). javac resolves that overlap via the
        // most-specific rule rather than calling it ambiguous, which
        // `most_specific` handles.
        (false, false) => {
            if lambda_block_completes_normally(body, ctx.doc.source) {
                BodyShape::VoidOnly
            } else {
                BodyShape::Either
            }
        }
    }
}

fn lambda_compatible<'t>(lambda: Node<'t>, target: &TypeRef, ctx: &Ctx<'_, 't>) -> Option<bool> {
    let descriptor = functional_descriptor(target, ctx)?;
    let parameters = lambda_parameters(lambda);
    if parameters.len() != descriptor.parameters.len() {
        return Some(false);
    }

    // Arity and declared parameter types alone cannot separate `Runnable`
    // from `Callable<T>` -- both take none. The body's shape is what does.
    let shape_fits = match lambda_body_shape(lambda, ctx) {
        BodyShape::Unknown | BodyShape::Either => true,
        BodyShape::VoidOnly => matches!(descriptor.result, TypeRef::Void),
        BodyShape::ValueOnly => !matches!(descriptor.result, TypeRef::Void),
    };
    if !shape_fits {
        return Some(false);
    }
    let declared_checks = parameters
        .iter()
        .zip(&descriptor.parameters)
        .filter_map(|(parameter, target)| {
            let type_node = parameter.child_by_field_name("type")?;
            if node_text(type_node, ctx.doc.source) == "var" {
                return None;
            }
            let declared =
                crate::resolve::resolve_type_node(type_node, ctx.doc.source, ctx)?.type_ref();
            Some(if declared == *target {
                Some(true)
            } else {
                all_proved(
                    [
                        assignable_refs(&declared, target, ctx),
                        assignable_refs(target, &declared, ctx),
                    ]
                    .into_iter(),
                )
            })
        })
        .collect::<Vec<_>>();
    all_proved(declared_checks.into_iter())
}

#[derive(Clone)]
struct ReferenceResolution {
    name: String,
    parameters: Vec<TypeRef>,
    result: TypeRef,
    /// Declared return type as written, for display only (see
    /// [`Candidate::result_display`]).
    result_display: Option<String>,
    constructor: bool,
}

fn reference_result_compatible(
    actual: &TypeRef,
    expected: &TypeRef,
    ctx: &Ctx<'_, '_>,
) -> Option<bool> {
    if *expected == TypeRef::Void {
        return Some(true);
    }
    if *actual == TypeRef::Void {
        return Some(false);
    }
    if matches!(expected, TypeRef::Variable(_)) {
        return Some(true);
    }
    // An unresolvable target result (a type argument whose class isn't on the
    // classpath) constrains nothing: it can neither select nor reject a
    // candidate, since Java never overloads on return type alone. Same
    // reasoning as the unsubstituted type variable above.
    if *expected == TypeRef::Unknown {
        return Some(true);
    }
    convertible(actual, expected, Phase::Loose, ctx)
}

fn resolved_reference(
    candidate: &Candidate,
    name: &str,
    result: TypeRef,
    constructor: bool,
) -> Option<ReferenceResolution> {
    Some(ReferenceResolution {
        name: name.to_string(),
        parameters: candidate
            .meta
            .parameters
            .as_ref()?
            .iter()
            .map(|parameter| parameter.substitute(&candidate.env))
            .collect(),
        result,
        result_display: candidate.result_display.clone(),
        constructor,
    })
}

/// How strictly a method reference must be resolved.
#[derive(Clone, Copy, PartialEq)]
enum ReferenceMode {
    /// Diagnostics: every constraint must be proved. An unresolvable target
    /// parameter type yields "unknown", never a guess.
    Proof,
    /// Hover/signature rendering: an unresolvable target parameter type
    /// constrains nothing, so it is skipped. Nothing is reported to the
    /// user as an error from this path.
    Display,
}

/// Whether a descriptor argument carries no usable type at all.
fn argument_is_unknown(argument: &Argument) -> bool {
    matches!(argument, Argument::Value(value) if value.contains_unknown())
}

fn method_reference_resolution<'t>(
    reference: Node<'t>,
    target: &TypeRef,
    ctx: &Ctx<'_, 't>,
    mode: ReferenceMode,
) -> Option<ReferenceResolution> {
    let descriptor = functional_descriptor(target, ctx)?;
    let children = named_children(reference);
    let qualifier = *children.first()?;
    let qualifier_type = crate::resolve::resolve_receiver_type(qualifier, ctx)?;
    let receiver = qualifier_type.ty.type_ref();
    let descriptor_args: Vec<Argument> = descriptor
        .parameters
        .iter()
        .cloned()
        .map(Argument::Value)
        .collect();

    if node_text(reference, ctx.doc.source)
        .trim_end()
        .ends_with("::new")
    {
        if !qualifier_type.static_only {
            return None;
        }
        let TypeRef::Named { id, args } = &receiver else {
            return None;
        };
        let facts = class_facts(id, ctx)?;
        if !facts.meta.constructors_complete {
            return None;
        }
        let env = type_environment(&facts.meta.type_parameters, args)?;
        let candidates: Vec<Candidate> = facts
            .members
            .iter()
            .filter(|member| member.kind == ExternalMemberKind::Constructor)
            .filter_map(|member| member.metadata.clone())
            .map(|meta| Candidate {
                meta,
                env: env.clone(),
                result_display: None,
                declared_signature: None,
            })
            .collect();
        let candidate = match select_candidate(&candidates, &descriptor_args, ctx) {
            Selection::Selected(candidate, _) => candidate,
            // Display mode with a single constructor: there is nothing to
            // disambiguate, so an unresolvable target parameter must not
            // suppress the signature the reader asked for.
            _ if mode == ReferenceMode::Display && candidates.len() == 1 => candidates.first()?,
            _ => return None,
        };
        // Proof mode demands the result actually fits; display mode only
        // needs it not to be provably wrong.
        match reference_result_compatible(&receiver, &descriptor.result, ctx) {
            Some(true) => {}
            None if mode == ReferenceMode::Display => {}
            _ => return None,
        }
        let name = id.as_named()?.rsplit(['.', '$']).next()?;
        return resolved_reference(candidate, name, receiver.clone(), true);
    }

    let name_node = children
        .iter()
        .rev()
        .find(|node| node.kind() == "identifier")?;
    let name = node_text(*name_node, ctx.doc.source);
    let candidates = method_candidates(&receiver, name, false, ctx)?;
    for phase in [Phase::Strict, Phase::Loose, Phase::Varargs] {
        let mut applicable = Vec::new();
        let mut unknown = false;
        for candidate in &candidates {
            let call_args = if qualifier_type.static_only && !candidate.meta.is_static {
                let Some((receiver_arg, remaining)) = descriptor_args.split_first() else {
                    continue;
                };
                let Argument::Value(receiver_arg) = receiver_arg else {
                    unreachable!("descriptor arguments are values");
                };
                match convertible(receiver_arg, &receiver, Phase::Strict, ctx) {
                    Some(true) => remaining,
                    Some(false) => continue,
                    None => {
                        unknown = true;
                        continue;
                    }
                }
            } else if qualifier_type.static_only == candidate.meta.is_static {
                &descriptor_args
            } else {
                continue;
            };
            // Display mode: a target parameter that lowered to `Unknown`
            // (its class isn't on the classpath) can neither select nor
            // reject this candidate, so substitute the candidate's own
            // declared parameter — an identity check. Ambiguity between
            // surviving overloads still yields nothing, below.
            let relaxed: Vec<Argument>;
            let call_args =
                if mode == ReferenceMode::Display && call_args.iter().any(argument_is_unknown) {
                    relaxed = call_args
                        .iter()
                        .enumerate()
                        .map(|(index, argument)| match argument {
                            Argument::Value(value) if value.contains_unknown() => {
                                match selected_parameter(candidate, phase, index) {
                                    Some(parameter) => Argument::Value(parameter),
                                    None => Argument::Value(value.clone()),
                                }
                            }
                            other => other.clone(),
                        })
                        .collect();
                    &relaxed
                } else {
                    call_args
                };
            match applicable_in(candidate, call_args, phase, ctx) {
                Some(true) => {
                    let result = candidate.meta.result.substitute(&candidate.env);
                    match reference_result_compatible(&result, &descriptor.result, ctx) {
                        Some(true) => applicable.push(candidate),
                        Some(false) => {}
                        None => unknown = true,
                    }
                }
                Some(false) => {}
                None => unknown = true,
            }
        }
        if applicable.is_empty() {
            if unknown {
                return None;
            }
            continue;
        }
        if unknown {
            return None;
        }
        // No lambda arguments exist here by construction: a method
        // reference's arguments are synthesized from the target descriptor,
        // so JLS 15.12.2.5's lambda clause cannot apply.
        let candidate = most_specific(&applicable, &[], ctx)?;
        let result = candidate.meta.result.substitute(&candidate.env);
        return resolved_reference(candidate, name, result, false);
    }
    None
}

pub(crate) fn method_reference_signature<'t>(
    reference: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<String> {
    let target = target_type(reference, ctx, 0)?;
    let resolved = method_reference_resolution(reference, &target, ctx, ReferenceMode::Display)?;
    let parameters = resolved
        .parameters
        .iter()
        .map(|parameter| crate::resolve::render_type_ref_in(parameter, ctx))
        .collect::<Vec<_>>()
        .join(", ");
    if resolved.constructor {
        return Some(format!("{}({parameters})", resolved.name));
    }
    // `Unknown` renders as `?`; the declared text (`String`) is what the
    // reader asked for when the class simply isn't on the classpath.
    let result = match (&resolved.result, &resolved.result_display) {
        (TypeRef::Unknown, Some(display)) => display.clone(),
        (result, _) => crate::resolve::render_type_ref_in(result, ctx),
    };
    Some(format!("{result} {}({parameters})", resolved.name))
}

pub(crate) fn inferred_lambda_parameter_type<'t>(
    parameter: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<TypeRef> {
    let mut current = Some(parameter);
    let lambda = loop {
        let node = current?;
        if node.kind() == "lambda_expression" {
            break node;
        }
        current = node.parent();
    };
    let index = lambda_parameters(lambda).iter().position(|candidate| {
        candidate.id() == parameter.id()
            || candidate
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == parameter.id())
    })?;
    let target = target_type(lambda, ctx, 0)?;
    functional_descriptor(&target, ctx)?
        .parameters
        .get(index)
        .cloned()
}

fn target_type<'t>(expression: Node<'t>, ctx: &Ctx<'_, 't>, depth: usize) -> Option<TypeRef> {
    if depth > 64 {
        return None;
    }
    let parent = expression.parent()?;
    match parent.kind() {
        "argument_list" => {
            let call = parent.parent()?;
            expected_argument_type(call, expression, ctx)
        }
        "variable_declarator"
            if parent
                .child_by_field_name("value")
                .is_some_and(|value| value.id() == expression.id()) =>
        {
            let declaration = parent.parent()?;
            let type_node = declaration.child_by_field_name("type")?;
            crate::resolve::resolve_type_node(type_node, ctx.doc.source, ctx)
                .map(|resolved| resolved.type_ref())
        }
        "assignment_expression"
            if parent
                .child_by_field_name("right")
                .is_some_and(|right| right.id() == expression.id()) =>
        {
            crate::resolve::resolve_expression_type(parent.child_by_field_name("left")?, ctx)
                .map(|resolved| resolved.type_ref())
        }
        "cast_expression"
            if parent
                .child_by_field_name("value")
                .is_some_and(|value| value.id() == expression.id()) =>
        {
            crate::resolve::resolve_type_node(
                parent.child_by_field_name("type")?,
                ctx.doc.source,
                ctx,
            )
            .map(|resolved| resolved.type_ref())
        }
        "parenthesized_expression" | "ternary_expression" => target_type(parent, ctx, depth + 1),
        "return_statement" => {
            let mut ancestor = parent.parent();
            while let Some(node) = ancestor {
                match node.kind() {
                    "lambda_expression" => {
                        let target = target_type(node, ctx, depth + 1)?;
                        return Some(functional_descriptor(&target, ctx)?.result);
                    }
                    "method_declaration" => {
                        return crate::resolve::resolve_type_node(
                            node.child_by_field_name("type")?,
                            ctx.doc.source,
                            ctx,
                        )
                        .map(|resolved| resolved.type_ref());
                    }
                    _ => ancestor = node.parent(),
                }
            }
            None
        }
        _ => None,
    }
}

/// Resolve a `recv.name(args)` / unqualified `name(args)` call to its
/// selected overload. Whether a member named `name` exists at all is
/// [`crate::diagnostics::check_member`]'s job; this only judges argument
/// applicability among candidates that do exist.
pub(crate) fn resolve_method_call<'t>(call: Node<'t>, ctx: &Ctx<'_, 't>) -> CallResolution {
    let Some(_nesting) = ctx.facts.enter() else {
        return CallResolution::Unknown;
    };
    let Some((candidates, args)) = method_call_inputs(call, ctx, false) else {
        return CallResolution::Unknown;
    };
    if candidates.is_empty() {
        return CallResolution::Unknown;
    }
    select(candidates, &args, ctx)
}

/// The selected overload's signature as instantiated at this call site.
/// Unlike declaration hover (`T first()`), this applies both the receiver's
/// class arguments and method arguments inferred from the invocation
/// (`Shelter<Dog>.first()` -> `Dog first()`). `None` when nothing is
/// substituted or a variable stays unbound: the declaration rendering
/// (verbatim source, `E` by name) is then the better hover.
pub(crate) fn contextual_method_call_signature<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<String> {
    let (candidates, arguments) = method_call_inputs(call, ctx, false)?;
    let Selection::Selected(candidate, _) = select_candidate(&candidates, &arguments, ctx) else {
        return None;
    };
    let declared_parameters = candidate.meta.parameters.as_ref()?;
    if !has_type_variable(&candidate.meta.result)
        && !declared_parameters.iter().any(has_type_variable)
    {
        return None;
    }
    let result = match candidate.meta.result.substitute(&candidate.env) {
        TypeRef::Wildcard {
            upper: Some(upper), ..
        } => *upper,
        TypeRef::Wildcard { .. } => TypeRef::named("java.lang.Object"),
        other => other,
    };
    let parameters: Vec<TypeRef> = declared_parameters
        .iter()
        .map(|parameter| parameter.substitute(&candidate.env))
        .collect();
    if !renders_concretely(&result) || !parameters.iter().all(renders_concretely) {
        return None;
    }

    let names = candidate
        .declared_signature
        .as_deref()
        .map(declared_parameter_names)
        .unwrap_or_default();
    let rendered_parameters = parameters
        .iter()
        .enumerate()
        .map(|(index, parameter)| {
            let ty = if candidate.meta.is_varargs && index + 1 == parameters.len() {
                match parameter {
                    TypeRef::Array(element) => format!("{}...", render_type_ref_in(element, ctx)),
                    other => render_type_ref_in(other, ctx),
                }
            } else {
                render_type_ref_in(parameter, ctx)
            };
            names
                .get(index)
                .and_then(Option::as_deref)
                .map_or(ty.clone(), |name| format!("{ty} {name}"))
        })
        .collect::<Vec<_>>()
        .join(", ");

    let result = render_type_ref_in(&result, ctx);
    let name = node_text(call.child_by_field_name("name")?, ctx.doc.source);
    let lead = if candidate.meta.is_static {
        format!("static {result}")
    } else {
        result
    };
    Some(format!("{lead} {name}({rendered_parameters})"))
}

fn has_type_variable(ty: &TypeRef) -> bool {
    match ty {
        TypeRef::Variable(_) => true,
        TypeRef::Named { args, .. } => args.iter().any(has_type_variable),
        TypeRef::Array(element) => has_type_variable(element),
        TypeRef::Wildcard { upper, lower } => {
            upper.as_deref().is_some_and(has_type_variable)
                || lower.as_deref().is_some_and(has_type_variable)
        }
        _ => false,
    }
}

/// Whether every position of `ty` has a concrete rendering: an unbound
/// variable or an unresolved type would show as `?`, which is worse than
/// the declaration's own text.
fn renders_concretely(ty: &TypeRef) -> bool {
    match ty {
        TypeRef::Variable(_) | TypeRef::Unknown => false,
        TypeRef::Named { args, .. } => args.iter().all(renders_concretely),
        TypeRef::Array(element) => renders_concretely(element),
        TypeRef::Wildcard { upper, lower } => {
            upper.as_deref().is_none_or(renders_concretely)
                && lower.as_deref().is_none_or(renders_concretely)
        }
        _ => true,
    }
}

/// Recover source parameter names from an erased declaration label. External
/// bytecode signatures generally contain only types and therefore yield
/// `None` entries.
fn declared_parameter_names(signature: &str) -> Vec<Option<String>> {
    let Some(start) = signature.find('(') else {
        return Vec::new();
    };
    let Some(end) = signature.rfind(')') else {
        return Vec::new();
    };
    if end <= start + 1 {
        return Vec::new();
    }
    let inner = &signature[start + 1..end];
    let mut labels = Vec::new();
    let mut depth = 0i32;
    let mut segment = 0usize;
    for (index, ch) in inner.char_indices() {
        match ch {
            '<' | '[' | '(' => depth += 1,
            '>' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                labels.push(&inner[segment..index]);
                segment = index + 1;
            }
            _ => {}
        }
    }
    labels.push(&inner[segment..]);
    labels
        .into_iter()
        .map(|label| {
            let mut words = label.split_whitespace();
            let first = words.next()?;
            let last = words.last()?;
            if first == last {
                return None;
            }
            let name = last.trim_end_matches("[]");
            (!name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch == '_' || ch == '$' || ch.is_alphanumeric()))
            .then(|| name.to_string())
        })
        .collect()
}
fn method_call_inputs<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
    contextual: bool,
) -> Option<(Vec<Candidate>, Vec<Argument<'t>>)> {
    let name = node_text(call.child_by_field_name("name")?, ctx.doc.source);
    let (receiver, static_only) = match call.child_by_field_name("object") {
        Some(object) => {
            let resolved = crate::resolve::resolve_checked_receiver(object, ctx).or_else(|| {
                contextual.then(|| crate::resolve::resolve_receiver_type(object, ctx))?
            })?;
            (resolved.ty.type_ref(), resolved.static_only)
        }
        None => {
            let declaration = crate::resolve::enclosing_typedecl(call, ctx.table, ctx.current)?;
            (
                TypeRef::Named {
                    id: declaration.type_id,
                    args: Vec::new(),
                },
                false,
            )
        }
    };
    let args = arguments(call, ctx)?;
    let mut candidates = method_candidates(&receiver, name, static_only, ctx)?;
    for candidate in &mut candidates {
        infer_method_type_arguments(candidate, &args, ctx);
    }
    Some((candidates, args))
}

/// Infer a generic method's own type variables from proper argument types
/// before applicability and return-type substitution. Class variables are
/// already in `candidate.env`; method variables are separate identities and
/// are appended here.
///
/// This is deliberately constraint-shaped rather than name-shaped:
/// `<E> Stream<E> of(E value)` binds `E` from `Dog`, including through
/// arrays and same-raw-type generic arguments. Conflicting lower bounds are
/// left unbound until this tier has a least-upper-bound implementation.
fn infer_method_type_arguments(
    candidate: &mut Candidate,
    args: &[Argument<'_>],
    ctx: &Ctx<'_, '_>,
) {
    if candidate.meta.type_parameters.is_empty() {
        return;
    }
    let Some(parameters) = &candidate.meta.parameters else {
        return;
    };
    let mut inferred: Vec<(TypeVariableId, Option<TypeRef>)> = Vec::new();
    let vararg_index = parameters.len().saturating_sub(1);
    for (index, argument) in args.iter().enumerate() {
        let Argument::Value(actual) = argument else {
            continue;
        };
        let Some(mut formal) = parameters.get(index).or_else(|| parameters.last()) else {
            continue;
        };
        if candidate.meta.is_varargs && index >= vararg_index {
            if let TypeRef::Array(element) = formal {
                // A lone array argument uses the declared array parameter;
                // ordinary trailing arguments use its element type.
                if !(args.len() == parameters.len() && matches!(actual, TypeRef::Array(_))) {
                    formal = element;
                }
            }
        }
        let formal = formal.substitute(&candidate.env);
        collect_method_inference(
            &candidate.meta.type_parameters,
            &formal,
            actual,
            &mut inferred,
            ctx,
        );
    }

    for (id, actual) in inferred {
        let Some(actual) = actual else {
            continue;
        };
        let Some(parameter) = candidate.meta.type_parameters.iter().find(|p| p.id == id) else {
            continue;
        };
        // A bound we can disprove (or cannot resolve) must not turn into a
        // guessed successful invocation. Leaving the variable unbound keeps
        // the existing conservative Unknown behavior.
        let bounds_hold = parameter.bounds.iter().all(|bound| {
            assignable_refs(&actual, &bound.substitute(&candidate.env), ctx) == Some(true)
        });
        if bounds_hold {
            candidate.env.push((id, actual));
        }
    }
}

/// Collect equality-shaped constraints for method type variables. Java's
/// full inference lattice is larger; these cases cover direct parameters,
/// arrays/varargs, and nested arguments of the same generic raw type without
/// manufacturing a least upper bound.
fn collect_method_inference(
    method_parameters: &[TypeParameter],
    formal: &TypeRef,
    actual: &TypeRef,
    inferred: &mut Vec<(TypeVariableId, Option<TypeRef>)>,
    ctx: &Ctx<'_, '_>,
) {
    let actual = match actual {
        TypeRef::Wildcard {
            upper: Some(upper), ..
        } => upper.as_ref(),
        TypeRef::Wildcard {
            lower: Some(lower), ..
        } => lower.as_ref(),
        TypeRef::Wildcard { .. } | TypeRef::Unknown | TypeRef::Null => return,
        _ => actual,
    };
    match (formal, actual) {
        (TypeRef::Variable(id), actual) if method_parameters.iter().any(|p| p.id == *id) => {
            let actual = match actual {
                TypeRef::Primitive(primitive) => TypeRef::named(primitive.box_fqn()),
                other => other.clone(),
            };
            if let Some((_, current)) = inferred.iter_mut().find(|(candidate, _)| candidate == id) {
                if current.as_ref().is_some_and(|previous| previous != &actual) {
                    *current = None;
                }
            } else {
                inferred.push((id.clone(), Some(actual)));
            }
        }
        (TypeRef::Array(formal), TypeRef::Array(actual)) => {
            collect_method_inference(method_parameters, formal, actual, inferred, ctx);
        }
        (
            TypeRef::Named {
                id: formal_id,
                args: formal_args,
            },
            TypeRef::Named {
                id: actual_id,
                args: actual_args,
            },
        ) if formal_id == actual_id && formal_args.len() == actual_args.len() => {
            for (formal, actual) in formal_args.iter().zip(actual_args) {
                collect_method_inference(method_parameters, formal, actual, inferred, ctx);
            }
        }
        (
            formal @ TypeRef::Named { id: formal_id, .. },
            actual @ TypeRef::Named { id: actual_id, .. },
        ) if formal_id != actual_id => {
            if let Some(projected) = type_as_supertype(actual, formal_id, ctx) {
                collect_method_inference(method_parameters, formal, &projected, inferred, ctx);
            }
        }
        (
            TypeRef::Wildcard {
                upper: Some(formal),
                ..
            },
            _,
        ) => {
            collect_method_inference(method_parameters, formal, actual, inferred, ctx);
        }
        (
            TypeRef::Wildcard {
                lower: Some(formal),
                ..
            },
            _,
        ) => {
            collect_method_inference(method_parameters, formal, actual, inferred, ctx);
        }
        _ => {}
    }
}

/// View `actual` through a named supertype while preserving substituted type
/// arguments: `ArrayList<Dog>` as `Collection<Dog>`. This is the constraint
/// shape generic factories such as `List.copyOf(Collection<? extends E>)`
/// need in order to infer `E`.
fn type_as_supertype(actual: &TypeRef, target: &TypeId, ctx: &Ctx<'_, '_>) -> Option<TypeRef> {
    let mut stack = vec![(actual.clone(), 0usize)];
    let mut visited = HashSet::new();
    while let Some((current, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(current.clone()) {
            continue;
        }
        let TypeRef::Named { id, args } = &current else {
            continue;
        };
        if id == target {
            return Some(current);
        }
        let Some(facts) = class_facts(id, ctx) else {
            continue;
        };
        if !facts.meta.hierarchy_complete {
            continue;
        }
        let env: Vec<_> = facts
            .meta
            .type_parameters
            .iter()
            .map(|parameter| parameter.id.clone())
            .zip(args.iter().cloned())
            .collect();
        stack.extend(
            facts
                .meta
                .supertypes
                .iter()
                .map(|supertype| (supertype.substitute(&env), depth + 1)),
        );
    }
    None
}

/// A generic in-project member's declared type as seen through `recv`'s type
/// arguments: `T item` / `T first()` on `Shelter<Dog>` — or on a
/// `Kennel extends Shelter<Dog>` — is `Dog`; a raw `Shelter` erases it to
/// the variable's bound. `None` when the member's declared type has no type
/// variable (the source node is authoritative then), the receiver can't be
/// projected onto the declaring class, or a variable stays unbound.
pub(crate) fn member_type_through(
    recv: &TypeRef,
    declaring: &TypeId,
    name: &str,
    kind: ExternalMemberKind,
    ctx: &Ctx<'_, '_>,
) -> Option<TypeRef> {
    let facts = class_facts(declaring, ctx)?;
    let member = facts
        .members
        .iter()
        .find(|member| member.name == name && member.kind == kind)?;
    let declared = &member.metadata.as_ref()?.result;
    if !has_type_variable(declared) {
        return None;
    }
    let TypeRef::Named { args, .. } = type_as_supertype(recv, declaring, ctx)? else {
        return None;
    };
    let env = type_environment(&facts.meta.type_parameters, &args)?;
    let substituted = declared.substitute(&env);
    (!has_type_variable(&substituted)).then_some(substituted)
}

fn selected_parameter(candidate: &Candidate, phase: Phase, index: usize) -> Option<TypeRef> {
    let parameters: Vec<TypeRef> = candidate
        .meta
        .parameters
        .as_ref()?
        .iter()
        .map(|parameter| parameter.substitute(&candidate.env))
        .collect();
    if phase == Phase::Varargs && index >= parameters.len().saturating_sub(1) {
        let TypeRef::Array(element) = parameters.last()? else {
            return None;
        };
        return Some((**element).clone());
    }
    parameters.get(index).cloned()
}

fn argument_index(arguments: Node, argument: Node) -> Option<usize> {
    named_children(arguments)
        .into_iter()
        .filter(|node| !matches!(node.kind(), "line_comment" | "block_comment"))
        .position(|node| node.id() == argument.id())
}

fn expected_argument_type<'t>(
    call: Node<'t>,
    argument: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<TypeRef> {
    let arguments_node = call.child_by_field_name("arguments")?;
    let index = argument_index(arguments_node, argument)?;
    match call.kind() {
        "method_invocation" => {
            let (candidates, arguments) = method_call_inputs(call, ctx, true)?;
            let Selection::Selected(candidate, phase) =
                select_candidate(&candidates, &arguments, ctx)
            else {
                return None;
            };
            selected_parameter(candidate, phase, index)
        }
        "object_creation_expression" => {
            let created = crate::resolve::resolve_object_creation_type(call, ctx)?;
            let TypeRef::Named { id, args } = created.type_ref() else {
                return None;
            };
            let facts = class_facts(&id, ctx)?;
            let arguments = arguments(call, ctx)?;
            let diamond = call
                .child_by_field_name("type")
                .is_some_and(|node| node_text(node, ctx.doc.source).ends_with("<>"));
            let fixed_env = type_environment(&facts.meta.type_parameters, &args)?;
            let candidates: Vec<Candidate> = facts
                .members
                .iter()
                .filter(|member| member.kind == ExternalMemberKind::Constructor)
                .filter_map(|member| member.metadata.clone())
                .map(|meta| {
                    let env = if diamond {
                        infer_env_from_args(&facts.meta.type_parameters, &meta, &arguments)
                    } else {
                        fixed_env.clone()
                    };
                    Candidate {
                        meta,
                        env,
                        result_display: None,
                        declared_signature: None,
                    }
                })
                .collect();
            let Selection::Selected(candidate, phase) =
                select_candidate(&candidates, &arguments, ctx)
            else {
                return None;
            };
            selected_parameter(candidate, phase, index)
        }
        _ => None,
    }
}

/// Why an `object_creation_expression` cannot be instantiated, independent
/// of argument applicability (checked before constructor selection even
/// runs).
pub(crate) enum InstantiationError {
    Abstract(ClassKind),
    Inaccessible,
    NeedsEnclosingInstance,
}

/// Whether `call` (an `object_creation_expression`) is the qualified form
/// (`primary.new Type(...)`) rather than plain `new Type(...)`.
/// tree-sitter-java exposes no named field for the qualifier, so the
/// signal is the node's first child: the `new` keyword for the
/// unqualified form, the qualifying expression otherwise.
fn is_qualified_creation(call: Node) -> bool {
    call.child(0).is_some_and(|c| c.kind() != "new")
}

/// The substitution a diamond constructor call infers for the class's own
/// type parameters: each one that appears as the exact type of a
/// constructor parameter is bound to that argument's type. Type variables
/// that never appear directly as a parameter type are left unbound.
fn infer_env_from_args(
    class_params: &[TypeParameter],
    meta: &MemberMetadata,
    args: &[Argument<'_>],
) -> Vec<(TypeVariableId, TypeRef)> {
    let Some(params) = &meta.parameters else {
        return Vec::new();
    };
    class_params
        .iter()
        .filter_map(|type_parameter| {
            params
                .iter()
                .zip(args)
                .find_map(|(parameter, argument)| {
                    let Argument::Value(value) = argument else {
                        return None;
                    };
                    matches!(
                        parameter,
                        TypeRef::Variable(variable) if *variable == type_parameter.id
                    )
                    .then(|| value.clone())
                })
                .map(|found| (type_parameter.id.clone(), found))
        })
        .collect()
}

/// The type a diamond (`new Type<>(...)`) creation instantiates to, once
/// its constructor is selected. Prefers the constructor-inferred
/// substitution; falls back to the declared variable's own type arguments
/// (e.g. `Box<User> b = new Box<>(...)`); otherwise raw — never a wrong
/// guess.
fn infer_diamond<'t>(
    facts: &ClassFacts,
    selected: &Candidate,
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> TypeRef {
    let id = facts.meta.id.clone();
    if !facts.meta.type_parameters.is_empty()
        && selected.env.len() == facts.meta.type_parameters.len()
    {
        let args = facts
            .meta
            .type_parameters
            .iter()
            .map(|tp| {
                selected
                    .env
                    .iter()
                    .find(|(k, _)| *k == tp.id)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(TypeRef::Unknown)
            })
            .collect();
        return TypeRef::Named { id, args };
    }
    let declared = call
        .parent()
        .filter(|p| {
            p.kind() == "variable_declarator"
                && p.child_by_field_name("value")
                    .is_some_and(|v| v.id() == call.id())
        })
        .and_then(|declarator| declarator.parent())
        .and_then(|d| d.child_by_field_name("type"))
        .and_then(|decl_type| crate::resolve::resolve_type_node(decl_type, ctx.doc.source, ctx));
    if let Some(resolved) = declared {
        if let TypeRef::Named {
            id: declared_id,
            args,
        } = resolved.type_ref()
        {
            if declared_id == id && !args.is_empty() {
                return TypeRef::Named { id, args };
            }
        }
    }
    TypeRef::Named {
        id,
        args: Vec::new(),
    }
}

/// Whether a constructor with `access`, declared in `declaring`, is
/// callable from `call`'s site: public is always callable, private only
/// within the same top-level nest, package-private/protected only within
/// the same package. A cross-package protected call is left to javac
/// (`None`) — `None` always means "stay silent", never "assume accessible".
fn constructor_accessible(
    access: Access,
    declaring: &TypeId,
    call: Node,
    ctx: &Ctx<'_, '_>,
) -> Option<bool> {
    match access {
        Access::Public => Some(true),
        Access::Private => {
            let declaring_nest = declaring.as_named()?.split('$').next()?;
            let caller_nest = crate::resolve::enclosing_binary_names(call, ctx)
                .into_iter()
                .find_map(|b| b.split('$').next().map(str::to_string));
            Some(caller_nest.as_deref() == Some(declaring_nest))
        }
        Access::Package | Access::Protected => {
            let declaring_name = declaring.as_named()?;
            let declaring_pkg = declaring_name
                .rsplit_once('$')
                .map(|(outer, _)| outer)
                .unwrap_or(declaring_name)
                .rsplit_once('.')
                .map(|(pkg, _)| pkg)
                .unwrap_or("");
            let caller_pkg = ctx.imports.package().unwrap_or("");
            if caller_pkg == declaring_pkg {
                Some(true)
            } else if access == Access::Protected {
                None
            } else {
                Some(false)
            }
        }
    }
}

/// Resolve an `object_creation_expression`'s constructor call:
/// instantiability as an [`InstantiationError`], then argument-applicable
/// constructor selection (including diamond inference) as a
/// [`CallResolution`]. Any unresolvable input yields
/// `Ok(CallResolution::Unknown)`.
pub(crate) fn resolve_constructor_call<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Result<CallResolution, InstantiationError> {
    let Some(_nesting) = ctx.facts.enter() else {
        return Ok(CallResolution::Unknown);
    };
    let Some(created) = crate::resolve::resolve_object_creation_type(call, ctx) else {
        return Ok(CallResolution::Unknown);
    };
    let created_ref = created.type_ref();
    let TypeRef::Named { id, args } = &created_ref else {
        return Ok(CallResolution::Unknown);
    };
    let Some(facts) = class_facts(id, ctx) else {
        return Ok(CallResolution::Unknown);
    };
    let anonymous = named_children(call)
        .into_iter()
        .any(|c| c.kind() == "class_body");
    match facts.meta.kind {
        ClassKind::Interface | ClassKind::Annotation if !anonymous => {
            return Err(InstantiationError::Abstract(facts.meta.kind));
        }
        // An anonymous class implementing an interface/annotation supplies
        // no constructor arguments of its own — `Object()` is the only
        // applicable "constructor" and it always succeeds.
        ClassKind::Interface | ClassKind::Annotation => {
            return Ok(CallResolution::Selected {
                result: created_ref.clone(),
                declaring: id.clone(),
                access: Access::Public,
            });
        }
        ClassKind::Enum => return Err(InstantiationError::Abstract(ClassKind::Enum)),
        ClassKind::Class if facts.meta.is_abstract && !anonymous => {
            return Err(InstantiationError::Abstract(ClassKind::Class));
        }
        _ => {}
    }
    if !facts.meta.is_static && facts.meta.enclosing_class.is_some() {
        let qualified = is_qualified_creation(call)
            || crate::resolve::enclosing_binary_names(call, ctx)
                .iter()
                .any(|b| {
                    Some(b.as_str())
                        == facts
                            .meta
                            .enclosing_class
                            .as_ref()
                            .and_then(TypeId::as_named)
                });
        if !qualified {
            return Err(InstantiationError::NeedsEnclosingInstance);
        }
    }
    if !facts.meta.constructors_complete {
        return Ok(CallResolution::Unknown);
    }
    let Some(call_args) = arguments(call, ctx) else {
        return Ok(CallResolution::Unknown);
    };
    let diamond = call
        .child_by_field_name("type")
        .is_some_and(|t| node_text(t, ctx.doc.source).ends_with("<>"));
    let fixed_env: Vec<(TypeVariableId, TypeRef)> = facts
        .meta
        .type_parameters
        .iter()
        .map(|p| p.id.clone())
        .zip(args.iter().cloned())
        .collect();
    let candidates: Vec<Candidate> = facts
        .members
        .iter()
        .filter(|m| m.kind == ExternalMemberKind::Constructor)
        .filter_map(|m| m.metadata.clone())
        .map(|meta| {
            // Diamond: each candidate infers its own class-variable bindings
            // from its own parameters, since no shared use-site substitution
            // exists yet. Non-diamond candidates share the receiver's type
            // arguments.
            let env = if diamond {
                infer_env_from_args(&facts.meta.type_parameters, &meta, &call_args)
            } else {
                fixed_env.clone()
            };
            Candidate {
                meta,
                env,
                result_display: None,
                declared_signature: None,
            }
        })
        .collect();
    if candidates.is_empty() {
        return Ok(CallResolution::Unknown);
    }
    match select_candidate(&candidates, &call_args, ctx) {
        Selection::Selected(c, _) => {
            match constructor_accessible(c.meta.access, &c.meta.declaring_class, call, ctx) {
                Some(true) => {}
                Some(false) => return Err(InstantiationError::Inaccessible),
                None => return Ok(CallResolution::Unknown),
            }
            let result = if diamond {
                infer_diamond(&facts, c, call, ctx)
            } else {
                created_ref.clone()
            };
            Ok(CallResolution::Selected {
                result,
                declaring: c.meta.declaring_class.clone(),
                access: c.meta.access,
            })
        }
        Selection::NoApplicable(n) => Ok(CallResolution::NoApplicable { candidates: n }),
        Selection::Ambiguous => Ok(CallResolution::Ambiguous),
        Selection::Unknown => Ok(CallResolution::Unknown),
    }
}
