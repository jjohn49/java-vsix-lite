//! The seam between pure analysis and bytecode-backed external symbols.
//!
//! `jvl-syntax` stays IO-free: it calls [`SymbolSource`] to learn the members and
//! supertypes of a fully-qualified type it cannot find in the open documents. The
//! server implements this over `jvl-classpath`; tests pass a mock.

/// A type resolved from outside the open documents (a JDK or dependency class).
pub struct ExternalClass {
    /// Superclass + interface FQNs.
    pub supers: Vec<String>,
    /// Formal type-parameter names, e.g. `["E"]` for `ArrayList<E>`.
    pub type_params: Vec<String>,
    pub members: Vec<ExternalMember>,
}

/// One member of an external type.
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExternalMemberKind {
    Method,
    Field,
}

/// Provides signature-level symbols for fully-qualified type names. Binary names
/// (nested types use `$`) are expected. Implementations must be cheap/cached;
/// `jvl-syntax` may call this many times per request.
pub trait SymbolSource {
    fn class(&self, fqn: &str) -> Option<ExternalClass>;

    /// Javadoc for a fully-qualified type (`member` = `None`) or its named
    /// member, recovered from source archives. Defaults to none.
    fn doc(&self, _fqn: &str, _member: Option<&str>) -> Option<String> {
        None
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
