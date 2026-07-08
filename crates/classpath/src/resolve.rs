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
/// Root-level filter: drop `test`; keep `compile`/`runtime`/`provided`.
/// Root-declared `optional` deps are still included (the owning project
/// itself compiles against them). Deps that can't be seeded (classifier
/// variant, unresolved version) are recorded in `degraded`.
fn root_seeds(effective: &EffectivePom, seeds: &mut Vec<Seed>, degraded: &mut Vec<String>) {
    for dep in &effective.dependencies {
        if dep.scope == "test" {
            continue;
        }
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
            &simple_dep_pom(
                "<dependency><groupId>g</groupId><artifactId>C</artifactId></dependency>",
            ),
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
}
