//! Refactoring code actions — extract variable/constant from a selected
//! expression, and source-generate actions (getters/setters, constructor,
//! `equals`/`hashCode`, `toString`) for the class under the cursor.
//!
//! Everything here is a pure text-edit producer ([`ActionSketch`]), same
//! contract as `codeaction.rs`: the server owns URIs and the LSP envelope.
//! Conservative throughout — an action is only offered when the edit is
//! unambiguous; anything uncertain produces no action rather than a wrong
//! one.

use ls_types::{Range, TextEdit};
use tree_sitter::Node;

use crate::codeaction::ActionSketch;
use crate::model::{named_children, MemberKind, TypeDecl, TypeKind};
use crate::{node_text, LineIndex, OpenDoc};

pub(crate) const KIND_REFACTOR_EXTRACT: &str = "refactor.extract";
pub(crate) const KIND_SOURCE_GENERATE: &str = "source.generate";

/// All refactor actions for `range`: extract actions when the selection is exactly
/// an expression, generate actions when the cursor sits inside a class.
pub(crate) fn refactor_actions(
    doc: &OpenDoc,
    index: &LineIndex,
    range: Range,
) -> Vec<ActionSketch> {
    let mut out = Vec::new();
    let start = index.offset(range.start);
    let end = index.offset(range.end);
    if let Some(action) = extract_variable(doc, index, start, end) {
        out.push(action);
    }
    if let Some(action) = extract_constant(doc, index, start, end) {
        out.push(action);
    }
    out.extend(generate_actions(doc, index, start));
    out
}

// --- Extract variable / constant ---

/// Expression node kinds worth extracting into a local. Deliberately omits
/// bare identifiers (extracting `x` into `var y = x` helps no one) and
/// lambdas/switches (their extraction has context rules this pass doesn't
/// model).
const EXTRACTABLE: &[&str] = &[
    "binary_expression",
    "method_invocation",
    "object_creation_expression",
    "field_access",
    "array_access",
    "cast_expression",
    "ternary_expression",
    "unary_expression",
    "instanceof_expression",
    "string_literal",
    "decimal_integer_literal",
    "hex_integer_literal",
    "octal_integer_literal",
    "binary_integer_literal",
    "decimal_floating_point_literal",
    "character_literal",
];

/// Literal kinds extract-constant accepts, with the constant's declared type.
fn literal_type(kind: &str, text: &str) -> Option<&'static str> {
    match kind {
        "string_literal" => Some("String"),
        "character_literal" => Some("char"),
        "decimal_floating_point_literal" => {
            if text.ends_with('f') || text.ends_with('F') {
                Some("float")
            } else {
                Some("double")
            }
        }
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => {
            if text.ends_with('l') || text.ends_with('L') {
                Some("long")
            } else {
                Some("int")
            }
        }
        "true" | "false" => Some("boolean"),
        _ => None,
    }
}

/// The named node that covers *exactly* the (whitespace-trimmed) selection,
/// or `None` — extraction never guesses at a partial expression.
fn selected_node<'t>(doc: &OpenDoc<'t>, start: usize, end: usize) -> Option<Node<'t>> {
    if start >= end {
        return None;
    }
    let text = doc.source.get(start..end)?;
    let lead = text.len() - text.trim_start().len();
    let trail = text.len() - text.trim_end().len();
    let (start, end) = (start + lead, end - trail);
    if start >= end {
        return None;
    }
    doc.tree
        .root_node()
        .named_descendant_for_byte_range(start, end - 1)
        .filter(|n| n.start_byte() == start && n.end_byte() == end)
}

/// The statement containing `node` — the ancestor whose parent is a
/// method/constructor body block — plus that statement's line indentation.
/// `None` when the statement doesn't start its own line (extraction would
/// mangle `if (x) stmt;` one-liners) or the node isn't inside a body.
fn enclosing_statement<'t>(doc: &OpenDoc<'t>, node: Node<'t>) -> Option<(Node<'t>, String)> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(parent.kind(), "block" | "constructor_body") {
            let indent = line_indent(doc.source, current.start_byte())?;
            return Some((current, indent));
        }
        current = parent;
    }
    None
}

