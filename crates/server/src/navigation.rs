//! Resolves `jvl_syntax::Definition`s into LSP `Location`s for the
//! definition/type-definition handlers, and hosts the bounded, two-tier
//! scan machinery shared by find-references, rename, call hierarchy, and
//! go-to-implementation.

use std::collections::HashMap;
use std::ops::Range as StdRange;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jvl_syntax::tree_sitter::Tree;
use jvl_syntax::{Definition, LineIndex};
use tower_lsp_server::ls_types::*;

use crate::backend::{byte_range_to_lsp, simple_name, Backend, Document};
use crate::project_symbols::{CombinedSymbols, ProjectSymbols};
use crate::references;
use crate::{open_doc_path, open_docs_and_uris, ClasspathSymbols};

/// Snapshot of everything a two-tier reference/rename scan needs, taken
/// before the slow workspace scan so the documents lock is never held
/// across a directory walk. Shared by `references()` and `rename()`.
pub(crate) struct TargetSnapshot {
    pub(crate) target: jvl_syntax::ReferenceTarget,
    pub(crate) target_uri: String,
    pub(crate) target_text: String,
    pub(crate) target_tree: Tree,
    /// Declaring document's LSP version, used for `rename`'s versioned
    /// edit. Always open, since resolution only targets open documents.
    pub(crate) target_version: i32,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    /// Open documents' live text/tree/version, keyed by canonicalized path
    /// (raw path if canonicalization fails). Lets a Tier-2 hit that's open
    /// use live text instead of stale disk content.
    pub(crate) open_snapshot: HashMap<PathBuf, (i32, String, Tree)>,
}

/// Snapshot for `goto_implementation`'s bounded scan, the
/// `textDocument/implementation` analogue of [`TargetSnapshot`]. No
/// `target_version`: implementation results are never versioned edits.
pub(crate) struct ImplTargetSnapshot {
    pub(crate) target: jvl_syntax::ImplementationTarget,
    pub(crate) target_uri: String,
    pub(crate) target_text: String,
    pub(crate) target_tree: Tree,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) open_snapshot: HashMap<PathBuf, (i32, String, Tree)>,
}

/// One scanned file's confirmed implementor/override ranges — the
/// implementation analogue of [`ScanHit`], without a version field.
pub(crate) struct ImplScanHit {
    pub(crate) uri: Uri,
    pub(crate) text: Arc<String>,
    pub(crate) ranges: Vec<StdRange<usize>>,
}

/// Aggregate result of the bounded `textDocument/implementation` scan.
pub(crate) struct ImplScanOutcome {
    pub(crate) hits: Vec<ImplScanHit>,
    /// Whether the workspace prefilter hit its file/byte cap
    /// (`references::MAX_FILES_SCANNED`/`MAX_BYTES_SCANNED`).
    pub(crate) truncated: bool,
}

/// One scanned file's confirmed occurrences of the target. `references()`
/// turns these into `Location`s; `rename()` turns them into edits.
pub(crate) struct ScanHit {
    pub(crate) uri: Uri,
    pub(crate) text: Arc<String>,
    /// `Some` when this file is a currently-open document (its LSP version,
    /// for a versioned rename edit); `None` for an on-disk/unopened file.
    pub(crate) version: Option<i32>,
    pub(crate) ranges: Vec<StdRange<usize>>,
}

/// Aggregate result of the two-tier reference scan shared by
/// `textDocument/references` and `textDocument/rename`.
pub(crate) struct ScanOutcome {
    pub(crate) hits: Vec<ScanHit>,
    /// Unconfirmed textual matches across every scanned file. `references`
    /// reports these as best-effort; `rename` must refuse if nonzero.
    pub(crate) possible: usize,
    /// Whether the Tier-2 workspace prefilter hit its file/byte cap.
    pub(crate) truncated: bool,
    /// Tier::Workspace only: prefiltered candidate files that couldn't be
    /// read/parsed. `references` skips these; `rename` must refuse if nonzero.
    pub(crate) unparsed_hit_files: usize,
}

