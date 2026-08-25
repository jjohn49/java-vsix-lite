//! LSP completion: member completion after `.` and in-scope identifier/keyword
//! completion while typing. Both run against open documents only.

use std::collections::HashSet;

use ls_types::{
    CompletionItem, CompletionItemKind, Documentation, InsertTextFormat, MarkupContent, MarkupKind,
    Position, TextEdit,
};
use serde_json::{json, Value};
use tree_sitter::Node;

use crate::external::{ExternalMember, ExternalMemberKind, SymbolSource};
use crate::imports::Imports;
use crate::model::{named_children, Member, MemberKind, TypeDecl, TypeKind, TypeTable};
use crate::resolve::{self, Binding, BindingKind, Ctx, HierMember, Resolved, ResolvedType};
use crate::signature::{javadoc, signature};
use crate::{node_text, LineIndex, OpenDoc};

/// Java reserved words + literals offered in scope completion. `pub(crate)`
/// so `rename.rs` can refuse a rename's new name when it's one of
/// these, rather than duplicating the list.
pub(crate) const KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "var",
    "yield",
    "record",
    "sealed",
    "permits",
    "true",
    "false",
    "null",
];

/// A completion answer — the items plus whether the set was capped, so
/// the server can tell the client to re-query as the user types
/// (LSP `CompletionList.isIncomplete`).
pub struct CompletionResult {
    pub items: Vec<CompletionItem>,
    pub is_incomplete: bool,
}

impl CompletionResult {
    fn complete(items: Vec<CompletionItem>) -> CompletionResult {
        CompletionResult {
            items,
            is_incomplete: false,
        }
    }
}

/// Minimum typed-identifier length before classpath type names join scope
/// completion — below this the candidate space is all of `java.*` and every
/// dependency, which is noise, not help.
const MIN_TYPE_PREFIX: usize = 2;

/// Cap on classpath type candidates per request; hitting it sets
/// `is_incomplete` so the client re-queries on further typing.
const MAX_CLASSPATH_TYPES: usize = 200;

/// Produce completion items for the cursor position. Inside an `import`
/// declaration this walks packages/types/static members; after a resolvable
/// `.` it is member completion; otherwise in-scope identifiers + keywords +
/// classpath type names (with auto-import). `snippets` reflects the client's
/// `completionItem.snippetSupport` capability.
pub fn completion(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    snippets: bool,
    symbols: &dyn SymbolSource,
) -> CompletionResult {
    let Some(doc) = docs.get(current) else {
        return CompletionResult::complete(Vec::new());
    };
    let cursor = index.offset(pos);
    let table = TypeTable::build(docs, current);
    let imports = Imports::parse(doc.tree, doc.source);
    let ctx = Ctx {
        doc,
        current,
        table: &table,
        imports: &imports,
        symbols,
    };

    // `import java.ut|` — checked before member access, which would
    // otherwise treat the path's trailing `.` as a member dot and resolve
    // nothing.
    if let Some(items) = import_items(doc.source, cursor, &ctx) {
        return CompletionResult::complete(items);
    }

    if let Some(recv) = resolve::member_receiver(doc.tree, doc.source, cursor) {
        // In a member-access position: only members, never scope fallback, so a
        // typed `.` never yields wrong global suggestions.
        return CompletionResult::complete(match resolve::resolve_receiver_type(recv, &ctx) {
            Some(resolved) => member_items(&resolved, &ctx, snippets),
            None => Vec::new(),
        });
    }

    scope_items(&ctx, cursor, snippets, index)
}

fn member_items<'t>(
    resolved: &Resolved<'t>,
    ctx: &Ctx<'_, 't>,
    snippets: bool,
) -> Vec<CompletionItem> {
    resolve::collect_members(resolved, ctx)
        .iter()
        .map(|m| hier_item(m, resolved, snippets))
        .collect()
}

