//! Reconstruct human-readable declaration signatures from parse-tree nodes, and
//! extract preceding Javadoc. Shared by hover and completion. Source-text based,
//! so generics render exactly as written (`List<Integer>`), even though the
//! resolver erases them for type lookup.

use tree_sitter::Node;

use crate::model::{children, modifiers_node, named_children};
use crate::node_text;

/// Render a one-line signature for a declaration node, or `None` if the node is
/// not a kind we render.
pub(crate) fn signature(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "method_declaration" | "annotation_type_element_declaration" => {
            Some(method_signature(node, source))
        }
        "constructor_declaration" => Some(constructor_signature(node, source)),
        "variable_declarator" => Some(field_signature(node, source)),
        "formal_parameter" | "spread_parameter" | "catch_formal_parameter" => {
            Some(param_signature(node, source))
        }
        "enhanced_for_statement" => Some(for_var_signature(node, source)),
        "enum_constant" => Some(node_text(node.child_by_field_name("name")?, source).to_string()),
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => Some(type_signature(node, source)),
        _ => None,
    }
}

fn method_signature(node: Node, source: &str) -> String {
    let mut parts = Vec::new();
    if let Some(m) = modifier_keywords(node, source) {
        parts.push(m);
    }
    if let Some(ret) = node.child_by_field_name("type") {
        parts.push(collapse_ws(node_text(ret, source)));
    }
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("");
    let params = parameters(node, source);
    let mut sig = format!("{} {name}({params})", parts.join(" "));
    sig = sig.trim_start().to_string();
    if let Some(throws) = throws_clause(node, source) {
        sig.push_str(&format!(" {throws}"));
    }
    sig
}

fn constructor_signature(node: Node, source: &str) -> String {
    let mut parts = Vec::new();
    if let Some(m) = modifier_keywords(node, source) {
        parts.push(m);
    }
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("");
    let params = parameters(node, source);
    format!("{} {name}({params})", parts.join(" "))
        .trim_start()
        .to_string()
}

/// `variable_declarator` -> `[modifiers ]<type> <name>`, reading the type and
/// modifiers from the enclosing `field_declaration` / `local_variable_declaration`.
fn field_signature(declarator: Node, source: &str) -> String {
    let name = declarator
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("");
    let parent = declarator.parent();
    let mut parts = Vec::new();
    if let Some(p) = parent {
        if let Some(m) = modifier_keywords(p, source) {
            parts.push(m);
        }
        if let Some(ty) = p.child_by_field_name("type") {
            parts.push(collapse_ws(node_text(ty, source)));
        }
    }
    parts.push(name.to_string());
    parts.join(" ").trim_start().to_string()
}

fn param_signature(node: Node, source: &str) -> String {
    // A varargs parameter is `(spread_parameter <type> (variable_declarator
    // name:(identifier)))` — the type is a positional child and the name lives on
    // the nested declarator, not on `name`/`type` fields.
    if node.kind() == "spread_parameter" {
        let ty = named_children(node)
            .into_iter()
            .find(|c| !matches!(c.kind(), "modifiers" | "variable_declarator"))
            .map(|n| collapse_ws(node_text(n, source)))
            .unwrap_or_default();
        let name = spread_param_name(node)
            .map(|n| node_text(n, source))
            .unwrap_or("");
        return format!("{ty}... {name}").trim().to_string();
    }
    let ty = node
        .child_by_field_name("type")
        .map(|n| collapse_ws(node_text(n, source)))
        .unwrap_or_default();
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("");
    format!("{ty} {name}").trim().to_string()
}

/// The name identifier of a `spread_parameter` (on its nested declarator).
pub(crate) fn spread_param_name(node: Node) -> Option<Node> {
    named_children(node)
        .into_iter()
        .find(|c| c.kind() == "variable_declarator")
        .and_then(|d| d.child_by_field_name("name"))
}

fn for_var_signature(node: Node, source: &str) -> String {
    let ty = node
        .child_by_field_name("type")
        .map(|n| collapse_ws(node_text(n, source)))
        .unwrap_or_default();
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("");
    format!("{ty} {name}").trim().to_string()
}

