use super::*;

struct M2Fixture {
    base: PathBuf,
    root: PathBuf,
    m2: PathBuf,
}

impl Drop for M2Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn fixture(name: &str) -> M2Fixture {
    let base = std::env::temp_dir().join(format!(
        "jvl-resolve-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let root = base.join("proj");
    let m2 = base.join("m2");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&m2).unwrap();
    M2Fixture { base, root, m2 }
}

fn put_artifact(m2: &Path, group: &str, artifact: &str, version: &str, pom: &str) {
    let dir = m2
        .join(group.replace('.', "/"))
        .join(artifact)
        .join(version);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{artifact}-{version}.jar")), b"jar").unwrap();
    std::fs::write(dir.join(format!("{artifact}-{version}.pom")), pom).unwrap();
}

fn put_pom_only(m2: &Path, group: &str, artifact: &str, version: &str, pom: &str) {
    let dir = m2
        .join(group.replace('.', "/"))
        .join(artifact)
        .join(version);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{artifact}-{version}.pom")), pom).unwrap();
}

fn simple_dep_pom(deps_xml: &str) -> String {
    format!(
        "<project><groupId>g</groupId><artifactId>x</artifactId><version>1.0</version>\
             <dependencies>{deps_xml}</dependencies></project>"
    )
}

fn jar_name(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}

#[test]
fn chain_a_b_c_resolves_all_jars() {
    let f = fixture("chain");
    put_artifact(
            &f.m2,
            "g",
            "B",
            "1.0",
            &simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>C</artifactId><version>1.0</version></dependency>",
            ),
        );
    put_artifact(&f.m2, "g", "C", "1.0", &simple_dep_pom(""));
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-1.0.jar".to_string()), "{names:?}");
    assert!(names.contains(&"C-1.0.jar".to_string()), "{names:?}");
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn nearest_wins_prefers_shallower_version() {
    let f = fixture("nearest");
    put_artifact(
            &f.m2,
            "g",
            "B",
            "1.0",
            &simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>C</artifactId><version>1.0</version></dependency>",
            ),
        );
    put_artifact(&f.m2, "g", "C", "1.0", &simple_dep_pom(""));
    put_artifact(&f.m2, "g", "C", "2.0", &simple_dep_pom(""));
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>\
                 <dependency><groupId>g</groupId><artifactId>C</artifactId><version>2.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"C-2.0.jar".to_string()), "{names:?}");
    assert!(!names.contains(&"C-1.0.jar".to_string()), "{names:?}");
}

#[test]
fn exclusion_drops_transitive_dependency() {
    let f = fixture("exclusion");
    put_artifact(
            &f.m2,
            "g",
            "B",
            "1.0",
            &simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>C</artifactId><version>1.0</version></dependency>",
            ),
        );
    put_artifact(&f.m2, "g", "C", "1.0", &simple_dep_pom(""));
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version>\
                 <exclusions><exclusion><groupId>g</groupId><artifactId>C</artifactId></exclusion></exclusions>\
                 </dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-1.0.jar".to_string()), "{names:?}");
    assert!(!names.contains(&"C-1.0.jar".to_string()), "{names:?}");
}

#[test]
fn optional_transitive_is_skipped() {
    let f = fixture("optional");
    put_artifact(
        &f.m2,
        "g",
        "B",
        "1.0",
        &simple_dep_pom(
            "<dependency><groupId>g</groupId><artifactId>C</artifactId><version>1.0</version>\
                 <optional>true</optional></dependency>",
        ),
    );
    put_artifact(&f.m2, "g", "C", "1.0", &simple_dep_pom(""));
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(!names.contains(&"C-1.0.jar".to_string()), "{names:?}");
}

#[test]
fn test_scope_transitive_dropped_runtime_kept() {
    let f = fixture("scope");
    put_artifact(
        &f.m2,
        "g",
        "B",
        "1.0",
        &simple_dep_pom(
            "<dependency><groupId>g</groupId><artifactId>C</artifactId><version>1.0</version>\
                 <scope>test</scope></dependency>\
                 <dependency><groupId>g</groupId><artifactId>D</artifactId><version>1.0</version>\
                 <scope>runtime</scope></dependency>",
        ),
    );
    put_artifact(&f.m2, "g", "C", "1.0", &simple_dep_pom(""));
    put_artifact(&f.m2, "g", "D", "1.0", &simple_dep_pom(""));
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(!names.contains(&"C-1.0.jar".to_string()), "{names:?}");
    assert!(names.contains(&"D-1.0.jar".to_string()), "{names:?}");
}

