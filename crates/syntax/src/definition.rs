//! Go-to-definition and go-to-type-definition: resolves the symbol under the
//! cursor to its declaration via a cheapest-first ladder over bindings, the
//! in-project type table, and the external [`SymbolSource`] seam — (a) local
//! binding, (b) in-project or external member, (c) unopened project type,
//! (d) classpath type. [`type_definition`] runs the same ladder over the
//! symbol's *type* instead of the symbol itself; external member types
//! aren't modeled structurally, so that case is skipped.

use std::ops::Range;

use ls_types::Position;
use tree_sitter::{Node, Tree};

use crate::external::SymbolSource;
use crate::hover::{field_is, identifier_at, is_decl_name};
use crate::imports::Imports;
use crate::model::{named_children, type_ref_simple_name, DeclSite, MemberKind, TypeTable};
use crate::resolve::{self, Ctx, FactsCache, HierMember, Resolved, ResolvedType};
use crate::{node_text, LineIndex, OpenDoc};

/// Where a symbol (or, for [`type_definition`], a symbol's type) is declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Definition {
    /// Declared in an open document: `doc` indexes the `&[OpenDoc]` slice the
    /// facade was called with.
    InOpenDoc {
        doc: usize,
        name_range: Range<usize>,
        full_range: Range<usize>,
    },
    /// A bare type name resolved (via imports/package) to a candidate FQN
    /// but declared in no open document and not on the classpath — likely
    /// an unopened project source file (step c).
    ProjectType {
        simple_name: String,
        fqn: Option<String>,
    },
    /// A JDK/dependency type or member the classpath recognizes; the server
    /// serves it as a virtual `jvl-src:` document (step d).
    External { fqn: String, member: Option<String> },
}

impl Definition {
    fn from_site(site: DeclSite) -> Definition {
        Definition::InOpenDoc {
            doc: site.doc,
            name_range: site.name_range,
            full_range: site.full_range,
        }
    }
}

/// Resolve the identifier under the cursor to where it's declared.
pub fn definition(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    symbols: &dyn SymbolSource,
) -> Option<Definition> {
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
    resolve_definition(name_node, &ctx)
}

/// Resolve the *type* of the identifier under the cursor to where that type is
/// declared (a variable's declared type, a method's return type, a field's
/// type).
pub fn type_definition(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    symbols: &dyn SymbolSource,
) -> Option<Definition> {
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
    // The expression's resolved type carries type arguments through
    // (`Shelter<Dog>.first()` is a `Dog`, `var d = s.first()` too); the
    // declared type node alone would name `T` or `var`, which no file
    // declares. Fall back to the declared node when resolution can't answer.
    if let Some(def) = expression_type_definition(name_node, &ctx) {
        return Some(def);
    }
    let (type_node, _, type_doc) = declared_type_of(name_node, &ctx)?;
    let dctx = if type_doc == current {
        None
    } else {
        ctx.for_document(type_doc)
    };
    definition_for_type_name(type_node, dctx.as_ref().unwrap_or(&ctx))
}

/// Where the type of the expression at `name_node` is declared, via full
/// resolution (overload selection, class and method type arguments applied).
/// `None` when the expression's type isn't a concrete in-project or
/// classpath type.
fn expression_type_definition<'t>(name_node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Definition> {
    let parent = name_node.parent()?;
    let expression = match parent.kind() {
        "field_access" if field_is(parent, "field", name_node) => parent,
        "method_invocation" if field_is(parent, "name", name_node) => parent,
        _ => name_node,
    };
    let resolved = resolve::resolve_expression_type(expression, ctx)?;
    definition_of_resolved_type(resolved, ctx)
}

fn definition_of_resolved_type(
    resolved: ResolvedType<'_>,
    ctx: &Ctx<'_, '_>,
) -> Option<Definition> {
    match resolved {
        ResolvedType::InProject { decl, .. } => decl.decl_site().map(Definition::from_site),
        ResolvedType::External { fqn, .. } => Some(Definition::External { fqn, member: None }),
        // An array's type definition is its element type's.
        ResolvedType::Array { element } => {
            definition_of_resolved_type(ResolvedType::from_type_ref(&element, ctx)?, ctx)
        }
        ResolvedType::Primitive(_) | ResolvedType::Void | ResolvedType::Null => None,
    }
}

