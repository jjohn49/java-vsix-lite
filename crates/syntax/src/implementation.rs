//! M4 (4.6): go-to-implementation — bounded scan + supers confirm.
//!
//! `textDocument/implementation` answers two related queries:
//!
//! 1. Cursor on an interface/abstract (or concrete) **type** name → every
//!    in-project type that `extends`/`implements` it.
//! 2. Cursor on one of that type's **method** names (its declaration, or a
//!    call resolving to it) → the overriding/implementing method
//!    declaration in each of those types (types that don't declare their own
//!    override are skipped — they inherit, nothing to jump to).
//!
//! Mechanism, reusing existing machinery rather than indexing anything new:
//!
//! - [`implementation_target`] resolves the cursor the same way
//!   [`crate::reference_target`]/[`crate::definition`] do (declaration name,
//!   member access/call, mid-edit scoped path, plain reference), yielding an
//!   [`ImplementationTarget`] — the target type's simple name (the server's
//!   bounded prefilter needle, same convention as
//!   `ReferenceTarget::name`/`references::prefilter`) plus, for a
//!   method-level query, the method's own name.
//! - [`implementations_in_doc`] is the per-file confirm, checked *per
//!   supertype-clause entry* (the raw `extends`/`implements` type nodes via
//!   `model.rs`'s `super_type_nodes` — the same clause entries
//!   [`TypeDecl::supers`] erases to simple names, so subclassing a concrete
//!   class and implementing an interface are the same check). An entry
//!   confirms iff its base simple name is the target's AND (fix round 1):
//!   - **unqualified** (`implements Foo`): the name, read in the scanned
//!     file's own import/package context, actually resolves to the target's
//!     real declaration — [`crate::references::confirm_bare_type`], the
//!     exact same import-aware confirm `references.rs`'s `bare_type_site`
//!     uses, reused rather than duplicated;
//!   - **fully qualified** (`implements com.example.Foo` — bypasses imports
//!     entirely, so the import gate must neither vouch for it nor be needed
//!     by it): the written dotted name equals the target's own real FQN
//!     (its declaring document's `package` + simple name). A qualified
//!     entry naming a *different* package's same-simple-name type is
//!     rejected even when the file separately imports the target.
//!
//!   Method-level narrows further: only a type's own (non-inherited),
//!   non-`static` method named `method_name` counts — `TypeDecl::own_members`
//!   already excludes inherited members, so "didn't override, just
//!   inherited" falls out for free, and a `static` same-named method hides
//!   rather than overrides (fix round 1), so it is skipped too. Overloads
//!   are matched by NAME only (no arity/parameter-type comparison — the
//!   codebase's member model doesn't compare signatures structurally), so
//!   an implementor declaring several same-named overloads is over-included:
//!   every one of its own non-static `method_name` declarations is reported,
//!   not just the true JLS override.
//!
//! External (JDK/jar) targets are out of scope: [`implementation_target`]
//! only ever names an in-project type (`ctx.table`/`confirm_bare_type` never
//! resolve to an external `SymbolSource` type), so a query on, say, `List`
//! itself yields `None` rather than a (correct but unactionable, since the
//! declaration isn't in an open document) result. An in-project type whose
//! `implements` clause names an *external* interface is unaffected by this —
//! it's the target side that's out of scope, not the implementor side (that
//! case isn't reachable from this module at all, since it never appears as a
//! query target in the first place).

use std::ops::Range;

use ls_types::Position;
use tree_sitter::Node;

use crate::external::{NoSymbols, SymbolSource};
use crate::hover::{field_is, identifier_at, is_decl_name};
use crate::imports::Imports;
use crate::model::{base_type_name, super_type_nodes, MemberKind, TypeDecl, TypeTable};
use crate::references::{confirm_bare_type, package_of};
use crate::resolve::{
    self, dotted_type_name, Ctx, HierMember, MemberNamespace, Resolved, ResolvedType,
};
use crate::{node_text, LineIndex, OpenDoc};

/// What the cursor resolved to: an in-project type (interface/abstract/
/// concrete class), optionally narrowed to one of its methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplementationTarget {
    /// The target type's simple name — the prefilter needle.
    pub type_name: String,
    /// Index into the `&[OpenDoc]` slice [`implementation_target`] was
    /// called with — where the target type itself is declared.
    pub type_doc: usize,
    /// `Some` for a method-level query.
    pub method_name: Option<String>,
}

