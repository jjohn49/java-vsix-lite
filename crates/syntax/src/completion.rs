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
/// (M4.4) so `rename.rs` can refuse a rename's new name when it's one of
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

/// M7: a completion answer — the items plus whether the set was capped, so
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

/// M7: minimum typed-identifier length before classpath type names join scope
/// completion — below this the candidate space is all of `java.*` and every
/// dependency, which is noise, not help.
const MIN_TYPE_PREFIX: usize = 2;

/// M7: cap on classpath type candidates per request; hitting it sets
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

    // M7: `import java.ut|` — checked before member access, which would
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

/// M6.3: no `documentation` here — Javadoc lookup is strictly lazy, deferred
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
        ResolvedType::InProject(_) | ResolvedType::Array { .. } => None,
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
        // M7: a classpath *type* item (scope or import completion).
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

/// M7: stable ranking buckets, prefixed onto `sort_text` so closer things
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

    // M7: classpath/project type names matching the typed prefix, with
    // auto-import. Open-document types shadow same-named candidates (they
    // were pushed above; the candidate is skipped entirely so a stale
    // classpath twin can't appear alongside).
    //
    // `is_incomplete` is set unconditionally whenever this branch runs at
    // all, not only when `MAX_CLASSPATH_TYPES` truncates — this is a
    // deliberate signal to the client, not the literal LSP-spec meaning.
    // VS Code treats an `isIncomplete: false` list as "stable, filter it
    // yourself as I keep typing" and stops re-querying; with a classpath/
    // dependency symbol table in the thousands, a client-side filter of one
    // early (possibly narrow, possibly capped) response reliably buries or
    // drops a genuine match as the prefix changes — precisely the
    // real-world failure this fixes (confirmed empirically: the same query
    // re-sent fresh, e.g. after a delete-and-retype forcing a new request,
    // returns the correct merged list every time). Marking it incomplete
    // forces a fresh request on every keystroke instead.
    let mut is_incomplete = false;
    let prefix = typed_prefix(doc.source, cursor);
    if prefix.len() >= MIN_TYPE_PREFIX {
        let (candidates, _truncated) = ctx.symbols.types_with_prefix(prefix, MAX_CLASSPATH_TYPES);
        is_incomplete = true;
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

/// M7: the identifier fragment immediately before the cursor — the typed
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

/// M7: whether (and how) a classpath type candidate needs an import to be
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

/// M7: where to insert a new `import`, computed once per request from the
/// tree: after the last existing import, else after the `package`
/// declaration, else at the very top.
struct ImportInsertion {
    position: Position,
    /// Text template around the path: `(before, after)`.
    wrap: (&'static str, &'static str),
}

impl ImportInsertion {
    fn compute(doc: &OpenDoc, index: &LineIndex) -> ImportInsertion {
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

    fn edit(&self, import_path: &str) -> TextEdit {
        TextEdit {
            range: ls_types::Range {
                start: self.position,
                end: self.position,
            },
            new_text: format!("{}{}{}", self.wrap.0, import_path, self.wrap.1),
        }
    }
}

/// M7: completion inside an `import` declaration — package segments, types,
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
    if let Some(fqn) = import_path_to_fqn(parent, ctx) {
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

/// M7: an import path (`java.util.Map.Entry`) to the binary FQN
/// (`java.util.Map$Entry`) — replace trailing dots with `$` until the symbol
/// source recognizes the name.
fn import_path_to_fqn(path: &str, ctx: &Ctx) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut candidate = path.to_string();
    for _ in 0..8 {
        if ctx.symbols.class(&candidate).is_some() {
            return Some(candidate);
        }
        let dot = candidate.rfind('.')?;
        candidate.replace_range(dot..dot + 1, "$");
    }
    None
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
mod tests {
    use super::*;
    use crate::external::{ExternalClass, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use std::collections::HashMap;
    use tree_sitter::Tree;

    /// A `SymbolSource` backed by a fixture map, for hermetic external-symbol tests.
    struct MockSymbols(HashMap<String, ExternalClass>);

    impl SymbolSource for MockSymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            self.0.get(fqn).map(|c| ExternalClass {
                supers: c.supers.clone(),
                type_params: c.type_params.clone(),
                members: c
                    .members
                    .iter()
                    .map(|m| ExternalMember {
                        name: m.name.clone(),
                        kind: m.kind,
                        signature: m.signature.clone(),
                        template: m.template.clone(),
                        is_static: m.is_static,
                        ret_fqn: m.ret_fqn.clone(),
                        ret_display: m.ret_display.clone(),
                    })
                    .collect(),
            })
        }
    }

    fn ext_method(name: &str, signature: &str) -> ExternalMember {
        ExternalMember {
            name: name.to_string(),
            kind: ExternalMemberKind::Method,
            signature: signature.to_string(),
            template: None,
            is_static: false,
            ret_fqn: None,
            ret_display: None,
        }
    }

    /// A method member carrying a generic template (e.g. `boolean add({0})`).
    fn ext_generic_method(name: &str, erased: &str, template: &str) -> ExternalMember {
        ExternalMember {
            name: name.to_string(),
            kind: ExternalMemberKind::Method,
            signature: erased.to_string(),
            template: Some(template.to_string()),
            is_static: false,
            ret_fqn: None,
            ret_display: None,
        }
    }

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    /// Completion at the byte position **immediately after** the first occurrence
    /// of `marker` (so `"d."` places the cursor right after the dot).
    fn complete(src: &str, marker: &str) -> Vec<CompletionItem> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present") + marker.len();
        completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    fn has(items: &[CompletionItem], label: &str) -> bool {
        items.iter().any(|i| i.label == label)
    }

    fn detail_of<'a>(items: &'a [CompletionItem], label: &str) -> Option<&'a str> {
        items
            .iter()
            .find(|i| i.label == label)
            .and_then(|i| i.detail.as_deref())
    }

    #[test]
    fn member_completion_on_local_variable() {
        let src = "class Box { int width; int height() { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
        let items = complete(src, "b.");
        assert!(has(&items, "width"), "{:?}", labels(&items));
        assert!(has(&items, "height"), "{:?}", labels(&items));
        assert_eq!(detail_of(&items, "width"), Some("int width"));
        assert_eq!(detail_of(&items, "height"), Some("int height()"));
        // Member completion must NOT include scope noise like keywords.
        assert!(!has(&items, "class"));
    }

    #[test]
    fn member_completion_on_this_includes_inherited() {
        let src = "class Base { int baseField; void baseM() {} }\n\
                   class Derived extends Base { int own; void m() { this.x; } }\n";
        let items = complete(src, "this.");
        assert!(has(&items, "own"), "{:?}", labels(&items));
        assert!(
            has(&items, "baseField"),
            "inherited field {:?}",
            labels(&items)
        );
        assert!(
            has(&items, "baseM"),
            "inherited method {:?}",
            labels(&items)
        );
    }

    #[test]
    fn member_completion_on_super() {
        let src = "class Base { int baseField; }\n\
                   class Derived extends Base { void m() { super.x; } }\n";
        let items = complete(src, "super.");
        assert!(has(&items, "baseField"), "{:?}", labels(&items));
    }

    #[test]
    fn member_completion_on_new_expression() {
        let src = "class Box { int w; }\n\
                   class C { void m() { new Box().x; } }\n";
        let items = complete(src, ").");
        assert!(has(&items, "w"), "{:?}", labels(&items));
    }

    #[test]
    fn member_completion_through_field_access_chain() {
        let src = "class Inner { int leaf; }\n\
                   class C { Inner inner; void m() { this.inner.x; } }\n";
        let items = complete(src, "inner.");
        assert!(has(&items, "leaf"), "{:?}", labels(&items));
    }

    #[test]
    fn inner_binding_shadows_outer_field_for_member_type() {
        let src = "class A { int aOnly; }\n\
                   class B { int bOnly; }\n\
                   class C { A v; void m() { B v; v.x; } }\n";
        let items = complete(src, "v.");
        assert!(
            has(&items, "bOnly"),
            "inner type wins: {:?}",
            labels(&items)
        );
        assert!(
            !has(&items, "aOnly"),
            "outer field shadowed: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn unresolved_receiver_yields_no_items() {
        // `foo()` is a method call — return-type inference is deferred.
        let src = "class C { void m() { foo().x; } }\n";
        let items = complete(src, "foo().");
        assert!(items.is_empty(), "{:?}", labels(&items));
    }

    #[test]
    fn scope_completion_lists_locals_params_fields_keywords() {
        let src = "class C { int field; void m(int param) { int local = 1; ZZZ } }\n";
        let items = complete(src, "ZZZ");
        assert!(has(&items, "local"), "local: {:?}", labels(&items));
        assert!(has(&items, "param"), "param: {:?}", labels(&items));
        assert!(has(&items, "field"), "field: {:?}", labels(&items));
        assert!(has(&items, "return"), "keyword: {:?}", labels(&items));
        assert!(has(&items, "C"), "type name: {:?}", labels(&items));
        assert!(has(&items, "m"), "method: {:?}", labels(&items));
    }

    #[test]
    fn scope_completion_respects_declaration_order() {
        let src = "class C { void m() { int before = 1; ZZZ; int after = 2; } }\n";
        let items = complete(src, "ZZZ");
        assert!(
            has(&items, "before"),
            "before-cursor local: {:?}",
            labels(&items)
        );
        assert!(
            !has(&items, "after"),
            "after-cursor local: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn method_item_inserts_call_snippet() {
        let src = "class Box { int height() { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
        let items = complete(src, "b.");
        let height = items.iter().find(|i| i.label == "height").unwrap();
        assert_eq!(height.insert_text.as_deref(), Some("height()"));
    }

    #[test]
    fn cross_file_type_resolution() {
        let lib = "class Widget { int spin; }\n";
        let use_src = "class C { void m() { Widget w; w.x; } }\n";
        let lib_tree = tree(lib);
        let use_tree = tree(use_src);
        let docs = [
            OpenDoc {
                source: use_src,
                tree: &use_tree,
            },
            OpenDoc {
                source: lib,
                tree: &lib_tree,
            },
        ];
        let index = LineIndex::new(use_src, PositionEncoding::Utf16);
        let at = use_src.find("w.").unwrap() + 2;
        let items = completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items;
        assert!(has(&items, "spin"), "cross-file: {:?}", labels(&items));
    }

    #[test]
    fn trailing_dot_does_not_panic() {
        // Pathological: bare receiver with nothing after the dot.
        let _ = complete("class C { void m() { x. } }\n", "x.");
        let _ = complete("class C { void m() { . } }\n", ".");
        let _ = complete("", "");
    }

    // --- Regression tests for the adversarial review findings ---

    fn complete_snip(src: &str, marker: &str, snippets: bool) -> Vec<CompletionItem> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present") + marker.len();
        completion(&docs, 0, &index, index.position(at), snippets, &NoSymbols).items
    }

    #[test]
    fn member_completion_while_typing_partial_unterminated() {
        // The dominant live case: partial member, no trailing `;`. tree-sitter
        // parses `b.wi` as a scoped_type_identifier of type_identifier segments.
        let src = "class Box { int width; int height; }\n\
                   class C { void m() { Box b; b.wi } }\n";
        let items = complete(src, "b.wi");
        assert!(has(&items, "width"), "{:?}", labels(&items));
        assert!(has(&items, "height"), "{:?}", labels(&items));
    }

    #[test]
    fn static_type_receiver_shows_static_members_only() {
        let src = "class Helper { static int S = 1; static void sm() {} int inst; }\n\
                   class C { void m() { Helper.s } }\n";
        let items = complete(src, "Helper.s");
        assert!(has(&items, "S"), "static field: {:?}", labels(&items));
        assert!(has(&items, "sm"), "static method: {:?}", labels(&items));
        assert!(
            !has(&items, "inst"),
            "instance member excluded: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn interface_constants_are_inherited_members() {
        let src = "interface Sized { int MAX = 10; int size(); }\n\
                   class C implements Sized { void m() { this.x; } }\n";
        let items = complete(src, "this.");
        assert!(
            has(&items, "MAX"),
            "interface constant: {:?}",
            labels(&items)
        );
        assert!(
            has(&items, "size"),
            "interface method: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn varargs_parameter_is_in_scope_with_correct_signature() {
        let src = "class C { void m(int... xs) { ZZZ } }\n";
        let items = complete(src, "ZZZ");
        assert!(has(&items, "xs"), "varargs binding: {:?}", labels(&items));
        assert_eq!(detail_of(&items, "xs"), Some("int... xs"));
    }

    #[test]
    fn method_insert_falls_back_to_plaintext_without_snippet_support() {
        let src = "class Box { int f(int n) { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
        let with = complete_snip(src, "b.", true);
        let without = complete_snip(src, "b.", false);
        let f_with = with.iter().find(|i| i.label == "f").unwrap();
        let f_without = without.iter().find(|i| i.label == "f").unwrap();
        assert_eq!(f_with.insert_text.as_deref(), Some("f($1)"));
        assert_eq!(f_with.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert_eq!(f_without.insert_text.as_deref(), Some("f("));
        assert_eq!(f_without.insert_text_format, None);
    }

    #[test]
    fn dollar_in_method_name_is_escaped_in_snippet() {
        // `$` is a legal Java identifier char and is snippet-special.
        let src = "class Box { int a$b(int n) { return 0; } }\n\
                   class C { void m() { Box b; b.x; } }\n";
        let items = complete(src, "b.");
        let m = items.iter().find(|i| i.label == "a$b").unwrap();
        assert_eq!(m.insert_text.as_deref(), Some("a\\$b($1)"));
    }

    #[test]
    fn deeply_nested_receiver_does_not_overflow_the_stack() {
        // Without a depth bound this recurses ~5000 deep and aborts the process.
        let depth = 5000;
        let src = format!(
            "class C {{ void m() {{ {}a{}. }} }}\n",
            "(".repeat(depth),
            ")".repeat(depth)
        );
        // Cursor right after the final dot; must return (gracefully empty) not crash.
        let tree = tree(&src);
        let docs = [OpenDoc {
            source: &src,
            tree: &tree,
        }];
        let index = LineIndex::new(&src, PositionEncoding::Utf16);
        let at = src.rfind('.').unwrap() + 1;
        let _ = completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items;
    }

    // --- External (JDK/dependency) symbol resolution, via a mock SymbolSource ---

    fn complete_ext(src: &str, marker: &str, symbols: &dyn SymbolSource) -> Vec<CompletionItem> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present") + marker.len();
        completion(&docs, 0, &index, index.position(at), true, symbols).items
    }

    fn mock(entries: Vec<(&str, ExternalClass)>) -> MockSymbols {
        MockSymbols(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    fn ext_class(supers: &[&str], members: Vec<ExternalMember>) -> ExternalClass {
        ExternalClass {
            supers: supers.iter().map(|s| s.to_string()).collect(),
            type_params: Vec::new(),
            members,
        }
    }

    fn ext_generic_class(
        type_params: &[&str],
        supers: &[&str],
        members: Vec<ExternalMember>,
    ) -> ExternalClass {
        ExternalClass {
            supers: supers.iter().map(|s| s.to_string()).collect(),
            type_params: type_params.iter().map(|s| s.to_string()).collect(),
            members,
        }
    }

    #[test]
    fn external_member_completion_via_explicit_import() {
        let src = "import java.util.List;\nclass C { void m() { List xs; xs.x; } }\n";
        let symbols = mock(vec![(
            "java.util.List",
            ext_class(
                &[],
                vec![
                    ext_method("add", "boolean add(Object)"),
                    ext_method("get", "Object get(int)"),
                    ext_method("size", "int size()"),
                ],
            ),
        )]);
        let items = complete_ext(src, "xs.", &symbols);
        assert!(has(&items, "add"), "{:?}", labels(&items));
        assert!(has(&items, "get"));
        assert!(has(&items, "size"));
        assert_eq!(detail_of(&items, "size"), Some("int size()"));
    }

    #[test]
    fn external_completion_via_wildcard_import() {
        let src = "import java.util.*;\nclass C { void m() { Map xs; xs.x; } }\n";
        let symbols = mock(vec![(
            "java.util.Map",
            ext_class(&[], vec![ext_method("put", "Object put(Object, Object)")]),
        )]);
        assert!(has(&complete_ext(src, "xs.", &symbols), "put"));
    }

    #[test]
    fn external_completion_via_implicit_java_lang() {
        let src = "class C { void m() { String s; s.x; } }\n";
        let symbols = mock(vec![(
            "java.lang.String",
            ext_class(&[], vec![ext_method("length", "int length()")]),
        )]);
        assert!(has(&complete_ext(src, "s.", &symbols), "length"));
    }

    #[test]
    fn inproject_extending_external_inherits_members() {
        let src = "import java.util.ArrayList;\nclass MyList extends ArrayList { void m() { this.x; } }\n";
        let symbols = mock(vec![
            (
                "java.util.ArrayList",
                ext_class(
                    &["java.lang.Object"],
                    vec![ext_method("add", "boolean add(Object)")],
                ),
            ),
            (
                "java.lang.Object",
                ext_class(&[], vec![ext_method("toString", "String toString()")]),
            ),
        ]);
        let items = complete_ext(src, "this.", &symbols);
        assert!(
            has(&items, "add"),
            "inherited external: {:?}",
            labels(&items)
        );
        assert!(has(&items, "toString"), "via Object: {:?}", labels(&items));
    }

    #[test]
    fn external_receiver_without_symbols_is_empty() {
        let src = "import java.util.List;\nclass C { void m() { List xs; xs.x; } }\n";
        let items = complete_ext(src, "xs.", &NoSymbols);
        assert!(items.is_empty(), "{:?}", labels(&items));
    }

    // --- M6.3: completion docs are strictly lazy (data payload, not eager) ---

    #[test]
    fn inproject_member_completion_carries_no_eager_documentation() {
        let src = "class Box {\n\
                   /** The width. */\n\
                   int width;\n\
                   }\n\
                   class C { void m() { Box b; b.x; } }\n";
        let items = complete(src, "b.");
        let width = items.iter().find(|i| i.label == "width").unwrap();
        assert!(
            width.documentation.is_none(),
            "completion must not eagerly fetch Javadoc: {:?}",
            width.documentation
        );
        let data = width.data.as_ref().expect("lazy-resolve data payload");
        // The payload names the declaring document by slice index (the
        // server translates it into a URI before the item goes on the wire).
        assert_eq!(data["doc"], 0, "{data:?}");
        assert_eq!(data["type"], "Box", "{data:?}");
        assert_eq!(data["member"], "width", "{data:?}");
    }

    /// M6.3 fix round 1: the `"doc"` index names the *declaring* document —
    /// for a member declared in another open file, that file's index, not
    /// the completion request's current document.
    #[test]
    fn inproject_data_doc_index_names_the_declaring_document() {
        let lib = "class Widget { /** spins */ int spin; }\n";
        let use_src = "class C { void m() { Widget w; w.x; } }\n";
        let lib_tree = tree(lib);
        let use_tree = tree(use_src);
        let docs = [
            OpenDoc {
                source: use_src,
                tree: &use_tree,
            },
            OpenDoc {
                source: lib,
                tree: &lib_tree,
            },
        ];
        let index = LineIndex::new(use_src, PositionEncoding::Utf16);
        let at = use_src.find("w.").unwrap() + 2;
        let items = completion(&docs, 0, &index, index.position(at), true, &NoSymbols).items;
        let spin = items.iter().find(|i| i.label == "spin").unwrap();
        let data = spin.data.as_ref().expect("lazy-resolve data payload");
        assert_eq!(data["doc"], 1, "declaring doc is docs[1]: {data:?}");
    }

    #[test]
    fn external_member_completion_carries_no_documentation_but_has_data_when_receiver_external() {
        let src = "import java.util.List;\nclass C { void m() { List xs; xs.x; } }\n";
        let symbols = mock(vec![(
            "java.util.List",
            ext_class(&[], vec![ext_method("size", "int size()")]),
        )]);
        let items = complete_ext(src, "xs.", &symbols);
        let size = items.iter().find(|i| i.label == "size").unwrap();
        assert!(size.documentation.is_none(), "{:?}", size.documentation);
        assert!(size.data.is_some(), "expected a lazy-resolve data payload");
    }

    #[test]
    fn external_member_reached_through_inproject_receiver_has_no_lazy_data() {
        // Same limitation hover already accepts (see `hover::member_target`):
        // an external member inherited through an *in-project* receiver has
        // no FQN on hand at the point the item is built, so it gets no lazy
        // doc key at all (rather than a broken one).
        let src = "import java.util.ArrayList;\nclass MyList extends ArrayList { void m() { this.x; } }\n";
        let symbols = mock(vec![(
            "java.util.ArrayList",
            ext_class(&[], vec![ext_method("add", "boolean add(Object)")]),
        )]);
        let items = complete_ext(src, "this.", &symbols);
        let add = items.iter().find(|i| i.label == "add").unwrap();
        assert!(add.data.is_none(), "{:?}", add.data);
    }

    #[test]
    fn resolve_documentation_finds_inproject_member_javadoc() {
        let src = "class Box {\n\
                   /** The width. */\n\
                   int width;\n\
                   }\n";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let data = serde_json::json!({"kind": "inproject", "type": "Box", "member": "width"});
        let doc = resolve_documentation(&docs, &data, &NoSymbols).expect("doc resolved");
        match doc {
            Documentation::MarkupContent(m) => assert_eq!(m.value, "The width."),
            other => panic!("expected markup, got {other:?}"),
        }
    }

    /// M6.3 fix round 1: with two files both declaring a `Box.width`, the
    /// caller narrows `docs` to the originating document — and gets *that*
    /// document's Javadoc, not whichever same-named type an all-docs scan
    /// would have found first. An empty slice (originating document closed
    /// since completion) yields no documentation rather than a guess.
    #[test]
    fn resolve_documentation_scoped_to_originating_document_only() {
        let src_a = "class Box { /** From A. */ int width; }\n";
        let src_b = "class Box { /** From B. */ int width; }\n";
        let tree_a = tree(src_a);
        let tree_b = tree(src_b);
        let data = serde_json::json!({
            "kind": "inproject", "type": "Box", "member": "width", "doc": 0,
        });

        let from = |src, t| {
            let docs = [OpenDoc {
                source: src,
                tree: t,
            }];
            resolve_documentation(&docs, &data, &NoSymbols)
        };
        match from(src_a, &tree_a).expect("doc resolved") {
            Documentation::MarkupContent(m) => assert_eq!(m.value, "From A."),
            other => panic!("expected markup, got {other:?}"),
        }
        match from(src_b, &tree_b).expect("doc resolved") {
            Documentation::MarkupContent(m) => assert_eq!(m.value, "From B."),
            other => panic!("expected markup, got {other:?}"),
        }
        // Originating document gone: no documentation, never a guess.
        assert!(resolve_documentation(&[], &data, &NoSymbols).is_none());
    }

    #[test]
    fn resolve_documentation_finds_external_member_javadoc() {
        let symbols = mock(vec![]);
        struct DocStub;
        impl SymbolSource for DocStub {
            fn class(&self, _fqn: &str) -> Option<ExternalClass> {
                None
            }
            fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
                (fqn == "java.util.List" && member == Some("size"))
                    .then(|| "Returns the size.".to_string())
            }
        }
        let _ = symbols; // unused; DocStub carries the fixture instead
        let src = "";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let data =
            serde_json::json!({"kind": "external", "fqn": "java.util.List", "member": "size"});
        let doc = resolve_documentation(&docs, &data, &DocStub).expect("doc resolved");
        match doc {
            Documentation::MarkupContent(m) => assert_eq!(m.value, "Returns the size."),
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn resolve_documentation_missing_or_unknown_data_is_none() {
        let src = "class Box { int width; }\n";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        assert!(resolve_documentation(&docs, &serde_json::json!({}), &NoSymbols).is_none());
        assert!(resolve_documentation(
            &docs,
            &serde_json::json!({"kind": "bogus", "member": "x"}),
            &NoSymbols
        )
        .is_none());
        assert!(resolve_documentation(
            &docs,
            &serde_json::json!({"kind": "inproject", "type": "NoSuchType", "member": "width"}),
            &NoSymbols
        )
        .is_none());
    }

    // --- M7: everyday IntelliSense — chains, statics, var, casts, arrays ---

    /// A method whose (erased) return type is an object — enough for a chain
    /// to continue through `ret_fqn`.
    fn ext_method_ret(name: &str, signature: &str, ret_fqn: &str) -> ExternalMember {
        ExternalMember {
            ret_fqn: Some(ret_fqn.to_string()),
            ..ext_method(name, signature)
        }
    }

    /// A generic method carrying both chain fields (`ret_display` in `{i}`
    /// template form).
    fn ext_method_ret_display(
        name: &str,
        signature: &str,
        ret_fqn: &str,
        ret_display: &str,
    ) -> ExternalMember {
        ExternalMember {
            ret_fqn: Some(ret_fqn.to_string()),
            ret_display: Some(ret_display.to_string()),
            ..ext_method(name, signature)
        }
    }

    fn ext_static_field_ret(name: &str, signature: &str, ret_fqn: &str) -> ExternalMember {
        ExternalMember {
            name: name.to_string(),
            kind: ExternalMemberKind::Field,
            signature: signature.to_string(),
            template: None,
            is_static: true,
            ret_fqn: Some(ret_fqn.to_string()),
            ret_display: None,
        }
    }

    /// A JDK-shaped fixture: List/Stream/String/System/PrintStream/Map.Entry
    /// with just enough members to exercise every chain shape.
    fn rich_mock() -> MockSymbols {
        mock(vec![
            (
                "java.util.List",
                ext_generic_class(
                    &["E"],
                    &[],
                    vec![
                        ext_generic_method("add", "boolean add(Object)", "boolean add({0})"),
                        ExternalMember {
                            ret_fqn: Some("java.lang.Object".to_string()),
                            ret_display: Some("{0}".to_string()),
                            ..ext_generic_method("get", "Object get(int)", "{0} get(int)")
                        },
                        ext_method_ret_display(
                            "stream",
                            "Stream stream()",
                            "java.util.stream.Stream",
                            "Stream<{0}>",
                        ),
                        ExternalMember {
                            is_static: true,
                            ..ext_method_ret_display("of", "List of()", "java.util.List", "List<E>")
                        },
                    ],
                ),
            ),
            (
                "java.util.ArrayList",
                ext_generic_class(
                    &["E"],
                    &[],
                    vec![ext_generic_method(
                        "add",
                        "boolean add(Object)",
                        "boolean add({0})",
                    )],
                ),
            ),
            (
                "java.util.stream.Stream",
                ext_generic_class(
                    &["T"],
                    &[],
                    vec![
                        ext_method("count", "long count()"),
                        ext_method_ret_display(
                            "filter",
                            "Stream filter(Predicate)",
                            "java.util.stream.Stream",
                            "Stream<{0}>",
                        ),
                    ],
                ),
            ),
            (
                "java.lang.String",
                ext_class(
                    &[],
                    vec![
                        ext_method("length", "int length()"),
                        ext_method_ret("trim", "String trim()", "java.lang.String"),
                    ],
                ),
            ),
            (
                "java.lang.System",
                ext_class(
                    &[],
                    vec![ext_static_field_ret(
                        "out",
                        "PrintStream out",
                        "java.io.PrintStream",
                    )],
                ),
            ),
            (
                "java.io.PrintStream",
                ext_class(&[], vec![ext_method("println", "void println(String)")]),
            ),
            (
                "java.util.Map",
                ext_class(&[], vec![ext_method("put", "Object put(Object, Object)")]),
            ),
            (
                "java.util.Map$Entry",
                ext_class(
                    &[],
                    vec![ExternalMember {
                        is_static: true,
                        ..ext_method("comparingByKey", "Comparator comparingByKey()")
                    }],
                ),
            ),
            (
                "java.lang.Object",
                ext_class(&[], vec![ext_method("toString", "String toString()")]),
            ),
        ])
    }

    #[test]
    fn chained_method_call_on_external_receiver() {
        let src = "import java.util.List;\n\
                   class C { void m() { List<String> xs; xs.stream().x; } }\n";
        let items = complete_ext(src, "xs.stream().", &rich_mock());
        assert!(has(&items, "count"), "{:?}", labels(&items));
        assert!(has(&items, "filter"), "{:?}", labels(&items));
    }

    #[test]
    fn chained_call_substitutes_type_var_return() {
        // List<String>.get(int) returns {0} = String — the chain must land on
        // java.lang.String via the implicit java.lang resolution.
        let src = "import java.util.List;\n\
                   class C { void m() { List<String> xs; xs.get(0).x; } }\n";
        let items = complete_ext(src, "xs.get(0).", &rich_mock());
        assert!(has(&items, "length"), "{:?}", labels(&items));
    }

    #[test]
    fn chain_through_erased_return_without_signature() {
        let src = "class C { void m() { String s; s.trim().x; } }\n";
        let items = complete_ext(src, "s.trim().", &rich_mock());
        assert!(has(&items, "length"), "{:?}", labels(&items));
    }

    #[test]
    fn system_out_member_completion() {
        let src = "class C { void m() { System.out.x; } }\n";
        let items = complete_ext(src, "System.out.", &rich_mock());
        assert!(has(&items, "println"), "{:?}", labels(&items));
    }

    #[test]
    fn static_method_chain_on_type_receiver() {
        let src = "import java.util.List;\n\
                   class C { void m() { List.of().x; } }\n";
        let items = complete_ext(src, "List.of().", &rich_mock());
        assert!(has(&items, "add"), "{:?}", labels(&items));
        assert!(has(&items, "stream"), "{:?}", labels(&items));
    }

    #[test]
    fn in_project_method_return_chains() {
        let src = "class Foo { int leaf; Foo self() { return this; } }\n\
                   class C { void m() { Foo f; f.self().x; } }\n";
        let items = complete(src, "f.self().");
        assert!(has(&items, "leaf"), "{:?}", labels(&items));
    }

    #[test]
    fn unqualified_call_in_own_class_chains() {
        let src = "class Foo { int leaf; }\n\
                   class C { Foo make() { return null; } void m() { make().x; } }\n";
        let items = complete(src, "make().");
        assert!(has(&items, "leaf"), "{:?}", labels(&items));
    }

    #[test]
    fn var_infers_from_object_creation_initializer() {
        let src = "import java.util.ArrayList;\n\
                   class C { void m() { var v = new ArrayList<String>(); v.x; } }\n";
        let items = complete_ext(src, "v.", &rich_mock());
        assert!(has(&items, "add"), "{:?}", labels(&items));
        // Generic substitution flows through the inferred type.
        assert_eq!(detail_of(&items, "add"), Some("boolean add(String)"));
    }

    #[test]
    fn var_infers_from_chained_initializer() {
        let src = "class C { void m() { String s; var t = s.trim(); t.x; } }\n";
        let items = complete_ext(src, "t.", &rich_mock());
        assert!(has(&items, "length"), "{:?}", labels(&items));
    }

    #[test]
    fn cast_receiver_resolves_to_cast_type() {
        let src = "import java.util.List;\n\
                   class C { void m(Object o) { ((List) o).x; } }\n";
        let items = complete_ext(src, "o).", &rich_mock());
        assert!(has(&items, "add"), "{:?}", labels(&items));
    }

    #[test]
    fn array_receiver_offers_length_and_clone_not_element_members() {
        let src = "class C { void m(String[] a) { a.x; } }\n";
        let items = complete_ext(src, "a.", &rich_mock());
        assert!(has(&items, "length"), "{:?}", labels(&items));
        assert!(has(&items, "clone"), "{:?}", labels(&items));
        assert!(
            !has(&items, "trim"),
            "element members must not leak: {:?}",
            labels(&items)
        );
        assert!(
            has(&items, "toString"),
            "arrays are Objects: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn nested_class_static_walk() {
        let src = "import java.util.Map;\n\
                   class C { void m() { Map.Entry.x; } }\n";
        let items = complete_ext(src, "Map.Entry.", &rich_mock());
        assert!(has(&items, "comparingByKey"), "{:?}", labels(&items));
    }

    #[test]
    fn fully_qualified_type_receiver_stays_static_only() {
        let src = "class C { void m() { java.util.List.x; } }\n";
        let items = complete_ext(src, "java.util.List.", &rich_mock());
        assert!(has(&items, "of"), "{:?}", labels(&items));
        assert!(
            !has(&items, "add"),
            "instance members excluded on a type receiver: {:?}",
            labels(&items)
        );
    }

    // --- M7: classpath type names, auto-import, import-path completion ---

    use crate::external::TypeCandidate;

    fn cand(simple: &str, fqn: &str, import_path: &str) -> TypeCandidate {
        TypeCandidate {
            simple: simple.to_string(),
            fqn: fqn.to_string(),
            import_path: import_path.to_string(),
        }
    }

    /// A `SymbolSource` with a name index: candidate types (prefix-filtered
    /// like the real index) and a package tree, plus optional classes.
    struct NameSymbols {
        classes: HashMap<String, ExternalClass>,
        candidates: Vec<TypeCandidate>,
        truncated: bool,
        packages: HashMap<String, (Vec<String>, Vec<TypeCandidate>)>,
    }

    impl NameSymbols {
        fn of_candidates(candidates: Vec<TypeCandidate>) -> NameSymbols {
            NameSymbols {
                classes: HashMap::new(),
                candidates,
                truncated: false,
                packages: HashMap::new(),
            }
        }
    }

    impl SymbolSource for NameSymbols {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            self.classes.get(fqn).map(|c| ExternalClass {
                supers: c.supers.clone(),
                type_params: c.type_params.clone(),
                members: c
                    .members
                    .iter()
                    .map(|m| ExternalMember {
                        name: m.name.clone(),
                        kind: m.kind,
                        signature: m.signature.clone(),
                        template: m.template.clone(),
                        is_static: m.is_static,
                        ret_fqn: m.ret_fqn.clone(),
                        ret_display: m.ret_display.clone(),
                    })
                    .collect(),
            })
        }

        fn types_with_prefix(&self, prefix: &str, limit: usize) -> (Vec<TypeCandidate>, bool) {
            let hits: Vec<TypeCandidate> = self
                .candidates
                .iter()
                .filter(|c| {
                    c.simple.len() >= prefix.len()
                        && c.simple[..prefix.len()].eq_ignore_ascii_case(prefix)
                })
                .cloned()
                .collect();
            let over = hits.len() > limit;
            (
                hits.into_iter().take(limit).collect(),
                self.truncated || over,
            )
        }

        fn package_children(&self, package: &str) -> (Vec<String>, Vec<TypeCandidate>) {
            self.packages.get(package).cloned().unwrap_or_default()
        }
    }

    /// Full-result variant of [`complete_ext`], for `is_incomplete` and
    /// edit assertions.
    fn complete_full(src: &str, marker: &str, symbols: &dyn SymbolSource) -> CompletionResult {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        let at = src.find(marker).expect("marker present") + marker.len();
        completion(&docs, 0, &index, index.position(at), true, symbols)
    }

    fn arraylist_symbols() -> NameSymbols {
        NameSymbols::of_candidates(vec![cand(
            "ArrayList",
            "java.util.ArrayList",
            "java.util.ArrayList",
        )])
    }

    fn find_type<'a>(items: &'a [CompletionItem], detail: &str) -> Option<&'a CompletionItem> {
        items.iter().find(|i| i.detail.as_deref() == Some(detail))
    }

    #[test]
    fn classpath_type_completion_with_auto_import_after_last_import() {
        let src = "package demo;\n\nimport java.util.List;\n\nclass C { void m() { ArrayLi } }\n";
        let result = complete_full(src, "ArrayLi", &arraylist_symbols());
        let item = find_type(&result.items, "java.util.ArrayList").expect("candidate offered");
        assert_eq!(item.label, "ArrayList");
        assert_eq!(item.sort_text.as_deref(), Some("3ArrayList"));
        let edits = item.additional_text_edits.as_ref().expect("auto-import");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "\nimport java.util.ArrayList;");
        // Right after `import java.util.List;` — line 2 (0-based), col 23.
        assert_eq!(edits[0].range.start.line, 2);
        assert_eq!(edits[0].range.start.character, 22); // after `import java.util.List;`
        assert_eq!(edits[0].range.start, edits[0].range.end);
        assert_eq!(
            item.data,
            Some(json!({ "kind": "external_type", "fqn": "java.util.ArrayList" }))
        );
        // Deliberately always incomplete once a classpath/project query ran
        // at all — see the doc comment on `scope_items`'s type-name branch:
        // this forces the client to re-query fresh on every keystroke
        // rather than client-side-filtering a stale response, which is what
        // let a real match get buried/dropped in practice.
        assert!(result.is_incomplete);
    }

    #[test]
    fn auto_import_lands_after_package_or_at_file_top() {
        // No imports: insert after the package declaration.
        let src = "package demo;\nclass C { void m() { ArrayLi } }\n";
        let result = complete_full(src, "ArrayLi", &arraylist_symbols());
        let item = find_type(&result.items, "java.util.ArrayList").unwrap();
        let edit = &item.additional_text_edits.as_ref().unwrap()[0];
        assert_eq!(edit.new_text, "\n\nimport java.util.ArrayList;");
        assert_eq!(edit.range.start.line, 0);
        assert_eq!(edit.range.start.character, 13); // after `package demo;`

        // No package either: insert at the very top.
        let src = "class C { void m() { ArrayLi } }\n";
        let result = complete_full(src, "ArrayLi", &arraylist_symbols());
        let item = find_type(&result.items, "java.util.ArrayList").unwrap();
        let edit = &item.additional_text_edits.as_ref().unwrap()[0];
        assert_eq!(edit.new_text, "import java.util.ArrayList;\n\n");
        assert_eq!(edit.range.start.line, 0);
        assert_eq!(edit.range.start.character, 0);
    }

    #[test]
    fn classpath_types_require_min_prefix() {
        let src = "class C { void m() { A } }\n";
        let result = complete_full(src, "{ A", &arraylist_symbols());
        assert!(
            find_type(&result.items, "java.util.ArrayList").is_none(),
            "1-char prefix must not query the classpath"
        );
        assert!(
            !result.is_incomplete,
            "no classpath query ran, so nothing forces a re-query"
        );
    }

    #[test]
    fn no_auto_import_when_already_usable() {
        // Already single-imported.
        let src = "import java.util.ArrayList;\nclass C { void m() { ArrayLi } }\n";
        let item_edits = |src: &str, symbols: &dyn SymbolSource| {
            let result = complete_full(src, "{ ArrayLi", symbols);
            let item = find_type(&result.items, "java.util.ArrayList")
                .unwrap_or_else(|| panic!("candidate offered for {src:?}"))
                .clone();
            item.additional_text_edits
        };
        assert_eq!(item_edits(src, &arraylist_symbols()), None);

        // Wildcard-covered.
        let src = "import java.util.*;\nclass C { void m() { ArrayLi } }\n";
        assert_eq!(item_edits(src, &arraylist_symbols()), None);

        // Same package.
        let symbols =
            NameSymbols::of_candidates(vec![cand("Widget", "demo.Widget", "demo.Widget")]);
        let src = "package demo;\nclass C { void m() { Widg } }\n";
        let result = complete_full(src, "Widg", &symbols);
        let item = find_type(&result.items, "demo.Widget").expect("same-package candidate");
        assert_eq!(item.additional_text_edits, None);

        // java.lang.
        let symbols = NameSymbols::of_candidates(vec![cand(
            "String",
            "java.lang.String",
            "java.lang.String",
        )]);
        let src = "class C { void m() { Stri } }\n";
        let result = complete_full(src, "Stri", &symbols);
        let item = find_type(&result.items, "java.lang.String").expect("java.lang candidate");
        assert_eq!(item.additional_text_edits, None);
    }

    #[test]
    fn conflicting_single_import_hides_the_candidate() {
        let src = "import other.ArrayList;\nclass C { void m() { ArrayLi } }\n";
        let result = complete_full(src, "{ ArrayLi", &arraylist_symbols());
        assert!(
            find_type(&result.items, "java.util.ArrayList").is_none(),
            "a same-simple-name import to a different type makes the candidate unusable"
        );
    }

    #[test]
    fn open_document_type_shadows_classpath_candidate() {
        let src = "class ArrayList {}\nclass C { void m() { ArrayLi } }\n";
        let result = complete_full(src, "{ ArrayLi", &arraylist_symbols());
        let hits: Vec<_> = result
            .items
            .iter()
            .filter(|i| i.label == "ArrayList")
            .collect();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            hits[0].sort_text.as_deref(),
            Some("2ArrayList"),
            "the in-project declaration wins"
        );
        assert_eq!(hits[0].additional_text_edits, None);
    }

    #[test]
    fn result_is_incomplete_whether_or_not_the_candidate_set_was_truncated() {
        // Not truncated...
        let result = complete_full(
            "class C { void m() { ArrayLi } }\n",
            "ArrayLi",
            &arraylist_symbols(),
        );
        assert!(result.is_incomplete);

        // ...and truncated: both force a re-query, for the same reason.
        let mut symbols = arraylist_symbols();
        symbols.truncated = true;
        let result = complete_full("class C { void m() { ArrayLi } }\n", "ArrayLi", &symbols);
        assert!(result.is_incomplete);
    }

    #[test]
    fn nested_candidate_always_carries_its_import() {
        // Even under `import java.util.*`, the *inner* simple name `Entry`
        // needs `import java.util.Map.Entry;`.
        let symbols = NameSymbols::of_candidates(vec![cand(
            "Entry",
            "java.util.Map$Entry",
            "java.util.Map.Entry",
        )]);
        let src = "import java.util.*;\nclass C { void m() { Entr } }\n";
        let result = complete_full(src, "Entr", &symbols);
        let item = find_type(&result.items, "java.util.Map.Entry").expect("nested candidate");
        let edits = item.additional_text_edits.as_ref().expect("nested import");
        assert_eq!(edits[0].new_text, "\nimport java.util.Map.Entry;");
    }

    #[test]
    fn scope_items_rank_in_stable_buckets() {
        let src = "class C { int field; void m(int param) { int local = 1; ZZZ } }\n";
        let items = complete(src, "ZZZ");
        let sort_of = |label: &str| {
            items
                .iter()
                .find(|i| i.label == label)
                .and_then(|i| i.sort_text.clone())
                .unwrap_or_else(|| panic!("{label} present"))
        };
        assert_eq!(sort_of("local"), "0local");
        assert_eq!(sort_of("field"), "1field");
        assert_eq!(sort_of("C"), "2C");
        assert_eq!(sort_of("return"), "4return");
    }

    // --- M7: import-path completion ---

    fn import_symbols() -> NameSymbols {
        let mut packages = HashMap::new();
        packages.insert("".to_string(), (vec!["java".to_string()], Vec::new()));
        packages.insert("java".to_string(), (vec!["util".to_string()], Vec::new()));
        packages.insert(
            "java.util".to_string(),
            (
                vec!["stream".to_string()],
                vec![
                    cand("ArrayList", "java.util.ArrayList", "java.util.ArrayList"),
                    cand("Map", "java.util.Map", "java.util.Map"),
                    cand("Entry", "java.util.Map$Entry", "java.util.Map.Entry"),
                ],
            ),
        );
        let mut classes = HashMap::new();
        classes.insert(
            "java.util.Map".to_string(),
            ext_class(
                &[],
                vec![
                    ExternalMember {
                        is_static: true,
                        ..ext_method("of", "Map of()")
                    },
                    ext_method("put", "Object put(Object, Object)"),
                ],
            ),
        );
        NameSymbols {
            classes,
            candidates: Vec::new(),
            truncated: false,
            packages,
        }
    }

    #[test]
    fn import_completion_walks_packages_and_types() {
        let symbols = import_symbols();
        let items = complete_ext("import ja\n", "import ja", &symbols);
        assert!(has(&items, "java"), "{:?}", labels(&items));

        let items = complete_ext("import java.ut\n", "import java.ut", &symbols);
        assert!(has(&items, "util"), "{:?}", labels(&items));

        let items = complete_ext("import java.util.\n", "import java.util.", &symbols);
        assert!(has(&items, "stream"), "{:?}", labels(&items));
        assert!(has(&items, "ArrayList"), "{:?}", labels(&items));

        // Prefix filters both kinds.
        let items = complete_ext("import java.util.A\n", "import java.util.A", &symbols);
        assert!(has(&items, "ArrayList"));
        assert!(!has(&items, "stream"));

        // Keywords/locals never leak into an import path.
        assert!(!has(
            &complete_ext("import java.util.\n", "import java.util.", &symbols),
            "return"
        ));
    }

    #[test]
    fn import_completion_walks_nested_types_and_static_members() {
        let symbols = import_symbols();
        // After a class segment: its nested types.
        let items = complete_ext("import java.util.Map.\n", "import java.util.Map.", &symbols);
        assert!(has(&items, "Entry"), "{:?}", labels(&items));

        // `import static` after a class: static members only.
        let items = complete_ext(
            "import static java.util.Map.\n",
            "import static java.util.Map.",
            &symbols,
        );
        assert!(has(&items, "of"), "{:?}", labels(&items));
        assert!(!has(&items, "put"), "instance member: {:?}", labels(&items));
        assert!(
            has(&items, "Entry"),
            "nested types stay: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn import_completion_offers_the_static_keyword() {
        let items = complete_ext("import st\n", "import st", &import_symbols());
        assert!(has(&items, "static"), "{:?}", labels(&items));
    }

    #[test]
    fn resolve_documentation_finds_external_type_javadoc() {
        struct TypeDocStub;
        impl SymbolSource for TypeDocStub {
            fn class(&self, _fqn: &str) -> Option<ExternalClass> {
                None
            }
            fn doc(&self, fqn: &str, member: Option<&str>) -> Option<String> {
                (fqn == "java.util.List" && member.is_none())
                    .then(|| "An ordered collection.".to_string())
            }
        }
        let src = "";
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let data = serde_json::json!({"kind": "external_type", "fqn": "java.util.List"});
        let doc = resolve_documentation(&docs, &data, &TypeDocStub).expect("type doc resolved");
        match doc {
            Documentation::MarkupContent(m) => assert_eq!(m.value, "An ordered collection."),
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn generic_type_args_substituted_in_member_signatures() {
        let src = "import java.util.ArrayList;\n\
                   class C { void m() { ArrayList<String> xs; xs.x; } }\n";
        let symbols = mock(vec![(
            "java.util.ArrayList",
            ext_generic_class(
                &["E"],
                &[],
                vec![
                    ext_generic_method("add", "boolean add(Object)", "boolean add({0})"),
                    ext_generic_method("get", "Object get(int)", "{0} get(int)"),
                ],
            ),
        )]);
        let items = complete_ext(src, "xs.", &symbols);
        assert_eq!(detail_of(&items, "add"), Some("boolean add(String)"));
        assert_eq!(detail_of(&items, "get"), Some("String get(int)"));
    }

    #[test]
    fn raw_type_without_args_keeps_erased_signature() {
        let src = "import java.util.ArrayList;\n\
                   class C { void m() { ArrayList xs; xs.x; } }\n";
        let symbols = mock(vec![(
            "java.util.ArrayList",
            ext_generic_class(
                &["E"],
                &[],
                vec![ext_generic_method(
                    "add",
                    "boolean add(Object)",
                    "boolean add({0})",
                )],
            ),
        )]);
        // No type args at the use site -> erased signature.
        assert_eq!(
            detail_of(&complete_ext(src, "xs.", &symbols), "add"),
            Some("boolean add(Object)")
        );
    }
}
