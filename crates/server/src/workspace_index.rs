//! Workspace symbols via a lazy, bounded index of top-level Java types.
//!
//! The index is built the first time a `workspace/symbol` request arrives —
//! never at startup (open-files-first: zero cost until asked for) — and is
//! kept intentionally shallow:
//!
//! - **Discovery** walks only the workspace's source roots (the conventional
//!   `src/main/java`/`src/test/java`, plus any root inferred from an open
//!   document's package declaration — see `Backend::source_roots` in
//!   `main.rs`, reused as-is), using the shared traversal in `fs_scan`
//!   (`fs_scan::walk_java_files`) — the same walk `references`'s workspace
//!   prefilter uses, just with a different per-file callback. `target/`,
//!   `build/`, `.git`, and any hidden (dot-prefixed) directory are skipped.
//! - **Per file**, the *filename* gives the public type's simple name — by
//!   Java convention a `.java` file's name matches the type it declares — so
//!   there is no parse at all. Only the first [`HEADER_BYTES`] of the file
//!   are read, to plain-line-scan a `package` declaration and best-effort
//!   sniff the declaration keyword (`class`/`interface`/`enum`/`record`/
//!   `@interface`) immediately preceding that name.
//! - **Caps**: at most [`DEFAULT_CAP`] entries; the walk stops early and the
//!   index is marked [`WorkspaceIndex::truncated`] rather than growing
//!   without bound.
//! - **Invalidation** is coarse and cheap: each source root's own (top-level)
//!   mtime is recorded, and a later `ensure_built` call only re-walks
//!   everything if some root's mtime no longer matches (a `stat`, not a
//!   walk). A monotonic generation counter marks each rebuild.
//! - **Safety**: every path is canonicalized and checked against the
//!   workspace boundary before being followed or indexed, so a symlink that
//!   escapes the workspace root is skipped rather than read — see
//!   `fs_scan::walk_java_files` for the shared symlink-escape and
//!   symlink-cycle protection this and `references` both rely on.
//!
//! Open documents are *not* looked up here — `jvl_syntax::document_symbols`
//! is always more precise (a real parse) and must shadow whatever the index
//! says about that same file; see the `symbol` handler in `main.rs`.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use tower_lsp_server::ls_types::SymbolKind;

/// Hard cap on the number of entries the index will hold. Walking stops as
/// soon as this many files have been indexed; the index is marked
/// [`WorkspaceIndex::truncated`] so a caller can tell the user once rather
/// than silently dropping results in a very large workspace.
pub(crate) const DEFAULT_CAP: usize = 5_000;

/// Bytes read from the head of each candidate `.java` file — enough for a
/// `package` declaration and the type's own keyword, never a full parse.
const HEADER_BYTES: usize = 4096;

/// One top-level Java type discovered by filename + header scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolEntry {
    pub(crate) simple_name: String,
    /// Dotted package path, or empty when no `package` declaration was found
    /// within the header budget (the entry is still listed by name).
    pub(crate) package: String,
    pub(crate) path: PathBuf,
    pub(crate) kind: SymbolKind,
}

/// The mutable, lockable state rebuilt wholesale on invalidation.
#[derive(Default)]
struct Inner {
    entries: Vec<SymbolEntry>,
    /// Each source root's own (top-level, non-recursive) mtime at the time
    /// of the last build — the cheap signal `ensure_built` checks before
    /// deciding to re-walk anything.
    root_mtimes: HashMap<PathBuf, Option<SystemTime>>,
    built: bool,
    truncated: bool,
    generation: u64,
}

/// Lazy, bounded workspace symbol index — see the module docs.
pub(crate) struct WorkspaceIndex {
    cap: usize,
    inner: Mutex<Inner>,
}

impl WorkspaceIndex {
    pub(crate) fn new() -> Self {
        Self::with_cap(DEFAULT_CAP)
    }

    /// A lower cap than [`DEFAULT_CAP`], so the truncation behavior can be
    /// exercised with a handful of files instead of generating 5,000+.
    pub(crate) fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Whether the last build stopped early because [`Self::cap`] was
    /// reached (`self.cap`, i.e. `DEFAULT_CAP` unless constructed via
    /// [`Self::with_cap`]).
    pub(crate) fn truncated(&self) -> bool {
        self.inner
            .lock()
            .expect("workspace index poisoned")
            .truncated
    }

