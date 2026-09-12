//! Cross-cutting robustness corpus: uncommon and edge-case Java sources run
//! through every public entry point. Two contracts are proven here — no entry
//! point may panic at any cursor position in any snippet, and no snippet may
//! produce a false-positive diagnostic. Snippets that the pinned
//! `tree-sitter-java` grammar cannot parse are deliberately included: a parse
//! gap must degrade to silence, never to a wrong diagnostic or a panic.

use ls_types::{Position, Range, Uri};
use tree_sitter::Tree;

use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, SymbolSource};
use crate::{new_parser, parse, LineIndex, OpenDoc, PositionEncoding};

/// One corpus entry: a stable name for failure messages plus a whole
/// compilation unit.
struct Snippet {
    name: &'static str,
    source: &'static str,
}

/// Resolves `java.lang.Object` and `java.lang.String` with complete
/// hierarchies, so member/invocation checks are actually *enabled* while the
/// corpus runs (with `NoSymbols` almost every check would be trivially
/// silent and the corpus would prove nothing).
struct CorpusSymbols;

impl CorpusSymbols {
    fn make(fqn: &str, supers: &[&str], members: &[&str]) -> ExternalClass {
        ExternalClass {
            supers: supers.iter().map(|s| s.to_string()).collect(),
            type_params: Vec::new(),
            members: members
                .iter()
                .map(|name| ExternalMember {
                    name: (*name).to_string(),
                    kind: ExternalMemberKind::Method,
                    signature: format!("{name}()"),
                    template: None,
                    is_static: false,
                    ret_fqn: None,
                    ret_display: None,
                    metadata: None,
                })
                .collect(),
            metadata: Some(jvl_types::ClassMetadata {
                id: jvl_types::TypeId::named(fqn),
                kind: jvl_types::ClassKind::Class,
                access: jvl_types::Access::Public,
                is_abstract: false,
                is_static: true,
                enclosing_class: None,
                type_parameters: Vec::new(),
                supertypes: supers
                    .iter()
                    .map(|s| jvl_types::TypeRef::named(s))
                    .collect(),
                hierarchy_complete: true,
                constructors_complete: true,
            }),
        }
    }
}

impl SymbolSource for CorpusSymbols {
    fn class(&self, fqn: &str) -> Option<ExternalClass> {
        match fqn {
            "java.lang.Object" => Some(Self::make(
                fqn,
                &[],
                &["toString", "equals", "hashCode", "getClass"],
            )),
            "java.lang.String" => Some(Self::make(
                fqn,
                &["java.lang.Object"],
                &["length", "isEmpty", "trim", "toUpperCase"],
            )),
            _ => None,
        }
    }
}

fn tree_of(source: &str) -> Tree {
    parse(&mut new_parser(), source, None).expect("tree-sitter always returns a tree")
}

/// Byte offsets to probe: every UTF-8 character boundary for ordinary
/// snippets, every 16th boundary for the oversized stress snippets, so the
/// whole sweep stays well under a minute in a debug build.
fn probe_offsets(source: &str) -> Vec<usize> {
    let stride = if source.len() > 600 { 16 } else { 1 };
    source
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(source.len()))
        .enumerate()
        .filter(|(n, _)| n % stride == 0)
        .map(|(_, byte)| byte)
        .collect()
}

/// Every position-taking entry point at one cursor position. Returning
/// `()` is the point: the assertion is "this call returns".
fn exercise_at(docs: &[OpenDoc], index: &LineIndex, position: Position) {
    let symbols = CorpusSymbols;
    let _ = crate::hover(docs, 0, index, position, &symbols);
    let _ = crate::definition(docs, 0, index, position, &symbols);
    let _ = crate::type_definition(docs, 0, index, position, &symbols);
    let _ = crate::completion(docs, 0, index, position, true, &symbols);
    let _ = crate::signature_help(docs, 0, index, position, &symbols);
    let _ = crate::prepare_rename(docs, 0, index, position, &symbols);
    let _ = crate::code_actions(
        docs,
        0,
        index,
        Range {
            start: position,
            end: position,
        },
        &symbols,
    );
    let _ = crate::selection_ranges(docs[0].tree, index, &[position]);
    let _ = crate::implementation_target(docs, 0, index, position, &symbols);
    if let Some(target) = crate::reference_target(docs, 0, index, position, &symbols) {
        let _ = crate::references_in_doc(docs, 0, &target, true, &symbols);
    }
}

