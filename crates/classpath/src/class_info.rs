//! Turn `.class` bytes into an owned [`ClassInfo`] via `cafebabe`, rendering
//! readable (raw, generics-erased) member signatures from JVM descriptors.

use cafebabe::attributes::{AttributeData, AttributeInfo};
use cafebabe::descriptors::{
    ClassName, FieldDescriptor, FieldType, MethodDescriptor, ReturnDescriptor,
};
use cafebabe::{parse_class_with_options, FieldAccessFlags, MethodAccessFlags, ParseOptions};

use crate::{generics, ClassInfo, Member, MemberKind};

/// Parse a class file into our owned model. Returns `None` on any parse error
/// (malformed/truncated input is skipped, never panicked on).
pub(crate) fn parse(bytes: &[u8]) -> Option<ClassInfo> {
    // We only need the structure (names, descriptors, flags), not code bodies.
    let mut opts = ParseOptions::default();
    opts.parse_bytecode(false);
    let class = parse_class_with_options(bytes, &opts).ok()?;

    let fqn = fqn_of(&class.this_class);
    let mut supers = Vec::new();
    if let Some(sc) = &class.super_class {
        supers.push(fqn_of(sc));
    }
    for iface in &class.interfaces {
        supers.push(fqn_of(iface));
    }

    // Formal type parameters of the class, e.g. ["E"] for ArrayList<E>.
    let type_params = signature_attr(&class.attributes)
        .map(generics::class_type_params)
        .unwrap_or_default();

    // Type arguments applied to each entry of `supers` (superclass, then
    // interfaces, in that order) by the class's own Signature attribute, e.g.
    // `class MyList extends AbstractList<String>` -> `[["String"], ...]`.
    // A parse failure or an entry-count mismatch against `supers` (malformed/
    // truncated attribute, or an edge case where the raw interface order and
    // the signature's superinterface order disagree) degrades to "no extra
    // info" for every entry rather than risk misaligning them.
    let super_type_args = signature_attr(&class.attributes)
        .map(|sig| generics::super_type_args(sig, &type_params))
        .filter(|args| args.len() == supers.len())
        .unwrap_or_else(|| vec![Vec::new(); supers.len()]);

    // The class's own simple name — a constructor member is displayed as
    // `ClassName(params)` (there's no method name to reuse, unlike a regular
    // method) and doubles as the docsrc lookup key for its Javadoc (a source
    // archive has no `<init>`, only a constructor declaration named after its
    // class).
    let this_simple = simple_name(&class.this_class);

    let mut members = Vec::new();
    for field in &class.fields {
        if !field_visible(field.access_flags) {
            continue;
        }
        let ret_display = signature_attr(&field.attributes)
            .and_then(|sig| generics::field_template(sig, &type_params));
        let template = ret_display
            .as_ref()
            .map(|ty| format!("{ty} {}", field.name));
        members.push(Member {
            signature: format!("{} {}", render_field(&field.descriptor), field.name),
            template,
            name: field.name.to_string(),
            kind: MemberKind::Field,
            is_static: field.access_flags.contains(FieldAccessFlags::STATIC),
            ret_fqn: object_fqn(&field.descriptor),
            ret_display,
        });
    }
    for method in &class.methods {
        // Compiler-synthesized bridge/synthetic methods are never surfaced,
        // constructor or otherwise.
        if !method_visible(method.access_flags) {
            continue;
        }
        if method.name == "<init>" {
            let template = signature_attr(&method.attributes)
                .and_then(|sig| generics::method_template(sig, &type_params))
                .map(|(method_type_params, _ret, params)| {
                    let prefix = if method_type_params.is_empty() {
                        String::new()
                    } else {
                        format!("<{}> ", method_type_params.join(", "))
                    };
                    format!("{prefix}{this_simple}({})", params.join(", "))
                });
            members.push(Member {
                signature: render_constructor(&this_simple, &method.descriptor),
                template,
                name: this_simple.clone(),
                kind: MemberKind::Constructor,
                is_static: false,
                // A constructor "returns" its own class, but chains reach that
                // via `object_creation_expression`, never through a member
                // result type — so nothing is carried here.
                ret_fqn: None,
                ret_display: None,
            });
            continue;
        }
        // Skip <clinit> and any other reserved/synthetic special name — the
        // JVM spec only defines `<init>`/`<clinit>` as `<`-prefixed method
        // names, but untrusted bytes could carry anything, so this stays a
        // blanket exclusion rather than an exact `<clinit>` match.
        if method.name.starts_with('<') {
            continue;
        }
        let parsed = signature_attr(&method.attributes)
            .and_then(|sig| generics::method_template(sig, &type_params));
        let template = parsed.as_ref().map(|(method_type_params, ret, params)| {
            let prefix = if method_type_params.is_empty() {
                String::new()
            } else {
                format!("<{}> ", method_type_params.join(", "))
            };
            format!("{prefix}{ret} {}({})", method.name, params.join(", "))
        });
        let ret_display = parsed
            .map(|(_, ret, _)| ret)
            .filter(|ret| ret != "void");
        members.push(Member {
            signature: render_method(&method.name, &method.descriptor),
            template,
            name: method.name.to_string(),
            kind: MemberKind::Method,
            is_static: method.access_flags.contains(MethodAccessFlags::STATIC),
            ret_fqn: match &method.descriptor.return_type {
                ReturnDescriptor::Return(field) => object_fqn(field),
                ReturnDescriptor::Void => None,
            },
            ret_display,
        });
    }

    Some(ClassInfo {
        fqn,
        supers,
        type_params,
        members,
        super_type_args,
    })
}

