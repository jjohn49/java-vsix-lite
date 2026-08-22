# java-vsix-lite

Minimal, low-compute Java language support for VS Code — powered by a pure-Rust language server.

## What it is

java-vsix-lite provides core Java editing features without the overhead of a JVM-based toolchain. The language server (`jvl-server`) is written entirely in Rust and starts instantly. It focuses on the features developers use most, with a security-first design that runs safely in untrusted workspaces.

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
- **Workspace symbols** — jump to any top-level type by name (lazy, bounded index; nothing scans until you ask)
- **Signature help** — parameter hints with overloads and active-parameter highlighting
- **Document outline, folding, and selection ranges**
- **Maven & Gradle awareness** — dependencies resolved statically and offline from `pom.xml` / `build.gradle` / version catalogs, including transitive dependencies, from your local `~/.m2` and `~/.gradle` caches; re-resolved automatically when build files change. Build scripts are **never executed**
- **Missing dependency download** — a declared dependency not yet in `~/.m2` can be fetched from Maven Central: run **Java: Download Missing Dependencies**, or accept the one-time prompt shown after opening a project with unresolved dependencies (see `java-vsix-lite.dependencies.autoDownload`). Workspace-trust-gated; HTTPS with checksum verification; nothing downloaded is ever executed
- **Code actions** — add-import quick fixes for unresolved type names (lightbulb on the name) and **Organize Imports** (sorts, dedupes, drops unused — conservatively: a name referenced only in Javadoc keeps its import)
- **Check Project (javac)** — a workspace-trust-gated `javac` check (annotation processing disabled) reporting real compiler errors in the Problems panel — no resident JVM. Runs on demand via the command, and automatically after saving a Java file (debounced and silent; disable with `java-vsix-lite.javac.checkOnSave`)

## Security posture

- The pure-Rust default tier never executes project code and runs in **untrusted workspaces**.
- The optional `javac` tier, build commands, and dependency download remain disabled until the workspace is trusted.
- The extension installs without a JVM and makes no outbound network requests without your consent — dependency download is the one exception, and only after you accept the prompt or run the command (see Features above); everything downloaded is HTTPS + checksum-verified and never executed.

## Untrusted workspace support

This extension declares `untrustedWorkspaces.supported: "limited"`. Core features (highlighting, semantic tokens, diagnostics, outline, folding, hover, completion) work in untrusted workspaces. Features that require workspace trust (javac tier, build integration, dependency download) stay disabled until trust is granted.

## License

MIT OR Apache-2.0 — see the [LICENSE](LICENSE) file.
