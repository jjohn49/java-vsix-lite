//! Backend state, parsing, caches, and classpath management.
//! The LSP trait lives in `main.rs`; sibling modules extend [`Backend`].

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

use crate::diagnostics::StaleJavac;
use crate::javac;
use crate::workspace_index;
use crate::{infer_source_root, open_doc_path};

/// Cap on the external stub/source cache; cleared wholesale (not LRU) once
/// exceeded, since this path is cold and eviction pressure is low.
const EXTERNAL_CACHE_CAP: usize = 32;

/// Bound on the on-demand project-file cache, shared by goto-definition's
/// file lookup and find-references' per-hit-file confirm. Cleared wholesale
/// like [`EXTERNAL_CACHE_CAP`] when exceeded.
const PROJECT_FILE_CACHE_CAP: usize = 256;

/// Max size of an unopened source file [`Backend::parsed_project_file`]
/// will read and cache, guarding against a pathologically large file being
/// parsed cold. A file over this cap is treated exactly like an unreadable
/// one.
pub(crate) const MAX_PROJECT_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// A project source file parsed on demand and cached, invalidated by
/// `(mtime, len)` so an on-disk edit is picked up without an explicit
/// notification. `evict_project_file` forces a re-read regardless of both.
pub(crate) struct CachedProjectFile {
    pub(crate) mtime: SystemTime,
    pub(crate) len: u64,
    pub(crate) text: Arc<String>,
    pub(crate) tree: Tree,
}

/// An open document: current text, synced parse tree, and LSP version.
/// `rename` prefers versioned `TextDocumentEdit`s for open documents over
/// the client's `documentChanges` capability.
pub(crate) struct Document {
    pub(crate) text: String,
    pub(crate) tree: Tree,
    pub(crate) version: i32,
}

