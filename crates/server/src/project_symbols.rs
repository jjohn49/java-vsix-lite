//! Symbols from unopened workspace `.java` files, loaded through
//! [`WorkspaceIndex`] and the project-file cache. Project types shadow dependencies.

use std::cell::RefCell;
use std::collections::HashMap;

use jvl_syntax::tree_sitter::{Node, Tree};
use jvl_syntax::{ExternalClass, SymbolSource, TypeCandidate};

use crate::backend::{Backend, Document};
use crate::workspace_index::WorkspaceIndex;

/// Splits a binary FQN (`demo.Outer$Inner`) into the declaring file's
/// package + outer simple name, and the dotted type-path (`Outer.Inner`).
fn split_project_fqn(fqn: &str) -> Option<(String, String, String)> {
    let (binary_outer, nested) = match fqn.split_once('$') {
        Some((outer, rest)) => (outer, Some(rest)),
        None => (fqn, None),
    };
    let (package, outer_simple) = match binary_outer.rsplit_once('.') {
        Some((p, s)) => (p.to_string(), s.to_string()),
        None => (String::new(), binary_outer.to_string()),
    };
    if outer_simple.is_empty() {
        return None;
    }
    let type_path = match nested {
        Some(rest) => format!("{outer_simple}.{}", rest.replace('$', ".")),
        None => outer_simple.clone(),
    };
    Some((package, outer_simple, type_path))
}

/// The dotted path text of a `package_declaration` node. Strips the
/// `package` keyword and `;` textually: this grammar gives the node no
/// named field for its identifier.
fn package_declaration_path(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let raw = node.utf8_text(bytes).ok()?;
    let rest = raw.trim_start().strip_prefix("package")?;
    Some(rest.trim().trim_end_matches(';').trim().to_string())
}

/// Every top-level type's binary name (`pkg.Outer`) in a parsed document,
/// used as the open-buffer overlay's lookup key. Only walks the root's
/// direct children; nested types are reached later via
/// `split_project_fqn`'s dotted type-path.
fn top_level_binary_names(text: &str, tree: &Tree) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut package = String::new();
    let mut names = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "package_declaration" => {
                if let Some(p) = package_declaration_path(child, bytes) {
                    package = p;
                }
            }
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => {
                if let Some(name) = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(bytes).ok())
                {
                    names.push(fqn_of(&package, name));
                }
            }
            _ => {}
        }
    }
    names
}

/// Workspace `.java` files as a [`SymbolSource`] layer, snapshotted once
/// per request. An open buffer's `overlay` entry shadows its on-disk file;
/// `memo` caches each FQN so repeated lookups in one request parse once.
pub(crate) struct ProjectSymbols<'a> {
    backend: &'a Backend,
    overlay: HashMap<String, (&'a str, &'a Tree)>,
    memo: RefCell<HashMap<String, Option<ExternalClass>>>,
}

impl<'a> ProjectSymbols<'a> {
    pub(crate) fn new(backend: &'a Backend, docs: &'a HashMap<String, Document>) -> Self {
        let mut overlay = HashMap::new();
        for doc in docs.values() {
            for binary in top_level_binary_names(&doc.text, &doc.tree) {
                overlay.insert(binary, (doc.text.as_str(), &doc.tree));
            }
        }
        ProjectSymbols {
            backend,
            overlay,
            memo: RefCell::new(HashMap::new()),
        }
    }

    fn index(&self) -> &WorkspaceIndex {
        self.backend.workspace_index()
    }

    /// Whether `fqn` resolves to something real, without reading its file.
    /// Used by `pick_fqn` to choose among import candidates.
    fn exists(&self, fqn: &str) -> bool {
        if self.backend.classpath().class(fqn).is_some() {
            return true;
        }
        match split_project_fqn(fqn) {
            Some((package, outer_simple, _)) => {
                let outer_binary = fqn_of(&package, &outer_simple);
                self.overlay.contains_key(&outer_binary)
                    || self.index().find_type(&package, &outer_simple).is_some()
            }
            None => false,
        }
    }

