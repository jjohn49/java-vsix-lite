use super::*;
use crate::external::{ExternalClass, NoSymbols};
use crate::{new_parser, parse, PositionEncoding};
use std::collections::HashMap;
use tree_sitter::Tree;

/// A `SymbolSource` backed by a fixture map, for hermetic external-symbol tests.
struct MockSymbols(HashMap<String, ExternalClass>);

impl SymbolSource for MockSymbols {
    fn class(&self, fqn: &str) -> Option<ExternalClass> {
        self.0.get(fqn).map(|c| ExternalClass {
            supers: c.supers.clone(),
            type_params: c.type_params.clone(),
            members: c
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

fn ext_method(name: &str, signature: &str) -> ExternalMember {
    ExternalMember {
        name: name.to_string(),
        kind: ExternalMemberKind::Method,
        signature: signature.to_string(),
        template: None,
        is_static: false,
        ret_fqn: None,
        ret_display: None,
    }
}

/// A method member carrying a generic template (e.g. `boolean add({0})`).
fn ext_generic_method(name: &str, erased: &str, template: &str) -> ExternalMember {
    ExternalMember {
        name: name.to_string(),
        kind: ExternalMemberKind::Method,
        signature: erased.to_string(),
        template: Some(template.to_string()),
        is_static: false,
        ret_fqn: None,
        ret_display: None,
    }
}

fn tree(src: &str) -> Tree {
    parse(&mut new_parser(), src, None).expect("parse")
}

/// Completion at the byte position **immediately after** the first occurrence
/// of `marker` (so `"d."` places the cursor right after the dot).
fn complete(src: &str, marker: &str) -> Vec<CompletionItem> {
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let index = LineIndex::new(src, PositionEncoding::Utf16);
    let at = src.find(marker).expect("marker present") + marker.len();
    completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

fn has(items: &[CompletionItem], label: &str) -> bool {
    items.iter().any(|i| i.label == label)
}

fn detail_of<'a>(items: &'a [CompletionItem], label: &str) -> Option<&'a str> {
    items
        .iter()
        .find(|i| i.label == label)
        .and_then(|i| i.detail.as_deref())
}

#[test]
fn member_completion_on_local_variable() {
    let src = "class Box { int width; int height() { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
    let items = complete(src, "b.");
    assert!(has(&items, "width"), "{:?}", labels(&items));
    assert!(has(&items, "height"), "{:?}", labels(&items));
    assert_eq!(detail_of(&items, "width"), Some("int width"));
    assert_eq!(detail_of(&items, "height"), Some("int height()"));
    // Member completion must NOT include scope noise like keywords.
    assert!(!has(&items, "class"));
}

#[test]
fn member_completion_on_this_includes_inherited() {
    let src = "class Base { int baseField; void baseM() {} }\n\
                   class Derived extends Base { int own; void m() { this.x; } }\n";
    let items = complete(src, "this.");
    assert!(has(&items, "own"), "{:?}", labels(&items));
    assert!(
        has(&items, "baseField"),
        "inherited field {:?}",
        labels(&items)
    );
    assert!(
        has(&items, "baseM"),
        "inherited method {:?}",
        labels(&items)
    );
}

#[test]
fn member_completion_on_super() {
    let src = "class Base { int baseField; }\n\
                   class Derived extends Base { void m() { super.x; } }\n";
    let items = complete(src, "super.");
    assert!(has(&items, "baseField"), "{:?}", labels(&items));
}

#[test]
fn member_completion_on_new_expression() {
    let src = "class Box { int w; }\n\
                   class C { void m() { new Box().x; } }\n";
    let items = complete(src, ").");
    assert!(has(&items, "w"), "{:?}", labels(&items));
}

#[test]
fn member_completion_through_field_access_chain() {
    let src = "class Inner { int leaf; }\n\
                   class C { Inner inner; void m() { this.inner.x; } }\n";
    let items = complete(src, "inner.");
    assert!(has(&items, "leaf"), "{:?}", labels(&items));
}

#[test]
fn inner_binding_shadows_outer_field_for_member_type() {
    let src = "class A { int aOnly; }\n\
                   class B { int bOnly; }\n\
                   class C { A v; void m() { B v; v.x; } }\n";
    let items = complete(src, "v.");
    assert!(
        has(&items, "bOnly"),
        "inner type wins: {:?}",
        labels(&items)
    );
    assert!(
        !has(&items, "aOnly"),
        "outer field shadowed: {:?}",
        labels(&items)
    );
}

#[test]
fn unresolved_receiver_yields_no_items() {
    // `foo()` is a method call — return-type inference is deferred.
    let src = "class C { void m() { foo().x; } }\n";
    let items = complete(src, "foo().");
    assert!(items.is_empty(), "{:?}", labels(&items));
}

#[test]
fn scope_completion_lists_locals_params_fields_keywords() {
    let src = "class C { int field; void m(int param) { int local = 1; ZZZ } }\n";
    let items = complete(src, "ZZZ");
    assert!(has(&items, "local"), "local: {:?}", labels(&items));
    assert!(has(&items, "param"), "param: {:?}", labels(&items));
    assert!(has(&items, "field"), "field: {:?}", labels(&items));
    assert!(has(&items, "return"), "keyword: {:?}", labels(&items));
    assert!(has(&items, "C"), "type name: {:?}", labels(&items));
    assert!(has(&items, "m"), "method: {:?}", labels(&items));
}

#[test]
fn scope_completion_respects_declaration_order() {
    let src = "class C { void m() { int before = 1; ZZZ; int after = 2; } }\n";
    let items = complete(src, "ZZZ");
    assert!(
        has(&items, "before"),
        "before-cursor local: {:?}",
        labels(&items)
    );
    assert!(
        !has(&items, "after"),
        "after-cursor local: {:?}",
        labels(&items)
    );
}

#[test]
fn method_item_inserts_call_snippet() {
    let src = "class Box { int height() { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
    let items = complete(src, "b.");
    let height = items.iter().find(|i| i.label == "height").unwrap();
    assert_eq!(height.insert_text.as_deref(), Some("height()"));
}

#[test]
fn cross_file_type_resolution() {
    let lib = "class Widget { int spin; }\n";
    let use_src = "class C { void m() { Widget w; w.x; } }\n";
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
    let at = use_src.find("w.").unwrap() + 2;
    let items = completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items;
    assert!(has(&items, "spin"), "cross-file: {:?}", labels(&items));
}

#[test]
fn trailing_dot_does_not_panic() {
    // Pathological: bare receiver with nothing after the dot.
    let _ = complete("class C { void m() { x. } }\n", "x.");
    let _ = complete("class C { void m() { . } }\n", ".");
    let _ = complete("", "");
}

// --- Regression tests for the adversarial review findings ---

fn complete_snip(src: &str, marker: &str, snippets: bool) -> Vec<CompletionItem> {
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let index = LineIndex::new(src, PositionEncoding::Utf16);
    let at = src.find(marker).expect("marker present") + marker.len();
    completion(&docs, 0, &index, index.position(at), snippets, &NoSymbols).items
}

#[test]
fn member_completion_while_typing_partial_unterminated() {
    // The dominant live case: partial member, no trailing `;`. tree-sitter
    // parses `b.wi` as a scoped_type_identifier of type_identifier segments.
    let src = "class Box { int width; int height; }\n\
                   class C { void m() { Box b; b.wi } }\n";
    let items = complete(src, "b.wi");
    assert!(has(&items, "width"), "{:?}", labels(&items));
    assert!(has(&items, "height"), "{:?}", labels(&items));
}

#[test]
fn static_type_receiver_shows_static_members_only() {
    let src = "class Helper { static int S = 1; static void sm() {} int inst; }\n\
                   class C { void m() { Helper.s } }\n";
    let items = complete(src, "Helper.s");
    assert!(has(&items, "S"), "static field: {:?}", labels(&items));
    assert!(has(&items, "sm"), "static method: {:?}", labels(&items));
    assert!(
        !has(&items, "inst"),
        "instance member excluded: {:?}",
        labels(&items)
    );
}

#[test]
fn interface_constants_are_inherited_members() {
    let src = "interface Sized { int MAX = 10; int size(); }\n\
                   class C implements Sized { void m() { this.x; } }\n";
    let items = complete(src, "this.");
    assert!(
        has(&items, "MAX"),
        "interface constant: {:?}",
        labels(&items)
    );
    assert!(
        has(&items, "size"),
        "interface method: {:?}",
        labels(&items)
    );
}

#[test]
fn varargs_parameter_is_in_scope_with_correct_signature() {
    let src = "class C { void m(int... xs) { ZZZ } }\n";
    let items = complete(src, "ZZZ");
    assert!(has(&items, "xs"), "varargs binding: {:?}", labels(&items));
    assert_eq!(detail_of(&items, "xs"), Some("int... xs"));
}

#[test]
fn method_insert_falls_back_to_plaintext_without_snippet_support() {
    let src = "class Box { int f(int n) { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
    let with = complete_snip(src, "b.", true);
    let without = complete_snip(src, "b.", false);
    let f_with = with.iter().find(|i| i.label == "f").unwrap();
    let f_without = without.iter().find(|i| i.label == "f").unwrap();
    assert_eq!(f_with.insert_text.as_deref(), Some("f($1)"));
    assert_eq!(f_with.insert_text_format, Some(InsertTextFormat::SNIPPET));
    assert_eq!(f_without.insert_text.as_deref(), Some("f("));
    assert_eq!(f_without.insert_text_format, None);
}

#[test]
fn dollar_in_method_name_is_escaped_in_snippet() {
    // `$` is a legal Java identifier char and is snippet-special.
    let src = "class Box { int a$b(int n) { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
    let items = complete(src, "b.");
    let m = items.iter().find(|i| i.label == "a$b").unwrap();
    assert_eq!(m.insert_text.as_deref(), Some("a\\$b($1)"));
}

#[test]
fn deeply_nested_receiver_does_not_overflow_the_stack() {
    // Without a depth bound this recurses ~5000 deep and aborts the process.
    let depth = 5000;
    let src = format!(
        "class C {{ void m() {{ {}a{}. }} }}\n",
        "(".repeat(depth),
        ")".repeat(depth)
    );
    // Cursor right after the final dot; must return (gracefully empty) not crash.
    let tree = tree(&src);
    let docs = [OpenDoc {
        source: &src,
        tree: &tree,
    }];
    let index = LineIndex::new(&src, PositionEncoding::Utf16);
    let at = src.rfind('.').unwrap() + 1;
    let _ = completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items;
}

// --- External (JDK/dependency) symbol resolution, via a mock SymbolSource ---

fn complete_ext(src: &str, marker: &str, symbols: &dyn SymbolSource) -> Vec<CompletionItem> {
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let index = LineIndex::new(src, PositionEncoding::Utf16);
    let at = src.find(marker).expect("marker present") + marker.len();
    completion(&docs, 0, &index, index.position(at), true, symbols).items
}

/// Lombok-synthesized accessors appear in member completion and
/// chain like any other member (`p.getName().` resolves to `String`).
#[test]
fn lombok_getter_completes_and_chains() {
    let symbols = mock(vec![(
        "java.lang.String",
        ext_class(&[], vec![ext_method("length", "int length()")]),
    )]);
    let src = "import lombok.Getter;\n\
                   @Getter class Person { private String name; void m(Person p) { p. } }\n";
    let items = complete_ext(src, "{ p.", &symbols);
    assert!(has(&items, "getName"), "{:?}", labels(&items));

    let src = "import lombok.Getter;\n\
                   @Getter class Person { private String name; void m(Person p) { p.getName().x; } }\n";
    let items = complete_ext(src, "p.getName().", &symbols);
    assert!(
        has(&items, "length"),
        "chain through the synthesized getter: {:?}",
        labels(&items)
    );

    // Without the lombok import, the same annotation synthesizes nothing.
    let src = "@Getter class Person { private String name; void m(Person p) { p. } }\n";
    let items = complete_ext(src, "{ p.", &symbols);
    assert!(!has(&items, "getName"), "{:?}", labels(&items));
}

fn mock(entries: Vec<(&str, ExternalClass)>) -> MockSymbols {
    MockSymbols(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn ext_class(supers: &[&str], members: Vec<ExternalMember>) -> ExternalClass {
    ExternalClass {
        supers: supers.iter().map(|s| s.to_string()).collect(),
        type_params: Vec::new(),
        members,
    }
}

fn ext_generic_class(
    type_params: &[&str],
    supers: &[&str],
    members: Vec<ExternalMember>,
) -> ExternalClass {
    ExternalClass {
        supers: supers.iter().map(|s| s.to_string()).collect(),
        type_params: type_params.iter().map(|s| s.to_string()).collect(),
        members,
    }
}

#[test]
fn external_member_completion_via_explicit_import() {
    let src = "import java.util.List;\nclass C { void m() { List xs; xs.x; } }\n";
    let symbols = mock(vec![(
        "java.util.List",
        ext_class(
            &[],
            vec![
                ext_method("add", "boolean add(Object)"),
                ext_method("get", "Object get(int)"),
                ext_method("size", "int size()"),
            ],
        ),
    )]);
    let items = complete_ext(src, "xs.", &symbols);
    assert!(has(&items, "add"), "{:?}", labels(&items));
    assert!(has(&items, "get"));
    assert!(has(&items, "size"));
    assert_eq!(detail_of(&items, "size"), Some("int size()"));
}

#[test]
fn external_completion_via_wildcard_import() {
    let src = "import java.util.*;\nclass C { void m() { Map xs; xs.x; } }\n";
    let symbols = mock(vec![(
        "java.util.Map",
        ext_class(&[], vec![ext_method("put", "Object put(Object, Object)")]),
    )]);
    assert!(has(&complete_ext(src, "xs.", &symbols), "put"));
}

#[test]
fn external_completion_via_implicit_java_lang() {
    let src = "class C { void m() { String s; s.x; } }\n";
    let symbols = mock(vec![(
        "java.lang.String",
        ext_class(&[], vec![ext_method("length", "int length()")]),
    )]);
    assert!(has(&complete_ext(src, "s.", &symbols), "length"));
}

#[test]
fn inproject_extending_external_inherits_members() {
    let src =
        "import java.util.ArrayList;\nclass MyList extends ArrayList { void m() { this.x; } }\n";
    let symbols = mock(vec![
        (
            "java.util.ArrayList",
            ext_class(
                &["java.lang.Object"],
                vec![ext_method("add", "boolean add(Object)")],
            ),
        ),
        (
            "java.lang.Object",
            ext_class(&[], vec![ext_method("toString", "String toString()")]),
        ),
    ]);
    let items = complete_ext(src, "this.", &symbols);
    assert!(
        has(&items, "add"),
        "inherited external: {:?}",
        labels(&items)
    );
    assert!(has(&items, "toString"), "via Object: {:?}", labels(&items));
}

#[test]
fn external_receiver_without_symbols_is_empty() {
    let src = "import java.util.List;\nclass C { void m() { List xs; xs.x; } }\n";
    let items = complete_ext(src, "xs.", &NoSymbols);
    assert!(items.is_empty(), "{:?}", labels(&items));
}

// --- Completion docs are strictly lazy (data payload, not eager) ---

#[test]
fn inproject_member_completion_carries_no_eager_documentation() {
    let src = "class Box {\n\
                   /** The width. */\n\
                   int width;\n\
                   }\n\
                   class C { void m() { Box b; b.x; } }\n";
    let items = complete(src, "b.");
    let width = items.iter().find(|i| i.label == "width").unwrap();
    assert!(
        width.documentation.is_none(),
        "completion must not eagerly fetch Javadoc: {:?}",
        width.documentation
    );
    let data = width.data.as_ref().expect("lazy-resolve data payload");
    // The payload names the declaring document by slice index (the
    // server translates it into a URI before the item goes on the wire).
    assert_eq!(data["doc"], 0, "{data:?}");
    assert_eq!(data["type"], "Box", "{data:?}");
    assert_eq!(data["member"], "width", "{data:?}");
}

/// The `"doc"` index names the *declaring* document —
/// for a member declared in another open file, that file's index, not
/// the completion request's current document.
#[test]
fn inproject_data_doc_index_names_the_declaring_document() {
    let lib = "class Widget { /** spins */ int spin; }\n";
    let use_src = "class C { void m() { Widget w; w.x; } }\n";
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
    let at = use_src.find("w.").unwrap() + 2;
    let items = completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items;
    let spin = items.iter().find(|i| i.label == "spin").unwrap();
    let data = spin.data.as_ref().expect("lazy-resolve data payload");
    assert_eq!(data["doc"], 1, "declaring doc is docs[1]: {data:?}");
}

#[test]
fn external_member_completion_carries_no_documentation_but_has_data_when_receiver_external() {
    let src = "import java.util.List;\nclass C { void m() { List xs; xs.x; } }\n";
    let symbols = mock(vec![(
        "java.util.List",
        ext_class(&[], vec![ext_method("size", "int size()")]),
    )]);
    let items = complete_ext(src, "xs.", &symbols);
    let size = items.iter().find(|i| i.label == "size").unwrap();
    assert!(size.documentation.is_none(), "{:?}", size.documentation);
    assert!(size.data.is_some(), "expected a lazy-resolve data payload");
}

#[test]
fn external_member_reached_through_inproject_receiver_has_no_lazy_data() {
    // Same limitation hover already accepts (see `hover::member_target`):
    // an external member inherited through an *in-project* receiver has
    // no FQN on hand at the point the item is built, so it gets no lazy
    // doc key at all (rather than a broken one).
    let src =
        "import java.util.ArrayList;\nclass MyList extends ArrayList { void m() { this.x; } }\n";
    let symbols = mock(vec![(
        "java.util.ArrayList",
        ext_class(&[], vec![ext_method("add", "boolean add(Object)")]),
    )]);
    let items = complete_ext(src, "this.", &symbols);
    let add = items.iter().find(|i| i.label == "add").unwrap();
    assert!(add.data.is_none(), "{:?}", add.data);
}

#[test]
fn resolve_documentation_finds_inproject_member_javadoc() {
    let src = "class Box {\n\
                   /** The width. */\n\
                   int width;\n\
                   }\n";
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let data = serde_json::json!({"kind": "inproject", "type": "Box", "member": "width"});
    let doc = resolve_documentation(&docs, &data, &NoSymbols).expect("doc resolved");
    match doc {
        Documentation::MarkupContent(m) => assert_eq!(m.value, "The width."),
        other => panic!("expected markup, got {other:?}"),
    }
}

/// With two files both declaring a `Box.width`, the
/// caller narrows `docs` to the originating document — and gets *that*
/// document's Javadoc, not whichever same-named type an all-docs scan
/// would have found first. An empty slice (originating document closed
/// since completion) yields no documentation rather than a guess.
#[test]
fn resolve_documentation_scoped_to_originating_document_only() {
    let src_a = "class Box { /** From A. */ int width; }\n";
    let src_b = "class Box { /** From B. */ int width; }\n";
    let tree_a = tree(src_a);
    let tree_b = tree(src_b);
    let data = serde_json::json!({
        "kind": "inproject", "type": "Box", "member": "width", "doc": 0,
    });

    let from = |src, t| {
        let docs = [OpenDoc {
            source: src,
            tree: t,
        }];
        resolve_documentation(&docs, &data, &NoSymbols)
    };
    match from(src_a, &tree_a).expect("doc resolved") {
        Documentation::MarkupContent(m) => assert_eq!(m.value, "From A."),
        other => panic!("expected markup, got {other:?}"),
    }
    match from(src_b, &tree_b).expect("doc resolved") {
        Documentation::MarkupContent(m) => assert_eq!(m.value, "From B."),
        other => panic!("expected markup, got {other:?}"),
    }
    // Originating document gone: no documentation, never a guess.
    assert!(resolve_documentation(&[], &data, &NoSymbols).is_none());
}

#[test]
fn resolve_documentation_finds_external_member_javadoc() {
    let symbols = mock(vec![]);
    struct DocStub;
    impl SymbolSource for DocStub {
        fn class(&self, _fqn: &str) -> Option<ExternalClass> {
            None
        }
        fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
            (fqn == "java.util.List" && member == Some("size"))
                .then(|| "Returns the size.".to_string())
        }
    }
    let _ = symbols; // unused; DocStub carries the fixture instead
    let src = "";
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let data = serde_json::json!({"kind": "external", "fqn": "java.util.List", "member": "size"});
    let doc = resolve_documentation(&docs, &data, &DocStub).expect("doc resolved");
    match doc {
        Documentation::MarkupContent(m) => assert_eq!(m.value, "Returns the size."),
        other => panic!("expected markup, got {other:?}"),
    }
}

#[test]
fn resolve_documentation_missing_or_unknown_data_is_none() {
    let src = "class Box { int width; }\n";
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    assert!(resolve_documentation(&docs, &serde_json::json!({}), &NoSymbols).is_none());
    assert!(resolve_documentation(
        &docs,
        &serde_json::json!({"kind": "bogus", "member": "x"}),
        &NoSymbols
    )
    .is_none());
    assert!(resolve_documentation(
        &docs,
        &serde_json::json!({"kind": "inproject", "type": "NoSuchType", "member": "width"}),
        &NoSymbols
    )
    .is_none());
}

// --- Everyday IntelliSense — chains, statics, var, casts, arrays ---

/// A method whose (erased) return type is an object — enough for a chain
/// to continue through `ret_fqn`.
fn ext_method_ret(name: &str, signature: &str, ret_fqn: &str) -> ExternalMember {
    ExternalMember {
        ret_fqn: Some(ret_fqn.to_string()),
        ..ext_method(name, signature)
    }
}

/// A generic method carrying both chain fields (`ret_display` in `{i}`
/// template form).
fn ext_method_ret_display(
    name: &str,
    signature: &str,
    ret_fqn: &str,
    ret_display: &str,
) -> ExternalMember {
    ExternalMember {
        ret_fqn: Some(ret_fqn.to_string()),
        ret_display: Some(ret_display.to_string()),
        ..ext_method(name, signature)
    }
}

fn ext_static_field_ret(name: &str, signature: &str, ret_fqn: &str) -> ExternalMember {
    ExternalMember {
        name: name.to_string(),
        kind: ExternalMemberKind::Field,
        signature: signature.to_string(),
        template: None,
        is_static: true,
        ret_fqn: Some(ret_fqn.to_string()),
        ret_display: None,
    }
}

/// A JDK-shaped fixture: List/Stream/String/System/PrintStream/Map.Entry
/// with just enough members to exercise every chain shape.
fn rich_mock() -> MockSymbols {
    mock(vec![
        (
            "java.util.List",
            ext_generic_class(
                &["E"],
                &[],
                vec![
                    ext_generic_method("add", "boolean add(Object)", "boolean add({0})"),
                    ExternalMember {
                        ret_fqn: Some("java.lang.Object".to_string()),
                        ret_display: Some("{0}".to_string()),
                        ..ext_generic_method("get", "Object get(int)", "{0} get(int)")
                    },
                    ext_method_ret_display(
                        "stream",
                        "Stream stream()",
                        "java.util.stream.Stream",
                        "Stream<{0}>",
                    ),
                    ExternalMember {
                        is_static: true,
                        ..ext_method_ret_display("of", "List of()", "java.util.List", "List<E>")
                    },
                ],
            ),
        ),
        (
            "java.util.ArrayList",
            ext_generic_class(
                &["E"],
                &[],
                vec![ext_generic_method(
                    "add",
                    "boolean add(Object)",
                    "boolean add({0})",
                )],
            ),
        ),
        (
            "java.util.stream.Stream",
            ext_generic_class(
                &["T"],
                &[],
                vec![
                    ext_method("count", "long count()"),
                    ext_method_ret_display(
                        "filter",
                        "Stream filter(Predicate)",
                        "java.util.stream.Stream",
                        "Stream<{0}>",
                    ),
                ],
            ),
        ),
        (
            "java.lang.String",
            ext_class(
                &[],
                vec![
                    ext_method("length", "int length()"),
                    ext_method_ret("trim", "String trim()", "java.lang.String"),
                ],
            ),
        ),
        (
            "java.lang.System",
            ext_class(
                &[],
                vec![ext_static_field_ret(
                    "out",
                    "PrintStream out",
                    "java.io.PrintStream",
                )],
            ),
        ),
        (
            "java.io.PrintStream",
            ext_class(&[], vec![ext_method("println", "void println(String)")]),
        ),
        (
            "java.util.Map",
            ext_class(&[], vec![ext_method("put", "Object put(Object, Object)")]),
        ),
        (
            "java.util.Map$Entry",
            ext_class(
                &[],
                vec![ExternalMember {
                    is_static: true,
                    ..ext_method("comparingByKey", "Comparator comparingByKey()")
                }],
            ),
        ),
        (
            "java.lang.Object",
            ext_class(&[], vec![ext_method("toString", "String toString()")]),
        ),
    ])
}

#[test]
fn chained_method_call_on_external_receiver() {
    let src = "import java.util.List;\n\
                   class C { void m() { List<String> xs; xs.stream().x; } }\n";
    let items = complete_ext(src, "xs.stream().", &rich_mock());
    assert!(has(&items, "count"), "{:?}", labels(&items));
    assert!(has(&items, "filter"), "{:?}", labels(&items));
}

#[test]
fn chained_call_substitutes_type_var_return() {
    // List<String>.get(int) returns {0} = String — the chain must land on
    // java.lang.String via the implicit java.lang resolution.
    let src = "import java.util.List;\n\
                   class C { void m() { List<String> xs; xs.get(0).x; } }\n";
    let items = complete_ext(src, "xs.get(0).", &rich_mock());
    assert!(has(&items, "length"), "{:?}", labels(&items));
}

#[test]
fn chain_through_erased_return_without_signature() {
    let src = "class C { void m() { String s; s.trim().x; } }\n";
    let items = complete_ext(src, "s.trim().", &rich_mock());
    assert!(has(&items, "length"), "{:?}", labels(&items));
}

#[test]
fn system_out_member_completion() {
    let src = "class C { void m() { System.out.x; } }\n";
    let items = complete_ext(src, "System.out.", &rich_mock());
    assert!(has(&items, "println"), "{:?}", labels(&items));
}

#[test]
fn static_method_chain_on_type_receiver() {
    let src = "import java.util.List;\n\
                   class C { void m() { List.of().x; } }\n";
    let items = complete_ext(src, "List.of().", &rich_mock());
    assert!(has(&items, "add"), "{:?}", labels(&items));
    assert!(has(&items, "stream"), "{:?}", labels(&items));
}

#[test]
fn in_project_method_return_chains() {
    let src = "class Foo { int leaf; Foo self() { return this; } }\n\
                   class C { void m() { Foo f; f.self().x; } }\n";
    let items = complete(src, "f.self().");
    assert!(has(&items, "leaf"), "{:?}", labels(&items));
}

#[test]
fn unqualified_call_in_own_class_chains() {
    let src = "class Foo { int leaf; }\n\
                   class C { Foo make() { return null; } void m() { make().x; } }\n";
    let items = complete(src, "make().");
    assert!(has(&items, "leaf"), "{:?}", labels(&items));
}

#[test]
fn var_infers_from_object_creation_initializer() {
    let src = "import java.util.ArrayList;\n\
                   class C { void m() { var v = new ArrayList<String>(); v.x; } }\n";
    let items = complete_ext(src, "v.", &rich_mock());
    assert!(has(&items, "add"), "{:?}", labels(&items));
    // Generic substitution flows through the inferred type.
    assert_eq!(detail_of(&items, "add"), Some("boolean add(String)"));
}

#[test]
fn var_infers_from_chained_initializer() {
    let src = "class C { void m() { String s; var t = s.trim(); t.x; } }\n";
    let items = complete_ext(src, "t.", &rich_mock());
    assert!(has(&items, "length"), "{:?}", labels(&items));
}

#[test]
fn cast_receiver_resolves_to_cast_type() {
    let src = "import java.util.List;\n\
                   class C { void m(Object o) { ((List) o).x; } }\n";
    let items = complete_ext(src, "o).", &rich_mock());
    assert!(has(&items, "add"), "{:?}", labels(&items));
}

#[test]
fn array_receiver_offers_length_and_clone_not_element_members() {
    let src = "class C { void m(String[] a) { a.x; } }\n";
    let items = complete_ext(src, "a.", &rich_mock());
    assert!(has(&items, "length"), "{:?}", labels(&items));
    assert!(has(&items, "clone"), "{:?}", labels(&items));
    assert!(
        !has(&items, "trim"),
        "element members must not leak: {:?}",
        labels(&items)
    );
    assert!(
        has(&items, "toString"),
        "arrays are Objects: {:?}",
        labels(&items)
    );
}

/// A field's *declared* type drives completion — whether or not any
/// constructor (or initializer) ever assigns it, and whether it's
/// reached bare, via `this.`, or through a generic container type.
/// (User-reported concern re: an uninitialized `byName` map field.)
#[test]
fn uninitialized_field_completes_from_declared_type() {
    let src = "import java.util.Map;\n\
                   class Repo {\n\
                   private Map<String, String> byName;\n\
                   void m() { byName.x; }\n\
                   void n() { this.byName.x; }\n\
                   }\n";
    let symbols = mock(vec![(
        "java.util.Map",
        ext_generic_class(
            &["K", "V"],
            &[],
            vec![ext_generic_method(
                "get",
                "Object get(Object)",
                "{1} get({0})",
            )],
        ),
    )]);
    let items = complete_ext(src, "{ byName.", &symbols);
    assert!(has(&items, "get"), "bare field: {:?}", labels(&items));
    assert_eq!(detail_of(&items, "get"), Some("String get(String)"));
    let items = complete_ext(src, "this.byName.", &symbols);
    assert!(has(&items, "get"), "via this.: {:?}", labels(&items));
}

#[test]
fn nested_class_static_walk() {
    let src = "import java.util.Map;\n\
                   class C { void m() { Map.Entry.x; } }\n";
    let items = complete_ext(src, "Map.Entry.", &rich_mock());
    assert!(has(&items, "comparingByKey"), "{:?}", labels(&items));
}

#[test]
fn fully_qualified_type_receiver_stays_static_only() {
    let src = "class C { void m() { java.util.List.x; } }\n";
    let items = complete_ext(src, "java.util.List.", &rich_mock());
    assert!(has(&items, "of"), "{:?}", labels(&items));
    assert!(
        !has(&items, "add"),
        "instance members excluded on a type receiver: {:?}",
        labels(&items)
    );
}

// --- Classpath type names, auto-import, import-path completion ---

use crate::external::TypeCandidate;

fn cand(simple: &str, fqn: &str, import_path: &str) -> TypeCandidate {
    TypeCandidate {
        simple: simple.to_string(),
        fqn: fqn.to_string(),
        import_path: import_path.to_string(),
    }
}

/// A `SymbolSource` with a name index: candidate types (prefix-filtered
/// like the real index) and a package tree, plus optional classes.
struct NameSymbols {
    classes: HashMap<String, ExternalClass>,
    candidates: Vec<TypeCandidate>,
    truncated: bool,
    packages: HashMap<String, (Vec<String>, Vec<TypeCandidate>)>,
}

impl NameSymbols {
    fn of_candidates(candidates: Vec<TypeCandidate>) -> NameSymbols {
        NameSymbols {
            classes: HashMap::new(),
            candidates,
            truncated: false,
            packages: HashMap::new(),
        }
    }
}

impl SymbolSource for NameSymbols {
    fn class(&self, fqn: &str) -> Option<ExternalClass> {
        self.classes.get(fqn).map(|c| ExternalClass {
            supers: c.supers.clone(),
            type_params: c.type_params.clone(),
            members: c
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

    fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<TypeCandidate>, bool) {
        let hits: Vec<TypeCandidate> = self
            .candidates
            .iter()
            .filter(|c| {
                c.simple.len() >= prefix.len()
                    && c.simple[..prefix.len()].eq_ignore_ascii_case(prefix)
            })
            .cloned()
            .collect();
        let over = hits.len() > limit;
        (
            hits.into_iter().take(limit).collect(),
            self.truncated || over,
        )
    }

    fn package_children(&self, package: &str) -> (Vec<String>, Vec<TypeCandidate>) {
        self.packages.get(package).cloned().unwrap_or_default()
    }
}

/// Full-result variant of [`complete_ext`], for `is_incomplete` and
/// edit assertions.
fn complete_full(src: &str, marker: &str, symbols: &dyn SymbolSource) -> CompletionResult {
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let index = LineIndex::new(src, PositionEncoding::Utf16);
    let at = src.find(marker).expect("marker present") + marker.len();
    completion(&docs, 0, &index, index.position(at), true, symbols)
}

fn arraylist_symbols() -> NameSymbols {
    NameSymbols::of_candidates(vec![cand(
        "ArrayList",
        "java.util.ArrayList",
        "java.util.ArrayList",
    )])
}

fn find_type<'a>(items: &'a [CompletionItem], detail: &str) -> Option<&'a CompletionItem> {
    items.iter().find(|i| i.detail.as_deref() == Some(detail))
}

#[test]
fn classpath_type_completion_with_auto_import_after_last_import() {
    let src = "package demo;\n\nimport java.util.List;\n\nclass C { void m() { ArrayLi } }\n";
    let result = complete_full(src, "ArrayLi", &arraylist_symbols());
    let item = find_type(&result.items, "java.util.ArrayList").expect("candidate offered");
    assert_eq!(item.label, "ArrayList");
    assert_eq!(item.sort_text.as_deref(), Some("3ArrayList"));
    let edits = item.additional_text_edits.as_ref().expect("auto-import");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "\nimport java.util.ArrayList;");
    // Right after `import java.util.List;` — line 2 (0-based), col 23.
    assert_eq!(edits[0].range.start.line, 2);
    assert_eq!(edits[0].range.start.character, 22); // after `import java.util.List;`
    assert_eq!(edits[0].range.start, edits[0].range.end);
    assert_eq!(
        item.data,
        Some(json!({ "kind": "external_type", "fqn": "java.util.ArrayList" }))
    );
    // Deliberately always incomplete once a classpath/project query ran
    // at all — see the doc comment on `scope_items`'s type-name branch:
    // this forces the client to re-query fresh on every keystroke
    // rather than client-side-filtering a stale response, which is what
    // let a real match get buried/dropped in practice.
    assert!(result.is_incomplete);
}

#[test]
fn auto_import_lands_after_package_or_at_file_top() {
    // No imports: insert after the package declaration.
    let src = "package demo;\nclass C { void m() { ArrayLi } }\n";
    let result = complete_full(src, "ArrayLi", &arraylist_symbols());
    let item = find_type(&result.items, "java.util.ArrayList").unwrap();
    let edit = &item.additional_text_edits.as_ref().unwrap()[0];
    assert_eq!(edit.new_text, "\n\nimport java.util.ArrayList;");
    assert_eq!(edit.range.start.line, 0);
    assert_eq!(edit.range.start.character, 13); // after `package demo;`

    // No package either: insert at the very top.
    let src = "class C { void m() { ArrayLi } }\n";
    let result = complete_full(src, "ArrayLi", &arraylist_symbols());
    let item = find_type(&result.items, "java.util.ArrayList").unwrap();
    let edit = &item.additional_text_edits.as_ref().unwrap()[0];
    assert_eq!(edit.new_text, "import java.util.ArrayList;\n\n");
    assert_eq!(edit.range.start.line, 0);
    assert_eq!(edit.range.start.character, 0);
}

#[test]
fn classpath_types_require_min_prefix() {
    let src = "class C { void m() { A } }\n";
    let result = complete_full(src, "{ A", &arraylist_symbols());
    assert!(
        find_type(&result.items, "java.util.ArrayList").is_none(),
        "1-char prefix must not query the classpath"
    );
    // ...but the result must still be incomplete: the classpath types
    // were *withheld*, not absent — the very next character brings them
    // in, and a complete-marked 1-char response would freeze this
    // classpath-less list for the rest of the word (the field-reported
    // "typing `person` never shows `Person`" failure).
    assert!(result.is_incomplete);
}

#[test]
fn no_auto_import_when_already_usable() {
    // Already single-imported.
    let src = "import java.util.ArrayList;\nclass C { void m() { ArrayLi } }\n";
    let item_edits = |src: &str, symbols: &dyn SymbolSource| {
        let result = complete_full(src, "{ ArrayLi", symbols);
        let item = find_type(&result.items, "java.util.ArrayList")
            .unwrap_or_else(|| panic!("candidate offered for {src:?}"))
            .clone();
        item.additional_text_edits
    };
    assert_eq!(item_edits(src, &arraylist_symbols()), None);

    // Wildcard-covered.
    let src = "import java.util.*;\nclass C { void m() { ArrayLi } }\n";
    assert_eq!(item_edits(src, &arraylist_symbols()), None);

    // Same package.
    let symbols = NameSymbols::of_candidates(vec![cand("Widget", "demo.Widget", "demo.Widget")]);
    let src = "package demo;\nclass C { void m() { Widg } }\n";
    let result = complete_full(src, "Widg", &symbols);
    let item = find_type(&result.items, "demo.Widget").expect("same-package candidate");
    assert_eq!(item.additional_text_edits, None);

    // java.lang.
    let symbols =
        NameSymbols::of_candidates(vec![cand("String", "java.lang.String", "java.lang.String")]);
    let src = "class C { void m() { Stri } }\n";
    let result = complete_full(src, "Stri", &symbols);
    let item = find_type(&result.items, "java.lang.String").expect("java.lang candidate");
    assert_eq!(item.additional_text_edits, None);
}

#[test]
fn conflicting_single_import_hides_the_candidate() {
    let src = "import other.ArrayList;\nclass C { void m() { ArrayLi } }\n";
    let result = complete_full(src, "{ ArrayLi", &arraylist_symbols());
    assert!(
        find_type(&result.items, "java.util.ArrayList").is_none(),
        "a same-simple-name import to a different type makes the candidate unusable"
    );
}

#[test]
fn open_document_type_shadows_classpath_candidate() {
    let src = "class ArrayList {}\nclass C { void m() { ArrayLi } }\n";
    let result = complete_full(src, "{ ArrayLi", &arraylist_symbols());
    let hits: Vec<_> = result
        .items
        .iter()
        .filter(|i| i.label == "ArrayList")
        .collect();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(
        hits[0].sort_text.as_deref(),
        Some("2ArrayList"),
        "the in-project declaration wins"
    );
    assert_eq!(hits[0].additional_text_edits, None);
}

#[test]
fn result_is_incomplete_whether_or_not_the_candidate_set_was_truncated() {
    // Not truncated...
    let result = complete_full(
        "class C { void m() { ArrayLi } }\n",
        "ArrayLi",
        &arraylist_symbols(),
    );
    assert!(result.is_incomplete);

    // ...and truncated: both force a re-query, for the same reason.
    let mut symbols = arraylist_symbols();
    symbols.truncated = true;
    let result = complete_full("class C { void m() { ArrayLi } }\n", "ArrayLi", &symbols);
    assert!(result.is_incomplete);
}

#[test]
fn nested_candidate_always_carries_its_import() {
    // Even under `import java.util.*`, the *inner* simple name `Entry`
    // needs `import java.util.Map.Entry;`.
    let symbols = NameSymbols::of_candidates(vec![cand(
        "Entry",
        "java.util.Map$Entry",
        "java.util.Map.Entry",
    )]);
    let src = "import java.util.*;\nclass C { void m() { Entr } }\n";
    let result = complete_full(src, "Entr", &symbols);
    let item = find_type(&result.items, "java.util.Map.Entry").expect("nested candidate");
    let edits = item.additional_text_edits.as_ref().expect("nested import");
    assert_eq!(edits[0].new_text, "\nimport java.util.Map.Entry;");
}

#[test]
fn scope_items_rank_in_stable_buckets() {
    let src = "class C { int field; void m(int param) { int local = 1; ZZZ } }\n";
    let items = complete(src, "ZZZ");
    let sort_of = |label: &str| {
        items
            .iter()
            .find(|i| i.label == label)
            .and_then(|i| i.sort_text.clone())
            .unwrap_or_else(|| panic!("{label} present"))
    };
    assert_eq!(sort_of("local"), "0local");
    assert_eq!(sort_of("field"), "1field");
    assert_eq!(sort_of("C"), "2C");
    assert_eq!(sort_of("return"), "4return");
}

// --- Import-path completion ---

fn import_symbols() -> NameSymbols {
    let mut packages = HashMap::new();
    packages.insert("".to_string(), (vec!["java".to_string()], Vec::new()));
    packages.insert("java".to_string(), (vec!["util".to_string()], Vec::new()));
    packages.insert(
        "java.util".to_string(),
        (
            vec!["stream".to_string()],
            vec![
                cand("ArrayList", "java.util.ArrayList", "java.util.ArrayList"),
                cand("Map", "java.util.Map", "java.util.Map"),
                cand("Entry", "java.util.Map$Entry", "java.util.Map.Entry"),
            ],
        ),
    );
    let mut classes = HashMap::new();
    classes.insert(
        "java.util.Map".to_string(),
        ext_class(
            &[],
            vec![
                ExternalMember {
                    is_static: true,
                    ..ext_method("of", "Map of()")
                },
                ext_method("put", "Object put(Object, Object)"),
            ],
        ),
    );
    NameSymbols {
        classes,
        candidates: Vec::new(),
        truncated: false,
        packages,
    }
}

#[test]
fn import_completion_walks_packages_and_types() {
    let symbols = import_symbols();
    let items = complete_ext("import ja\n", "import ja", &symbols);
    assert!(has(&items, "java"), "{:?}", labels(&items));

    let items = complete_ext("import java.ut\n", "import java.ut", &symbols);
    assert!(has(&items, "util"), "{:?}", labels(&items));

    let items = complete_ext("import java.util.\n", "import java.util.", &symbols);
    assert!(has(&items, "stream"), "{:?}", labels(&items));
    assert!(has(&items, "ArrayList"), "{:?}", labels(&items));

    // Prefix filters both kinds.
    let items = complete_ext("import java.util.A\n", "import java.util.A", &symbols);
    assert!(has(&items, "ArrayList"));
    assert!(!has(&items, "stream"));

    // Keywords/locals never leak into an import path.
    assert!(!has(
        &complete_ext("import java.util.\n", "import java.util.", &symbols),
        "return"
    ));
}

#[test]
fn import_completion_walks_nested_types_and_static_members() {
    let symbols = import_symbols();
    // After a class segment: its nested types.
    let items = complete_ext("import java.util.Map.\n", "import java.util.Map.", &symbols);
    assert!(has(&items, "Entry"), "{:?}", labels(&items));

    // `import static` after a class: static members only.
    let items = complete_ext(
        "import static java.util.Map.\n",
        "import static java.util.Map.",
        &symbols,
    );
    assert!(has(&items, "of"), "{:?}", labels(&items));
    assert!(!has(&items, "put"), "instance member: {:?}", labels(&items));
    assert!(
        has(&items, "Entry"),
        "nested types stay: {:?}",
        labels(&items)
    );
}

#[test]
fn import_completion_offers_the_static_keyword() {
    let items = complete_ext("import st\n", "import st", &import_symbols());
    assert!(has(&items, "static"), "{:?}", labels(&items));
}

#[test]
fn resolve_documentation_finds_external_type_javadoc() {
    struct TypeDocStub;
    impl SymbolSource for TypeDocStub {
        fn class(&self, _fqn: &str) -> Option<ExternalClass> {
            None
        }
        fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
            (fqn == "java.util.List" && member.is_none())
                .then(|| "An ordered collection.".to_string())
        }
    }
    let src = "";
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let data = serde_json::json!({"kind": "external_type", "fqn": "java.util.List"});
    let doc = resolve_documentation(&docs, &data, &TypeDocStub).expect("type doc resolved");
    match doc {
        Documentation::MarkupContent(m) => assert_eq!(m.value, "An ordered collection."),
        other => panic!("expected markup, got {other:?}"),
    }
}

#[test]
fn generic_type_args_substituted_in_member_signatures() {
    let src = "import java.util.ArrayList;\n\
                   class C { void m() { ArrayList<String> xs; xs.x; } }\n";
    let symbols = mock(vec![(
        "java.util.ArrayList",
        ext_generic_class(
            &["E"],
            &[],
            vec![
                ext_generic_method("add", "boolean add(Object)", "boolean add({0})"),
                ext_generic_method("get", "Object get(int)", "{0} get(int)"),
            ],
        ),
    )]);
    let items = complete_ext(src, "xs.", &symbols);
    assert_eq!(detail_of(&items, "add"), Some("boolean add(String)"));
    assert_eq!(detail_of(&items, "get"), Some("String get(int)"));
}

#[test]
fn raw_type_without_args_keeps_erased_signature() {
    let src = "import java.util.ArrayList;\n\
                   class C { void m() { ArrayList xs; xs.x; } }\n";
    let symbols = mock(vec![(
        "java.util.ArrayList",
        ext_generic_class(
            &["E"],
            &[],
            vec![ext_generic_method(
                "add",
                "boolean add(Object)",
                "boolean add({0})",
            )],
        ),
    )]);
    // No type args at the use site -> erased signature.
    assert_eq!(
        detail_of(&complete_ext(src, "xs.", &symbols), "add"),
        Some("boolean add(Object)")
    );
}
