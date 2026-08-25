//! The DAP↔JDWP bridge session.
//!
//! One event loop owns all mutable state; stdin requests, JDWP composite
//! events, debuggee output, and debuggee exit are funneled into a single
//! `mpsc` channel by small forwarder tasks. Supported requests are handled
//! explicitly; everything else — including `evaluate` — gets a
//! `success:false` response (expression evaluation is out of scope by
//! design: the adapter never invokes debuggee methods).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use super::protocol::{
    self, AttachArgs, DapWriter, DisconnectArgs, FrameIdArgs, LaunchArgs, SetBreakpointsArgs,
    SetExceptionBreakpointsArgs, StackTraceArgs, ThreadIdArgs, VariablesArgs,
};
use crate::jdwp::codec::{Event, EventSet, Location, Value as JValue};
use crate::jdwp::commands::{event_kind, step, suspend_policy, tag};
use crate::jdwp::{JdwpClient, Modifier, Vm};
use crate::launch;

/// Deadline for the JVM to print its JDWP listen address after spawn.
const LISTEN_DEADLINE: Duration = Duration::from_secs(10);

/// Deadline for `VirtualMachine.Exit` to take effect before the child is
/// killed outright.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Chunk cap for forwarded debuggee output.
const OUTPUT_CHUNK_CAP: usize = 8 * 1024;

/// Rendered string values are truncated here (DAP display, not wire cap).
const STRING_DISPLAY_CAP: usize = 512;

/// Array children shown per `variables` request.
const ARRAY_CHILD_CAP: u32 = 200;

/// Superclass-chain walk cap for field collection.
const SUPER_CHAIN_CAP: usize = 32;

/// Everything the event loop can receive.
enum Inbound {
    Request(Value),
    StdinClosed,
    JdwpEvent(Vec<u8>),
    JdwpClosed,
    ChildOut {
        category: &'static str,
        chunk: String,
    },
    ChildExit(Option<i32>),
    KillDeadline,
}

#[derive(PartialEq)]
enum Flow {
    Continue,
    Exit,
}

/// Run the adapter over this process's stdio until the client disconnects.
pub async fn run_stdio_adapter() -> ExitCode {
    let (tx, mut rx) = mpsc::channel::<Inbound>(256);

    let stdin_tx = tx.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
        loop {
            match protocol::read_message(&mut stdin).await {
                Ok(Some(message)) => {
                    if stdin_tx.send(Inbound::Request(message)).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => {
                    let _ = stdin_tx.send(Inbound::StdinClosed).await;
                    break;
                }
            }
        }
    });

    let mut session = Session::new(tx);
    while let Some(message) = rx.recv().await {
        if session.handle(message).await == Flow::Exit {
            break;
        }
    }
    session.cleanup().await;
    // A plain return would hang: `tokio::io::stdin`'s in-flight blocking
    // read keeps runtime shutdown waiting until the *client* closes our
    // stdin, which VS Code only does after a kill timeout. The session is
    // fully torn down (child killed and reaped, scratch dir removed) — exit
    // the process directly.
    std::process::exit(0);
}

/// A DAP breakpoint for one requested source line.
struct LineBp {
    id: i64,
    line: u32,
    verified: bool,
    actual_line: Option<u32>,
    message: Option<String>,
    /// JDWP BREAKPOINT request ids (one per class the line bound in).
    request_ids: Vec<u32>,
}

/// Per-source-file breakpoint state.
struct FileBps {
    /// `pkg.Cls` for the file's top-level class.
    class_name: String,
    /// The two CLASS_PREPARE requests (`pkg.Cls`, `pkg.Cls$*`).
    prepare_request_ids: Vec<u32>,
    bps: Vec<LineBp>,
}

/// What a `variablesReference` denotes.
enum VarRef {
    Locals {
        thread: u64,
        frame: u64,
        location: Location,
    },
    Object(u64),
    Array(u64),
}

struct FrameHandle {
    thread: u64,
    frame: u64,
    location: Location,
}

/// Cached per-class metadata (`None` fields = not fetched yet; inner
/// `None` = VM reported absent information).
#[derive(Default)]
struct ClassMeta {
    signature: Option<String>,
    source_file: Option<Option<String>>,
    methods: Option<Vec<crate::jdwp::MethodEntry>>,
    line_tables: HashMap<u64, Option<crate::jdwp::LineTable>>,
}

/// One-shot stop-on-entry state.
struct EntryState {
    prepare_request_id: u32,
    bp_request_id: Option<u32>,
}

struct Session {
    writer: DapWriter,
    tx: mpsc::Sender<Inbound>,
    vm: Option<Arc<Vm>>,
    launch_mode: bool,
    child_kill: Option<oneshot::Sender<()>>,
    /// Resolves once the child-wait task has reaped the child — lets
    /// `cleanup` finish teardown deterministically before process exit.
    child_reaped: Option<oneshot::Receiver<()>>,
    child_alive: bool,
    scratch_dir: Option<PathBuf>,
    source_roots: Vec<PathBuf>,
    stop_on_entry: bool,
    main_class: Option<String>,
    /// Outstanding SUSPEND_ALL count (events received + explicit pauses).
    suspend_depth: u32,
    terminated_sent: bool,
    /// A held `suspend=y` VM_START was delivered (attach to a suspended
    /// VM); `configurationDone` owes it one resume.
    vm_start_pending: bool,

    // DAP handle maps (JDWP 64-bit ids never leak into DAP numbers).
    thread_to_dap: HashMap<u64, i64>,
    dap_to_thread: HashMap<i64, u64>,
    next_thread_id: i64,
    next_handle: i64,
    frames: HashMap<i64, FrameHandle>,
    varrefs: HashMap<i64, VarRef>,

    classes: HashMap<u64, ClassMeta>,
    files: HashMap<String, FileBps>,
    next_bp_id: i64,
    exception_request_ids: Vec<u32>,
    entry: Option<EntryState>,
    step_request_ids: HashSet<u32>,
    /// jdwp thread → (exception object, caught).
    last_exception: HashMap<u64, (u64, bool)>,
    /// Classes already warned about missing `-g` local info.
    locals_warned: HashSet<u64>,
}

impl Session {
    fn new(tx: mpsc::Sender<Inbound>) -> Session {
        Session {
            writer: DapWriter::new(),
            tx,
            vm: None,
            launch_mode: false,
            child_kill: None,
            child_reaped: None,
            child_alive: false,
            scratch_dir: None,
            source_roots: Vec::new(),
            stop_on_entry: false,
            main_class: None,
            suspend_depth: 0,
            terminated_sent: false,
            vm_start_pending: false,
            thread_to_dap: HashMap::new(),
            dap_to_thread: HashMap::new(),
            next_thread_id: 1,
            next_handle: 1000,
            frames: HashMap::new(),
            varrefs: HashMap::new(),
            classes: HashMap::new(),
            files: HashMap::new(),
            next_bp_id: 1,
            exception_request_ids: Vec::new(),
            entry: None,
            step_request_ids: HashSet::new(),
            last_exception: HashMap::new(),
            locals_warned: HashSet::new(),
        }
    }

    fn vm(&self) -> Result<Arc<Vm>, String> {
        self.vm
            .clone()
            .ok_or_else(|| "No debug session".to_string())
    }

