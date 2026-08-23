//! Navigation, references, and implementations: resolving a
//! `jvl_syntax::Definition` into an LSP `Location` (the definition/type-
//! definition handlers' shared helper), and the bounded, two-tier scan
//! machinery shared by find-references, rename, call hierarchy, and
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

/// Everything a two-tier reference/rename scan needs, snapshotted from the
/// documents lock before the (possibly slow, Tier-2-only) workspace scan —
/// mirrors `workspace_index::ensure_built`'s own pattern of never holding the
/// lock across a directory walk. Shared by `references()` and
/// `rename()`/`prepare_rename()`, which all resolve the cursor to the
/// same `jvl_syntax::ReferenceTarget` first.
pub(crate) struct TargetSnapshot {
    pub(crate) target: jvl_syntax::ReferenceTarget,
    pub(crate) target_uri: String,
    pub(crate) target_text: String,
    pub(crate) target_tree: Tree,
    /// The declaring document's own LSP version, for `rename`'s versioned
    /// `TextDocumentEdit` (this document is always open — resolution only
    /// ever targets an open document).
    pub(crate) target_version: i32,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    /// Currently-open documents' live text/tree/version, keyed by
    /// canonicalized path (falling back to the raw path when
    /// canonicalization fails) — a Tier-2 hit file that is itself open is
    /// read from here instead of disk, so unsaved edits are reflected.
    pub(crate) open_snapshot: HashMap<PathBuf, (i32, String, Tree)>,
}

/// Everything `goto_implementation`'s bounded scan needs,
/// snapshotted from the documents lock before the (possibly slow)
/// workspace prefilter — the `textDocument/implementation` analogue of
/// [`TargetSnapshot`] (references/rename). No `target_version` (unlike
/// `TargetSnapshot`): go-to-implementation only ever produces `Location`s,
/// never a versioned edit.
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
/// `textDocument/implementation` analogue of [`ScanHit`], minus the open-doc
/// `version` (never needed for a read-only `Location` result).
pub(crate) struct ImplScanHit {
    pub(crate) uri: Uri,
    pub(crate) text: Arc<String>,
    pub(crate) ranges: Vec<StdRange<usize>>,
}

/// Aggregate result of the bounded `textDocument/implementation` scan.
pub(crate) struct ImplScanOutcome {
    pub(crate) hits: Vec<ImplScanHit>,
    /// Whether the workspace prefilter hit its file/byte cap (the same
    /// `references::MAX_FILES_SCANNED`/`MAX_BYTES_SCANNED` caps — this
    /// feature reuses that prefilter outright, not a new one).
    pub(crate) truncated: bool,
}

/// One scanned file's confirmed occurrences of the target — the shared unit
/// `references()` turns into `Location`s and `rename()` turns into a
/// `TextDocumentEdit`.
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
    /// Textual matches that could not be confirmed by resolution (aggregate
    /// across every scanned file) — `references` reports these as a
    /// best-effort notice; `rename` must refuse outright (any one of them
    /// could be a real, unconfirmed occurrence).
    pub(crate) possible: usize,
    /// Whether the Tier-2 workspace prefilter hit its file/byte cap.
    pub(crate) truncated: bool,
    /// Tier::Workspace only: how many prefiltered candidate files could not
    /// be read/parsed at all. `references` silently skips these (best
    /// effort); `rename` must refuse whenever this is nonzero, since an
    /// occurrence could be hiding in one of them.
    pub(crate) unparsed_hit_files: usize,
}

/// One workspace-prefilter candidate file, resolved to either loadable
/// content or an explicit reason it can't be. This is the per-candidate
/// portion `scan_references`'s Tier 2 and `scan_implementations` perform
/// identically, before each runs its own (different) semantic confirmation:
/// skip the candidate that's actually the already-handled target file,
/// prefer a currently-open document's live text/tree over disk, and
/// otherwise fall back to the parse-on-demand project-file cache
/// (`Backend::parsed_project_file`).
enum CandidateLoad {
    /// `hit_path` canonicalizes to the same file as the scan's target — the
    /// caller already scanned it directly (`own_hits`), so this candidate is
    /// skipped here without being counted as unavailable.
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
    /// Could not be read or parsed at all — unreadable, unparseable, or (per
    /// `parsed_project_file`'s size guard) too large to read safely. Every
    /// occurrence potentially hiding in this file is unaccounted for: a
    /// caller that needs conservative completeness counts this
    /// (`scan_references`'s `unparsed_hit_files`, which forces `rename` to
    /// refuse); a read-only caller (`scan_implementations`) just skips it
    /// best-effort.
    Unavailable,
}