fn hier_item(member: &HierMember, resolved: &Resolved, snippets: bool) -> CompletionItem {
    match member {
        HierMember::InProject(m) => inproject_item(m, snippets),
        HierMember::External(m) => external_item(m, resolved, snippets),
    }
}

/// No `documentation` here — Javadoc lookup is strictly lazy, deferred
/// to `completionItem/resolve` (see [`resolve_documentation`]) so a plain
/// `textDocument/completion` request never pays for it. `data` carries just
/// enough to re-find the same Javadoc later.
fn inproject_item(member: &Member, snippets: bool) -> CompletionItem {
    let mut item = CompletionItem {
        label: member.name.to_string(),
        kind: Some(member_kind(member.kind)),
        detail: signature(member.node, member.source),
        data: inproject_data(member),
        ..Default::default()
    };
    if member.kind == MemberKind::Method {
        apply_method_insert(&mut item, method_has_params(member.node), snippets);
    }
    item
}

/// Lazy-resolve key for an in-project member's Javadoc: the *declaring*
/// type's own simple name (found by walking up from the member's node, not
/// the resolved receiver's — precise even for an inherited member, and needs
/// no inheritance walk to re-find at resolve time) plus the member's name,
/// plus the index (`"doc"`) of the declaring document in the `&[OpenDoc]`
/// slice this request ran against. The index is meaningless across requests
/// (document maps have no stable order); the caller — the server, the only
/// party that knows URIs (this crate is deliberately URI-free) — must
/// translate it into the originating document's URI before the item goes on
/// the wire, so `completionItem/resolve` can re-find *that exact document*
/// rather than scanning all open documents, where a same-simple-name type
/// declared elsewhere could win the lookup and yield the wrong member's
/// Javadoc. `None` if the member somehow isn't inside any type declaration
/// (never true for a `Member` built from `TypeDecl::own_members`, but
/// resolution never panics on a shape it didn't expect).
fn inproject_data(member: &Member) -> Option<Value> {
    let type_node = resolve::enclosing_type_node(member.node)?;
    let type_name = node_text(type_node.child_by_field_name("name")?, member.source);
    Some(json!({
        "kind": "inproject",
        "type": type_name,
        "member": member.name,
        "doc": member.doc,
    }))
}

fn external_item(member: &ExternalMember, resolved: &Resolved, snippets: bool) -> CompletionItem {
    let kind = match member.kind {
        ExternalMemberKind::Method => CompletionItemKind::METHOD,
        ExternalMemberKind::Field => CompletionItemKind::FIELD,
        ExternalMemberKind::Constructor => CompletionItemKind::CONSTRUCTOR,
    };
    let mut item = CompletionItem {
        label: member.name.clone(),
        kind: Some(kind),
        detail: Some(member.signature.clone()),
        data: external_data(member, resolved),
        ..Default::default()
    };
    if member.kind == ExternalMemberKind::Method {
        // A rendered signature ending in `()` takes no parameters.
        apply_method_insert(&mut item, !member.signature.ends_with("()"), snippets);
    }
    item
}

/// Lazy-resolve key for an external member's Javadoc: the *receiver's* FQN —
/// only available when the receiver itself is external. An external member
/// reached transitively through an in-project receiver (`class Derived
/// extends ArrayList`) has no FQN on hand here, so it gets no lazy-resolve
/// key at all — the same limitation hover already accepts (see
/// `hover::member_target`'s doc comment), not a new regression.
fn external_data(member: &ExternalMember, resolved: &Resolved) -> Option<Value> {
    match &resolved.ty {
        ResolvedType::External { fqn, .. } => Some(json!({
            "kind": "external",
            "fqn": fqn,
            "member": member.name,
        })),
        ResolvedType::InProject(_)
        | ResolvedType::Primitive(_)
        | ResolvedType::Void
        | ResolvedType::Null
        | ResolvedType::Array { .. } => None,
    }
}

