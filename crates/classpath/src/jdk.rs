//! Locate the user's JDK and its `jmods/` — by filesystem probing only, never by
//! spawning a process. A JDK is usable here only if it ships jmods (Java 9+).

use std::path::{Path, PathBuf};

/// The best available JDK home (highest major version with jmods), or `None`.
pub fn best_jdk() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let home = PathBuf::from(home);
        if has_jmods(&home) {
            return Some(home);
        }
    }
    let mut candidates: Vec<PathBuf> = candidate_homes()
        .into_iter()
        .filter(|p| has_jmods(p))
        .collect();
    // Prefer the highest *actual* feature version — read from each JDK's
    // `release` file, which is accurate; a digit-in-the-path parse is only the
    // fallback (and can be wrong, e.g. a `/Users/john2/...` home).
    candidates.sort_by_key(|p| std::cmp::Reverse(ranked_version(p)));
    candidates.into_iter().next()
}

/// Ranking key for [`best_jdk`]: the JDK's real feature version when its
/// `release` file is readable, else the leading digits of its path.
fn ranked_version(home: &Path) -> u32 {
    jdk_feature_version(home).unwrap_or_else(|| major_version(home))
}

/// The JDK's feature (major) version — read from the `JAVA_VERSION` property of
/// its `release` file (a plain properties file every JDK 9+ ships at its home
/// root), so *no process is spawned* (consistent with the rest of this module).
/// `None` when the file is absent or the value can't be parsed.
///
/// Used by the `javac` check to pass `-source`/`-target`/`--enable-preview` at
/// the running JDK's own level, so preview language features (e.g. pattern
/// matching in `switch` on JDK 17–20) don't surface as false compiler errors.
pub fn jdk_feature_version(home: &Path) -> Option<u32> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    release
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))
        .and_then(|raw| parse_feature_version(raw.trim().trim_matches('"')))
}

/// Parse a `JAVA_VERSION` value to its feature number: `"21.0.2"` → 21,
/// `"17"` → 17, `"11.0.20"` → 11, and the legacy `"1.8.0_392"` → 8.
fn parse_feature_version(value: &str) -> Option<u32> {
    let mut parts = value.split('.');
    let first = parts.next()?;
    if first == "1" {
        // Legacy `1.N` scheme (Java 8 and earlier).
        return parts.next().and_then(leading_number);
    }
    leading_number(first)
}

/// Leading run of ASCII digits parsed as a `u32` (`"17-ea"` → 17), or `None`.
fn leading_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
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
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    // macOS: each JVM bundle exposes its home under `Contents/Home` — both the
    // system location and the per-user one (`~/Library/...`), which tools like
    // Homebrew casks and manual installs use and which we previously missed.
    scan_dir(
        &mut out,
        "/Library/Java/JavaVirtualMachines",
        Some("Contents/Home"),
    );
    if let Some(h) = &user_home {
        scan_dir(
            &mut out,
            h.join("Library/Java/JavaVirtualMachines"),
            Some("Contents/Home"),
        );
    }
    // Linux distributions (Debian/RH layouts).
    scan_dir(&mut out, "/usr/lib/jvm", None);
    scan_dir(&mut out, "/usr/java", None);
    // Cross-platform developer tooling: JetBrains-managed JDKs and SDKMAN.
    if let Some(h) = &user_home {
        scan_dir(&mut out, h.join(".jdks"), None);
        scan_dir(&mut out, h.join(".sdkman/candidates/java"), None);
    }
    // A `java` on PATH, resolved to its home (no execution).
    if let Some(home) = java_on_path() {
        out.push(home);
    }
    out
}

/// Append each immediate subdirectory of `dir` (with an optional `suffix`
/// joined on, e.g. `Contents/Home` for macOS bundles) to `out`. A missing or
/// unreadable directory contributes nothing.
fn scan_dir(out: &mut Vec<PathBuf>, dir: impl AsRef<Path>, suffix: Option<&str>) {
    if let Ok(read) = std::fs::read_dir(dir) {
        out.extend(read.flatten().map(|e| match suffix {
            Some(s) => e.path().join(s),
            None => e.path(),
        }));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_feature_version_handles_modern_and_legacy_schemes() {
        assert_eq!(parse_feature_version("21.0.2"), Some(21));
        assert_eq!(parse_feature_version("17"), Some(17));
        assert_eq!(parse_feature_version("11.0.20"), Some(11));
        assert_eq!(parse_feature_version("1.8.0_392"), Some(8));
        assert_eq!(parse_feature_version("21-ea"), Some(21));
        assert_eq!(parse_feature_version("garbage"), None);
        assert_eq!(parse_feature_version(""), None);
    }

    #[test]
    fn jdk_feature_version_reads_release_file() {
        let dir = std::env::temp_dir().join(format!(
            "jvl-jdk-release-test-{}-{}",
            std::process::id(),
            "a"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("release"),
            "IMPLEMENTOR=\"Azul Systems, Inc.\"\nJAVA_VERSION=\"21.0.2\"\nOS_ARCH=\"aarch64\"\n",
        )
        .unwrap();
        assert_eq!(jdk_feature_version(&dir), Some(21));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ranked_version_prefers_release_file_over_misleading_path() {
        // A path whose leading digits (`8`) understate the real version (21).
        let dir = std::env::temp_dir().join(format!("jvl-jdk8-really21-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("release"), "JAVA_VERSION=\"21.0.2\"\n").unwrap();
        assert_eq!(ranked_version(&dir), 21);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jdk_feature_version_none_when_release_absent() {
        let dir = std::env::temp_dir().join(format!(
            "jvl-jdk-release-test-{}-{}",
            std::process::id(),
            "b"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(jdk_feature_version(&dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