#[test]
fn parent_pom_properties_resolve_version() {
    let f = fixture("parent-props");
    put_pom_only(
        &f.m2,
        "g",
        "parent",
        "1.0",
        "<project><groupId>g</groupId><artifactId>parent</artifactId><version>1.0</version>\
             <properties><x.version>2.0</x.version></properties></project>",
    );
    put_artifact(&f.m2, "g", "B", "2.0", &simple_dep_pom(""));
    std::fs::write(
            f.root.join("pom.xml"),
            "<project><parent><groupId>g</groupId><artifactId>parent</artifactId><version>1.0</version>\
             </parent><artifactId>x</artifactId>\
             <dependencies><dependency><groupId>g</groupId><artifactId>B</artifactId>\
             <version>${x.version}</version></dependency></dependencies></project>",
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-2.0.jar".to_string()), "{names:?}");
}

#[test]
fn relative_path_escaping_project_root_is_not_read() {
    let f = fixture("relpath-escape");
    // A "secret" pom OUTSIDE the project root (sibling of proj/ in the
    // fixture base). If the resolver followed the traversal it would
    // inherit x.version=9.9 and resolve B-9.9.
    let outside = f.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("pom.xml"),
        "<project><groupId>g</groupId><artifactId>evil</artifactId><version>1.0</version>\
             <properties><x.version>9.9</x.version></properties></project>",
    )
    .unwrap();
    put_artifact(&f.m2, "g", "B", "9.9", &simple_dep_pom(""));
    std::fs::write(
        f.root.join("pom.xml"),
        "<project><parent><groupId>g</groupId><artifactId>evil</artifactId><version>1.0</version>\
             <relativePath>../outside/pom.xml</relativePath></parent><artifactId>x</artifactId>\
             <dependencies><dependency><groupId>g</groupId><artifactId>B</artifactId>\
             <version>${x.version}</version></dependency></dependencies></project>",
    )
    .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(
        !names.contains(&"B-9.9.jar".to_string()),
        "traversal target was read: {names:?}"
    );
    assert!(
        result
            .degraded
            .iter()
            .any(|d| d.contains("parent g:evil:1.0")),
        "{:?}",
        result.degraded
    );
}

#[test]
fn deep_relative_path_traversal_is_rejected_without_panic() {
    let f = fixture("relpath-deep");
    std::fs::write(
        f.root.join("pom.xml"),
        "<project><parent><groupId>g</groupId><artifactId>evil</artifactId><version>1.0</version>\
             <relativePath>../../../../../../../../etc/hosts</relativePath></parent>\
             <artifactId>x</artifactId></project>",
    )
    .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    assert!(result.jars.is_empty());
    assert!(
        result
            .degraded
            .iter()
            .any(|d| d.contains("parent g:evil:1.0")),
        "{:?}",
        result.degraded
    );
}

#[test]
fn cached_pom_relative_path_is_ignored_parent_comes_from_cache() {
    let f = fixture("relpath-cache");
    // An "evil" pom placed exactly where the cached pom's relativePath
    // points (outside ~/.m2, inside the fixture base). If followed, it
    // would set x.version=9.9 and resolve C-9.9.
    let evil = f.base.join("evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(
        evil.join("pom.xml"),
        "<project><groupId>g</groupId><artifactId>parent</artifactId><version>1.0</version>\
             <properties><x.version>9.9</x.version></properties></project>",
    )
    .unwrap();
    // The legitimate parent, in the cache, pins x.version=1.0.
    put_pom_only(
        &f.m2,
        "g",
        "parent",
        "1.0",
        "<project><groupId>g</groupId><artifactId>parent</artifactId><version>1.0</version>\
             <properties><x.version>1.0</x.version></properties></project>",
    );
    // B's cached pom declares the parent WITH a malicious relativePath
    // (from m2/g/B/1.0/ four `..`s reach the fixture base).
    put_artifact(
        &f.m2,
        "g",
        "B",
        "1.0",
        "<project><parent><groupId>g</groupId><artifactId>parent</artifactId>\
             <version>1.0</version><relativePath>../../../../evil/pom.xml</relativePath></parent>\
             <artifactId>B</artifactId>\
             <dependencies><dependency><groupId>g</groupId><artifactId>C</artifactId>\
             <version>${x.version}</version></dependency></dependencies></project>",
    );
    put_artifact(&f.m2, "g", "C", "1.0", &simple_dep_pom(""));
    put_artifact(&f.m2, "g", "C", "9.9", &simple_dep_pom(""));
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(
        names.contains(&"C-1.0.jar".to_string()),
        "parent should come from the cache: {names:?}"
    );
    assert!(
        !names.contains(&"C-9.9.jar".to_string()),
        "cached pom's relativePath must be ignored: {names:?}"
    );
}

