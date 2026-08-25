//! End-to-end tests driving the real `jvl-server dap` debug adapter over raw
//! DAP (JSON + `Content-Length` framing on stdio — the identical framing
//! `lifecycle.rs` hand-rolls for LSP), against a real JVM debuggee.
//!
//! Like `lifecycle.rs`'s classpath fixture, these tests run `javac`/`java`
//! from `PATH` unconditionally — a JDK is assumed present in Rust CI.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// Bound on frames read while waiting for one message — a missing message
/// fails fast instead of hanging (raised above lifecycle.rs's 64: a debug
/// session emits output/thread events interleaved with responses).
const MAX_FRAMES: usize = 256;

fn frame(payload: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload)
}

/// Read a single `Content-Length`-framed message, or `None` on EOF.
fn read_frame(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
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
    serde_json::from_slice(&buf).ok()
}

/// The adapter under test plus request plumbing.
struct Dap {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_seq: i64,
    seen: Vec<Value>,
}

impl Dap {
    fn spawn() -> Dap {
        let bin = env!("CARGO_BIN_EXE_jvl-server");
        let mut child = Command::new(bin)
            .arg("dap")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn jvl-server dap");
        let stdin = child.stdin.take().expect("stdin");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        Dap {
            child,
            stdin,
            reader,
            next_seq: 0,
            seen: Vec::new(),
        }
    }

    /// Send one request; returns its `seq`.
    fn send(&mut self, command: &str, arguments: Value) -> i64 {
        self.next_seq += 1;
        let payload = json!({
            "seq": self.next_seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        })
        .to_string();
        self.stdin
            .write_all(frame(&payload).as_bytes())
            .expect("write to adapter");
        self.next_seq
    }

    /// Read frames until `pred` matches, accumulating everything seen.
    fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..MAX_FRAMES {
            let message = read_frame(&mut self.reader)
                .unwrap_or_else(|| panic!("adapter closed stdout while waiting for {what}"));
            self.seen.push(message.clone());
            if pred(&message) {
                return message;
            }
        }
        panic!(
            "did not observe {what} within {MAX_FRAMES} frames; saw:\n{:#?}",
            self.seen
        );
    }

    /// Send a request and read to its (successful) response.
    fn request(&mut self, command: &str, arguments: Value) -> Value {
        let seq = self.send(command, arguments);
        let response = self.until(&format!("response to {command}"), |m| {
            m["type"] == "response" && m["request_seq"] == json!(seq)
        });
        assert_eq!(
            response["success"],
            json!(true),
            "request {command} failed: {response}"
        );
        response
    }

    fn event(&mut self, name: &str) -> Value {
        self.until(&format!("event {name}"), |m| {
            m["type"] == "event" && m["event"] == json!(name)
        })
    }

    fn stopped(&mut self, reason: &str) -> Value {
        self.until(&format!("stopped ({reason})"), |m| {
            m["type"] == "event" && m["event"] == "stopped" && m["body"]["reason"] == json!(reason)
        })
    }

    /// The adapter process must exit by itself shortly after `disconnect`
    /// (VS Code only force-kills after a timeout — a hang here is a bug;
    /// regression guard for the tokio stdin-blocked-shutdown pitfall).
    fn expect_exit(&mut self) {
        for _ in 0..100 {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "adapter exited non-zero: {status:?}");
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => panic!("try_wait failed: {e}"),
            }
        }
        panic!("adapter did not exit within 5s of disconnect");
    }
}

