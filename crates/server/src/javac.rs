//! The trust-gated `javac` check command.
//!
//! This is a *separate, sandboxed OS process* (see `docs/THREAT_MODEL.md`'s
//! "optional javac tier" trust boundary) spawned only after the extension has
//! confirmed the workspace is trusted (this module trusts its caller on that
//! point; the server has no notion of VS Code's Workspace Trust itself, so
//! the gate lives entirely in `editors/vscode`). In a trusted workspace the
//! extension runs it automatically — once on project load and again,
//! debounced and silent, after every save of a Java file — in addition to
//! the manual `java-vsix-lite.checkProject` command; in an untrusted
//! workspace it never runs, automatically or manually. The constraints that
//! keep it safe:
//!
//! - **`-proc:none` is MANDATORY on every invocation, automatic or
//!   manual.** Annotation processors are arbitrary code execution (they run
//!   as compiler plugins with the compiler's own privileges) — a hostile
//!   `pom.xml`/`build.gradle` could declare one and have it run the instant
//!   `javac` is invoked. This flag must never be removed or made optional.
//! - The JDK is **discovered, never downloaded** (`locate_javac`).
//! - Every argument is passed as an `argv` element to `std::process::Command`
//!   — never through a shell — so a crafted file/dependency name cannot
//!   inject extra flags or commands.
//! - The child is **bounded**: a hard timeout (kills and reaps — no zombie),
//!   and captured stdout/stderr are capped so pathological output can't
//!   exhaust memory. Reader threads abandoned by timed-out runs (a killed
//!   child's pipe-holding orphan can block them indefinitely) are counted
//!   and capped too — see [`LeakedReaders`].
//! - No network access is implied by anything here (javac itself does none
//!   for a plain compile).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// The `workspace/executeCommand` command name the extension invokes.
///
/// This is the server-INTERNAL id and must NOT equal any command the
/// extension contributes in package.json (`java-vsix-lite.checkProject`):
/// `vscode-languageclient` auto-registers a VS Code command for every id
/// advertised in `executeCommandProvider`, so a matching id makes
/// `registerCommand` throw `command '<id>' already exists` during
/// `initializeFeatures` and the whole client fails with "Server
/// initialization failed". Guarded by the
/// `server_commands_do_not_collide_with_extension_commands` lifecycle test.
pub(crate) const CHECK_PROJECT_COMMAND: &str = "jvl.checkProject.run";

/// Hard cap on how many source files a single check run will feed to
/// `javac` — bounds both the argfile size and the compile time. A project
/// this large is pathological for a one-shot IDE check; the run still
/// proceeds over whatever the cap allows rather than refusing outright.
pub(crate) const MAX_SOURCE_FILES: usize = 2_000;

/// Cap on captured stdout/stderr bytes from the child, in bytes (2MB) —
/// guards against a pathological amount of compiler output exhausting
/// memory. Bytes beyond the cap are still drained (never stored) so the
/// child's pipe never fills up and blocks it.
const OUTPUT_CAP: usize = 2 * 1024 * 1024;

/// Default timeout, in seconds, before the child is killed.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Clamp bounds for `javacTimeoutSecs` (`initializationOptions`).
const MIN_TIMEOUT_SECS: u64 = 10;
const MAX_TIMEOUT_SECS: u64 = 600;

/// How often the timeout loop polls the child for exit.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Cap on leaked (un-joined, still-blocked) output-reader threads before
/// further runs are refused — see [`LeakedReaders`]'s doc comment for the
/// security rationale.
const MAX_LEAKED_READERS: usize = 8;

/// Directory names skipped unconditionally while walking for source files —
/// the same convention `workspace_index`'s walk uses (build output and VCS
/// metadata never contain source worth compiling).
const SKIPPED_DIR_NAMES: [&str; 3] = ["target", "build", ".git"];

