# Changelog

## 0.1.0 — 2026-08-22

First tagged release. Pure-Rust language server (no JVM resident), thin
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
- Prebuilt for linux x64/arm64/alpine, macOS x64/arm64, windows x64;
  published to the VS Code Marketplace and Open VSX
