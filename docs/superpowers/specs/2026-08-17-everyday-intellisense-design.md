# M7: Everyday IntelliSense — chains, statics, `var`, type names, imports

> Status: **approved design** (autonomous session; user directive: "intellisense
> on imported packages basically does not work — make this a lightweight
> replacement for Red Hat, Ruff for Java"). Implementation in this branch.

## Problem (live-repro evidence, 2026-08-17)

Against the built server with a real JDK 21, only one completion shape works:

| Case | Result |
|---|---|
| `List<String> xs; xs.` (declared imported type) | ✅ 55 items |
| `xs.stream().` (chained call) | ❌ 0 items |
| `System.out.` (static field of external type) | ❌ 0 items |
| `s.trim().` (String chain) | ❌ 0 items |
| `var v = new ArrayList<String>(); v.` | ❌ 0 items |
| `ArrayLi` (classpath type name while typing) | ❌ 63 items, none of them `ArrayList` |
| `import java.ut` (import path) | ❌ 0 items |

Everyday Java **is** the failing rows. The working row is a narrow island, which
is why users experience "imported-package intellisense basically does not work."

Root causes, all confirmed in code:

1. `resolve.rs::resolve_receiver_depth` has no `method_invocation` arm — the
   type of `xs.stream()` is never computed (return-type resolution was deferred
   in sub-project 1 and never landed).
2. The `field_access` arm bails for external receivers ("external field-type
   chaining is deferred") and `resolve_scoped_path` only walks in-project
   var.field chains — `System.out` resolves through neither shape.
3. `var` bindings carry a `var` type node that resolves to nothing; the
   initializer expression is never consulted.
4. `completion.rs::scope_items` draws type names only from open documents —
   `jvl-classpath` has no name-listing API at all (`class(fqn)` is exact-match),
   so classpath types cannot be offered, and there is no auto-import.
5. Nothing handles completion inside an `import` declaration.
6. Bug: an array receiver resolves to its **element** type (`base_type_name`
   unwraps `array_type`), so `String[] a; a.` offers `trim()` etc. instead of
   `length`/`clone()`.

## Goal

All seven rows green, from bytecode already on disk, within the project's hard
constraints: no JVM, no network, no build execution, bounded compute, pure-Rust
default tier.

## Approaches considered

- **A (chosen): extend the existing single-pass resolver + classpath crate.**
  Add structured member *result types* to the bytecode model (descriptor +
  `Signature` attribute already carry them), a lazily-built type-name index over
  archive central directories (names are already in memory), and the missing
  resolver arms. Incremental, hermetically testable, no new dependencies, no
  architectural change.
- **B: ship the escalation tier (javac-based LSP subprocess) now.** Solves
  chains "for free" but costs a JVM by default — exactly what this project
  exists to avoid; still needed later for flow typing, but not the fix for
  bread-and-butter completion.
- **C: full semantic database (rust-analyzer-style salsa engine).** The right
  end-state for exhaustive semantics, but a rewrite-scale effort; not needed to
  make everyday completion work.

## Design (approach A)

### 1. `jvl-classpath`: structured member result types

`Member` (and mirrored `jvl-syntax::ExternalMember`) gains:

- `ret_fqn: Option<String>` — dotted FQN of the **erased** method return type /
  field declared type, from the descriptor (`Ljava/util/stream/Stream;` →
  `java.util.stream.Stream`). `None` for primitives, `void`, arrays, and
  constructors.
- `ret_display: Option<String>` — the generic return/field type in the existing
  template convention (`Stream<{0}>`, `{0}`, `List<Map<{0},{1}>>`; `{i}` are
  the declaring class's type params, method-own type params render by name),
  from the `Signature` attribute. `None` when there is no `Signature`.

`generics.rs::method_template` already computes the return component
separately; `field_template` already yields the field type. This is plumbing,
not new parsing. Erased fallback keeps chains working for non-generic types
(`s.trim()` → `java.lang.String` with no `Signature` attribute).

### 2. `jvl-classpath`: type-name index

New lazily-built (`OnceLock`) index over all archives' central-directory entry
names — strings already resident in memory; no bytecode parsing:

- Entry filter: `*.class` only; strip jmod `classes/` prefix; skip
  `module-info`/`package-info`; skip anonymous/local/synthetic shapes (`$` +
  digit, `$$`); skip JDK-internal namespaces (`sun.`, `com.sun.`,
  `jdk.internal.`, `oracle.`, `netscape.`, `apple.`, `com.apple.`) — these stay
  resolvable by exact FQN, they are just never *offered*.
- Dedup by FQN, first archive wins (same rule as `class()`).
- API:
  - `types_with_prefix(prefix, limit) -> Vec<TypeEntry>` — case-insensitive
    simple-name prefix match, deterministic order, hard cap.
  - `package_children(pkg) -> (Vec<String> subpackages, Vec<TypeEntry> types)`
    — `""` lists roots.
  - `TypeEntry { simple, fqn (binary, `$` nested), import_path (dots) }`.

Scale: a JDK 21's jmods plus typical deps ≈ tens of thousands of names → a few
MB and single-digit ms to build once.

### 3. `jvl-syntax`: resolver arms (the headline)

`SymbolSource` gains defaulted methods (`types_with_prefix`,
`package_children`) so existing mocks/`NoSymbols` compile unchanged;
`ExternalMember` carries the two new fields.

- **`method_invocation` receivers.** Resolve the object (no object → enclosing
  type as instance), find the named member in the Method namespace, then
  compute its *result type*:
  - In-project member → its return-type AST node through the existing
    `resolve_type_node` (current-file import context; same best-effort
    convention as field chains today).
  - External member → substitute the use-site type args into `ret_display`,
    parse `Base<A, B>` (top-level comma split), resolve `Base`: in-project
    table first, else `ret_fqn` when simple names agree, else import
    candidates (covers `{0} get(int)` → `String` → `java.lang.String`, and
    project types flowing out of generic containers). No `ret_display` →
    `ret_fqn` erased. Neither → unresolved (primitives/void/arrays).
- **`field_access` with external object** — same result-type machinery on the
  Field namespace (`System.out.` → `java.io.PrintStream`).
- **Dotted-path walk.** `resolve_scoped_path` gains a general segment walk:
  resolve the first segment (binding → type → imports/`java.lang`), then each
  next segment as nested class (`fqn$Seg`) or field member, tracking
  static→instance transitions. Whole-path-as-FQN stays as the fallback for
  fully-qualified references. Covers `System.out.`, `Map.Entry.`,
  `java.util.List.` under both `field_access` and mid-edit
  `scoped_type_identifier` parse shapes.
- **`var` inference.** When a binding's declared type is `var` (or absent),
  resolve the declarator's initializer expression as a receiver (depth-capped;
  forced instance). `var` in enhanced-`for` headers is out of scope (needs
  element-type inference).
- **Casts.** `((Type) expr).` — resolve the cast expression's type node.
- **Arrays.** New `ResolvedType::Array`: members are `length` (int field) and
  `clone()`, plus `java.lang.Object`'s; fixes the wrong-element-members bug and
  keeps unresolved-member diagnostics honest.

Collateral wins for free: hover, signature help, go-to-definition, and
unresolved-member diagnostics all share this resolver, so chains light up
everywhere at once.

### 4. `jvl-syntax`: completion surfaces

- `completion()` returns `CompletionResult { items, is_incomplete }` (server
  maps to an LSP `CompletionList`).
- **Classpath type names in scope completion**: only when the typed identifier
  prefix is ≥ 2 chars; capped (`is_incomplete = true` at the cap so clients
  re-query); deduped against open-doc type names; label = simple name, detail =
  dotted import path.
- **Auto-import**: each classpath type item carries `additional_text_edits`
  inserting `import <path>;` at the computed insertion point (after the last
  import, else after `package`, else file top) — skipped when already imported,
  same package, `java.lang`, or covered by a wildcard import. A candidate whose
  simple name is single-imported to a *different* FQN is filtered out entirely
  (it cannot be referenced by simple name).
- **Import-statement completion**: text-anchored detection (`^\s*import\s+
  (static\s+)?path-so-far` on the cursor line, checked before the member-access
  branch). Completes subpackage segments and types from `package_children`;
  once the path prefix resolves to a class, completes its nested types and (for
  `import static`) its static members.
- **Ranking**: stable `sort_text` buckets — bindings < members < in-project
  types < classpath types < keywords — so classpath noise never buries locals.
- Type items get lazy Javadoc via the existing `completionItem/resolve` path
  (`doc(fqn, None)`).

### 5. Server + extension

- `ClasspathSymbols` delegates the two new `SymbolSource` methods to the index;
  completion handler returns `CompletionResponse::List`.
- No new settings this wave (Ruff philosophy: good defaults first). No TS
  changes required; README feature list refreshed.

## Threat model

No new attack surface: the index reads central-directory names already parsed
under existing ZIP hardening; no network, no execution, no new deps. Result
caps bound completion payloads. Auto-import emits client-side text edits only.

## Testing

- **classpath**: fixture-built class files (existing builder) assert
  `ret_fqn`/`ret_display` across generic/erased/primitive/array/constructor
  shapes; real-JDK (skip-if-absent) asserts `List.stream`, `String.trim`,
  index prefix/package queries, and internal-namespace filtering.
- **syntax**: hermetic mock-`SymbolSource` tests for every failing row (chains
  incl. type-var returns and in-project returns, `System.out.` under both
  parse shapes, fully-qualified statics, nested classes, `var` incl. chained
  initializers, casts, arrays, type-name completion + auto-import edge cases,
  import-path completion, ranking buckets).
- **server**: lifecycle E2E against the real JDK for the repro's seven rows.
- The `/tmp/jvl-repro/drive.py` script re-run at the end must print 7 × OK.

## Addendum (user directives, 2026-08-17 session)

- **Dependency jars are first-class** (user: "packages referenced in gradle or
  maven … want intellisense as well"): the type-name index covers every
  archive on the resolved classpath — JDK jmods *and* Maven/Gradle dependency
  jars (transitives included, per M5.1). The JDK-internal namespace filter
  applies **only to jmod-sourced names**; a dependency legitimately shipping
  `com.sun.*` (e.g. Jersey) keeps full IntelliSense.
- **Proactive dependency install** (user: "if it is not in .m2 … try and
  install / index it"): when classpath resolution reports fetchable missing
  coordinates (`jvl/missingDependencies`, M6.2), the extension now offers the
  download *proactively* — a notification with **Download / Always (this
  workspace) / Never** — instead of waiting for the manual command. "Always"
  is a workspace setting (`jvl.dependencies.autoDownload`) that makes future
  misses download + rebuild + index silently. TLS + SHA-checksum verification
  and the Maven-Central-only origin stay mandatory (threat model: no *silent*
  network without a standing opt-in; the opt-in is explicit and revocable).

- **Closed project files are first-class** (user: "type `Person` when the
  class is imported [same project] — no option; `Person p = new Person();
  p.` gives no methods"): the server was open-files-only — a project class
  whose file isn't open was invisible. Fix: a **project-source symbol
  layer** — `ProjectSymbols`, backed by the existing lazy/bounded
  `WorkspaceIndex` (M4.5) — that resolves an FQN to a workspace `.java`
  file, parses it on demand (tree-sitter, mtime-cached, size-capped), and
  exposes it as an `ExternalClass` via a new `jvl_syntax::class_from_source`
  (supers/ret types resolved through the *declaring* file's imports with an
  existence-checking `pick_fqn` closure). Composed as
  `CombinedSymbols(project, classpath)` — project first — so closed files
  get name completion + auto-import, member completion, chains, hover, and
  Javadoc identically to jars. Open documents still shadow the disk copy in
  simple-name resolution (the TypeTable wins), keeping live-buffer edits
  authoritative.

## Non-goals (follow-ups tracked)

Organize-imports & add-import code actions on diagnostics; enhanced-`for`
`var`; lambda parameter inference; overload-precise return types (first
name-match wins today); jimage (JRE-only) fallback; formatting; escalation
tier. Static-member *scope* completion (`asList` unqualified via
`import static`) is also out.
