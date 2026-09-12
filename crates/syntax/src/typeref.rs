//! Lower declared-type parse-tree nodes to structured [`TypeRef`]s.
//!
//! Shared by [`crate::srcclass`] and `resolve.rs`, which use the same
//! JLS type-node-to-`TypeRef` algorithm but differ in how a bare/dotted
//! base name maps to a binary name.

use tree_sitter::Node;

use jvl_types::{Access, PrimitiveType, TypeId, TypeParameter, TypeRef, TypeVariableId};

use crate::model::{has_modifier, named_children};
use crate::node_text;

/// Lower a declared-type node to a structured `TypeRef`.
/// `resolve_named` maps a bare/dotted base name to a binary name; `vars`
/// lists type parameters in scope, innermost last.
pub(crate) fn lower_type_node(
    node: Node,
    source: &str,
    vars: &[(String, TypeVariableId)],
    resolve_named: &dyn Fn(&str, bool) -> Option<String>,
) -> TypeRef {
    // Recovery nodes carry no trustworthy type: any diagnostic built on
    // them would be a guess, so the whole type is Unknown.
    if node.is_error() || node.is_missing() || node.has_error() {
        return TypeRef::Unknown;
    }
    let text = node_text(node, source).trim();
    if text == "void" {
        return TypeRef::Void;
    }
    if let Some(p) = PrimitiveType::from_name(text) {
        return TypeRef::Primitive(p);
    }
    match node.kind() {
        "array_type" => {
            let elem = node
                .child_by_field_name("element")
                .map(|e| lower_type_node(e, source, vars, resolve_named))
                .unwrap_or(TypeRef::Unknown);
            let dims = node
                .child_by_field_name("dimensions")
                .map(|d| node_text(d, source).matches('[').count())
                .unwrap_or(1);
            (0..dims).fold(elem, |t, _| TypeRef::Array(Box::new(t)))
        }
        "annotated_type" => named_children(node)
            .into_iter()
            .find(|c| !matches!(c.kind(), "annotation" | "marker_annotation"))
            .map(|c| lower_type_node(c, source, vars, resolve_named))
            .unwrap_or(TypeRef::Unknown),
        "generic_type" => {
            let base = named_children(node)
                .into_iter()
                .find(|c| c.kind() != "type_arguments");
            let args = named_children(node)
                .into_iter()
                .find(|c| c.kind() == "type_arguments")
                .map(|ta| {
                    named_children(ta)
                        .into_iter()
                        .filter(|a| !matches!(a.kind(), "annotation" | "marker_annotation"))
                        .map(|a| lower_type_node(a, source, vars, resolve_named))
                        .collect()
                })
                .unwrap_or_default();
            match base.map(|b| lower_type_node(b, source, vars, resolve_named)) {
                Some(TypeRef::Named { id, .. }) => TypeRef::Named { id, args },
                _ => TypeRef::Unknown,
            }
        }
        "wildcard" => {
            let bound = named_children(node)
                .into_iter()
                .find(|c| !matches!(c.kind(), "annotation" | "marker_annotation" | "super"))
                .map(|b| Box::new(lower_type_node(b, source, vars, resolve_named)));
            // `super` has its own node kind here, unlike `extends` (an
            // anonymous token) — check kind, not text.
            let is_super = named_children(node)
                .into_iter()
                .any(|c| c.kind() == "super");
            if is_super {
                TypeRef::Wildcard {
                    upper: None,
                    lower: bound,
                }
            } else {
                TypeRef::Wildcard {
                    upper: bound,
                    lower: None,
                }
            }
        }
        "type_identifier" | "scoped_type_identifier" => {
            if node.kind() == "type_identifier" {
                if let Some((_, id)) = vars.iter().rev().find(|(n, _)| n == text) {
                    return TypeRef::Variable(id.clone());
                }
            }
            match resolve_named(text, node.kind() == "scoped_type_identifier") {
                Some(b) => TypeRef::Named {
                    id: TypeId::Named(b),
                    args: vec![],
                },
                None => TypeRef::Unknown,
            }
        }
        _ => TypeRef::Unknown,
    }
}

