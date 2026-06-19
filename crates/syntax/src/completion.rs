//! LSP completion: member completion after `.` and in-scope identifier/keyword
//! completion while typing. Both run against open documents only.

use std::collections::HashSet;

use ls_types::{
    CompletionItem, CompletionItemKind, Documentation, InsertTextFormat, MarkupContent, MarkupKind,
    Position,
};
use tree_sitter::Node;

use crate::model::{named_children, Member, MemberKind, TypeDecl, TypeKind, TypeTable};
use crate::resolve::{self, Binding, BindingKind, Resolved};
use crate::signature::{javadoc, signature};
use crate::{LineIndex, OpenDoc};

/// Java reserved words + literals offered in scope completion.
const KEYWORDS: &[&str] = &[
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "continue",
    "default", "do", "double", "else", "enum", "extends", "final", "finally", "float", "for", "if",
    "implements", "import", "instanceof", "int", "interface", "long", "native", "new", "package",
    "private", "protected", "public", "return", "short", "static", "strictfp", "super", "switch",
    "synchronized", "this", "throw", "throws", "transient", "try", "void", "volatile", "while",
    "var", "yield", "record", "sealed", "permits", "true", "false", "null",
];

/// Produce completion items for the cursor position. After a resolvable `.` this
/// is member completion; otherwise in-scope identifiers + keywords. `snippets`
/// reflects the client's `completionItem.snippetSupport` capability.
pub fn completion(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    snippets: bool,
) -> Vec<CompletionItem> {
    let Some(doc) = docs.get(current) else {
        return Vec::new();
    };
    let cursor = index.offset(pos);
    let table = TypeTable::build(docs, current);

    if let Some(recv) = resolve::member_receiver(doc.tree, doc.source, cursor) {
        // In a member-access position: only members, never scope fallback, so a
        // typed `.` never yields wrong global suggestions.
        return match resolve::resolve_receiver_type(recv, doc, &table) {
            Some(resolved) => member_items(&resolved, &table, snippets),
            None => Vec::new(),
        };
    }

    scope_items(doc, cursor, &table, snippets)
}

fn member_items<'t>(
    resolved: &Resolved<'t>,
    table: &TypeTable<'t>,
    snippets: bool,
) -> Vec<CompletionItem> {
    table
        .all_members(&resolved.decl, resolved.static_only)
        .iter()
        .map(|m| member_item(m, snippets))
        .collect()
}

fn member_item(member: &Member, snippets: bool) -> CompletionItem {
    let detail = signature(member.node, member.source);
    let doc = javadoc(member.node, member.source);
    let kind = member_kind(member.kind);
    let mut item = CompletionItem {
        label: member.name.to_string(),
        kind: Some(kind),
        detail,
        documentation: doc.map(markdown),
        ..Default::default()
    };
    if member.kind == MemberKind::Method {
        apply_method_insert(&mut item, member.node, snippets);
    }
    item
}

/// Decide a method's insert text. With snippet support: `name()` (zero-arg) or a
/// `name($1)` tab-stop snippet. Without it: `name()` or `name(` (the editor
/// leaves the cursor after the paren). `$` is escaped because it is legal in Java
/// identifiers and is snippet-special.
fn apply_method_insert(item: &mut CompletionItem, method: Node, snippets: bool) {
    let has_params = method
        .child_by_field_name("parameters")
        .map(|p| {
            named_children(p)
                .iter()
                .any(|c| matches!(c.kind(), "formal_parameter" | "spread_parameter"))
        })
        .unwrap_or(false);
    if !has_params {
        item.insert_text = Some(format!("{}()", item.label));
    } else if snippets {
        let label = item.label.replace('$', "\\$");
        item.insert_text = Some(format!("{label}($1)"));
        item.insert_text_format = Some(InsertTextFormat::SNIPPET);
    } else {
        item.insert_text = Some(format!("{}(", item.label));
    }
}

