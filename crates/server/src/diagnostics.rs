//! Diagnostics for a single document (syntax, unresolved members, structural
//! checks), republishing after classpath rebuilds, and the `javac`
//! check-project orchestration that merges compiler diagnostics in.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jvl_syntax::LineIndex;
use tower_lsp_server::ls_types::*;

use crate::backend::{Backend, Document};
use crate::javac;
use crate::project_symbols::{CombinedSymbols, ProjectSymbols};
use crate::{filename_from_uri, infer_source_root, open_docs, ClasspathSymbols};

/// RAII guard releasing `Backend::javac_running` on drop — see
/// `Backend::execute_command`.
pub(crate) struct JavacRunningGuard<'a>(pub(crate) &'a std::sync::atomic::AtomicBool);

impl Drop for JavacRunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Which stored `javac` diagnostics `Backend::refresh_open_diagnostics` must
/// drop as stale before recomputing: a compiler result is only trustworthy
/// against the exact source it came from.
pub(crate) enum StaleJavac {
    /// Nothing about any open document's own source changed (e.g. a
    /// classpath rebuild) — every stored javac diagnostic is still valid.
    None,
    /// Every open document's stored javac diagnostics are stale and are
    /// dropped before recomputation.
    AllOpen,
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

/// Parse an LSP document URI into a `file:` path ending in `.java`. Returns
/// `None` for a non-`file:` URI or one that isn't a `.java` file.
fn parse_java_file_uri(uri_str: &str) -> Option<PathBuf> {
    let uri: Uri = uri_str.parse().ok()?;
    let path = uri.to_file_path()?.into_owned();
    path.extension()
        .is_some_and(|e| e == "java")
        .then_some(path)
}

/// Whether `path` lives under any of `module_roots`. Checks the literal path
/// first, then the canonicalized path, so a non-canonically keyed path (e.g.
/// macOS `/var` vs `/private/var`) still matches.
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

/// Native codes `javac` can independently confirm. `jvl.unused` is
/// deliberately absent since javac has no equivalent diagnostic.
const JAVAC_CONFIRMABLE_CODES: [&str; 6] = [
    jvl_syntax::INCOMPATIBLE_RETURN_CODE,
    jvl_syntax::INCOMPATIBLE_ASSIGNMENT_CODE,
    jvl_syntax::UNREACHABLE_CODE,
    jvl_syntax::CANNOT_FIND_SYMBOL_CODE,
    jvl_syntax::INVALID_INVOCATION_CODE,
    jvl_syntax::INVALID_INSTANTIATION_CODE,
];

/// Whether two first message lines carry the same payload. Only the first
/// line is compared since javac folds continuation lines in; the
/// `incompatible types: ` prefix strips only when present on both sides.
fn equivalent_payload(native: &str, javac: &str) -> bool {
    let native = native.lines().next().unwrap_or("");
    let javac = javac.lines().next().unwrap_or("");
    // javac's message is exactly `cannot find symbol`; the detail lives on
    // folded continuation lines, so only the prefix is compared.
    if native.starts_with("cannot find symbol") && javac.starts_with("cannot find symbol") {
        return true;
    }
    // Invocation/instantiation families: javac says "no suitable method
    // found for", "no suitable constructor found for", "is abstract; cannot be
    // instantiated", "has private access in", "reference to X is ambiguous".
    // Our range already overlaps on the same line (checked by the caller), so
    // family agreement is enough.
    let native_family = if native.starts_with("no applicable method")
        || native.starts_with("ambiguous method call")
    {
        Some("method")
    } else if native.starts_with("no applicable constructor")
        || native.starts_with("ambiguous constructor call")
        || native.starts_with("cannot instantiate")
        || native.contains("is not accessible")
        || native.contains("enclosing instance required")
    {
        Some("constructor")
    } else {
        None
    };
    let javac_family = if javac.starts_with("no suitable method found")
        || (javac.contains("reference to") && javac.contains("is ambiguous"))
        || (javac.starts_with("method ") && javac.contains("cannot be applied"))
    {
        Some("method")
    } else if javac.starts_with("no suitable constructor found")
        || javac.contains("cannot be instantiated")
        || javac.contains("has private access")
        || (javac.starts_with("constructor ") && javac.contains("cannot be applied"))
        || javac.contains("an enclosing instance that contains")
    {
        Some("constructor")
    } else {
        None
    };
    if let (Some(n), Some(j)) = (native_family, javac_family) {
        return n == j;
    }
    match (
        native.strip_prefix("incompatible types: "),
        javac.strip_prefix("incompatible types: "),
    ) {
        (Some(native), Some(javac)) => native == javac,
        _ => native == javac,
    }
}

/// Whether `javac` confirms `native` as the same error: requires a
/// confirmable code, both severities ERROR, overlapping ranges on the same
/// line, and an equivalent payload ([`equivalent_payload`]).
fn javac_confirms_native(native: &Diagnostic, javac: &Diagnostic) -> bool {
    matches!(
        &native.code,
        Some(NumberOrString::String(code)) if JAVAC_CONFIRMABLE_CODES.contains(&code.as_str())
    ) && native.severity == Some(DiagnosticSeverity::ERROR)
        && javac.severity == Some(DiagnosticSeverity::ERROR)
        && native.range.start.line == javac.range.start.line
        && native.range.start.character < javac.range.end.character
        && javac.range.start.character < native.range.end.character
        && equivalent_payload(&native.message, &javac.message)
}

/// Keep native diagnostics that javac confirms, then append unmatched compiler
/// diagnostics. Native entries always remain first and unchanged.
fn merge_javac_diagnostics(diagnostics: &mut Vec<Diagnostic>, javac: Vec<Diagnostic>) {
    let unmatched: Vec<Diagnostic> = javac
        .into_iter()
        .filter(|javac_diag| {
            !diagnostics
                .iter()
                .any(|native| javac_confirms_native(native, javac_diag))
        })
        .collect();
    diagnostics.extend(unmatched);
}

impl Backend {
    /// Recompute and republish diagnostics for every open document, used
    /// after a classpath swap since unresolved-member diagnostics depend on
    /// it. A thin wrapper: no stored `javac` diagnostic is invalidated by a
    /// classpath swap alone, so it never drops any.
    pub(crate) async fn republish_all_diagnostics(&self) {
        self.refresh_open_diagnostics(StaleJavac::None).await;
    }

