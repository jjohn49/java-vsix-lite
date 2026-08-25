//! Syntax-level analysis for the pure-Rust default tier.
//!
//! Wraps `tree-sitter-java` for error-tolerant, incremental parsing and turns
//! the resulting tree into LSP artifacts: syntax diagnostics, document symbols,
//! folding/selection ranges, and semantic tokens (each over a single document),
//! plus hover and completion, which additionally resolve types declared in other
//! open documents (see [`OpenDoc`]). No workspace or JAR/JDK indexing.
//!
//! ## Position encoding
//!
//! tree-sitter reports **byte** offsets; LSP reports `(line, character)` where
//! `character` is counted in UTF-16 code units by default, or per the
//! negotiated `positionEncoding` (LSP 3.17+). [`LineIndex`] converts between the
//! two for whichever encoding was negotiated, so non-ASCII source (identifiers,
//! string literals, comments) maps to correct ranges.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ls_types::{
    Diagnostic, DiagnosticSeverity, DocumentSymbol, FoldingRange, FoldingRangeKind, Position,
    Range, SelectionRange, SemanticToken, SemanticTokenType, SymbolKind,
};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

/// Re-exported so the server can name `Tree`/`Parser` without a direct
/// dependency on a specific tree-sitter version.
pub use tree_sitter;

mod codeaction;
mod completion;
mod definition;
mod diagnostics;
mod docsrc;
mod external;
mod generate;
mod hierarchy;
mod hover;
mod implementation;
mod imports;
mod jdoc;
mod lombok;
mod model;
mod references;
mod rename;
mod resolve;
mod signature;
mod signature_help;
mod srcclass;
mod structural;

pub use codeaction::{code_actions, ActionSketch, KIND_ORGANIZE_IMPORTS, KIND_QUICKFIX};
pub use completion::{completion, resolve_documentation, CompletionResult};
pub use definition::{definition, locate_type_in_source, type_definition, Definition};
pub use diagnostics::{
    semantic_diagnostics, INCOMPATIBLE_ASSIGNMENT_CODE, INCOMPATIBLE_RETURN_CODE, UNREACHABLE_CODE,
};
pub use docsrc::{javadoc_in_source, locate_in_source};
pub use external::{
    ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols, SymbolSource, TypeCandidate,
};
pub use hierarchy::{
    callable_decl_at_name, enclosing_callable, outgoing_call_sites, type_decl_at,
    type_decl_at_byte, type_info_in, CallSite, CallableInfo, CallableKind, SuperRef, TypeInfo,
    TypeInfoKind,
};
pub use hover::hover;
pub use implementation::{
    implementation_target, implementations_in_doc, ImplementationHit, ImplementationTarget,
};
pub use references::{reference_target, references_in_doc, ReferenceHits, ReferenceTarget, Tier};
pub use rename::{
    collides_with_existing, is_public_top_level_type, is_valid_new_name, prepare_rename,
    PrepareRename,
};
pub use signature_help::signature_help;
pub use srcclass::class_from_source;
pub use structural::structural_diagnostics;