/// One confirmed implementor (type-level) or overriding declaration
/// (method-level) found in a scanned document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplementationHit {
    pub name_range: Range<usize>,
    pub full_range: Range<usize>,
}

/// Resolve the cursor to an [`ImplementationTarget`]. `None` for anything
/// that isn't an in-project type/method reference — an external (JDK/
/// dependency) symbol, a local/param/field, `this`/`super`, or a
/// non-identifier — mirroring [`crate::reference_target`]'s refusal shape.
pub fn implementation_target(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    symbols: &dyn SymbolSource,
) -> Option<ImplementationTarget> {
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
    classify_target(name_node, &ctx)
}

/// Resolve one identifier-like node to an [`ImplementationTarget`]: a
/// method's own declaration name (method-level, on its enclosing type), a
/// type's own declaration name (type-level), a method call/access resolving
/// to an in-project method (method-level, on the type that actually
/// declares it), or a bare type-name reference (type-level, via
/// [`confirm_bare_type`]).
fn classify_target<'t>(name_node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<ImplementationTarget> {
    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        if parent.kind() == "method_declaration" && field_is(parent, "name", name_node) {
            let owner = resolve::enclosing_typedecl(name_node, ctx.doc.source, ctx.current)?;
            return Some(ImplementationTarget {
                type_name: owner.name.to_string(),
                type_doc: owner.doc,
                method_name: Some(name.to_string()),
            });
        }
        if is_decl_name(parent, name_node) && resolve::is_type_decl(parent.kind()) {
            let td = TypeDecl::from_node(parent, ctx.doc.source, ctx.current)?;
            return Some(ImplementationTarget {
                type_name: td.name.to_string(),
                type_doc: td.doc,
                method_name: None,
            });
        }
        if parent.kind() == "method_invocation" && field_is(parent, "name", name_node) {
            let resolved = match parent.child_by_field_name("object") {
                Some(object) => resolve::resolve_receiver_type(object, ctx)?,
                None => Resolved {
                    ty: ResolvedType::InProject(resolve::enclosing_typedecl(
                        name_node,
                        ctx.doc.source,
                        ctx.current,
                    )?),
                    static_only: false,
                },
            };
            return method_target_from_member(&resolved, ctx, name);
        }
    }

    // Plain bare type-name reference (not a declaration): e.g. `Foo f;`'s
    // type node, or the `Foo` inside `implements Foo`.
    let td = confirm_bare_type(name, ctx)?;
    Some(ImplementationTarget {
        type_name: td.name.to_string(),
        type_doc: td.doc,
        method_name: None,
    })
}

/// Resolve `name` as a METHOD on `resolved`'s type hierarchy (namespace-aware
/// — never a same-named field), then report the target as the type that
/// actually *declares* that method (its own enclosing type, not necessarily
/// `resolved` itself when the method is inherited) — that declaring type is
/// what candidates must `implements`/`extends` to be an implementor.
/// External members (no open-document `DeclSite`, hence no owner `TypeDecl`
/// to report) are out of scope.
fn method_target_from_member<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    name: &str,
) -> Option<ImplementationTarget> {
    let member =
        match resolve::find_member_hier_of_kind(resolved, ctx, name, MemberNamespace::Method)? {
            HierMember::InProject(m) => m,
            HierMember::External(_) => return None,
        };
    let owner_node = resolve::enclosing_type_node(member.node)?;
    let owner = TypeDecl::from_node(owner_node, member.source, member.doc)?;
    Some(ImplementationTarget {
        type_name: owner.name.to_string(),
        type_doc: owner.doc,
        method_name: Some(name.to_string()),
    })
}

