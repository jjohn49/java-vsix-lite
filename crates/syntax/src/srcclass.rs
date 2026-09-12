//! Turns a closed project `.java` file's source into an [`ExternalClass`], the same
//! shape `jvl-classpath` produces from bytecode, so unopened workspace types still get
//! full completion, chains, and hover. `jvl-syntax` stays IO-free: the server parses the
//! file once and hands this module the parsed [`OpenDoc`] and type path.

use tree_sitter::Node;

use jvl_types::{
    Access, ClassKind, ClassMetadata, MemberMetadata, TypeId, TypeRef, TypeVariableId,
};

use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind};
use crate::imports::Imports;
use crate::model::{has_modifier, named_children, MemberKind, TypeDecl, TypeKind};
use crate::{node_text, OpenDoc};

/// Extracts `type_path` (`"Person"`, or `"Outer.Inner"` for nested) from an already-parsed
/// document as an [`ExternalClass`], resolving supertypes/member types through the
/// declaring file's own imports via `pick_fqn`. `None` if the document doesn't declare
/// that type.
pub fn class_from_doc(
    doc: &OpenDoc<'_>,
    type_path: &str,
    pick_fqn: &dyn Fn(&[String]) -> Option<String>,
) -> Option<ExternalClass> {
    let source = doc.source;
    let root = doc.tree.root_node();
    let imports = Imports::parse(doc.tree, source);
    if let Some(td) = find_type_by_path(root, source, type_path, imports.package()) {
        return Some(to_external_class(&td, source, &imports, pick_fqn));
    }
    // `Outer.OuterBuilder` — the `@Builder` companion type that exists
    // only in Lombok's generated code, synthesized on demand so builder
    // chains (`Person.builder().name("x").build()`) resolve.
    let (outer_path, last) = type_path.rsplit_once('.')?;
    let outer = find_type_by_path(root, source, outer_path, imports.package())?;
    if last != format!("{}Builder", outer.name)
        || !crate::lombok::file_uses_lombok(outer.node, source)
    {
        return None;
    }
    crate::lombok::builder_class(&outer, source)
}

/// A hash of every top-level and member type's structural signature in `doc`, changing
/// only when a declaration that could affect another file's semantic checks changes
/// (member bodies aren't hashed). Used to decide whether dependents need re-checking.
pub fn declaration_fingerprint(doc: &OpenDoc<'_>) -> u64 {
    use std::hash::{Hash, Hasher};

    let imports = Imports::parse(doc.tree, doc.source);
    let package = imports.package().map(str::to_string);
    let pick_first = |candidates: &[String]| candidates.first().cloned();

    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![doc.tree.root_node()];
    while let Some(node) = stack.pop() {
        if let Some(td) = TypeDecl::from_node(node, doc.source, 0, package.as_deref()) {
            if let Some(binary) = td.binary_name.clone() {
                let ext = to_external_class(&td, doc.source, &imports, &pick_first);
                let shape = (
                    &ext.metadata,
                    ext.members
                        .iter()
                        .map(|m| (&m.name, m.kind, &m.metadata))
                        .collect::<Vec<_>>(),
                );
                let mut h = std::collections::hash_map::DefaultHasher::new();
                format!("{shape:?}").hash(&mut h);
                entries.push((binary, h.finish()));
            }
        }
        stack.extend(crate::model::children(node));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    entries.hash(&mut h);
    h.finish()
}

/// Walk from the file's top-level type declarations through `.`-separated
/// nested-type segments (`Outer.Inner.Deepest`) to the named [`TypeDecl`].
fn find_type_by_path<'t>(
    root: Node<'t>,
    source: &'t str,
    type_path: &str,
    package: Option<&str>,
) -> Option<TypeDecl<'t>> {
    let mut segments = type_path.split('.');
    let first = segments.next()?;
    let mut current = crate::model::named_children(root)
        .into_iter()
        .find_map(|n| TypeDecl::from_node(n, source, 0, package).filter(|td| td.name == first))?;
    for seg in segments {
        let member = current
            .own_members()
            .into_iter()
            .find(|m| m.name == seg && matches!(m.kind, MemberKind::NestedType(_)))?;
        current = TypeDecl::from_node(member.node, source, 0, package)?;
    }
    Some(current)
}

/// Per-member lowering context: everything [`member_from_node`] needs beyond
/// the member's own node, bundled to keep that function's signature sane.
struct MemberCtx<'a> {
    source: &'a str,
    imports: &'a Imports,
    pick_fqn: &'a dyn Fn(&[String]) -> Option<String>,
    binary_name: &'a str,
    td_kind: TypeKind,
    /// The declaring class's type parameters (name, id), innermost last —
    /// the base scope every member's own type-node lowering extends.
    class_names: &'a [(String, TypeVariableId)],
    resolve_named: &'a dyn Fn(&str, bool) -> Option<String>,
}

/// Whether a member declaration has a parse error in its *header* (modifiers,
/// name, type parameters, parameter list, return type). Errors inside the
/// body are ignored: a user typing inside a method must not turn the
/// enclosing class into "unknown" for every consumer.
fn signature_has_error(node: Node) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    let broken = node
        .children(&mut cursor)
        .any(|c| !matches!(c.kind(), "block" | "constructor_body") && c.has_error());
    broken
}

/// Whether the type body holds a member tree-sitter could not classify (an
/// `ERROR` node at member level). Such a member might be a constructor, so
/// the constructor set cannot be trusted as exhaustive.
fn body_has_unclassified_member(td: &TypeDecl) -> bool {
    named_children(td.node)
        .into_iter()
        .filter(|c| c.kind().ends_with("_body"))
        .flat_map(named_children)
        .flat_map(|m| {
            if m.kind() == "enum_body_declarations" {
                named_children(m)
            } else {
                vec![m]
            }
        })
        .any(|m| m.is_error())
}

