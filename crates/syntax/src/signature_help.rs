//! LSP signature help: at a call site, list the overloads of the invoked
//! method/constructor and highlight the parameter the cursor is in. Reuses
//! hover/completion's substrate (bindings, [`TypeTable`], the external
//! [`SymbolSource`] seam, and `resolve::collect_members`'s hierarchy walk) —
//! the only new machinery here is finding the *nearest enclosing*
//! `argument_list` and counting its direct `,` tokens to place the cursor
//! among the parameters.
//!
//! Active-signature heuristic (documented, not "correct" overload
//! resolution — matching argument *types* is out of scope; see the task
//! report): the first overload whose arity exceeds the active parameter
//! index, else the first overload. Cheap and right far more often than not,
//! since arity alone disambiguates the common case (different overloads take
//! different numbers of arguments).

use ls_types::{
    Documentation, ParameterInformation, ParameterLabel, Position, SignatureHelp,
    SignatureInformation,
};
use tree_sitter::{Node, Tree};

use crate::external::{ExternalMemberKind, SymbolSource};
use crate::imports::Imports;
use crate::model::{base_type_name, MemberKind, TypeTable};
use crate::resolve::{self, Ctx, HierMember, Resolved, ResolvedType};
use crate::signature;
use crate::{node_text, LineIndex, OpenDoc};

/// Build signature help for the call at the cursor. `None` if the cursor
/// isn't inside any `argument_list`, or the enclosing call doesn't resolve to
/// any overload (e.g. an external `new` — see [`constructor_overloads`]).
pub fn signature_help(
    docs: &[OpenDoc],
    current: usize,
    index: &LineIndex,
    pos: Position,
    symbols: &dyn SymbolSource,
) -> Option<SignatureHelp> {
    let doc = docs.get(current)?;
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

    let arg_list = enclosing_argument_list(doc.tree, cursor)?;
    let active_parameter = comma_count_before(arg_list, cursor);
    let call = arg_list.parent()?;

    let signatures = match call.kind() {
        "method_invocation" => method_overloads(call, &ctx)?,
        "object_creation_expression" => constructor_overloads(call, &ctx)?,
        _ => return None,
    };
    if signatures.is_empty() {
        return None;
    }

    // Cheap heuristic (see module docs): first overload whose arity beats the
    // active-parameter index, else the first.
    let active_signature = signatures
        .iter()
        .position(|s| arity(s) > active_parameter)
        .unwrap_or(0);

    Some(SignatureHelp {
        signatures,
        active_signature: Some(active_signature as u32),
        active_parameter: Some(active_parameter),
    })
}

fn arity(sig: &SignatureInformation) -> u32 {
    sig.parameters.as_ref().map_or(0, |p| p.len() as u32)
}

/// The nearest `argument_list` enclosing `cursor` — an ancestor walk up from
/// the smallest node there, so a nested call (`outer(inner(x, |), y)`) finds
/// `inner`'s argument list, not `outer`'s.
fn enclosing_argument_list(tree: &Tree, cursor: usize) -> Option<Node<'_>> {
    let mut node = Some(resolve::node_at(tree, cursor));
    while let Some(n) = node {
        if n.kind() == "argument_list" {
            return Some(n);
        }
        node = n.parent();
    }
    None
}

/// Count of `,` tokens that are *direct children* of `arg_list` and start
/// before `cursor`. Direct children only — a comma inside a nested call's own
/// argument list, a string literal, or a comment isn't a child of *this*
/// `argument_list` node, so it's never miscounted; no text scanning involved.
fn comma_count_before(arg_list: Node, cursor: usize) -> u32 {
    let mut walker = arg_list.walk();
    arg_list
        .children(&mut walker)
        .filter(|c| c.kind() == "," && c.start_byte() < cursor)
        .count() as u32
}

/// Overloads of a method call: `recv.method(...)` resolves the receiver's
/// type the same way hover does (`resolve_receiver_type`); an unqualified
/// `method(...)` resolves against the enclosing type. Every member sharing
/// the call's name and a `Method` kind is an overload candidate —
/// `resolve::collect_members` already walks the full in-project/external
/// hierarchy and dedups by rendered signature, so this just filters its
/// output by name instead of taking the first match (unlike hover's
/// `find_member_hier`, signature help wants *every* overload, not one).
fn method_overloads<'t>(call: Node<'t>, ctx: &Ctx<'_, 't>) -> Option<Vec<SignatureInformation>> {
    let name_node = call.child_by_field_name("name")?;
    let name = node_text(name_node, ctx.doc.source);
    let resolved = match call.child_by_field_name("object") {
        Some(object) => resolve::resolve_receiver_type(object, ctx)?,
        None => Resolved {
            ty: ResolvedType::InProject(resolve::enclosing_typedecl(
                call,
                ctx.doc.source,
                ctx.current,
            )?),
            static_only: false,
        },
    };
    let signatures = resolve::collect_members(&resolved, ctx)
        .into_iter()
        .filter(|m| m.name() == name && is_method(m))
        .filter_map(|m| member_signature(&m))
        .collect();
    Some(signatures)
}

