# Changelog

## Unreleased

### Added
- **JUnit test support**: JUnit 4/5 tests appear in VS Code's Testing view
  (discovered statically by the language server — no build-tool execution),
  with run and debug profiles, per-test pass/fail results, and failure
  messages. Tests run through the built-in debug adapter using the JUnit
  Platform Console Launcher, downloaded once (HTTPS + checksum verification,
  with a consent prompt) into `~/.m2/repository`; the version is the new
  `java-vsix-lite.test.junitLauncherVersion` setting (default `1.13.4`).
  Running tests executes project code, so it is disabled in untrusted
  workspaces. Launch configurations gain `includeTestOutputs` and
  `additionalClassPaths` attributes.
- Incompatible **reassignments** (`String s = ...; s = 10;`) are now flagged
  natively (`incompatible types: int cannot be converted to String`), same
  conservative rules as the existing initializer check; compound operators
  (`+=` …) stay silent.
- **Undeclared variables are flagged natively** (`cannot find symbol:
  variable 'x'`): a bare variable reference with provably no declaration in
  scope — a deleted declaration that is still referenced, a use before its
  declaration, or a block-scoped local used after its block — errors
  immediately, without waiting for `javac`. Conservative like the
  unresolved-member check (same `diagnostics.unresolvedMembers` setting):
  silent on parse recovery, unresolvable supertypes, static wildcard
  imports, and anonymous class bodies; positions where a class name would
  be legal are never judged.
- **Extract method** and **inline variable** refactorings, joining the
  existing rename and extract variable/constant actions. Both are
  conservative: extraction refuses abrupt control flow, out-params, and
  `var` captures; inlining refuses reassigned variables and never
  duplicates a call.
- **Styled diagnostic hovers**: hovering an error/warning from this
  extension shows a styled section — severity-colored header, code-styled
  type names, source/code badge, and (for type-mismatch errors) a clickable
  "declared as … here" link to the declaration site. While enabled (the
  default), the declaration link appears only in the styled section, so
  the editor's plain diagnostic block stays a single message line. Disable
  with `java-vsix-lite.diagnostics.styledHover` to restore fully plain
  hovers with standard related-information rows.

### Changed
- The status bar item now shows live Java error/warning counts with an
  error/warning background color, and its tooltip reports the server state,
  diagnostic counts, and the JDK in effect.

## 0.1.7 — 2026-08-25

### Fixed
- The automatic check for pre-existing errors on project load now also
  runs when a Java file is opened later in the session, not only for
  files already open at the moment the extension activates — previously
  this left it a near-permanent no-op whenever no Java file happened to
  be open yet when the extension started.
- A background check spanning multiple files no longer silently skips
  every file in the batch just because one of them sits outside any
  recognized Maven/Gradle module or conventional source root — the rest
  are still checked.

## 0.1.6 — 2026-08-25

### Added
- Immediate native semantic diagnostics report incompatible method returns
  and variable/field initializers, unreachable statements (with correct
  `try`/`catch`/`finally` reachability — a `finally` block is always
  checked, and a pending return/throw from `try` or `catch` survives a
  normally-completing `finally`), and unused locals, eligible parameters,
  and unreferenced private fields/methods — all without waiting for a save
  or `javac`. Unused warnings can be disabled with
  `java-vsix-lite.diagnostics.unused`; automatic `javac` remains the broad,
  revision-safe correctness backstop.

### Changed
- When the automatic `javac` check confirms a native diagnostic, the native
  entry is now kept in place instead of being replaced by the compiler's
  copy — a save no longer flips an already-showing error's source or
  otherwise changes what's displayed for an already-proven issue.

## 0.1.5 — 2026-08-24

### Added
- **Java debugging** (launch and attach) via a pure-Rust DAP↔JDWP adapter
  built into the existing `jvl-server` binary (`jvl-server dap`) — no new
  native artifacts, no JVM-side components. F5 on a Java file with a `main`
  method works with no `launch.json`. Supported: line breakpoints, stepping
  (over/into/out), pause, threads, stack traces with source mapping,
  variable inspection (locals, `this`, object fields, arrays, strings),
  caught/uncaught exception break filters with exception details, program
  output in the Debug Console, and `stopOnEntry`. The debuggee classpath
  uses explicit `classPaths` when given, else existing Maven/Gradle build
  output plus resolved dependency jars, else a one-shot `javac -g`
  auto-compile. Expression evaluation and hot code replace are not
  supported. Debugging runs project code, so it is disabled in untrusted
  workspaces; the JVM is chosen by the machine-scoped
  `java-vsix-lite.jdk.home` setting (never by the workspace), and the JDWP
  connection is loopback-only.