/// Every whole-document entry point.
fn exercise_document(source: &str, tree: &Tree, index: &LineIndex) {
    let _ = crate::syntax_diagnostics(tree, index);
    let _ = crate::document_symbols(tree, source, index);
    let _ = crate::folding_ranges(tree);
    let _ = crate::semantic_tokens(tree, source, index);
    let _ = crate::discover_tests(tree, source, index);
    let _ = crate::structural_diagnostics(tree, source, index, Some("Corpus.java"), None);
}

#[test]
fn corpus_entry_points_never_panic_at_any_position() {
    for snippet in CORPUS {
        let tree = tree_of(snippet.source);
        let docs = [OpenDoc {
            source: snippet.source,
            tree: &tree,
        }];
        let index = LineIndex::new(snippet.source, PositionEncoding::Utf16);
        exercise_document(snippet.source, &tree, &index);
        for byte in probe_offsets(snippet.source) {
            exercise_at(&docs, &index, index.position(byte));
        }
    }
}

/// Mid-edit robustness: every prefix of every snippet is a source state the
/// editor really sends on `didChange`, and every entry point must survive it.
#[test]
fn corpus_truncations_never_panic() {
    for snippet in CORPUS {
        let mut cut = 0usize;
        while cut <= snippet.source.len() {
            let boundary = (0..=cut)
                .rev()
                .find(|b| snippet.source.is_char_boundary(*b))
                .unwrap_or(0);
            let prefix = &snippet.source[..boundary];
            let tree = tree_of(prefix);
            let docs = [OpenDoc {
                source: prefix,
                tree: &tree,
            }];
            let index = LineIndex::new(prefix, PositionEncoding::Utf16);
            exercise_document(prefix, &tree, &index);
            exercise_at(&docs, &index, index.position(prefix.len()));
            cut += 8;
        }
    }
}