/// One workspace-prefilter candidate file, resolved to loadable content or
/// an explicit reason it can't be. Shared loading step for
/// `scan_references`'s Tier 2 and `scan_implementations`, before each runs
/// its own semantic confirmation.
enum CandidateLoad {
    /// Canonicalizes to the target file itself, already scanned directly
    /// (`own_hits`) — skipped here without counting as unavailable.
    IsTargetFile,
    /// Read and parsed, either from the open-document snapshot or from disk.
    Loaded {
        text: Arc<String>,
        tree: Tree,
        /// `Some` when this candidate is itself a currently-open document
        /// (its LSP version, for `rename`'s versioned edits); `None` when
        /// read from disk via `parsed_project_file`.
        version: Option<i32>,
    },
    /// Could not be read or parsed — unreadable, unparseable, or too large.
    /// `scan_references` counts this to force `rename` to refuse;
    /// `scan_implementations` just skips it best-effort.
    Unavailable,
}

impl Backend {
    /// Resolve a `jvl_syntax::Definition` into an LSP `Location`. `docs`/`uris`
    /// are the same snapshot the original definition/type-definition call used.
    pub(crate) fn resolve_location(
        &self,
        def: Definition,
        docs: &HashMap<String, Document>,
        uris: &[&str],
    ) -> Option<Location> {
        match def {
            Definition::InOpenDoc {
                doc, name_range, ..
            } => {
                let uri_str = *uris.get(doc)?;
                let target = docs.get(uri_str)?;
                let index = LineIndex::new(&target.text, self.encoding());
                Some(Location {
                    uri: uri_str.parse().ok()?,
                    range: byte_range_to_lsp(&index, name_range),
                })
            }
            Definition::ProjectType { simple_name, fqn } => {
                let fqn = fqn?;
                for path in self.candidate_paths(docs, &fqn) {
                    if let Some((range, text)) = self.locate_in_project_file(&path, &simple_name) {
                        let index = LineIndex::new(&text, self.encoding());
                        return Some(Location {
                            uri: Uri::from_file_path(&path)?,
                            range: byte_range_to_lsp(&index, range),
                        });
                    }
                }
                None
            }
            Definition::External { fqn, member } => {
                let text = self.external_source_text(&fqn)?;
                let simple = simple_name(&fqn);
                let byte_range = jvl_syntax::locate_in_source(&text, simple, member.as_deref());
                let index = LineIndex::new(&text, self.encoding());
                let range = match byte_range {
                    Some(r) => byte_range_to_lsp(&index, r),
                    // Couldn't locate the member/type in the source or stub —
                    // point somewhere in the virtual document rather than
                    // failing outright.
                    None => Range::default(),
                };
                Some(Location {
                    uri: jvl_src_uri(&fqn)?,
                    range,
                })
            }
        }
    }

    /// Resolve one workspace-prefilter hit to a [`CandidateLoad`].
    /// `target_path`/`target_canon` are the scan's target file, precomputed
    /// once by the caller rather than re-derived per candidate.
    fn load_scan_candidate(
        &self,
        hit_path: &Path,
        target_path: Option<&Path>,
        target_canon: Option<&Path>,
        open_snapshot: &HashMap<PathBuf, (i32, String, Tree)>,
    ) -> CandidateLoad {
        let hit_canon = std::fs::canonicalize(hit_path).ok();
        let is_target_file = target_path == Some(hit_path)
            || (hit_canon.is_some() && hit_canon.as_deref() == target_canon);
        if is_target_file {
            return CandidateLoad::IsTargetFile;
        }

        if let Some((version, text, tree)) = hit_canon.as_ref().and_then(|c| open_snapshot.get(c)) {
            return CandidateLoad::Loaded {
                text: Arc::new(text.clone()),
                tree: tree.clone(),
                version: Some(*version),
            };
        }

        match self.parsed_project_file(hit_path) {
            Some((text, tree)) => CandidateLoad::Loaded {
                text,
                tree,
                version: None,
            },
            None => CandidateLoad::Unavailable,
        }
    }