/// Every member type visible by simple name from inside `decl` (JLS
/// 6.5.5.1): the types `decl` itself declares, then those of each
/// enclosing declaration, innermost first so an inner name shadows an
/// outer one. Returned as `(simple, binary)` pairs; binary names are
/// derived by trimming `$` segments off `binary_name` in step with the
/// ancestor walk, so no lookup is needed.
///
/// Inherited member types are deliberately absent: resolving them needs
/// the supertype hierarchy this function is used to build, and missing one
/// only costs silence.
fn nested_types_in_scope(decl: Node, source: &str, binary_name: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut scope_binary = binary_name;
    let mut current = Some(decl);
    while let Some(node) = current {
        for member in named_children(node)
            .into_iter()
            .filter(|c| c.kind().ends_with("_body"))
            .flat_map(named_children)
        {
            if TypeKind::from_kind(member.kind()).is_none() {
                continue;
            }
            let Some(name) = member.child_by_field_name("name") else {
                continue;
            };
            let simple = node_text(name, source);
            out.push((simple.to_string(), format!("{scope_binary}${simple}")));
        }
        // Step out one nesting level; a top-level declaration has no `$`.
        let Some((outer, _)) = scope_binary.rsplit_once('$') else {
            break;
        };
        scope_binary = outer;
        current = node
            .parent()
            .and_then(|p| p.parent())
            .filter(|p| TypeKind::from_kind(p.kind()).is_some());
    }
    out
}

/// Lowers `td` (an already-located type declaration) to its [`ExternalClass`] shape.
/// `pub(crate)`: also called directly by `resolve.rs`'s `class_facts`, which already
/// holds the `TypeDecl` and skips [`class_from_doc`]'s document round-trip.
pub(crate) fn to_external_class(
    td: &TypeDecl,
    source: &str,
    imports: &Imports,
    pick_fqn: &dyn Fn(&[String]) -> Option<String>,
) -> ExternalClass {
    let mut supers: Vec<String> = td
        .super_simple_names()
        .iter()
        .filter_map(|s| pick_fqn(&imports.candidates(s)))
        .collect();
    // Enums implicitly extend `java.lang.Enum` (source of `name()`/`ordinal()`/etc.);
    // `supers` itself holds only explicit interfaces.
    if td.kind == TypeKind::Enum {
        supers.insert(0, "java.lang.Enum".to_string());
    } else if supers.is_empty() {
        supers.push("java.lang.Object".to_string());
    }

    // `binary_name` is already correctly computed by `TypeDecl::from_node`; the
    // `unwrap_or` fallback is defensive and never actually exercised.
    let binary_name = td
        .binary_name
        .clone()
        .unwrap_or_else(|| td.name.to_string());
    let enclosing = binary_name
        .rsplit_once('$')
        .map(|(outer, _)| outer.to_string());

    // JLS 6.5.5.1: a member type of this class or of any enclosing class is
    // in scope by simple name and shadows imports. `Imports` is file-level,
    // so `Fn` inside `demo.Bad` (binary name `demo.Bad$Fn`) is named by no
    // import candidate. Member types always live in this same file, so the
    // whole scope is a pure tree walk — never an index or classpath probe,
    // which would cost a lookup per miss on the hot metadata path.
    let nested_scope = nested_types_in_scope(td.node, source, &binary_name);

    let resolve_named = |name: &str, dotted: bool| -> Option<String> {
        if dotted {
            // `a.b.Outer.Inner` -> try `a.b.Outer.Inner`, `a.b.Outer$Inner`,
            // `a.b$Outer$Inner`, ... — the first the project/classpath knows.
            if let Some(b) = crate::resolve::import_path_to_binary(name, |candidate| {
                pick_fqn(&[candidate.to_string()]).is_some()
            }) {
                return Some(b);
            }
            // `Outer.Inner` with no package: qualify `Outer` through imports.
            let (head, rest) = name.split_once('.')?;
            let outer = pick_fqn(&imports.candidates(head))?;
            let candidate = format!("{outer}${}", rest.replace('.', "$"));
            return pick_fqn(&[candidate]);
        }
        if let Some(found) = nested_scope
            .iter()
            .find(|(simple, _)| simple == name)
            .map(|(_, binary)| binary.clone())
        {
            return Some(found);
        }
        pick_fqn(&imports.candidates(name))
    };

    let (class_names, type_parameters) = crate::typeref::lower_type_parameters(
        td.node.child_by_field_name("type_parameters"),
        source,
        &binary_name,
        &[],
        &resolve_named,
    );
    let mut supertypes: Vec<TypeRef> = crate::model::super_type_nodes(td.node)
        .into_iter()
        .map(|n| crate::typeref::lower_type_node(n, source, &class_names, &resolve_named))
        .collect();
    match td.kind {
        TypeKind::Enum => supertypes.insert(
            0,
            TypeRef::named_with("java.lang.Enum", vec![TypeRef::named(&binary_name)]),
        ),
        TypeKind::Record => supertypes.insert(0, TypeRef::named("java.lang.Record")),
        TypeKind::Class
            if td.node.child_by_field_name("superclass").is_none()
                && !crate::model::super_type_nodes(td.node)
                    .iter()
                    .any(|n| n.parent().is_some_and(|p| p.kind() == "superclass")) =>
        {
            supertypes.insert(0, TypeRef::named("java.lang.Object"))
        }
        _ => {}
    }
    // Any error in the declaration header (a half-typed `extends`,
    // `implements`, `<T`, or an ERROR child where a clause should be) makes
    // the supertype list untrustworthy even if what parsed lowered cleanly.
    // Bodies (`*_body` children) are excluded on purpose.
    let header_broken = td.node.is_error()
        || named_children(td.node)
            .into_iter()
            .any(|c| !c.kind().ends_with("_body") && c.has_error());
    let hierarchy_complete = !header_broken && !supertypes.iter().any(TypeRef::contains_unknown);
    let kind = match td.kind {
        TypeKind::Class => ClassKind::Class,
        TypeKind::Interface => ClassKind::Interface,
        TypeKind::Enum => ClassKind::Enum,
        TypeKind::Record => ClassKind::Record,
        TypeKind::Annotation => ClassKind::Annotation,
    };
    let class_access = crate::typeref::access_of(td.node, source, Access::Package);
    // A constructor whose header is mid-edit, or an unclassifiable member
    // that might be one, means the set cannot be enumerated faithfully; a
    // missing entry would turn a valid `new X(...)` into a false
    // "no applicable constructor".
    let constructors_complete = td.constructors().iter().all(|c| !signature_has_error(*c))
        && td
            .compact_constructor()
            .is_none_or(|c| !signature_has_error(c))
        && !body_has_unclassified_member(td);
    let metadata = Some(ClassMetadata {
        id: TypeId::named(&binary_name),
        kind,
        access: class_access,
        is_abstract: has_modifier(td.node, source, "abstract")
            || matches!(kind, ClassKind::Interface | ClassKind::Annotation),
        is_static: enclosing.is_none()
            || has_modifier(td.node, source, "static")
            || kind != ClassKind::Class,
        enclosing_class: enclosing.as_deref().map(TypeId::named),
        type_parameters,
        supertypes,
        hierarchy_complete,
        constructors_complete,
    });

    let member_ctx = MemberCtx {
        source,
        imports,
        pick_fqn,
        binary_name: &binary_name,
        td_kind: td.kind,
        class_names: &class_names,
        resolve_named: &resolve_named,
    };
    let mut members: Vec<ExternalMember> = td
        .own_members()
        .into_iter()
        .filter(|m| externally_visible(m.node, source, td.kind))
        .filter_map(|m| member_from_node(m.node, m.kind, m.is_static, &member_ctx))
        .collect();
    members.extend(constructor_members(
        td,
        source,
        &binary_name,
        class_access,
        &class_names,
        &resolve_named,
    ));

    // Enums get compiler-synthesized `values()`/`valueOf(String)` statics, absent
    // from source text. Instance methods come from the `java.lang.Enum` supertype above.
    if td.kind == TypeKind::Enum {
        members.push(ExternalMember {
            name: "values".to_string(),
            kind: ExternalMemberKind::Method,
            signature: format!("{}[] values()", td.name),
            template: None,
            is_static: true,
            ret_fqn: None,
            ret_display: Some(format!("{}[]", td.name)),
            metadata: None,
        });
        members.push(ExternalMember {
            name: "valueOf".to_string(),
            kind: ExternalMemberKind::Method,
            signature: format!("{} valueOf(String)", td.name),
            template: None,
            is_static: true,
            ret_fqn: pick_fqn(&imports.candidates(td.name)),
            ret_display: Some(td.name.to_string()),
            metadata: None,
        });
    }

    // Lombok-generated accessors, gated the same as open documents; `synthesize`
    // never duplicates a declared method. Return FQN is recovered from the display
    // type via the declaring file's imports, same as `member_from_node`.
    if crate::lombok::file_uses_lombok(td.node, source) {
        for sm in crate::lombok::synthesize(td.node, source) {
            let mut m = sm.into_external();
            if m.ret_fqn.is_none() {
                m.ret_fqn = m
                    .ret_display
                    .as_deref()
                    .and_then(display_base_simple)
                    .and_then(|simple| pick_fqn(&imports.candidates(simple)));
            }
            members.push(m);
        }
    }

    ExternalClass {
        supers,
        type_params: class_names.iter().map(|(name, _)| name.clone()).collect(),
        members,
        metadata,
    }
}