## 0.1.4 — 2026-08-23

### Changed
- The automatic background `javac` check is now **module-scoped**. On save it
  compiles only the Maven/Gradle module(s) owning the saved file(s) instead of
  the whole workspace, so unrelated modules aren't recompiled on every save.
  Sibling-module sources are still resolved (passed to `javac` via
  `-sourcepath`) so cross-module references don't produce false "cannot find
  symbol" errors. On activation/trust-grant the check now covers only the
  modules of already-open Java documents rather than the entire project. If a
  scoped compile flags a file outside the checked modules, the run transparently
  falls back to a full-project check so no partial/misleading result is ever
  published. The manual **Java: Check Project (javac)** command remains a
  complete full-workspace check (and now covers every module in a multi-module
  project, not just the root module and open files).

### Added
- **Rebuild Classpath (Refresh IntelliSense)** command — re-reads the build
  files and local dependency caches and rebuilds the classpath without
  restarting the language server (the light counterpart to **Restart Language
  Server**). Offline and side-effect-free, so it works in untrusted workspaces.

## 0.1.3 — 2026-08-23

### Changed
- The `javac` check now compiles at the **project's declared Java
  release** rather than always the newest installed JDK's level. The
  target level is read statically from the build files (Maven
  `maven.compiler.release`/`maven.compiler.source`/`java.version` across
  the parent chain; best-effort Gradle toolchain/`sourceCompatibility`)
  and compiled with `--release N` — validated against release N's API,
  matching the real build — with `--enable-preview` only when N equals
  the running JDK. When the level is undeclared it falls back to the
  JDK's own level as before.

### Added
- When the newest detected JDK is **older** than the project's declared
  Java level (e.g. a Java 21 project with only a JDK 17), the check is
  skipped and a single clear message is shown — both as a diagnostic on
  the build file and as a notification with a shortcut to the
  `java-vsix-lite.jdk.home` setting — instead of a flood of
  "not supported in -source" errors. The pure-Rust tier is unaffected.

## 0.1.2 — 2026-08-23

### Fixed
- Resolve the Maven built-in `${project.parent.version}` /
  `${project.parent.groupId}` properties when computing dependency
  versions. Some libraries version a dependency against their own parent
  this way — notably `swagger-core-jakarta`, which declares
  `swagger-annotations-jakarta` at `${project.parent.version}` — so the
  version was left unresolved and the artifact dropped. This made
  `io.swagger.v3.oas.annotations.*` (pulled in via springdoc-openapi)
  report as unresolved in Spring projects such as cBioPortal. The full
  swagger/springdoc chain now resolves.
- Inherit a parent POM's `<dependencies>` into its child modules (Maven
  adds them to the child, not only `<dependencyManagement>`). Some
  multi-module libraries depend on a sibling only through the parent —
  e.g. `datumbox-framework-storage` declares `datumbox-framework-common`,
  which its storage child modules inherit — so without this the artifact
  was never resolved and its whole package (`com.datumbox.framework.common.*`)
  reported as unresolved.

## 0.1.1 — 2026-08-23

### Fixed
- Test-scope dependencies are now on the classpath. `src/test/java` is
  analyzed like `src/main/java`, but the resolver was dropping the
  project's own test-scope dependencies, so imports such as
  `org.assertj.core.api.Assertions`, JUnit, Mockito, and the rest of
  `spring-boot-starter-test` all reported as unresolved in test files.
  Root test-scope deps (and their compile/runtime transitives) are now
  included; transitive test scope is still dropped, so a compile
  dependency never pulls in another library's test dependencies.

## 0.1.0 — 2026-08-22

First release. Pure-Rust language server (no JVM resident), thin
TypeScript shell, security-first design: build scripts are never executed,
network access is consent-gated, nothing downloaded is ever executed.

### Language features
- Completion: locals, members, chains, type names with auto-import, import
  paths — across open files, closed project files, and JDK/dependency jars,
  with generics rendered from bytecode signatures
