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
//! pom/jar or parent pom, unresolved version, unsupported classifier,
//! depth/node bound hit) is recorded in `degraded` instead of failing the
//! whole resolution.
//!
//! Path safety: all cache paths flow through the coordinate guards
//! (`maven::unsafe_coord`); `<relativePath>` parent references are honored
//! only for project-local poms and are lexically containment-checked against
//! the project root before any file I/O (see [`parse_effective_pom`]) —
//! cache poms resolve their parent by coordinates alone.

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
    /// Problems hit while building the effective pom (e.g. a declared parent
    /// pom that could not be found in the local cache).
    pub degraded: Vec<String>,
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
///
/// `project_root` is `Some` only for poms that live inside the project being
/// resolved (the workspace root pom and its `<modules>` siblings): their
/// `<relativePath>` parent reference is honored, but strictly confined to the
/// project root (lexically normalized and containment-checked **before any
/// file I/O**, then canonicalize-re-checked against symlinks). For cache poms
/// (`~/.m2`, `~/.gradle`) it is `None` and `<relativePath>` is ignored
/// entirely — parents of repository poms resolve by coordinates only, which
/// is also Maven's own install-time behavior. A malicious cached pom can
/// therefore never steer the resolver to read a file outside the project
/// dir / cache roots.
pub(crate) fn parse_effective_pom(
    pom_path: &Path,
    locator: &dyn Locator,
    chain_depth: usize,
    project_root: Option<&Path>,
) -> Option<EffectivePom> {
    let raw = parse_effective_pom_raw(pom_path, locator, chain_depth, project_root)?;
    Some(EffectivePom {
        dependencies: raw.dependencies,
        modules: raw.modules,
        degraded: raw.degraded,
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
    degraded: Vec<String>,
}

/// Resolve a `<relativePath>` parent reference from a **project-local** pom,
/// refusing anything that would escape `project_root`. The containment check
/// is lexical (`..`/`.` components resolved without touching the filesystem),
/// so no out-of-root path is ever the subject of any file I/O; a follow-up
/// canonicalize re-check defends against symlinks inside the root pointing
/// out of it.
fn project_local_parent(pom_path: &Path, rel: &str, project_root: &Path) -> Option<PathBuf> {
    if rel.is_empty() || Path::new(rel).is_absolute() {
        return None;
    }
    let joined = pom_path.parent()?.join(rel);
    let candidate = lexical_normalize(&joined)?;
    let root = lexical_normalize(project_root)?;
    if !candidate.starts_with(&root) {
        return None; // would escape the project — never touched on disk.
    }
    if !candidate.is_file() {
        return None;
    }
    // Symlink defense: the real location must also be under the real root.
    let canon = candidate.canonicalize().ok()?;
    let canon_root = project_root.canonicalize().ok()?;
    canon.starts_with(&canon_root).then_some(candidate)
}

/// Resolve `.` and `..` components of `path` purely lexically (no file I/O).
/// `None` if `..` would climb above the path's root.
fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    let mut depth = 0usize; // Normal components currently in `out`
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None; // would climb above the root
                }
                out.pop();
                depth -= 1;
            }
            Component::Normal(part) => {
                out.push(part);
                depth += 1;
            }
        }
    }
    Some(out)
}