/// Scan `docs[current]` for confirmed implementors of `target` (or, when
/// `target.method_name` is `Some`, their overriding method declaration).
/// `target.type_doc` indexes into this *same* `docs` slice — the caller
/// places the target's declaring document there (mirroring
/// [`crate::references_in_doc`]'s convention: the single-file scan for the
/// target's own declaring file, and the two-document `[hit, target]` slice
/// built per prefiltered hit file otherwise).
///
/// No `SymbolSource` is threaded through: the confirm gate
/// ([`confirm_bare_type`]) only ever needs the scanned file's own
/// `TypeTable`/`Imports`, never the external symbol source — an
/// [`NoSymbols`] stand-in is used internally.
pub fn implementations_in_doc(
    docs: &[OpenDoc],
    current: usize,
    target: &ImplementationTarget,
) -> Vec<ImplementationHit> {
    let Some(doc) = docs.get(current) else {
        return Vec::new();
    };
    let table = TypeTable::build(docs, current);
    let imports = Imports::parse(doc.tree, doc.source);
    let no_symbols = NoSymbols;
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols: &no_symbols,
    };

    // File-level import/package confirm for UNQUALIFIED supertype entries:
    // whether a bare `target.type_name`, read in THIS file's own
    // import/package context, actually names the target's own declaration —
    // not an unrelated same-simple-name type from a different package (see
    // `confirm_bare_type`'s doc comment). Computed once per document; only
    // vouches for unqualified entries (a fully-qualified entry bypasses
    // imports, so it is confirmed against `target_fqn` below instead).
    let unqualified_confirmed =
        confirm_bare_type(&target.type_name, &ctx).is_some_and(|td| td.doc == target.type_doc);

    // The target's own real FQN (its declaring document's `package` + simple
    // name), for confirming FULLY-QUALIFIED supertype entries. `None` for a
    // default-package target — no qualified reference can name the default
    // package, so qualified entries then never match.
    let target_fqn = docs.get(target.type_doc).and_then(|target_doc| {
        package_of(target_doc.tree.root_node(), target_doc.source)
            .map(|pkg| format!("{pkg}.{}", target.type_name))
    });

    let mut hits = Vec::new();
    for td in table.iter() {
        // Only types actually declared *in this scanned document* — `table`
        // spans the whole given `docs` slice (which also contains the
        // target's own document when scanning a different file).
        if td.doc != current {
            continue;
        }
        // Per-supertype-entry confirm (fix round 1): an erased simple-name
        // match alone is not enough — `implements com.other.Foo` must not
        // pass on the strength of an unrelated `import com.example.Foo`.
        let implements_target = super_type_nodes(td.node).into_iter().any(|ty| {
            if base_type_name(ty, td.source) != Some(target.type_name.as_str()) {
                return false;
            }
            match dotted_type_name(ty, td.source) {
                // Fully qualified: matches iff it names the target's own FQN.
                Some(written_fqn) => Some(written_fqn) == target_fqn,
                // Unqualified: matches iff this file's imports/package
                // resolve the bare name to the target.
                None => unqualified_confirmed,
            }
        });
        if implements_target {
            push_hits_for(td, target, &mut hits);
        }
    }
    hits
}

