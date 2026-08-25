//! Diagnostics and diagnostic publication: syntax/unresolved-member/
//! structural diagnostics for a single document, republishing every open
//! document's diagnostics after a classpath rebuild, and the `javac`
//! check-project orchestration that merges compiler diagnostics in.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jvl_syntax::LineIndex;
use tower_lsp_server::ls_types::*;

use crate::backend::{Backend, Document};
use crate::javac;
use crate::{filename_from_uri, infer_source_root, open_docs, ClasspathSymbols};

/// RAII guard releasing `Backend::javac_running` on drop — see
/// `Backend::execute_command`.
pub(crate) struct JavacRunningGuard<'a>(pub(crate) &'a std::sync::atomic::AtomicBool);

impl Drop for JavacRunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The result of the shared javac-invocation machinery ([`Backend::execute_javac_check`]),
/// letting the project- and module-scoped callers decide how to publish.
enum CheckExecution {
    /// A terminal JSON result to return to the client verbatim (javac not
    /// found, timed out, cancelled, spawn error, jdk-too-old, or "no sources").
    Terminal(serde_json::Value),
    /// `javac` completed; here are its parsed/grouped diagnostics (keyed by the
    /// path `javac` echoed) and severity counts, for the caller to publish.
    Completed {
        grouped: HashMap<String, Vec<Diagnostic>>,
        error_count: usize,
        warning_count: usize,
    },
}

/// Parse an LSP document URI into a `file:` filesystem path that ends in
/// `.java`. Uses the URI type's own parser (never manual string surgery) so
/// percent-encoding and platform path shapes are handled correctly. `None`
/// for a non-`file:` URI or one that doesn't name a `.java` file.
fn parse_java_file_uri(uri_str: &str) -> Option<PathBuf> {
    let uri: Uri = uri_str.parse().ok()?;
    let path = uri.to_file_path()?.into_owned();
    path.extension()
        .is_some_and(|e| e == "java")
        .then_some(path)
}

/// Whether `path` lives under any of `module_roots`. Checks the literal path
/// first, then the canonicalized path — so a path keyed non-canonically (e.g.
/// macOS `/var` vs the canonical `/private/var`, as a prior full-project run's
/// stored diagnostics may be) is still recognized as belonging to a checked
/// module. A missing/unreadable path falls back to the literal check only (it
/// can't escape via a symlink if it doesn't resolve).
fn path_under_any_module(path: &Path, module_roots: &[PathBuf]) -> bool {
    if module_roots.iter().any(|m| path.starts_with(m)) {
        return true;
    }
    std::fs::canonicalize(path)
        .map(|canon| module_roots.iter().any(|m| canon.starts_with(m)))
        .unwrap_or(false)
}

/// [`path_under_any_module`] for a document URI key — parses the `file:` URI to
/// a path first; `false` for a non-`file:` URI.
fn uri_is_under_module(uri_str: &str, module_roots: &[PathBuf]) -> bool {
    let Ok(uri) = uri_str.parse::<Uri>() else {
        return false;
    };
    let Some(path) = uri.to_file_path() else {
        return false;
    };
    path_under_any_module(path.as_ref(), module_roots)
}

/// Native codes `javac` can independently confirm: the type-directed return
/// and initializer checks and the unreachable-statement check. `jvl.unused`
/// is deliberately absent — javac has no equivalent diagnostic, so an unused
/// warning is never deduplicated.
const JAVAC_CONFIRMABLE_CODES: [&str; 3] = [
    jvl_syntax::INCOMPATIBLE_RETURN_CODE,
    jvl_syntax::INCOMPATIBLE_ASSIGNMENT_CODE,
    jvl_syntax::UNREACHABLE_CODE,
];

/// Whether two first message lines carry the same payload. javac folds
/// `required:`/`found:` continuation lines into its message, so only the
/// first line is comparable; the `incompatible types: ` prefix strips ONLY
/// when present on BOTH sides — otherwise the lines compare verbatim (javac
/// emits `unreachable statement` exactly, with no prefix).
fn equivalent_payload(native: &str, javac: &str) -> bool {
    let native = native.lines().next().unwrap_or("");
    let javac = javac.lines().next().unwrap_or("");
    match (
        native.strip_prefix("incompatible types: "),
        javac.strip_prefix("incompatible types: "),
    ) {
        (Some(native), Some(javac)) => native == javac,
        _ => native == javac,
    }
}

/// Whether `javac` confirms `native` as the same diagnostic. Deliberately
/// narrow: the native code is allowlisted; incompatible-type rules are native
/// errors while unreachable code is intentionally a native warning; javac is
/// always an error; ranges overlap on one line; and first-line payloads agree.
/// The severity exception lets the compiler replace (not duplicate) the
/// friendlier immediate unreachable warning after a save.
fn javac_confirms_native(native: &Diagnostic, javac: &Diagnostic) -> bool {
    let Some(NumberOrString::String(code)) = &native.code else {
        return false;
    };
    if !JAVAC_CONFIRMABLE_CODES.contains(&code.as_str()) {
        return false;
    }
    let expected_native_severity = if code == jvl_syntax::UNREACHABLE_CODE {
        DiagnosticSeverity::WARNING
    } else {
        DiagnosticSeverity::ERROR
    };
    native.severity == Some(expected_native_severity)
        && javac.severity == Some(DiagnosticSeverity::ERROR)
        && native.range.start.line == javac.range.start.line
        && native.range.start.character < javac.range.end.character
        && javac.range.start.character < native.range.end.character
        && equivalent_payload(&native.message, &javac.message)
}