/// The `Signature` (generic) attribute string from an attribute list, if present.
fn signature_attr<'a>(attributes: &'a [AttributeInfo]) -> Option<&'a str> {
    attributes.iter().find_map(|a| match &a.data {
        AttributeData::Signature(sig) => Some(sig.as_ref()),
        _ => None,
    })
}

fn field_visible(flags: FieldAccessFlags) -> bool {
    (flags.contains(FieldAccessFlags::PUBLIC) || flags.contains(FieldAccessFlags::PROTECTED))
        && !flags.contains(FieldAccessFlags::SYNTHETIC)
}

fn method_visible(flags: MethodAccessFlags) -> bool {
    (flags.contains(MethodAccessFlags::PUBLIC) || flags.contains(MethodAccessFlags::PROTECTED))
        && !flags.contains(MethodAccessFlags::SYNTHETIC)
        && !flags.contains(MethodAccessFlags::BRIDGE)
}

/// `java/util/List` → `java.util.List`.
fn fqn_of(name: &ClassName) -> String {
    name.to_string().replace('/', ".")
}

/// Simple display name: last `/` segment, with `$` (nested) shown as `.`.
fn simple_name(name: &ClassName) -> String {
    let internal = name.to_string();
    internal
        .rsplit('/')
        .next()
        .unwrap_or(&internal)
        .replace('$', ".")
}

fn render_method(name: &str, descriptor: &MethodDescriptor) -> String {
    let params = descriptor
        .parameters
        .iter()
        .map(render_field)
        .collect::<Vec<_>>()
        .join(", ");
    let ret = match &descriptor.return_type {
        ReturnDescriptor::Void => "void".to_string(),
        ReturnDescriptor::Return(field) => render_field(field),
    };
    format!("{ret} {name}({params})")
}

/// `ClassName(paramTypes)` — a constructor's display shape, the same
/// parameter rendering as [`render_method`] but with no return type and the
/// declaring class's simple name standing in for a method name.
fn render_constructor(name: &str, descriptor: &MethodDescriptor) -> String {
    let params = descriptor
        .parameters
        .iter()
        .map(render_field)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({params})")
}