impl Drop for Dap {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn temp_root(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jvl-dap-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

const MAIN_JAVA: &str = "package demo;\n\
\n\
public class Main {\n\
    public static void main(String[] args) {\n\
        int count = 41;\n\
        int bumped = bump(count);\n\
        System.out.println(\"SENTINEL:\" + bumped);\n\
    }\n\
\n\
    static int bump(int value) {\n\
        return value + 1;\n\
    }\n\
}\n";

/// Line (1-based) of the helper call `int bumped = bump(count);`.
const BP_LINE: u32 = 6;

const BOOM_JAVA: &str = "package demo;\n\
\n\
public class Boom {\n\
    public static void main(String[] args) {\n\
        throw new IllegalStateException(\"boom\");\n\
    }\n\
}\n";

/// Write the fixture source under `<root>/src/main/java/demo/` and compile
/// it with `-g` into `<root>/classes`. Returns (source file, classes dir).
fn write_and_compile_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let pkg = root.join("src/main/java/demo");
    std::fs::create_dir_all(&pkg).expect("create fixture package dir");
    let main_java = pkg.join("Main.java");
    std::fs::write(&main_java, MAIN_JAVA).expect("write Main.java");
    std::fs::write(pkg.join("Boom.java"), BOOM_JAVA).expect("write Boom.java");
    let classes = root.join("classes");
    std::fs::create_dir_all(&classes).expect("create classes dir");
    let status = Command::new("javac")
        .arg("-g")
        .arg("-d")
        .arg(&classes)
        .arg(&main_java)
        .arg(pkg.join("Boom.java"))
        .status()
        .expect("run javac");
    assert!(status.success(), "javac failed to compile the fixture");
    (main_java, classes)
}

/// Whether any frame seen so far verified breakpoint (line adjusted or not).
fn saw_verified_breakpoint(seen: &[Value]) -> bool {
    seen.iter().any(|m| {
        let bp = if m["type"] == "response" && m["command"] == "setBreakpoints" {
            &m["body"]["breakpoints"][0]
        } else if m["type"] == "event" && m["event"] == "breakpoint" {
            &m["body"]["breakpoint"]
        } else {
            return false;
        };
        bp["verified"] == json!(true)
    })
}

#[test]
fn launch_breakpoint_step_variables_and_output() {
    let root = temp_root("launch");
    let (main_java, classes) = write_and_compile_fixture(&root);
    let mut dap = Dap::spawn();

    // initialize: capabilities.
    let init = dap.request("initialize", json!({ "adapterID": "java-vsix-lite" }));
    assert_eq!(
        init["body"]["supportsConfigurationDoneRequest"],
        json!(true),
        "{init}"
    );
    let filters = init["body"]["exceptionBreakpointFilters"]
        .as_array()
        .expect("exceptionBreakpointFilters")
        .clone();
    assert!(
        filters.iter().any(|f| f["filter"] == "uncaught"),
        "missing uncaught filter: {init}"
    );

    // launch → initialized.
    dap.request(
        "launch",
        json!({
            "mainClass": "demo.Main",
            "classPaths": [classes.to_string_lossy()],
            "cwd": root.to_string_lossy(),
            "projectRoot": root.to_string_lossy(),
        }),
    );
    dap.event("initialized");

    // Breakpoint on the helper call; verified now or via a later event.
    dap.request(
        "setBreakpoints",
        json!({
            "source": { "path": main_java.to_string_lossy() },
            "breakpoints": [{ "line": BP_LINE }],
        }),
    );
    dap.request("configurationDone", json!({}));

    // Stop at the breakpoint.
    let stopped = dap.stopped("breakpoint");
    assert!(
        saw_verified_breakpoint(&dap.seen),
        "breakpoint never verified; saw:\n{:#?}",
        dap.seen
    );
    let thread_id = stopped["body"]["threadId"].as_i64().expect("threadId");

    // Threads include the stopped one.
    let threads = dap.request("threads", json!({}));
    assert!(
        threads["body"]["threads"]
            .as_array()
            .expect("threads array")
            .iter()
            .any(|t| t["id"] == json!(thread_id)),
        "{threads}"
    );

    // Top frame: demo.Main.main at the breakpoint line, source Main.java.
    // `levels: 20` over-asks a 1-frame stack exactly like VS Code does —
    // regression guard for HotSpot rejecting out-of-range Frames lengths.
    let stack = dap.request(
        "stackTrace",
        json!({ "threadId": thread_id, "startFrame": 0, "levels": 20 }),
    );
    let top = &stack["body"]["stackFrames"][0];
    assert_eq!(top["name"], json!("demo.Main.main"), "{stack}");
    assert_eq!(top["line"], json!(BP_LINE), "{stack}");
    assert!(
        top["source"]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("Main.java")),
        "{stack}"
    );
    let frame_id = top["id"].as_i64().expect("frame id");