/// Merge a file's stored `javac` diagnostics into its freshly computed
/// native set, preferring the compiler for an equivalent current result:
/// every native entry some javac diagnostic confirms (see
/// [`javac_confirms_native`]) is removed, then ALL javac diagnostics
/// are appended in their original order. Surviving natives keep their
/// order; unrelated diagnostics are never deduplicated.
fn merge_javac_diagnostics(diagnostics: &mut Vec<Diagnostic>, javac: Vec<Diagnostic>) {
    diagnostics.retain(|native| {
        !javac
            .iter()
            .any(|javac| javac_confirms_native(native, javac))
    });
    diagnostics.extend(javac);
}

impl Backend {
    /// Recompute and republish diagnostics for every currently open
    /// document — used after a classpath swap, since
    /// unresolved-member diagnostics depend on it and may change once new
    /// dependency types become resolvable.
    ///
    /// Locks `self.documents` exactly once: every open document's
    /// diagnostics (unresolved-member diagnostics fully included — see
    /// `compute_diagnostics`) are computed into an owned snapshot while the
    /// lock is held, then the lock is dropped before the `publish_diagnostics`
    /// `.await`s that follow, so it is never held across an `.await`.
    pub(crate) async fn republish_all_diagnostics(&self) {
        let snapshot: Vec<(String, Vec<Diagnostic>, i32)> = {
            let docs = self.documents.lock().await;
            docs.iter()
                .map(|(uri_str, doc)| {
                    (
                        uri_str.clone(),
                        self.compute_diagnostics(&docs, uri_str),
                        doc.version,
                    )
                })
                .collect()
        };
        for (uri_str, diagnostics, version) in snapshot {
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, Some(version))
                    .await;
            }
        }
    }

    /// Syntax, immediate semantic, and structural diagnostics for a document
    /// already stored under `uri`, plus any `javac` diagnostics still on file
    /// for `uri` — merged in via [`merge_javac_diagnostics`], which removes a
    /// native error that an equivalent compiler result confirms and
    /// appends every javac entry, never clobbering anything unrelated.
    /// Unlike the native
    /// diagnostics, the `javac` diagnostics don't require `uri` to be an open
    /// document: a checked file the editor never opened still gets its
    /// diagnostics published (see `Backend::publish_javac_diagnostics`).
    pub(crate) fn compute_diagnostics(
        &self,
        docs: &HashMap<String, Document>,
        uri: &str,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = match docs.get(uri) {
            Some(doc) => {
                let index = LineIndex::new(&doc.text, self.encoding());
                let mut d = jvl_syntax::syntax_diagnostics(&doc.tree, &index);
                let open = open_docs(docs, uri, doc);
                // Classpath-only, not `CombinedSymbols` — this is a
                // synchronous fn on the didOpen/didChange hot path, and
                // `ProjectSymbols` needs an async `ensure_workspace_index`
                // pass first. Conservative semantic checks stay silent when
                // project/classpath resolution is incomplete, so a closed-file
                // project type is a missed diagnosis, never a false positive.
                let symbols = ClasspathSymbols(self.classpath());
                d.extend(jvl_syntax::semantic_diagnostics(
                    &open,
                    0,
                    &index,
                    &symbols,
                    self.unresolved_member_diagnostics
                        .get()
                        .copied()
                        .unwrap_or(true),
                    self.unused_diagnostics.get().copied().unwrap_or(true),
                ));
                let filename = filename_from_uri(uri);
                let expected_package = self.expected_package(uri);
                d.extend(jvl_syntax::structural_diagnostics(
                    &doc.tree,
                    &doc.text,
                    &index,
                    filename.as_deref(),
                    expected_package.as_deref(),
                ));
                d
            }
            None => Vec::new(),
        };
        if let Some(javac_diags) = self
            .javac_diagnostics
            .lock()
            .expect("javac diagnostics poisoned")
            .get(uri)
        {
            merge_javac_diagnostics(&mut diagnostics, javac_diags.clone());
        }
        diagnostics
    }

    /// The `javacTimeoutSecs` initialization option, already clamped.
    fn javac_timeout(&self) -> Duration {
        Duration::from_secs(self.javac_timeout_secs.get().copied().unwrap_or(120))
    }

    /// The check-project run (`jvl.checkProject.run`, forwarded by the
    /// extension's trust-gated `java-vsix-lite.checkProject`) — see the module
    /// doc comment on `javac` for the security invariants this must never
    /// violate.
    ///
    /// `scope` selects the whole workspace (the manual command — unchanged
    /// behavior, full diagnostic-map replacement) or a set of saved documents'
    /// modules (the automatic on-save/on-load check — compiles only affected
    /// modules with sibling sources reachable via `-sourcepath`, and replaces
    /// diagnostics only within those modules). See [`javac::JavacCheckScope`].
    ///
    /// Concurrency (one run at a time) is enforced by the caller
    /// (`execute_command`), which claims `javac_running` before calling this
    /// and releases it afterward; this method assumes that's already done.
    pub(crate) async fn run_check_project(
        &self,
        scope: javac::JavacCheckScope,
    ) -> serde_json::Value {
        match scope {
            javac::JavacCheckScope::Project => self.run_check_full_project().await,
            javac::JavacCheckScope::Modules { document_uris } => {
                self.run_check_scoped(document_uris).await
            }
        }
    }

    /// The full-workspace check: every discovered source root compiled as one
    /// explicit input set, replacing the entire javac diagnostic map. This is
    /// the manual `Java: Check Project (javac)` behavior, unchanged.
    async fn run_check_full_project(&self) -> serde_json::Value {
        let Some(project_root) = self.project_root() else {
            return serde_json::json!({
                "status": "error",
                "message": "no project root (open a workspace folder or a file under a Maven/Gradle project)",
            });
        };
        let classpath = self.classpath();
        // Snapshot every open document's version under the SAME lock hold
        // that derives the source roots: this is the revision baseline the
        // publication step compares against, so a result computed from these
        // roots can never be attributed to a newer buffer state.
        let (mut roots, start_versions) = {
            let docs = self.documents.lock().await;
            let mut roots = self.source_roots(&docs, &project_root);
            roots.extend(classpath.source_roots().iter().cloned());
            let start_versions: HashMap<String, i32> = docs
                .iter()
                .map(|(uri_str, doc)| (uri_str.clone(), doc.version))
                .collect();
            (roots, start_versions)
        };
        // Cover every Maven/Gradle module in the workspace, not just the root
        // module + currently-open files — so a full-workspace check is actually
        // complete on a multi-module project (and the scoped-check fallback that
        // routes here is authoritative). `collect_source_files` dedups roots.
        // Uses the project root as-is (not canonicalized): the paths must stay
        // in the same space as the client's document URIs so published
        // diagnostics attach to the right files.
        roots.extend(javac::discover_workspace_source_roots(&project_root));
        match self
            .execute_javac_check(&project_root, roots, Vec::new())
            .await
        {
            CheckExecution::Terminal(value) => value,
            CheckExecution::Completed {
                grouped,
                error_count,
                warning_count,
            } => {
                // Whole-project run: the map is authoritative for every file.
                self.publish_javac_diagnostics(grouped, &start_versions)
                    .await;
                serde_json::json!({
                    "status": "ok",
                    "errorCount": error_count,
                    "warningCount": warning_count,
                })
            }
        }
    }

    /// The scoped (automatic) check: resolve each saved document to its
    /// Maven/Gradle module, compile only those modules (with every workspace
    /// source root on `-sourcepath` so sibling sources resolve without being
    /// compiled eagerly), and replace javac diagnostics only within the
    /// checked modules. A malformed/empty request is an error — never widened
    /// into a project compile — and a compiler diagnostic against a file
    /// *outside* the checked modules means the scoped view is incomplete, so
    /// the run transparently falls back to one full-project check.
    async fn run_check_scoped(&self, document_uris: Vec<String>) -> serde_json::Value {
        if document_uris.is_empty() {
            return serde_json::json!({
                "status": "error",
                "message": "scoped check requires at least one document URI",
            });
        }
        let Some(project_root) = self.project_root() else {
            return serde_json::json!({
                "status": "error",
                "message": "no project root (open a workspace folder or a file under a Maven/Gradle project)",
            });
        };
        // Canonicalized workspace root — used ONLY for the security containment
        // check (which must resolve symlinks). Module resolution, source
        // collection, and diagnostic keying all use the *original* path space
        // so published diagnostics attach to the client's document URIs.
        let Ok(workspace_canonical) = std::fs::canonicalize(&project_root) else {
            return serde_json::json!({
                "status": "error",
                "message": "could not canonicalize the workspace root",
            });
        };

        // Resolve every saved document to an in-workspace `.java` file and the
        // module that owns it. Any invalid/outside URI is a hard error (never
        // silently widened to a project compile).
        let mut module_roots: Vec<PathBuf> = Vec::new();
        let mut explicit_roots: Vec<PathBuf> = Vec::new();
        // Same-lock version snapshot as `run_check_full_project`: the
        // revision baseline handed to `publish_javac_diagnostics_scoped`.
        let start_versions: HashMap<String, i32> = {
            let docs = self.documents.lock().await;
            for uri_str in &document_uris {
                let Some(file) = parse_java_file_uri(uri_str) else {
                    return serde_json::json!({
                        "status": "error",
                        "message": format!("not a file: .java URI: {uri_str}"),
                    });
                };
                // Security gate only: reject anything whose real (symlink-
                // resolved) path escapes the workspace. The canonical result is
                // deliberately NOT used for resolution below.
                if javac::canonical_within_workspace(&file, &workspace_canonical).is_none() {
                    return serde_json::json!({
                        "status": "error",
                        "message": format!("document is outside the workspace or unreadable: {uri_str}"),
                    });
                }

                let module_root = match javac::nearest_module_root(&file, &project_root) {
                    Some(root) => root,
                    None => {
                        // No build marker up to the workspace root: fall back to
                        // the workspace root only if the file sits under a
                        // conventional source root there; otherwise it's an
                        // unsupported layout for a scoped check.
                        let src_roots = javac::conventional_source_roots(&project_root);
                        if src_roots.iter().any(|r| file.starts_with(r)) {
                            project_root.clone()
                        } else {
                            return serde_json::json!({
                                "status": "unsupported-layout",
                                "message": format!(
                                    "{uri_str} is not inside a Maven/Gradle module or a conventional source root; use Check Project (javac) instead"
                                ),
                            });
                        }
                    }
                };

                if !module_roots.contains(&module_root) {
                    module_roots.push(module_root.clone());
                    explicit_roots.extend(javac::conventional_source_roots(&module_root));
                }
                // A nonstandard-layout file may sit outside src/main|test/java:
                // add its confidently inferred source root too, but only when it
                // stays inside this module so a scoped compile can't pull in
                // unrelated trees.
                if let Some(doc) = docs.get(uri_str) {
                    if let Some(inferred) = infer_source_root(uri_str, &doc.tree, &doc.text) {
                        if inferred.starts_with(&module_root) && !explicit_roots.contains(&inferred)
                        {
                            explicit_roots.push(inferred);
                        }
                    }
                }
            }
            docs.iter()
                .map(|(uri_str, doc)| (uri_str.clone(), doc.version))
                .collect()
        };

        // Sibling-module sources on `-sourcepath`: a bounded, directory-only
        // scan for module source roots, plus any dependency source roots.
        let classpath = self.classpath();
        let mut sourcepath_roots = javac::discover_workspace_source_roots(&project_root);
        sourcepath_roots.extend(classpath.source_roots().iter().cloned());

        match self
            .execute_javac_check(&project_root, explicit_roots, sourcepath_roots)
            .await
        {
            CheckExecution::Terminal(value) => value,
            CheckExecution::Completed {
                grouped,
                error_count,
                warning_count,
            } => {
                // If javac flagged a source outside the checked modules (a
                // sibling reached through -sourcepath), the scoped result is
                // incomplete — never publish a partial/misleading result.
                // Redo the run as a full project check instead.
                let external = grouped
                    .keys()
                    .any(|path| !path_under_any_module(Path::new(path), &module_roots));
                if external {
                    return self.run_check_full_project().await;
                }
                self.publish_javac_diagnostics_scoped(grouped, &module_roots, &start_versions)
                    .await;
                serde_json::json!({
                    "status": "ok",
                    "scope": "modules",
                    "errorCount": error_count,
                    "warningCount": warning_count,
                })
            }
        }
    }

    /// Shared javac-invocation core for both scopes: locate `javac`, collect
    /// the explicit source files from `explicit_roots`, apply the JDK/project
    /// source-level and jdk-too-old guard (keyed off `release_root`), run the
    /// compiler with `sourcepath_roots` on `-sourcepath`, and parse the result.
    /// Publishing is left to the caller (project- vs module-scoped).
    async fn execute_javac_check(
        &self,
        release_root: &Path,
        explicit_roots: Vec<PathBuf>,
        sourcepath_roots: Vec<PathBuf>,
    ) -> CheckExecution {
        let javac_path = match javac::locate_javac(
            self.jdk_home_override.get().and_then(|o| o.as_deref()),
        ) {
            Some(path) => path,
            None => {
                return CheckExecution::Terminal(serde_json::json!({
                    "status": "javac-not-found",
                    "message": "could not locate javac: set $JAVA_HOME or the java-vsix-lite.jdk.home setting (never downloaded)",
                }));
            }
        };

        let source_files = javac::collect_source_files(&explicit_roots);
        if source_files.is_empty() {
            return CheckExecution::Terminal(serde_json::json!({
                "status": "error",
                "message": "no .java source files found under the discovered source roots",
            }));
        }

        // JDK level: the JDK home is `<home>/bin/javac`; read its feature
        // version (no process spawn). Project level: read statically from the
        // build files. Together they pick the language level below.
        let jdk_release = javac_path
            .parent()
            .and_then(|bin| bin.parent())
            .and_then(jvl_classpath::jdk_feature_version);
        let project_release = jvl_classpath::project_java_release(release_root);

        // If the project targets a newer Java than the newest detected JDK,
        // javac can't compile it and would emit a flood of "not supported in
        // -source N" noise. Publish one clear diagnostic on the build file and
        // skip the run entirely.
        if let (Some(proj), Some(jdk)) = (project_release, jdk_release) {
            if jdk < proj {
                self.publish_jdk_too_old(release_root, proj, jdk).await;
                return CheckExecution::Terminal(serde_json::json!({
                    "status": "jdk-too-old",
                    "message": format!(
                        "project targets Java {proj} but the newest detected JDK is {jdk}; javac check skipped"
                    ),
                }));
            }
        }

        // Language level: compile faithfully at the project's declared release
        // when known (validated against that release's API), enabling preview
        // only when it matches the JDK's own version; else fall back to the
        // JDK's level with preview on; or a bare compile when neither is known.
        let source_level = match (project_release, jdk_release) {
            (Some(release), Some(jdk)) => javac::SourceLevel::Release {
                release,
                preview: release == jdk,
            },
            (None, Some(jdk)) => javac::SourceLevel::JdkDefault(jdk),
            (_, None) => javac::SourceLevel::None,
        };
        let classpath = self.classpath();
        let config = javac::RunConfig {
            javac_path,
            source_files,
            classpath_entries: classpath.entries().to_vec(),
            sourcepath_entries: sourcepath_roots,
            timeout: self.javac_timeout(),
            source_level,
        };
        let slot = Arc::clone(&self.javac_child);
        let leaked = self.javac_leaked_readers.clone();
        let outcome = tokio::task::spawn_blocking(move || javac::run(config, &slot, &leaked))
            .await
            .unwrap_or_else(|err| {
                javac::RunOutcome::SpawnError(format!("javac task panicked: {err}"))
            });

        match outcome {
            javac::RunOutcome::TimedOut => CheckExecution::Terminal(serde_json::json!({
                "status": "timeout",
                "message": format!("javac timed out after {}s and was killed", self.javac_timeout().as_secs()),
            })),
            javac::RunOutcome::Cancelled => CheckExecution::Terminal(serde_json::json!({
                "status": "cancelled",
                "message": "javac was killed by server shutdown",
            })),
            javac::RunOutcome::SpawnError(message) => CheckExecution::Terminal(serde_json::json!({
                "status": "spawn-error",
                "message": message,
            })),
            javac::RunOutcome::Completed { stderr } => {
                let raw = javac::parse_stderr(&stderr);
                let (error_count, warning_count) = javac::count_severities(&raw);
                let grouped = javac::group_diagnostics(raw);
                CheckExecution::Completed {
                    grouped,
                    error_count,
                    warning_count,
                }
            }
        }
    }

    /// Publish a single diagnostic on the project's build file explaining that
    /// the detected JDK is too old for the project's declared Java level and
    /// the javac check was skipped. Routed through
    /// [`Self::publish_javac_diagnostics`] so it clears on the next run like
    /// any other javac diagnostic.
    async fn publish_jdk_too_old(&self, project_root: &Path, project: u32, jdk: u32) {
        let Some(build_file) = ["pom.xml", "build.gradle", "build.gradle.kts"]
            .iter()
            .map(|n| project_root.join(n))
            .find(|p| p.is_file())
        else {
            return;
        };
        let diag = Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("java-vsix-lite".to_string()),
            message: format!(
                "This project targets Java {project}, but the newest detected JDK is {jdk}. \
                 The javac check was skipped (it cannot compile Java {project} sources). \
                 Install a JDK {project} or newer, or set `java-vsix-lite.jdk.home` to one. \
                 Highlighting, completion, and navigation work regardless."
            ),
            ..Default::default()
        };
        let mut map: HashMap<String, Vec<Diagnostic>> = HashMap::new();
        map.insert(build_file.to_string_lossy().into_owned(), vec![diag]);
        // No compile ran, so nothing raced an edit: a fresh version snapshot
        // makes the publication's revision filter vacuously current.
        let start_versions: HashMap<String, i32> = {
            let docs = self.documents.lock().await;
            docs.iter()
                .map(|(uri_str, doc)| (uri_str.clone(), doc.version))
                .collect()
        };
        self.publish_javac_diagnostics(map, &start_versions).await;
    }

    /// Replace the javac-diagnostics set wholesale with `new_diags`
    /// (keyed by filesystem path, as `javac` echoed it) and (re)publish
    /// every affected URI — both newly (or still) diagnosed files and any
    /// file that had javac diagnostics before this run but doesn't anymore
    /// (which must be published empty-of-javac to actually clear in the
    /// client's Problems panel; LSP has no "leave unchanged" — an omitted
    /// publish just means "nothing changed", not "clear"). Merges with each
    /// file's other diagnostics via `compute_diagnostics`, never clobbering.
    ///
    /// `start_versions` is the caller's open-document version snapshot,
    /// taken under the documents lock before the compiler ran: a result for
    /// a file that is open now is discarded unless its start version is
    /// present and equal to the current `Document.version` (absent or
    /// different means an edit raced the compile — the result is stale).
    /// Closed files can't be edited in flight, so they always publish.
    ///
    /// Lock order: the revision filter, the javac-map swap, and the
    /// republish snapshot all happen under ONE `documents` lock hold,
    /// serializing them against `did_change` (which bumps the version and
    /// removes the file's javac entry inside its own documents critical
    /// section). The `javac_diagnostics` guard is scoped shut before
    /// `compute_diagnostics` re-acquires it per URI, and the documents lock
    /// is dropped before the publish `.await`s. Live buffers publish with
    /// `Some(version)`; closed files with `None`.
    async fn publish_javac_diagnostics(
        &self,
        new_diags: HashMap<String, Vec<Diagnostic>>,
        start_versions: &HashMap<String, i32>,
    ) {
        let new_map: HashMap<String, Vec<Diagnostic>> = new_diags
            .into_iter()
            .filter_map(|(path, diags)| {
                Uri::from_file_path(Path::new(&path)).map(|uri| (uri.as_str().to_string(), diags))
            })
            .collect();

        let snapshot: Vec<(String, Vec<Diagnostic>, Option<i32>)> = {
            let docs = self.documents.lock().await;
            let new_map: HashMap<String, Vec<Diagnostic>> = new_map
                .into_iter()
                .filter(|(uri_str, _)| match docs.get(uri_str) {
                    Some(doc) => start_versions.get(uri_str) == Some(&doc.version),
                    None => true,
                })
                .collect();

            let affected: Vec<String> = {
                let mut map = self
                    .javac_diagnostics
                    .lock()
                    .expect("javac diagnostics poisoned");
                let mut affected: HashSet<String> = map.keys().cloned().collect();
                affected.extend(new_map.keys().cloned());
                *map = new_map;
                affected.into_iter().collect()
            };

            affected
                .into_iter()
                .map(|uri_str| {
                    let diagnostics = self.compute_diagnostics(&docs, &uri_str);
                    let version = docs.get(&uri_str).map(|doc| doc.version);
                    (uri_str, diagnostics, version)
                })
                .collect()
        };

        for (uri_str, diagnostics, version) in snapshot {
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, version)
                    .await;
            }
        }
    }

    /// Scoped counterpart to [`Self::publish_javac_diagnostics`]: replace the
    /// javac diagnostics only for files **beneath `module_roots`**, preserving
    /// every other module's diagnostics untouched. The same revision filter,
    /// single documents-lock hold, and versioned publication apply — see the
    /// project-wide function's doc comment for the lock-order reasoning.
    ///
    /// Existing entries under a checked module root are dropped (this run is
    /// authoritative for them); `new_diags` are inserted; any dropped file not
    /// re-added was clean this run and is republished empty-of-javac so it
    /// clears in Problems. Only files inside the checked modules are ever
    /// republished — no diagnostics are touched outside the requested scope.
    async fn publish_javac_diagnostics_scoped(
        &self,
        new_diags: HashMap<String, Vec<Diagnostic>>,
        module_roots: &[PathBuf],
        start_versions: &HashMap<String, i32>,
    ) {
        let new_map: HashMap<String, Vec<Diagnostic>> = new_diags
            .into_iter()
            .filter_map(|(path, diags)| {
                Uri::from_file_path(Path::new(&path)).map(|uri| (uri.as_str().to_string(), diags))
            })
            .collect();

        let snapshot: Vec<(String, Vec<Diagnostic>, Option<i32>)> = {
            let docs = self.documents.lock().await;
            // Same revision filter as `publish_javac_diagnostics`.
            let new_map: HashMap<String, Vec<Diagnostic>> = new_map
                .into_iter()
                .filter(|(uri_str, _)| match docs.get(uri_str) {
                    Some(doc) => start_versions.get(uri_str) == Some(&doc.version),
                    None => true,
                })
                .collect();

            let affected: Vec<String> = {
                let mut map = self
                    .javac_diagnostics
                    .lock()
                    .expect("javac diagnostics poisoned");
                let mut affected: HashSet<String> = HashSet::new();
                // Drop prior diagnostics for files inside the checked modules —
                // recording each as affected so a now-clean file is republished
                // (and thereby cleared). Other modules' entries are retained.
                map.retain(|uri_str, _| {
                    if uri_is_under_module(uri_str, module_roots) {
                        affected.insert(uri_str.clone());
                        false
                    } else {
                        true
                    }
                });
                // Insert this run's diagnostics (each also affected), overwriting.
                for (uri_str, diags) in new_map {
                    affected.insert(uri_str.clone());
                    map.insert(uri_str, diags);
                }
                affected.into_iter().collect()
            };

            affected
                .into_iter()
                .map(|uri_str| {
                    let diagnostics = self.compute_diagnostics(&docs, &uri_str);
                    let version = docs.get(&uri_str).map(|doc| doc.version);
                    (uri_str, diagnostics, version)
                })
                .collect()
        };

        for (uri_str, diagnostics, version) in snapshot {
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, version)
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Task 2 native return contract's message for the canonical
    /// wrong-return fixture (`int code() { return "bad"; }`).
    const RETURN_MESSAGE: &str = "incompatible types: String cannot be converted to int";

    /// A native diagnostic exactly as the syntax crate emits it for `code`:
    /// severity ERROR, source `java-vsix-lite`, single-line range.
    fn native_coded(code: &str, line: u32, start: u32, end: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String(code.to_string())),
            source: Some("java-vsix-lite".to_string()),
            message: message.to_string(),
            ..Default::default()
        }
    }

    /// A native return diagnostic exactly as `jvl_syntax`'s return check
    /// emits it (Task 2 contract): code `jvl.incompatibleReturn`, severity
    /// ERROR, source `java-vsix-lite`, single-line range on the returned
    /// expression.
    fn native_return(line: u32, start: u32, end: u32, message: &str) -> Diagnostic {
        native_coded(
            jvl_syntax::INCOMPATIBLE_RETURN_CODE,
            line,
            start,
            end,
            message,
        )
    }

    /// A native diagnostic that is NOT the return rule — a syntax,
    /// structural, or member diagnostic (no `jvl.incompatibleReturn` code).
    fn native_other(line: u32, start: u32, end: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("java-vsix-lite".to_string()),
            message: message.to_string(),
            ..Default::default()
        }
    }

    /// A compiler diagnostic exactly as `javac::group_diagnostics` builds it:
    /// source `javac`, no code, column-derived single-line range.
    fn javac_diag(
        line: u32,
        start: u32,
        end: u32,
        severity: DiagnosticSeverity,
        message: &str,
    ) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
            severity: Some(severity),
            source: Some("javac".to_string()),
            message: message.to_string(),
            ..Default::default()
        }
    }

    /// An equivalent current javac result replaces the native return
    /// diagnostic instead of duplicating it: the native entry is removed and
    /// the javac diagnostic is appended, leaving exactly one javac-sourced
    /// incompatibility.
    #[test]
    fn merge_replaces_equivalent_native_return_diagnostic() {
        let mut merged = vec![native_return(4, 15, 20, RETURN_MESSAGE)];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                15,
                21,
                DiagnosticSeverity::ERROR,
                RETURN_MESSAGE,
            )],
        );
        assert_eq!(
            merged.len(),
            1,
            "expected the native entry replaced: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("javac"));
        assert_eq!(merged[0].message, RETURN_MESSAGE);
    }

    /// Equivalence compares only the FIRST message line, after stripping the
    /// `incompatible types: ` prefix — javac folds `required:`/`found:`
    /// continuation lines into its message and they must not defeat the
    /// match. A line-wide javac range (no caret parsed → character 0 to
    /// u32::MAX) still overlaps the native expression range on that line.
    #[test]
    fn merge_matches_first_message_line_and_line_wide_javac_range() {
        let folded = format!("{RETURN_MESSAGE}\n  required: int\n  found:    String");
        let mut merged = vec![native_return(4, 15, 20, RETURN_MESSAGE)];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                0,
                u32::MAX,
                DiagnosticSeverity::ERROR,
                &folded,
            )],
        );
        assert_eq!(
            merged.len(),
            1,
            "expected the native entry replaced: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("javac"));
    }

    /// A different first-line payload (after stripping `incompatible
    /// types: `) is NOT equivalent — both diagnostics survive.
    #[test]
    fn merge_requires_equal_stripped_payload() {
        let mut merged = vec![native_return(4, 15, 20, RETURN_MESSAGE)];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                15,
                21,
                DiagnosticSeverity::ERROR,
                "incompatible types: String cannot be converted to long",
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "differing payloads must never merge: {merged:#?}"
        );
    }

    /// Only a native diagnostic carrying the `jvl.incompatibleReturn` code
    /// can be replaced. A same-line, same-message native diagnostic WITHOUT
    /// that code (syntax/structural/member rules) is never deduplicated.
    #[test]
    fn merge_requires_native_return_code() {
        let mut merged = vec![native_other(4, 15, 20, RETURN_MESSAGE)];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                15,
                21,
                DiagnosticSeverity::ERROR,
                RETURN_MESSAGE,
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "a non-return native diagnostic must never be removed: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(merged[1].source.as_deref(), Some("javac"));
    }

    /// Both severities must be ERROR: a javac WARNING never confirms (and so
    /// never removes) a native return error, and a hypothetical non-ERROR
    /// native entry is never removed by a javac error.
    #[test]
    fn merge_requires_error_severity_on_both_sides() {
        // (a) javac warning against a native error: both kept.
        let mut merged = vec![native_return(4, 15, 20, RETURN_MESSAGE)];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                15,
                21,
                DiagnosticSeverity::WARNING,
                RETURN_MESSAGE,
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "a javac warning must never dedupe: {merged:#?}"
        );

        // (b) non-ERROR native entry against a javac error: native kept.
        let mut downgraded = native_return(4, 15, 20, RETURN_MESSAGE);
        downgraded.severity = Some(DiagnosticSeverity::WARNING);
        let mut merged = vec![downgraded];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                15,
                21,
                DiagnosticSeverity::ERROR,
                RETURN_MESSAGE,
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "a non-error native entry must never be removed: {merged:#?}"
        );
    }

    /// Confirmation requires overlapping ranges on the SAME line: an equal
    /// payload on a different line, or a disjoint range on the same line,
    /// keeps both diagnostics.
    #[test]
    fn merge_requires_overlapping_ranges_on_same_line() {
        // (a) same payload, different line.
        let mut merged = vec![native_return(4, 15, 20, RETURN_MESSAGE)];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                6,
                15,
                21,
                DiagnosticSeverity::ERROR,
                RETURN_MESSAGE,
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "a different line must never merge: {merged:#?}"
        );

        // (b) same line, disjoint ranges (two returns on one line — distinct
        // errors that merely share a message).
        let mut merged = vec![native_return(4, 15, 20, RETURN_MESSAGE)];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                30,
                35,
                DiagnosticSeverity::ERROR,
                RETURN_MESSAGE,
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "disjoint same-line ranges must never merge: {merged:#?}"
        );
    }

    /// Unrelated native diagnostics sharing the line with a confirmed return
    /// error are untouched: only the one equivalent native entry is removed,
    /// and every javac diagnostic (matching or not) is appended after the
    /// surviving native entries.
    #[test]
    fn merge_removes_only_the_equivalent_entry_and_appends_javac() {
        let mut merged = vec![
            native_other(4, 8, 14, "Syntax error"),
            native_return(4, 15, 20, RETURN_MESSAGE),
            native_other(4, 22, 30, "cannot resolve member frobnicate"),
        ];
        merge_javac_diagnostics(
            &mut merged,
            vec![
                javac_diag(4, 15, 21, DiagnosticSeverity::ERROR, RETURN_MESSAGE),
                javac_diag(
                    9,
                    0,
                    u32::MAX,
                    DiagnosticSeverity::ERROR,
                    "cannot find symbol",
                ),
            ],
        );
        assert_eq!(
            merged.len(),
            4,
            "only the equivalent entry may go: {merged:#?}"
        );
        // Surviving natives first, in their original order…
        assert_eq!(merged[0].message, "Syntax error");
        assert_eq!(merged[1].message, "cannot resolve member frobnicate");
        // …then the javac diagnostics, appended in their original order.
        assert_eq!(merged[2].source.as_deref(), Some("javac"));
        assert_eq!(merged[2].message, RETURN_MESSAGE);
        assert_eq!(merged[3].source.as_deref(), Some("javac"));
        assert_eq!(merged[3].message, "cannot find symbol");
    }

    /// With no equivalent native entry at all, merging is a plain append —
    /// the pre-existing `compute_diagnostics` behavior is preserved.
    #[test]
    fn merge_without_equivalent_is_plain_append() {
        let mut merged = vec![native_other(1, 0, 4, "Syntax error")];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                9,
                0,
                u32::MAX,
                DiagnosticSeverity::ERROR,
                "cannot find symbol",
            )],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].message, "Syntax error");
        assert_eq!(merged[1].source.as_deref(), Some("javac"));
    }

    /// javac confirms a native incompatible-initializer error exactly like a
    /// return error: `jvl.incompatibleAssignment` is a dedupable code, the
    /// folded `required:`/`found:` continuation lines are ignored, and the
    /// `incompatible types: ` prefix strips when present on BOTH first lines.
    #[test]
    fn merge_replaces_equivalent_native_assignment_diagnostic() {
        let message = "incompatible types: int cannot be converted to boolean";
        let folded = format!("{message}\n  required: boolean\n  found:    int");
        let mut merged = vec![native_coded(
            jvl_syntax::INCOMPATIBLE_ASSIGNMENT_CODE,
            4,
            15,
            20,
            message,
        )];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                0,
                u32::MAX,
                DiagnosticSeverity::ERROR,
                &folded,
            )],
        );
        assert_eq!(
            merged.len(),
            1,
            "expected the native entry replaced: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("javac"));
    }

    /// javac emits `unreachable statement` verbatim — no `incompatible
    /// types: ` prefix on either side — so the first lines compare verbatim
    /// and the native `jvl.unreachable` entry is replaced.
    #[test]
    fn merge_replaces_equivalent_native_unreachable_diagnostic() {
        let mut merged = vec![native_coded(
            jvl_syntax::UNREACHABLE_CODE,
            7,
            8,
            22,
            "unreachable statement",
        )];
        merged[0].severity = Some(DiagnosticSeverity::WARNING);
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                7,
                8,
                9,
                DiagnosticSeverity::ERROR,
                "unreachable statement",
            )],
        );
        assert_eq!(
            merged.len(),
            1,
            "expected the native entry replaced: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("javac"));
    }

    /// The `incompatible types: ` prefix strips only when BOTH first lines
    /// carry it; a prefix on one side only compares verbatim and never
    /// merges.
    #[test]
    fn merge_requires_prefix_on_both_sides_or_neither() {
        let mut merged = vec![native_coded(
            jvl_syntax::UNREACHABLE_CODE,
            7,
            8,
            22,
            "unreachable statement",
        )];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                7,
                8,
                9,
                DiagnosticSeverity::ERROR,
                "incompatible types: unreachable statement",
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "a one-sided prefix must never merge: {merged:#?}"
        );
    }

    /// `jvl.unused` never merges — javac cannot emit it. Neither the real
    /// shape (WARNING + Unnecessary tag) nor a hypothetical ERROR-severity
    /// entry is in the dedupable-code list.
    #[test]
    fn merge_never_dedupes_unused_diagnostics() {
        // (a) as actually emitted: WARNING severity, Unnecessary tag.
        let mut warning = native_coded("jvl.unused", 4, 15, 20, "unused local variable 'x'");
        warning.severity = Some(DiagnosticSeverity::WARNING);
        warning.tags = Some(vec![DiagnosticTag::UNNECESSARY]);
        let mut merged = vec![warning];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                15,
                21,
                DiagnosticSeverity::WARNING,
                "unused local variable 'x'",
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "unused warnings must never merge: {merged:#?}"
        );

        // (b) even a hypothetical ERROR-severity `jvl.unused` entry with an
        // exactly matching javac error survives: the code is not in the list.
        let mut merged = vec![native_coded(
            "jvl.unused",
            4,
            15,
            20,
            "unused local variable 'x'",
        )];
        merge_javac_diagnostics(
            &mut merged,
            vec![javac_diag(
                4,
                15,
                21,
                DiagnosticSeverity::ERROR,
                "unused local variable 'x'",
            )],
        );
        assert_eq!(
            merged.len(),
            2,
            "jvl.unused is never dedupable: {merged:#?}"
        );
    }
}
