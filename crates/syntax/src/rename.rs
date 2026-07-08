//! M4.4: rename — conservative, refuse rather than corrupt.
//!
//! This module supplies the syntax-level primitives the server's
//! `prepareRename`/`rename` handlers build on; the server owns the
//! orchestration (scanning, WorkspaceEdit assembly, the file-rename
//! resource op), same division of labor as M4.3's find-references (see
//! `references.rs`'s module doc).
//!
//! - [`prepare_rename`] answers `textDocument/prepareRename`: `Some` only
//!   when the cursor is on an identifier that [`crate::reference_target`]
//!   resolves to an in-project declaration — which already refuses external
//!   (JDK/dependency) symbols, keywords, literals, and `this`/`super` for
//!   free (`identifier_at` only matches `identifier`/`type_identifier` node
//!   kinds; keywords/literals are their own distinct tree-sitter node kinds
//!   and never reach that far).
//! - [`is_valid_new_name`] gates the *new* name: a syntactically valid Java
//!   identifier that isn't a reserved word or literal.
//! - [`collides_with_existing`] is the cheap, conservative same-scope
//!   collision guard: does the declaring scope already bind the new name to
//!   a sibling of the same kind (namespace-aware for members, via the same
//!   substrate `references.rs` uses)?
//! - [`is_public_top_level_type`] answers the file-rename question: is this
//!   target a `public` type declared directly at a compilation unit's top
//!   level (as opposed to nested, package-private, or a member/local)?

use std::ops::Range;

use ls_types::Position;
use tree_sitter::Node;

use crate::completion::KEYWORDS;
use crate::external::SymbolSource;
use crate::hover::identifier_at;
use crate::model::{has_modifier, named_children, MemberKind, TypeDecl, TypeTable};
use crate::references::{reference_target, ReferenceTarget};
use crate::resolve;
use crate::{node_text, LineIndex, OpenDoc};

/// The range of the identifier under the cursor (in the *requesting*
/// document — not necessarily where the symbol is declared) plus its
/// current text as the rename placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareRename {
    pub range: Range<usize>,
    pub placeholder: String,
}

/// Resolve the cursor to a renameable target: `Some` only when it lands on
/// an identifier [`crate::reference_target`] resolves to an in-project
/// declaration. Everything that must be refused — an external (JDK/
/// dependency) symbol, a keyword, a literal, `this`/`super`, or a
/// non-identifier — already yields `None` from `reference_target` itself
/// (or, for `this`/`super`, is filtered here since `identifier_at` also
/// matches those two node kinds, which `reference_target`'s own `classify`
/// step does not consider renameable in the first place).
pub fn prepare_rename(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    symbols: &dyn SymbolSource,
) -> Option<PrepareRename> {
    let doc = docs.get(current)?;
    let cursor = index.offset(pos);
    let name_node = identifier_at(doc.tree, cursor)?;
    if !matches!(name_node.kind(), "identifier" | "type_identifier") {
        return None; // `this`/`super` are not renameable
    }
    // Confirms the cursor resolves to an in-project declaration — refuses
    // external (JDK/dependency) symbols for free, the same way
    // `textDocument/references` does.
    reference_target(docs, current, index, pos, symbols)?;
    Some(PrepareRename {
        range: name_node.byte_range(),
        placeholder: node_text(name_node, doc.source).to_string(),
    })
}

/// Whether `name` is a syntactically valid Java identifier (starts with a
/// letter/`_`/`$`, continues with letters/digits/`_`/`$`) and not a reserved
/// word or literal (`class`, `true`, …).
pub fn is_valid_new_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    !KEYWORDS.contains(&name)
}

/// Cheap, conservative same-scope collision guard: does the scope that
/// declares `target` already bind `new_name` to a sibling of the same kind —
/// another local/parameter in the same method, a field/method of the
/// enclosing type (namespace-aware — a field named `new_name` doesn't
/// collide with a method rename, and vice versa), or a sibling type
/// (nested-in-the-same-enclosing-type, or top-level-in-the-same-file)?
///
/// Deliberately cheap rather than exhaustive: locals/params are checked
/// against whatever [`resolve::collect_bindings`] already reports in scope
/// *before* the declaration point (the same substrate `lookup_binding`
/// uses) — a same-named sibling declared *later* in the identical block is
/// not caught. Members and types (own_members/sibling scan) have no such
/// direction bias since Java's own scoping doesn't care about declaration
/// order there.
pub fn collides_with_existing(docs: &[OpenDoc], target: &ReferenceTarget, new_name: &str) -> bool {
    let Some(doc) = docs.get(target.doc) else {
        return false;
    };
    let decl_node = resolve::node_at(doc.tree, target.name_range.start);
    let Some(parent) = decl_node.parent() else {
        return false;
    };

    match parent.kind() {
        "formal_parameter"
        | "spread_parameter"
        | "inferred_parameters"
        | "lambda_expression"
        | "enhanced_for_statement" => local_binding_collision(docs, doc, target, new_name),
        "variable_declarator" => {
            let grandparent = parent.parent().map(|g| g.kind());
            if matches!(
                grandparent,
                Some("local_variable_declaration") | Some("resource") | Some("for_statement")
            ) {
                local_binding_collision(docs, doc, target, new_name)
            } else {
                // `field_declaration` / `constant_declaration` / a record
                // component's `formal_parameter` grandparent.
                member_collision(doc, target, decl_node, new_name, MemberNamespace::Field)
            }
        }
        "method_declaration" => {
            member_collision(doc, target, decl_node, new_name, MemberNamespace::Method)
        }
        "enum_constant" => {
            member_collision(doc, target, decl_node, new_name, MemberNamespace::Field)
        }
        k if resolve::is_type_decl(k) => type_sibling_collision(doc, target, decl_node, new_name),
        _ => false,
    }
}

