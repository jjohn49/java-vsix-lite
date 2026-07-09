//! M5.4: the one-shot, trust-gated `javac` check command.
//!
//! This is a *separate, sandboxed OS process* (see `docs/THREAT_MODEL.md`'s
//! "optional javac tier" trust boundary) spawned only on **explicit user
//! invocation** — never automatically or in the background — and only after
//! the extension has confirmed the workspace is trusted (this module trusts
//! its caller on that point; the server has no notion of VS Code's Workspace
//! Trust itself, so the gate lives entirely in `editors/vscode`). The
//! constraints that keep it safe:
//!
//! - **`-proc:none` is MANDATORY.** Annotation processors are arbitrary code
//!   execution (they run as compiler plugins with the compiler's own
//!   privileges) — a hostile `pom.xml`/`build.gradle` could declare one and
//!   have it run the instant `javac` is invoked. This flag must never be
//!   removed or made optional.
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
pub(crate) const CHECK_PROJECT_COMMAND: &str = "java-vsix-lite.checkProject";

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
    let home = std::env::var_os("JAVA_HOME")?;
    let candidate = PathBuf::from(home).join("bin").join(exe_name);
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
    } else if let Some(p) = line.find(": warning: ") {
        (p, ": warning: ".len(), DiagnosticSeverity::WARNING)
    } else {
        return None;
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
/// when `javac` gave one (per the brief: "byte-range = the whole reported
/// line ... use the column for the range start if present").
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

/// What one check run needs: where `javac` lives, which files to compile,
/// the dependency jars for `-cp`, and how long to allow before killing it.
pub(crate) struct RunConfig {
    pub javac_path: PathBuf,
    pub source_files: Vec<PathBuf>,
    pub classpath_entries: Vec<PathBuf>,
    pub timeout: Duration,
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
/// server's lifetime — a slow resource-exhaustion DoS driven by repeated
/// explicit invocations. The threat model's "resource exhaustion / DoS"
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
    let scratch = unique_scratch_dir();
    if std::fs::create_dir_all(&scratch).is_err() {
        return RunOutcome::SpawnError("failed to create scratch directory".to_string());
    }
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

fn unique_scratch_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("jvl-javac-{}-{}", std::process::id(), nanos))
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
mod tests {
    use super::*;

    #[test]
    fn error_with_column_caret() {
        let stderr = "Broken.java:8: error: incompatible types: String cannot be converted to int\n        int x = \"hello\";\n                ^\n1 error\n";
        let diags = parse_stderr(stderr);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.path, "Broken.java");
        assert_eq!(d.line, 8);
        assert_eq!(d.severity, DiagnosticSeverity::ERROR);
        assert_eq!(
            d.message,
            "incompatible types: String cannot be converted to int"
        );
        assert_eq!(d.column, Some(16));
        assert_eq!(d.end_character, Some(24)); // "        int x = "hello";".len() in UTF-16 units
    }

    #[test]
    fn warning_is_reported_with_warning_severity() {
        let stderr = "Broken.java:7: warning: [unchecked] unchecked call to add(E) as a member of the raw type List\n        list.add(1);\n                ^\n1 warning\n";
        let diags = parse_stderr(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, DiagnosticSeverity::WARNING);
        assert_eq!(
            diags[0].message,
            "[unchecked] unchecked call to add(E) as a member of the raw type List"
        );
    }

    #[test]
    fn multi_line_message_folds_continuation_lines_in() {
        let stderr = "Broken.java:9: error: cannot find symbol\n        System.out.println(foo);\n                           ^\n  symbol:   variable foo\n  location: class Broken\nNote: Broken.java uses unchecked or unsafe operations.\n1 error\n";
        let diags = parse_stderr(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].message,
            "cannot find symbol\n  symbol:   variable foo\n  location: class Broken"
        );
    }

    #[test]
    fn note_lines_are_ignored_and_do_not_break_parsing() {
        let stderr = "Broken.java:8: error: incompatible types: String cannot be converted to int\n        int x = \"hello\";\n                ^\nNote: Broken.java uses unchecked or unsafe operations.\nNote: Recompile with -Xlint:unchecked for details.\n1 error\n";
        let diags = parse_stderr(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].message,
            "incompatible types: String cannot be converted to int"
        );
    }

    #[test]
    fn path_with_spaces_parses_correctly() {
        let stderr = "My File.java:9: error: cannot find symbol\n        System.out.println(foo);\n                           ^\n  symbol:   variable foo\n  location: class Broken\n1 error\n";
        let diags = parse_stderr(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].path, "My File.java");
        assert_eq!(diags[0].line, 9);
    }

    #[test]
    fn xmaxerrs_truncation_notice_is_skipped_not_a_diagnostic() {
        let stderr = "Many.java:2: error: incompatible types: String cannot be converted to int\n    void m0() { int x0 = \"bad0\"; }\n                         ^\nMany.java:3: error: incompatible types: String cannot be converted to int\n    void m1() { int x1 = \"bad1\"; }\n                         ^\nonly showing the first 2 errors, of 20 total; use -Xmaxerrs if you would like to see more\n";
        let diags = parse_stderr(stderr);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].line, 2);
        assert_eq!(diags[1].line, 3);
    }

    #[test]
    fn malformed_lines_are_skipped_without_panicking() {
        let stderr = "garbage line with no colon-marker\n:::: also garbage ::::\nBroken.java:8: error: incompatible types: String cannot be converted to int\n        int x = \"hello\";\n                ^\ntrailing garbage\n1 error\n";
        let diags = parse_stderr(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 8);
    }

    #[test]
    fn empty_stderr_yields_no_diagnostics() {
        assert_eq!(parse_stderr(""), Vec::new());
    }

    #[test]
    fn group_diagnostics_tags_source_javac_and_groups_by_path() {
        let raw = vec![
            RawDiagnostic {
                path: "A.java".to_string(),
                line: 3,
                severity: DiagnosticSeverity::ERROR,
                message: "boom".to_string(),
                column: Some(4),
                end_character: Some(10),
            },
            RawDiagnostic {
                path: "A.java".to_string(),
                line: 5,
                severity: DiagnosticSeverity::WARNING,
                message: "meh".to_string(),
                column: None,
                end_character: None,
            },
        ];
        let grouped = group_diagnostics(raw);
        assert_eq!(grouped.len(), 1);
        let diags = &grouped["A.java"];
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].source.as_deref(), Some("javac"));
        assert_eq!(
            diags[0].range,
            Range {
                start: Position {
                    line: 2,
                    character: 4
                },
                end: Position {
                    line: 2,
                    character: 10
                }
            }
        );
        assert_eq!(
            diags[1].range.start,
            Position {
                line: 4,
                character: 0
            }
        );
        assert_eq!(diags[1].range.end.character, u32::MAX);
    }

    #[test]
    fn count_severities_counts_errors_and_warnings_separately() {
        let stderr = "A.java:1: error: e1\n^\nA.java:2: warning: w1\n^\nA.java:3: error: e2\n^\n2 errors\n1 warning\n";
        let diags = parse_stderr(stderr);
        assert_eq!(count_severities(&diags), (2, 1));
    }

    #[test]
    fn clamp_timeout_secs_clamps_to_bounds() {
        assert_eq!(clamp_timeout_secs(None), 120);
        assert_eq!(clamp_timeout_secs(Some(1)), 10);
        assert_eq!(clamp_timeout_secs(Some(10_000)), 600);
        assert_eq!(clamp_timeout_secs(Some(30)), 30);
    }

    #[test]
    fn locate_javac_override_wins_and_must_itself_be_valid() {
        let dir =
            std::env::temp_dir().join(format!("jvl-javac-locate-test-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe_name = if cfg!(windows) { "javac.exe" } else { "javac" };
        std::fs::write(bin.join(exe_name), b"").unwrap();

        assert_eq!(locate_javac(Some(&dir)), Some(bin.join(exe_name)));

        // A bogus override must not silently fall back to $JAVA_HOME.
        assert_eq!(locate_javac(Some(Path::new("/nonexistent/jdk/home"))), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn collect_source_files_skips_build_output_and_hidden_dirs() {
        let dir =
            std::env::temp_dir().join(format!("jvl-javac-collect-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("src/Good.java"), "class Good {}").unwrap();
        std::fs::write(dir.join("target/Ignored.java"), "class Ignored {}").unwrap();
        std::fs::write(dir.join(".git/Ignored2.java"), "class Ignored2 {}").unwrap();
        std::fs::write(dir.join(".hidden/Ignored3.java"), "class Ignored3 {}").unwrap();

        let files = collect_source_files(std::slice::from_ref(&dir));
        assert_eq!(files, vec![dir.join("src/Good.java")]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `sh` fixture standing in for `javac`: its *first* statement forks
    /// a background `sleep 10` that inherits (and keeps holding) the
    /// stdout/stderr pipes — the "pipe-holding orphan" a killed child can
    /// leave behind — then sleeps far longer than the configured timeout,
    /// then (only if it ever gets there) touches `marker` — so "the marker
    /// never appears" proves the child was actually killed, not merely that
    /// `run` gave up waiting on it. Backgrounding the pipe-holder first
    /// thing (rather than relying on the foreground `sleep`'s own fork)
    /// keeps the leaked-reader assertions deterministic: the kill only has
    /// to land after the script's very first statement, and the tests give
    /// that a full second of margin. Unix-only (a shell script fixture);
    /// the timeout path itself is exercised here since racing a real
    /// multi-second `javac` invocation in CI would be slow and flaky —
    /// everything else about process hygiene (kill + reap, no zombie) is
    /// covered by inspection in `poll_until_exit_or_timeout` and these
    /// tests' own promptness/marker assertions.
    #[cfg(unix)]
    fn write_slow_fake_javac(dir: &Path, marker: &Path, sleep_secs: u64) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-javac.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 10 &\nsleep {sleep_secs}\ntouch \"{}\"\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn run_kills_and_reaps_on_timeout() {
        let dir =
            std::env::temp_dir().join(format!("jvl-javac-timeout-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran-too-long");
        let script = write_slow_fake_javac(&dir, &marker, 5);

        let config = RunConfig {
            javac_path: script,
            source_files: vec![],
            classpath_entries: vec![],
            timeout: Duration::from_secs(1),
        };
        let slot: SharedChild = Arc::new(StdMutex::new(None));
        let leaked = LeakedReaders::new();
        let start = Instant::now();
        let outcome = run(config, &slot, &leaked);
        let elapsed = start.elapsed();

        assert!(matches!(outcome, RunOutcome::TimedOut));
        assert!(
            elapsed < Duration::from_secs(3),
            "should have been killed well before the fake script's 5s sleep: {elapsed:?}"
        );
        // The backgrounded `sleep 10` orphan still holds both pipes open,
        // so both abandoned reader threads must be accounted for.
        assert_eq!(
            leaked.live_count(),
            2,
            "both abandoned reader threads should be counted as leaked"
        );
        // Give a killed-but-somehow-still-running process a moment, then
        // confirm it never reached the `touch` — i.e. it was truly killed.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !marker.exists(),
            "child should have been killed before reaching the marker touch"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn kill_running_child_cancels_an_in_flight_run() {
        let dir =
            std::env::temp_dir().join(format!("jvl-javac-shutdown-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran-too-long");
        let script = write_slow_fake_javac(&dir, &marker, 5);

        let config = RunConfig {
            javac_path: script,
            source_files: vec![],
            classpath_entries: vec![],
            timeout: Duration::from_secs(30), // long enough that only the kill ends it
        };
        let slot: SharedChild = Arc::new(StdMutex::new(None));
        let run_slot = Arc::clone(&slot);
        let start = Instant::now();
        let handle = std::thread::spawn(move || run(config, &run_slot, &LeakedReaders::new()));

        // Give `run` time to spawn the fake javac and store it in `slot`
        // (it sleeps 5s before touching anything, so this is comfortably
        // before it would exit on its own).
        std::thread::sleep(Duration::from_millis(300));
        kill_running_child(&slot); // simulates server shutdown mid-check.

        let outcome = handle.join().expect("run thread panicked");
        let elapsed = start.elapsed();
        assert!(matches!(outcome, RunOutcome::Cancelled));
        assert!(
            elapsed < Duration::from_secs(3),
            "should have ended promptly on kill, not waited for the 30s timeout: {elapsed:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !marker.exists(),
            "child should have been killed before reaching the marker touch"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaked_readers_sweep_completed_flags() {
        let leaked = LeakedReaders::with_cap(8);
        let still_blocked = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        leaked.register(Arc::clone(&still_blocked));
        leaked.register(Arc::clone(&finished));
        assert_eq!(leaked.live_count(), 2);

        // A leaked thread finally unblocking (its flag set) frees capacity
        // on the next sweep — the refusal is self-healing, not permanent.
        finished.store(true, Ordering::SeqCst);
        assert_eq!(leaked.live_count(), 1);
        still_blocked.store(true, Ordering::SeqCst);
        assert_eq!(leaked.live_count(), 0);
    }

    /// The refusal half of the bounded-leak invariant, deterministically:
    /// with the still-blocked count at a (test-lowered) cap, [`run`] must
    /// refuse outright — before creating the scratch dir or spawning
    /// anything — with a clear, actionable error, so a hostile `jdk.home`
    /// binary can't grow blocked threads without bound. The leak-*counting*
    /// half (a timed-out run registers its abandoned readers) is covered by
    /// `run_kills_and_reaps_on_timeout`'s `live_count` assertion; here the
    /// flags are registered by hand instead of racing real processes, which
    /// keeps the test load-independent.
    #[test]
    fn leaked_reader_cap_refuses_further_runs() {
        let leaked = LeakedReaders::with_cap(2);
        leaked.register(Arc::new(AtomicBool::new(false)));
        leaked.register(Arc::new(AtomicBool::new(false)));

        let slot: SharedChild = Arc::new(StdMutex::new(None));
        let outcome = run(
            RunConfig {
                // Never reached: the refusal must precede any spawn attempt.
                javac_path: PathBuf::from("/nonexistent/javac-never-spawned"),
                source_files: vec![],
                classpath_entries: vec![],
                timeout: Duration::from_millis(200),
            },
            &slot,
            &leaked,
        );
        match outcome {
            RunOutcome::SpawnError(msg) => {
                assert!(
                    msg.contains("leaking resources") && msg.contains("jdk.home"),
                    "refusal message should be clear and actionable: {msg}"
                );
            }
            _ => panic!("expected the run to be refused at the leak cap"),
        }

        // One leaked thread unblocking frees capacity: cap 2 with only 1
        // still blocked runs again (and immediately fails to spawn the
        // nonexistent binary — proving it got *past* the refusal gate).
        let leaked = LeakedReaders::with_cap(2);
        leaked.register(Arc::new(AtomicBool::new(false)));
        let outcome = run(
            RunConfig {
                javac_path: PathBuf::from("/nonexistent/javac-never-spawned"),
                source_files: vec![],
                classpath_entries: vec![],
                timeout: Duration::from_millis(200),
            },
            &slot,
            &leaked,
        );
        match outcome {
            RunOutcome::SpawnError(msg) => {
                assert!(
                    msg.contains("failed to spawn javac"),
                    "below the cap the run must proceed to the (failing) spawn: {msg}"
                );
            }
            _ => panic!("expected a spawn failure, not a refusal, below the cap"),
        }
    }

    /// Fix round 1 (IMPORTANT 2): an entry `std::env::join_paths` can't
    /// represent (it contains the platform's path-list separator) must fail
    /// the run loudly, naming the offending entry — never silently drop
    /// `-cp` and let the user chase misleading cannot-find-symbol
    /// diagnostics.
    #[cfg(unix)]
    #[test]
    fn classpath_entry_with_separator_fails_loud() {
        let config = RunConfig {
            javac_path: PathBuf::from("/nonexistent/javac-never-spawned"),
            source_files: vec![],
            classpath_entries: vec![
                PathBuf::from("/deps/fine.jar"),
                PathBuf::from("/deps/evil:name.jar"),
            ],
            timeout: Duration::from_secs(10),
        };
        let slot: SharedChild = Arc::new(StdMutex::new(None));
        let outcome = run(config, &slot, &LeakedReaders::new());
        match outcome {
            RunOutcome::SpawnError(msg) => {
                assert!(
                    msg.contains("/deps/evil:name.jar"),
                    "the offending entry must be named: {msg}"
                );
                assert!(msg.contains("separator"), "the cause must be stated: {msg}");
            }
            _ => panic!("expected a loud SpawnError for the unjoinable classpath entry"),
        }
    }
}
