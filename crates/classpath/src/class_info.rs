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

    let mut members = Vec::new();
    for field in &class.fields {
        if !field_visible(field.access_flags) {
            continue;
        }
        let template = signature_attr(&field.attributes)
            .and_then(|sig| generics::field_template(sig, &type_params))
            .map(|ty| format!("{ty} {}", field.name));
        members.push(Member {
            signature: format!("{} {}", render_field(&field.descriptor), field.name),
            template,
            name: field.name.to_string(),
            kind: MemberKind::Field,
            is_static: field.access_flags.contains(FieldAccessFlags::STATIC),
        });
    }
    for method in &class.methods {
        // Skip <init>/<clinit> and compiler-synthesized bridge/synthetic methods.
        if method.name.starts_with('<') || !method_visible(method.access_flags) {
            continue;
        }
        let template = signature_attr(&method.attributes)
            .and_then(|sig| generics::method_template(sig, &type_params))
            .map(|(method_type_params, ret, params)| {
                let prefix = if method_type_params.is_empty() {
                    String::new()
                } else {
                    format!("<{}> ", method_type_params.join(", "))
                };
                format!("{prefix}{ret} {}({})", method.name, params.join(", "))
            });
        members.push(Member {
            signature: render_method(&method.name, &method.descriptor),
            template,
            name: method.name.to_string(),
            kind: MemberKind::Method,
            is_static: method.access_flags.contains(MethodAccessFlags::STATIC),
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
    /// A method or field to include, plus its optional `Signature` attribute.
    pub(super) struct MemberSpec {
        pub name: &'static str,
        pub descriptor: &'static str,
        pub signature: Option<&'static str>,
    }

    fn member(name: &'static str, descriptor: &'static str) -> MemberSpec {
        MemberSpec {
            name,
            descriptor,
            signature: None,
        }
    }

    pub(super) fn method(name: &'static str, descriptor: &'static str) -> MemberSpec {
        member(name, descriptor)
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
        out.extend_from_slice(&0x0001u16.to_be_bytes()); // access_flags: ACC_PUBLIC
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
    use super::fixture::{build, field_sig, method, method_sig};
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
}