/// Bindings (locals/params/for-vars — never fields, which may legally be
/// shadowed) already in scope at the declaration's own start byte.
fn local_binding_collision(
    docs: &[OpenDoc],
    doc: &OpenDoc,
    target: &ReferenceTarget,
    new_name: &str,
) -> bool {
    let table = TypeTable::build(docs, target.doc);
    resolve::collect_bindings(
        doc.tree,
        doc.source,
        target.name_range.start,
        &table,
        false,
        target.doc,
    )
    .iter()
    .any(|b| b.name == new_name)
}

fn kind_matches_namespace(kind: MemberKind, ns: MemberNamespace) -> bool {
    match ns {
        MemberNamespace::Method => matches!(kind, MemberKind::Method),
        MemberNamespace::Field => matches!(kind, MemberKind::Field | MemberKind::EnumConstant),
    }
}

/// Which Java member namespace to check for a collision — mirrors
/// `resolve::MemberNamespace` (fields/methods live in separate namespaces,
/// JLS §6.5), duplicated here rather than reused since that type is
/// `pub(crate)` to `resolve.rs`'s own confirm-by-resolution machinery and
/// this check needs no receiver/hierarchy resolution at all — just the
/// enclosing type's own declared members.
#[derive(Clone, Copy)]
enum MemberNamespace {
    Method,
    Field,
}

/// Does the enclosing type already declare another member named `new_name`
/// in the same namespace as the one being renamed?
fn member_collision(
    doc: &OpenDoc,
    target: &ReferenceTarget,
    decl_node: Node,
    new_name: &str,
    ns: MemberNamespace,
) -> bool {
    let Some(td) = resolve::enclosing_typedecl(decl_node, doc.source, target.doc) else {
        return false;
    };
    td.own_members()
        .into_iter()
        .any(|m| m.name == new_name && kind_matches_namespace(m.kind, ns))
}

/// Does a sibling type already use `new_name` — another nested type of the
/// same *outer* enclosing type, or (for a top-level type) another top-level
/// type declared in the same file?
fn type_sibling_collision(
    doc: &OpenDoc,
    target: &ReferenceTarget,
    decl_node: Node,
    new_name: &str,
) -> bool {
    let type_node = match decl_node.parent() {
        Some(p) => p,
        None => return false,
    };
    let outer = type_node.parent().and_then(resolve::enclosing_type_node);
    match outer {
        Some(outer_node) => {
            let Some(outer_td) = TypeDecl::from_node(outer_node, doc.source, target.doc) else {
                return false;
            };
            outer_td
                .own_members()
                .into_iter()
                .any(|m| m.name == new_name && matches!(m.kind, MemberKind::NestedType(_)))
        }
        None => named_children(doc.tree.root_node())
            .into_iter()
            .filter_map(|c| TypeDecl::from_node(c, doc.source, target.doc))
            .any(|td| td.name == new_name && td.name != target.name),
    }
}

