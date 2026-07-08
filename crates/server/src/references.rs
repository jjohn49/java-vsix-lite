//! M4.3: server-side orchestration for `textDocument/references` — the
//! bounded workspace prefilter that finds candidate files for `jvl_syntax`'s
//! per-file semantic confirm (`jvl_syntax::references_in_doc`) to parse and
//! check. See `Backend::references` in `main.rs` for the full two-tier
//! orchestration (Tier 1 — file-local — never reaches this module at all;
//! only Tier 2 — package-visible/protected/public — needs a workspace scan).
//!
//! This walk is structurally the same shape as `workspace_index`'s (skip
//! `target/`/`build/`/`.git`/hidden dirs, canonicalize + boundary-check every
//! path so a symlink can't escape the workspace root, de-duplicate visited
//! canonical directories so an in-boundary symlink can't be walked twice or
//! cycle forever, yield to the runtime periodically for cancellability) —
//! it's a separate, independent implementation because its leaf action
//! differs: a full-content substring search rather than a filename + 4KB
//! header scan.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Hard cap on the number of `.java` files read in one prefilter scan.
/// Hardcoded per the task brief (config-overridable later, not yet).
pub(crate) const MAX_FILES_SCANNED: usize = 500;

/// Hard cap on total bytes read across all files in one prefilter scan.
pub(crate) const MAX_BYTES_SCANNED: usize = 20 * 1024 * 1024;

/// Directory names skipped unconditionally while walking (mirrors
/// `workspace_index::SKIPPED_DIR_NAMES`; kept as its own copy since the two
/// walks are otherwise independent implementations).
const SKIPPED_DIR_NAMES: [&str; 3] = ["target", "build", ".git"];

/// The result of one bounded prefilter scan: candidate files whose raw bytes
/// contain the searched identifier, and whether either cap cut the scan
/// short.
pub(crate) struct Prefilter {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) truncated: bool,
}

/// Walk `roots` for `.java` files containing `needle`'s exact bytes anywhere
/// in their content (no parsing — a plain substring search), stopping once
/// [`MAX_FILES_SCANNED`] files have been read or [`MAX_BYTES_SCANNED`] bytes
/// have been read in total. `boundary`, when given, is the directory every
/// candidate path must canonicalize under (guards a symlink escaping the
/// workspace root, same as `workspace_index`). Yields to the async runtime
/// every so often so a cancelled (dropped) caller future actually stops
/// rather than running the whole scan to completion.
pub(crate) async fn prefilter(
    roots: &[PathBuf],
    boundary: Option<&Path>,
    needle: &str,
) -> Prefilter {
    let boundary_canon = boundary.and_then(|b| fs::canonicalize(b).ok());
    let needle_bytes = needle.as_bytes();
    let mut files = Vec::new();
    let mut files_scanned = 0usize;
    let mut bytes_scanned = 0usize;
    let mut truncated = false;
    let mut since_yield = 0usize;

    'walk: for root in roots {
        let mut stack = vec![root.clone()];
        let mut visited_dirs: HashSet<PathBuf> = HashSet::new();
        if let Ok(root_canon) = fs::canonicalize(root) {
            visited_dirs.insert(root_canon);
        }

        while let Some(dir) = stack.pop() {
            let Ok(read_dir) = fs::read_dir(&dir) else {
                continue;
            };
            for dir_entry in read_dir.flatten() {
                if files_scanned >= MAX_FILES_SCANNED || bytes_scanned >= MAX_BYTES_SCANNED {
                    truncated = true;
                    break 'walk;
                }

                let path = dir_entry.path();
                let name = dir_entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name_str.as_ref()) {
                    continue;
                }

                // Canonicalize + boundary check up front — also what keeps a
                // symlink escaping the workspace root from being followed (a
                // dir) or read (a file).
                let Ok(canon) = fs::canonicalize(&path) else {
                    continue;
                };
                if let Some(boundary) = boundary_canon.as_deref() {
                    if !canon.starts_with(boundary) {
                        continue;
                    }
                }

                if canon.is_dir() {
                    if visited_dirs.insert(canon) {
                        stack.push(path);
                    }
                    // Already-walked canonical directory (in-boundary
                    // sibling symlink, or an ancestor symlink cycle): skip.
                } else if name_str.ends_with(".java") {
                    if let Ok(bytes) = fs::read(&path) {
                        files_scanned += 1;
                        bytes_scanned += bytes.len();
                        if contains_subslice(&bytes, needle_bytes) {
                            files.push(path);
                        }
                    }
                }

                since_yield += 1;
                if since_yield >= 32 {
                    since_yield = 0;
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    Prefilter { files, truncated }
}

/// Plain substring search over raw bytes — an empty needle can't legitimately
/// be a Java identifier, so it never matches (defensive: avoids the
/// degenerate "every file matches" case `windows(0)` would otherwise hit).
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jvl-references-prefilter-{label}-{}-{}",
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

    #[tokio::test]
    async fn finds_files_containing_the_needle_and_skips_target_dir() {
        let root = temp_dir("basic");
        let hit = write(&root, "src/Hit.java", "class Hit { void Widget() {} }\n");
        write(&root, "src/Miss.java", "class Miss {}\n");
        write(
            &root,
            "target/Generated.java",
            "class Generated { void Widget() {} }\n",
        );

        let result = prefilter(std::slice::from_ref(&root), Some(&root), "Widget").await;
        assert_eq!(result.files, vec![hit]);
        assert!(!result.truncated);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cap_truncates_and_marks_truncated() {
        let root = temp_dir("cap");
        for i in 0..600 {
            write(
                &root,
                &format!("src/File{i}.java"),
                &format!("class File{i} {{ void other() {{}} }}\n"),
            );
        }

        let result = prefilter(std::slice::from_ref(&root), Some(&root), "nomatch").await;
        assert!(result.truncated, "600 files must exceed the 500-file cap");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escaping_the_root_is_skipped() {
        let root = temp_dir("symlink-root");
        let secret_root = temp_dir("symlink-secret");
        write(
            &secret_root,
            "Secret.java",
            "class Secret { void Widget() {} }\n",
        );

        std::os::unix::fs::symlink(&secret_root, root.join("escape"))
            .expect("create escaping symlink");

        let result = prefilter(std::slice::from_ref(&root), Some(&root), "Widget").await;
        assert!(
            result.files.is_empty(),
            "a symlink escaping the workspace root must not be followed"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&secret_root);
    }
}
