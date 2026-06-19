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
use std::path::Path;
use std::sync::{Arc, RwLock};

mod class_info;
mod jdk;
mod zip;

use zip::ZipArchive;

/// A type read from bytecode: its fully-qualified name, its direct supertypes
/// (superclass + interfaces, as FQNs), and its visible members.
#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub fqn: String,
    pub supers: Vec<String>,
    pub members: Vec<Member>,
}

/// One member of a class: a method or field, with a rendered (raw) signature.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
    /// Readable signature, e.g. `boolean add(Object)` or `int size`.
    pub signature: String,
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

/// An ordered set of archives (JDK jmods + dependency jars) answering
/// "what are the members of fully-qualified type `X`?", with a result cache.
pub struct Classpath {
    archives: Vec<Archive>,
    cache: RwLock<HashMap<String, Option<Arc<ClassInfo>>>>,
}

impl Classpath {
    /// An empty classpath (resolves nothing) — used as a safe fallback when no
    /// JDK is found and in tests.
    pub fn empty() -> Classpath {
        Classpath {
            archives: Vec::new(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Build a classpath from the best available JDK's jmods. Empty if none.
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
        }
        cp
    }

    /// Add a dependency jar to the classpath (no-op if it can't be opened).
    pub fn add_jar(&mut self, path: &Path) {
        if let Some(zip) = ZipArchive::open(path, 0) {
            self.archives.push(Archive { zip, prefix: "" });
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
