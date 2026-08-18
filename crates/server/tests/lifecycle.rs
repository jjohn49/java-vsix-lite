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

use serde_json::Value;

/// Frame a JSON-RPC payload with LSP `Content-Length` headers.
fn frame(payload: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload)
}

/// Escape a Java source string for embedding as a JSON string literal's
/// contents inside one of this file's hand-written request bodies —
/// backslashes and quotes (`"hello"` literals are common in real source),
/// then newlines. Order matters: backslashes first, so escaping quotes and
/// newlines doesn't get double-escaped.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
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

/// M4.2: `textDocument/signatureHelp` end-to-end — a call site with two
/// in-project overloads (`helper(int)` / `helper(int, int)`) returns both
/// signatures, with `activeParameter` correctly picking out the second
/// argument (one comma precedes the cursor).
#[test]
fn signature_help_round_trip() {
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
        init.contains("\"signatureHelpProvider\""),
        "missing signatureHelpProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    // Line 3, "  void m() { helper(1, 2); }" — character 23 is the `2` in
    // `helper(1, 2)`, one comma past the open paren.
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///Sig.java","languageId":"java","version":1,"text":"class Sample {\n  void helper(int a) {}\n  void helper(int a, int b) {}\n  void m() { helper(1, 2); }\n}\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/signatureHelp","params":{"textDocument":{"uri":"file:///Sig.java"},"position":{"line":3,"character":23}}}"#,
    );
    let help = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        help.contains("helper(int a)") && help.contains("helper(int a, int b)"),
        "expected both overloads: {help}"
    );
    assert!(
        help.contains("\"activeParameter\":1"),
        "expected activeParameter 1 (second argument): {help}"
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

/// M5 (5.3b): parameterized-supertype type arguments flow through the real
/// classpath into inherited-member hover, end to end. `ArrayList<String>`
/// does NOT declare `stream()` — it inherits it from `java.util.Collection`
/// (a default method), so rendering `Stream<String> stream()` proves the
/// whole chain: `ClassInfo::super_type_args` → `ClasspathSymbols` →
/// `walk_members`'s substitution mapping. (Hovering `get` would prove
/// nothing: `ArrayList` declares `get` itself, and direct-member
/// substitution predates this work.) Skips gracefully (like the definition
/// round-trip above and `jvl-classpath`'s own JDK-gated tests) if no JDK is
/// discoverable.
#[test]
fn hover_inherited_generic_member_jdk_round_trip() {
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
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///HoverGen.java","languageId":"java","version":1,"text":"import java.util.ArrayList; class H { void m() { new ArrayList<String>().stream(); } }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `stream` in `new ArrayList<String>().stream()` (line 0, char 73).
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///HoverGen.java"},"position":{"line":0,"character":73}}}"#,
    );
    let hover = read_until(&mut reader, "\"id\":2", &mut seen);

    if hover.contains("\"result\":null") {
        // No JDK discoverable in this environment — nothing further to check.
    } else {
        assert!(
            hover.contains("Stream<String> stream()"),
            "inherited member should substitute the use-site type argument: {hover}"
        );
    }

    send(r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":3", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}

/// M4.5: `workspace/symbol` end-to-end — the lazy, bounded index built on
/// the first request finds a top-level type declared in an *unopened*
/// project source file (via the conventional `src/main/java` source root),
/// with a zero-length 0:0 range (never parsed). Once that same file is
/// opened, a later query for the same name returns the *live*
/// `document_symbol`-derived range instead — the open-document-shadows-the-
/// index behavior.
#[test]
fn workspace_symbol_unopened_then_shadowed_by_open_doc() {
    let root = std::env::temp_dir().join(format!(
        "jvl-wsym-test-{}-{}",
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
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    assert!(
        init.contains("\"workspaceSymbolProvider\":true"),
        "missing workspaceSymbolProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    let foo_uri = format!("file://{}", foo_path.display());

    // 1. `Foo` is never opened -> the first workspace/symbol query builds
    // the on-disk index and finds it, with a zero-length range at 0:0.
    send(r#"{"jsonrpc":"2.0","id":2,"method":"workspace/symbol","params":{"query":"Foo"}}"#);
    let unopened = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        unopened.contains(&foo_uri),
        "expected a result pointing at {foo_uri}: {unopened}"
    );
    assert!(
        unopened.contains(r#""start":{"character":0,"line":0}"#)
            && unopened.contains(r#""end":{"character":0,"line":0}"#),
        "expected a zero-length 0:0 range for the unopened file: {unopened}"
    );

    // 2. Camel-hump query: "Fo" alone also matches (substring), and "F" would
    // too, but exercise the documented camel-hump rule with a query that
    // isn't a plain substring is covered by the module's own unit tests;
    // here just confirm a case-insensitive substring query also finds it.
    send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/symbol","params":{"query":"foo"}}"#);
    let ci = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        ci.contains(&foo_uri),
        "expected a case-insensitive match for 'foo': {ci}"
    );

    // 3. A non-matching query returns no results.
    send(r#"{"jsonrpc":"2.0","id":4,"method":"workspace/symbol","params":{"query":"NoSuchType"}}"#);
    let none = read_until(&mut reader, "\"id\":4", &mut seen);
    assert!(
        none.contains("\"result\":[]"),
        "expected an empty result for a non-matching query: {none}"
    );

    // 4. Open `Foo.java` -> a later query must return the *live* range from
    // `document_symbol` (line 2, char 13 — "Foo" in "public class Foo"),
    // shadowing the index's zero-length entry for the same path.
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{foo_uri}","languageId":"java","version":1,"text":"package p;\n\npublic class Foo {{\n}}\n"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    send(r#"{"jsonrpc":"2.0","id":5,"method":"workspace/symbol","params":{"query":"Foo"}}"#);
    let shadowed = read_until(&mut reader, "\"id\":5", &mut seen);
    assert!(
        shadowed.contains(&foo_uri),
        "expected a result pointing at {foo_uri}: {shadowed}"
    );
    assert!(
        shadowed.contains(r#""start":{"character":13,"line":2}"#),
        "expected the live document_symbol range once Foo.java is open: {shadowed}"
    );
    assert!(
        !shadowed.contains(r#""start":{"character":0,"line":0}"#),
        "the stale zero-position index entry for {foo_uri} must not still appear once the \
         doc is open — shadowing must replace it, not append alongside it: {shadowed}"
    );

    send(r#"{"jsonrpc":"2.0","id":6,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":6", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
}

fn temp_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "jvl-refs-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ))
}

/// M4 (4.3): `textDocument/references` end-to-end, Tier 2 (Workspace
/// visibility — `public`) — a public class's references are found across
/// three files: the declaring file (open, cursor on its own declaration
/// name) plus two same-package unopened files on disk, each holding a plain
/// field-typed use. `includeDeclaration` gates whether the declaration's own
/// occurrence is included.
#[test]
fn references_cross_file_public_class_round_trip() {
    let root = temp_root("cross-file");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    std::fs::write(
        src_dir.join("UseB.java"),
        "package p;\nclass UseB {\n  Foo f;\n}\n",
    )
    .expect("write UseB.java");
    std::fs::write(
        src_dir.join("UseC.java"),
        "package p;\nclass UseC {\n  Foo f;\n}\n",
    )
    .expect("write UseC.java");
    let foo_path = src_dir.join("Foo.java");
    let foo_text = "package p;\n\npublic class Foo {\n}\n";
    std::fs::write(&foo_path, foo_text).expect("write Foo.java");

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
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    assert!(
        init.contains("\"referencesProvider\":true"),
        "missing referencesProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    let foo_uri = format!("file://{}", foo_path.display());
    let use_b_uri = format!("file://{}", src_dir.join("UseB.java").display());
    let use_c_uri = format!("file://{}", src_dir.join("UseC.java").display());

    // Open only Foo.java (the declaring file); UseB/UseC stay on disk.
    let escaped_foo_text = foo_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{foo_uri}","languageId":"java","version":1,"text":"{escaped_foo_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Foo` in `public class Foo` (line 2, char 13).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/references","params":{{"textDocument":{{"uri":"{foo_uri}"}},"position":{{"line":2,"character":13}},"context":{{"includeDeclaration":false}}}}}}"#
    ));
    let without_decl = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        without_decl.contains(&use_b_uri),
        "expected a result in UseB.java: {without_decl}"
    );
    assert!(
        without_decl.contains(&use_c_uri),
        "expected a result in UseC.java: {without_decl}"
    );
    assert!(
        !without_decl.contains(&foo_uri),
        "declaration itself must be excluded when includeDeclaration is false: {without_decl}"
    );

    send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"textDocument/references","params":{{"textDocument":{{"uri":"{foo_uri}"}},"position":{{"line":2,"character":13}},"context":{{"includeDeclaration":true}}}}}}"#
    ));
    let with_decl = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        with_decl.contains(&use_b_uri)
            && with_decl.contains(&use_c_uri)
            && with_decl.contains(&foo_uri),
        "expected results in all three files when includeDeclaration is true: {with_decl}"
    );

    send(r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":4", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// M4 (4.3): `textDocument/references` end-to-end — a workspace with more
/// `.java` files under the source root than the hardcoded 500-file scan cap
/// surfaces truncation via a `window/showMessage` (Info) notification,
/// worded per the task brief.
#[test]
fn references_truncation_notice_round_trip() {
    let root = temp_root("truncation");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    // Comfortably over the 500-file cap so the scan is truncated regardless
    // of directory-walk order.
    for i in 0..510 {
        std::fs::write(
            src_dir.join(format!("Filler{i}.java")),
            format!("package p;\nclass Filler{i} {{}}\n"),
        )
        .expect("write filler file");
    }
    let target_path = src_dir.join("Trigger.java");
    let target_text = "package p;\n\npublic class Trigger {\n}\n";
    std::fs::write(&target_path, target_text).expect("write Trigger.java");

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

    let target_uri = format!("file://{}", target_path.display());
    let escaped_text = target_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{target_uri}","languageId":"java","version":1,"text":"{escaped_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Trigger` in `public class Trigger` (line 2, char 13).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/references","params":{{"textDocument":{{"uri":"{target_uri}"}},"position":{{"line":2,"character":13}},"context":{{"includeDeclaration":true}}}}}}"#
    ));
    // The `window/showMessage` notification and the `id:2` response reach
    // stdout via independent server output paths, so their relative order is
    // NOT guaranteed. Wait for the notification first (`read_until`
    // accumulates whatever else arrives — possibly the response — into
    // `seen`), then only read further frames for the response if it didn't
    // already race ahead of the notification; a second blocking read for a
    // frame that was already consumed would hang forever.
    let notice = read_until(&mut reader, "window/showMessage", &mut seen);
    assert!(
        notice.contains("truncated at 500 files"),
        "expected the task brief's truncation wording: {notice}"
    );
    // The request itself must still complete (the declaring file's own
    // occurrence, at minimum, is always found directly).
    if !seen.iter().any(|f| f.contains("\"id\":2")) {
        let _ = read_until(&mut reader, "\"id\":2", &mut seen);
    }

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

/// M4 (4.3): `textDocument/references` end-to-end — a file under `target/`
/// (build output) whose content textually matches the searched identifier is
/// never scanned, even though it lies under the workspace root.
#[test]
fn references_skips_target_directory() {
    let root = temp_root("skip-target");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    let marker_path = src_dir.join("Marker.java");
    let marker_text = "package p;\n\npublic class Marker {\n}\n";
    std::fs::write(&marker_path, marker_text).expect("write Marker.java");

    // Build-output directory holding a textual (but not semantic) match —
    // must never be scanned.
    let build_dir = root.join("target/generated/p");
    std::fs::create_dir_all(&build_dir).expect("create target/ dir");
    std::fs::write(
        build_dir.join("Fake.java"),
        "package p;\nclass Fake {\n  Marker m;\n}\n",
    )
    .expect("write file under target/");

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

    let marker_uri = format!("file://{}", marker_path.display());
    let escaped_text = marker_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{marker_uri}","languageId":"java","version":1,"text":"{escaped_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Marker` in `public class Marker` (line 2, char 13).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/references","params":{{"textDocument":{{"uri":"{marker_uri}"}},"position":{{"line":2,"character":13}},"context":{{"includeDeclaration":false}}}}}}"#
    ));
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        !result.contains("Fake.java"),
        "a file under target/ must never be scanned, even with a textual match: {result}"
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

/// M4 (4.6): `textDocument/implementation` end-to-end — an interface (open)
/// with two same-package files on disk: one implements it (`Bar`), the other
/// doesn't (`Other`). The request confirms the on-disk implementor and
/// excludes the unrelated file, exactly like `references`'s bounded
/// workspace scan.
#[test]
fn implementation_cross_file_round_trip() {
    let root = temp_root("impl-cross-file");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    std::fs::write(
        src_dir.join("Bar.java"),
        "package p;\nclass Bar implements Foo {\n  public void run() {}\n}\n",
    )
    .expect("write Bar.java");
    // Textually mentions `Foo` (so it survives the bounded prefilter's
    // substring scan) but doesn't `implements` it — the per-file confirm
    // (not just the prefilter) must exclude it.
    std::fs::write(
        src_dir.join("Other.java"),
        "package p;\nclass Other {\n  Foo f;\n  void run() {}\n}\n",
    )
    .expect("write Other.java");
    let foo_path = src_dir.join("Foo.java");
    let foo_text = "package p;\n\npublic interface Foo {\n  void run();\n}\n";
    std::fs::write(&foo_path, foo_text).expect("write Foo.java");

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
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    assert!(
        init.contains("\"implementationProvider\":true"),
        "missing implementationProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    let foo_uri = format!("file://{}", foo_path.display());
    let bar_uri = format!("file://{}", src_dir.join("Bar.java").display());
    let other_uri = format!("file://{}", src_dir.join("Other.java").display());

    // Open only Foo.java (the declaring file); Bar/Other stay on disk.
    let escaped_foo_text = foo_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{foo_uri}","languageId":"java","version":1,"text":"{escaped_foo_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Type-level: cursor on `Foo` in `public interface Foo` (line 2, char 17).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/implementation","params":{{"textDocument":{{"uri":"{foo_uri}"}},"position":{{"line":2,"character":17}}}}}}"#
    ));
    let type_level = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        type_level.contains(&bar_uri),
        "expected Bar.java (the implementor): {type_level}"
    );
    assert!(
        !type_level.contains(&other_uri),
        "Other.java doesn't implement Foo — must be excluded: {type_level}"
    );

    // Method-level: cursor on `run` in `void run();` (line 3, char 7).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"textDocument/implementation","params":{{"textDocument":{{"uri":"{foo_uri}"}},"position":{{"line":3,"character":7}}}}}}"#
    ));
    let method_level = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        method_level.contains(&bar_uri),
        "expected Bar.java's overriding `run`: {method_level}"
    );
    assert!(
        !method_level.contains(&other_uri),
        "Other.java's `run` is unrelated (no `implements Foo`) — must be excluded: {method_level}"
    );

    send(r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":4", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// M4 (4.6): `textDocument/implementation` end-to-end — reuses the exact
/// same bounded-prefilter cap and truncation notice wording as `references`
/// (M4.3): a workspace with more `.java` files than the 500-file scan cap
/// still surfaces the identical `window/showMessage` text.
#[test]
fn implementation_truncation_notice_reuses_references_wording() {
    let root = temp_root("impl-truncation");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    for i in 0..510 {
        std::fs::write(
            src_dir.join(format!("Filler{i}.java")),
            format!("package p;\nclass Filler{i} {{}}\n"),
        )
        .expect("write filler file");
    }
    let target_path = src_dir.join("Trigger.java");
    let target_text = "package p;\n\npublic interface Trigger {\n}\n";
    std::fs::write(&target_path, target_text).expect("write Trigger.java");

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

    let target_uri = format!("file://{}", target_path.display());
    let escaped_text = target_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{target_uri}","languageId":"java","version":1,"text":"{escaped_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Trigger` in `public interface Trigger` (line 2, char 17).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/implementation","params":{{"textDocument":{{"uri":"{target_uri}"}},"position":{{"line":2,"character":17}}}}}}"#
    ));
    // Same race as `references_truncation_notice_round_trip`: the
    // notification and the `id:2` response arrive via independent output
    // paths, so order isn't guaranteed.
    let notice = read_until(&mut reader, "window/showMessage", &mut seen);
    assert!(
        notice.contains("truncated at 500 files") && notice.contains("References search"),
        "expected the exact same (reused, not new) truncation wording as `references`: {notice}"
    );
    if !seen.iter().any(|f| f.contains("\"id\":2")) {
        let _ = read_until(&mut reader, "\"id\":2", &mut seen);
    }

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

/// M4 (4.4): `textDocument/rename` end-to-end — a local variable's
/// declaration and both outer occurrences are edited in the one open
/// document; a same-named variable in a *shadowed* inner block is untouched
/// (no edit at line 5, where the shadow's declaration/uses live).
#[test]
fn rename_local_variable_in_one_doc_shadow_untouched() {
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
        init.contains("\"renameProvider\"") && init.contains("\"prepareProvider\":true"),
        "missing renameProvider (prepareProvider) capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    // class Sample {
    //   void m() {
    //     int count = 0;             (line 2, char 8: "count")
    //     count = 1;                 (line 3, char 4)
    //     print(count);              (line 4, char 10)
    //     { int count = 2; count = 3; print(count); }   (line 5 — shadow)
    //   }
    // }
    let text = "class Sample {\\n  void m() {\\n    int count = 0;\\n    count = 1;\\n    \
                print(count);\\n    { int count = 2; count = 3; print(count); }\\n  }\\n}\\n";
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///RenameLocal.java","languageId":"java","version":1,"text":"{text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///RenameLocal.java"},"position":{"line":2,"character":8},"newName":"total"}}"#,
    );
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        !result.contains("\"error\""),
        "rename must succeed: {result}"
    );
    assert_eq!(
        result.matches(r#""newText":"total""#).count(),
        3,
        "expected exactly 3 edits (declaration + 2 outer uses): {result}"
    );
    assert!(
        result.contains(r#""start":{"character":8,"line":2}"#),
        "missing declaration edit: {result}"
    );
    assert!(
        result.contains(r#""start":{"character":4,"line":3}"#),
        "missing first outer use edit: {result}"
    );
    assert!(
        result.contains(r#""start":{"character":10,"line":4}"#),
        "missing second outer use edit: {result}"
    );
    assert!(
        !result.contains(r#""line":5"#),
        "shadowed inner `count` (line 5) must not be touched: {result}"
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

/// M4 (4.4): `textDocument/prepareRename` end-to-end — an external (JDK)
/// symbol and a keyword/literal are both refused (`result: null`), never an
/// error (the client should simply not offer rename UI for these).
#[test]
fn prepare_rename_refuses_external_symbol_and_keyword_and_literal() {
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
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///PrepExt.java","languageId":"java","version":1,"text":"import java.util.List;\nclass C { List l; }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `List` in `List l;` (line 1, char 10) — external (JDK) symbol.
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/prepareRename","params":{"textDocument":{"uri":"file:///PrepExt.java"},"position":{"line":1,"character":10}}}"#,
    );
    let ext = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        ext.contains("\"result\":null"),
        "external symbol must be refused with a null result: {ext}"
    );

    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///PrepKw.java","languageId":"java","version":1,"text":"class C { void m() { boolean b = true; } }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `boolean` (line 0, char 21) — a keyword/primitive type token.
    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/prepareRename","params":{"textDocument":{"uri":"file:///PrepKw.java"},"position":{"line":0,"character":21}}}"#,
    );
    let kw = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        kw.contains("\"result\":null"),
        "keyword must be refused with a null result: {kw}"
    );

    // Cursor on `true` (line 0, char 33) — a literal.
    send(
        r#"{"jsonrpc":"2.0","id":4,"method":"textDocument/prepareRename","params":{"textDocument":{"uri":"file:///PrepKw.java"},"position":{"line":0,"character":33}}}"#,
    );
    let lit = read_until(&mut reader, "\"id\":4", &mut seen);
    assert!(
        lit.contains("\"result\":null"),
        "literal must be refused with a null result: {lit}"
    );

    send(r#"{"jsonrpc":"2.0","id":5,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":5", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}

/// M4 (4.4): `textDocument/rename` end-to-end — an invalid new name
/// ("123abc": leading digit; "class": a reserved word) is refused with an
/// LSP error, never a (partial) edit.
#[test]
fn rename_rejects_invalid_new_name() {
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
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///InvalidName.java","languageId":"java","version":1,"text":"class C { void m() { int count = 0; count++; } }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `count` at its declaration (line 0, char 25 — "int count = 0").
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///InvalidName.java"},"position":{"line":0,"character":25},"newName":"123abc"}}"#,
    );
    let digit = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        digit.contains("\"error\""),
        "leading-digit new name must be refused: {digit}"
    );

    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///InvalidName.java"},"position":{"line":0,"character":25},"newName":"class"}}"#,
    );
    let reserved = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        reserved.contains("\"error\""),
        "reserved-word new name must be refused: {reserved}"
    );

    // M4.4 fix round 1: `goto`/`const` (JLS §3.9 reserved-but-unusable
    // keywords) and a lone `_` (reserved since Java 9) are absent from the
    // completion-oriented keyword list but must still be refused, with the
    // invalid-name error specifically.
    for (id, name) in [(4, "goto"), (5, "const"), (6, "_")] {
        send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"textDocument/rename","params":{{"textDocument":{{"uri":"file:///InvalidName.java"}},"position":{{"line":0,"character":25}},"newName":"{name}"}}}}"#
        ));
        let refused = read_until(&mut reader, &format!("\"id\":{id}"), &mut seen);
        assert!(
            refused.contains("\"error\"") && refused.contains("not a valid Java identifier"),
            "reserved `{name}` must be refused with the invalid-name error: {refused}"
        );
    }

    send(r#"{"jsonrpc":"2.0","id":7,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":7", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}

/// M4 (4.4): `textDocument/rename` end-to-end — the same-name collision
/// guard refuses renaming local `a` to `b` when `b` already exists in the
/// same enclosing scope.
#[test]
fn rename_refuses_same_scope_collision() {
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
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///Collide.java","languageId":"java","version":1,"text":"class C { void m() { int b = 0; int a = 1; print(a); } }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `a` at its declaration (line 0, char 36 — "int a = 1").
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///Collide.java"},"position":{"line":0,"character":36},"newName":"b"}}"#,
    );
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        result.contains("\"error\"") && result.contains("already in scope"),
        "renaming into a name already bound in the same scope must be refused: {result}"
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

/// M4 (4.4): `textDocument/rename` end-to-end — a workspace with more
/// `.java` files under the source root than the hardcoded 500-file scan cap
/// refuses the rename outright (never a partial edit), with a message
/// mentioning full confirmation, per the task brief's wording.
#[test]
fn rename_refuses_when_scan_truncated() {
    let root = temp_root("rename-truncation");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    for i in 0..510 {
        std::fs::write(
            src_dir.join(format!("Filler{i}.java")),
            format!("package p;\nclass Filler{i} {{}}\n"),
        )
        .expect("write filler file");
    }
    let target_path = src_dir.join("Trigger.java");
    let target_text = "package p;\n\npublic class Trigger {\n}\n";
    std::fs::write(&target_path, target_text).expect("write Trigger.java");

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

    let target_uri = format!("file://{}", target_path.display());
    let escaped_text = target_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{target_uri}","languageId":"java","version":1,"text":"{escaped_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Trigger` in `public class Trigger` (line 2, char 13).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{{"textDocument":{{"uri":"{target_uri}"}},"position":{{"line":2,"character":13}},"newName":"Renamed"}}}}"#
    ));
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        result.contains("\"error\"") && result.contains("full confirmation"),
        "truncated scan must refuse the rename, mentioning full confirmation: {result}"
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

/// M4 (4.4): `textDocument/rename` end-to-end — renaming a `public`
/// top-level type whose file name matches it, across three files (one open,
/// two on disk), when the client advertises
/// `workspace.workspaceEdit.resourceOperations` including `"rename"`:
/// expects text edits in all three files AND a `RenameFile` resource op
/// renaming the declaring file to match.
#[test]
fn rename_public_class_includes_file_rename_when_capability_advertised() {
    let root = temp_root("rename-file-op");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    std::fs::write(
        src_dir.join("UseB.java"),
        "package p;\nclass UseB {\n  Foo f;\n}\n",
    )
    .expect("write UseB.java");
    std::fs::write(
        src_dir.join("UseC.java"),
        "package p;\nclass UseC {\n  Foo f;\n}\n",
    )
    .expect("write UseC.java");
    let foo_path = src_dir.join("Foo.java");
    let foo_text = "package p;\n\npublic class Foo {\n}\n";
    std::fs::write(&foo_path, foo_text).expect("write Foo.java");

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
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{"workspace":{{"workspaceEdit":{{"resourceOperations":["rename"]}}}}}},"workspaceFolders":[{{"uri":"{root_uri}","name":"proj"}}]}}}}"#
    ));
    let _ = read_until(&mut reader, "\"id\":1", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    let foo_uri = format!("file://{}", foo_path.display());
    let use_b_uri = format!("file://{}", src_dir.join("UseB.java").display());
    let use_c_uri = format!("file://{}", src_dir.join("UseC.java").display());
    let bar_uri = format!("file://{}", src_dir.join("Bar.java").display());

    let escaped_foo_text = foo_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{foo_uri}","languageId":"java","version":1,"text":"{escaped_foo_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Foo` in `public class Foo` (line 2, char 13).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{{"textDocument":{{"uri":"{foo_uri}"}},"position":{{"line":2,"character":13}},"newName":"Bar"}}}}"#
    ));
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        !result.contains("\"error\""),
        "rename must succeed: {result}"
    );
    assert!(
        result.contains(&foo_uri) && result.contains(&use_b_uri) && result.contains(&use_c_uri),
        "expected text edits in all three files: {result}"
    );
    assert!(
        result.matches(r#""newText":"Bar""#).count() >= 3,
        "expected at least 3 occurrences renamed to Bar: {result}"
    );
    assert!(
        result.contains("\"kind\":\"rename\""),
        "expected a RenameFile resource op: {result}"
    );
    assert!(
        result.contains(&bar_uri),
        "expected the RenameFile op's newUri to be {bar_uri}: {result}"
    );
    // M4.4 fix round 1: versioned TextDocumentEdits — the open declaring
    // file carries its LSP version (1, from didOpen); the two on-disk-only
    // files carry an explicit null version.
    assert!(
        result.contains("\"version\":1"),
        "expected the open Foo.java edit to carry version 1: {result}"
    );
    assert_eq!(
        result.matches("\"version\":null").count(),
        2,
        "expected exactly the two unopened files' edits to carry a null version: {result}"
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

/// M4 (4.4): `textDocument/rename` end-to-end — the same scenario as
/// [`rename_public_class_includes_file_rename_when_capability_advertised`]
/// but the client does NOT advertise
/// `workspace.workspaceEdit.resourceOperations` including `"rename"`: the
/// text edits must still succeed, but no `RenameFile` op may be present.
#[test]
fn rename_public_class_skips_file_rename_without_capability() {
    let root = temp_root("rename-no-file-op");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    std::fs::write(
        src_dir.join("UseB.java"),
        "package p;\nclass UseB {\n  Foo f;\n}\n",
    )
    .expect("write UseB.java");
    std::fs::write(
        src_dir.join("UseC.java"),
        "package p;\nclass UseC {\n  Foo f;\n}\n",
    )
    .expect("write UseC.java");
    let foo_path = src_dir.join("Foo.java");
    let foo_text = "package p;\n\npublic class Foo {\n}\n";
    std::fs::write(&foo_path, foo_text).expect("write Foo.java");

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

    let foo_uri = format!("file://{}", foo_path.display());
    let use_b_uri = format!("file://{}", src_dir.join("UseB.java").display());
    let use_c_uri = format!("file://{}", src_dir.join("UseC.java").display());

    let escaped_foo_text = foo_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{foo_uri}","languageId":"java","version":1,"text":"{escaped_foo_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `Foo` in `public class Foo` (line 2, char 13).
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{{"textDocument":{{"uri":"{foo_uri}"}},"position":{{"line":2,"character":13}},"newName":"Bar"}}}}"#
    ));
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        !result.contains("\"error\""),
        "rename must succeed: {result}"
    );
    assert!(
        result.contains(&foo_uri) && result.contains(&use_b_uri) && result.contains(&use_c_uri),
        "expected text edits in all three files: {result}"
    );
    assert!(
        !result.contains("\"kind\":\"rename\""),
        "no RenameFile op must be present without the client capability: {result}"
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

/// M5.2: classpath invalidation on build-file change, end to end. A Maven
/// project declares a dependency (`com.example:extlib:1.0`) that isn't yet
/// present in the (fixture, per-test) local `~/.m2/repository`, so a member
/// call on it doesn't resolve (`textDocument/hover` -> `null`). The
/// dependency's real jar (compiled with `javac`/`jar`, not hand-rolled
/// bytecode) then appears in the fixture repo, `pom.xml` is "touched" by
/// driving a `workspace/didChangeWatchedFiles` notification directly over
/// stdio (matching the task brief — no real filesystem watcher is
/// involved), and — after the debounced rebuild (overridden via
/// `initializationOptions.classpathDebounceMs` to a few ms so this test
/// doesn't sleep multiple seconds) swaps in the new classpath and
/// republishes diagnostics — the same hover now resolves it.
///
/// Skips gracefully if `javac`/`jar` aren't on `PATH` (mirrors this suite's
/// JDK-gated skips elsewhere): there's no fixture dependency to build.
#[test]
fn classpath_invalidation_on_build_file_change_round_trip() {
    if Command::new("javac").arg("-version").output().is_err()
        || Command::new("jar").arg("--version").output().is_err()
    {
        return;
    }

    let root = temp_root("classpath-invalidation");
    let src_dir = root.join("src/main/java/p");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    std::fs::write(
        root.join("pom.xml"),
        "<project><groupId>com.example</groupId><artifactId>proj</artifactId>\
         <version>1.0</version><dependencies><dependency><groupId>com.example</groupId>\
         <artifactId>extlib</artifactId><version>1.0</version></dependency>\
         </dependencies></project>",
    )
    .expect("write pom.xml");

    let use_text = "package p;\n\nimport com.example.ExternalDep;\n\npublic class Use {\n  \
                     void m() {\n    ExternalDep e = new ExternalDep();\n    e.hello();\n  }\n}\n";
    std::fs::write(src_dir.join("Use.java"), use_text).expect("write Use.java");

    // A fixture `HOME` for the *child server process only* — `~/.m2` under
    // it is ours to control; the real user's `~/.m2` is never touched.
    let fixture_home = temp_root("classpath-invalidation-home");
    std::fs::create_dir_all(&fixture_home).expect("create fixture HOME");

    // Build the dependency jar in a scratch area — not yet installed into
    // the fixture `~/.m2`, so the first resolution/hover below sees nothing.
    let build_dir = temp_root("classpath-invalidation-build");
    let classes_dir = build_dir.join("classes");
    std::fs::create_dir_all(&classes_dir).expect("create classes dir");
    let pkg_src_dir = build_dir.join("src/com/example");
    std::fs::create_dir_all(&pkg_src_dir).expect("create dep src dir");
    std::fs::write(
        pkg_src_dir.join("ExternalDep.java"),
        "package com.example;\npublic class ExternalDep {\n    public void hello() {}\n}\n",
    )
    .expect("write ExternalDep.java");
    let javac_status = Command::new("javac")
        .arg("-d")
        .arg(&classes_dir)
        .arg(pkg_src_dir.join("ExternalDep.java"))
        .status()
        .expect("run javac");
    assert!(
        javac_status.success(),
        "javac failed to compile the fixture dependency"
    );
    let jar_path = build_dir.join("extlib-1.0.jar");
    let jar_status = Command::new("jar")
        .arg("--create")
        .arg("--file")
        .arg(&jar_path)
        .arg("-C")
        .arg(&classes_dir)
        .arg(".")
        .status()
        .expect("run jar");
    assert!(
        jar_status.success(),
        "jar failed to package the fixture dependency"
    );

    let bin = env!("CARGO_BIN_EXE_jvl-server");
    let mut child: Child = Command::new(bin)
        .env("HOME", &fixture_home)
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
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{}},"workspaceFolders":[{{"uri":"{root_uri}","name":"proj"}}],"initializationOptions":{{"classpathDebounceMs":20}}}}}}"#
    ));
    let _ = read_until(&mut reader, "\"id\":1", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    let use_uri = format!("file://{}", src_dir.join("Use.java").display());
    let escaped_use_text = use_text.replace('\n', "\\n");
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{use_uri}","languageId":"java","version":1,"text":"{escaped_use_text}"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor on `hello` in `e.hello()` (line 7, char 6). The dependency jar
    // isn't in the fixture `~/.m2` yet, so this must resolve to nothing.
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{{"textDocument":{{"uri":"{use_uri}"}},"position":{{"line":7,"character":6}}}}}}"#
    ));
    let before = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        before.contains("\"result\":null"),
        "expected no hover before the dependency exists: {before}"
    );

    // The dependency now appears in the fixture `~/.m2/repository`...
    let artifact_dir = fixture_home.join(".m2/repository/com/example/extlib/1.0");
    std::fs::create_dir_all(&artifact_dir).expect("create m2 artifact dir");
    std::fs::copy(&jar_path, artifact_dir.join("extlib-1.0.jar")).expect("install fixture jar");

    // ...and `pom.xml` is (notionally) touched: drive the notification
    // directly over stdio rather than relying on a real filesystem watcher.
    let pom_uri = format!("file://{}", root.join("pom.xml").display());
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{{"changes":[{{"uri":"{pom_uri}","type":2}}]}}}}"#
    ));

    let _ = read_until(&mut reader, "classpath rebuild: finished", &mut seen);
    // The rebuild republishes diagnostics for every open document too.
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"textDocument/hover","params":{{"textDocument":{{"uri":"{use_uri}"}},"position":{{"line":7,"character":6}}}}}}"#
    ));
    let after = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        after.contains("hello()"),
        "expected the dependency to resolve after the classpath rebuild: {after}"
    );

    send(r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":4", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&fixture_home);
    let _ = std::fs::remove_dir_all(&build_dir);
}