    // Locals show the int local.
    let scopes = dap.request("scopes", json!({ "frameId": frame_id }));
    let varref = scopes["body"]["scopes"][0]["variablesReference"]
        .as_i64()
        .expect("variablesReference");
    let variables = dap.request("variables", json!({ "variablesReference": varref }));
    let vars = variables["body"]["variables"]
        .as_array()
        .expect("variables array");
    assert!(
        vars.iter()
            .any(|v| v["name"] == "count" && v["value"] == "41"),
        "expected local count=41: {variables}"
    );

    // Step over → stopped(step).
    dap.request("next", json!({ "threadId": thread_id }));
    dap.stopped("step");

    // Continue → program output → termination.
    dap.request("continue", json!({ "threadId": thread_id }));
    dap.until("SENTINEL output", |m| {
        m["type"] == "event"
            && m["event"] == "output"
            && m["body"]["output"]
                .as_str()
                .is_some_and(|s| s.contains("SENTINEL:42"))
    });
    dap.event("terminated");
    dap.request("disconnect", json!({}));
    dap.expect_exit();
}

#[test]
fn attach_breakpoint_and_terminate_debuggee() {
    let root = temp_root("attach");
    let (main_java, classes) = write_and_compile_fixture(&root);

    // Spawn the debuggee directly, suspended, on an ephemeral port.
    let mut debuggee = Command::new("java")
        .arg("-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0")
        .arg("-cp")
        .arg(&classes)
        .arg("demo.Main")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn java debuggee");
    let mut debuggee_out = BufReader::new(debuggee.stdout.take().expect("stdout"));
    let mut listen_line = String::new();
    debuggee_out
        .read_line(&mut listen_line)
        .expect("read listen line");
    let port: u16 = listen_line
        .rsplit(' ')
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("unparseable listen line: {listen_line:?}"));

    let mut dap = Dap::spawn();
    dap.request("initialize", json!({ "adapterID": "java-vsix-lite" }));
    dap.request(
        "attach",
        json!({ "port": port, "projectRoot": root.to_string_lossy() }),
    );
    dap.event("initialized");
    dap.request(
        "setBreakpoints",
        json!({
            "source": { "path": main_java.to_string_lossy() },
            "breakpoints": [{ "line": BP_LINE }],
        }),
    );
    dap.request("configurationDone", json!({}));
    dap.stopped("breakpoint");

    // Disconnect terminating the debuggee: the java process must exit.
    dap.request("disconnect", json!({ "terminateDebuggee": true }));
    dap.expect_exit();
    let status = debuggee.wait().expect("debuggee wait");
    assert!(
        status.code().is_some(),
        "debuggee killed by signal rather than exiting: {status:?}"
    );
}

#[test]
fn stop_on_entry_breaks_at_main_then_runs_to_completion() {
    let root = temp_root("entry");
    let (_, classes) = write_and_compile_fixture(&root);
    let mut dap = Dap::spawn();
    dap.request("initialize", json!({ "adapterID": "java-vsix-lite" }));
    dap.request(
        "launch",
        json!({
            "mainClass": "demo.Main",
            "classPaths": [classes.to_string_lossy()],
            "cwd": root.to_string_lossy(),
            "stopOnEntry": true,
        }),
    );
    dap.event("initialized");
    dap.request("configurationDone", json!({}));

    // The internal one-shot entry breakpoint: first line of main.
    let stopped = dap.stopped("entry");
    let thread_id = stopped["body"]["threadId"].as_i64().expect("threadId");
    let stack = dap.request(
        "stackTrace",
        json!({ "threadId": thread_id, "startFrame": 0, "levels": 20 }),
    );
    assert_eq!(
        stack["body"]["stackFrames"][0]["name"],
        json!("demo.Main.main"),
        "{stack}"
    );

    // No user breakpoints: continue runs the program to completion.
    dap.request("continue", json!({ "threadId": thread_id }));
    dap.until("SENTINEL output", |m| {
        m["type"] == "event"
            && m["event"] == "output"
            && m["body"]["output"]
                .as_str()
                .is_some_and(|s| s.contains("SENTINEL:42"))
    });
    dap.event("terminated");
    dap.request("disconnect", json!({}));
    dap.expect_exit();
}

