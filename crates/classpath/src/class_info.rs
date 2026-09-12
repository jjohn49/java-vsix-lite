//! Parses `.class` bytes into an owned [`ClassInfo`] via `cafebabe`.
//! Produces both display-ready member signatures and structured
//! [`jvl_types`] metadata for semantic checks.

use cafebabe::attributes::{AttributeData, AttributeInfo, InnerClassAccessFlags};
use cafebabe::descriptors::{
    ClassName, FieldDescriptor, FieldType, MethodDescriptor, ReturnDescriptor,
};
use cafebabe::{
    parse_class_with_options, ClassAccessFlags, FieldAccessFlags, FieldInfo, MethodAccessFlags,
    ParseOptions,
};
use jvl_types::{
    Access, ClassKind, ClassMetadata, MemberMetadata, PrimitiveType, TypeId, TypeParameter, TypeRef,
};

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

    // Type arguments for each entry of `supers` (superclass, then interfaces),
    // from the class's own Signature attribute. Falls back to empty for
    // every entry on a parse failure or count mismatch, rather than risk
    // misaligning them.
    let super_type_args = signature_attr(&class.attributes)
        .map(|sig| generics::super_type_args(sig, &type_params))
        .filter(|args| args.len() == supers.len())
        .unwrap_or_else(|| vec![Vec::new(); supers.len()]);

    // The class's simple name: displays constructors as `ClassName(params)`
    // and doubles as the Javadoc lookup key (source has no `<init>`).
    let this_simple = simple_name(&class.this_class);

    // --- structured metadata (jvl_types): kind/access/hierarchy/type params ---
    let class_flags = class.access_flags;
    let access = if class_flags.contains(ClassAccessFlags::PUBLIC) {
        Access::Public
    } else {
        Access::Package
    };
    let kind = if class_flags.contains(ClassAccessFlags::ANNOTATION) {
        ClassKind::Annotation
    } else if class_flags.contains(ClassAccessFlags::INTERFACE) {
        ClassKind::Interface
    } else if class_flags.contains(ClassAccessFlags::ENUM) {
        ClassKind::Enum
    } else if class.super_class.as_ref().map(fqn_of).as_deref() == Some("java.lang.Record") {
        ClassKind::Record
    } else {
        ClassKind::Class
    };
    let structured =
        signature_attr(&class.attributes).and_then(|s| generics::parse_class_signature(s, &fqn));
    let (type_parameters, supertypes) = match structured {
        Some((params, sups)) if sups.len() == supers.len() => (params, sups),
        // Malformed or mismatched Signature: fall back to erased identity
        // only, same conservative rule as `super_type_args` above.
        _ => (
            Vec::new(),
            supers.iter().map(|s| TypeRef::named(s)).collect(),
        ),
    };
    // Class params paired with their declared names, since member Signature
    // text refers to a class type variable by name (`TE;`), not index.
    // `type_params` and `type_parameters` are index-aligned.
    let named_type_parameters: Vec<(String, TypeParameter)> = type_params
        .iter()
        .cloned()
        .zip(type_parameters.iter().cloned())
        .collect();
    let enclosing_class = fqn.rsplit_once('$').map(|(outer, _)| TypeId::named(outer));
    // The InnerClasses attribute carries the real static flag via a
    // self-referencing entry (JVMS §4.7.6). Without one, conservatively
    // assume non-static unless the kind implies static, so an unknown
    // enclosing-instance requirement stays silent rather than wrongly
    // flagged.
    let this_internal = class.this_class.to_string();
    let inner_classes_static = class.attributes.iter().find_map(|a| match &a.data {
        AttributeData::InnerClasses(entries) => entries
            .iter()
            .find(|e| e.inner_class_info.as_ref() == this_internal.as_str())
            .map(|e| e.access_flags.contains(InnerClassAccessFlags::STATIC)),
        _ => None,
    });
    let is_static = inner_classes_static.unwrap_or_else(|| {
        enclosing_class.is_none()
            || matches!(
                kind,
                ClassKind::Interface | ClassKind::Enum | ClassKind::Record | ClassKind::Annotation
            )
    });
    let metadata = Some(ClassMetadata {
        id: TypeId::named(&fqn),
        kind,
        access,
        is_abstract: class_flags.contains(ClassAccessFlags::ABSTRACT)
            || kind == ClassKind::Interface,
        is_static,
        enclosing_class,
        type_parameters: type_parameters.clone(),
        supertypes,
        hierarchy_complete: true,
        constructors_complete: true,
    });

    let mut members = Vec::new();
    for field in &class.fields {
        if !field_visible(field.access_flags) {
            continue;
        }
        let generic_display = signature_attr(&field.attributes)
            .and_then(|sig| generics::field_template(sig, &type_params));
        let template = generic_display
            .as_ref()
            .map(|ty| format!("{ty} {}", field.name));
        let ret_display = Some(generic_display.unwrap_or_else(|| render_field(&field.descriptor)));
        members.push(Member {
            signature: format!("{} {}", render_field(&field.descriptor), field.name),
            template,
            name: field.name.to_string(),
            kind: MemberKind::Field,
            is_static: field.access_flags.contains(FieldAccessFlags::STATIC),
            ret_fqn: object_fqn(&field.descriptor),
            ret_display,
            hidden: false,
            metadata: Some(MemberMetadata {
                declaring_class: TypeId::named(&fqn),
                access: field_access(field.access_flags),
                is_static: field.access_flags.contains(FieldAccessFlags::STATIC),
                is_abstract: false,
                parameters: None,
                result: field_result_type(field, &named_type_parameters),
                type_parameters: Vec::new(),
                is_varargs: false,
            }),
        });
    }
    for method in &class.methods {
        // Compiler-synthesized bridge/synthetic methods are never surfaced.
        if method.access_flags.contains(MethodAccessFlags::SYNTHETIC)
            || method.access_flags.contains(MethodAccessFlags::BRIDGE)
        {
            continue;
        }
        // Skip `<clinit>` and any other `<`-prefixed name: untrusted bytes
        // could carry more than the spec defines, so exclude the whole class.
        if method.name.starts_with('<') && method.name != "<init>" {
            continue;
        }
        // Non-public/protected methods are kept, not dropped: accessibility
        // checks need their metadata. `hidden` excludes them from completion.
        let hidden = !method_visible(method.access_flags);
        let owner = format!(
            "{fqn}#{}({})",
            method.name,
            erased_descriptor_key(&method.descriptor)
        );
        let (method_type_params, method_params, result) = method_structured(
            &method.descriptor,
            signature_attr(&method.attributes),
            &named_type_parameters,
            &owner,
        );
        let is_varargs = method.access_flags.contains(MethodAccessFlags::VARARGS);
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
                // A constructor's class is reached via `object_creation_expression`,
                // not a member result type, so nothing is carried here.
                ret_fqn: None,
                ret_display: None,
                hidden,
                metadata: Some(MemberMetadata {
                    declaring_class: TypeId::named(&fqn),
                    access: method_access(method.access_flags),
                    is_static: false,
                    is_abstract: false,
                    parameters: Some(method_params),
                    // `<init>`'s descriptor return type is always void per
                    // JVMS; `result` from `method_structured` agrees.
                    result: TypeRef::Void,
                    type_parameters: method_type_params,
                    is_varargs,
                }),
            });
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
        let ret_display = Some(match parsed {
            Some((_, ret, _)) => ret,
            None => match &method.descriptor.return_type {
                ReturnDescriptor::Return(field) => render_field(field),
                ReturnDescriptor::Void => "void".to_string(),
            },
        });
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
            hidden,
            metadata: Some(MemberMetadata {
                declaring_class: TypeId::named(&fqn),
                access: method_access(method.access_flags),
                is_static: method.access_flags.contains(MethodAccessFlags::STATIC),
                is_abstract: method.access_flags.contains(MethodAccessFlags::ABSTRACT),
                parameters: Some(method_params),
                result,
                type_parameters: method_type_params,
                is_varargs,
            }),
        });
    }

    Some(ClassInfo {
        fqn,
        supers,
        type_params,
        members,
        super_type_args,
        metadata,
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

fn field_access(flags: FieldAccessFlags) -> Access {
    if flags.contains(FieldAccessFlags::PUBLIC) {
        Access::Public
    } else if flags.contains(FieldAccessFlags::PROTECTED) {
        Access::Protected
    } else if flags.contains(FieldAccessFlags::PRIVATE) {
        Access::Private
    } else {
        Access::Package
    }
}

fn method_access(flags: MethodAccessFlags) -> Access {
    if flags.contains(MethodAccessFlags::PUBLIC) {
        Access::Public
    } else if flags.contains(MethodAccessFlags::PROTECTED) {
        Access::Protected
    } else if flags.contains(MethodAccessFlags::PRIVATE) {
        Access::Private
    } else {
        Access::Package
    }
}

/// The structured [`TypeRef`] for a raw (non-generic) descriptor: maps a
/// primitive `FieldType` directly, an object type to its dotted FQN, and
/// wraps in [`TypeRef::Array`] per `dimensions`.
fn descriptor_type_ref(descriptor: &FieldDescriptor) -> TypeRef {
    let base = match &descriptor.field_type {
        FieldType::Byte => TypeRef::Primitive(PrimitiveType::Byte),
        FieldType::Char => TypeRef::Primitive(PrimitiveType::Char),
        FieldType::Double => TypeRef::Primitive(PrimitiveType::Double),
        FieldType::Float => TypeRef::Primitive(PrimitiveType::Float),
        FieldType::Integer => TypeRef::Primitive(PrimitiveType::Int),
        FieldType::Long => TypeRef::Primitive(PrimitiveType::Long),
        FieldType::Short => TypeRef::Primitive(PrimitiveType::Short),
        FieldType::Boolean => TypeRef::Primitive(PrimitiveType::Boolean),
        FieldType::Object(class) => TypeRef::named(&fqn_of(class)),
    };
    (0..descriptor.dimensions).fold(base, |t, _| TypeRef::Array(Box::new(t)))
}

/// A field's structured declared type: from the `Signature` attribute when
/// present, otherwise the erased descriptor.
fn field_result_type(field: &FieldInfo, class_params: &[(String, TypeParameter)]) -> TypeRef {
    signature_attr(&field.attributes)
        .and_then(|sig| generics::parse_field_signature(sig, class_params))
        .unwrap_or_else(|| descriptor_type_ref(&field.descriptor))
}

/// Erased parameter types joined into a key, used to build a
/// `TypeVariableId` owner string that stays unique across overloads.
fn erased_descriptor_key(descriptor: &MethodDescriptor) -> String {
    descriptor
        .parameters
        .iter()
        .map(render_field)
        .collect::<Vec<_>>()
        .join(",")
}

/// The method's structured type parameters, parameters, and return type:
/// from the `Signature` attribute when its parameter count matches the
/// descriptor, otherwise the erased descriptor.
fn method_structured(
    descriptor: &MethodDescriptor,
    signature: Option<&str>,
    class_params: &[(String, TypeParameter)],
    owner: &str,
) -> (Vec<TypeParameter>, Vec<TypeRef>, TypeRef) {
    if let Some(sig) = signature {
        if let Some((type_params, ret, params)) =
            generics::parse_method_signature(sig, class_params, owner)
        {
            if params.len() == descriptor.parameters.len() {
                return (type_params, params, ret);
            }
        }
    }
    let params = descriptor
        .parameters
        .iter()
        .map(descriptor_type_ref)
        .collect();
    let result = match &descriptor.return_type {
        ReturnDescriptor::Return(field) => descriptor_type_ref(field),
        ReturnDescriptor::Void => TypeRef::Void,
    };
    (Vec::new(), params, result)
}

/// Hand-assembles minimal `.class` bytes so `class_info::parse` can be
/// tested without a real JDK, with configurable methods, fields, and
/// optional (possibly malformed) `Signature` attributes.
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

    /// Assembles a minimal class named `this_name` (e.g. `"test/Box"`)
    /// extending `java/lang/Object`, with the given methods, fields, and
    /// an optional class-level `Signature`.
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

        // Built last so its Utf8 entries land after everything else; order
        // doesn't matter to a conforming reader.
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
        // class Box<T> { <T> T foo(T x) }: the method's own T shadows the
        // class's T, so the template must render literal `T`, never `{0}`.
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
        // A superclass Signature entry so truncated it never parses. The
        // class must still parse, with `super_type_args` degrading to empty
        // per entry rather than panicking or misaligning.
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
        // Metadata is still populated on a malformed class signature; it
        // degrades to erased-identity supertypes with no type parameters.
        let metadata = info.metadata.expect("metadata always present");
        assert!(metadata.type_parameters.is_empty());
        assert_eq!(
            metadata.supertypes,
            vec![TypeRef::named("java.lang.Object")]
        );
    }

    #[test]
    fn class_signature_produces_structured_type_parameters_and_supertypes() {
        // class MyAbstractList<E> extends AbstractList<E>: Signature carries
        // the real type parameter and a structured supertype, not just `{0}`.
        let bytes = build(
            "test/MyAbstractList",
            Some("<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;"),
            &[],
            &[],
        );
        let info = parse(&bytes).expect("parses");
        let metadata = info.metadata.expect("metadata present");
        assert_eq!(metadata.type_parameters.len(), 1);
        let e = metadata.type_parameters[0].id.clone();
        assert_eq!(
            metadata.supertypes,
            vec![TypeRef::Named {
                id: TypeId::named("java.util.AbstractList"),
                args: vec![TypeRef::Variable(e)],
            }]
        );
    }

    #[test]
    fn method_without_signature_has_structured_parameters_from_descriptor() {
        let bytes = build(
            "test/Plain2",
            None,
            &[],
            &[method("greet", "(ILjava/lang/String;)V")],
        );
        let info = parse(&bytes).expect("parses");
        let greet = info.members.iter().find(|m| m.name == "greet").unwrap();
        let metadata = greet.metadata.as_ref().expect("metadata present");
        assert_eq!(
            metadata.parameters,
            Some(vec![
                TypeRef::Primitive(PrimitiveType::Int),
                TypeRef::named("java.lang.String"),
            ])
        );
    }

    #[test]
    fn member_with_no_signature_attribute_has_no_template() {
        let bytes = build("test/Plain", None, &[], &[method("size", "()I")]);
        let info = parse(&bytes).expect("parses");
        let size = info.members.iter().find(|m| m.name == "size").unwrap();
        assert_eq!(size.signature, "int size()");
        assert_eq!(size.template, None);
    }

    /// `<init>` methods surface as `Constructor` members named after the
    /// class, rendered `ClassName(params)` — including one with its own
    /// generic type parameter.
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

    // --- structured member result types (chain resolution) ---

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
    fn plain_object_return_without_signature_retains_display() {
        let bytes = build(
            "test/S",
            None,
            &[],
            &[method("trim", "()Ljava/lang/String;")],
        );
        let info = parse(&bytes).expect("parses");
        let m = info.members.iter().find(|m| m.name == "trim").unwrap();
        assert_eq!(m.ret_fqn.as_deref(), Some("java.lang.String"));
        assert_eq!(m.ret_display.as_deref(), Some("String"));
    }

    #[test]
    fn primitive_void_and_array_returns_retain_displays() {
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
        assert_eq!(by("size").ret_display.as_deref(), Some("int"));
        assert_eq!(by("clear").ret_fqn, None);
        assert_eq!(by("clear").ret_display.as_deref(), Some("void"));
        assert_eq!(by("toArray").ret_fqn, None);
        assert_eq!(by("toArray").ret_display.as_deref(), Some("Object[]"));
    }

    #[test]
    fn field_declared_types_retain_plain_generic_primitive_and_array_displays() {
        let bytes = build(
            "test/F",
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[
                field("out", "Ljava/io/PrintStream;"),
                field_sig("items", "Ljava/util/List;", "Ljava/util/List<TE;>;"),
                field("count", "I"),
                field("values", "[Ljava/lang/Object;"),
            ],
            &[],
        );
        let info = parse(&bytes).expect("parses");
        let by = |n: &str| info.members.iter().find(|m| m.name == n).unwrap();
        assert_eq!(by("out").ret_fqn.as_deref(), Some("java.io.PrintStream"));
        assert_eq!(by("out").ret_display.as_deref(), Some("PrintStream"));
        assert_eq!(by("items").ret_fqn.as_deref(), Some("java.util.List"));
        assert_eq!(by("items").ret_display.as_deref(), Some("List<{0}>"));
        assert_eq!(by("count").ret_fqn, None);
        assert_eq!(by("count").ret_display.as_deref(), Some("int"));
        assert_eq!(by("values").ret_fqn, None);
        assert_eq!(by("values").ret_display.as_deref(), Some("Object[]"));
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

    /// Private and package-private `<init>` methods are kept with
    /// `hidden: true` and real `metadata.access`, since accessibility
    /// checks need them; only completion listings filter `hidden` out.
    #[test]
    fn non_visible_constructors_are_hidden_but_present() {
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
        let ctors: Vec<_> = info
            .members
            .iter()
            .filter(|m| matches!(m.kind, MemberKind::Constructor))
            .collect();
        assert_eq!(ctors.len(), 4, "{:?}", info.members);
        let by_sig = |sig: &str| ctors.iter().find(|m| m.signature == sig).unwrap();
        assert!(by_sig("Vis()").hidden);
        assert_eq!(
            by_sig("Vis()").metadata.as_ref().unwrap().access,
            Access::Private
        );
        assert!(by_sig("Vis(int)").hidden);
        assert_eq!(
            by_sig("Vis(int)").metadata.as_ref().unwrap().access,
            Access::Package
        );
        assert!(!by_sig("Vis(long)").hidden);
        assert_eq!(
            by_sig("Vis(long)").metadata.as_ref().unwrap().access,
            Access::Protected
        );
        assert!(!by_sig("Vis(double)").hidden);
        assert_eq!(
            by_sig("Vis(double)").metadata.as_ref().unwrap().access,
            Access::Public
        );
    }
    #[test]
    fn abstract_method_flag_is_preserved_in_structured_metadata() {
        let bytes = build(
            "test/Abstract",
            None,
            &[],
            &[method_flags("run", "()V", 0x0401)],
        );
        let info = parse(&bytes).expect("parses");
        let run = info
            .members
            .iter()
            .find(|member| member.name == "run")
            .unwrap();
        assert!(run.metadata.as_ref().unwrap().is_abstract);
    }
}
