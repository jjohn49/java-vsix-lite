//! DAP wire plumbing: `Content-Length` framing over stdio (identical framing
//! to LSP) plus serde structs for exactly the request arguments this adapter
//! supports. Responses and events are built as `serde_json` values by the
//! session.

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Stdin, Stdout};

/// Cap on an accepted DAP message body (the client is trusted — VS Code —
/// but bounded work is this codebase's posture everywhere).
const MAX_BODY_LEN: usize = 16 * 1024 * 1024;

/// Read one framed DAP message from stdin. `Ok(None)` on clean EOF.
pub async fn read_message(stdin: &mut BufReader<Stdin>) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = stdin.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(len) = content_length.filter(|&l| l <= MAX_BODY_LEN) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing or oversized Content-Length",
        ));
    };
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Sequenced, framed writer for responses and events on stdout (stdout is
/// the DAP wire — nothing else may print there).
pub struct DapWriter {
    out: Stdout,
    seq: i64,
}

impl DapWriter {
    pub fn new() -> DapWriter {
        DapWriter {
            out: tokio::io::stdout(),
            seq: 0,
        }
    }

    async fn send(&mut self, mut message: Value) {
        self.seq += 1;
        message["seq"] = Value::from(self.seq);
        let body = message.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        // A write failure means the client is gone; the session's stdin
        // reader observes the same condition and shuts the adapter down.
        let _ = self.out.write_all(frame.as_bytes()).await;
        let _ = self.out.flush().await;
    }

    pub async fn respond(
        &mut self,
        request_seq: i64,
        command: &str,
        result: Result<Value, String>,
    ) {
        let message = match result {
            Ok(body) => serde_json::json!({
                "type": "response",
                "request_seq": request_seq,
                "command": command,
                "success": true,
                "body": body,
            }),
            Err(text) => serde_json::json!({
                "type": "response",
                "request_seq": request_seq,
                "command": command,
                "success": false,
                "message": text,
            }),
        };
        self.send(message).await;
    }

    pub async fn event(&mut self, name: &str, body: Value) {
        self.send(serde_json::json!({
            "type": "event",
            "event": name,
            "body": body,
        }))
        .await;
    }

    /// An `output` event (`category`: `stdout` | `stderr` | `console`).
    pub async fn output(&mut self, category: &str, text: &str) {
        self.event(
            "output",
            serde_json::json!({ "category": category, "output": text }),
        )
        .await;
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LaunchArgs {
    pub main_class: Option<String>,
    pub args: Vec<String>,
    pub vm_args: Vec<String>,
    pub cwd: Option<String>,
    pub env: std::collections::HashMap<String, String>,
    pub project_root: Option<String>,
    pub class_paths: Vec<String>,
    pub stop_on_entry: bool,
    /// Injected by the extension from the machine-scoped
    /// `java-vsix-lite.jdk.home` setting — never workspace-controlled.
    #[serde(rename = "__jvlJdkHome")]
    pub jdk_home: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AttachArgs {
    pub host_name: Option<String>,
    pub port: Option<u16>,
    /// Connect deadline, milliseconds.
    pub timeout: Option<u64>,
    pub project_root: Option<String>,
    pub source_paths: Vec<String>,
}

#[derive(Deserialize)]
pub struct Source {
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub struct SourceBreakpoint {
    pub line: u32,
}

#[derive(Deserialize)]
pub struct SetBreakpointsArgs {
    pub source: Source,
    #[serde(default)]
    pub breakpoints: Vec<SourceBreakpoint>,
    /// DAP-deprecated alternative to `breakpoints` some clients still send.
    #[serde(default)]
    pub lines: Vec<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SetExceptionBreakpointsArgs {
    pub filters: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadIdArgs {
    pub thread_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackTraceArgs {
    pub thread_id: i64,
    #[serde(default)]
    pub start_frame: u32,
    #[serde(default)]
    pub levels: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameIdArgs {
    pub frame_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VariablesArgs {
    pub variables_reference: i64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DisconnectArgs {
    pub terminate_debuggee: Option<bool>,
}
