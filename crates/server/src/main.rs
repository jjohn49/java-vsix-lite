//! java-vsix-lite language server entry point.
//!
//! This is the single LSP server the editor talks to (see the implementation
//! plan's "Process topology"). The TypeScript extension shell launches this
//! binary over stdio and stays thin; all analysis lives here and in the
//! `jvl-*` crates.
//!
//! Open Java files are parsed incrementally with tree-sitter and get syntax
//! diagnostics, document symbols, folding/selection ranges, and semantic
//! tokens. Hover, completion, navigation, and diagnostics also draw on
//! closed project source files (via the lazy workspace index and
//! `project_symbols`) and on JDK/dependency types (via the `jvl-classpath`
//! crate) — not just what's currently open in the editor. An optional
//! `javac`-backed check adds real compiler diagnostics, running
//! automatically on project load and after every save in trusted
//! workspaces (see `javac`'s module doc comment), plus on demand via a
//! manual command.
//!
//! Invariant: **stdout is reserved for the wire protocol** — LSP by default,
//! DAP when launched as `jvl-server dap` (the debug adapter subcommand; see
//! `jvl-debug`). All logging goes to stderr via `tracing`.

#![forbid(unsafe_code)]

mod backend;
mod diagnostics;
mod fs_scan;
mod hierarchy;
mod javac;
mod navigation;
mod project_symbols;
mod references;
mod workspace_index;

use backend::{byte_range_to_lsp, Backend, Document};
#[cfg(test)]
use backend::{
    insert_bounded_project_file, CachedProjectFile, RebuildCoalescer, MAX_PROJECT_FILE_BYTES,
};
use diagnostics::JavacRunningGuard;
use hierarchy::{callable_item, type_item};
use navigation::truncation_notice;
use project_symbols::{CombinedSymbols, ProjectSymbols};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::time::SystemTime;

use jvl_syntax::tree_sitter::Tree;
use jvl_syntax::{LineIndex, PositionEncoding};
use serde::{Deserialize, Serialize};
use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::request::{
    GotoImplementationParams, GotoImplementationResponse, GotoTypeDefinitionParams,
    GotoTypeDefinitionResponse,
};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{LanguageServer, LspService, Server};

/// The server-internal `executeCommand` id the extension's trust-gated
/// `java-vsix-lite.downloadDependencies` command forwards to, to trigger an
/// immediate (non-debounced) classpath rebuild after installing consented-to
/// dependencies. Deliberately namespaced `jvl.*` and NOT the same id as any
/// extension-contributed command — see `CHECK_PROJECT_COMMAND`'s doc comment
/// (in `javac.rs`) for why a collision would break client startup; the
/// `server_commands_do_not_collide_with_extension_commands` lifecycle test
/// guards this for every server command, this one included.
const REBUILD_CLASSPATH_COMMAND: &str = "jvl.classpath.rebuild";

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
pub(crate) fn infer_source_root(uri: &str, tree: &Tree, source: &str) -> Option<PathBuf> {
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

/// The document's own file name (e.g.
/// `"MavenDemo2.java"`), or `None` for a non-`file:` URI (untitled/in-memory
/// document) or one whose path doesn't end in `.java`. Derived purely from
/// the URI — no filesystem access, no workspace/project root needed, so a
/// lone file with no workspace still gets this check.
fn filename_from_uri(uri: &str) -> Option<String> {
    let path = open_doc_path(uri)?;
    let name = path.file_name()?.to_str()?.to_string();
    name.ends_with(".java").then_some(name)
}

/// Decode the `jvl.checkProject.run` scope from its `executeCommand`
/// arguments (see [`javac::JavacCheckScope`]).
///
/// Backward compatible: no arguments, a `null`/empty first argument, or an
/// object without a `scope` all mean the whole project (the historical
/// behavior, and what the manual command sends explicitly as
/// `{"scope":"project"}`). A `{"scope":"modules","documentUris":[...]}` request
/// must carry a non-empty string array; a malformed or empty `modules` request
/// is an `Err` — the caller rejects it rather than compiling the whole project.
fn parse_check_scope(
    arguments: &[serde_json::Value],
) -> std::result::Result<javac::JavacCheckScope, String> {
    let Some(first) = arguments.first() else {
        return Ok(javac::JavacCheckScope::Project);
    };
    if first.is_null() {
        return Ok(javac::JavacCheckScope::Project);
    }
    let obj = first
        .as_object()
        .ok_or_else(|| "checkProject argument must be a JSON object".to_string())?;
    if obj.is_empty() {
        return Ok(javac::JavacCheckScope::Project);
    }
    match obj.get("scope").and_then(|v| v.as_str()) {
        None | Some("project") => Ok(javac::JavacCheckScope::Project),
        Some("modules") => {
            let array = obj
                .get("documentUris")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "modules scope requires a documentUris array".to_string())?;
            let mut document_uris = Vec::with_capacity(array.len());
            for value in array {
                let uri = value
                    .as_str()
                    .ok_or_else(|| "documentUris entries must be strings".to_string())?;
                document_uris.push(uri.to_string());
            }
            if document_uris.is_empty() {
                return Err("modules scope requires at least one document URI".to_string());
            }
            Ok(javac::JavacCheckScope::Modules { document_uris })
        }
        Some(other) => Err(format!("unknown checkProject scope: {other}")),
    }
}

/// The FQN encoded in a `jvl-src:` virtual-document URI (the inverse of
/// [`jvl_src_uri`]), as sent by the extension's `jvl/externalSource` request.
fn fqn_from_jvl_src_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("jvl-src:/")?
        .strip_suffix(".java")
        .map(str::to_string)
}

