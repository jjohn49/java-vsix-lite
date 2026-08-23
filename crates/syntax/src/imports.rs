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

    /// The file's own package, if declared.
    pub(crate) fn package(&self) -> Option<&str> {
        self.package.as_deref()
    }

    /// Candidate FQNs for a simple type name, in resolution-priority order:
    /// explicit import, same package, each wildcard package, then `java.lang`.
    pub(crate) fn candidates(&self, simple: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(fqn) = self.single.get(simple) {
            out.push(fqn.clone());
        }
        if let Some(pkg) = &self.package {
            out.push(format!("{pkg}.{simple}"));
        }
        for wildcard in &self.wildcards {
            out.push(format!("{wildcard}.{simple}"));
        }
        out.push(format!("java.lang.{simple}"));
        out
    }
}

/// Extract the dotted path from a `package x.y;` / `import x.y.Z;` declaration:
/// strip the keyword and the trailing `;`, and collapse any stray whitespace.
///
/// `pub(crate)`: also used by `references.rs` to recover an arbitrary
/// declaration's own *actual* package (from its declaring document's
/// `package_declaration`, walked to independent of any particular `Imports`
/// instance) for package-aware type-reference confirmation.
pub(crate) fn dotted_path(text: &str, keyword: &str) -> Option<String> {
    let rest = text.trim().strip_prefix(keyword)?;
    let path: String = rest
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect();
    (!path.is_empty()).then_some(path)
}