    async fn handle(&mut self, message: Inbound) -> Flow {
        match message {
            Inbound::Request(request) => self.handle_request(request).await,
            Inbound::StdinClosed => Flow::Exit,
            Inbound::JdwpEvent(payload) => {
                let Some(vm) = self.vm.clone() else {
                    return Flow::Continue;
                };
                match crate::jdwp::codec::parse_composite(&payload, vm.sizes) {
                    Ok(set) => self.handle_event_set(set).await,
                    Err(e) => {
                        // Untrusted debuggee sent malformed data: tear down.
                        tracing::warn!("JDWP decode error: {e}");
                        self.send_terminated().await;
                    }
                }
                Flow::Continue
            }
            Inbound::JdwpClosed => {
                self.vm = None;
                self.send_terminated().await;
                Flow::Continue
            }
            Inbound::ChildOut { category, chunk } => {
                self.writer.output(category, &chunk).await;
                Flow::Continue
            }
            Inbound::ChildExit(code) => {
                self.child_alive = false;
                self.child_kill = None;
                self.writer
                    .event("exited", json!({ "exitCode": code.unwrap_or(0) }))
                    .await;
                self.send_terminated().await;
                Flow::Continue
            }
            Inbound::KillDeadline => {
                if self.child_alive {
                    if let Some(kill) = self.child_kill.take() {
                        let _ = kill.send(());
                    }
                }
                Flow::Continue
            }
        }
    }

    async fn send_terminated(&mut self) {
        if !self.terminated_sent {
            self.terminated_sent = true;
            self.writer.event("terminated", json!({})).await;
        }
    }

    async fn handle_request(&mut self, request: Value) -> Flow {
        let seq = request["seq"].as_i64().unwrap_or(0);
        let command = request["command"].as_str().unwrap_or("").to_string();
        let args = request["arguments"].clone();

        let result = match command.as_str() {
            "initialize" => Ok(capabilities()),
            "launch" => self.launch(args).await,
            "attach" => self.attach(args).await,
            "setBreakpoints" => self.set_breakpoints(args).await,
            "setExceptionBreakpoints" => self.set_exception_breakpoints(args).await,
            "configurationDone" => self.configuration_done().await,
            "threads" => self.threads().await,
            "stackTrace" => self.stack_trace(args).await,
            "scopes" => self.scopes(args),
            "variables" => self.variables(args).await,
            "continue" => self.continue_all().await,
            "next" => self.step(args, step::DEPTH_OVER).await,
            "stepIn" => self.step(args, step::DEPTH_INTO).await,
            "stepOut" => self.step(args, step::DEPTH_OUT).await,
            "pause" => self.pause(args).await,
            "exceptionInfo" => self.exception_info(args).await,
            "terminate" => self.terminate().await,
            "disconnect" => self.disconnect(args).await,
            "evaluate" => Err("Expression evaluation is not supported".to_string()),
            other => Err(format!("Unsupported request: {other}")),
        };

        let succeeded = result.is_ok();
        self.writer.respond(seq, &command, result).await;

        match command.as_str() {
            // The client configures breakpoints only after `initialized`.
            "launch" | "attach" if succeeded => {
                self.writer.event("initialized", json!({})).await;
                Flow::Continue
            }
            "disconnect" => Flow::Exit,
            _ => Flow::Continue,
        }
    }

    // ------------------------------------------------------------------
    // Session start
    // ------------------------------------------------------------------

    async fn launch(&mut self, args: Value) -> Result<Value, String> {
        let args: LaunchArgs =
            serde_json::from_value(args).map_err(|e| format!("Bad launch arguments: {e}"))?;
        let main_class = args
            .main_class
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| "Missing required launch attribute \"mainClass\".".to_string())?
            .to_string();
        let jdk_home = args.jdk_home.clone().map(PathBuf::from);
        let java = launch::locate_java(jdk_home.as_deref())
            .ok_or_else(|| "No JDK found. Set java-vsix-lite.jdk.home.".to_string())?;
        let project_root = args.project_root.clone().map(PathBuf::from);

