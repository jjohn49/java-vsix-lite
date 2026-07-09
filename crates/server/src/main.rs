//! java-vsix-lite language server entry point.
//!
//! This is the single LSP server the editor talks to (see the implementation
//! plan's "Process topology"). The TypeScript extension shell launches this
//! binary over stdio and stays thin; all analysis lives here and in the
//! `jvl-*` crates.
//!
//! The default tier parses open Java files incrementally with tree-sitter and
//! provides syntax diagnostics, document symbols, folding/selection ranges,
//! semantic tokens, and — over in-file/open-file types — hover and completion.
//! Documents are tracked open-files-only; there is no workspace or JAR/JDK
//! indexing (that arrives with the bytecode sub-project).
//!
//! Invariant: **stdout is reserved for the LSP wire protocol.** All logging goes
//! to stderr via `tracing`.

#![forbid(unsafe_code)]

mod references;
mod workspace_index;

use std::collections::{HashMap, HashSet};
use std::ops::Range as StdRange;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime};

use jvl_syntax::tree_sitter::{Parser, Tree};
use jvl_syntax::{Definition, LineIndex, PositionEncoding};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::request::{GotoTypeDefinitionParams, GotoTypeDefinitionResponse};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

/// Bound on the step-(c) parse-on-demand cache (an unopened project source
/// file referenced by an open one) and the step-(d) external stub/source
/// cache: cleared wholesale past this many entries rather than tracking LRU —
/// both paths are rare enough (cold, one-off lookups) that eviction pressure
/// is low and a simple bound is not worth extra bookkeeping.
const EXTERNAL_CACHE_CAP: usize = 32;
const PROJECT_FILE_CACHE_CAP: usize = 32;

/// A single project source file parsed on demand for ladder step (c),
/// invalidated by `mtime` so an on-disk edit is picked up without an explicit
/// notification (the server never watches files).
struct CachedProjectFile {
    mtime: SystemTime,
    text: Arc<String>,
    tree: Tree,
}

/// A single open document: its current text and the parse tree kept in sync
/// with it.
struct Document {
    text: String,
    tree: Tree,
}

/// M5.2's debounce/coalescing decision state for classpath rebuilds
/// triggered by watched build-file changes — pure and synchronous, so it's
/// unit-tested directly with no real timers involved (see the `tests`
/// module below). The actual timing (the debounce wait, `spawn_blocking`
/// for the rebuild itself) lives in `Backend::drive_classpath_rebuild`,
/// which just calls these methods at the right points. `on_event`'s `bool`
/// return elects exactly one caller as "the driver" for however many
/// debounce-then-rebuild cycles it takes to settle, so at most one rebuild
/// ever runs at a time and no unbounded queue of pending rebuilds can build
/// up — a fresh event always folds into whichever cycle is already running.
#[derive(Default)]
struct RebuildCoalescer {
    /// Some caller already owns driving a debounce-wait-then-maybe-rebuild
    /// cycle; a later event just needs to mark `dirty` and return.
    driving: bool,
    /// A matching change happened since the driver last started waiting (or
    /// last started a rebuild) that it hasn't accounted for yet.
    dirty: bool,
    /// A rebuild is actually running right now, as opposed to still waiting
    /// out the debounce window — tracked only for clarity/assertions.
    in_flight: bool,
}

impl RebuildCoalescer {
    fn new() -> Self {
        Self::default()
    }

    /// A matching build-file change arrived. Returns `true` exactly once per
    /// debounce-then-rebuild cycle: the caller that gets `true` must drive
    /// it (wait the debounce window, call `on_debounce_elapsed`, and so on
    /// until settled); every other concurrent/later caller gets `false` —
    /// its event has already been folded into the driver's next decision.
    fn on_event(&mut self) -> bool {
        if self.driving {
            // Already being handled by the current debounce-wait-then-maybe-
            // rebuild cycle; this event just needs to be accounted for.
            self.dirty = true;
            false
        } else {
            self.driving = true;
            true
        }
    }

