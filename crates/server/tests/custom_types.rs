//! End-to-end proof that the pure-Rust tier checks the user's own classes
//! across open and unopened project files with no JDK, and that provider
//! changes (edit, close, on-disk create/change/delete) re-check open
//! consumers.
//!
//! Separate test binary (tests cannot import each other): `frame`,
//! `json_escape`, `read_frame`, `read_until` are copied from
//! `crates/server/tests/lifecycle.rs`.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use serde_json::Value;

/// Frame a JSON-RPC payload with LSP `Content-Length` headers.
fn frame(payload: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload)
}

/// Escape Java source for embedding inside a hand-written JSON string.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Read one `Content-Length`-framed message body, or `None` on EOF.
fn read_frame(reader: &mut BufReader<ChildStdout>) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
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

/// Read frames until one contains `needle`, collecting everything seen.
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

/// One file per top-level type, package `demo`; only `Object` ancestry is
/// used, so no JDK is required. `Client.java`'s `L<N>` comments mark the
/// lines the native tier must flag.
fn corpus_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Animal.java", "package demo;\npublic class Animal { }\n"),
        ("Dog.java", "package demo;\npublic class Dog extends Animal { }\n"),
        ("Cat.java", "package demo;\npublic class Cat extends Animal { }\n"),
        ("Named.java", "package demo;\npublic interface Named { }\n"),
        ("Order.java", "package demo;\npublic class Order { }\n"),
        (
            "User.java",
            "package demo;\npublic class User implements Named { public User(User copy) { } public User() { } public User pick(User u) { return u; } public Order pick(Order o) { return o; } }\n",
        ),
        (
            "Box.java",
            "package demo;\npublic class Box<T> { public Box(T v) { } public T get() { return null; } }\n",
        ),
        (
            "UserBox.java",
            "package demo;\npublic class UserBox extends Box<User> { public UserBox() { super(new User()); } }\n",
        ),
        ("Shape.java", "package demo;\npublic abstract class Shape { }\n"),
        (
            "Client.java",
            "package demo;\npublic class Client {\n    User u1 = new Order();                       // L1 incompatibleAssignment\n    Animal a = new Dog();                        // ok\n    Dog d = new Animal();                        // L3 incompatibleAssignment\n    Box<User> b1 = new Box<Order>(new Order());  // L4 incompatibleAssignment\n    User u2 = new UserBox().get();               // ok\n    Order o1 = new UserBox().get();              // L6 incompatibleAssignment\n    Object s = new Shape();                      // L7 invalidInstantiation (abstract)\n    User u3 = new User(new Order());             // L8 invalidInstantiation (no applicable)\n    Order o2 = new User().pick(new User());      // L9 incompatibleAssignment (selected User)\n    User make() { return new Order(); }          // L10 incompatibleReturn\n    void re() { User u = new User(); u = new Order(); }  // L11 incompatibleAssignment\n}\n",
        ),
    ]
}

fn corpus_text(name: &str) -> &'static str {
    corpus_files()
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("{name} in corpus"))
        .1
}

/// A fresh temp project root; `with_corpus` writes the corpus files under
/// `src/main/java/demo/`.
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_root(with_corpus: bool) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "jvl-custom-{}-{}-{}",
        std::process::id(),
        seq,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let src_dir = root.join("src/main/java/demo");
    std::fs::create_dir_all(&src_dir).expect("create temp project dirs");
    if with_corpus {
        for (name, contents) in corpus_files() {
            std::fs::write(src_dir.join(name), contents).expect("write corpus file");
        }
    }
    root
}

