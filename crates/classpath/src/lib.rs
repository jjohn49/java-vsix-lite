//! Signature-level symbols for imported types (JDK + declared dependencies).
//!
//! Reads `.class` bytecode out of jmod/jar archives with `cafebabe` to expose,
//! for a fully-qualified type name, its members and supertypes — **without
//! running a JVM or any project code**. All archive and bytecode IO is isolated
//! here; the analysis crate (`jvl-syntax`) stays pure and talks to this through a
//! trait.
//!
//! Lookups are `&self` and cached (archives are immutable), so the server can
//! share one [`Classpath`] across requests without blocking document parsing.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

mod class_info;
mod generics;
mod gradle;
mod index;
mod jdk;
mod maven;
mod resolve;
mod zip;

pub use index::TypeEntry;
use index::TypeIndex;
/// Re-exported so the server's `javac` locator can fall back to
/// the same filesystem-probing JDK discovery the classpath layer uses — a
/// GUI-launched editor has no `$JAVA_HOME`, but the JDK is still findable.
pub use jdk::{best_jdk, jdk_feature_version};
use zip::ZipArchive;

/// A type read from bytecode: its fully-qualified name, its direct supertypes
/// (superclass + interfaces, as FQNs), its formal type parameters, and its
/// visible members.
#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub fqn: String,
    pub supers: Vec<String>,
    /// Formal type-parameter names, e.g. `["E"]` for `ArrayList<E>`.
    pub type_params: Vec<String>,
    pub members: Vec<Member>,
    /// Type arguments applied to each entry of `supers` (index-aligned:
    /// superclass, then interfaces, in that order), from the class's own
    /// `Signature` attribute — e.g. for `class MyList extends
    /// AbstractList<String>`, `super_type_args[0]` is `["String"]`. An
    /// argument that is one of *this* class's own type parameters renders as
    /// a `{i}` placeholder (as in [`Member::template`]), so a caller can
    /// substitute inherited-member templates through the hierarchy from a
    /// use-site instantiation. Empty for a non-generic supertype entry, or
    /// when there's no `Signature` attribute / it fails to parse.
    pub super_type_args: Vec<Vec<String>>,
}

/// One member of a class: a method or field.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
    /// Raw (generics-erased) signature, e.g. `boolean add(Object)` or `int size`.
    /// Stable across declarations, so it doubles as a dedup key.
    pub signature: String,
    /// Generic signature with `{i}` placeholders for the class's type parameters,
    /// e.g. `boolean add({0})`. `None` when the member uses no type variables.
    pub template: Option<String>,
    pub is_static: bool,
    /// Dotted FQN of the **erased** method return type / field declared
    /// type, from the descriptor (`Ljava/util/stream/Stream;` →
    /// `java.util.stream.Stream`) — what a `recv.member().` chain resolves
    /// through. `None` for primitives, `void`, arrays, and constructors.
    pub ret_fqn: Option<String>,
    /// The generic return/field type alone, in the same `{i}` template
    /// convention as [`Member::template`] (`Stream<{0}>`, `{0}`), so a chain
    /// can substitute use-site type arguments before re-resolving. `None`
    /// without a `Signature` attribute, for `void`, and for constructors.
    pub ret_display: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Method,
    Field,
    /// An `<init>` method, surfaced as `ClassName(paramTypes)` (see
    /// `class_info::parse`'s `<init>` handling). `Member::name` is the
    /// declaring class's simple name — also the docsrc lookup key for
    /// recovering its Javadoc from a source archive (which has no notion of
    /// `<init>`, only a constructor declaration named after its class).
    Constructor,
}

/// An archive on the classpath plus the entry-name prefix for its class files
/// (`classes/` inside a jmod, empty inside a jar).
struct Archive {
    zip: ZipArchive,
    prefix: &'static str,
}

/// A source archive (the JDK's `src.zip` or a dependency `-sources.jar`) used to
/// recover Javadoc. `suffix_match` is set for src.zip, whose entries are
/// module-prefixed (`java.base/java/util/List.java`).
struct SourceArchive {
    zip: ZipArchive,
    suffix_match: bool,
}

/// An ordered set of archives (JDK jmods + dependency jars) answering
/// "what are the members of fully-qualified type `X`?", with a result cache.
/// Parallel source archives provide Javadoc.
pub struct Classpath {
    archives: Vec<Archive>,
    sources: Vec<SourceArchive>,
    cache: RwLock<HashMap<String, Option<Arc<ClassInfo>>>>,
    source_cache: RwLock<HashMap<String, Option<Arc<String>>>>,
    /// Extra source roots surfaced by multi-module Maven resolution (sibling
    /// modules' `src/main/java`), beyond the project root itself.
    source_roots: Vec<PathBuf>,
    /// Coordinates that could not be fully resolved (missing pom/jar in the
    /// local cache, or a resolution bound was hit) — e.g. for surfacing
    /// "IntelliSense partial: N unresolved deps" to the user.
    degraded: Vec<String>,
    /// Every dependency jar path added via [`Classpath::add_jar`], in
    /// the order added — a record of what was already resolved, not a new
    /// resolution path. This is the only external caller of this crate that
    /// needs real filesystem paths rather than bytecode lookups: the one-shot
    /// `javac` check command builds its `-cp` argument from these (JDK jmods
    /// are deliberately excluded — javac's own installation already supplies
    /// its bootclasspath, and jmods aren't valid `-cp` entries anyway).
    entries: Vec<PathBuf>,
    /// Lazily-built type-name index over every archive's central
    /// directory (see [`index`]) — powers classpath type-name completion,
    /// auto-import, and import-path completion. Built at most once per
    /// `Classpath`; a rebuild swaps in a whole new `Classpath`, so the index
    /// can never go stale relative to its archives.
    name_index: OnceLock<TypeIndex>,
}