    /// Bumped every time the index is (re)built — exposed mainly so tests
    /// can observe that an mtime change actually triggered a rebuild.
    pub(crate) fn generation(&self) -> u64 {
        self.inner
            .lock()
            .expect("workspace index poisoned")
            .generation
    }

    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("workspace index poisoned")
            .entries
            .len()
    }

    /// Build the index if it has never been built, or rebuild it if any
    /// root's top-level mtime has changed since the last build. A no-op
    /// (just a handful of `stat`s) on the common case: every query after the
    /// first, with nothing added/removed directly under a source root.
    ///
    /// `boundary`, when given, is the directory paths must stay under
    /// (canonicalized once here) — anything that canonicalizes outside it,
    /// including a symlink pointing outside it, is skipped rather than
    /// followed or read.
    ///
    /// Cancellable in practice via tower-lsp's own request-cancellation
    /// (dropping this future mid-walk): the walk yields to the runtime
    /// periodically (see [`crate::fs_scan::walk_java_files`]) instead of
    /// running one uninterrupted synchronous burst, so a dropped future
    /// actually stops promptly.
    pub(crate) async fn ensure_built(&self, roots: &[PathBuf], boundary: Option<&Path>) {
        let should_rebuild = {
            let inner = self.inner.lock().expect("workspace index poisoned");
            needs_rebuild(&inner, roots)
        };
        if !should_rebuild {
            return;
        }

        let boundary_canon = boundary.and_then(|b| fs::canonicalize(b).ok());
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut root_mtimes = HashMap::new();
        for root in roots {
            root_mtimes.insert(root.clone(), root_mtime(root));
            if entries.len() >= self.cap {
                truncated = true;
                continue; // still record every root's mtime, just stop scanning
            }
            let cap = self.cap;
            let stopped_early =
                crate::fs_scan::walk_java_files(root, boundary_canon.as_deref(), |path| {
                    if entries.len() >= cap {
                        return false;
                    }
                    if let Some(entry) = index_file(path) {
                        entries.push(entry);
                    }
                    true
                })
                .await;
            if stopped_early {
                truncated = true;
            }
        }

        let mut inner = self.inner.lock().expect("workspace index poisoned");
        inner.entries = entries;
        inner.root_mtimes = root_mtimes;
        inner.truncated = truncated;
        inner.built = true;
        inner.generation += 1;
    }

    /// Entries whose simple name [`matches_query`] (case-insensitive
    /// substring, or camel-hump prefix match).
    pub(crate) fn matching(&self, query: &str) -> Vec<SymbolEntry> {
        let inner = self.inner.lock().expect("workspace index poisoned");
        inner
            .entries
            .iter()
            .filter(|e| matches_query(query, &e.simple_name))
            .cloned()
            .collect()
    }

    /// Every indexed path for an exact simple name — the "simple name ->
    /// paths" lookup a future add-import feature (and, if it doesn't already
    /// have one, go-to-definition's unopened-file ladder step) can reuse
    /// rather than re-walking the workspace itself. `project_symbols`
    /// needed package-exact lookup instead (see `find_type`), so this
    /// still has no production caller — only this module's own tests.
    #[allow(dead_code)]
    pub(crate) fn paths_for_simple_name(&self, name: &str) -> Vec<PathBuf> {
        let inner = self.inner.lock().expect("workspace index poisoned");
        inner
            .entries
            .iter()
            .filter(|e| e.simple_name == name)
            .map(|e| e.path.clone())
            .collect()
    }

    /// The file declaring the exact `(package, simple_name)` pair —
    /// `project_symbols`'s FQN-to-file lookup for closed-file completion.
    /// The first match wins (two same-named top-level types in the same
    /// package is invalid Java; ambiguity here just means "one of them").
    pub(crate) fn find_type(&self, package: &str, simple_name: &str) -> Option<PathBuf> {
        let inner = self.inner.lock().expect("workspace index poisoned");
        inner
            .entries
            .iter()
            .find(|e| e.package == package && e.simple_name == simple_name)
            .map(|e| e.path.clone())
    }

    /// Entries whose simple name starts with `prefix` (case-insensitive),
    /// shortest-name-first, capped at `limit`; the bool reports whether the
    /// cap cut candidates off. The classpath-index counterpart to
    /// `jvl_classpath::Classpath::types_with_prefix`, so project types
    /// complete the same way dependency types do.
    pub(crate) fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<SymbolEntry>, bool) {
        if prefix.is_empty() || limit == 0 {
            return (Vec::new(), false);
        }
        let needle = prefix.to_ascii_lowercase();
        let inner = self.inner.lock().expect("workspace index poisoned");
        let mut hits: Vec<&SymbolEntry> = inner
            .entries
            .iter()
            .filter(|e| {
                e.simple_name.len() >= needle.len()
                    && e.simple_name[..needle.len()].eq_ignore_ascii_case(&needle)
            })
            .collect();
        hits.sort_by(|a, b| {
            (a.simple_name.len(), &a.simple_name).cmp(&(b.simple_name.len(), &b.simple_name))
        });
        let truncated = hits.len() > limit;
        (hits.into_iter().take(limit).cloned().collect(), truncated)
    }

    /// Immediate child packages and top-level types of a dotted package
    /// (`""` = roots) — the project-source counterpart to
    /// `jvl_classpath::Classpath::package_children`, for import-path
    /// completion across project files. Nested types aren't tracked by this
    /// index (it only records the filename-matching top-level type per
    /// file), so only top-level types are ever returned here.
    pub(crate) fn package_children(&self, package: &str) -> (Vec<String>, Vec<SymbolEntry>) {
        let inner = self.inner.lock().expect("workspace index poisoned");
        let mut subpackages: Vec<String> = Vec::new();
        let mut types = Vec::new();
        for e in &inner.entries {
            if e.package == package {
                types.push(e.clone());
                continue;
            }
            let prefix = if package.is_empty() {
                String::new()
            } else {
                format!("{package}.")
            };
            if let Some(rest) = e.package.strip_prefix(&prefix) {
                if !rest.is_empty() {
                    let segment = rest.split('.').next().unwrap_or(rest).to_string();
                    if !subpackages.contains(&segment) {
                        subpackages.push(segment);
                    }
                }
            }
        }
        subpackages.sort();
        types.sort_by(|a, b| a.simple_name.cmp(&b.simple_name));
        (subpackages, types)
    }
}

