//! java-vsix-lite language server entry point.
//!
//! This is the single LSP server the editor talks to (see the implementation
//! plan's "Process topology"). The TypeScript extension shell launches this
//! binary over stdio and stays thin. For M0 this is a walking skeleton: it
//! speaks the LSP lifecycle, tracks open documents, and publishes a placeholder
//! diagnostic so the end-to-end pipe (editor -> TS client -> Rust server) is
//! provably wired before any analysis crates are added.
//!
//! Invariant: **stdout is reserved for the LSP wire protocol.** All logging goes
//! to stderr via `tracing`.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
struct Backend {
    client: Client,
    /// Open-document text, keyed by URI string. Bounded to open files by
    /// design — the default tier never indexes the whole workspace.
    documents: Mutex<HashMap<String, String>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
        }
    }

    /// M0 placeholder analysis: proves diagnostics flow back to the editor.
    /// Replaced in M1 by tree-sitter syntax diagnostics.
    async fn publish_placeholder_diagnostics(&self, uri: Uri, text: &str) {
        let line_count = text.lines().count().max(1) as u32;
        let diagnostic = Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
            severity: Some(DiagnosticSeverity::HINT),
            source: Some("java-vsix-lite".to_string()),
            message: format!(
                "java-vsix-lite active (default tier). Document tracked: {line_count} line(s)."
            ),
            ..Default::default()
        };
        self.client
            .publish_diagnostics(uri, vec![diagnostic], None)
            .await;
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "java-vsix-lite".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // M0 uses FULL sync so document tracking is correct without an
                // edit-application layer. M1 switches to INCREMENTAL and feeds
                // ranged edits into tree-sitter's `InputEdit` for proportional,
                // low-compute reparsing.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
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
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.documents
            .lock()
            .await
            .insert(uri.as_str().to_string(), text.clone());
        self.publish_placeholder_diagnostics(uri, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        // FULL sync: the final change in the batch carries the entire document.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        let text = change.text;
        self.documents
            .lock()
            .await
            .insert(uri.as_str().to_string(), text.clone());
        self.publish_placeholder_diagnostics(uri, &text).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().await.remove(uri.as_str());
        // Clear diagnostics for the closed file.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }
}

#[tokio::main]
async fn main() {
    // Logs go to stderr; stdout is the LSP transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("JVL_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("starting java-vsix-lite language server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