impl Classpath {
    /// An empty classpath (resolves nothing) — used as a safe fallback when no
    /// JDK is found and in tests.
    pub fn empty() -> Classpath {
        Classpath {
            archives: Vec::new(),
            sources: Vec::new(),
            cache: RwLock::new(HashMap::new()),
            source_cache: RwLock::new(HashMap::new()),
            source_roots: Vec::new(),
            degraded: Vec::new(),
            entries: Vec::new(),
            name_index: OnceLock::new(),
        }
    }

    /// Build a classpath from the best available JDK's jmods (and its `src.zip`
    /// for Javadoc). Empty if none.
    pub fn from_jdk() -> Classpath {
        let mut cp = Classpath::empty();
        if let Some(home) = jdk::best_jdk() {
            for jmod in jdk::jmods(&home) {
                // jmod = 4-byte `JM` header, then a standard ZIP.
                if let Some(zip) = ZipArchive::open(&jmod, 4) {
                    cp.archives.push(Archive {
                        zip,
                        prefix: "classes/",
                    });
                }
            }
            // src.zip holds the JDK source (module-prefixed) for Javadoc.
            if let Some(zip) = ZipArchive::open(&home.join("lib/src.zip"), 0) {
                cp.sources.push(SourceArchive {
                    zip,
                    suffix_match: true,
                });
            }
        }
        cp
    }

    /// The JDK classpath plus a project's dependency jars, resolved
    /// **transitively** and statically (Maven `pom.xml` + `~/.m2/repository`
    /// parent/BOM/exclusion/scope semantics; Gradle build files +
    /// `libs.versions.toml` scraped, then walked through `~/.gradle/caches`'
    /// cached POMs) from `root`. No build tool is executed, nothing is ever
    /// fetched from the network; resolution is bounded (depth/node caps) and
    /// degrades gracefully — see [`Classpath::degraded`].
    pub fn from_jdk_and_project(root: Option<&Path>) -> Classpath {
        let mut cp = Classpath::from_jdk();
        if let (Some(root), Some(home)) = (root, home_dir()) {
            let maven = maven::resolve_project(root, &home.join(".m2/repository"));
            for jar in &maven.jars {
                cp.add_jar(jar);
            }
            cp.source_roots.extend(maven.source_roots);
            cp.degraded.extend(maven.degraded);

            // Gradle's cache honors `$GRADLE_USER_HOME` (commonly relocated
            // outside `$HOME` in CI and governed environments like Foundry),
            // falling back to `~/.gradle`.
            let gradle_caches = gradle_user_home()
                .unwrap_or_else(|| home.join(".gradle"))
                .join("caches");
            let gradle =
                gradle::resolve_project(root, &gradle_caches, &home.join(".m2/repository"));
            for jar in &gradle.jars {
                cp.add_jar(jar);
            }
            cp.degraded.extend(gradle.degraded);
        }
        cp
    }

    /// Extra source roots surfaced by multi-module Maven resolution (sibling
    /// modules' `src/main/java` directories), beyond the project root itself.
    /// Empty unless `root` was a multi-module Maven reactor.
    pub fn source_roots(&self) -> &[PathBuf] {
        &self.source_roots
    }

    /// Dependency jar paths added via [`Classpath::add_jar`] (JDK jmods are
    /// not included — see the field doc on `entries`). The one-shot
    /// `javac` check command's `-cp` argument.
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// Dependency coordinates that could not be fully resolved from the local
    /// cache (missing pom/jar, or a resolution bound was hit), e.g. to
    /// surface "IntelliSense partial: N unresolved deps" to the user.
    pub fn degraded(&self) -> &[String] {
        &self.degraded
    }

    /// [`degraded`](Self::degraded) entries parsed into structured
    /// coordinates, for the `jvl/missingDependencies` request that backs the
    /// consent-gated dependency download command. Unparseable entries
    /// (project-structure problems like an unreadable parent/module pom, or
    /// a resolver-bound message) are silently dropped — they don't name a
    /// single fetchable coordinate, so there's nothing actionable to offer.
    /// See [`parse_degraded_entry`] for exactly which shapes survive.
    pub fn missing_dependencies(&self) -> Vec<DegradedCoordinate> {
        self.degraded
            .iter()
            .filter_map(|entry| parse_degraded_entry(entry))
            .collect()
    }