    /// The driver's debounce wait elapsed. `true` means proceed straight to
    /// a rebuild (nothing arrived during the wait); `false` means a fresh
    /// event reset the window and the driver must wait a full debounce
    /// window again before re-checking.
    fn on_debounce_elapsed(&mut self) -> bool {
        if self.dirty {
            self.dirty = false;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// The in-flight rebuild finished. `true` means at least one event
    /// arrived during the rebuild and exactly one follow-up debounce/rebuild
    /// cycle must run (the driver keeps going); `false` means it's fully
    /// settled and the driver may stop.
    fn on_rebuild_finished(&mut self) -> bool {
        self.in_flight = false;
        if self.dirty {
            self.dirty = false;
            true
        } else {
            self.driving = false;
            false
        }
    }
}

struct Backend {
    client: Client,
    /// Reused across parses; held only for synchronous parse calls, never across
    /// an `.await`.
    parser: StdMutex<Parser>,
    /// Open documents, keyed by URI string. Open-files-only by design — the
    /// default tier never indexes the whole workspace.
    documents: Mutex<HashMap<String, Document>>,
    /// LSP position encoding negotiated during `initialize` (defaults to UTF-16).
    encoding: OnceLock<PositionEncoding>,
    /// Whether the client supports snippet completion (`$1` tab stops). Defaults
    /// to `false` until negotiated during `initialize`.
    snippet_support: OnceLock<bool>,
    /// Bytecode-backed symbols for imported (JDK/dependency) types. Built lazily
    /// on first use so the JDK's jmods aren't scanned until completion/hover needs
    /// them. M5.2: now swappable — a watched build-file change triggers a
    /// debounced rebuild (see `RebuildCoalescer`/`drive_classpath_rebuild`)
    /// that atomically swaps in a freshly resolved `Classpath`. Every read
    /// path takes its own `Arc` snapshot via `classpath()` at the start of a
    /// request and never holds this lock across an `.await`.
    classpath: StdRwLock<Option<Arc<jvl_classpath::Classpath>>>,
    /// Whether the client supports dynamic registration of
    /// `workspace/didChangeWatchedFiles` (negotiated during `initialize`) —
    /// gates whether `initialized()` bothers registering the build-file
    /// watch at all; the LSP spec has no static alternative for this
    /// capability, so a client without it simply never gets watched.
    classpath_watch_dynamic: OnceLock<bool>,
    /// Debounce window for a classpath rebuild after a watched build-file
    /// change (`classpath_debounce_ms_opt`) — 2s by default, overridable via
    /// `initializationOptions.classpathDebounceMs` so tests aren't forced to
    /// sleep multiple seconds.
    classpath_debounce_ms: OnceLock<u64>,
    /// M5.2's debounce/coalescing decision state (see [`RebuildCoalescer`]),
    /// guarded by a plain `Mutex` — decisions are synchronous and quick,
    /// never held across an `.await`.
    classpath_rebuild: StdMutex<RebuildCoalescer>,
    /// Workspace root (from `initialize`), used to discover project dependencies.
    workspace_root: OnceLock<Option<PathBuf>>,
    /// Fallback project root derived from the first opened document (so deps
    /// resolve even when a lone file is opened with no workspace folder).
    project_root_hint: OnceLock<Option<PathBuf>>,
    /// Whether to emit unresolved-member diagnostics (default on; opt-out).
    unresolved_member_diagnostics: OnceLock<bool>,
    /// Ladder step (c): a single unopened project source file, parsed on
    /// demand and cached by path (see [`CachedProjectFile`]). Never held
    /// across an `.await`.
    project_file_cache: StdMutex<HashMap<PathBuf, CachedProjectFile>>,
    /// Ladder step (d): a JDK/dependency type's source (or, absent that, a
    /// signature-only stub rendered from `ClassInfo`) — the text served
    /// through the `jvl-src:` virtual document scheme. Keyed by FQN. Never
    /// held across an `.await`.
    external_stub_cache: StdMutex<HashMap<String, Arc<String>>>,
    /// M4.5: the lazy, bounded workspace symbol index (built on the first
    /// `workspace/symbol` request, not at startup).
    workspace_index: workspace_index::WorkspaceIndex,
    /// Whether [`workspace_index`]'s cap-truncation has already been logged
    /// to the client — logged once, not on every subsequent query.
    workspace_index_truncation_logged: std::sync::atomic::AtomicBool,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            parser: StdMutex::new(jvl_syntax::new_parser()),
            documents: Mutex::new(HashMap::new()),
            encoding: OnceLock::new(),
            snippet_support: OnceLock::new(),
            classpath: StdRwLock::new(None),
            classpath_watch_dynamic: OnceLock::new(),
            classpath_debounce_ms: OnceLock::new(),
            classpath_rebuild: StdMutex::new(RebuildCoalescer::new()),
            workspace_root: OnceLock::new(),
            project_root_hint: OnceLock::new(),
            unresolved_member_diagnostics: OnceLock::new(),
            project_file_cache: StdMutex::new(HashMap::new()),
            external_stub_cache: StdMutex::new(HashMap::new()),
            workspace_index: workspace_index::WorkspaceIndex::new(),
            workspace_index_truncation_logged: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn encoding(&self) -> PositionEncoding {
        self.encoding
            .get()
            .copied()
            .unwrap_or(PositionEncoding::Utf16)
    }

    fn snippet_support(&self) -> bool {
        self.snippet_support.get().copied().unwrap_or(false)
    }

    /// The imported-type symbol source, built on first use from the user's JDK
    /// plus the project's declared dependencies. The project root is the
    /// workspace folder, or (if none) one derived from the first opened file.
    ///
    /// Returns a snapshot `Arc`: the caller holds its own reference-counted
    /// handle to whichever `Classpath` was current the moment it asked, so a
    /// concurrent M5.2 rebuild swap never invalidates work already in
    /// flight, and this method never holds `self.classpath`'s lock across an
    /// `.await`.
    fn classpath(&self) -> Arc<jvl_classpath::Classpath> {
        if let Some(existing) = self
            .classpath
            .read()
            .expect("classpath lock poisoned")
            .clone()
        {
            return existing;
        }
        // Double-checked: the first caller (of possibly several racing here)
        // builds it; everyone else just reads back what got stored.
        let mut guard = self.classpath.write().expect("classpath lock poisoned");
        if let Some(existing) = guard.clone() {
            return existing;
        }
        let built = Arc::new(jvl_classpath::Classpath::from_jdk_and_project(
            self.project_root().as_deref(),
        ));
        *guard = Some(Arc::clone(&built));
        built
    }

    /// The debounce window for M5.2 classpath rebuilds (see
    /// `classpath_debounce_ms_opt`); 2s unless overridden.
    fn classpath_debounce(&self) -> Duration {
        Duration::from_millis(self.classpath_debounce_ms.get().copied().unwrap_or(2000))
    }

    /// Re-resolve the classpath from scratch on the blocking pool — the same
    /// static, offline resolution the initial lazy build uses (no build tool
    /// is ever invoked) — and swap it in atomically. Never holds
    /// `self.classpath`'s lock across the `.await`.
    async fn rebuild_classpath(&self) {
        let root = self.project_root();
        let built = tokio::task::spawn_blocking(move || {
            jvl_classpath::Classpath::from_jdk_and_project(root.as_deref())
        })
        .await
        .unwrap_or_else(|_| jvl_classpath::Classpath::empty());
        *self.classpath.write().expect("classpath lock poisoned") = Some(Arc::new(built));
    }

    /// Recompute and republish diagnostics for every currently open
    /// document — used after a classpath swap (M5.2), since
    /// unresolved-member diagnostics depend on it and may change once new
    /// dependency types become resolvable.
    async fn republish_all_diagnostics(&self) {
        let uris: Vec<String> = self.documents.lock().await.keys().cloned().collect();
        for uri_str in uris {
            let diagnostics = {
                let docs = self.documents.lock().await;
                self.compute_diagnostics(&docs, &uri_str)
            };
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .await;
            }
        }
    }

    /// M5.2: drive one or more debounce-then-rebuild cycles until settled.
    /// Only the single caller that won `RebuildCoalescer::on_event`'s race
    /// calls this (see `did_change_watched_files`) — every other concurrent
    /// or later matching event just folds into the cycle already running
    /// here, so at most one rebuild is ever in flight and exactly one more
    /// runs if anything changed while it was.
    async fn drive_classpath_rebuild(&self) {
        loop {
            tokio::time::sleep(self.classpath_debounce()).await;
            let proceed = {
                let mut c = self
                    .classpath_rebuild
                    .lock()
                    .expect("classpath rebuild coalescer poisoned");
                c.on_debounce_elapsed()
            };
            if !proceed {
                continue; // a fresh event arrived during the wait; wait a full window again
            }

            self.client
                .log_message(MessageType::INFO, "classpath rebuild: started")
                .await;
            let start = Instant::now();
            self.rebuild_classpath().await;
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("classpath rebuild: finished in {:?}", start.elapsed()),
                )
                .await;
            self.republish_all_diagnostics().await;

            let again = {
                let mut c = self
                    .classpath_rebuild
                    .lock()
                    .expect("classpath rebuild coalescer poisoned");
                c.on_rebuild_finished()
            };
            if !again {
                break;
            }
        }
    }

    /// The workspace folder, or (if none) the root derived from the first
    /// opened file — used both for classpath discovery and (here) as the base
    /// for ladder step (c)'s conventional source-root candidates.
    fn project_root(&self) -> Option<PathBuf> {
        self.workspace_root
            .get()
            .and_then(|r| r.clone())
            .or_else(|| self.project_root_hint.get().and_then(|r| r.clone()))
    }

