//! M8a: code actions — the "Organize Imports" source action and add-import
//! quick fixes for unresolved type names.
//!
//! Both are pure text-edit producers over the open document; the server wraps
//! each [`ActionSketch`] in an LSP `CodeAction` carrying the document's URI
//! (this crate never sees URIs, matching the rest of the analysis API).

use std::collections::HashSet;

use ls_types::{Position, Range, TextEdit};

use crate::completion::ImportInsertion;
use crate::external::SymbolSource;
use crate::imports::Imports;
use crate::model::{named_children, TypeTable};
use crate::resolve::{self, Ctx};
use crate::{node_text, LineIndex, OpenDoc};

/// LSP `CodeActionKind` strings this module produces. The server advertises
/// exactly these in its capabilities and filters against the client's
/// `context.only` request.
pub const KIND_QUICKFIX: &str = "quickfix";
pub const KIND_ORGANIZE_IMPORTS: &str = "source.organizeImports";

/// Candidate scan width for the add-import fix: wide enough that the exact
/// simple-name matches survive the shared prefix index's cap.
const ADD_IMPORT_SCAN_LIMIT: usize = 500;

/// At most this many "Import '…'" fixes per identifier — past a handful the
/// lightbulb menu is noise, not help.
const MAX_ADD_IMPORT_ACTIONS: usize = 8;

/// One code action: a title, its LSP kind string, and the text edits to apply
/// to the requesting document.
pub struct ActionSketch {
    pub title: String,
    pub kind: &'static str,
    pub edits: Vec<TextEdit>,
    /// Marks the action the client may apply via "auto fix" — set only when
    /// it is unambiguous (a lone import candidate).
    pub is_preferred: bool,
}

/// All code actions available for `range` in the current document: add-import
/// quick fixes for the identifier under the range start, plus "Organize
/// imports" whenever the import block isn't already in organized form.
pub fn code_actions(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    range: Range,
    symbols: &dyn SymbolSource,
) -> Vec<ActionSketch> {
    let Some(doc) = docs.get(current) else {
        return Vec::new();
    };
    let table = TypeTable::build(docs, current);
    let imports = Imports::parse(doc.tree, doc.source);
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
    };

    let mut out = add_import_actions(&ctx, index, range.start);
    // M8d: extract variable/constant + source-generate actions.
    out.extend(crate::generate::refactor_actions(doc, index, range));
    if let Some(edit) = organize_imports_edit(doc, index) {
        out.push(ActionSketch {
            title: "Organize imports".to_string(),
            kind: KIND_ORGANIZE_IMPORTS,
            edits: vec![edit],
            is_preferred: false,
        });
    }
    out
}

// --- Organize imports ---

struct ImportDecl {
    is_static: bool,
    path: String,
    start: usize,
    end: usize,
}

impl ImportDecl {
    /// Rendered organized form.
    fn line(&self) -> String {
        if self.is_static {
            format!("import static {};", self.path)
        } else {
            format!("import {};", self.path)
        }
    }
}