/// A fake JDK home whose `jmods/java.base.jmod` is a valid-but-empty jmod
/// (4-byte `JM\0\0` header + an empty ZIP end-of-central-directory record).
/// `best_jdk` accepts it (the file exists), so the classpath is built from it
/// and contains no classes at all — a hermetic "no standard library" server
/// regardless of what the host machine has installed.
fn empty_jdk_home(root: &Path) -> PathBuf {
    let home = root.join("emptyjdk");
    let jmods = home.join("jmods");
    std::fs::create_dir_all(&jmods).expect("create jmods");
    let mut bytes = vec![b'J', b'M', 0, 0];
    // Empty ZIP: EOCD only.
    bytes.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
    bytes.extend_from_slice(&[0; 18]);
    std::fs::write(jmods.join("java.base.jmod"), bytes).expect("write empty jmod");
    std::fs::write(home.join("release"), "JAVA_VERSION=\"21\"\n").expect("write release");
    home
}

fn temp_project() -> PathBuf {
    temp_root(true)
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

struct ServerHandle {
    child: Child,
    reader: BufReader<ChildStdout>,
    seen: Vec<String>,
}

impl ServerHandle {
    fn spawn(java_home: &Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_jvl-server");
        let mut child: Child = Command::new(bin)
            // Hermetic: the corpus must be proven with an empty standard library.
            .env("JAVA_HOME", java_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn jvl-server");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        ServerHandle {
            child,
            reader,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, msg: &str) {
        self.child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(frame(msg).as_bytes())
            .expect("write to server");
    }

    fn read_until(&mut self, needle: &str) -> String {
        read_until(&mut self.reader, needle, &mut self.seen)
    }

    /// `initialize` + `initialized` with `root` as the workspace folder and
    /// the given client `capabilities` JSON object.
    fn init(&mut self, root: &Path, capabilities: &str) {
        let root_uri = file_uri(root);
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{capabilities},"workspaceFolders":[{{"uri":"{root_uri}","name":"proj"}}]}}}}"#
        ));
        let _ = self.read_until("\"id\":1");
        self.send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
    }

    fn did_open(&mut self, uri: &str, text: &str) {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","languageId":"java","version":1,"text":"{}"}}}}}}"#,
            json_escape(text)
        ));
    }

    fn did_change(&mut self, uri: &str, version: i32, text: &str) {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":{version}}},"contentChanges":[{{"text":"{}"}}]}}}}"#,
            json_escape(text)
        ));
    }

    fn did_close(&mut self, uri: &str) {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
        ));
    }

    /// `workspace/didChangeWatchedFiles` with one event: 1 = Created,
    /// 2 = Changed, 3 = Deleted.
    fn watched(&mut self, uri: &str, change_type: u8) {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{{"changes":[{{"uri":"{uri}","type":{change_type}}}]}}}}"#
        ));
    }

    /// Position in `seen` to pass as `since` for the next action.
    fn mark(&self) -> usize {
        self.seen.len()
    }

    /// Diagnostics of the newest `publishDiagnostics` for `uri` that arrived
    /// after `since` and satisfies `ok`, polling (via a request/response sync
    /// point) for up to ~60s: the first publish waits on a JDK classpath
    /// build, which is slow when many test servers start at once. Returns
    /// the newest publish regardless on timeout so the assertion shows it.
    fn publish_after(
        &mut self,
        uri: &str,
        since: usize,
        sync_base: u32,
        ok: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        let mut newest: Option<Vec<Value>> = None;
        for attempt in 0..240u32 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let sync_id = sync_base * 1000 + attempt;
            self.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{sync_id},"method":"textDocument/documentSymbol","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
            ));
            let _ = self.read_until(&format!("\"id\":{sync_id}"));
            if let Some(p) = last_publish_for(&self.seen[since..], uri) {
                let diags = p["params"]["diagnostics"]
                    .as_array()
                    .expect("diagnostics array")
                    .clone();
                if ok(&diags) {
                    return diags;
                }
                newest = Some(diags);
            }
        }
        newest.expect("a publishDiagnostics for the uri after the action")
    }

    fn shutdown(mut self) {
        self.send(r#"{"jsonrpc":"2.0","id":9999,"method":"shutdown"}"#);
        let _ = self.read_until("\"id\":9999");
        self.send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
        drop(self.child.stdin.take());
        let mut rest = String::new();
        let _ = self.reader.read_to_string(&mut rest);
        let status = self.child.wait().expect("wait for server exit");
        assert!(status.success(), "server exited with failure: {status:?}");
    }
}

