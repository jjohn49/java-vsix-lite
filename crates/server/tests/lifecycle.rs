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

    // 7. semanticTokens/full -> non-empty token data for `Sample` and `x`.
    send(
        r#"{"jsonrpc":"2.0","id":4,"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":"file:///Sample.java"}}}"#,
    );
    let tokens = read_until(&mut reader, "\"id\":4", &mut seen);
    assert!(
        tokens.contains("\"data\":[") && !tokens.contains("\"data\":[]"),
        "expected non-empty semantic tokens: {tokens}"
    );

    // 8. shutdown -> wait for result, then exit.
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

/// M4.1: `textDocument/definition` end-to-end, ladder step (b) — a
/// cross-document member call (`this.methodFromA()` in an open doc `B` that
/// extends an open doc `A`) resolves into `A`, at `methodFromA`'s name.
#[test]
fn definition_cross_doc_round_trip() {
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

    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#);
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    assert!(
        init.contains("\"definitionProvider\":true"),
        "missing definitionProvider capability: {init}"
    );
    assert!(
        init.contains("\"typeDefinitionProvider\":true"),
        "missing typeDefinitionProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    // Doc A declares the method; doc B (extending A) calls it unqualified.
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///DefA.java","languageId":"java","version":1,"text":"class A {\n  void methodFromA() {}\n}\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///DefB.java","languageId":"java","version":1,"text":"class B extends A {\n  void m() { this.methodFromA(); }\n}\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor inside `methodFromA` at the call site in B (line 1, char 20 — the
    // "m" in "...this.methodFromA();").
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///DefB.java"},"position":{"line":1,"character":20}}}"#,
    );
    let def = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        def.contains("file:///DefA.java"),
        "expected a Location into A: {def}"
    );
    assert!(
        def.contains(r#""start":{"character":7,"line":1}"#),
        "expected the range at methodFromA's name: {def}"
    );

    send(r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":3", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}

/// M4.1: `textDocument/definition` end-to-end, ladder step (c) — a type
/// referenced from an open document but declared in an *unopened* project
/// source file resolves by locating and parsing that file on demand, using
/// the workspace folder + conventional `src/main/java` source root.
#[test]
fn definition_into_unopened_project_file() {
    let root = std::env::temp_dir().join(format!(
        "jvl-def-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    let foo_path = src_dir.join("Foo.java");
    std::fs::write(&foo_path, "package p;\n\npublic class Foo {\n}\n").expect("write Foo.java");

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

    let root_uri = format!("file://{}", root.display());
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{}},"workspaceFolders":[{{"uri":"{root_uri}","name":"proj"}}]}}}}"#
    ));
    let _ = read_until(&mut reader, "\"id\":1", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    // An *open* document, in the same package, referencing `Foo` — but `Foo`
    // itself is never opened; the server must locate it on disk.
    let use_uri = format!("file://{}", root.join("src/main/java/p/Use.java").display());
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{use_uri}","languageId":"java","version":1,"text":"package p;\nclass Use {{ Foo f; }}\n"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Foo` in `Foo f;` (line 1, char 13 — inside the type name).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/definition","params":{{"textDocument":{{"uri":"{use_uri}"}},"position":{{"line":1,"character":13}}}}}}"#
    ));
    let def = read_until(&mut reader, "\"id\":2", &mut seen);
    let foo_uri = format!("file://{}", foo_path.display());
    assert!(
        def.contains(&foo_uri),
        "expected a Location into {foo_uri}: {def}"
    );
    assert!(
        def.contains(r#""start":{"character":13,"line":2}"#),
        "expected the range at Foo's name (package p;\\n\\npublic class Foo): {def}"
    );

    send(r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":3", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// M4.1: `textDocument/definition` end-to-end, ladder step (d) — a member
/// call on an externally-typed receiver (`"".length()`) resolves to a
/// `jvl-src:` virtual-document `Location`, and the extension-side
/// `jvl/externalSource` request returns non-empty content for it (real JDK
/// source, or a signature-only stub — either is acceptable here). Skips
/// (gracefully, like `jvl-classpath`'s own JDK-gated tests) if no JDK is
/// discoverable in this environment.
#[test]
fn definition_external_jdk_member_round_trip() {
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

    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#);
    let _ = read_until(&mut reader, "\"id\":1", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///DefExt.java","languageId":"java","version":1,"text":"class C { void m() { \"\".length(); } }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `length` in `"".length();` (line 0, char 24).
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///DefExt.java"},"position":{"line":0,"character":24}}}"#,
    );
    let def = read_until(&mut reader, "\"id\":2", &mut seen);

    if def.contains("\"result\":null") {
        // No JDK discoverable in this environment — nothing further to check.
    } else {
        assert!(
            def.contains("jvl-src:/java.lang.String.java"),
            "expected a jvl-src Location for java.lang.String: {def}"
        );
        send(
            r#"{"jsonrpc":"2.0","id":3,"method":"jvl/externalSource","params":{"uri":"jvl-src:/java.lang.String.java"}}"#,
        );
        let src = read_until(&mut reader, "\"id\":3", &mut seen);
        assert!(
            src.contains("\"text\":") && !src.contains(r#""text":"""#),
            "expected non-empty external source/stub text: {src}"
        );
    }

    send(r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":4", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}
