# java-vsix-lite

A **minimal, low-compute Java language extension for VS Code**. The goal is a fast,
lightweight alternative to the heavyweight Eclipse JDT–based extensions (e.g. the
"Language Support for Java by Red Hat" / `vscode-java` stack, backed by the Eclipse
JDT Language Server), trading exhaustive features for a small footprint and low
CPU/RAM usage.

> Status: **active implementation**. Current user-facing features, security behavior,
> and benchmark methodology are documented in the
> [VS Code extension README](editors/vscode/README.md).

## Measured footprint

On an Apple Silicon Mac, a release-build benchmark across five established small Maven projects (Spring PetClinic, Apache Commons CLI, Gson, Joda-Time, JUnit 4) compared a first-use workflow — fresh server start, readiness, diagnostics, JDK/project completions, and workspace symbols — against the full JDT server in Red Hat Java 1.55.0.

Across all measured runs the workflow averaged **≈157 ms and ≈17.9 MiB RSS** for java-vsix-lite versus **≈9.4 s and ≈931 MiB RSS** for Red Hat — roughly **60× faster and 52× lower-memory** for the shared operations tested. The installed payload is **≈4.1 MiB** versus **176 MiB**.

These figures demonstrate substantially lower cold-start, idle-memory, and basic-editing overhead; they do **not** claim feature equivalence — Red Hat's Eclipse JDT engine provides broader refactoring and large-project semantic analysis. See the [full per-project results, methodology, and limits](editors/vscode/README.md#measured-footprint) in the extension README.

## Goals

- **Syntax highlighting** for Java source.
- **Basic linting** — surface obvious errors/warnings without a full type-checking
  compiler running constantly.
- **Basic IntelliSense** — completion, hover, go-to-definition at a "good enough"
  level, including **signature-level IntelliSense for imported modules** (types,
  methods, fields, and signatures from dependency/JDK JARs).
- **Compute-minded by default** — avoid eagerly indexing the entire project. Prefer
  lazy / on-demand analysis; where closed project files must be consulted (references,
  rename, workspace symbols, cross-file completion), the scan is lazy, bounded,
  cancellable, and never executes project code.
- **Maven & Gradle awareness** — detect the build system, surface dependencies, and
  provide a way to *pull and check that builds work*, while being mindful of compute.

## Hard constraints

- **Low compute is a feature, not a nice-to-have.** Every feature must justify its
  CPU/RAM cost. Background work must be debounced, bounded, and cancellable.
- **Security is paramount. The extension must not be vulnerable to any known attack
  class.** See the Security threat model section.

## Architecture (decided)

A **two-tier hybrid** with a thin TypeScript shell. The driving principle: **get
Java's "brains" only through process isolation — never by embedding a JVM in-process.**

### Components

- **Extension shell — TypeScript** (mandatory; the VS Code extension host is JS-only).
  A thin LSP client + command surface. No analysis logic.
- **Default tier — pure Rust, no JVM (always on):**
  - **Parsing / syntax highlighting:** `tree-sitter-java` (incremental, error-tolerant).
  - **Structure & basic IntelliSense:** outline, document/workspace symbols, basic
    lint (syntax/structural rules), and intra-file + open-files completion, hover,
    and go-to-definition.
  - **Imported-module IntelliSense (signature level):** read `.class` bytecode from
    dependency and JDK JARs with **`cafebabe`** (pure Rust, parses to the Java 21
    class-file spec). This yields types/methods/fields/generic signatures for
    imported symbols **without running a JVM or executing any project code.**
  - **Memory-safe:** Rust eliminates buffer-overflow / use-after-free attack classes
    when parsing untrusted source, bytecode, and project files.
- **Escalation tier — optional Java subprocess (opt-in):**
  - For `javac`-grade inference that `cafebabe` cannot do (generics inference,
    overload resolution, `var` / flow typing), spawn a **separate Java language
    server as an LSP subprocess over stdio** (the `javac`-based
    `georgewfraser/java-language-server` model — *not* Eclipse JDT).
  - Gated behind **Workspace Trust + explicit user opt-in**, launched with `-Xmx`
    caps and OS process limits, **killable and restartable**. The JVM cost is paid
    only when a user explicitly wants deep semantics. Crash/OOM in the subprocess
    cannot take down the editor.

