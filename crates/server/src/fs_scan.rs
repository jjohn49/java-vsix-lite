//! Shared depth-first `.java` file traversal used by both the workspace
//! symbol index (`workspace_index`) and the references/rename prefilter
//! (`references`). Both need the exact same walking discipline — skip
//! hidden/`build`/`target`/`.git` dirs, canonicalize and boundary-check every
//! path so a symlink can't escape the workspace root, de-duplicate visited
//! canonical directories so an in-boundary symlink can't be walked twice or
//! cycle forever, and yield to the async runtime periodically for
//! cancellability — but apply a different cap and a different leaf action to
//! what they find (a filename + header scan vs. a full-content substring
//! search), so this module owns only the walk itself: each caller supplies
//! its own per-file callback and decides for itself when its own cap has
//! been reached.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory names skipped unconditionally while walking (build output and
/// VCS metadata never contain source worth indexing or searching). Hidden
/// (dot-prefixed) directories are skipped separately, by name pattern.
pub(crate) const SKIPPED_DIR_NAMES: [&str; 3] = ["target", "build", ".git"];

/// How many directory entries are processed between yields to the async
/// runtime — keeps a cancelled (dropped) caller future stopping promptly
/// rather than running a whole scan to completion in one synchronous burst.
const YIELD_INTERVAL: usize = 32;

/// Depth-first walk of `root` for `.java` files, invoking `on_file` once per
/// candidate file found (in unspecified order).
///
/// `on_file` returns whether the walk should keep going: a caller enforcing
/// its own cap (entry count, byte count, or some combination) checks that
/// cap at the top of its callback and returns `false` once it's already been
/// reached, without doing any further work for that file. The walk stops as
/// soon as `on_file` returns `false`, and this function returns `true` (this
/// root's scan was cut short); it returns `false` once `root`'s subtree has
/// been walked to completion.
///
/// Applies uniformly, regardless of what `on_file` does with each file:
/// - **hidden/build dirs**: any dot-prefixed directory name, plus
///   `target`/`build`/`.git` ([`SKIPPED_DIR_NAMES`]), is skipped outright.
/// - **workspace boundary**: every path is canonicalized before being
///   followed (a directory) or handed to `on_file` (a file); when `boundary`
///   is given — expected already-canonicalized, since callers canonicalize
///   it once rather than once per root — anything that canonicalizes
///   outside it, including a symlink pointing outside it, is skipped rather
///   than followed or read.
/// - **symlink escape/cycle protection**: canonical directory paths already
///   walked (within this one call, i.e. this one root) are tracked; a
///   symlink pointing at an already-visited sibling (would duplicate its
///   contents) or at an ancestor (would cycle forever) resolves to an
///   already-seen canonical path and is skipped rather than re-descended
///   into.
/// - **cancellation**: yields to the tokio runtime every [`YIELD_INTERVAL`]
///   directory entries processed, so a dropped caller future actually stops
///   promptly instead of running one uninterrupted synchronous burst.
pub(crate) async fn walk_java_files(
    root: &Path,
    boundary: Option<&Path>,
    mut on_file: impl FnMut(&Path) -> bool,
) -> bool {
    let mut stack = vec![root.to_path_buf()];
    let mut since_yield = 0usize;
    let mut visited_dirs: HashSet<PathBuf> = HashSet::new();
    if let Ok(root_canon) = fs::canonicalize(root) {
        visited_dirs.insert(root_canon);
    }

    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for dir_entry in read_dir.flatten() {
            let path = dir_entry.path();
            let name = dir_entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name_str.as_ref()) {
                continue;
            }

            // Canonicalize + boundary check up front: this is also what
            // keeps a symlink escaping the workspace root from being
            // followed (a dir) or read (a file).
            let Ok(canon) = fs::canonicalize(&path) else {
                continue;
            };
            if let Some(boundary) = boundary {
                if !canon.starts_with(boundary) {
                    continue;
                }
            }

            if canon.is_dir() {
                if visited_dirs.insert(canon) {
                    // Newly-seen canonical directory: descend into it.
                    stack.push(path);
                }
                // Already-walked canonical directory: a same-root sibling
                // symlink (would duplicate every file under it) or an
                // ancestor symlink (would cycle). Skip either way.
            } else if name_str.ends_with(".java") && !on_file(&path) {
                return true;
            }

            since_yield += 1;
            if since_yield >= YIELD_INTERVAL {
                since_yield = 0;
                tokio::task::yield_now().await;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jvl-fs-scan-{label}-{}-{}",
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
    async fn skips_hidden_and_build_output_dirs() {
        let root = temp_dir("skip-dirs");
        write(&root, "src/Foo.java", "class Foo {}\n");
        write(&root, "target/Generated.java", "class Generated {}\n");
        write(&root, "build/Generated2.java", "class Generated2 {}\n");
        write(&root, ".git/Weird.java", "class Weird {}\n");
        write(&root, ".hidden/Sneaky.java", "class Sneaky {}\n");

        let mut found = Vec::new();
        let root_canon = fs::canonicalize(&root).expect("canonicalize root");
        let truncated = walk_java_files(&root, Some(&root_canon), |path| {
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert!(!truncated);
        assert_eq!(found.len(), 1, "only the non-skipped file must be found");
        assert!(found[0].ends_with("Foo.java"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn boundary_stops_a_symlink_from_escaping_the_root() {
        let root = temp_dir("boundary-root");
        let secret = temp_dir("boundary-secret");
        write(&secret, "Secret.java", "class Secret {}\n");
        std::os::unix::fs::symlink(&secret, root.join("escape")).expect("create symlink");

        let mut found = Vec::new();
        let root_canon = fs::canonicalize(&root).expect("canonicalize root");
        let truncated = walk_java_files(&root, Some(&root_canon), |path| {
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert!(!truncated);
        assert!(
            found.is_empty(),
            "a symlink escaping the boundary must not be followed"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&secret);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ancestor_symlink_cycle_terminates_without_duplicates() {
        let root = temp_dir("cycle-root");
        write(&root, "src/Foo.java", "class Foo {}\n");
        std::os::unix::fs::symlink(&root, root.join("src/back-to-root"))
            .expect("create ancestor symlink cycle");

        let mut found = Vec::new();
        let root_canon = fs::canonicalize(&root).expect("canonicalize root");
        let truncated = walk_java_files(&root, Some(&root_canon), |path| {
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert!(!truncated, "a cycle must not be reported as a cap hit");
        assert_eq!(found.len(), 1, "the cycle must not produce duplicates");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn on_file_returning_false_stops_the_walk_and_reports_truncated() {
        let root = temp_dir("cap");
        for i in 0..10 {
            write(
                &root,
                &format!("src/File{i}.java"),
                &format!("class File{i} {{}}\n"),
            );
        }

        let mut found = Vec::new();
        let truncated = walk_java_files(&root, None, |path| {
            if found.len() >= 3 {
                return false;
            }
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert!(truncated);
        assert_eq!(found.len(), 3);

        let _ = std::fs::remove_dir_all(&root);
    }
}