/// A snapshot of one open document the analysis can read: its source text and
/// the parse tree kept in sync with it. Borrowed for the duration of one request
/// (resolution runs synchronously while the server holds the documents lock).
pub struct OpenDoc<'a> {
    pub source: &'a str,
    pub tree: &'a Tree,
}

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

    /// Convert an LSP [`Position`] back into a byte offset (the inverse of
    /// [`Self::position`]). Out-of-range lines/characters clamp to the document.
    pub fn offset(&self, position: Position) -> usize {
        let Some(&line_start) = self.line_starts.get(position.line as usize) else {
            return self.text.len();
        };
        let line_end = self
            .line_starts
            .get(position.line as usize + 1)
            .copied()
            .unwrap_or(self.text.len());
        let target = position.character as usize;
        let mut units = 0usize;
        let mut byte = line_start;
        for ch in self.text[line_start..line_end].chars() {
            if units >= target {
                break;
            }
            units += match self.encoding {
                PositionEncoding::Utf8 => ch.len_utf8(),
                PositionEncoding::Utf16 => ch.len_utf16(),
            };
            byte += ch.len_utf8();
        }
        byte
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

/// Compute folding ranges: type/method/lambda bodies, switch and array blocks,
/// multi-line block comments, and the leading import group. tree-sitter rows are
/// 0-based line numbers, identical to LSP lines, so no encoding conversion is
/// needed here.
pub fn folding_ranges(tree: &Tree) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();
    let root = tree.root_node();
    fold_import_group(root, &mut ranges);

    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = match node.kind() {
            "class_body"
            | "interface_body"
            | "enum_body"
            | "annotation_type_body"
            | "block"
            | "constructor_body"
            | "switch_block"
            | "array_initializer" => Some(None),
            "block_comment" => Some(Some(FoldingRangeKind::Comment)),
            _ => None,
        };
        if let Some(kind) = kind {
            push_fold(node, kind, &mut ranges);
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    ranges
}

fn fold_import_group(root: Node, ranges: &mut Vec<FoldingRange>) {
    let mut cursor = root.walk();
    let imports: Vec<Node> = root
        .children(&mut cursor)
        .filter(|c| c.kind() == "import_declaration")
        .collect();
    if let (Some(first), Some(last)) = (imports.first(), imports.last()) {
        let (start, end) = (
            first.start_position().row as u32,
            last.end_position().row as u32,
        );
        if end > start {
            ranges.push(FoldingRange {
                start_line: start,
                end_line: end,
                kind: Some(FoldingRangeKind::Imports),
                ..Default::default()
            });
        }
    }
}

fn push_fold(node: Node, kind: Option<FoldingRangeKind>, ranges: &mut Vec<FoldingRange>) {
    let start = node.start_position().row as u32;
    let end = node.end_position().row as u32;
    if end > start {
        ranges.push(FoldingRange {
            start_line: start,
            end_line: end,
            kind,
            ..Default::default()
        });
    }
}

/// The result of applying one LSP incremental content change: the new document
/// text and the [`InputEdit`] to feed the previous tree (via [`Tree::edit`])
/// before an incremental reparse.
pub struct AppliedEdit {
    pub new_text: String,
    pub input_edit: InputEdit,
}

/// Apply a single ranged content change to `text`, producing the new text and
/// the matching tree-sitter [`InputEdit`].
///
/// tree-sitter [`Point`] columns are **byte** offsets within a line (encoding
/// independent), so positions are converted through the byte domain. An
/// inverted or out-of-range range is clamped rather than panicking.
pub fn apply_content_change(
    text: &str,
    encoding: PositionEncoding,
    range: Range,
    replacement: &str,
) -> AppliedEdit {
    let index = LineIndex::new(text, encoding);
    let start_byte = index.offset(range.start);
    let old_end_byte = index.offset(range.end).max(start_byte);

    let line_start = |byte: usize| text[..byte].rfind('\n').map_or(0, |nl| nl + 1);
    let start_position = Point {
        row: range.start.line as usize,
        column: start_byte - line_start(start_byte),
    };
    let old_end_position = Point {
        row: range.end.line as usize,
        column: old_end_byte - line_start(old_end_byte),
    };

    let mut new_text =
        String::with_capacity(text.len() - (old_end_byte - start_byte) + replacement.len());
    new_text.push_str(&text[..start_byte]);
    new_text.push_str(replacement);
    new_text.push_str(&text[old_end_byte..]);

    let new_end_byte = start_byte + replacement.len();
    let new_end_position = match replacement.rfind('\n') {
        None => Point {
            row: start_position.row,
            column: start_position.column + replacement.len(),
        },
        Some(last_nl) => Point {
            row: start_position.row + replacement.matches('\n').count(),
            column: replacement.len() - last_nl - 1,
        },
    };

    AppliedEdit {
        new_text,
        input_edit: InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        },
    }
}

/// For each requested position, return the chain of enclosing syntax ranges
/// (innermost first, each pointing to its larger parent up to the file root) —
/// the data backing editor "expand/shrink selection".
pub fn selection_ranges(
    tree: &Tree,
    index: &LineIndex,
    positions: &[Position],
) -> Vec<SelectionRange> {
    positions
        .iter()
        .map(|&position| selection_range_at(tree, index, position))
        .collect()
}