/// Resolve `name_node` to what `definition` should return, mirroring
/// `hover::resolve_target`'s branching but yielding a [`Definition`] instead
/// of rendered hover content.
fn resolve_definition<'t>(name_node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Definition> {
    if matches!(name_node.kind(), "this" | "super") {
        let resolved = resolve::resolve_receiver_type(name_node, ctx)?;
        return match resolved.ty {
            ResolvedType::InProject { decl, .. } => decl.decl_site().map(Definition::from_site),
            ResolvedType::External { fqn, .. } => Some(Definition::External { fqn, member: None }),
            ResolvedType::Primitive(_)
            | ResolvedType::Void
            | ResolvedType::Null
            | ResolvedType::Array { .. } => None,
        };
    }

    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        if is_decl_name(parent, name_node) {
            // Already at the declaration; harmless since clients rarely
            // invoke goto-definition on the declaring identifier itself.
            return Some(Definition::InOpenDoc {
                doc: ctx.current,
                name_range: name_node.byte_range(),
                full_range: parent.byte_range(),
            });
        }
        match parent.kind() {
            "field_access" if field_is(parent, "field", name_node) => {
                let object = parent.child_by_field_name("object")?;
                let resolved = resolve::resolve_receiver_type(object, ctx)?;
                return member_definition(&resolved, ctx, name);
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
                return member_definition(&resolved, ctx, name);
            }
            // Mid-edit `recv.member` (no trailing `;`) parses as a scoped
            // path; treat like hover: resolve the trailing segment as a
            // member of the prefix's type.
            "scoped_type_identifier" | "scoped_identifier" => {
                let segments = named_children(parent);
                if segments.len() >= 2 && segments.last() == Some(&name_node) {
                    let resolved = resolve::resolve_receiver_type(segments[0], ctx)?;
                    return member_definition(&resolved, ctx, name);
                }
            }
            _ => {}
        }
    }

    // Plain reference: (a) a local/param/field binding, else a bare type name
    // (c)/(d).
    if let Some(binding) = resolve::lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        name_node.start_byte(),
        name,
        ctx.table,
        ctx.current,
    ) {
        if let Some(site) = binding.decl_site() {
            return Some(Definition::from_site(site));
        }
    }
    definition_for_type_name(name_node, ctx)
}

/// (b): `name`'s definition as a member of `resolved`'s type — an
/// in-project member's own declaration, or (d) an external member's FQN +
/// name when the receiver's own type is itself external. A member reachable
/// only by inheriting through an external supertype yields no result: the
/// owning FQN isn't tracked, so we don't guess.
fn member_definition<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    name: &str,
) -> Option<Definition> {
    let member = resolve::find_member_hier(resolved, ctx, name)?;
    if let Some(site) = member.decl_site() {
        return Some(Definition::from_site(site));
    }
    // No `DeclSite` (external): fall to (d) when the receiver's own resolved
    // type is itself external — we have its FQN.
    match &resolved.ty {
        ResolvedType::External { fqn, .. } => Some(Definition::External {
            fqn: fqn.clone(),
            member: Some(name.to_string()),
        }),
        ResolvedType::InProject { .. }
        | ResolvedType::Primitive(_)
        | ResolvedType::Void
        | ResolvedType::Null
        | ResolvedType::Array { .. } => None,
    }
}

/// (c)/(d): resolve a bare type's simple name — in-project if an open
/// document declares it, else the first import candidate the classpath
/// recognizes (d), else the best candidate FQN for the server to try as an
/// unopened project source file (c).
fn definition_for_type_name<'t>(node: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Definition> {
    let simple = type_ref_simple_name(node, ctx.doc.source)?;
    if let Some(td) =
        ctx.table
            .resolve_type_name_node(node, ctx.doc.source, ctx.current, ctx.imports)
    {
        return td.decl_site().map(Definition::from_site);
    }
    let candidates = ctx.imports.candidates(simple);
    for fqn in &candidates {
        if ctx.symbols.class(fqn).is_some() {
            return Some(Definition::External {
                fqn: fqn.clone(),
                member: None,
            });
        }
    }
    Some(Definition::ProjectType {
        simple_name: simple.to_string(),
        fqn: candidates.into_iter().next(),
    })
}