fn is_method(member: &HierMember) -> bool {
    match member {
        HierMember::InProject(m) => matches!(m.kind, MemberKind::Method),
        HierMember::External(m) => matches!(m.kind, ExternalMemberKind::Method),
    }
}

fn member_signature(member: &HierMember) -> Option<SignatureInformation> {
    match member {
        HierMember::InProject(m) => {
            let (label, offsets) = signature::signature_with_param_offsets(m.node, m.source)?;
            let doc = signature::javadoc(m.node, m.source);
            Some(build_signature(label, offsets, doc))
        }
        HierMember::External(m) => {
            let offsets = external_param_offsets(&m.signature);
            Some(build_signature(m.signature.clone(), offsets, None))
        }
    }
}

/// Overloads of a `new Type(...)` call: the created type's declared
/// constructors. In-project only — the external `SymbolSource` seam has no
/// notion of a constructor (`ExternalMemberKind` is `Method`/`Field` only), so
/// `new` on an external (JDK/dependency) type yields no signature help; see
/// the task report.
fn constructor_overloads<'t>(
    call: Node<'t>,
    ctx: &Ctx<'_, 't>,
) -> Option<Vec<SignatureInformation>> {
    let type_node = call.child_by_field_name("type")?;
    let simple = base_type_name(type_node, ctx.doc.source)?;
    let td = ctx.table.get(simple)?;
    let signatures = td
        .constructors()
        .into_iter()
        .filter_map(|node| {
            let (label, offsets) = signature::signature_with_param_offsets(node, td.source)?;
            let doc = signature::javadoc(node, td.source);
            Some(build_signature(label, offsets, doc))
        })
        .collect();
    Some(signatures)
}

fn build_signature(
    label: String,
    offsets: Vec<[u32; 2]>,
    doc: Option<String>,
) -> SignatureInformation {
    let parameters = (!offsets.is_empty()).then(|| {
        offsets
            .into_iter()
            .map(|o| ParameterInformation {
                label: ParameterLabel::LabelOffsets(o),
                documentation: None,
            })
            .collect()
    });
    SignatureInformation {
        label,
        documentation: doc.map(Documentation::String),
        parameters,
        active_parameter: None,
    }
}

/// Parameter label offsets parsed out of a rendered external-member
/// signature string — no parse-tree node backs an external member, only the
/// display string `SymbolSource` returned, so offsets are recovered by
/// splitting the parenthesized parameter list on top-level commas (depth
/// tracked over `()[]<>` so a generic argument's own comma, e.g.
/// `Map<String, Integer> m`, doesn't split).
fn external_param_offsets(label: &str) -> Vec<[u32; 2]> {
    let bytes = label.as_bytes();
    let Some(open) = bytes.iter().position(|&b| b == b'(') else {
        return Vec::new();
    };
    let mut depth = 0i32;
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return Vec::new();
    };
    let inner = &label[open + 1..close];
    if inner.trim().is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, b) in inner.bytes().enumerate() {
        match b {
            b'(' | b'[' | b'<' => depth += 1,
            b')' | b']' | b'>' => depth -= 1,
            b',' if depth == 0 => {
                spans.push(param_span(inner, start, i, open + 1));
                start = i + 1;
            }
            _ => {}
        }
    }
    spans.push(param_span(inner, start, inner.len(), open + 1));
    spans
}