    /// Resolve the cursor in the open `uri` to a `jvl_syntax::ReferenceTarget`
    /// and snapshot everything the two-tier scan needs. Returns `None` for an
    /// external/JDK symbol, `this`/`super`, a non-identifier, or a closed doc.
    pub(crate) async fn target_snapshot(
        &self,
        uri: &str,
        position: Position,
    ) -> Option<TargetSnapshot> {
        self.ensure_workspace_index().await;
        let docs = self.documents.lock().await;
        let (open, uris) = open_docs_and_uris(&docs, uri)?;
        let index = LineIndex::new(open[0].source, self.encoding());
        let symbols = CombinedSymbols(
            ProjectSymbols::new(self, &docs),
            ClasspathSymbols(self.classpath()),
        );
        let target = jvl_syntax::reference_target(&open, 0, &index, position, &symbols)?;

        let target_uri = uris[target.doc].to_string();
        let target_doc = docs.get(&target_uri)?;
        let target_text = target_doc.text.clone();
        let target_tree = target_doc.tree.clone();
        let target_version = target_doc.version;

        let project_root = self.project_root();
        let roots = project_root
            .as_deref()
            .map(|root| self.source_roots(&docs, root))
            .unwrap_or_default();

        let mut open_snapshot = HashMap::new();
        for (doc_uri, doc) in docs.iter() {
            if let Some(path) = open_doc_path(doc_uri) {
                let key = std::fs::canonicalize(&path).unwrap_or(path);
                open_snapshot.insert(key, (doc.version, doc.text.clone(), doc.tree.clone()));
            }
        }

        Some(TargetSnapshot {
            target,
            target_uri,
            target_text,
            target_tree,
            target_version,
            roots,
            project_root,
            open_snapshot,
        })
    }

