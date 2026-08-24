//! Best-effort **static** discovery of a Gradle project's dependencies:
//! `group:artifact:version` string literals in `build.gradle(.kts)` and
//! `gradle/libs.versions.toml`, located in the Gradle module cache, then
//! walked transitively. The build is never executed, so dynamic/computed/
//! `platform` dependencies are not seen — the initial coordinate list is
//! intentionally a heuristic. Gradle caches each downloaded artifact's Maven
//! POM alongside its jar (in its own hash-keyed subdirectory), so the
//! transitive walk reuses the exact same POM semantics as the Maven backend
//! (`resolve.rs`) — only the "locate this coordinate's pom/jar" step differs.

use std::path::{Path, PathBuf};

use crate::maven::MavenLocator;
use crate::resolve::{self, Locator, ResolvedProject};

const MAX_BUILD_BYTES: usize = 4 * 1024 * 1024;

/// Best-effort scrape of the project's declared Java release from
/// `build.gradle(.kts)`: a Java-toolchain `languageVersion`, a
/// `JavaVersion.VERSION_*` constant, or a numeric `sourceCompatibility` /
/// `targetCompatibility` / `release` assignment. Static-only (the build is
/// never run), so computed/plugin-driven levels aren't seen — `None` then, and
/// the caller falls back to the JDK's own level.
pub(crate) fn compiler_release(root: &Path) -> Option<u32> {
    for name in ["build.gradle", "build.gradle.kts"] {
        let path = root.join(name);
        if std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(u64::MAX)
            > MAX_BUILD_BYTES as u64
        {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(n) = parse_gradle_release(&text) {
                return Some(n);
            }
        }
    }
    None
}

fn parse_gradle_release(text: &str) -> Option<u32> {
    // 1) Java toolchain: `JavaLanguageVersion.of(N)`.
    if let Some(pos) = text.find("JavaLanguageVersion.of(") {
        if let Some(n) = leading_u32(text[pos + "JavaLanguageVersion.of(".len()..].trim_start()) {
            return Some(n);
        }
    }
    // 2) `JavaVersion.VERSION_<n>` / `VERSION_1_<n>`.
    if let Some(pos) = text.find("VERSION_") {
        let rest = &text[pos + "VERSION_".len()..];
        if let Some(stripped) = rest.strip_prefix("1_") {
            if let Some(n) = leading_u32(stripped) {
                return Some(n);
            }
        } else if let Some(n) = leading_u32(rest) {
            return Some(n);
        }
    }
    // 3) numeric `sourceCompatibility` / `targetCompatibility` / `release`.
    for key in ["sourceCompatibility", "targetCompatibility", "release"] {
        if let Some(n) = version_after_key(text, key) {
            return Some(n);
        }
    }
    None
}

/// The leading run of ASCII digits of `s`, parsed as a feature number.
fn leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The first `N` or `1.N` version token appearing after any occurrence of
/// `key`, skipping assignment punctuation (`= ( ) ' " space`) but bailing at a
/// newline or a letter — so a `= JavaVersion.VERSION_*` form is left to the
/// dedicated `VERSION_` scanner rather than mis-read here.
fn version_after_key(text: &str, key: &str) -> Option<u32> {
    let mut from = 0;
    while let Some(rel) = text[from..].find(key) {
        let after = from + rel + key.len();
        if let Some(n) = version_token(&text[after..]) {
            return Some(n);
        }
        from = after;
    }
    None
}

fn version_token(s: &str) -> Option<u32> {
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() {
            let rest = &s[i..];
            return match rest.strip_prefix("1.") {
                Some(minor) => leading_u32(minor),
                None => leading_u32(rest),
            };
        }
        if c == '\n' || c.is_ascii_alphabetic() {
            return None;
        }
    }
    None
}

/// Resolve a Gradle project's dependencies (direct + transitive) from what
/// can be statically scraped out of its build files, located first in
/// `gradle_cache` (`~/.gradle/caches`) and, failing that, in `m2_repo`
/// (`~/.m2/repository`) — see [`FallbackLocator`]. The `~/.m2` fallback
/// matters because dependencies the user consents to download are always
/// installed into `~/.m2` (repo-agnostic `g:a:v` coordinates), regardless of
/// whether the project is Maven or Gradle, so a Gradle project must also be
/// able to *find* them there after a rebuild.
pub(crate) fn resolve_project(root: &Path, gradle_cache: &Path, m2_repo: &Path) -> ResolvedProject {
    let mut coords = Vec::new();
    for name in [
        "build.gradle",
        "build.gradle.kts",
        "gradle/libs.versions.toml",
    ] {
        let path = root.join(name);
        // Bound the read by file size before reading it into memory.
        if std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(u64::MAX)
            > MAX_BUILD_BYTES as u64
        {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            scrape_coords(&text, &mut coords);
        }
    }
    coords.sort();
    coords.dedup();

    // Dynamic versions (`+`, `latest.*`, `[..]`/`(..)` ranges) can't be
    // pinned without executing Gradle — skip them, but record each skip so
    // callers can surface the gap (per the resolution contract).
    let mut degraded_dynamic = Vec::new();
    coords.retain(|(g, a, v)| {
        if is_dynamic_version(v) {
            degraded_dynamic.push(format!("{g}:{a}:{v} (dynamic version unsupported)"));
            false
        } else {
            true
        }
    });

    let locator = FallbackLocator {
        primary: GradleLocator {
            cache: gradle_cache,
        },
        secondary: MavenLocator { m2_repo },
    };
    let seeds = resolve::coord_seeds(&coords);
    let (jars, mut degraded) = resolve::resolve_transitive(seeds, &locator);
    degraded_dynamic.append(&mut degraded);
    ResolvedProject {
        jars,
        source_roots: Vec::new(),
        degraded: degraded_dynamic,
    }
}

