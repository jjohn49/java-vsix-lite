//! Call and type hierarchy support: fetching the text/tree behind a
//! hierarchy item's file, and wrapping a located callable/type into the
//! LSP `CallHierarchyItem`/`TypeHierarchyItem` shapes.

use std::sync::Arc;

use jvl_syntax::tree_sitter::Tree;
use jvl_syntax::LineIndex;
use tower_lsp_server::ls_types::*;

use crate::backend::{byte_range_to_lsp, Backend};
use crate::open_doc_path;

impl Backend {
    /// The text + tree behind a hierarchy item's file — the open
    /// document if there is one, else the on-disk file through
    /// `parsed_project_file`'s cache (hierarchy items can point at files the
    /// user never opened, e.g. a supertype found through the workspace
    /// index).
    pub(crate) async fn hierarchy_doc(&self, uri: &str) -> Option<(Arc<String>, Tree)> {
        {
            let docs = self.documents.lock().await;
            if let Some(d) = docs.get(uri) {
                return Some((Arc::new(d.text.clone()), d.tree.clone()));
            }
        }
        let path = open_doc_path(uri)?;
        self.parsed_project_file(&path)
    }
}

/// Convert a byte range (from `jvl-syntax`) into an LSP `Range` via `index`.
/// Wrap a located callable into a `CallHierarchyItem`.
pub(crate) fn callable_item(
    info: &jvl_syntax::CallableInfo,
    uri: Uri,
    index: &LineIndex,
) -> CallHierarchyItem {
    CallHierarchyItem {
        name: info.name.clone(),
        kind: match info.kind {
            jvl_syntax::CallableKind::Method => SymbolKind::METHOD,
            jvl_syntax::CallableKind::Constructor => SymbolKind::CONSTRUCTOR,
            jvl_syntax::CallableKind::Type => SymbolKind::CLASS,
        },
        tags: None,
        detail: info.detail.clone(),
        uri,
        range: byte_range_to_lsp(index, info.decl_range.clone()),
        selection_range: byte_range_to_lsp(index, info.name_range.clone()),
        data: None,
    }
}

/// Wrap a located type into a `TypeHierarchyItem`.
pub(crate) fn type_item(
    info: &jvl_syntax::TypeInfo,
    uri: Uri,
    index: &LineIndex,
) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: info.name.clone(),
        kind: match info.kind {
            jvl_syntax::TypeInfoKind::Class => SymbolKind::CLASS,
            jvl_syntax::TypeInfoKind::Interface => SymbolKind::INTERFACE,
            jvl_syntax::TypeInfoKind::Enum => SymbolKind::ENUM,
            jvl_syntax::TypeInfoKind::Record => SymbolKind::STRUCT,
            jvl_syntax::TypeInfoKind::Annotation => SymbolKind::INTERFACE,
        },
        tags: None,
        detail: None,
        uri,
        range: byte_range_to_lsp(index, info.decl_range.clone()),
        selection_range: byte_range_to_lsp(index, info.name_range.clone()),
        data: None,
    }
}
