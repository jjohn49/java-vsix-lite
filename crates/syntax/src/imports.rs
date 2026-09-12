//! Resolve a simple type name to candidate fully-qualified names using a file's
//! `package` and `import` declarations, so external (JDK/dependency) types can be
//! looked up by FQN.

use std::collections::HashMap;

use tree_sitter::Tree;

use crate::model::named_children;
use crate::node_text;

/// The import context of one source file.
pub(crate) struct Imports {
    package: Option<String>,
    /// simple name → FQN, from explicit single-type imports.
    single: HashMap<String, String>,
    /// package prefixes from wildcard (`a.b.*`) imports.
    wildcards: Vec<String>,
}

impl Imports {
    pub(crate) fn parse(tree: &Tree, source: &str) -> Imports {
        let mut package = None;
        let mut single = HashMap::new();
        let mut wildcards = Vec::new();

        for child in named_children(tree.root_node()) {
            match child.kind() {
                "package_declaration" => {
                    package = dotted_path(node_text(child, source), "package");
                }
                "import_declaration" => {
                    let text = node_text(child, source);
                    // Static-member imports bind a member, not a type name.
                    if text.trim_start().starts_with("import static") {
                        continue;
                    }
                    if let Some(path) = dotted_path(text, "import") {
                        if let Some(pkg) = path.strip_suffix(".*") {
                            wildcards.push(pkg.to_string());
                        } else if let Some(simple) = path.rsplit('.').next() {
                            single.insert(simple.to_string(), path.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        Imports {
            package,
            single,
            wildcards,
        }
    }

    /// The single-type import path bound to a simple name, if any.
    pub(crate) fn single_import(&self, simple: &str) -> Option<&str> {
        self.single.get(simple).map(String::as_str)
    }

    /// Whether `import <pkg>.*;` is in force.
    pub(crate) fn has_wildcard(&self, pkg: &str) -> bool {
        self.wildcards.iter().any(|w| w == pkg)
    }

    /// Package prefixes from wildcard (`a.b.*`) imports.
    pub(crate) fn wildcard_packages(&self) -> impl Iterator<Item = &str> {
        self.wildcards.iter().map(String::as_str)
    }

    /// The file's own package, if declared.
    pub(crate) fn package(&self) -> Option<&str> {
        self.package.as_deref()
    }

    /// Candidate FQNs for a simple type name, in resolution-priority order:
    /// explicit import, same package, each wildcard package, then `java.lang`.
    ///
    /// A file with no `package` declaration is in the *unnamed* package
    /// (JLS 7.4.2), whose members' binary names are their simple names — so
    /// the bare name is that file's "same package" candidate. Without it,
    /// sibling types in the default package resolve to nothing and every
    /// consumer of this list silently degrades to Unknown.
    pub(crate) fn candidates(&self, simple: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(fqn) = self.single.get(simple) {
            out.push(fqn.clone());
        }
        match &self.package {
            Some(pkg) => out.push(format!("{pkg}.{simple}")),
            None => out.push(simple.to_string()),
        }
        for wildcard in &self.wildcards {
            out.push(format!("{wildcard}.{simple}"));
        }
        out.push(format!("java.lang.{simple}"));
        out
    }
}

/// Extract the dotted path from a `package x.y;` / `import x.y.Z;` declaration.
///
/// Also used by `references.rs` to recover a declaration's actual package
/// independent of any particular `Imports` instance.
pub(crate) fn dotted_path(text: &str, keyword: &str) -> Option<String> {
    let rest = text.trim().strip_prefix(keyword)?;
    let path: String = rest
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect();
    (!path.is_empty()).then_some(path)
}
