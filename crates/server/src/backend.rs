//! Backend state, document parsing, caches, and classpath management.
//!
//! Holds the [`Backend`] struct definition plus construction and the
//! cache/classpath/project-root parsing helpers built directly on its
//! state. The `impl LanguageServer for Backend` trait implementation and
//! `main()` stay in `main.rs`; other responsibility groups (diagnostics,
//! navigation/references/implementations, call/type hierarchy) live in
//! their own sibling modules and share this struct via additional
//! `impl Backend` blocks.

use std::collections::HashMap;
use std::ops::Range as StdRange;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime};

use jvl_syntax::tree_sitter::{Parser, Tree};
use jvl_syntax::{LineIndex, PositionEncoding};
use tokio::sync::Mutex;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::Client;

use crate::javac;
use crate::workspace_index;
use crate::{infer_source_root, open_doc_path};

/// Bound on the step-(d) external stub/source cache: cleared wholesale past
/// this many entries rather than tracking LRU — the path is rare enough
/// (cold, one-off lookups) that eviction pressure is low and a simple bound
/// is not worth extra bookkeeping.
const EXTERNAL_CACHE_CAP: usize = 32;

/// Bound on the parse-on-demand project-file cache, shared by goto-definition
/// ladder step (c) (one file per request) and find-references' Tier-2
/// confirm (up to `references::MAX_FILES_SCANNED` = 500 hit files in a single
/// request). Same clear-wholesale-on-overflow policy as
/// [`EXTERNAL_CACHE_CAP`], but sized so a typical references request's hit
/// set survives within one request *and* is still warm for a follow-up
/// request on the same symbol (500 hit files is the pathological cap;
/// real hit sets are far smaller). ~256 parsed small-to-medium `.java` files
/// is a few tens of MB at worst — bounded, and cleared rather than grown when
/// exceeded.
const PROJECT_FILE_CACHE_CAP: usize = 256;

/// Hard cap on a single unopened project source file's size before
/// [`Backend::parsed_project_file`] will read and cache it. Guards both of
/// that function's callers — a one-off goto-definition lookup and, more
/// importantly, find-references' Tier-2 per-hit-file confirm — against a
/// pathologically large file (vendored/generated code, a mis-tagged binary
/// blob sitting under a source root) being read wholesale and handed to
/// tree-sitter for a cold, on-demand parse. Generous for a hand-written Java
/// source file (a few MB) — a guard against a resource pathology, not a
/// functional limit on real code. A file over this cap is treated exactly
/// like an unreadable one (see the doc comment on `parsed_project_file`
/// itself for why that's conservatively safe for `rename`).
pub(crate) const MAX_PROJECT_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// A single project source file parsed on demand for ladder step (c),
/// invalidated by `mtime` so an on-disk edit is picked up without an explicit
/// notification (the server never watches files).
pub(crate) struct CachedProjectFile {
    pub(crate) mtime: SystemTime,
    pub(crate) text: Arc<String>,
    pub(crate) tree: Tree,
}

/// A single open document: its current text and the parse tree kept in sync
/// with it, plus its LSP version (`rename`'s `WorkspaceEdit` prefers
/// versioned `TextDocumentEdit`s for open documents over the client's
/// `documentChanges` capability — see `Backend::rename`).
pub(crate) struct Document {
    pub(crate) text: String,
    pub(crate) tree: Tree,
    pub(crate) version: i32,
}