/// Lower a `formal_parameters` node's children to structured parameter types, in order.
/// Handles a `formal_parameter`'s trailing C-style array dims (`int xs[]`) and a
/// `spread_parameter`'s implicit array wrap (`int... xs`).
fn lower_parameters(
    params_node: Option<Node>,
    source: &str,
    scope: &[(String, TypeVariableId)],
    resolve_named: &dyn Fn(&str, bool) -> Option<String>,
) -> (Vec<TypeRef>, bool) {
    let Some(params_node) = params_node else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let mut is_varargs = false;
    for p in crate::model::named_children(params_node) {
        match p.kind() {
            "formal_parameter" => {
                let Some(ty_node) = p.child_by_field_name("type") else {
                    continue;
                };
                let mut t = crate::typeref::lower_type_node(ty_node, source, scope, resolve_named);
                if let Some(d) = p.child_by_field_name("dimensions") {
                    for _ in 0..node_text(d, source).matches('[').count() {
                        t = TypeRef::Array(Box::new(t));
                    }
                }
                out.push(t);
            }
            "spread_parameter" => {
                is_varargs = true;
                // No `type` field on `spread_parameter`: its type is a positional
                // child alongside optional `modifiers` and the trailing
                // `variable_declarator` name.
                let ty_node = crate::model::named_children(p)
                    .into_iter()
                    .find(|c| !matches!(c.kind(), "modifiers" | "variable_declarator"));
                let t = ty_node
                    .map(|n| crate::typeref::lower_type_node(n, source, scope, resolve_named))
                    .unwrap_or(TypeRef::Unknown);
                out.push(TypeRef::Array(Box::new(t)));
            }
            _ => {}
        }
    }
    (out, is_varargs)
}