fn selection_range_at(tree: &Tree, index: &LineIndex, position: Position) -> SelectionRange {
    let byte = index.offset(position);
    let root = tree.root_node();
    let mut node = root
        .named_descendant_for_byte_range(byte, byte)
        .unwrap_or(root);

    // Walk node -> root, collecting the ancestor chain.
    let mut chain = vec![node];
    while let Some(parent) = node.parent() {
        chain.push(parent);
        node = parent;
    }

    // Fold outermost -> innermost so each range's `parent` is the larger one.
    let mut current: Option<SelectionRange> = None;
    for node in chain.into_iter().rev() {
        current = Some(SelectionRange {
            range: index.range(node),
            parent: current.map(Box::new),
        });
    }
    current.expect("ancestor chain is never empty")
}

// Semantic token type indices. These MUST stay in sync with the order of
// [`semantic_token_types`], which the server passes to the client as the legend.
const TT_TYPE: u32 = 0;
const TT_METHOD: u32 = 1;
/// Deliberately no longer emitted — kept in the legend so the other
/// indices stay stable. Parameters are classified [`TT_VARIABLE`] instead:
/// several popular themes style the `parameter` semantic token greyed/italic,
/// which users read as "unused", and the parameter/variable distinction isn't
/// worth that confusion (user directive).
#[allow(dead_code)]
const TT_PARAMETER: u32 = 2;
const TT_PROPERTY: u32 = 3;
const TT_VARIABLE: u32 = 4;
const TT_ENUM_MEMBER: u32 = 5;
const TT_DECORATOR: u32 = 6;
const TT_NAMESPACE: u32 = 7;

/// The semantic-token legend, in index order. Deliberately a focused set: the
/// cases the built-in TextMate grammar cannot reliably tell apart (type vs
/// method vs parameter vs field vs package). Keywords/strings/numbers/comments
/// are left to TextMate, so semantic highlighting *enhances* rather than
/// replaces it.
pub fn semantic_token_types() -> Vec<SemanticTokenType> {
    vec![
        SemanticTokenType::TYPE,
        SemanticTokenType::METHOD,
        SemanticTokenType::PARAMETER,
        SemanticTokenType::PROPERTY,
        SemanticTokenType::VARIABLE,
        SemanticTokenType::ENUM_MEMBER,
        SemanticTokenType::DECORATOR,
        SemanticTokenType::NAMESPACE,
    ]
}

struct RawToken {
    line: u32,
    start: u32,
    len: u32,
    token_type: u32,
}

