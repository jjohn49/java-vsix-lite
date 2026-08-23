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

/// Javadoc block/inline tags reach the hover as rendered Markdown
/// sections, not raw `@param` tag soup.
#[test]
fn hover_renders_javadoc_tags_as_markdown() {
    let src = "class C {\n\
                   /**\n\
                   \u{20}* Greets a person.\n\
                   \u{20}*\n\
                   \u{20}* @param name who to greet, never {@code null}\n\
                   \u{20}* @return the greeting\n\
                   \u{20}*/\n\
                   String greet(String name) { return name; }\n\
                   void m() { greet(\"x\"); } }\n";
    let text = hover_text(src, "greet(\"x\"").expect("hover");
    assert!(text.contains("**Parameters:**"), "{text}");
    assert!(
        text.contains("- `name` — who to greet, never `null`"),
        "{text}"
    );
    assert!(text.contains("**Returns:** the greeting"), "{text}");
    assert!(!text.contains("@param"), "raw tag leaked: {text}");
}

/// An override with no doc of its own — or only `{@inheritDoc}` —
/// inherits the supertype's Javadoc, at both the call site and the
/// declaration name.
#[test]
fn hover_inherits_javadoc_from_in_project_supertype() {
    let src = "class Base { /** Runs the base behavior. */ void go() {} }\n\
                   class Sub extends Base { @Override void go() {} }\n\
                   class C { void m() { Sub s; s.go(); } }\n";
    let text = hover_text(src, "go();").expect("hover");
    assert!(text.contains("Runs the base behavior."), "{text}");

    let src = "class Base { /** Runs it. */ void go() {} }\n\
                   class Sub extends Base { /** {@inheritDoc} */ void go() {} }\n\
                   class C { void m() { Sub s; s.go(); } }\n";
    let text = hover_text(src, "go();").expect("hover");
    assert!(text.contains("Runs it."), "inheritDoc-only: {text}");

    // Hover on the overriding declaration itself inherits too.
    let text = hover_text(src, "go() {} }\nclass C").expect("hover");
    assert!(text.contains("Runs it."), "decl name: {text}");

    // A method with its OWN doc never shows the super's.
    let src = "class Base { /** Base doc. */ void go() {} }\n\
                   class Sub extends Base { /** Own doc. */ void go() {} }\n\
                   class C { void m() { Sub s; s.go(); } }\n";
    let text = hover_text(src, "go();").expect("hover");
    assert!(text.contains("Own doc."), "{text}");
    assert!(!text.contains("Base doc."), "{text}");
}

/// Hover on an *external* type — in the import line, as a bare
/// usage, and as a nested class — shows its signature (FQN + type
/// params) plus the type-level Javadoc.
#[test]
fn hover_on_imported_external_type_shows_signature_and_javadoc() {
    struct Types;
    impl SymbolSource for Types {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            match fqn {
                "java.util.List" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: vec!["E".to_string()],
                    members: Vec::new(),
                }),
                "java.util.Map" | "java.util.Map$Entry" => Some(ExternalClass {
                    supers: Vec::new(),
                    type_params: Vec::new(),
                    members: Vec::new(),
                }),
                _ => None,
            }
        }
        fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
            (member.is_none() && fqn == "java.util.List")
                .then(|| "An ordered collection.".to_string())
        }
    }

    let hover_with = |src: &str, marker: &str| -> Option<String> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker");
        let h = hover(&docs, 0, &index, index.position(at), &Types)?;
        match h.contents {
            HoverContents::Markup(m) => Some(m.value),
            _ => None,
        }
    };

    // In the import declaration itself.
    let src = "import java.util.List;\nclass C { }\n";
    let text = hover_with(src, "List;").expect("hover in import");
    assert!(text.contains("java.util.List<E>"), "{text}");
    assert!(text.contains("An ordered collection."), "{text}");

    // On a bare usage of the imported name.
    let src = "import java.util.List;\nclass C { List xs; }\n";
    let text = hover_with(src, "List xs").expect("hover on usage");
    assert!(text.contains("java.util.List<E>"), "{text}");
    assert!(text.contains("An ordered collection."), "{text}");

    // A nested class in an import resolves through `$` substitution.
    let src = "import java.util.Map.Entry;\nclass C { }\n";
    let text = hover_with(src, "Entry;").expect("hover on nested import");
    assert!(text.contains("java.util.Map$Entry"), "{text}");

    // An unknown name still hovers to nothing.
    let src = "import no.such.Thing;\nclass C { }\n";
    assert!(hover_with(src, "Thing;").is_none());
}