fn member_from_node(
    node: Node,
    kind: MemberKind,
    is_static: bool,
    ctx: &MemberCtx,
) -> Option<ExternalMember> {
    // Enum constants have no declared type/parameters, so `erased_signature` can't
    // render one; handle directly here, or the `?` below would silently drop every
    // constant and falsely report "Cannot resolve field" on cross-file enums.
    if kind == MemberKind::EnumConstant {
        let name = node_text(node.child_by_field_name("name")?, ctx.source).to_string();
        return Some(ExternalMember {
            name: name.clone(),
            kind: ExternalMemberKind::Field,
            signature: name,
            template: None,
            is_static, // enum constants are implicitly static
            ret_fqn: None,
            ret_display: None,
            metadata: Some(MemberMetadata {
                declaring_class: TypeId::named(ctx.binary_name),
                access: Access::Public,
                is_static,
                is_abstract: false,
                parameters: None,
                result: TypeRef::named(ctx.binary_name),
                type_parameters: Vec::new(),
                is_varargs: false,
            }),
        });
    }
    let ext_kind = match kind {
        MemberKind::Method => ExternalMemberKind::Method,
        MemberKind::Field | MemberKind::EnumConstant => ExternalMemberKind::Field,
        MemberKind::NestedType(_) => return None, // walked via find_type_by_path, not a value
    };
    let signature = crate::signature::erased_signature(node, ctx.source)?;
    let name = node_text(node.child_by_field_name("name")?, ctx.source).to_string();
    // A regular field's type node lives on the enclosing `field_declaration`; a
    // record component's type is on `node` itself (its own `formal_parameter`).
    let field_type = || {
        if node.kind() == "formal_parameter" {
            node.child_by_field_name("type")
        } else {
            node.parent()?.child_by_field_name("type")
        }
    };
    let (ret_fqn, ret_display) = match kind {
        MemberKind::Method => node.child_by_field_name("type"),
        MemberKind::Field => field_type(),
        MemberKind::EnumConstant | MemberKind::NestedType(_) => None,
    }
    .map(|ty| {
        (
            resolve_ret_fqn(ty, ctx.source, ctx.imports, ctx.pick_fqn),
            node_text(ty, ctx.source).to_string(),
        )
    })
    .map_or((None, None), |(fqn, text)| (fqn, Some(text)));

    // A field's modifiers live on the enclosing `field_declaration`, not its own
    // `variable_declarator` — same lookup `externally_visible` uses.
    let modifiers_owner = match node.kind() {
        "variable_declarator" => node.parent().unwrap_or(node),
        _ => node,
    };
    let default_access = if ctx.td_kind == TypeKind::Interface {
        Access::Public
    } else {
        Access::Package
    };
    let access = crate::typeref::access_of(modifiers_owner, ctx.source, default_access);

    let metadata = match kind {
        MemberKind::Method => {
            let params_node = node.child_by_field_name("parameters");
            // A half-typed header (`void m(int a, `) lowers to a misleading
            // arity; `parameters: None` makes `call.rs` treat it as unknown.
            let header_broken = signature_has_error(node);
            // Owner key for this method's type variables; raw parameter text
            // disambiguates overloads (uniqueness is all `TypeVariableId::owner` needs).
            let owner = match params_node {
                Some(p) => format!("{}#{name}{}", ctx.binary_name, node_text(p, ctx.source)),
                None => format!("{}#{name}()", ctx.binary_name),
            };
            let (names, type_parameters) = crate::typeref::lower_type_parameters(
                node.child_by_field_name("type_parameters"),
                ctx.source,
                &owner,
                ctx.class_names,
                ctx.resolve_named,
            );
            let (parameters, is_varargs) =
                lower_parameters(params_node, ctx.source, &names, ctx.resolve_named);
            let result = node
                .child_by_field_name("type")
                .map(|ty| {
                    crate::typeref::lower_type_node(ty, ctx.source, &names, ctx.resolve_named)
                })
                .unwrap_or(TypeRef::Unknown);
            MemberMetadata {
                declaring_class: TypeId::named(ctx.binary_name),
                access,
                is_static,
                is_abstract: has_modifier(node, ctx.source, "abstract")
                    || (ctx.td_kind == TypeKind::Interface
                        && !is_static
                        && !has_modifier(node, ctx.source, "private")
                        && !has_modifier(node, ctx.source, "default")
                        && node.child_by_field_name("body").is_none()),
                parameters: if header_broken {
                    None
                } else {
                    Some(parameters)
                },
                result,
                type_parameters,
                is_varargs,
            }
        }
        MemberKind::Field => {
            let mut result = field_type()
                .map(|ty| {
                    crate::typeref::lower_type_node(
                        ty,
                        ctx.source,
                        ctx.class_names,
                        ctx.resolve_named,
                    )
                })
                .unwrap_or(TypeRef::Unknown);
            // C-style trailing array dimensions after the name (`int x[];`),
            // on `variable_declarator` or a record component's own
            // `formal_parameter` alike.
            if let Some(d) = node.child_by_field_name("dimensions") {
                for _ in 0..node_text(d, ctx.source).matches('[').count() {
                    result = TypeRef::Array(Box::new(result));
                }
            }
            MemberMetadata {
                declaring_class: TypeId::named(ctx.binary_name),
                access,
                is_static,
                is_abstract: false,
                parameters: None,
                result,
                type_parameters: Vec::new(),
                is_varargs: false,
            }
        }
        MemberKind::EnumConstant | MemberKind::NestedType(_) => unreachable!("handled above"),
    };

    Some(ExternalMember {
        name,
        kind: ext_kind,
        signature,
        template: None,
        is_static,
        ret_fqn,
        ret_display,
        metadata: Some(metadata),
    })
}

/// This type's constructors as [`ExternalMember`]s: real declared ones (any access —
/// caller filters), or the compiler-synthesized default (JLS 8.8.9) / record canonical
/// constructor (JLS 8.10.4) when none is written. Interfaces/annotations never have constructors.
fn constructor_members(
    td: &TypeDecl,
    source: &str,
    binary_name: &str,
    class_access: Access,
    class_names: &[(String, TypeVariableId)],
    resolve_named: &dyn Fn(&str, bool) -> Option<String>,
) -> Vec<ExternalMember> {
    if matches!(td.kind, TypeKind::Interface | TypeKind::Annotation) {
        return Vec::new();
    }
    let real = td.constructors();
    if real.is_empty() {
        if td.kind == TypeKind::Record {
            return vec![record_canonical_constructor(
                td,
                source,
                binary_name,
                class_access,
                class_names,
                resolve_named,
            )];
        }
        return vec![default_constructor(td, binary_name, class_access)];
    }
    real.into_iter()
        .filter_map(|ctor| {
            constructor_member_from_node(ctor, source, binary_name, class_names, resolve_named)
        })
        .collect()
}

fn constructor_member_from_node(
    node: Node,
    source: &str,
    binary_name: &str,
    class_names: &[(String, TypeVariableId)],
    resolve_named: &dyn Fn(&str, bool) -> Option<String>,
) -> Option<ExternalMember> {
    let signature = crate::signature::erased_signature(node, source)?;
    let name = node_text(node.child_by_field_name("name")?, source).to_string();
    let params_node = node.child_by_field_name("parameters");
    // `<init>` marker keeps a constructor's owner key distinct from a
    // same-named regular method (Java allows a method named like its class).
    let owner = match params_node {
        Some(p) => format!("{binary_name}#<init>{}", node_text(p, source)),
        None => format!("{binary_name}#<init>()"),
    };
    let (names, type_parameters) = crate::typeref::lower_type_parameters(
        node.child_by_field_name("type_parameters"),
        source,
        &owner,
        class_names,
        resolve_named,
    );
    let (parameters, is_varargs) = lower_parameters(params_node, source, &names, resolve_named);
    let access = crate::typeref::access_of(node, source, Access::Package);
    Some(ExternalMember {
        name,
        kind: ExternalMemberKind::Constructor,
        signature,
        template: None,
        is_static: false,
        ret_fqn: None,
        ret_display: None,
        metadata: Some(MemberMetadata {
            declaring_class: TypeId::named(binary_name),
            access,
            is_static: false,
            is_abstract: false,
            parameters: if signature_has_error(node) {
                None
            } else {
                Some(parameters)
            },
            result: TypeRef::Void,
            type_parameters,
            is_varargs,
        }),
    })
}