    /// Add a dependency jar to the classpath (no-op if it can't be opened). Its
    /// sibling `-sources.jar`, if present, is registered for Javadoc.
    pub fn add_jar(&mut self, path: &Path) {
        if let Some(zip) = ZipArchive::open(path, 0) {
            self.archives.push(Archive { zip, prefix: "" });
            self.entries.push(path.to_path_buf());
        }
        if let Some(sources) = sources_jar_path(path) {
            if let Some(zip) = ZipArchive::open(&sources, 0) {
                self.sources.push(SourceArchive {
                    zip,
                    suffix_match: false,
                });
            }
        }
    }

    /// Whether any archive was loaded (false ⇒ no JDK / nothing to resolve).
    pub fn is_empty(&self) -> bool {
        self.archives.is_empty()
    }

    /// Members + supertypes of a fully-qualified type, or `None` if not on the
    /// classpath. Binary names (nested types use `$`, e.g. `java.util.Map$Entry`)
    /// are expected. Cached, including negative results.
    pub fn class(&self, fqn: &str) -> Option<Arc<ClassInfo>> {
        if let Some(hit) = self.cache.read().expect("classpath cache").get(fqn) {
            return hit.clone();
        }
        let result = self.load(fqn);
        self.cache
            .write()
            .expect("classpath cache")
            .insert(fqn.to_string(), result.clone());
        result
    }

    fn load(&self, fqn: &str) -> Option<Arc<ClassInfo>> {
        let path = fqn.replace('.', "/");
        for archive in &self.archives {
            let entry = format!("{}{}.class", archive.prefix, path);
            if archive.zip.contains(&entry) {
                if let Some(bytes) = archive.zip.read(&entry) {
                    if let Some(info) = class_info::parse(&bytes) {
                        return Some(Arc::new(info));
                    }
                }
            }
        }
        None
    }

    /// The lazily-built name index (see [`index`]). First call walks every
    /// archive's central-directory names (strings already in memory — no
    /// bytecode parsing, no extra IO); subsequent calls are free.
    fn name_index(&self) -> &TypeIndex {
        self.name_index.get_or_init(|| {
            TypeIndex::build(self.archives.iter().flat_map(|archive| {
                // A jmod's entries live under `classes/` — also the marker
                // that this archive is JDK-sourced (dependency jars have no
                // prefix), which scopes the internal-namespace filter.
                let from_jdk = !archive.prefix.is_empty();
                archive.zip.names().filter_map(move |name| {
                    name.strip_prefix(archive.prefix)
                        .and_then(|n| n.strip_suffix(".class"))
                        .map(|n| (n, from_jdk))
                })
            }))
        })
    }

    /// Classpath types whose simple name starts with `prefix`
    /// (case-insensitive), best-first, capped at `limit`; the bool reports
    /// whether the cap cut candidates off (LSP `isIncomplete`).
    pub fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<TypeEntry>, bool) {
        self.name_index().types_with_prefix(prefix, limit)
    }

    /// Immediate children of a dotted package (`""` = roots):
    /// `(subpackage segments, types)`, both sorted — the shape import-path
    /// completion walks.
    pub fn package_children(&self, package: &str) -> (Vec<String>, Vec<TypeEntry>) {
        self.name_index().package_children(package)
    }

    /// The `.java` source for a fully-qualified type from a source archive, if
    /// available (for Javadoc). Cached, including misses.
    pub fn source(&self, fqn: &str) -> Option<Arc<String>> {
        if let Some(hit) = self.source_cache.read().expect("source cache").get(fqn) {
            return hit.clone();
        }
        let result = self.load_source(fqn).map(Arc::new);
        self.source_cache
            .write()
            .expect("source cache")
            .insert(fqn.to_string(), result.clone());
        result
    }

    fn load_source(&self, fqn: &str) -> Option<String> {
        // Nested types live in their outer class's source file.
        let path = fqn.replace('.', "/");
        let path = path.split('$').next().unwrap_or(&path);
        let suffix = format!("/{path}.java");
        let direct = format!("{path}.java");
        for source in &self.sources {
            let name = if source.suffix_match {
                match source.zip.find_suffix(&suffix) {
                    Some(name) => name,
                    None => continue,
                }
            } else {
                direct.clone()
            };
            if source.zip.contains(&name) {
                if let Some(bytes) = source.zip.read(&name) {
                    if let Ok(text) = String::from_utf8(bytes) {
                        return Some(text);
                    }
                }
            }
        }
        None
    }
}

/// The `<name>-sources.jar` for a dependency jar, if present: same directory
/// (Maven layout), else a sibling directory under the version dir (Gradle keeps
/// the sources jar in its own hash directory).
fn sources_jar_path(jar: &Path) -> Option<PathBuf> {
    let stem = jar.file_stem()?.to_str()?;
    let name = format!("{stem}-sources.jar");

    let same_dir = jar.with_file_name(&name);
    if same_dir.is_file() {
        return Some(same_dir);
    }

    let version_dir = jar.parent()?.parent()?;
    std::fs::read_dir(version_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&name))
        .find(|candidate| candidate.is_file())
}