fn type_signature(node: Node, source: &str) -> String {
    let kind = match node.kind() {
        "interface_declaration" => "interface",
        "enum_declaration" => "enum",
        "record_declaration" => "record",
        "annotation_type_declaration" => "@interface",
        _ => "class",
    };
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source))
        .unwrap_or("");
    let mut sig = format!("{kind} {name}");
    for child in named_children(node) {
        match child.kind() {
            "superclass" | "extends_interfaces" => {
                sig.push_str(&format!(" extends {}", clause_types(child, source)));
            }
            "super_interfaces" => {
                sig.push_str(&format!(" implements {}", clause_types(child, source)));
            }
            _ => {}
        }
    }
    sig
}

/// Join the (possibly comma-separated) type names inside an extends/implements
/// clause node.
fn clause_types(clause: Node, source: &str) -> String {
    let mut names = Vec::new();
    for child in named_children(clause) {
        if child.kind() == "type_list" {
            for t in named_children(child) {
                names.push(collapse_ws(node_text(t, source)));
            }
        } else {
            names.push(collapse_ws(node_text(child, source)));
        }
    }
    names.join(", ")
}

/// Render the `formal_parameters` of a method/constructor as `type name, …`.
fn parameters(node: Node, source: &str) -> String {
    param_labels(node, source).join(", ")
}

/// Each parameter's rendered label (`type name`), in declaration order — the
/// same per-parameter rendering [`parameters`] joins into a method/constructor
/// signature. Exposed (rather than folded into `parameters`' `String` return)
/// for signature help, which needs each parameter's own byte span within the
/// full label to drive LSP's per-parameter highlight.
pub(crate) fn param_labels(node: Node, source: &str) -> Vec<String> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    named_children(params)
        .into_iter()
        .filter(|p| {
            matches!(
                p.kind(),
                "formal_parameter" | "spread_parameter" | "receiver_parameter"
            )
        })
        .map(|p| param_signature(p, source))
        .collect()
}

/// A method/constructor declaration's rendered label plus each parameter's
/// `[start, end)` **byte** offsets within that label (the caller converts to
/// whatever code units the wire protocol wants — LSP signature help uses
/// UTF-16), for the per-parameter highlight. `None` if `node` isn't a
/// signature-renderable declaration (see [`signature`]).
///
/// Offsets are found by searching for each parameter's own rendering, in
/// order, starting from just past the previous match — so a parameter list
/// with repeated types/names (`(int a, int a)`) still lines up left-to-right
/// instead of all collapsing onto the first occurrence. The search is
/// anchored inside the label's parameter-list parentheses
/// ([`param_list_span`]), so a parameter whose rendering also occurs in the
/// return-type + method-name prefix (`String name(String name)`) can't match
/// the prefix at byte 0.
pub(crate) fn signature_with_param_offsets(
    node: Node,
    source: &str,
) -> Option<(String, Vec<[u32; 2]>)> {
    let label = signature(node, source)?;
    let params = param_labels(node, source);
    if params.is_empty() {
        return Some((label, Vec::new()));
    }
    let (open, close) = param_list_span(&label)?;
    let mut offsets = Vec::new();
    let mut search_from = open + 1;
    for param in params {
        let idx = label[search_from..close].find(param.as_str())?;
        let start = search_from + idx;
        let end = start + param.len();
        offsets.push([start as u32, end as u32]);
        search_from = end;
    }
    Some((label, offsets))
}

/// Byte indices of the parameter list's `(` and its matching `)` in a
/// rendered signature label — the first `(` (nothing parenthesized precedes
/// the parameter list in any rendering this module produces), closed at
/// depth 0 so a nested paren inside the list can't end the span early.
/// Shared with signature help's external-signature parsing.
pub(crate) fn param_list_span(label: &str) -> Option<(usize, usize)> {
    let bytes = label.as_bytes();
    let open = bytes.iter().position(|&b| b == b'(')?;
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, i));
                }
            }
            _ => {}
        }
    }
    None
}