/// The `completionItem/resolve` counterpart to [`completion`]'s strictly-lazy
/// `data` payload: given the JSON a completion item's `data` field carried,
/// find and render that member's Javadoc through the same markdown pipeline
/// hover uses (`javadoc` + [`markdown`]). `None` on any missing/unrecognized
/// key, or when the member no longer resolves (e.g. edited away since the
/// completion request) — never a panic.
///
/// For an `"inproject"` key, `docs` must contain **only the originating
/// document** (the one the item's `"doc"` index — translated by the server
/// into a URI — named at completion time). Passing every open document
/// instead would re-introduce the wrong-doc collision the index/URI exists
/// to prevent: with two open files declaring same-named types and members,
/// the simple-name `TypeTable` lookup could silently return the *other*
/// file's member and attach the wrong Javadoc. When the originating document
/// is no longer open, pass an empty slice — this resolves to `None` (no
/// documentation) rather than guessing.
pub fn resolve_documentation(
    docs: &[OpenDoc],
    data: &Value,
    symbols: &dyn SymbolSource,
) -> Option<Documentation> {
    let kind = data.get("kind")?.as_str()?;
    let member = data.get("member").and_then(Value::as_str);
    let text = match kind {
        "inproject" => {
            let type_name = data.get("type")?.as_str()?;
            let table = TypeTable::build(docs, 0);
            let td = table.get(type_name)?;
            let m = table.find_member(td, member?)?;
            javadoc(m.node, m.source)
        }
        "external" => {
            let fqn = data.get("fqn")?.as_str()?;
            symbols.doc(fqn, Some(member?))
        }
        // A classpath *type* item (scope or import completion).
        "external_type" => {
            let fqn = data.get("fqn")?.as_str()?;
            symbols.doc(fqn, None)
        }
        _ => None,
    }?;
    Some(markdown(text))
}

fn method_has_params(method: Node) -> bool {
    method
        .child_by_field_name("parameters")
        .map(|p| {
            named_children(p)
                .iter()
                .any(|c| matches!(c.kind(), "formal_parameter" | "spread_parameter"))
        })
        .unwrap_or(false)
}

/// Decide a method's insert text. With snippet support: `name()` (zero-arg) or a
/// `name($1)` tab-stop snippet. Without it: `name()` or `name(` (the editor
/// leaves the cursor after the paren). `$` is escaped because it is legal in Java
/// identifiers and is snippet-special.
fn apply_method_insert(item: &mut CompletionItem, has_params: bool, snippets: bool) {
    if !has_params {
        item.insert_text = Some(format!("{}()", item.label));
    } else if snippets {
        let label = item.label.replace('$', "\\$");
        item.insert_text = Some(format!("{label}($1)"));
        item.insert_text_format = Some(InsertTextFormat::SNIPPET);
    } else {
        item.insert_text = Some(format!("{}(", item.label));
    }
}

/// Stable ranking buckets, prefixed onto `sort_text` so closer things
/// sort first: bindings < enclosing members < in-project types < classpath
/// types < keywords. Clients that fuzzy-rank still respect this as the
/// tiebreak.
fn bucketed(mut item: CompletionItem, bucket: u8) -> CompletionItem {
    item.sort_text = Some(format!("{bucket}{}", item.label));
    item
}