    /// Candidate source roots for ladder step (c): the conventional
    /// `src/main/java` and `src/test/java` under the project root, plus one
    /// inferred from each open document's own path and `package` declaration
    /// (`file = root/a/b/C.java` + `package a.b;` ⇒ `root`). No directory
    /// walking — every root here is either a fixed convention or derived from
    /// data already in memory (open documents' parsed trees).
    fn source_roots(&self, docs: &HashMap<String, Document>, project_root: &Path) -> Vec<PathBuf> {
        let mut roots = vec![
            project_root.join("src/main/java"),
            project_root.join("src/test/java"),
        ];
        for (uri, doc) in docs {
            if let Some(root) = infer_source_root(uri, &doc.tree, &doc.text) {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        roots
    }

    /// Candidate file paths for an FQN under every discovered source root
    /// (ladder step (c)). `None` (rather than an empty list) when no project
    /// root is known at all, or when `fqn` isn't safe to turn into a path
    /// (guards against a crafted `package`/`import` escaping the root).
    fn candidate_paths(&self, docs: &HashMap<String, Document>, fqn: &str) -> Vec<PathBuf> {
        if !is_safe_fqn(fqn) {
            return Vec::new();
        }
        let Some(root) = self.project_root() else {
            return Vec::new();
        };
        let rel = format!("{}.java", fqn.replace('.', "/"));
        self.source_roots(docs, &root)
            .into_iter()
            .map(|source_root| source_root.join(&rel))
            .collect()
    }

    /// The lazy, bounded workspace symbol index (see `workspace_index`),
    /// exposed so another feature in this crate (e.g. a future add-import)
    /// can do "simple name -> paths" lookups without re-walking the
    /// workspace itself. Building/rebuilding only happens via `ensure_built`
    /// (called from the `symbol` handler); this accessor never triggers it.
    pub(crate) fn workspace_index(&self) -> &workspace_index::WorkspaceIndex {
        &self.workspace_index
    }

    /// Parse (or reuse a cached parse of) a single project source file,
    /// invalidated by `mtime` so an on-disk edit is picked up without an
    /// explicit notification (the server never watches files). Shared by
    /// ladder step (c) (`locate_in_project_file`, below) and M4.3
    /// find-references' Tier 2 per-hit-file confirm (`references()`/
    /// `references.rs`) — both are one-off, cold, parse-on-demand lookups
    /// of a file that isn't open in the editor.
    fn parsed_project_file(&self, path: &Path) -> Option<(Arc<String>, Tree)> {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;

        let mut cache = self
            .project_file_cache
            .lock()
            .expect("project file cache poisoned");
        let fresh = cache.get(path).is_some_and(|c| c.mtime == mtime);
        if !fresh {
            let text = std::fs::read_to_string(path).ok()?;
            let tree = self.parse(&text, None);
            if cache.len() >= PROJECT_FILE_CACHE_CAP {
                cache.clear();
            }
            cache.insert(
                path.to_path_buf(),
                CachedProjectFile {
                    mtime,
                    text: Arc::new(text),
                    tree,
                },
            );
        }
        let cached = cache.get(path)?;
        Some((Arc::clone(&cached.text), cached.tree.clone()))
    }

    /// Parse (or reuse a cached parse of) a single project source file for
    /// ladder step (c). At most one file is read per candidate tried, and the
    /// caller (`resolve_location`) stops at the first hit — no directory
    /// walking or indexing.
    fn locate_in_project_file(
        &self,
        path: &Path,
        simple_name: &str,
    ) -> Option<(StdRange<usize>, Arc<String>)> {
        let (text, tree) = self.parsed_project_file(path)?;
        let range = jvl_syntax::locate_type_in_source(&tree, &text, simple_name)?;
        Some((range, text))
    }

    /// The source text to serve for an external (JDK/dependency) FQN: real
    /// source when the classpath's sibling source archive has it, else a
    /// signature-only stub synthesized from `ClassInfo`. Stub text is cached
    /// per FQN (real source is already cached inside `Classpath` itself).
    fn external_source_text(&self, fqn: &str) -> Option<Arc<String>> {
        if let Some(src) = self.classpath().source(fqn) {
            return Some(src);
        }
        self.stub_text(fqn)
    }

    fn stub_text(&self, fqn: &str) -> Option<Arc<String>> {
        if let Some(hit) = self
            .external_stub_cache
            .lock()
            .expect("external stub cache poisoned")
            .get(fqn)
        {
            return Some(Arc::clone(hit));
        }
        let info = self.classpath().class(fqn)?;
        let text = Arc::new(render_stub(&info));
        let mut cache = self
            .external_stub_cache
            .lock()
            .expect("external stub cache poisoned");
        if cache.len() >= EXTERNAL_CACHE_CAP {
            cache.clear();
        }
        cache.insert(fqn.to_string(), Arc::clone(&text));
        Some(text)
    }

    /// Resolve a `jvl_syntax::Definition` into an LSP `Location`, dispatching
    /// on which ladder step produced it. `docs`/`uris` are the same
    /// documents-lock-held snapshot the `jvl_syntax::definition`/
    /// `type_definition` call was made against.
    fn resolve_location(
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

    /// Parse `text`, reusing `old` for an incremental reparse when the caller has
    /// already applied the corresponding `InputEdit`s to it.
    fn parse(&self, text: &str, old: Option<&Tree>) -> Tree {
        let mut parser = self.parser.lock().expect("parser mutex poisoned");
        jvl_syntax::parse(&mut parser, text, old).expect("parser yields a tree for in-memory text")
    }

    /// Syntax diagnostics for a document already stored under `uri`, plus
    /// unresolved-member diagnostics unless that setting has been turned off.
    fn compute_diagnostics(&self, docs: &HashMap<String, Document>, uri: &str) -> Vec<Diagnostic> {
        let Some(doc) = docs.get(uri) else {
            return Vec::new();
        };
        let index = LineIndex::new(&doc.text, self.encoding());
        let mut diagnostics = jvl_syntax::syntax_diagnostics(&doc.tree, &index);
        if self.unresolved_member_diagnostics.get().copied() == Some(true) {
            let open = open_docs(docs, uri, doc);
            let symbols = ClasspathSymbols(self.classpath());
            diagnostics.extend(jvl_syntax::member_diagnostics(&open, 0, &index, &symbols));
        }
        diagnostics
    }

    /// Parse a freshly opened (or fully replaced) document from scratch, store
    /// it, and publish its diagnostics.
    async fn open_document(&self, uri: Uri, text: String) {
        let tree = self.parse(&text, None);
        let diagnostics = {
            let mut docs = self.documents.lock().await;
            docs.insert(uri.as_str().to_string(), Document { text, tree });
            self.compute_diagnostics(&docs, uri.as_str())
        };
        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }
}

/// The workspace root as a filesystem path, from the first workspace folder
/// (falling back to the deprecated `rootUri`). Uses the URI type's own
/// percent-decoding file-path conversion rather than hand-parsing.
fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    let uri = params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .map(|folder| folder.uri.clone())
        .or_else(|| {
            #[allow(deprecated)]
            params.root_uri.clone()
        })?;
    Some(uri.to_file_path()?.into_owned())
}

/// Walk up from a document's path to the nearest ancestor containing a Maven or
/// Gradle build file, used as the project root when no workspace folder is set.
fn derive_project_root(uri: &Uri) -> Option<PathBuf> {
    const MARKERS: [&str; 4] = [
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
    ];
    let path = uri.to_file_path()?;
    let mut dir = path.parent();
    while let Some(d) = dir {
        if MARKERS.iter().any(|m| d.join(m).is_file()) {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Infer a document's source root from its own file path and its `package`
/// declaration: if the path's parent directories match the package's dotted
/// segments (walking outward from the file), the root is whatever remains
/// above them (`root/a/b/C.java` + `package a.b;` ⇒ `root`). `None` for a
/// non-`file:` URI, a document with no package declaration, or one whose path
/// doesn't actually match its package (nothing to infer).
fn infer_source_root(uri: &str, tree: &Tree, source: &str) -> Option<PathBuf> {
    let uri: Uri = uri.parse().ok()?;
    let path = uri.to_file_path()?.into_owned();
    let package = extract_package(tree, source)?;
    let mut dir = path.parent();
    for segment in package.split('.').rev() {
        let d = dir?;
        if d.file_name().and_then(|n| n.to_str()) != Some(segment) {
            return None;
        }
        dir = d.parent();
    }
    dir.map(Path::to_path_buf)
}

/// The dotted path of a document's `package` declaration, read directly off
/// its already-parsed tree (no extra parsing).
fn extract_package(tree: &Tree, source: &str) -> Option<String> {
    let mut cursor = tree.root_node().walk();
    let decl = tree
        .root_node()
        .children(&mut cursor)
        .find(|c| c.kind() == "package_declaration")?;
    let text = decl.utf8_text(source.as_bytes()).ok()?;
    let path: String = text
        .trim()
        .strip_prefix("package")?
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect();
    (!path.is_empty()).then_some(path)
}

/// An open document's URI (its `HashMap` key) as a filesystem path, or
/// `None` for a non-`file:` URI (e.g. an in-memory/untitled document) — such
/// documents simply can't shadow anything in the on-disk workspace index.
fn open_doc_path(uri: &str) -> Option<PathBuf> {
    let uri: Uri = uri.parse().ok()?;
    Some(uri.to_file_path()?.into_owned())
}

/// Whether every dotted segment of a fully-qualified name is a safe, single
/// path component — guards ladder step (c)'s file lookup against a crafted
/// `package`/`import` declaration escaping the inferred source root (e.g. a
/// `..` segment) when building a candidate path.
fn is_safe_fqn(fqn: &str) -> bool {
    !fqn.is_empty()
        && fqn
            .split('.')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.contains(['/', '\\']))
}

/// `java.util.Map$Entry` -> `Entry`: the simple name the virtual-document
/// tree-sitter lookup (`jvl_syntax::locate_in_source`) searches for.
fn simple_name(fqn: &str) -> &str {
    fqn.rsplit(['.', '$']).next().unwrap_or(fqn)
}

/// The `jvl-src:` virtual-document URI for an external (JDK/dependency) FQN.
fn jvl_src_uri(fqn: &str) -> Option<Uri> {
    format!("jvl-src:/{fqn}.java").parse().ok()
}

/// The FQN encoded in a `jvl-src:` virtual-document URI (the inverse of
/// [`jvl_src_uri`]), as sent by the extension's `jvl/externalSource` request.
fn fqn_from_jvl_src_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("jvl-src:/")?
        .strip_suffix(".java")
        .map(str::to_string)
}

/// Render a signature-only stub `.java`-shaped text from bytecode-derived
/// `ClassInfo` — used when no `-sources.jar`/`src.zip` entry exists for an
/// external type. Reuses the member signatures `jvl-classpath` already
/// rendered (via its own signature/generics helpers) rather than re-deriving
/// them; declarations have no bodies, which is ordinary Java syntax
/// (abstract methods, interface methods) and parses fine.
fn render_stub(info: &jvl_classpath::ClassInfo) -> String {
    let mut out = format!(
        "// Signature-only stub for {} (no sources available)\nclass {} {{\n",
        info.fqn,
        simple_name(&info.fqn)
    );
    for member in &info.members {
        out.push_str("    ");
        out.push_str(&member.signature);
        out.push_str(";\n");
    }
    out.push_str("}\n");
    out
}

/// Convert a byte range (from `jvl-syntax`) into an LSP `Range` via `index`.
fn byte_range_to_lsp(index: &LineIndex, range: StdRange<usize>) -> Range {
    Range {
        start: index.position(range.start),
        end: index.position(range.end),
    }
}

/// The `unresolvedMemberDiagnostics` flag from `initializationOptions`.
/// Default-on (M5.6): the diagnostic itself (`member_diagnostics`) is
/// conservative and stays silent whenever resolution is incomplete, so this
/// flag exists only for a user who wants to opt back out, not to gate an
/// otherwise-risky feature.
fn unresolved_member_diagnostics_opt(params: &InitializeParams) -> bool {
    params
        .initialization_options
        .as_ref()
        .and_then(|opts| opts.get("unresolvedMemberDiagnostics"))
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// The `classpathDebounceMs` field from `initializationOptions` — the wait
/// after the last matching build-file change before M5.2's classpath
/// rebuild runs. Defaults to 2000ms; tests override it to a few
/// milliseconds so the watched-build-file E2E round trip doesn't have to
/// sleep multiple seconds.
fn classpath_debounce_ms_opt(params: &InitializeParams) -> u64 {
    params
        .initialization_options
        .as_ref()
        .and_then(|opts| opts.get("classpathDebounceMs"))
        .and_then(|value| value.as_u64())
        .unwrap_or(2000)
}

/// Whether the client declared dynamic-registration support for
/// `workspace/didChangeWatchedFiles` — if not, M5.2's build-file watch is
/// simply never registered (graceful fallback; the LSP spec gives servers
/// no static-capability alternative for this one).
fn supports_watched_files_registration(params: &InitializeParams) -> bool {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.did_change_watched_files.as_ref())
        .and_then(|d| d.dynamic_registration)
        .unwrap_or(false)
}

/// Whether `uri` names one of M5.2's watched build files: `pom.xml`,
/// `build.gradle`, `build.gradle.kts`, or `gradle/libs.versions.toml`
/// (matched by filename, and — for the last, since the filename alone isn't
/// distinctive — its parent directory too). Checked server-side on receipt
/// as well as registered client-side, so a client that (like a test driving
/// the notification directly) sends an unrelated event never triggers a
/// rebuild.
fn is_classpath_build_file(uri: &Uri) -> bool {
    let Some(path) = uri.to_file_path() else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    match name {
        "pom.xml" | "build.gradle" | "build.gradle.kts" => true,
        "libs.versions.toml" => {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("gradle")
        }
        _ => false,
    }
}

/// Whether the client supports snippet (`$1` tab-stop) completion inserts.
fn supports_snippets(params: &InitializeParams) -> bool {
    params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|td| td.completion.as_ref())
        .and_then(|c| c.completion_item.as_ref())
        .and_then(|ci| ci.snippet_support)
        .unwrap_or(false)
}

/// Pick UTF-8 if the client advertises support (lets tree-sitter byte offsets
/// pass through unconverted); otherwise the LSP default, UTF-16.
fn negotiate_encoding(params: &InitializeParams) -> PositionEncoding {
    let supports_utf8 = params
        .capabilities
        .general
        .as_ref()
        .and_then(|general| general.position_encodings.as_ref())
        .is_some_and(|encodings| encodings.contains(&PositionEncodingKind::UTF8));
    if supports_utf8 {
        PositionEncoding::Utf8
    } else {
        PositionEncoding::Utf16
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let encoding = negotiate_encoding(&params);
        let _ = self.encoding.set(encoding);
        let _ = self.snippet_support.set(supports_snippets(&params));
        let _ = self.workspace_root.set(workspace_root(&params));
        let _ = self
            .unresolved_member_diagnostics
            .set(unresolved_member_diagnostics_opt(&params));
        let _ = self
            .classpath_watch_dynamic
            .set(supports_watched_files_registration(&params));
        let _ = self
            .classpath_debounce_ms
            .set(classpath_debounce_ms_opt(&params));
        let position_encoding = Some(match encoding {
            PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
            PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
        });

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "java-vsix-lite".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                position_encoding,
                // INCREMENTAL: ranged edits are applied to the cached tree via
                // tree-sitter `InputEdit`, so reparsing is proportional to the
                // edit, not the file size.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    // `.` requests member completion; identifier/keyword
                    // completion is requested explicitly (Ctrl-Space) or by the
                    // editor as the user types.
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    // `(` opens a call's signature help; `,` re-triggers it as
                    // the user moves to the next argument.
                    trigger_characters: Some(vec!["(".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: jvl_syntax::semantic_token_types(),
                                token_modifiers: vec![],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "java-vsix-lite server initialized")
            .await;

        // M5.2: watch build files for classpath invalidation, client
        // permitting. VS Code supports dynamic registration; a client that
        // doesn't just never gets watched — the LSP spec has no static
        // alternative for this capability.
        if self.classpath_watch_dynamic.get().copied().unwrap_or(false) {
            let watchers = [
                "**/pom.xml",
                "**/build.gradle",
                "**/build.gradle.kts",
                "**/gradle/libs.versions.toml",
            ]
            .into_iter()
            .map(|pattern| FileSystemWatcher {
                glob_pattern: GlobPattern::String(pattern.to_string()),
                kind: None,
            })
            .collect();
            let register_options = DidChangeWatchedFilesRegistrationOptions { watchers };
            let registration = Registration {
                id: "jvl-classpath-watch".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: serde_json::to_value(register_options).ok(),
            };
            if let Err(err) = self.client.register_capability(vec![registration]).await {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("failed to register build-file watch: {err}"),
                    )
                    .await;
            }
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        // Derive a fallback project root from the first file, in case no
        // workspace folder was provided. Done before open_document so the
        // classpath (built lazily there for diagnostics) can see it.
        if self.workspace_root.get().and_then(|r| r.as_ref()).is_none() {
            let _ = self
                .project_root_hint
                .set(derive_project_root(&params.text_document.uri));
        }
        self.open_document(params.text_document.uri, params.text_document.text)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let encoding = self.encoding();

        // Apply edits to the cached text + tree under the lock (all synchronous),
        // reparse, then drop the lock before the async publish.
        let diagnostics = {
            let mut docs = self.documents.lock().await;
            {
                let Some(doc) = docs.get_mut(uri.as_str()) else {
                    return; // change for a document we never opened
                };

                let mut from_scratch = false;
                for change in params.content_changes {
                    match change.range {
                        Some(range) => {
                            let applied = jvl_syntax::apply_content_change(
                                &doc.text,
                                encoding,
                                range,
                                &change.text,
                            );
                            // A full replacement earlier in the batch invalidated
                            // the tree; skip incremental edits and reparse fresh.
                            if !from_scratch {
                                doc.tree.edit(&applied.input_edit);
                            }
                            doc.text = applied.new_text;
                        }
                        None => {
                            doc.text = change.text;
                            from_scratch = true;
                        }
                    }
                }

                let old = (!from_scratch).then_some(&doc.tree);
                doc.tree = self.parse(&doc.text, old);
            }
            self.compute_diagnostics(&docs, uri.as_str())
        };

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().await.remove(uri.as_str());
        // Clear diagnostics for the closed file.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    /// M5.2: a watched build file changed. Events that don't actually name
    /// one of the watched build files (`is_classpath_build_file`) are
    /// ignored outright — no log message, no coalescer state touched — so
    /// an unrelated file's change never triggers a rebuild (verified
    /// end to end via log absence, since a test drives this notification
    /// directly rather than through a real filesystem watcher).
    ///
    /// A matching event either elects this call as the debounce/rebuild
    /// driver (`RebuildCoalescer::on_event` returning `true`, in which case
    /// it runs `drive_classpath_rebuild` to completion) or folds into
    /// whichever call already is.
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        if !params
            .changes
            .iter()
            .any(|c| is_classpath_build_file(&c.uri))
        {
            return;
        }
        let become_driver = {
            let mut c = self
                .classpath_rebuild
                .lock()
                .expect("classpath rebuild coalescer poisoned");
            c.on_event()
        };
        if become_driver {
            self.drive_classpath_rebuild().await;
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let docs = self.documents.lock().await;
        let Some(doc) = docs.get(params.text_document.uri.as_str()) else {
            return Ok(None);
        };
        // Built from the cached tree — no reparse. Sync work, no await held.
        let index = LineIndex::new(&doc.text, self.encoding());
        let symbols = jvl_syntax::document_symbols(&doc.tree, &doc.text, &index);
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    /// M4.5: `workspace/symbol` over the lazy, bounded workspace index (see
    /// `workspace_index`), built here on the first request. Query matching
    /// is case-insensitive substring or camel-hump prefix (see
    /// `workspace_index::matches_query`). Open documents are looked up live
    /// via `jvl_syntax::document_symbols` (a real parse, so more precise)
    /// and shadow whatever the index says about that same file, rather than
    /// being merged with it.
    ///
    /// Locations for entries the index found on disk (i.e. never opened)
    /// use a zero-length range at 0:0 — resolving the exact name range would
    /// require parsing the file, which is exactly what the index avoids;
    /// VS Code jumps to the top of the file, and opening it makes precise,
    /// live symbols (and later queries) reflect the real position.
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let query = params.query;

        // Compute the source roots (needs a peek at open documents, for the
        // package-inferred ones) and release the lock *before* the
        // potentially slow disk walk in `ensure_built`, so a concurrent
        // `didOpen`/`didChange` isn't blocked on it.
        let project_root = self.project_root();
        let roots = {
            let docs = self.documents.lock().await;
            project_root
                .as_deref()
                .map(|root| self.source_roots(&docs, root))
                .unwrap_or_default()
        };
        self.workspace_index()
            .ensure_built(&roots, project_root.as_deref())
            .await;
        tracing::debug!(
            entries = self.workspace_index().len(),
            generation = self.workspace_index().generation(),
            "workspace symbol index ready"
        );

        if self.workspace_index().truncated()
            && self
                .workspace_index_truncation_logged
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "workspace symbol index truncated at its entry cap; some results may be missing",
                )
                .await;
        }

        // Open documents shadow the index for the same path: their symbols
        // come live from a real parse, and their path is excluded from the
        // index's (possibly-stale, name-only) results below.
        let docs = self.documents.lock().await;
        let mut results = Vec::new();
        let mut shadowed_paths = HashSet::new();
        for (uri, doc) in docs.iter() {
            let Some(path) = open_doc_path(uri) else {
                continue;
            };
            shadowed_paths.insert(path.clone());
            let Some(uri) = Uri::from_file_path(&path) else {
                continue;
            };
            let index = LineIndex::new(&doc.text, self.encoding());
            // Only the top-level Vec entries are top-level type
            // declarations (Java allows only types at a file's root); each
            // one's own children (methods/fields/nested types) are
            // intentionally not flattened in here, matching the on-disk
            // index's top-level-types-only scope.
            for symbol in jvl_syntax::document_symbols(&doc.tree, &doc.text, &index) {
                if !workspace_index::matches_query(&query, &symbol.name) {
                    continue;
                }
                results.push(WorkspaceSymbol {
                    name: symbol.name,
                    kind: symbol.kind,
                    tags: None,
                    container_name: None,
                    location: OneOf::Left(Location {
                        uri: uri.clone(),
                        range: symbol.selection_range,
                    }),
                    data: None,
                });
            }
        }

        for entry in self.workspace_index().matching(&query) {
            if shadowed_paths.contains(&entry.path) {
                continue;
            }
            let Some(uri) = Uri::from_file_path(&entry.path) else {
                continue;
            };
            results.push(WorkspaceSymbol {
                name: entry.simple_name,
                kind: entry.kind,
                tags: None,
                container_name: (!entry.package.is_empty()).then_some(entry.package),
                // Zero-length range at 0:0 — see the doc comment above.
                location: OneOf::Left(Location {
                    uri,
                    range: Range::default(),
                }),
                data: None,
            });
        }

        Ok(Some(WorkspaceSymbolResponse::Nested(results)))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let docs = self.documents.lock().await;
        let Some(doc) = docs.get(params.text_document.uri.as_str()) else {
            return Ok(None);
        };
        Ok(Some(jvl_syntax::folding_ranges(&doc.tree)))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let docs = self.documents.lock().await;
        let Some(doc) = docs.get(params.text_document.uri.as_str()) else {
            return Ok(None);
        };
        let index = LineIndex::new(&doc.text, self.encoding());
        Ok(Some(jvl_syntax::selection_ranges(
            &doc.tree,
            &index,
            &params.positions,
        )))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let docs = self.documents.lock().await;
        let Some(doc) = docs.get(params.text_document.uri.as_str()) else {
            return Ok(None);
        };
        let index = LineIndex::new(&doc.text, self.encoding());
        let data = jvl_syntax::semantic_tokens(&doc.tree, &doc.text, &index);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.documents.lock().await;
        let Some(current) = docs.get(uri.as_str()) else {
            return Ok(None);
        };
        // Resolution reads the cursor's document plus every other open document
        // (for cross-file types). All synchronous — no await held.
        let open = open_docs(&docs, uri.as_str(), current);
        let index = LineIndex::new(&current.text, self.encoding());
        let symbols = ClasspathSymbols(self.classpath());
        Ok(jvl_syntax::hover(&open, 0, &index, position, &symbols))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.documents.lock().await;
        let Some(current) = docs.get(uri.as_str()) else {
            return Ok(None);
        };
        // Same plumbing as hover: cursor's document plus every other open
        // document, for cross-file overload resolution. All synchronous.
        let open = open_docs(&docs, uri.as_str(), current);
        let index = LineIndex::new(&current.text, self.encoding());
        let symbols = ClasspathSymbols(self.classpath());
        Ok(jvl_syntax::signature_help(
            &open, 0, &index, position, &symbols,
        ))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let docs = self.documents.lock().await;
        let Some(current) = docs.get(uri.as_str()) else {
            return Ok(None);
        };
        let open = open_docs(&docs, uri.as_str(), current);
        let index = LineIndex::new(&current.text, self.encoding());
        let symbols = ClasspathSymbols(self.classpath());
        let items =
            jvl_syntax::completion(&open, 0, &index, position, self.snippet_support(), &symbols);
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.documents.lock().await;
        let Some((open, uris)) = open_docs_and_uris(&docs, uri.as_str()) else {
            return Ok(None);
        };
        let index = LineIndex::new(open[0].source, self.encoding());
        let symbols = ClasspathSymbols(self.classpath());
        let def = jvl_syntax::definition(&open, 0, &index, position, &symbols);
        let location = def.and_then(|def| self.resolve_location(def, &docs, &uris));
        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    async fn goto_type_definition(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> Result<Option<GotoTypeDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.documents.lock().await;
        let Some((open, uris)) = open_docs_and_uris(&docs, uri.as_str()) else {
            return Ok(None);
        };
        let index = LineIndex::new(open[0].source, self.encoding());
        let symbols = ClasspathSymbols(self.classpath());
        let def = jvl_syntax::type_definition(&open, 0, &index, position, &symbols);
        let location = def.and_then(|def| self.resolve_location(def, &docs, &uris));
        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    /// M4 (4.3): `textDocument/references` — two-tier, bounded, confirm-by-
    /// resolution (see the `jvl_syntax::references` module doc for the full
    /// design). The target's visibility tier (`jvl_syntax::Tier`, read off
    /// its declaration's modifiers) decides the scan's reach:
    ///
    /// - `FileLocal` (local/param/`private` member): only the declaring file
    ///   is scanned — already open, already parsed, no disk I/O.
    /// - `Workspace` (package-private/protected/public): a bounded textual
    ///   prefilter (`references::prefilter`) finds candidate files under the
    ///   discovered source roots; each is parsed on demand (reusing the
    ///   ladder-step-(c) `(path, mtime)` cache, or a currently-open
    ///   document's live text/tree when the hit is itself open) and
    ///   semantically confirmed one file at a time
    ///   (`jvl_syntax::references_in_doc`), yielding to the runtime between
    ///   files so a cancelled request actually stops promptly.
    ///
    /// Only symbols declared in a currently *open* document are supported
    /// (open-files-first, like every other feature here) —
    /// `jvl_syntax::reference_target` answers `None` for anything else
    /// (an external/JDK symbol, an unopened project file, `this`/`super`, a
    /// non-identifier), and this handler answers `Ok(None)` in that case.
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        // Resolve the target and snapshot everything the (possibly slow,
        // Tier-2-only) workspace scan below needs, then release the
        // documents lock before it — mirrors `workspace_index::ensure_built`'s
        // own pattern of never holding the lock across a directory walk.
        struct Snapshot {
            target: jvl_syntax::ReferenceTarget,
            target_uri: String,
            target_text: String,
            target_tree: Tree,
            roots: Vec<PathBuf>,
            project_root: Option<PathBuf>,
            /// Currently-open documents' live text/tree, keyed by
            /// canonicalized path (falling back to the raw path when
            /// canonicalization fails) — a Tier-2 hit file that is itself
            /// open is read from here instead of disk, so unsaved edits are
            /// reflected.
            open_snapshot: HashMap<PathBuf, (String, Tree)>,
        }

        let snapshot = {
            let docs = self.documents.lock().await;
            let Some((open, uris)) = open_docs_and_uris(&docs, uri.as_str()) else {
                return Ok(None);
            };
            let index = LineIndex::new(open[0].source, self.encoding());
            let symbols = ClasspathSymbols(self.classpath());
            let Some(target) = jvl_syntax::reference_target(&open, 0, &index, position, &symbols)
            else {
                return Ok(None);
            };

            let target_uri = uris[target.doc].to_string();
            let Some(target_doc) = docs.get(&target_uri) else {
                return Ok(None);
            };
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
                    open_snapshot.insert(key, (doc.text.clone(), doc.tree.clone()));
                }
            }

            Snapshot {
                target,
                target_uri,
                target_text,
                target_tree,
                roots,
                project_root,
                open_snapshot,
            }
        };

        let symbols = ClasspathSymbols(self.classpath());
        let target_index = LineIndex::new(&snapshot.target_text, self.encoding());
        let target_open = jvl_syntax::OpenDoc {
            source: &snapshot.target_text,
            tree: &snapshot.target_tree,
        };
        let Some(target_uri_parsed) = snapshot.target_uri.parse::<Uri>().ok() else {
            return Ok(None);
        };

        let mut locations = Vec::new();

        // The declaring file's own occurrences are always in scope — Tier 1
        // stops here entirely; Tier 2 also always checks it directly
        // (self-references), whether or not it happens to lie under a
        // discovered source root.
        let self_target = jvl_syntax::ReferenceTarget {
            doc: 0,
            ..snapshot.target.clone()
        };
        let own_hits = jvl_syntax::references_in_doc(
            std::slice::from_ref(&target_open),
            0,
            &self_target,
            include_declaration,
            &symbols,
        );
        locations.extend(own_hits.ranges.into_iter().map(|range| Location {
            uri: target_uri_parsed.clone(),
            range: byte_range_to_lsp(&target_index, range),
        }));

        if snapshot.target.tier == jvl_syntax::Tier::Workspace {
            let scan = references::prefilter(
                &snapshot.roots,
                snapshot.project_root.as_deref(),
                &snapshot.target.name,
            )
            .await;

            let target_path = open_doc_path(&snapshot.target_uri);
            let target_canon = target_path
                .as_ref()
                .and_then(|p| std::fs::canonicalize(p).ok());

            for hit_path in scan.files {
                let hit_canon = std::fs::canonicalize(&hit_path).ok();
                let is_target_file = target_path.as_ref() == Some(&hit_path)
                    || (hit_canon.is_some() && hit_canon == target_canon);
                if is_target_file {
                    continue; // already handled above
                }

                let cached = hit_canon
                    .as_ref()
                    .and_then(|c| snapshot.open_snapshot.get(c))
                    .map(|(text, tree)| (Arc::new(text.clone()), tree.clone()))
                    .or_else(|| self.parsed_project_file(&hit_path));
                let Some((hit_text, hit_tree)) = cached else {
                    tokio::task::yield_now().await;
                    continue;
                };

                let docs_for_scan = [
                    jvl_syntax::OpenDoc {
                        source: &hit_text,
                        tree: &hit_tree,
                    },
                    jvl_syntax::OpenDoc {
                        source: &snapshot.target_text,
                        tree: &snapshot.target_tree,
                    },
                ];
                let remapped = jvl_syntax::ReferenceTarget {
                    doc: 1,
                    ..snapshot.target.clone()
                };
                let hits = jvl_syntax::references_in_doc(
                    &docs_for_scan,
                    0,
                    &remapped,
                    include_declaration,
                    &symbols,
                );
                if !hits.ranges.is_empty() {
                    if let Some(hit_uri) = Uri::from_file_path(&hit_path) {
                        let hit_index = LineIndex::new(&hit_text, self.encoding());
                        locations.extend(hits.ranges.into_iter().map(|range| Location {
                            uri: hit_uri.clone(),
                            range: byte_range_to_lsp(&hit_index, range),
                        }));
                    }
                }

                tokio::task::yield_now().await;
            }

            if scan.truncated {
                self.client
                    .show_message(
                        MessageType::INFO,
                        format!(
                            "References search truncated at {} files; results may be incomplete.",
                            references::MAX_FILES_SCANNED
                        ),
                    )
                    .await;
            }
        }

        Ok((!locations.is_empty()).then_some(locations))
    }
}

/// The `jvl/externalSource` custom request: the extension's virtual-document
/// content provider for the `jvl-src:` scheme calls this to fetch the text a
/// `Definition::External` `Location` points into (real source when available,
/// else a signature-only stub — see `Backend::external_source_text`).
#[derive(Debug, Deserialize)]
struct ExternalSourceParams {
    uri: String,
}

#[derive(Debug, Serialize)]
struct ExternalSourceResult {
    text: String,
}

impl Backend {
    async fn external_source(&self, params: ExternalSourceParams) -> Result<ExternalSourceResult> {
        let text = fqn_from_jvl_src_uri(&params.uri)
            .and_then(|fqn| self.external_source_text(&fqn))
            .map(|t| (*t).clone())
            .unwrap_or_default();
        Ok(ExternalSourceResult { text })
    }
}

/// Adapts `jvl-classpath` to `jvl-syntax`'s `SymbolSource`, converting the
/// bytecode model into the analysis crate's external-symbol types. Holds an
/// owned snapshot `Arc` (from `Backend::classpath()`) rather than a borrow,
/// so it's unaffected by a concurrent M5.2 rebuild swap mid-request.
struct ClasspathSymbols(Arc<jvl_classpath::Classpath>);

impl jvl_syntax::SymbolSource for ClasspathSymbols {
    fn class(&self, fqn: &str) -> Option<jvl_syntax::ExternalClass> {
        let info = self.0.class(fqn)?;
        Some(jvl_syntax::ExternalClass {
            supers: info.supers.clone(),
            type_params: info.type_params.clone(),
            members: info
                .members
                .iter()
                .map(|m| jvl_syntax::ExternalMember {
                    name: m.name.clone(),
                    kind: match m.kind {
                        jvl_classpath::MemberKind::Method => jvl_syntax::ExternalMemberKind::Method,
                        jvl_classpath::MemberKind::Field => jvl_syntax::ExternalMemberKind::Field,
                    },
                    signature: m.signature.clone(),
                    template: m.template.clone(),
                    is_static: m.is_static,
                })
                .collect(),
        })
    }