    fn pick_fqn(&self, candidates: &[String]) -> Option<String> {
        candidates.iter().find(|c| self.exists(c)).cloned()
    }
}

impl SymbolSource for ProjectSymbols<'_> {
    fn class(&self, fqn: &str) -> Option<ExternalClass> {
        if let Some(hit) = self.memo.borrow().get(fqn) {
            return hit.clone();
        }
        let result = (|| {
            let (package, outer_simple, type_path) = split_project_fqn(fqn)?;
            let outer_binary = fqn_of(&package, &outer_simple);
            let pick = |candidates: &[String]| self.pick_fqn(candidates);
            if let Some(&(text, tree)) = self.overlay.get(&outer_binary) {
                let doc = jvl_syntax::OpenDoc { source: text, tree };
                return jvl_syntax::class_from_doc(&doc, &type_path, &pick);
            }
            let path = self.index().find_type(&package, &outer_simple)?;
            let (text, tree) = self.backend.parsed_project_file(&path)?;
            let doc = jvl_syntax::OpenDoc {
                source: text.as_str(),
                tree: &tree,
            };
            jvl_syntax::class_from_doc(&doc, &type_path, &pick)
        })();
        self.memo
            .borrow_mut()
            .insert(fqn.to_string(), result.clone());
        result
    }

    fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<TypeCandidate>, bool) {
        let (hits, truncated) = self.index().types_with_prefix(prefix, limit);
        (
            hits.into_iter()
                .map(|e| TypeCandidate {
                    simple: e.simple_name.clone(),
                    fqn: fqn_of(&e.package, &e.simple_name),
                    import_path: fqn_of(&e.package, &e.simple_name),
                })
                .collect(),
            truncated,
        )
    }

    fn package_children(&self, package: &str) -> (Vec<String>, Vec<TypeCandidate>) {
        let (subpackages, types) = self.index().package_children(package);
        (
            subpackages,
            types
                .into_iter()
                .map(|e| TypeCandidate {
                    simple: e.simple_name.clone(),
                    fqn: fqn_of(&e.package, &e.simple_name),
                    import_path: fqn_of(&e.package, &e.simple_name),
                })
                .collect(),
        )
    }

    /// Javadoc from one declaring source file, without walking supertypes.
    /// Open buffers are handled by the earlier overlay layer.
    fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
        let (package, outer_simple, type_path) = split_project_fqn(fqn)?;
        let path = self.index().find_type(&package, &outer_simple)?;
        let (text, _tree) = self.backend.parsed_project_file(&path)?;
        let inner_simple = type_path.rsplit('.').next().unwrap_or(&type_path);
        jvl_syntax::javadoc_in_source(&text, inner_simple, member)
    }
}

fn fqn_of(package: &str, simple: &str) -> String {
    if package.is_empty() {
        simple.to_string()
    } else {
        format!("{package}.{simple}")
    }
}

/// Composes a project-source layer ahead of a classpath layer: a workspace
/// type wins over a same-named dependency type; lookups otherwise fall
/// through to the classpath.
pub(crate) struct CombinedSymbols<P, C>(pub(crate) P, pub(crate) C);

impl<P: SymbolSource, C: SymbolSource> SymbolSource for CombinedSymbols<P, C> {
    fn class(&self, fqn: &str) -> Option<ExternalClass> {
        self.0.class(fqn).or_else(|| self.1.class(fqn))
    }

    fn super_type_args(&self, fqn: &str) -> Vec<Vec<String>> {
        if self.0.class(fqn).is_some() {
            self.0.super_type_args(fqn)
        } else {
            self.1.super_type_args(fqn)
        }
    }

