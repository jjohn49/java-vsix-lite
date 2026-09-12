//! The seam between pure analysis and bytecode-backed external symbols.
//!
//! `jvl-syntax` stays IO-free: it calls [`SymbolSource`] to learn the members and
//! supertypes of a fully-qualified type it cannot find in the open documents. The
//! server implements this over `jvl-classpath`; tests pass a mock.

/// A type resolved from outside the open documents (a JDK or dependency class).
#[derive(Debug, Clone)]
pub struct ExternalClass {
    /// Superclass + interface FQNs.
    pub supers: Vec<String>,
    /// Formal type-parameter names, e.g. `["E"]` for `ArrayList<E>`.
    pub type_params: Vec<String>,
    pub members: Vec<ExternalMember>,
    /// Structured class metadata; `None` when the source didn't supply it
    /// (e.g. test stubs, synthetic classes).
    pub metadata: Option<jvl_types::ClassMetadata>,
}

/// One member of an external type.
#[derive(Debug, Clone)]
pub struct ExternalMember {
    pub name: String,
    pub kind: ExternalMemberKind,
    /// Raw (generics-erased) signature — also the cross-declaration dedup key.
    pub signature: String,
    /// Generic signature using `{i}` for class parameters, or `None` when
    /// no class type variable appears.
    pub template: Option<String>,
    pub is_static: bool,
    /// Dotted FQN of the erased return/field type — what a `recv.member().`
    /// chain resolves through. `None` for primitives, `void`, arrays, constructors.
    pub ret_fqn: Option<String>,
    /// The return/field type for display, preferring the generic `{i}`
    /// template over the descriptor form. `None` only for constructors.
    pub ret_display: Option<String>,
    /// Structured member metadata (declaring class, access, parameter/result
    /// types, type parameters) — `None` when the source didn't supply it.
    pub metadata: Option<jvl_types::MemberMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalMemberKind {
    Method,
    Field,
    /// A constructor (`name` = the declaring class's simple name). Never
    /// returned by the ordinary member-hierarchy walk — only a dedicated
    /// constructor lookup (hover, signature help) asks for these.
    Constructor,
}

/// A classpath type offerable by name: completion label plus the names
/// needed to resolve and to import it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeCandidate {
    /// Innermost simple name (`Entry`) — the completion label.
    pub simple: String,
    /// Binary dotted name (`java.util.Map$Entry`) — the `class()` lookup key.
    pub fqn: String,
    /// Canonical import path (`java.util.Map.Entry`).
    pub import_path: String,
}

/// Provides signature-level symbols for fully-qualified type names (binary
/// names; nested types use `$`). Implementations must be cheap/cached.
pub trait SymbolSource {
    fn class(&self, fqn: &str) -> Option<ExternalClass>;

    /// Classpath types starting with `prefix` (case-insensitive), best-first,
    /// capped at `limit`; the bool reports whether the cap truncated results.
    fn types_with_prefix(&self, _prefix: &str, _limit: usize) -> (Vec<TypeCandidate>, bool) {
        (Vec::new(), false)
    }

    /// Immediate children of a dotted package (`""` = roots), as
    /// `(subpackage segments, types)`.
    fn package_children(&self, _package: &str) -> (Vec<String>, Vec<TypeCandidate>) {
        (Vec::new(), Vec::new())
    }

    /// Javadoc for a fully-qualified type (`member` = `None`) or its named
    /// member, recovered from source archives. Defaults to none.
    fn doc(&self, _fqn: &str, _member: Option<&str>) -> Option<String> {
        None
    }

    /// Type arguments for each `class(fqn)` supertype, index-aligned with
    /// `supers`, using the same `{i}` placeholder convention as
    /// [`ExternalMember::template`]. `[]` means raw or untracked (the default).
    fn super_type_args(&self, _fqn: &str) -> Vec<Vec<String>> {
        Vec::new()
    }
}

/// A [`SymbolSource`] that resolves nothing — used when no JDK is available and
/// in tests that exercise only in-project resolution.
pub struct NoSymbols;

impl SymbolSource for NoSymbols {
    fn class(&self, _fqn: &str) -> Option<ExternalClass> {
        None
    }
}