/// Clamp a raw `javacTimeoutSecs` value (or the default) into
/// `[MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS]`.
pub(crate) fn clamp_timeout_secs(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)
}

/// Locate `javac`, never downloading it. `jdk_home_override` (from the
/// `java-vsix-lite.jdk.home` initialization option) wins outright when
/// given: if it doesn't resolve to a real `javac`, this returns `None`
/// rather than silently falling back to `$JAVA_HOME`, so a user's
/// misconfiguration surfaces instead of being masked. Absent an override,
/// falls back to `$JAVA_HOME/bin/javac`. `None` either way means the caller
/// must report a clear "javac not found" error — never a download.
pub(crate) fn locate_javac(jdk_home_override: Option<&Path>) -> Option<PathBuf> {
    let exe_name = if cfg!(windows) { "javac.exe" } else { "javac" };
    if let Some(home) = jdk_home_override {
        let candidate = home.join("bin").join(exe_name);
        return candidate.is_file().then_some(candidate);
    }
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin").join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // A GUI-launched editor (macOS especially) has no `$JAVA_HOME`
    // in its environment even when a JDK is installed. Fall back to the
    // classpath layer's JDK discovery — the same filesystem probing
    // (`/Library/Java/JavaVirtualMachines`, `/usr/lib/jvm`, `java` on PATH)
    // that already found the jmods powering intellisense; it never spawns a
    // process, and a jmods-bearing JDK always ships `javac`.
    let home = jvl_classpath::best_jdk()?;
    let candidate = home.join("bin").join(exe_name);
    candidate.is_file().then_some(candidate)
}

/// Walk `roots` for `.java` files, skipping `target/`, `build/`, `.git`, any
/// hidden (dot-prefixed) directory, and symlinks entirely (never followed —
/// simplest way to rule out a symlink cycle without extra bookkeeping).
/// Stops as soon as `MAX_SOURCE_FILES` files have been collected.
pub(crate) fn collect_source_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen_roots = std::collections::HashSet::new();
    for root in roots {
        if !seen_roots.insert(root.clone()) {
            continue;
        }
        if out.len() >= MAX_SOURCE_FILES {
            break;
        }
        walk_for_java_files(root, &mut out);
    }
    out
}

fn walk_for_java_files(root: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= MAX_SOURCE_FILES {
            return;
        }
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for dir_entry in read_dir.flatten() {
            if out.len() >= MAX_SOURCE_FILES {
                return;
            }
            let Ok(file_type) = dir_entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue; // never followed — see the doc comment above.
            }
            let name = dir_entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name_str.as_ref()) {
                continue;
            }
            let path = dir_entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && path.extension().is_some_and(|e| e == "java") {
                out.push(path);
            }
        }
    }
}

/// One diagnostic parsed out of `javac`'s stderr — see [`parse_stderr`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawDiagnostic {
    /// The path exactly as `javac` echoed it — whatever we handed it in the
    /// argfile, verbatim, so this is a valid map key back to the same file.
    pub path: String,
    /// 1-based, as reported by `javac`.
    pub line: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// 0-based column of the `^` caret, when `javac` printed one.
    pub column: Option<u32>,
    /// UTF-16 length of the echoed source line, when `javac` printed one —
    /// used as the range's end column (a whole-line range) when known.
    pub end_character: Option<u32>,
}