/// The single edit that rewrites the import block into organized form —
/// unused single imports dropped, duplicates removed, sorted (static block
/// first, then non-static, each ASCII order) — or `None` when the block is
/// already organized (or can't be rewritten safely).
///
/// Deliberately conservative in two ways:
/// - "used" means the imported name occurs *anywhere* in the file outside the
///   import block — including comments, Javadoc `{@link}`s, and strings — so
///   a doc-only reference never loses its import (slight under-removal, never
///   over-removal). Wildcard imports are always kept: whether any of their
///   types are used is unknowable without full resolution.
/// - if anything other than whitespace sits *between* import declarations
///   (a comment, say), no edit is offered at all rather than one that would
///   silently delete it.
fn organize_imports_edit(doc: &OpenDoc, index: &LineIndex) -> Option<TextEdit> {
    let mut decls: Vec<ImportDecl> = Vec::new();
    for child in named_children(doc.tree.root_node()) {
        if child.kind() != "import_declaration" {
            continue;
        }
        let decl = parse_import(
            node_text(child, doc.source),
            child.start_byte(),
            child.end_byte(),
        )?;
        decls.push(decl);
    }
    if decls.is_empty() {
        return None;
    }

    let region_start = decls.first()?.start;
    let region_end = decls.last()?.end;
    // Safety check: the region must contain nothing but the imports and
    // whitespace, or the rewrite would destroy it.
    let mut cursor = region_start;
    for d in &decls {
        if doc.source[cursor..d.start]
            .chars()
            .any(|c| !c.is_whitespace())
        {
            return None;
        }
        cursor = d.end;
    }

    // Everything outside the import block, for the word-occurrence check.
    let outside = format!(
        "{}{}",
        &doc.source[..region_start],
        &doc.source[region_end..]
    );

    let mut kept: Vec<&ImportDecl> = Vec::new();
    let mut seen: HashSet<(bool, &str)> = HashSet::new();
    for d in &decls {
        if !seen.insert((d.is_static, &d.path)) {
            continue; // duplicate
        }
        if d.path.ends_with(".*") {
            kept.push(d); // wildcard: usage unknowable, always kept
            continue;
        }
        let simple = d.path.rsplit('.').next().unwrap_or(&d.path);
        if contains_word(&outside, simple) {
            kept.push(d);
        }
    }

    let mut statics: Vec<String> = kept
        .iter()
        .filter(|d| d.is_static)
        .map(|d| d.line())
        .collect();
    let mut plain: Vec<String> = kept
        .iter()
        .filter(|d| !d.is_static)
        .map(|d| d.line())
        .collect();
    statics.sort();
    plain.sort();

    let mut blocks: Vec<String> = Vec::new();
    if !statics.is_empty() {
        blocks.push(statics.join("\n"));
    }
    if !plain.is_empty() {
        blocks.push(plain.join("\n"));
    }
    let new_text = blocks.join("\n\n");

    if new_text == doc.source[region_start..region_end] {
        return None; // already organized
    }
    Some(TextEdit {
        range: Range {
            start: index.position(region_start),
            end: index.position(region_end),
        },
        new_text,
    })
}

/// Parse one `import …;` declaration's text into its static flag and
/// whitespace-collapsed dotted path (`import java . util. List ;` is legal
/// Java and must normalize to `java.util.List`).
fn parse_import(text: &str, start: usize, end: usize) -> Option<ImportDecl> {
    let mut rest = text.trim().strip_prefix("import")?.trim_start();
    let is_static = rest
        .strip_prefix("static")
        .is_some_and(|r| r.starts_with(|c: char| c.is_whitespace()));
    if is_static {
        rest = rest["static".len()..].trim_start();
    }
    let path: String = rest
        .trim_end()
        .trim_end_matches(';')
        .split_whitespace()
        .collect();
    (!path.is_empty()).then_some(ImportDecl {
        is_static,
        path,
        start,
        end,
    })
}

/// Whether `word` occurs in `hay` with non-identifier characters (or the
/// text's edges) on both sides. `pub(crate)`: `generate.rs` uses the same
/// check for its name-collision scans.
pub(crate) fn contains_word(hay: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let bytes = hay.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut from = 0;
    while let Some(rel) = hay[from..].find(word) {
        let at = from + rel;
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after = at + word.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

// --- Add-import quick fix ---

/// "Import 'a.b.C'" fixes for the type-cased identifier at `pos`, when the
/// name doesn't already resolve (open documents, imports, same package,
/// wildcards, `java.lang`) and the symbol index knows types with exactly that
/// simple name.
fn add_import_actions(ctx: &Ctx, index: &LineIndex, pos: Position) -> Vec<ActionSketch> {
    let offset = index.offset(pos);
    let Some(node) = identifier_at(ctx.doc, offset) else {
        return Vec::new();
    };
    let word = node_text(node, ctx.doc.source);
    if !crate::looks_like_type_name(word) {
        return Vec::new();
    }
    // Not inside an import/package declaration, and only the *head* of a
    // qualified name (in `util.List` the `List` segment is package-relative,
    // not a simple name an import could bind).
    let mut anc = node;
    while let Some(parent) = anc.parent() {
        if matches!(parent.kind(), "import_declaration" | "package_declaration") {
            return Vec::new();
        }
        anc = parent;
    }
    if let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "scoped_type_identifier" | "scoped_identifier" | "field_access"
        ) && parent.named_child(0).map(|c| c.id()) != Some(node.id())
        {
            return Vec::new();
        }
    }
    // Already resolvable → nothing to fix. A same-named single import that
    // *doesn't* resolve means the import itself is broken (missing
    // dependency); adding a second `import` of the same simple name would be
    // invalid Java, so offer nothing there either.
    if ctx.table.get(word).is_some()
        || ctx.imports.single_import(word).is_some()
        || resolve::resolve_simple_to_fqn(word, ctx).is_some()
    {
        return Vec::new();
    }

    let (candidates, _) = ctx.symbols.types_with_prefix(word, ADD_IMPORT_SCAN_LIMIT);
    let insertion = ImportInsertion::compute(ctx.doc, index);
    let mut seen = HashSet::new();
    let matches: Vec<_> = candidates
        .into_iter()
        .filter(|c| c.simple == word && seen.insert(c.import_path.clone()))
        .take(MAX_ADD_IMPORT_ACTIONS)
        .collect();
    let lone = matches.len() == 1;
    matches
        .into_iter()
        .map(|c| ActionSketch {
            title: format!("Import '{}'", c.import_path),
            kind: KIND_QUICKFIX,
            edits: vec![insertion.edit(&c.import_path)],
            is_preferred: lone,
        })
        .collect()
}