- Hover with rendered Javadoc (`@param`/`@return`/`{@code}`/HTML → Markdown,
  `{@inheritDoc}` and supertype inheritance), signature help, go to
  definition / type definition / implementation, find references, rename,
  workspace symbols, document outline, folding, selection ranges, semantic
  tokens
- Call hierarchy (incoming/outgoing) and type hierarchy (supertypes/subtypes)
- Code actions: add-import quick fix, Organize Imports, extract
  variable/constant, generate accessors / constructor / equals+hashCode /
  toString
- Lombok awareness: `@Getter`/`@Setter`/`@Data`/`@Value`/`@With`/`@Builder`
  members synthesized (builder chains included) — no annotation processor run
- Diagnostics: syntax + structural + conservative unresolved-member checks
  live; real `javac` errors on project load and after every save (trusted
  workspaces; `-proc:none`; debounced and silent). The check compiles at the
  detected JDK's own source level with `--enable-preview`, so preview syntax
  (e.g. pattern matching in `switch` on JDK 17–20) isn't flagged as an error
- Hover on a `var` local shows its inferred type (`Widget w`,
  `ArrayList<String> list`) rather than the literal `var` keyword
- Java 21 pattern bindings — `case Type name` (guarded patterns included),
  record deconstruction (`case Point(int x, int y)`), and `instanceof Type
  name` — are now first-class: their binding and every use are semantically
  highlighted, hover shows `Type name`, and go-to-definition / references /
  rename / completion all recognize them
- Semantic-highlight defaults ship for parameters, variables, and properties
  (via `editor.semanticTokenColorCustomizations`), so references stay colored
  on themes that otherwise leave them at the default foreground
- Method references (`Map.Entry::getKey`, `String::valueOf`) highlight the
  referenced method, and a nested-type qualifier (`Map.Entry`) is no longer
  mis-flagged as a missing field
- Resolution accuracy: cross-file enums expose their constants, implicit
  `java.lang.Enum` members (`name()`/`ordinal()`/…), and synthesized
  `values()`/`valueOf`; enhanced-for `var` binds the element type, not the
  collection; a project type's package-private members are visible to
  same-project code; and a generic chain that erases to `Object`
  (`stream().findFirst().orElseThrow()`) no longer produces false
  unresolved-member errors (validated by a full sweep of a 1200-file project:
  ~986 → a dozen residual false positives)

### Build & dependencies
- Maven/Gradle classpaths resolved statically and offline from build files +
  local caches (transitives included); rebuilt automatically on build-file
  changes; build scripts never executed. Gradle's cache honors
  `$GRADLE_USER_HOME` (falling back to `~/.gradle`) for relocated caches
- JDK auto-detection scans `$JAVA_HOME`, the system and **per-user** macOS JVM
  locations, Linux `/usr/lib/jvm` + `/usr/java`, and JetBrains `~/.jdks` /
  SDKMAN — ranked by the JDK's real feature version (from its `release` file),
  so the newest installed JDK is used (fixes picking an older JDK that rejects
  modern syntax like `switch` `when` guards). Overridable via
  `java-vsix-lite.jdk.home`
- Missing dependencies downloadable from Maven Central or a machine-configured
  internal repository (`dependencies.repository`) — HTTPS-only, mandatory
  checksum verification, consent-gated, capped. When nothing is directly
  fetchable (a version managed by a parent POM/BOM that isn't cached), the
  command now says so and offers to install instead of reporting "none"
- `Install Dependencies (runs build tool)` command — runs the project's
  `mvn -B dependency:go-offline` / `gradle dependencies` (preferring an
  `mvnw`/`gradlew` wrapper) to populate the local cache with BOM/parent-managed
  transitives the offline resolver can't version on its own, then rebuilds the
  classpath. **Trusted workspaces only**, with an explicit per-run confirmation
  that it executes the project's build scripts — never automatic. Executable
  overridable via machine-scoped `maven.path` / `gradle.path`

### Platforms
- Prebuilt for linux x64/arm64, linux-alpine x64, macOS x64/arm64, and
  windows x64; the release workflow packages a platform-specific VSIX for
  each and publishes to the VS Code Marketplace and Open VSX on a version
  tag. (Windows arm64 and alpine arm64 are not currently built.)
