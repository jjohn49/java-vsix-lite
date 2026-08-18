//! LSP hover: show a reconstructed signature (fenced `java`) plus the symbol's
//! Javadoc. Works on declarations and on references that resolve to a
//! declaration in an open file.

use ls_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use tree_sitter::{Node, Tree};

use crate::external::{ExternalMemberKind, SymbolSource};
use crate::imports::Imports;
use crate::model::{named_children, TypeTable};
use crate::resolve::{self, Ctx, HierMember, Resolved, ResolvedType};
use crate::signature::{javadoc, param_count_in_label, param_labels, signature};
use crate::{node_text, LineIndex, OpenDoc};

/// What to render: an in-project declaration node (signature + Javadoc from the
/// tree), or an external member's pre-rendered signature (no Javadoc — JDK/jar
/// bytecode carries none).
enum Target<'t> {
    InProject(Node<'t>, &'t str),
    /// A pre-rendered external signature plus optional Javadoc.
    External(String, Option<String>),
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
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
    };

    let name_node = identifier_at(doc.tree, cursor)?;
    let value = match resolve_target(name_node, &ctx)? {
        Target::InProject(node, source) => {
            let sig = signature(node, source)?;
            let mut value = format!("```java\n{sig}\n```");
            if let Some(doc_text) = javadoc(node, source) {
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

    // Cursor on the type name inside `new Foo(...)` (or `new ArrayList<String>(...)`
    // — the `type` field may be wrapped in a `generic_type`/`scoped_type_identifier`):
    // show the best-matching constructor rather than falling through to a plain
    // type-name reference (which would just show the class declaration).
    if let Some(call) = enclosing_object_creation(name_node) {
        return constructor_target(call, ctx);
    }

    let name = node_text(name_node, ctx.doc.source);

    if let Some(parent) = name_node.parent() {
        if is_decl_name(parent, name_node) {
            return Some(Target::InProject(parent, ctx.doc.source));
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
                        ty: ResolvedType::InProject(resolve::enclosing_typedecl(
                            name_node,
                            ctx.doc.source,
                            ctx.current,
                        )?),
                        static_only: false,
                    },
                };
                return member_target(&resolved, ctx, name);
            }
            // Mid-edit `recv.member` (no trailing `;`) parses as a scoped path; if
            // the cursor is on the trailing segment, resolve it as a member of the
            // prefix's type.
            "scoped_type_identifier" | "scoped_identifier" => {
                let segments = named_children(parent);
                if segments.len() >= 2 && segments.last() == Some(&name_node) {
                    let resolved = resolve::resolve_receiver_type(segments[0], ctx)?;
                    return member_target(&resolved, ctx, name);
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
        return Some(Target::InProject(binding.decl_node, binding.source));
    }
    ctx.table
        .get(name)
        .map(|td| Target::InProject(td.node, td.source))
}

fn inproject_target<'t>(resolved: &Resolved<'t>) -> Option<Target<'t>> {
    match &resolved.ty {
        ResolvedType::InProject(td) => Some(Target::InProject(td.node, td.source)),
        ResolvedType::External { .. } | ResolvedType::Array { .. } => None,
    }
}

fn member_target<'t>(resolved: &Resolved<'t>, ctx: &Ctx<'_, 't>, name: &str) -> Option<Target<'t>> {
    match resolve::find_member_hier(resolved, ctx, name)? {
        HierMember::InProject(m) => Some(Target::InProject(m.node, m.source)),
        HierMember::External(m) => {
            // Javadoc only when the receiver itself is external (we have its FQN).
            let doc = match &resolved.ty {
                ResolvedType::External { fqn, .. } => ctx.symbols.doc(fqn, Some(name)),
                ResolvedType::InProject(_) | ResolvedType::Array { .. } => None,
            };
            Some(Target::External(m.signature, doc))
        }
    }
}

/// Walk up from `name_node` through the type-node shapes that can wrap a
/// `new` type reference (`generic_type` for `new ArrayList<String>()`,
/// `scoped_type_identifier`/`annotated_type` for a qualified or annotated
/// one), returning the enclosing `object_creation_expression` if `name_node`
/// is (part of) its `type` field — `None` for anything else (e.g. an
/// argument expression inside the call, whose parent chain never reaches one
/// of these type-node kinds).
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

/// Number of arguments actually written at a call site (`new Foo(1, 2)` -> 2)
/// — the direct argument expressions inside the object-creation's own
/// `argument_list` (a nested call's arguments live in their own
/// `argument_list` node, so they're never counted here).
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

