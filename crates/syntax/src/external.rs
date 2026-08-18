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
}

/// One member of an external type.
#[derive(Debug, Clone)]
pub struct ExternalMember {
    pub name: String,
    pub kind: ExternalMemberKind,
    /// Raw (generics-erased) signature — also the cross-declaration dedup key.
    pub signature: String,
    /// Generic signature with `{i}` placeholders for the declaring class's type
    /// parameters (e.g. `boolean add({0})`), substituted with a use site's type
    /// arguments. `None` when the member uses no type variables.
    pub template: Option<String>,
    pub is_static: bool,
    /// M7: dotted FQN of the erased method return / field declared type —
    /// what a `recv.member().` chain resolves through. `None` for
    /// primitives, `void`, arrays, and constructors.
    pub ret_fqn: Option<String>,
    /// M7: the generic return/field type alone in `{i}` template form
    /// (`Stream<{0}>`, `{0}`), so chains substitute use-site type arguments
    /// before re-resolving. `None` without generic info.
    pub ret_display: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalMemberKind {
    Method,
    Field,
    /// A constructor (`ClassName(paramTypes)`, `name` = the declaring
    /// class's simple name — see `jvl_classpath::MemberKind::Constructor`).
    /// Never yielded by [`SymbolSource::class`]'s members through the
    /// ordinary member-hierarchy walk (`resolve::collect_members` filters it
    /// out, same as it never lists constructors for in-project types) —
    /// only a dedicated constructor lookup (hover on `new Foo(...)`,
    /// constructor signature help) asks for these.
    Constructor,
}

/// A classpath type offerable by name (M7): completion label plus the names
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

/// Provides signature-level symbols for fully-qualified type names. Binary names
/// (nested types use `$`) are expected. Implementations must be cheap/cached;
/// `jvl-syntax` may call this many times per request.
pub trait SymbolSource {
    fn class(&self, fqn: &str) -> Option<ExternalClass>;

    /// M7: classpath types whose simple name starts with `prefix`
    /// (case-insensitive), best-first, at most `limit`; the bool reports
    /// whether the cap cut candidates off. Defaults to none (mocks, and a
    /// server with no classpath).
    fn types_with_prefix(&self, _prefix: &str, _limit: usize) -> (Vec<TypeCandidate>, bool) {
        (Vec::new(), false)
    }

    /// M7: immediate children of a dotted package (`""` = roots):
    /// `(subpackage segments, types)` — the shape import-path completion
    /// walks. Defaults to none.
    fn package_children(&self, _package: &str) -> (Vec<String>, Vec<TypeCandidate>) {
        (Vec::new(), Vec::new())
    }

    /// Javadoc for a fully-qualified type (`member` = `None`) or its named
    /// member, recovered from source archives. Defaults to none.
    fn doc(&self, _fqn: &str, _member: Option<&str>) -> Option<String> {
        None
    }

    /// Type arguments applied to each entry of `class(fqn)`'s `supers` list —
    /// index-aligned with `supers`, e.g. for `class MyList<T> extends
    /// AbstractList<T>`, entry 0 is `["{0}"]`. Uses the same `{i}` placeholder
    /// convention as [`ExternalMember::template`] (referring to `fqn`'s own
    /// `type_params`), so a caller substitutes through it exactly like a member
    /// template. A raw (unparameterized) supertype, or one whose arguments
    /// aren't tracked, is `[]`.
    ///
    /// Defaults to "nothing tracked" for every entry: an implementation that
    /// doesn't override this (e.g. [`NoSymbols`], test stubs) degrades
    /// inherited members to erased rendering — the same graceful degradation
    /// as a raw supertype.
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
