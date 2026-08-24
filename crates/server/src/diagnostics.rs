//! Diagnostics and diagnostic publication: syntax/unresolved-member/
//! structural diagnostics for a single document, republishing every open
//! document's diagnostics after a classpath rebuild, and the `javac`
//! check-project orchestration that merges compiler diagnostics in.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jvl_syntax::LineIndex;
use tower_lsp_server::ls_types::*;

use crate::backend::{Backend, Document};
use crate::javac;
use crate::{filename_from_uri, open_docs, ClasspathSymbols};

/// RAII guard releasing `Backend::javac_running` on drop — see
/// `Backend::execute_command`.
pub(crate) struct JavacRunningGuard<'a>(pub(crate) &'a std::sync::atomic::AtomicBool);

impl Drop for JavacRunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
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

    /// Syntax diagnostics for a document already stored under `uri`, plus
    /// unresolved-member diagnostics unless that setting has been turned
    /// off, plus any `javac` diagnostics still on file for `uri` —
    /// merged in, never clobbering either set. Unlike the first two, the
    /// `javac` diagnostics don't require `uri` to be an open document: a
    /// checked file the editor never opened still gets its diagnostics
    /// published (see `Backend::publish_javac_diagnostics`).
    pub(crate) fn compute_diagnostics(
        &self,
        docs: &HashMap<String, Document>,
        uri: &str,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = match docs.get(uri) {
            Some(doc) => {
                let index = LineIndex::new(&doc.text, self.encoding());
                let mut d = jvl_syntax::syntax_diagnostics(&doc.tree, &index);
                if self.unresolved_member_diagnostics.get().copied() == Some(true) {
                    let open = open_docs(docs, uri, doc);
                    // Classpath-only, not `CombinedSymbols` — this is a
                    // synchronous fn on the didOpen/didChange hot path, and
                    // `ProjectSymbols` needs an async `ensure_workspace_index`
                    // pass first. The unresolved-member check already stays
                    // silent whenever a receiver's type doesn't resolve at
                    // all (see `member_names`'s `complete` flag), so a
                    // closed-file project type is a missed diagnosis, never a
                    // false positive — an accepted gap, not a regression.
                    let symbols = ClasspathSymbols(self.classpath());
                    d.extend(jvl_syntax::member_diagnostics(&open, 0, &index, &symbols));
                }
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
    /// Concurrency (one run at a time) is enforced by the caller
    /// (`execute_command`), which claims `javac_running` before calling this
    /// and releases it afterward; this method assumes that's already done.
    pub(crate) async fn run_check_project(&self) -> serde_json::Value {
        let Some(project_root) = self.project_root() else {
            return serde_json::json!({
                "status": "error",
                "message": "no project root (open a workspace folder or a file under a Maven/Gradle project)",
            });
        };

        let javac_path = match javac::locate_javac(
            self.jdk_home_override.get().and_then(|o| o.as_deref()),
        ) {
            Some(path) => path,
            None => {
                return serde_json::json!({
                    "status": "javac-not-found",
                    "message": "could not locate javac: set $JAVA_HOME or the java-vsix-lite.jdk.home setting (never downloaded)",
                });
            }
        };

        let classpath = self.classpath();
        let roots = {
            let docs = self.documents.lock().await;
            let mut roots = self.source_roots(&docs, &project_root);
            roots.extend(classpath.source_roots().iter().cloned());
            roots
        };
        let source_files = javac::collect_source_files(&roots);
        if source_files.is_empty() {
            return serde_json::json!({
                "status": "error",
                "message": "no .java source files found under the discovered source roots",
            });
        }

        // JDK level: the JDK home is `<home>/bin/javac`; read its feature
        // version (no process spawn). Project level: read statically from the
        // build files. Together they pick the language level below.
        let jdk_release = javac_path
            .parent()
            .and_then(|bin| bin.parent())
            .and_then(jvl_classpath::jdk_feature_version);
        let project_release = jvl_classpath::project_java_release(&project_root);

        // If the project targets a newer Java than the newest detected JDK,
        // javac can't compile it and would emit a flood of "not supported in
        // -source N" noise. Publish one clear diagnostic on the build file and
        // skip the run entirely.
        if let (Some(proj), Some(jdk)) = (project_release, jdk_release) {
            if jdk < proj {
                self.publish_jdk_too_old(&project_root, proj, jdk).await;
                return serde_json::json!({
                    "status": "jdk-too-old",
                    "message": format!(
                        "project targets Java {proj} but the newest detected JDK is {jdk}; javac check skipped"
                    ),
                });
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
        let config = javac::RunConfig {
            javac_path,
            source_files,
            classpath_entries: classpath.entries().to_vec(),
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
            javac::RunOutcome::TimedOut => serde_json::json!({
                "status": "timeout",
                "message": format!("javac timed out after {}s and was killed", self.javac_timeout().as_secs()),
            }),
            javac::RunOutcome::Cancelled => serde_json::json!({
                "status": "cancelled",
                "message": "javac was killed by server shutdown",
            }),
            javac::RunOutcome::SpawnError(message) => serde_json::json!({
                "status": "spawn-error",
                "message": message,
            }),
            javac::RunOutcome::Completed { stderr } => {
                let raw = javac::parse_stderr(&stderr);
                let (error_count, warning_count) = javac::count_severities(&raw);
                let grouped = javac::group_diagnostics(raw);
                self.publish_javac_diagnostics(grouped).await;
                serde_json::json!({
                    "status": "ok",
                    "errorCount": error_count,
                    "warningCount": warning_count,
                })
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
}