        // Classpath assembly can walk the project and run a bounded javac.
        let class_paths = args.class_paths.clone();
        let blocking_root = project_root.clone();
        let blocking_jdk = jdk_home.clone();
        let plan = tokio::task::spawn_blocking(move || {
            launch::assemble_classpath(
                blocking_root.as_deref(),
                &class_paths,
                blocking_jdk.as_deref(),
            )
        })
        .await
        .map_err(|e| format!("Classpath assembly failed: {e}"))?;
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                if let Some(stderr) = error.javac_stderr {
                    self.writer.output("stderr", &stderr).await;
                }
                return Err(error.message);
            }
        };
        self.source_roots = plan.source_roots;
        self.scratch_dir = plan.scratch_dir;

        let joined = std::env::join_paths(&plan.classpath)
            .map_err(|e| format!("Unrepresentable classpath entry: {e}"))?;
        let mut command = tokio::process::Command::new(&java);
        command
            .args(&args.vm_args)
            .arg("-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0")
            .arg("-cp")
            .arg(joined)
            .arg(&main_class)
            .args(&args.args)
            .envs(&args.env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let cwd = args.cwd.clone().map(PathBuf::from).or(project_root);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Could not start {}: {e}", java.display()))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // The JVM prints `Listening for transport dt_socket at address: <port>`
        // (stdout normally; watch both streams) before anything runs.
        let listen =
            tokio::time::timeout(LISTEN_DEADLINE, wait_for_listen_port(stdout, stderr)).await;
        let (port, stdout, stdout_rest, stderr, stderr_rest, early) = match listen {
            Ok(Ok(found)) => found,
            Ok(Err(e)) => {
                child.start_kill().ok();
                child.wait().await.ok();
                return Err(format!("The JVM exited before accepting a debugger: {e}"));
            }
            Err(_) => {
                child.start_kill().ok();
                child.wait().await.ok();
                return Err("Timed out waiting for the JVM's JDWP listen address.".to_string());
            }
        };
        for (category, chunk) in early {
            self.writer.output(category, &chunk).await;
        }

        // Connect (3 retries × 200 ms), handshake, IDSizes.
        let mut stream = None;
        for attempt in 0..3 {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) if attempt == 2 => {
                    child.start_kill().ok();
                    child.wait().await.ok();
                    return Err(format!("Could not connect to the JVM's JDWP port: {e}"));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        let stream = stream.expect("connect loop yields a stream or returns");
        self.start_vm(stream, true).await?;

        // Forward all further child output; own the child in a wait task
        // with a kill channel.
        tokio::spawn(forward_stream(
            stdout,
            stdout_rest,
            "stdout",
            self.tx.clone(),
        ));
        tokio::spawn(forward_stream(
            stderr,
            stderr_rest,
            "stderr",
            self.tx.clone(),
        ));
        let (kill_tx, kill_rx) = oneshot::channel();
        let (reaped_tx, reaped_rx) = oneshot::channel();
        tokio::spawn(child_wait_task(child, kill_rx, reaped_tx, self.tx.clone()));
        self.child_kill = Some(kill_tx);
        self.child_reaped = Some(reaped_rx);
        self.child_alive = true;
        self.stop_on_entry = args.stop_on_entry;
        self.main_class = Some(main_class);
        Ok(json!({}))
    }

    async fn attach(&mut self, args: Value) -> Result<Value, String> {
        let args: AttachArgs =
            serde_json::from_value(args).map_err(|e| format!("Bad attach arguments: {e}"))?;
        let host = args
            .host_name
            .clone()
            .unwrap_or_else(|| "localhost".to_string());
        let port = args
            .port
            .ok_or_else(|| "Missing required attach attribute \"port\".".to_string())?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(args.timeout.unwrap_or(30_000));
        let stream = loop {
            match TcpStream::connect((host.as_str(), port)).await {
                Ok(stream) => break stream,
                Err(e) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(format!("Could not attach to {host}:{port}: {e}"));
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        };
        self.start_vm(stream, false).await?;
        self.source_roots = args
            .project_root
            .as_deref()
            .map(|root| launch::discover_source_roots(Path::new(root)))
            .unwrap_or_default();
        self.source_roots
            .extend(args.source_paths.iter().map(PathBuf::from));
        Ok(json!({}))
    }

    /// Handshake + reader task + IDSizes + event pump.
    async fn start_vm(&mut self, stream: TcpStream, launch_mode: bool) -> Result<(), String> {
        let (client, mut event_rx) = JdwpClient::connect(stream)
            .await
            .map_err(|e| format!("JDWP handshake failed: {e}"))?;
        let vm = Vm::new(client)
            .await
            .map_err(|e| format!("JDWP IDSizes failed: {e}"))?;
        self.vm = Some(Arc::new(vm));
        self.launch_mode = launch_mode;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(payload) = event_rx.recv().await {
                if tx.send(Inbound::JdwpEvent(payload)).await.is_err() {
                    return;
                }
            }
            let _ = tx.send(Inbound::JdwpClosed).await;
        });
        Ok(())
    }

    async fn configuration_done(&mut self) -> Result<Value, String> {
        if self.launch_mode && self.stop_on_entry {
            let vm = self.vm()?;
            if let Some(main_class) = self.main_class.clone() {
                match vm
                    .event_request_set(
                        event_kind::CLASS_PREPARE,
                        suspend_policy::ALL,
                        &[Modifier::ClassMatch(main_class)],
                    )
                    .await
                {
                    Ok(request_id) => {
                        self.entry = Some(EntryState {
                            prepare_request_id: request_id,
                            bp_request_id: None,
                        });
                    }
                    Err(e) => {
                        self.writer
                            .output("console", &format!("stopOnEntry unavailable: {e}\n"))
                            .await;
                    }
                }
            }
        }
        // Release the initial `suspend=y` VM_START suspension (VM_START
        // deliberately does not count into suspend_depth — this resume is
        // its counterpart). Launch mode always holds one; attach mode holds
        // one only when the debuggee started with `suspend=y` and its
        // VM_START was delivered to us on connect.
        if self.launch_mode || self.vm_start_pending {
            self.vm_start_pending = false;
            let vm = self.vm()?;
            vm.resume()
                .await
                .map_err(|e| format!("resume failed: {e}"))?;
        }
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // Breakpoints
    // ------------------------------------------------------------------

    async fn set_breakpoints(&mut self, args: Value) -> Result<Value, String> {
        let args: SetBreakpointsArgs = serde_json::from_value(args)
            .map_err(|e| format!("Bad setBreakpoints arguments: {e}"))?;
        let path = args
            .source
            .path
            .ok_or_else(|| "setBreakpoints without a source path".to_string())?;
        let mut requested: Vec<u32> = args.breakpoints.iter().map(|b| b.line).collect();
        if requested.is_empty() {
            requested = args.lines.clone();
        }
        let vm = self.vm()?;

        if !self.files.contains_key(&path) {
            let class_name = fqcn_for_source(Path::new(&path))
                .ok_or_else(|| format!("Could not determine the class declared in {path}"))?;
            let mut prepare_request_ids = Vec::new();
            for pattern in [class_name.clone(), format!("{class_name}$*")] {
                match vm
                    .event_request_set(
                        event_kind::CLASS_PREPARE,
                        suspend_policy::ALL,
                        &[Modifier::ClassMatch(pattern)],
                    )
                    .await
                {
                    Ok(id) => prepare_request_ids.push(id),
                    Err(e) => tracing::warn!("CLASS_PREPARE request failed: {e}"),
                }
            }
            self.files.insert(
                path.clone(),
                FileBps {
                    class_name,
                    prepare_request_ids,
                    bps: Vec::new(),
                },
            );
        }

        // Drop removed lines (clearing their JDWP requests), keep retained
        // ones, add new ones unverified.
        let file = self.files.get_mut(&path).expect("inserted above");
        let mut kept = Vec::new();
        let mut cleared: Vec<u32> = Vec::new();
        for bp in file.bps.drain(..) {
            if requested.contains(&bp.line) {
                kept.push(bp);
            } else {
                cleared.extend(&bp.request_ids);
            }
        }
        for &line in &requested {
            if !kept.iter().any(|bp| bp.line == line) {
                kept.push(LineBp {
                    id: self.next_bp_id,
                    line,
                    verified: false,
                    actual_line: None,
                    message: None,
                    request_ids: Vec::new(),
                });
                self.next_bp_id += 1;
            }
        }
        file.bps = kept;
        let class_name = file.class_name.clone();
        for request_id in cleared {
            vm.event_request_clear(event_kind::BREAKPOINT, request_id)
                .await
                .ok();
        }

        // Bind against already-loaded classes (attach, or mid-run adds).
        let exact = format!("L{};", class_name.replace('.', "/"));
        let prefix = format!("L{}$", class_name.replace('.', "/"));
        if let Ok(classes) = vm.all_classes().await {
            for class in classes {
                if class.signature == exact || class.signature.starts_with(&prefix) {
                    self.bind_lines_in_class(&path, class.type_id).await;
                }
            }
        }

        let file = self.files.get(&path).expect("still present");
        let breakpoints: Vec<Value> = requested
            .iter()
            .filter_map(|line| file.bps.iter().find(|bp| bp.line == *line))
            .map(breakpoint_json)
            .collect();
        Ok(json!({ "breakpoints": breakpoints }))
    }

    /// Bind every still-unbound breakpoint line of `path` against a
    /// (prepared) class. Newly-verified breakpoints are reported via
    /// `breakpoint` changed events by the caller when appropriate.
    async fn bind_lines_in_class(&mut self, path: &str, class_id: u64) -> Vec<i64> {
        let Ok(vm) = self.vm() else {
            return Vec::new();
        };
        let Some(methods) = self.class_methods(class_id).await else {
            return Vec::new();
        };
        // Collect (method, table) pairs lazily-cached.
        let mut tables: Vec<(u64, crate::jdwp::LineTable)> = Vec::new();
        for method in &methods {
            if let Some(table) = self.method_line_table(class_id, method.id).await {
                tables.push((method.id, table));
            }
        }
        let Some(file) = self.files.get_mut(path) else {
            return Vec::new();
        };
        let mut changed = Vec::new();
        for bp in file.bps.iter_mut().filter(|bp| !bp.verified) {
            // First entry with the exact line; else the smallest line-table
            // line greater than the request, within this class.
            let mut chosen: Option<(u64, u64, u32)> = None; // (method, index, line)
            'exact: for (method_id, table) in &tables {
                for &(index, line) in &table.lines {
                    if line == bp.line {
                        chosen = Some((*method_id, index, line));
                        break 'exact;
                    }
                }
            }
            if chosen.is_none() {
                for (method_id, table) in &tables {
                    for &(index, line) in &table.lines {
                        if line > bp.line && chosen.is_none_or(|(_, _, best)| line < best) {
                            chosen = Some((*method_id, index, line));
                        }
                    }
                }
            }
            let Some((method_id, index, line)) = chosen else {
                if bp.message.is_none() {
                    bp.message = Some("No executable code at this line".to_string());
                }
                continue;
            };
            let location = Location {
                type_tag: 1, // CLASS
                class_id,
                method_id,
                index,
            };
            match vm
                .event_request_set(
                    event_kind::BREAKPOINT,
                    suspend_policy::ALL,
                    &[Modifier::LocationOnly(location)],
                )
                .await
            {
                Ok(request_id) => {
                    bp.request_ids.push(request_id);
                    bp.verified = true;
                    bp.actual_line = Some(line);
                    bp.message = None;
                    changed.push(bp.id);
                }
                Err(e) => tracing::warn!("BREAKPOINT request failed: {e}"),
            }
        }
        changed
    }

    async fn set_exception_breakpoints(&mut self, args: Value) -> Result<Value, String> {
        let args: SetExceptionBreakpointsArgs = serde_json::from_value(args)
            .map_err(|e| format!("Bad setExceptionBreakpoints arguments: {e}"))?;
        let vm = self.vm()?;
        for request_id in self.exception_request_ids.drain(..) {
            vm.event_request_clear(event_kind::EXCEPTION, request_id)
                .await
                .ok();
        }
        let caught = args.filters.iter().any(|f| f == "caught");
        let uncaught = args.filters.iter().any(|f| f == "uncaught");
        if caught || uncaught {
            let request_id = vm
                .event_request_set(
                    event_kind::EXCEPTION,
                    suspend_policy::ALL,
                    &[Modifier::ExceptionOnly { caught, uncaught }],
                )
                .await
                .map_err(|e| format!("EXCEPTION request failed: {e}"))?;
            self.exception_request_ids.push(request_id);
        }
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // JDWP events
    // ------------------------------------------------------------------

    async fn handle_event_set(&mut self, set: EventSet) {
        let all = set.suspend_policy == suspend_policy::ALL;
        let has_vm_start = set
            .events
            .iter()
            .any(|e| matches!(e, Event::VmStart { .. }));
        if all && !has_vm_start {
            // VM_START's suspension is consumed by configurationDone's
            // unconditional resume instead of the depth counter.
            self.suspend_depth += 1;
        }
        // Resume automatically unless some event in the set is a real stop.
        let mut auto_resume = all && !has_vm_start;

        for event in set.events {
            match event {
                Event::VmStart { .. } => {
                    self.vm_start_pending = true;
                }
                Event::ClassPrepare {
                    request_id,
                    type_id,
                    signature,
                    ..
                } => {
                    if self
                        .entry
                        .as_ref()
                        .is_some_and(|e| e.prepare_request_id == request_id)
                    {
                        self.arm_entry_breakpoint(type_id).await;
                        continue;
                    }
                    // Which file registered this prepare request?
                    let path = self.files.iter().find_map(|(path, file)| {
                        file.prepare_request_ids
                            .contains(&request_id)
                            .then(|| path.clone())
                    });
                    if let Some(path) = path {
                        tracing::debug!(%signature, "binding breakpoints on class prepare");
                        let changed = self.bind_lines_in_class(&path, type_id).await;
                        let updates: Vec<Value> = {
                            let Some(file) = self.files.get(&path) else {
                                continue;
                            };
                            file.bps
                                .iter()
                                .filter(|bp| changed.contains(&bp.id))
                                .map(breakpoint_json)
                                .collect()
                        };
                        for breakpoint in updates {
                            self.writer
                                .event(
                                    "breakpoint",
                                    json!({ "reason": "changed", "breakpoint": breakpoint }),
                                )
                                .await;
                        }
                    }
                }
                Event::Breakpoint {
                    request_id, thread, ..
                } => {
                    auto_resume = false;
                    let reason = if self
                        .entry
                        .as_ref()
                        .is_some_and(|e| e.bp_request_id == Some(request_id))
                    {
                        self.entry = None; // one-shot (Count=1 expired it)
                        "entry"
                    } else {
                        "breakpoint"
                    };
                    self.on_stop(thread, reason).await;
                }
                Event::SingleStep {
                    request_id, thread, ..
                } => {
                    auto_resume = false;
                    if self.step_request_ids.remove(&request_id) {
                        if let Ok(vm) = self.vm() {
                            vm.event_request_clear(event_kind::SINGLE_STEP, request_id)
                                .await
                                .ok();
                        }
                    }
                    self.on_stop(thread, "step").await;
                }
                Event::Exception {
                    thread,
                    exception,
                    catch_location,
                    ..
                } => {
                    auto_resume = false;
                    self.last_exception
                        .insert(thread, (exception, catch_location.is_some()));
                    self.on_stop(thread, "exception").await;
                }
                Event::VmDeath { .. } => {
                    auto_resume = false;
                    self.send_terminated().await;
                }
            }
        }

        if auto_resume {
            if let Ok(vm) = self.vm() {
                if vm.resume().await.is_ok() {
                    self.suspend_depth = self.suspend_depth.saturating_sub(1);
                }
            }
        }
    }

    /// The `stopOnEntry` CLASS_PREPARE fired for the main class: set a
    /// one-shot breakpoint on `main([Ljava/lang/String;)V`'s first line.
    async fn arm_entry_breakpoint(&mut self, type_id: u64) {
        let Ok(vm) = self.vm() else { return };
        if let Some(entry) = self.entry.as_ref() {
            vm.event_request_clear(event_kind::CLASS_PREPARE, entry.prepare_request_id)
                .await
                .ok();
        }
        let main = match self.class_methods(type_id).await {
            Some(methods) => methods
                .iter()
                .find(|m| m.name == "main" && m.signature == "([Ljava/lang/String;)V")
                .map(|m| m.id),
            None => None,
        };
        let location = match main {
            Some(method_id) => self
                .method_line_table(type_id, method_id)
                .await
                .and_then(|table| table.lines.first().copied())
                .map(|(index, _)| Location {
                    type_tag: 1,
                    class_id: type_id,
                    method_id,
                    index,
                }),
            None => None,
        };
        let Some(location) = location else {
            self.entry = None;
            self.writer
                .output(
                    "console",
                    "stopOnEntry skipped: no line info for the main method \
                     (compiled without -g)\n",
                )
                .await;
            return;
        };
        match vm
            .event_request_set(
                event_kind::BREAKPOINT,
                suspend_policy::ALL,
                &[Modifier::LocationOnly(location), Modifier::Count(1)],
            )
            .await
        {
            Ok(request_id) => {
                if let Some(entry) = self.entry.as_mut() {
                    entry.bp_request_id = Some(request_id);
                }
            }
            Err(e) => {
                self.entry = None;
                tracing::warn!("entry breakpoint failed: {e}");
            }
        }
    }

    /// A SUSPEND_ALL stop: fresh handles, then the DAP `stopped` event.
    async fn on_stop(&mut self, thread: u64, reason: &str) {
        self.invalidate_handles();
        let thread_id = self.dap_thread_id(thread);
        self.writer
            .event(
                "stopped",
                json!({
                    "reason": reason,
                    "threadId": thread_id,
                    "allThreadsStopped": true,
                }),
            )
            .await;
    }

    /// Fresh stop or any resume invalidates frame and variable handles
    /// (DAP contract). Recorded exceptions are cleared on resume only —
    /// `exceptionInfo` must work while stopped on the exception.
    fn invalidate_handles(&mut self) {
        self.frames.clear();
        self.varrefs.clear();
    }

    // ------------------------------------------------------------------
    // Execution control
    // ------------------------------------------------------------------

    async fn continue_all(&mut self) -> Result<Value, String> {
        let vm = self.vm()?;
        self.invalidate_handles();
        self.last_exception.clear();
        // One resume per outstanding SUSPEND_ALL (at least one).
        let resumes = self.suspend_depth.max(1);
        for _ in 0..resumes {
            vm.resume()
                .await
                .map_err(|e| format!("resume failed: {e}"))?;
        }
        self.suspend_depth = 0;
        Ok(json!({ "allThreadsContinued": true }))
    }

    async fn step(&mut self, args: Value, depth: u32) -> Result<Value, String> {
        let args: ThreadIdArgs =
            serde_json::from_value(args).map_err(|e| format!("Bad step arguments: {e}"))?;
        let thread = self.jdwp_thread(args.thread_id)?;
        let vm = self.vm()?;
        let request_id = vm
            .event_request_set(
                event_kind::SINGLE_STEP,
                suspend_policy::ALL,
                &[
                    Modifier::Step {
                        thread,
                        size: step::SIZE_LINE,
                        depth,
                    },
                    Modifier::Count(1),
                ],
            )
            .await
            .map_err(|e| format!("step request failed: {e}"))?;
        self.step_request_ids.insert(request_id);
        self.continue_all().await?;
        Ok(json!({}))
    }

    async fn pause(&mut self, args: Value) -> Result<Value, String> {
        let args: ThreadIdArgs =
            serde_json::from_value(args).map_err(|e| format!("Bad pause arguments: {e}"))?;
        let vm = self.vm()?;
        vm.suspend()
            .await
            .map_err(|e| format!("suspend failed: {e}"))?;
        self.suspend_depth += 1;
        self.invalidate_handles();
        self.writer
            .event(
                "stopped",
                json!({
                    "reason": "pause",
                    "threadId": args.thread_id,
                    "allThreadsStopped": true,
                }),
            )
            .await;
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // Threads / stacks / variables
    // ------------------------------------------------------------------

    fn dap_thread_id(&mut self, thread: u64) -> i64 {
        if let Some(&id) = self.thread_to_dap.get(&thread) {
            return id;
        }
        let id = self.next_thread_id;
        self.next_thread_id += 1;
        self.thread_to_dap.insert(thread, id);
        self.dap_to_thread.insert(id, thread);
        id
    }

    fn jdwp_thread(&self, dap_id: i64) -> Result<u64, String> {
        self.dap_to_thread
            .get(&dap_id)
            .copied()
            .ok_or_else(|| format!("Unknown thread {dap_id}"))
    }

    fn new_handle(&mut self) -> i64 {
        let id = self.next_handle;
        self.next_handle += 1;
        id
    }

    async fn threads(&mut self) -> Result<Value, String> {
        let vm = self.vm()?;
        let threads = vm
            .all_threads()
            .await
            .map_err(|e| format!("AllThreads failed: {e}"))?;
        let mut list = Vec::new();
        for thread in threads {
            let name = vm
                .thread_name(thread)
                .await
                .unwrap_or_else(|_| "<unnamed>".to_string());
            let id = self.dap_thread_id(thread);
            list.push(json!({ "id": id, "name": name }));
        }
        Ok(json!({ "threads": list }))
    }

    async fn stack_trace(&mut self, args: Value) -> Result<Value, String> {
        let args: StackTraceArgs =
            serde_json::from_value(args).map_err(|e| format!("Bad stackTrace arguments: {e}"))?;
        let thread = self.jdwp_thread(args.thread_id)?;
        let vm = self.vm()?;
        let total = vm.frame_count(thread).await.ok();
        // HotSpot's `ThreadReference.Frames` ERRORS (rather than truncating)
        // when `startFrame + length` exceeds the actual frame count — and
        // VS Code always over-asks (`levels: 20` on a 1-frame `main`), which
        // would kill the whole call stack (and the editor's jump to the
        // stopped line). Clamp to what actually exists; 0/absent means all.
        let length = match total {
            Some(total) => {
                let available = total.saturating_sub(args.start_frame);
                if available == 0 {
                    return Ok(json!({ "stackFrames": [], "totalFrames": total }));
                }
                match args.levels {
                    0 => -1,
                    levels => levels.min(available) as i32,
                }
            }
            None if args.levels == 0 => -1,
            None => args.levels as i32,
        };
        let frames = vm
            .frames(thread, args.start_frame, length)
            .await
            .map_err(|e| format!("Frames failed: {e}"))?;
        let mut out = Vec::new();
        for frame in frames {
            let location = frame.location;
            let signature = self.class_signature(location.class_id).await;
            let type_name = signature
                .as_deref()
                .map(pretty_type)
                .unwrap_or_else(|| "<unknown>".to_string());
            let method_name = self
                .class_methods(location.class_id)
                .await
                .and_then(|methods| {
                    methods
                        .iter()
                        .find(|m| m.id == location.method_id)
                        .map(|m| m.name.clone())
                })
                .unwrap_or_else(|| "<unknown>".to_string());
            let line = self
                .method_line_table(location.class_id, location.method_id)
                .await
                .and_then(|table| line_for_index(&table, location.index));
            let source = match signature.as_deref() {
                Some(sig) => self.resolve_source(sig, location.class_id).await,
                None => None,
            };
            let handle = self.new_handle();
            self.frames.insert(
                handle,
                FrameHandle {
                    thread,
                    frame: frame.frame_id,
                    location,
                },
            );
            let mut frame_json = json!({
                "id": handle,
                "name": format!("{type_name}.{method_name}"),
                "line": line.unwrap_or(0),
                "column": 0,
            });
            if let Some(path) = source {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                frame_json["source"] = json!({ "name": name, "path": path.to_string_lossy() });
            }
            out.push(frame_json);
        }
        let mut body = json!({ "stackFrames": out });
        if let Some(total) = total {
            body["totalFrames"] = json!(total);
        }
        Ok(body)
    }

    fn scopes(&mut self, args: Value) -> Result<Value, String> {
        let args: FrameIdArgs =
            serde_json::from_value(args).map_err(|e| format!("Bad scopes arguments: {e}"))?;
        let frame = self
            .frames
            .get(&args.frame_id)
            .ok_or_else(|| "Unknown or stale frame".to_string())?;
        let varref = VarRef::Locals {
            thread: frame.thread,
            frame: frame.frame,
            location: frame.location,
        };
        let handle = self.new_handle();
        self.varrefs.insert(handle, varref);
        Ok(json!({
            "scopes": [{
                "name": "Locals",
                "variablesReference": handle,
                "expensive": false,
            }]
        }))
    }

    async fn variables(&mut self, args: Value) -> Result<Value, String> {
        let args: VariablesArgs =
            serde_json::from_value(args).map_err(|e| format!("Bad variables arguments: {e}"))?;
        let variables = match self.varrefs.get(&args.variables_reference) {
            Some(VarRef::Locals {
                thread,
                frame,
                location,
            }) => {
                let (thread, frame, location) = (*thread, *frame, *location);
                self.local_variables(thread, frame, location).await?
            }
            Some(VarRef::Object(id)) => {
                let id = *id;
                self.object_children(id).await?
            }
            Some(VarRef::Array(id)) => {
                let id = *id;
                self.array_children(id).await?
            }
            None => return Err("Unknown or stale variablesReference".to_string()),
        };
        Ok(json!({ "variables": variables }))
    }

    async fn local_variables(
        &mut self,
        thread: u64,
        frame: u64,
        location: Location,
    ) -> Result<Vec<Value>, String> {
        let vm = self.vm()?;
        let mut variables = Vec::new();
        // `this` first, when present (absent in static frames).
        if let Ok(JValue::Object { tag: t, id }) = vm.this_object(thread, frame).await {
            if id != 0 {
                let rendered = self.render_value(JValue::Object { tag: t, id }).await;
                variables.push(variable_json("this", rendered));
            }
        }
        match vm
            .variable_table(location.class_id, location.method_id)
            .await
        {
            Ok(slots) => {
                let live: Vec<_> = slots
                    .into_iter()
                    .filter(|s| {
                        s.code_index <= location.index
                            && location.index < s.code_index + u64::from(s.length)
                    })
                    .collect();
                if !live.is_empty() {
                    let request: Vec<(u32, u8)> = live
                        .iter()
                        .map(|s| {
                            (
                                s.slot,
                                s.signature.as_bytes().first().copied().unwrap_or(b'L'),
                            )
                        })
                        .collect();
                    match vm.stack_values(thread, frame, &request).await {
                        Ok(values) => {
                            for (slot, value) in live.iter().zip(values) {
                                let rendered = self.render_value(value).await;
                                variables.push(variable_json(&slot.name, rendered));
                            }
                        }
                        Err(e) => return Err(format!("StackFrame.GetValues failed: {e}")),
                    }
                }
            }
            Err(e) if e.is_absent_information() => {
                if self.locals_warned.insert(location.class_id) {
                    let class = self
                        .class_signature(location.class_id)
                        .await
                        .as_deref()
                        .map(pretty_type)
                        .unwrap_or_else(|| "<unknown>".to_string());
                    self.writer
                        .output(
                            "console",
                            &format!("Locals unavailable for {class} (compiled without -g)\n"),
                        )
                        .await;
                }
            }
            Err(e) => return Err(format!("VariableTable failed: {e}")),
        }
        Ok(variables)
    }

    async fn object_children(&mut self, object: u64) -> Result<Vec<Value>, String> {
        let vm = self.vm()?;
        let (_, mut type_id) = vm
            .object_type(object)
            .await
            .map_err(|e| format!("ReferenceType failed: {e}"))?;
        // Instance fields up the superclass chain (statics omitted).
        const ACC_STATIC: u32 = 0x0008;
        let mut fields = Vec::new();
        for _ in 0..SUPER_CHAIN_CAP {
            if type_id == 0 {
                break;
            }
            if let Ok(class_fields) = vm.fields(type_id).await {
                fields.extend(
                    class_fields
                        .into_iter()
                        .filter(|f| f.mod_bits & ACC_STATIC == 0),
                );
            }
            type_id = vm.superclass(type_id).await.unwrap_or(0);
        }
        let field_ids: Vec<u64> = fields.iter().map(|f| f.id).collect();
        if field_ids.is_empty() {
            return Ok(Vec::new());
        }
        let values = vm
            .object_values(object, &field_ids)
            .await
            .map_err(|e| format!("GetValues failed: {e}"))?;
        let mut variables = Vec::new();
        for (field, value) in fields.iter().zip(values) {
            let rendered = self.render_value(value).await;
            variables.push(variable_json(&field.name, rendered));
        }
        Ok(variables)
    }

    async fn array_children(&mut self, object: u64) -> Result<Vec<Value>, String> {
        let vm = self.vm()?;
        let length = vm
            .array_length(object)
            .await
            .map_err(|e| format!("ArrayReference.Length failed: {e}"))?;
        let shown = length.min(ARRAY_CHILD_CAP);
        let mut variables = Vec::new();
        if shown > 0 {
            let values = vm
                .array_values(object, 0, shown)
                .await
                .map_err(|e| format!("ArrayReference.GetValues failed: {e}"))?;
            for (i, value) in values.into_iter().enumerate() {
                let rendered = self.render_value(value).await;
                variables.push(variable_json(&format!("[{i}]"), rendered));
            }
        }
        if length > shown {
            variables.push(variable_json(
                "…",
                (format!("({} more elements not shown)", length - shown), 0),
            ));
        }
        Ok(variables)
    }

    /// Render a JDWP value for display → (value string, variablesReference).
    /// Never invokes debuggee methods (no `toString()`).
    async fn render_value(&mut self, value: JValue) -> (String, i64) {
        match value {
            JValue::Void => ("void".to_string(), 0),
            JValue::Boolean(v) => (v.to_string(), 0),
            JValue::Byte(v) => (v.to_string(), 0),
            JValue::Short(v) => (v.to_string(), 0),
            JValue::Int(v) => (v.to_string(), 0),
            JValue::Long(v) => (v.to_string(), 0),
            JValue::Float(v) => (v.to_string(), 0),
            JValue::Double(v) => (v.to_string(), 0),
            JValue::Char(v) => match char::from_u32(u32::from(v)) {
                Some(c) => (format!("'{c}'"), 0),
                None => (format!("'\\u{v:04x}'"), 0),
            },
            JValue::Object { id: 0, .. } => ("null".to_string(), 0),
            JValue::Object {
                tag: tag::STRING,
                id,
            } => {
                let Ok(vm) = self.vm() else {
                    return ("<gone>".to_string(), 0);
                };
                match vm.string_value(id).await {
                    Ok(s) => {
                        let mut display: String = s.chars().take(STRING_DISPLAY_CAP).collect();
                        if display.len() < s.len() {
                            display.push('…');
                        }
                        (format!("\"{display}\""), 0)
                    }
                    Err(_) => ("<string unavailable>".to_string(), 0),
                }
            }
            JValue::Object {
                tag: tag::ARRAY,
                id,
            } => {
                let Ok(vm) = self.vm() else {
                    return ("<gone>".to_string(), 0);
                };
                let element = match vm.object_type(id).await {
                    Ok((_, type_id)) => self
                        .class_signature(type_id)
                        .await
                        .as_deref()
                        .map(pretty_type)
                        .unwrap_or_else(|| "?[]".to_string()),
                    Err(_) => "?[]".to_string(),
                };
                let length = vm.array_length(id).await.unwrap_or(0);
                // `int[]` + 3 → `int[3]`.
                let display = match element.strip_suffix("[]") {
                    Some(base) => format!("{base}[{length}]"),
                    None => format!("{element}[{length}]"),
                };
                let handle = self.new_handle();
                self.varrefs.insert(handle, VarRef::Array(id));
                (display, handle)
            }
            JValue::Object { id, .. } => {
                let class = match self.vm() {
                    Ok(vm) => match vm.object_type(id).await {
                        Ok((_, type_id)) => self
                            .class_signature(type_id)
                            .await
                            .as_deref()
                            .map(pretty_type)
                            .unwrap_or_else(|| "Object".to_string()),
                        Err(_) => "Object".to_string(),
                    },
                    Err(_) => "Object".to_string(),
                };
                let handle = self.new_handle();
                self.varrefs.insert(handle, VarRef::Object(id));
                (format!("{class} (id={id})"), handle)
            }
        }
    }

    // ------------------------------------------------------------------
    // Exceptions
    // ------------------------------------------------------------------

    async fn exception_info(&mut self, args: Value) -> Result<Value, String> {
        let args: ThreadIdArgs = serde_json::from_value(args)
            .map_err(|e| format!("Bad exceptionInfo arguments: {e}"))?;
        let thread = self.jdwp_thread(args.thread_id)?;
        let (exception, caught) = self
            .last_exception
            .get(&thread)
            .copied()
            .ok_or_else(|| "No exception on this thread".to_string())?;
        let vm = self.vm()?;
        let type_id = vm
            .object_type(exception)
            .await
            .map(|(_, t)| t)
            .map_err(|e| format!("ReferenceType failed: {e}"))?;
        let type_name = self
            .class_signature(type_id)
            .await
            .as_deref()
            .map(pretty_type)
            .unwrap_or_else(|| "Throwable".to_string());
        // `Throwable.detailMessage`, read by field access only (no method
        // invocation) — walk the superclass chain to find it.
        let mut message = None;
        let mut walk = type_id;
        for _ in 0..SUPER_CHAIN_CAP {
            if walk == 0 {
                break;
            }
            if let Ok(fields) = vm.fields(walk).await {
                if let Some(field) = fields.iter().find(|f| f.name == "detailMessage") {
                    if let Ok(values) = vm.object_values(exception, &[field.id]).await {
                        if let Some(JValue::Object {
                            tag: tag::STRING,
                            id,
                        }) = values.first()
                        {
                            if *id != 0 {
                                message = vm.string_value(*id).await.ok();
                            }
                        }
                    }
                    break;
                }
            }
            walk = vm.superclass(walk).await.unwrap_or(0);
        }
        let description = match &message {
            Some(m) => format!("{type_name}: {m}"),
            None => type_name.clone(),
        };
        Ok(json!({
            "exceptionId": type_name,
            "description": description,
            "breakMode": if caught { "always" } else { "unhandled" },
            "details": {
                "message": message,
                "typeName": type_name,
            },
        }))
    }

    // ------------------------------------------------------------------
    // Termination
    // ------------------------------------------------------------------

    /// `VirtualMachine.Exit(1)`; no reply within [`EXIT_GRACE`] → kill the
    /// child (launch mode).
    async fn terminate(&mut self) -> Result<Value, String> {
        if let Some(vm) = self.vm.clone() {
            let exited = tokio::time::timeout(EXIT_GRACE, vm.exit(1)).await;
            if !matches!(exited, Ok(Ok(()))) {
                if let Some(kill) = self.child_kill.take() {
                    let _ = kill.send(());
                }
            }
        } else if let Some(kill) = self.child_kill.take() {
            let _ = kill.send(());
        }
        // Backstop: if the VM accepted Exit but never dies, kill anyway.
        if self.launch_mode && self.child_alive {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(EXIT_GRACE).await;
                let _ = tx.send(Inbound::KillDeadline).await;
            });
        }
        Ok(json!({}))
    }

    async fn disconnect(&mut self, args: Value) -> Result<Value, String> {
        let args: DisconnectArgs = serde_json::from_value(args).unwrap_or_default();
        let terminate_debuggee = args.terminate_debuggee.unwrap_or(self.launch_mode);
        if terminate_debuggee {
            self.terminate().await?;
        } else if let Some(vm) = self.vm.take() {
            // Attach: leave the debuggee running.
            vm.dispose().await.ok();
        }
        Ok(json!({}))
    }

    async fn cleanup(&mut self) {
        // Ask the child-wait task to kill (a no-op if the child already
        // exited) and wait — bounded — for the reap, so no debuggee ever
        // outlives the adapter as an orphan.
        if let Some(kill) = self.child_kill.take() {
            let _ = kill.send(());
        }
        if let Some(reaped) = self.child_reaped.take() {
            let _ = tokio::time::timeout(EXIT_GRACE, reaped).await;
        }
        if let Some(scratch) = self.scratch_dir.take() {
            std::fs::remove_dir_all(scratch).ok();
        }
    }

    // ------------------------------------------------------------------
    // Class metadata caches
    // ------------------------------------------------------------------

    async fn class_signature(&mut self, class_id: u64) -> Option<String> {
        if let Some(meta) = self.classes.get(&class_id) {
            if meta.signature.is_some() {
                return meta.signature.clone();
            }
        }
        let vm = self.vm().ok()?;
        let signature = vm.type_signature(class_id).await.ok()?;
        self.classes.entry(class_id).or_default().signature = Some(signature.clone());
        Some(signature)
    }

    async fn class_source_file(&mut self, class_id: u64) -> Option<String> {
        if let Some(meta) = self.classes.get(&class_id) {
            if let Some(cached) = &meta.source_file {
                return cached.clone();
            }
        }
        let vm = self.vm().ok()?;
        let source_file = vm.source_file(class_id).await.ok();
        self.classes.entry(class_id).or_default().source_file = Some(source_file.clone());
        source_file
    }

    async fn class_methods(&mut self, class_id: u64) -> Option<Vec<crate::jdwp::MethodEntry>> {
        if let Some(meta) = self.classes.get(&class_id) {
            if let Some(methods) = &meta.methods {
                return Some(methods.clone());
            }
        }
        let vm = self.vm().ok()?;
        let methods = vm.methods(class_id).await.ok()?;
        self.classes.entry(class_id).or_default().methods = Some(methods.clone());
        Some(methods)
    }

    /// `None` = absent information (compiled without `-g`) or any failure.
    async fn method_line_table(
        &mut self,
        class_id: u64,
        method_id: u64,
    ) -> Option<crate::jdwp::LineTable> {
        if let Some(meta) = self.classes.get(&class_id) {
            if let Some(cached) = meta.line_tables.get(&method_id) {
                return cached.clone();
            }
        }
        let vm = self.vm().ok()?;
        let table = vm.line_table(class_id, method_id).await.ok();
        // A native/absent-info method reports start == -1: treat as absent.
        let table = table.filter(|t| t.start >= 0);
        self.classes
            .entry(class_id)
            .or_default()
            .line_tables
            .insert(method_id, table.clone());
        table
    }

    /// Probe the session's source roots for the frame's source file:
    /// `<root>/<package dirs>/<SourceFile name>`, first existing wins.
    async fn resolve_source(&mut self, signature: &str, class_id: u64) -> Option<PathBuf> {
        let binary = signature.strip_prefix('L')?.strip_suffix(';')?;
        let outer = binary.split('$').next().unwrap_or(binary);
        let package_dir: PathBuf = match outer.rsplit_once('/') {
            Some((pkg, _)) => PathBuf::from(pkg),
            None => PathBuf::new(),
        };
        let file_name = match self.class_source_file(class_id).await {
            Some(name) => name,
            None => {
                let simple = outer.rsplit('/').next().unwrap_or(outer);
                format!("{simple}.java")
            }
        };
        // Defensive: the debuggee controls SourceFile — never let it path-
        // traverse out of the source roots.
        if file_name.contains('/') || file_name.contains('\\') || file_name.contains("..") {
            return None;
        }
        for root in &self.source_roots {
            let candidate = root.join(&package_dir).join(&file_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

/// The `initialize` response body.
fn capabilities() -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsTerminateRequest": true,
        "supportsExceptionInfoRequest": true,
        "supportTerminateDebuggee": true,
        "exceptionBreakpointFilters": [
            { "filter": "uncaught", "label": "Uncaught Exceptions", "default": true },
            { "filter": "caught", "label": "Caught Exceptions", "default": false },
        ],
    })
}

