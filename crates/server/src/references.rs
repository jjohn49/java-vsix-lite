//! Server-side orchestration for `textDocument/references` — the
//! bounded workspace prefilter that finds candidate files for `jvl_syntax`'s
//! per-file semantic confirm (`jvl_syntax::references_in_doc`) to parse and
//! check. See `Backend::references` in `main.rs` for the full two-tier
//! orchestration (Tier 1 — file-local — never reaches this module at all;
//! only Tier 2 — package-visible/protected/public — needs a workspace scan).
//!
//! The walk itself — skip `target/`/`build/`/`.git`/hidden dirs, canonicalize
//! and boundary-check every path so a symlink can't escape the workspace
//! root, de-duplicate visited canonical directories so an in-boundary
//! symlink can't be walked twice or cycle forever, yield to the runtime
//! periodically for cancellability — is the shared traversal in `fs_scan`
//! (`fs_scan::walk_java_files`), the same walk `workspace_index` uses to
//! build its index; only the leaf action and cap differ here: a full-content
//! substring search against this scan's own file-count/byte caps, rather
//! than a filename + 4KB header scan against the index's entry-count cap.

use std::fs;
use std::path::{Path, PathBuf};

/// Hard cap on the number of `.java` files read in one prefilter scan.
/// Hardcoded for now (config-overridable later, not yet).
pub(crate) const MAX_FILES_SCANNED: usize = 500;

/// Hard cap on total bytes read across all files in one prefilter scan.
pub(crate) const MAX_BYTES_SCANNED: usize = 20 * 1024 * 1024;

/// Hard cap on a single candidate file's size for the substring prefilter.
/// Without this, one pathologically large file (generated code, vendored
/// source, a stray non-source blob sitting under a `.java` name) would be
/// read wholesale by a single `fs::read` before [`MAX_BYTES_SCANNED`] ever
/// gets a chance to notice — checking `metadata().len()` first is a cheap
/// stat, not a full read, so it catches that case up front. Generous for
/// real Java source (a few MB) — a guard against a pathology, not a
/// functional limit.
pub(crate) const MAX_SINGLE_FILE_BYTES: u64 = 4 * 1024 * 1024;

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
    let boundary_canon = match boundary {
        Some(boundary) => match fs::canonicalize(boundary) {
            Ok(boundary) => Some(boundary),
            Err(_) => {
                return Prefilter {
                    files: Vec::new(),
                    truncated: true,
                };
            }
        },
        None => None,
    };
    let needle_bytes = needle.as_bytes();
    let mut files = Vec::new();
    let mut files_scanned = 0usize;
    let mut bytes_scanned = 0usize;
    let mut truncated = false;

    for root in roots {
        let walk = crate::fs_scan::walk_java_files(root, boundary_canon.as_deref(), |path| {
            if files_scanned >= MAX_FILES_SCANNED || bytes_scanned >= MAX_BYTES_SCANNED {
                return false;
            }
            let oversized = fs::metadata(path)
                .map(|meta| meta.len() > MAX_SINGLE_FILE_BYTES)
                .unwrap_or(false);
            if oversized {
                // An unread candidate makes a complete result impossible.
                truncated = true;
                return true;
            }
            match fs::read(path) {
                Ok(bytes) => {
                    files_scanned += 1;
                    bytes_scanned += bytes.len();
                    if contains_subslice(&bytes, needle_bytes) {
                        files.push(path.to_path_buf());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => truncated = true,
            }
            true
        })
        .await;
        if walk.stopped_early || walk.io_errors {
            truncated = true;
        }
        if walk.stopped_early {
            break;
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
    async fn oversized_file_is_skipped_without_a_full_read_and_marks_truncated() {
        let root = temp_dir("oversized");
        // Contains the needle, but too large for the per-file guard — must
        // be excluded from the results (never read for the substring
        // search) even though it *would* match, and must mark the scan
        // `truncated` so a caller needing completeness (`rename`) can't
        // mistake the skip for "no match".
        let mut oversized_contents = "Widget".to_string();
        oversized_contents.push_str(&"x".repeat(MAX_SINGLE_FILE_BYTES as usize));
        write(&root, "src/Big.java", &oversized_contents);
        let small_hit = write(
            &root,
            "src/Small.java",
            "class Small { void Widget() {} }\n",
        );

        let result = prefilter(std::slice::from_ref(&root), Some(&root), "Widget").await;
        assert_eq!(
            result.files,
            vec![small_hit],
            "the oversized file must be excluded; a normal-sized candidate must still be found"
        );
        assert!(
            result.truncated,
            "an oversized candidate must mark the scan truncated, not silently skipped"
        );

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
