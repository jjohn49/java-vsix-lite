//! Syntax-level analysis for the pure-Rust default tier.
//!
//! Wraps `tree-sitter-java` for error-tolerant, incremental parsing and turns
//! the resulting tree into LSP artifacts (currently: syntax diagnostics).
//! Everything here operates on a single open document — no workspace indexing.
//!
//! ## Position encoding
//!
//! tree-sitter reports **byte** offsets; LSP reports `(line, character)` where
//! `character` is counted in UTF-16 code units by default, or per the
//! negotiated `positionEncoding` (LSP 3.17+). [`LineIndex`] converts between the
//! two for whichever encoding was negotiated, so non-ASCII source (identifiers,
//! string literals, comments) maps to correct ranges.

#![forbid(unsafe_code)]

use ls_types::{Diagnostic, DiagnosticSeverity, DocumentSymbol, Position, Range, SymbolKind};
use tree_sitter::{Node, Parser, Tree};

/// Re-exported so the server can name `Tree`/`Parser` without a direct
/// dependency on a specific tree-sitter version.
pub use tree_sitter;

/// Cap on diagnostics emitted per document — pathological input (thousands of
/// errors) must not flood the client or the traversal.
const MAX_DIAGNOSTICS: usize = 100;

/// LSP position encoding negotiated with the client during `initialize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    /// `character` counts UTF-8 bytes (matches tree-sitter directly).
    Utf8,
    /// `character` counts UTF-16 code units (LSP default).
    Utf16,
}

/// Build a tree-sitter parser configured for Java. Panics only if the bundled
/// grammar is ABI-incompatible with the linked tree-sitter, which is a build
/// error, not a runtime/input condition.
pub fn new_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("tree-sitter-java grammar is ABI-compatible");
    parser
}

/// Parse `text`, optionally reusing `old_tree` for incremental reparsing.
pub fn parse(parser: &mut Parser, text: &str, old_tree: Option<&Tree>) -> Option<Tree> {
    parser.parse(text, old_tree)
}

/// Maps byte offsets to LSP [`Position`]s for a single document, honoring the
/// negotiated [`PositionEncoding`].
pub struct LineIndex<'a> {
    text: &'a str,
    /// Byte offset of the start of each line (`line_starts[0] == 0`).
    line_starts: Vec<usize>,
    encoding: PositionEncoding,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str, encoding: PositionEncoding) -> Self {
        let mut line_starts = vec![0usize];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self {
            text,
            line_starts,
            encoding,
        }
    }

    /// Convert a byte offset into an LSP [`Position`]. Offsets are clamped to the
    /// document and snapped to a UTF-8 char boundary, so even an out-of-range or
    /// mid-codepoint offset is handled without panicking.
    pub fn position(&self, byte: usize) -> Position {
        let byte = floor_char_boundary(self.text, byte);
        let line = match self.line_starts.binary_search(&byte) {
            Ok(line) => line,
            Err(next) => next - 1,
        };
        let line_start = self.line_starts[line];
        let prefix = &self.text[line_start..byte];
        let character: usize = match self.encoding {
            PositionEncoding::Utf8 => prefix.len(),
            PositionEncoding::Utf16 => prefix.chars().map(char::len_utf16).sum(),
        };
        Position {
            line: line as u32,
            character: character as u32,
        }
    }

    fn range(&self, node: Node) -> Range {
        Range {
            start: self.position(node.start_byte()),
            end: self.position(node.end_byte()),
        }
    }
}

