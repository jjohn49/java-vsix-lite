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
  workspaces; `-proc:none`; debounced and silent)

### Build & dependencies
- Maven/Gradle classpaths resolved statically and offline from build files +
  local caches (transitives included); rebuilt automatically on build-file
  changes; build scripts never executed
- Missing dependencies downloadable from Maven Central or a machine-configured
  internal repository (`dependencies.repository`) — HTTPS-only, mandatory
  checksum verification, consent-gated, capped

### Platforms
- Prebuilt for linux x64/arm64/alpine, macOS x64/arm64, windows x64;
  published to the VS Code Marketplace and Open VSX
