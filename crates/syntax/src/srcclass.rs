//! Turn a **closed** project `.java` file's source text into an
//! [`ExternalClass`] — the same shape `jvl-classpath` produces from bytecode
//! — so a workspace type the user hasn't opened still gets full member
//! completion, chains, and hover. `jvl-syntax` stays IO-free: the server
//! reads the file and hands this module the text plus the type path inside
//! it (for a nested type); parsing here is pure tree-sitter, no filesystem.

use tree_sitter::Node;

use crate::external::{ExternalClass, ExternalMember, ExternalMemberKind};
use crate::imports::Imports;
use crate::model::{has_modifier, MemberKind, TypeDecl, TypeKind};
use crate::{new_parser, node_text, parse};

/// Parse `source` and extract `type_path` (`"Person"`, or `"Outer.Inner"`
/// for a nested type) as an [`ExternalClass`]. Supertypes and member result
/// types are simple names resolved through the *declaring file's own*
/// imports; `pick_fqn` — supplied by the caller, which owns the rest of the
/// classpath/project — picks the real FQN among the resulting candidates
/// (typically "first one that actually resolves"). `None` if the file
/// doesn't parse or doesn't declare that type.
pub fn class_from_source(
    source: &str,
    type_path: &str,
    pick_fqn: &dyn Fn(&[String]) -> Option<String>,
) -> Option<ExternalClass> {
    let tree = parse(&mut new_parser(), source, None)?;
    let imports = Imports::parse(&tree, source);
    if let Some(td) = find_type_by_path(tree.root_node(), source, type_path) {
        return Some(to_external_class(&td, source, &imports, pick_fqn));
    }
    // `Outer.OuterBuilder` — the `@Builder` companion type that exists
    // only in Lombok's generated code, synthesized on demand so builder
    // chains (`Person.builder().name("x").build()`) resolve.
    let (outer_path, last) = type_path.rsplit_once('.')?;
    let outer = find_type_by_path(tree.root_node(), source, outer_path)?;
    if last != format!("{}Builder", outer.name)
        || !crate::lombok::file_uses_lombok(outer.node, source)
    {
        return None;
    }
    crate::lombok::builder_class(&outer, source)
}

/// Walk from the file's top-level type declarations through `.`-separated
/// nested-type segments (`Outer.Inner.Deepest`) to the named [`TypeDecl`].
fn find_type_by_path<'t>(root: Node<'t>, source: &'t str, type_path: &str) -> Option<TypeDecl<'t>> {
    let mut segments = type_path.split('.');
    let first = segments.next()?;
    let mut current = crate::model::named_children(root)
        .into_iter()
        .find_map(|n| TypeDecl::from_node(n, source, 0).filter(|td| td.name == first))?;
    for seg in segments {
        let member = current
            .own_members()
            .into_iter()
            .find(|m| m.name == seg && matches!(m.kind, MemberKind::NestedType(_)))?;
        current = TypeDecl::from_node(member.node, source, 0)?;
    }
    Some(current)
}