#[test]
fn project_local_parent_within_root_resolves() {
    let f = fixture("relpath-legit");
    put_artifact(&f.m2, "g", "B", "4.0", &simple_dep_pom(""));
    // Workspace root pom is the parent (properties) and the aggregator.
    std::fs::write(
        f.root.join("pom.xml"),
        "<project><groupId>g</groupId><artifactId>parent</artifactId><version>1.0</version>\
             <packaging>pom</packaging>\
             <properties><b.version>4.0</b.version></properties>\
             <modules><module>modA</module></modules></project>",
    )
    .unwrap();
    // The module inherits ${b.version} via the default ../pom.xml
    // relativePath — the parent is NOT in the cache.
    std::fs::create_dir_all(f.root.join("modA")).unwrap();
    std::fs::write(
        f.root.join("modA/pom.xml"),
        "<project><parent><groupId>g</groupId><artifactId>parent</artifactId>\
             <version>1.0</version></parent><artifactId>modA</artifactId>\
             <dependencies><dependency><groupId>g</groupId><artifactId>B</artifactId>\
             <version>${b.version}</version></dependency></dependencies></project>",
    )
    .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-4.0.jar".to_string()), "{names:?}");
}

#[test]
fn unresolved_version_records_degraded() {
    let f = fixture("unresolved-version");
    // Transitive case: B's pom has a versionless dep C.
    put_artifact(
        &f.m2,
        "g",
        "B",
        "1.0",
        &simple_dep_pom("<dependency><groupId>g</groupId><artifactId>C</artifactId></dependency>"),
    );
    // Root case: dep D declared without a version and no management.
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>\
                 <dependency><groupId>g</groupId><artifactId>D</artifactId></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    assert!(
        result
            .degraded
            .contains(&"g:D (unresolved version)".to_string()),
        "{:?}",
        result.degraded
    );
    assert!(
        result
            .degraded
            .contains(&"g:C (unresolved version)".to_string()),
        "{:?}",
        result.degraded
    );
}

#[test]
fn missing_parent_pom_records_degraded() {
    let f = fixture("missing-parent");
    put_artifact(&f.m2, "g", "B", "1.0", &simple_dep_pom(""));
    std::fs::write(
        f.root.join("pom.xml"),
        "<project><parent><groupId>g</groupId><artifactId>ghost</artifactId>\
             <version>7.0</version></parent><artifactId>x</artifactId>\
             <dependencies><dependency><groupId>g</groupId><artifactId>B</artifactId>\
             <version>1.0</version></dependency></dependencies></project>",
    )
    .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-1.0.jar".to_string()), "{names:?}");
    assert!(
        result
            .degraded
            .contains(&"x: parent g:ghost:7.0 unreadable".to_string()),
        "{:?}",
        result.degraded
    );
}

#[test]
fn classifier_dependency_records_degraded() {
    let f = fixture("classifier");
    put_artifact(&f.m2, "g", "B", "1.0", &simple_dep_pom(""));
    std::fs::write(
        f.root.join("pom.xml"),
        simple_dep_pom(
            "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version>\
                 <classifier>natives-linux</classifier></dependency>",
        ),
    )
    .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    assert!(result.jars.is_empty(), "{:?}", result.jars);
    assert!(
        result
            .degraded
            .contains(&"g:B:1.0 (classifier natives-linux unsupported)".to_string()),
        "{:?}",
        result.degraded
    );
}

#[test]
fn implicit_project_version_property_resolves_dependency_version() {
    let f = fixture("project-version");
    put_artifact(&f.m2, "g", "B", "2.5.0", &simple_dep_pom(""));
    std::fs::write(
        f.root.join("pom.xml"),
        "<project><groupId>g</groupId><artifactId>x</artifactId><version>2.5.0</version>\
             <dependencies><dependency><groupId>g</groupId><artifactId>B</artifactId>\
             <version>${project.version}</version></dependency></dependencies></project>",
    )
    .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-2.5.0.jar".to_string()), "{names:?}");
}

