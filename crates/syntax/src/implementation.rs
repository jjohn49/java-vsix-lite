//! Bounded go-to-implementation for in-project types and methods.
//! Candidates are confirmed by resolved supertype clauses; external targets are unsupported.

use std::ops::Range;

use ls_types::Position;
use tree_sitter::Node;

use crate::external::{NoSymbols, SymbolSource};
use crate::hover::{field_is, identifier_at, is_decl_name};
use crate::imports::Imports;
use crate::model::{base_type_name, super_type_nodes, MemberKind, TypeDecl, TypeTable};
use crate::references::{confirm_bare_type, package_of};
use crate::resolve::{
    self, dotted_type_name, Ctx, FactsCache, HierMember, MemberNamespace, Resolved, ResolvedType,
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

/// Resolve the cursor to an [`ImplementationTarget`]. Returns `None` for
/// anything that isn't an in-project type/method reference (external
/// symbol, local/param/field, `this`/`super`, or non-identifier).
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
    classify_target(name_node, &ctx)
}

/// Resolve one identifier-like node to an [`ImplementationTarget`]: a
/// method or type declaration name, a method call resolving to an
/// in-project method, or a bare type reference.
fn classify_target<'t>(name_node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<ImplementationTarget> {
    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        if parent.kind() == "method_declaration" && field_is(parent, "name", name_node) {
            let owner = resolve::enclosing_typedecl(name_node, ctx.table, ctx.current)?;
            return Some(ImplementationTarget {
                type_name: owner.name.to_string(),
                type_doc: owner.doc,
                method_name: Some(name.to_string()),
            });
        }
        if is_decl_name(parent, name_node) && resolve::is_type_decl(parent.kind()) {
            let td = TypeDecl::from_node(parent, ctx.doc.source, ctx.current, None)?;
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
                    ty: ResolvedType::InProject {
                        decl: resolve::enclosing_typedecl(name_node, ctx.table, ctx.current)?,
                        args: Vec::new(),
                    },
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

/// Resolve `name` as a method (never a same-named field) on `resolved`'s
/// hierarchy, reporting the type that actually declares it (not
/// necessarily `resolved`, when inherited). External members are out of
/// scope.
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
    let owner = ctx.table.by_node(member.doc, owner_node.id())?.clone();
    Some(ImplementationTarget {
        type_name: owner.name.to_string(),
        type_doc: owner.doc,
        method_name: Some(name.to_string()),
    })
}

/// Scan `docs[current]` for confirmed implementors of `target` (or, when
/// `target.method_name` is `Some`, their overriding method declaration).
/// `target.type_doc` indexes into this same `docs` slice, pointing at the
/// target's declaring document.
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
    let facts = FactsCache::default();
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols: &no_symbols,
        docs,
        facts: &facts,
    };

    // Confirms UNQUALIFIED supertype entries: whether the bare
    // `target.type_name`, read in this file's imports/package, resolves to
    // the target (not an unrelated same-simple-name type). Qualified
    // entries are confirmed separately via `target_fqn` below.
    let unqualified_confirmed =
        confirm_bare_type(&target.type_name, &ctx).is_some_and(|td| td.doc == target.type_doc);

    // The target's real FQN, for confirming FULLY-QUALIFIED supertype
    // entries. `None` for a default-package target, so qualified entries
    // never match it.
    let target_fqn = docs.get(target.type_doc).and_then(|target_doc| {
        package_of(target_doc.tree.root_node(), target_doc.source)
            .map(|pkg| format!("{pkg}.{}", target.type_name))
    });

    let mut hits = Vec::new();
    for td in table.iter() {
        // Only types declared in this scanned document — `table` spans the
        // whole `docs` slice.
        if td.doc != current {
            continue;
        }
        // Simple-name match alone isn't enough: `implements com.other.Foo`
        // must not pass on the strength of an unrelated `import
        // com.example.Foo`.
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
                // a `static` method hides, it does not override.
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

    /// An interface's type name resolves as a type-level target, and
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

        // Scan doc B (index 0) with target remapped to doc index 1.
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

    /// A method-level query on the interface's method resolves to the
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

    /// A same-simple-name interface in a different package must NOT be
    /// confirmed as the target — the scanned file's own import/package
    /// context must resolve `Foo` to the real target.
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

    /// Concrete-class subclassing works through the same `supers`
    /// mechanism; a subclass that doesn't override the method is skipped
    /// at the method level.
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

    /// A file that imports the target (`com.example.Foo`) but whose
    /// `implements` clause names a different fully-qualified type
    /// (`com.other.Foo`) must NOT be reported — qualified references
    /// bypass imports entirely.
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

    /// A file with no import whose `implements` clause names the target
    /// fully qualified (matching its real package) IS a confirmed
    /// implementor.
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

    /// A `static` method with the same name in a subclass hides — it does
    /// not override — so a method-level query must not report it.
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
