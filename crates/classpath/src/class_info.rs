//! Turn `.class` bytes into an owned [`ClassInfo`] via `cafebabe`, rendering
//! readable (raw, generics-erased) member signatures from JVM descriptors.

use cafebabe::descriptors::{ClassName, FieldDescriptor, FieldType, MethodDescriptor, ReturnDescriptor};
use cafebabe::{parse_class_with_options, FieldAccessFlags, MethodAccessFlags, ParseOptions};

use crate::{ClassInfo, Member, MemberKind};

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

    let mut members = Vec::new();
    for field in &class.fields {
        if !field_visible(field.access_flags) {
            continue;
        }
        members.push(Member {
            signature: format!("{} {}", render_field(&field.descriptor), field.name),
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
        members.push(Member {
            signature: render_method(&method.name, &method.descriptor),
            name: method.name.to_string(),
            kind: MemberKind::Method,
            is_static: method.access_flags.contains(MethodAccessFlags::STATIC),
        });
    }

    Some(ClassInfo {
        fqn,
        supers,
        members,
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
