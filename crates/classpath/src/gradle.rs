//! Best-effort **static** discovery of a Gradle project's dependencies:
//! `group:artifact:version` string literals in `build.gradle(.kts)` and
//! `gradle/libs.versions.toml`, located in the Gradle module cache. The build is
//! never executed, so dynamic/computed/`platform` dependencies are not seen —
//! this is intentionally a heuristic.

use std::path::{Path, PathBuf};

const MAX_BUILD_BYTES: usize = 4 * 1024 * 1024;

/// Jars for the coordinates found statically in the project's build files that
/// exist in the Gradle cache.
pub(crate) fn dependency_jars(root: &Path, gradle_cache: &Path) -> Vec<PathBuf> {
    let mut coords = Vec::new();
    for name in ["build.gradle", "build.gradle.kts", "gradle/libs.versions.toml"] {
        let path = root.join(name);
        // Bound the read by file size before reading it into memory.
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX) > MAX_BUILD_BYTES as u64 {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            scrape_coords(&text, &mut coords);
        }
    }
    coords.sort();
    coords.dedup();
    coords
        .iter()
        .filter_map(|(g, a, v)| cache_jar(gradle_cache, g, a, v))
        .collect()
}

/// Pull `group:artifact:version` out of quoted string literals — the dominant
/// declaration form. Conservative: three non-empty, whitespace-free,
/// non-interpolated segments, with a digit in the version.
pub(crate) fn scrape_coords(text: &str, out: &mut Vec<(String, String, String)>) {
    for literal in string_literals(text) {
        if literal.contains('$') {
            continue; // interpolated — can't resolve statically
        }
        let parts: Vec<&str> = literal.split(':').collect();
        if parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && !p.contains(char::is_whitespace))
            && parts[2].chars().any(|c| c.is_ascii_digit())
        {
            out.push((parts[0].into(), parts[1].into(), parts[2].into()));
        }
    }
}

/// Contents of every single- or double-quoted string literal in the text.
fn string_literals(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        if quote == b'"' || quote == b'\'' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            if j < bytes.len() {
                if let Ok(s) = std::str::from_utf8(&bytes[start..j]) {
                    out.push(s.to_string());
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `<cache>/modules-2/files-2.1/<group>/<artifact>/<version>/<hash>/<artifact>-<version>.jar`
/// (the leaf hash directory is globbed).
fn cache_jar(cache: &Path, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
    if [group, artifact, version]
        .iter()
        .any(|s| s.is_empty() || s.contains("..") || s.contains('/') || s.contains('\\'))
    {
        return None;
    }
    let version_dir = cache
        .join("modules-2/files-2.1")
        .join(group)
        .join(artifact)
        .join(version);
    let jar_name = format!("{artifact}-{version}.jar");
    for hash_dir in std::fs::read_dir(&version_dir).ok()?.flatten() {
        let candidate = hash_dir.path().join(&jar_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coords(text: &str) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        scrape_coords(text, &mut out);
        out
    }

    #[test]
    fn scrapes_groovy_and_kotlin_string_coordinates() {
        let build = r#"
            dependencies {
                implementation 'com.google.guava:guava:33.0.0-jre'
                implementation("org.apache.commons:commons-lang3:3.14.0")
                testImplementation 'org.junit.jupiter:junit-jupiter:5.10.0'
            }"#;
        let found = coords(build);
        assert!(found.contains(&("com.google.guava".into(), "guava".into(), "33.0.0-jre".into())));
        assert!(found.contains(&(
            "org.apache.commons".into(),
            "commons-lang3".into(),
            "3.14.0".into()
        )));
        assert!(found.contains(&(
            "org.junit.jupiter".into(),
            "junit-jupiter".into(),
            "5.10.0".into()
        )));
    }

    #[test]
    fn scrapes_inline_catalog_coordinates() {
        let toml = r#"
            [libraries]
            guava = "com.google.guava:guava:33.0.0-jre"
        "#;
        assert!(coords(toml).contains(&(
            "com.google.guava".into(),
            "guava".into(),
            "33.0.0-jre".into()
        )));
    }

    #[test]
    fn ignores_interpolated_and_non_coordinate_strings() {
        let build = r#"
            implementation "com.example:lib:${libVersion}"
            description = "a project"
            url 'https://example.com/foo'
        "#;
        assert!(coords(build).is_empty(), "{:?}", coords(build));
    }

    #[test]
    fn locates_jar_in_gradle_cache() {
        let base = std::env::temp_dir().join(format!("jvl-gradle-{}", std::process::id()));
        let root = base.join("proj");
        let cache = base.join("gcache");
        std::fs::create_dir_all(&root).unwrap();
        let hash_dir = cache.join("modules-2/files-2.1/com.example/lib/1.0/deadbeef");
        std::fs::create_dir_all(&hash_dir).unwrap();
        std::fs::write(hash_dir.join("lib-1.0.jar"), b"jar").unwrap();
        std::fs::write(
            root.join("build.gradle"),
            "dependencies { implementation 'com.example:lib:1.0' }",
        )
        .unwrap();

        let jars = dependency_jars(&root, &cache);
        assert_eq!(jars.len(), 1, "{jars:?}");
        assert!(jars[0].ends_with("lib-1.0.jar"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
