//! M4.3: find references — two-tier, bounded, confirm-by-resolution.
//!
//! Every reference target falls into one of two visibility tiers, read off
//! the declaration's modifiers via [`crate::model::has_modifier`] (a missing
//! modifier is Java's package-private default):
//!
//! - [`Tier::FileLocal`]: a local variable, a parameter, or a `private`
//!   member — such a declaration cannot be referenced outside the file that
//!   declares it, so its references are found by scanning *that one file*.
//! - [`Tier::Workspace`]: package-private, `protected`, or `public` — such a
//!   declaration may be referenced from any file in the workspace (subject to
//!   the server's bounded prefilter/scan; this crate stays filesystem-free
//!   and only ever sees documents the caller hands it).
//!
//! Both tiers use the same confirm-by-resolution substrate as goto-definition
//! (`lookup_binding`, `resolve_receiver_type`, and the namespace-aware
//! `find_member_hier_of_kind` — fields and methods are separate namespaces in
//! Java, so a field and a same-named method must never conflate) — this
//! module adds no new resolution logic beyond one local refinement: a bare
//! type name is only accepted as a match when the *scanned file's own*
//! imports/package would actually resolve that name to the candidate's real
//! package (see [`bare_type_site`]). Without that check, two
//! same-simple-name types from different packages would be conflated
//! whenever only one of them is present in the small per-file confirm slice
//! (see the module's `different_package_import_is_not_confirmed` test).
//!
//! [`reference_target`] resolves the cursor to a target + tier; the server
//! decides, from the tier, whether to scan just the declaring file (already
//! open, already parsed) or to run its own bounded workspace prefilter and
//! call [`references_in_doc`] once per hit file (parsed on demand). Keeping
//! that orchestration in the server — not here — is what keeps this crate a
//! pure library over given sources (no directory walking, no file reads).

use std::ops::Range;

use ls_types::Position;
use tree_sitter::Node;

use crate::external::SymbolSource;
use crate::hover::{field_is, identifier_at};
use crate::imports::{dotted_path, Imports};
use crate::model::{
    has_modifier, named_children, DeclSite, Member, MemberKind, TypeDecl, TypeTable,
};
use crate::resolve::{
    self, Binding, BindingKind, Ctx, HierMember, MemberNamespace, Resolved, ResolvedType,
};
use crate::{node_text, LineIndex, OpenDoc};

/// How far a declaration's references can reach.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Local variable, parameter, or `private` member: references cannot
    /// leave the declaring file.
    FileLocal,
    /// Package-private (no modifier — Java's default), `protected`, or
    /// `public`: references may appear anywhere in the workspace.
    Workspace,
}

/// Where the symbol under the cursor is declared, its raw identifier text
/// (for the server's textual prefilter), and its visibility [`Tier`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceTarget {
    /// Indexes the `&[OpenDoc]` slice [`reference_target`] was called with.
    pub doc: usize,
    pub name_range: Range<usize>,
    pub name: String,
    pub tier: Tier,
}

/// The confirmed reference ranges within one scanned document, plus a count
/// of textually-plausible member-access occurrences that could not be
/// confirmed at all (the receiver's type didn't resolve — e.g. an unknown
/// supertype). Those are conservatively excluded from `ranges` rather than
/// risking a false positive; `possible` lets a caller surface that the
/// result may be incomplete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReferenceHits {
    pub ranges: Vec<Range<usize>>,
    pub possible: usize,
}