/// Debounce/coalescing decision state for classpath rebuilds
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
pub(crate) struct RebuildCoalescer {
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
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A matching build-file change arrived. Returns `true` exactly once per
    /// debounce-then-rebuild cycle: the caller that gets `true` must drive
    /// it (wait the debounce window, call `on_debounce_elapsed`, and so on
    /// until settled); every other concurrent/later caller gets `false` —
    /// its event has already been folded into the driver's next decision.
    pub(crate) fn on_event(&mut self) -> bool {
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
    pub(crate) fn on_debounce_elapsed(&mut self) -> bool {
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
    pub(crate) fn on_rebuild_finished(&mut self) -> bool {
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

pub(crate) struct Backend {
    pub(crate) client: Client,
    /// Reused across parses; held only for synchronous parse calls, never across
    /// an `.await`.
    parser: StdMutex<Parser>,
    /// Open documents, keyed by URI string — their live in-memory text and
    /// parse tree. Closed project source files are indexed separately (see
    /// `workspace_index` and `project_symbols`), so the workspace is not
    /// limited to whatever happens to be open here.
    pub(crate) documents: Mutex<HashMap<String, Document>>,
    /// LSP position encoding negotiated during `initialize` (defaults to UTF-16).
    pub(crate) encoding: OnceLock<PositionEncoding>,
    /// Whether the client supports snippet completion (`$1` tab stops). Defaults
    /// to `false` until negotiated during `initialize`.
    pub(crate) snippet_support: OnceLock<bool>,
    /// Bytecode-backed symbols for imported (JDK/dependency) types. Built lazily
    /// on first use so the JDK's jmods aren't scanned until completion/hover needs
    /// them. Swappable — a watched build-file change triggers a
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
    pub(crate) classpath_watch_dynamic: OnceLock<bool>,
    /// Whether the client supports dynamic registration for type
    /// hierarchy. `ls-types` 0.0.6's `ServerCapabilities` has no
    /// `typeHierarchyProvider` field to advertise statically, so the
    /// feature is registered dynamically in `initialized` instead — VS Code
    /// supports exactly that.
    pub(crate) type_hierarchy_dynamic: OnceLock<bool>,
    /// Debounce window for a classpath rebuild after a watched build-file
    /// change (`classpath_debounce_ms_opt`) — 2s by default, overridable via
    /// `initializationOptions.classpathDebounceMs` so tests aren't forced to
    /// sleep multiple seconds.
    pub(crate) classpath_debounce_ms: OnceLock<u64>,
    /// Debounce/coalescing decision state (see [`RebuildCoalescer`]),
    /// guarded by a plain `Mutex` — decisions are synchronous and quick,
    /// never held across an `.await`.
    pub(crate) classpath_rebuild: StdMutex<RebuildCoalescer>,
    /// Workspace root (from `initialize`), used to discover project dependencies.
    pub(crate) workspace_root: OnceLock<Option<PathBuf>>,
    /// Fallback project root derived from the first opened document (so deps
    /// resolve even when a lone file is opened with no workspace folder).
    pub(crate) project_root_hint: OnceLock<Option<PathBuf>>,
    /// Whether to emit unresolved-member diagnostics (default on; opt-out).
    pub(crate) unresolved_member_diagnostics: OnceLock<bool>,
    /// Whether to emit unused-code diagnostics (default on; opt-out).
    pub(crate) unused_diagnostics: OnceLock<bool>,
    /// Ladder step (c): a single unopened project source file, parsed on
    /// demand and cached by path (see [`CachedProjectFile`]). Never held
    /// across an `.await`.
    pub(crate) project_file_cache: StdMutex<HashMap<PathBuf, CachedProjectFile>>,
    /// Ladder step (d): a JDK/dependency type's source (or, absent that, a
    /// signature-only stub rendered from `ClassInfo`) — the text served
    /// through the `jvl-src:` virtual document scheme. Keyed by FQN. Never
    /// held across an `.await`.
    external_stub_cache: StdMutex<HashMap<String, Arc<String>>>,
    /// The lazy, bounded workspace symbol index (built on the first
    /// `workspace/symbol` request, not at startup).
    workspace_index: workspace_index::WorkspaceIndex,
    /// Whether [`workspace_index`]'s cap-truncation has already been logged
    /// to the client — logged once, not on every subsequent query.
    pub(crate) workspace_index_truncation_logged: std::sync::atomic::AtomicBool,
    /// Whether the client's `workspace.workspaceEdit.resourceOperations`
    /// includes `"rename"` — gates whether `rename`'s `WorkspaceEdit` may
    /// include a `RenameFile` resource op (text edits are emitted either way).
    pub(crate) supports_rename_file: OnceLock<bool>,
    /// An explicit override for the JDK home to find `javac` under
    /// (`java-vsix-lite.jdk.home` initialization option), tried before
    /// `$JAVA_HOME` — see `javac::locate_javac`.
    pub(crate) jdk_home_override: OnceLock<Option<PathBuf>>,
    /// The `javac` check's timeout, already clamped to
    /// `[10, 600]` seconds (`javacTimeoutSecs` initialization option,
    /// default 120) — see `javac::clamp_timeout_secs`.
    pub(crate) javac_timeout_secs: OnceLock<u64>,
    /// One `checkProject` run at a time — `true` while a run is in
    /// flight; a concurrent `executeCommand` sees `true` and returns
    /// "already running" instead of starting a second `javac`.
    pub(crate) javac_running: std::sync::atomic::AtomicBool,
    /// The currently-running `javac` child (if any), shared with
    /// `javac::run`'s polling loop so `shutdown` can kill+reap it — see
    /// `javac::kill_running_child`.
    pub(crate) javac_child: javac::SharedChild,
    /// Bounded accounting of the reader threads abandoned by
    /// timed-out/cancelled runs, so a misbehaving `jdk.home` binary can't
    /// leak blocked threads without bound — `javac::run` refuses to start
    /// once the cap is hit. See `javac::LeakedReaders` for the security
    /// rationale.
    pub(crate) javac_leaked_readers: javac::LeakedReaders,
    /// Diagnostics from the last `checkProject` run, keyed by URI
    /// string, merged into `compute_diagnostics`'s result for that file.
    /// Cleared for a file on its next `didChange` (stale after edit) and
    /// wholesale-replaced (with a publish to clear anything that dropped
    /// out) on every new `checkProject` run — see
    /// `Backend::publish_javac_diagnostics`.
    pub(crate) javac_diagnostics: StdMutex<HashMap<String, Vec<Diagnostic>>>,
}

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            parser: StdMutex::new(jvl_syntax::new_parser()),
            documents: Mutex::new(HashMap::new()),
            encoding: OnceLock::new(),
            snippet_support: OnceLock::new(),
            classpath: StdRwLock::new(None),
            classpath_watch_dynamic: OnceLock::new(),
            type_hierarchy_dynamic: OnceLock::new(),
            classpath_debounce_ms: OnceLock::new(),
            classpath_rebuild: StdMutex::new(RebuildCoalescer::new()),
            workspace_root: OnceLock::new(),
            project_root_hint: OnceLock::new(),
            unresolved_member_diagnostics: OnceLock::new(),
            unused_diagnostics: OnceLock::new(),
            project_file_cache: StdMutex::new(HashMap::new()),
            external_stub_cache: StdMutex::new(HashMap::new()),
            workspace_index: workspace_index::WorkspaceIndex::new(),
            workspace_index_truncation_logged: std::sync::atomic::AtomicBool::new(false),
            supports_rename_file: OnceLock::new(),
            jdk_home_override: OnceLock::new(),
            javac_timeout_secs: OnceLock::new(),
            javac_running: std::sync::atomic::AtomicBool::new(false),
            javac_child: Arc::new(StdMutex::new(None)),
            javac_leaked_readers: javac::LeakedReaders::new(),
            javac_diagnostics: StdMutex::new(HashMap::new()),
        }
    }

    pub(crate) fn encoding(&self) -> PositionEncoding {
        self.encoding
            .get()
            .copied()
            .unwrap_or(PositionEncoding::Utf16)
    }

    pub(crate) fn snippet_support(&self) -> bool {
        self.snippet_support.get().copied().unwrap_or(false)
    }

    /// Whether the client advertised `resourceOperations` including
    /// `"rename"` — see [`supports_rename_file_op`], negotiated in
    /// `initialize`.
    pub(crate) fn supports_rename_file(&self) -> bool {
        self.supports_rename_file.get().copied().unwrap_or(false)
    }

    /// The imported-type symbol source, built on first use from the user's JDK
    /// plus the project's declared dependencies. The project root is the
    /// workspace folder, or (if none) one derived from the first opened file.
    ///
    /// Returns a snapshot `Arc`: the caller holds its own reference-counted
    /// handle to whichever `Classpath` was current the moment it asked, so a
    /// concurrent rebuild swap never invalidates work already in
    /// flight, and this method never holds `self.classpath`'s lock across an
    /// `.await`.
    pub(crate) fn classpath(&self) -> Arc<jvl_classpath::Classpath> {
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

    /// The debounce window for classpath rebuilds (see
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

    /// Drive one or more debounce-then-rebuild cycles until settled.
    /// Only the single caller that won `RebuildCoalescer::on_event`'s race
    /// calls this (see `did_change_watched_files`) — every other concurrent
    /// or later matching event just folds into the cycle already running
    /// here, so at most one rebuild is ever in flight and exactly one more
    /// runs if anything changed while it was.
    pub(crate) async fn drive_classpath_rebuild(&self) {
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
    pub(crate) fn project_root(&self) -> Option<PathBuf> {
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
    pub(crate) fn source_roots(
        &self,
        docs: &HashMap<String, Document>,
        project_root: &Path,
    ) -> Vec<PathBuf> {
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
    pub(crate) fn candidate_paths(
        &self,
        docs: &HashMap<String, Document>,
        fqn: &str,
    ) -> Vec<PathBuf> {
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

    /// Rule (c): the package a document's own path implies it should
    /// declare — `None` when no project root is known at all (a lone file
    /// with no workspace) or the URI isn't a `file:`
    /// path; otherwise the dotted directory segments between the *most
    /// specific* containing source root and the file (possibly `""` — the
    /// unnamed/default package — when the file sits directly under the
    /// root). Only a root that is an actual prefix of the file's directory
    /// counts as "confidently containing" it, matching rule (c)'s
    /// conservative gate.
    ///
    /// Deliberately does NOT reuse [`Self::source_roots`]: that ladder
    /// includes roots *inferred from other open documents'* package/path
    /// coincidences (`infer_source_root`), which is fine for best-effort
    /// navigation but unacceptable for a diagnostic — what error a file gets
    /// must never depend on which unrelated sibling files happen to be open.
    /// Only the fixed conventional roots (`src/main/java`, `src/test/java`
    /// under the workspace root) qualify. The bare workspace root itself is
    /// also deliberately excluded: a file at `root/tools/Foo.java` is far
    /// more likely an ad-hoc/unconventional layout than a genuine claim
    /// that `Foo` belongs to package `tools`, so flagging it would be a
    /// false positive by construction. Unconventional layouts simply stay
    /// silent — lone files and unknown roots produce no diagnostic.
    pub(crate) fn expected_package(&self, uri: &str) -> Option<String> {
        let path = open_doc_path(uri)?;
        let dir = path.parent()?;
        let project_root = self.project_root()?;
        [
            project_root.join("src/main/java"),
            project_root.join("src/test/java"),
        ]
        .into_iter()
        .find(|root| dir.starts_with(root))
        .map(|root| {
            dir.strip_prefix(&root)
                .into_iter()
                .flat_map(|rel| rel.components())
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join(".")
        })
    }

    /// The lazy, bounded workspace symbol index (see `workspace_index`),
    /// exposed so another feature in this crate (e.g. a future add-import)
    /// can do "simple name -> paths" lookups without re-walking the
    /// workspace itself. Building/rebuilding only happens via `ensure_built`
    /// (called from the `symbol` handler); this accessor never triggers it.
    pub(crate) fn workspace_index(&self) -> &workspace_index::WorkspaceIndex {
        &self.workspace_index
    }

    /// Refresh the workspace type-name index (see `workspace_index`)
    /// before consulting `ProjectSymbols` — the same lock-roots-then-
    /// release-then-walk choreography the `symbol` handler already used,
    /// factored out so every interactive handler that now consults
    /// closed project files can call it too. Cheap on the common case (a
    /// handful of `stat`s, no walk) once nothing under a source root has
    /// changed. Must be called *before* taking `self.documents`'s own lock
    /// — it briefly takes that lock itself to compute source roots, and
    /// `tokio::sync::Mutex` isn't reentrant.
    pub(crate) async fn ensure_workspace_index(&self) {
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
    }

    /// Parse (or reuse a cached parse of) a single project source file,
    /// invalidated by `mtime` so an on-disk edit is picked up without an
    /// explicit notification (the server never watches files). Shared by
    /// ladder step (c) (`locate_in_project_file`, below) and
    /// find-references' Tier 2 per-hit-file confirm (`references()`/
    /// `references.rs`) — both are one-off, cold, parse-on-demand lookups
    /// of a file that isn't open in the editor.
    ///
    /// A file over [`MAX_PROJECT_FILE_BYTES`] is reported exactly like an
    /// unreadable one (`None`), never read or cached — conservatively safe
    /// for every caller: ladder step (c) just tries the next fallback, and
    /// `scan_references`'s Tier 2 confirm folds a `None` here into
    /// `unparsed_hit_files`, which already forces `rename` to refuse rather
    /// than risk missing an occurrence hiding in the skipped file.
    pub(crate) fn parsed_project_file(&self, path: &Path) -> Option<(Arc<String>, Tree)> {
        let metadata = std::fs::metadata(path).ok()?;
        if metadata.len() > MAX_PROJECT_FILE_BYTES {
            return None;
        }
        let mtime = metadata.modified().ok()?;

        let mut cache = self
            .project_file_cache
            .lock()
            .expect("project file cache poisoned");
        let fresh = cache.get(path).is_some_and(|c| c.mtime == mtime);
        if !fresh {
            let text = std::fs::read_to_string(path).ok()?;
            let tree = self.parse(&text, None);
            insert_bounded_project_file(
                &mut cache,
                PROJECT_FILE_CACHE_CAP,
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
    pub(crate) fn locate_in_project_file(
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
    pub(crate) fn external_source_text(&self, fqn: &str) -> Option<Arc<String>> {
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

    /// Parse `text`, reusing `old` for an incremental reparse when the caller has
    /// already applied the corresponding `InputEdit`s to it.
    pub(crate) fn parse(&self, text: &str, old: Option<&Tree>) -> Tree {
        let mut parser = self.parser.lock().expect("parser mutex poisoned");
        jvl_syntax::parse(&mut parser, text, old).expect("parser yields a tree for in-memory text")
    }

    /// Parse a freshly opened (or fully replaced) document from scratch, store
    /// it, and publish its diagnostics.
    pub(crate) async fn open_document(&self, uri: Uri, version: i32, text: String) {
        let tree = self.parse(&text, None);
        // Warm the classpath (its first-ever build scans the JDK/project
        // dependencies and can be comparatively slow) before taking the
        // documents lock below — `compute_diagnostics` calls `self.classpath()`
        // again for the unresolved-member pass, but by then it's just an
        // `Arc` clone under `classpath`'s own (separate, briefly-held) lock,
        // never the expensive build. Safe because `classpath()` is an
        // idempotent, order-independent double-checked read/build.
        self.classpath();
        let diagnostics = {
            let mut docs = self.documents.lock().await;
            docs.insert(
                uri.as_str().to_string(),
                Document {
                    text,
                    tree,
                    version,
                },
            );
            self.compute_diagnostics(&docs, uri.as_str())
        };
        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    /// `REBUILD_CLASSPATH_COMMAND` — an immediate, synchronous classpath
    /// rebuild (unlike `drive_classpath_rebuild`'s debounced version driven by
    /// watched build-file changes), used by the fixed-point loop that follows
    /// a consent-gated dependency install: the extension awaits this
    /// `executeCommand` response before re-querying `jvl/missingDependencies`,
    /// so it must reflect the just-installed jar(s) by the time it returns.
    /// Reuses the exact same static/offline resolution as every other
    /// rebuild path — this command itself never touches the network; it only
    /// re-reads whatever the extension already placed under `~/.m2`.
    pub(crate) async fn run_rebuild_classpath_command(&self) -> serde_json::Value {
        self.rebuild_classpath().await;
        self.republish_all_diagnostics().await;
        serde_json::json!({ "status": "ok" })
    }
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
pub(crate) fn simple_name(fqn: &str) -> &str {
    fqn.rsplit(['.', '$']).next().unwrap_or(fqn)
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

/// Insert into the bounded project-file cache, clearing it wholesale when
/// the cap is reached (the same simple bound-not-LRU policy as the external
/// stub cache — see [`PROJECT_FILE_CACHE_CAP`]'s doc comment for sizing).
/// A free function (not a `Backend` method) so the overflow behavior is
/// directly unit-testable without constructing a `Client`.
pub(crate) fn insert_bounded_project_file(
    cache: &mut HashMap<PathBuf, CachedProjectFile>,
    cap: usize,
    path: PathBuf,
    file: CachedProjectFile,
) {
    if cache.len() >= cap {
        cache.clear();
    }
    cache.insert(path, file);
}

pub(crate) fn byte_range_to_lsp(index: &LineIndex, range: StdRange<usize>) -> Range {
    Range {
        start: index.position(range.start),
        end: index.position(range.end),
    }
}