/// Parse `javac`'s stderr into one [`RawDiagnostic`] per reported
/// error/warning. Format (observed from a real JDK 21 `javac`):
///
/// ```text
/// Foo.java:8: error: incompatible types: String cannot be converted to int
///         int x = "hello";
///                 ^
/// Foo.java:9: error: cannot find symbol
///         System.out.println(foo);
///                            ^
///   symbol:   variable foo
///   location: class Foo
/// Note: Foo.java uses unchecked or unsafe operations.
/// Note: Recompile with -Xlint:unchecked for details.
/// 2 errors
/// ```
///
/// A header line (`path:line: error|warning: message`) starts a diagnostic;
/// an echoed source line + `^` caret line (when present) supply the column
/// and the whole-line end; any further non-blank lines up to the next
/// header/note/summary/`-Xmaxerrs` line are folded into the message
/// (multi-line messages like `symbol:`/`location:` or unchecked-warning
/// detail). `Note:` lines, summary count lines (`N error(s)`/`N
/// warning(s)`), the `-Xmaxerrs` truncation notice, and any other line that
/// isn't part of an open diagnostic are silently skipped — never panics on
/// malformed/unrecognized input.
pub(crate) fn parse_stderr(stderr: &str) -> Vec<RawDiagnostic> {
    let lines: Vec<&str> = stderr.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((path, line_no, severity, msg)) = parse_header(lines[i]) else {
            i += 1;
            continue;
        };
        i += 1;
        let mut message = msg;
        let mut column = None;
        let mut end_character = None;

        if i < lines.len() && is_caret_line(lines[i]) {
            column = Some(caret_column(lines[i]));
            i += 1;
        } else if i + 1 < lines.len()
            && is_caret_line(lines[i + 1])
            && parse_header(lines[i]).is_none()
        {
            end_character = Some(utf16_len(lines[i]));
            column = Some(caret_column(lines[i + 1]));
            i += 2;
        }

        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty()
                || parse_header(l).is_some()
                || is_note_line(l)
                || is_summary_line(l)
                || is_maxerrs_notice(l)
            {
                break;
            }
            message.push('\n');
            message.push_str(l.trim_end());
            i += 1;
        }

        out.push(RawDiagnostic {
            path,
            line: line_no,
            severity,
            message,
            column,
            end_character,
        });
    }
    out
}

/// Parse a `path:line: error|warning: message` header line. Splits on the
/// *last* `:` before the marker (rather than the first `:` in the line) so a
/// Windows drive-letter path (`C:\foo\Bar.java:10: error: ...`) still parses
/// correctly — the line number is always immediately before the marker.
fn parse_header(line: &str) -> Option<(String, u32, DiagnosticSeverity, String)> {
    let (marker_pos, marker_len, severity) = if let Some(p) = line.find(": error: ") {
        (p, ": error: ".len(), DiagnosticSeverity::ERROR)
    } else {
        let p = line.find(": warning: ")?;
        (p, ": warning: ".len(), DiagnosticSeverity::WARNING)
    };
    let before = &line[..marker_pos];
    let colon = before.rfind(':')?;
    let path = &before[..colon];
    let line_no: u32 = before[colon + 1..].parse().ok()?;
    if path.is_empty() {
        return None;
    }
    let message = line[marker_pos + marker_len..].to_string();
    Some((path.to_string(), line_no, severity, message))
}

/// A line consisting solely (after trimming) of one or more `^` characters —
/// `javac`'s column-caret line.
fn is_caret_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '^')
}

/// 0-based column of the first `^` — the leading whitespace width.
fn caret_column(line: &str) -> u32 {
    line.chars().take_while(|c| c.is_whitespace()).count() as u32
}

fn utf16_len(line: &str) -> u32 {
    line.encode_utf16().count() as u32
}

/// `Note: ...` lines (the unchecked-operations / `-Xlint` hint) — not tied
/// to a specific diagnostic, always ignored.
fn is_note_line(line: &str) -> bool {
    line.trim_start().starts_with("Note:")
}

/// The `N error`/`N errors`/`N warning`/`N warnings` summary `javac` prints
/// at the end (each on its own line).
fn is_summary_line(line: &str) -> bool {
    let trimmed = line.trim();
    let Some((count, rest)) = trimmed.split_once(' ') else {
        return false;
    };
    if count.parse::<u64>().is_err() {
        return false;
    }
    matches!(rest, "error" | "errors" | "warning" | "warnings")
}