/// Produce LSP semantic tokens (delta-encoded) for the whole document, in the
/// negotiated position encoding. Declarations are classified directly; plain
/// identifier *usages* are classified by matching the file's declared
/// variable/parameter/field names (so references — not just declarations — get
/// highlighted), and package/import path segments get namespace tokens.
pub fn semantic_tokens(tree: &Tree, source: &str, index: &LineIndex) -> Vec<SemanticToken> {
    let roles = declared_roles(tree, source);
    let mut raw: Vec<RawToken> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => emit_named(node, "name", TT_TYPE, index, &mut raw),
            "type_identifier" => emit(node, TT_TYPE, index, &mut raw),
            "method_declaration" | "method_invocation" => {
                emit_named(node, "name", TT_METHOD, index, &mut raw)
            }
            // Parameters classify as plain variables — see [`TT_PARAMETER`].
            "formal_parameter" | "spread_parameter" | "catch_formal_parameter" => {
                emit_named(node, "name", TT_VARIABLE, index, &mut raw)
            }
            "enhanced_for_statement" => emit_named(node, "name", TT_VARIABLE, index, &mut raw),
            // Java 21 pattern bindings (`case Type name`, record deconstruction
            // components) — the binding identifier classifies as a variable so
            // it (and, via `declared_roles`, its uses in the guard/body) get
            // lit instead of falling to TextMate's default (white) foreground.
            "type_pattern" | "record_pattern_component" => {
                if let Some(id) = pattern_binding_identifier(node) {
                    emit(id, TT_VARIABLE, index, &mut raw);
                }
            }
            // `f instanceof String s` — the `name`-field binding.
            "instanceof_expression" => emit_named(node, "name", TT_VARIABLE, index, &mut raw),
            // `Type::method` / `expr::method` — the referenced method name.
            "method_reference" => {
                if let Some(m) = method_reference_method(node) {
                    emit(m, TT_METHOD, index, &mut raw);
                }
            }
            "variable_declarator" => {
                let is_field = node
                    .parent()
                    .is_some_and(|p| p.kind() == "field_declaration");
                let token_type = if is_field { TT_PROPERTY } else { TT_VARIABLE };
                emit_named(node, "name", token_type, index, &mut raw);
            }
            "field_access" => emit_named(node, "field", TT_PROPERTY, index, &mut raw),
            "enum_constant" => emit_named(node, "name", TT_ENUM_MEMBER, index, &mut raw),
            "marker_annotation" | "annotation" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        emit(name, TT_DECORATOR, index, &mut raw);
                    }
                }
            }
            "package_declaration" => emit_namespace_path(node, false, source, index, &mut raw),
            "import_declaration" => emit_namespace_path(node, true, source, index, &mut raw),
            // A plain identifier *usage* (not a declaration name handled above):
            // classify it from the file's declared names so references are lit.
            "identifier" if !is_classified_elsewhere(node) => {
                let text = node_text(node, source);
                if let Some(&token_type) = roles.get(text) {
                    emit(node, token_type, index, &mut raw);
                } else if looks_like_type_name(text) {
                    // An undeclared, type-cased name (`Math` in
                    // `Math.abs()`, `Person` in an expression) is a type
                    // reference by Java convention — without this it
                    // falls to TextMate's variable color (user report:
                    // "Math turns light blue"). SCREAMING_CASE constants
                    // don't match (no lowercase char) and stay untouched.
                    emit(node, TT_TYPE, index, &mut raw);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }

    raw.sort_by_key(|t| (t.line, t.start));
    raw.dedup_by_key(|t| (t.line, t.start));
    delta_encode(&raw)
}

/// Map each declared variable/parameter/field name to its token type, so plain
/// identifier usages can be classified by name. Lexical and scope-insensitive
/// (a name maps to one role) — approximate but cheap, and good enough for
/// highlighting references.
fn declared_roles<'t>(tree: &'t Tree, source: &'t str) -> HashMap<&'t str, u32> {
    let mut roles = HashMap::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "variable_declarator" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let is_field = node
                        .parent()
                        .is_some_and(|p| p.kind() == "field_declaration");
                    roles.insert(
                        node_text(name, source),
                        if is_field { TT_PROPERTY } else { TT_VARIABLE },
                    );
                }
            }
            // Parameters classify as plain variables — see [`TT_PARAMETER`].
            "formal_parameter" | "catch_formal_parameter" => {
                if let Some(name) = node.child_by_field_name("name") {
                    roles.insert(node_text(name, source), TT_VARIABLE);
                }
            }
            // Varargs parameter: its name lives on a nested declarator.
            "spread_parameter" => {
                if let Some(name) = crate::signature::spread_param_name(node) {
                    roles.insert(node_text(name, source), TT_VARIABLE);
                }
            }
            "enhanced_for_statement" => {
                if let Some(name) = node.child_by_field_name("name") {
                    roles.insert(node_text(name, source), TT_VARIABLE);
                }
            }
            // Java 21 pattern bindings — so their uses in the guard/body light up.
            "type_pattern" | "record_pattern_component" => {
                if let Some(id) = pattern_binding_identifier(node) {
                    roles.insert(node_text(id, source), TT_VARIABLE);
                }
            }
            "instanceof_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    roles.insert(node_text(name, source), TT_VARIABLE);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    roles
}

