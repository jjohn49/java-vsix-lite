//! Shared, static, offline transitive-dependency resolver core.
//!
//! Both the Maven and Gradle backends resolve a graph of Maven-format POMs
//! (Gradle caches the same POM metadata alongside its jars) purely from the
//! local cache — **never the network, never a build tool**. The only thing
//! that differs between the two backends is *where* a coordinate's pom/jar
//! live on disk, which is abstracted behind the [`Locator`] trait.
//!
//! Maven semantics implemented (the documented "minimum" set):
//! - parent-POM chain, with `<properties>` inheritance
//! - `<dependencyManagement>`, including `scope=import` BOM merges
//! - scope filtering (`compile`/`runtime` transitively; root POMs may also
//!   keep `provided`; `test` is always dropped)
//! - `<optional>` transitives skipped
//! - `<exclusions>` honored along the winning path
//! - nearest-wins version mediation (breadth-first; first-declared wins ties)
//!
//! Hard bounds: max depth 25, max 2,000 visited nodes, cycle detection on
//! `(group, artifact)`. A dependency the resolver can't finish (missing
//! pom/jar, depth/node bound hit) is recorded in `degraded` instead of
//! failing the whole resolution.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Refuse to read a pom larger than this (defense against a maliciously huge
/// file being parsed into memory).
pub(crate) const MAX_POM_BYTES: usize = 4 * 1024 * 1024;
/// Hard cap on BFS depth (and, reused for simplicity, on parent/BOM chain
/// recursion depth).
const MAX_DEPTH: usize = 25;
/// Hard cap on the number of distinct (group, artifact) nodes visited.
const MAX_NODES: usize = 2_000;