fn scope_items<'t>(
    ctx: &Ctx<'_, 't>,
    cursor: usize,
    snippets: bool,
    index: &LineIndex,
) -> CompletionResult {
    let doc = ctx.doc;
    let mut items = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |item: CompletionItem, items: &mut Vec<CompletionItem>| {
        let key = format!("{}\u{0}{:?}", item.label, item.kind);
        if seen.insert(key) {
            items.push(item);
        }
    };

    // Locals, params, for-vars. Fields come from the member pass below, so they
    // are excluded here to avoid recomputing the member set.
    for binding in
        resolve::collect_bindings(doc.tree, doc.source, cursor, ctx.table, false, ctx.current)
    {
        push(bucketed(binding_item(&binding), 0), &mut items);
    }

    // The enclosing type's members (fields + methods + nested types), own and
    // inherited (including from external supertypes), callable unqualified.
    let node = resolve::node_at(doc.tree, cursor);
    if let Some(td) = resolve::enclosing_typedecl(node, doc.source, ctx.current) {
        let resolved = Resolved {
            ty: ResolvedType::InProject(td),
            static_only: false,
        };
        for member in resolve::collect_members(&resolved, ctx) {
            push(
                bucketed(hier_item(&member, &resolved, snippets), 1),
                &mut items,
            );
        }
    }

    // In-scope type names (current + open files).
    for decl in ctx.table.iter() {
        push(bucketed(type_item(decl), 2), &mut items);
    }

    // Classpath/project type names matching the typed prefix, with
    // auto-import. Open-document types shadow same-named candidates (they
    // were pushed above; the candidate is skipped entirely so a stale
    // classpath twin can't appear alongside).
    //
    // Every scope-completion result is `is_incomplete` — a deliberate
    // signal to the client, not the literal LSP-spec meaning. VS Code
    // treats an `isIncomplete: false` list as "stable, filter it yourself
    // as I keep typing" and stops re-querying, and the scope candidate set
    // genuinely changes with the typed prefix in BOTH regimes:
    //
    // - below `MIN_TYPE_PREFIX`, classpath/project types are deliberately
    //   withheld — the word's very first (1-char) request marked complete
    //   would freeze that classpath-less list for the whole word, which is
    //   exactly the field-reported failure ("typing `person` only shows
    //   the variable; forcing a fresh request via delete-and-retype shows
    //   `Person` and the other classes too");
    // - at or past it, the candidate list is a prefix-filtered (and
    //   possibly capped) slice that a longer prefix re-ranks and refills.
    //
    // Member and import-path completion stay complete: their item sets
    // only ever narrow under further typing, so client-side filtering is
    // correct there.
    let is_incomplete = true;
    let prefix = typed_prefix(doc.source, cursor);
    if prefix.len() >= MIN_TYPE_PREFIX {
        let (candidates, _truncated) = ctx.symbols.types_with_prefix(prefix, MAX_CLASSPATH_TYPES);
        let insertion = ImportInsertion::compute(doc, index);
        for c in &candidates {
            if ctx.table.get(&c.simple).is_some() {
                continue; // an open document declares this simple name
            }
            match import_status(c, ctx.imports) {
                ImportStatus::Conflicting => continue,
                status => {
                    let mut item = CompletionItem {
                        label: c.simple.clone(),
                        kind: Some(CompletionItemKind::CLASS),
                        detail: Some(c.import_path.clone()),
                        data: Some(json!({ "kind": "external_type", "fqn": c.fqn })),
                        ..Default::default()
                    };
                    if status == ImportStatus::NeedsImport {
                        item.additional_text_edits = Some(vec![insertion.edit(&c.import_path)]);
                    }
                    push(bucketed(item, 3), &mut items);
                }
            }
        }
    }

    // Keywords.
    for kw in KEYWORDS {
        push(
            bucketed(
                CompletionItem {
                    label: kw.to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    ..Default::default()
                },
                4,
            ),
            &mut items,
        );
    }

    CompletionResult {
        items,
        is_incomplete,
    }
}

/// The identifier fragment immediately before the cursor — the typed
/// prefix classpath type-name completion matches against.
fn typed_prefix(source: &str, cursor: usize) -> &str {
    let bytes = source.as_bytes();
    let end = cursor.min(bytes.len());
    let mut start = end;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric() || matches!(bytes[start - 1], b'_' | b'$'))
    {
        start -= 1;
    }
    &source[start..end]
}