/// The pure-whitespace prefix of `at`'s line, or `None` if anything else
/// precedes `at` on that line.
fn line_indent(source: &str, at: usize) -> Option<String> {
    let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
    let prefix = &source[line_start..at];
    prefix
        .chars()
        .all(|c| c == ' ' || c == '\t')
        .then(|| prefix.to_string())
}

/// A name for the extracted local: method name with a `get`/`is` prefix
/// peeled (`getName()` → `name`), a lowercased type name for `new Foo()`,
/// else `value` — made unique against the enclosing method's text.
fn variable_name(node: Node, source: &str) -> String {
    let base = match node.kind() {
        "method_invocation" => node
            .child_by_field_name("name")
            .map(|n| node_text(n, source))
            .map(|n| {
                let peeled = n
                    .strip_prefix("get")
                    .or_else(|| n.strip_prefix("is"))
                    .filter(|r| r.starts_with(|c: char| c.is_ascii_uppercase()))
                    .unwrap_or(n);
                decapitalized(peeled)
            })
            .unwrap_or_else(|| "value".to_string()),
        "object_creation_expression" => node
            .child_by_field_name("type")
            .and_then(|t| crate::model::base_type_name(t, source))
            .map(decapitalized)
            .unwrap_or_else(|| "value".to_string()),
        _ => "value".to_string(),
    };
    if base.is_empty() || crate::completion::KEYWORDS.contains(&base.as_str()) {
        return "value".to_string();
    }
    base
}

fn decapitalized(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Suffix `base` with the smallest counter that makes it not occur as a word
/// in `scope_text` (`name`, `name2`, `name3`, …).
fn unique_in(base: String, scope_text: &str) -> String {
    if !contains_word(scope_text, &base) {
        return base;
    }
    for n in 2..100 {
        let candidate = format!("{base}{n}");
        if !contains_word(scope_text, &candidate) {
            return candidate;
        }
    }
    base
}

fn contains_word(hay: &str, word: &str) -> bool {
    crate::codeaction::contains_word(hay, word)
}

fn extract_variable(
    doc: &OpenDoc,
    index: &LineIndex,
    start: usize,
    end: usize,
) -> Option<ActionSketch> {
    let node = selected_node(doc, start, end)?;
    if !EXTRACTABLE.contains(&node.kind()) {
        return None;
    }
    let (statement, indent) = enclosing_statement(doc, node)?;
    // The selection must sit inside the statement (not *be* it via some
    // wrapper) and inside a method-ish body — both guaranteed by
    // `enclosing_statement` walking from the node itself.
    let scope = enclosing_body_text(doc, node)?;
    let name = unique_in(variable_name(node, doc.source), scope);

    let expr_text = node_text(node, doc.source);
    let decl_at = index.position(statement.start_byte());
    let edits = vec![
        TextEdit {
            range: Range {
                start: decl_at,
                end: decl_at,
            },
            new_text: format!("var {name} = {expr_text};\n{indent}"),
        },
        TextEdit {
            range: Range {
                start: index.position(node.start_byte()),
                end: index.position(node.end_byte()),
            },
            new_text: name.clone(),
        },
    ];
    Some(ActionSketch {
        title: format!("Extract to local variable '{name}'"),
        kind: KIND_REFACTOR_EXTRACT,
        edits,
        is_preferred: false,
    })
}

/// The text of the method/constructor body enclosing `node`, used as the
/// (deliberately over-wide) scope for name-collision checks.
fn enclosing_body_text<'t>(doc: &OpenDoc<'t>, node: Node<'t>) -> Option<&'t str> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            return Some(node_text(parent, doc.source));
        }
        current = parent;
    }
    None
}

/// A constant name derived from a string literal's content
/// (`"not-found"` → `NOT_FOUND`) or a generic `CONSTANT` for other literals.
fn constant_name(node: Node, source: &str) -> String {
    if node.kind() == "string_literal" {
        let text = node_text(node, source).trim_matches('"').to_string();
        let mut name = String::new();
        let mut last_us = true; // suppress a leading underscore
        for c in text.chars().take(24) {
            if c.is_ascii_alphanumeric() {
                name.push(c.to_ascii_uppercase());
                last_us = false;
            } else if !last_us {
                name.push('_');
                last_us = true;
            }
        }
        let name = name.trim_end_matches('_').to_string();
        if !name.is_empty() && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            return name;
        }
    }
    "CONSTANT".to_string()
}

