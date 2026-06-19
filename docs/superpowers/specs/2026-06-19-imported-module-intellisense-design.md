# Imported-module IntelliSense — Sub-project 2a: JDK + declarative-dependency members

> Status: **approved design**, implementation pending.
> Builds on sub-project 1 (in-file/open-file resolver, hover, completion). This
> slice adds **signature-level IntelliSense for imported types** — JDK classes and
> declared project dependencies — by reading `.class` bytecode, **without running
> a JVM or any project code**.

## Goal

`String s; s.` → `length()`, `substring(int)`, … and `List<X> xs; xs.` →
`add`, `get`, `size`, `stream`, … sourced from the user's JDK and from a project's
declared dependencies. Hover renders the same external signatures.

## Decisions (from brainstorming)

- **Scope:** JDK types **and** direct project dependencies (Maven + Gradle).
- **JDK access:** read `$JAVA_HOME/jmods/*.jmod` (a 4-byte `JM\x01\x00` header + a
  standard ZIP; classes at `classes/<fqn with '/'>.class`, DEFLATE-compressed).
  Verified against the user's JDK 21. jimage (`lib/modules`) fallback for
  JRE-only environments is deferred to 2c.
- **ZIP reading:** a hand-rolled, read-only central-directory reader + a pure-Rust
  DEFLATE decompressor, with zip-bomb / zip-slip guards baked in (no `zip` crate).
- **Bytecode:** `cafebabe` (pure Rust, Java-21 class-file spec) — no JVM, no exec.
- **Maven:** static-parse `pom.xml` (XXE-safe) → locate jars in `~/.m2`. Direct
  dependencies only.
- **Gradle:** **static heuristic, no execution** — scrape string-literal
  `group:artifact:version` coordinates from `build.gradle(.kts)` + parse
  `libs.versions.toml` version catalogs → locate jars in `~/.gradle/caches`.
  Best-effort and clearly logged. The accurate "run Gradle" path (trust-gated,
  sandboxed) is deferred to 2c.
- **Signatures:** rendered from **raw descriptors** (generics erased, as in
  sub-project 1). Generic `Signature`-attribute rendering is a later refinement.
- **Classpath timing:** resolved at server init; re-resolution on build-file edit
  is deferred.

## Non-goals (this slice)

- Transitive dependencies, parent POMs, `dependencyManagement`/BOM-derived
  versions, Gradle dynamic/computed/`platform` deps.
- Running any build tool (Gradle execution → 2c).
- jimage parsing (JRE-only environments → 2c).
- Generic type-argument rendering (`add(E)`); members render with erased types.
- Network access of any kind; downloading artifacts.

## Build order (three compiling, tested slices)

### Slice A — `jvl-classpath` crate (the engine)

A new workspace crate owning all bytecode/archive IO. `#![forbid(unsafe_code)]`.