/// Resolve the identifier under the cursor to a [`ReferenceTarget`]. Only
/// symbols declared in one of the given open documents are supported
/// (open-files-first, matching the rest of this crate): `None` for anything
/// else — an external (JDK/dependency) symbol, a bare type name that isn't
/// declared in any given document, `this`/`super`, or a non-identifier.
pub fn reference_target(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    symbols: &dyn SymbolSource,
) -> Option<ReferenceTarget> {
    let doc = docs.get(current)?;
    let cursor = index.offset(pos);
    let table = TypeTable::build(docs, current);
    let imports = Imports::parse(doc.tree, doc.source);
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
    };

    let name_node = identifier_at(doc.tree, cursor)?;
    let Occurrence::Site(site) = classify(name_node, &ctx) else {
        return None;
    };
    let decl_site = site.decl_site()?;
    Some(ReferenceTarget {
        doc: decl_site.doc,
        name_range: decl_site.name_range,
        name: node_text(name_node, doc.source).to_string(),
        tier: site.tier(),
    })
}

/// List every reference to `target` inside `docs[current]`: walk every
/// identifier-like node in its tree, resolve each the same way
/// [`reference_target`] resolves the cursor (using `docs[current]`'s own
/// imports/package context — [`bare_type_site`] is what makes this
/// file-context-sensitive rather than a blind textual match), and keep the
/// ones whose resolved declaration site is `target`.
///
/// `target_doc` indexes into *this same* `docs` slice — the caller places the
/// target's declaring document there (at the same index in both the
/// single-file scan for [`Tier::FileLocal`] and the two-document
/// `[hit, target]` slice built per hit file for [`Tier::Workspace`]).
///
/// When `include_declaration` is `false`, the declaration's own name
/// occurrence (identified by its byte range exactly matching `target`'s) is
/// omitted from the result.
pub fn references_in_doc(
    docs: &[OpenDoc],
    current: usize,
    target: &ReferenceTarget,
    include_declaration: bool,
    symbols: &dyn SymbolSource,
) -> ReferenceHits {
    let Some(doc) = docs.get(current) else {
        return ReferenceHits::default();
    };
    let table = TypeTable::build(docs, current);
    let imports = Imports::parse(doc.tree, doc.source);
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
    };

    let mut ranges = Vec::new();
    let mut possible = 0usize;
    let mut stack = vec![doc.tree.root_node()];
    while let Some(node) = stack.pop() {
        // Only identifiers spelled exactly like the target can reference it
        // (Java has no aliasing) — everything else is skipped before any
        // resolution work, which both keeps the walk cheap and scopes the
        // `possible` counter to *this target's* unconfirmable textual hits.
        if matches!(node.kind(), "identifier" | "type_identifier")
            && node_text(node, doc.source) == target.name
        {
            match classify(node, &ctx) {
                Occurrence::Site(site) => {
                    if let Some(site_decl) = site.decl_site() {
                        if site_decl.doc == target.doc && site_decl.name_range == target.name_range
                        {
                            let is_declaration =
                                current == target.doc && node.byte_range() == target.name_range;
                            if include_declaration || !is_declaration {
                                ranges.push(node.byte_range());
                            }
                        }
                    }
                }
                Occurrence::UnresolvedReceiver => possible += 1,
                Occurrence::None => {}
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    ReferenceHits { ranges, possible }
}

/// What resolving one candidate identifier node yields.
enum Occurrence<'t> {
    /// Resolves to some declaration — a local/param/field binding, an
    /// in-project member, or an in-project type. May or may not be the
    /// requested target; the caller compares `DeclSite`s.
    Site(ResolvedSite<'t>),
    /// A member-access position (`recv.name` / `recv.name()`) whose receiver
    /// type could not be resolved at all — textually plausible but
    /// unconfirmable (tracked as `possible`, never counted as a match).
    UnresolvedReceiver,
    /// Not a reference this analysis recognizes, or one that resolves to an
    /// external symbol (no `DeclSite` to compare against).
    None,
}

/// A resolved declaration, tagged by which of the three substrates
/// (`resolve.rs`'s bindings, `model.rs`'s members, or its type table)
/// produced it — enough to answer both "where is it declared" and "how
/// visible is it".
enum ResolvedSite<'t> {
    Binding(Binding<'t>),
    Member(Member<'t>),
    Type(TypeDecl<'t>),
}

impl<'t> ResolvedSite<'t> {
    fn decl_site(&self) -> Option<DeclSite> {
        match self {
            ResolvedSite::Binding(b) => b.decl_site(),
            ResolvedSite::Member(m) => m.decl_site(),
            ResolvedSite::Type(t) => t.decl_site(),
        }
    }

    /// This declaration's visibility tier, from its own modifiers (a missing
    /// modifier on a field/method/type is Java's package-private default —
    /// `Tier::Workspace`, since another file in the same package may
    /// reference it).
    fn tier(&self) -> Tier {
        match self {
            ResolvedSite::Binding(b) => match b.kind {
                BindingKind::Field => field_tier(b.decl_node, b.source),
                BindingKind::Local | BindingKind::Param | BindingKind::ForVar => Tier::FileLocal,
            },
            ResolvedSite::Member(m) => member_tier(m),
            ResolvedSite::Type(t) => type_tier(t),
        }
    }
}

/// A field/record-component binding's tier: `private` on either the
/// declarator itself or its owning `field_declaration`/`constant_declaration`
/// (a bare `variable_declarator` carries no modifiers of its own).
fn field_tier(decl_node: Node, source: &str) -> Tier {
    let private = has_modifier(decl_node, source, "private")
        || decl_node
            .parent()
            .is_some_and(|p| has_modifier(p, source, "private"));
    if private {
        Tier::FileLocal
    } else {
        Tier::Workspace
    }
}

/// A member's tier: same `private`-on-self-or-owner check as
/// [`field_tier`] for a field member (record components are field-shaped
/// `formal_parameter` nodes with no modifiers of their own — parent lookup
/// is harmless there and simply finds none); methods and nested types carry
/// their own modifiers directly.
fn member_tier(m: &Member) -> Tier {
    let private = match m.kind {
        MemberKind::Field => field_tier(m.node, m.source) == Tier::FileLocal,
        _ => has_modifier(m.node, m.source, "private"),
    };
    if private {
        Tier::FileLocal
    } else {
        Tier::Workspace
    }
}

fn type_tier(td: &TypeDecl) -> Tier {
    if has_modifier(td.node, td.source, "private") {
        Tier::FileLocal
    } else {
        Tier::Workspace
    }
}

/// Resolve one identifier-like node the same way goto-definition's ladder
/// does (mirrors `definition::resolve_definition`'s branching), with three
/// reference-specific differences:
///
/// - a bare type name goes through [`bare_type_site`]'s package-aware gate
///   instead of a blind `TypeTable` lookup;
/// - a method declaration's own name — the one shape goto-definition never
///   resolves *from* — is its own declaration site directly (no lookup: the
///   node in hand IS the declaration, so a same-named field can't hijack it);
/// - member lookups are namespace-aware (JLS §6.5: fields and methods live
///   in separate namespaces): a `method_invocation`'s name resolves against
///   METHODS only, a `field_access` field / bare expression identifier
///   against FIELDS only — a field and a same-named method must never
///   conflate (see `resolve::MemberNamespace`).
fn classify<'t>(name_node: Node<'t>, ctx: &Ctx<'_, 't>) -> Occurrence<'t> {
    if !matches!(name_node.kind(), "identifier" | "type_identifier") {
        return Occurrence::None;
    }
    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        // A method's own declaration name resolves to ITS OWN declaration —
        // the node in hand is the declaration, so no member lookup is needed
        // (and a name-only lookup could wrongly land on a same-named field).
        if parent.kind() == "method_declaration" && field_is(parent, "name", name_node) {
            return Occurrence::Site(ResolvedSite::Member(Member {
                name,
                kind: MemberKind::Method,
                node: parent,
                is_static: has_modifier(parent, ctx.doc.source, "static"),
                source: ctx.doc.source,
                doc: ctx.current,
            }));
        }
        match parent.kind() {
            "field_access" if field_is(parent, "field", name_node) => {
                let Some(object) = parent.child_by_field_name("object") else {
                    return Occurrence::None;
                };
                return match resolve::resolve_receiver_type(object, ctx) {
                    Some(resolved) => {
                        member_occurrence(&resolved, ctx, name, MemberNamespace::Field)
                    }
                    None => Occurrence::UnresolvedReceiver,
                };
            }
            "method_invocation" if field_is(parent, "name", name_node) => {
                let resolved = match parent.child_by_field_name("object") {
                    Some(object) => resolve::resolve_receiver_type(object, ctx),
                    None => resolve::enclosing_typedecl(name_node, ctx.doc.source, ctx.current)
                        .map(|td| Resolved {
                            ty: ResolvedType::InProject(td),
                            static_only: false,
                        }),
                };
                return match resolved {
                    Some(resolved) => {
                        member_occurrence(&resolved, ctx, name, MemberNamespace::Method)
                    }
                    None => Occurrence::UnresolvedReceiver,
                };
            }
            // Mid-edit `recv.member` (no trailing `;`) parses as a scoped
            // path; same treatment as goto-definition/hover. Field-shaped
            // (no argument list yet), so the field namespace.
            "scoped_type_identifier" | "scoped_identifier" => {
                let segments = named_children(parent);
                if segments.len() >= 2 && segments.last() == Some(&name_node) {
                    return match resolve::resolve_receiver_type(segments[0], ctx) {
                        Some(resolved) => {
                            member_occurrence(&resolved, ctx, name, MemberNamespace::Field)
                        }
                        None => Occurrence::UnresolvedReceiver,
                    };
                }
            }
            _ => {}
        }
    }

    // Plain reference: a local/param/field binding (a bare identifier in
    // expression position can only be a variable or field, never a method —
    // methods require an argument list), else a bare in-project type name.
    if let Some(binding) = resolve::lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        name_node.start_byte(),
        name,
        ctx.table,
        ctx.current,
    ) {
        return Occurrence::Site(ResolvedSite::Binding(binding));
    }
    bare_type_site(name, ctx)
}

