//! Bounded find-references with file-local and workspace visibility tiers.
//! Every text hit is confirmed through the same resolver as goto-definition.

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
    self, Binding, BindingKind, Ctx, FactsCache, HierMember, MemberNamespace, Resolved,
    ResolvedType,
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

/// Confirmed reference ranges in one scanned document, plus a count of
/// textually-plausible member-access occurrences whose receiver type
/// didn't resolve. Those are excluded from `ranges`; `possible` lets a
/// caller flag that the result may be incomplete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReferenceHits {
    pub ranges: Vec<Range<usize>>,
    pub possible: usize,
}

/// Resolve the identifier under the cursor to a [`ReferenceTarget`]. Only
/// symbols declared in one of the given open documents are supported;
/// `None` covers everything else (external symbols, undeclared bare
/// types, `this`/`super`, non-identifiers).
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

/// Resolve references to `target` inside `docs[current]`.
/// `target.doc` must index this slice; declarations are optional.
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

    let mut ranges = Vec::new();
    let mut possible = 0usize;
    let mut stack = vec![doc.tree.root_node()];
    while let Some(node) = stack.pop() {
        // Only identifiers spelled like the target can reference it (Java
        // has no aliasing), so everything else skips resolution entirely.
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
    /// A member-access position (`recv.name` / `recv.name()`) whose
    /// receiver type didn't resolve — plausible but unconfirmable, tracked
    /// as `possible`, never a match.
    UnresolvedReceiver,
    /// Not a reference this analysis recognizes, or one that resolves to an
    /// external symbol (no `DeclSite` to compare against).
    None,
}

/// A resolved declaration, tagged by which substrate produced it —
/// bindings, members, or the type table — enough to know where it's
/// declared and how visible it is.
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

    /// This declaration's visibility tier, from its own modifiers. A
    /// missing modifier is Java's package-private default (`Tier::Workspace`).
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

/// A member's tier: same private-on-self-or-owner check as [`field_tier`]
/// for fields; methods and nested types carry their own modifiers
/// directly.
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

/// Resolve one identifier node the same way goto-definition does, with two
/// reference-specific differences: bare type names use
/// [`bare_type_site`]'s package-aware gate, and a method declaration's own
/// name is its own declaration site directly (no lookup needed). Member
/// lookups are also namespace-aware — fields and methods (JLS §6.5) never
/// conflate.
fn classify<'t>(name_node: Node<'t>, ctx: &Ctx<'_, 't>) -> Occurrence<'t> {
    if !matches!(name_node.kind(), "identifier" | "type_identifier") {
        return Occurrence::None;
    }
    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        // A method's own declared name is its own declaration; no lookup
        // is needed, so a same-named field can't hijack it.
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
                let resolved =
                    match parent.child_by_field_name("object") {
                        Some(object) => resolve::resolve_receiver_type(object, ctx),
                        None => resolve::enclosing_typedecl(name_node, ctx.table, ctx.current).map(
                            |td| Resolved {
                                ty: ResolvedType::InProject {
                                    decl: td,
                                    args: Vec::new(),
                                },
                                static_only: false,
                            },
                        ),
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

    // Plain reference: a local/param/field binding, else a bare
    // in-project type name. A bare identifier can never be a method —
    // methods require an argument list.
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
        // External member, or no such member on a resolved receiver: not
        // our target, and not a `possible` receiver-resolution failure.
        _ => Occurrence::None,
    }
}

/// A bare type name, gated by whether the scanned file's own
/// imports/package actually resolve `name` to the candidate's real
/// package. Without this gate, same-simple-name types from different
/// packages could be conflated whenever the small per-file confirm slice
/// contains only one of them.
fn bare_type_site<'t>(name: &str, ctx: &Ctx<'_, 't>) -> Occurrence<'t> {
    match confirm_bare_type(name, ctx) {
        Some(td) => Occurrence::Site(ResolvedSite::Type(td)),
        None => Occurrence::None,
    }
}

/// Core of [`bare_type_site`], factored out so `implementation.rs` can
/// reuse the same package-aware gate when confirming `extends`/
/// `implements` supertype references.
pub(crate) fn confirm_bare_type<'t>(name: &str, ctx: &Ctx<'_, 't>) -> Option<TypeDecl<'t>> {
    resolve::resolve_simple_in_project(name, ctx)
}

/// The dotted `package` path declared in `node`'s own document — used to
/// learn a *candidate* type's actual package, as opposed to the scanned
/// file's own (already given by `Imports::parse`).
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

    /// A local variable's target is `Tier::FileLocal`; its two usages are
    /// found (plus the declaration when included). A shadowing inner
    /// declaration of the same name is not counted for the outer variable.
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

    /// A `private` method's target is `Tier::FileLocal`. Calls in the
    /// declaring file are counted, but a same-named method on a different
    /// type is not.
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

    /// A public type's reference is confirmed from a document that imports
    /// it, but not from one that imports a same-simple-name type from a
    /// different package — import-aware confirm beats a blind name match.
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

    /// `this.name` (the field) and a same-named local/parameter are
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

    /// A field and method with the same name in one class (separate
    /// namespaces in Java) must not conflate — each finds only its own
    /// occurrences, from every cursor position.
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

        // All four cursor positions resolve to exactly two distinct
        // targets: the field's DeclSite, or the method's DeclSite.
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

    /// A superclass field and subclass method sharing a name stay separate
    /// through inheritance: `b.foo` resolves to the field, `b.foo()` to
    /// the method, with no cross-contamination.
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