/// The declared-type node backing [`type_definition`]: mirrors
/// [`resolve_definition`]'s branching, one level removed (the type of the
/// thing, not the thing itself). Returns the node, its source text, and the
/// document that DECLARES the type, which may differ from `ctx.current` —
/// the type name must be resolved against that document's own
/// imports/package, not the caller's.
fn declared_type_of<'t>(
    name_node: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<(Node<'t>, &'t str, usize)> {
    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        if is_decl_name(parent, name_node) {
            return decl_type_of(parent, ctx.doc.source, ctx.current);
        }
        match parent.kind() {
            "field_access" if field_is(parent, "field", name_node) => {
                let object = parent.child_by_field_name("object")?;
                let resolved = resolve::resolve_receiver_type(object, ctx)?;
                return member_type_of(&resolved, ctx, name);
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
                return member_type_of(&resolved, ctx, name);
            }
            "scoped_type_identifier" | "scoped_identifier" => {
                let segments = named_children(parent);
                if segments.len() >= 2 && segments.last() == Some(&name_node) {
                    let resolved = resolve::resolve_receiver_type(segments[0], ctx)?;
                    return member_type_of(&resolved, ctx, name);
                }
            }
            _ => {}
        }
    }

    let binding = resolve::lookup_binding(
        ctx.doc.tree,
        ctx.doc.source,
        name_node.start_byte(),
        name,
        ctx.table,
        ctx.current,
    )?;
    Some((binding.type_node?, binding.source, ctx.current))
}

/// The declared-type node of a declaration name's own declaration (`parent`
/// is the decl node): a method's return type or a field/local/param's type.
/// `None` for declarations with no type of their own (inferred var, type
/// declaration, enum constant).
fn decl_type_of<'t>(
    parent: Node<'t>,
    source: &'t str,
    doc: usize,
) -> Option<(Node<'t>, &'t str, usize)> {
    match parent.kind() {
        "method_declaration" => parent.child_by_field_name("type").map(|t| (t, source, doc)),
        "variable_declarator" => parent
            .parent()?
            .child_by_field_name("type")
            .map(|t| (t, source, doc)),
        "formal_parameter" | "enhanced_for_statement" => {
            parent.child_by_field_name("type").map(|t| (t, source, doc))
        }
        _ => None,
    }
}

/// The declared-type node of `name`'s member on `resolved` — a method's
/// return type or a field's type. `None` for an external member (its type
/// isn't modeled structurally, only as a rendered signature string) or a
/// member kind with no type of its own (an enum constant, a nested type).
fn member_type_of<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    name: &str,
) -> Option<(Node<'t>, &'t str, usize)> {
    match resolve::find_member_hier(resolved, ctx, name)? {
        HierMember::InProject(m) => match m.kind {
            MemberKind::Method => m
                .node
                .child_by_field_name("type")
                .map(|t| (t, m.source, m.doc)),
            MemberKind::Field => member_field_type_node(m.node).map(|t| (t, m.source, m.doc)),
            _ => None,
        },
        HierMember::External(_) => None,
    }
}

/// The declared-type node of a field/record-component member declaration.
fn member_field_type_node<'t>(declarator: Node<'t>) -> Option<Node<'t>> {
    if declarator.kind() == "formal_parameter" {
        return declarator.child_by_field_name("type");
    }
    declarator.parent()?.child_by_field_name("type")
}

