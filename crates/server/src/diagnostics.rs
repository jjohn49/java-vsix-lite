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
        let snapshot: Vec<(String, Vec<Diagnostic>)> = {
            let docs = self.documents.lock().await;
            docs.keys()
                .map(|uri_str| (uri_str.clone(), self.compute_diagnostics(&docs, uri_str)))
                .collect()
        };
        for (uri_str, diagnostics) in snapshot {
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .await;
            }
        }
    }

    /// Syntax, immediate semantic, and structural diagnostics for a document
    /// already stored under `uri`, plus any `javac` diagnostics still on file
    /// for `uri` — merged in, never clobbering either set. Unlike the native
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
            diagnostics.extend(javac_diags.clone());
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
        let mut roots = {
            let docs = self.documents.lock().await;
            let mut roots = self.source_roots(&docs, &project_root);
            roots.extend(classpath.source_roots().iter().cloned());
            roots
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
                self.publish_javac_diagnostics(grouped).await;
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
        {
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
        }

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
                self.publish_javac_diagnostics_scoped(grouped, &module_roots)
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

    /// Replace the javac-diagnostics set wholesale with `new_diags`
    /// (keyed by filesystem path, as `javac` echoed it) and (re)publish
    /// every affected URI — both newly (or still) diagnosed files and any
    /// file that had javac diagnostics before this run but doesn't anymore
    /// (which must be published empty-of-javac to actually clear in the
    /// client's Problems panel; LSP has no "leave unchanged" — an omitted
    /// publish just means "nothing changed", not "clear"). Merges with each
    /// file's other diagnostics via `compute_diagnostics`, never clobbering.
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
        self.publish_javac_diagnostics(map).await;
    }

    async fn publish_javac_diagnostics(&self, new_diags: HashMap<String, Vec<Diagnostic>>) {
        let new_map: HashMap<String, Vec<Diagnostic>> = new_diags
            .into_iter()
            .filter_map(|(path, diags)| {
                Uri::from_file_path(Path::new(&path)).map(|uri| (uri.as_str().to_string(), diags))
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

        // Same lock-once-then-publish shape as `republish_all_diagnostics`:
        // every affected URI's diagnostics are computed into an owned
        // snapshot under a single `documents` lock hold, which is then
        // dropped before the `publish_diagnostics` `.await`s below.
        let snapshot: Vec<(String, Vec<Diagnostic>)> = {
            let docs = self.documents.lock().await;
            affected
                .into_iter()
                .map(|uri_str| {
                    let diagnostics = self.compute_diagnostics(&docs, &uri_str);
                    (uri_str, diagnostics)
                })
                .collect()
        };

        for (uri_str, diagnostics) in snapshot {
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .await;
            }
        }
    }

    /// Scoped counterpart to [`Self::publish_javac_diagnostics`]: replace the
    /// javac diagnostics only for files **beneath `module_roots`**, preserving
    /// every other module's diagnostics untouched.
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
    ) {
        let new_map: HashMap<String, Vec<Diagnostic>> = new_diags
            .into_iter()
            .filter_map(|(path, diags)| {
                Uri::from_file_path(Path::new(&path)).map(|uri| (uri.as_str().to_string(), diags))
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

        // Same lock-once-then-publish shape as `publish_javac_diagnostics`.
        let snapshot: Vec<(String, Vec<Diagnostic>)> = {
            let docs = self.documents.lock().await;
            affected
                .into_iter()
                .map(|uri_str| {
                    let diagnostics = self.compute_diagnostics(&docs, &uri_str);
                    (uri_str, diagnostics)
                })
                .collect()
        };

        for (uri_str, diagnostics) in snapshot {
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .await;
            }
        }
    }
}
