//! A name index over every archive's central directory, so completion can
//! offer classpath *type names* (with auto-import) and walk *package paths* —
//! built lazily from entry-name strings already resident in memory. No
//! bytecode is parsed here; a name's class is only ever loaded when the user
//! actually resolves it.

use std::collections::{BTreeMap, BTreeSet, HashSet};

/// One offerable classpath type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeEntry {
    /// Display / completion label: the innermost simple name (`Entry`).
    pub simple: String,
    /// Binary dotted name, the `Classpath::class` lookup key
    /// (`java.util.Map$Entry`).
    pub fqn: String,
    /// Canonical import path (`java.util.Map.Entry`).
    pub import_path: String,
}

/// JDK-internal namespaces that are never *offered* (still resolvable by
/// exact FQN through `Classpath::class`). Matching is whole-segment
/// (`sun` or `sun.…`, never `sunshine.…`). Applies **only to JDK (jmod)
/// archives**: a dependency jar legitimately shipping e.g. `com.sun.jersey`
/// must keep full IntelliSense.
const HIDDEN_JDK_NAMESPACES: &[&str] = &[
    "sun",
    "com.sun",
    "jdk.internal",
    "oracle",
    "netscape",
    "apple",
    "com.apple",
];

pub(crate) struct TypeIndex {
    /// Sorted by (lowercased simple, fqn) so a case-insensitive prefix scan is
    /// one contiguous range.
    entries: Vec<TypeEntry>,
    /// Lowercased simple names, index-aligned with `entries` (the sort key).
    lower: Vec<String>,
    /// Package (dotted, `""` = default) → indices into `entries`.
    by_package: BTreeMap<String, Vec<usize>>,
    /// Every package that (transitively) contains an offerable type,
    /// including intermediate ancestors (`java` for `java.lang`).
    packages: BTreeSet<String>,
}

impl TypeIndex {
    /// Build from `(internal class path, from_jdk)` pairs (`java/util/Map$Entry`
    /// — no `.class` suffix, no jmod `classes/` prefix), first occurrence of an
    /// FQN wins. `from_jdk` marks jmod-sourced names, the only ones subject to
    /// the internal-namespace filter.
    pub(crate) fn build<'a>(names: impl Iterator<Item = (&'a str, bool)>) -> TypeIndex {
        let mut seen: HashSet<String> = HashSet::new();
        let mut entries: Vec<TypeEntry> = Vec::new();
        let mut packages: BTreeSet<String> = BTreeSet::new();

        for (internal, from_jdk) in names {
            let Some((package, last)) = split_internal(internal) else {
                continue;
            };
            if !offerable(&package, last, from_jdk) {
                continue;
            }
            let fqn = if package.is_empty() {
                last.to_string()
            } else {
                format!("{package}.{last}")
            };
            if !seen.insert(fqn.clone()) {
                continue;
            }
            let simple = last.rsplit('$').next().unwrap_or(last).to_string();
            let import_path = fqn.replace('$', ".");
            entries.push(TypeEntry {
                simple,
                fqn,
                import_path,
            });
            // Register the package and every ancestor, so `java` lists `lang`
            // as a child even though `java` itself holds no classes.
            let mut pkg: &str = &package;
            while !pkg.is_empty() {
                if !packages.insert(pkg.to_string()) {
                    break; // ancestors already present
                }
                pkg = match pkg.rfind('.') {
                    Some(dot) => &pkg[..dot],
                    None => "",
                };
            }
        }

        entries.sort_by(|a, b| {
            (a.simple.to_ascii_lowercase(), &a.fqn).cmp(&(b.simple.to_ascii_lowercase(), &b.fqn))
        });
        let lower: Vec<String> = entries
            .iter()
            .map(|e| e.simple.to_ascii_lowercase())
            .collect();
        let mut by_package: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, e) in entries.iter().enumerate() {
            let pkg = match e.fqn.rfind('.') {
                Some(dot) => e.fqn[..dot].to_string(),
                None => String::new(),
            };
            by_package.entry(pkg).or_default().push(i);
        }

        TypeIndex {
            entries,
            lower,
            by_package,
            packages,
        }
    }

    /// Types whose simple name starts with `prefix`, case-insensitively —
    /// best (shortest simple name) first, capped at `limit`, plus whether the
    /// cap cut anything off. An empty prefix matches nothing (callers gate on
    /// a minimum typed length anyway; this keeps the API safe by default).
    pub(crate) fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<TypeEntry>, bool) {
        if prefix.is_empty() || limit == 0 {
            return (Vec::new(), false);
        }
        let needle = prefix.to_ascii_lowercase();
        let start = self.lower.partition_point(|s| s.as_str() < needle.as_str());
        let mut hits: Vec<&TypeEntry> = self.lower[start..]
            .iter()
            .take_while(|s| s.starts_with(&needle))
            .enumerate()
            .map(|(i, _)| &self.entries[start + i])
            .collect();
        let truncated = hits.len() > limit;
        hits.sort_by(|a, b| {
            (a.simple.len(), &a.simple, &a.fqn).cmp(&(b.simple.len(), &b.simple, &b.fqn))
        });
        hits.truncate(limit);
        (hits.into_iter().cloned().collect(), truncated)
    }

    /// Immediate children of a package: `(subpackage segments, types)`, both
    /// sorted. `""` lists the roots. Unknown packages yield empty results.
    pub(crate) fn package_children(&self, package: &str) -> (Vec<String>, Vec<TypeEntry>) {
        let mut subpackages: Vec<String> = Vec::new();
        let prefix = if package.is_empty() {
            String::new()
        } else {
            format!("{package}.")
        };
        for pkg in self.packages.range(prefix.clone()..) {
            if !pkg.starts_with(&prefix) {
                break;
            }
            let segment = &pkg[prefix.len()..];
            let segment = segment.split('.').next().unwrap_or(segment);
            // `packages` is sorted, so duplicates of a segment are adjacent.
            if subpackages.last().map(String::as_str) != Some(segment) {
                subpackages.push(segment.to_string());
            }
        }
        let mut types: Vec<TypeEntry> = self
            .by_package
            .get(package)
            .map(|idxs| idxs.iter().map(|&i| self.entries[i].clone()).collect())
            .unwrap_or_default();
        types.sort_by(|a, b| (&a.simple, &a.fqn).cmp(&(&b.simple, &b.fqn)));
        (subpackages, types)
    }
}