/// The `-Xmaxerrs` truncation notice (`"only showing the first N errors, of
/// M total; use -Xmaxerrs if you would like to see more"`).
fn is_maxerrs_notice(line: &str) -> bool {
    line.contains("Xmaxerrs")
}

/// Count of `error`/`warning` severities in a parsed batch — for the
/// extension's summary message.
pub(crate) fn count_severities(raw: &[RawDiagnostic]) -> (usize, usize) {
    let errors = raw
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::ERROR)
        .count();
    let warnings = raw
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::WARNING)
        .count();
    (errors, warnings)
}

/// Group parsed diagnostics by file path (as `javac` echoed it, per
/// [`RawDiagnostic::path`]) into LSP `Diagnostic`s tagged `source: "javac"`.
/// Range: the whole reported line, using the caret column for the start
/// when `javac` gave one.
pub(crate) fn group_diagnostics(raw: Vec<RawDiagnostic>) -> HashMap<String, Vec<Diagnostic>> {
    let mut out: HashMap<String, Vec<Diagnostic>> = HashMap::new();
    for d in raw {
        let line = d.line.saturating_sub(1);
        let start_char = d.column.unwrap_or(0);
        let end_char = d.end_character.unwrap_or(u32::MAX);
        let diagnostic = Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: start_char,
                },
                end: Position {
                    line,
                    character: end_char,
                },
            },
            severity: Some(d.severity),
            source: Some("javac".to_string()),
            message: d.message,
            ..Default::default()
        };
        out.entry(d.path).or_default().push(diagnostic);
    }
    out
}

/// The outcome of one `javac` invocation ([`run`]).
pub(crate) enum RunOutcome {
    /// The child exited (any status — a nonzero exit for compile errors is
    /// normal and expected); `stderr` is the captured, capped output.
    Completed { stderr: String },
    /// Killed and reaped after exceeding the timeout.
    TimedOut,
    /// The child was reaped out from under this run by
    /// [`kill_running_child`] (server shutdown mid-check).
    Cancelled,
    /// Couldn't even spawn the child (bad path, permissions, ...).
    SpawnError(String),
}

/// How the throwaway `javac` check sets the Java language level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceLevel {
    /// No level information (JDK version undetectable) — bare compile, exactly
    /// the historical behavior.
    None,
    /// The project's target release is unknown; compile at the running JDK's
    /// own feature version with `-source N -target N --enable-preview`, so
    /// preview syntax valid for that JDK (pattern matching in `switch` on
    /// 17–20, etc.) isn't flagged as an error.
    JdkDefault(u32),
    /// The project's declared release, compiled faithfully with `--release R`
    /// (validated against release R's API surface, matching the real build).
    /// `preview` is set only when R equals the running JDK's own version, since
    /// `--enable-preview` is legal only for the compiler's current release.
    Release { release: u32, preview: bool },
}

/// What one check run needs: where `javac` lives, which files to compile,
/// the dependency jars for `-cp`, and how long to allow before killing it.
pub(crate) struct RunConfig {
    pub javac_path: PathBuf,
    pub source_files: Vec<PathBuf>,
    pub classpath_entries: Vec<PathBuf>,
    pub timeout: Duration,
    /// The language level to compile at — see [`SourceLevel`]. Class files are
    /// discarded, so these flags only steer which diagnostics `javac` emits.
    pub source_level: SourceLevel,
}

/// The `javac` source-level arguments for a [`SourceLevel`]. Factored out so
/// the flag policy is unit-testable without spawning `javac`.
fn source_level_args(level: SourceLevel) -> Vec<String> {
    match level {
        SourceLevel::None => Vec::new(),
        SourceLevel::JdkDefault(n) => vec![
            "-source".to_string(),
            n.to_string(),
            "-target".to_string(),
            n.to_string(),
            "--enable-preview".to_string(),
        ],
        SourceLevel::Release { release, preview } => {
            let mut args = vec!["--release".to_string(), release.to_string()];
            if preview {
                args.push("--enable-preview".to_string());
            }
            args
        }
    }
}