/// Locates cached artifacts for a Maven coordinate. The only difference
/// between the Maven (`~/.m2/repository`) and Gradle
/// (`~/.gradle/caches/modules-2/files-2.1`) backends.
pub(crate) trait Locator {
    fn locate_pom(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf>;
    fn locate_jar(&self, group: &str, artifact: &str, version: &str) -> Option<PathBuf>;
}

/// The result of resolving a project's dependency graph.
#[derive(Debug, Default, Clone)]
pub(crate) struct ResolvedProject {
    /// Jars on the classpath (direct + transitive), nearest-wins mediated.
    pub jars: Vec<PathBuf>,
    /// Extra source roots (multi-module sibling `src/main/java` dirs).
    pub source_roots: Vec<PathBuf>,
    /// Coordinates that could not be fully resolved (missing pom/jar, or a
    /// hard bound was hit), for "IntelliSense partial: N unresolved deps".
    pub degraded: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RawDep {
    pub group: String,
    pub artifact: String,
    /// `None` when the version could not be resolved (no literal version and
    /// no `dependencyManagement` entry covers it).
    pub version: Option<String>,
    pub scope: String,
    pub optional: bool,
    pub classifier: Option<String>,
    pub exclusions: HashSet<(String, String)>,
}

#[derive(Debug, Clone, Default)]
struct ManagedDep {
    version: Option<String>,
    scope: Option<String>,
}

/// A pom fully resolved against its parent chain and its own/imported
/// `dependencyManagement`: properties substituted, versions filled in from
/// management where the dependency omits one.
#[derive(Debug, Clone, Default)]
pub(crate) struct EffectivePom {
    pub dependencies: Vec<RawDep>,
    pub modules: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct Seed {
    group: String,
    artifact: String,
    version: String,
    exclusions: Rc<HashSet<(String, String)>>,
}

fn coord_str(group: &str, artifact: &str, version: &str) -> String {
    format!("{group}:{artifact}:{version}")
}

/// Parse `pom_path` into its effective form: parent chain merged in
/// (properties + dependencyManagement), own `dependencyManagement`
/// (including `scope=import` BOMs) applied, own `<dependencies>` versions
/// filled in from management where absent. `chain_depth` bounds parent/BOM
/// recursion (reuses the same hard cap as BFS depth).
pub(crate) fn parse_effective_pom(
    pom_path: &Path,
    locator: &dyn Locator,
    chain_depth: usize,
) -> Option<EffectivePom> {
    let raw = parse_effective_pom_raw(pom_path, locator, chain_depth)?;
    Some(EffectivePom {
        dependencies: raw.dependencies,
        modules: raw.modules,
    })
}

/// Internal richer result (also exposes merged `props`/`managed`, needed when
/// this pom itself is acting as a parent for a child).
struct RawEffective {
    group: String,
    version: String,
    props: HashMap<String, String>,
    managed: HashMap<(String, String), ManagedDep>,
    dependencies: Vec<RawDep>,
    modules: Vec<String>,
}

fn parse_effective_pom_raw(
    pom_path: &Path,
    locator: &dyn Locator,
    chain_depth: usize,
) -> Option<RawEffective> {
    if chain_depth > MAX_DEPTH {
        return None;
    }
    if std::fs::metadata(pom_path)
        .map(|m| m.len())
        .unwrap_or(u64::MAX)
        > MAX_POM_BYTES as u64
    {
        return None;
    }
    let text = std::fs::read_to_string(pom_path).ok()?;
    let doc = roxmltree::Document::parse(&text).ok()?;
    let project = doc.descendants().find(|n| n.has_tag_name("project"))?;

    let parent = project
        .children()
        .find(|n| n.is_element() && n.has_tag_name("parent"));
    let mut parent_props: HashMap<String, String> = HashMap::new();
    let mut parent_managed: HashMap<(String, String), ManagedDep> = HashMap::new();
    let mut parent_group = None;
    let mut parent_version = None;
    if let Some(parent) = parent {
        let pg = child_text(parent, "groupId");
        let pa = child_text(parent, "artifactId");
        let pv = child_text(parent, "version");
        let rel_path = child_text(parent, "relativePath").unwrap_or_else(|| "../pom.xml".into());
        parent_group = pg.clone();
        parent_version = pv.clone();

        let parent_pom_path = if !Path::new(&rel_path).is_absolute() && !rel_path.is_empty() {
            pom_path.parent().map(|dir| dir.join(&rel_path))
        } else {
            None
        }
        .filter(|p| p.is_file())
        .or_else(|| match (&pg, &pa, &pv) {
            (Some(g), Some(a), Some(v)) => locator.locate_pom(g, a, v),
            _ => None,
        });

        if let Some(parent_pom_path) = parent_pom_path {
            if let Some(parent_effective) =
                parse_effective_pom_raw(&parent_pom_path, locator, chain_depth + 1)
            {
                parent_props = parent_effective.props;
                parent_managed = parent_effective.managed;
                parent_group = Some(parent_effective.group);
                parent_version = Some(parent_effective.version);
            }
        }
    }

    Some(parse_effective_pom_raw_from_doc(
        project,
        locator,
        chain_depth,
        parent_props,
        parent_managed,
        parent_group,
        parent_version,
    ))
}

#[allow(clippy::too_many_arguments)]
fn parse_effective_pom_raw_from_doc(
    project: roxmltree::Node,
    locator: &dyn Locator,
    chain_depth: usize,
    parent_props: HashMap<String, String>,
    parent_managed: HashMap<(String, String), ManagedDep>,
    parent_group: Option<String>,
    parent_version: Option<String>,
) -> RawEffective {
    // Properties: parent's, overlaid with this pom's own <properties>.
    let mut props = parent_props;
    if let Some(properties) = project
        .children()
        .find(|n| n.is_element() && n.has_tag_name("properties"))
    {
        for prop in properties.children().filter(|n| n.is_element()) {
            if let Some(text) = prop.text() {
                props.insert(prop.tag_name().name().to_string(), text.trim().to_string());
            }
        }
    }

    let own_group = child_text(project, "groupId").map(|g| substitute(&g, &props));
    let own_artifact = child_text(project, "artifactId")
        .map(|a| substitute(&a, &props))
        .unwrap_or_default();
    let own_version = child_text(project, "version").map(|v| substitute(&v, &props));

    let group = own_group.or(parent_group).unwrap_or_default();
    let version = own_version.or(parent_version).unwrap_or_default();

    // Implicit properties, resolved after inheritance so `${project.version}`
    // etc. see the effective values.
    props.insert("project.version".into(), version.clone());
    props.insert("project.groupId".into(), group.clone());
    props.insert("project.artifactId".into(), own_artifact.clone());
    props.insert("pom.version".into(), version.clone());

    // dependencyManagement: parent's, overlaid with this pom's own entries
    // (including scope=import BOM merges) — this pom's declarations always
    // win over inherited ones; within this pom, first-declared wins.
    let mut managed = parent_managed;
    if let Some(dep_mgmt) = project
        .children()
        .find(|n| n.is_element() && n.has_tag_name("dependencyManagement"))
    {
        let mut own_managed: HashMap<(String, String), ManagedDep> = HashMap::new();
        if let Some(deps) = dep_mgmt
            .children()
            .find(|n| n.is_element() && n.has_tag_name("dependencies"))
        {
            for dep in deps
                .children()
                .filter(|n| n.is_element() && n.has_tag_name("dependency"))
            {
                let Some(g) = child_text(dep, "groupId").map(|g| substitute(&g, &props)) else {
                    continue;
                };
                let Some(a) = child_text(dep, "artifactId").map(|a| substitute(&a, &props)) else {
                    continue;
                };
                let scope = child_text(dep, "scope").map(|s| substitute(&s, &props));
                let version_txt = child_text(dep, "version").map(|v| substitute(&v, &props));

                if scope.as_deref() == Some("import") {
                    // BOM import: merge its managed entries in, first-wins
                    // among entries declared in *this* pom (imports included).
                    if let Some(bom_version) = &version_txt {
                        if let Some(bom_path) = locator.locate_pom(&g, &a, bom_version) {
                            if let Some(bom) =
                                parse_effective_pom_raw(&bom_path, locator, chain_depth + 1)
                            {
                                for (k, v) in bom.managed {
                                    own_managed.entry(k).or_insert(v);
                                }
                            }
                        }
                    }
                    continue;
                }
                own_managed.entry((g, a)).or_insert(ManagedDep {
                    version: version_txt,
                    scope,
                });
            }
        }
        for (k, v) in own_managed {
            managed.insert(k, v);
        }
    }

    // This pom's own <project><dependencies><dependency> (not management).
    let mut dependencies = Vec::new();
    if let Some(deps) = project
        .children()
        .find(|n| n.is_element() && n.has_tag_name("dependencies"))
    {
        for dep in deps
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("dependency"))
        {
            let Some(g) = child_text(dep, "groupId").map(|g| substitute(&g, &props)) else {
                continue;
            };
            let Some(a) = child_text(dep, "artifactId").map(|a| substitute(&a, &props)) else {
                continue;
            };
            let mut version = child_text(dep, "version").map(|v| substitute(&v, &props));
            if version
                .as_ref()
                .is_none_or(|v| v.is_empty() || v.contains("${"))
            {
                version = managed
                    .get(&(g.clone(), a.clone()))
                    .and_then(|m| m.version.clone());
            }
            let mut scope = child_text(dep, "scope").map(|s| substitute(&s, &props));
            if scope.as_ref().is_none_or(|s| s.is_empty()) {
                scope = managed
                    .get(&(g.clone(), a.clone()))
                    .and_then(|m| m.scope.clone());
            }
            let scope = scope.unwrap_or_else(|| "compile".to_string());
            let optional = child_text(dep, "optional")
                .map(|o| substitute(&o, &props).eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let classifier = child_text(dep, "classifier").map(|c| substitute(&c, &props));
            let mut exclusions = HashSet::new();
            if let Some(excl_list) = dep
                .children()
                .find(|n| n.is_element() && n.has_tag_name("exclusions"))
            {
                for excl in excl_list
                    .children()
                    .filter(|n| n.is_element() && n.has_tag_name("exclusion"))
                {
                    let eg = child_text(excl, "groupId").map(|g| substitute(&g, &props));
                    let ea = child_text(excl, "artifactId").map(|a| substitute(&a, &props));
                    if let (Some(eg), Some(ea)) = (eg, ea) {
                        exclusions.insert((eg, ea));
                    }
                }
            }
            dependencies.push(RawDep {
                group: g,
                artifact: a,
                version,
                scope,
                optional,
                classifier,
                exclusions,
            });
        }
    }

    // <modules> (multi-module aggregator poms).
    let mut modules = Vec::new();
    if let Some(mods) = project
        .children()
        .find(|n| n.is_element() && n.has_tag_name("modules"))
    {
        for m in mods
            .children()
            .filter(|n| n.is_element() && n.has_tag_name("module"))
        {
            if let Some(text) = m.text() {
                modules.push(substitute(text.trim(), &props));
            }
        }
    }

    RawEffective {
        group,
        version,
        props,
        managed,
        dependencies,
        modules,
    }
}

/// Substitute `${prop}` references, repeating a few passes so that
/// property-references-property chains (e.g. `${a}` -> `${b}` -> literal)
/// resolve. Bounded so a malicious/cyclic property set can't loop forever.
pub(crate) fn substitute(value: &str, props: &HashMap<String, String>) -> String {
    let mut out = value.to_string();
    for _ in 0..5 {
        let mut changed = false;
        for (key, val) in props {
            let needle = format!("${{{key}}}");
            if out.contains(&needle) {
                out = out.replace(&needle, val);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    out
}

pub(crate) fn child_text(node: roxmltree::Node, tag: &str) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Root-level dependency filter: drop `test`; keep `compile`/`runtime`/
/// `provided`. Root-declared `optional` deps are still included (the owning
/// project itself compiles against them).
fn keep_at_root(dep: &RawDep) -> bool {
    dep.scope != "test" && dep.classifier.is_none()
}

/// Transitive-edge filter: only `compile`/`runtime`, never `optional`, never
/// a classifier variant (out of scope beyond none/sources).
fn keep_transitively(dep: &RawDep) -> bool {
    (dep.scope == "compile" || dep.scope == "runtime") && !dep.optional && dep.classifier.is_none()
}

/// Seed the graph from a pom's own direct dependencies (root pom, or a
/// sibling multi-module pom — both are "depth 0" for nearest-wins purposes).
fn root_seeds(effective: &EffectivePom, seeds: &mut Vec<Seed>) {
    for dep in &effective.dependencies {
        if !keep_at_root(dep) {
            continue;
        }
        let Some(version) = &dep.version else {
            continue;
        };
        seeds.push(Seed {
            group: dep.group.clone(),
            artifact: dep.artifact.clone(),
            version: version.clone(),
            exclusions: Rc::new(dep.exclusions.clone()),
        });
    }
}

/// Bare coordinate seeds (Gradle: statically scraped `g:a:v` literals, no
/// pom to derive scope/exclusions from — treated as root-level compile
/// deps).
pub(crate) fn coord_seeds(coords: &[(String, String, String)]) -> Vec<Seed> {
    coords
        .iter()
        .map(|(g, a, v)| Seed {
            group: g.clone(),
            artifact: a.clone(),
            version: v.clone(),
            exclusions: Rc::new(HashSet::new()),
        })
        .collect()
}

/// Breadth-first, nearest-wins transitive resolution from a set of root-level
/// seeds. Every node's own pom is (re)parsed from the cache via `locator`;
/// missing poms/jars degrade gracefully instead of failing the resolution.
pub(crate) fn resolve_transitive(
    seeds: Vec<Seed>,
    locator: &dyn Locator,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut degraded = Vec::new();
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queue: VecDeque<(Seed, usize)> = seeds.into_iter().map(|s| (s, 0)).collect();
    let mut jars = Vec::new();
    let mut node_budget = MAX_NODES;

    while let Some((seed, depth)) = queue.pop_front() {
        let key = (seed.group.clone(), seed.artifact.clone());
        if visited.contains(&key) {
            continue; // nearest-wins: first (shallowest, first-declared) visit already won.
        }
        if node_budget == 0 {
            degraded.push(format!(
                "resolution truncated: node limit ({MAX_NODES}) exceeded"
            ));
            break;
        }
        visited.insert(key);
        node_budget -= 1;

        if depth > MAX_DEPTH {
            degraded.push(format!(
                "{} (max depth exceeded)",
                coord_str(&seed.group, &seed.artifact, &seed.version)
            ));
            continue;
        }

        let jar = locator.locate_jar(&seed.group, &seed.artifact, &seed.version);
        let Some(jar) = jar else {
            degraded.push(coord_str(&seed.group, &seed.artifact, &seed.version));
            continue;
        };
        jars.push(jar);

        let Some(pom_path) = locator.locate_pom(&seed.group, &seed.artifact, &seed.version) else {
            degraded.push(coord_str(&seed.group, &seed.artifact, &seed.version));
            continue;
        };
        let Some(effective) = parse_effective_pom(&pom_path, locator, 0) else {
            degraded.push(coord_str(&seed.group, &seed.artifact, &seed.version));
            continue;
        };

        for dep in &effective.dependencies {
            if !keep_transitively(dep) {
                continue;
            }
            if seed
                .exclusions
                .contains(&(dep.group.clone(), dep.artifact.clone()))
            {
                continue;
            }
            let Some(version) = &dep.version else {
                continue;
            };
            let mut child_exclusions = (*seed.exclusions).clone();
            for ex in &dep.exclusions {
                child_exclusions.insert(ex.clone());
            }
            queue.push_back((
                Seed {
                    group: dep.group.clone(),
                    artifact: dep.artifact.clone(),
                    version: version.clone(),
                    exclusions: Rc::new(child_exclusions),
                },
                depth + 1,
            ));
        }
    }

    (jars, degraded)
}

/// Resolve a Maven-shaped project rooted at `root_pom`'s directory: the pom's
/// own direct deps, walking `<modules>` for sibling contributions and extra
/// source roots, then the full transitive graph.
pub(crate) fn resolve_maven_like_project(
    project_root: &Path,
    locator: &dyn Locator,
) -> ResolvedProject {
    let root_pom = project_root.join("pom.xml");
    let Some(effective) = parse_effective_pom(&root_pom, locator, 0) else {
        return ResolvedProject::default();
    };

    let mut seeds = Vec::new();
    let mut source_roots = Vec::new();
    let mut degraded = Vec::new();

    root_seeds(&effective, &mut seeds);
    let root_src = project_root.join("src/main/java");
    if root_src.is_dir() {
        source_roots.push(root_src);
    }

    for module in &effective.modules {
        if module.is_empty() || module.contains("..") || Path::new(module).is_absolute() {
            continue;
        }
        let module_dir = project_root.join(module);
        let module_pom = module_dir.join("pom.xml");
        match parse_effective_pom(&module_pom, locator, 0) {
            Some(module_effective) => root_seeds(&module_effective, &mut seeds),
            None => degraded.push(format!("module {module} (pom unreadable)")),
        }
        let module_src = module_dir.join("src/main/java");
        if module_src.is_dir() {
            source_roots.push(module_src);
        }
    }

    let (jars, mut transitive_degraded) = resolve_transitive(seeds, locator);
    degraded.append(&mut transitive_degraded);

    ResolvedProject {
        jars,
        source_roots,
        degraded,
    }
}

#[cfg(test)]
mod tests {
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
}
