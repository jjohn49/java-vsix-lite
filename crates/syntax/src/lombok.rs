//! Lombok awareness — synthesize the members Lombok's annotation
//! processor would generate (`@Getter`/`@Setter`/`@Data`/`@Value`/`@With`/
//! `@Builder` accessors, fluent builders) so a project class using Lombok
//! still completes, chains, and passes the unresolved-member check, even
//! though its source declares none of those methods.
//!
//! Scope is deliberate: only the members that affect *callers* (getters,
//! setters, withers, `builder()` and the builder type's fluent API).
//! Constructors (`@AllArgsConstructor` et al.) are out — the member model
//! never lists constructors anyway — and `toString`/`equals`/`hashCode`
//! already arrive via `java.lang.Object`. Synthesis is gated on the
//! declaring **file** importing `lombok.*`: a homemade `@Getter` annotation
//! never conjures phantom members.

use tree_sitter::Node;

use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind};
use crate::model::{has_modifier, modifiers_node, named_children, MemberKind, TypeDecl};
use crate::node_text;

/// A member Lombok would generate. Always a method; `ret_display` is the
/// source-level type text (resolved later against the declaring file's
/// context), `ret_fqn` is set only where synthesis itself knows the binary
/// name (the builder type).
pub(crate) struct SyntheticMember {
    pub name: String,
    pub signature: String,
    pub is_static: bool,
    pub ret_display: Option<String>,
    pub ret_fqn: Option<String>,
}

impl SyntheticMember {
    pub(crate) fn into_external(self) -> ExternalMember {
        ExternalMember {
            name: self.name,
            kind: ExternalMemberKind::Method,
            signature: self.signature,
            template: None,
            is_static: self.is_static,
            ret_fqn: self.ret_fqn,
            ret_display: self.ret_display,
        }
    }
}

/// Whether the file containing `node` imports anything from `lombok` —
/// the synthesis gate.
pub(crate) fn file_uses_lombok(node: Node, source: &str) -> bool {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    named_children(root).into_iter().any(|c| {
        c.kind() == "import_declaration"
            && crate::imports::dotted_path(node_text(c, source), "import")
                // `dotted_path` collapses all whitespace, so a static import
                // arrives as `staticlombok.Getter` — peel the keyword off.
                .map(|p| p.strip_prefix("static").unwrap_or(&p).to_string())
                .is_some_and(|p| p == "lombok" || p.starts_with("lombok."))
    })
}

/// The declaring file's `package`, read from `node`'s own tree (which may be
/// a *different* document than the one resolution started from).
pub(crate) fn file_package(node: Node, source: &str) -> Option<String> {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    named_children(root)
        .into_iter()
        .find(|c| c.kind() == "package_declaration")
        .and_then(|c| crate::imports::dotted_path(node_text(c, source), "package"))
}

/// The binary FQN of a (possibly nested) type declaration node:
/// `pkg.Outer$Inner`. `None` when any enclosing declaration is anonymous.
pub(crate) fn binary_fqn_of(class_node: Node, source: &str) -> Option<String> {
    let mut names: Vec<&str> = Vec::new();
    let mut n = Some(class_node);
    while let Some(node) = n {
        if crate::model::TypeKind::from_kind(node.kind()).is_some() {
            names.push(node_text(node.child_by_field_name("name")?, source));
        }
        n = node.parent();
    }
    names.reverse();
    let path = names.join("$");
    if path.is_empty() {
        return None;
    }
    Some(match file_package(class_node, source) {
        Some(pkg) => format!("{pkg}.{path}"),
        None => path,
    })
}

/// Lombok annotations relevant to member synthesis, gathered from one
/// declaration's `modifiers`.
#[derive(Default, Clone, Copy)]
struct Marks {
    getter: bool,
    setter: bool,
    data: bool,
    value: bool,
    builder: bool,
    with: bool,
}