/// The identifier/type-identifier node covering `offset` — retried one byte
/// left so a cursor sitting just past the word's last character still hits.
fn identifier_at<'t>(doc: &OpenDoc<'t>, offset: usize) -> Option<tree_sitter::Node<'t>> {
    let root = doc.tree.root_node();
    let at = |o: usize| {
        root.named_descendant_for_byte_range(o, o)
            .filter(|n| matches!(n.kind(), "identifier" | "type_identifier"))
    };
    at(offset).or_else(|| at(offset.checked_sub(1)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::TypeCandidate;
    use crate::{new_parser, PositionEncoding};

    struct IndexOnly(Vec<TypeCandidate>);

    impl SymbolSource for IndexOnly {
        fn class(&self, fqn: &str) -> Option<crate::ExternalClass> {
            // Types the index knows exist as classes too (so
            // `resolve_simple_to_fqn` can confirm an imported name).
            self.0
                .iter()
                .any(|c| c.fqn == fqn)
                .then(|| crate::ExternalClass {
                    supers: Vec::new(),
                    type_params: Vec::new(),
                    members: Vec::new(),
                })
        }
        fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<TypeCandidate>, bool) {
            let hits: Vec<TypeCandidate> = self
                .0
                .iter()
                .filter(|c| c.simple.starts_with(prefix))
                .cloned()
                .collect();
            let over = hits.len() > limit;
            (hits.into_iter().take(limit).collect(), over)
        }
    }

    fn cand(simple: &str, fqn: &str) -> TypeCandidate {
        TypeCandidate {
            simple: simple.to_string(),
            fqn: fqn.to_string(),
            import_path: fqn.replace('$', "."),
        }
    }

    fn actions_at(src: &str, marker: &str, symbols: &dyn SymbolSource) -> Vec<ActionSketch> {
        let mut parser = new_parser();
        let tree = crate::parse(&mut parser, src, None).expect("parse");
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present") + marker.len();
        let pos = index.position(at);
        code_actions(
            &docs,
            0,
            &index,
            Range {
                start: pos,
                end: pos,
            },
            symbols,
        )
    }

    fn apply(src: &str, edit: &TextEdit) -> String {
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let start = index.offset(edit.range.start);
        let end = index.offset(edit.range.end);
        format!("{}{}{}", &src[..start], edit.new_text, &src[end..])
    }

    fn organize(src: &str) -> Option<String> {
        let symbols = IndexOnly(Vec::new());
        let sketches = actions_at(src, "class", &symbols);
        let sketch = sketches
            .into_iter()
            .find(|s| s.kind == KIND_ORGANIZE_IMPORTS)?;
        Some(apply(src, &sketch.edits[0]))
    }

    #[test]
    fn organize_sorts_dedupes_and_drops_unused() {
        let src = "package demo;\n\n\
                   import java.util.Map;\n\
                   import java.util.List;\n\
                   import java.util.List;\n\
                   import java.io.File;\n\n\
                   class C { List<String> l; File f; }\n";
        let organized = organize(src).expect("an organize edit");
        // `Map` unused → dropped; duplicate `List` removed; rest sorted.
        assert_eq!(
            organized,
            "package demo;\n\n\
             import java.io.File;\nimport java.util.List;\n\n\
             class C { List<String> l; File f; }\n"
        );
    }

    #[test]
    fn organize_puts_static_imports_first_and_keeps_wildcards() {
        let src = "import java.util.List;\n\
                   import static java.lang.Math.max;\n\
                   import java.util.*;\n\n\
                   class C { List<Integer> l = null; int m = max(1, 2); }\n";
        let organized = organize(src).expect("an organize edit");
        assert_eq!(
            organized,
            "import static java.lang.Math.max;\n\n\
             import java.util.*;\nimport java.util.List;\n\n\
             class C { List<Integer> l = null; int m = max(1, 2); }\n"
        );
    }

    #[test]
    fn organize_keeps_imports_referenced_only_in_javadoc() {
        let src = "import java.util.Map;\n\n\
                   /** See {@link Map}. */\nclass C { }\n";
        // `Map` appears only in a comment — conservatively "used", and the
        // block is otherwise already organized, so no action at all.
        assert!(organize(src).is_none());
    }

    #[test]
    fn organize_offers_nothing_when_already_organized() {
        let src = "import java.io.File;\nimport java.util.List;\n\n\
                   class C { List<File> l; }\n";
        assert!(organize(src).is_none());
    }

    #[test]
    fn organize_bails_on_comments_between_imports() {
        let src = "import java.util.Map;\n// pinned comment\nimport java.util.List;\n\n\
                   class C { List<Object> l; Map<Object, Object> m; }\n";
        assert!(organize(src).is_none());
    }

    #[test]
    fn add_import_offered_for_unresolved_type_name() {
        let symbols = IndexOnly(vec![cand("ArrayList", "java.util.ArrayList")]);
        let src = "package demo;\n\nclass C { void m() { ArrayList l; } }\n";
        let sketches = actions_at(src, "ArrayLis", &symbols);
        let fix = sketches
            .iter()
            .find(|s| s.kind == KIND_QUICKFIX)
            .expect("an import fix");
        assert_eq!(fix.title, "Import 'java.util.ArrayList'");
        assert!(fix.is_preferred, "a lone candidate is preferred");
        let fixed = apply(src, &fix.edits[0]);
        assert!(
            fixed.contains("package demo;\n\nimport java.util.ArrayList;"),
            "{fixed}"
        );
    }

    #[test]
    fn add_import_offers_every_exact_match_but_no_prefix_matches() {
        let symbols = IndexOnly(vec![
            cand("List", "java.util.List"),
            cand("List", "java.awt.List"),
            cand("ListModel", "javax.swing.ListModel"),
        ]);
        let src = "class C { void m() { List l; } }\n";
        let sketches = actions_at(src, "Lis", &symbols);
        let titles: Vec<&str> = sketches
            .iter()
            .filter(|s| s.kind == KIND_QUICKFIX)
            .map(|s| s.title.as_str())
            .collect();
        assert_eq!(
            titles,
            vec!["Import 'java.util.List'", "Import 'java.awt.List'"]
        );
        // Ambiguous → neither is auto-fix preferred.
        assert!(sketches
            .iter()
            .filter(|s| s.kind == KIND_QUICKFIX)
            .all(|s| !s.is_preferred));
    }

    #[test]
    fn add_import_not_offered_when_name_already_resolves() {
        let symbols = IndexOnly(vec![cand("List", "java.util.List")]);
        // Already imported.
        let src = "import java.util.List;\n\nclass C { void m() { List l; } }\n";
        assert!(actions_at(src, "{ Lis", &symbols)
            .iter()
            .all(|s| s.kind != KIND_QUICKFIX));
        // Declared by the file itself.
        let src = "class List { void m() { List l; } }\n";
        assert!(actions_at(src, "{ Lis", &symbols)
            .iter()
            .all(|s| s.kind != KIND_QUICKFIX));
    }

    #[test]
    fn add_import_not_offered_on_lowercase_or_import_line() {
        let symbols = IndexOnly(vec![cand("List", "java.util.List")]);
        let src = "class C { void m() { int list = 0; } }\n";
        assert!(actions_at(src, "int lis", &symbols).is_empty());
        // On the (broken) import line itself: no fix.
        let src = "import java.util.NoSuch;\n\nclass C { }\n";
        assert!(actions_at(src, "NoSuc", &symbols)
            .iter()
            .all(|s| s.kind != KIND_QUICKFIX));
    }

    #[test]
    fn add_import_not_offered_when_simple_name_import_conflicts() {
        // `List` is single-imported to a type the classpath doesn't have —
        // a second `import …List;` would be invalid Java.
        let symbols = IndexOnly(vec![cand("List", "java.util.List")]);
        let src = "import broken.List;\n\nclass C { void m() { List l; } }\n";
        assert!(actions_at(src, "{ Lis", &symbols)
            .iter()
            .all(|s| s.kind != KIND_QUICKFIX));
    }
}