/// The most recent `publishDiagnostics` for `uri` among `frames`.
fn last_publish_for(frames: &[String], uri: &str) -> Option<Value> {
    frames
        .iter()
        .rev()
        .filter_map(|f| serde_json::from_str::<Value>(f).ok())
        .find(|v| v["method"] == "textDocument/publishDiagnostics" && v["params"]["uri"] == uri)
}

/// `(line, code)` pairs of a diagnostics array, sorted by line.
fn line_codes(diagnostics: &[Value]) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = diagnostics
        .iter()
        .map(|d| {
            (
                d["range"]["start"]["line"].as_u64().expect("line"),
                d["code"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

fn has_code(diagnostics: &[Value], code: &str) -> bool {
    diagnostics.iter().any(|d| d["code"] == code)
}

/// Opens only `Client.java`; every provider stays closed on disk. The
/// first publish must carry exactly the corpus's L-marked diagnostics,
/// all from the native tier with no compiler involved.
#[test]
fn closed_custom_types_are_checked_natively() {
    let root = temp_project();
    let mut server = ServerHandle::spawn(&empty_jdk_home(&root));
    server.init(&root, "{}");

    let client_uri = file_uri(&root.join("src/main/java/demo/Client.java"));
    let expected: Vec<(u64, String)> = vec![
        (2, "jvl.incompatibleAssignment"),  // L1
        (4, "jvl.incompatibleAssignment"),  // L3
        (5, "jvl.incompatibleAssignment"),  // L4
        (7, "jvl.incompatibleAssignment"),  // L6
        (8, "jvl.invalidInstantiation"),    // L7 abstract
        (9, "jvl.invalidInstantiation"),    // L8 no applicable constructor
        (10, "jvl.incompatibleAssignment"), // L9 selected overload returns User
        (11, "jvl.incompatibleReturn"),     // L10
        (12, "jvl.incompatibleAssignment"), // L11
    ]
    .into_iter()
    .map(|(l, c)| (l, c.to_string()))
    .collect();
    let mark = server.mark();
    server.did_open(&client_uri, corpus_text("Client.java"));
    let diagnostics = server.publish_after(&client_uri, mark, 40, |d| line_codes(d) == expected);
    let first = format!("{diagnostics:?}");
    assert_eq!(line_codes(&diagnostics), expected, "full payload: {first}");
    assert!(
        diagnostics.iter().all(|d| d["source"] == "java-vsix-lite"),
        "every diagnostic must be native: {first}"
    );
    assert!(
        !server
            .seen
            .iter()
            .any(|f| f.contains("\"source\":\"javac\"")),
        "no compiler diagnostic may appear: {:#?}",
        server.seen
    );

    server.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Editing an open provider re-checks the open consumer at its unchanged
/// version; closing the provider makes the disk copy win again.
#[test]
fn provider_edit_rechecks_open_consumer() {
    let root = temp_project();
    let mut server = ServerHandle::spawn(&empty_jdk_home(&root));
    server.init(&root, "{}");

    let client_uri = file_uri(&root.join("src/main/java/demo/Client.java"));
    let dog_uri = file_uri(&root.join("src/main/java/demo/Dog.java"));
    let client_text = "package demo;\npublic class Client {\n    Animal a = new Dog();\n}\n";
    let mark = server.mark();

    server.did_open(&client_uri, client_text);
    let clean = server.publish_after(&client_uri, mark, 10, |d| d.is_empty());
    assert!(clean.is_empty(), "Dog extends Animal on disk: {clean:?}");
    let mark = server.mark();

    server.did_open(&dog_uri, corpus_text("Dog.java"));
    let still_clean = server.publish_after(&client_uri, mark, 11, |d| d.is_empty());
    assert!(
        still_clean.is_empty(),
        "unchanged Dog buffer: {still_clean:?}"
    );
    let mark = server.mark();

    // Drop `extends Animal` in the open buffer: the consumer must be flagged.
    server.did_change(&dog_uri, 2, "package demo;\npublic class Dog { }\n");
    let broken = server.publish_after(&client_uri, mark, 12, |d| {
        has_code(d, "jvl.incompatibleAssignment")
    });
    assert!(
        has_code(&broken, "jvl.incompatibleAssignment"),
        "consumer must be re-checked after the provider edit: {broken:?}"
    );
    // The consumer itself was never edited, so its version is still 1.
    let last_client = last_publish_for(&server.seen, &client_uri).expect("client publish");
    assert_eq!(last_client["params"]["version"], 1);
    let mark = server.mark();

    // Restore in the buffer: clean again.
    server.did_change(&dog_uri, 3, corpus_text("Dog.java"));
    let fixed = server.publish_after(&client_uri, mark, 13, |d| d.is_empty());
    assert!(fixed.is_empty(), "restored provider: {fixed:?}");
    let mark = server.mark();

    // Break it again, then CLOSE the buffer: the on-disk (correct) copy wins.
    server.did_change(&dog_uri, 4, "package demo;\npublic class Dog { }\n");
    let broken_again = server.publish_after(&client_uri, mark, 14, |d| {
        has_code(d, "jvl.incompatibleAssignment")
    });
    assert!(has_code(&broken_again, "jvl.incompatibleAssignment"));
    let mark = server.mark();
    server.did_close(&dog_uri);
    let disk_wins = server.publish_after(&client_uri, mark, 15, |d| d.is_empty());
    assert!(
        disk_wins.is_empty(),
        "disk version must win after close: {disk_wins:?}"
    );

    server.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// The server watches `**/*.java`; on-disk create/change/delete of a
/// closed provider re-checks the open consumer.
#[test]
fn watched_java_create_change_delete() {
    let root = temp_root(false);
    let src = root.join("src/main/java/demo");
    std::fs::write(src.join("Animal.java"), corpus_text("Animal.java")).expect("write Animal");
    let mut server = ServerHandle::spawn(&empty_jdk_home(&root));
    server.init(
        &root,
        r#"{"workspace":{"didChangeWatchedFiles":{"dynamicRegistration":true}}}"#,
    );

    // Answer the watcher registration and check it covers Java sources.
    let register = server.read_until("client/registerCapability");
    let register: Value = serde_json::from_str(&register).expect("json");
    assert!(
        register.to_string().contains("**/*.java"),
        "watcher must include **/*.java: {register}"
    );
    let id = register["id"].clone();
    server.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#));

    let client_uri = file_uri(&root.join("src/main/java/demo/Client.java"));
    let dog_path = src.join("Dog.java");
    let dog_uri = file_uri(&dog_path);
    let mark = server.mark();
    server.did_open(
        &client_uri,
        "package demo;\npublic class Client {\n    Animal a = new Dog();\n}\n",
    );
    let unknown = server.publish_after(&client_uri, mark, 20, |d| d.is_empty());
    assert!(
        unknown.is_empty(),
        "Dog absent: unknown, never an error: {unknown:?}"
    );

    std::fs::write(&dog_path, "package demo;\npublic class Dog { }\n").expect("create Dog");
    let mark = server.mark();
    server.watched(&dog_uri, 1);
    let created = server.publish_after(&client_uri, mark, 21, |d| {
        has_code(d, "jvl.incompatibleAssignment")
    });
    assert!(
        has_code(&created, "jvl.incompatibleAssignment"),
        "created provider without extends must flag the consumer: {created:?}"
    );

    std::fs::write(&dog_path, corpus_text("Dog.java")).expect("change Dog");
    let mark = server.mark();
    server.watched(&dog_uri, 2);
    let changed = server.publish_after(&client_uri, mark, 22, |d| d.is_empty());
    assert!(
        changed.is_empty(),
        "changed provider now extends Animal: {changed:?}"
    );

    std::fs::remove_file(&dog_path).expect("delete Dog");
    let mark = server.mark();
    server.watched(&dog_uri, 3);
    let deleted = server.publish_after(&client_uri, mark, 23, |d| d.is_empty());
    assert!(
        deleted.is_empty(),
        "deleted provider: unknown again: {deleted:?}"
    );

    server.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Two closed files declaring the same `demo.Dog` in different source
/// roots are ambiguous, so the consumer stays silent.
#[test]
fn duplicate_fqn_is_unknown() {
    let root = temp_root(false);
    let main_src = root.join("src/main/java/demo");
    let test_src = root.join("src/test/java/demo");
    std::fs::create_dir_all(&test_src).expect("test src");
    std::fs::write(main_src.join("Animal.java"), corpus_text("Animal.java")).expect("Animal");
    std::fs::write(
        main_src.join("Dog.java"),
        "package demo;\npublic class Dog { }\n",
    )
    .expect("Dog main");
    std::fs::write(test_src.join("Dog.java"), corpus_text("Dog.java")).expect("Dog test");

    let mut server = ServerHandle::spawn(&empty_jdk_home(&root));
    server.init(&root, "{}");
    let client_uri = file_uri(&root.join("src/main/java/demo/Client.java"));
    let mark = server.mark();
    server.did_open(
        &client_uri,
        "package demo;\npublic class Client {\n    Animal a = new Dog();\n}\n",
    );
    let silent = server.publish_after(&client_uri, mark, 30, |d| d.is_empty());
    assert!(
        silent.is_empty(),
        "duplicate demo.Dog must be unknown: {silent:?}"
    );

    server.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// With a real JDK on the machine, standard-library types are checked from
/// bytecode: `String s = new ArrayList<String>();` is flagged natively and no
/// `javac` diagnostic ever appears. Skips when no JDK is discoverable.
#[test]
fn standard_library_types_are_checked_natively_from_bytecode() {
    let Some(jdk) = jvl_classpath::best_jdk() else {
        eprintln!("skipping: no JDK with jmods found");
        return;
    };
    let root = temp_root(false);
    let mut server = ServerHandle::spawn(&jdk);
    server.init(&root, "{}");
    let uri = file_uri(&root.join("src/main/java/demo/Lib.java"));
    let text = "package demo;\nimport java.util.ArrayList;\nimport java.util.List;\npublic class Lib {\n    String s = new ArrayList<String>();\n    List<String> ok = new ArrayList<>();\n    Integer boxed = 1;\n    String bad = 1;\n}\n";
    let mark = server.mark();
    server.did_open(&uri, text);
    let expected = vec![
        (4, "jvl.incompatibleAssignment".to_string()),
        (7, "jvl.incompatibleAssignment".to_string()),
    ];
    let d = server.publish_after(&uri, mark, 50, |d| line_codes(d) == expected);
    assert_eq!(line_codes(&d), expected, "{d:?}");
    assert!(d.iter().all(|x| x["source"] == "java-vsix-lite"));
    assert!(!server
        .seen
        .iter()
        .any(|f| f.contains("\"source\":\"javac\"")));
    server.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// The same library-typed code with an empty standard library is unknown,
/// never an error: no proof exists without `java.util.ArrayList` metadata.
#[test]
fn standard_library_types_without_jdk_stay_silent() {
    let root = temp_root(false);
    let mut server = ServerHandle::spawn(&empty_jdk_home(&root));
    server.init(&root, "{}");
    let uri = file_uri(&root.join("src/main/java/demo/Lib.java"));
    let text = "package demo;\nimport java.util.ArrayList;\npublic class Lib {\n    String s = new ArrayList<String>();\n}\n";
    let mark = server.mark();
    server.did_open(&uri, text);
    let d = server.publish_after(&uri, mark, 51, |d| {
        !d.iter().any(|x| x["code"] == "jvl.incompatibleAssignment")
    });
    assert!(
        !d.iter().any(|x| x["code"] == "jvl.incompatibleAssignment"),
        "{d:?}"
    );
    server.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}