#[test]
fn launch_auto_compiles_unbuilt_maven_project() {
    let root = temp_root("autocompile");
    // A Maven project that has never been built: pom.xml + sources, no
    // target/classes anywhere.
    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>com.example</groupId>\
         <artifactId>demo</artifactId><version>1.0</version></project>",
    )
    .expect("write pom.xml");
    let pkg = root.join("src/main/java/demo");
    std::fs::create_dir_all(&pkg).expect("create package dir");
    let main_java = pkg.join("Main.java");
    std::fs::write(&main_java, MAIN_JAVA).expect("write Main.java");

    let mut dap = Dap::spawn();
    dap.request("initialize", json!({ "adapterID": "java-vsix-lite" }));
    dap.request(
        "launch",
        json!({
            "mainClass": "demo.Main",
            "projectRoot": root.to_string_lossy(),
            "cwd": root.to_string_lossy(),
        }),
    );
    dap.event("initialized");
    dap.request(
        "setBreakpoints",
        json!({
            "source": { "path": main_java.to_string_lossy() },
            "breakpoints": [{ "line": BP_LINE }],
        }),
    );
    dap.request("configurationDone", json!({}));
    // Reaching the breakpoint proves the auto-compile fallback end to end.
    dap.stopped("breakpoint");
    dap.request("disconnect", json!({}));
    dap.expect_exit();
}

#[test]
fn uncaught_exception_stops_with_exception_info() {
    let root = temp_root("exception");
    let (_, classes) = write_and_compile_fixture(&root);
    let mut dap = Dap::spawn();
    dap.request("initialize", json!({ "adapterID": "java-vsix-lite" }));
    dap.request(
        "launch",
        json!({
            "mainClass": "demo.Boom",
            "classPaths": [classes.to_string_lossy()],
            "cwd": root.to_string_lossy(),
        }),
    );
    dap.event("initialized");
    dap.request(
        "setExceptionBreakpoints",
        json!({ "filters": ["uncaught"] }),
    );
    dap.request("configurationDone", json!({}));

    let stopped = dap.stopped("exception");
    let thread_id = stopped["body"]["threadId"].as_i64().expect("threadId");
    let info = dap.request("exceptionInfo", json!({ "threadId": thread_id }));
    assert_eq!(
        info["body"]["exceptionId"],
        json!("java.lang.IllegalStateException"),
        "{info}"
    );
    assert!(
        info["body"]["description"]
            .as_str()
            .is_some_and(|d| d.contains("boom")),
        "{info}"
    );
    assert_eq!(info["body"]["breakMode"], json!("unhandled"), "{info}");
    dap.request("disconnect", json!({}));
    dap.expect_exit();
}

/// `evaluate` is deliberately unsupported (no expression evaluation, no
/// method invocation in the debuggee) — the response must say so politely.
#[test]
fn evaluate_is_politely_rejected() {
    let mut dap = Dap::spawn();
    dap.request("initialize", json!({ "adapterID": "java-vsix-lite" }));
    let seq = dap.send("evaluate", json!({ "expression": "1 + 1" }));
    let response = dap.until("evaluate response", |m| {
        m["type"] == "response" && m["request_seq"] == json!(seq)
    });
    assert_eq!(response["success"], json!(false), "{response}");
    assert_eq!(
        response["message"],
        json!("Expression evaluation is not supported"),
        "{response}"
    );
    dap.request("disconnect", json!({}));
    dap.expect_exit();
}