/// The dotted FQN of a non-array object descriptor (`Ljava/io/PrintStream;` →
/// `java.io.PrintStream`), or `None` for primitives and arrays — the erased
/// type a member-access chain can continue through.
fn object_fqn(descriptor: &FieldDescriptor) -> Option<String> {
    if descriptor.dimensions != 0 {
        return None;
    }
    match &descriptor.field_type {
        FieldType::Object(class) => Some(fqn_of(class)),
        _ => None,
    }
}

fn render_field(descriptor: &FieldDescriptor) -> String {
    let base = match &descriptor.field_type {
        FieldType::Byte => "byte".to_string(),
        FieldType::Char => "char".to_string(),
        FieldType::Double => "double".to_string(),
        FieldType::Float => "float".to_string(),
        FieldType::Integer => "int".to_string(),
        FieldType::Long => "long".to_string(),
        FieldType::Short => "short".to_string(),
        FieldType::Boolean => "boolean".to_string(),
        FieldType::Object(class) => simple_name(class),
    };
    format!("{base}{}", "[]".repeat(descriptor.dimensions as usize))
}

/// Hand-assembles minimal, spec-valid `.class` bytes so `class_info::parse`
/// can be exercised without a real JDK: one class extending `java/lang/Object`
/// (optionally with a class-level `Signature`), plus configurable methods and
/// fields (each optionally carrying its own `Signature` attribute, which may
/// be deliberately malformed to test the fallback path).
#[cfg(test)]
mod fixture {
    /// A method or field to include, plus its optional `Signature` attribute
    /// and access flags (`ACC_PUBLIC` unless a test overrides them).
    pub(super) struct MemberSpec {
        pub name: &'static str,
        pub descriptor: &'static str,
        pub signature: Option<&'static str>,
        pub flags: u16,
    }

    fn member(name: &'static str, descriptor: &'static str) -> MemberSpec {
        MemberSpec {
            name,
            descriptor,
            signature: None,
            flags: 0x0001, // ACC_PUBLIC
        }
    }

    pub(super) fn method(name: &'static str, descriptor: &'static str) -> MemberSpec {
        member(name, descriptor)
    }

    /// A method with explicit access flags (e.g. `0x0002` = ACC_PRIVATE,
    /// `0x0000` = package-private), for visibility-gate tests.
    pub(super) fn method_flags(
        name: &'static str,
        descriptor: &'static str,
        flags: u16,
    ) -> MemberSpec {
        MemberSpec {
            flags,
            ..member(name, descriptor)
        }
    }

    pub(super) fn method_sig(
        name: &'static str,
        descriptor: &'static str,
        signature: &'static str,
    ) -> MemberSpec {
        MemberSpec {
            signature: Some(signature),
            ..member(name, descriptor)
        }
    }

    pub(super) fn field_sig(
        name: &'static str,
        descriptor: &'static str,
        signature: &'static str,
    ) -> MemberSpec {
        MemberSpec {
            signature: Some(signature),
            ..member(name, descriptor)
        }
    }

    /// A public field with no `Signature` attribute.
    pub(super) fn field(name: &'static str, descriptor: &'static str) -> MemberSpec {
        member(name, descriptor)
    }

    /// Growable constant pool: each push returns its 1-based CP index.
    #[derive(Default)]
    struct ConstantPool {
        buf: Vec<u8>,
        next: u16,
    }

    impl ConstantPool {
        fn new() -> Self {
            ConstantPool {
                buf: Vec::new(),
                next: 1,
            }
        }

        fn utf8(&mut self, s: &str) -> u16 {
            let idx = self.next;
            self.buf.push(1); // CONSTANT_Utf8
            self.buf.extend_from_slice(&(s.len() as u16).to_be_bytes());
            self.buf.extend_from_slice(s.as_bytes());
            self.next += 1;
            idx
        }

