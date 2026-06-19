//! End-to-end test driving the real `jvl-server` binary over raw LSP
//! (JSON-RPC + `Content-Length` framing on stdio).
//!
//! Covers the lifecycle and the M1 default-tier features end to end:
//! `initialize` (capabilities) -> open invalid Java (syntax diagnostic) ->
//! full-replace to valid (diagnostics clear) -> INCREMENTAL ranged edits
//! (delete `;` -> error, re-insert -> clear) -> `documentSymbol` (outline) ->
//! `shutdown` -> `exit`.
//!
//! It drives the server like a real client would — waiting for each response
//! before sending the next request — because the server handles messages
//! concurrently, so firing everything at once lets `exit` race ahead and tear
//! the server down mid-handshake.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

/// Frame a JSON-RPC payload with LSP `Content-Length` headers.
fn frame(payload: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload)
}

/// Read a single `Content-Length`-framed message body, or `None` on EOF.
fn read_frame(reader: &mut BufReader<ChildStdout>) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // blank line terminates headers
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Read frames until one matches `needle`, accumulating everything seen.
/// Bounded so a missing message fails fast instead of hanging.
fn read_until(reader: &mut BufReader<ChildStdout>, needle: &str, seen: &mut Vec<String>) -> String {
    for _ in 0..64 {
        let frame = read_frame(reader).expect("server closed stdout early");
        seen.push(frame.clone());
        if frame.contains(needle) {
            return frame;
        }
    }
    panic!("did not observe {needle:?} within 64 frames; saw:\n{seen:#?}");
}

#[test]
fn lifecycle_smoke() {
    let bin = env!("CARGO_BIN_EXE_jvl-server");
    let mut child: Child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn jvl-server");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let mut send = |msg: &str| {
        stdin
            .write_all(frame(msg).as_bytes())
            .expect("write to server")
    };
    let mut seen: Vec<String> = Vec::new();

    // 1. initialize -> wait for result.
    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#);
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    assert!(
        init.contains("java-vsix-lite"),
        "missing serverInfo: {init}"
    );
    assert!(
        init.contains("\"textDocumentSync\":2") || init.contains("\"change\":2"),
        "missing INCREMENTAL sync capability: {init}"
    );

    // 2. initialized + didOpen of *invalid* Java -> expect a syntax diagnostic.
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///Sample.java","languageId":"java","version":1,"text":"class Sample {\n"}}}"#,
    );
    let diag = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        diag.contains("Syntax error"),
        "expected syntax error: {diag}"
    );
    assert!(diag.contains("file:///Sample.java"), "wrong uri: {diag}");
    assert!(
        diag.contains("\"severity\":1"),
        "expected ERROR severity: {diag}"
    );

    // 3. didChange to *valid* Java -> diagnostics must clear (reparse on change).
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///Sample.java","version":2},"contentChanges":[{"text":"class Sample { int x; }\n"}]}}"#,
    );
    let cleared = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        cleared.contains("\"diagnostics\":[]"),
        "diagnostics should clear after fix: {cleared}"
    );

    // 4. INCREMENTAL ranged edit: delete the `;` (col 20) -> missing-semicolon.
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///Sample.java","version":3},"contentChanges":[{"range":{"start":{"line":0,"character":20},"end":{"line":0,"character":21}},"text":""}]}}"#,
    );
    let broke = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        broke.contains("Syntax error") && broke.contains("\"severity\":1"),
        "ranged delete should reintroduce a syntax error: {broke}"
    );

    // 5. INCREMENTAL ranged edit: re-insert the `;` -> diagnostics clear again.
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///Sample.java","version":4},"contentChanges":[{"range":{"start":{"line":0,"character":20},"end":{"line":0,"character":20}},"text":";"}]}}"#,
    );
    let refixed = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        refixed.contains("\"diagnostics\":[]"),
        "ranged insert should clear diagnostics: {refixed}"
    );

    // 6. documentSymbol -> outline with the class and its field.
    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":"file:///Sample.java"}}}"#,
    );
    let outline = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        outline.contains("Sample"),
        "outline missing class: {outline}"
    );
    assert!(
        outline.contains("\"kind\":5"),
        "expected CLASS kind: {outline}"
    );
    assert!(outline.contains('x'), "outline missing field: {outline}");
    assert!(
        outline.contains("\"kind\":8"),
        "expected FIELD kind: {outline}"
    );

    // 7. shutdown -> wait for result, then exit.
    send(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#);
    let shutdown = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(!shutdown.contains("error"), "shutdown errored: {shutdown}");
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);

    // Drain anything remaining and confirm a clean exit.
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}