/// Largest byte index `<= byte` that is a valid char boundary (and `<= len`).
fn floor_char_boundary(text: &str, byte: usize) -> usize {
    if byte >= text.len() {
        return text.len();
    }
    let mut b = byte;
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// Collect syntax diagnostics (ERROR and MISSING nodes) from a parsed tree.
/// Subtrees without errors are pruned, so the traversal cost is proportional to
/// the damaged region, not the whole file.
pub fn syntax_diagnostics(tree: &Tree, line_index: &LineIndex) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let root = tree.root_node();
    if !root.has_error() {
        return diagnostics;
    }

    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if diagnostics.len() >= MAX_DIAGNOSTICS {
            break;
        }

        if node.is_missing() {
            diagnostics.push(diagnostic(
                line_index.range(node),
                format!("Syntax error: missing `{}`", node.kind()),
            ));
            continue;
        }
        if node.is_error() {
            diagnostics.push(diagnostic(
                line_index.range(node),
                "Syntax error: unexpected input".to_string(),
            ));
            // Don't descend: the whole erroneous region is reported once.
            continue;
        }

        // Descend only where an error/missing node actually lives.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.has_error() || child.is_missing() {
                stack.push(child);
            }
        }
    }

    diagnostics
}

fn diagnostic(range: Range, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("java-vsix-lite".to_string()),
        message,
        ..Default::default()
    }
}

/// Build the document outline (nested [`DocumentSymbol`]s) for a parsed tree:
/// types, methods, constructors, fields, and enum constants. Tolerant of parse
/// errors — it simply reports whatever declarations tree-sitter recovered.
pub fn document_symbols(tree: &Tree, source: &str, index: &LineIndex) -> Vec<DocumentSymbol> {
    container_children(tree.root_node(), source, index)
}

/// Collect the declaration symbols directly contained in `node` (a `program`,
/// `*_body`, or `enum_body_declarations`).
fn container_children(node: Node, source: &str, index: &LineIndex) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            // A field declaration introduces one symbol per declarator.
            "field_declaration" => push_field_symbols(child, source, index, &mut out),
            // Methods/fields inside an enum live under this wrapper node.
            "enum_body_declarations" => out.extend(container_children(child, source, index)),
            _ => {
                if let Some(symbol) = symbol_for(child, source, index) {
                    out.push(symbol);
                }
            }
        }
    }
    out
}

/// Build a symbol for a single declaration node, recursing into its body.
fn symbol_for(node: Node, source: &str, index: &LineIndex) -> Option<DocumentSymbol> {
    let (kind, body_kind) = match node.kind() {
        "class_declaration" | "record_declaration" => (SymbolKind::CLASS, Some("class_body")),
        "interface_declaration" => (SymbolKind::INTERFACE, Some("interface_body")),
        "annotation_type_declaration" => (SymbolKind::INTERFACE, Some("annotation_type_body")),
        "enum_declaration" => (SymbolKind::ENUM, Some("enum_body")),
        "method_declaration" => (SymbolKind::METHOD, None),
        "constructor_declaration" => (SymbolKind::CONSTRUCTOR, None),
        "enum_constant" => (SymbolKind::ENUM_MEMBER, None),
        _ => return None,
    };
    let name_node = node.child_by_field_name("name")?;
    let children = body_kind
        .and_then(|kind| child_of_kind(node, kind))
        .map(|body| container_children(body, source, index))
        .unwrap_or_default();
    Some(make_symbol(
        node_text(name_node, source).to_string(),
        kind,
        node,
        name_node,
        children,
        index,
    ))
}

fn push_field_symbols(node: Node, source: &str, index: &LineIndex, out: &mut Vec<DocumentSymbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        if let Some(name_node) = child.child_by_field_name("name") {
            out.push(make_symbol(
                node_text(name_node, source).to_string(),
                SymbolKind::FIELD,
                child,
                name_node,
                Vec::new(),
                index,
            ));
        }
    }
}