/// `type_parameters` node -> (names+ids in scope INCLUDING `outer_vars`,
/// structured params) owned by `owner`.
pub(crate) fn lower_type_parameters(
    node: Option<Node>,
    source: &str,
    owner: &str,
    outer_vars: &[(String, TypeVariableId)],
    resolve_named: &dyn Fn(&str, bool) -> Option<String>,
) -> (Vec<(String, TypeVariableId)>, Vec<TypeParameter>) {
    let mut names = outer_vars.to_vec();
    let mut params = Vec::new();
    let Some(node) = node else {
        return (names, params);
    };
    for (index, tp) in named_children(node)
        .into_iter()
        .filter(|c| c.kind() == "type_parameter")
        .enumerate()
    {
        let name = named_children(tp)
            .into_iter()
            .find(|c| c.kind() == "type_identifier")
            .map(|n| node_text(n, source).to_string())
            .unwrap_or_default();
        let id = TypeVariableId {
            owner: owner.to_string(),
            index,
        };
        names.push((name, id.clone()));
        let bounds = named_children(tp)
            .into_iter()
            .find(|c| c.kind() == "type_bound")
            .map(|b| {
                named_children(b)
                    .into_iter()
                    .map(|t| lower_type_node(t, source, &names, resolve_named))
                    .collect()
            })
            .unwrap_or_default();
        params.push(TypeParameter { id, bounds });
    }
    (names, params)
}

/// Modifiers -> access, falling back to `default` when no access modifier
/// is present.
pub(crate) fn access_of(decl: Node, source: &str, default: Access) -> Access {
    if has_modifier(decl, source, "public") {
        Access::Public
    } else if has_modifier(decl, source, "protected") {
        Access::Protected
    } else if has_modifier(decl, source, "private") {
        Access::Private
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse};

    fn tree(src: &str) -> tree_sitter::Tree {
        parse(&mut new_parser(), src, None).expect("parse")
    }

    /// The `type` field of the first `local_variable_declaration`.
    fn first_local_type<'t>(tree: &'t tree_sitter::Tree, src: &str) -> tree_sitter::Node<'t> {
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if n.kind() == "local_variable_declaration" {
                return n.child_by_field_name("type").expect("type field");
            }
            let mut c = n.walk();
            stack.extend(n.children(&mut c));
        }
        panic!("no local_variable_declaration in {src}");
    }

    /// Every type-shaped node (generic/array/scoped/simple) that tree-sitter
    /// marked as containing an error, anywhere in the tree.
    fn errored_type_nodes<'t>(tree: &'t tree_sitter::Tree) -> Vec<tree_sitter::Node<'t>> {
        let mut out = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if matches!(
                n.kind(),
                "generic_type" | "array_type" | "scoped_type_identifier" | "type_identifier"
            ) && n.has_error()
            {
                out.push(n);
            }
            let mut c = n.walk();
            stack.extend(n.children(&mut c));
        }
        out
    }

    /// Recovery shapes differ per grammar version, so this asserts the
    /// contract over several malformed inputs at once: every type node
    /// carrying an error lowers to Unknown, and at least one such node
    /// exists across the corpus (otherwise the test proves nothing).
    #[test]
    fn errored_type_nodes_lower_to_unknown() {
        let corpus = [
            "class C { java.util.List< x; }",
            "class C { List<String x; }",
            "class C { String[ x; }",
            "class C { Map<String, > x; }",
            "class C { void m() { List< x = null; } }",
        ];
        let resolved = |name: &str, _dotted: bool| Some(format!("java.util.{name}"));
        let mut seen = 0;
        for src in corpus {
            let tree = tree(src);
            for node in errored_type_nodes(&tree) {
                seen += 1;
                assert_eq!(
                    lower_type_node(node, src, &[], &resolved),
                    TypeRef::Unknown,
                    "{src}: {}",
                    node.to_sexp()
                );
            }
        }
        assert!(
            seen > 0,
            "corpus produced no errored type node; add a malformed input"
        );
    }

    #[test]
    fn well_formed_generic_type_still_lowers() {
        let src = "class C { void m() { List<String> x = null; } }";
        let tree = tree(src);
        let ty = first_local_type(&tree, src);
        let resolved = |name: &str, _dotted: bool| match name {
            "List" => Some("java.util.List".to_string()),
            "String" => Some("java.lang.String".to_string()),
            _ => None,
        };
        assert_eq!(
            lower_type_node(ty, src, &[], &resolved),
            TypeRef::named_with("java.util.List", vec![TypeRef::named("java.lang.String")])
        );
    }
}
