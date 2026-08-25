# java-vsix-lite

Minimal, low-compute Java language support for VS Code — powered by a pure-Rust language server.

## What it is

java-vsix-lite provides core Java editing features without the overhead of a JVM-based toolchain. The language server (`jvl-server`) is written entirely in Rust and starts instantly. It focuses on the features developers use most, with a security-first design that runs safely in untrusted workspaces.

## Measured footprint

The following local benchmark compares java-vsix-lite with the full language server bundled in Red Hat Java 1.55.0 across five established small Maven projects. Each result is **mean / median / sample standard deviation** from 10 measured fresh-server runs after one unrecorded warm-up.

Across all 50 measured runs per server, the complete workflow averaged **163 ms and 19.6 MiB RSS** for java-vsix-lite versus **9.56 s and 937 MiB RSS** for Red Hat—about **59x faster and 48x lower-memory** for the shared operations tested here.

| Project | Java files | java-vsix-lite total | Red Hat total | Speedup | java-vsix-lite RSS | Red Hat RSS |
|---|---:|---:|---:|---:|---:|---:|
| [Spring PetClinic](https://github.com/spring-projects/spring-petclinic) | 49 | 348.6 / 357.5 / 21.1 ms | 13.80 / 13.94 / 0.31 s | 39.6x | 26.3 / 26.0 / 1.2 MiB | 1430 / 1432 / 19 MiB |
| [Apache Commons CLI](https://github.com/apache/commons-cli) | 87 | 110.3 / 100.5 / 15.0 ms | 9.27 / 9.40 / 0.28 s | 84.1x | 18.2 / 18.1 / 0.4 MiB | 727 / 726 / 32 MiB |
| [Gson core](https://github.com/google/gson) | 210 | 113.2 / 103.2 / 14.9 ms | 9.97 / 10.00 / 0.23 s | 88.0x | 18.4 / 18.4 / 0.4 MiB | 856 / 856 / 60 MiB |
| [Joda-Time](https://github.com/JodaOrg/joda-time) | 330 | 125.3 / 114.9 / 14.9 ms | 7.31 / 7.40 / 0.14 s | 58.3x | 18.5 / 18.5 / 0.4 MiB | 844 / 842 / 9 MiB |
| [JUnit 4](https://github.com/junit-team/junit4) | 471 | 115.1 / 107.7 / 19.9 ms | 7.43 / 7.40 / 0.11 s | 64.5x | 16.4 / 16.3 / 0.4 MiB | 827 / 832 / 17 MiB |

“Total” is a sequential first-use workflow: start a fresh server, wait for readiness, open a synthetic Java file inside the project, receive diagnostics, request first and warm JDK completions, request a project/dependency-aware completion, then request first and warm workspace-symbol results. RSS is the observed resident memory of the language-server process tree after those operations.

<details>
<summary>Detailed first-use operation timings</summary>

All values below are **mean / median / sample standard deviation in milliseconds**.

| Project | Server | Ready | Diagnostics | JDK completion | Project/dependency completion | Workspace symbol |
|---|---|---:|---:|---:|---:|---:|
| PetClinic | java-vsix-lite | 14.1 / 2.33 / 17.5 | 14.1 / 2.33 / 17.5 | 269.8 / 265.3 / 20.2 | 3.25 / 3.27 / 0.11 | 0.98 / 0.97 / 0.06 | 0.18 / 0.15 / 0.04 |
|  | Red Hat | 3378 / 3378 / 133 | 4595 / 4649 / 146 | 1647 / 1644 / 19.7 | 6952 / 6989 / 119 | 42.4 / 41.9 / 2.20 | 275.6 / 293.2 / 61.1 |
| Commons CLI | java-vsix-lite | 12.6 / 0.37 / 15.8 | 12.6 / 0.37 / 15.8 | 28.2 / 27.8 / 0.97 | 4.96 / 4.98 / 0.22 | 3.70 / 3.67 / 0.16 | 0.17 / 0.16 / 0.03 |
|  | Red Hat | 3159 / 3150 / 20.2 | 4637 / 4639 / 31.6 | 1446 / 1450 / 16.1 | 2955 / 3063 / 271 | 31.8 / 27.5 / 10.8 | 67.1 / 63.5 / 16.8 |
| Gson | java-vsix-lite | 12.3 / 0.40 / 15.5 | 12.3 / 0.40 / 15.5 | 18.0 / 17.8 / 0.93 | 10.5 / 10.5 / 0.19 | 10.5 / 10.4 / 0.36 | 0.27 / 0.28 / 0.04 |
|  | Red Hat | 3213 / 3198 / 59.8 | 4169 / 4161 / 64.4 | 1758 / 1753 / 40.5 | 380.5 / 409.3 / 78.8 | 135.4 / 118.2 / 37.8 | 3399 / 3409 / 132 |
| Joda-Time | java-vsix-lite | 12.7 / 0.40 / 16.0 | 12.7 / 0.40 / 16.0 | 15.2 / 15.0 / 1.00 | 15.9 / 15.9 / 0.32 | 20.1 / 20.2 / 0.21 | 0.30 / 0.30 / 0.06 |
|  | Red Hat | 3173 / 3161 / 34.5 | 3617 / 3607 / 43.2 | 1544 / 1536 / 19.5 | 377.1 / 391.4 / 24.1 | 235.8 / 238.7 / 18.8 | 1373 / 1410 / 111 |
| JUnit 4 | java-vsix-lite | 15.2 / 0.39 / 21.0 | 15.2 / 0.39 / 21.0 | 14.9 / 14.7 / 0.66 | 22.1 / 21.6 / 1.94 | 0.26 / 0.25 / 0.03 | 0.29 / 0.29 / 0.02 |
|  | Red Hat | 3228 / 3204 / 75.2 | 3738 / 3721 / 75.1 | 1555 / 1537 / 32.9 | 254.0 / 255.2 / 14.6 | 22.5 / 18.9 / 9.10 | 1716 / 1694 / 103 |

</details>

### Benchmark method and limits

- Tested on an 8-core Apple M1 Pro MacBook Pro with 16 GB RAM and macOS 26.5.2. The java-vsix-lite server was a release build at commit `294d8b63`; the comparison used Red Hat Java 1.55.0 for Apple Silicon, including its embedded JRE and standard 100 MiB initial / 2 GiB maximum heap settings.
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