    /// Recompute open-document diagnostics from one symbol snapshot, aborting
    /// publication if a newer semantic generation wins. The document lock is
    /// released before awaiting client publishes.
    pub(crate) async fn refresh_open_diagnostics(&self, stale_javac_for: StaleJavac) {
        self.ensure_workspace_index().await;
        let classpath = self.classpath();
        let (snapshot, generation) = {
            let docs = self.documents.lock().await;
            if matches!(stale_javac_for, StaleJavac::AllOpen) {
                let mut javac = self
                    .javac_diagnostics
                    .lock()
                    .expect("javac diagnostics poisoned");
                for uri in docs.keys() {
                    javac.remove(uri);
                }
            }
            let generation = self
                .semantic_generation
                .load(std::sync::atomic::Ordering::SeqCst);
            let symbols = CombinedSymbols(
                ProjectSymbols::new(self, &docs),
                ClasspathSymbols(Arc::clone(&classpath)),
            );
            let snapshot: Vec<(String, Vec<Diagnostic>, i32)> = docs
                .iter()
                .map(|(uri, doc)| {
                    (
                        uri.clone(),
                        self.compute_diagnostics(&docs, uri, &symbols),
                        doc.version,
                    )
                })
                .collect();
            (snapshot, generation)
        };
        for (uri_str, diagnostics, version) in snapshot {
            if self
                .semantic_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                != generation
            {
                return;
            }
            if let Ok(uri) = uri_str.parse::<Uri>() {
                self.client
                    .publish_diagnostics(uri, diagnostics, Some(version))
                    .await;
            }
        }
    }

