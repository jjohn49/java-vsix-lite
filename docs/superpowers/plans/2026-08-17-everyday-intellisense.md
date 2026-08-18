# M7 Everyday IntelliSense Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the seven live-repro completion rows green (chains, `System.out.`, `var`, classpath type names + auto-import, import paths) per `docs/superpowers/specs/2026-08-17-everyday-intellisense-design.md`.

**Architecture:** Extend `jvl-classpath`'s member model with structured result types and a lazy type-name index over central-directory names; add the missing resolver arms in `jvl-syntax::resolve`; grow completion with classpath type items (auto-import `additional_text_edits`) and import-path completion; wire through `SymbolSource`/server.

**Tech Stack:** Rust workspace (`cafebabe`, tree-sitter-java, tower-lsp-server types via `ls_types`), hermetic class-file fixture builder in `class_info.rs`, real-JDK skip-if-absent tests.

## Global Constraints

- `#![forbid(unsafe_code)]` in `jvl-classpath`; `jvl-syntax` stays IO-free (talks only to `SymbolSource`).
- No new dependencies; no network; no execution; parse failures degrade to `None`, never panic.
- Bounded compute: result caps on all listing APIs; depth caps via existing `MAX_RESOLVE_DEPTH`.
- Keep all existing tests green (352 at baseline); `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` clean.
- Existing conventions: `{i}` template placeholders; binary FQNs (`$` for nested) as `class()` keys.

---

### Task 1: `jvl-classpath` structured member result types