/// Gradle dynamic-version notations that need the build tool to pin:
/// `1.+`/`+` prefix wildcards, `latest.release`-style, and Ivy/Maven
/// `[1.0,2.0)` ranges.
fn is_dynamic_version(version: &str) -> bool {
    version.contains('+')
        || version.starts_with("latest.")
        || version.starts_with('[')
        || version.starts_with('(')
}

/// Pull `group:artifact:version` out of quoted string literals — the dominant
/// declaration form. Conservative: three non-empty, whitespace-free,
/// non-interpolated segments, with a digit or `+` wildcard in the version
/// (dynamic versions are kept here as candidates so the resolver can record
/// them as degraded rather than dropping them invisibly).
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
            && parts[2].chars().any(|c| c.is_ascii_digit() || c == '+')
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

/// Locates artifacts in the Gradle module cache
/// (`<cache>/modules-2/files-2.1/<group>/<artifact>/<version>/<hash>/...`).
/// The pom and jar for the same coordinate can live in different hash
/// directories (Gradle hashes each downloaded file independently), so each
/// extension is globbed for separately.
struct GradleLocator<'a> {
    cache: &'a Path,
}

impl Locator for GradleLocator<'_> {
    fn locate_pom(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
        cache_file(self.cache, group, artifact, version, "pom")
    }

    fn locate_jar(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
        cache_file(self.cache, group, artifact, version, "jar")
    }
}

/// Tries `primary` (the Gradle module cache) first, falling back to
/// `secondary` (`~/.m2/repository`) only when the primary has neither the pom
/// nor the jar. Static and offline like both locators it wraps — this is
/// purely a "where might this coordinate already be on disk" lookup, never a
/// network fetch.
struct FallbackLocator<'a> {
    primary: GradleLocator<'a>,
    secondary: MavenLocator<'a>,
}

impl Locator for FallbackLocator<'_> {
    fn locate_pom(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
        self.primary
            .locate_pom(group, artifact, version)
            .or_else(|| self.secondary.locate_pom(group, artifact, version))
    }

    fn locate_jar(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
        self.primary
            .locate_jar(group, artifact, version)
            .or_else(|| self.secondary.locate_jar(group, artifact, version))
    }
}

