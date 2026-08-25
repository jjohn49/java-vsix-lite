# java-vsix-lite

Minimal, low-compute Java language support for VS Code — powered by a pure-Rust language server.

## What it is

java-vsix-lite provides core Java editing features without the overhead of a JVM-based toolchain. The language server (`jvl-server`) is written entirely in Rust and starts instantly. It focuses on the features developers use most, with a security-first design that runs safely in untrusted workspaces.

## Measured footprint

The following local benchmark compares java-vsix-lite with the full language server bundled in Red Hat Java 1.55.0 across five established small Maven projects. Each result is **mean / median / sample standard deviation** from 10 measured fresh-server runs after one unrecorded warm-up.

Across all 50 measured runs per server, the complete workflow averaged **157 ms and 17.9 MiB RSS** for java-vsix-lite versus **9.41 s and 931 MiB RSS** for Red Hat—about **60x faster and 52x lower-memory** for the shared operations tested here.

| Project | Java files | java-vsix-lite total | Red Hat total | Speedup | java-vsix-lite RSS | Red Hat RSS |
|---|---:|---:|---:|---:|---:|---:|
| [Spring PetClinic](https://github.com/spring-projects/spring-petclinic) | 49 | 336 / 329 / 14 ms | 13.61 / 13.61 / 0.16 s | 40.5x | 21.5 / 21.8 / 0.7 MiB | 1436 / 1438 / 17 MiB |
| [Apache Commons CLI](https://github.com/apache/commons-cli) | 87 | 95.8 / 95.8 / 0.5 ms | 8.88 / 8.79 / 0.25 s | 92.7x | 16.8 / 16.9 / 0.4 MiB | 693 / 695 / 31 MiB |
| [Gson core](https://github.com/google/gson) | 210 | 108 / 108 / 1.2 ms | 9.51 / 9.61 / 0.23 s | 87.9x | 17.4 / 17.3 / 0.4 MiB | 864 / 866 / 55 MiB |
| [Joda-Time](https://github.com/JodaOrg/joda-time) | 330 | 128 / 124 / 9.7 ms | 7.20 / 7.11 / 0.41 s | 56.3x | 18.1 / 18.1 / 0.4 MiB | 843 / 848 / 12 MiB |
| [JUnit 4](https://github.com/junit-team/junit4) | 471 | 116 / 114 / 6.7 ms | 7.85 / 7.64 / 0.76 s | 67.4x | 15.9 / 15.7 / 0.6 MiB | 819 / 824 / 17 MiB |

“Total” is a sequential first-use workflow: start a fresh server, wait for readiness, open a synthetic Java file inside the project, receive diagnostics, request first and warm JDK completions, request a project/dependency-aware completion, then request first and warm workspace-symbol results. RSS is the observed resident memory of the language-server process tree after those operations.

<details>
<summary>Detailed first-use operation timings</summary>

All values below are **mean / median / sample standard deviation in milliseconds**.

| Project | Server | Ready | Diagnostics | JDK completion | Project/dependency completion | Workspace symbol |
|---|---|---:|---:|---:|---:|---:|
| PetClinic | java-vsix-lite | 1.6 / 2.2 / 1.0 | 260 / 251 / 14 | 3.7 / 3.6 / 0.4 | 1.0 / 1.0 / 0.1 | 0.14 / 0.12 / 0.04 |
|  | Red Hat | 4356 / 4345 / 71 | 1633 / 1617 / 32 | 6935 / 6936 / 117 | 52.7 / 43.0 / 22.7 | 312 / 326 / 63 |
| Commons CLI | java-vsix-lite | 0.37 / 0.36 / 0.04 | 15.7 / 15.7 / 0.5 | 5.3 / 5.3 / 0.1 | 3.7 / 3.7 / 0.2 | 0.17 / 0.16 / 0.04 |
|  | Red Hat | 4431 / 4427 / 48 | 1431 / 1424 / 19 | 2784 / 2659 / 232 | 29.0 / 27.7 / 7.3 | 64 / 58 / 19 |
| Gson | java-vsix-lite | 0.36 / 0.34 / 0.03 | 14.7 / 14.5 / 0.8 | 11.7 / 11.6 / 0.4 | 10.5 / 10.3 / 0.4 | 0.27 / 0.24 / 0.08 |
|  | Red Hat | 3949 / 3944 / 55 | 1699 / 1715 / 63 | 375 / 398 / 68 | 132 / 123 / 29 | 3208 / 3211 / 115 |
| Joda-Time | java-vsix-lite | 0.59 / 0.39 / 0.58 | 16.7 / 14.5 / 6.1 | 18.2 / 17.9 / 1.2 | 20.8 / 20.3 / 1.3 | 0.31 / 0.31 / 0.03 |
|  | Red Hat | 3511 / 3458 / 148 | 1535 / 1531 / 21 | 370 / 364 / 33 | 221 / 220 / 15 | 1402 / 1339 / 249 |
| JUnit 4 | java-vsix-lite | 0.80 / 0.38 / 0.89 | 16.7 / 15.0 / 5.2 | 24.6 / 24.7 / 1.0 | 0.38 / 0.34 / 0.12 | 0.34 / 0.33 / 0.04 |
|  | Red Hat | 3886 / 3691 / 443 | 1641 / 1596 / 139 | 306 / 251 / 97 | 29.1 / 20.2 / 25.0 | 1842 / 1766 / 217 |

</details>

### Benchmark method and limits

- Tested on an 8-core Apple M1 Pro MacBook Pro with 16 GB RAM and macOS 26.5.2. The java-vsix-lite server was a release build at commit `a358c663`; the comparison used Red Hat Java 1.55.0 for Apple Silicon, including its embedded JRE and standard 100 MiB initial / 2 GiB maximum heap settings.
- Dependencies were prefetched once with Maven's `dependency:go-offline` **before** timing. Every measured Red Hat run used Maven offline mode with Gradle import disabled, and java-vsix-lite resolved artifacts already in the local Maven cache. No measured run downloaded dependencies.
- Each measurement launched a new server. Red Hat also received a new JDT workspace/index for every run. Filesystem, OS, and artifact caches remained warm; run order alternated between the two servers.
- The project-aware request resolved Spring's `ApplicationContext`, Hamcrest's `Matcher`, Commons CLI's `Options`, Gson's `Gson`, or Joda-Time's `DateTime`, depending on the project. This ensures the test goes beyond completing the initialization handshake. Different completion-item counts are not treated as quality scores.
- The harness invoked the language-server backends directly. It excludes the common VS Code extension-host process, Red Hat's temporary secondary syntax server, and java-vsix-lite's separate VS Code-side `javac` validation. Red Hat's documented default **Hybrid** mode starts its syntax server while the full server warms up; see [Red Hat Java launch modes](https://github.com/redhat-developer/vscode-java/blob/main/README.md#settings).
- “Ready” is not equivalent work: java-vsix-lite can acknowledge initialization without building a full workspace index, whereas Red Hat waits for richer project import. The end-to-end workflow is the more useful comparison.

These numbers demonstrate substantially lower fresh-start, resident-memory, and basic-editing overhead for java-vsix-lite's supported workflow. They do **not** claim feature equivalence: Red Hat's Eclipse JDT engine maintains a richer incremental project model and provides broader refactoring, code-action, build-integration, semantic-analysis, and ecosystem support.

### Installed size and optional javac cost

| Measurement | java-vsix-lite | Red Hat Java / JDT LS |
|---|---:|---:|
| Installed language-server/client payload | approximately 4.1 MiB | 176 MiB |

The payload row compares java-vsix-lite's 3.73 MiB native server plus 365 KiB minified client bundle (package metadata and licenses add a small amount) with the installed 176 MiB platform-specific Red Hat extension previously inspected locally, of which approximately 118 MiB was its embedded JRE and 53 MiB was its JDT server.

The automatic `javac` check is a separate, transient cost: compiling the repository's one-file test fixture took 0.29 s and reached 87 MiB maximum RSS, after which the JVM exited and released that memory. Real cost scales with the project because the check runs across its Java sources. It runs after trusted-project load and Java saves by default; set `java-vsix-lite.javac.checkOnSave` to `false` to disable it.

## Features

- **Syntax highlighting and semantic tokens** — accurate Java token colouring driven by the Rust parser
- **Diagnostics** — immediate parse, incompatible-return/initializer, unreachable-statement, unused-code, and conservative unresolved-member feedback without waiting for `javac`; compiler diagnostics remain the automatic trusted-workspace backstop
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
- **Check Project (javac)** — a workspace-trust-gated `javac` check (annotation processing disabled) reporting real compiler errors in the Problems panel — no resident JVM. The manual command checks the **whole workspace**; the automatic on-save/on-load check is **module-scoped** — it compiles only the Maven/Gradle module(s) owning the saved file(s) (sibling modules still resolve via `-sourcepath`), so unrelated modules aren't recompiled on every save. Debounced and silent; disable with `java-vsix-lite.javac.checkOnSave`
- **Rebuild Classpath (Refresh IntelliSense)** — re-reads build files and local dependency caches and rebuilds the classpath without restarting the server (the light counterpart to **Restart Language Server**). Offline and side-effect-free, so it works even in an untrusted workspace

## Configuration

Everything works out of the box: the JDK, Maven, Gradle, and the server binary are **auto-detected**, and each one has a **manual override** in your settings for when detection guesses wrong or you want a specific toolchain. Nothing here is required for normal use.

### Paths & tools (auto-detected, overridable)

| Setting | Overrides | Default behavior |
|---|---|---|
| `java-vsix-lite.jdk.home` | JDK used for the `javac` check | Auto-detected across `$JAVA_HOME`, system + per-user macOS JVM locations, Linux `/usr/lib/jvm` and `/usr/java`, JetBrains `~/.jdks`, and SDKMAN — ranked by the JDK's real feature version (from its `release` file), so the newest installed JDK wins |
| `java-vsix-lite.maven.path` | `mvn` executable for **Install Dependencies** | Project `mvnw` wrapper → `mvn` on `PATH` → common install dirs (Homebrew, SDKMAN, `/usr/local/bin`, …) |
| `java-vsix-lite.gradle.path` | `gradle` executable for **Install Dependencies** | Project `gradlew` wrapper → `gradle` on `PATH` → common install dirs |
| `java-vsix-lite.server.path` | the `jvl-server` binary | The binary bundled in the extension (the `JVL_SERVER_PATH` environment variable also overrides this) |
| `java-vsix-lite.dependencies.repository` | Maven repository for downloads | Maven Central (set this to an internal Artifactory/Nexus proxy on a governed network) |

### Behavior

| Setting | Default | Purpose |
|---|---|---|
| `java-vsix-lite.javac.checkOnSave` | `true` | Run a **module-scoped** `javac` check automatically on project load and after saving a Java file — only the module(s) owning the saved file(s) are compiled (trusted workspaces only; debounced and silent). The manual **Check Project (javac)** command stays full-workspace. Set to `false` to make the check on-demand only |
| `java-vsix-lite.diagnostics.unresolvedMembers` | `true` | Report an error for a member access when the receiver's full type hierarchy resolves but declares no such member. Conservative — stays silent whenever resolution is incomplete |
| `java-vsix-lite.diagnostics.unused` | `true` | Fade and warn on provably unused locals, eligible parameters, and unreferenced private fields/methods. Disable to hide unused-code warnings |
| `java-vsix-lite.javac.timeoutSecs` | `120` | How long the `javac` check waits before timing out (clamped to 10–600) |
| `java-vsix-lite.dependencies.autoDownload` | `prompt` | What to do when missing dependencies are detected in a trusted workspace: `prompt` (ask once per session), `always` (download silently), or `never` (disable the automatic check; the manual command still works) |
| `java-vsix-lite.trace.server` | `off` | Trace the JSON-RPC traffic between VS Code and the server (for debugging) |

### Why the path settings are machine-scoped

`jdk.home`, `maven.path`, `gradle.path`, `server.path`, and `dependencies.repository` are **machine-scoped** and cannot be set by a workspace — this is deliberate security, not a limitation. A cloned repository must never be able to redirect which `javac`, `mvn`, `gradle`, or server binary is launched on your machine (otherwise a malicious repo could point them at a binary it ships), nor where your dependency downloads come from. These overrides therefore take effect only from your user/machine settings, and they are all listed in `restrictedConfigurations` so an untrusted workspace can't set them at all.

## Security posture

- The pure-Rust default tier never executes project code and runs in **untrusted workspaces**.
- The optional `javac` tier, build commands, and dependency download remain disabled until the workspace is trusted.
- The extension installs without a JVM and makes no outbound network requests without your consent — dependency download is the one exception, and only after you accept the prompt or run the command (see Features above); everything downloaded is HTTPS + checksum-verified and never executed.

## Untrusted workspace support

This extension declares `untrustedWorkspaces.supported: "limited"`. Core features (highlighting, semantic tokens, diagnostics, outline, folding, hover, completion) work in untrusted workspaces. Features that require workspace trust (javac tier, build integration, dependency download) stay disabled until trust is granted.

## Supported platforms

Platform-specific builds are published for:

- Linux x64 and arm64
- Linux (Alpine/musl) x64
- macOS x64 (Intel) and arm64 (Apple Silicon)
- Windows x64

Windows arm64 and Alpine arm64 are not currently built. The correct native server for your platform is bundled in the VSIX you install — no separate download or toolchain is needed at install time.

## Using alongside other Java extensions

java-vsix-lite is a standalone language server, not a companion to the Red Hat Java (Eclipse JDT) extension. Running both at once means two sets of diagnostics, completions, and hovers for the same files. For the lightweight experience this extension is designed for, **disable Red Hat Java (`redhat.java`) for the workspace** (Extensions view → Red Hat Java → Disable (Workspace)) so providers don't compete. Use Red Hat Java instead when you need its deeper, project-model-based refactoring and analysis.

## Building from source

Requires the Rust toolchain (to build `jvl-server`) and **Node.js 20 or newer** (the packaging tool `@vscode/vsce` needs Node 20+; it fails under Node 18). From `editors/vscode`, `npm run package:vsix` builds the release server, copies it into the bundled `server/` directory, and produces a `.vsix` — failing loudly if the server binary is missing rather than shipping a serverless package.

## License

MIT — see the [LICENSE](LICENSE) file.