fn to_external_class(
    td: &TypeDecl,
    source: &str,
    imports: &Imports,
    pick_fqn: &dyn Fn(&[String]) -> Option<String>,
) -> ExternalClass {
    let mut supers: Vec<String> = td
        .supers
        .iter()
        .filter_map(|s| pick_fqn(&imports.candidates(s)))
        .collect();
    // An enum implicitly extends `java.lang.Enum` — the source of `name()`,
    // `ordinal()`, `compareTo()`, etc. Without this, every `myEnum.name()`
    // looks unresolved. Its own `supers` hold only explicit interfaces.
    if td.kind == TypeKind::Enum {
        supers.insert(0, "java.lang.Enum".to_string());
    } else if supers.is_empty() {
        supers.push("java.lang.Object".to_string());
    }

    let mut members: Vec<ExternalMember> = td
        .own_members()
        .into_iter()
        .filter(|m| externally_visible(m.node, source, td.kind))
        .filter_map(|m| member_from_node(m.node, m.kind, m.is_static, source, imports, pick_fqn))
        .collect();
    for ctor in td.constructors() {
        if externally_visible(ctor, source, td.kind) {
            if let Some(m) = constructor_member(ctor, td.name, source) {
                members.push(m);
            }
        }
    }

    // Enums carry two compiler-synthesized static methods, absent from source
    // text: `values()` and `valueOf(String)`. Add them so `E.values()` /
    // `E.valueOf(..)` resolve. Instance `name()`/`ordinal()`/… come from
    // the implicit `java.lang.Enum` supertype set above.
    if td.kind == TypeKind::Enum {
        members.push(ExternalMember {
            name: "values".to_string(),
            kind: ExternalMemberKind::Method,
            signature: format!("{}[] values()", td.name),
            template: None,
            is_static: true,
            ret_fqn: None,
            ret_display: Some(format!("{}[]", td.name)),
        });
        members.push(ExternalMember {
            name: "valueOf".to_string(),
            kind: ExternalMemberKind::Method,
            signature: format!("{} valueOf(String)", td.name),
            template: None,
            is_static: true,
            ret_fqn: pick_fqn(&imports.candidates(td.name)),
            ret_display: Some(td.name.to_string()),
        });
    }

    // Lombok-generated accessors, under the same gate as open documents.
    // `synthesize` never duplicates a declared method; the erased return FQN
    // is recovered from the display type through the declaring file's own
    // imports, exactly like `member_from_node` does for real members.
    if crate::lombok::file_uses_lombok(td.node, source) {
        for sm in crate::lombok::synthesize(td.node, source) {
            let mut m = sm.into_external();
            if m.ret_fqn.is_none() {
                m.ret_fqn = m
                    .ret_display
                    .as_deref()
                    .and_then(display_base_simple)
                    .and_then(|simple| pick_fqn(&imports.candidates(simple)));
            }
            members.push(m);
        }
    }

    ExternalClass {
        supers,
        // Project-source scope: type parameters aren't tracked (no
        // Signature-attribute equivalent to parse from source text), so
        // generic members render erased, same as a raw-type classpath use.
        type_params: Vec::new(),
        members,
    }
}

fn member_from_node(
    node: Node,
    kind: MemberKind,
    is_static: bool,
    source: &str,
    imports: &Imports,
    pick_fqn: &dyn Fn(&[String]) -> Option<String>,
) -> Option<ExternalMember> {
    // An enum constant has no declared type or parameters, so
    // `erased_signature` can't render one — handle it directly (its
    // "signature" is just its name) rather than letting the `?` below drop it,
    // which would erase every constant and make `Enum.CONSTANT` look
    // unresolved (a false "Cannot resolve field" on cross-file enums).
    if kind == MemberKind::EnumConstant {
        let name = node_text(node.child_by_field_name("name")?, source).to_string();
        return Some(ExternalMember {
            name: name.clone(),
            kind: ExternalMemberKind::Field,
            signature: name,
            template: None,
            is_static, // enum constants are implicitly static
            ret_fqn: None,
            ret_display: None,
        });
    }
    let ext_kind = match kind {
        MemberKind::Method => ExternalMemberKind::Method,
        MemberKind::Field | MemberKind::EnumConstant => ExternalMemberKind::Field,
        MemberKind::NestedType(_) => return None, // walked via find_type_by_path, not a value
    };
    let signature = crate::signature::erased_signature(node, source)?;
    let name = node_text(node.child_by_field_name("name")?, source).to_string();
    // A field's declared-type node lives on the enclosing `field_declaration`
    // for a regular field (`node` is its `variable_declarator`), but directly
    // on `node` itself for a record component (`node` is the component's own
    // `formal_parameter` — see `TypeDecl::own_members`'s record handling).
    let field_type = || {
        if node.kind() == "formal_parameter" {
            node.child_by_field_name("type")
        } else {
            node.parent()?.child_by_field_name("type")
        }
    };
    let (ret_fqn, ret_display) = match kind {
        MemberKind::Method => node.child_by_field_name("type"),
        MemberKind::Field => field_type(),
        MemberKind::EnumConstant | MemberKind::NestedType(_) => None,
    }
    .map(|ty| {
        (
            resolve_ret_fqn(ty, source, imports, pick_fqn),
            node_text(ty, source).to_string(),
        )
    })
    .map_or((None, None), |(fqn, text)| (fqn, Some(text)));
    Some(ExternalMember {
        name,
        kind: ext_kind,
        signature,
        template: None,
        is_static,
        ret_fqn,
        ret_display,
    })
}