/// M5.2 negative case: a `workspace/didChangeWatchedFiles` notification for
/// a file that isn't one of the watched build files must never trigger a
/// classpath rebuild. Verified via log absence: with the debounce
/// overridden to 20ms, a deliberate (bounded, short) wait comfortably longer
/// than that gives a wrongly-triggered rebuild time to have logged
/// "classpath rebuild: started" before a follow-up request/response pair
/// (used only as a synchronization point) is checked against.
#[test]
fn did_change_watched_files_ignores_unrelated_file() {
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

    send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"initializationOptions":{"classpathDebounceMs":20}}}"#,
    );
    let _ = read_until(&mut reader, "\"id\":1", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    // Not one of the watched build files (a plain `.java` file).
    send(
        r#"{"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{"changes":[{"uri":"file:///Sample.java","type":2}]}}"#,
    );

    // Give a (hypothetically, wrongly triggered) rebuild time to have
    // started and logged, well past the 20ms debounce override.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // An unrelated request/response, used only to pull any pending log
    // messages out of the pipe before the final assertion.
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":"file:///Sample.java"}}}"#,
    );
    let response = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        response.contains("\"result\":null"),
        "Sample.java was never opened: {response}"
    );

    assert!(
        !seen.iter().any(|f| f.contains("classpath rebuild")),
        "an unrelated file's change must never trigger a classpath rebuild: {seen:#?}"
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

/// The JDK's home directory for M5.4's `checkProject` end-to-end test,
/// which needs a *real* `javac` to invoke — unlike the JDK-gated
/// definition/hover round trips above (which only need `jvl-classpath`'s
/// broader `best_jdk()` probing, i.e. jmods on disk), `javac::locate_javac`
/// deliberately only checks `$JAVA_HOME`/an explicit override (per the task
/// brief — never a PATH search), so this test sets `JAVA_HOME` explicitly
/// on the spawned server's environment rather than relying on the ambient
/// one (which may well be unset even where a JDK is otherwise
/// discoverable, e.g. via `/usr/libexec/java_home` on macOS or a `java` on
/// PATH). Still filesystem/OS-tool probing only, never a network fetch.
fn discover_java_home() -> Option<String> {
    if let Ok(existing) = std::env::var("JAVA_HOME") {
        if !existing.is_empty() {
            return Some(existing);
        }
    }
    if let Ok(output) = Command::new("/usr/libexec/java_home").output() {
        if output.status.success() {
            let home = String::from_utf8(output.stdout).ok()?.trim().to_string();
            if !home.is_empty() {
                return Some(home);
            }
        }
    }
    None
}

/// M5 (5.4): the one-shot `javac` check command, end to end.
/// Workspace-Trust gating lives entirely in `editors/vscode` (out of scope
/// for a server-only test — the server has no notion of it and just does
/// what it's told); this drives `workspace/executeCommand` directly, as the
/// extension would after confirming trust. Covers: a broken and a good file
/// on disk (neither ever opened) → `checkProject` → a `javac`-sourced
/// diagnostic published against the broken file's URI and *no*
/// `publishDiagnostics` at all for the good one; opening the broken file
/// still shows its stale `javac` diagnostic (merged in, not clobbered); and
/// editing it clears that diagnostic immediately (stale after edit), well
/// before any second `checkProject` run. Skips gracefully (like the JDK
/// round trips above) if no JDK is discoverable in this environment.
#[test]
fn check_project_javac_round_trip() {
    let Some(java_home) = discover_java_home() else {
        eprintln!("skipping check_project_javac_round_trip: no JDK discoverable");
        return;
    };

    let root = temp_root("javac-check");
    let src_dir = root.join("src/main/java/demo");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    let good_path = src_dir.join("Good.java");
    std::fs::write(
        &good_path,
        "package demo;\n\npublic class Good {\n    void ok() {\n        System.out.println(\"fine\");\n    }\n}\n",
    )
    .expect("write Good.java");
    let broken_path = src_dir.join("Broken.java");
    let broken_text = "package demo;\n\npublic class Broken {\n    void m() {\n        int x = \"hello\";\n    }\n}\n";
    std::fs::write(&broken_path, broken_text).expect("write Broken.java");

    let bin = env!("CARGO_BIN_EXE_jvl-server");
    let mut child: Child = Command::new(bin)
        .env("JAVA_HOME", &java_home)
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
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    assert!(
        init.contains("\"executeCommandProvider\"") && init.contains("jvl.checkProject.run"),
        "missing executeCommandProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    let good_uri = format!("file://{}", good_path.display());
    let broken_uri = format!("file://{}", broken_path.display());

    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"workspace/executeCommand","params":{"command":"jvl.checkProject.run","arguments":[]}}"#,
    );
    // The handler's diagnostics publish and its own response are written by
    // independent tower-lsp paths, so their wire order is NOT guaranteed
    // (the same race the references truncation-notice test hit). Read the
    // response first, then keep reading order-tolerantly until the publish
    // for Broken.java has also arrived.
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        result.contains("\"status\":\"ok\""),
        "expected checkProject to run successfully: {result}"
    );
    assert!(
        result.contains("\"errorCount\":1"),
        "expected exactly one javac error (Broken.java): {result}"
    );

    if !seen
        .iter()
        .any(|f| f.contains(&broken_uri) && f.contains("\"source\":\"javac\""))
    {
        let _ = read_until(&mut reader, "\"source\":\"javac\"", &mut seen);
    }
    assert!(
        seen.iter()
            .any(|f| f.contains(&broken_uri) && f.contains("\"source\":\"javac\"")),
        "expected a javac-sourced diagnostic published for Broken.java: {seen:#?}"
    );
    assert!(
        !seen.iter().any(|f| f.contains(&good_uri)),
        "Good.java compiled clean — it must never receive a publishDiagnostics notification: {seen:#?}"
    );

    // Opening the broken file still shows its stale javac diagnostic —
    // merged with (here, zero) syntax diagnostics, not clobbered.
    let escaped_broken_text = json_escape(broken_text);
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{broken_uri}","languageId":"java","version":1,"text":"{escaped_broken_text}"}}}}}}"#
    ));
    let opened_diags = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        opened_diags.contains("\"source\":\"javac\""),
        "opening the file should still show its stale javac diagnostic: {opened_diags}"
    );

    // Editing it clears the javac diagnostic immediately (stale after
    // edit), before any second checkProject run.
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{broken_uri}","version":2}},"contentChanges":[{{"text":"{escaped_broken_text}// edited\n"}}]}}}}"#
    ));
    let after_edit = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        !after_edit.contains("\"source\":\"javac\""),
        "javac diagnostic must clear on didChange, before any new checkProject run: {after_edit}"
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