fn member_occurrence<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    name: &str,
    namespace: MemberNamespace,
) -> Occurrence<'t> {
    match resolve::find_member_hier_of_kind(resolved, ctx, name, namespace) {
        Some(HierMember::InProject(m)) => Occurrence::Site(ResolvedSite::Member(m)),
        // External (no `DeclSite`) or no such member on an otherwise-resolved
        // receiver: genuinely not our target, not a receiver-resolution
        // failure — not "possible" either.
        _ => Occurrence::None,
    }
}

/// A bare type name, gated by whether the *scanned file's own*
/// imports/package would actually resolve `name` to the candidate's real
/// package. Without this, a same-simple-name type declared in a different
/// package would be wrongly confirmed whenever the per-file confirm slice
/// happens to contain only the target's declaration and not the
/// unrelated same-named type the scanned file actually imports (there is
/// nothing else in that small slice for `TypeTable::get` to prefer). A type
/// declared in the *scanned file itself* needs no such check — it is
/// unambiguously in scope regardless of what it imports.
fn bare_type_site<'t>(name: &str, ctx: &Ctx<'_, 't>) -> Occurrence<'t> {
    match confirm_bare_type(name, ctx) {
        Some(td) => Occurrence::Site(ResolvedSite::Type(td)),
        None => Occurrence::None,
    }
}