/// Hover for the type name inside `new Foo(...)`: the best-matching
/// constructor's signature + Javadoc. "Best-matching" is an arity match
/// against the call's argument count (first declared wins a tie — the same
/// convention `signature_help`'s active-overload heuristic uses), falling
/// back to the first declared constructor if none matches. When a chosen
/// constructor has no Javadoc of its own, falls back to the class-level
/// Javadoc; when the type declares no explicit constructor at all, shows a
/// synthesized `Foo()` plus the class-level Javadoc (bytecode always carries
/// at least the compiler-synthesized no-arg `<init>`, so the external path
/// only takes this branch for a type with no constructors whatsoever, e.g.
/// an interface — not valid to `new`, but handled gracefully all the same).
fn constructor_target<'t>(call: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Target<'t>> {
    let resolved_ty = resolve::resolve_object_creation_type(call, ctx)?;
    let arg_count = call_arg_count(call);
    match resolved_ty {
        // Array creation is a different node kind — a stray Array resolution
        // has no constructors to show.
        ResolvedType::Array { .. } => None,
        ResolvedType::InProject(td) => {
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
            // A constructor with no Javadoc of its own falls back to the
            // class-level Javadoc (also the only doc a synthesized default
            // constructor can show, since there's no declaration node to
            // carry one).
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
            let sig = resolve::display_signature(chosen, &args, &class.type_params);
            // Constructor Javadoc is recovered docsrc-style, by the class's
            // simple name (a source archive has no `<init>`, only a
            // constructor declaration named after its class — see
            // `jvl_classpath::MemberKind::Constructor`); fall back to the
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
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

    /// A `SymbolSource` that resolves exactly one class, for external hover tests.
    struct OneClass {
        fqn: &'static str,
        members: Vec<ExternalMember>,
    }

    impl SymbolSource for OneClass {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            (fqn == self.fqn).then(|| ExternalClass {
                supers: Vec::new(),
                type_params: Vec::new(),
                members: self
                    .members
                    .iter()
                    .map(|m| ExternalMember {
                        name: m.name.clone(),
                        kind: m.kind,
                        signature: m.signature.clone(),
                        template: m.template.clone(),
                        is_static: m.is_static,
                        ret_fqn: m.ret_fqn.clone(),
                        ret_display: m.ret_display.clone(),
                    })
                    .collect(),
            })
        }
    }

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    /// Hover with the cursor one byte into the first occurrence of `marker`
    /// (markers start with the identifier of interest).
    fn hover_text(src: &str, marker: &str) -> Option<String> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        // Cursor on the first byte of the identifier the marker begins with.
        let at = src.find(marker).expect("marker present");
        let h = hover(&docs, 0, &index, index.position(at), &NoSymbols)?;
        match h.contents {
            HoverContents::Markup(m) => Some(m.value),
            _ => None,
        }
    }

    #[test]
    fn hover_on_method_call_shows_signature_and_javadoc() {
        let src = "class C { /** Adds two numbers. */ int add(int a, int b) { return a + b; }\n\
                   void m() { add(1, 2); } }\n";
        let text = hover_text(src, "add(1").expect("hover");
        assert!(text.contains("int add(int a, int b)"), "{text}");
        assert!(text.contains("Adds two numbers."), "{text}");
    }

    #[test]
    fn hover_on_declaration_name() {
        let src = "class C { /** Adds. */ int add(int a, int b) { return a + b; } }\n";
        let text = hover_text(src, "add(int").expect("hover");
        assert!(text.contains("int add(int a, int b)"), "{text}");
        assert!(text.contains("Adds."), "{text}");
    }

    #[test]
    fn hover_on_field_reference() {
        let src = "class C { /** the user's name */ String name;\n\
                   void m() { name.length(); } }\n";
        let text = hover_text(src, "name.").expect("hover");
        assert!(text.contains("String name"), "{text}");
        assert!(text.contains("the user's name"), "{text}");
    }

    #[test]
    fn hover_on_local_reference() {
        let src = "class C { void m() { int count = 0; count++; } }\n";
        let text = hover_text(src, "count+").expect("hover");
        assert!(text.contains("int count"), "{text}");
    }

    #[test]
    fn hover_on_type_identifier() {
        let src = "class Widget {}\nclass C { Widget w; }\n";
        let text = hover_text(src, "Widget w").expect("hover");
        assert!(text.contains("class Widget"), "{text}");
    }

    #[test]
    fn hover_signature_is_fenced_java() {
        let src = "class C { int x; }\n";
        let text = hover_text(src, "x;").expect("hover");
        assert!(text.starts_with("```java\n"), "{text}");
    }

    #[test]
    fn hover_cross_file_member() {
        let lib = "class Gadget { /** ticks */ int tick() { return 0; } }\n";
        let use_src = "class C { void m() { Gadget g; g.tick(); } }\n";
        let lib_tree = tree(lib);
        let use_tree = tree(use_src);
        let docs = [
            OpenDoc {
                source: use_src,
                tree: &use_tree,
            },
            OpenDoc {
                source: lib,
                tree: &lib_tree,
            },
        ];
        let index = LineIndex::new(use_src, PositionEncoding::Utf16);
        let at = use_src.find("tick(").unwrap();
        let h = hover(&docs, 0, &index, index.position(at), &NoSymbols).expect("hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("int tick()"), "{}", m.value);
        assert!(m.value.contains("ticks"), "{}", m.value);
    }

    #[test]
    fn hover_on_unterminated_scoped_member() {
        // `Helper.S` with no trailing `;` parses as a scoped_type_identifier;
        // hover on the trailing segment must still resolve the member.
        let src = "class Helper { /** the constant */ static int S = 1; }\n\
                   class C { void m() { Helper.S } }\n";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("Helper.S").unwrap() + "Helper.".len(); // cursor on `S`
        let h = hover(&docs, 0, &index, index.position(at), &NoSymbols).expect("hover on S");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("static int S"), "{}", m.value);
        assert!(m.value.contains("the constant"), "{}", m.value);
    }

    #[test]
    fn hover_on_non_identifier_is_none() {
        // Cursor on the opening brace -> nothing to show.
        let src = "class C { }\n";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find('{').unwrap();
        assert!(hover(&docs, 0, &index, index.position(at), &NoSymbols).is_none());
    }

    // --- M6.3: hover on the type name inside `new Foo(...)` ---

    #[test]
    fn hover_on_new_expression_picks_arity_matching_constructor() {
        let src = "class Foo {\n\
                   /** No-arg. */\n\
                   Foo() {}\n\
                   /** Takes two ints. */\n\
                   Foo(int a, int b) {}\n\
                   }\n\
                   class C { void m() { Foo f = new Foo(1, 2); } }\n";
        let text = hover_text(src, "Foo(1, 2)").expect("hover");
        assert!(text.contains("Foo(int a, int b)"), "{text}");
        assert!(text.contains("Takes two ints."), "{text}");
        assert!(!text.contains("No-arg."), "{text}");
    }

    #[test]
    fn hover_on_new_expression_falls_back_to_class_doc_when_ctor_undocumented() {
        let src = "/** The Foo type. */\n\
                   class Foo {\n\
                   Foo(int a) {}\n\
                   }\n\
                   class C { void m() { Foo f = new Foo(1); } }\n";
        let text = hover_text(src, "Foo(1)").expect("hover");
        assert!(text.contains("Foo(int a)"), "{text}");
        assert!(text.contains("The Foo type."), "{text}");
    }

    #[test]
    fn hover_on_new_expression_without_explicit_constructor_shows_default_and_class_doc() {
        let src = "/** The Foo type. */\n\
                   class Foo {}\n\
                   class C { void m() { Foo f = new Foo(); } }\n";
        let text = hover_text(src, "Foo()").expect("hover");
        assert!(text.contains("Foo()"), "{text}");
        assert!(text.contains("The Foo type."), "{text}");
    }

    #[test]
    fn hover_on_external_new_expression_shows_constructor_signature_and_doc() {
        let src = "import test.Widget;\nclass C { void m() { Widget w = new Widget(1); } }\n";
        let symbols = OneClass {
            fqn: "test.Widget",
            members: vec![
                ExternalMember {
                    name: "Widget".to_string(),
                    kind: ExternalMemberKind::Constructor,
                    signature: "Widget()".to_string(),
                    template: None,
                    is_static: false,
                    ret_fqn: None,
                    ret_display: None,
                },
                ExternalMember {
                    name: "Widget".to_string(),
                    kind: ExternalMemberKind::Constructor,
                    signature: "Widget(int)".to_string(),
                    template: None,
                    is_static: false,
                    ret_fqn: None,
                    ret_display: None,
                },
            ],
        };
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("Widget(1)").unwrap();
        let h = hover(&docs, 0, &index, index.position(at), &symbols).expect("hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("Widget(int)"), "{}", m.value);
    }

    #[test]
    fn hover_on_external_new_expression_without_constructors_shows_default_and_class_doc() {
        let src = "import test.Widget;\nclass C { void m() { Widget w = new Widget(); } }\n";
        struct NoCtorClass;
        impl SymbolSource for NoCtorClass {
            fn class(&self, fqn: &str) -> Option<ExternalClass> {
                (fqn == "test.Widget").then(|| ExternalClass {
                    supers: Vec::new(),
                    type_params: Vec::new(),
                    members: Vec::new(),
                })
            }
            fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
                (fqn == "test.Widget" && member.is_none()).then(|| "The Widget type.".to_string())
            }
        }
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("Widget()").unwrap();
        let h = hover(&docs, 0, &index, index.position(at), &NoCtorClass).expect("hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("Widget()"), "{}", m.value);
        assert!(m.value.contains("The Widget type."), "{}", m.value);
    }

    #[test]
    fn hover_on_external_member() {
        let src = "import java.util.List;\nclass C { void m() { List xs; xs.size(); } }\n";
        let symbols = OneClass {
            fqn: "java.util.List",
            members: vec![ExternalMember {
                name: "size".to_string(),
                kind: ExternalMemberKind::Method,
                signature: "int size()".to_string(),
                template: None,
                is_static: false,
                ret_fqn: None,
                ret_display: None,
            }],
        };
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("size(").unwrap(); // cursor on `size`
        let h = hover(&docs, 0, &index, index.position(at), &symbols).expect("hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("markup")
        };
        assert!(m.value.contains("int size()"), "{}", m.value);
    }
}