/// Whether an `identifier` is already classified by a declaration/access rule
/// above, so the usage pass must not re-emit or misclassify it.
fn is_classified_elsewhere(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declarator"
        | "formal_parameter"
        | "spread_parameter"
        | "catch_formal_parameter"
        | "enhanced_for_statement"
        | "method_declaration"
        | "constructor_declaration"
        | "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration"
        | "enum_constant"
        | "method_invocation"
        | "marker_annotation"
        | "annotation" => parent.child_by_field_name("name") == Some(node),
        "field_access" => parent.child_by_field_name("field") == Some(node),
        // The binding identifier of a pattern is emitted by the pattern rule.
        "type_pattern" | "record_pattern_component" => {
            pattern_binding_identifier(parent) == Some(node)
        }
        "instanceof_expression" => parent.child_by_field_name("name") == Some(node),
        "method_reference" => method_reference_method(parent) == Some(node),
        _ => false,
    }
}

/// Emit tokens for the dotted segments of a `package`/`import` path. A
/// `package` header's segments are namespaces. An `import`'s segments are
/// ALL type tokens (user directive: the whole imported path colors
/// like the class, not just its final segment) — except a static import's
/// final lowercase segment, which is the imported *member* and gets a
/// method token.
fn emit_namespace_path(
    node: Node,
    is_import: bool,
    source: &str,
    index: &LineIndex,
    out: &mut Vec<RawToken>,
) {
    let mut ids = Vec::new();
    collect_identifiers(node, &mut ids);
    if !is_import {
        for id in &ids {
            emit(*id, TT_NAMESPACE, index, out);
        }
        return;
    }
    let is_static = has_child_kind(node, "static");
    let last = ids.len().saturating_sub(1);
    for (i, id) in ids.iter().enumerate() {
        let text = node_text(*id, source);
        let token_type = if is_static
            && i == last
            && text.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        {
            TT_METHOD
        } else {
            TT_TYPE
        };
        emit(*id, token_type, index, out);
    }
}

/// Type-cased by Java convention: starts uppercase and contains at least one
/// lowercase character (so `Math`/`Person` match, `MAX_VALUE` doesn't).
///
/// `pub(crate)`: also gates `codeaction.rs`'s add-import quick fix.
pub(crate) fn looks_like_type_name(text: &str) -> bool {
    text.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && text.chars().any(|c| c.is_ascii_lowercase())
}

fn collect_identifiers<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if node.kind() == "identifier" {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers(child, out);
    }
}

fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    // Bind to a local so the borrowing iterator drops before the return.
    let found = node.children(&mut cursor).any(|c| c.kind() == kind);
    found
}

fn emit_named(
    node: Node,
    field: &str,
    token_type: u32,
    index: &LineIndex,
    out: &mut Vec<RawToken>,
) {
    if let Some(name) = node.child_by_field_name(field) {
        emit(name, token_type, index, out);
    }
}

/// The binding-name identifier of a Java 21 pattern node (`type_pattern`'s
/// `String s`, a `record_pattern_component`'s `int x`): its lone child of kind
/// `identifier`. Neither node exposes a `name` field, and the *type* part is a
/// `type_identifier`/`generic_type`/`integral_type`/… — never a bare
/// `identifier` — so the sole `identifier` child is always the binding.
/// `None` for a component that nests another pattern instead of binding a name
/// (`case Line(Point(var a, var b), ...)`).
fn pattern_binding_identifier(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "identifier");
    found
}

/// The referenced-method identifier of a `method_reference`
/// (`Type::method`, `expr::method`): its trailing `identifier` child, after
/// the qualifier and `::`. `None` for a constructor reference (`Type::new`,
/// whose trailing child is the `new` keyword) or an incomplete one.
fn method_reference_method(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    let named: Vec<Node> = node.named_children(&mut cursor).collect();
    if named.len() < 2 {
        return None;
    }
    let last = *named.last()?;
    (last.kind() == "identifier").then_some(last)
}

fn emit(node: Node, token_type: u32, index: &LineIndex, out: &mut Vec<RawToken>) {
    let start = index.position(node.start_byte());
    let end = index.position(node.end_byte());
    // Semantic tokens may not span lines; our identifier tokens never do.
    if end.line != start.line || end.character <= start.character {
        return;
    }
    out.push(RawToken {
        line: start.line,
        start: start.character,
        len: end.character - start.character,
        token_type,
    });
}

