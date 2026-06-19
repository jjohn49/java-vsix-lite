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
use std::path::PathBuf;
use std::sync::{Mutex as StdMutex, OnceLock};

use jvl_syntax::tree_sitter::{Parser, Tree};
use jvl_syntax::{LineIndex, PositionEncoding};
use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

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
    /// plus the workspace's declared dependencies.
    fn classpath(&self) -> &jvl_classpath::Classpath {
        self.classpath.get_or_init(|| {
            let root = self.workspace_root.get().and_then(|r| r.as_deref());
            jvl_classpath::Classpath::from_jdk_and_project(root)
        })
    }

    /// Parse `text`, reusing `old` for an incremental reparse when the caller has
    /// already applied the corresponding `InputEdit`s to it.
    fn parse(&self, text: &str, old: Option<&Tree>) -> Tree {
        let mut parser = self.parser.lock().expect("parser mutex poisoned");
        jvl_syntax::parse(&mut parser, text, old).expect("parser yields a tree for in-memory text")
    }

    fn diagnostics_for(&self, text: &str, tree: &Tree) -> Vec<Diagnostic> {
        let index = LineIndex::new(text, self.encoding());
        jvl_syntax::syntax_diagnostics(tree, &index)
    }

    /// Parse a freshly opened (or fully replaced) document from scratch, store
    /// it, and publish its diagnostics.
    async fn open_document(&self, uri: Uri, text: String) {
        let tree = self.parse(&text, None);
        let diagnostics = self.diagnostics_for(&text, &tree);
        self.documents
            .lock()
            .await
            .insert(uri.as_str().to_string(), Document { text, tree });
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
                completion_provider: Some(CompletionOptions {
                    // `.` requests member completion; identifier/keyword
                    // completion is requested explicitly (Ctrl-Space) or by the
                    // editor as the user types.
                    trigger_characters: Some(vec![".".to_string()]),
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
                        // A full replacement earlier in the batch invalidated the
                        // tree; skip incremental edits and reparse from scratch.
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
            self.diagnostics_for(&doc.text, &doc.tree)
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
}

/// Adapts `jvl-classpath` to `jvl-syntax`'s `SymbolSource`, converting the
/// bytecode model into the analysis crate's external-symbol types.
struct ClasspathSymbols<'a>(&'a jvl_classpath::Classpath);

impl jvl_syntax::SymbolSource for ClasspathSymbols<'_> {
    fn class(&self, fqn: &str) -> Option<jvl_syntax::ExternalClass> {
        let info = self.0.class(fqn)?;
        Some(jvl_syntax::ExternalClass {
            supers: info.supers.clone(),
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
                    is_static: m.is_static,
                })
                .collect(),
        })
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

#[tokio::main]
async fn main() {
    // Logs go to stderr; stdout is the LSP transport. ANSI is disabled because
    // the editor's output panel renders raw escape codes as a jumble; the noisy
    // module-path target is dropped; and the default filter mutes the LSP
    // framework's debug chatter (e.g. spurious cancel-request notices).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("JVL_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tower_lsp_server=warn")),
        )
        .init();

    tracing::info!("starting java-vsix-lite language server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
