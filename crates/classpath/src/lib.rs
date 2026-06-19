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
use std::sync::{Arc, RwLock};

mod class_info;
mod generics;
mod gradle;
mod jdk;
mod maven;
mod zip;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Method,
    Field,
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

    /// The JDK classpath plus a project's **direct, declared** dependency jars,
    /// discovered statically (Maven `pom.xml`; Gradle build files +
    /// `libs.versions.toml`) from `root`. No build tool is executed.
    pub fn from_jdk_and_project(root: Option<&Path>) -> Classpath {
        let mut cp = Classpath::from_jdk();
        if let (Some(root), Some(home)) = (root, home_dir()) {
            for jar in maven::dependency_jars(root, &home.join(".m2/repository")) {
                cp.add_jar(&jar);
            }
            for jar in gradle::dependency_jars(root, &home.join(".gradle/caches")) {
                cp.add_jar(&jar);
            }
        }
        cp
    }

    /// Add a dependency jar to the classpath (no-op if it can't be opened). Its
    /// sibling `-sources.jar`, if present, is registered for Javadoc.
    pub fn add_jar(&mut self, path: &Path) {
        if let Some(zip) = ZipArchive::open(path, 0) {
            self.archives.push(Archive { zip, prefix: "" });
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
    }
}

#[cfg(test)]
mod generic_tests {
    use super::*;
    #[test]
    fn arraylist_add_get_have_generic_templates() {
        let cp = Classpath::from_jdk();
        if cp.is_empty() { return; }
        let al = cp.class("java.util.ArrayList").unwrap();
        assert_eq!(al.type_params, vec!["E".to_string()]);
        let t = |n: &str| al.members.iter().find(|m| m.name==n).and_then(|m| m.template.clone());
        assert_eq!(t("add").as_deref(), Some("boolean add({0})"), "all add templates: {:?}", al.members.iter().filter(|m|m.name=="add").map(|m|(&m.signature,&m.template)).collect::<Vec<_>>());
        assert_eq!(t("get").as_deref(), Some("{0} get(int)"));
    }
}