/// The confirm-by-resolution core of [`bare_type_site`], factored out so
/// M4.6's go-to-implementation (`implementation.rs`) can reuse the exact same
/// import/package-aware gate for confirming a supertype (`extends`/
/// `implements`) reference in a scanned file actually names the target type
/// declaration, not an unrelated same-simple-name type from a different
/// package. See [`bare_type_site`]'s doc comment for the gate's rationale.
pub(crate) fn confirm_bare_type<'t>(name: &str, ctx: &Ctx<'_, 't>) -> Option<TypeDecl<'t>> {
    let td = ctx.table.get(name)?;
    if td.doc != ctx.current {
        let actual_fqn = package_of(td.node, td.source)
            .map(|pkg| format!("{pkg}.{name}"))
            .unwrap_or_else(|| name.to_string());
        if !ctx.imports.candidates(name).contains(&actual_fqn) {
            return None;
        }
    }
    Some(td.clone())
}

/// The dotted `package` path declared in `node`'s own document (walked up to
/// the root, independent of any particular file's `Imports` — used to learn
/// a *candidate* type's actual package, as opposed to the scanned file's
/// own, which `Imports::parse` already gives us).
///
/// `pub(crate)` (M4.6): also used by `implementation.rs` to compute the
/// TARGET type's real FQN for confirming fully-qualified `extends`/
/// `implements` entries.
pub(crate) fn package_of(node: Node, source: &str) -> Option<String> {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    named_children(root)
        .into_iter()
        .find(|c| c.kind() == "package_declaration")
        .and_then(|c| dotted_path(node_text(c, source), "package"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::NoSymbols;
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn target_at(docs: &[OpenDoc], current: usize, marker: &str) -> ReferenceTarget {
        let src = docs[current].source;
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present");
        reference_target(docs, current, &index, index.position(at), &NoSymbols)
            .expect("reference target resolved")
    }

    /// M4.3: a local variable's target resolves with `Tier::FileLocal`, and
    /// scanning its declaring document finds exactly its two usages (plus the
    /// declaration when `includeDeclaration` is honored) — an inner shadowing
    /// declaration of the same name is NOT counted for the outer variable.
    #[test]
    fn local_var_usages_found_with_shadow_excluded() {
        let src = "class C {\n\
                   void m() {\n\
                   int x = 0;\n\
                   x = 1;\n\
                   print(x);\n\
                   { int x = 2; x = 3; print(x); }\n\
                   }\n\
                   }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let target = target_at(&docs, 0, "x = 0");
        assert_eq!(target.tier, Tier::FileLocal);
        assert_eq!(target.name, "x");

        let without_decl = references_in_doc(&docs, 0, &target, false, &NoSymbols);
        assert_eq!(without_decl.ranges.len(), 2, "{without_decl:?}");
        let outer_x_assignment = src.find("x = 1").unwrap();
        assert!(without_decl
            .ranges
            .contains(&(outer_x_assignment..outer_x_assignment + 1)));
        let outer_print_x = src.find("print(x)").unwrap() + "print(".len();
        assert!(without_decl
            .ranges
            .contains(&(outer_print_x..outer_print_x + 1)));
        // Neither of the inner shadowed `x`'s three occurrences (decl + 2
        // usages) must appear.
        let inner_decl = src.find("x = 2").unwrap();
        assert!(!without_decl.ranges.iter().any(|r| r.start == inner_decl));

        let with_decl = references_in_doc(&docs, 0, &target, true, &NoSymbols);
        assert_eq!(with_decl.ranges.len(), 3, "{with_decl:?}");
        let decl = src.find("x = 0").unwrap();
        assert!(with_decl.ranges.contains(&(decl..decl + 1)));
    }

    /// M4.3: a `private` method's target resolves with `Tier::FileLocal`;
    /// calls within the declaring file (unqualified and via `this.`) are all
    /// counted, but a same-named method on a *different* type in the same
    /// file is not.
    #[test]
    fn private_method_calls_counted_different_type_excluded() {
        let src = "class A {\n\
                   private void helper() {}\n\
                   void m() { helper(); this.helper(); }\n\
                   }\n\
                   class B {\n\
                   void helper() {}\n\
                   void n() { helper(); }\n\
                   }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let target = target_at(&docs, 0, "helper() {}");
        assert_eq!(target.tier, Tier::FileLocal);

        let hits = references_in_doc(&docs, 0, &target, true, &NoSymbols);
        // Declaration + unqualified call + `this.` call == 3; B's `helper`
        // (declaration and call) must not appear.
        assert_eq!(hits.ranges.len(), 3, "{hits:?}");
        let b_helper_decl = src.rfind("void helper").unwrap() + "void ".len();
        assert!(!hits.ranges.iter().any(|r| r.start == b_helper_decl));
    }

    /// M4.3: a public type's target (declared in one given document) is
    /// confirmed as a reference from a *second* given document that imports
    /// it, but NOT from a third document that imports a same-simple-name
    /// type from a *different* package — semantic (import-aware) confirm
    /// beats a blind textual/simple-name match.
    #[test]
    fn public_type_confirmed_via_import_different_package_excluded() {
        let doc_a = "package pub1;\npublic class Foo {}\n";
        let doc_b = "package other;\nimport pub1.Foo;\nclass UseB { Foo f; }\n";
        let doc_c = "package other2;\nimport pub2.Foo;\nclass UseC { Foo f; }\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let tree_c = tree(doc_c);

        let docs_for_target = [OpenDoc {
            source: doc_a,
            tree: &tree_a,
        }];
        let target = target_at(&docs_for_target, 0, "Foo {}");
        assert_eq!(target.tier, Tier::Workspace);

        // Confirm slice for B: [B (hit, idx 0), A (target, idx 1)].
        let docs_b = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let target_in_b_slice = ReferenceTarget {
            doc: 1,
            ..target.clone()
        };
        let hits_b = references_in_doc(&docs_b, 0, &target_in_b_slice, false, &NoSymbols);
        assert_eq!(
            hits_b.ranges.len(),
            1,
            "expected B's `Foo f;` use: {hits_b:?}"
        );

        // Confirm slice for C: [C (hit, idx 0), A (target, idx 1)] — C's own
        // import names a *different* package's `Foo`.
        let docs_c = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let target_in_c_slice = ReferenceTarget { doc: 1, ..target };
        let hits_c = references_in_doc(&docs_c, 0, &target_in_c_slice, false, &NoSymbols);
        assert!(
            hits_c.ranges.is_empty(),
            "C's Foo import names a different package — must not be confirmed: {hits_c:?}"
        );
    }

    /// M4.3: `this.name` (the field) and a same-named local/parameter are
    /// disambiguated — referencing the field finds only the qualified use.
    #[test]
    fn field_vs_local_disambiguation() {
        let src = "class C {\n\
                   String name;\n\
                   void m(String name) {\n\
                   this.name = name;\n\
                   }\n\
                   }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let target = target_at(&docs, 0, "name;");
        assert_eq!(
            target.tier,
            Tier::Workspace,
            "no modifier => package-private"
        );

        let hits = references_in_doc(&docs, 0, &target, false, &NoSymbols);
        assert_eq!(hits.ranges.len(), 1, "{hits:?}");
        let this_name = src.find("this.name").unwrap() + "this.".len();
        assert_eq!(hits.ranges[0], this_name..this_name + "name".len());
    }

    /// M4.3 fix round 1 (Critical): a field and a method with the SAME name
    /// in the same class (Java keeps them in separate namespaces) must not
    /// conflate — references on the field find only field occurrences
    /// (declaration + `c.foo`), references on the method only method
    /// occurrences (declaration + `c.foo()`), from every cursor position.
    #[test]
    fn field_and_method_with_same_name_do_not_conflate() {
        let src = "class C {\n\
                   int foo;\n\
                   void foo() {}\n\
                   void m(C c) { int a = c.foo + 1; c.foo(); }\n\
                   }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];

        let field_decl = src.find("foo;").unwrap();
        let method_decl = src.find("foo() {}").unwrap();
        let field_use = src.find("foo + 1").unwrap();
        let call = src.find("foo();").unwrap();

        // All four cursor positions resolve, and to exactly two distinct
        // targets: {field decl, field use} -> the field's DeclSite;
        // {method decl, call} -> the method's DeclSite.
        let t_field_decl = target_at(&docs, 0, "foo;");
        let t_method_decl = target_at(&docs, 0, "foo() {}");
        let t_field_use = target_at(&docs, 0, "foo + 1");
        let t_call = target_at(&docs, 0, "foo();");
        assert_eq!(
            t_field_decl.name_range,
            field_decl..field_decl + 3,
            "field decl cursor must target the field"
        );
        assert_eq!(
            t_field_use.name_range, t_field_decl.name_range,
            "field use cursor must target the field"
        );
        assert_eq!(
            t_method_decl.name_range,
            method_decl..method_decl + 3,
            "method decl cursor must target the method"
        );
        assert_eq!(
            t_call.name_range, t_method_decl.name_range,
            "call cursor must target the method"
        );
        assert_ne!(t_field_decl.name_range, t_method_decl.name_range);

        // Field references: its declaration + the `c.foo` use — NOT the
        // method's declaration or the `c.foo()` call.
        let field_hits = references_in_doc(&docs, 0, &t_field_decl, true, &NoSymbols);
        assert_eq!(field_hits.ranges.len(), 2, "{field_hits:?}");
        assert!(field_hits.ranges.contains(&(field_decl..field_decl + 3)));
        assert!(field_hits.ranges.contains(&(field_use..field_use + 3)));

        // Method references: its declaration + the `c.foo()` call — NOT the
        // field's declaration or the `c.foo` use.
        let method_hits = references_in_doc(&docs, 0, &t_method_decl, true, &NoSymbols);
        assert_eq!(method_hits.ranges.len(), 2, "{method_hits:?}");
        assert!(method_hits.ranges.contains(&(method_decl..method_decl + 3)));
        assert!(method_hits.ranges.contains(&(call..call + 3)));
    }

    /// M4.3 fix round 1 (Critical): cross-kind through the hierarchy — a
    /// field `foo` in the superclass (another doc) and a method `foo()` in
    /// the subclass must stay separate: `b.foo` resolves to the inherited
    /// field, `b.foo()` to the subclass method, and neither's references
    /// include the other's occurrences.
    #[test]
    fn cross_kind_same_name_through_hierarchy_does_not_conflate() {
        let doc_a = "class A { int foo; }\n";
        let doc_b = "class B extends A {\n\
                     void foo() {}\n\
                     void m(B b) { int x = b.foo + 1; b.foo(); }\n\
                     }\n";
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

        // `b.foo` (field position) resolves through the hierarchy to A's
        // field, skipping B's same-named method.
        let t_field = target_at(&docs, 0, "foo + 1");
        assert_eq!(t_field.doc, 1, "field target is declared in doc A");
        let a_field = doc_a.find("foo").unwrap();
        assert_eq!(t_field.name_range, a_field..a_field + 3);

        // `b.foo()` (call position) resolves to B's own method.
        let t_method = target_at(&docs, 0, "foo();");
        assert_eq!(t_method.doc, 0, "method target is declared in doc B");
        let b_method = doc_b.find("foo() {}").unwrap();
        assert_eq!(t_method.name_range, b_method..b_method + 3);

        // Scanning B for the field finds only the `b.foo` use; for the
        // method, only its declaration + the `b.foo()` call.
        let field_hits = references_in_doc(&docs, 0, &t_field, false, &NoSymbols);
        let field_use = doc_b.find("foo + 1").unwrap();
        assert_eq!(field_hits.ranges, vec![field_use..field_use + 3]);

        let method_hits = references_in_doc(&docs, 0, &t_method, true, &NoSymbols);
        let call = doc_b.find("foo();").unwrap();
        assert_eq!(method_hits.ranges.len(), 2, "{method_hits:?}");
        assert!(method_hits.ranges.contains(&(b_method..b_method + 3)));
        assert!(method_hits.ranges.contains(&(call..call + 3)));
    }
}
