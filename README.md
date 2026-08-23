# java-vsix-lite

Fast, low-footprint Java language support for VS Code, powered by a pure-Rust language server that starts instantly and stays out of the way.

It targets everyday Java editing — highlighting, completion, navigation, diagnostics — without the memory and CPU cost of a full Eclipse JDT toolchain. It is not a replacement for [Language Support for Java by Red Hat](https://marketplace.visualstudio.com/items?itemName=redhat.java) on large, refactoring-heavy codebases; it trades exhaustive semantic analysis for a small, quick, secure default that covers the majority of day-to-day work.

The full feature list, settings, and security details live in the [VS Code extension README](editors/vscode/README.md).

## Performance

On an Apple Silicon Mac, a release-build benchmark across five established small Maven projects (Spring PetClinic, Apache Commons CLI, Gson, Joda-Time, JUnit 4) compared a first-use workflow — fresh server start, readiness, diagnostics, JDK/project completions, and workspace symbols — against the full JDT server in Red Hat Java 1.55.0.

Across all measured runs the workflow averaged **≈157 ms and ≈17.9 MiB RSS** for java-vsix-lite versus **≈9.4 s and ≈931 MiB RSS** for Red Hat — roughly **60× faster and 52× lower-memory** for the shared operations tested. The installed payload is **≈4.1 MiB** versus **176 MiB**. These figures reflect lower cold-start, idle-memory, and basic-editing overhead; they are not a claim of feature parity. Full per-project results and methodology are in the [extension README](editors/vscode/README.md#measured-footprint).

## How it works

A thin TypeScript client (the VS Code extension host is JavaScript-only) hosts commands and speaks LSP to a native server; all analysis happens in Rust across two tiers.

**Default tier — pure Rust, no JVM. Always on, runs in untrusted workspaces.**

- Incremental, error-tolerant parsing and semantic highlighting via `tree-sitter-java`.
- Structure, diagnostics, completion, hover, go-to-definition, find references, rename, call/type hierarchy, and code actions.
- Signature-level IntelliSense for JDK and dependency symbols by reading `.class` bytecode directly with [`cafebabe`](https://crates.io/crates/cafebabe) — types, methods, fields, and generic signatures, with no JVM and without executing any project code.
- Written in Rust with `#![forbid(unsafe_code)]`, which removes memory-safety attack classes when parsing untrusted source, bytecode, and archives.

**Compiler tier — optional `javac` diagnostics. Trusted workspaces only.**

- Runs the detected JDK's `javac` (annotation processing disabled with `-proc:none`) on project load and after saves, publishing real compiler errors to the Problems panel. There is no resident JVM — each run is a bounded, timeout-guarded, killable subprocess, debounced and silent.
- The JDK is auto-detected (ranked by real feature version) and overridable in settings; the check compiles at the JDK's own source level so preview syntax is not misreported.

## Build tooling

- Maven and Gradle classpaths are resolved **statically and offline** from `pom.xml` / `build.gradle(.kts)` / version catalogs and the local `~/.m2` and `~/.gradle` caches, transitive dependencies included, and re-resolved when build files change. Build scripts are never executed to do this.
- Missing dependencies can be fetched from Maven Central or a configured internal repository (HTTPS with checksum verification, consent-gated), or populated by an explicit **Install Dependencies** command that runs the project's own `mvn`/`gradle` — only in a trusted workspace, with per-run confirmation.

## Security

- **Build scripts are never executed** implicitly or in the background; anything that runs them is explicit, per-run consented, and disabled in untrusted workspaces. VS Code Workspace Trust is honored throughout.
- **No network access without consent.** Dependency download is the only outbound path, is opt-in, and everything fetched is HTTPS + checksum-verified and never executed.
- **`javac` always runs with `-proc:none`**, so annotation processors — arbitrary code — never run.
- **Robust against malformed input**: no XXE in XML, no zip-slip or path traversal on archives, and bounded allocations on untrusted archives and files.
- **Machine-scoped tool paths.** The JDK, `mvn`/`gradle`, server binary, and download repository can only be set from user/machine settings, so a workspace can never redirect which binary is launched on your behalf.

## Repository layout

| Crate / directory | Responsibility |
| --- | --- |
| `crates/syntax` | IO-free analysis over the tree-sitter tree — the default tier's language features |
| `crates/classpath` | Bytecode, archive, JDK, and build-file reading (no JVM) |
| `crates/server` | The `tower-lsp` language-server binary |
| `editors/vscode` | The VS Code extension — a thin TypeScript LSP client |

## Building

Requires the Rust toolchain and Node.js 20 or newer. From `editors/vscode`, `npm run package:vsix` builds the release server, bundles it into the extension, and produces a platform-specific `.vsix` (failing if the native server is missing). See the [extension README](editors/vscode/README.md#building-from-source) for details.

## License

MIT — see the [LICENSE](LICENSE) file.