### Explicitly rejected approaches (with rationale)

- **Embedding a JVM in the Rust process via JNI (`jni-rs`, `j4rs`, `jni-utils`).**
  - JNI does **not** reduce the JVM footprint — `java` is itself just a launcher
    calling `JNI_CreateJavaVM()`; you pay the full JVM cost either way, and
    in-process is often worse (JVM heap/metaspace/GC threads live in the editor
    process for the whole session).
  - **No crash isolation:** a malformed/malicious `.class` crashing `javac` takes
    down the whole backend, and per the JNI spec the VM cannot be unloaded or
    recreated in-process — no recovery without restarting the process.
  - **Surrenders Rust's safety:** large `unsafe` FFI surface; `AttachCurrentThread`
    is expensive and daemon-thread attachment is flagged unsafe by `jni-rs` itself.
  - **Harder distribution** (locating/linking `libjvm` per platform/arch).
  - `j4rs` additionally auto-downloads Maven artifacts with **no checksum/signature
    verification** — a supply-chain attack surface. `jni-utils` is ~3 years stale,
    pinned to obsolete `jni 0.19`, and ships a Gradle-invoking build step.
  - JNI's only real benefit (low call latency) is irrelevant at LSP request cadence.
- **Project Panama / Foreign Function & Memory API.** Wrong direction (Java→native)
  and still requires a JVM. Not applicable to a Rust-anchored core. May be revisited
  *only if* a future Java-host accelerator path is ever chosen.
- **Eclipse JDT as the engine.** Whole-workspace, always-on incremental compilation
  is exactly the heavyweight behavior this project exists to avoid.

## Maven & Gradle handling

- **Detect** the build system from project files.
- **Classpath resolution, done safely and once, then cached:**
  - **Maven:** statically parse `pom.xml` defensively (no XXE) and locate artifacts
    in the local repository; resolve the classpath without executing the build.
  - **Gradle:** `build.gradle(.kts)` is executable code, so deriving its classpath
    requires the build tool. Do this **only behind Workspace Trust + explicit
    consent**, as a one-shot, sandboxed invocation, with the result cached. **Never
    execute build scripts implicitly or in the background.**
- **"Pull and check builds work"** is an **explicit, user-invoked** command, run
  sandboxed and honoring Workspace Trust — **never** an automatic background build.

## Security threat model

The extension must not be vulnerable to any known attack class. Specifically:

- **Never execute untrusted build scripts.** Honor VS Code Workspace Trust. Prefer
  static parsing / read-only tooling; any build execution is explicit, consented,
  sandboxed, and bounded.
- **Supply chain.** No silent download-and-execute. Any artifact fetching must use
  TLS and verify checksums/signatures; auto-download features (e.g. `j4rs`-style)
  are disabled. Prefer vendored/pinned, locally-present artifacts.
- **Untrusted input parsing.** Robust against malformed/malicious POMs, build files,
  source, and JARs: no XXE in XML, no zip-slip on archive extraction, no path
  traversal, no command injection when shelling out to build tools.
- **Memory safety.** Core analysis in Rust; no `unsafe` JVM embedding in the editor
  process.
- **Least privilege.** No telemetry or network calls the user has not opted into.

## Non-goals (initial)

- Full Eclipse JDT–level semantic analysis and refactoring.
- Whole-project, always-on indexing.
- Running tests / full builds automatically in the background.
- `javac`-grade type inference in the default tier (it lives in the opt-in subprocess).

## Remaining open questions (for the implementation plan)

- LSP framing: single Rust server process speaking LSP, or analysis embedded in the
  TS host with a thin native module? (Leaning: standalone Rust LSP server.)
- Lint rule set for the default tier (which structural/syntactic rules ship first).
- Caching strategy and invalidation for the bytecode symbol index.
- Packaging of the optional Java subprocess (bundled JRE vs. user-provided JDK).