/// `<cache>/modules-2/files-2.1/<group>/<artifact>/<version>/<hash>/<artifact>-<version>.<ext>`
/// (the leaf hash directory is globbed).
fn cache_file(
    cache: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
) -> Option<PathBuf> {
    if [group, artifact, version]
        .iter()
        .any(|s| crate::maven::unsafe_coord(s))
    {
        return None;
    }
    let version_dir = cache
        .join("modules-2/files-2.1")
        .join(group)
        .join(artifact)
        .join(version);
    let file_name = format!("{artifact}-{version}.{ext}");
    for hash_dir in std::fs::read_dir(&version_dir).ok()?.flatten() {
        let candidate = hash_dir.path().join(&file_name);
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
    fn parse_gradle_release_recognizes_common_forms() {
        assert_eq!(
            parse_gradle_release(
                "java { toolchain { languageVersion = JavaLanguageVersion.of(21) } }"
            ),
            Some(21)
        );
        assert_eq!(
            parse_gradle_release("sourceCompatibility = JavaVersion.VERSION_17"),
            Some(17)
        );
        assert_eq!(
            parse_gradle_release("sourceCompatibility = JavaVersion.VERSION_1_8"),
            Some(8)
        );
        assert_eq!(parse_gradle_release("sourceCompatibility = '11'"), Some(11));
        assert_eq!(parse_gradle_release("targetCompatibility = 17"), Some(17));
        assert_eq!(
            parse_gradle_release("dependencies { implementation 'a:b:1' }"),
            None
        );
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
        assert!(found.contains(&(
            "com.google.guava".into(),
            "guava".into(),
            "33.0.0-jre".into()
        )));
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

        let result = resolve_project(&root, &cache, &base.join("m2"));
        assert_eq!(result.jars.len(), 1, "{:?}", result.jars);
        assert!(result.jars[0].ends_with("lib-1.0.jar"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dynamic_versions_are_skipped_with_degraded_record() {
        let base = std::env::temp_dir().join(format!("jvl-gradle-dynamic-{}", std::process::id()));
        let root = base.join("proj");
        let cache = base.join("gcache");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            root.join("build.gradle"),
            "dependencies {\n\
             implementation 'g:wild:1.+'\n\
             implementation 'g:range:[1.0,2.0)'\n\
             }",
        )
        .unwrap();

        let result = resolve_project(&root, &cache, &base.join("m2"));
        assert!(result.jars.is_empty(), "{:?}", result.jars);
        assert!(
            result
                .degraded
                .contains(&"g:wild:1.+ (dynamic version unsupported)".to_string()),
            "{:?}",
            result.degraded
        );
        assert!(
            result
                .degraded
                .contains(&"g:range:[1.0,2.0) (dynamic version unsupported)".to_string()),
            "{:?}",
            result.degraded
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolves_one_transitive_hop_via_cached_pom() {
        let base =
            std::env::temp_dir().join(format!("jvl-gradle-transitive-{}", std::process::id()));
        let root = base.join("proj");
        let cache = base.join("gcache");
        std::fs::create_dir_all(&root).unwrap();

        // B: jar + pom (in separate hash dirs, as Gradle actually lays them out),
        // depending on C.
        let b_jar_dir = cache.join("modules-2/files-2.1/g/B/1.0/hash-jar");
        let b_pom_dir = cache.join("modules-2/files-2.1/g/B/1.0/hash-pom");
        std::fs::create_dir_all(&b_jar_dir).unwrap();
        std::fs::create_dir_all(&b_pom_dir).unwrap();
        std::fs::write(b_jar_dir.join("B-1.0.jar"), b"jar").unwrap();
        std::fs::write(
            b_pom_dir.join("B-1.0.pom"),
            "<project><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version>\
             <dependencies><dependency><groupId>g</groupId><artifactId>C</artifactId>\
             <version>1.0</version></dependency></dependencies></project>",
        )
        .unwrap();

        // C: jar + pom, no further deps.
        let c_dir = cache.join("modules-2/files-2.1/g/C/1.0/hash-c");
        std::fs::create_dir_all(&c_dir).unwrap();
        std::fs::write(c_dir.join("C-1.0.jar"), b"jar").unwrap();
        std::fs::write(
            c_dir.join("C-1.0.pom"),
            "<project><groupId>g</groupId><artifactId>C</artifactId><version>1.0</version></project>",
        )
        .unwrap();

        std::fs::write(
            root.join("build.gradle"),
            "dependencies { implementation 'g:B:1.0' }",
        )
        .unwrap();

        let result = resolve_project(&root, &cache, &base.join("m2"));
        let names: Vec<String> = result
            .jars
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"B-1.0.jar".to_string()), "{names:?}");
        assert!(names.contains(&"C-1.0.jar".to_string()), "{names:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A dependency the user has downloaded lands in `~/.m2/repository`
    /// (repo-agnostic `g:a:v` coordinates), regardless of whether the
    /// project is Maven or Gradle. A Gradle project must therefore be able to
    /// find it there too when the Gradle module cache doesn't have it —
    /// see [`FallbackLocator`].
    #[test]
    fn falls_back_to_m2_repository_when_gradle_cache_misses() {
        let base =
            std::env::temp_dir().join(format!("jvl-gradle-m2-fallback-{}", std::process::id()));
        let root = base.join("proj");
        let cache = base.join("gcache"); // deliberately never populated
        let m2 = base.join("m2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&cache).unwrap();

        // The artifact lives ONLY in ~/.m2/repository, laid out exactly as
        // the Maven backend would install it — not in the Gradle cache.
        let m2_dir = m2.join("com/example/lib/1.0");
        std::fs::create_dir_all(&m2_dir).unwrap();
        std::fs::write(m2_dir.join("lib-1.0.jar"), b"jar").unwrap();
        std::fs::write(
            m2_dir.join("lib-1.0.pom"),
            "<project><groupId>com.example</groupId><artifactId>lib</artifactId>\
             <version>1.0</version></project>",
        )
        .unwrap();

        std::fs::write(
            root.join("build.gradle"),
            "dependencies { implementation 'com.example:lib:1.0' }",
        )
        .unwrap();

        let result = resolve_project(&root, &cache, &m2);
        let names: Vec<String> = result
            .jars
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"lib-1.0.jar".to_string()),
            "expected the Gradle project to find the dependency via the ~/.m2 fallback: {names:?}"
        );
        assert!(result.degraded.is_empty(), "{:?}", result.degraded);

        let _ = std::fs::remove_dir_all(&base);
    }
}