fn marks_on(decl: Node, source: &str) -> Marks {
    let mut marks = Marks::default();
    let Some(mods) = modifiers_node(decl) else {
        return marks;
    };
    for child in crate::model::named_children(mods) {
        if !matches!(child.kind(), "annotation" | "marker_annotation") {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let name = node_text(name_node, source);
        let simple = name.rsplit('.').next().unwrap_or(name);
        // `@Getter(AccessLevel.NONE)` (and PRIVATE/PROTECTED, which callers
        // outside the class can't see either) suppresses generation for our
        // purposes — only public accessors are modeled, matching the
        // bytecode layer's visibility gate.
        if let Some(args) = child.child_by_field_name("arguments") {
            let args = node_text(args, source);
            if ["NONE", "PRIVATE", "PROTECTED", "PACKAGE", "MODULE"]
                .iter()
                .any(|lvl| args.contains(lvl))
            {
                continue;
            }
        }
        match simple {
            "Getter" => marks.getter = true,
            "Setter" => marks.setter = true,
            "Data" => marks.data = true,
            "Value" => marks.value = true,
            "Builder" => marks.builder = true,
            "With" | "Wither" => marks.with = true,
            _ => {}
        }
    }
    marks
}

/// One instance field eligible for synthesis.
struct FieldInfo<'t> {
    name: &'t str,
    type_text: &'t str,
    is_final: bool,
    marks: Marks,
}

/// The non-static fields of a class, with their declared-type text and
/// field-level Lombok marks. Record components are skipped — records get
/// their accessors from the language, not Lombok.
fn instance_fields<'t>(td: &TypeDecl<'t>) -> Vec<FieldInfo<'t>> {
    td.own_members()
        .into_iter()
        .filter(|m| {
            matches!(m.kind, MemberKind::Field)
                && !m.is_static
                && m.node.kind() == "variable_declarator"
        })
        .filter_map(|m| {
            let decl = m.node.parent()?; // field_declaration
            let ty = decl.child_by_field_name("type")?;
            Some(FieldInfo {
                name: m.name,
                type_text: node_text(ty, td.source),
                is_final: has_modifier(decl, td.source, "final"),
                marks: marks_on(decl, td.source),
            })
        })
        .collect()
}

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Lombok's getter name: `getFoo`, except `boolean` fields get `isFoo` —
/// and a `boolean` field already *named* `isFoo` keeps its own name.
fn getter_name(field: &FieldInfo) -> String {
    if field.type_text == "boolean" {
        let mut rest = field.name.strip_prefix("is").unwrap_or("").chars();
        if rest.next().is_some_and(|c| c.is_ascii_uppercase()) {
            return field.name.to_string();
        }
        format!("is{}", capitalized(field.name))
    } else {
        format!("get{}", capitalized(field.name))
    }
}

/// The members Lombok would generate for `class_node` (a class declaration
/// in a file that imports `lombok.*` — callers check [`file_uses_lombok`]
/// first). Methods the class already declares by the same name are never
/// duplicated, mirroring Lombok's own skip-if-present rule.
pub(crate) fn synthesize(class_node: Node, source: &str) -> Vec<SyntheticMember> {
    let Some(td) = TypeDecl::from_node(class_node, source, 0) else {
        return Vec::new();
    };
    if td.kind != crate::model::TypeKind::Class {
        return Vec::new();
    }
    let class_marks = marks_on(class_node, source);
    let existing: std::collections::HashSet<&str> = td
        .own_members()
        .into_iter()
        .filter(|m| matches!(m.kind, MemberKind::Method))
        .map(|m| m.name)
        .collect();

    let mut out = Vec::new();
    let mut push = |m: SyntheticMember| {
        if !existing.contains(m.name.as_str()) {
            out.push(m);
        }
    };

    for field in instance_fields(&td) {
        let want_getter =
            class_marks.getter || class_marks.data || class_marks.value || field.marks.getter;
        // `@Value` makes every field final; final fields never get setters.
        let want_setter = (class_marks.setter || class_marks.data || field.marks.setter)
            && !field.is_final
            && !class_marks.value;
        let want_wither = class_marks.with || field.marks.with;

        if want_getter {
            let name = getter_name(&field);
            push(SyntheticMember {
                signature: format!("{} {name}()", field.type_text),
                name,
                is_static: false,
                ret_display: Some(field.type_text.to_string()),
                ret_fqn: None,
            });
        }
        if want_setter {
            let name = format!("set{}", capitalized(field.name));
            push(SyntheticMember {
                signature: format!("void {name}({} {})", field.type_text, field.name),
                name,
                is_static: false,
                ret_display: Some("void".to_string()),
                ret_fqn: None,
            });
        }
        if want_wither {
            let name = format!("with{}", capitalized(field.name));
            push(SyntheticMember {
                signature: format!("{} {name}({} {})", td.name, field.type_text, field.name),
                name,
                is_static: false,
                ret_display: Some(td.name.to_string()),
                ret_fqn: None,
            });
        }
    }

    if class_marks.builder {
        let builder_simple = format!("{}Builder", td.name);
        push(SyntheticMember {
            name: "builder".to_string(),
            signature: format!("{}.{builder_simple} builder()", td.name),
            is_static: true,
            ret_display: Some(builder_simple.clone()),
            ret_fqn: binary_fqn_of(class_node, source).map(|f| format!("{f}${builder_simple}")),
        });
    }
    out
}