**Files:**
- Modify: `crates/classpath/src/lib.rs` (Member struct)
- Modify: `crates/classpath/src/class_info.rs` (populate fields; tests)
- Modify: `crates/classpath/src/generics.rs` (only if the ret component isn't already separable — it is: `method_template` returns `(type_params, ret, params)`; `field_template` returns the type)

**Interfaces:**
- Produces: `Member { …, ret_fqn: Option<String>, ret_display: Option<String> }`
  - `ret_fqn`: dotted FQN of erased method return / field declared type; `None` for primitives, `void`, arrays, constructors.
  - `ret_display`: generic return/field type in `{i}` template form; `None` without a `Signature` attribute (and for constructors).

**Steps:**
- [ ] Failing tests in `class_info.rs` (fixture builder): generic method `()Ljava/util/stream/Stream;` + sig `()Ljava/util/stream/Stream<TE;>;` on class w/ `<E>` → `ret_fqn == Some("java.util.stream.Stream")`, `ret_display == Some("Stream<{0}>")`; type-var return `()TT;`-style → `ret_display Some("{0}")`, `ret_fqn Some("java.lang.Object")` (erasure from descriptor); primitive `()I` → both `None`… except `ret_fqn` must be `None` (primitive) while a plain object return without signature → `ret_fqn Some`, `ret_display None`; array `()[Ljava/lang/String;` → both `None`; field `Ljava/io/PrintStream;` → `ret_fqn Some("java.io.PrintStream")`; constructor → both `None`.
- [ ] Implement: in `parse`, compute from `method.descriptor.return_type` (`ReturnDescriptor::Return(fd)` with `fd.dimensions == 0` and `FieldType::Object(c)` → `fqn_of(c)`), and `field.descriptor` likewise; `ret_display` from the already-separate `ret` component of `generics::method_template` / `field_template` output.
- [ ] `cargo test -p jvl-classpath` green (real-JDK asserts: `List.stream` ret_fqn `java.util.stream.Stream`; `String.trim` ret_fqn `java.lang.String` with `ret_display None`).
- [ ] Commit.

### Task 2: `jvl-classpath` type-name index

**Files:**
- Modify: `crates/classpath/src/zip.rs` (`pub(crate) fn names(&self) -> impl Iterator<Item = &str>`)
- Create: `crates/classpath/src/index.rs`
- Modify: `crates/classpath/src/lib.rs` (OnceLock field + public API)

**Interfaces:**
- Produces: `pub struct TypeEntry { pub simple: String, pub fqn: String, pub import_path: String }`
- Produces: `Classpath::types_with_prefix(&self, prefix: &str, limit: usize) -> Vec<TypeEntry>` (case-insensitive simple-name prefix; sorted `(simple.len, simple, fqn)`; capped)
- Produces: `Classpath::package_children(&self, package: &str) -> (Vec<String>, Vec<TypeEntry>)` (`""` = roots; subpackages sorted+deduped; types sorted)

**Steps:**
- [ ] Failing tests (real JDK, skip-if-absent): `types_with_prefix("ArrayLi", 50)` contains fqn `java.util.ArrayList`; filtering: no result fqn starts with `sun.`/`com.sun.`/`jdk.internal.`; no `…$1`-shaped entries; `package_children("java.util")` subpackages contain `stream` + types contain `List`; `package_children("")` contains `java`. Hermetic test for filter fn on synthetic name list (skip `module-info`, `package-info`, `Foo$1`, `Foo$$Lambda`, keep `Map$Entry` with simple `Entry`, import_path `java.util.Map.Entry`).
- [ ] Implement `index.rs`: build from `archives` iter of `names()` (strip prefix/suffix, filter, dedup by fqn first-wins), store sorted `Vec<TypeEntry>` + `BTreeMap<String, Vec<usize>>` package→type idxs + `BTreeSet<String>` packages; `OnceLock` init in `Classpath`.
- [ ] `cargo test -p jvl-classpath` green; commit.

### Task 3: `jvl-syntax` resolver arms

**Files:**
- Modify: `crates/syntax/src/external.rs` (`ExternalMember.ret_fqn/ret_display`; `SymbolSource::{types_with_prefix, package_children}` defaulted; `pub struct TypeCandidate { simple, fqn, import_path }`)
- Modify: `crates/syntax/src/resolve.rs` (new arms + `ResolvedType::Array` + result-type machinery + segment walk + var/cast)
- Modify: `crates/syntax/src/completion.rs`, `hover.rs`, `definition.rs`, `signature_help.rs`, `references.rs`, `diagnostics.rs` — only as needed for `ResolvedType::Array` exhaustive matches (Array → behave like "no decl site / no fqn data").
- Modify: `crates/server/src/main.rs` `ClasspathSymbols` (map the two new Member fields; delegate the two new listing methods) — do here so the workspace compiles.

**Interfaces:**
- Consumes: Task 1 fields, Task 2 API.
- Produces (crate-internal): `resolve_receiver_depth` handles `method_invocation`, external `field_access`, general dotted segment walk, `cast_expression` (via parenthesized), `var` initializers, `ResolvedType::Array { component: String }`.
- Key helper: `fn external_result_type(m: &ExternalMember, recv_args: &[String], recv_type_params: &[String], ctx) -> Option<ResolvedType>`; `fn parse_display_type(s: &str) -> Option<(&str base, Vec<String> args)>` (top-level `<…>` split, nesting-aware).

**Steps (TDD, one commit per green batch):**
- [ ] Chain tests (mock symbols): `xs.stream().` → Stream members; `xs.get(0).` with `List<String>` + `{0} get(int)` → String members (mock `java.lang.String`); in-project return `Foo make() {}` → `make().` → Foo members; unqualified `make().` inside class; static chain `List.of().` erased.
- [ ] `System.out.` both parse shapes (statement `System.out.` mid-edit and inside method call arg) → println (mock System/out/PrintStream); `Map.Entry.` nested-class static walk; fully-qualified `java.util.List.` still statics.
- [ ] `var v = new ArrayList<String>(); v.` → add w/ `boolean add(String)`; `var s2 = s.trim(); s2.` → length; `var` chained through method_invocation initializer.
- [ ] Cast `((List) o).` → List members.
- [ ] Array: `String[] a; a.` → `length` + `clone` + Object members, and NOT `trim`; existing `deeply_nested` guard stays green.
- [ ] Implement per spec §3; keep `walk_members`/`diag_walk`/`member_names` Array-aware (`length`/`clone` names; complete=true only when Object resolves).
- [ ] `cargo test -p jvl-syntax` + workspace build green; commit.

### Task 4: completion surfaces

**Files:**
- Modify: `crates/syntax/src/completion.rs` (CompletionResult, classpath type items + auto-import, import-path completion, sort buckets)
- Modify: `crates/syntax/src/lib.rs` (re-export CompletionResult)
- Modify: `crates/server/src/main.rs` (completion handler → `CompletionResponse::List`)

**Interfaces:**
- Produces: `pub struct CompletionResult { pub items: Vec<CompletionItem>, pub is_incomplete: bool }`; `completion(…) -> CompletionResult`.
- Constants: `MIN_TYPE_PREFIX = 2`, `MAX_CLASSPATH_TYPES = 200`.
- Import detection: line-up-to-cursor regex-free scan `^\s*import\s+(static\s+)?([\w.$]*)$` implemented with str ops.

**Steps:**
- [ ] Tests: prefix <2 → no classpath items; `ArrayLi` → item label `ArrayList`, detail `java.util.ArrayList`, additional_text_edits inserts `import java.util.ArrayList;\n` after last existing import (assert exact Position); already-imported/same-package/`java.lang`/wildcard-covered → no edit; conflicting single import (same simple, different fqn) → candidate absent; dedup vs open-doc type; cap → `is_incomplete`.
- [ ] Import-path tests: `import java.ut` → `util` (subpackage); `import java.util.` → types + subpackages; `import java.util.Map.` → nested `Entry`; `import static java.util.Arrays.` → static member names; non-import lines unaffected.
- [ ] Sort buckets test: binding sort_text < member < in-project type < classpath type < keyword.
- [ ] Implement; update test helpers to `.items`; server handler returns `CompletionResponse::List(CompletionList { is_incomplete, items })`, still `None` when empty and complete.
- [ ] Workspace green; commit.

### Task 4b: proactive dependency install (extension)

**Files:**
- Modify: `editors/vscode/src/extension.ts` (on classpath build/rebuild completion, query `jvl/missingDependencies`; notification Download / Always (workspace) / Never; honor `jvl.dependencies.autoDownload`)
- Modify: `editors/vscode/src/mavenFetch.ts` (reuse fetch pipeline unchanged)
- Modify: `editors/vscode/package.json` (setting `jvl.dependencies.autoDownload`: `"prompt" | "always" | "never"`, default `"prompt"`)

**Steps:**
- [ ] Read current wiring (when the manual command runs, how results surface).
- [ ] Implement prompt + setting + auto path; `npm run compile` + lint clean.
- [ ] Manual note in README (consent model unchanged: TLS + checksum, Maven Central only).
- [ ] Commit.

### Task 5: E2E + polish

**Files:**
- Modify: `crates/server/tests/lifecycle.rs` (E2E: the seven rows against real JDK, skip-if-absent)
- Modify: `README.md` (feature refresh)
- Modify: `.superpowers/sdd/progress.md` (wave record)

**Steps:**
- [ ] E2E test per existing lifecycle.rs harness patterns covering rows A–G.
- [ ] `cargo fmt` + `clippy -D warnings` + full `cargo test --workspace` green.
- [ ] Re-run `/tmp/jvl-repro/drive.py` → 7 × OK (manual gate; paste output into commit message).
- [ ] Commit + update progress ledger.

## Self-Review

- Spec coverage: §1→Task 1, §2→Task 2, §3→Task 3, §4→Task 4, §5+testing→Tasks 3–5. Lazy type-item Javadoc (`doc(fqn, None)`) folded into Task 4. ✓
- No placeholders; types named consistently (`TypeEntry`/`TypeCandidate` mirror across crates; `ret_fqn`/`ret_display` everywhere). ✓
- Risk noted: `ResolvedType::Array` exhaustive-match ripple across feature files — bounded, compiler-driven.