/// Debounce/coalescing decision state for classpath rebuilds, kept pure
/// and synchronous so it's easy to unit-test (see `tests` below). Elects
/// exactly one caller as the driver per rebuild cycle, so at most one
/// rebuild runs at a time and later events fold into it.
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

    /// A build-file change arrived. Returns `true` for the one caller that
    /// must drive the debounce/rebuild cycle; other callers get `false`
    /// since their event is already folded in.
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

    /// The debounce wait elapsed. Returns `true` to proceed with the
    /// rebuild, or `false` if a fresh event reset the window.
    pub(crate) fn on_debounce_elapsed(&mut self) -> bool {
        if self.dirty {
            self.dirty = false;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// The in-flight rebuild finished. Returns `true` if a follow-up cycle
    /// must run (an event arrived mid-rebuild), else `false` once settled.
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
    /// Open documents keyed by URI string: live text and parse tree.
    /// Closed project files are indexed separately via `workspace_index`.
    pub(crate) documents: Mutex<HashMap<String, Document>>,
    /// LSP position encoding negotiated during `initialize` (defaults to UTF-16).
    pub(crate) encoding: OnceLock<PositionEncoding>,
    /// Whether the client supports snippet completion (`$1` tab stops). Defaults
    /// to `false` until negotiated during `initialize`.
    pub(crate) snippet_support: OnceLock<bool>,
    /// Bytecode-backed symbols for imported types, built lazily on first
    /// use. A watched build-file change triggers a debounced rebuild that
    /// atomically swaps in a fresh `Classpath`; readers take their own
    /// `Arc` snapshot via `classpath()` and never hold this lock across an
    /// `.await`.
    classpath: StdRwLock<Option<Arc<jvl_classpath::Classpath>>>,
    /// Whether the client supports dynamic registration of
    /// `workspace/didChangeWatchedFiles`. Gates whether `initialized()`
    /// registers the build-file watch; a client without it never gets
    /// watched, since the LSP spec has no static alternative.
    pub(crate) classpath_watch_dynamic: OnceLock<bool>,
    /// Whether the client supports dynamic registration for type
    /// hierarchy, since `ServerCapabilities` has no static field to
    /// advertise it.
    pub(crate) type_hierarchy_dynamic: OnceLock<bool>,
    /// Debounce window for a classpath rebuild after a watched build-file
    /// change; 2s by default, overridable via `classpathDebounceMs` so
    /// tests don't need multi-second sleeps.
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
    /// Bounded accounting of reader threads abandoned by timed-out or
    /// cancelled runs. `javac::run` refuses to start once the cap is hit,
    /// so a misbehaving `jdk.home` binary can't leak threads without bound.
    pub(crate) javac_leaked_readers: javac::LeakedReaders,
    /// Diagnostics from the last `checkProject` run, keyed by URI, merged
    /// into `compute_diagnostics`'s result. Cleared for a file on its next
    /// `didChange`, and wholesale-replaced on each new `checkProject` run.
    pub(crate) javac_diagnostics: StdMutex<HashMap<String, Vec<Diagnostic>>>,
    /// Bumped on every event that can change an open document's
    /// diagnostics (edit, open, close, watched file event).
    /// `refresh_open_diagnostics` checks this before publishing, so a pass
    /// superseded by a newer edit never overwrites fresher results.
    pub(crate) semantic_generation: std::sync::atomic::AtomicU64,
    /// Bumped only when a *provider* changes meaning for other files: an
    /// accepted on-disk `.java` create/change/delete, an open document
    /// closing, or a declaration-level edit. A `javac` run that started
    /// before such a bump describes a workspace that no longer exists and
    /// must not publish. Not bumped by `did_open`/`did_save` (autosave
    /// would otherwise starve the compiler backstop).
    pub(crate) provider_generation: std::sync::atomic::AtomicU64,
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
            semantic_generation: std::sync::atomic::AtomicU64::new(0),
            provider_generation: std::sync::atomic::AtomicU64::new(0),
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

    /// The imported-type symbol source, built on first use from the JDK
    /// plus the project's declared dependencies.
    ///
    /// Returns a snapshot `Arc`, so a concurrent rebuild swap never
    /// invalidates work already in flight, and this method never holds
    /// `self.classpath`'s lock across an `.await`.
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

    /// Re-resolve the classpath from scratch on the blocking pool, using
    /// the same static offline resolution as the initial build, and swap
    /// it in atomically without holding the lock across an `.await`.
    async fn rebuild_classpath(&self) {
        let root = self.project_root();
        let built = tokio::task::spawn_blocking(move || {
            jvl_classpath::Classpath::from_jdk_and_project(root.as_deref())
        })
        .await
        .unwrap_or_else(|_| jvl_classpath::Classpath::empty());
        *self.classpath.write().expect("classpath lock poisoned") = Some(Arc::new(built));
    }

    /// Drive one or more debounce-then-rebuild cycles until settled. Only
    /// the caller that wins `RebuildCoalescer::on_event`'s race calls this;
    /// every other concurrent event folds into the cycle already running.
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
    /// opened file. Used for classpath discovery and as the base for
    /// conventional source-root candidates.
    pub(crate) fn project_root(&self) -> Option<PathBuf> {
        self.workspace_root
            .get()
            .and_then(|r| r.clone())
            .or_else(|| self.project_root_hint.get().and_then(|r| r.clone()))
    }

    /// Candidate source roots: the conventional `src/main/java` and
    /// `src/test/java` under the project root, plus one inferred from each
    /// open document's own path and `package` declaration. No directory
    /// walking; every root is either a fixed convention or derived from
    /// data already in memory.
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

    /// Candidate file paths for an FQN under every discovered source root.
    /// Empty when no project root is known, or when `fqn` isn't safe to
    /// turn into a path (guards against a crafted `package`/`import`
    /// escaping the root).
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

    /// Package implied by a file's location under fixed `src/main/java` or
    /// `src/test/java` roots. Unknown layouts and the bare workspace root stay silent.
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

    /// The lazy, bounded workspace symbol index, exposed for "simple name
    /// -> paths" lookups without re-walking the workspace. This accessor
    /// never triggers a build; only `ensure_built` does that.
    pub(crate) fn workspace_index(&self) -> &workspace_index::WorkspaceIndex {
        &self.workspace_index
    }

    /// Build the workspace type index before using `ProjectSymbols`.
    /// Call before locking `documents`, because this method locks it briefly.
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

    /// Read or reuse a project file cached by `(mtime, len)`; watched changes
    /// evict it explicitly. Oversized or unreadable files return `None`.
    pub(crate) fn parsed_project_file(&self, path: &Path) -> Option<(Arc<String>, Tree)> {
        let metadata = std::fs::metadata(path).ok()?;
        let len = metadata.len();
        if len > MAX_PROJECT_FILE_BYTES {
            return None;
        }
        let mtime = metadata.modified().ok()?;

        let mut cache = self
            .project_file_cache
            .lock()
            .expect("project file cache poisoned");
        let fresh = cache
            .get(path)
            .is_some_and(|c| c.mtime == mtime && c.len == len);
        if !fresh {
            let text = std::fs::read_to_string(path).ok()?;
            let tree = self.parse(&text, None);
            insert_bounded_project_file(
                &mut cache,
                PROJECT_FILE_CACHE_CAP,
                path.to_path_buf(),
                CachedProjectFile {
                    mtime,
                    len,
                    text: Arc::new(text),
                    tree,
                },
            );
        }
        let cached = cache.get(path)?;
        Some((Arc::clone(&cached.text), cached.tree.clone()))
    }

    /// Evict `path` (both its literal and canonicalized form) from the
    /// project-file cache, forcing the next [`Self::parsed_project_file`]
    /// call to re-read it regardless of `(mtime, len)` — needed since a
    /// same-size in-place rewrite can land within one mtime tick.
    pub(crate) fn evict_project_file(&self, path: &Path) {
        let mut cache = self
            .project_file_cache
            .lock()
            .expect("project file cache poisoned");
        cache.remove(path);
        if let Ok(canon) = std::fs::canonicalize(path) {
            if canon != path {
                cache.remove(&canon);
            }
        }
    }

    /// Parse (or reuse a cached parse of) a project source file candidate.
    /// The caller stops at the first hit, so at most one file is read per
    /// candidate tried — no directory walking or indexing.
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

    /// Parse a freshly opened (or fully replaced) document, store it, and
    /// recompute every open document's diagnostics, since opening a file
    /// can change what other already-open files can now resolve.
    pub(crate) async fn open_document(&self, uri: Uri, version: i32, text: String) {
        let tree = self.parse(&text, None);
        // Warm the classpath (first build scans the JDK/project deps and
        // can be slow) before taking the documents lock below; `classpath()`
        // is an idempotent, order-independent double-checked read/build.
        self.classpath();
        {
            let mut docs = self.documents.lock().await;
            docs.insert(
                uri.as_str().to_string(),
                Document {
                    text,
                    tree,
                    version,
                },
            );
            self.semantic_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.refresh_open_diagnostics(StaleJavac::None).await;
    }

    /// `REBUILD_CLASSPATH_COMMAND`: an immediate, synchronous classpath
    /// rebuild (unlike `drive_classpath_rebuild`'s debounced version), used
    /// after a dependency install so `jvl/missingDependencies` reflects the
    /// just-installed jars. Never touches the network itself; it only
    /// re-reads what the extension already placed under `~/.m2`.
    pub(crate) async fn run_rebuild_classpath_command(&self) -> serde_json::Value {
        self.rebuild_classpath().await;
        self.republish_all_diagnostics().await;
        serde_json::json!({ "status": "ok" })
    }
}

/// Whether every dotted segment of a fully-qualified name is a safe, single
/// path component. Guards file lookup against a crafted `package`/`import`
/// escaping the source root (e.g. via a `..` segment).
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
/// `ClassInfo`, used when no `-sources.jar`/`src.zip` entry exists for an
/// external type. Reuses the member signatures `jvl-classpath` already
/// rendered rather than re-deriving them.
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
/// the cap is reached. A free function (not a `Backend` method) so the
/// overflow behavior is unit-testable without constructing a `Client`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::LspService;

    /// A bare `Backend` for a direct (non-LSP-transport) unit test — mirrors
    /// `main.rs`'s own `test_backend` helper (private to that module's test
    /// suite, so duplicated here rather than shared).
    fn test_backend() -> LspService<Backend> {
        let (service, _socket) = LspService::new(Backend::new);
        service
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jvl-backend-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A same-mtime, different-length content change must be picked up: the
    /// `(mtime, len)` cache key catches what an `mtime`-only key would miss
    /// whenever a rewrite happens to land within one filesystem mtime tick.
    #[test]
    fn parsed_project_file_rereads_when_length_changes_at_same_mtime() {
        let service = test_backend();
        let backend = service.inner();
        let dir = temp_dir("len-change");
        let path = dir.join("Foo.java");

        std::fs::write(&path, "class Foo {}\n").expect("write");
        let (text, _) = backend.parsed_project_file(&path).expect("first parse");
        assert_eq!(text.as_str(), "class Foo {}\n");
        let mtime = std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");

        // Different length, same mtime forced back onto the file.
        std::fs::write(&path, "class FooRenamed {}\n").expect("rewrite");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen for mtime reset")
            .set_modified(mtime)
            .expect("reset mtime");

        let (text, _) = backend.parsed_project_file(&path).expect("second parse");
        assert_eq!(
            text.as_str(),
            "class FooRenamed {}\n",
            "a different length at the same mtime must force a re-read"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `evict_project_file` forces a re-read even when a rewrite happens to
    /// land on both the exact same mtime AND the exact same length — the one
    /// case the `(mtime, len)` cache key alone cannot distinguish from "the
    /// file didn't change".
    #[test]
    fn evict_project_file_forces_reread_even_when_mtime_and_len_unchanged() {
        let service = test_backend();
        let backend = service.inner();
        let dir = temp_dir("evict");
        let path = dir.join("Foo.java");

        std::fs::write(&path, "class Foo1 {}\n").expect("write");
        let (text, _) = backend.parsed_project_file(&path).expect("first parse");
        assert!(text.contains("Foo1"));
        let mtime = std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");

        // Same length ("Foo1" / "Foo2"), same mtime forced back onto the file.
        std::fs::write(&path, "class Foo2 {}\n").expect("rewrite, same length");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen for mtime reset")
            .set_modified(mtime)
            .expect("reset mtime");

        let (stale, _) = backend
            .parsed_project_file(&path)
            .expect("still-cached parse");
        assert!(
            stale.contains("Foo1"),
            "sanity: an unchanged (mtime, len) key must still serve the stale cached text"
        );

        backend.evict_project_file(&path);
        let (fresh, _) = backend
            .parsed_project_file(&path)
            .expect("post-evict parse");
        assert!(
            fresh.contains("Foo2"),
            "evict_project_file must force a re-read even with an unchanged (mtime, len) key"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