    /// Run the bounded, two-tier reference scan for `target`, the shared
    /// orchestration `references()`/`rename()` both build on. Never holds
    /// the documents lock: everything it reads is already snapshotted.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn scan_references(
        &self,
        target: &jvl_syntax::ReferenceTarget,
        target_uri: &str,
        target_text: &str,
        target_tree: &Tree,
        target_version: i32,
        roots: &[PathBuf],
        project_root: Option<&Path>,
        open_snapshot: &HashMap<PathBuf, (i32, String, Tree)>,
        include_declaration: bool,
    ) -> ScanOutcome {
        self.ensure_workspace_index().await;
        // Held only to back `symbols`'s open-buffer overlay borrow, so it
        // stays locked for the whole scan. Nothing else here ever locks
        // `self.documents`, so this can't deadlock.
        let docs = self.documents.lock().await;
        let symbols = CombinedSymbols(
            ProjectSymbols::new(self, &docs),
            ClasspathSymbols(self.classpath()),
        );
        let empty = || ScanOutcome {
            hits: Vec::new(),
            possible: 0,
            truncated: false,
            unparsed_hit_files: 0,
        };
        let Some(target_uri_parsed) = target_uri.parse::<Uri>().ok() else {
            return empty();
        };

        let mut hits = Vec::new();

        // The declaring file's own occurrences are always in scope: Tier 1
        // stops here; Tier 2 also always checks it directly, regardless of
        // source roots.
        let self_target = jvl_syntax::ReferenceTarget {
            doc: 0,
            ..target.clone()
        };
        let target_open = jvl_syntax::OpenDoc {
            source: target_text,
            tree: target_tree,
        };
        let own_hits = jvl_syntax::references_in_doc(
            std::slice::from_ref(&target_open),
            0,
            &self_target,
            include_declaration,
            &symbols,
        );
        // Unconfirmed textual hits (e.g. unresolved receiver type) are
        // conservatively omitted; their count is aggregated so `references`
        // can warn the list may be incomplete and `rename` can refuse.
        let mut possible = own_hits.possible;
        let mut truncated = false;
        let mut unparsed_hit_files = 0usize;
        if !own_hits.ranges.is_empty() {
            hits.push(ScanHit {
                uri: target_uri_parsed.clone(),
                text: Arc::new(target_text.to_string()),
                version: Some(target_version),
                ranges: own_hits.ranges,
            });
        }

        if target.tier == jvl_syntax::Tier::Workspace {
            let scan = references::prefilter(roots, project_root, &target.name).await;

            let target_path = open_doc_path(target_uri);
            let target_canon = target_path
                .as_ref()
                .and_then(|p| std::fs::canonicalize(p).ok());

            for hit_path in scan.files {
                let (hit_text, hit_tree, version) = match self.load_scan_candidate(
                    &hit_path,
                    target_path.as_deref(),
                    target_canon.as_deref(),
                    open_snapshot,
                ) {
                    CandidateLoad::IsTargetFile => continue, // already handled above
                    CandidateLoad::Unavailable => {
                        unparsed_hit_files += 1;
                        tokio::task::yield_now().await;
                        continue;
                    }
                    CandidateLoad::Loaded {
                        text,
                        tree,
                        version,
                    } => (text, tree, version),
                };

                let docs_for_scan = [
                    jvl_syntax::OpenDoc {
                        source: &hit_text,
                        tree: &hit_tree,
                    },
                    jvl_syntax::OpenDoc {
                        source: target_text,
                        tree: target_tree,
                    },
                ];
                let remapped = jvl_syntax::ReferenceTarget {
                    doc: 1,
                    ..target.clone()
                };
                let scan_hits = jvl_syntax::references_in_doc(
                    &docs_for_scan,
                    0,
                    &remapped,
                    include_declaration,
                    &symbols,
                );
                possible += scan_hits.possible;
                if !scan_hits.ranges.is_empty() {
                    if let Some(hit_uri) = Uri::from_file_path(&hit_path) {
                        hits.push(ScanHit {
                            uri: hit_uri,
                            text: hit_text,
                            version,
                            ranges: scan_hits.ranges,
                        });
                    }
                }

                tokio::task::yield_now().await;
            }

            truncated = scan.truncated;
        }

        ScanOutcome {
            hits,
            possible,
            truncated,
            unparsed_hit_files,
        }
    }

    /// Resolve the cursor to a `jvl_syntax::ImplementationTarget` and
    /// snapshot everything the bounded scan needs — the
    /// `implementation_target` mirror of `target_snapshot`. Returns `None`
    /// for an external/JDK symbol, a local/param/field, `this`/`super`, a
    /// non-identifier, or a closed doc.
    pub(crate) async fn implementation_target_snapshot(
        &self,
        uri: &str,
        position: Position,
    ) -> Option<ImplTargetSnapshot> {
        self.ensure_workspace_index().await;
        let docs = self.documents.lock().await;
        let (open, uris) = open_docs_and_uris(&docs, uri)?;
        let index = LineIndex::new(open[0].source, self.encoding());
        let symbols = CombinedSymbols(
            ProjectSymbols::new(self, &docs),
            ClasspathSymbols(self.classpath()),
        );
        let target = jvl_syntax::implementation_target(&open, 0, &index, position, &symbols)?;

        let target_uri = uris[target.type_doc].to_string();
        let target_doc = docs.get(&target_uri)?;
        let target_text = target_doc.text.clone();
        let target_tree = target_doc.tree.clone();

        let project_root = self.project_root();
        let roots = project_root
            .as_deref()
            .map(|root| self.source_roots(&docs, root))
            .unwrap_or_default();

        let mut open_snapshot = HashMap::new();
        for (doc_uri, doc) in docs.iter() {
            if let Some(path) = open_doc_path(doc_uri) {
                let key = std::fs::canonicalize(&path).unwrap_or(path);
                open_snapshot.insert(key, (doc.version, doc.text.clone(), doc.tree.clone()));
            }
        }

        Some(ImplTargetSnapshot {
            target,
            target_uri,
            target_text,
            target_tree,
            roots,
            project_root,
            open_snapshot,
        })
    }

    /// The bounded, single-tier scan behind `textDocument/implementation`.
    /// Reuses `references::prefilter` on the target type's simple name, then
    /// confirms candidates with `implementations_in_doc` — read-only and
    /// best-effort, with no visibility tier or refusal bookkeeping.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn scan_implementations(
        &self,
        target: &jvl_syntax::ImplementationTarget,
        target_uri: &str,
        target_text: &str,
        target_tree: &Tree,
        roots: &[PathBuf],
        project_root: Option<&Path>,
        open_snapshot: &HashMap<PathBuf, (i32, String, Tree)>,
    ) -> ImplScanOutcome {
        let mut hits = Vec::new();

        // The declaring file may itself contain an implementor (or, for a
        // method query, the override) — always checked directly.
        let target_open = jvl_syntax::OpenDoc {
            source: target_text,
            tree: target_tree,
        };
        let self_target = jvl_syntax::ImplementationTarget {
            type_doc: 0,
            ..target.clone()
        };
        let own_hits =
            jvl_syntax::implementations_in_doc(std::slice::from_ref(&target_open), 0, &self_target);
        if !own_hits.is_empty() {
            if let Ok(target_uri_parsed) = target_uri.parse::<Uri>() {
                hits.push(ImplScanHit {
                    uri: target_uri_parsed,
                    text: Arc::new(target_text.to_string()),
                    ranges: own_hits.into_iter().map(|h| h.name_range).collect(),
                });
            }
        }

        let scan = references::prefilter(roots, project_root, &target.type_name).await;
        let target_path = open_doc_path(target_uri);
        let target_canon = target_path
            .as_ref()
            .and_then(|p| std::fs::canonicalize(p).ok());

        for hit_path in scan.files {
            let (hit_text, hit_tree) = match self.load_scan_candidate(
                &hit_path,
                target_path.as_deref(),
                target_canon.as_deref(),
                open_snapshot,
            ) {
                CandidateLoad::IsTargetFile => continue, // already handled above
                CandidateLoad::Unavailable => {
                    tokio::task::yield_now().await;
                    continue;
                }
                CandidateLoad::Loaded { text, tree, .. } => (text, tree),
            };

            let docs_for_scan = [
                jvl_syntax::OpenDoc {
                    source: &hit_text,
                    tree: &hit_tree,
                },
                jvl_syntax::OpenDoc {
                    source: target_text,
                    tree: target_tree,
                },
            ];
            let remapped = jvl_syntax::ImplementationTarget {
                type_doc: 1,
                ..target.clone()
            };
            let scan_hits = jvl_syntax::implementations_in_doc(&docs_for_scan, 0, &remapped);
            if !scan_hits.is_empty() {
                if let Some(hit_uri) = Uri::from_file_path(&hit_path) {
                    hits.push(ImplScanHit {
                        uri: hit_uri,
                        text: hit_text,
                        ranges: scan_hits.into_iter().map(|h| h.name_range).collect(),
                    });
                }
            }

            tokio::task::yield_now().await;
        }

        ImplScanOutcome {
            hits,
            truncated: scan.truncated,
        }
    }
}

/// The `jvl-src:` virtual-document URI for an external (JDK/dependency) FQN.
fn jvl_src_uri(fqn: &str) -> Option<Uri> {
    format!("jvl-src:/{fqn}.java").parse().ok()
}

/// The bounded-prefilter cap-truncation notice, shared verbatim by
/// `references()` and `goto_implementation()` when the file cap is hit.
pub(crate) fn truncation_notice() -> String {
    format!(
        "References search truncated at {} files; results may be incomplete.",
        references::MAX_FILES_SCANNED
    )
}