#[test]
fn bom_import_pins_versionless_dependency() {
    let f = fixture("bom");
    put_pom_only(
            &f.m2,
            "g",
            "bom",
            "1.0",
            "<project><groupId>g</groupId><artifactId>bom</artifactId><version>1.0</version>\
             <dependencyManagement><dependencies>\
             <dependency><groupId>g</groupId><artifactId>B</artifactId><version>3.0</version></dependency>\
             </dependencies></dependencyManagement></project>",
        );
    put_artifact(&f.m2, "g", "B", "3.0", &simple_dep_pom(""));
    std::fs::write(
        f.root.join("pom.xml"),
        "<project><groupId>g</groupId><artifactId>x</artifactId><version>1.0</version>\
             <dependencyManagement><dependencies>\
             <dependency><groupId>g</groupId><artifactId>bom</artifactId><version>1.0</version>\
             <type>pom</type><scope>import</scope></dependency>\
             </dependencies></dependencyManagement>\
             <dependencies><dependency><groupId>g</groupId><artifactId>B</artifactId></dependency>\
             </dependencies></project>",
    )
    .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-3.0.jar".to_string()), "{names:?}");
}

#[test]
fn cycle_terminates() {
    let f = fixture("cycle");
    put_artifact(
            &f.m2,
            "g",
            "X",
            "1.0",
            &simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>Y</artifactId><version>1.0</version></dependency>",
            ),
        );
    put_artifact(
            &f.m2,
            "g",
            "Y",
            "1.0",
            &simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>X</artifactId><version>1.0</version></dependency>",
            ),
        );
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>X</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"X-1.0.jar".to_string()));
    assert!(names.contains(&"Y-1.0.jar".to_string()));
}

#[test]
fn depth_bound_truncates_deep_chain() {
    let f = fixture("depth");
    const CHAIN_LEN: usize = 30;
    for i in 0..CHAIN_LEN {
        let next = if i + 1 < CHAIN_LEN {
            format!(
                    "<dependency><groupId>g</groupId><artifactId>N{}</artifactId><version>1.0</version></dependency>",
                    i + 1
                )
        } else {
            String::new()
        };
        put_artifact(&f.m2, "g", &format!("N{i}"), "1.0", &simple_dep_pom(&next));
    }
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>N0</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    let last = format!("N{}-1.0.jar", CHAIN_LEN - 1);
    assert!(!names.contains(&last), "{names:?}");
    assert!(names.contains(&"N0-1.0.jar".to_string()));
    assert!(
        result.degraded.iter().any(|d| d.contains("max depth")),
        "{:?}",
        result.degraded
    );
}

#[test]
fn missing_pom_skips_subtree_and_records_degraded() {
    let f = fixture("missing");
    put_artifact(
            &f.m2,
            "g",
            "B",
            "1.0",
            &simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>C</artifactId><version>1.0</version></dependency>",
            ),
        );
    // C is never created in the cache.
    std::fs::write(
            f.root.join("pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-1.0.jar".to_string()), "{names:?}");
    assert!(!names.contains(&"C-1.0.jar".to_string()), "{names:?}");
    assert!(
        result.degraded.iter().any(|d| d.contains("g:C:1.0")),
        "{:?}",
        result.degraded
    );
}

#[test]
fn multi_module_sibling_dep_and_source_root_surfaced() {
    let f = fixture("multimodule");
    put_artifact(&f.m2, "g", "B", "1.0", &simple_dep_pom(""));
    std::fs::write(
        f.root.join("pom.xml"),
        "<project><groupId>g</groupId><artifactId>parent</artifactId><version>1.0</version>\
             <packaging>pom</packaging>\
             <modules><module>modA</module><module>modB</module></modules></project>",
    )
    .unwrap();
    std::fs::create_dir_all(f.root.join("modA/src/main/java")).unwrap();
    std::fs::write(
            f.root.join("modA/pom.xml"),
            simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>B</artifactId><version>1.0</version></dependency>",
            ),
        )
        .unwrap();
    std::fs::create_dir_all(f.root.join("modB/src/main/java")).unwrap();
    std::fs::write(f.root.join("modB/pom.xml"), simple_dep_pom("")).unwrap();

    let locator = crate::maven::MavenLocator { m2_repo: &f.m2 };
    let result = resolve_maven_like_project(&f.root, &locator);
    let names: Vec<String> = result.jars.iter().map(|p| jar_name(p)).collect();
    assert!(names.contains(&"B-1.0.jar".to_string()), "{names:?}");
    assert!(
        result
            .source_roots
            .iter()
            .any(|p| p.ends_with("modA/src/main/java")),
        "{:?}",
        result.source_roots
    );
    assert!(
        result
            .source_roots
            .iter()
            .any(|p| p.ends_with("modB/src/main/java")),
        "{:?}",
        result.source_roots
    );
}