/// Whether (and how) a classpath type candidate needs an import to be
/// referenced by its simple name in this file.
#[derive(PartialEq, Eq, Clone, Copy)]
enum ImportStatus {
    /// Insert an import on accept.
    NeedsImport,
    /// Usable as-is (already imported / same package / `java.lang` /
    /// wildcard-covered).
    NoImportNeeded,
    /// The simple name is single-imported to a *different* type — this
    /// candidate can't be referenced by simple name at all; don't offer it.
    Conflicting,
}

fn import_status(c: &crate::external::TypeCandidate, imports: &Imports) -> ImportStatus {
    if let Some(existing) = imports.single_import(&c.simple) {
        return if existing == c.import_path {
            ImportStatus::NoImportNeeded
        } else {
            ImportStatus::Conflicting
        };
    }
    // A nested type (`Map.Entry`) always needs its explicit import — a
    // wildcard or same-package context never puts the *inner* simple name in
    // scope.
    if c.fqn.contains('$') {
        return ImportStatus::NeedsImport;
    }
    let package = match c.import_path.rsplit_once('.') {
        Some((pkg, _)) => pkg,
        None => "", // default package — never importable, usable as-is
    };
    if package.is_empty()
        || package == "java.lang"
        || Some(package) == imports.package()
        || imports.has_wildcard(package)
    {
        ImportStatus::NoImportNeeded
    } else {
        ImportStatus::NeedsImport
    }
}

/// Where to insert a new `import`, computed once per request from the
/// tree: after the last existing import, else after the `package`
/// declaration, else at the very top.
///
/// `pub(crate)`: also used by `codeaction.rs`'s add-import quick fix.
pub(crate) struct ImportInsertion {
    position: Position,
    /// Text template around the path: `(before, after)`.
    wrap: (&'static str, &'static str),
}

impl ImportInsertion {
    pub(crate) fn compute(doc: &OpenDoc, index: &LineIndex) -> ImportInsertion {
        let mut last_import_end = None;
        let mut package_end = None;
        for child in crate::model::named_children(doc.tree.root_node()) {
            match child.kind() {
                "import_declaration" => last_import_end = Some(child.end_byte()),
                "package_declaration" => package_end = Some(child.end_byte()),
                _ => {}
            }
        }
        if let Some(end) = last_import_end {
            ImportInsertion {
                position: index.position(end),
                wrap: ("\nimport ", ";"),
            }
        } else if let Some(end) = package_end {
            ImportInsertion {
                position: index.position(end),
                wrap: ("\n\nimport ", ";"),
            }
        } else {
            ImportInsertion {
                position: index.position(0),
                wrap: ("import ", ";\n\n"),
            }
        }
    }

    pub(crate) fn edit(&self, import_path: &str) -> TextEdit {
        TextEdit {
            range: ls_types::Range {
                start: self.position,
                end: self.position,
            },
            new_text: format!("{}{}{}", self.wrap.0, import_path, self.wrap.1),
        }
    }
}