/// A source root's own mtime (not recursive — just the directory entry
/// itself), or `None` if it doesn't exist / can't be stat'd (a root that
/// isn't there yet, e.g. `src/test/java` in a source-only project).
fn root_mtime(root: &Path) -> Option<SystemTime> {
    fs::metadata(root).and_then(|m| m.modified()).ok()
}

fn needs_rebuild(inner: &Inner, roots: &[PathBuf]) -> bool {
    if !inner.built {
        return true;
    }
    if inner.root_mtimes.len() != roots.len() {
        return true;
    }
    roots
        .iter()
        .any(|root| inner.root_mtimes.get(root).copied() != Some(root_mtime(root)))
}

/// Index a single `.java` file: simple name from the filename (no parse),
/// package + kind from at most [`HEADER_BYTES`] of its content.
fn index_file(path: &Path) -> Option<SymbolEntry> {
    let simple_name = path.file_stem()?.to_str()?.to_string();
    if simple_name.is_empty() {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; HEADER_BYTES];
    let read = file.read(&mut buf).ok()?;
    buf.truncate(read);
    let header = String::from_utf8_lossy(&buf);

    let package = extract_package(&header).unwrap_or_default();
    let kind = detect_kind(&header, &simple_name);
    Some(SymbolEntry {
        simple_name,
        package,
        path: path.to_path_buf(),
        kind,
    })
}

/// Plain line scan for a `package a.b.c;` declaration — no parsing, just
/// enough to strip the keyword, trim, and cut at the `;`. `None` if no such
/// line appears in `header` (either there truly isn't one, or — for a huge
/// leading comment — it fell past the header budget).
fn extract_package(header: &str) -> Option<String> {
    for line in header.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("package") else {
            continue;
        };
        // Require a word boundary after `package` (reject e.g. `packagex`).
        if !rest.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let rest = rest.trim_start();
        let Some(end) = rest.find(';') else {
            continue;
        };
        let dotted: String = rest[..end].split_whitespace().collect();
        if !dotted.is_empty() {
            return Some(dotted);
        }
    }
    None
}