/// The `unresolvedMemberDiagnostics` flag from `initializationOptions`.
/// Default-on: the member rule inside `semantic_diagnostics` is conservative
/// and stays silent whenever resolution is incomplete, so this flag exists
/// only for a user who wants to opt back out of that rule, not to gate the
/// always-enabled return checks.
fn unresolved_member_diagnostics_opt(params: &InitializeParams) -> bool {
    params
        .initialization_options
        .as_ref()
        .and_then(|opts| opts.get("unresolvedMemberDiagnostics"))
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// The `classpathDebounceMs` field from `initializationOptions` — the wait
/// after the last matching build-file change before the classpath
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

/// The `jdkHome` field from `initializationOptions` (the
/// `java-vsix-lite.jdk.home` VS Code setting) — an explicit override for
/// where to find `javac`, tried before `$JAVA_HOME`. `None` (the default)
/// when unset or empty.
fn jdk_home_opt(params: &InitializeParams) -> Option<PathBuf> {
    params
        .initialization_options
        .as_ref()
        .and_then(|opts| opts.get("jdkHome"))
        .and_then(|value| value.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// The `javacTimeoutSecs` field from `initializationOptions` — how long
/// a `checkProject` run waits before killing `javac`. Clamped to
/// `[10, 600]` seconds (default 120) via `javac::clamp_timeout_secs`.
fn javac_timeout_secs_opt(params: &InitializeParams) -> u64 {
    let raw = params
        .initialization_options
        .as_ref()
        .and_then(|opts| opts.get("javacTimeoutSecs"))
        .and_then(|value| value.as_u64());
    javac::clamp_timeout_secs(raw)
}

/// Whether the client declared dynamic-registration support for
/// `workspace/didChangeWatchedFiles` — if not, the build-file watch is
/// simply never registered (graceful fallback; the LSP spec gives servers
/// no static-capability alternative for this one).
/// Whether the client can dynamically register type hierarchy — the
/// only way to enable it, since `ls-types` 0.0.6 has no static
/// `typeHierarchyProvider` capability field (see `type_hierarchy_dynamic`).
fn supports_type_hierarchy_registration(params: &InitializeParams) -> bool {
    params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|td| td.type_hierarchy.as_ref())
        .and_then(|c| c.dynamic_registration)
        .unwrap_or(false)
}

fn supports_watched_files_registration(params: &InitializeParams) -> bool {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.did_change_watched_files.as_ref())
        .and_then(|d| d.dynamic_registration)
        .unwrap_or(false)
}

/// Whether `uri` names one of the watched build files: `pom.xml`,
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

/// Whether the client's `workspace.workspaceEdit.resourceOperations`
/// includes `"rename"` — gates `rename`'s `RenameFile` resource op: skip the
/// file op, but still emit the text edits, when unsupported.
fn supports_rename_file_op(params: &InitializeParams) -> bool {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.workspace_edit.as_ref())
        .and_then(|we| we.resource_operations.as_ref())
        .is_some_and(|ops| ops.contains(&ResourceOperationKind::Rename))
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
            .supports_rename_file
            .set(supports_rename_file_op(&params));
        let _ = self
            .classpath_watch_dynamic
            .set(supports_watched_files_registration(&params));
        let _ = self
            .type_hierarchy_dynamic
            .set(supports_type_hierarchy_registration(&params));
        let _ = self
            .classpath_debounce_ms
            .set(classpath_debounce_ms_opt(&params));
        let _ = self.jdk_home_override.set(jdk_home_opt(&params));
        let _ = self.javac_timeout_secs.set(javac_timeout_secs_opt(&params));
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
                // Go-to-implementation.
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                // `prepareRename` support advertised so the client
                // validates/positions the rename before sending
                // `textDocument/rename`.
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                // Add-import quick fixes + "Organize Imports" (VS Code's
                // shift+alt+O and `source.organizeImports` on save both work
                // through this), plus extract variable/constant and
                // source-generate actions (accessors, constructor,
                // equals/hashCode, toString).
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                            CodeActionKind::REFACTOR_EXTRACT,
                            CodeActionKind::new("source.generate"),
                        ]),
                        resolve_provider: Some(false),
                        work_done_progress_options: Default::default(),
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    // `.` requests member completion; identifier/keyword
                    // completion is requested explicitly (Ctrl-Space) or by the
                    // editor as the user types.
                    trigger_characters: Some(vec![".".to_string()]),
                    // Javadoc is fetched lazily, only when the client
                    // asks via `completionItem/resolve` — never during
                    // `textDocument/completion` itself. See
                    // `Backend::completion_resolve`.
                    resolve_provider: Some(true),
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
                // Call hierarchy (incoming/outgoing calls). Its type-
                // hierarchy sibling is registered dynamically in
                // `initialized` — see `type_hierarchy_dynamic`.
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                // The one-shot, trust-gated `javac` check command
                // and the classpath-rebuild command. The extension only
                // sends either after confirming Workspace Trust; see
                // `javac`'s module doc comment (and `REBUILD_CLASSPATH_COMMAND`'s)
                // for the rest of the security invariants.
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        javac::CHECK_PROJECT_COMMAND.to_string(),
                        REBUILD_CLASSPATH_COMMAND.to_string(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "java-vsix-lite server initialized")
            .await;

        // Type hierarchy is registered dynamically, client permitting —
        // `ls-types` 0.0.6's `ServerCapabilities` cannot advertise it
        // statically (no `typeHierarchyProvider` field).
        if self.type_hierarchy_dynamic.get().copied().unwrap_or(false) {
            let registration = Registration {
                id: "jvl-type-hierarchy".to_string(),
                method: "textDocument/prepareTypeHierarchy".to_string(),
                register_options: serde_json::to_value(TypeHierarchyRegistrationOptions::default())
                    .ok(),
            };
            if let Err(err) = self.client.register_capability(vec![registration]).await {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("failed to register type hierarchy: {err}"),
                    )
                    .await;
            }
        }

        // Watch build files for classpath invalidation, client
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
        // A `checkProject` run in flight must never survive the
        // server as a zombie or an orphaned process — kill and reap it.
        javac::kill_running_child(&self.javac_child);
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
        self.open_document(
            params.text_document.uri,
            params.text_document.version,
            params.text_document.text,
        )
        .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let encoding = self.encoding();

        // Warm the classpath before taking the documents lock — see
        // `open_document`'s identical call for why: `compute_diagnostics`
        // below needs it for the unresolved-member pass, and a cold
        // first-ever build (JDK/project dependency scan) should not run
        // while other requests are blocked on the documents lock.
        self.classpath();

        // Apply edits to the cached text + tree under the lock (all synchronous),
        // reparse, then drop the lock before the async publish.
        let diagnostics = {
            let mut docs = self.documents.lock().await;
            {
                let Some(doc) = docs.get_mut(uri.as_str()) else {
                    return; // change for a document we never opened
                };
                doc.version = version;

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
            // An edit invalidates any javac diagnostics for this file —
            // they're stale the instant the source they were computed from
            // changes. Removed INSIDE the documents critical section, after
            // the version bump above: the compiler publication paths check
            // their run's start version and swap the javac map under this
            // same lock, so an in-flight run can neither observe the
            // pre-edit version as still current nor re-insert a stale entry
            // after this removal — and the entry is gone before
            // `compute_diagnostics` below would merge it.
            self.javac_diagnostics
                .lock()
                .expect("javac diagnostics poisoned")
                .remove(uri.as_str());
            self.compute_diagnostics(&docs, uri.as_str())
        };

        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().await.remove(uri.as_str());
        // Clear diagnostics for the closed file.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    /// A watched build file changed. Events that don't actually name
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

    /// `workspace/executeCommand` — `javac::CHECK_PROJECT_COMMAND`
    /// or `REBUILD_CLASSPATH_COMMAND`. One-shot per invocation; the extension
    /// only sends either after confirming Workspace Trust (the server itself
    /// has no notion of that and just does what it's told — see `javac`'s
    /// module doc comment). The extension additionally sends the javac
    /// check on project load and after Java file saves (debounced, still
    /// trust-gated, opt-out via `javac.checkOnSave`) — the single-flight
    /// guard below is what makes that safe against overlapping runs.
    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command == REBUILD_CLASSPATH_COMMAND {
            return Ok(Some(self.run_rebuild_classpath_command().await));
        }
        if params.command != javac::CHECK_PROJECT_COMMAND {
            return Err(Error::method_not_found());
        }
        // Decode the check scope BEFORE claiming the single-flight flag: a
        // malformed request is rejected outright (never widened to a project
        // compile) without ever blocking a legitimate concurrent run.
        let scope = match parse_check_scope(&params.arguments) {
            Ok(scope) => scope,
            Err(message) => {
                return Ok(Some(serde_json::json!({
                    "status": "error",
                    "message": message,
                })));
            }
        };
        if self
            .javac_running
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(Some(serde_json::json!({ "status": "already-running" })));
        }
        // RAII: releases `javac_running` on every exit path (including an
        // unexpected panic unwinding through here), so a single bad run can
        // never wedge every future `checkProject` invocation.
        let _guard = JavacRunningGuard(&self.javac_running);
        let result = self.run_check_project(scope).await;
        Ok(Some(result))
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

    /// `workspace/symbol` over the lazy, bounded workspace index (see
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

        self.ensure_workspace_index().await;
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
        self.ensure_workspace_index().await;
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
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
        Ok(jvl_syntax::hover(&open, 0, &index, position, &symbols))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        self.ensure_workspace_index().await;
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
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
        Ok(jvl_syntax::signature_help(
            &open, 0, &index, position, &symbols,
        ))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.ensure_workspace_index().await;
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let docs = self.documents.lock().await;
        // `open_docs_and_uris` (not plain `open_docs`) because in-project
        // items' lazy-resolve `data` payloads carry the declaring document
        // as a slice index that is meaningless once this request ends —
        // `stamp_completion_data_uri` translates it into the document's URI
        // before the items go on the wire.
        let Some((open, uris)) = open_docs_and_uris(&docs, uri.as_str()) else {
            return Ok(None);
        };
        let index = LineIndex::new(open[0].source, self.encoding());
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
        let mut result =
            jvl_syntax::completion(&open, 0, &index, position, self.snippet_support(), &symbols);
        for item in &mut result.items {
            stamp_completion_data_uri(item, &uris);
        }
        // A `CompletionList` (not a bare array) so `isIncomplete` reaches
        // the client — it re-queries as the user types past a capped set.
        Ok(
            (!result.items.is_empty() || result.is_incomplete).then_some(CompletionResponse::List(
                CompletionList {
                    is_incomplete: result.is_incomplete,
                    items: result.items,
                },
            )),
        )
    }

    /// Code actions — add-import quick fixes for the identifier under
    /// the cursor plus "Organize Imports". `jvl_syntax::code_actions` returns
    /// URI-less sketches; the request's own document URI is stamped on here.
    /// The client's `context.only` filter is honored hierarchically (a
    /// requested `source` matches our `source.organizeImports`).
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        self.ensure_workspace_index().await;
        let uri = params.text_document.uri;
        let docs = self.documents.lock().await;
        let Some(current) = docs.get(uri.as_str()) else {
            return Ok(None);
        };
        let open = open_docs(&docs, uri.as_str(), current);
        let index = LineIndex::new(&current.text, self.encoding());
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
        let sketches = jvl_syntax::code_actions(&open, 0, &index, params.range, &symbols);

        let allowed = |kind: &str| match &params.context.only {
            None => true,
            Some(only) => only.iter().any(|k| {
                let k = k.as_str();
                k.is_empty() || kind == k || kind.starts_with(k) && kind.as_bytes()[k.len()] == b'.'
            }),
        };
        let actions: Vec<CodeActionOrCommand> = sketches
            .into_iter()
            .filter(|s| allowed(s.kind))
            .map(|s| {
                CodeActionOrCommand::CodeAction(CodeAction {
                    title: s.title,
                    kind: Some(CodeActionKind::new(s.kind)),
                    edit: Some(WorkspaceEdit {
                        changes: Some(HashMap::from([(uri.clone(), s.edits)])),
                        ..Default::default()
                    }),
                    is_preferred: s.is_preferred.then_some(true),
                    ..Default::default()
                })
            })
            .collect();
        Ok((!actions.is_empty()).then_some(actions))
    }

    /// The strictly-lazy counterpart to `completion` — Javadoc is
    /// fetched only here, on demand, from whatever key `completion` attached
    /// to the item's `data` field (see `jvl_syntax::resolve_documentation`).
    /// A `data`-less item (locals, params, keywords, type names — none of
    /// which ever carried eager docs) is returned unchanged.
    ///
    /// An in-project key is re-resolved against the
    /// **originating document only** (the `data.uri` stamped at completion
    /// time), never by scanning all open documents — with two open files
    /// declaring same-named types and members, a simple-name scan could
    /// silently attach the *other* file's Javadoc. A stale URI (document
    /// closed since the completion request) resolves to no documentation
    /// rather than a guess. External keys carry no URI and ignore `open`.
    async fn completion_resolve(&self, mut item: CompletionItem) -> Result<CompletionItem> {
        self.ensure_workspace_index().await;
        let Some(data) = item.data.clone() else {
            return Ok(item);
        };
        let docs = self.documents.lock().await;
        let open: Vec<jvl_syntax::OpenDoc> = data
            .get("uri")
            .and_then(|u| u.as_str())
            .and_then(|u| docs.get(u))
            .map(|d| {
                vec![jvl_syntax::OpenDoc {
                    source: &d.text,
                    tree: &d.tree,
                }]
            })
            .unwrap_or_default();
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
        if let Some(doc) = jvl_syntax::resolve_documentation(&open, &data, &symbols) {
            item.documentation = Some(doc);
        }
        Ok(item)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Deliberately ClasspathSymbols-only, NOT CombinedSymbols.
        // jvl_syntax::definition's ladder step (c) — landing precisely
        // inside an unopened project file via locate_in_project_file —
        // is *triggered* by the SymbolSource failing to recognize a bare
        // type name (see definition.rs's module doc). ProjectSymbols
        // would make that name resolve instead, short-circuiting the
        // ladder into step (d)'s classpath-only External handling, which
        // can't locate a real project file at all. See
        // `definition_into_unopened_project_file` in lifecycle.rs.
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
        // Deliberately ClasspathSymbols-only, NOT CombinedSymbols.
        // jvl_syntax::definition's ladder step (c) — landing precisely
        // inside an unopened project file via locate_in_project_file —
        // is *triggered* by the SymbolSource failing to recognize a bare
        // type name (see definition.rs's module doc). ProjectSymbols
        // would make that name resolve instead, short-circuiting the
        // ladder into step (d)'s classpath-only External handling, which
        // can't locate a real project file at all. See
        // `definition_into_unopened_project_file` in lifecycle.rs.
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

    /// `textDocument/implementation` — bounded, single-tier scan
    /// (see `scan_implementations`'s doc comment): resolve the cursor to an
    /// in-project type or method (`jvl_syntax::implementation_target`),
    /// prefilter the workspace for candidate files by the type's simple
    /// name (reusing the `references::prefilter`), and confirm each
    /// with `jvl_syntax::implementations_in_doc` (supertype simple-name
    /// match + import/package-aware resolution — the same confirm-by-
    /// resolution convention `references.rs`'s `bare_type_site` uses).
    ///
    /// Only symbols declared in a currently *open* document are supported
    /// (open-files-first, like every other feature here) —
    /// `jvl_syntax::implementation_target` answers `None` for anything else,
    /// and this handler answers `Ok(None)` in that case.
    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(snapshot) = self
            .implementation_target_snapshot(uri.as_str(), position)
            .await
        else {
            return Ok(None);
        };

        let outcome = self
            .scan_implementations(
                &snapshot.target,
                &snapshot.target_uri,
                &snapshot.target_text,
                &snapshot.target_tree,
                &snapshot.roots,
                snapshot.project_root.as_deref(),
                &snapshot.open_snapshot,
            )
            .await;

        let mut locations = Vec::new();
        for hit in &outcome.hits {
            let hit_index = LineIndex::new(&hit.text, self.encoding());
            locations.extend(hit.ranges.iter().cloned().map(|range| Location {
                uri: hit.uri.clone(),
                range: byte_range_to_lsp(&hit_index, range),
            }));
        }

        // Same truncation notice as `references()` — one bounded prefilter,
        // one wording (`truncation_notice`).
        if outcome.truncated {
            self.client
                .show_message(MessageType::INFO, truncation_notice())
                .await;
        }

        Ok((!locations.is_empty()).then_some(GotoImplementationResponse::Array(locations)))
    }

    /// `textDocument/references` — two-tier, bounded, confirm-by-
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
    /// `textDocument/prepareCallHierarchy` — the cursor must resolve
    /// (via the reference machinery) to a method/constructor
    /// **declared in an open document**; anything else answers `None`, the
    /// same open-files-first refusal shape as references/rename.
    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(snapshot) = self.target_snapshot(uri.as_str(), position).await else {
            return Ok(None);
        };
        let doc = jvl_syntax::OpenDoc {
            source: &snapshot.target_text,
            tree: &snapshot.target_tree,
        };
        let Some(info) = jvl_syntax::callable_decl_at_name(&doc, snapshot.target.name_range.start)
        else {
            return Ok(None); // the target is a field/type/local, not a callable
        };
        let index = LineIndex::new(&snapshot.target_text, self.encoding());
        let Ok(item_uri) = snapshot.target_uri.parse::<Uri>() else {
            return Ok(None);
        };
        Ok(Some(vec![callable_item(&info, item_uri, &index)]))
    }

    /// `callHierarchy/incomingCalls` — the bounded reference scan,
    /// with every confirmed call site grouped under its enclosing
    /// method/constructor (or type, for field-initializer references).
    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let item = params.item;
        let Some(snapshot) = self
            .target_snapshot(item.uri.as_str(), item.selection_range.start)
            .await
        else {
            return Ok(Some(Vec::new())); // file closed since prepare — empty, not an error
        };
        let outcome = self
            .scan_references(
                &snapshot.target,
                &snapshot.target_uri,
                &snapshot.target_text,
                &snapshot.target_tree,
                snapshot.target_version,
                &snapshot.roots,
                snapshot.project_root.as_deref(),
                &snapshot.open_snapshot,
                false,
            )
            .await;
        let mut calls: Vec<CallHierarchyIncomingCall> = Vec::new();
        for hit in outcome.hits {
            let tree = self.parse(&hit.text, None);
            let index = LineIndex::new(&hit.text, self.encoding());
            let doc = jvl_syntax::OpenDoc {
                source: &hit.text,
                tree: &tree,
            };
            for range in hit.ranges {
                let Some(info) = jvl_syntax::enclosing_callable(&doc, range.start) else {
                    continue;
                };
                let from = callable_item(&info, hit.uri.clone(), &index);
                let from_range = byte_range_to_lsp(&index, range.clone());
                if let Some(existing) = calls.iter_mut().find(|c| {
                    c.from.uri == from.uri && c.from.selection_range == from.selection_range
                }) {
                    existing.from_ranges.push(from_range);
                } else {
                    calls.push(CallHierarchyIncomingCall {
                        from,
                        from_ranges: vec![from_range],
                    });
                }
            }
        }
        Ok(Some(calls))
    }

    /// `callHierarchy/outgoingCalls` — every call site inside the
    /// item's body, each resolved through the same ladder as
    /// go-to-definition (open docs, closed project files, JDK/dependency
    /// stubs as `jvl-src` virtual documents). Unresolvable callees are
    /// silently omitted rather than guessed.
    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let item = params.item;
        let docs = self.documents.lock().await;
        let Some((open, uris)) = open_docs_and_uris(&docs, item.uri.as_str()) else {
            return Ok(Some(Vec::new()));
        };
        let index = LineIndex::new(open[0].source, self.encoding());
        let decl_byte = index.offset(item.selection_range.start);
        let sites = jvl_syntax::outgoing_call_sites(&open[0], decl_byte);
        // ClasspathSymbols-only for the same reason as `goto_definition` —
        // see that handler's comment on the ladder's step (c).
        let symbols = ClasspathSymbols(self.classpath());
        let mut calls: Vec<CallHierarchyOutgoingCall> = Vec::new();
        for site in sites {
            let pos = index.position(site.name_range.start);
            let Some(def) = jvl_syntax::definition(&open, 0, &index, pos, &symbols) else {
                continue;
            };
            let Some(loc) = self.resolve_location(def, &docs, &uris) else {
                continue;
            };
            let from_range = byte_range_to_lsp(&index, site.name_range.clone());
            if let Some(existing) = calls
                .iter_mut()
                .find(|c| c.to.uri == loc.uri && c.to.selection_range == loc.range)
            {
                existing.from_ranges.push(from_range);
                continue;
            }
            calls.push(CallHierarchyOutgoingCall {
                to: CallHierarchyItem {
                    name: site.name.clone(),
                    kind: SymbolKind::METHOD,
                    tags: None,
                    detail: None,
                    uri: loc.uri,
                    range: loc.range,
                    selection_range: loc.range,
                    data: None,
                },
                from_ranges: vec![from_range],
            });
        }
        Ok(Some(calls))
    }

    /// `textDocument/prepareTypeHierarchy` — the cursor must name a
    /// type declared in an open document (its declaration or any reference
    /// the open-document table resolves).
    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.documents.lock().await;
        let Some((open, uris)) = open_docs_and_uris(&docs, uri.as_str()) else {
            return Ok(None);
        };
        let index = LineIndex::new(open[0].source, self.encoding());
        let Some((doc_idx, info)) = jvl_syntax::type_decl_at(&open, 0, &index, position) else {
            return Ok(None);
        };
        let decl_index = LineIndex::new(open[doc_idx].source, self.encoding());
        let Some(item_uri) = uris.get(doc_idx).and_then(|u| u.parse::<Uri>().ok()) else {
            return Ok(None);
        };
        Ok(Some(vec![type_item(&info, item_uri, &decl_index)]))
    }

    /// `typeHierarchy/supertypes` — the item's `extends`/`implements`
    /// simple names, located open-files-first, then in closed workspace
    /// files through the declaring file's import candidates + the workspace
    /// index. JDK/dependency supertypes are omitted (no real file to point
    /// at) — a documented gap, not a guess.
    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let item = params.item;
        self.ensure_workspace_index().await;
        let Some((text, tree)) = self.hierarchy_doc(item.uri.as_str()).await else {
            return Ok(Some(Vec::new()));
        };
        let doc = jvl_syntax::OpenDoc {
            source: &text,
            tree: &tree,
        };
        let Some(info) = jvl_syntax::type_info_in(&doc, &item.name) else {
            return Ok(Some(Vec::new()));
        };
        let mut out = Vec::new();
        let docs = self.documents.lock().await;
        'supers: for sup in &info.supers {
            for (doc_uri, d) in docs.iter() {
                let sdoc = jvl_syntax::OpenDoc {
                    source: &d.text,
                    tree: &d.tree,
                };
                if let Some(sinfo) = jvl_syntax::type_info_in(&sdoc, &sup.simple) {
                    if let Ok(u) = doc_uri.parse::<Uri>() {
                        let sindex = LineIndex::new(&d.text, self.encoding());
                        out.push(type_item(&sinfo, u, &sindex));
                        continue 'supers;
                    }
                }
            }
            for fqn in &sup.candidates {
                let Some((pkg, simple)) = fqn.rsplit_once('.') else {
                    continue;
                };
                let Some(path) = self.workspace_index().find_type(pkg, simple) else {
                    continue;
                };
                let Some((stext, stree)) = self.parsed_project_file(&path) else {
                    continue;
                };
                let sdoc = jvl_syntax::OpenDoc {
                    source: &stext,
                    tree: &stree,
                };
                if let Some(sinfo) = jvl_syntax::type_info_in(&sdoc, simple) {
                    if let Some(u) = Uri::from_file_path(&path) {
                        let sindex = LineIndex::new(&stext, self.encoding());
                        out.push(type_item(&sinfo, u, &sindex));
                        continue 'supers;
                    }
                }
            }
        }
        Ok(Some(out))
    }

    /// `typeHierarchy/subtypes` — the bounded implementation scan
    /// (same prefilter, same confirm-by-resolution), each hit wrapped back
    /// into its enclosing type declaration.
    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let item = params.item;
        self.ensure_workspace_index().await;
        let Some((text, tree)) = self.hierarchy_doc(item.uri.as_str()).await else {
            return Ok(Some(Vec::new()));
        };
        let (roots, project_root, open_snapshot) = {
            let docs = self.documents.lock().await;
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
            (roots, project_root, open_snapshot)
        };
        let target = jvl_syntax::ImplementationTarget {
            type_name: item.name.clone(),
            type_doc: 0,
            method_name: None,
        };
        let outcome = self
            .scan_implementations(
                &target,
                item.uri.as_str(),
                &text,
                &tree,
                &roots,
                project_root.as_deref(),
                &open_snapshot,
            )
            .await;
        let mut out = Vec::new();
        for hit in outcome.hits {
            let tree = self.parse(&hit.text, None);
            let index = LineIndex::new(&hit.text, self.encoding());
            let doc = jvl_syntax::OpenDoc {
                source: &hit.text,
                tree: &tree,
            };
            for range in hit.ranges {
                if let Some(sinfo) = jvl_syntax::type_decl_at_byte(&doc, range.start) {
                    out.push(type_item(&sinfo, hit.uri.clone(), &index));
                }
            }
        }
        Ok(Some(out))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        let Some(snapshot) = self.target_snapshot(uri.as_str(), position).await else {
            return Ok(None);
        };

        let outcome = self
            .scan_references(
                &snapshot.target,
                &snapshot.target_uri,
                &snapshot.target_text,
                &snapshot.target_tree,
                snapshot.target_version,
                &snapshot.roots,
                snapshot.project_root.as_deref(),
                &snapshot.open_snapshot,
                include_declaration,
            )
            .await;

        let mut locations = Vec::new();
        for hit in &outcome.hits {
            let hit_index = LineIndex::new(&hit.text, self.encoding());
            locations.extend(hit.ranges.iter().cloned().map(|range| Location {
                uri: hit.uri.clone(),
                range: byte_range_to_lsp(&hit_index, range),
            }));
        }

        // One informational notice per request at most, covering both
        // incompleteness signals: the scan hit its cap, and/or textual hits
        // were omitted because they couldn't be confirmed by resolution.
        if outcome.possible > 0 {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "references: {} textual match(es) for `{}` could not be \
                         confirmed by resolution and were omitted",
                        outcome.possible, snapshot.target.name
                    ),
                )
                .await;
        }
        let mut notice_parts: Vec<String> = Vec::new();
        if outcome.truncated {
            notice_parts.push(truncation_notice());
        }
        if outcome.possible > 0 {
            notice_parts.push(format!(
                "{} possible additional match(es) could not be confirmed.",
                outcome.possible
            ));
        }
        if !notice_parts.is_empty() {
            self.client
                .show_message(MessageType::INFO, notice_parts.join(" "))
                .await;
        }

        Ok((!locations.is_empty()).then_some(locations))
    }

    /// `textDocument/prepareRename` — `Some` only when the cursor
    /// resolves to an in-project declaration (see
    /// `jvl_syntax::prepare_rename`'s doc comment for the full refusal
    /// list: external/JDK symbols, keywords, literals, `this`/`super`, and
    /// non-identifiers all yield `None`, never an error — the client should
    /// simply not offer rename UI for these).
    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.ensure_workspace_index().await;
        let uri = params.text_document.uri;
        let position = params.position;
        let docs = self.documents.lock().await;
        let Some(current) = docs.get(uri.as_str()) else {
            return Ok(None);
        };
        let open = open_docs(&docs, uri.as_str(), current);
        let index = LineIndex::new(&current.text, self.encoding());
        let symbols = CombinedSymbols(ProjectSymbols(self), ClasspathSymbols(self.classpath()));
        let Some(prep) = jvl_syntax::prepare_rename(&open, 0, &index, position, &symbols) else {
            return Ok(None);
        };
        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: byte_range_to_lsp(&index, prep.range),
            placeholder: prep.placeholder,
        }))
    }

    /// `textDocument/rename` — conservative: refuse rather than
    /// corrupt. Reuses the find-references orchestration
    /// (`target_snapshot`/`scan_references`) to collect every occurrence
    /// (declaration included — rename always renames it too), then applies
    /// every refusal guard, in order:
    ///
    /// 1. `new_name` must be a syntactically valid Java identifier and not
    ///    a reserved word/literal (`jvl_syntax::is_valid_new_name`).
    /// 2. The cursor must resolve to an in-project declaration (same as
    ///    `prepareRename`).
    /// 3. The declaring scope must not already bind `new_name` to a sibling
    ///    of the same kind (`jvl_syntax::collides_with_existing`).
    /// 4. The scan must not have hit its file cap (`outcome.truncated`).
    /// 5. Every textual hit must have been confirmed by resolution
    ///    (`outcome.possible == 0`).
    /// 6. For a `Tier::Workspace` target, every prefiltered candidate file
    ///    must have parsed (`outcome.unparsed_hit_files == 0`) — an
    ///    occurrence could be hiding in one that didn't.
    ///
    /// Only once all of these pass is a `WorkspaceEdit` assembled: one
    /// `TextDocumentEdit` per file (versioned when the file is open), plus —
    /// when renaming a `public` top-level type whose file name matches it,
    /// and the client's `workspace.workspaceEdit.resourceOperations`
    /// includes `"rename"` — a trailing `RenameFile` resource op (text edits
    /// come first in the array, addressed by the OLD uri, which is still
    /// valid at that point in document-change application order).
    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        if !jvl_syntax::is_valid_new_name(&new_name) {
            return Err(Error::invalid_params(format!(
                "`{new_name}` is not a valid Java identifier (or is a reserved word/literal)"
            )));
        }

        let Some(snapshot) = self.target_snapshot(uri.as_str(), position).await else {
            return Err(Error::invalid_params(
                "rename is only supported for in-project declarations (locals, fields, \
                 methods, types) — external symbols, keywords, and literals cannot be renamed",
            ));
        };

        let target_open_doc = [jvl_syntax::OpenDoc {
            source: &snapshot.target_text,
            tree: &snapshot.target_tree,
        }];
        let remapped_target = jvl_syntax::ReferenceTarget {
            doc: 0,
            ..snapshot.target.clone()
        };
        if jvl_syntax::collides_with_existing(&target_open_doc, &remapped_target, &new_name) {
            return Err(Error::invalid_params("target name already in scope"));
        }

        let outcome = self
            .scan_references(
                &snapshot.target,
                &snapshot.target_uri,
                &snapshot.target_text,
                &snapshot.target_tree,
                snapshot.target_version,
                &snapshot.roots,
                snapshot.project_root.as_deref(),
                &snapshot.open_snapshot,
                true, // rename always includes (and renames) the declaration
            )
            .await;

        if outcome.truncated {
            return Err(Error::invalid_params(format!(
                "rename requires full confirmation; the reference search was truncated at {} \
                 files",
                references::MAX_FILES_SCANNED
            )));
        }
        if outcome.possible > 0 {
            return Err(Error::invalid_params(format!(
                "rename requires full confirmation; {} occurrence(s) could not be verified",
                outcome.possible
            )));
        }
        if snapshot.target.tier == jvl_syntax::Tier::Workspace && outcome.unparsed_hit_files > 0 {
            return Err(Error::invalid_params(format!(
                "rename requires full confirmation; {} candidate file(s) could not be parsed",
                outcome.unparsed_hit_files
            )));
        }
        if outcome.hits.is_empty() {
            // Never a no-op/empty WorkspaceEdit — the declaration itself is
            // always a hit when resolution succeeded at all.
            return Ok(None);
        }

        let mut document_changes = Vec::with_capacity(outcome.hits.len() + 1);
        for hit in &outcome.hits {
            let index = LineIndex::new(&hit.text, self.encoding());
            let edits = hit
                .ranges
                .iter()
                .cloned()
                .map(|range| {
                    OneOf::Left(TextEdit {
                        range: byte_range_to_lsp(&index, range),
                        new_text: new_name.clone(),
                    })
                })
                .collect();
            document_changes.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: hit.uri.clone(),
                    version: hit.version,
                },
                edits,
            }));
        }

        if self.supports_rename_file() {
            if let Some(old_path) = open_doc_path(&snapshot.target_uri) {
                let old_name_matches_file = old_path.file_stem().and_then(|s| s.to_str())
                    == Some(snapshot.target.name.as_str());
                if old_name_matches_file
                    && jvl_syntax::is_public_top_level_type(&target_open_doc, &remapped_target)
                {
                    let new_path = old_path.with_file_name(format!("{new_name}.java"));
                    if let (Some(old_uri), Some(new_uri)) = (
                        Uri::from_file_path(&old_path),
                        Uri::from_file_path(&new_path),
                    ) {
                        document_changes.push(DocumentChangeOperation::Op(ResourceOp::Rename(
                            RenameFile {
                                old_uri,
                                new_uri,
                                options: None,
                                annotation_id: None,
                            },
                        )));
                    }
                }
            }
        }

        Ok(Some(WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(document_changes)),
            change_annotations: None,
        }))
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