/// Number of parameters in a rendered signature label's parameter list (the
/// top-level-comma count, plus one; `0` for empty parens or no parens found
/// at all). Used to pick the arity-matching overload of an *external*
/// member — no parse-tree node backs one, only its rendered signature
/// string, so arity is recovered the same way
/// [`signature_help::external_param_offsets`] recovers per-parameter spans:
/// splitting the parenthesized parameter list on depth-0 commas (`()[]<>`
/// tracked so a generic argument's own comma doesn't split).
pub(crate) fn param_count_in_label(label: &str) -> usize {
    let Some((open, close)) = param_list_span(label) else {
        return 0;
    };
    let inner = &label[open + 1..close];
    if inner.trim().is_empty() {
        return 0;
    }
    let mut depth = 0i32;
    let mut count = 1usize;
    for b in inner.bytes() {
        match b {
            b'(' | b'[' | b'<' => depth += 1,
            b')' | b']' | b'>' => depth -= 1,
            b',' if depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

fn throws_clause(node: Node, source: &str) -> Option<String> {
    let throws = named_children(node)
        .into_iter()
        .find(|c| c.kind() == "throws")?;
    let types: Vec<String> = named_children(throws)
        .into_iter()
        .map(|t| collapse_ws(node_text(t, source)))
        .collect();
    Some(format!("throws {}", types.join(", ")))
}

/// M7: the same signature text as [`signature`] but with modifiers dropped —
/// the display convention `jvl-classpath` uses for bytecode-derived members
/// (`int size()`, not `public int size()`), so a project-source-derived
/// `ExternalMember` (closed-file completion/hover) renders identically to a
/// classpath one. Field/record-component/enum-constant/parameter shapes
/// were already modifier-free (see [`field_signature`] et al.), so this
/// only needs its own method/constructor branches; everything else
/// delegates to [`signature`].
pub(crate) fn erased_signature(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "method_declaration" | "annotation_type_element_declaration" => {
            let ret = node
                .child_by_field_name("type")
                .map(|n| collapse_ws(node_text(n, source)))
                .unwrap_or_else(|| "void".to_string());
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or("");
            Some(format!("{ret} {name}({})", parameters(node, source)))
        }
        "constructor_declaration" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or("");
            Some(format!("{name}({})", parameters(node, source)))
        }
        // A field's modifiers live on its *parent* `field_declaration`
        // (mirrors `field_signature`'s own parent lookup) — dropping them
        // means not consulting that parent for anything but its `type`.
        "variable_declarator" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or("");
            let ty = node
                .parent()
                .and_then(|p| p.child_by_field_name("type"))
                .map(|n| collapse_ws(node_text(n, source)))
                .unwrap_or_default();
            Some(format!("{ty} {name}").trim().to_string())
        }
        _ => signature(node, source),
    }
}

/// Space-joined modifier keywords (annotations excluded), or `None` if there are
/// none.
fn modifier_keywords(decl: Node, source: &str) -> Option<String> {
    let m = modifiers_node(decl)?;
    let kws: Vec<&str> = children(m)
        .into_iter()
        .filter(|c| !matches!(c.kind(), "marker_annotation" | "annotation"))
        .map(|c| node_text(c, source))
        .filter(|s| !s.is_empty())
        .collect();
    if kws.is_empty() {
        None
    } else {
        Some(kws.join(" "))
    }
}

/// Collapse runs of ASCII whitespace (incl. newlines) to single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract and clean the Javadoc immediately preceding a declaration node, if it
/// is a `/** … */` block comment. Returns markdown-ready text.
pub(crate) fn javadoc(decl: Node, source: &str) -> Option<String> {
    // The Javadoc attaches to the outermost declaration (e.g. a field's comment
    // precedes `field_declaration`, not `variable_declarator`).
    let target = match decl.kind() {
        "variable_declarator" => decl.parent().unwrap_or(decl),
        _ => decl,
    };
    let mut sib = target.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "line_comment" => sib = s.prev_sibling(),
            "block_comment" => {
                let text = node_text(s, source);
                return text.starts_with("/**").then(|| strip_javadoc(text));
            }
            _ => return None,
        }
    }
    None
}