fn push_hits_for(td: &TypeDecl, target: &ImplementationTarget, hits: &mut Vec<ImplementationHit>) {
    match &target.method_name {
        None => {
            if let Some(site) = td.decl_site() {
                hits.push(ImplementationHit {
                    name_range: site.name_range,
                    full_range: site.full_range,
                });
            }
        }
        Some(method_name) => {
            for m in td.own_members() {
                // Kind-aware (never a same-named field) and non-static only:
                // a `static` method hides, it does not override (fix round 1).
                if m.name == method_name && matches!(m.kind, MemberKind::Method) && !m.is_static {
                    if let Some(site) = m.decl_site() {
                        hits.push(ImplementationHit {
                            name_range: site.name_range,
                            full_range: site.full_range,
                        });
                    }
                }
            }
        }
    }
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

    fn target_at(docs: &[OpenDoc], current: usize, marker: &str) -> ImplementationTarget {
        let src = docs[current].source;
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present");
        implementation_target(docs, current, &index, index.position(at), &NoSymbols)
            .expect("implementation target resolved")
    }

    /// M4.6: an interface's type name resolves as a type-level target, and
    /// scanning a doc with an implementing class finds that class's own
    /// declaration.
    #[test]
    fn interface_type_level_query_finds_implementing_class_decl() {
        let doc_a = "package p;\ninterface Foo { void run(); }\n";
        let doc_b = "package p;\nclass Bar implements Foo {\n  public void run() {}\n}\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let docs = [
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
        ];
        let target = target_at(&docs, 0, "Foo {");
        assert_eq!(target.type_name, "Foo");
        assert_eq!(target.type_doc, 0);
        assert_eq!(target.method_name, None);

        // Scan doc B (index 0 in this 2-doc confirm slice) with the target
        // remapped to doc index 1 (mirrors `references_in_doc`'s convention).
        let scan_docs = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped = ImplementationTarget {
            type_doc: 1,
            ..target
        };
        let hits = implementations_in_doc(&scan_docs, 0, &remapped);
        assert_eq!(hits.len(), 1, "{hits:?}");
        let expected = doc_b.find("Bar").unwrap();
        assert_eq!(hits[0].name_range, expected..expected + "Bar".len());
    }

    /// M4.6: a method-level query on the interface's method resolves to the
    /// overriding method declaration in the implementing class.
    #[test]
    fn interface_method_level_query_finds_overriding_method_decl() {
        let doc_a = "package p;\ninterface Foo { void run(); }\n";
        let doc_b = "package p;\nclass Bar implements Foo {\n  public void run() {}\n}\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let docs = [
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
        ];
        let target = target_at(&docs, 0, "run();");
        assert_eq!(target.type_name, "Foo");
        assert_eq!(target.method_name.as_deref(), Some("run"));

        let scan_docs = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped = ImplementationTarget {
            type_doc: 1,
            ..target
        };
        let hits = implementations_in_doc(&scan_docs, 0, &remapped);
        assert_eq!(hits.len(), 1, "{hits:?}");
        let expected = doc_b.find("run() {}").unwrap();
        assert_eq!(hits[0].name_range, expected..expected + "run".len());
    }

    /// M4.6: a same-simple-name interface declared in a *different* package
    /// in a third doc must NOT be confirmed as the query's target — the
    /// scanned file's own import/package context must actually resolve
    /// `Foo` to the real target (mirrors `references.rs`'s
    /// `different_package_import_is_not_confirmed`-style test).
    #[test]
    fn same_simple_name_interface_different_package_is_not_confirmed() {
        let doc_a = "package pub1;\npublic interface Foo { void run(); }\n";
        let doc_c = "package other2;\nimport pub2.Foo;\nclass UseC implements Foo {\n  public void run() {}\n}\n";
        let tree_a = tree(doc_a);
        let tree_c = tree(doc_c);

        let docs_for_target = [OpenDoc {
            source: doc_a,
            tree: &tree_a,
        }];
        let target = target_at(&docs_for_target, 0, "Foo {");

        let scan_docs = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped = ImplementationTarget {
            type_doc: 1,
            ..target
        };
        let hits = implementations_in_doc(&scan_docs, 0, &remapped);
        assert!(
            hits.is_empty(),
            "UseC's Foo import names a different package — must not be confirmed: {hits:?}"
        );
    }

    /// M4.6: concrete-class subclassing (not just interface implementing)
    /// works through the same `supers` mechanism, and a subclass that
    /// doesn't override the method is skipped at the method level (it
    /// inherits — nothing new to jump to).
    #[test]
    fn concrete_subclass_override_lookup_skips_non_overriding_subclass() {
        let doc_a = "package p;\nclass A {\n  void greet() {}\n}\n";
        let doc_b = "package p;\nclass B extends A {\n  void greet() {}\n}\n"; // overrides
        let doc_c = "package p;\nclass C extends A {\n}\n"; // does NOT override
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let tree_c = tree(doc_c);
        let docs = [
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
        ];
        let target = target_at(&docs, 0, "greet() {}");
        assert_eq!(target.type_name, "A");
        assert_eq!(target.method_name.as_deref(), Some("greet"));

        // B overrides: found.
        let scan_b = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped = ImplementationTarget {
            type_doc: 1,
            ..target.clone()
        };
        let hits_b = implementations_in_doc(&scan_b, 0, &remapped);
        assert_eq!(hits_b.len(), 1, "{hits_b:?}");
        let expected = doc_b.find("greet() {}").unwrap();
        assert_eq!(hits_b[0].name_range, expected..expected + "greet".len());

        // C subclasses A but doesn't override: no hits at the method level.
        let scan_c = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped_c = ImplementationTarget {
            type_doc: 1,
            ..target
        };
        let hits_c = implementations_in_doc(&scan_c, 0, &remapped_c);
        assert!(
            hits_c.is_empty(),
            "C doesn't override greet — must not be reported: {hits_c:?}"
        );
    }

    /// M4.6 fix round 1 (Important, false positive): a scanned file that
    /// imports the target (`com.example.Foo`) but whose `implements` clause
    /// names a *fully-qualified different* type (`com.other.Foo`) must NOT
    /// be reported — the qualified supertype reference bypasses imports
    /// entirely, so the file-level import confirm alone must not vouch for
    /// it.
    #[test]
    fn qualified_super_naming_different_fqn_is_not_confirmed() {
        let doc_a = "package com.example;\npublic interface Foo { void run(); }\n";
        let doc_c = "package other;\nimport com.example.Foo;\nclass Bar implements com.other.Foo {\n  public void run() {}\n}\n";
        let tree_a = tree(doc_a);
        let tree_c = tree(doc_c);

        let docs_for_target = [OpenDoc {
            source: doc_a,
            tree: &tree_a,
        }];
        let target = target_at(&docs_for_target, 0, "Foo {");

        let scan_docs = [
            OpenDoc {
                source: doc_c,
                tree: &tree_c,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped = ImplementationTarget {
            type_doc: 1,
            ..target
        };
        let hits = implementations_in_doc(&scan_docs, 0, &remapped);
        assert!(
            hits.is_empty(),
            "Bar implements com.other.Foo (fully qualified, different type) — the \
             import of com.example.Foo must not confirm it: {hits:?}"
        );
    }

    /// M4.6 fix round 1 (Important, false negative): a scanned file with NO
    /// import whose `implements` clause names the target *fully qualified*
    /// (`implements com.example.Foo`, matching the target's real package)
    /// IS a confirmed implementor.
    #[test]
    fn fully_qualified_super_with_matching_package_is_confirmed() {
        let doc_a = "package com.example;\npublic interface Foo { void run(); }\n";
        let doc_b =
            "package other;\nclass Bar implements com.example.Foo {\n  public void run() {}\n}\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);

        let docs_for_target = [OpenDoc {
            source: doc_a,
            tree: &tree_a,
        }];
        let target = target_at(&docs_for_target, 0, "Foo {");

        let scan_docs = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped = ImplementationTarget {
            type_doc: 1,
            ..target
        };
        let hits = implementations_in_doc(&scan_docs, 0, &remapped);
        assert_eq!(
            hits.len(),
            1,
            "Bar implements com.example.Foo fully qualified (no import) — must be \
             confirmed via the target's own package: {hits:?}"
        );
        let expected = doc_b.find("Bar").unwrap();
        assert_eq!(hits[0].name_range, expected..expected + "Bar".len());
    }

    /// M4.6 fix round 1 (Minor): a `static` method with the same name in a
    /// subclass hides — it does not override — so a method-level query must
    /// not report it.
    #[test]
    fn static_same_name_method_is_not_an_override() {
        let doc_a = "package p;\nclass A {\n  void greet() {}\n}\n";
        let doc_b = "package p;\nclass B extends A {\n  static void greet() {}\n}\n";
        let tree_a = tree(doc_a);
        let tree_b = tree(doc_b);
        let docs = [OpenDoc {
            source: doc_a,
            tree: &tree_a,
        }];
        let target = target_at(&docs, 0, "greet() {}");
        assert_eq!(target.method_name.as_deref(), Some("greet"));

        let scan_docs = [
            OpenDoc {
                source: doc_b,
                tree: &tree_b,
            },
            OpenDoc {
                source: doc_a,
                tree: &tree_a,
            },
        ];
        let remapped = ImplementationTarget {
            type_doc: 1,
            ..target
        };
        let hits = implementations_in_doc(&scan_docs, 0, &remapped);
        assert!(
            hits.is_empty(),
            "B's static greet hides (not overrides) — must not be reported: {hits:?}"
        );
    }
}
