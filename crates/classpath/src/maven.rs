//! Maven backend: statically parse `pom.xml` (XXE-safe — roxmltree resolves
//! no DTDs/external entities) and resolve the **transitive** dependency graph
//! from the local repository (`~/.m2/repository`). Read-only — the build is
//! never executed. The graph-walking semantics (parent POMs, BOM imports,
//! scope/exclusion/optional filtering, nearest-wins mediation, bounds) live in
//! `resolve.rs`; this module only knows how a Maven coordinate maps to a path
//! under `~/.m2/repository`.

use std::path::{Path, PathBuf};

use crate::resolve::{self, Locator, ResolvedProject};

/// Resolve a Maven project rooted at `root` (must contain `pom.xml`) against
/// the local repository at `m2_repo`, including transitive dependencies,
/// parent-POM chains, BOM imports, and sibling multi-modules.
pub(crate) fn resolve_project(root: &Path, m2_repo: &Path) -> ResolvedProject {
    let locator = MavenLocator { m2_repo };
    resolve::resolve_maven_like_project(root, &locator)
}

/// The project's declared Java release (`maven.compiler.release` /
/// `maven.compiler.source` / `java.version`) from the effective root POM, or
/// `None` when undeclared.
pub(crate) fn compiler_release(root: &Path, m2_repo: &Path) -> Option<u32> {
    resolve::maven_like_compiler_release(root, &MavenLocator { m2_repo })
}

/// Locates artifacts under a Maven local repository (`~/.m2/repository`).
pub(crate) struct MavenLocator<'a> {
    pub m2_repo: &'a Path,
}

impl Locator for MavenLocator<'_> {
    fn locate_pom(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
        let rel = m2_relative_ext(group, artifact, version, "pom")?;
        let path = self.m2_repo.join(rel);
        path.is_file().then_some(path)
    }

    fn locate_jar(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
        let rel = m2_relative(group, artifact, version)?;
        let path = self.m2_repo.join(rel);
        path.is_file().then_some(path)
    }
}

/// `<group as path>/<artifact>/<version>/<artifact>-<version>.jar`, or `None`
/// if any coordinate is path-unsafe.
pub(crate) fn m2_relative(group: &str, artifact: &str, version: &str) -> Option<String> {
    m2_relative_ext(group, artifact, version, "jar")
}

/// As [`m2_relative`], but for an arbitrary file extension (`.pom` for
/// metadata, `.jar` for the artifact).
pub(crate) fn m2_relative_ext(
    group: &str,
    artifact: &str,
    version: &str,
    ext: &str,
) -> Option<String> {
    if [group, artifact, version].iter().any(|s| unsafe_coord(s)) {
        return None;
    }
    Some(format!(
        "{}/{}/{}/{}-{}.{}",
        group.replace('.', "/"),
        artifact,
        version,
        artifact,
        version,
        ext
    ))
}

pub(crate) fn unsafe_coord(s: &str) -> bool {
    s.is_empty() || s.contains("..") || s.contains('/') || s.contains('\\') || s.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m2_path_layout_and_safety() {
        assert_eq!(
            m2_relative("com.google.guava", "guava", "33.0.0-jre").as_deref(),
            Some("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar")
        );
        assert_eq!(
            m2_relative_ext("com.google.guava", "guava", "33.0.0-jre", "pom").as_deref(),
            Some("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.pom")
        );
        assert_eq!(m2_relative("..", "a", "1"), None);
        assert_eq!(m2_relative("g", "a/b", "1"), None);
    }

    #[test]
    fn locates_jar_in_local_repository() {
        let base = std::env::temp_dir().join(format!("jvl-mvn-{}", std::process::id()));
        let root = base.join("proj");
        let m2 = base.join("m2");
        std::fs::create_dir_all(&root).unwrap();
        let jar_dir = m2.join("com/example/lib/1.0");
        std::fs::create_dir_all(&jar_dir).unwrap();
        std::fs::write(jar_dir.join("lib-1.0.jar"), b"jar").unwrap();
        std::fs::write(
            root.join("pom.xml"),
            "<project><groupId>com.example</groupId><artifactId>proj</artifactId>\
             <version>1.0</version><dependencies><dependency><groupId>com.example</groupId>\
             <artifactId>lib</artifactId><version>1.0</version></dependency>\
             </dependencies></project>",
        )
        .unwrap();

        let result = resolve_project(&root, &m2);
        assert_eq!(result.jars.len(), 1, "{:?}", result.jars);
        assert!(result.jars[0].ends_with("lib-1.0.jar"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn malformed_pom_yields_no_deps() {
        let base = std::env::temp_dir().join(format!("jvl-mvn-malformed-{}", std::process::id()));
        let root = base.join("proj");
        let m2 = base.join("m2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&m2).unwrap();
        std::fs::write(root.join("pom.xml"), "<project><dependencies>").unwrap();

        let result = resolve_project(&root, &m2);
        assert!(result.jars.is_empty(), "{:?}", result.jars);

        let _ = std::fs::remove_dir_all(&base);
    }
}