/// One fetchable coordinate the current classpath's resolution
/// couldn't find in the local cache — a candidate for the extension's
/// consent-gated Maven Central download.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct MissingDependencyCoord {
    group: String,
    artifact: String,
    version: String,
}

/// A degraded coordinate the server deliberately will not offer to
/// download (a dynamic/unresolved version, an unsupported classifier, a
/// resolver bound) — surfaced so the extension's UI can explain the gap
/// rather than silently drop it.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct SkippedDependency {
    group: String,
    artifact: String,
    version: Option<String>,
    reason: String,
}

/// The `jvl/missingDependencies` custom request's result: the extension's
/// `java-vsix-lite.downloadDependencies` command queries this (after
/// confirming Workspace Trust) to learn what it may offer to download, and
/// re-queries it after each rebuild in its fixed-point loop.
#[derive(Debug, Serialize, Default)]
struct MissingDependenciesResult {
    missing: Vec<MissingDependencyCoord>,
    skipped: Vec<SkippedDependency>,
}

impl Backend {
    /// `jvl/missingDependencies` — reports the current classpath's
    /// degraded coordinates, split into `missing` (a real `g:a:v` absent
    /// from the local cache — fetchable) and `skipped` (resolution
    /// deliberately declined to pursue further — not fetchable, with a
    /// reason). Read-only and network-free: this only inspects whatever the
    /// last (static, offline) resolution already recorded — see
    /// `jvl_classpath::parse_degraded_entry` for the parsing rules.
    async fn missing_dependencies(&self) -> Result<MissingDependenciesResult> {
        let classpath = self.classpath();
        let mut result = MissingDependenciesResult::default();
        for coord in classpath.missing_dependencies() {
            if coord.is_fetchable() {
                result.missing.push(MissingDependencyCoord {
                    group: coord.group,
                    artifact: coord.artifact,
                    version: coord.version.unwrap_or_default(),
                });
            } else if let Some(reason) = coord.reason {
                result.skipped.push(SkippedDependency {
                    group: coord.group,
                    artifact: coord.artifact,
                    version: coord.version,
                    reason,
                });
            }
        }
        Ok(result)
    }
}