/// Shared slot holding the currently-running `javac` child (if any), so
/// [`kill_running_child`] (server shutdown) can reach in and kill+reap it
/// from a different task than the one polling it in [`run`]. At most one
/// check ever runs at a time (see `Backend::javac_running` in `main.rs`), so
/// there is never more than one child here.
pub(crate) type SharedChild = Arc<StdMutex<Option<Child>>>;

/// Bounds the reader threads deliberately left un-joined by timed-out/
/// cancelled runs (see [`run`]'s exit-path handling: joining after a kill
/// can block on a pipe-holding orphan outside our control).
///
/// **Security rationale**: per-run, leaving those threads un-joined is
/// correct — but a misbehaving or outright hostile binary at a
/// misconfigured `java-vsix-lite.jdk.home` could fork a long-lived,
/// pipe-holding orphan on *every* invocation, leaking one blocked thread
/// (well, two: stdout + stderr) per `checkProject` run, unbounded for the
/// server's lifetime — a slow resource-exhaustion DoS made worse by the
/// automatic on-load/on-save runs in a trusted workspace, on top of manual
/// invocations. The threat model's "resource exhaustion / DoS"
/// row requires all work to be bounded, so: every leaked reader gets a
/// completion flag it sets when its pipe finally closes; [`Self::live_count`]
/// sweeps completed ones out; and when the still-blocked count reaches the
/// cap, [`run`] refuses to start another `javac` at all (with an error
/// telling the user to check `jdk.home` and that a server restart resets
/// the state). Threads unblocking naturally (the orphan exiting) free
/// capacity again — the refusal is self-healing, not permanent.
#[derive(Clone)]
pub(crate) struct LeakedReaders {
    cap: usize,
    /// One completion flag per leaked reader thread, set by the thread
    /// itself when its `drain_capped` finally returns.
    flags: Arc<StdMutex<Vec<Arc<AtomicBool>>>>,
}

impl LeakedReaders {
    pub(crate) fn new() -> Self {
        Self::with_cap(MAX_LEAKED_READERS)
    }

    /// A lower cap than [`MAX_LEAKED_READERS`], so the refusal behavior can
    /// be exercised in a test with one leaky run instead of several.
    pub(crate) fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            flags: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    /// Track one more leaked reader thread by its completion flag.
    fn register(&self, flag: Arc<AtomicBool>) {
        self.flags
            .lock()
            .expect("leaked readers poisoned")
            .push(flag);
    }

    /// Sweep out threads that have since completed and return how many are
    /// still blocked.
    pub(crate) fn live_count(&self) -> usize {
        let mut flags = self.flags.lock().expect("leaked readers poisoned");
        flags.retain(|f| !f.load(Ordering::SeqCst));
        flags.len()
    }

    /// Whether the still-blocked count has reached the cap — the signal for
    /// [`run`] to refuse starting another `javac`.
    fn at_capacity(&self) -> bool {
        self.live_count() >= self.cap
    }
}

/// A capped pipe-drain thread ([`drain_capped`]) plus the completion flag
/// it sets on the way out — so a run that must abandon it (timeout/cancel)
/// can hand the flag to [`LeakedReaders`] instead of blocking on a join.
struct ReaderThread {
    handle: std::thread::JoinHandle<Vec<u8>>,
    done: Arc<AtomicBool>,
}

fn spawn_reader(stream: impl Read + Send + 'static) -> ReaderThread {
    let done = Arc::new(AtomicBool::new(false));
    let done_in_thread = Arc::clone(&done);
    let handle = std::thread::spawn(move || {
        let bytes = drain_capped(stream);
        done_in_thread.store(true, Ordering::SeqCst);
        bytes
    });
    ReaderThread { handle, done }
}

