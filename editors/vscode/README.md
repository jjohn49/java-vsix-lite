# java-vsix-lite

Minimal, low-compute Java language support for VS Code — powered by a pure-Rust language server.

## What it is

java-vsix-lite provides core Java editing features without the overhead of a JVM-based toolchain. The language server (`jvl-server`) is written entirely in Rust and starts instantly. It focuses on the features developers use most, with a security-first design that runs safely in untrusted workspaces.

## Measured footprint

The following is a local microbenchmark on an Apple Silicon Mac, using a release build and a clean temporary single-file Java workspace. Times are the median of three fresh language-server launches; memory is one resident-set sample taken after completion had finished.

| Measurement | java-vsix-lite | Red Hat Java / JDT LS |
|---|---:|---:|
| Server initialization | 2.4 ms | 2.65 s |
| First diagnostics after opening the file | 14 ms | 436 ms |
| First completion | 0.65 ms | 3.01 s |
| Repeated warm completion | 0.49 ms | 33 ms |
| Idle resident memory after completion | 15 MB | 685 MB |
| Installed language-server/client payload | approximately 4.1 MiB | 176 MiB |

The comparison used the full JDT server bundled with locally installed Red Hat Java 1.54.0, launched with that extension's 100 MiB initial / 2 GiB maximum heap settings. It excludes the common VS Code extension-host process and Red Hat's temporary secondary syntax server; Red Hat's documented default **Hybrid** mode starts that syntax server while the full server is warming up. See [Red Hat Java launch modes](https://github.com/redhat-developer/vscode-java/blob/main/README.md#settings).

The payload row compares java-vsix-lite's 3.73 MiB native server plus 365 KiB minified client bundle (package metadata and licenses add a small amount) with the installed 176 MiB platform-specific Red Hat extension, of which approximately 118 MiB is its embedded JRE and 53 MiB is its JDT server.

This is evidence for substantially lower cold-start, idle-memory, and basic-editing overhead—not a claim that java-vsix-lite wins every workload. Red Hat's Eclipse JDT engine maintains a richer incremental project model and may perform better, and provide more complete results, for large-project semantic operations and refactoring.

The automatic `javac` check is a separate, transient cost: compiling the repository's one-file test fixture took 0.29 s and reached 87 MiB maximum RSS, after which the JVM exited and released that memory. Real cost scales with the project because the check runs across its Java sources. It runs after trusted-project load and Java saves by default; set `java-vsix-lite.javac.checkOnSave` to `false` to disable it.

## Features

- **Syntax highlighting and semantic tokens** — accurate Java token colouring driven by the Rust parser
- **Diagnostics** — parse errors inline, plus unresolved-member errors (`obj.noSuchMethod()`) when the receiver's full type hierarchy resolves — conservative by design, on by default
- **Completion** — locals, members, chains (`list.stream().filter(...)`), and classpath/project type names with auto-import, including JDK and dependency signatures with generic types rendered (`V get(Object key)` on a `Map<K, V>`) — for project types too, whether or not their file is open
- **Lombok awareness** — `@Getter`/`@Setter`/`@Data`/`@Value`/`@With`/`@Builder` members are synthesized for completion, chains (`Person.builder().name(…).build()`), hover, and the unresolved-member check — no annotation processor run, and only in files that actually import `lombok.*`
- **Hover with Javadoc** — signatures and attached Javadoc for project symbols, JDK types (from `src.zip`), and dependencies (from `-sources.jar`)
- **Go to definition / type definition** — into project files (open or not) and into JDK/dependency sources shown as read-only virtual documents
- **Find references** — bounded, confirm-by-resolution workspace search that never reports a match it can't verify
- **Rename** — conservative by design: refuses (with the reason) rather than producing a partial or wrong edit; renames the file along with a public type
- **Go to implementation** — from an interface or abstract method to its implementors
- **Call & type hierarchy** — incoming/outgoing calls for a method (Peek Call Hierarchy) and supertype/subtype trees for a class or interface, powered by the same bounded confirm-by-resolution scans as Find References
- **Workspace symbols** — jump to any top-level type by name (lazy, bounded index; nothing scans until you ask)
- **Signature help** — parameter hints with overloads and active-parameter highlighting
- **Document outline, folding, and selection ranges**
- **Maven & Gradle awareness** — dependencies resolved statically and offline from `pom.xml` / `build.gradle` / version catalogs, including transitive dependencies, from your local `~/.m2` and `~/.gradle` caches; re-resolved automatically when build files change. Build scripts are **never executed**
- **Missing dependency download** — a declared dependency not yet in `~/.m2` can be fetched from Maven Central — or from an internal Maven proxy via the machine-scoped `java-vsix-lite.dependencies.repository` setting (governed networks): run **Java: Download Missing Dependencies**, or accept the one-time prompt shown after opening a project with unresolved dependencies (see `java-vsix-lite.dependencies.autoDownload`). Workspace-trust-gated; HTTPS with checksum verification; nothing downloaded is ever executed
- **Code actions** — add-import quick fixes for unresolved type names (lightbulb on the name) and **Organize Imports** (sorts, dedupes, drops unused — conservatively: a name referenced only in Javadoc keeps its import)
- **Refactoring & code generation** — extract a selected expression to a local variable or a `private static final` constant; generate getters/setters, an all-fields constructor, `equals()`/`hashCode()`, and `toString()` from the Source Action menu
- **Check Project (javac)** — a workspace-trust-gated `javac` check (annotation processing disabled) reporting real compiler errors in the Problems panel — no resident JVM. Runs on demand via the command, and automatically on project load and after saving a Java file (debounced and silent; disable with `java-vsix-lite.javac.checkOnSave`)

## Security posture

- The pure-Rust default tier never executes project code and runs in **untrusted workspaces**.
- The optional `javac` tier, build commands, and dependency download remain disabled until the workspace is trusted.
- The extension installs without a JVM and makes no outbound network requests without your consent — dependency download is the one exception, and only after you accept the prompt or run the command (see Features above); everything downloaded is HTTPS + checksum-verified and never executed.

## Untrusted workspace support

This extension declares `untrustedWorkspaces.supported: "limited"`. Core features (highlighting, semantic tokens, diagnostics, outline, folding, hover, completion) work in untrusted workspaces. Features that require workspace trust (javac tier, build integration, dependency download) stay disabled until trust is granted.

## License

MIT OR Apache-2.0 — see the [LICENSE](LICENSE) file.
