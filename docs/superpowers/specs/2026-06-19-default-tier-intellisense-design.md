# Default-tier IntelliSense — Sub-project 1: in-file/open-file resolver, completion & hover

> Status: **approved design**, implementation in progress.
> Scope: the first of several sub-projects toward the README's "basic IntelliSense"
> default tier. This one delivers hover + completion for types defined in the
> **current and other open files**, with **no JAR/JDK/classpath** machinery.

## Goal

Add LSP **completion** and **hover** to the pure-Rust default tier, built entirely
on the existing tree-sitter engine (no type checker, no bytecode reading). Deliver
a real "type a dot, see members" + "hover shows a signature preview" experience
for symbols the parser can see in open documents.

This sub-project also builds the **name/type resolver foundation** that the later
JAR/JDK sub-project (cafebabe bytecode + classpath) will plug into.

## Non-goals (this sub-project)

- Members of imported types from JARs or the JDK (`list.` → `java.util.List`). Needs
  the bytecode + classpath sub-project.
- Method-call return-type chains (`foo().bar().`) and `var` local inference.
- Cross-package disambiguation / true import resolution (we match by **simple name**).
- Generic type argument tracking (generics are erased to raw for resolution; the
  *rendered* signature still shows generics as written in source).
- Anything requiring whole-workspace indexing (open-files-only, per the README).

## Decisions (from brainstorming)

- **Two completion kinds:** member completion after `.`, and in-scope identifier +
  keyword completion while typing.
- **Resolver reach:** receiver resolved when it is `this` / `super`, a local / param /
  field referenced by name (via its **declared** type), `new Type(...)`, or a
  `field_access` whose object resolves (e.g. `this.values.`). Members include those
  inherited from supertypes that are themselves in-file/open-file. Deferred:
  method-invocation receivers, `var`.
- **Hover:** reconstructed signature in a fenced `java` block + the symbol's preceding
  `/** Javadoc */` rendered as markdown. References resolve to their declaration.
- **Cross-file:** all open documents; type names matched by simple name; the current
  file wins ties.

## Architecture & module layout

All analysis stays in `crates/syntax` (the server remains a thin LSP shell). The
crate's single 875-line `lib.rs` is split so each unit has one responsibility:

```
crates/syntax/src/
  lib.rs          # facade: keeps LineIndex/parse/PositionEncoding + existing fns;
                  #   declares the new modules and re-exports their public API.
  model.rs        # TypeTable / TypeDecl / Member — declarations extracted from trees
  signature.rs    # reconstruct "int sum()" / "private List<Integer> values" from a node
  resolve.rs      # cursor → enclosing type, scope bindings; receiver expr → resolved type
  completion.rs   # member + scope CompletionItems
  hover.rs        # Hover (signature + Javadoc)
```

Existing functions (`syntax_diagnostics`, `document_symbols`, `folding_ranges`,
`selection_ranges`, `semantic_tokens`) are NOT rewritten; we only split files where
it reduces coupling. `signature.rs` may also fill the currently-empty `detail` field
on `document_symbols` (nice-to-have, not required).

### Execution model

tree-sitter `Node<'t>` borrows its `Tree`, so all resolution runs **synchronously
while the `documents` lock is held**, exactly like `diagnostics_for` today — no
`.await` is held across resolution. The server snapshots the open documents into a
slice the syntax crate can read:

```rust
// jvl_syntax public input
pub struct OpenDoc<'a> { pub source: &'a str, pub tree: &'a Tree }

pub fn completion(docs: &[OpenDoc], current: usize, index: &LineIndex, pos: Position)
    -> Vec<CompletionItem>;
pub fn hover(docs: &[OpenDoc], current: usize, index: &LineIndex, pos: Position)
    -> Option<Hover>;
```