/// Find a top-level type's name range in one parsed unopened source file.
/// This mirrors `TypeDecl::decl_site` without adding the file to open documents.
pub fn locate_type_in_source(tree: &Tree, source: &str, simple_name: &str) -> Option<Range<usize>> {
    let docs = [OpenDoc { source, tree }];
    let table = TypeTable::build(&docs, 0);
    let td = table.candidates(simple_name).next()?;
    td.decl_site().map(|site| site.name_range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

    /// A `SymbolSource` that resolves exactly the fixture classes given.
    struct Stub(Vec<(&'static str, Vec<&'static str>)>);

    impl SymbolSource for Stub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            self.0
                .iter()
                .find(|(name, _)| *name == fqn)
                .map(|(_, members)| ExternalClass {
                    supers: Vec::new(),
                    type_params: Vec::new(),
                    members: members
                        .iter()
                        .map(|m| ExternalMember {
                            name: m.to_string(),
                            kind: ExternalMemberKind::Method,
                            signature: format!("int {m}()"),
                            template: None,
                            is_static: false,
                            ret_fqn: None,
                            ret_display: None,
                            metadata: None,
                        })
                        .collect(),
                    metadata: None,
                })
        }
    }

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn def_at(src: &str, marker: &str, symbols: &dyn SymbolSource) -> Option<Definition> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present");
        definition(&docs, 0, &index, index.position(at), symbols)
    }

    fn type_def_at(src: &str, marker: &str, symbols: &dyn SymbolSource) -> Option<Definition> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present");
        type_definition(&docs, 0, &index, index.position(at), symbols)
    }

    // --- (a) local/param/field ---

    #[test]
    fn local_var_usage_resolves_to_its_declarator() {
        let src = "class C { void m() { int count = 0; count++; } }\n";
        let def = def_at(src, "count++", &NoSymbols).expect("definition");
        let Definition::InOpenDoc {
            doc, name_range, ..
        } = def
        else {
            panic!("expected InOpenDoc: {def:?}")
        };
        assert_eq!(doc, 0);
        let expected = src.find("count = 0").unwrap();
        assert_eq!(name_range, expected..expected + "count".len());
    }

    #[test]
    fn field_usage_resolves_to_its_declarator() {
        let src = "class C { int name; void m() { name = 1; } }\n";
        let def = def_at(src, "name = 1", &NoSymbols).expect("definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("name;").unwrap();
        assert_eq!(name_range, expected..expected + "name".len());
    }

    // --- (b) member access, in-project (possibly cross-doc) ---

    #[test]
    fn cross_doc_member_call_resolves_to_supertype_method() {
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
        let index = LineIndex::new(doc_b, PositionEncoding::Utf16);
        let at = doc_b.find("methodFromA();").unwrap();
        let def = definition(&docs, 0, &index, index.position(at), &NoSymbols).expect("definition");
        let Definition::InOpenDoc {
            doc, name_range, ..
        } = def
        else {
            panic!("expected InOpenDoc: {def:?}")
        };
        assert_eq!(doc, 1, "resolves into doc A, not the calling doc B");
        let expected = doc_a.find("methodFromA").unwrap();
        assert_eq!(name_range, expected..expected + "methodFromA".len());
    }

    // --- (b-ext) member access on an externally-typed receiver ---

    #[test]
    fn external_receiver_member_call_yields_external_definition() {
        let src = r#"class C { void m() { "".length(); } }"#;
        let symbols = Stub(vec![("java.lang.String", vec!["length"])]);
        let def = def_at(src, "length()", &symbols).expect("definition");
        assert_eq!(
            def,
            Definition::External {
                fqn: "java.lang.String".to_string(),
                member: Some("length".to_string()),
            }
        );
    }

    // --- (c) bare type name resolving to an unopened project file ---

    #[test]
    fn imported_type_not_open_and_not_on_classpath_is_a_project_type() {
        let src = "import p.Foo;\nclass C { Foo f; }\n";
        let def = def_at(src, "Foo f", &NoSymbols).expect("definition");
        assert_eq!(
            def,
            Definition::ProjectType {
                simple_name: "Foo".to_string(),
                fqn: Some("p.Foo".to_string()),
            }
        );
    }

    #[test]
    fn same_package_type_not_open_is_a_project_type() {
        let src = "package p;\nclass C { Bar b; }\n";
        let def = def_at(src, "Bar b", &NoSymbols).expect("definition");
        assert_eq!(
            def,
            Definition::ProjectType {
                simple_name: "Bar".to_string(),
                fqn: Some("p.Bar".to_string()),
            }
        );
    }

    // --- (d) bare type name the classpath recognizes ---

    #[test]
    fn type_name_on_classpath_yields_external_definition() {
        let src = "import java.util.List;\nclass C { List xs; }\n";
        let symbols = Stub(vec![("java.util.List", vec![])]);
        let def = def_at(src, "List xs", &symbols).expect("definition");
        assert_eq!(
            def,
            Definition::External {
                fqn: "java.util.List".to_string(),
                member: None,
            }
        );
    }

    // --- type-definition ---

    #[test]
    fn type_definition_of_method_call_resolves_to_return_type_decl() {
        // `foo.bar()` where `bar` returns `Baz` (declared in the same open doc).
        let src = "class Baz {}\n\
                   class Box { Baz bar() { return null; } }\n\
                   class C { void m() { Box foo = new Box(); foo.bar(); } }\n";
        let def = type_def_at(src, "bar();", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("Baz {}").unwrap();
        assert_eq!(name_range, expected..expected + "Baz".len());
    }

    #[test]
    fn type_definition_of_variable_resolves_to_declared_type_decl() {
        let src = "class Widget {}\nclass C { void m() { Widget w; w.toString(); } }\n";
        let def = type_def_at(src, "w.toString", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("Widget {}").unwrap();
        assert_eq!(name_range, expected..expected + "Widget".len());
    }

    #[test]
    fn type_definition_of_field_resolves_to_field_type_decl() {
        let src = "class Widget {}\nclass C { Widget w; void m() { w.toString(); } }\n";
        let def = type_def_at(src, "w.toString", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("Widget {}").unwrap();
        assert_eq!(name_range, expected..expected + "Widget".len());
    }

    /// Definition of a generic member call lands on the member declaration
    /// regardless of the receiver's type arguments.
    #[test]
    fn definition_of_generic_member_call_resolves_to_declaration() {
        let src = "class Dog {}\n\
                   class Shelter<T> { T first() { return null; } }\n\
                   class C { void m() { Shelter<Dog> s; s.first(); } }\n";
        let def = def_at(src, "first();", &NoSymbols).expect("definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("first() {").unwrap();
        assert_eq!(name_range, expected..expected + "first".len());
    }

    /// Type definition of a generic call follows the substituted type
    /// (`Shelter<Dog>.first()` -> `Dog`), not the declared variable `T`.
    #[test]
    fn type_definition_of_generic_call_resolves_to_substituted_type() {
        let src = "class Dog {}\n\
                   class Shelter<T> { T first() { return null; } T item; }\n\
                   class C { void m() { Shelter<Dog> s; s.first(); s.item.toString(); } }\n";
        let def = type_def_at(src, "first();", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("Dog {}").unwrap();
        assert_eq!(name_range, expected..expected + "Dog".len());

        let def = type_def_at(src, "item.toString", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        assert_eq!(name_range, expected..expected + "Dog".len());
    }

    /// A generic method's inferred type argument drives type definition too
    /// (`<E> E pick(E)` called with `Dog` -> `Dog`).
    #[test]
    fn type_definition_of_generic_method_call_uses_inferred_argument() {
        let src = "class Dog {}\n\
                   class Util { static <E> E pick(E value) { return value; } }\n\
                   class C { void m() { Util.pick(new Dog()); } }\n";
        let def = type_def_at(src, "pick(new", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("Dog {}").unwrap();
        assert_eq!(name_range, expected..expected + "Dog".len());
    }

    /// `var` locals follow the initializer's type.
    #[test]
    fn type_definition_of_var_local_uses_inferred_type() {
        let src = "class Dog {}\n\
                   class Shelter<T> { T first() { return null; } }\n\
                   class C { void m() { Shelter<Dog> s; var d = s.first(); d.toString(); } }\n";
        let def = type_def_at(src, "d.toString", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("Dog {}").unwrap();
        assert_eq!(name_range, expected..expected + "Dog".len());
    }

    #[test]
    fn definition_on_non_identifier_is_none() {
        let src = "class C { }\n";
        let def = def_at(src, "{", &NoSymbols);
        assert!(def.is_none());
    }

    /// A generic member inherited through a parameterized supertype projects
    /// the subclass's argument (`Kennel extends Shelter<Dog>`: `k.item` is
    /// a `Dog`), so the jump doesn't dead-end on `T`.
    #[test]
    fn type_definition_of_inherited_generic_field_projects_supertype_argument() {
        let src = "class Dog {}\n\
                   class Shelter<T> { T item; }\n\
                   class Kennel extends Shelter<Dog> {}\n\
                   class C { void m() { Kennel k; k.item.toString(); } }\n";
        let def = type_def_at(src, "item.toString", &NoSymbols).expect("type definition");
        let Definition::InOpenDoc { name_range, .. } = def else {
            panic!("expected InOpenDoc: {def:?}")
        };
        let expected = src.find("Dog {}").unwrap();
        assert_eq!(name_range, expected..expected + "Dog".len());
    }

    // --- locate_type_in_source (server-side step-(c) parse-on-demand path) ---

    #[test]
    fn locate_type_in_source_finds_the_type_name_range() {
        let src = "package p;\nclass Foo {\n  int x;\n}\n";
        let t = tree(src);
        let range = locate_type_in_source(&t, src, "Foo").expect("Foo located");
        let expected = src.find("Foo").unwrap();
        assert_eq!(range, expected..expected + "Foo".len());
    }

    #[test]
    fn locate_type_in_source_missing_type_is_none() {
        let src = "class Foo {}\n";
        let t = tree(src);
        assert!(locate_type_in_source(&t, src, "Bar").is_none());
    }
}
