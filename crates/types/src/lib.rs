//! Structured Java types shared by `jvl-classpath` (bytecode) and `jvl-syntax`
//! (source); semantic checks compare these values, never display strings.
//! `TypeRef::Unknown` means "insufficient information": consumers must stay
//! silent on it.
#![forbid(unsafe_code)]

pub mod jvm_signature;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    Boolean,
    Byte,
    Short,
    Int,
    Long,
    Char,
    Float,
    Double,
}

impl PrimitiveType {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "boolean" => Self::Boolean,
            "byte" => Self::Byte,
            "short" => Self::Short,
            "int" => Self::Int,
            "long" => Self::Long,
            "char" => Self::Char,
            "float" => Self::Float,
            "double" => Self::Double,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Byte => "byte",
            Self::Short => "short",
            Self::Int => "int",
            Self::Long => "long",
            Self::Char => "char",
            Self::Float => "float",
            Self::Double => "double",
        }
    }
    pub fn box_fqn(self) -> &'static str {
        match self {
            Self::Boolean => "java.lang.Boolean",
            Self::Byte => "java.lang.Byte",
            Self::Short => "java.lang.Short",
            Self::Int => "java.lang.Integer",
            Self::Long => "java.lang.Long",
            Self::Char => "java.lang.Character",
            Self::Float => "java.lang.Float",
            Self::Double => "java.lang.Double",
        }
    }
    pub fn from_box_fqn(fqn: &str) -> Option<Self> {
        [
            Self::Boolean,
            Self::Byte,
            Self::Short,
            Self::Int,
            Self::Long,
            Self::Char,
            Self::Float,
            Self::Double,
        ]
        .into_iter()
        .find(|p| p.box_fqn() == fqn)
    }
    /// JLS 5.1.2 widening primitive conversion (identity excluded).
    pub fn widens_to(self, to: Self) -> bool {
        use PrimitiveType::*;
        matches!(
            (self, to),
            (Byte, Short | Int | Long | Float | Double)
                | (Short, Int | Long | Float | Double)
                | (Char, Int | Long | Float | Double)
                | (Int, Long | Float | Double)
                | (Long, Float | Double)
                | (Float, Double)
        )
    }
}

/// Identity of a class-like type. `Named` is the binary name
/// (`demo.Outer$Inner`); `Local` is a local/anonymous class that has no
/// stable binary name outside the request that parsed it (never cached).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeId {
    Named(String),
    Local { document: usize, declaration: usize },
}

impl TypeId {
    pub fn named(fqn: &str) -> Self {
        TypeId::Named(fqn.to_string())
    }
    pub fn as_named(&self) -> Option<&str> {
        match self {
            TypeId::Named(s) => Some(s),
            TypeId::Local { .. } => None,
        }
    }
}

/// A type variable, identified by its declaring owner and index — never by
/// its letter, so a class literally named `T` is not a variable.
/// `owner` is the class binary name, or `<binary name>#<name>(<erased params>)`
/// for a method/constructor, or `local:<doc>:<decl>` for a local class.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeVariableId {
    pub owner: String,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeRef {
    Primitive(PrimitiveType),
    Void,
    Null,
    /// Empty `args` = raw or non-generic use.
    Named {
        id: TypeId,
        args: Vec<TypeRef>,
    },
    Array(Box<TypeRef>),
    Variable(TypeVariableId),
    Wildcard {
        upper: Option<Box<TypeRef>>,
        lower: Option<Box<TypeRef>>,
    },
    Unknown,
}