- **ZIP reader** (`zip.rs`): locate the End-Of-Central-Directory record, parse the
  central directory into `name → (offset, comp_size, uncomp_size, method)`. Read a
  single entry by name on demand: seek, read the local header, DEFLATE-decompress
  (pure-Rust), enforcing caps. Supports a byte offset so a jmod is read as
  "skip 4 bytes, then ZIP". **Security:** reject entry names containing `..`, a
  leading `/`, or `\`; cap per-entry uncompressed size and the compression ratio
  (zip-bomb); bounded reads; never write to disk.
- **Class parsing** (`class_info.rs`): `cafebabe` parses entry bytes → `ClassInfo`
  { fqn, super FQN, interface FQNs, members:[{name, kind (Method/Field), signature,
  is_static, is_public}] }. Signatures rendered from field/method **descriptors**
  (`(Ljava/lang/Object;)Z` → `boolean add(Object)`); internal names `a/b/C` → `a.b.C`
  → simple `C` for display. Parse failures yield `None` (skipped, never panic).
- **Archive set + cache** (`lib.rs`): a `Classpath` holding an ordered list of
  archives (jmods + jars) each with its lazily-parsed central directory, plus a
  cache `fqn → Option<ClassInfo>`. `fn class(&self, fqn) -> Option<ClassInfo>`
  maps `a.b.C` → `classes/a/b/C.class` (jmod) or `a/b/C.class` (jar), first archive
  that has it wins. Interior mutability (e.g. `RwLock`) so lookups are `&self` and
  cached; independent of the server's documents lock.
- **JDK discovery** (`jdk.rs`): `JAVA_HOME` → `/usr/libexec/java_home` (macOS) →
  `java` on `PATH` resolved to its home. Enumerate `jmods/*.jmod`. Graceful `None`
  when no JDK / no jmods.
- **Tests:** against the real JDK — `class("java.util.List")` has `add`/`get`/`size`;
  `class("java.lang.String")` has `length`/`substring`; super-chain reaches
  `java.lang.Object`; signature rendering. ZIP hardening: crafted zip-bomb and
  bad-name entries are rejected. Missing-JDK path returns `None`.

### Slice B — `jvl-syntax` integration (the headline lands here)

- **`SymbolSource` trait** (in `jvl-syntax`, keeping it IO-free):
  ```rust
  pub trait SymbolSource {
      fn class(&self, fqn: &str) -> Option<ExternalClass>;
  }
  ```
  `ExternalClass` = supers (Vec<Fqn>) + members (name, kind, signature, is_static).
  Tests pass a mock; the server passes a `jvl-classpath`-backed impl. Completion
  and hover gain a `&dyn SymbolSource` parameter.
- **Imports → FQN** (`imports.rs`): parse the file's `package` and `import`
  declarations once. Resolve a simple type name absent from the in-project
  `TypeTable`, in order: explicit `import a.b.C;` → same package → each
  `import a.b.*;` (validated via `source.class(fqn).is_some()`) → implicit
  `java.lang.*`. A `scoped_type_identifier` receiver/type (`java.util.List`)
  yields its FQN directly.
- **`TypeRef`**: receiver resolution returns `InProject(TypeDecl)` or
  `External(Fqn)`. Member completion / hover for `External` query the
  `SymbolSource`; `static_only` still filters.
- **Mixed inheritance:** `all_members` / `find_member`, when a supertype simple
  name misses the `TypeTable`, resolve it to an FQN and descend through the
  `SymbolSource` (depth/cycle guarded). Every chain terminates at
  `java.lang.Object`.
- **Tests:** hermetic, with a mock `SymbolSource` — explicit/wildcard/same-package/
  `java.lang` import resolution; external member completion; external + mixed
  inheritance; hover on external members.

### Slice C — declarative dependency discovery

- **Maven** (`maven.rs`): find workspace `pom.xml`; `roxmltree` parse (no DTD /
  external entities → no XXE; size caps) of `<dependencies>`; substitute simple
  `${prop}` from `<properties>`; sanitize coordinates (no `..`/separators); locate
  `~/.m2/repository/<group with '/'>/<artifact>/<version>/<artifact>-<version>.jar`;
  register in the `Classpath`. Direct deps only; missing-version (BOM-derived) deps
  are logged and skipped.
- **Gradle** (`gradle.rs`): scrape string-literal `"g:a:v"` / `'g:a:v'` coordinates
  from `build.gradle` / `build.gradle.kts`; parse `gradle/libs.versions.toml`
  `[libraries]`/`[versions]`; locate jars under
  `~/.gradle/caches/modules-2/files-2.1/<group>/<artifact>/<version>/*/<artifact>-<version>.jar`
  (hashed leaf dir). **No execution.** Best-effort; unresolved/dynamic deps logged.
- **Server wiring:** at `initialize`, discover the JDK and (from `rootUri`) the
  project's declared deps; build the `Classpath`; inject the `SymbolSource`.
- **Tests:** POM/Gradle coordinate extraction from fixtures; `~/.m2`/`~/.gradle`
  path construction; XXE-malformed POM rejected.

## Architecture & threat model

`jvl-syntax` remains pure analysis (no IO) — it calls the `SymbolSource` trait.
All filesystem/ZIP/bytecode work is isolated in `jvl-classpath`, which:
- does **no execution, no network**; reads only local archives and build files;
- is `#![forbid(unsafe_code)]`; parse failures degrade to `None`, never panic;
- enforces zip-bomb/zip-slip/XXE/path-traversal guards (see Slices A & C);
- caches aggressively (immutable archives) behind its own lock — external lookups
  never block document parsing.

New dependencies (`cafebabe`, a pure-Rust DEFLATE crate, `roxmltree`) are pinned in
`Cargo.lock` and covered by `cargo-deny` (advisories/licenses/bans/sources); no
build-time downloads. Maven/Gradle file reads are read-only static parsing and honor
Workspace Trust (no build execution in this slice).

## Open items tracked for later (2b/2c)

Transitive + parent-POM/BOM resolution; Gradle execution behind Workspace Trust;
jimage fallback; generic `Signature` rendering; classpath re-resolution on
build-file edits; fuzz targets for the ZIP + class readers (M8).