fn extract_constant(
    doc: &OpenDoc,
    index: &LineIndex,
    start: usize,
    end: usize,
) -> Option<ActionSketch> {
    let node = selected_node(doc, start, end)?;
    let lit_text = node_text(node, doc.source);
    let ty = literal_type(node.kind(), lit_text)?;
    let class = enclosing_class(node)?;
    let td = TypeDecl::from_node(class, doc.source, 0)?;
    let (insert_at, member_indent, wrap) = member_insertion_at_top(doc, &td)?;
    let name = unique_in(
        constant_name(node, doc.source),
        node_text(class, doc.source),
    );

    let decl = format!(
        "{}{member_indent}private static final {ty} {name} = {lit_text};{}",
        wrap.0, wrap.1
    );
    let at = index.position(insert_at);
    let edits = vec![
        TextEdit {
            range: Range { start: at, end: at },
            new_text: decl,
        },
        TextEdit {
            range: Range {
                start: index.position(node.start_byte()),
                end: index.position(node.end_byte()),
            },
            new_text: name.clone(),
        },
    ];
    Some(ActionSketch {
        title: format!("Extract to constant '{name}'"),
        kind: KIND_REFACTOR_EXTRACT,
        edits,
        is_preferred: false,
    })
}

// --- Generate actions ---

/// The innermost class declaration containing `node`.
fn enclosing_class(node: Node) -> Option<Node> {
    let mut current = node;
    loop {
        if current.kind() == "class_declaration" {
            return Some(current);
        }
        current = current.parent()?;
    }
}

/// One instance field of the class, as generation sees it.
struct GenField<'t> {
    name: &'t str,
    type_text: &'t str,
}

fn gen_fields<'t>(td: &TypeDecl<'t>) -> Vec<GenField<'t>> {
    td.own_members()
        .into_iter()
        .filter(|m| {
            matches!(m.kind, MemberKind::Field)
                && !m.is_static
                && m.node.kind() == "variable_declarator"
        })
        .filter_map(|m| {
            let ty = m.node.parent()?.child_by_field_name("type")?;
            Some(GenField {
                name: m.name,
                type_text: node_text(ty, td.source),
            })
        })
        .collect()
}

const PRIMITIVES: &[&str] = &[
    "int", "long", "short", "byte", "char", "boolean", "float", "double",
];

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Getter name matching the Lombok/JavaBeans convention used elsewhere.
fn getter_name(field: &GenField) -> String {
    if field.type_text == "boolean" {
        format!("is{}", capitalized(field.name))
    } else {
        format!("get{}", capitalized(field.name))
    }
}

/// Where to insert generated members: just before the class body's closing
/// brace. Returns the byte offset, the member indentation, and the class's
/// own line indentation (for re-indenting the `}` line's neighborhood).
fn member_insertion<'t>(doc: &OpenDoc<'t>, td: &TypeDecl<'t>) -> Option<(usize, String, String)> {
    let body = named_children(td.node)
        .into_iter()
        .find(|c| c.kind() == "class_body")?;
    let class_indent = line_indent_lenient(doc.source, td.node.start_byte());
    let member_indent = named_children(body)
        .first()
        .and_then(|m| line_indent(doc.source, m.start_byte()))
        .unwrap_or_else(|| format!("{class_indent}    "));
    // `body.end_byte() - 1` is the `}` itself.
    Some((body.end_byte().checked_sub(1)?, member_indent, class_indent))
}

/// Where to insert a new *first* member (extract-constant wants constants at
/// the top): right after the class body's `{`. Returns the offset, member
/// indent, and the `(before, after)` text wrapping the declaration.
fn member_insertion_at_top<'t>(
    doc: &OpenDoc<'t>,
    td: &TypeDecl<'t>,
) -> Option<(usize, String, (&'static str, &'static str))> {
    let body = named_children(td.node)
        .into_iter()
        .find(|c| c.kind() == "class_body")?;
    let class_indent = line_indent_lenient(doc.source, td.node.start_byte());
    let member_indent = named_children(body)
        .first()
        .and_then(|m| line_indent(doc.source, m.start_byte()))
        .unwrap_or_else(|| format!("{class_indent}    "));
    // After the opening `{`.
    Some((body.start_byte() + 1, member_indent, ("\n", "\n")))
}

