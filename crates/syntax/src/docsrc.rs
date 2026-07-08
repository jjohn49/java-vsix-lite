//! Extract Javadoc for a type or member from external `.java` source text
//! (recovered from a JDK `src.zip` / dependency `-sources.jar`). Reuses the same
//! tree-sitter parser and Javadoc extractor as in-document hover.

use tree_sitter::Node;

use crate::model::named_children;
use crate::signature::javadoc;
use crate::{new_parser, node_text, parse};

/// Parse external source and return the Javadoc for the type named `type_simple`
/// (when `member` is `None`) or for its named method/field.
pub fn javadoc_in_source(source: &str, type_simple: &str, member: Option<&str>) -> Option<String> {
    let mut parser = new_parser();
    let tree = parse(&mut parser, source, None)?;
    let type_node = find_type(tree.root_node(), source, type_simple)?;
    match member {
        None => javadoc(type_node, source),
        Some(name) => javadoc(find_member_decl(type_node, source, name)?, source),
    }
}

fn is_type_decl(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

/// The declaration of the type named `simple`, anywhere in the tree.
fn find_type<'t>(root: Node<'t>, source: &str, simple: &str) -> Option<Node<'t>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_type_decl(node.kind())
            && node
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                == Some(simple)
        {
            return Some(node);
        }
        stack.extend(named_children(node));
    }
    None
}

/// The method/field declaration named `name` directly in a type's body.
fn find_member_decl<'t>(type_node: Node<'t>, source: &str, name: &str) -> Option<Node<'t>> {
    let body = named_children(type_node)
        .into_iter()
        .find(|c| c.kind().ends_with("_body"))?;
    for child in named_children(body) {
        match child.kind() {
            "method_declaration" => {
                if named_field(child, source) == Some(name) {
                    return Some(child);
                }
            }
            "field_declaration" | "constant_declaration" => {
                for declarator in named_children(child) {
                    if declarator.kind() == "variable_declarator"
                        && named_field(declarator, source) == Some(name)
                    {
                        return Some(declarator);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn named_field<'a>(node: Node, source: &'a str) -> Option<&'a str> {
    node.child_by_field_name("name")
        .map(|n| node_text(n, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "package p;\n\
        /** The list type. */\n\
        public interface MyList {\n\
        \x20 /** Adds an element. */\n\
        \x20 boolean add(Object e);\n\
        \x20 /** The size field. */\n\
        \x20 int size = 0;\n\
        }\n";

    #[test]
    fn extracts_member_javadoc() {
        assert_eq!(
            javadoc_in_source(SRC, "MyList", Some("add")).as_deref(),
            Some("Adds an element.")
        );
        assert_eq!(
            javadoc_in_source(SRC, "MyList", Some("size")).as_deref(),
            Some("The size field.")
        );
    }

    #[test]
    fn extracts_type_javadoc() {
        assert_eq!(
            javadoc_in_source(SRC, "MyList", None).as_deref(),
            Some("The list type.")
        );
    }

    #[test]
    fn missing_member_or_type_is_none() {
        assert_eq!(javadoc_in_source(SRC, "MyList", Some("nope")), None);
        assert_eq!(javadoc_in_source(SRC, "Other", None), None);
    }
}