    /// Member docs inherit across the project/classpath boundary: an
    /// overriding class shows the supertype's Javadoc when its own layer
    /// has none. Each layer's `doc` already walks supers within its own
    /// world; this carries the lookup between worlds, bounded and
    /// cycle-guarded.
    fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
        if let Some(direct) = self.0.doc(fqn, member).or_else(|| self.1.doc(fqn, member)) {
            return Some(direct);
        }
        let member = member?; // type-level docs never inherit
        let mut queue = self.class(fqn)?.supers;
        let mut visited = std::collections::HashSet::new();
        let mut budget = 64usize;
        while let Some(super_fqn) = queue.pop() {
            if budget == 0 || !visited.insert(super_fqn.clone()) {
                continue;
            }
            budget -= 1;
            if let Some(doc) = self
                .0
                .doc(&super_fqn, Some(member))
                .or_else(|| self.1.doc(&super_fqn, Some(member)))
            {
                return Some(doc);
            }
            if let Some(class) = self.class(&super_fqn) {
                queue.extend(class.supers);
            }
        }
        None
    }

    fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<TypeCandidate>, bool) {
        let (mut project, project_truncated) = self.0.types_with_prefix(prefix, limit);
        let seen: std::collections::HashSet<String> =
            project.iter().map(|c| c.fqn.clone()).collect();
        let remaining = limit.saturating_sub(project.len());
        let (classpath, classpath_truncated) = self.1.types_with_prefix(prefix, remaining);
        project.extend(classpath.into_iter().filter(|c| !seen.contains(&c.fqn)));
        (project, project_truncated || classpath_truncated)
    }

    fn package_children(&self, package: &str) -> (Vec<String>, Vec<TypeCandidate>) {
        let (mut subpackages, mut types) = self.0.package_children(package);
        let (more_subs, more_types) = self.1.package_children(package);
        for s in more_subs {
            if !subpackages.contains(&s) {
                subpackages.push(s);
            }
        }
        let seen: std::collections::HashSet<String> = types.iter().map(|c| c.fqn.clone()).collect();
        types.extend(more_types.into_iter().filter(|c| !seen.contains(&c.fqn)));
        (subpackages, types)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layer with fixed classes and member docs, standing in for either
    /// side of the combined source.
    struct Stub {
        classes: Vec<(&'static str, Vec<&'static str>)>, // (fqn, supers)
        docs: Vec<((&'static str, &'static str), &'static str)>,
    }

    impl SymbolSource for Stub {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            self.classes
                .iter()
                .find(|(f, _)| *f == fqn)
                .map(|(_, supers)| ExternalClass {
                    supers: supers.iter().map(|s| s.to_string()).collect(),
                    type_params: Vec::new(),
                    members: Vec::new(),
                    metadata: None,
                })
        }
        fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
            let member = member?;
            self.docs
                .iter()
                .find(|((f, m), _)| *f == fqn && *m == member)
                .map(|(_, d)| (*d).to_string())
        }
    }

    /// A project class overriding a classpath method inherits the
    /// classpath supertype's member doc — the walk crosses layer worlds.
    #[test]
    fn combined_doc_walks_supers_across_layers() {
        let project = Stub {
            classes: vec![("demo.Task", vec!["java.lang.Runnable"])],
            docs: vec![],
        };
        let classpath = Stub {
            classes: vec![("java.lang.Runnable", vec![])],
            docs: vec![(("java.lang.Runnable", "run"), "Runs the task.")],
        };
        let combined = CombinedSymbols(project, classpath);
        assert_eq!(
            combined.doc("demo.Task", Some("run")).as_deref(),
            Some("Runs the task.")
        );
        // Type-level docs never inherit.
        assert_eq!(combined.doc("demo.Task", None), None);
        // A miss everywhere stays a miss (bounded, no panic).
        assert_eq!(combined.doc("demo.Task", Some("nope")), None);
        assert_eq!(combined.doc("no.such.Type", Some("run")), None);
    }

    /// A supertype cycle must terminate (visited set + budget).
    #[test]
    fn combined_doc_survives_supertype_cycles() {
        let project = Stub {
            classes: vec![("a.A", vec!["b.B"]), ("b.B", vec!["a.A"])],
            docs: vec![],
        };
        let classpath = Stub {
            classes: vec![],
            docs: vec![],
        };
        let combined = CombinedSymbols(project, classpath);
        assert_eq!(combined.doc("a.A", Some("m")), None);
    }
}