/// Whether a directly-declared member/constructor should be exposed for a
/// **project** source file — everything except an explicitly `private` member.
///
/// This is deliberately looser than `jvl-classpath`'s bytecode gate
/// (public/protected only): a project's own files aren't compiled
/// dependencies, and same-package code legitimately uses their
/// package-private members (a package-private field accessed across two
/// files in the same package was being falsely flagged "Cannot resolve").
/// Erring toward *including* a member only ever makes completion more generous
/// and the unresolved-member diagnostic more conservative — both safe
/// directions. `private` stays hidden (never accessible from another file).
fn externally_visible(node: Node, source: &str, _enclosing_kind: TypeKind) -> bool {
    if matches!(node.kind(), "formal_parameter" | "enum_constant") {
        return true; // record component / enum constant: no modifier slot
    }
    // A field's modifiers live on the enclosing `field_declaration`, not on
    // its own `variable_declarator` node — same parent lookup
    // `field_signature` uses to render them.
    let modifiers_owner = match node.kind() {
        "variable_declarator" => node.parent().unwrap_or(node),
        _ => node,
    };
    !has_modifier(modifiers_owner, source, "private")
}

/// The FQN a declared-type node's *base* name resolves to, through the
/// declaring file's own imports — `None` for primitives, arrays, and
/// anything `pick_fqn` can't place (never surfaced, same graceful
/// degradation as a bytecode member with no `ret_fqn`).
fn resolve_ret_fqn(
    type_node: Node,
    source: &str,
    imports: &Imports,
    pick_fqn: &dyn Fn(&[String]) -> Option<String>,
) -> Option<String> {
    let simple = crate::model::base_type_name(type_node, source)?;
    pick_fqn(&imports.candidates(simple))
}

/// The base simple type name of a rendered display type: `List<String>` →
/// `List`, `demo.Person` → `Person`. `None` for primitives (lowercase
/// first letter) and arrays — neither carries an importable FQN.
fn display_base_simple(display: &str) -> Option<&str> {
    let base = display.split('<').next()?.trim();
    if base.ends_with("[]") {
        return None;
    }
    let simple = base.rsplit('.').next()?;
    simple
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        .then_some(simple)
}