fn variable_json(name: &str, (value, varref): (String, i64)) -> Value {
    json!({ "name": name, "value": value, "variablesReference": varref })
}

fn breakpoint_json(bp: &LineBp) -> Value {
    let mut out = json!({
        "id": bp.id,
        "verified": bp.verified,
        "line": bp.actual_line.unwrap_or(bp.line),
    });
    if let Some(message) = &bp.message {
        out["message"] = json!(message);
    }
    out
}

/// The source line for a bytecode index: greatest line-table entry whose
/// index is `<=` the frame's.
fn line_for_index(table: &crate::jdwp::LineTable, index: u64) -> Option<u32> {
    table
        .lines
        .iter()
        .filter(|&&(i, _)| i <= index)
        .max_by_key(|&&(i, _)| i)
        .map(|&(_, line)| line)
}

/// `Ljava/lang/String;` → `java.lang.String`, `[I` → `int[]`, etc.
fn pretty_type(signature: &str) -> String {
    let mut dims = 0;
    let mut rest = signature;
    while let Some(inner) = rest.strip_prefix('[') {
        dims += 1;
        rest = inner;
    }
    let base = match rest.as_bytes().first() {
        Some(b'L') => rest
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
            .unwrap_or(rest)
            .replace(['/', '$'], "."),
        Some(b'Z') => "boolean".to_string(),
        Some(b'B') => "byte".to_string(),
        Some(b'C') => "char".to_string(),
        Some(b'S') => "short".to_string(),
        Some(b'I') => "int".to_string(),
        Some(b'J') => "long".to_string(),
        Some(b'F') => "float".to_string(),
        Some(b'D') => "double".to_string(),
        Some(b'V') => "void".to_string(),
        _ => rest.to_string(),
    };
    format!("{base}{}", "[]".repeat(dims))
}