/// M6.2: `jvl/missingDependencies` reports the current classpath's degraded
/// coordinates split into `missing` (a real `g:a:v` absent from the fixture
/// `~/.m2` — the extension's download candidate) and `skipped` (a
/// classifier variant resolution deliberately won't pursue, surfaced with a
/// reason so the UI can explain the gap instead of silently dropping it).
#[test]
fn missing_dependencies_reports_fetchable_and_skipped_coordinates() {
    let root = temp_root("missing-deps");
    std::fs::create_dir_all(&root).expect("create temp project dir");
    std::fs::write(
        root.join("pom.xml"),
        "<project><groupId>com.example</groupId><artifactId>proj</artifactId>\
         <version>1.0</version><dependencies>\
         <dependency><groupId>com.example</groupId><artifactId>extlib</artifactId>\
         <version>1.0</version></dependency>\
         <dependency><groupId>com.example</groupId><artifactId>natives</artifactId>\
         <version>2.0</version><classifier>natives-linux</classifier></dependency>\
         </dependencies></project>",
    )
    .expect("write pom.xml");

    // A fixture `HOME` for the *child server process only*, with an empty
    // `~/.m2` — neither dependency is present.
    let fixture_home = temp_root("missing-deps-home");
    std::fs::create_dir_all(&fixture_home).expect("create fixture HOME");

    let bin = env!("CARGO_BIN_EXE_jvl-server");
    let mut child: Child = Command::new(bin)
        .env("HOME", &fixture_home)
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

    // No `params` field at all — `jvl/missingDependencies` takes none (see
    // `tower_lsp_server`'s `FromParams for ()`, which only accepts an
    // absent/`null` params value).
    send(r#"{"jsonrpc":"2.0","id":2,"method":"jvl/missingDependencies"}"#);
    let result = read_until(&mut reader, "\"id\":2", &mut seen);
    let json: Value = serde_json::from_str(&result).expect("parse missingDependencies response");
    let missing = json["result"]["missing"].as_array().expect("missing array");
    assert!(
        missing.iter().any(|m| m["group"] == "com.example"
            && m["artifact"] == "extlib"
            && m["version"] == "1.0"),
        "expected extlib to be reported as a fetchable missing dependency: {result}"
    );
    let skipped = json["result"]["skipped"].as_array().expect("skipped array");
    assert!(
        skipped.iter().any(|s| s["group"] == "com.example"
            && s["artifact"] == "natives"
            && s["reason"]
                .as_str()
                .is_some_and(|r| r.contains("classifier natives-linux unsupported"))),
        "expected the classifier variant to be reported as skipped with a reason: {result}"
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
    let _ = std::fs::remove_dir_all(&fixture_home);
}

/// M6.2: the fixed-point rebuild loop's server-side half — after the
/// extension installs a consented-to dependency into `~/.m2` and calls
/// `jvl.classpath.rebuild`, a follow-up `jvl/missingDependencies` query must
/// no longer report it. Uses dummy (non-`javac`-built) jar/pom bytes, same as
/// the `resolve.rs`/`gradle.rs` unit tests — this test is about the
/// rebuild/re-query wiring, not the jar's actual bytecode, so it never needs
/// a real JDK and always runs.
#[test]
fn rebuild_classpath_command_shrinks_missing_dependencies_list() {
    let root = temp_root("rebuild-missing-deps");
    std::fs::create_dir_all(&root).expect("create temp project dir");
    std::fs::write(
        root.join("pom.xml"),
        "<project><groupId>com.example</groupId><artifactId>proj</artifactId>\
         <version>1.0</version><dependencies>\
         <dependency><groupId>com.example</groupId><artifactId>extlib</artifactId>\
         <version>1.0</version></dependency>\
         </dependencies></project>",
    )
    .expect("write pom.xml");

    let fixture_home = temp_root("rebuild-missing-deps-home");
    std::fs::create_dir_all(&fixture_home).expect("create fixture HOME");

    let bin = env!("CARGO_BIN_EXE_jvl-server");
    let mut child: Child = Command::new(bin)
        .env("HOME", &fixture_home)
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
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    assert!(
        init.contains("jvl.classpath.rebuild"),
        "missing executeCommandProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    send(r#"{"jsonrpc":"2.0","id":2,"method":"jvl/missingDependencies"}"#);
    let before = read_until(&mut reader, "\"id\":2", &mut seen);
    let before_json: Value = serde_json::from_str(&before).expect("parse response");
    assert!(
        before_json["result"]["missing"]
            .as_array()
            .expect("missing array")
            .iter()
            .any(|m| m["artifact"] == "extlib"),
        "expected extlib to be missing before install: {before}"
    );

    // Simulate what `mavenFetch.ts` does after a consented download:
    // install the pom + jar directly into the fixture `~/.m2/repository`,
    // laid out exactly as the Maven backend expects.
    let artifact_dir = fixture_home.join(".m2/repository/com/example/extlib/1.0");
    std::fs::create_dir_all(&artifact_dir).expect("create m2 artifact dir");
    std::fs::write(artifact_dir.join("extlib-1.0.jar"), b"jar").expect("install fixture jar");
    std::fs::write(
        artifact_dir.join("extlib-1.0.pom"),
        "<project><groupId>com.example</groupId><artifactId>extlib</artifactId>\
         <version>1.0</version></project>",
    )
    .expect("install fixture pom");

    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"workspace/executeCommand","params":{"command":"jvl.classpath.rebuild","arguments":[]}}"#,
    );
    let rebuild = read_until(&mut reader, "\"id\":3", &mut seen);
    assert!(
        rebuild.contains("\"status\":\"ok\""),
        "expected the rebuild command to report ok: {rebuild}"
    );

    send(r#"{"jsonrpc":"2.0","id":4,"method":"jvl/missingDependencies"}"#);
    let after = read_until(&mut reader, "\"id\":4", &mut seen);
    let after_json: Value = serde_json::from_str(&after).expect("parse response");
    assert!(
        !after_json["result"]["missing"]
            .as_array()
            .expect("missing array")
            .iter()
            .any(|m| m["artifact"] == "extlib"),
        "expected extlib to no longer be missing after rebuild: {after}"
    );

    send(r#"{"jsonrpc":"2.0","id":5,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":5", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&fixture_home);
}

/// The VS Code extension registers its user-facing commands itself (with a
/// Workspace-Trust gate), and `vscode-languageclient` ALSO auto-registers a
/// VS Code command for every ID the server advertises in
/// `executeCommandProvider` — so a server-advertised ID that matches an
/// extension-contributed ID throws `command '<id>' already exists` during
/// `initializeFeatures` and kills client startup ("Server initialization
/// failed" in the editor). Raw-LSP tests can't see the VS Code command
/// registry, so this guards the invariant statically: the two ID sets must
/// be disjoint.
#[test]
fn server_commands_do_not_collide_with_extension_commands() {
    // The extension's contributed (user-facing) command IDs.
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../editors/vscode/package.json"),
        )
        .expect("read editors/vscode/package.json"),
    )
    .expect("parse editors/vscode/package.json");
    let contributed: Vec<String> = manifest["contributes"]["commands"]
        .as_array()
        .expect("contributes.commands array")
        .iter()
        .map(|c| c["command"].as_str().expect("command id").to_string())
        .collect();
    assert!(
        !contributed.is_empty(),
        "expected at least one contributed command"
    );

    // The server's advertised executeCommand IDs, from a real initialize.
    let bin = env!("CARGO_BIN_EXE_jvl-server");
    let mut child: Child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn jvl-server");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    stdin
        .write_all(
            frame(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#)
                .as_bytes(),
        )
        .expect("write initialize");
    let mut seen: Vec<String> = Vec::new();
    let init = read_until(&mut reader, "\"id\":1", &mut seen);
    let init_json: serde_json::Value =
        serde_json::from_str(&init).expect("parse initialize response");
    let advertised: Vec<String> = init_json["result"]["capabilities"]["executeCommandProvider"]
        ["commands"]
        .as_array()
        .expect("executeCommandProvider.commands")
        .iter()
        .map(|c| c.as_str().expect("command id").to_string())
        .collect();
    assert!(
        !advertised.is_empty(),
        "expected at least one server-advertised command"
    );

    for id in &advertised {
        assert!(
            !contributed.contains(id),
            "server-advertised executeCommand id {id:?} collides with an \
             extension-contributed command — vscode-languageclient would fail \
             initializeFeatures with `command '{id}' already exists`"
        );
    }

    stdin
        .write_all(frame(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#).as_bytes())
        .expect("write shutdown");
    let _ = read_until(&mut reader, "\"id\":2", &mut seen);
    stdin
        .write_all(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#).as_bytes())
        .expect("write exit");
    drop(stdin);
    let _ = child.wait();
}

/// M6 (6.1): structural Java-rule diagnostics end to end, rule (a) — a lone
/// file (no workspace) at `file:///Foo.java` declaring `public class Bar`
/// gets the javac-worded "should be declared in a file named" error; this is
/// the user-reported motivating case (`MavenDemo2.java` / `class
/// MavenDemo3`) reproduced directly.
#[test]
fn structural_filename_mismatch_round_trip() {
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
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///Foo.java","languageId":"java","version":1,"text":"public class Bar {\n}\n"}}}"#,
    );
    let diag = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        diag.contains("class Bar is public, should be declared in a file named Bar.java"),
        "expected the javac-worded filename-mismatch error: {diag}"
    );
    assert!(diag.contains("file:///Foo.java"), "wrong uri: {diag}");
    assert!(
        diag.contains("\"severity\":1"),
        "expected ERROR severity: {diag}"
    );

    send(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":2", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}

/// M6 (6.1): structural Java-rule diagnostics end to end, rule (c) — a
/// workspace file under `src/main/java/com/x/` declaring `package com.y;`
/// gets a package/directory mismatch error; the same file with a matching
/// `package com.x;` is clean.
#[test]
fn structural_package_mismatch_then_clean_round_trip() {
    let root = temp_root("structural-package");
    let src_dir = root.join("src/main/java/com/x");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");

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

    let file_uri = format!("file://{}", src_dir.join("Mismatch.java").display());

    // Declared package (`com.y`) doesn't match the directory (`com/x`).
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{file_uri}","languageId":"java","version":1,"text":"package com.y;\nclass Mismatch {{}}\n"}}}}}}"#
    ));
    let diag = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        diag.contains(
            "The declared package \\\"com.y\\\" does not match the expected package \\\"com.x\\\""
        ),
        "expected a package/directory mismatch error: {diag}"
    );
    assert!(
        diag.contains("\"severity\":1"),
        "expected ERROR severity: {diag}"
    );

    // Fix the package to match the directory -> diagnostics clear.
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{file_uri}","version":2}},"contentChanges":[{{"text":"package com.x;\nclass Mismatch {{}}\n"}}]}}}}"#
    ));
    let cleared = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        cleared.contains("\"diagnostics\":[]"),
        "diagnostics should clear once the package matches the directory: {cleared}"
    );

    send(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":2", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// M6 (6.1) fix round 1: the package-vs-directory diagnostic must use only
/// the fixed conventional source roots — never roots *inferred from other
/// open documents* (`infer_source_root`), which are fine for best-effort
/// navigation but would make what error a file gets depend on which
/// unrelated sibling files happen to be open. Scenario: doc A at
/// `root/lib/a/b/A.java` declaring `package a.b;` makes `root/lib` an
/// inferred source root; doc B at `root/lib/c/B.java` declaring an
/// unrelated `package x.y;` would then have "expected package c" under the
/// old behavior and get a false mismatch error. It must stay silent.
#[test]
fn structural_package_rule_ignores_open_doc_inferred_roots() {
    let root = temp_root("structural-inferred-root");
    let a_dir = root.join("lib/a/b");
    let b_dir = root.join("lib/c");
    std::fs::create_dir_all(&a_dir).expect("create temp project dirs");
    std::fs::create_dir_all(&b_dir).expect("create temp project dirs");

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

    // Doc A: its path (`lib/a/b/A.java`) matches its package (`a.b`), so
    // `infer_source_root` derives `root/lib` from it once it's open.
    let a_uri = format!("file://{}", a_dir.join("A.java").display());
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{a_uri}","languageId":"java","version":1,"text":"package a.b;\nclass A {{}}\n"}}}}}}"#
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Doc B: under `root/lib/c/` with an unrelated package. The inferred
    // `root/lib` root "contains" it, but only conventional roots may drive
    // the diagnostic — B must publish NO diagnostics at all.
    let b_uri = format!("file://{}", b_dir.join("B.java").display());
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{b_uri}","languageId":"java","version":1,"text":"package x.y;\nclass B {{}}\n"}}}}}}"#
    ));
    let b_diag = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    assert!(
        b_diag.contains(&b_uri),
        "expected B's own diagnostics notification: {b_diag}"
    );
    assert!(
        b_diag.contains("\"diagnostics\":[]"),
        "a file under an open-doc-INFERRED root (not a conventional one) must stay \
         silent — no package-mismatch diagnostic: {b_diag}"
    );

    send(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":2", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// M6 (6.3): `textDocument/completion` never fetches Javadoc — items come
/// back with a `data` payload but no `documentation` — and
/// `completionItem/resolve` is what actually fetches it, on demand, for an
/// in-project member. No JDK needed (in-project resolution only), so this
/// always runs.
#[test]
fn completion_resolve_lazy_documentation_round_trip() {
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
        init.contains("\"resolveProvider\":true"),
        "missing completionProvider.resolveProvider capability: {init}"
    );
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///CompDoc.java","languageId":"java","version":1,"text":"class Box {\n  /** Adds two numbers. */\n  int add(int a, int b) { return a + b; }\n  void m() { this.a }\n}\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor right after `this.a` (line 3, char 19).
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///CompDoc.java"},"position":{"line":3,"character":19}}}"#,
    );
    let completion = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        !completion.contains("Adds two numbers."),
        "completion must not eagerly fetch Javadoc: {completion}"
    );

    let completion_json: Value =
        serde_json::from_str(&completion).expect("parse completion response");
    let items = completion_json["result"]["items"]
        .as_array()
        .expect("completion result items");
    let add_item = items
        .iter()
        .find(|i| i["label"] == "add")
        .expect("add item present")
        .clone();
    assert!(
        add_item.get("documentation").is_none(),
        "no eager documentation: {add_item:?}"
    );
    assert!(
        add_item.get("data").is_some(),
        "expected a lazy-resolve data payload: {add_item:?}"
    );

    let resolve_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "completionItem/resolve",
        "params": add_item,
    });
    send(&resolve_request.to_string());
    let resolved = read_until(&mut reader, "\"id\":3", &mut seen);
    let resolved_json: Value = serde_json::from_str(&resolved).expect("parse resolve response");
    let doc_value = resolved_json["result"]["documentation"]["value"]
        .as_str()
        .expect("documentation.value string");
    assert_eq!(doc_value, "Adds two numbers.");

    send(r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":4", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}