/// JLS 8.8.9: a class with no declared constructor gets one synthesized,
/// no-arg, with the same accessibility as the class itself.
fn default_constructor(td: &TypeDecl, binary_name: &str, class_access: Access) -> ExternalMember {
    ExternalMember {
        name: td.name.to_string(),
        kind: ExternalMemberKind::Constructor,
        signature: format!("{}()", td.name),
        template: None,
        is_static: false,
        ret_fqn: None,
        ret_display: None,
        metadata: Some(MemberMetadata {
            declaring_class: TypeId::named(binary_name),
            access: class_access,
            is_static: false,
            is_abstract: false,
            parameters: Some(Vec::new()),
            result: TypeRef::Void,
            type_parameters: Vec::new(),
            is_varargs: false,
        }),
    }
}

/// JLS 8.10.4: a record with neither an explicit nor a compact constructor
/// gets a canonical one synthesized over its own components; a compact
/// constructor (present but declares no parameter list of its own) yields
/// the same parameter shape, only its own access modifier (if any) differs.
fn record_canonical_constructor(
    td: &TypeDecl,
    source: &str,
    binary_name: &str,
    class_access: Access,
    class_names: &[(String, TypeVariableId)],
    resolve_named: &dyn Fn(&str, bool) -> Option<String>,
) -> ExternalMember {
    let params_node = td.node.child_by_field_name("parameters");
    let (parameters, is_varargs) =
        lower_parameters(params_node, source, class_names, resolve_named);
    let access = match td.compact_constructor() {
        Some(cc) => crate::typeref::access_of(cc, source, class_access),
        None => class_access,
    };
    let labels = crate::signature::param_labels(td.node, source).join(", ");
    ExternalMember {
        name: td.name.to_string(),
        kind: ExternalMemberKind::Constructor,
        signature: format!("{}({labels})", td.name),
        template: None,
        is_static: false,
        ret_fqn: None,
        ret_display: None,
        metadata: Some(MemberMetadata {
            declaring_class: TypeId::named(binary_name),
            access,
            is_static: false,
            is_abstract: false,
            parameters: Some(parameters),
            result: TypeRef::Void,
            type_parameters: Vec::new(),
            is_varargs,
        }),
    }
}

/// Expose non-private project-source members, including package-private access.
/// Constructors remain present so callers can report inaccessible ones precisely.
fn externally_visible(node: Node, source: &str, _enclosing_kind: TypeKind) -> bool {
    if matches!(node.kind(), "formal_parameter" | "enum_constant") {
        return true; // record component / enum constant: no modifier slot
    }
    // A field's modifiers live on the enclosing `field_declaration`, not its own
    // `variable_declarator` — same lookup `field_signature` uses.
    let modifiers_owner = match node.kind() {
        "variable_declarator" => node.parent().unwrap_or(node),
        _ => node,
    };
    !has_modifier(modifiers_owner, source, "private")
}

/// The FQN a declared-type node's base name resolves to, via the declaring file's own
/// imports. `None` for primitives, arrays, and anything `pick_fqn` can't place — same
/// graceful degradation as a bytecode member with no `ret_fqn`.
fn resolve_ret_fqn(
    type_node: Node,
    source: &str,
    imports: &Imports,
    pick_fqn: &dyn Fn(&[String]) -> Option<String>,
) -> Option<String> {
    let simple = crate::model::base_type_name(type_node, source)?;
    pick_fqn(&imports.candidates(simple))
}