impl Backend {
    /// Resolve a `jvl_syntax::Definition` into an LSP `Location`, dispatching
    /// on which ladder step produced it. `docs`/`uris` are the same
    /// documents-lock-held snapshot the `jvl_syntax::definition`/
    /// `type_definition` call was made against.
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
                    // The member/type couldn't be located inside the source or
                    // stub (e.g. an overload set quirk) — still point *somewhere*
                    // inside the virtual document rather than failing outright.
                    None => Range::default(),
                };
                Some(Location {
                    uri: jvl_src_uri(&fqn)?,
                    range,
                })
            }
        }
    }

    /// Resolve one workspace-prefilter hit to a [`CandidateLoad`] — see its
    /// doc comment for the three outcomes. `target_path`/`target_canon` are
    /// the scan's own target file, precomputed once by the caller (not
    /// re-derived per candidate).
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

    /// Resolve the cursor (in the currently open `uri`) to a
    /// `jvl_syntax::ReferenceTarget` and snapshot everything the two-tier
    /// scan needs — `None` for the same reasons
    /// `jvl_syntax::reference_target` itself returns `None` (an external/JDK
    /// symbol, `this`/`super`, a non-identifier, or a document that isn't
    /// currently open).
    pub(crate) async fn target_snapshot(
        &self,
        uri: &str,
        position: Position,
    ) -> Option<TargetSnapshot> {
        self.ensure_workspace_index().await;
        let docs = self.documents.lock().await;
        let (open, uris) = open_docs_and_uris(&docs, uri)?;
        let index = LineIndex::new(open[0].source, self.encoding());
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
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

    /// Run the bounded, two-tier reference scan for `target` (see
    /// `references()`'s doc comment for the tier semantics) — the shared
    /// orchestration `references()`/`rename()` both build on. Never holds
    /// the documents lock (everything it reads is already snapshotted).
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
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
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

        // The declaring file's own occurrences are always in scope — Tier 1
        // stops here entirely; Tier 2 also always checks it directly
        // (self-references), whether or not it happens to lie under a
        // discovered source root.
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
        // Textual hits that could not be confirmed by resolution (receiver
        // type unresolved — e.g. an unknown supertype) are conservatively
        // OMITTED from the results; aggregate their count across every
        // scanned file so the caller can be told the list may be incomplete
        // (`references`) or must refuse outright (`rename`).
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

    /// Resolve the cursor to a `jvl_syntax::ImplementationTarget`
    /// and snapshot everything the bounded scan needs — mirrors
    /// `target_snapshot`, over `jvl_syntax::implementation_target` instead
    /// of `reference_target`. `None` for the same reasons that returns
    /// `None`: an external/JDK symbol, a local/param/field, `this`/`super`,
    /// a non-identifier, or a document that isn't currently open.
    pub(crate) async fn implementation_target_snapshot(
        &self,
        uri: &str,
        position: Position,
    ) -> Option<ImplTargetSnapshot> {
        self.ensure_workspace_index().await;
        let docs = self.documents.lock().await;
        let (open, uris) = open_docs_and_uris(&docs, uri)?;
        let index = LineIndex::new(open[0].source, self.encoding());
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
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

    /// The bounded, single-tier scan behind
    /// `textDocument/implementation` — reuses the references prefilter
    /// (`references::prefilter`) with the target type's simple name as
    /// needle (same caps, cancellation, source-root confinement as
    /// `scan_references`'s Tier 2), then confirms each candidate file with
    /// `jvl_syntax::implementations_in_doc`. Unlike `scan_references`, there
    /// is no visibility tier (an interface/class name is always workspace-
    /// reaching) and no "possible"/unparsed-hit-file refusal bookkeeping —
    /// this is a read-only, best-effort query, not `rename`'s all-or-nothing
    /// one.
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

        // The target's own declaring file may itself contain an implementor
        // (or, for a method-level query, the overriding declaration) — always
        // checked directly, same as `scan_references`'s `own_hits`.
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

/// The bounded-prefilter cap-truncation notice text — shared verbatim by
/// `references()` and `goto_implementation()`, which both run
/// the same `references::prefilter` and must tell the user the same thing
/// when it hits its file cap.
pub(crate) fn truncation_notice() -> String {
    format!(
        "References search truncated at {} files; results may be incomplete.",
        references::MAX_FILES_SCANNED
    )
}