/// M6 (6.3) fix round 1: with TWO open documents declaring the same type
/// simple name AND the same member name (different Javadoc), resolve returns
/// the doc from the ORIGINATING document — the item's `data.uri`, stamped at
/// completion time — never whichever same-named type an unordered all-docs
/// scan happens to find first. And once the originating document is closed
/// (stale URI), resolve returns the item with no documentation, without
/// erroring.
#[test]
fn completion_resolve_same_named_types_uses_originating_document() {
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

    // The decoy first: same `class Box`, same `int width`, different Javadoc.
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///DocB.java","languageId":"java","version":1,"text":"class Box { /** From B. */ int width; }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///DocA.java","languageId":"java","version":1,"text":"class Box {\n  /** From A. */\n  int width;\n  void m() { this.w }\n}\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Completion in DocA, cursor right after `this.w` (line 3, char 19).
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///DocA.java"},"position":{"line":3,"character":19}}}"#,
    );
    let completion = read_until(&mut reader, "\"id\":2", &mut seen);
    let completion_json: Value =
        serde_json::from_str(&completion).expect("parse completion response");
    let width_item = completion_json["result"]["items"]
        .as_array()
        .expect("completion result items")
        .iter()
        .find(|i| i["label"] == "width")
        .expect("width item present")
        .clone();
    assert_eq!(
        width_item["data"]["uri"], "file:///DocA.java",
        "data must name the originating document: {width_item:?}"
    );

    let resolve_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "completionItem/resolve",
        "params": width_item,
    });
    send(&resolve_request.to_string());
    let resolved = read_until(&mut reader, "\"id\":3", &mut seen);
    let resolved_json: Value = serde_json::from_str(&resolved).expect("parse resolve response");
    assert_eq!(
        resolved_json["result"]["documentation"]["value"]
            .as_str()
            .expect("documentation.value string"),
        "From A.",
        "must be the ORIGINATING document's Javadoc, not the decoy's: {resolved_json:?}"
    );

    // Stale URI: close the originating document, resolve the same item again
    // — no documentation, no error.
    send(
        r#"{"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":"file:///DocA.java"}}}"#,
    );
    let stale_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "completionItem/resolve",
        "params": resolve_request["params"],
    });
    send(&stale_request.to_string());
    let stale = read_until(&mut reader, "\"id\":4", &mut seen);
    let stale_json: Value = serde_json::from_str(&stale).expect("parse stale resolve response");
    assert!(
        stale_json.get("error").is_none(),
        "stale resolve must not error: {stale_json:?}"
    );
    assert!(
        stale_json["result"].get("documentation").is_none(),
        "stale URI must yield no documentation, never a guess: {stale_json:?}"
    );

    send(r#"{"jsonrpc":"2.0","id":5,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":5", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");
}