fn strip_javadoc(raw: &str) -> String {
    let inner = raw
        .strip_prefix("/**")
        .unwrap_or(raw)
        .strip_suffix("*/")
        .unwrap_or(raw);
    inner
        .lines()
        .map(|line| {
            let t = line.trim_start();
            let t = t.strip_prefix('*').unwrap_or(t);
            t.strip_prefix(' ').unwrap_or(t)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse};
    use tree_sitter::Tree;

    fn tree(src: &str) -> Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    fn find_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
        if node.kind() == kind {
            return Some(node);
        }
        for child in children(node) {
            if let Some(found) = find_kind(child, kind) {
                return Some(found);
            }
        }
        None
    }

    fn sig(src: &str, kind: &str) -> String {
        let tree = tree(src);
        let node = find_kind(tree.root_node(), kind).expect("node kind present");
        signature(node, src).expect("signature")
    }

    #[test]
    fn method_signature_with_modifiers_generics_throws() {
        let s = sig(
            "class C { public static List<Integer> f(int a, String b) throws IOException { return null; } }",
            "method_declaration",
        );
        assert_eq!(
            s,
            "public static List<Integer> f(int a, String b) throws IOException"
        );
    }

    #[test]
    fn field_signature_includes_modifiers_and_generics() {
        let s = sig(
            "class C { private final Map<String, Integer> m = null; }",
            "variable_declarator",
        );
        assert_eq!(s, "private final Map<String, Integer> m");
    }

    #[test]
    fn type_signature_with_extends_and_implements() {
        let s = sig("class C extends B implements I, J {}", "class_declaration");
        assert_eq!(s, "class C extends B implements I, J");
    }

    #[test]
    fn constructor_signature_renders_params() {
        let s = sig("class C { C(int a) {} }", "constructor_declaration");
        assert_eq!(s, "C(int a)");
    }

    #[test]
    fn no_arg_method_signature() {
        let s = sig("class C { void run() {} }", "method_declaration");
        assert_eq!(s, "void run()");
    }

    /// Regression: when a parameter's rendering (`String name`) also occurs as
    /// the return-type + method-name prefix of the label
    /// (`String name(String name)`), the offsets must anchor inside the
    /// parameter list's parentheses — not match the prefix at byte 0.
    #[test]
    fn param_offsets_skip_matching_prefix_outside_parens() {
        let src = "class C { String name(String name) { return name; } }";
        let tree = tree(src);
        let node = find_kind(tree.root_node(), "method_declaration").unwrap();
        let (label, offsets) = signature_with_param_offsets(node, src).expect("offsets");
        assert_eq!(label, "String name(String name)");
        assert_eq!(
            offsets,
            vec![[12, 23]],
            "must point at the parameter, not the prefix"
        );
    }

    #[test]
    fn javadoc_strips_margins() {
        let src = "class C {\n  /**\n   * Line one.\n   * Line two.\n   */\n  int x;\n}\n";
        let tree = tree(src);
        let node = find_kind(tree.root_node(), "variable_declarator").unwrap();
        assert_eq!(javadoc(node, src).as_deref(), Some("Line one.\nLine two."));
    }

    #[test]
    fn param_count_in_label_counts_top_level_commas() {
        assert_eq!(param_count_in_label("Foo()"), 0);
        assert_eq!(param_count_in_label("Foo(int a)"), 1);
        assert_eq!(param_count_in_label("Foo(int a, String b)"), 2);
        // A generic argument's own comma must not split the count.
        assert_eq!(param_count_in_label("Foo(Map<String, Integer> m)"), 1);
        assert_eq!(param_count_in_label("no parens here"), 0);
    }

    #[test]
    fn javadoc_absent_when_no_block_comment() {
        let src = "class C { int x; }\n";
        let tree = tree(src);
        let node = find_kind(tree.root_node(), "variable_declarator").unwrap();
        assert_eq!(javadoc(node, src), None);
    }
}
