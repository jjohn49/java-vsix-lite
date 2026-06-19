# java-vsix-lite — Threat model & security controls

> Security is a hard requirement, not a feature (see `README.md`). This document
> tracks the attack classes the extension must resist, the control that defends
> against each, and where it is verified. It is updated as each milestone lands.

## Trust boundaries

1. **Untrusted project content** — source, `pom.xml`, `build.gradle(.kts)`, and
   especially **JARs/`.class` bytecode** in the dependency graph and local Maven
   repo. Treated as hostile input at all times.
2. **Build tooling** (`mvn`, `gradle`/`gradlew`) — arbitrary code execution by
   design. Only invoked under Workspace Trust + explicit, per-action consent.
3. **The user's JDK** — discovered, never downloaded (decision: user-provided).
4. **The optional javac tier** — a separate, sandboxed OS process; a crash there
   must not affect the editor or the default tier.

## Attack classes and controls

| Class | Control | Status / where verified |
|---|---|---|
| **Untrusted build-script execution** | Never run build scripts implicitly. Honor VS Code Workspace Trust; the extension declares `untrustedWorkspaces: limited`. Gradle classpath + "check build" are explicit, consented, one-shot. | Manifest in place (M0). Enforced M6. TS trust tests (M6/M8). |
| **XXE / XML entity expansion** (malicious `pom.xml`) | Parse with `roxmltree` (no DTD / external-entity resolution) + size/expansion caps. No network resolver. | M4. Malicious-POM corpus in `fixtures/`. |
| **Zip-slip / path traversal** (malicious JAR) | Read entries by name only; reject `..` and absolute paths; never extract to disk implicitly. | M4. Zip-slip corpus. |
| **Zip-bomb / decompression bomb** | Cap per-entry and total decompressed bytes; bounded, streaming reads. | M4. Zip-bomb corpus. |
| **Malformed `.class` parsing** | `cafebabe` (pure Rust, memory-safe); wrapper is `#![forbid(unsafe_code)]`; fuzzed. | M4 / fuzz target M8. |
| **Command injection** (crafted project/path names) | Spawn tools via `argv` arrays — never a shell string; canonicalize tool paths; refuse paths outside the workspace. | M6. |
| **Supply chain (Rust)** | Committed `Cargo.lock`; `cargo-deny` (advisories/licenses/bans/sources). No build-time downloads. | M0 (gate live). |
| **Supply chain (npm)** | Committed `package-lock.json`; `npm ci`; `npm audit` gate. | M0 (gate live). |
| **Supply chain (Java tier jar)** | Vendored, pinned, **checksum-verified** prebuilt jar; provenance recorded; **no runtime download/exec**. | M7. |
| **Memory safety** | Core analysis in Rust; no `unsafe` JVM embedding (JNI/Panama rejected). | Architectural. `#![forbid(unsafe_code)]` per crate. |
| **Resource exhaustion / DoS** | Debounced, bounded, cancellable work; open-files-only; `-Xmx` + OS rlimits on the Java tier; crash isolation + restart backoff. | M1+ (defaults), M7 (tier limits). |
| **Least privilege / telemetry** | No telemetry; no network calls without explicit opt-in. | Ongoing; CI tests assert no network/process-spawn on file open (M8). |

## Verification strategy

- **Per-crate unit tests** including dedicated malicious-input fixtures.
- **LSP integration tests** driving the real `jvl-server` binary over stdio
  (the M0 `lifecycle_smoke` test is the first of these).
- **Fuzzing** (`cargo-fuzz`) of the `.class`/POM parsers (M8).
- **Negative tests**: assert no network and no child process is spawned when a
  file is merely opened; assert the javac tier and build commands stay disabled
  without trust + consent.

## Open items (tracked as milestones land)

- M4: finalize cache poisoning resistance (key by canonical path + size + mtime).
- M6: Gradle one-shot sandbox specifics per OS.
- M7: per-OS process sandboxing (`setrlimit` on Linux, Job Objects on Windows).