/// M6 (6.3): the same lazy-resolve round trip for an *external* member —
/// `String.length`'s Javadoc, recovered from the JDK's `src.zip` only when
/// `completionItem/resolve` is actually invoked. Skips gracefully (like the
/// other JDK-gated round trips in this file) if no JDK is discoverable —
/// signaled here by the completion request returning no items at all (no
/// classpath means `resolve_type_node`'s external branch never resolves).
#[test]
fn completion_resolve_external_member_jdk_round_trip() {
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
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///CompExt.java","languageId":"java","version":1,"text":"class C { void m() { String s; s.le } }\n"}}}"#,
    );
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // Cursor right after `s.le` (line 0, char 35).
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///CompExt.java"},"position":{"line":0,"character":35}}}"#,
    );
    let completion = read_until(&mut reader, "\"id\":2", &mut seen);

    if completion.contains("\"result\":null") {
        // No JDK discoverable in this environment — nothing further to check.
    } else {
        let completion_json: Value =
            serde_json::from_str(&completion).expect("parse completion response");
        let items = completion_json["result"]["items"]
            .as_array()
            .expect("completion result items");
        let length_item = items
            .iter()
            .find(|i| i["label"] == "length")
            .expect("length item present")
            .clone();
        assert!(
            length_item.get("documentation").is_none(),
            "no eager documentation: {length_item:?}"
        );
        assert!(
            length_item.get("data").is_some(),
            "expected a lazy-resolve data payload: {length_item:?}"
        );

        let resolve_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "completionItem/resolve",
            "params": length_item,
        });
        send(&resolve_request.to_string());
        let resolved = read_until(&mut reader, "\"id\":3", &mut seen);
        let resolved_json: Value = serde_json::from_str(&resolved).expect("parse resolve response");
        assert!(
            resolved_json["result"]["documentation"].is_object()
                || resolved_json["result"]["documentation"].is_string(),
            "expected documentation from src.zip: {resolved_json:?}"
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

/// M7: project-source symbol layer end-to-end — a workspace type the user
/// never opens (`Person.java`) still drives member completion, classpath-
/// style type-name completion, and lazy Javadoc, identically to a compiled
/// dependency. Only `Main.java` is opened.
#[test]
fn project_source_symbols_closed_file_round_trip() {
    let root = temp_root("project-symbols");
    let src_dir = root.join("src/main/java/demo");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    std::fs::write(
        src_dir.join("Person.java"),
        "package demo;\n\n\
         /** Represents a person. */\n\
         public class Person {\n\
         \u{20}   /** Returns the person's name. */\n\
         \u{20}   public String getName() { return null; }\n\
         }\n",
    )
    .expect("write Person.java");

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
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{"textDocument":{{"completion":{{"completionItem":{{"snippetSupport":true}}}}}}}},"workspaceFolders":[{{"uri":"{root_uri}","name":"proj"}}]}}}}"#
    ));
    let _ = read_until(&mut reader, "\"id\":1", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);

    // `Person` used (same package, no import needed), but its own file is
    // never opened. Kept on a single line so a byte offset from `.find`
    // doubles as the `character` column (`line` stays 0).
    let main_uri = format!(
        "file://{}",
        root.join("src/main/java/demo/Main.java").display()
    );
    let main_text = "package demo; class Main { void m() { Person p = new Person(\"x\"); p.getName(); Per } }\n";
    assert_eq!(
        main_text.find('\n'),
        Some(main_text.len() - 1),
        "must stay single-line for the byte-offset-as-column math below"
    );
    send(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{main_uri}","languageId":"java","version":1,"text":"{}"}}}}}}"#,
        json_escape(main_text)
    ));
    let _ = read_until(&mut reader, "textDocument/publishDiagnostics", &mut seen);

    // A: member completion on the closed type — `p.` yields `getName`.
    let dot_after_p = main_text.find("p.getName").unwrap() + 2;
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/completion","params":{{"textDocument":{{"uri":"{main_uri}"}},"position":{{"line":0,"character":{dot_after_p}}}}}}}"#
    ));
    let member_completion = read_until(&mut reader, "\"id\":2", &mut seen);
    assert!(
        member_completion.contains(r#""label":"getName""#),
        "expected getName from the closed Person.java: {member_completion}"
    );

    // B: classpath-style type-name completion for the closed type itself.
    let after_per = main_text.rfind("Per").unwrap() + 3;
    send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"textDocument/completion","params":{{"textDocument":{{"uri":"{main_uri}"}},"position":{{"line":0,"character":{after_per}}}}}}}"#
    ));
    let type_completion = read_until(&mut reader, "\"id\":3", &mut seen);
    let type_json: Value = serde_json::from_str(&type_completion).expect("parse completion");
    let items = type_json["result"]["items"]
        .as_array()
        .expect("completion result items");
    let person_item = items
        .iter()
        .find(|i| i["label"] == "Person")
        .expect("Person type item present")
        .clone();
    assert_eq!(
        person_item["additionalTextEdits"],
        Value::Null,
        "same package, no import needed: {person_item:?}"
    );

    // C: lazy Javadoc for the closed type resolves via completionItem/resolve.
    let resolve_type = serde_json::json!({
        "jsonrpc": "2.0", "id": 4, "method": "completionItem/resolve", "params": person_item,
    });
    send(&resolve_type.to_string());
    let resolved_type = read_until(&mut reader, "\"id\":4", &mut seen);
    assert!(
        resolved_type.contains("Represents a person."),
        "expected the closed file's own type Javadoc: {resolved_type}"
    );

    // D: lazy Javadoc for a member of the closed type.
    let member_completion_json: Value =
        serde_json::from_str(&member_completion).expect("parse member completion");
    let get_name_item = member_completion_json["result"]["items"]
        .as_array()
        .expect("member completion items")
        .iter()
        .find(|i| i["label"] == "getName")
        .expect("getName item present")
        .clone();
    let resolve_member = serde_json::json!({
        "jsonrpc": "2.0", "id": 5, "method": "completionItem/resolve", "params": get_name_item,
    });
    send(&resolve_member.to_string());
    let resolved_member = read_until(&mut reader, "\"id\":5", &mut seen);
    assert!(
        resolved_member.contains("Returns the person's name."),
        "expected the closed file's own member Javadoc: {resolved_member}"
    );

    send(r#"{"jsonrpc":"2.0","id":6,"method":"shutdown"}"#);
    let _ = read_until(&mut reader, "\"id\":6", &mut seen);
    send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    drop(stdin);
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    let status = child.wait().expect("wait for server exit");
    assert!(status.success(), "server exited with failure: {status:?}");

    let _ = std::fs::remove_dir_all(&root);
}