/// Split an internal class path into `(dotted package, last segment)`.
/// `java/util/Map$Entry` → `("java.util", "Map$Entry")`.
fn split_internal(internal: &str) -> Option<(String, &str)> {
    if internal.is_empty() {
        return None;
    }
    match internal.rsplit_once('/') {
        Some((pkg, last)) => Some((pkg.replace('/', "."), last)),
        None => Some((String::new(), internal)),
    }
}

/// Whether a class-file name is worth offering: not a module/package
/// descriptor, not an anonymous/local/synthetic class, and (for jmod-sourced
/// names only) not JDK-internal.
fn offerable(package: &str, last: &str, from_jdk: bool) -> bool {
    if last == "module-info" || last == "package-info" {
        return false;
    }
    if last.contains("$$") {
        return false; // synthetic (lambdas, proxies)
    }
    let simple = last.rsplit('$').next().unwrap_or(last);
    if simple.is_empty() || simple.starts_with(|c: char| c.is_ascii_digit()) {
        return false; // anonymous (`Foo$1`) / local (`Foo$1Local`)
    }
    !from_jdk
        || !HIDDEN_JDK_NAMESPACES
            .iter()
            .any(|ns| package == *ns || package.starts_with(&format!("{ns}.")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> TypeIndex {
        TypeIndex::build(
            [
                ("java/util/ArrayList", true),
                ("java/util/Map", true),
                ("java/util/Map$Entry", true),
                ("java/util/Map$1", true),
                ("java/util/stream/Stream", true),
                ("java/lang/String", true),
                ("module-info", true),
                ("java/util/package-info", true),
                ("sun/misc/Unsafe", true),
                ("com/sun/Internal", true),
                ("jdk/internal/misc/Signal", true),
                // A *dependency* legitimately shipping a com.sun namespace
                // (e.g. Jersey) — must stay visible.
                ("com/sun/jersey/api/Client", false),
                ("a/b/Foo$$Lambda$1", false),
                ("demo/Widget", false),
                ("java/util/ArrayList", false), // duplicate: first wins
            ]
            .into_iter(),
        )
    }

    #[test]
    fn prefix_match_is_case_insensitive_and_capped() {
        let idx = index();
        let (hits, truncated) = idx.types_with_prefix("arrayli", 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].fqn, "java.util.ArrayList");
        assert!(!truncated);

        let (hits, _) = idx.types_with_prefix("MA", 10);
        assert_eq!(hits.len(), 1, "Map$1 filtered: {hits:?}");
        assert_eq!(hits[0].fqn, "java.util.Map");

        let (hits, truncated) = idx.types_with_prefix("s", 1);
        assert_eq!(hits.len(), 1);
        assert!(truncated, "Stream + String exceed the cap of 1");

        let (hits, _) = idx.types_with_prefix("", 10);
        assert!(hits.is_empty(), "empty prefix offers nothing");
    }

    #[test]
    fn nested_types_offer_inner_simple_name_and_canonical_import_path() {
        let idx = index();
        let (hits, _) = idx.types_with_prefix("Entry", 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].simple, "Entry");
        assert_eq!(hits[0].fqn, "java.util.Map$Entry");
        assert_eq!(hits[0].import_path, "java.util.Map.Entry");
    }

    #[test]
    fn noise_and_jdk_internal_namespaces_are_never_offered() {
        let idx = index();
        for needle in ["Unsafe", "Internal", "Signal", "Foo", "module-info"] {
            let (hits, _) = idx.types_with_prefix(needle, 10);
            assert!(hits.is_empty(), "{needle} should be hidden: {hits:?}");
        }
    }

    /// The internal-namespace filter is JDK-scoped: a dependency jar's
    /// `com.sun.*` classes keep IntelliSense.
    #[test]
    fn dependency_jars_in_sun_namespaces_stay_visible() {
        let idx = index();
        let (hits, _) = idx.types_with_prefix("Client", 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].fqn, "com.sun.jersey.api.Client");
    }

    #[test]
    fn package_children_lists_subpackages_and_types() {
        let idx = index();
        let (subs, types) = idx.package_children("java.util");
        assert_eq!(subs, vec!["stream".to_string()]);
        let names: Vec<&str> = types.iter().map(|t| t.simple.as_str()).collect();
        assert_eq!(names, vec!["ArrayList", "Entry", "Map"]);

        let (roots, root_types) = idx.package_children("");
        // `a.b` held only a synthetic lambda, so `a` never registers; hidden
        // JDK namespaces never register; the dependency's `com.sun.jersey`
        // registers `com`.
        assert_eq!(
            roots,
            vec!["com".to_string(), "demo".to_string(), "java".to_string()]
        );
        assert!(root_types.is_empty());

        let (subs, types) = idx.package_children("java");
        assert_eq!(subs, vec!["lang".to_string(), "util".to_string()]);
        assert!(types.is_empty());

        let (subs, types) = idx.package_children("no.such.pkg");
        assert!(subs.is_empty() && types.is_empty());
    }
}