/// Kill and reap whatever child is currently in `slot`, if any — called on
/// server shutdown so a check in flight never survives as a zombie or an
/// orphan past the server's own lifetime.
pub(crate) fn kill_running_child(slot: &SharedChild) {
    if let Some(mut child) = slot.lock().expect("javac child slot poisoned").take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Run `javac` per `config`, blocking the calling thread until it exits, is
/// killed for the timeout, or is reaped by a concurrent
/// [`kill_running_child`] (shutdown). Must be called on a blocking thread
/// (e.g. `tokio::task::spawn_blocking`) — this does real, potentially
/// long-lived (up to the timeout) blocking I/O.
///
/// Refuses outright (before touching the filesystem or spawning anything)
/// when `leaked`'s still-blocked reader-thread count has reached its cap —
/// see [`LeakedReaders`] for the security rationale.
///
/// A scratch directory under `std::env::temp_dir()` is created for `-d` and
/// the `@argfile`, and removed again before returning (best-effort — a
/// failure to clean up is not itself an error).
pub(crate) fn run(config: RunConfig, slot: &SharedChild, leaked: &LeakedReaders) -> RunOutcome {
    if leaked.at_capacity() {
        return RunOutcome::SpawnError(format!(
            "javac runs are leaking resources ({} output-reader threads still blocked by \
             earlier timed-out runs) — check that java-vsix-lite.jdk.home / $JAVA_HOME points \
             at a real JDK; restart the server to reset",
            leaked.live_count()
        ));
    }
    let Some(scratch) = create_fresh_scratch_dir() else {
        return RunOutcome::SpawnError("failed to create scratch directory".to_string());
    };
    let outcome = run_in_scratch(&config, &scratch, slot, leaked);
    let _ = std::fs::remove_dir_all(&scratch);
    outcome
}

fn run_in_scratch(
    config: &RunConfig,
    scratch: &Path,
    slot: &SharedChild,
    leaked: &LeakedReaders,
) -> RunOutcome {
    let argfile_path = scratch.join("sources.argfile");
    if let Err(err) = write_argfile(&argfile_path, &config.source_files) {
        return RunOutcome::SpawnError(format!("failed to write argfile: {err}"));
    }

    let mut cmd = Command::new(&config.javac_path);
    cmd
        // MANDATORY (see the module doc comment): annotation processors are
        // arbitrary code execution. Never remove or make this optional.
        .arg("-proc:none")
        .arg("-Xmaxerrs")
        .arg("200")
        .arg("-d")
        .arg(scratch);
    // Compile at the running JDK's own source level with preview features
    // enabled, so modern-but-preview syntax isn't reported as an error.
    cmd.args(source_level_args(config.source_level));
    if !config.classpath_entries.is_empty() {
        match std::env::join_paths(&config.classpath_entries) {
            Ok(joined) => {
                cmd.arg("-cp").arg(joined);
            }
            // Fail loud, never degrade silently: dropping `-cp` here would
            // "work" but produce a wall of misleading cannot-find-symbol
            // diagnostics for every dependency type. Name the offending
            // entry (the one that itself fails a single-entry join — i.e.
            // contains the platform's path-list separator) so the user can
            // actually act on it.
            Err(_) => {
                let offender = config
                    .classpath_entries
                    .iter()
                    .find(|p| std::env::join_paths(std::iter::once(*p)).is_err());
                return RunOutcome::SpawnError(match offender {
                    Some(p) => format!(
                        "classpath entry contains the platform path-list separator and cannot \
                         be passed to javac -cp: {}",
                        p.display()
                    ),
                    None => "classpath entries cannot be joined for javac -cp".to_string(),
                });
            }
        }
    }
    cmd.arg(format!("@{}", argfile_path.display()));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => return RunOutcome::SpawnError(format!("failed to spawn javac: {err}")),
    };

    let stdout_reader = child.stdout.take().map(spawn_reader);
    let stderr_reader = child.stderr.take().map(spawn_reader);

    *slot.lock().expect("javac child slot poisoned") = Some(child);

    let start = Instant::now();
    let result = poll_until_exit_or_timeout(slot, start, config.timeout);

    match result {
        // Only join the reader threads on a normal exit. On a timeout/kill,
        // the killed process may have left a grandchild that inherited the
        // pipe's write end still holding it open (our own timeout-path test
        // fixture does exactly this with a forked `sleep`) — joining here
        // would then block on something outside our control, defeating the
        // whole point of the timeout. We already killed+reaped the process
        // we actually spawned; its stderr is irrelevant to a `TimedOut`/
        // `Cancelled` outcome anyway, so the reader threads are abandoned
        // un-joined — but *counted*, via `leaked` (see [`LeakedReaders`]
        // for why an unbounded number of these would be a security problem).
        PollResult::Exited => {
            let stderr_bytes = stderr_reader
                .and_then(|r| r.handle.join().ok())
                .unwrap_or_default();
            // Stdout is drained (never left to fill the pipe and block the
            // child) but not otherwise used.
            let _ = stdout_reader.and_then(|r| r.handle.join().ok());
            RunOutcome::Completed {
                stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
            }
        }
        PollResult::TimedOut | PollResult::Cancelled => {
            for reader in [stdout_reader, stderr_reader].into_iter().flatten() {
                leaked.register(reader.done);
            }
            match result {
                PollResult::TimedOut => RunOutcome::TimedOut,
                _ => RunOutcome::Cancelled,
            }
        }
    }
}