`docs[current]` is the file the cursor is in; the rest provide additional type
declarations. The handler resolves entirely against in-memory open documents — **no
filesystem, no network, no code execution** — preserving least-privilege and the
`untrustedWorkspaces: limited` posture (completion/hover are safe in untrusted
workspaces). `#![forbid(unsafe_code)]` holds in every new module.

## The resolver model (`model.rs`)

- **`TypeTable<'t>`** — built per request by scanning every `OpenDoc`'s tree for type
  declarations (top-level and nested), keyed by **simple name** → `TypeDecl`.
  Built current-doc-first; first insertion wins on name collision.
- **`TypeDecl<'t>`** — from a `class/interface/enum/record/annotation_type_declaration`
  node: `name`, kind, `supers: Vec<String>` (extends + implements simple names), the
  declaration node, and its members.
- **`Member<'t>`** — `name`, `MemberKind` (Field / Method / NestedType), the source node
  (for signature + Javadoc), and `is_static`. Methods keep all overloads.

### Member collection

Given a `TypeDecl`, collect its own members, then walk `supers` through the
`TypeTable` (visited-set guards cycles; bounded depth), stopping at any super not in
the table (a JAR/JDK type — absent until the next sub-project). Dedup by full rendered
signature so an override hides the inherited copy but overloads survive.

## Resolution (`resolve.rs`)

- **Enclosing type** — climb ancestors from the cursor to the nearest type declaration;
  map to its `TypeDecl` (current doc).
- **Scope bindings** — walk enclosing nodes outward collecting, with inner shadowing
  outer: `local_variable_declaration`s whose `start_byte < cursor` in each enclosing
  block, `formal_parameter`s, `enhanced_for` / classic-for loop variables, catch params,
  and the enclosing type's fields (own + inherited). Each binding carries its declared
  **type node** (used to resolve member receivers) or `var` (→ unresolved).
- **In-scope type names** — imports (`import_declaration` last identifier), same-file +
  open-file top-level type names, enclosing/nested type names.
- **Receiver → type** — for member completion/hover, find the receiver expression and
  resolve:
  - `this` → enclosing `TypeDecl`; `super` → enclosing type's first super resolved via
    `TypeTable`.
  - `identifier` → matching scope binding → declared type's **simple name** → `TypeTable`.
  - `object_creation_expression` (`new T(...)`) → `T` simple name → `TypeTable`.
  - `field_access` → resolve object's type, find the named field member, take that
    field's declared type → `TypeTable`. (One field hop; not a method chain.)
  - `parenthesized_expression` → unwrap. `method_invocation` / unresolved → `None`.

### Receiver detection for incomplete input

After `.` the buffer is often `recv.` with a MISSING field node. Detection is
text-anchored, not shape-dependent:

1. `cursor = index.offset(pos)`; scan back over whitespace; if the previous
   non-whitespace byte is not `.`, it is **not** member completion → scope completion.
2. Let `dot` = that byte. The receiver is the **largest node whose `end_byte == dot`**:
   descend at `dot-1`, then climb while `parent.end_byte() == dot`. This yields `d`,
   `a.b`, `new Demo()`, `this`, etc., and excludes the `field_access` that contains the
   dot (its MISSING field makes `end_byte > dot`).

## Completion behavior (`completion.rs`)

**Member completion** (receiver resolved): one item per collected member.
- Method → `kind = METHOD`, `label = name`, `detail = <signature>` (e.g. `int sum()`),
  `documentation` = Javadoc markdown; insert `name()` for zero-arg, else a
  `name($1)` snippet (`InsertTextFormat::SNIPPET`).
- Field → `kind = FIELD`, `detail = <type> name`, insert `name`.
- Nested type → `kind = CLASS/INTERFACE/ENUM`.
- Unresolved receiver → empty member list (the editor shows nothing rather than wrong
  suggestions).