impl TypeRef {
    pub fn named(fqn: &str) -> Self {
        TypeRef::Named {
            id: TypeId::named(fqn),
            args: Vec::new(),
        }
    }
    pub fn named_with(fqn: &str, args: Vec<TypeRef>) -> Self {
        TypeRef::Named {
            id: TypeId::named(fqn),
            args,
        }
    }
    pub fn is_reference(&self) -> bool {
        matches!(
            self,
            TypeRef::Named { .. } | TypeRef::Array(_) | TypeRef::Variable(_) | TypeRef::Null
        )
    }
    pub fn contains_unknown(&self) -> bool {
        match self {
            TypeRef::Unknown => true,
            TypeRef::Named { args, .. } => args.iter().any(TypeRef::contains_unknown),
            TypeRef::Array(e) => e.contains_unknown(),
            TypeRef::Wildcard { upper, lower } => {
                upper.as_deref().is_some_and(TypeRef::contains_unknown)
                    || lower.as_deref().is_some_and(TypeRef::contains_unknown)
            }
            _ => false,
        }
    }
    /// Replace type variables by identity. Unbound variables stay as-is.
    pub fn substitute(&self, env: &[(TypeVariableId, TypeRef)]) -> TypeRef {
        match self {
            TypeRef::Variable(v) => env
                .iter()
                .find(|(k, _)| k == v)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| self.clone()),
            TypeRef::Named { id, args } => TypeRef::Named {
                id: id.clone(),
                args: args.iter().map(|a| a.substitute(env)).collect(),
            },
            TypeRef::Array(e) => TypeRef::Array(Box::new(e.substitute(env))),
            TypeRef::Wildcard { upper, lower } => TypeRef::Wildcard {
                upper: upper.as_ref().map(|u| Box::new(u.substitute(env))),
                lower: lower.as_ref().map(|l| Box::new(l.substitute(env))),
            },
            other => other.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    Public,
    Protected,
    Package,
    Private,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeParameter {
    pub id: TypeVariableId,
    pub bounds: Vec<TypeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClassMetadata {
    pub id: TypeId,
    pub kind: ClassKind,
    pub access: Access,
    pub is_abstract: bool,
    /// `true` for top-level, static nested, interfaces, enums, records.
    pub is_static: bool,
    pub enclosing_class: Option<TypeId>,
    pub type_parameters: Vec<TypeParameter>,
    /// Direct supertypes with their applied arguments; an explicit but
    /// unresolvable `extends X` is recorded as `TypeRef::Unknown`.
    pub supertypes: Vec<TypeRef>,
    /// `false` when any supertype is Unknown (a negative subtype proof is impossible).
    pub hierarchy_complete: bool,
    /// `false` when constructors could not be fully enumerated.
    pub constructors_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MemberMetadata {
    pub declaring_class: TypeId,
    pub access: Access,
    pub is_static: bool,
    /// Whether this method is abstract. Always `false` for fields and constructors.
    pub is_abstract: bool,
    /// `None` for fields; `Some(vec![])` for a no-arg callable.
    pub parameters: Option<Vec<TypeRef>>,
    /// Field type / method return; `Void` for constructors.
    pub result: TypeRef,
    pub type_parameters: Vec<TypeParameter>,
    pub is_varargs: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widens_to_covers_jls_widening_table() {
        assert!(PrimitiveType::Int.widens_to(PrimitiveType::Long));
        assert!(!PrimitiveType::Long.widens_to(PrimitiveType::Int));
        assert!(!PrimitiveType::Char.widens_to(PrimitiveType::Short));
        assert!(!PrimitiveType::Byte.widens_to(PrimitiveType::Char));
    }

    #[test]
    fn substitute_replaces_only_matching_variable_by_owner_and_index() {
        let target = TypeVariableId {
            owner: "demo.Box".to_string(),
            index: 0,
        };
        let other = TypeVariableId {
            owner: "demo.Box".to_string(),
            index: 1,
        };
        let unrelated = TypeVariableId {
            owner: "demo.Other".to_string(),
            index: 0,
        };
        let env = vec![(target.clone(), TypeRef::named("demo.User"))];

        // Nested inside Named args.
        let named = TypeRef::Named {
            id: TypeId::named("java.util.Map"),
            args: vec![
                TypeRef::Variable(target.clone()),
                TypeRef::Variable(other.clone()),
            ],
        };
        assert_eq!(
            named.substitute(&env),
            TypeRef::Named {
                id: TypeId::named("java.util.Map"),
                args: vec![
                    TypeRef::named("demo.User"),
                    TypeRef::Variable(other.clone())
                ],
            }
        );

        // Nested inside Array.
        let array = TypeRef::Array(Box::new(TypeRef::Variable(target.clone())));
        assert_eq!(
            array.substitute(&env),
            TypeRef::Array(Box::new(TypeRef::named("demo.User")))
        );

        // Nested inside Wildcard bounds.
        let wildcard = TypeRef::Wildcard {
            upper: Some(Box::new(TypeRef::Variable(target.clone()))),
            lower: Some(Box::new(TypeRef::Variable(unrelated.clone()))),
        };
        assert_eq!(
            wildcard.substitute(&env),
            TypeRef::Wildcard {
                upper: Some(Box::new(TypeRef::named("demo.User"))),
                lower: Some(Box::new(TypeRef::Variable(unrelated))),
            }
        );

        // An unbound variable (different owner/index) stays as-is.
        assert_eq!(
            TypeRef::Variable(other).substitute(&env),
            TypeRef::Variable(TypeVariableId {
                owner: "demo.Box".to_string(),
                index: 1
            })
        );
    }

    #[test]
    fn contains_unknown_detects_nested_unknown_args() {
        let ty = TypeRef::Named {
            id: TypeId::named("java.util.List"),
            args: vec![TypeRef::Unknown],
        };
        assert!(ty.contains_unknown());
        assert!(!TypeRef::named("java.util.List").contains_unknown());
    }
}
