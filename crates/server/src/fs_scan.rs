//! Shared bounded `.java` traversal for workspace symbols and references.
//! Skips unsafe paths, prevents symlink cycles, and yields for cancellation.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Directories always skipped when walking (build output and VCS metadata).
/// Hidden (dot-prefixed) directories are skipped separately.
pub(crate) const SKIPPED_DIR_NAMES: [&str; 3] = ["target", "build", ".git"];

/// Whether a path component is skipped by the walk: hidden (dot-prefixed) or
/// one of [`SKIPPED_DIR_NAMES`]. Also used by watched-file validation so
/// excluded paths are rejected consistently.
pub(crate) fn is_excluded_component(name: &str) -> bool {
    name.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name)
}

/// Directory entries processed between yields to the async runtime, so a
/// cancelled scan stops promptly instead of running to completion.
const YIELD_INTERVAL: usize = 32;

/// Outcome of a [`walk_java_files`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Walk {
    /// `on_file` returned `false`, so the walk was cut short.
    pub(crate) stopped_early: bool,
    /// A directory read or path canonicalization failed, so files may be
    /// missing. Callers caching the result should treat it as incomplete.
    pub(crate) io_errors: bool,
}

/// Walk `.java` files under `root` until `on_file` returns `false`.
/// Paths must stay inside `boundary`; [`Walk`] reports partial scans.
pub(crate) async fn walk_java_files(
    root: &Path,
    boundary: Option<&Path>,
    mut on_file: impl FnMut(&Path) -> bool,
) -> Walk {
    let mut walk = Walk::default();
    let mut stack = vec![root.to_path_buf()];
    let mut since_yield = 0usize;
    let mut visited_dirs: HashSet<PathBuf> = HashSet::new();
    match fs::canonicalize(root) {
        Ok(root_canon) => {
            visited_dirs.insert(root_canon);
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => walk.io_errors = true,
        Err(_) => {}
    }

    while let Some(dir) = stack.pop() {
        let read_dir = match fs::read_dir(&dir) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    walk.io_errors = true;
                }
                continue;
            }
        };
        for result in read_dir {
            let dir_entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        walk.io_errors = true;
                    }
                    continue;
                }
            };
            let path = dir_entry.path();
            let name = dir_entry.file_name();
            let name_str = name.to_string_lossy();
            if is_excluded_component(name_str.as_ref()) {
                continue;
            }
            let canon = match fs::canonicalize(&path) {
                Ok(canon) => canon,
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        walk.io_errors = true;
                    }
                    continue;
                }
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
                // Already-walked directory: a duplicate sibling symlink or a
                // cycling ancestor symlink. Skip either way.
            } else if name_str.ends_with(".java") && !on_file(&path) {
                walk.stopped_early = true;
                return walk;
            }

            since_yield += 1;
            if since_yield >= YIELD_INTERVAL {
                since_yield = 0;
                tokio::task::yield_now().await;
            }
        }
    }

    walk
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
        let walk = walk_java_files(&root, Some(&root_canon), |path| {
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert_eq!(walk, Walk::default());
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
        let walk = walk_java_files(&root, Some(&root_canon), |path| {
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert_eq!(walk, Walk::default());
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
        let walk = walk_java_files(&root, Some(&root_canon), |path| {
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert_eq!(walk, Walk::default(), "a cycle is a complete walk");
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
        let walk = walk_java_files(&root, None, |path| {
            if found.len() >= 3 {
                return false;
            }
            found.push(path.to_path_buf());
            true
        })
        .await;

        assert!(walk.stopped_early);
        assert!(!walk.io_errors);
        assert_eq!(found.len(), 3);

        let _ = std::fs::remove_dir_all(&root);
    }
}