/// The fully-qualified name of the file's top-level class: the `package`
/// declaration (found with a scanner that skips `//`, `/* */`, and string
/// literals) plus the file stem.
fn fqcn_for_source(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.to_string();
    let text = std::fs::read_to_string(path).ok()?;
    match scan_package(&text) {
        Some(pkg) if !pkg.is_empty() => Some(format!("{pkg}.{stem}")),
        _ => Some(stem),
    }
}

/// Find the `package` declaration: the first identifier token outside
/// comments and string literals. Returns `None` for the default package.
fn scan_package(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut first_word: Option<(usize, usize)> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'@' => {
                // Annotation: skip its name (package-info files).
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                    i += 1;
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                first_word = Some((start, i));
                break;
            }
            _ => i += 1,
        }
    }
    let (start, end) = first_word?;
    if &text[start..end] != "package" {
        return None;
    }
    // Read the dotted name up to `;`.
    let rest = &text[end..];
    let semi = rest.find(';')?;
    let name: String = rest[..semi]
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '$')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Read the child's stdout/stderr until the HotSpot
/// `Listening for transport dt_socket at address: <port>` line appears on
/// either stream (10 s deadline enforced by the caller). Returns the port,
/// both streams, their leftover buffered bytes (everything except the
/// suppressed listen line itself), and any complete non-listen lines seen
/// meanwhile as `(category, text)` output to forward.
#[allow(clippy::type_complexity)]
async fn wait_for_listen_port(
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
) -> Result<
    (
        u16,
        tokio::process::ChildStdout,
        Vec<u8>,
        tokio::process::ChildStderr,
        Vec<u8>,
        Vec<(&'static str, String)>,
    ),
    String,
> {
    const PATTERN: &str = "Listening for transport dt_socket at address: ";
    let mut out_buf: Vec<u8> = Vec::new();
    let mut err_buf: Vec<u8> = Vec::new();
    let mut out_chunk = [0u8; 4096];
    let mut err_chunk = [0u8; 4096];
    loop {
        let (n, is_stdout) = tokio::select! {
            r = stdout.read(&mut out_chunk) => (r.map_err(|e| e.to_string())?, true),
            r = stderr.read(&mut err_chunk) => (r.map_err(|e| e.to_string())?, false),
        };
        if n == 0 {
            return Err("output ended before the JDWP listen line".to_string());
        }
        let chunk = if is_stdout { &out_chunk } else { &err_chunk };
        let buf = if is_stdout {
            &mut out_buf
        } else {
            &mut err_buf
        };
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(buf).into_owned();
        if let Some(at) = text.find(PATTERN) {
            let after = &text[at + PATTERN.len()..];
            if let Some(nl) = after.find('\n') {
                let port: u16 = after[..nl]
                    .trim()
                    .parse()
                    .map_err(|_| format!("unparseable JDWP listen line: {}", &after[..nl]))?;
                // Everything before the listen line + after it is program
                // output to preserve; the listen line itself is suppressed.
                let mut kept = text[..at].to_string();
                kept.push_str(&after[nl + 1..]);
                let category = if is_stdout { "stdout" } else { "stderr" };
                let mut early = Vec::new();
                if !kept.is_empty() {
                    early.push((category, kept));
                }
                let other = if is_stdout { err_buf } else { out_buf };
                if !other.is_empty() {
                    let other_cat = if is_stdout { "stderr" } else { "stdout" };
                    early.push((other_cat, String::from_utf8_lossy(&other).into_owned()));
                }
                return Ok((port, stdout, Vec::new(), stderr, Vec::new(), early));
            }
        }
    }
}

/// Forward one child stream as `output` events, line-buffered with an
/// [`OUTPUT_CHUNK_CAP`] cap on any single buffered chunk.
async fn forward_stream(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    initial: Vec<u8>,
    category: &'static str,
    tx: mpsc::Sender<Inbound>,
) {
    let mut pending: Vec<u8> = initial;
    let mut chunk = [0u8; 4096];
    loop {
        // Flush complete lines (and oversized partials).
        loop {
            let newline = pending.iter().position(|&b| b == b'\n');
            let flush_len = match newline {
                Some(at) => at + 1,
                None if pending.len() >= OUTPUT_CHUNK_CAP => pending.len(),
                None => break,
            };
            let line: Vec<u8> = pending.drain(..flush_len).collect();
            let text = String::from_utf8_lossy(&line).into_owned();
            if tx
                .send(Inbound::ChildOut {
                    category,
                    chunk: text,
                })
                .await
                .is_err()
            {
                return;
            }
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
        }
    }
    if !pending.is_empty() {
        let text = String::from_utf8_lossy(&pending).into_owned();
        let _ = tx
            .send(Inbound::ChildOut {
                category,
                chunk: text,
            })
            .await;
    }
}

/// Own the child: report its natural exit, or kill+reap on request (the
/// kill channel firing *or* being dropped both mean "kill now").
async fn child_wait_task(
    mut child: tokio::process::Child,
    kill_rx: oneshot::Receiver<()>,
    reaped_tx: oneshot::Sender<()>,
    tx: mpsc::Sender<Inbound>,
) {
    tokio::select! {
        status = child.wait() => {
            let code = status.ok().and_then(|s| s.code());
            let _ = tx.send(Inbound::ChildExit(code)).await;
        }
        _ = kill_rx => {
            child.start_kill().ok();
            let code = child.wait().await.ok().and_then(|s| s.code());
            let _ = tx.send(Inbound::ChildExit(code)).await;
        }
    }
    let _ = reaped_tx.send(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_package_skips_comments_and_strings() {
        let text = r#"
// package wrong.one;
/* package also.wrong; */
package demo.app;
class Main {}
"#;
        assert_eq!(scan_package(text).as_deref(), Some("demo.app"));
        assert_eq!(scan_package("class Main {}"), None);
        assert_eq!(
            scan_package("/* x */ package a.b ;\nclass C {}").as_deref(),
            Some("a.b")
        );
    }

    #[test]
    fn pretty_type_handles_objects_primitives_and_arrays() {
        assert_eq!(pretty_type("Ljava/lang/String;"), "java.lang.String");
        assert_eq!(pretty_type("Ldemo/Main$Inner;"), "demo.Main.Inner");
        assert_eq!(pretty_type("[I"), "int[]");
        assert_eq!(pretty_type("[[Ljava/util/List;"), "java.util.List[][]");
        assert_eq!(pretty_type("Z"), "boolean");
    }

    #[test]
    fn line_for_index_picks_greatest_entry_at_or_below() {
        let table = crate::jdwp::LineTable {
            start: 0,
            end: 30,
            lines: vec![(0, 4), (5, 5), (12, 7)],
        };
        assert_eq!(line_for_index(&table, 0), Some(4));
        assert_eq!(line_for_index(&table, 6), Some(5));
        assert_eq!(line_for_index(&table, 30), Some(7));
    }
}