/// The user's home directory, for locating `~/.m2` and `~/.gradle`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The Java release the project declares it targets, as a feature number
/// (`21`, `17`, `8` for `1.8`). Read statically from the build files — a
/// Maven `pom.xml` (`maven.compiler.release` / `maven.compiler.source` /
/// `java.version`, effective across the parent chain) or, best-effort, a
/// Gradle `build.gradle(.kts)` (toolchain `languageVersion`,
/// `JavaVersion.VERSION_*`, or a numeric `sourceCompatibility`). `None` when
/// undeclared or unreadable — callers then fall back to the JDK's own level.
/// The build is never executed.
pub fn project_java_release(root: &Path) -> Option<u32> {
    if root.join("pom.xml").is_file() {
        let m2 = home_dir()?.join(".m2/repository");
        return maven::compiler_release(root, &m2);
    }
    if root.join("build.gradle").is_file() || root.join("build.gradle.kts").is_file() {
        return gradle::compiler_release(root);
    }
    None
}

/// Bound on the directory-walk depth for [`module_output_dirs`] — a build
/// tree deeper than this is pathological for module discovery.
const OUTPUT_DIR_WALK_MAX_DEPTH: usize = 8;

/// Bound on directories visited by [`module_output_dirs`] — keeps a
/// pathological tree from turning candidate discovery into unbounded work
/// (the same defensive posture as the rest of this crate).
const OUTPUT_DIR_WALK_MAX_DIRS: usize = 4096;

/// Conventional build-output directory *candidates* for every Maven/Gradle
/// module under `root`: `<module>/target/classes` for a `pom.xml` module,
/// `<module>/build/classes/java/main` + `<module>/build/resources/main` for a
/// `build.gradle(.kts)` module. Candidates are returned **without existence
/// filtering** — the caller decides what "none exist" means (the debugger
/// treats it as "project not built yet"). A bounded directory walk: hidden
/// dirs, `target`/`build`/`node_modules`, and symlinks are skipped; depth is
/// capped at [`OUTPUT_DIR_WALK_MAX_DEPTH`] and visited directories at
/// [`OUTPUT_DIR_WALK_MAX_DIRS`]. No build execution, no file reads beyond
/// directory listing.
pub fn module_output_dirs(root: &Path) -> Vec<PathBuf> {
    const SKIPPED: [&str; 3] = ["target", "build", "node_modules"];
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if visited >= OUTPUT_DIR_WALK_MAX_DIRS {
            break;
        }
        visited += 1;
        if dir.join("pom.xml").is_file() {
            out.push(dir.join("target/classes"));
        }
        if dir.join("build.gradle").is_file() || dir.join("build.gradle.kts").is_file() {
            out.push(dir.join("build/classes/java/main"));
            out.push(dir.join("build/resources/main"));
        }
        if depth >= OUTPUT_DIR_WALK_MAX_DEPTH {
            continue;
        }
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || SKIPPED.contains(&name_str.as_ref()) {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    out
}

/// The Gradle user home — `$GRADLE_USER_HOME` when set to a non-empty path
/// (Gradle's own override, standard in CI and governed environments where the
/// cache lives outside `$HOME`), else `~/.gradle`. `None` only when neither is
/// available.
fn gradle_user_home() -> Option<PathBuf> {
    match std::env::var_os("GRADLE_USER_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home_dir().map(|h| h.join(".gradle")),
    }
}

/// A [`Classpath::degraded`] entry parsed into a structured coordinate — see
/// [`parse_degraded_entry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedCoordinate {
    pub group: String,
    pub artifact: String,
    /// `None` only when the version itself couldn't be resolved (always
    /// paired with `reason` in that case — see [`parse_degraded_entry`]).
    pub version: Option<String>,
    /// `None` means this is a plain "missing from the local cache" record —
    /// exactly the fetchable case `jvl/missingDependencies` wants: a real
    /// `g:a:v` that resolution simply couldn't find a pom/jar for. `Some`
    /// means resolution deliberately declined to pursue it further (an
    /// unresolvable/dynamic version, an unsupported classifier, a
    /// depth/node bound) — surfaced so the UI can explain the gap, but never
    /// auto-downloaded.
    pub reason: Option<String>,
}

impl DegradedCoordinate {
    /// A real `g:a:v` this crate simply doesn't have locally — the shape
    /// `jvl/missingDependencies` treats as downloadable from Maven Central.
    pub fn is_fetchable(&self) -> bool {
        self.reason.is_none() && self.version.is_some()
    }
}

/// A record's coordinate segment is safe to surface if it's non-empty and
/// contains no whitespace (real Maven coordinate segments never do; this
/// also happens to reject the free-text messages — "resolution truncated:
/// node limit (2000) exceeded", "x: parent g:ghost:7.0 unreadable" — that
/// aren't about a single coordinate at all).
fn is_plain_segment(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_whitespace)
}

