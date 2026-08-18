//! M7: workspace `.java` files the user hasn't opened, as a [`SymbolSource`]
//! layer — the fix for "I imported `Person`, it's not open, and I get no
//! completion for it." Resolves an FQN to a file via [`WorkspaceIndex`],
//! reads it through `Backend::parsed_project_file`'s existing mtime cache
//! (no new IO path), and hands the text to `jvl_syntax::class_from_source`.
//!
//! [`CombinedSymbols`] composes this ahead of the classpath: a project type
//! wins over a same-named dependency type, matching how the open-document
//! `TypeTable` already wins over both (see the call sites in `main.rs`).

use jvl_syntax::{ExternalClass, SymbolSource, TypeCandidate};

use crate::workspace_index::WorkspaceIndex;
use crate::Backend;

/// Split a binary FQN (`demo.Outer$Inner`) into the pieces a
/// [`WorkspaceIndex`] lookup and [`jvl_syntax::class_from_source`] each want:
/// the declaring file's package + outer simple name (index lookup key), and
/// the dotted type-path `class_from_source` walks (`Outer.Inner`).
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

pub(crate) struct ProjectSymbols<'a>(pub(crate) &'a Backend);

impl ProjectSymbols<'_> {
    fn index(&self) -> &WorkspaceIndex {
        self.0.workspace_index()
    }

    /// Whether `fqn` resolves to something real, on the classpath or in the
    /// workspace — the existence check `class_from_source`'s `pick_fqn`
    /// uses to choose among a name's import candidates without reading (or
    /// even locating) the candidate's own file.
    fn exists(&self, fqn: &str) -> bool {
        if self.0.classpath().class(fqn).is_some() {
            return true;
        }
        match split_project_fqn(fqn) {
            Some((package, outer_simple, _)) => {
                self.index().find_type(&package, &outer_simple).is_some()
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
        let (package, outer_simple, type_path) = split_project_fqn(fqn)?;
        let path = self.index().find_type(&package, &outer_simple)?;
        let (text, _tree) = self.0.parsed_project_file(&path)?;
        let pick = |candidates: &[String]| self.pick_fqn(candidates);
        jvl_syntax::class_from_source(&text, &type_path, &pick)
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

    /// Type or member Javadoc, read straight from the declaring file's own
    /// source — a project file has no separate archive to consult, the way
    /// a dependency's `-sources.jar` does; the file *is* the source.
    /// Single-file only (no supertype walk on a member miss, unlike
    /// [`crate::ClasspathSymbols::doc`]) — an accepted, documented gap
    /// rather than a silent one.
    fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
        let (package, outer_simple, type_path) = split_project_fqn(fqn)?;
        let path = self.index().find_type(&package, &outer_simple)?;
        let (text, _tree) = self.0.parsed_project_file(&path)?;
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
/// type wins over a same-named dependency type, and every lookup falls
/// through to the classpath when the project doesn't have it.
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

    fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
        self.0.doc(fqn, member).or_else(|| self.1.doc(fqn, member))
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