    /// Type arguments each supertype entry is instantiated with (index-aligned
    /// with `class(fqn)?.supers`, `{i}` placeholders over `fqn`'s own type
    /// params) — the shapes match by design, so this is a straight delegation
    /// to the classpath crate's `Signature`-attribute parse.
    fn super_type_args(&self, fqn: &str) -> Vec<Vec<String>> {
        self.0
            .class(fqn)
            .map(|info| info.super_type_args.clone())
            .unwrap_or_default()
    }

    /// Find a member's Javadoc by walking the type and its supertypes' sources
    /// (a member may be declared in a supertype), first match wins.
    fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
        let mut stack = vec![fqn.to_string()];
        let mut visited = std::collections::HashSet::new();
        let mut budget = 64;
        while let Some(current) = stack.pop() {
            if budget == 0 || !visited.insert(current.clone()) {
                continue;
            }
            budget -= 1;
            if let Some(src) = self.0.source(&current) {
                let simple = current
                    .rsplit('.')
                    .next()
                    .and_then(|s| s.rsplit('$').next())
                    .unwrap_or(&current);
                if let Some(doc) = jvl_syntax::javadoc_in_source(&src, simple, member) {
                    return Some(doc);
                }
            }
            // Members can be inherited; type Javadoc lives only in its own source.
            if member.is_some() {
                if let Some(info) = self.0.class(&current) {
                    stack.extend(info.supers.iter().cloned());
                }
            }
        }
        None
    }
}