fn constructor_member(node: Node, class_name: &str, source: &str) -> Option<ExternalMember> {
    let params = crate::signature::signature(node, source)?;
    // `signature()` renders a constructor as `ClassName(params)` already
    // (no return type prefix), matching the bytecode-derived convention.
    Some(ExternalMember {
        name: class_name.to_string(),
        kind: ExternalMemberKind::Constructor,
        signature: params,
        template: None,
        is_static: false,
        ret_fqn: None,
        ret_display: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick_first(candidates: &[String]) -> Option<String> {
        candidates.first().cloned()
    }

    /// `pick_fqn` that only "knows about" a fixed set of real types — models
    /// a caller (classpath + workspace index) that discards candidates
    /// nothing on disk/JDK actually resolves to.
    fn known(names: &'static [&'static str]) -> impl Fn(&[String]) -> Option<String> {
        move |candidates: &[String]| {
            candidates
                .iter()
                .find(|c| names.contains(&c.as_str()))
                .cloned()
        }
    }

    #[test]
    fn extracts_fields_and_methods_with_result_types() {
        let src = "package demo;\n\
                   public class Person {\n\
                   public String name;\n\
                   public int getAge() { return 0; }\n\
                   public Person self() { return this; }\n\
                   }\n";
        let class = class_from_source(src, "Person", &pick_first).expect("parses");
        assert_eq!(class.supers, vec!["java.lang.Object".to_string()]);
        let name = class.members.iter().find(|m| m.name == "name").unwrap();
        assert_eq!(name.signature, "String name");
        assert_eq!(name.ret_fqn.as_deref(), Some("demo.String"));
        assert_eq!(name.ret_display.as_deref(), Some("String"));

        let age = class.members.iter().find(|m| m.name == "getAge").unwrap();
        assert_eq!(age.kind, ExternalMemberKind::Method);
        assert_eq!(age.signature, "int getAge()");

        let self_m = class.members.iter().find(|m| m.name == "self").unwrap();
        assert_eq!(self_m.ret_fqn.as_deref(), Some("demo.Person"));
    }

    #[test]
    fn resolves_return_type_through_explicit_import() {
        let src = "package demo;\n\
                   import java.util.List;\n\
                   public class Repo {\n\
                   public List all() { return null; }\n\
                   }\n";
        let class = class_from_source(src, "Repo", &known(&["java.util.List"])).unwrap();
        let all = class.members.iter().find(|m| m.name == "all").unwrap();
        assert_eq!(all.ret_fqn.as_deref(), Some("java.util.List"));
    }

    #[test]
    fn extracts_constructors_and_static_members() {
        let src = "package demo;\n\
                   public class Box {\n\
                   public Box() {}\n\
                   public Box(int n) {}\n\
                   public static int COUNT = 0;\n\
                   private int hidden() { return 0; }\n\
                   }\n";
        let class = class_from_source(src, "Box", &pick_first).unwrap();
        let ctors: Vec<_> = class
            .members
            .iter()
            .filter(|m| m.kind == ExternalMemberKind::Constructor)
            .collect();
        assert_eq!(ctors.len(), 2, "{:?}", class.members);
        assert!(ctors.iter().all(|m| m.name == "Box"));
        let count = class.members.iter().find(|m| m.name == "COUNT").unwrap();
        assert!(count.is_static);
        // Private members are excluded (own_members() already filters).
        assert!(!class.members.iter().any(|m| m.name == "hidden"));
    }

    #[test]
    fn extends_resolves_supertype_through_imports() {
        let src = "package demo;\n\
                   import java.util.ArrayList;\n\
                   public class Widgets extends ArrayList {\n\
                   }\n";
        let class = class_from_source(src, "Widgets", &known(&["java.util.ArrayList"])).unwrap();
        assert_eq!(class.supers, vec!["java.util.ArrayList".to_string()]);
    }

    #[test]
    fn nested_type_path_finds_inner_declaration() {
        let src = "package demo;\n\
                   public class Outer {\n\
                   public static class Inner {\n\
                   public int leaf;\n\
                   }\n\
                   }\n";
        let class = class_from_source(src, "Outer.Inner", &pick_first).unwrap();
        assert!(class.members.iter().any(|m| m.name == "leaf"));
    }

    #[test]
    fn unknown_type_path_is_none_not_panic() {
        let src = "class Person {}\n";
        assert!(class_from_source(src, "NoSuchType", &pick_first).is_none());
        assert!(class_from_source(src, "Person.Missing", &pick_first).is_none());
        assert!(class_from_source("not even java {{{", "Person", &pick_first).is_none());
    }

    /// An enum's constants, its implicit `java.lang.Enum` super, and the
    /// synthesized static `values()`/`valueOf(String)` all surface — so
    /// `E.CONSTANT`, `e.name()`, and `E.values()` resolve on a cross-file enum.
    #[test]
    fn enum_exposes_constants_enum_super_and_synthetic_statics() {
        let src = "package p;\npublic enum E { SAMPLE, PATIENT;\n\
                   @Override public String toString() { return name(); } }\n";
        let class = class_from_source(src, "E", &pick_first).unwrap();
        assert!(
            class.supers.iter().any(|s| s == "java.lang.Enum"),
            "implicit Enum super: {:?}",
            class.supers
        );
        let statics: Vec<&str> = class
            .members
            .iter()
            .filter(|m| m.is_static)
            .map(|m| m.name.as_str())
            .collect();
        assert!(statics.contains(&"SAMPLE"), "constant: {statics:?}");
        assert!(statics.contains(&"PATIENT"), "constant: {statics:?}");
        assert!(
            statics.contains(&"values"),
            "synthetic values(): {statics:?}"
        );
        assert!(
            statics.contains(&"valueOf"),
            "synthetic valueOf(): {statics:?}"
        );
    }

    #[test]
    fn only_private_members_are_excluded_from_project_source() {
        // A project's own source exposes package-private members too
        // (same-package code legitimately uses them) — only explicitly
        // `private` members stay hidden. Looser than the bytecode gate on
        // purpose; see `externally_visible`.
        let src = "package demo;\n\
                   public class Widget {\n\
                   public int pub_f;\n\
                   protected int prot_f;\n\
                   private int priv_f;\n\
                   int pkg_f;\n\
                   private void priv_m() {}\n\
                   void pkg_m() {}\n\
                   private Widget() {}\n\
                   Widget(int n) {}\n\
                   }\n";
        let class = class_from_source(src, "Widget", &pick_first).unwrap();
        let names: Vec<&str> = class.members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"pub_f"), "{names:?}");
        assert!(names.contains(&"prot_f"), "{names:?}");
        assert!(
            names.contains(&"pkg_f"),
            "package-private field included: {names:?}"
        );
        assert!(
            names.contains(&"pkg_m"),
            "package-private method included: {names:?}"
        );
        assert!(
            !names.contains(&"priv_f"),
            "private field hidden: {names:?}"
        );
        assert!(
            !names.contains(&"priv_m"),
            "private method hidden: {names:?}"
        );
        let ctors: Vec<_> = class
            .members
            .iter()
            .filter(|m| m.kind == ExternalMemberKind::Constructor)
            .collect();
        assert_eq!(
            ctors.len(),
            1,
            "the package-private ctor is included, the private one is not: {:?}",
            class.members
        );
    }

    #[test]
    fn interface_members_are_implicitly_public() {
        let src = "package demo;\n\
                   public interface Greeter {\n\
                   int MAX = 10;\n\
                   String greet();\n\
                   private void helper() {}\n\
                   }\n";
        let class = class_from_source(src, "Greeter", &pick_first).unwrap();
        let names: Vec<&str> = class.members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"MAX"), "{names:?}");
        assert!(names.contains(&"greet"), "{names:?}");
        assert!(!names.contains(&"helper"), "explicit private: {names:?}");
    }

    #[test]
    fn lombok_accessors_synthesized_for_closed_file() {
        let src = "package demo;\nimport lombok.Data;\n\
                   @Data public class Person {\n\
                   private String name;\n\
                   private final int age;\n\
                   }\n";
        let class = class_from_source(src, "Person", &pick_first).unwrap();
        let names: Vec<&str> = class.members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"getName"), "{names:?}");
        assert!(names.contains(&"setName"), "{names:?}");
        assert!(names.contains(&"getAge"), "{names:?}");
        assert!(!names.contains(&"setAge"), "final: {names:?}");
        let get_name = class.members.iter().find(|m| m.name == "getName").unwrap();
        // Return FQN recovered through the declaring file's imports
        // (pick_first hands back the first candidate — the package-local one).
        assert_eq!(get_name.ret_fqn.as_deref(), Some("demo.String"));
        assert_eq!(get_name.ret_display.as_deref(), Some("String"));
    }

    #[test]
    fn lombok_builder_companion_class_resolves_by_nested_path() {
        let src = "package demo;\nimport lombok.Builder;\n\
                   @Builder public class Person {\n\
                   private String name;\n\
                   }\n";
        // The @Builder entry point on the class itself…
        let class = class_from_source(src, "Person", &pick_first).unwrap();
        let builder = class.members.iter().find(|m| m.name == "builder").unwrap();
        assert!(builder.is_static);
        assert_eq!(
            builder.ret_fqn.as_deref(),
            Some("demo.Person$PersonBuilder")
        );
        // …and the synthesized companion type by its nested path.
        let companion = class_from_source(src, "Person.PersonBuilder", &pick_first)
            .expect("synthesized builder class");
        let fluent = companion.members.iter().find(|m| m.name == "name").unwrap();
        assert_eq!(fluent.ret_fqn.as_deref(), Some("demo.Person$PersonBuilder"));
        assert!(companion.members.iter().any(|m| m.name == "build"));
        // Without @Builder, the nested path stays unknown.
        let no_builder = "package demo;\nimport lombok.Getter;\n\
                          @Getter public class Person { private String name; }\n";
        assert!(class_from_source(no_builder, "Person.PersonBuilder", &pick_first).is_none());
    }

    #[test]
    fn record_component_is_visible_with_erased_signature() {
        let src = "package demo;\npublic record Point(int x, int y) {}\n";
        let class = class_from_source(src, "Point", &pick_first).unwrap();
        let x = class.members.iter().find(|m| m.name == "x").unwrap();
        assert_eq!(x.signature, "int x");
        assert_eq!(x.kind, ExternalMemberKind::Field);
    }
}