/// Like [`line_indent`] but falls back to an empty indent when the
/// declaration doesn't start its own line (annotated `@Getter class X` on
/// one line, say) — generation still works, just left-aligned.
fn line_indent_lenient(source: &str, at: usize) -> String {
    line_indent(source, at).unwrap_or_default()
}

fn generate_actions(doc: &OpenDoc, index: &LineIndex, at: usize) -> Vec<ActionSketch> {
    let Some(node) = doc
        .tree
        .root_node()
        .named_descendant_for_byte_range(at, at)
        .and_then(|n| enclosing_class(n))
    else {
        return Vec::new();
    };
    let Some(td) = TypeDecl::from_node(node, doc.source, 0) else {
        return Vec::new();
    };
    if td.kind != TypeKind::Class {
        return Vec::new();
    }
    let Some((insert_at, indent, class_indent)) = member_insertion(doc, &td) else {
        return Vec::new();
    };
    let fields = gen_fields(&td);
    if fields.is_empty() {
        return Vec::new();
    }
    let existing_methods: std::collections::HashSet<&str> = td
        .own_members()
        .into_iter()
        .filter(|m| matches!(m.kind, MemberKind::Method))
        .map(|m| m.name)
        .collect();

    let insert = |body: String| -> Vec<TextEdit> {
        let pos = index.position(insert_at);
        vec![TextEdit {
            range: Range {
                start: pos,
                end: pos,
            },
            new_text: format!("\n{body}{class_indent}"),
        }]
    };
    let mut out = Vec::new();

    // Getters and setters for every field missing them.
    let mut accessor_lines = String::new();
    for f in &fields {
        let getter = getter_name(f);
        if !existing_methods.contains(getter.as_str()) {
            accessor_lines.push_str(&format!(
                "{indent}public {} {getter}() {{\n{indent}    return {};\n{indent}}}\n\n",
                f.type_text, f.name
            ));
        }
        let setter = format!("set{}", capitalized(f.name));
        if !existing_methods.contains(setter.as_str()) {
            accessor_lines.push_str(&format!(
                "{indent}public void {setter}({} {}) {{\n{indent}    this.{} = {};\n{indent}}}\n\n",
                f.type_text, f.name, f.name, f.name
            ));
        }
    }
    if !accessor_lines.is_empty() {
        out.push(ActionSketch {
            title: "Generate getters and setters".to_string(),
            kind: KIND_SOURCE_GENERATE,
            edits: insert(accessor_lines),
            is_preferred: false,
        });
    }

    // All-fields constructor — only when the class declares none, so we
    // never fight an existing constructor set.
    if td.constructors().is_empty() {
        let params: Vec<String> = fields
            .iter()
            .map(|f| format!("{} {}", f.type_text, f.name))
            .collect();
        let assigns: Vec<String> = fields
            .iter()
            .map(|f| format!("{indent}    this.{} = {};\n", f.name, f.name))
            .collect();
        let body = format!(
            "{indent}public {}({}) {{\n{}{indent}}}\n\n",
            td.name,
            params.join(", "),
            assigns.join("")
        );
        out.push(ActionSketch {
            title: "Generate constructor".to_string(),
            kind: KIND_SOURCE_GENERATE,
            edits: insert(body),
            is_preferred: false,
        });
    }

    // equals + hashCode (java.util.Objects based).
    if !existing_methods.contains("equals") && !existing_methods.contains("hashCode") {
        let comparisons: Vec<String> = fields
            .iter()
            .map(|f| {
                if PRIMITIVES.contains(&f.type_text) {
                    format!("this.{} == other.{}", f.name, f.name)
                } else {
                    format!(
                        "java.util.Objects.equals(this.{}, other.{})",
                        f.name, f.name
                    )
                }
            })
            .collect();
        let names: Vec<&str> = fields.iter().map(|f| f.name).collect();
        let body = format!(
            "{indent}@Override\n\
             {indent}public boolean equals(Object o) {{\n\
             {indent}    if (this == o) {{\n{indent}        return true;\n{indent}    }}\n\
             {indent}    if (!(o instanceof {name})) {{\n{indent}        return false;\n{indent}    }}\n\
             {indent}    {name} other = ({name}) o;\n\
             {indent}    return {cmp};\n\
             {indent}}}\n\n\
             {indent}@Override\n\
             {indent}public int hashCode() {{\n\
             {indent}    return java.util.Objects.hash({args});\n\
             {indent}}}\n\n",
            name = td.name,
            cmp = comparisons.join("\n            && "),
            args = names.join(", "),
        );
        out.push(ActionSketch {
            title: "Generate equals() and hashCode()".to_string(),
            kind: KIND_SOURCE_GENERATE,
            edits: insert(body),
            is_preferred: false,
        });
    }

    // toString.
    if !existing_methods.contains("toString") {
        let parts: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let sep = if i == 0 { "\"" } else { "\", " };
                format!("{sep}{}=\" + {}", f.name, f.name)
            })
            .collect();
        let body = format!(
            "{indent}@Override\n\
             {indent}public String toString() {{\n\
             {indent}    return \"{name}{{\" + {parts} + \"}}\";\n\
             {indent}}}\n\n",
            name = td.name,
            parts = parts.join(" + "),
        );
        out.push(ActionSketch {
            title: "Generate toString()".to_string(),
            kind: KIND_SOURCE_GENERATE,
            edits: insert(body),
            is_preferred: false,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse, PositionEncoding};

    fn actions(src: &str, sel_start: usize, sel_end: usize) -> Vec<ActionSketch> {
        let tree = parse(&mut new_parser(), src, None).expect("parse");
        let doc = OpenDoc {
            source: src,
            tree: &tree,
        };
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        refactor_actions(
            &doc,
            &index,
            Range {
                start: index.position(sel_start),
                end: index.position(sel_end),
            },
        )
    }

    fn select(src: &str, needle: &str) -> (usize, usize) {
        let start = src.find(needle).expect("needle present");
        (start, start + needle.len())
    }

    fn apply_all(src: &str, edits: &[TextEdit]) -> String {
        // Apply in reverse document order so earlier offsets stay valid.
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let mut spans: Vec<(usize, usize, &str)> = edits
            .iter()
            .map(|e| {
                (
                    index.offset(e.range.start),
                    index.offset(e.range.end),
                    e.new_text.as_str(),
                )
            })
            .collect();
        spans.sort_by_key(|(s, _, _)| std::cmp::Reverse(*s));
        let mut out = src.to_string();
        for (s, e, text) in spans {
            out.replace_range(s..e, text);
        }
        out
    }

    #[test]
    fn extract_variable_from_method_invocation() {
        let src = "class C {\n\
                   \u{20}   void m(C c) {\n\
                   \u{20}       use(c.getName());\n\
                   \u{20}   }\n\
                   \u{20}   String getName() { return null; }\n\
                   \u{20}   void use(String s) {}\n\
                   }\n";
        let (s, e) = select(src, "c.getName()");
        let all = actions(src, s, e);
        let extract = all
            .iter()
            .find(|a| a.kind == KIND_REFACTOR_EXTRACT)
            .expect("extract action");
        assert_eq!(extract.title, "Extract to local variable 'name'");
        let fixed = apply_all(src, &extract.edits);
        assert!(
            fixed.contains("var name = c.getName();\n        use(name);"),
            "{fixed}"
        );
    }

    #[test]
    fn extract_variable_derives_unique_name() {
        let src = "class C {\n\
                   \u{20}   void m(C c) {\n\
                   \u{20}       String name = \"x\";\n\
                   \u{20}       use(c.getName());\n\
                   \u{20}   }\n\
                   }\n";
        let (s, e) = select(src, "c.getName()");
        let all = actions(src, s, e);
        let extract = all
            .iter()
            .find(|a| a.kind == KIND_REFACTOR_EXTRACT)
            .expect("extract action");
        // `name` is taken — the derived name gets a counter.
        assert_eq!(extract.title, "Extract to local variable 'name2'");
    }

    #[test]
    fn extract_variable_not_offered_for_partial_or_non_expression() {
        let src = "class C { void m(C c) { use(c.getName()); } }\n";
        // Partial selection (`c.getNam`) → nothing.
        let start = src.find("c.getName()").unwrap();
        assert!(actions(src, start, start + 8)
            .iter()
            .all(|a| a.kind != KIND_REFACTOR_EXTRACT));
        // Statement on the same line as `if` → refused (line-start rule)…
        let src2 = "class C { void m(int x) { if (x > 0) use(f(x)); } }\n";
        let (s, e) = select(src2, "f(x)");
        assert!(actions(src2, s, e)
            .iter()
            .all(|a| a.kind != KIND_REFACTOR_EXTRACT));
    }

    #[test]
    fn extract_constant_from_string_literal() {
        let src = "class C {\n\
                   \u{20}   void m() {\n\
                   \u{20}       send(\"not-found\");\n\
                   \u{20}   }\n\
                   }\n";
        let (s, e) = select(src, "\"not-found\"");
        let all = actions(src, s, e);
        let extract = all
            .iter()
            .find(|a| a.title.contains("constant"))
            .expect("constant action");
        assert_eq!(extract.title, "Extract to constant 'NOT_FOUND'");
        let fixed = apply_all(src, &extract.edits);
        assert!(
            fixed.contains("private static final String NOT_FOUND = \"not-found\";"),
            "{fixed}"
        );
        assert!(fixed.contains("send(NOT_FOUND);"), "{fixed}");
    }

    #[test]
    fn generate_accessors_constructor_equals_tostring() {
        let src = "package demo;\n\
                   public class Person {\n\
                   \u{20}   private String name;\n\
                   \u{20}   private int age;\n\
                   \u{20}   public String getName() { return name; }\n\
                   }\n";
        let at = src.find("private String").unwrap();
        let all = actions(src, at, at);
        let titles: Vec<&str> = all.iter().map(|a| a.title.as_str()).collect();
        assert!(
            titles.contains(&"Generate getters and setters"),
            "{titles:?}"
        );
        assert!(titles.contains(&"Generate constructor"), "{titles:?}");
        assert!(
            titles.contains(&"Generate equals() and hashCode()"),
            "{titles:?}"
        );
        assert!(titles.contains(&"Generate toString()"), "{titles:?}");

        let accessors = all
            .iter()
            .find(|a| a.title == "Generate getters and setters")
            .unwrap();
        let fixed = apply_all(src, &accessors.edits);
        // Existing getName is respected; the rest appear, properly indented.
        assert_eq!(fixed.matches("getName()").count(), 1, "{fixed}");
        assert!(
            fixed.contains(
                "    public void setName(String name) {\n        this.name = name;\n    }"
            ),
            "{fixed}"
        );
        assert!(fixed.contains("public int getAge()"), "{fixed}");

        let ctor = all
            .iter()
            .find(|a| a.title == "Generate constructor")
            .unwrap();
        let fixed = apply_all(src, &ctor.edits);
        assert!(
            fixed.contains("public Person(String name, int age) {"),
            "{fixed}"
        );
        assert!(fixed.contains("this.name = name;"), "{fixed}");

        let eq = all
            .iter()
            .find(|a| a.title == "Generate equals() and hashCode()")
            .unwrap();
        let fixed = apply_all(src, &eq.edits);
        assert!(
            fixed.contains("java.util.Objects.equals(this.name, other.name)"),
            "{fixed}"
        );
        assert!(fixed.contains("this.age == other.age"), "{fixed}");
        assert!(
            fixed.contains("java.util.Objects.hash(name, age)"),
            "{fixed}"
        );
    }

    #[test]
    fn generate_constructor_not_offered_when_one_exists() {
        let src = "class Person {\n\
                   \u{20}   private String name;\n\
                   \u{20}   Person(String name) { this.name = name; }\n\
                   }\n";
        let at = src.find("private").unwrap();
        let titles: Vec<String> = actions(src, at, at).into_iter().map(|a| a.title).collect();
        assert!(
            !titles.iter().any(|t| t == "Generate constructor"),
            "{titles:?}"
        );
    }

    #[test]
    fn generate_offers_nothing_outside_a_class_or_without_fields() {
        let src = "interface Greeter { String greet(); }\n";
        let at = src.find("String").unwrap();
        assert!(actions(src, at, at).is_empty());
        let src = "class Empty { void m() {} }\n";
        let at = src.find("void").unwrap();
        assert!(actions(src, at, at)
            .iter()
            .all(|a| a.kind != KIND_SOURCE_GENERATE));
    }
}