/// Parse one [`Classpath::degraded`] string into a [`DegradedCoordinate`],
/// or `None` if it doesn't name a single coordinate at all (a project-
/// structure problem, or a resolver-bound message — see `resolve.rs` and
/// `gradle.rs` for every format this must handle). Every format currently
/// produced:
///
/// - `"g:a:v"` — missing pom/jar in the cache: fetchable, no reason.
/// - `"g:a:v (classifier X unsupported)"` / `"g:a:v (max depth exceeded)"` /
///   `"g:a:v (dynamic version unsupported)"` — a real coordinate resolution
///   deliberately didn't pursue: not fetchable, reason explains why.
/// - `"g:a (unresolved version)"` — no version could be determined at all:
///   not fetchable (nothing to download), reason explains why.
/// - anything else (`"module modA (pom unreadable)"`, `"x: parent
///   g:ghost:7.0 unreadable"`, `"resolution truncated: ..."`) — not about a
///   single coordinate: `None`.
pub fn parse_degraded_entry(entry: &str) -> Option<DegradedCoordinate> {
    let (base, reason) = match entry.strip_suffix(')') {
        Some(without_close) => {
            let open = without_close.rfind(" (")?;
            (
                &without_close[..open],
                Some(without_close[open + 2..].to_string()),
            )
        }
        None => (entry, None),
    };

    match base.split(':').collect::<Vec<_>>().as_slice() {
        [g, a, v] if is_plain_segment(g) && is_plain_segment(a) && is_plain_segment(v) => {
            Some(DegradedCoordinate {
                group: (*g).to_string(),
                artifact: (*a).to_string(),
                version: Some((*v).to_string()),
                reason,
            })
        }
        // A bare (no-reason) 2-segment base never occurs in practice, but
        // requiring `reason.is_some()` here keeps that case from being
        // misread as some kind of coordinate rather than free text.
        [g, a] if reason.is_some() && is_plain_segment(g) && is_plain_segment(a) => {
            Some(DegradedCoordinate {
                group: (*g).to_string(),
                artifact: (*a).to_string(),
                version: None,
                reason,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real JDK, or `None` so tests skip gracefully where none is installed.
    fn jdk() -> Option<Classpath> {
        let cp = Classpath::from_jdk();
        (!cp.is_empty()).then_some(cp)
    }

    fn member_names(info: &ClassInfo) -> Vec<&str> {
        info.members.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn list_has_core_methods() {
        let Some(cp) = jdk() else { return };
        let list = cp.class("java.util.List").expect("java.util.List");
        let names = member_names(&list);
        for want in ["add", "get", "size", "isEmpty", "contains"] {
            assert!(names.contains(&want), "List.{want} missing: {names:?}");
        }
    }

    #[test]
    fn signatures_are_raw_and_readable() {
        let Some(cp) = jdk() else { return };
        let list = cp.class("java.util.List").unwrap();
        let sig = |n: &str| {
            list.members
                .iter()
                .find(|m| m.name == n)
                .map(|m| m.signature.clone())
        };
        assert_eq!(sig("size").as_deref(), Some("int size()"));
        assert_eq!(sig("get").as_deref(), Some("Object get(int)")); // E erased
        assert!(
            list.members
                .iter()
                .any(|m| m.name == "add" && m.signature == "boolean add(Object)"),
            "add overloads: {:?}",
            list.members
                .iter()
                .filter(|m| m.name == "add")
                .map(|m| &m.signature)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn string_super_is_object_with_members() {
        let Some(cp) = jdk() else { return };
        let s = cp.class("java.lang.String").unwrap();
        let names = member_names(&s);
        assert!(names.contains(&"length"));
        assert!(names.contains(&"substring"));
        assert!(
            s.supers.iter().any(|x| x == "java.lang.Object"),
            "supers: {:?}",
            s.supers
        );
        let object = cp.class("java.lang.Object").unwrap();
        assert!(member_names(&object).contains(&"toString"));
    }

    #[test]
    fn arraylist_super_chain_includes_list() {
        let Some(cp) = jdk() else { return };
        let al = cp.class("java.util.ArrayList").unwrap();
        assert!(
            al.supers.iter().any(|x| x == "java.util.List"),
            "supers: {:?}",
            al.supers
        );
    }

    /// Real-JDK proof that chains have what they need — `stream()`
    /// (declared on `java.util.Collection`; `List` reaches it through the
    /// supers walk) carries its erased return FQN (+ generic display),
    /// `String.trim()` its FQN alone (no `Signature` attribute on a
    /// non-generic method), and `System.out` its field type FQN.
    #[test]
    fn member_result_types_from_real_jdk() {
        let Some(cp) = jdk() else { return };
        let list = cp.class("java.util.Collection").unwrap();
        let stream = list.members.iter().find(|m| m.name == "stream").unwrap();
        assert_eq!(stream.ret_fqn.as_deref(), Some("java.util.stream.Stream"));
        assert_eq!(stream.ret_display.as_deref(), Some("Stream<{0}>"));

        let string = cp.class("java.lang.String").unwrap();
        let trim = string.members.iter().find(|m| m.name == "trim").unwrap();
        assert_eq!(trim.ret_fqn.as_deref(), Some("java.lang.String"));

        let system = cp.class("java.lang.System").unwrap();
        let out = system.members.iter().find(|m| m.name == "out").unwrap();
        assert!(out.is_static);
        assert_eq!(out.ret_fqn.as_deref(), Some("java.io.PrintStream"));
    }

    #[test]
    fn missing_class_is_none_and_cached() {
        let Some(cp) = jdk() else { return };
        assert!(cp.class("no.such.Type").is_none());
        assert!(cp.class("no.such.Type").is_none()); // negative cache hit
    }

    #[test]
    fn empty_classpath_resolves_nothing() {
        let cp = Classpath::empty();
        assert!(cp.is_empty());
        assert!(cp.class("java.util.List").is_none());
        assert!(cp.types_with_prefix("Array", 10).0.is_empty());
        assert!(cp.package_children("").0.is_empty());
    }

    /// The name index over a real JDK — type-name prefix search finds
    /// `ArrayList`, package walking sees `java.util`'s children, and
    /// JDK-internal namespaces never surface.
    #[test]
    fn name_index_from_real_jdk() {
        let Some(cp) = jdk() else { return };
        let (hits, _) = cp.types_with_prefix("ArrayLi", 50);
        assert!(
            hits.iter().any(|t| t.fqn == "java.util.ArrayList"),
            "{hits:?}"
        );

        let (hits, _) = cp.types_with_prefix("Unsafe", 50);
        assert!(
            hits.iter()
                .all(|t| !t.fqn.starts_with("sun.") && !t.fqn.starts_with("jdk.internal.")),
            "internal namespaces leaked: {hits:?}"
        );

        let (subs, types) = cp.package_children("java.util");
        assert!(subs.iter().any(|s| s == "stream"), "{subs:?}");
        assert!(types.iter().any(|t| t.simple == "List"), "missing List");
        let (roots, _) = cp.package_children("");
        assert!(roots.iter().any(|s| s == "java"), "{roots:?}");

        // Nested types are offered by inner simple name with a canonical
        // (dot-separated) import path.
        let (hits, _) = cp.types_with_prefix("Entry", 200);
        let entry = hits
            .iter()
            .find(|t| t.fqn == "java.util.Map$Entry")
            .expect("Map$Entry offered");
        assert_eq!(entry.simple, "Entry");
        assert_eq!(entry.import_path, "java.util.Map.Entry");
    }

    /// Minimal STORED-method zip: enough structure for `ZipArchive::open`.
    /// Test-only — production reading stays hardened elsewhere.
    fn stored_zip(entries: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for name in entries {
            let offset = out.len() as u32;
            let n = name.as_bytes();
            // Local file header (empty content, method STORE).
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // ver/flags/method/time/date
            out.extend_from_slice(&[0; 12]); // crc, comp, uncomp
            out.extend_from_slice(&(n.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(n);
            // Central directory record.
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&[0; 12]); // crc, comp, uncomp
            central.extend_from_slice(&(n.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]); // extra/comment/disk/attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(n);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        // End of central directory.
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    /// Dependency jars get full name-index IntelliSense
    /// — including `com.sun.*` namespaces that the JDK-scoped filter would
    /// hide if they came from a jmod.
    #[test]
    fn dependency_jar_types_are_indexed() {
        let dir = std::env::temp_dir().join(format!("jvl-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar = dir.join("dep.jar");
        std::fs::write(
            &jar,
            stored_zip(&[
                "com/example/widgets/Widget.class",
                "com/sun/jersey/api/Client.class",
                "com/example/widgets/Widget$1.class",
            ]),
        )
        .unwrap();

        let mut cp = Classpath::empty();
        cp.add_jar(&jar);
        let (hits, _) = cp.types_with_prefix("Widg", 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].fqn, "com.example.widgets.Widget");
        let (hits, _) = cp.types_with_prefix("Client", 10);
        assert_eq!(
            hits.first().map(|t| t.fqn.as_str()),
            Some("com.sun.jersey.api.Client"),
            "dependency com.sun.* must stay visible"
        );
        let (subs, types) = cp.package_children("com.example");
        assert_eq!(subs, vec!["widgets".to_string()]);
        assert!(types.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Real JDK bytecode surfaces `<init>` methods as `Constructor`
    /// members named after the class, with both a no-arg and a
    /// parameterized overload present.
    #[test]
    fn arraylist_exposes_constructors() {
        let Some(cp) = jdk() else { return };
        let al = cp
            .class("java.util.ArrayList")
            .expect("java.util.ArrayList");
        let ctors: Vec<_> = al
            .members
            .iter()
            .filter(|m| matches!(m.kind, MemberKind::Constructor))
            .collect();
        assert!(!ctors.is_empty(), "ArrayList should expose constructors");
        assert!(ctors.iter().all(|m| m.name == "ArrayList"), "{:?}", ctors);
        assert!(
            ctors.iter().any(|m| m.signature == "ArrayList()"),
            "expected a no-arg constructor: {:?}",
            ctors
        );
        assert!(
            ctors.iter().any(|m| m.signature != "ArrayList()"),
            "expected at least one parameterized constructor: {:?}",
            ctors
        );
    }

    #[test]
    fn module_output_dirs_finds_maven_gradle_and_nested_modules() {
        let base = std::env::temp_dir().join(format!(
            "jvl-output-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Root Maven module with a nested Maven submodule, plus a sibling
        // Gradle module; a hidden dir and a `target` dir must not be walked.
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::create_dir_all(base.join("gradle-mod")).unwrap();
        std::fs::create_dir_all(base.join(".hidden/inner")).unwrap();
        std::fs::create_dir_all(base.join("target/nested")).unwrap();
        std::fs::write(base.join("pom.xml"), "<project/>").unwrap();
        std::fs::write(base.join("sub/pom.xml"), "<project/>").unwrap();
        std::fs::write(base.join("gradle-mod/build.gradle"), "").unwrap();
        std::fs::write(base.join(".hidden/inner/pom.xml"), "<project/>").unwrap();
        std::fs::write(base.join("target/nested/pom.xml"), "<project/>").unwrap();

        let dirs = module_output_dirs(&base);
        assert!(dirs.contains(&base.join("target/classes")), "{dirs:?}");
        assert!(dirs.contains(&base.join("sub/target/classes")), "{dirs:?}");
        assert!(
            dirs.contains(&base.join("gradle-mod/build/classes/java/main")),
            "{dirs:?}"
        );
        assert!(
            dirs.contains(&base.join("gradle-mod/build/resources/main")),
            "{dirs:?}"
        );
        // Candidates are returned without existence filtering (none exist).
        assert!(dirs.iter().all(|d| !d.exists()), "{dirs:?}");
        // Hidden and build-output dirs are never walked.
        assert_eq!(dirs.len(), 4, "{dirs:?}");

        std::fs::remove_dir_all(&base).ok();
    }
}

#[cfg(test)]
mod generic_tests {
    use super::*;
    #[test]
    fn arraylist_add_get_have_generic_templates() {
        let cp = Classpath::from_jdk();
        if cp.is_empty() {
            return;
        }
        let al = cp.class("java.util.ArrayList").unwrap();
        assert_eq!(al.type_params, vec!["E".to_string()]);
        let t = |n: &str| {
            al.members
                .iter()
                .find(|m| m.name == n)
                .and_then(|m| m.template.clone())
        };
        assert_eq!(
            t("add").as_deref(),
            Some("boolean add({0})"),
            "all add templates: {:?}",
            al.members
                .iter()
                .filter(|m| m.name == "add")
                .map(|m| (&m.signature, &m.template))
                .collect::<Vec<_>>()
        );
        assert_eq!(t("get").as_deref(), Some("{0} get(int)"));
    }

    #[test]
    fn map_put_template_has_two_slots() {
        let cp = Classpath::from_jdk();
        if cp.is_empty() {
            return;
        }
        let map = cp.class("java.util.Map").unwrap();
        assert_eq!(map.type_params, vec!["K".to_string(), "V".to_string()]);
        let put = map
            .members
            .iter()
            .find(|m| m.name == "put")
            .and_then(|m| m.template.clone());
        assert_eq!(put.as_deref(), Some("{1} put({0}, {1})"));
    }

    /// `ArrayList<E>`'s Signature attribute parameterizes its supertypes
    /// (`AbstractList<E>`, `List<E>`) with ArrayList's own type parameter, and
    /// leaves its non-generic ones (`RandomAccess`, `Cloneable`,
    /// `Serializable`) with no extra info — `super_type_args` is index-aligned
    /// with `supers` (superclass, then interfaces, in declaration order).
    #[test]
    fn arraylist_super_type_args_map_e_across_hierarchy() {
        let cp = Classpath::from_jdk();
        if cp.is_empty() {
            return;
        }
        let al = cp.class("java.util.ArrayList").unwrap();
        assert_eq!(al.supers.len(), al.super_type_args.len());
        let by_super = |fqn: &str| -> Option<&Vec<String>> {
            al.supers
                .iter()
                .position(|s| s == fqn)
                .map(|i| &al.super_type_args[i])
        };
        assert_eq!(
            by_super("java.util.AbstractList"),
            Some(&vec!["{0}".to_string()]),
            "supers: {:?}, super_type_args: {:?}",
            al.supers,
            al.super_type_args
        );
        assert_eq!(by_super("java.util.List"), Some(&vec!["{0}".to_string()]));
        // Non-generic interfaces: present, with no type arguments.
        for plain in ["java.util.RandomAccess", "java.lang.Cloneable"] {
            assert_eq!(
                by_super(plain),
                Some(&Vec::new()),
                "expected no super_type_args for {plain}"
            );
        }
    }

    /// End-to-end proof that `Member::template` + `ClassInfo::type_params`
    /// carry enough structured information for a caller to substitute a real
    /// use-site instantiation — mirroring (without depending on) the
    /// `{i}`-placeholder substitution `crates/syntax`'s hover/completion path
    /// performs today.
    #[test]
    fn template_substitution_end_to_end_list_of_string() {
        let cp = Classpath::from_jdk();
        if cp.is_empty() {
            return;
        }
        let list = cp.class("java.util.List").unwrap();
        assert_eq!(list.type_params, vec!["E".to_string()]);
        let get_template = list
            .members
            .iter()
            .find(|m| m.name == "get")
            .and_then(|m| m.template.clone())
            .expect("List.get has a generic template");

        // `List<String>` instantiation: substitute {0} -> "String".
        let args = ["String".to_string()];
        let rendered = substitute_placeholders(&get_template, &args);
        assert_eq!(rendered, "String get(int)");
    }

    /// Minimal `{i}` → argument substitution, standing in for the real
    /// substitution logic living in `crates/syntax::resolve` (out of scope for
    /// this crate) — exists only to prove the classpath crate's templates are
    /// sufficient to perform it.
    fn substitute_placeholders(template: &str, args: &[String]) -> String {
        let mut out = template.to_string();
        for (i, arg) in args.iter().enumerate() {
            out = out.replace(&format!("{{{i}}}"), arg);
        }
        out
    }

    // `parse_degraded_entry` must handle every shape `resolve.rs` and
    // `gradle.rs` actually produce (see their `degraded.push(...)` call
    // sites) — one case per distinct format string in this codebase today.

    #[test]
    fn parses_bare_missing_coordinate_as_fetchable() {
        let parsed = parse_degraded_entry("g:C:1.0").expect("should parse");
        assert_eq!(parsed.group, "g");
        assert_eq!(parsed.artifact, "C");
        assert_eq!(parsed.version.as_deref(), Some("1.0"));
        assert!(parsed.reason.is_none());
        assert!(parsed.is_fetchable());
    }

    #[test]
    fn parses_classifier_unsupported_as_not_fetchable_with_reason() {
        let parsed = parse_degraded_entry("g:B:1.0 (classifier natives-linux unsupported)")
            .expect("should parse");
        assert_eq!(parsed.group, "g");
        assert_eq!(parsed.artifact, "B");
        assert_eq!(parsed.version.as_deref(), Some("1.0"));
        assert_eq!(
            parsed.reason.as_deref(),
            Some("classifier natives-linux unsupported")
        );
        assert!(!parsed.is_fetchable());
    }

    #[test]
    fn parses_dynamic_version_as_not_fetchable_with_reason() {
        let parsed =
            parse_degraded_entry("g:wild:1.+ (dynamic version unsupported)").expect("should parse");
        assert_eq!(parsed.version.as_deref(), Some("1.+"));
        assert_eq!(
            parsed.reason.as_deref(),
            Some("dynamic version unsupported")
        );
        assert!(!parsed.is_fetchable());
    }

    #[test]
    fn parses_max_depth_exceeded_as_not_fetchable_with_reason() {
        let parsed = parse_degraded_entry("g:N29:1.0 (max depth exceeded)").expect("should parse");
        assert_eq!(parsed.version.as_deref(), Some("1.0"));
        assert_eq!(parsed.reason.as_deref(), Some("max depth exceeded"));
        assert!(!parsed.is_fetchable());
    }

    #[test]
    fn parses_unresolved_version_with_no_version_field() {
        let parsed = parse_degraded_entry("g:D (unresolved version)").expect("should parse");
        assert_eq!(parsed.group, "g");
        assert_eq!(parsed.artifact, "D");
        assert!(parsed.version.is_none());
        assert_eq!(parsed.reason.as_deref(), Some("unresolved version"));
        assert!(!parsed.is_fetchable());
    }

    #[test]
    fn non_coordinate_project_structure_messages_are_dropped() {
        assert!(parse_degraded_entry("resolution truncated: node limit (2000) exceeded").is_none());
        assert!(parse_degraded_entry("x: parent g:ghost:7.0 unreadable").is_none());
        assert!(parse_degraded_entry("module modA (pom unreadable)").is_none());
    }

    #[test]
    fn missing_dependencies_filters_degraded_list_through_the_parser() {
        let mut cp = Classpath::empty();
        cp.degraded = vec![
            "g:C:1.0".to_string(),
            "g:B:1.0 (classifier natives-linux unsupported)".to_string(),
            "module modA (pom unreadable)".to_string(),
        ];
        let missing = cp.missing_dependencies();
        assert_eq!(missing.len(), 2, "{missing:?}");
        assert!(missing
            .iter()
            .any(|c| c.is_fetchable() && c.artifact == "C"));
        assert!(missing
            .iter()
            .any(|c| !c.is_fetchable() && c.artifact == "B"));
    }
}