fn scope_items<'t>(
    doc: &OpenDoc<'t>,
    cursor: usize,
    table: &TypeTable<'t>,
    snippets: bool,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |item: CompletionItem, items: &mut Vec<CompletionItem>| {
        let key = format!("{}\u{0}{:?}", item.label, item.kind);
        if seen.insert(key) {
            items.push(item);
        }
    };

    // Locals, params, for-vars. Fields come from the single all_members() pass
    // below, so they are excluded here to avoid recomputing the member set.
    for binding in resolve::collect_bindings(doc.tree, doc.source, cursor, table, false) {
        push(binding_item(&binding), &mut items);
    }

    // The enclosing type's members (fields + methods + nested types), own and
    // inherited, callable unqualified — one all_members() pass.
    let node = resolve::node_at(doc.tree, cursor);
    if let Some(td) = resolve::enclosing_typedecl(node, doc.source) {
        for member in table.all_members(&td, false) {
            push(member_item(&member, snippets), &mut items);
        }
    }

    // In-scope type names (current + open files).
    for decl in table.iter() {
        push(type_item(decl), &mut items);
    }

    // Keywords.
    for kw in KEYWORDS {
        push(
            CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            },
            &mut items,
        );
    }

    items
}

fn binding_item(binding: &Binding) -> CompletionItem {
    let kind = match binding.kind {
        BindingKind::Field => CompletionItemKind::FIELD,
        _ => CompletionItemKind::VARIABLE,
    };
    CompletionItem {
        label: binding.name.to_string(),
        kind: Some(kind),
        detail: signature(binding.decl_node, binding.source),
        ..Default::default()
    }
}

fn type_item(decl: &TypeDecl) -> CompletionItem {
    CompletionItem {
        label: decl.name.to_string(),
        kind: Some(type_kind(decl.kind)),
        detail: Some(format!("{} {}", decl.kind.keyword(), decl.name)),
        ..Default::default()
    }
}

fn member_kind(kind: MemberKind) -> CompletionItemKind {
    match kind {
        MemberKind::Method => CompletionItemKind::METHOD,
        MemberKind::Field => CompletionItemKind::FIELD,
        MemberKind::EnumConstant => CompletionItemKind::ENUM_MEMBER,
        MemberKind::NestedType(tk) => type_kind(tk),
    }
}

fn type_kind(kind: TypeKind) -> CompletionItemKind {
    match kind {
        TypeKind::Class | TypeKind::Record => CompletionItemKind::CLASS,
        TypeKind::Interface | TypeKind::Annotation => CompletionItemKind::INTERFACE,
        TypeKind::Enum => CompletionItemKind::ENUM,
    }
}

fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

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
        completion(&docs, 0, &index, index.position(at), true)
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
        assert!(has(&items, "baseField"), "inherited field {:?}", labels(&items));
        assert!(has(&items, "baseM"), "inherited method {:?}", labels(&items));
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
        assert!(has(&items, "bOnly"), "inner type wins: {:?}", labels(&items));
        assert!(!has(&items, "aOnly"), "outer field shadowed: {:?}", labels(&items));
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
        assert!(has(&items, "before"), "before-cursor local: {:?}", labels(&items));
        assert!(!has(&items, "after"), "after-cursor local: {:?}", labels(&items));
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
        let items = completion(&docs, 0, &index, index.position(at), true);
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
        completion(&docs, 0, &index, index.position(at), snippets)
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
        assert!(!has(&items, "inst"), "instance member excluded: {:?}", labels(&items));
    }

    #[test]
    fn interface_constants_are_inherited_members() {
        let src = "interface Sized { int MAX = 10; int size(); }\n\
                   class C implements Sized { void m() { this.x; } }\n";
        let items = complete(src, "this.");
        assert!(has(&items, "MAX"), "interface constant: {:?}", labels(&items));
        assert!(has(&items, "size"), "interface method: {:?}", labels(&items));
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
        let _ = completion(&docs, 0, &index, index.position(at), true);
    }
}