**Scope completion** (no resolvable `.` before cursor): visible bindings + keywords.
- locals/params/for-vars → `VARIABLE`, `detail = <type> name`.
- enclosing-type fields (own + inherited) → `FIELD`; own/inherited methods → `METHOD`.
- in-scope type names → `CLASS/INTERFACE/ENUM`.
- Java keywords → `KEYWORD`, from a static list.
- Return the full list (`is_incomplete = false`) and let the client prefix-filter;
  open-files-only keeps it small.

## Hover behavior (`hover.rs`)

Find the identifier/name node under the cursor and pick the declaration to show:
- a declaration's own name → that declaration;
- `recv.member` with cursor on `member` → resolve receiver type, find the member;
- a simple identifier reference → resolve binding (local/param/field/type) to its
  declaration.

Render:
````
```java
<reconstructed signature>
```

<Javadoc as markdown>
````
as `MarkupKind::Markdown`, with the hover `range` set to the identifier node. Javadoc =
nearest preceding sibling `block_comment` whose text starts with `/**`; strip `/**`,
`*/`, and per-line `*` margins; emit the remainder (no `@tag` reformatting yet). No
declaration found → `Ok(None)`.

## Signature reconstruction (`signature.rs`)

Source-text-based so generics render as written:
- **method** → `[modifiers ]<return> <name>(<ptype> <pname>, …)`, optional `throws …`.
- **constructor** → `<name>(<params>)`.
- **field / local / param** → `[modifiers ]<type> <name>`.
- **type** → `<class|interface|enum|record> <Name>[ extends …][ implements …]`.

Modifiers = the keyword children of the `modifiers` node only (annotations skipped).
Type/param text = source slice with internal whitespace collapsed to single spaces.

## Error handling, performance, security

- **Robust:** every step is `Option`-returning and best-effort; failure → `Ok(None)`
  or partial results, never a panic. Tolerates tree-sitter ERROR/MISSING nodes,
  cursor at EOF, cursor in a comment/string (→ no member completion). Inheritance
  walking is cycle-guarded and depth-bounded.
- **Compute-minded:** completion/hover are on-demand LSP requests (never background).
  Per request the cost is O(open files × declarations), bounded by open-files-only.
  Work is synchronous under the lock with no `.await` held, so it never blocks on or
  contends with async parsing. No caching this sub-project (rebuild is cheap); a
  version-keyed `TypeTable` cache is a future optimization.
- **Secure:** no new I/O surface — only in-memory open documents are read; no build
  execution, no archive/bytecode parsing (that arrives with its own threat-model work
  in sub-project 2). `#![forbid(unsafe_code)]` preserved.

## Testing strategy

Unit tests in `jvl-syntax` (same inline `#[cfg(test)]` style as today), over Java
source fixtures:
- **signature:** method/field/constructor/type rendering incl. generics, modifiers,
  `throws`.
- **scope completion:** locals visible only before the cursor; params; own + inherited
  fields; shadowing; keywords present; in-scope type names.
- **member completion:** `this.`, `super.`, `ident.`, `new T().`, `this.field.`;
  inherited members; instance + static; override dedup vs. overload survival;
  unresolved receiver → empty; method-chain / `var` → empty (deferred markers).
- **hover:** declaration name; reference → declaration; member access; field/method/type;
  Javadoc extraction + margin stripping; no-doc case; reference resolving across open
  files.
- **robustness:** malformed/partial input, cursor at EOF, cursor in a comment, ERROR
  nodes → no panic, graceful empty/None.

Manual acceptance in the Extension Development Host: `Demo.java` plus a second open
file declaring a supertype; verify dot-completion, scope completion, and hover preview.

## Build order

1. `signature.rs` + `model.rs` (+ tests)
2. `resolve.rs` (+ tests)
3. `completion.rs` + `hover.rs` (+ tests)
4. Server wiring: `completion_provider` (trigger `.`) + `hover_provider`, `completion`
   / `hover` handlers building the `OpenDoc` slice under the `documents` lock.
5. Adversarial multi-dimension review (correctness, LSP conformance, threat model,
   robustness, idioms); verify & fix.
