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

use std::collections::HashMap;
use std::ops::Range as StdRange;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::SystemTime;

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
    /// them.
    classpath: OnceLock<jvl_classpath::Classpath>,
    /// Workspace root (from `initialize`), used to discover project dependencies.
    workspace_root: OnceLock<Option<PathBuf>>,
    /// Fallback project root derived from the first opened document (so deps
    /// resolve even when a lone file is opened with no workspace folder).
    project_root_hint: OnceLock<Option<PathBuf>>,
    /// Whether to emit unresolved-member diagnostics (opt-in; default off).
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
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            parser: StdMutex::new(jvl_syntax::new_parser()),
            documents: Mutex::new(HashMap::new()),
            encoding: OnceLock::new(),
            snippet_support: OnceLock::new(),
            classpath: OnceLock::new(),
            workspace_root: OnceLock::new(),
            project_root_hint: OnceLock::new(),
            unresolved_member_diagnostics: OnceLock::new(),
            project_file_cache: StdMutex::new(HashMap::new()),
            external_stub_cache: StdMutex::new(HashMap::new()),
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
    fn classpath(&self) -> &jvl_classpath::Classpath {
        self.classpath.get_or_init(|| {
            jvl_classpath::Classpath::from_jdk_and_project(self.project_root().as_deref())
        })
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

    /// Parse (or reuse a cached parse of) a single project source file for
    /// ladder step (c). At most one file is read per candidate tried, and the
    /// caller (`resolve_location`) stops at the first hit — no directory
    /// walking or indexing.
    fn locate_in_project_file(
        &self,
        path: &Path,
        simple_name: &str,
    ) -> Option<(StdRange<usize>, Arc<String>)> {
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
        let range = jvl_syntax::locate_type_in_source(&cached.tree, &cached.text, simple_name)?;
        Some((range, Arc::clone(&cached.text)))
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
    /// unresolved-member diagnostics when that opt-in setting is enabled.
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

/// The opt-in `unresolvedMemberDiagnostics` flag from `initializationOptions`.
fn unresolved_member_diagnostics_opt(params: &InitializeParams) -> bool {
    params
        .initialization_options
        .as_ref()
        .and_then(|opts| opts.get("unresolvedMemberDiagnostics"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
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
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
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
/// bytecode model into the analysis crate's external-symbol types.
struct ClasspathSymbols<'a>(&'a jvl_classpath::Classpath);

impl jvl_syntax::SymbolSource for ClasspathSymbols<'_> {
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