    /// Compute native diagnostics for a stored document and merge saved javac
    /// results. `symbols` is prebuilt, keeping this function synchronous and IO-free.
    pub(crate) fn compute_diagnostics(
        &self,
        docs: &HashMap<String, Document>,
        uri: &str,
        symbols: &dyn jvl_syntax::SymbolSource,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = match docs.get(uri) {
            Some(doc) => {
                let index = LineIndex::new(&doc.text, self.encoding());
                let mut d = jvl_syntax::syntax_diagnostics(&doc.tree, &index);
                let open = open_docs(docs, uri, doc);
                // This parse cannot realistically fail since the key
                // already round-tripped through the client's URI; a
                // failure would only skip the semantic pass, never panic.
                if let Ok(parsed_uri) = uri.parse::<Uri>() {
                    d.extend(jvl_syntax::semantic_diagnostics(
                        &open,
                        0,
                        &index,
                        &parsed_uri,
                        symbols,
                        self.unresolved_member_diagnostics
                            .get()
                            .copied()
                            .unwrap_or(true),
                        self.unused_diagnostics.get().copied().unwrap_or(true),
                    ));
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
            merge_javac_diagnostics(&mut diagnostics, javac_diags.clone());
        }
        diagnostics
    }

    /// The `javacTimeoutSecs` initialization option, already clamped.
    fn javac_timeout(&self) -> Duration {
        Duration::from_secs(self.javac_timeout_secs.get().copied().unwrap_or(120))
    }

    /// Run the javac check for the full project or selected saved-file modules.
    /// The caller enforces single-flight execution and Workspace Trust.
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

    /// The full-workspace check: every discovered source root compiled as
    /// one explicit input set, replacing the entire javac diagnostic map.
    async fn run_check_full_project(&self) -> serde_json::Value {
        let Some(project_root) = self.project_root() else {
            return serde_json::json!({
                "status": "error",
                "message": "no project root (open a workspace folder or a file under a Maven/Gradle project)",
            });
        };
        let classpath = self.classpath();
        // Snapshot every open document's version under the same lock that
        // derives the source roots, so results are never attributed to a
        // newer buffer state.
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
        let start_generation = self
            .provider_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        // Cover every Maven/Gradle module in the workspace, not just the
        // root module, so a multi-module check is complete. Uses the
        // project root as-is (not canonicalized), since paths must stay in
        // the same space as client document URIs.
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
                if self
                    .provider_generation
                    .load(std::sync::atomic::Ordering::SeqCst)
                    != start_generation
                {
                    // A provider changed while javac ran: its output describes
                    // a workspace that no longer exists. Drop it; the client
                    // re-queues and the next run (≤30s) checks the new state.
                    return serde_json::json!({ "status": "stale" });
                }
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

    /// Compile modules containing the saved documents and replace only their
    /// diagnostics. Invalid URIs fail; unsupported modules are skipped unless
    /// none resolve, and diagnostics outside the scope trigger a full check.
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
        // Canonicalized workspace root, used only for the security
        // containment check (must resolve symlinks); module resolution and
        // diagnostic keying stay in the original path space.
        let Ok(workspace_canonical) = std::fs::canonicalize(&project_root) else {
            return serde_json::json!({
                "status": "error",
                "message": "could not canonicalize the workspace root",
            });
        };

        // Resolve each saved document to an in-workspace `.java` file and
        // its owning module; unsupported layouts are collected in
        // `unsupported_layout` below.
        let mut module_roots: Vec<PathBuf> = Vec::new();
        let mut explicit_roots: Vec<PathBuf> = Vec::new();
        let mut unsupported_layout: Option<String> = None;
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
                // Security gate only: reject any path whose symlink-resolved
                // form escapes the workspace; the canonical result isn't
                // used for resolution below.
                if javac::canonical_within_workspace(&file, &workspace_canonical).is_none() {
                    return serde_json::json!({
                        "status": "error",
                        "message": format!("document is outside the workspace or unreadable: {uri_str}"),
                    });
                }

                let module_root = match javac::nearest_module_root(&file, &project_root) {
                    Some(root) => root,
                    None => {
                        // No build marker up to the workspace root: fall
                        // back to it only if the file sits under a
                        // conventional source root there, otherwise mark
                        // this URI unsupported and keep resolving the rest.
                        let src_roots = javac::conventional_source_roots(&project_root);
                        if src_roots.iter().any(|r| file.starts_with(r)) {
                            project_root.clone()
                        } else {
                            if unsupported_layout.is_none() {
                                unsupported_layout = Some(uri_str.clone());
                            }
                            continue;
                        }
                    }
                };

                if !module_roots.contains(&module_root) {
                    module_roots.push(module_root.clone());
                    explicit_roots.extend(javac::conventional_source_roots(&module_root));
                }
                // Add a nonstandard-layout file's inferred source root too,
                // but only if it stays inside this module, so a scoped
                // compile can't pull in unrelated trees.
                if let Some(doc) = docs.get(uri_str) {
                    if let Some(inferred) = infer_source_root(uri_str, &doc.tree, &doc.text) {
                        if inferred.starts_with(&module_root) && !explicit_roots.contains(&inferred)
                        {
                            explicit_roots.push(inferred);
                        }
                    }
                }
            }
            if module_roots.is_empty() {
                // Every uri in the batch was unsupported; nothing to check.
                let uri_str = unsupported_layout.expect(
                    "non-empty document_uris with no resolved module roots implies at least one unsupported-layout uri",
                );
                return serde_json::json!({
                    "status": "unsupported-layout",
                    "message": format!(
                        "{uri_str} is not inside a Maven/Gradle module or a conventional source root; use Check Project (javac) instead"
                    ),
                });
            }
            docs.iter()
                .map(|(uri_str, doc)| (uri_str.clone(), doc.version))
                .collect()
        };
        let start_generation = self
            .provider_generation
            .load(std::sync::atomic::Ordering::SeqCst);

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
                if self
                    .provider_generation
                    .load(std::sync::atomic::Ordering::SeqCst)
                    != start_generation
                {
                    // A provider changed while javac ran: its output describes
                    // a workspace that no longer exists. Drop it; the client
                    // re-queues and the next run (≤30s) checks the new state.
                    return serde_json::json!({ "status": "stale" });
                }
                // If javac flagged a source outside the checked modules,
                // the scoped result is incomplete, so redo the run as a
                // full project check instead.
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

    /// Shared javac-invocation core for both scopes: locate `javac`,
    /// collect explicit source files, apply the JDK/project source-level
    /// guard, run the compiler, and parse the result. Publishing is left to
    /// the caller.
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

        // JDK level is read from `<home>/bin/javac`'s feature version (no
        // process spawn); project level is read statically from the build
        // files. Together they pick the language level below.
        let jdk_release = javac_path
            .parent()
            .and_then(|bin| bin.parent())
            .and_then(jvl_classpath::jdk_feature_version);
        let project_release = jvl_classpath::project_java_release(release_root);

        // If the project targets a newer Java than the newest detected JDK,
        // javac would emit a flood of "not supported in -source N" noise.
        // Publish one clear diagnostic instead and skip the run entirely.
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

        // Compile at the project's declared release when known, enabling
        // preview only if it matches the JDK version; otherwise fall back
        // to the JDK's level with preview on, or a bare compile if neither
        // is known.
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

    /// Publish a single diagnostic on the project's build file explaining
    /// the JDK is too old and the javac check was skipped. Routed through
    /// [`Self::publish_javac_diagnostics`] so it clears on the next run.
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

    /// Replace the full javac map and republish every affected URI, including
    /// clean files that need an empty publish. Revision filtering, the map swap,
    /// and snapshot happen under the document lock; publishing happens afterward.
    async fn publish_javac_diagnostics(
        &self,
        new_diags: HashMap<String, Vec<Diagnostic>>,
        start_versions: &HashMap<String, i32>,
    ) {
        self.ensure_workspace_index().await;
        let classpath = self.classpath();
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

            let symbols = CombinedSymbols(
                ProjectSymbols::new(self, &docs),
                ClasspathSymbols(Arc::clone(&classpath)),
            );
            affected
                .into_iter()
                .map(|uri_str| {
                    let diagnostics = self.compute_diagnostics(&docs, &uri_str, &symbols);
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

    /// Replace javac diagnostics only beneath `module_roots`, preserving other
    /// modules. Checked files absent from `new_diags` republish empty.
    async fn publish_javac_diagnostics_scoped(
        &self,
        new_diags: HashMap<String, Vec<Diagnostic>>,
        module_roots: &[PathBuf],
        start_versions: &HashMap<String, i32>,
    ) {
        self.ensure_workspace_index().await;
        let classpath = self.classpath();
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
                // Drop prior diagnostics for checked-module files, marking
                // each affected so a now-clean file republishes empty.
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

            let symbols = CombinedSymbols(
                ProjectSymbols::new(self, &docs),
                ClasspathSymbols(Arc::clone(&classpath)),
            );
            affected
                .into_iter()
                .map(|uri_str| {
                    let diagnostics = self.compute_diagnostics(&docs, &uri_str, &symbols);
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

    /// Native return diagnostic message for the canonical wrong-return
    /// fixture (`int code() { return "bad"; }`).
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
    /// emits it: code `jvl.incompatibleReturn`, severity ERROR, single-line
    /// range on the returned expression.
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

    /// An equivalent current javac result confirms the native diagnostic
    /// instead of replacing it: the native entry is left unchanged and the
    /// redundant javac diagnostic is dropped.
    #[test]
    fn merge_keeps_native_return_diagnostic_when_javac_confirms() {
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
            "expected the confirming javac entry dropped, native kept: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(merged[0].message, RETURN_MESSAGE);
    }

    /// Equivalence compares only the first message line after stripping the
    /// `incompatible types: ` prefix, since javac folds continuation lines
    /// into its message. A line-wide javac range (no caret parsed) still
    /// overlaps the native range on that line.
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
            "expected the confirming javac entry dropped: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("java-vsix-lite"));
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

    /// Both severities must be ERROR: a javac warning never confirms a
    /// native error, and a non-ERROR native entry is never removed by a
    /// javac error.
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

    /// Unrelated natives on the same line, and the confirmed native itself,
    /// are kept unchanged; only unmatched javac diagnostics are appended
    /// after all natives.
    #[test]
    fn merge_keeps_all_natives_and_appends_only_unmatched_javac() {
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
            "all natives survive, only the unmatched javac entry is added: {merged:#?}"
        );
        // Every native, in original order, none swapped…
        assert_eq!(merged[0].message, "Syntax error");
        assert_eq!(merged[1].source.as_deref(), Some("java-vsix-lite"));
        assert_eq!(merged[1].message, RETURN_MESSAGE);
        assert_eq!(merged[2].message, "cannot resolve member frobnicate");
        // …then only the javac diagnostic that confirmed nothing.
        assert_eq!(merged[3].source.as_deref(), Some("javac"));
        assert_eq!(merged[3].message, "cannot find symbol");
    }

    /// With no equivalent native entry, merging is a plain append.
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

    /// javac confirms a native incompatible-assignment error the same way
    /// as a return error. The native entry is kept; its confirming javac
    /// copy is dropped.
    #[test]
    fn merge_keeps_native_assignment_diagnostic_when_javac_confirms() {
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
            "expected the confirming javac entry dropped: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("java-vsix-lite"));
    }

    /// javac emits `unreachable statement` verbatim with no prefix, so the
    /// lines compare as-is and the native entry is kept, its javac copy
    /// dropped.
    #[test]
    fn merge_keeps_native_unreachable_diagnostic_when_javac_confirms() {
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
                "unreachable statement",
            )],
        );
        assert_eq!(
            merged.len(),
            1,
            "expected the confirming javac entry dropped, native kept: {merged:#?}"
        );
        assert_eq!(merged[0].source.as_deref(), Some("java-vsix-lite"));
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

    /// `jvl.unused` never merges since javac cannot emit it, regardless of
    /// severity or tags.
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

    /// `refresh_open_diagnostics(StaleJavac::AllOpen)` drops every open
    /// document's stored javac diagnostics before recomputing. A closed
    /// file's javac diagnostics are untouched, since only open documents
    /// are recomputed by this pass.
    #[tokio::test]
    async fn refresh_open_diagnostics_all_open_drops_stale_javac_entries() {
        let (service, _socket) = tower_lsp_server::LspService::new(Backend::new);
        let backend = service.inner();

        let mut parser = jvl_syntax::new_parser();
        let text = "class A {}\n".to_string();
        let tree = jvl_syntax::parse(&mut parser, &text, None).expect("parse");
        {
            let mut docs = backend.documents.lock().await;
            docs.insert(
                "file:///A.java".to_string(),
                Document {
                    text,
                    tree,
                    version: 1,
                },
            );
        }
        {
            let mut javac = backend
                .javac_diagnostics
                .lock()
                .expect("javac diagnostics poisoned");
            javac.insert(
                "file:///A.java".to_string(),
                vec![javac_diag(0, 0, 1, DiagnosticSeverity::ERROR, "stale")],
            );
            // Not a currently open document — its entry must survive.
            javac.insert(
                "file:///Closed.java".to_string(),
                vec![javac_diag(0, 0, 1, DiagnosticSeverity::ERROR, "stale")],
            );
        }

        backend.refresh_open_diagnostics(StaleJavac::AllOpen).await;

        let javac = backend
            .javac_diagnostics
            .lock()
            .expect("javac diagnostics poisoned");
        assert!(
            !javac.contains_key("file:///A.java"),
            "an open document's stale javac diagnostics must be dropped"
        );
        assert!(
            javac.contains_key("file:///Closed.java"),
            "a closed file's javac diagnostics are untouched by an open-document refresh"
        );
    }

    /// javac's wording for an inapplicable call differs from ours, so the
    /// two are matched by family; one error must not show two squiggles.
    #[test]
    fn javac_confirms_native_invocation_error() {
        let native = native_coded(
            jvl_syntax::INVALID_INVOCATION_CODE,
            3,
            10,
            14,
            "no applicable method 'pick' for argument types (int); 2 candidate(s) considered",
        );
        let javac = javac_diag(
            3,
            8,
            20,
            DiagnosticSeverity::ERROR,
            "no suitable method found for pick(int)",
        );
        assert!(javac_confirms_native(&native, &javac));
    }

    #[test]
    fn javac_confirms_native_instantiation_error() {
        let native = native_coded(
            jvl_syntax::INVALID_INSTANTIATION_CODE,
            5,
            15,
            20,
            "cannot instantiate abstract class 'Shape'",
        );
        let javac = javac_diag(
            5,
            11,
            22,
            DiagnosticSeverity::ERROR,
            "Shape is abstract; cannot be instantiated",
        );
        assert!(javac_confirms_native(&native, &javac));
    }

    #[test]
    fn mismatched_families_do_not_confirm() {
        let native = native_coded(
            jvl_syntax::INVALID_INVOCATION_CODE,
            3,
            10,
            14,
            "no applicable method 'pick' for argument types (int); 2 candidate(s) considered",
        );
        let javac = javac_diag(
            3,
            8,
            20,
            DiagnosticSeverity::ERROR,
            "no suitable constructor found for User(int)",
        );
        assert!(!javac_confirms_native(&native, &javac));
    }

    #[test]
    fn different_line_never_confirms() {
        let native = native_coded(
            jvl_syntax::INVALID_INVOCATION_CODE,
            3,
            10,
            14,
            "no applicable method 'pick' for argument types (int); 2 candidate(s) considered",
        );
        let javac = javac_diag(
            4,
            8,
            20,
            DiagnosticSeverity::ERROR,
            "no suitable method found for pick(int)",
        );
        assert!(!javac_confirms_native(&native, &javac));
    }
}