fn delta_encode(tokens: &[RawToken]) -> Vec<SemanticToken> {
    let mut data = Vec::with_capacity(tokens.len());
    let (mut prev_line, mut prev_start) = (0u32, 0u32);
    for token in tokens {
        let delta_line = token.line - prev_line;
        let delta_start = if delta_line == 0 {
            token.start - prev_start
        } else {
            token.start
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: token.len,
            token_type: token.token_type,
            token_modifiers_bitset: 0,
        });
        prev_line = token.line;
        prev_start = token.start;
    }
    data
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

    #[test]
    fn offset_round_trips_with_position() {
        let src = "class A {\n  int x;\n}\n";
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        for byte in [0usize, 6, src.find("int").unwrap(), src.find('x').unwrap()] {
            assert_eq!(index.offset(index.position(byte)), byte);
        }
    }

    #[test]
    fn offset_handles_utf16_surrogates() {
        let src = "class 😀 {}";
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let brace = src.find('{').unwrap();
        assert_eq!(index.offset(index.position(brace)), brace);
    }

    #[test]
    fn folding_covers_class_and_method_bodies() {
        let src = "class A {\n  void m() {\n    return;\n  }\n}\n";
        let folds = folding_ranges(&parse_str(src));
        // class_body spans lines 0..4, method block spans lines 1..3.
        assert!(folds.iter().any(|f| f.start_line == 0 && f.end_line == 4));
        assert!(folds.iter().any(|f| f.start_line == 1 && f.end_line == 3));
    }

    #[test]
    fn folding_groups_imports() {
        let src = "import a.B;\nimport c.D;\nimport e.F;\nclass A {}\n";
        let folds = folding_ranges(&parse_str(src));
        let imports = folds
            .iter()
            .find(|f| f.kind == Some(FoldingRangeKind::Imports))
            .expect("import fold");
        assert_eq!((imports.start_line, imports.end_line), (0, 2));
    }

    /// Apply a change spanning the first occurrence of `find` and assert the
    /// incrementally-reparsed tree is identical to a from-scratch parse.
    fn assert_incremental_matches_full(
        original: &str,
        find: &str,
        replacement: &str,
        expected: &str,
    ) {
        let mut parser = new_parser();
        let tree = parse(&mut parser, original, None).unwrap();
        let index = LineIndex::new(original, PositionEncoding::Utf16);
        let at = original.find(find).unwrap();
        let range = Range {
            start: index.position(at),
            end: index.position(at + find.len()),
        };
        let applied = apply_content_change(original, PositionEncoding::Utf16, range, replacement);
        assert_eq!(applied.new_text, expected);

        let mut edited = tree.clone();
        edited.edit(&applied.input_edit);
        let incremental = parse(&mut parser, &applied.new_text, Some(&edited)).unwrap();
        let full = parse(&mut parser, &applied.new_text, None).unwrap();
        assert_eq!(
            incremental.root_node().to_sexp(),
            full.root_node().to_sexp(),
            "incremental reparse diverged from full reparse"
        );
    }

    #[test]
    fn incremental_same_line_edit_matches_full_reparse() {
        assert_incremental_matches_full(
            "class A { int x = 1; }\n",
            "1",
            "42",
            "class A { int x = 42; }\n",
        );
    }

    #[test]
    fn incremental_multiline_insert_matches_full_reparse() {
        // Insert a new method (with newlines) after the opening brace.
        assert_incremental_matches_full(
            "class A {}\n",
            "{}",
            "{\n  void m() {}\n}",
            "class A {\n  void m() {}\n}\n",
        );
    }

    #[test]
    fn selection_range_widens_from_identifier_to_file() {
        let src = "class A { int field; }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = Position {
            line: 0,
            character: src.find("field").unwrap() as u32,
        };
        let inner = &selection_ranges(&tree, &index, &[at])[0];
        // Innermost range is the `field` identifier; ranges only widen outward.
        assert_eq!(inner.range.start.character, 14);
        assert_eq!(inner.range.end.character, 19);
        let outer = inner.parent.as_ref().expect("has an enclosing range");
        assert!(outer.range.start.character <= inner.range.start.character);
        assert!(outer.range.end.character >= inner.range.end.character);
    }

    /// Decode delta-encoded semantic tokens back to absolute (char, len, type)
    /// on a single line (so char == byte for ASCII).
    fn decode_line0(data: &[SemanticToken]) -> Vec<(u32, u32, u32)> {
        let mut out = Vec::new();
        let mut ch = 0u32;
        for t in data {
            assert_eq!(t.delta_line, 0, "test source is single-line");
            ch += t.delta_start;
            out.push((ch, t.length, t.token_type));
        }
        out
    }

    #[test]
    fn semantic_tokens_classify_identifier_roles() {
        let src = "class A { MyType field; void m(int p) {} }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let decoded = decode_line0(&semantic_tokens(&tree, src, &index));
        // Map the highlighted text -> token type.
        let labeled: Vec<(&str, u32)> = decoded
            .iter()
            .map(|&(ch, len, tt)| (&src[ch as usize..(ch + len) as usize], tt))
            .collect();
        assert!(labeled.contains(&("A", TT_TYPE)), "class name: {labeled:?}");
        assert!(
            labeled.contains(&("MyType", TT_TYPE)),
            "type use: {labeled:?}"
        );
        assert!(
            labeled.contains(&("field", TT_PROPERTY)),
            "field: {labeled:?}"
        );
        assert!(labeled.contains(&("m", TT_METHOD)), "method: {labeled:?}");
        // Parameters classify as variables — see `TT_PARAMETER`'s doc.
        assert!(labeled.contains(&("p", TT_VARIABLE)), "param: {labeled:?}");
        // `int` is a primitive (left to TextMate), not emitted as a type token.
        assert!(
            !labeled.contains(&("int", TT_TYPE)),
            "primitive: {labeled:?}"
        );
    }

    #[test]
    fn semantic_tokens_highlight_usages_and_packages() {
        // A param usage, a field usage, and an import path on one line each.
        let src = "package com.demo;\nclass A { int field; void m(int p) { field = p; } }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let data = semantic_tokens(&tree, src, &index);

        // Decode to (line, char, len, type) and label by source slice.
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(src.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let (mut line, mut ch) = (0u32, 0u32);
        let mut labeled: Vec<(&str, u32)> = Vec::new();
        for t in &data {
            if t.delta_line != 0 {
                line += t.delta_line;
                ch = t.delta_start;
            } else {
                ch += t.delta_start;
            }
            let start = line_starts[line as usize] + ch as usize;
            labeled.push((&src[start..start + t.length as usize], t.token_type));
        }

        // Package segments are namespaces; the `p` param and `field` are lit at
        // their *usage* sites in `field = p;`, not just their declarations.
        assert!(
            labeled.contains(&("com", TT_NAMESPACE)),
            "package: {labeled:?}"
        );
        assert!(
            labeled.contains(&("demo", TT_NAMESPACE)),
            "package: {labeled:?}"
        );
        assert!(
            labeled.contains(&("p", TT_VARIABLE)),
            "param usage: {labeled:?}"
        );
        assert!(
            labeled.contains(&("field", TT_PROPERTY)),
            "field usage: {labeled:?}"
        );
    }

    /// Decode delta-encoded tokens across multiple lines, labeling each by its
    /// source slice.
    fn decode_labeled<'s>(src: &'s str, data: &[SemanticToken]) -> Vec<(&'s str, u32)> {
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(src.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let (mut line, mut ch) = (0u32, 0u32);
        let mut out = Vec::new();
        for t in data {
            if t.delta_line != 0 {
                line += t.delta_line;
                ch = t.delta_start;
            } else {
                ch += t.delta_start;
            }
            let start = line_starts[line as usize] + ch as usize;
            out.push((&src[start..start + t.length as usize], t.token_type));
        }
        out
    }

    /// Java 21 pattern bindings (`case Type name`, record deconstruction) get a
    /// variable token at their declaration *and* every use in the guard/body —
    /// otherwise they fall to TextMate's default (white) foreground.
    #[test]
    fn semantic_tokens_light_up_pattern_bindings() {
        let src = "class C {\n\
                   Object m(Object f) {\n\
                   return switch (f) {\n\
                   case String s when !s.isEmpty() -> s;\n\
                   case Point(int x, int y) -> x + y;\n\
                   default -> null;\n\
                   };\n\
                   }\n\
                   }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let labeled = decode_labeled(src, &semantic_tokens(&tree, src, &index));
        // `s`: bound in the label, used in the guard and the body.
        assert_eq!(
            labeled
                .iter()
                .filter(|&&(t, tt)| t == "s" && tt == TT_VARIABLE)
                .count(),
            3,
            "type-pattern binding + its two uses: {labeled:?}"
        );
        // Record-deconstruction components and their uses.
        assert_eq!(
            labeled
                .iter()
                .filter(|&&(t, tt)| t == "x" && tt == TT_VARIABLE)
                .count(),
            2,
            "record component `x` decl + use: {labeled:?}"
        );
        assert!(
            labeled.contains(&("y", TT_VARIABLE)),
            "record component `y`: {labeled:?}"
        );
        // The pattern *type* is still a type token, never a variable.
        assert!(
            labeled.contains(&("String", TT_TYPE)),
            "pattern type: {labeled:?}"
        );
    }

    /// The method name after `::` in a method reference is a method token
    /// (`Map.Entry::getKey`, `String::valueOf`), not left white.
    #[test]
    fn semantic_tokens_method_reference_name_is_a_method() {
        let src =
            "class C { void m() { use(String::valueOf); use(java.util.Map.Entry::getKey); } }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let labeled = decode_labeled(src, &semantic_tokens(&tree, src, &index));
        assert!(
            labeled.contains(&("valueOf", TT_METHOD)),
            "method-ref name: {labeled:?}"
        );
        assert!(
            labeled.contains(&("getKey", TT_METHOD)),
            "qualified method-ref name: {labeled:?}"
        );
        // The receiver type is still a type token, not the method color.
        assert!(
            labeled.contains(&("String", TT_TYPE)),
            "method-ref type: {labeled:?}"
        );
    }

    /// An undeclared type-cased receiver (`Math.abs()`) is a type
    /// token, never left for TextMate's variable color; SCREAMING_CASE and
    /// unknown lowercase names stay unclassified.
    #[test]
    fn semantic_tokens_static_receiver_is_a_type() {
        let src = "class A { void m() { Math.abs(1); use(MAX_LIMIT); } }\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let decoded = decode_line0(&semantic_tokens(&tree, src, &index));
        let labeled: Vec<(&str, u32)> = decoded
            .iter()
            .map(|&(ch, len, tt)| (&src[ch as usize..(ch + len) as usize], tt))
            .collect();
        assert!(
            labeled.contains(&("Math", TT_TYPE)),
            "static receiver: {labeled:?}"
        );
        assert!(labeled.contains(&("abs", TT_METHOD)), "{labeled:?}");
        assert!(
            !labeled.iter().any(|(text, _)| *text == "MAX_LIMIT"),
            "constants left to TextMate: {labeled:?}"
        );
    }

    /// Every segment of an import path is a type token (user
    /// directive: the whole path colors like the class); a static import's
    /// lowercase member segment is a method token; package headers keep
    /// namespace tokens.
    #[test]
    fn semantic_tokens_import_paths_are_type_colored() {
        let src = "import java.util.List;\nimport static java.lang.Math.abs;\n";
        let tree = parse_str(src);
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let data = semantic_tokens(&tree, src, &index);
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(src.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let (mut line, mut ch) = (0u32, 0u32);
        let mut labeled: Vec<(&str, u32)> = Vec::new();
        for t in &data {
            if t.delta_line != 0 {
                line += t.delta_line;
                ch = t.delta_start;
            } else {
                ch += t.delta_start;
            }
            let start = line_starts[line as usize] + ch as usize;
            labeled.push((&src[start..start + t.length as usize], t.token_type));
        }
        for segment in ["java", "util", "List", "lang", "Math"] {
            assert!(
                labeled.contains(&(segment, TT_TYPE)),
                "{segment}: {labeled:?}"
            );
        }
        assert!(
            labeled.contains(&("abs", TT_METHOD)),
            "static member: {labeled:?}"
        );
    }
}