/// Adapts `jvl-classpath` to `jvl-syntax`'s `SymbolSource`, converting the
/// bytecode model into the analysis crate's external-symbol types. Holds an
/// owned snapshot `Arc` (from `Backend::classpath()`) rather than a borrow,
/// so it's unaffected by a concurrent rebuild swap mid-request.
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
                        jvl_classpath::MemberKind::Constructor => {
                            jvl_syntax::ExternalMemberKind::Constructor
                        }
                    },
                    signature: m.signature.clone(),
                    template: m.template.clone(),
                    is_static: m.is_static,
                    ret_fqn: m.ret_fqn.clone(),
                    ret_display: m.ret_display.clone(),
                })
                .collect(),
        })
    }

    /// Name-index delegation — classpath type-name completion.
    fn types_with_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> (Vec<jvl_syntax::TypeCandidate>, bool) {
        let (entries, truncated) = self.0.types_with_prefix(prefix, limit);
        (entries.into_iter().map(candidate).collect(), truncated)
    }

    /// Name-index delegation — import-path completion.
    fn package_children(&self, package: &str) -> (Vec<String>, Vec<jvl_syntax::TypeCandidate>) {
        let (subpackages, types) = self.0.package_children(package);
        (subpackages, types.into_iter().map(candidate).collect())
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

/// A classpath [`jvl_classpath::TypeEntry`] as the analysis crate's
/// [`jvl_syntax::TypeCandidate`] — same fields, crate-local types.
fn candidate(entry: jvl_classpath::TypeEntry) -> jvl_syntax::TypeCandidate {
    jvl_syntax::TypeCandidate {
        simple: entry.simple,
        fqn: entry.fqn,
        import_path: entry.import_path,
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

/// Translate an in-project completion item's lazy-resolve
/// `data.doc` (the declaring document's index in the `&[OpenDoc]` slice this
/// request ran against — `jvl-syntax` is URI-free, so an index is all it can
/// name) into that document's URI, which stays meaningful across requests.
/// `completion_resolve` uses it to re-find the exact originating document,
/// never scanning all open documents (where a same-simple-name type declared
/// elsewhere could win the lookup and attach the wrong member's Javadoc).
/// An item whose payload can't be translated (no object, no `doc` index, or
/// an out-of-range index — none reachable from `jvl-syntax`'s own output,
/// but a resolve key must never be emitted broken) loses its `data` entirely,
/// degrading to "no documentation on resolve".
fn stamp_completion_data_uri(item: &mut CompletionItem, uris: &[&str]) {
    let Some(obj) = item.data.as_mut().and_then(|d| d.as_object_mut()) else {
        return;
    };
    let Some(idx) = obj.get("doc").and_then(serde_json::Value::as_u64) else {
        // External keys ({kind, fqn, member}) carry no `doc` and need no URI.
        return;
    };
    obj.remove("doc");
    match usize::try_from(idx).ok().and_then(|i| uris.get(i)) {
        Some(uri) => {
            obj.insert(
                "uri".to_string(),
                serde_json::Value::String((*uri).to_string()),
            );
        }
        None => item.data = None,
    }
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
async fn main() -> std::process::ExitCode {
    print_version_and_exit_if_requested();

    // Logs go to stderr; stdout is the wire transport (LSP, or DAP for the
    // `dap` subcommand). ANSI is disabled because the editor's output panel
    // renders raw escape codes as a jumble; the noisy module-path target is
    // dropped; and the default filter mutes the LSP framework's debug
    // chatter (e.g. spurious cancel-request notices).
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

    // `jvl-server dap`: run the DAP↔JDWP debug adapter instead of the LSP
    // server — same binary, so packaging is unchanged. Stdout becomes the
    // DAP wire (same reserved-stdout invariant).
    if std::env::args().nth(1).as_deref() == Some("dap") {
        tracing::info!("starting java-vsix-lite debug adapter");
        return jvl_debug::run_stdio_adapter().await;
    }

    tracing::info!("starting java-vsix-lite language server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(Backend::new)
        .custom_method("jvl/externalSource", Backend::external_source)
        .custom_method("jvl/missingDependencies", Backend::missing_dependencies)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backward compatibility: no arguments, a `null`/empty first argument,
    /// an object without a `scope`, and an explicit `{"scope":"project"}` all
    /// decode to a whole-project check.
    #[test]
    fn parse_check_scope_defaults_to_project() {
        use javac::JavacCheckScope::Project;
        assert_eq!(parse_check_scope(&[]), Ok(Project));
        assert_eq!(parse_check_scope(&[serde_json::Value::Null]), Ok(Project));
        assert_eq!(parse_check_scope(&[serde_json::json!({})]), Ok(Project));
        assert_eq!(
            parse_check_scope(&[serde_json::json!({ "scope": "project" })]),
            Ok(Project)
        );
    }

    /// A well-formed `modules` request decodes to its URI list.
    #[test]
    fn parse_check_scope_modules_decodes_uris() {
        let scope = parse_check_scope(&[serde_json::json!({
            "scope": "modules",
            "documentUris": ["file:///ws/a/src/main/java/A.java", "file:///ws/b/src/main/java/B.java"],
        })])
        .expect("valid modules request");
        assert_eq!(
            scope,
            javac::JavacCheckScope::Modules {
                document_uris: vec![
                    "file:///ws/a/src/main/java/A.java".to_string(),
                    "file:///ws/b/src/main/java/B.java".to_string(),
                ],
            }
        );
    }

    /// A malformed or empty `modules` request is an error — NEVER silently
    /// widened into a project-wide compilation.
    #[test]
    fn parse_check_scope_rejects_malformed_or_empty_modules() {
        // Empty URI list.
        assert!(parse_check_scope(&[serde_json::json!({
            "scope": "modules",
            "documentUris": [],
        })])
        .is_err());
        // Missing documentUris.
        assert!(parse_check_scope(&[serde_json::json!({ "scope": "modules" })]).is_err());
        // Non-string entry.
        assert!(parse_check_scope(&[serde_json::json!({
            "scope": "modules",
            "documentUris": [42],
        })])
        .is_err());
        // Unknown scope.
        assert!(parse_check_scope(&[serde_json::json!({ "scope": "everything" })]).is_err());
        // Non-object argument.
        assert!(parse_check_scope(&[serde_json::json!("modules")]).is_err());
    }

    /// With no `initializationOptions` at all, unresolved-member
    /// diagnostics must default to **on** (the conservative member gating
    /// inside `semantic_diagnostics` is what keeps this safe, not this flag).
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

    /// The project-file cache never exceeds its cap — at
    /// the cap it clears wholesale and keeps accepting inserts (a Tier-2
    /// references request can push up to 500 files through it in one go).
    #[test]
    fn project_file_cache_clears_at_cap_and_stays_bounded() {
        let mut parser = jvl_syntax::new_parser();
        let tree = jvl_syntax::parse(&mut parser, "class X {}\n", None).expect("parse");
        let mut cache: HashMap<PathBuf, CachedProjectFile> = HashMap::new();
        let cap = 8; // small stand-in; the policy is cap-independent
        for i in 0..cap * 3 {
            insert_bounded_project_file(
                &mut cache,
                cap,
                PathBuf::from(format!("/proj/F{i}.java")),
                CachedProjectFile {
                    mtime: SystemTime::now(),
                    text: Arc::new("class X {}\n".to_string()),
                    tree: tree.clone(),
                },
            );
            assert!(
                cache.len() <= cap,
                "cache must never exceed its cap (len {} > cap {cap})",
                cache.len()
            );
        }
        // After a clear the newest entry is always present.
        assert!(cache.contains_key(Path::new(&format!("/proj/F{}.java", cap * 3 - 1))));
    }

    /// A fresh, empty temp directory for a filesystem-backed test — mirrors
    /// `references.rs`'s own `temp_dir` helper (small enough, and specific
    /// enough to each module's needs, not worth sharing across files).
    fn temp_project_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jvl-main-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A bare `Backend` for a direct (non-LSP-transport) unit test —
    /// `LspService::new` is the only public way to get a `Client` to build
    /// one; the returned service's `.inner()` hands back the `Backend`
    /// itself. The paired `ClientSocket` is dropped: these tests never send
    /// client notifications.
    fn test_backend() -> LspService<Backend> {
        let (service, _socket) = LspService::new(Backend::new);
        service
    }

    /// The oversize guard added to `parsed_project_file`: a file over
    /// `MAX_PROJECT_FILE_BYTES` must be reported exactly like an unreadable
    /// one — `None`, never read into memory, never inserted into the parse
    /// cache — rather than being parsed wholesale.
    #[test]
    fn parsed_project_file_skips_oversized_files_without_reading_or_caching_them() {
        let service = test_backend();
        let backend = service.inner();

        let dir = temp_project_dir("oversize-project-file");
        let path = dir.join("Big.java");
        let oversized_contents = "x".repeat(MAX_PROJECT_FILE_BYTES as usize + 1);
        std::fs::write(&path, &oversized_contents).expect("write oversized file");

        assert!(
            backend.parsed_project_file(&path).is_none(),
            "a file over MAX_PROJECT_FILE_BYTES must be reported unavailable, not parsed"
        );
        assert!(
            backend
                .project_file_cache
                .lock()
                .expect("cache lock")
                .is_empty(),
            "an oversized file must never be inserted into the parse-on-demand cache"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The conservative-completeness contract this whole audit item exists
    /// for: `scan_references`'s Tier-2 workspace scan must treat BOTH an
    /// oversized candidate (caught by `references::prefilter`'s own
    /// per-file guard, before it's ever read) AND an unreadable one (caught
    /// by `parsed_project_file`, after prefiltering) as incomplete —
    /// flipping the exact signals `rename()` already refuses on
    /// (`ScanOutcome::truncated` and `ScanOutcome::unparsed_hit_files`),
    /// rather than silently pretending either file simply had no
    /// occurrences.
    #[tokio::test]
    async fn scan_references_treats_oversized_and_unreadable_candidates_as_incomplete() {
        let service = test_backend();
        let backend = service.inner();

        let root = temp_project_dir("scan-references-incomplete");

        // The declaring ("target") file: always scanned directly by
        // `scan_references`'s own-file step, never through the candidate
        // loop this test is exercising.
        let target_path = root.join("Target.java");
        let target_text = "class Target {\n    void widget() {}\n}\n".to_string();
        std::fs::write(&target_path, &target_text).expect("write target file");
        let target_uri = Uri::from_file_path(&target_path)
            .expect("file uri")
            .to_string();
        let mut parser = jvl_syntax::new_parser();
        let target_tree =
            jvl_syntax::parse(&mut parser, &target_text, None).expect("parse target file");
        let name_offset = target_text.find("widget").expect("target text has widget");
        let target = jvl_syntax::ReferenceTarget {
            doc: 0,
            name_range: name_offset..name_offset + "widget".len(),
            name: "widget".to_string(),
            tier: jvl_syntax::Tier::Workspace,
        };

        // A candidate containing the needle but too large to read safely —
        // `references::prefilter`'s per-file guard must skip it via a
        // `metadata()` stat alone, and never hand it to this loop at all.
        let big_path = root.join("Big.java");
        let mut big_contents = "widget".to_string();
        big_contents.push_str(&"x".repeat(references::MAX_SINGLE_FILE_BYTES as usize));
        std::fs::write(&big_path, &big_contents).expect("write oversized candidate");

        // A candidate that passes the prefilter (small, and a plain
        // substring search never requires valid UTF-8) but is not valid
        // UTF-8 text, so `parsed_project_file`'s `read_to_string` fails and
        // it can't be parsed for the semantic confirm step.
        let bad_path = root.join("Bad.java");
        let mut bad_bytes = b"class Bad { void widget() {} }\n".to_vec();
        bad_bytes.push(0xFF); // not valid UTF-8 on its own
        std::fs::write(&bad_path, &bad_bytes).expect("write unreadable candidate");

        let open_snapshot: HashMap<PathBuf, (i32, String, Tree)> = HashMap::new();
        let outcome = backend
            .scan_references(
                &target,
                &target_uri,
                &target_text,
                &target_tree,
                1,
                std::slice::from_ref(&root),
                Some(root.as_path()),
                &open_snapshot,
                true,
            )
            .await;

        assert!(
            outcome.truncated,
            "an oversized candidate must mark the scan truncated — the same signal \
             `rename` already refuses on"
        );
        assert!(
            outcome.unparsed_hit_files > 0,
            "an unreadable candidate must be counted as unparsed — the same signal \
             `rename` already refuses on"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Absent `initializationOptions`, the debounce defaults to 2s.
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

    /// Only the four documented build-file names/locations match —
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

    /// The debounce/coalescing state machine — pure, no real timers.
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