/// Whether `target` is a `public` type declared directly at its
/// compilation unit's top level (no enclosing type) — the condition under
/// which the server's rename handler also emits a `RenameFile` resource op
/// (when the file's name matches the type's old name and the client
/// supports it).
pub fn is_public_top_level_type(docs: &[OpenDoc], target: &ReferenceTarget) -> bool {
    let Some(doc) = docs.get(target.doc) else {
        return false;
    };
    let decl_node = resolve::node_at(doc.tree, target.name_range.start);
    let Some(parent) = decl_node.parent() else {
        return false;
    };
    if !resolve::is_type_decl(parent.kind()) {
        return false;
    }
    if parent
        .parent()
        .and_then(resolve::enclosing_type_node)
        .is_some()
    {
        return false; // nested — not top-level
    }
    has_modifier(parent, doc.source, "public")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::NoSymbols;
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn prepare_at(docs: &[OpenDoc], current: usize, marker: &str) -> Option<PrepareRename> {
        let src = docs[current].source;
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present");
        prepare_rename(docs, current, &index, index.position(at), &NoSymbols)
    }

    #[test]
    fn is_valid_new_name_accepts_ordinary_identifier() {
        assert!(is_valid_new_name("total"));
        assert!(is_valid_new_name("_x1"));
        assert!(is_valid_new_name("$hidden"));
    }

    #[test]
    fn is_valid_new_name_rejects_leading_digit() {
        assert!(!is_valid_new_name("123abc"));
    }

    #[test]
    fn is_valid_new_name_rejects_reserved_word() {
        assert!(!is_valid_new_name("class"));
        assert!(!is_valid_new_name("true"));
    }

    #[test]
    fn is_valid_new_name_rejects_empty_and_invalid_chars() {
        assert!(!is_valid_new_name(""));
        assert!(!is_valid_new_name("a-b"));
        assert!(!is_valid_new_name("a b"));
    }

    /// M4.4: `prepareRename` on a local variable resolves to the identifier's
    /// own range at the cursor, with the current name as the placeholder.
    #[test]
    fn prepare_rename_on_local_var_returns_its_own_range_and_placeholder() {
        let src = "class C { void m() { int count = 0; count++; } }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let prep = prepare_at(&docs, 0, "count++").expect("prepare rename resolved");
        let at = src.find("count++").unwrap();
        assert_eq!(prep.range, at..at + "count".len());
        assert_eq!(prep.placeholder, "count");
    }

    /// M4.4: `prepareRename` on an external (JDK) symbol is refused.
    #[test]
    fn prepare_rename_on_external_symbol_is_refused() {
        let src = "import java.util.List;\nclass C { List l; }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        assert!(prepare_at(&docs, 0, "List l").is_none());
    }

    /// M4.4: `prepareRename` on a keyword or a literal is refused.
    #[test]
    fn prepare_rename_on_keyword_or_literal_is_refused() {
        let src = "class C { void m() { boolean b = true; } }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        // Cursor on `boolean` (a keyword/primitive type token, not an
        // identifier at all).
        assert!(prepare_at(&docs, 0, "boolean b").is_none());
        // Cursor on `true` (a literal).
        assert!(prepare_at(&docs, 0, "true;").is_none());
    }

    /// M4.4: a local variable collides with another local of the same name
    /// already bound in the same enclosing scope.
    #[test]
    fn local_var_collides_with_existing_sibling_local() {
        let src = "class C { void m() { int b = 0; int a = 1; print(a); } }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("a = 1").unwrap();
        let target = reference_target(&docs, 0, &index, index.position(at), &NoSymbols)
            .expect("target resolved");
        assert!(collides_with_existing(&docs, &target, "b"));
        assert!(!collides_with_existing(&docs, &target, "c"));
    }

    /// M4.4: a field collides with another field of the same name on the
    /// same type, but not with a same-named method (separate namespaces).
    #[test]
    fn field_collides_with_sibling_field_not_method() {
        let src = "class C { int a; int b; void c() {} }\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find("a;").unwrap();
        let target = reference_target(&docs, 0, &index, index.position(at), &NoSymbols)
            .expect("target resolved");
        assert!(collides_with_existing(&docs, &target, "b"));
        assert!(
            !collides_with_existing(&docs, &target, "c"),
            "field/method are separate namespaces"
        );
        assert!(!collides_with_existing(&docs, &target, "d"));
    }

    /// M4.4: a `public` top-level type is recognized as such; a nested type
    /// and a package-private top-level type are not.
    #[test]
    fn is_public_top_level_type_distinguishes_visibility_and_nesting() {
        let src = "public class Outer { class Inner {} }\nclass PackagePrivate {}\n";
        let t = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &t,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);

        let outer_at = src.find("Outer {").unwrap();
        let outer_target = reference_target(&docs, 0, &index, index.position(outer_at), &NoSymbols)
            .expect("outer target resolved");
        assert!(is_public_top_level_type(&docs, &outer_target));

        let inner_at = src.find("Inner {}").unwrap();
        let inner_target = reference_target(&docs, 0, &index, index.position(inner_at), &NoSymbols)
            .expect("inner target resolved");
        assert!(!is_public_top_level_type(&docs, &inner_target));

        let pp_at = src.find("PackagePrivate {}").unwrap();
        let pp_target = reference_target(&docs, 0, &index, index.position(pp_at), &NoSymbols)
            .expect("package-private target resolved");
        assert!(!is_public_top_level_type(&docs, &pp_target));
    }
}