fn parse_effective_pom_raw(
    pom_path: &Path,
    locator: &dyn Locator,
    chain_depth: usize,
    project_root: Option<&Path>,
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
    let mut degraded: Vec<String> = Vec::new();
    if let Some(parent) = parent {
        let pg = child_text(parent, "groupId");
        let pa = child_text(parent, "artifactId");
        let pv = child_text(parent, "version");
        let rel_path = child_text(parent, "relativePath").unwrap_or_else(|| "../pom.xml".into());
        parent_group = pg.clone();
        parent_version = pv.clone();

        // <relativePath> is honored only for project-local poms, and only
        // within the project root (see `project_local_parent`). Cache poms
        // resolve their parent by coordinates alone.
        let local = project_root.and_then(|root| project_local_parent(pom_path, &rel_path, root));

        let parsed = if let Some(local_path) = local {
            parse_effective_pom_raw(&local_path, locator, chain_depth + 1, project_root)
        } else if let (Some(g), Some(a), Some(v)) = (&pg, &pa, &pv) {
            locator
                .locate_pom(g, a, v)
                .and_then(|p| parse_effective_pom_raw(&p, locator, chain_depth + 1, None))
        } else {
            None
        };

        match parsed {
            Some(parent_effective) => {
                parent_props = parent_effective.props;
                parent_managed = parent_effective.managed;
                parent_group = Some(parent_effective.group);
                parent_version = Some(parent_effective.version);
                degraded.extend(parent_effective.degraded);
            }
            None => {
                let child = child_text(project, "artifactId")
                    .or_else(|| {
                        pom_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default();
                let parent_coord = coord_str(
                    pg.as_deref().unwrap_or("?"),
                    pa.as_deref().unwrap_or("?"),
                    pv.as_deref().unwrap_or("?"),
                );
                degraded.push(format!("{child}: parent {parent_coord} unreadable"));
            }
        }
    }

    let mut raw = parse_effective_pom_raw_from_doc(
        project,
        locator,
        chain_depth,
        parent_props,
        parent_managed,
        parent_group,
        parent_version,
    );
    degraded.append(&mut raw.degraded);
    raw.degraded = degraded;
    Some(raw)
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

    // Capture the parent's coordinates before `.or()` consumes them below:
    // Maven exposes them as the built-in `${project.parent.version}` /
    // `${project.parent.groupId}` properties, and real POMs use them to version
    // a dependency against their own parent (e.g. swagger-core-jakarta declares
    // swagger-annotations-jakarta at `${project.parent.version}`). Without these
    // the dependency version stays unresolved and the artifact is dropped.
    let parent_group_prop = parent_group.clone();
    let parent_version_prop = parent_version.clone();

    let group = own_group.or(parent_group).unwrap_or_default();
    let version = own_version.or(parent_version).unwrap_or_default();

    // Implicit properties, resolved after inheritance so `${project.version}`
    // etc. see the effective values.
    props.insert("project.version".into(), version.clone());
    props.insert("project.groupId".into(), group.clone());
    props.insert("project.artifactId".into(), own_artifact.clone());
    props.insert("pom.version".into(), version.clone());
    if let Some(pv) = &parent_version_prop {
        props.insert("project.parent.version".into(), pv.clone());
        props.insert("pom.parent.version".into(), pv.clone());
    }
    if let Some(pg) = &parent_group_prop {
        props.insert("project.parent.groupId".into(), pg.clone());
    }

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
                    // BOMs are located in the cache, so `<relativePath>`
                    // handling is off (`project_root = None`).
                    if let Some(bom_version) = &version_txt {
                        if let Some(bom_path) = locator.locate_pom(&g, &a, bom_version) {
                            if let Some(bom) =
                                parse_effective_pom_raw(&bom_path, locator, chain_depth + 1, None)
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
        degraded: Vec::new(),
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

/// Classifier variants (out of scope beyond the implicit none/`-sources`)
/// are not resolved; the drop is made visible via a `degraded` record.
fn record_classifier_skip(dep: &RawDep, classifier: &str, degraded: &mut Vec<String>) {
    degraded.push(format!(
        "{}:{}:{} (classifier {classifier} unsupported)",
        dep.group,
        dep.artifact,
        dep.version.as_deref().unwrap_or("?"),
    ));
}

fn record_unresolved_version(dep: &RawDep, degraded: &mut Vec<String>) {
    degraded.push(format!(
        "{}:{} (unresolved version)",
        dep.group, dep.artifact
    ));
}

/// Seed the graph from a pom's own direct dependencies (root pom, or a
/// sibling multi-module pom — both are "depth 0" for nearest-wins purposes).
/// Root-level scope policy: include the project's own dependencies of every
/// scope, `test` included. The project's own test sources (`src/test/java`)
/// are analyzed just like `src/main/java`, so their test-scope libraries
/// (JUnit, AssertJ, Mockito, …) must be on the classpath or every test import
/// resolves as missing. Test scope is still dropped *transitively* (see
/// `resolve_transitive`), so seeding a root test dep pulls in its own
/// compile/runtime dependencies but never another library's test deps.
/// Root-declared `optional` deps are likewise kept (the owning project itself
/// compiles against them). Deps that can't be seeded (classifier variant,
/// unresolved version) are recorded in `degraded`.
fn root_seeds(effective: &EffectivePom, seeds: &mut Vec<Seed>, degraded: &mut Vec<String>) {
    for dep in &effective.dependencies {
        if let Some(classifier) = &dep.classifier {
            record_classifier_skip(dep, classifier, degraded);
            continue;
        }
        let Some(version) = &dep.version else {
            record_unresolved_version(dep, degraded);
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
        // Cache poms never honor `<relativePath>` (`project_root = None`).
        let Some(effective) = parse_effective_pom(&pom_path, locator, 0, None) else {
            degraded.push(coord_str(&seed.group, &seed.artifact, &seed.version));
            continue;
        };
        degraded.extend(effective.degraded.iter().cloned());

        for dep in &effective.dependencies {
            // Transitive-edge filter: only `compile`/`runtime`, never
            // `optional` (both are silent by design — correct Maven
            // semantics, not degradation).
            if (dep.scope != "compile" && dep.scope != "runtime") || dep.optional {
                continue;
            }
            if seed
                .exclusions
                .contains(&(dep.group.clone(), dep.artifact.clone()))
            {
                continue;
            }
            if let Some(classifier) = &dep.classifier {
                record_classifier_skip(dep, classifier, &mut degraded);
                continue;
            }
            let Some(version) = &dep.version else {
                record_unresolved_version(dep, &mut degraded);
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
    let Some(effective) = parse_effective_pom(&root_pom, locator, 0, Some(project_root)) else {
        return ResolvedProject::default();
    };

    let mut seeds = Vec::new();
    let mut source_roots = Vec::new();
    let mut degraded = effective.degraded.clone();

    root_seeds(&effective, &mut seeds, &mut degraded);
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
        match parse_effective_pom(&module_pom, locator, 0, Some(project_root)) {
            Some(module_effective) => {
                degraded.extend(module_effective.degraded.iter().cloned());
                root_seeds(&module_effective, &mut seeds, &mut degraded);
            }
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
#[path = "resolve_tests.rs"]
mod tests;