fn make_symbol(
    name: String,
    kind: SymbolKind,
    range_node: Node,
    selection_node: Node,
    children: Vec<DocumentSymbol>,
    index: &LineIndex,
) -> DocumentSymbol {
    #[allow(deprecated)] // `deprecated` is a required (if deprecated) struct field
    DocumentSymbol {
        name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: index.range(range_node),
        selection_range: index.range(selection_node),
        children: (!children.is_empty()).then_some(children),
    }
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    // Bind to a local so the borrowing iterator drops before we return the
    // node (whose lifetime is the tree's, not the cursor's).
    let found = node.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(text: &str) -> Tree {
        parse(&mut new_parser(), text, None).expect("parse")
    }

    #[test]
    fn valid_java_has_no_diagnostics() {
        let src = "class A { void m() {} }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        assert!(syntax_diagnostics(&tree, &index).is_empty());
    }

    #[test]
    fn missing_brace_is_reported() {
        let src = "class A {\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let diags = syntax_diagnostics(&tree, &index);
        assert!(
            !diags.is_empty(),
            "expected a diagnostic for unclosed class"
        );
    }

    #[test]
    fn missing_semicolon_is_reported() {
        let src = "class A { int x = 1 }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let diags = syntax_diagnostics(&tree, &index);
        assert!(
            diags.iter().any(|d| d.message.contains("missing")),
            "expected a missing-token diagnostic, got: {diags:?}"
        );
    }

    #[test]
    fn utf16_position_counts_code_units() {
        // 😀 is one code point but two UTF-16 code units and four UTF-8 bytes.
        let src = "class 😀 {}";
        let byte = src.find('{').unwrap();
        let utf16 = LineIndex::new(src, PositionEncoding::Utf16).position(byte);
        let utf8 = LineIndex::new(src, PositionEncoding::Utf8).position(byte);
        assert_eq!(utf16.line, 0);
        // "class " (6) + emoji (2 UTF-16 units) + " " (1) = 9
        assert_eq!(utf16.character, 9);
        // "class " (6) + emoji (4 bytes) + " " (1) = 11
        assert_eq!(utf8.character, 11);
    }

    #[test]
    fn position_handles_second_line() {
        let src = "class A {\n  int x;\n}\n";
        let byte = src.find("int").unwrap();
        let pos = LineIndex::new(src, PositionEncoding::Utf16).position(byte);
        assert_eq!((pos.line, pos.character), (1, 2));
    }

    fn symbols(src: &str) -> Vec<DocumentSymbol> {
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        document_symbols(&tree, src, &index)
    }

    #[test]
    fn outline_nests_members_under_class() {
        let src =
            "package p;\nclass Outer {\n  int field;\n  void method(int a) {}\n  Outer() {}\n}\n";
        let syms = symbols(src);
        assert_eq!(syms.len(), 1, "expected one top-level class: {syms:?}");
        let class = &syms[0];
        assert_eq!(class.name, "Outer");
        assert_eq!(class.kind, SymbolKind::CLASS);
        let members = class.children.as_ref().expect("class has members");
        let by_name = |k: SymbolKind| {
            members
                .iter()
                .find(|m| m.kind == k)
                .map(|m| m.name.as_str())
        };
        assert_eq!(by_name(SymbolKind::FIELD), Some("field"));
        assert_eq!(by_name(SymbolKind::METHOD), Some("method"));
        assert_eq!(by_name(SymbolKind::CONSTRUCTOR), Some("Outer"));
    }

    #[test]
    fn outline_includes_enum_constants_and_methods() {
        let src = "enum E { A, B; void m() {} }\n";
        let syms = symbols(src);
        let e = &syms[0];
        assert_eq!(e.kind, SymbolKind::ENUM);
        let members = e.children.as_ref().expect("enum has members");
        let constants: Vec<_> = members
            .iter()
            .filter(|m| m.kind == SymbolKind::ENUM_MEMBER)
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(constants, vec!["A", "B"]);
        assert!(members
            .iter()
            .any(|m| m.kind == SymbolKind::METHOD && m.name == "m"));
    }

    #[test]
    fn outline_selection_range_is_the_name() {
        let src = "class Foo {}\n";
        let class = &symbols(src)[0];
        // selection range covers "Foo" (chars 6..9), within the full decl range.
        assert_eq!(class.selection_range.start.character, 6);
        assert_eq!(class.selection_range.end.character, 9);
        assert_eq!(class.range.start.character, 0);
    }
}
