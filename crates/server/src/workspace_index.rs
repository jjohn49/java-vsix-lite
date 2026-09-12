//! Lazy, bounded index of top-level types in unopened workspace files.
//! It scans filenames and small headers, skips unsafe paths, and lets open documents shadow disk.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use tower_lsp_server::ls_types::SymbolKind;

/// Hard cap on index entries; once reached, walking stops and
/// [`WorkspaceIndex::truncated`] is set so callers can warn the user.
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
    /// Set by `invalidate()` to force the next `ensure_built` to rebuild
    /// regardless of `root_mtimes`, since editing a file in place doesn't
    /// always change its directory's mtime.
    dirty: bool,
    /// Bumped by every `invalidate()` call; a rebuild clears `dirty` only
    /// if this counter didn't change during the walk, so a racing
    /// invalidation is never lost.
    dirty_epoch: u64,
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

    /// Whether the last build stopped early because the cap was reached.
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

    /// Force the next [`Self::ensure_built`] call to rebuild regardless of
    /// root mtimes, since editing a file in place doesn't always change
    /// its containing directory's mtime.
    pub(crate) fn invalidate(&self) {
        let mut inner = self.inner.lock().expect("workspace index poisoned");
        inner.dirty = true;
        inner.dirty_epoch += 1;
    }

    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("workspace index poisoned")
            .entries
            .len()
    }

    /// Build lazily or rebuild after invalidation/root changes.
    /// The bounded, cancellable walk rejects paths outside `boundary`.
    pub(crate) async fn ensure_built(&self, roots: &[PathBuf], boundary: Option<&Path>) {
        let (should_rebuild, epoch_before) = {
            let inner = self.inner.lock().expect("workspace index poisoned");
            (needs_rebuild(&inner, roots), inner.dirty_epoch)
        };
        if !should_rebuild {
            return;
        }

        let boundary_canon = match boundary {
            Some(boundary) => match fs::canonicalize(boundary) {
                Ok(boundary) => Some(boundary),
                Err(_) => return,
            },
            None => None,
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut incomplete = false;
        let mut root_mtimes = HashMap::new();
        for root in roots {
            root_mtimes.insert(root.clone(), root_mtime(root));
            if entries.len() >= self.cap {
                truncated = true;
                continue; // still record every root's mtime, just stop scanning
            }
            let cap = self.cap;
            let walk = crate::fs_scan::walk_java_files(root, boundary_canon.as_deref(), |path| {
                if entries.len() >= cap {
                    return false;
                }
                match index_file(path) {
                    Ok(Some(entry)) => entries.push(entry),
                    Ok(None) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => incomplete = true,
                }
                true
            })
            .await;
            if walk.stopped_early {
                truncated = true;
            }
            if walk.io_errors {
                incomplete = true;
            }
        }

        let mut inner = self.inner.lock().expect("workspace index poisoned");
        inner.entries = entries;
        inner.root_mtimes = root_mtimes;
        inner.truncated = truncated;
        inner.built = true;
        inner.generation += 1;
        // Retry after a racing invalidation or incomplete IO.
        inner.dirty = inner.dirty_epoch != epoch_before || incomplete;
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

    /// Every indexed path for an exact simple name. Currently used only by
    /// this module's tests; `find_type` covers the package-exact lookup
    /// production code needs.
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

    /// The file declaring the exact `(package, simple_name)` pair. Returns
    /// `None` if more than one file matches — never guess which one the
    /// caller meant.
    pub(crate) fn find_type(&self, package: &str, simple_name: &str) -> Option<PathBuf> {
        let inner = self.inner.lock().expect("workspace index poisoned");
        let mut matches = inner
            .entries
            .iter()
            .filter(|e| e.package == package && e.simple_name == simple_name);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first.path.clone())
    }

    /// Entries whose simple name starts with `prefix` (case-insensitive),
    /// shortest-name-first, capped at `limit`; the bool reports whether
    /// the cap cut candidates off.
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
    /// (`""` = roots). Nested types aren't tracked by this index, so only
    /// top-level types are ever returned.
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

/// A source root's own (non-recursive) mtime, or `None` if it doesn't
/// exist yet (e.g. `src/test/java` in a source-only project).
fn root_mtime(root: &Path) -> Option<SystemTime> {
    fs::metadata(root).and_then(|m| m.modified()).ok()
}

fn needs_rebuild(inner: &Inner, roots: &[PathBuf]) -> bool {
    if !inner.built || inner.dirty {
        return true;
    }
    if inner.root_mtimes.len() != roots.len() {
        return true;
    }
    roots
        .iter()
        .any(|root| inner.root_mtimes.get(root).copied() != Some(root_mtime(root)))
}

/// Index a `.java` filename and its package/type keyword from a small header.
fn index_file(path: &Path) -> io::Result<Option<SymbolEntry>> {
    let Some(simple_name) = path.file_stem().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if simple_name.is_empty() {
        return Ok(None);
    }
    let mut file = fs::File::open(path)?;
    let mut buf = vec![0u8; HEADER_BYTES];
    let read = file.read(&mut buf)?;
    buf.truncate(read);
    let header = String::from_utf8_lossy(&buf);

    Ok(Some(SymbolEntry {
        simple_name: simple_name.to_string(),
        package: extract_package(&header).unwrap_or_default(),
        path: path.to_path_buf(),
        kind: detect_kind(&header, simple_name),
    }))
}

/// Plain line scan for a `package a.b.c;` declaration. Returns `None` if
/// no such line appears in `header` (missing, or past the header budget).
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
/// mirroring `jvl_syntax::document_symbols`'s declaration -> kind mapping.
const KIND_KEYWORDS: [(&str, SymbolKind); 5] = [
    ("class", SymbolKind::CLASS),
    ("interface", SymbolKind::INTERFACE),
    ("enum", SymbolKind::ENUM),
    ("record", SymbolKind::CLASS),
    ("@interface", SymbolKind::INTERFACE),
];

/// Best-effort declaration kind: the earliest `keyword simple_name` match
/// in the header, defaulting to `CLASS` if nothing matches.
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

/// Case-insensitive substring or camel-hump match (`FoBa` matches `FooBar`).
/// Empty queries match every candidate.
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

        // Without de-duplication by canonical path, `alias` would cause
        // every file under `real` to be indexed twice.
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

        // Without visited-dir tracking, this symlink cycle would make the
        // walk re-descend forever, duplicating entries until the cap
        // falsely reported truncation.
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

    /// `invalidate()` forces a rebuild without an mtime change: a nested
    /// file the mtime check alone wouldn't notice is picked up
    /// immediately.
    #[tokio::test]
    async fn invalidate_forces_rebuild_without_mtime_change() {
        let root = temp_dir("explicit-invalidation");
        let src = root.join("src/main/java");
        // Only `p/q`'s mtime changes when `New.java` is added, never
        // `src`'s (the root the mtime check actually watches).
        std::fs::create_dir_all(src.join("p/q")).expect("create nested dirs");

        let index = WorkspaceIndex::new();
        built(&index, std::slice::from_ref(&src), &root).await;
        let first_generation = index.generation();
        assert_eq!(index.len(), 0);

        std::fs::write(src.join("p/q/New.java"), "package p.q;\nclass New {}\n")
            .expect("write nested file");

        built(&index, std::slice::from_ref(&src), &root).await;
        assert_eq!(
            index.generation(),
            first_generation,
            "no invalidation yet: a nested addition must not trigger a rebuild on its own"
        );
        assert_eq!(index.len(), 0, "the new nested file must not be found yet");

        index.invalidate();
        built(&index, std::slice::from_ref(&src), &root).await;
        assert!(
            index.generation() > first_generation,
            "invalidate() must force a rebuild"
        );
        assert_eq!(
            index.len(),
            1,
            "the nested file is found only after invalidate()"
        );
        assert!(index.find_type("p.q", "New").is_some());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_file_keeps_index_dirty_until_retry_succeeds() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("retry-read-error");
        let src = root.join("src");
        write(&root, "src/Good.java", "class Good {}\n");
        let blocked = write(&root, "src/Blocked.java", "class Blocked {}\n");
        let original = fs::metadata(&blocked).expect("metadata").permissions();
        let mut denied = original.clone();
        denied.set_mode(0o0);
        fs::set_permissions(&blocked, denied).expect("remove read permission");

        // Mode bits do not stop a privileged reader, and CI runs this suite as
        // root inside a container. No portable trick induces a read error for
        // one either: the scanner descends into anything `is_dir()`, so a
        // directory named `Blocked.java` gets walked rather than read. Probe
        // the precondition rather than assuming it: asserting `len() == 1`
        // where the blocked file is in fact readable fails for a reason that
        // has nothing to do with the behaviour under test. The message shows
        // under `--nocapture`; libtest offers no way to report a skip.
        if fs::read(&blocked).is_ok() {
            eprintln!(
                "SKIP unreadable_file_keeps_index_dirty_until_retry_succeeds: \
                 this process can read a 0o000 file (privileged/root), so the \
                 read-error path cannot be exercised here"
            );
            let _ = fs::set_permissions(&blocked, original);
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let index = WorkspaceIndex::new();
        built(&index, std::slice::from_ref(&src), &root).await;
        let first_generation = index.generation();
        assert_eq!(index.len(), 1);
        assert!(
            index.inner.lock().expect("workspace index poisoned").dirty,
            "a read error must leave the index dirty"
        );

        fs::set_permissions(&blocked, original).expect("restore read permission");
        built(&index, std::slice::from_ref(&src), &root).await;
        assert!(index.generation() > first_generation);
        assert_eq!(index.matching("Blocked").len(), 1);
        assert!(!index.inner.lock().expect("workspace index poisoned").dirty);

        let _ = fs::remove_dir_all(&root);
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
        // Only one S-match exists, so capping at 1 must not report
        // truncation.
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