        fn class(&mut self, internal_name: &str) -> u16 {
            let name_idx = self.utf8(internal_name);
            let idx = self.next;
            self.buf.push(7); // CONSTANT_Class
            self.buf.extend_from_slice(&name_idx.to_be_bytes());
            self.next += 1;
            idx
        }
    }

    fn signature_attribute(cp: &mut ConstantPool, sig_name_idx: u16, sig: &str) -> Vec<u8> {
        let sig_idx = cp.utf8(sig);
        let mut out = Vec::new();
        out.extend_from_slice(&sig_name_idx.to_be_bytes()); // attribute_name_index
        out.extend_from_slice(&2u32.to_be_bytes()); // attribute_length
        out.extend_from_slice(&sig_idx.to_be_bytes());
        out
    }

    fn member_info(cp: &mut ConstantPool, sig_name_idx: u16, m: &MemberSpec) -> Vec<u8> {
        let name_idx = cp.utf8(m.name);
        let desc_idx = cp.utf8(m.descriptor);
        let mut out = Vec::new();
        out.extend_from_slice(&m.flags.to_be_bytes()); // access_flags
        out.extend_from_slice(&name_idx.to_be_bytes());
        out.extend_from_slice(&desc_idx.to_be_bytes());
        match m.signature {
            Some(sig) => {
                out.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
                out.extend_from_slice(&signature_attribute(cp, sig_name_idx, sig));
            }
            None => out.extend_from_slice(&0u16.to_be_bytes()),
        }
        out
    }