/// Build the open-document slice the analysis reads, with the cursor's document
/// first (index 0) so it wins simple-name collisions.
fn open_docs<'a>(
    docs: &'a HashMap<String, Document>,
    current_uri: &str,
    current: &'a Document,
) -> Vec<jvl_syntax::OpenDoc<'a>> {
    let mut open = Vec::with_capacity(docs.len());
    open.push(jvl_syntax::OpenDoc {
        source: &current.text,
        tree: &current.tree,
    });
    for (uri, doc) in docs.iter() {
        if uri != current_uri {
            open.push(jvl_syntax::OpenDoc {
                source: &doc.text,
                tree: &doc.tree,
            });
        }
    }
    open
}

/// Like [`open_docs`], but also returns a parallel vector of URIs (index-for-
/// index with the `OpenDoc`s) — needed by goto-definition/type-definition to
/// build a `Location` in whichever open document a cross-file symbol resolves
/// into (`Definition::InOpenDoc { doc, .. }` names an index into this same
/// slice). `None` if `current_uri` isn't an open document.
fn open_docs_and_uris<'a>(
    docs: &'a HashMap<String, Document>,
    current_uri: &str,
) -> Option<(Vec<jvl_syntax::OpenDoc<'a>>, Vec<&'a str>)> {
    let (current_key, current_doc) = docs.get_key_value(current_uri)?;
    let mut open = Vec::with_capacity(docs.len());
    let mut uris = Vec::with_capacity(docs.len());
    open.push(jvl_syntax::OpenDoc {
        source: &current_doc.text,
        tree: &current_doc.tree,
    });
    uris.push(current_key.as_str());
    for (uri, doc) in docs.iter() {
        if uri != current_key {
            open.push(jvl_syntax::OpenDoc {
                source: &doc.text,
                tree: &doc.tree,
            });
            uris.push(uri.as_str());
        }
    }
    Some((open, uris))
}