/// The base simple type name of a rendered display type: `List<String>` →
/// `List`, `demo.Person` → `Person`. `None` for primitives (lowercase
/// first letter) and arrays — neither carries an importable FQN.
fn display_base_simple(display: &str) -> Option<&str> {
    let base = display.split('<').next()?.trim();
    if base.ends_with("[]") {
        return None;
    }
    let simple = base.rsplit('.').next()?;
    simple
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        .then_some(simple)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse};
    use jvl_types::PrimitiveType;
    use tree_sitter::Tree;

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn pick_first(candidates: &[String]) -> Option<String> {
        candidates.first().cloned()
    }

    /// `pick_fqn` that only "knows about" a fixed set of real types — models
    /// a caller (classpath + workspace index) that discards candidates
    /// nothing on disk/JDK actually resolves to.
    fn known(names: &'static [&'static str]) -> impl Fn(&[String]) -> Option<String> {
        move |candidates: &[String]| {
            candidates
                .iter()
                .find(|c| names.contains(&c.as_str()))
                .cloned()
        }
    }

    fn class_of_with(
        src: &str,
        type_path: &str,
        pick: &dyn Fn(&[String]) -> Option<String>,
    ) -> ExternalClass {
        let t = tree(src);
        class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            type_path,
            pick,
        )
        .unwrap_or_else(|| panic!("{type_path} not declared in {src}"))
    }

    fn class_of(src: &str, type_path: &str) -> ExternalClass {
        class_of_with(src, type_path, &pick_first)
    }

    fn class_meta(src: &str, type_path: &str) -> jvl_types::ClassMetadata {
        class_of(src, type_path)
            .metadata
            .expect("source classes always carry metadata")
    }

    #[test]
    fn broken_extends_clause_marks_hierarchy_incomplete() {
        let src = "package p; public class Dog extends { }";
        let meta = class_meta(src, "Dog");
        assert!(!meta.hierarchy_complete);
    }

    #[test]
    fn body_only_error_keeps_hierarchy_complete() {
        let src =
            "package p; class Animal {} public class Dog extends Animal { void m() { int x = ; } }";
        let meta = class_meta(src, "Dog");
        assert!(meta.hierarchy_complete, "{meta:?}");
        assert!(meta.constructors_complete, "{meta:?}");
        // A body-only error must also keep the method itself usable.
        let m = class_of(src, "Dog")
            .members
            .into_iter()
            .find(|m| m.name == "m")
            .expect("m");
        assert_eq!(m.metadata.unwrap().parameters, Some(Vec::new()));
    }

    #[test]
    fn broken_constructor_header_marks_constructors_incomplete() {
        let src = "package p; public class Dog { public Dog(int a, { } }";
        let meta = class_meta(src, "Dog");
        assert!(!meta.constructors_complete);
    }

    #[test]
    fn broken_method_header_has_no_parameter_proof() {
        let src = "package p; public class Dog { public void bark(int a, { } }";
        let class = class_of(src, "Dog");
        // Either recovery keeps a `bark` member (then it must carry no
        // parameter proof) or drops it entirely; both are safe.
        let bark = class.members.iter().find(|m| m.name == "bark");
        assert!(
            bark.is_none_or(|m| m.metadata.as_ref().unwrap().parameters.is_none()),
            "{bark:?}"
        );
    }

    #[test]
    fn dotted_nested_type_resolves_to_binary_name() {
        let src = "package p; public class Holder { public Outer.Inner field; }";
        let class = class_of_with(src, "Holder", &known(&["p.Outer$Inner", "p.Outer"]));
        let f = class.members.iter().find(|m| m.name == "field").unwrap();
        assert_eq!(
            f.metadata.as_ref().unwrap().result,
            TypeRef::named("p.Outer$Inner")
        );
    }

    #[test]
    fn extracts_fields_and_methods_with_result_types() {
        let src = "package demo;\n\
                   public class Person {\n\
                   public String name;\n\
                   public int getAge() { return 0; }\n\
                   public Person self() { return this; }\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Person",
            &pick_first,
        )
        .expect("parses");
        assert_eq!(class.supers, vec!["java.lang.Object".to_string()]);
        let name = class.members.iter().find(|m| m.name == "name").unwrap();
        assert_eq!(name.signature, "String name");
        assert_eq!(name.ret_fqn.as_deref(), Some("demo.String"));
        assert_eq!(name.ret_display.as_deref(), Some("String"));

        let age = class.members.iter().find(|m| m.name == "getAge").unwrap();
        assert_eq!(age.kind, ExternalMemberKind::Method);
        assert_eq!(age.signature, "int getAge()");

        let self_m = class.members.iter().find(|m| m.name == "self").unwrap();
        assert_eq!(self_m.ret_fqn.as_deref(), Some("demo.Person"));
    }

    #[test]
    fn resolves_return_type_through_explicit_import() {
        let src = "package demo;\n\
                   import java.util.List;\n\
                   public class Repo {\n\
                   public List all() { return null; }\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Repo",
            &known(&["java.util.List"]),
        )
        .unwrap();
        let all = class.members.iter().find(|m| m.name == "all").unwrap();
        assert_eq!(all.ret_fqn.as_deref(), Some("java.util.List"));
    }

    #[test]
    fn extracts_constructors_and_static_members() {
        let src = "package demo;\n\
                   public class Box {\n\
                   public Box() {}\n\
                   public Box(int n) {}\n\
                   public static int COUNT = 0;\n\
                   private int hidden() { return 0; }\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Box",
            &pick_first,
        )
        .unwrap();
        let ctors: Vec<_> = class
            .members
            .iter()
            .filter(|m| m.kind == ExternalMemberKind::Constructor)
            .collect();
        assert_eq!(ctors.len(), 2, "{:?}", class.members);
        assert!(ctors.iter().all(|m| m.name == "Box"));
        let count = class.members.iter().find(|m| m.name == "COUNT").unwrap();
        assert!(count.is_static);
        // Private members are excluded (own_members() already filters).
        assert!(!class.members.iter().any(|m| m.name == "hidden"));
    }

    #[test]
    fn extends_resolves_supertype_through_imports() {
        let src = "package demo;\n\
                   import java.util.ArrayList;\n\
                   public class Widgets extends ArrayList {\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Widgets",
            &known(&["java.util.ArrayList"]),
        )
        .unwrap();
        assert_eq!(class.supers, vec!["java.util.ArrayList".to_string()]);
    }

    #[test]
    fn nested_type_path_finds_inner_declaration() {
        let src = "package demo;\n\
                   public class Outer {\n\
                   public static class Inner {\n\
                   public int leaf;\n\
                   }\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Outer.Inner",
            &pick_first,
        )
        .unwrap();
        assert!(class.members.iter().any(|m| m.name == "leaf"));
    }

    #[test]
    fn unknown_type_path_is_none_not_panic() {
        let src = "class Person {}\n";
        let t = tree(src);
        let doc = OpenDoc {
            source: src,
            tree: &t,
        };
        assert!(class_from_doc(&doc, "NoSuchType", &pick_first).is_none());
        assert!(class_from_doc(&doc, "Person.Missing", &pick_first).is_none());
        let bad = "not even java {{{";
        let bad_t = tree(bad);
        assert!(class_from_doc(
            &OpenDoc {
                source: bad,
                tree: &bad_t
            },
            "Person",
            &pick_first
        )
        .is_none());
    }

    /// Constants, the implicit `Enum` super, and synthesized statics all
    /// surface for a cross-file enum.
    #[test]
    fn enum_exposes_constants_enum_super_and_synthetic_statics() {
        let src = "package p;\npublic enum E { SAMPLE, PATIENT;\n\
                   @Override public String toString() { return name(); } }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "E",
            &pick_first,
        )
        .unwrap();
        assert!(
            class.supers.iter().any(|s| s == "java.lang.Enum"),
            "implicit Enum super: {:?}",
            class.supers
        );
        let statics: Vec<&str> = class
            .members
            .iter()
            .filter(|m| m.is_static)
            .map(|m| m.name.as_str())
            .collect();
        assert!(statics.contains(&"SAMPLE"), "constant: {statics:?}");
        assert!(statics.contains(&"PATIENT"), "constant: {statics:?}");
        assert!(
            statics.contains(&"values"),
            "synthetic values(): {statics:?}"
        );
        assert!(
            statics.contains(&"valueOf"),
            "synthetic valueOf(): {statics:?}"
        );
    }

    #[test]
    fn only_private_members_are_excluded_from_project_source() {
        // Package-private members are exposed for project sources; only
        // `private` stays hidden (see `externally_visible`).
        let src = "package demo;\n\
                   public class Widget {\n\
                   public int pub_f;\n\
                   protected int prot_f;\n\
                   private int priv_f;\n\
                   int pkg_f;\n\
                   private void priv_m() {}\n\
                   void pkg_m() {}\n\
                   private Widget() {}\n\
                   Widget(int n) {}\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Widget",
            &pick_first,
        )
        .unwrap();
        let names: Vec<&str> = class.members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"pub_f"), "{names:?}");
        assert!(names.contains(&"prot_f"), "{names:?}");
        assert!(
            names.contains(&"pkg_f"),
            "package-private field included: {names:?}"
        );
        assert!(
            names.contains(&"pkg_m"),
            "package-private method included: {names:?}"
        );
        assert!(
            !names.contains(&"priv_f"),
            "private field hidden: {names:?}"
        );
        assert!(
            !names.contains(&"priv_m"),
            "private method hidden: {names:?}"
        );
        // Both constructors are present, including the private one;
        // `metadata.access` records its real access.
        let ctors: Vec<_> = class
            .members
            .iter()
            .filter(|m| m.kind == ExternalMemberKind::Constructor)
            .collect();
        assert_eq!(
            ctors.len(),
            2,
            "both constructors retained regardless of access: {:?}",
            class.members
        );
        let priv_ctor = ctors
            .iter()
            .find(|m| m.metadata.as_ref().unwrap().access == Access::Private);
        assert!(
            priv_ctor.is_some(),
            "private ctor present with recorded access: {:?}",
            ctors
        );
    }

    #[test]
    fn interface_members_are_implicitly_public() {
        let src = "package demo;\n\
                   public interface Greeter {\n\
                   int MAX = 10;\n\
                   String greet();\n\
                   private void helper() {}\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Greeter",
            &pick_first,
        )
        .unwrap();
        let names: Vec<&str> = class.members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"MAX"), "{names:?}");
        assert!(names.contains(&"greet"), "{names:?}");
        assert!(!names.contains(&"helper"), "explicit private: {names:?}");
        // Interfaces declare no constructors, not even a synthetic one.
        assert!(!class
            .members
            .iter()
            .any(|m| m.kind == ExternalMemberKind::Constructor));
    }

    #[test]
    fn lombok_accessors_synthesized_for_closed_file() {
        let src = "package demo;\nimport lombok.Data;\n\
                   @Data public class Person {\n\
                   private String name;\n\
                   private final int age;\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Person",
            &pick_first,
        )
        .unwrap();
        let names: Vec<&str> = class.members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"getName"), "{names:?}");
        assert!(names.contains(&"setName"), "{names:?}");
        assert!(names.contains(&"getAge"), "{names:?}");
        assert!(!names.contains(&"setAge"), "final: {names:?}");
        let get_name = class.members.iter().find(|m| m.name == "getName").unwrap();
        // Return FQN recovered through the declaring file's imports;
        // `pick_first` returns the package-local candidate.
        assert_eq!(get_name.ret_fqn.as_deref(), Some("demo.String"));
        assert_eq!(get_name.ret_display.as_deref(), Some("String"));
    }

    #[test]
    fn lombok_builder_companion_class_resolves_by_nested_path() {
        let src = "package demo;\nimport lombok.Builder;\n\
                   @Builder public class Person {\n\
                   private String name;\n\
                   }\n";
        let t = tree(src);
        let doc = OpenDoc {
            source: src,
            tree: &t,
        };
        // The @Builder entry point on the class itself…
        let class = class_from_doc(&doc, "Person", &pick_first).unwrap();
        let builder = class.members.iter().find(|m| m.name == "builder").unwrap();
        assert!(builder.is_static);
        assert_eq!(
            builder.ret_fqn.as_deref(),
            Some("demo.Person$PersonBuilder")
        );
        // …and the synthesized companion type by its nested path.
        let companion = class_from_doc(&doc, "Person.PersonBuilder", &pick_first)
            .expect("synthesized builder class");
        let fluent = companion.members.iter().find(|m| m.name == "name").unwrap();
        assert_eq!(fluent.ret_fqn.as_deref(), Some("demo.Person$PersonBuilder"));
        assert!(companion.members.iter().any(|m| m.name == "build"));
        // Without @Builder, the nested path stays unknown.
        let no_builder = "package demo;\nimport lombok.Getter;\n\
                          @Getter public class Person { private String name; }\n";
        let no_builder_t = tree(no_builder);
        let no_builder_doc = OpenDoc {
            source: no_builder,
            tree: &no_builder_t,
        };
        assert!(class_from_doc(&no_builder_doc, "Person.PersonBuilder", &pick_first).is_none());
    }

    #[test]
    fn record_component_is_visible_with_erased_signature() {
        let src = "package demo;\npublic record Point(int x, int y) {}\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Point",
            &pick_first,
        )
        .unwrap();
        let x = class.members.iter().find(|m| m.name == "x").unwrap();
        assert_eq!(x.signature, "int x");
        assert_eq!(x.kind, ExternalMemberKind::Field);
    }

    #[test]
    fn class_metadata_type_parameters_and_supertype_reference_the_same_variable() {
        let src = "package demo;\n\
                   public class Box<T> extends Base<T> implements Named {\n\
                   public Box(T v) {}\n\
                   public T get() { return null; }\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Box",
            &known(&["demo.Base", "demo.Named"]),
        )
        .unwrap();
        let metadata = class.metadata.as_ref().expect("class metadata present");
        assert_eq!(metadata.type_parameters.len(), 1, "{metadata:?}");
        let box_var = metadata.type_parameters[0].id.clone();
        assert_eq!(
            metadata.supertypes,
            vec![
                TypeRef::named_with("demo.Base", vec![TypeRef::Variable(box_var.clone())]),
                TypeRef::named("demo.Named"),
            ],
            "{metadata:?}"
        );

        let ctor = class
            .members
            .iter()
            .find(|m| m.kind == ExternalMemberKind::Constructor)
            .unwrap();
        assert_eq!(
            ctor.metadata.as_ref().unwrap().parameters,
            Some(vec![TypeRef::Variable(box_var.clone())])
        );

        let get = class.members.iter().find(|m| m.name == "get").unwrap();
        assert_eq!(
            get.metadata.as_ref().unwrap().result,
            TypeRef::Variable(box_var)
        );
    }

    #[test]
    fn unresolvable_supertype_is_unknown_and_hierarchy_incomplete() {
        let src = "package demo;\npublic class C extends Missing {}\n";
        let t = tree(src);
        // `pick_fqn` resolves nothing, modeling an unresolvable supertype.
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "C",
            &|_| None,
        )
        .unwrap();
        let metadata = class.metadata.as_ref().unwrap();
        assert_eq!(metadata.supertypes, vec![TypeRef::Unknown]);
        assert!(!metadata.hierarchy_complete);
    }

    #[test]
    fn class_with_no_declared_constructor_gets_one_synthetic_no_arg_constructor() {
        let src = "package demo;\npublic class D {}\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "D",
            &pick_first,
        )
        .unwrap();
        let ctors: Vec<_> = class
            .members
            .iter()
            .filter(|m| m.kind == ExternalMemberKind::Constructor)
            .collect();
        assert_eq!(ctors.len(), 1, "{:?}", class.members);
        assert_eq!(
            ctors[0].metadata.as_ref().unwrap().parameters,
            Some(Vec::new())
        );
    }

    #[test]
    fn record_with_no_explicit_constructor_gets_canonical_constructor_from_components() {
        let src = "package demo;\npublic record R(int a, String b) {}\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "R",
            &known(&["java.lang.String"]),
        )
        .unwrap();
        let ctors: Vec<_> = class
            .members
            .iter()
            .filter(|m| m.kind == ExternalMemberKind::Constructor)
            .collect();
        assert_eq!(ctors.len(), 1, "{:?}", class.members);
        assert_eq!(
            ctors[0].metadata.as_ref().unwrap().parameters,
            Some(vec![
                TypeRef::Primitive(PrimitiveType::Int),
                TypeRef::named("java.lang.String")
            ])
        );
    }

    #[test]
    fn private_constructor_is_retained_with_recorded_access() {
        let src = "package demo;\npublic class D { private D(int x) {} }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "D",
            &pick_first,
        )
        .unwrap();
        let ctor = class
            .members
            .iter()
            .find(|m| m.kind == ExternalMemberKind::Constructor)
            .unwrap();
        assert_eq!(ctor.metadata.as_ref().unwrap().access, Access::Private);
    }

    #[test]
    fn varargs_method_records_is_varargs_and_array_parameter() {
        let src = "package demo;\npublic class V { public void f(int... xs) {} }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "V",
            &pick_first,
        )
        .unwrap();
        let f = class.members.iter().find(|m| m.name == "f").unwrap();
        let meta = f.metadata.as_ref().unwrap();
        assert!(meta.is_varargs);
        assert_eq!(
            meta.parameters,
            Some(vec![TypeRef::Array(Box::new(TypeRef::Primitive(
                PrimitiveType::Int
            )))])
        );
    }

    #[test]
    fn functional_interface_metadata_preserves_type_names_and_abstract_methods() {
        let src = "package demo;\n\
                   public interface Fn<T> {\n\
                   void accept(T value);\n\
                   default void reset() {}\n\
                   static void create() {}\n\
                   }\n";
        let t = tree(src);
        let class = class_from_doc(
            &OpenDoc {
                source: src,
                tree: &t,
            },
            "Fn",
            &pick_first,
        )
        .unwrap();
        assert_eq!(class.type_params, ["T"]);
        let metadata = |name: &str| {
            class
                .members
                .iter()
                .find(|member| member.name == name)
                .and_then(|member| member.metadata.as_ref())
                .unwrap()
        };
        assert!(metadata("accept").is_abstract);
        assert!(!metadata("reset").is_abstract);
        assert!(!metadata("create").is_abstract);
    }

    #[test]
    fn declaration_fingerprint_is_stable_across_reparse_of_identical_source() {
        let src = "package demo;\npublic class F { public int m() { return 1; } }\n";
        let t = tree(src);
        let doc = OpenDoc {
            source: src,
            tree: &t,
        };
        assert_eq!(declaration_fingerprint(&doc), declaration_fingerprint(&doc));

        let t2 = tree(src);
        let doc2 = OpenDoc {
            source: src,
            tree: &t2,
        };
        assert_eq!(
            declaration_fingerprint(&doc),
            declaration_fingerprint(&doc2)
        );
    }

    #[test]
    fn declaration_fingerprint_ignores_method_body_but_reacts_to_signature_changes() {
        let base = "package demo;\npublic class F { public int m() { return 1; } }\n";
        let body_changed = "package demo;\npublic class F { public int m() { return 2; } }\n";
        let t_base = tree(base);
        let t_body = tree(body_changed);
        let fp_base = declaration_fingerprint(&OpenDoc {
            source: base,
            tree: &t_base,
        });
        let fp_body = declaration_fingerprint(&OpenDoc {
            source: body_changed,
            tree: &t_body,
        });
        assert_eq!(
            fp_base, fp_body,
            "a method-body-only edit must not change the fingerprint"
        );

        let sig_changed = "package demo;\npublic class F { public long m() { return 1; } }\n";
        let t_sig = tree(sig_changed);
        let fp_sig = declaration_fingerprint(&OpenDoc {
            source: sig_changed,
            tree: &t_sig,
        });
        assert_ne!(
            fp_base, fp_sig,
            "a return-type change must change the fingerprint"
        );

        let field_added =
            "package demo;\npublic class F { public int m() { return 1; } public int extra; }\n";
        let t_field = tree(field_added);
        let fp_field = declaration_fingerprint(&OpenDoc {
            source: field_added,
            tree: &t_field,
        });
        assert_ne!(fp_base, fp_field, "a new field must change the fingerprint");
    }
}