/// `(keyword, resulting SymbolKind)` pairs scanned for in the header,
/// mirroring `jvl_syntax::document_symbols`' own declaration -> kind mapping
/// (`class_declaration`/`record_declaration` -> `CLASS`,
/// `interface_declaration`/`annotation_type_declaration` -> `INTERFACE`,
/// `enum_declaration` -> `ENUM`).
const KIND_KEYWORDS: [(&str, SymbolKind); 5] = [
    ("class", SymbolKind::CLASS),
    ("interface", SymbolKind::INTERFACE),
    ("enum", SymbolKind::ENUM),
    ("record", SymbolKind::CLASS),
    ("@interface", SymbolKind::INTERFACE),
];

/// Best-effort declaration kind: the earliest `keyword simple_name` match in
/// the header (e.g. `"class Foo"`, `"public interface Foo"` — the match
/// starts at `interface`, modifiers before it don't matter), defaulting to
/// `CLASS` if nothing matches (e.g. the package line fell past the header
/// budget and took the declaration keyword with it).
fn detect_kind(header: &str, simple_name: &str) -> SymbolKind {
    let mut best: Option<(usize, SymbolKind)> = None;
    for (keyword, kind) in KIND_KEYWORDS {
        let needle = format!("{keyword} {simple_name}");
        if let Some(pos) = header.find(&needle) {
            let better = best.map(|(best_pos, _)| pos < best_pos).unwrap_or(true);
            if better {
                best = Some((pos, kind));
            }
        }
    }
    best.map(|(_, kind)| kind).unwrap_or(SymbolKind::CLASS)
}

/// `workspace/symbol` query-matching rule: `candidate` matches `query` if
/// either
///
/// 1. it contains `query` as a case-insensitive substring, or
/// 2. it **camel-hump prefix matches**: split both strings into "humps" — a
///    new hump starts at index 0 and at every uppercase letter — then each of
///    `query`'s humps must be a case-insensitive prefix of *some* hump of
///    `candidate`, consumed left-to-right without reordering or reuse. E.g.
///    `"FoBa"` matches `"FooBar"` (`"Fo"` prefixes `"Foo"`, `"Ba"` prefixes
///    `"Bar"`), as does the shorter `"FB"`; `"BaFo"` does not (wrong order).
///
/// An empty `query` matches everything (defensive default; the LSP spec
/// requires clients send a non-empty query, but nothing stops one from
/// sending `""`).
pub(crate) fn matches_query(query: &str, candidate: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if candidate.to_lowercase().contains(&query.to_lowercase()) {
        return true;
    }
    camel_hump_matches(query, candidate)
}