/// `jvl-server --version` prints the crate version and exits, with no LSP
/// startup. This lets the extension shell do a lightweight version handshake
/// (comparing the bundled server's version against its own) before spawning
/// the real LSP session.
fn print_version_and_exit_if_requested() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
}

#[tokio::main]
async fn main() {
    print_version_and_exit_if_requested();

    // Logs go to stderr; stdout is the LSP transport. ANSI is disabled because
    // the editor's output panel renders raw escape codes as a jumble; the noisy
    // module-path target is dropped; and the default filter mutes the LSP
    // framework's debug chatter (e.g. spurious cancel-request notices).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("JVL_LOG").unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,tower_lsp_server=warn")
            }),
        )
        .init();

    tracing::info!("starting java-vsix-lite language server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(Backend::new)
        .custom_method("jvl/externalSource", Backend::external_source)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M5.6: with no `initializationOptions` at all, unresolved-member
    /// diagnostics must default to **on** (the conservative gating inside
    /// `member_diagnostics` is what keeps this safe, not this flag).
    #[test]
    fn unresolved_member_diagnostics_defaults_to_true_when_absent() {
        let params = InitializeParams::default();
        assert!(unresolved_member_diagnostics_opt(&params));
    }

    /// An explicit `false` (the user opting back out) must still be honored.
    #[test]
    fn unresolved_member_diagnostics_respects_explicit_false() {
        let params = InitializeParams {
            initialization_options: Some(
                serde_json::json!({ "unresolvedMemberDiagnostics": false }),
            ),
            ..Default::default()
        };
        assert!(!unresolved_member_diagnostics_opt(&params));
    }

    /// An explicit `true` is, of course, still `true`.
    #[test]
    fn unresolved_member_diagnostics_respects_explicit_true() {
        let params = InitializeParams {
            initialization_options: Some(
                serde_json::json!({ "unresolvedMemberDiagnostics": true }),
            ),
            ..Default::default()
        };
        assert!(unresolved_member_diagnostics_opt(&params));
    }

    /// M5.2: absent `initializationOptions`, the debounce defaults to 2s.
    #[test]
    fn classpath_debounce_defaults_to_2000ms() {
        let params = InitializeParams::default();
        assert_eq!(classpath_debounce_ms_opt(&params), 2000);
    }

    /// The test-only override is honored (so E2E tests don't sleep 2s+).
    #[test]
    fn classpath_debounce_respects_override() {
        let params = InitializeParams {
            initialization_options: Some(serde_json::json!({ "classpathDebounceMs": 10 })),
            ..Default::default()
        };
        assert_eq!(classpath_debounce_ms_opt(&params), 10);
    }

    /// No `workspace.didChangeWatchedFiles.dynamicRegistration` capability
    /// at all -> the build-file watch must not be registered.
    #[test]
    fn watched_files_registration_defaults_to_false() {
        let params = InitializeParams::default();
        assert!(!supports_watched_files_registration(&params));
    }

    #[test]
    fn watched_files_registration_respects_client_capability() {
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration: Some(true),
                        relative_pattern_support: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(supports_watched_files_registration(&params));
    }

    /// M5.2: only the four documented build-file names/locations match —
    /// everything else (including an unrelated `.java` file) must not.
    #[test]
    fn classpath_build_file_matching() {
        let matches = |s: &str| is_classpath_build_file(&s.parse::<Uri>().unwrap());
        assert!(matches("file:///proj/pom.xml"));
        assert!(matches("file:///proj/sub/pom.xml"));
        assert!(matches("file:///proj/build.gradle"));
        assert!(matches("file:///proj/build.gradle.kts"));
        assert!(matches("file:///proj/gradle/libs.versions.toml"));
        // `libs.versions.toml` outside a `gradle/` dir doesn't count.
        assert!(!matches("file:///proj/libs.versions.toml"));
        assert!(!matches("file:///proj/src/Main.java"));
        assert!(!matches("file:///proj/pom.xml.bak"));
    }

    /// M5.2's debounce/coalescing state machine — pure, no real timers.
    mod rebuild_coalescer {
        use super::*;

        /// N events arriving before the debounce settles must still yield
        /// exactly one rebuild: every event after the first is coalesced
        /// (not a new driver), and the debounce only proceeds to a rebuild
        /// once nothing further arrived during the wait.
        #[test]
        fn n_events_in_one_window_yield_one_rebuild() {
            let mut c = RebuildCoalescer::new();
            assert!(c.on_event(), "first event becomes the driver");
            assert!(!c.on_event(), "second event coalesces into the driver");
            assert!(!c.on_event(), "third event coalesces into the driver");
            // The two coalesced events landed during the wait, so the driver
            // must wait a full debounce window again ("a fresh event resets
            // the timer") before it may proceed...
            assert!(
                !c.on_debounce_elapsed(),
                "coalesced events reset the window once"
            );
            // ...and only then, with nothing further arriving, settles to
            // exactly one rebuild — not one per event.
            assert!(
                c.on_debounce_elapsed(),
                "settled with nothing new -> proceed to exactly one rebuild"
            );
        }

        /// A fresh event during the debounce wait resets it: the driver must
        /// wait a full window again rather than proceeding immediately.
        #[test]
        fn event_during_wait_resets_the_window() {
            let mut c = RebuildCoalescer::new();
            assert!(c.on_event());
            assert!(!c.on_event(), "still just one driver");
            assert!(
                !c.on_debounce_elapsed(),
                "a coalesced event arrived during the wait -> must wait again"
            );
            assert!(
                c.on_debounce_elapsed(),
                "nothing arrived during the second wait -> now proceed"
            );
        }

        /// An event arriving while a rebuild is in flight schedules exactly
        /// one follow-up rebuild — not one per coalesced event, and no
        /// overlapping rebuild is ever started.
        #[test]
        fn event_during_rebuild_schedules_exactly_one_followup() {
            let mut c = RebuildCoalescer::new();
            assert!(c.on_event());
            assert!(c.on_debounce_elapsed(), "settles into the first rebuild");

            // Several changes land while that rebuild is running.
            assert!(
                !c.on_event(),
                "an event during an in-flight rebuild never starts a second driver"
            );
            assert!(!c.on_event(), "neither does another one");

            assert!(
                c.on_rebuild_finished(),
                "exactly one follow-up cycle must be scheduled"
            );
            assert!(
                c.on_debounce_elapsed(),
                "the follow-up settles to a single rebuild, not one per coalesced event"
            );
            assert!(
                !c.on_rebuild_finished(),
                "nothing pending afterwards -> driving stops"
            );
        }

        /// The base case: no events at all -> nothing to do, and a rebuild
        /// finishing cleanly (no `dirty`) stops driving rather than looping
        /// forever.
        #[test]
        fn quiescent_rebuild_stops_driving() {
            let mut c = RebuildCoalescer::new();
            assert!(c.on_event());
            assert!(c.on_debounce_elapsed());
            assert!(!c.on_rebuild_finished(), "nothing arrived -> stop driving");
        }
    }
}
