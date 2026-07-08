//! Locate a Maven project's **direct** dependency jars by statically parsing
//! `pom.xml`. XXE-safe (roxmltree resolves no DTDs/external entities) and
//! read-only — the build is never executed. Transitive deps, parent POMs, and
//! BOM-managed versions are out of scope here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_POM_BYTES: usize = 4 * 1024 * 1024;

/// Jars of a pom's direct dependencies that exist in the local repository.
pub(crate) fn dependency_jars(root: &Path, m2_repo: &Path) -> Vec<PathBuf> {
    let pom = root.join("pom.xml");
    // Bound the read by file size before pulling the whole file into memory.
    if std::fs::metadata(&pom).map(|m| m.len()).unwrap_or(u64::MAX) > MAX_POM_BYTES as u64 {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&pom) else {
        return Vec::new();
    };
    parse_dependencies(&text)
        .into_iter()
        .filter_map(|(g, a, v)| {
            let rel = m2_relative(&g, &a, &v)?;
            let jar = m2_repo.join(rel);
            jar.is_file().then_some(jar)
        })
        .collect()
}

/// Direct `<dependency>` coordinates with simple `${property}` substitution.
/// Skips `dependencyManagement` and any dependency whose version stays
/// unresolved (e.g. inherited from a BOM).
pub(crate) fn parse_dependencies(text: &str) -> Vec<(String, String, String)> {
    let Ok(doc) = roxmltree::Document::parse(text) else {
        return Vec::new();
    };
    let props = collect_properties(&doc);
    let mut out = Vec::new();
    for dep in doc.descendants().filter(|n| n.has_tag_name("dependency")) {
        if !is_direct_dependency(dep) {
            continue;
        }
        let (Some(g), Some(a), Some(v)) = (
            child_text(dep, "groupId"),
            child_text(dep, "artifactId"),
            child_text(dep, "version"),
        ) else {
            continue;
        };
        let (g, a, v) = (
            substitute(&g, &props),
            substitute(&a, &props),
            substitute(&v, &props),
        );
        if [&g, &a, &v].iter().any(|s| s.contains("${")) {
            continue; // unresolved property
        }
        out.push((g, a, v));
    }
    out
}

/// `<project><dependencies><dependency>` only — not `dependencyManagement`/plugins.
fn is_direct_dependency(dep: roxmltree::Node) -> bool {
    match dep.parent() {
        Some(deps) if deps.has_tag_name("dependencies") => {
            matches!(deps.parent(), Some(p) if p.has_tag_name("project"))
        }
        _ => false,
    }
}

fn collect_properties(doc: &roxmltree::Document) -> HashMap<String, String> {
    let mut props = HashMap::new();
    for properties in doc.descendants().filter(|n| n.has_tag_name("properties")) {
        if !matches!(properties.parent(), Some(p) if p.has_tag_name("project")) {
            continue;
        }
        for prop in properties.children().filter(|n| n.is_element()) {
            if let Some(text) = prop.text() {
                props.insert(prop.tag_name().name().to_string(), text.trim().to_string());
            }
        }
    }
    props
}

fn child_text(node: roxmltree::Node, tag: &str) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn substitute(value: &str, props: &HashMap<String, String>) -> String {
    let mut out = value.to_string();
    for (key, val) in props {
        out = out.replace(&format!("${{{key}}}"), val);
    }
    out
}

/// `<group as path>/<artifact>/<version>/<artifact>-<version>.jar`, or `None` if
/// any coordinate is path-unsafe.
pub(crate) fn m2_relative(group: &str, artifact: &str, version: &str) -> Option<String> {
    if [group, artifact, version].iter().any(|s| unsafe_coord(s)) {
        return None;
    }
    Some(format!(
        "{}/{}/{}/{}-{}.jar",
        group.replace('.', "/"),
        artifact,
        version,
        artifact,
        version
    ))
}

fn unsafe_coord(s: &str) -> bool {
    s.is_empty() || s.contains("..") || s.contains('/') || s.contains('\\') || s.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_direct_dependencies_with_property_substitution() {
        let pom = r#"
            <project>
              <properties><junit.version>5.10.0</junit.version></properties>
              <dependencies>
                <dependency>
                  <groupId>com.google.guava</groupId>
                  <artifactId>guava</artifactId>
                  <version>33.0.0-jre</version>
                </dependency>
                <dependency>
                  <groupId>org.junit.jupiter</groupId>
                  <artifactId>junit-jupiter</artifactId>
                  <version>${junit.version}</version>
                </dependency>
              </dependencies>
            </project>"#;
        let deps = parse_dependencies(pom);
        assert!(deps.contains(&(
            "com.google.guava".into(),
            "guava".into(),
            "33.0.0-jre".into()
        )));
        assert!(deps.contains(&(
            "org.junit.jupiter".into(),
            "junit-jupiter".into(),
            "5.10.0".into()
        )));
    }

    #[test]
    fn skips_dependency_management_and_unresolved_versions() {
        let pom = r#"
            <project>
              <dependencyManagement><dependencies>
                <dependency><groupId>g</groupId><artifactId>managed</artifactId><version>1</version></dependency>
              </dependencies></dependencyManagement>
              <dependencies>
                <dependency><groupId>g</groupId><artifactId>nov</artifactId></dependency>
                <dependency><groupId>g</groupId><artifactId>unresolved</artifactId><version>${missing}</version></dependency>
              </dependencies>
            </project>"#;
        let deps = parse_dependencies(pom);
        assert!(deps.iter().all(|(_, a, _)| a != "managed"), "{deps:?}");
        assert!(
            deps.iter().all(|(_, a, _)| a != "nov"),
            "version-less skipped"
        );
        assert!(
            deps.iter().all(|(_, a, _)| a != "unresolved"),
            "unresolved skipped"
        );
    }

    #[test]
    fn m2_path_layout_and_safety() {
        assert_eq!(
            m2_relative("com.google.guava", "guava", "33.0.0-jre").as_deref(),
            Some("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar")
        );
        assert_eq!(m2_relative("..", "a", "1"), None);
        assert_eq!(m2_relative("g", "a/b", "1"), None);
    }

    #[test]
    fn malformed_pom_yields_no_deps() {
        assert!(parse_dependencies("<project><dependencies>").is_empty());
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
            "<project><dependencies><dependency><groupId>com.example</groupId>\
             <artifactId>lib</artifactId><version>1.0</version></dependency>\
             </dependencies></project>",
        )
        .unwrap();

        let jars = dependency_jars(&root, &m2);
        assert_eq!(jars.len(), 1, "{jars:?}");
        assert!(jars[0].ends_with("lib-1.0.jar"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