/// Completion inside an `import` declaration — package segments, types,
/// nested types, and (for `import static`) static members. `None` when the
/// cursor line isn't a plain import path, letting ordinary completion run.
fn import_items(source: &str, cursor: usize, ctx: &Ctx) -> Option<Vec<CompletionItem>> {
    let cursor = cursor.min(source.len());
    let line_start = source[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &source[line_start..cursor];
    let rest = line.trim_start().strip_prefix("import")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None; // an identifier like `imported` — not the keyword
    }
    let rest = rest.trim_start();
    let (is_static, path) = match rest.strip_prefix("static") {
        Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim_start()),
        _ => (false, rest),
    };
    if path
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.')))
    {
        return None; // past the path (`;`, comment, …) — nothing to offer
    }
    let (parent, prefix) = match path.rfind('.') {
        Some(dot) => (&path[..dot], &path[dot + 1..]),
        None => ("", path),
    };

    let mut items = Vec::new();
    // The `static` keyword itself, while still typing the first word.
    if parent.is_empty() && !is_static && "static".starts_with(prefix) && !prefix.is_empty() {
        items.push(CompletionItem {
            label: "static".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    let (subpackages, types) = ctx.symbols.package_children(parent);
    if !subpackages.is_empty() || !types.is_empty() {
        // `parent` is a package: offer its subpackages and types.
        let matches = |s: &str| prefix.is_empty() || starts_with_ci(s, prefix);
        for pkg in &subpackages {
            if matches(pkg) {
                items.push(CompletionItem {
                    label: pkg.clone(),
                    kind: Some(CompletionItemKind::MODULE),
                    ..Default::default()
                });
            }
        }
        for t in &types {
            if matches(&t.simple) {
                items.push(CompletionItem {
                    label: t.simple.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(t.import_path.clone()),
                    data: Some(json!({ "kind": "external_type", "fqn": t.fqn })),
                    ..Default::default()
                });
            }
        }
        return Some(items);
    }

    // `parent` may instead be a *type* path (`java.util.Map` /
    // `java.util.Map.Entry`): offer its nested types and, for a static
    // import, its static members.
    if let Some(fqn) = resolve::import_path_to_fqn(parent, ctx) {
        let package = fqn.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
        let (_, siblings) = ctx.symbols.package_children(package);
        let nested_prefix = format!("{fqn}$");
        for t in &siblings {
            let Some(inner) = t.fqn.strip_prefix(&nested_prefix) else {
                continue;
            };
            if inner.contains('$') {
                continue; // not an immediate child
            }
            if prefix.is_empty() || starts_with_ci(inner, prefix) {
                items.push(CompletionItem {
                    label: inner.to_string(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(t.import_path.clone()),
                    data: Some(json!({ "kind": "external_type", "fqn": t.fqn })),
                    ..Default::default()
                });
            }
        }
        if is_static {
            if let Some(class) = ctx.symbols.class(&fqn) {
                let mut seen = HashSet::new();
                for m in class.members {
                    if !m.is_static
                        || m.kind == ExternalMemberKind::Constructor
                        || !(prefix.is_empty() || starts_with_ci(&m.name, prefix))
                        || !seen.insert(m.name.clone())
                    {
                        continue;
                    }
                    items.push(CompletionItem {
                        kind: Some(match m.kind {
                            ExternalMemberKind::Field => CompletionItemKind::FIELD,
                            _ => CompletionItemKind::METHOD,
                        }),
                        label: m.name,
                        detail: Some(m.signature),
                        ..Default::default()
                    });
                }
            }
        }
    }
    Some(items)
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn binding_item(binding: &Binding) -> CompletionItem {
    let kind = match binding.kind {
        BindingKind::Field => CompletionItemKind::FIELD,
        _ => CompletionItemKind::VARIABLE,
    };
    CompletionItem {
        label: binding.name.to_string(),
        kind: Some(kind),
        detail: signature(binding.decl_node, binding.source),
        ..Default::default()
    }
}

fn type_item(decl: &TypeDecl) -> CompletionItem {
    CompletionItem {
        label: decl.name.to_string(),
        kind: Some(type_kind(decl.kind)),
        detail: Some(format!("{} {}", decl.kind.keyword(), decl.name)),
        ..Default::default()
    }
}

fn member_kind(kind: MemberKind) -> CompletionItemKind {
    match kind {
        MemberKind::Method => CompletionItemKind::METHOD,
        MemberKind::Field => CompletionItemKind::FIELD,
        MemberKind::EnumConstant => CompletionItemKind::ENUM_MEMBER,
        MemberKind::NestedType(tk) => type_kind(tk),
    }
}

fn type_kind(kind: TypeKind) -> CompletionItemKind {
    match kind {
        TypeKind::Class | TypeKind::Record => CompletionItemKind::CLASS,
        TypeKind::Interface | TypeKind::Annotation => CompletionItemKind::INTERFACE,
        TypeKind::Enum => CompletionItemKind::ENUM,
    }
}

fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