fn camel_hump_matches(query: &str, candidate: &str) -> bool {
    let query_humps = split_humps(query);
    let candidate_humps = split_humps(candidate);
    let mut candidate_idx = 0;
    for query_hump in &query_humps {
        let mut matched = false;
        while candidate_idx < candidate_humps.len() {
            let candidate_hump = candidate_humps[candidate_idx];
            candidate_idx += 1;
            if candidate_hump
                .to_lowercase()
                .starts_with(&query_hump.to_lowercase())
            {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    true
}

/// Split `s` into "humps": a new hump starts at index 0 and at every
/// uppercase letter, so `"FooBar"` -> `["Foo", "Bar"]` and an all-lowercase
/// string is a single hump.
fn split_humps(s: &str) -> Vec<&str> {
    let mut humps = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if i == 0 || c.is_uppercase() {
            if let Some(prev_start) = start {
                humps.push(&s[prev_start..i]);
            }
            start = Some(i);
        }
    }
    if let Some(prev_start) = start {
        humps.push(&s[prev_start..]);
    }
    humps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jvl-workspace-index-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, contents).expect("write file");
        path
    }

    async fn built(index: &WorkspaceIndex, roots: &[PathBuf], boundary: &Path) {
        index.ensure_built(roots, Some(boundary)).await;
    }

    #[tokio::test]
    async fn indexes_top_level_types_with_package_and_kind_nested_absent() {
        let root = temp_dir("build");
        write(
            &root,
            "src/main/java/p/Foo.java",
            "package p;\n\npublic class Foo {\n  class Inner {}\n}\n",
        );
        write(
            &root,
            "src/main/java/p/q/Bar.java",
            "package p.q;\n\npublic interface Bar {}\n",
        );
        write(
            &root,
            "src/main/java/p/q/Color.java",
            "package p.q;\n\npublic enum Color { RED, GREEN }\n",
        );
        write(
            &root,
            "src/main/java/p/q/Point.java",
            "package p.q;\n\npublic record Point(int x, int y) {}\n",
        );
        write(
            &root,
            "src/main/java/p/Marker.java",
            "package p;\n\npublic @interface Marker {}\n",
        );

        let index = WorkspaceIndex::new();
        built(&index, &[root.join("src/main/java")], &root).await;

        assert_eq!(index.len(), 5, "expected exactly the 5 top-level types");
        assert!(
            index.paths_for_simple_name("Inner").is_empty(),
            "a member type must not get its own entry"
        );

        let foo = index.matching("Foo");
        assert_eq!(foo.len(), 1);
        assert_eq!(foo[0].package, "p");
        assert_eq!(foo[0].kind, SymbolKind::CLASS);

        let bar = index.matching("Bar");
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].package, "p.q");
        assert_eq!(bar[0].kind, SymbolKind::INTERFACE);

        let color = index.matching("Color");
        assert_eq!(color.len(), 1);
        assert_eq!(color[0].kind, SymbolKind::ENUM);

        let point = index.matching("Point");
        assert_eq!(point.len(), 1);
        assert_eq!(point[0].kind, SymbolKind::CLASS);

        let marker = index.matching("Marker");
        assert_eq!(marker.len(), 1);
        assert_eq!(marker[0].kind, SymbolKind::INTERFACE);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn package_within_header_budget_indexes_package_beyond_it_is_best_effort_empty() {
        let root = temp_dir("header");
        let padding = "// filler line to pad out the header comment\n".repeat(20);
        assert!(padding.len() < HEADER_BYTES, "test padding must be < 4KB");
        write(
            &root,
            "src/main/java/Near.java",
            &format!("/*\n{padding}*/\npackage near;\npublic class Near {{}}\n"),
        );

        let huge_padding = "x".repeat(HEADER_BYTES + 512);
        write(
            &root,
            "src/main/java/Far.java",
            &format!("/*\n{huge_padding}\n*/\npackage far;\npublic class Far {{}}\n"),
        );

        let index = WorkspaceIndex::new();
        built(&index, &[root.join("src/main/java")], &root).await;

        let near = index.matching("Near");
        assert_eq!(near.len(), 1);
        assert_eq!(near[0].package, "near", "package within budget must parse");

        let far = index.matching("Far");
        assert_eq!(
            far.len(),
            1,
            "Far must still be listed by name even though its package fell past the header budget"
        );
        assert_eq!(
            far[0].package, "",
            "package beyond budget is best-effort empty"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cap_truncates_and_marks_truncated() {
        let root = temp_dir("cap");
        for i in 0..15 {
            write(
                &root,
                &format!("src/File{i}.java"),
                &format!("class File{i} {{}}\n"),
            );
        }

        let index = WorkspaceIndex::with_cap(10);
        built(&index, &[root.join("src")], &root).await;

        assert_eq!(index.len(), 10);
        assert!(index.truncated());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escaping_the_root_is_skipped() {
        let root = temp_dir("symlink-root");
        let secret_root = temp_dir("symlink-secret");
        write(&secret_root, "Secret.java", "class Secret {}\n");

        std::os::unix::fs::symlink(&secret_root, root.join("escape"))
            .expect("create escaping symlink");

        let index = WorkspaceIndex::new();
        built(&index, std::slice::from_ref(&root), &root).await;

        assert!(
            index.matching("Secret").is_empty(),
            "a symlink escaping the workspace root must not be followed"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&secret_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sibling_symlink_inside_root_indexes_each_file_once() {
        let root = temp_dir("symlink-sibling");
        write(&root, "real/Foo.java", "class Foo {}\n");

        // A symlink inside the root pointing at another in-boundary
        // directory: without de-duplication by canonical path, every file
        // under `real` would be indexed a second time via `alias`.
        std::os::unix::fs::symlink(root.join("real"), root.join("alias"))
            .expect("create in-boundary sibling symlink");

        let index = WorkspaceIndex::new();
        built(&index, std::slice::from_ref(&root), &root).await;

        assert_eq!(
            index.len(),
            1,
            "Foo.java must be indexed exactly once despite the sibling symlink"
        );
        let foo = index.matching("Foo");
        assert_eq!(foo.len(), 1, "no duplicate entries for Foo");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ancestor_symlink_cycle_terminates_without_duplicates_or_truncation() {
        let root = temp_dir("symlink-cycle");
        write(&root, "src/Foo.java", "class Foo {}\n");

        // A symlink under `src` pointing back at an ancestor (`root`)
        // creates a cycle: without visited-dir tracking the walk would
        // re-descend into `src` (and back into the cycle) until the cap
        // stopped it, filling the index with duplicates and reporting
        // spurious truncation for what is really a tiny workspace.
        std::os::unix::fs::symlink(&root, root.join("src/back-to-root"))
            .expect("create ancestor symlink cycle");

        let index = WorkspaceIndex::new();
        built(&index, std::slice::from_ref(&root), &root).await;

        assert_eq!(
            index.len(),
            1,
            "the cycle must not produce duplicate entries for Foo"
        );
        assert!(
            !index.truncated(),
            "a tiny workspace with a symlink cycle must not report truncation"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn mtime_change_triggers_lazy_rebuild() {
        let root = temp_dir("invalidation");
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("create src dir");

        let index = WorkspaceIndex::new();
        built(&index, std::slice::from_ref(&src), &root).await;
        let first_generation = index.generation();
        assert_eq!(index.len(), 0);

        // Adding a file directly under `src` changes its own mtime.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(&root, "src/New.java", "class New {}\n");

        built(&index, std::slice::from_ref(&src), &root).await;
        assert!(
            index.generation() > first_generation,
            "root mtime change must trigger a rebuild"
        );
        assert_eq!(index.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn query_matching_substring_and_camel_hump() {
        assert!(matches_query("foobar", "FooBar"));
        assert!(matches_query("FoBa", "FooBar"));
        assert!(matches_query("FB", "FooBar"));
        assert!(!matches_query("xyz", "FooBar"));
        assert!(
            !matches_query("BaFo", "FooBar"),
            "out-of-order humps must not match"
        );
    }

    // --- Project-source symbol layer query methods ---

    async fn person_index() -> (WorkspaceIndex, PathBuf) {
        let root = temp_dir("project-symbols");
        write(
            &root,
            "src/main/java/demo/Person.java",
            "package demo;\npublic class Person {}\n",
        );
        write(
            &root,
            "src/main/java/demo/util/StringUtils.java",
            "package demo.util;\npublic class StringUtils {}\n",
        );
        write(
            &root,
            "src/main/java/demo/util/Other.java",
            "package demo.util;\npublic class Other {}\n",
        );
        let index = WorkspaceIndex::new();
        built(&index, &[root.join("src/main/java")], &root).await;
        (index, root)
    }

    #[tokio::test]
    async fn find_type_locates_exact_package_and_simple_name() {
        let (index, root) = person_index().await;
        let found = index.find_type("demo", "Person").expect("found");
        assert_eq!(found, root.join("src/main/java/demo/Person.java"));
        assert!(index.find_type("demo", "NoSuchType").is_none());
        assert!(
            index.find_type("other.pkg", "Person").is_none(),
            "wrong package must not match"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn types_with_prefix_matches_case_insensitively_and_caps() {
        let (index, root) = person_index().await;
        let (hits, truncated) = index.types_with_prefix("Person", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].simple_name, "Person");
        assert!(!truncated);

        let (hits, _) = index.types_with_prefix("person", 10);
        assert_eq!(hits.len(), 1, "case-insensitive");

        let (hits, truncated) = index.types_with_prefix("S", 1);
        // "StringUtils" matches; capped at 1 with more available is still
        // correctly reported even though there's exactly one S-match here.
        assert_eq!(hits.len(), 1);
        assert!(!truncated);

        assert!(index.types_with_prefix("", 10).0.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn package_children_lists_subpackages_and_types() {
        let (index, root) = person_index().await;
        let (subs, types) = index.package_children("demo");
        assert_eq!(subs, vec!["util".to_string()]);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].simple_name, "Person");

        let (subs, types) = index.package_children("demo.util");
        assert!(subs.is_empty());
        let names: Vec<&str> = types.iter().map(|t| t.simple_name.as_str()).collect();
        assert_eq!(names, vec!["Other", "StringUtils"]);

        let (subs, types) = index.package_children("no.such.pkg");
        assert!(subs.is_empty() && types.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