/// Trim leading/trailing whitespace from `inner[start..end]`, returning the
/// trimmed extent's absolute byte offsets within the outer label (`base` is
/// `inner`'s own byte offset in that label).
fn param_span(inner: &str, start: usize, end: usize, base: usize) -> [u32; 2] {
    let seg = &inner[start..end];
    let lead = seg.len() - seg.trim_start().len();
    let trimmed = seg.trim();
    let a = (base + start + lead) as u32;
    [a, a + trimmed.len() as u32]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind, NoSymbols};
    use crate::{new_parser, parse, PositionEncoding};
    use tree_sitter::Tree;

    /// A `SymbolSource` resolving exactly one class, for external-overload tests.
    struct OneClass {
        fqn: &'static str,
        members: Vec<(&'static str, &'static str)>, // (name, rendered signature)
    }

    impl SymbolSource for OneClass {
        fn class(&self, fqn: &str) -> Option<ExternalClass> {
            (fqn == self.fqn).then(|| ExternalClass {
                supers: Vec::new(),
                type_params: Vec::new(),
                members: self
                    .members
                    .iter()
                    .map(|(name, sig)| ExternalMember {
                        name: name.to_string(),
                        kind: ExternalMemberKind::Method,
                        signature: sig.to_string(),
                        template: None,
                        is_static: false,
                    })
                    .collect(),
            })
        }
    }

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn help_at(src: &str, byte: usize, symbols: &dyn SymbolSource) -> Option<SignatureHelp> {
        let tree = tree(src);
        let docs = [OpenDoc {
            source: src,
            tree: &tree,
        }];
        let index = LineIndex::new(src, PositionEncoding::Utf16);
        signature_help(&docs, 0, &index, index.position(byte), symbols)
    }

    #[test]
    fn unqualified_call_lists_overloads_with_active_parameter() {
        let src = "class C {\n\
                   void helper(int a) {}\n\
                   void helper(int a, int b) {}\n\
                   void m() { helper(1, 2); }\n\
                   }\n";
        let at = src.find("2)").unwrap();
        let help = help_at(src, at, &NoSymbols).expect("signature help");
        assert_eq!(help.signatures.len(), 2, "{help:?}");
        assert!(help.signatures[0].label.contains("helper(int a)"));
        assert!(help.signatures[1].label.contains("helper(int a, int b)"));
        assert_eq!(help.active_parameter, Some(1));
        assert_eq!(help.active_signature, Some(1), "2-arg overload is active");
    }

    #[test]
    fn qualified_call_on_external_receiver_lists_both_overloads() {
        let src = "import test.Widget;\n\
                   class C { void m(Widget w) { w.append(1, 2); } }\n";
        let symbols = OneClass {
            fqn: "test.Widget",
            members: vec![
                ("append", "void append(String s)"),
                ("append", "void append(int i, int j)"),
            ],
        };
        let at = src.find("append(").unwrap() + "append(".len();
        let help = help_at(src, at, &symbols).expect("signature help");
        assert_eq!(help.signatures.len(), 2, "{help:?}");
        assert_eq!(help.active_parameter, Some(0));
    }

    #[test]
    fn nested_call_uses_nearest_argument_list() {
        let src = "class C {\n\
                   void outer(int a, int b) {}\n\
                   int inner(int p, int q) { return 0; }\n\
                   void m(int x, int y) { outer(inner(x, 5), y); }\n\
                   }\n";
        let at = src.find("5)").unwrap();
        let help = help_at(src, at, &NoSymbols).expect("signature help");
        assert_eq!(help.signatures.len(), 1, "{help:?}");
        assert!(
            help.signatures[0].label.contains("inner(int p, int q)"),
            "{help:?}"
        );
        assert_eq!(
            help.active_parameter,
            Some(1),
            "nearest (inner) argument_list wins"
        );
    }

    #[test]
    fn comma_inside_string_literal_does_not_bump_active_parameter() {
        let src = "class C {\n\
                   void helper(String a, int b) {}\n\
                   void m() { helper(\"a,b\", 5); }\n\
                   }\n";
        let at = src.find("5)").unwrap();
        let help = help_at(src, at, &NoSymbols).expect("signature help");
        assert_eq!(help.signatures.len(), 1, "{help:?}");
        assert_eq!(
            help.active_parameter,
            Some(1),
            "only the real comma counts, not the one inside the string"
        );
    }

    #[test]
    fn constructor_call_resolves_declared_constructor() {
        let src = "class Foo {\n  Foo(int a, int b) {}\n}\n\
                   class C { void m() { Foo f = new Foo(1, 2); } }\n";
        let at = src.find("new Foo(").unwrap() + "new Foo(".len();
        let help = help_at(src, at, &NoSymbols).expect("signature help");
        assert_eq!(help.signatures.len(), 1, "{help:?}");
        assert!(help.signatures[0].label.contains("Foo(int a, int b)"));
        assert_eq!(help.active_parameter, Some(0));
    }

    #[test]
    fn no_enclosing_argument_list_is_none() {
        let src = "class C { void m() { int x = 1; } }\n";
        let at = src.find("x = 1").unwrap();
        assert!(help_at(src, at, &NoSymbols).is_none());
    }
}
