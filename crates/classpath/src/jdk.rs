//! Locate the user's JDK and its `jmods/` — by filesystem probing only, never by
//! spawning a process. A JDK is usable here only if it ships jmods (Java 9+).

use std::path::{Path, PathBuf};

/// The best available JDK home (highest major version with jmods), or `None`.
pub(crate) fn best_jdk() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let home = PathBuf::from(home);
        if has_jmods(&home) {
            return Some(home);
        }
    }
    let mut candidates: Vec<PathBuf> = candidate_homes().into_iter().filter(|p| has_jmods(p)).collect();
    candidates.sort_by_key(|p| std::cmp::Reverse(major_version(p)));
    candidates.into_iter().next()
}

/// Paths to every `*.jmod` in a JDK home's `jmods/` directory.
pub(crate) fn jmods(home: &Path) -> Vec<PathBuf> {
    let dir = home.join("jmods");
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
        .collect()
}

fn has_jmods(home: &Path) -> bool {
    home.join("jmods").join("java.base.jmod").is_file()
}

fn candidate_homes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // macOS: each JVM exposes its home under Contents/Home.
    if let Ok(read) = std::fs::read_dir("/Library/Java/JavaVirtualMachines") {
        out.extend(read.flatten().map(|e| e.path().join("Contents/Home")));
    }
    // Linux distributions.
    if let Ok(read) = std::fs::read_dir("/usr/lib/jvm") {
        out.extend(read.flatten().map(|e| e.path()));
    }
    // A `java` on PATH, resolved to its home (no execution).
    if let Some(home) = java_on_path() {
        out.push(home);
    }
    out
}

/// `<dir>/java` on PATH, canonicalized, with the home two levels up (`<home>/bin/java`).
fn java_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let java = dir.join("java");
        if java.is_file() {
            if let Ok(real) = std::fs::canonicalize(&java) {
                if let Some(home) = real.parent().and_then(Path::parent) {
                    return Some(home.to_path_buf());
                }
            }
        }
    }
    None
}

/// First run of digits in the path (e.g. `openjdk-21.0.2` → 21), for ranking.
fn major_version(home: &Path) -> u32 {
    let s = home.to_string_lossy();
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}