    /// Assemble a minimal class named `this_name` (internal form, e.g.
    /// `"test/Box"`) extending `java/lang/Object`, with the given methods and
    /// fields, and an optional class-level `Signature` attribute.
    pub(super) fn build(
        this_name: &str,
        class_signature: Option<&str>,
        fields: &[MemberSpec],
        methods: &[MemberSpec],
    ) -> Vec<u8> {
        let mut cp = ConstantPool::new();
        let this_class = cp.class(this_name);
        let super_class = cp.class("java/lang/Object");
        let sig_name_idx = cp.utf8("Signature");

        let field_bytes: Vec<u8> = fields
            .iter()
            .map(|f| member_info(&mut cp, sig_name_idx, f))
            .collect::<Vec<_>>()
            .concat();
        let method_bytes: Vec<u8> = methods
            .iter()
            .map(|m| member_info(&mut cp, sig_name_idx, m))
            .collect::<Vec<_>>()
            .concat();

        // Class-level Signature attribute is built last so its Utf8 entries
        // land after everything referenced above (order doesn't matter to a
        // conforming reader, but keeping it last keeps this function simple).
        let (class_attr_count, class_attr_bytes): (u16, Vec<u8>) = match class_signature {
            Some(sig) => (1, signature_attribute(&mut cp, sig_name_idx, sig)),
            None => (0, Vec::new()),
        };

        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // minor_version
        out.extend_from_slice(&55u16.to_be_bytes()); // major_version (Java 11)
        out.extend_from_slice(&(cp.next).to_be_bytes()); // constant_pool_count = next unused index
        out.extend_from_slice(&cp.buf);
        out.extend_from_slice(&0x0021u16.to_be_bytes()); // access_flags: PUBLIC | SUPER
        out.extend_from_slice(&this_class.to_be_bytes());
        out.extend_from_slice(&super_class.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        out.extend_from_slice(&(fields.len() as u16).to_be_bytes());
        out.extend_from_slice(&field_bytes);
        out.extend_from_slice(&(methods.len() as u16).to_be_bytes());
        out.extend_from_slice(&method_bytes);
        out.extend_from_slice(&class_attr_count.to_be_bytes());
        out.extend_from_slice(&class_attr_bytes);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{build, field, field_sig, method, method_flags, method_sig};
    use super::*;

    #[test]
    fn descriptor_vs_signature_preference_method_wins() {
        // class Box<T> { T get() { ... } } — descriptor erases to Object,
        // Signature carries the real type variable.
        let bytes = build(
            "test/Box",
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[],
            &[method_sig("get", "()Ljava/lang/Object;", "()TT;")],
        );
        let info = parse(&bytes).expect("parses");
        assert_eq!(info.type_params, vec!["T".to_string()]);
        let get = info.members.iter().find(|m| m.name == "get").unwrap();
        // Erased signature stays stable (dedup key)...
        assert_eq!(get.signature, "Object get()");
        // ...but the generic template — preferred for display — carries the
        // real type variable via its class-param placeholder.
        assert_eq!(get.template.as_deref(), Some("{0} get()"));
    }

    #[test]
    fn method_type_param_shadowing_class_param_renders_by_name() {
        // class Box<T> { <T> T foo(T x) } — legal Java: the method's own T
        // shadows the class's T, so the rendered template must show the
        // literal `T` everywhere, never the class's {0} placeholder.
        let bytes = build(
            "test/Box",
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[],
            &[method_sig(
                "foo",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                "<T:Ljava/lang/Object;>(TT;)TT;",
            )],
        );
        let info = parse(&bytes).expect("parses");
        assert_eq!(info.type_params, vec!["T".to_string()]);
        let foo = info.members.iter().find(|m| m.name == "foo").unwrap();
        assert_eq!(foo.template.as_deref(), Some("<T> T foo(T)"));
    }

    #[test]
    fn method_type_params_rendered_in_signature_text() {
        // static <T> T foo(Class<T> c) — method-level type param, no class
        // type params at all.
        let bytes = build(
            "test/Utils",
            None,
            &[],
            &[method_sig(
                "foo",
                "(Ljava/lang/Class;)Ljava/lang/Object;",
                "<T:Ljava/lang/Object;>(Ljava/lang/Class<TT;>;)TT;",
            )],
        );
        let info = parse(&bytes).expect("parses");
        assert!(info.type_params.is_empty());
        let foo = info.members.iter().find(|m| m.name == "foo").unwrap();
        assert_eq!(foo.template.as_deref(), Some("<T> T foo(Class<T>)"));
    }

    #[test]
    fn malformed_method_signature_falls_back_to_erased_no_panic() {
        // Truncated signature (missing the closing `;`) must not panic, and
        // must leave the erased rendering untouched.
        let bytes = build(
            "test/Bad",
            None,
            &[],
            &[method_sig("bar", "(I)Ljava/lang/Object;", "(I)TE")],
        );
        let info = parse(&bytes).expect("still parses the class");
        let bar = info.members.iter().find(|m| m.name == "bar").unwrap();
        assert_eq!(bar.signature, "Object bar(int)");
        assert_eq!(bar.template, None);
    }

    #[test]
    fn malformed_field_signature_falls_back_to_erased_no_panic() {
        let bytes = build(
            "test/BadField",
            None,
            &[field_sig("x", "Ljava/lang/Object;", "Ljava/util/List<TE")],
            &[],
        );
        let info = parse(&bytes).expect("still parses the class");
        let x = info.members.iter().find(|m| m.name == "x").unwrap();
        assert_eq!(x.signature, "Object x");
        assert_eq!(x.template, None);
    }

    #[test]
    fn malformed_class_signature_falls_back_to_no_super_args() {
        // A class-level Signature so badly truncated its superclass entry
        // never parses — the class must still parse, with `super_type_args`
        // degrading to "no extra info" (empty per entry) rather than
        // panicking or misaligning against `supers`.
        let bytes = build(
            "test/BadClass",
            Some("<T:Ljava/util/List<TE"),
            &[],
            &[method("plain", "()V")],
        );
        let info = parse(&bytes).expect("still parses the class");
        assert_eq!(info.supers, vec!["java.lang.Object".to_string()]);
        assert!(info.super_type_args.iter().all(Vec::is_empty));
        // `plain` has no Signature attribute, so it's unaffected either way.
        let plain = info.members.iter().find(|m| m.name == "plain").unwrap();
        assert_eq!(plain.signature, "void plain()");
    }

    #[test]
    fn member_with_no_signature_attribute_has_no_template() {
        let bytes = build("test/Plain", None, &[], &[method("size", "()I")]);
        let info = parse(&bytes).expect("parses");
        let size = info.members.iter().find(|m| m.name == "size").unwrap();
        assert_eq!(size.signature, "int size()");
        assert_eq!(size.template, None);
    }

    /// M6.3: `<init>` methods are surfaced as `Constructor` members named
    /// after the declaring class (not `<init>`), rendered `ClassName(params)`
    /// — one plain, one carrying its own generic type parameter (so its
    /// template renders the type variable by name, same shadow-by-name rule
    /// a method's own type parameter follows).
    #[test]
    fn constructors_become_members_named_after_the_class() {
        let bytes = build(
            "test/Foo",
            None,
            &[],
            &[
                method("<init>", "(I)V"),
                method_sig(
                    "<init>",
                    "(Ljava/util/List;)V",
                    "<T:Ljava/lang/Object;>(Ljava/util/List<TT;>;)V",
                ),
                method("plain", "()V"),
            ],
        );
        let info = parse(&bytes).expect("parses");
        let ctors: Vec<_> = info
            .members
            .iter()
            .filter(|m| matches!(m.kind, MemberKind::Constructor))
            .collect();
        assert_eq!(ctors.len(), 2, "{:?}", info.members);
        assert!(
            ctors.iter().all(|m| m.name == "Foo"),
            "constructor member name must be the class's simple name: {:?}",
            ctors
        );
        assert!(
            ctors
                .iter()
                .any(|m| m.signature == "Foo(int)" && m.template.is_none()),
            "{:?}",
            ctors
        );
        assert!(
            ctors
                .iter()
                .any(|m| m.signature == "Foo(List)"
                    && m.template.as_deref() == Some("<T> Foo(List<T>)")),
            "{:?}",
            ctors
        );
        // `<init>` must never leak through as an ordinary `Method` member.
        assert!(!info.members.iter().any(|m| m.name == "<init>"));
        // A regular method alongside the constructors is unaffected.
        assert!(info
            .members
            .iter()
            .any(|m| m.name == "plain" && matches!(m.kind, MemberKind::Method)));
    }

    // --- M7: structured member result types (chain resolution) ---

    #[test]
    fn generic_return_carries_erased_fqn_and_display_template() {
        // class Box<E> { Stream<E> stream() } — descriptor erases to Stream,
        // Signature carries Stream<E> (rendered Stream<{0}>).
        let bytes = build(
            "test/Box",
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[],
            &[method_sig(
                "stream",
                "()Ljava/util/stream/Stream;",
                "()Ljava/util/stream/Stream<TE;>;",
            )],
        );
        let info = parse(&bytes).expect("parses");
        let m = info.members.iter().find(|m| m.name == "stream").unwrap();
        assert_eq!(m.ret_fqn.as_deref(), Some("java.util.stream.Stream"));
        assert_eq!(m.ret_display.as_deref(), Some("Stream<{0}>"));
    }

    #[test]
    fn type_var_return_keeps_erasure_fqn_with_placeholder_display() {
        // class Box<E> { E get(int) } — erasure Object, display {0}.
        let bytes = build(
            "test/Box",
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[],
            &[method_sig("get", "(I)Ljava/lang/Object;", "(I)TE;")],
        );
        let info = parse(&bytes).expect("parses");
        let m = info.members.iter().find(|m| m.name == "get").unwrap();
        assert_eq!(m.ret_fqn.as_deref(), Some("java.lang.Object"));
        assert_eq!(m.ret_display.as_deref(), Some("{0}"));
    }

    #[test]
    fn plain_object_return_without_signature_has_fqn_only() {
        let bytes = build(
            "test/S",
            None,
            &[],
            &[method("trim", "()Ljava/lang/String;")],
        );
        let info = parse(&bytes).expect("parses");
        let m = info.members.iter().find(|m| m.name == "trim").unwrap();
        assert_eq!(m.ret_fqn.as_deref(), Some("java.lang.String"));
        assert_eq!(m.ret_display, None);
    }

    #[test]
    fn primitive_void_and_array_returns_have_no_result_type() {
        let bytes = build(
            "test/P",
            None,
            &[],
            &[
                method("size", "()I"),
                method_sig("clear", "()V", "()V"),
                method("toArray", "()[Ljava/lang/Object;"),
            ],
        );
        let info = parse(&bytes).expect("parses");
        let by = |n: &str| info.members.iter().find(|m| m.name == n).unwrap();
        assert_eq!(by("size").ret_fqn, None);
        assert_eq!(by("size").ret_display, None);
        // Even with a Signature attribute, a void return carries no display.
        assert_eq!(by("clear").ret_fqn, None);
        assert_eq!(by("clear").ret_display, None);
        assert_eq!(by("toArray").ret_fqn, None);
    }

    #[test]
    fn field_declared_type_carries_fqn_and_generic_display() {
        let bytes = build(
            "test/F",
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[
                field("out", "Ljava/io/PrintStream;"),
                field_sig("items", "Ljava/util/List;", "Ljava/util/List<TE;>;"),
                field("count", "I"),
            ],
            &[],
        );
        let info = parse(&bytes).expect("parses");
        let by = |n: &str| info.members.iter().find(|m| m.name == n).unwrap();
        assert_eq!(by("out").ret_fqn.as_deref(), Some("java.io.PrintStream"));
        assert_eq!(by("out").ret_display, None);
        assert_eq!(by("items").ret_fqn.as_deref(), Some("java.util.List"));
        assert_eq!(by("items").ret_display.as_deref(), Some("List<{0}>"));
        assert_eq!(by("count").ret_fqn, None);
    }

    #[test]
    fn constructors_carry_no_result_type() {
        let bytes = build("test/C", None, &[], &[method("<init>", "(I)V")]);
        let info = parse(&bytes).expect("parses");
        let ctor = info
            .members
            .iter()
            .find(|m| matches!(m.kind, MemberKind::Constructor))
            .unwrap();
        assert_eq!(ctor.ret_fqn, None);
        assert_eq!(ctor.ret_display, None);
    }

    /// `<clinit>` (the static initializer) is never surfaced as a member,
    /// constructor or otherwise.
    #[test]
    fn clinit_is_never_a_member() {
        let bytes = build("test/HasClinit", None, &[], &[method("<clinit>", "()V")]);
        let info = parse(&bytes).expect("parses");
        assert!(info.members.is_empty(), "{:?}", info.members);
    }

    /// M6.3 fix round 1: private and package-private `<init>` methods go
    /// through the same `method_visible` gate as regular methods — only
    /// public/protected constructors are surfaced as `Constructor` members.
    #[test]
    fn non_visible_constructors_are_excluded() {
        let bytes = build(
            "test/Vis",
            None,
            &[],
            &[
                method_flags("<init>", "()V", 0x0002),  // ACC_PRIVATE
                method_flags("<init>", "(I)V", 0x0000), // package-private
                method_flags("<init>", "(J)V", 0x0004), // ACC_PROTECTED
                method_flags("<init>", "(D)V", 0x0001), // ACC_PUBLIC
            ],
        );
        let info = parse(&bytes).expect("parses");
        let ctor_sigs: Vec<_> = info
            .members
            .iter()
            .filter(|m| matches!(m.kind, MemberKind::Constructor))
            .map(|m| m.signature.as_str())
            .collect();
        assert_eq!(
            ctor_sigs,
            vec!["Vis(long)", "Vis(double)"],
            "only protected/public constructors survive: {:?}",
            info.members
        );
    }
}