/// The inherited-doc walk crosses into the external world — an
/// undocumented override of a JDK/dependency method asks the symbol
/// source for the supertype member's doc.
#[test]
fn hover_inherits_javadoc_from_external_supertype() {
    struct DocSource;
    impl SymbolSource for DocSource {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            (fqn == "java.lang.Runnable").then(|| ExternalClass {
                supers: Vec::new(),
                type_params: Vec::new(),
                members: vec![ExternalMember {
                    name: "run".to_string(),
                    kind: ExternalMemberKind::Method,
                    signature: "void run()".to_string(),
                    template: None,
                    is_static: false,
                    ret_fqn: None,
                    ret_display: None,
                }],
            })
        }
        fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
            (fqn == "java.lang.Runnable" && member == Some("run"))
                .then(|| "Runs the task.".to_string())
        }
    }
    let src = "class Task implements Runnable { public void run() {} }\n\
                   class C { void m() { Task t; t.run(); } }\n";
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let index = LineIndex::new(src, PositionEncoding::Utf16);
    let at = src.find("run();").expect("marker");
    let h = hover(&docs, 0, &index, index.position(at), &DocSource).expect("hover");
    let HoverContents::Markup(m) = h.contents else {
        panic!("markup expected");
    };
    assert!(m.value.contains("Runs the task."), "{}", m.value);
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

// --- Hover on the type name inside `new Foo(...)` ---

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

/// Hover with an arbitrary symbol source (external types), cursor one byte
/// into the first occurrence of `marker`.
fn hover_text_with(src: &str, marker: &str, symbols: &dyn SymbolSource) -> Option<String> {
    let tree = tree(src);
    let docs = [OpenDoc {
        source: src,
        tree: &tree,
    }];
    let index = LineIndex::new(src, PositionEncoding::Utf16);
    let at = src.find(marker).expect("marker present");
    let h = hover(&docs, 0, &index, index.position(at), symbols)?;
    match h.contents {
        HoverContents::Markup(m) => Some(m.value),
        _ => None,
    }
}

/// A `var` local infers its type from the initializer — hovering the
/// declaration name shows the inferred in-project type, not the literal
/// `var` keyword.
#[test]
fn hover_on_var_declaration_shows_inferred_in_project_type() {
    let src = "class Widget {}\n\
                   class C { void m() { var w = new Widget(); } }\n";
    let text = hover_text(src, "w = new").expect("hover");
    assert!(text.contains("Widget w"), "{text}");
    assert!(!text.contains("var w"), "raw `var` leaked: {text}");
}

/// A `var` local's inferred type also shows when hovering a later *use* of
/// the variable, not only its declaration.
#[test]
fn hover_on_var_usage_shows_inferred_in_project_type() {
    let src = "class Widget {}\n\
                   class C { void m() { var w = new Widget(); w.toString(); } }\n";
    let text = hover_text(src, "w.toString").expect("hover");
    assert!(text.contains("Widget w"), "{text}");
    assert!(!text.contains("var w"), "raw `var` leaked: {text}");
}

/// A `var` bound to a generic external type infers the parameterized type
/// (`ArrayList<String>`), not just the raw name.
#[test]
fn hover_on_var_infers_generic_external_type() {
    let src = "import java.util.ArrayList;\n\
                   class C { void m() { var list = new ArrayList<String>(); } }\n";
    let symbols = OneClass {
        fqn: "java.util.ArrayList",
        members: Vec::new(),
    };
    let text = hover_text_with(src, "list = new", &symbols).expect("hover");
    assert!(text.contains("ArrayList<String> list"), "{text}");
    assert!(!text.contains("var list"), "raw `var` leaked: {text}");
}

/// A `switch` type-pattern binding hovers as `Type name` — at its
/// declaration, in the guard, and in the case body.
#[test]
fn hover_on_switch_type_pattern_binding() {
    let src = "class Widget {}\n\
                   class C { Object m(Object f) { return switch (f) {\n\
                   case Widget w when w != null -> w;\n\
                   default -> null; }; } }\n";
    // declaration
    assert!(hover_text(src, "w when")
        .expect("hover")
        .contains("Widget w"));
    // guard use
    assert!(hover_text(src, "w != null")
        .expect("hover")
        .contains("Widget w"));
    // body use
    assert!(hover_text(src, "w;").expect("hover").contains("Widget w"));
}

/// An `instanceof` pattern binding hovers as `Type name` both where it's
/// bound and where it's used in the guarded branch.
#[test]
fn hover_on_instanceof_pattern_binding() {
    let src = "class Widget {}\n\
                   class C { void m(Object f) {\n\
                   if (f instanceof Widget w) { w.toString(); } } }\n";
    assert!(hover_text(src, "w)").expect("hover").contains("Widget w"));
    assert!(hover_text(src, "w.toString")
        .expect("hover")
        .contains("Widget w"));
}

/// An explicitly-typed local is unaffected — its declared type renders as
/// before (no inference override).
#[test]
fn hover_on_explicitly_typed_local_is_unchanged() {
    let src = "class Widget {}\n\
                   class C { void m() { Widget w = new Widget(); } }\n";
    let text = hover_text(src, "w = new").expect("hover");
    assert!(text.contains("Widget w"), "{text}");
}