/// The load-bearing conservatism contract: none of these snippets contains a
/// real error, so every semantic and structural check must stay silent —
/// including on the snippets the pinned grammar cannot parse.
#[test]
fn corpus_snippets_produce_no_false_positive_diagnostics() {
    let uri: Uri = "file:///Corpus.java".parse().expect("test uri");
    for snippet in CORPUS {
        let tree = tree_of(snippet.source);
        let docs = [OpenDoc {
            source: snippet.source,
            tree: &tree,
        }];
        let index = LineIndex::new(snippet.source, PositionEncoding::Utf16);
        let semantic =
            crate::semantic_diagnostics(&docs, 0, &index, &uri, &CorpusSymbols, true, false);
        assert!(
            semantic.is_empty(),
            "{}: false-positive semantic diagnostics: {:#?}",
            snippet.name,
            semantic.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let structural = crate::structural_diagnostics(&tree, snippet.source, &index, None, None);
        assert!(
            structural.is_empty(),
            "{}: false-positive structural diagnostics: {:#?}",
            snippet.name,
            structural.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}

/// Unused-code mode is the one rule that legitimately fires on toy sources,
/// so it gets its own weaker contract: it must not panic and must not emit
/// anything that isn't the unused warning code.
#[test]
fn corpus_unused_mode_emits_only_unused_warnings() {
    let uri: Uri = "file:///Corpus.java".parse().expect("test uri");
    for snippet in CORPUS {
        let tree = tree_of(snippet.source);
        let docs = [OpenDoc {
            source: snippet.source,
            tree: &tree,
        }];
        let index = LineIndex::new(snippet.source, PositionEncoding::Utf16);
        for diagnostic in
            crate::semantic_diagnostics(&docs, 0, &index, &uri, &CorpusSymbols, false, true)
        {
            let code = match &diagnostic.code {
                Some(ls_types::NumberOrString::String(code)) => code.clone(),
                other => panic!("{}: unexpected code {other:?}", snippet.name),
            };
            assert_eq!(
                code, "jvl.unused",
                "{}: non-unused diagnostic in unused mode: {}",
                snippet.name, diagnostic.message
            );
        }
    }
}

/// Position mapping must round-trip for astral-plane text in both encodings;
/// a UTF-16 surrogate pair counts as two units and one character.
#[test]
fn line_index_round_trips_every_char_boundary() {
    for snippet in CORPUS {
        for encoding in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
            let index = LineIndex::new(snippet.source, encoding);
            for (byte, _) in snippet.source.char_indices() {
                let position = index.position(byte);
                assert_eq!(
                    index.offset(position),
                    byte,
                    "{}: {encoding:?} round trip failed at byte {byte}",
                    snippet.name
                );
            }
        }
    }
}

/// `PositionEncoding::Utf8` counts bytes and `Utf16` counts code units, so a
/// 4-byte / 2-unit astral character makes the same byte offset land on two
/// different columns — the conversion every LSP client depends on.
#[test]
fn astral_plane_columns_differ_between_encodings() {
    let source = "class C { String s = \"\u{1F680}\"; int after = 1; }\n";
    let byte = source.find("int after").expect("marker present");
    let utf8 = LineIndex::new(source, PositionEncoding::Utf8).position(byte);
    let utf16 = LineIndex::new(source, PositionEncoding::Utf16).position(byte);
    assert_eq!(utf8.line, 0);
    assert_eq!(utf16.line, 0);
    assert_eq!(
        utf8.character,
        utf16.character + 2,
        "the rocket is 4 UTF-8 bytes but 2 UTF-16 units, so the byte column \
         must sit exactly 2 ahead of the UTF-16 column"
    );
    assert_eq!(
        LineIndex::new(source, PositionEncoding::Utf16).offset(utf16),
        byte,
        "the UTF-16 column must map back to the same byte"
    );
}

/// Uncommon but valid Java, plus the constructs the pinned grammar is known
/// to mis-parse (marked `gap:`). Every entry must stay diagnostic-free.
const CORPUS: &[Snippet] = &[
    Snippet {
        name: "unicode-escape-identifier",
        source: "class C { void m() { int \\u0061 = 5; System.out.println(\\u0061); } }\n",
    },
    Snippet {
        name: "gap: unicode-escape-in-string",
        source: "class C { String s = \"\\uuu0041\"; }\n",
    },
    Snippet {
        name: "gap: escaped-quote-via-unicode",
        source: "class C { char c = '\\u005c''; }\n",
    },
    Snippet {
        name: "text-block",
        source: r#"class C {
    String t = """
        hello "world"
        line \s trailing \
        continued
        """;
}
"#,
    },
    Snippet {
        name: "nested-generics-shift-token",
        source: "import java.util.*;\nclass C { Map<String, List<Map<Integer, String>>> m; }\n",
    },
    Snippet {
        name: "explicit-type-arguments",
        source: "import java.util.*;\nclass C { List<String> m() { return Collections.<String>emptyList(); } }\n",
    },
    Snippet {
        name: "shift-versus-generics-expression",
        source: "class C { boolean m(int a, int b, int c) { return a < b >> c; } }\n",
    },
    Snippet {
        name: "intersection-cast-lambda",
        source: "import java.io.Serializable;\nclass C { Runnable r = (Runnable & Serializable) () -> {}; }\n",
    },
    Snippet {
        name: "method-references",
        source: "import java.util.function.*;\nclass C {\n    Function<Integer, String> a = String::valueOf;\n    IntFunction<int[]> b = int[]::new;\n    Supplier<String> c = this::name;\n    String name() { return \"n\"; }\n}\n",
    },
    Snippet {
        name: "labeled-break-and-continue",
        source: "class C { void m() { outer: for (int i = 0; i < 3; i++) { inner: for (int j = 0; j < 3; j++) { if (j == 1) continue outer; if (i == 2) break outer; } } } }\n",
    },
    Snippet {
        name: "switch-expression-yield",
        source: "class C {\n    int m(int day) {\n        return switch (day) {\n            case 1, 2, 3 -> 1;\n            case 4: yield 2;\n            default -> { yield 3; }\n        };\n    }\n}\n",
    },
    Snippet {
        name: "switch-pattern-with-guard",
        source: "class C {\n    String m(Object o) {\n        return switch (o) {\n            case String s when s.length() > 2 -> s;\n            case Integer i -> i.toString();\n            default -> \"other\";\n        };\n    }\n}\n",
    },
    Snippet {
        name: "gap: switch-pattern-final-modifier",
        source: "class C {\n    String m(Object o) {\n        return switch (o) {\n            case final String s -> s;\n            default -> \"other\";\n        };\n    }\n}\n",
    },
    Snippet {
        name: "record-pattern-nested",
        source: "class C {\n    record Point(int x, int y) {}\n    record Line(Point a, Point b) {}\n    int m(Object o) {\n        if (o instanceof Line(Point(int x, int y), Point b)) { return x + y; }\n        return 0;\n    }\n}\n",
    },
    Snippet {
        name: "gap: qualified-record-pattern",
        source: "class Outer {\n    record Created(String id) {}\n    String m(Object o) {\n        if (o instanceof Outer.Created(String id)) { return id; }\n        return \"\";\n    }\n}\n",
    },
    Snippet {
        name: "sealed-hierarchy",
        source: "sealed interface Shape permits Circle, Square {}\nfinal class Circle implements Shape {}\nnon-sealed class Square implements Shape {}\n",
    },
    Snippet {
        name: "record-compact-constructor",
        source: "record Range<T extends Comparable<T>>(T low, T high) {\n    static final String LABEL = \"range\";\n    Range {\n        if (low.compareTo(high) > 0) { throw new IllegalArgumentException(LABEL); }\n    }\n    T mid() { return low; }\n}\n",
    },
    Snippet {
        name: "enum-with-constant-bodies",
        source: "enum Op {\n    ADD { int apply(int a, int b) { return a + b; } },\n    SUB { int apply(int a, int b) { return a - b; } };\n    abstract int apply(int a, int b);\n}\n",
    },
    Snippet {
        name: "annotation-declaration",
        source: "@interface Marker {\n    String value() default \"\";\n    int[] codes() default { 1, 2 };\n    Class<?> type() default Object.class;\n}\n",
    },
    Snippet {
        name: "type-use-annotations",
        source: "import java.lang.annotation.*;\nimport java.util.*;\n@Target(ElementType.TYPE_USE) @interface NonNull {}\nclass C {\n    List<@NonNull String> items;\n    int @NonNull [] numbers;\n    void m(C this, @NonNull String s) {}\n}\n",
    },
    Snippet {
        name: "gap: annotated-varargs",
        source: "import java.lang.annotation.*;\n@Target(ElementType.TYPE_USE) @interface A {}\nclass C { void g(int @A [] @A ... xs) {} }\n",
    },
    Snippet {
        name: "gap: annotated-union-catch",
        source: "import java.lang.annotation.*;\n@Target(ElementType.TYPE_USE) @interface A {}\nclass C {\n    void m() {\n        try { work(); } catch (IllegalStateException | @A IllegalArgumentException e) { }\n    }\n    void work() {}\n}\n",
    },
    Snippet {
        name: "union-catch-and-try-with-resources",
        source: "import java.io.*;\nclass C {\n    void m(InputStream given) {\n        try (given; ByteArrayInputStream b = new ByteArrayInputStream(new byte[0])) {\n            b.read();\n        } catch (IOException | RuntimeException e) {\n        } finally {\n        }\n    }\n}\n",
    },
    Snippet {
        name: "anonymous-class-diamond",
        source: "import java.util.*;\nclass C { Comparator<String> c = new Comparator<>() { public int compare(String a, String b) { return 0; } }; }\n",
    },
    Snippet {
        name: "local-declarations",
        source: "class C {\n    void m() {\n        record Pair(int a, int b) {}\n        interface Op { int apply(); }\n        class Impl implements Op { public int apply() { return new Pair(1, 2).a(); } }\n        enum Tiny { ONE }\n        System.out.println(new Impl().apply() + Tiny.ONE.name());\n    }\n}\n",
    },
    Snippet {
        name: "initializer-blocks",
        source: "class C {\n    static int counter;\n    int instance;\n    static { counter = 1; }\n    { instance = 2; }\n}\n",
    },
    Snippet {
        name: "var-lambda-parameters",
        source: "import java.util.function.*;\nclass C { BiFunction<String, String, String> f = (var a, var b) -> a + b; }\n",
    },
    Snippet {
        name: "generic-bounds",
        source: "class C {\n    <T extends Comparable<? super T> & Cloneable> T pick(T a, T b) { return a.compareTo(b) >= 0 ? a : b; }\n}\n",
    },
    Snippet {
        name: "array-forms",
        source: "class C {\n    int[][] grid = new int[2][3];\n    int legacy[] = { 1, 2, 3 };\n    String[][] names = { { \"a\" }, { \"b\" } };\n    void m() { for (int[] row : grid) { for (int cell : row) { System.out.println(cell); } } }\n}\n",
    },
    Snippet {
        name: "numeric-literals",
        source: "class C {\n    int hex = 0xCAFE_BABE;\n    long big = 1_000_000L;\n    int bin = 0b1010_1010;\n    double hexFloat = 0x1.8p3;\n    float f = 1.5e-3f;\n    int octal = 0777;\n}\n",
    },
    Snippet {
        name: "gap: multiple-underscore-literal",
        source: "class C { int a = 1____1234; }\n",
    },
    Snippet {
        name: "char-escapes",
        source: "class C {\n    char quote = '\\'';\n    char slash = '\\\\';\n    char newline = '\\n';\n    char high = '\\uFFFF';\n    char octalEscape = '\\101';\n}\n",
    },
    Snippet {
        name: "astral-plane-text",
        source: "class C {\n    String rocket = \"\u{1F680} launch\";\n    // comment with \u{1F680} and \u{2603}\n    void m() { System.out.println(rocket.length()); }\n}\n",
    },
    Snippet {
        name: "non-ascii-identifiers",
        source: "class C {\n    int f\u{00FC}geKnoten = 1;\n    int \u{53D8}\u{91CF} = 2;\n    int sum() { return f\u{00FC}geKnoten + \u{53D8}\u{91CF}; }\n}\n",
    },
    Snippet {
        name: "module-info",
        source: "module demo.core {\n    requires java.base;\n    requires transitive java.logging;\n    exports demo.api;\n    opens demo.internal to demo.tests;\n    uses demo.api.Service;\n    provides demo.api.Service with demo.internal.Impl;\n}\n",
    },
    Snippet {
        name: "package-info",
        source: "@Deprecated\npackage demo.legacy;\n",
    },
    Snippet {
        name: "interface-method-flavors",
        source: "interface Service {\n    int base();\n    default int doubled() { return helper() * 2; }\n    static Service none() { return () -> 0; }\n    private int helper() { return base(); }\n}\n",
    },
    Snippet {
        name: "inner-class-instantiation",
        source: "class Outer {\n    class Inner { int v = 1; }\n    static class Nested<T> { T value; }\n    int m() {\n        Outer outer = new Outer();\n        Outer.Inner inner = outer.new Inner();\n        Outer.Nested<String> nested = new Outer.Nested<>();\n        nested.value = \"x\";\n        return inner.v;\n    }\n}\n",
    },
    Snippet {
        name: "control-flow-mix",
        source: "class C {\n    void m(int n) {\n        assert n >= 0 : \"negative\";\n        int i = 0;\n        do { i++; } while (i < n);\n        for (int a = 0, b = n; a < b; a++, b--) { ; }\n        synchronized (this) { }\n        ;;\n    }\n}\n",
    },
    Snippet {
        name: "instanceof-forms",
        source: "import java.util.*;\nclass C {\n    boolean m(Object o) {\n        if (o instanceof List<?> list && !list.isEmpty()) { return true; }\n        return o instanceof final String s && s.isEmpty();\n    }\n}\n",
    },
    Snippet {
        name: "javadoc-shapes",
        source: "class C {\n    /**\n     * Uses {@code a/*b} and <b>html</b>.\n     *\n     * @param value the value, never {@code null}\n     * @return the value\n     */\n    String echo(String value) { return value; }\n}\n",
    },
    Snippet {
        name: "crlf-line-endings",
        source: "class C {\r\n    int value = 1;\r\n    int get() { return value; }\r\n}\r\n",
    },
    Snippet {
        name: "deeply-nested-blocks",
        source: "class C { void m() { { { { { { { { { { { { { { { { { { { { int x = 1; System.out.println(x); } } } } } } } } } } } } } } } } } } } }\n",
    },
    Snippet {
        name: "deeply-nested-parentheses",
        source: "class C { int m() { return ((((((((((((((((((((1)))))))))))))))))))); } }\n",
    },
    Snippet {
        name: "long-single-line",
        source: "class C { int m() { return 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1; } }\n",
    },
    Snippet {
        name: "empty-file",
        source: "",
    },
    Snippet {
        name: "only-comment",
        source: "// nothing here\n/* not here either */\n",
    },
];