enum PollResult {
    Exited,
    TimedOut,
    Cancelled,
}

fn poll_until_exit_or_timeout(slot: &SharedChild, start: Instant, timeout: Duration) -> PollResult {
    loop {
        {
            let mut guard = slot.lock().expect("javac child slot poisoned");
            match guard.as_mut() {
                None => return PollResult::Cancelled, // reaped by kill_running_child
                Some(child) => match child.try_wait() {
                    Ok(Some(_status)) => return PollResult::Exited,
                    Ok(None) => {}
                    Err(_) => return PollResult::Exited, // can't observe status; stop polling
                },
            }
        }
        if start.elapsed() >= timeout {
            let mut guard = slot.lock().expect("javac child slot poisoned");
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
                let _ = child.wait(); // reap — never leave a zombie behind.
            }
            *guard = None;
            return PollResult::TimedOut;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Read a pipe to completion, keeping only the first [`OUTPUT_CAP`] bytes —
/// the rest is still read (and discarded) so the child's pipe never fills
/// up and blocks it.
fn drain_capped(mut reader: impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < OUTPUT_CAP {
                    let take = (OUTPUT_CAP - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    buf
}

/// Create the `-d` scratch directory with `create_dir` (NOT `create_dir_all`):
/// on a shared world-writable temp dir, a predictable name that already
/// exists (or a pre-planted symlink to a directory) would let another local
/// user redirect javac's class-file output. `create_dir` fails on anything
/// pre-existing — including a symlink — so each attempt is guaranteed fresh;
/// a few retries with a varying suffix absorb benign collisions.
fn create_fresh_scratch_dir() -> Option<PathBuf> {
    for attempt in 0..8u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "jvl-javac-{}-{}-{}",
            std::process::id(),
            nanos,
            attempt
        ));
        if std::fs::create_dir(&dir).is_ok() {
            return Some(dir);
        }
    }
    None
}

/// Write one (double-quoted, backslash/quote-escaped) path per line — a
/// `javac` `@argfile`, used instead of a giant command line to stay well
/// under `ARG_MAX` for large projects.
fn write_argfile(path: &Path, files: &[PathBuf]) -> std::io::Result<()> {
    let mut content = String::new();
    for f in files {
        content.push('"');
        let escaped = f
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        content.push_str(&escaped);
        content.push_str("\"\n");
    }
    std::fs::write(path, content)
}

#[cfg(test)]
#[path = "javac_tests.rs"]
mod tests;