/// The synthesized `@Builder` companion class for `outer` (an
/// `ExternalClass` for `Outer$OuterBuilder`): one fluent setter per instance
/// field returning the builder itself, plus `build()` returning the outer
/// type. `None` when `outer` isn't `@Builder`-annotated.
pub(crate) fn builder_class(outer: &TypeDecl, source: &str) -> Option<ExternalClass> {
    if !marks_on(outer.node, source).builder {
        return None;
    }
    let outer_fqn = binary_fqn_of(outer.node, source)?;
    let builder_fqn = format!("{outer_fqn}${}Builder", outer.name);
    let builder_simple = format!("{}Builder", outer.name);

    let mut members: Vec<ExternalMember> = instance_fields(outer)
        .into_iter()
        .map(|field| ExternalMember {
            name: field.name.to_string(),
            kind: ExternalMemberKind::Method,
            signature: format!(
                "{builder_simple} {}({} {})",
                field.name, field.type_text, field.name
            ),
            template: None,
            is_static: false,
            ret_fqn: Some(builder_fqn.clone()),
            ret_display: Some(builder_simple.clone()),
        })
        .collect();
    members.push(ExternalMember {
        name: "build".to_string(),
        kind: ExternalMemberKind::Method,
        signature: format!("{} build()", outer.name),
        template: None,
        is_static: false,
        ret_fqn: Some(outer_fqn),
        ret_display: Some(outer.name.to_string()),
    });

    Some(ExternalClass {
        supers: vec!["java.lang.Object".to_string()],
        type_params: Vec::new(),
        members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{new_parser, parse};

    fn synth(src: &str) -> Vec<SyntheticMember> {
        let tree = parse(&mut new_parser(), src, None).expect("parse");
        let class = crate::model::named_children(tree.root_node())
            .into_iter()
            .find(|n| n.kind() == "class_declaration")
            .expect("a class");
        assert!(file_uses_lombok(class, src), "fixture must import lombok");
        synthesize(class, src)
    }

    fn names(members: &[SyntheticMember]) -> Vec<&str> {
        members.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn class_level_getter_setter_cover_all_instance_fields() {
        let src = "package demo;\nimport lombok.Getter;\nimport lombok.Setter;\n\
                   @Getter @Setter public class Person {\n\
                   private String name;\n\
                   private boolean active;\n\
                   private static int COUNT;\n\
                   }\n";
        let members = synth(src);
        let got = names(&members);
        assert!(got.contains(&"getName"), "{got:?}");
        assert!(got.contains(&"setName"), "{got:?}");
        assert!(got.contains(&"isActive"), "boolean getter: {got:?}");
        assert!(got.contains(&"setActive"), "{got:?}");
        assert!(!got.contains(&"getCOUNT"), "static excluded: {got:?}");
        let get_name = members.iter().find(|m| m.name == "getName").unwrap();
        assert_eq!(get_name.signature, "String getName()");
        assert_eq!(get_name.ret_display.as_deref(), Some("String"));
        let set_name = members.iter().find(|m| m.name == "setName").unwrap();
        assert_eq!(set_name.signature, "void setName(String name)");
    }

    #[test]
    fn data_skips_setters_for_final_fields_and_value_is_all_getters() {
        let src = "import lombok.Data;\n\
                   @Data public class Point {\n\
                   private final int x;\n\
                   private int y;\n\
                   }\n";
        let members = synth(src);
        let got = names(&members);
        assert!(got.contains(&"getX") && got.contains(&"getY"), "{got:?}");
        assert!(!got.contains(&"setX"), "final field: {got:?}");
        assert!(got.contains(&"setY"), "{got:?}");

        let src = "import lombok.Value;\n\
                   @Value public class Point {\n\
                   int x;\n\
                   }\n";
        let members = synth(src);
        let got = names(&members);
        assert!(got.contains(&"getX"), "{got:?}");
        assert!(!got.contains(&"setX"), "@Value is immutable: {got:?}");
    }

    #[test]
    fn field_level_marks_and_existing_methods_respected() {
        let src = "import lombok.Getter;\nimport lombok.With;\n\
                   public class Config {\n\
                   @Getter private String url;\n\
                   @With private int retries;\n\
                   private int hidden;\n\
                   public String getUrl() { return url; }\n\
                   }\n";
        let members = synth(src);
        let got = names(&members);
        // The class already declares getUrl — never duplicated.
        assert!(!got.contains(&"getUrl"), "{got:?}");
        assert!(got.contains(&"withRetries"), "{got:?}");
        assert!(!got.contains(&"getHidden"), "unannotated: {got:?}");
        let wither = members.iter().find(|m| m.name == "withRetries").unwrap();
        assert_eq!(wither.signature, "Config withRetries(int retries)");
        assert_eq!(wither.ret_display.as_deref(), Some("Config"));
    }

    #[test]
    fn access_level_none_suppresses_and_boolean_is_prefix_kept() {
        let src = "import lombok.*;\n\
                   public class Flag {\n\
                   @Getter(AccessLevel.NONE) private int internal;\n\
                   @Getter private boolean isReady;\n\
                   }\n";
        let members = synth(src);
        let got = names(&members);
        assert!(!got.contains(&"getInternal"), "{got:?}");
        assert!(got.contains(&"isReady"), "kept as-is: {got:?}");
        assert!(!got.contains(&"isIsReady"), "{got:?}");
    }

    #[test]
    fn builder_method_and_builder_class() {
        let src = "package demo;\nimport lombok.Builder;\n\
                   @Builder public class Person {\n\
                   private String name;\n\
                   private int age;\n\
                   }\n";
        let members = synth(src);
        let builder = members.iter().find(|m| m.name == "builder").unwrap();
        assert!(builder.is_static);
        assert_eq!(
            builder.ret_fqn.as_deref(),
            Some("demo.Person$PersonBuilder")
        );

        let tree = parse(&mut new_parser(), src, None).unwrap();
        let class = crate::model::named_children(tree.root_node())
            .into_iter()
            .find(|n| n.kind() == "class_declaration")
            .unwrap();
        let td = TypeDecl::from_node(class, src, 0).unwrap();
        let builder_class = builder_class(&td, src).expect("@Builder class");
        let fluent = builder_class
            .members
            .iter()
            .find(|m| m.name == "name")
            .expect("fluent setter");
        assert_eq!(fluent.ret_fqn.as_deref(), Some("demo.Person$PersonBuilder"));
        let build = builder_class
            .members
            .iter()
            .find(|m| m.name == "build")
            .expect("build()");
        assert_eq!(build.ret_fqn.as_deref(), Some("demo.Person"));
        assert_eq!(build.signature, "Person build()");
    }

    #[test]
    fn no_lombok_import_means_no_synthesis_gate() {
        // A homemade @Getter without the lombok import: the gate reports
        // false, and callers must not synthesize.
        let src = "public class NotLombok { @Getter private int x; }\n";
        let tree = parse(&mut new_parser(), src, None).unwrap();
        let class = crate::model::named_children(tree.root_node())
            .into_iter()
            .find(|n| n.kind() == "class_declaration")
            .unwrap();
        assert!(!file_uses_lombok(class, src));
    }
}
