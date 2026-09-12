//! Bounded parser for untrusted JVM generic signatures (JVMS §4.7.9.1).
//! Malformed input returns `None`/empty; display templates use `{i}` for class parameters.

use crate::{PrimitiveType, TypeId, TypeParameter, TypeRef, TypeVariableId};

/// Recursion cap on nested generic types (`List<List<List<...>>>`, array
/// nesting, etc.) — bounds stack depth against adversarial `.class` bytes.
const MAX_SIG_DEPTH: usize = 32;

/// Names of a class signature's formal type parameters, e.g. `["E"]` for
/// `<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;…`.
pub fn class_type_params(sig: &str) -> Vec<String> {
    SigParser::new(sig.as_bytes(), &[]).type_params()
}

/// The method's own type parameters (by name), plus its
/// `(return, [param, …])` with `{i}` placeholders for the class's type
/// parameters. `None` on any parse failure.
pub fn method_template(
    sig: &str,
    class_params: &[String],
) -> Option<(Vec<String>, String, Vec<String>)> {
    let mut p = SigParser::new(sig.as_bytes(), class_params);
    // The method's own type parameters render by name, not substituted, and
    // SHADOW same-named class type parameters (e.g. `class Box<T> { <T> T
    // foo(T x) }`), so `T` here always means the method's own.
    let method_type_params = p.type_params();
    p.shadow = method_type_params.clone();
    p.expect(b'(')?;
    let mut params = Vec::new();
    while p.peek()? != b')' {
        params.push(p.type_render()?);
    }
    p.expect(b')')?;
    let ret = if p.peek()? == b'V' {
        p.bump();
        "void".to_string()
    } else {
        p.type_render()?
    };
    Some((method_type_params, ret, params))
}

/// A field's type rendered with `{i}` placeholders.
pub fn field_template(sig: &str, class_params: &[String]) -> Option<String> {
    SigParser::new(sig.as_bytes(), class_params).type_render()
}

/// Parse index-aligned generic arguments for superclass and interfaces.
/// Malformed tails return the clean prefix; callers reject length mismatches.
pub fn super_type_args(sig: &str, class_params: &[String]) -> Vec<Vec<String>> {
    let mut p = SigParser::new(sig.as_bytes(), class_params);
    p.skip_type_params();
    let mut out = Vec::new();
    let mut guard = 0;
    while p.peek() == Some(b'L') {
        guard += 1;
        if guard > 64 {
            break;
        }
        p.bump(); // consume the 'L' that class_type_parts() assumes is gone
        match p.class_type_parts() {
            Some((_, args)) => out.push(args),
            None => break,
        }
    }
    out
}

/// `<T:...>` list → structured parameters owned by `owner`.
pub fn parse_type_params(sig: &str, owner: &str) -> Vec<TypeParameter> {
    let mut p = SigParser::new(sig.as_bytes(), &[]);
    p.type_params_structured(owner)
        .into_iter()
        .map(|(_, tp)| tp)
        .collect()
}

/// Class signature → (own params, structured supertypes: superclass then interfaces).
pub fn parse_class_signature(sig: &str, owner: &str) -> Option<(Vec<TypeParameter>, Vec<TypeRef>)> {
    let mut p = SigParser::new(sig.as_bytes(), &[]);
    let params = p.type_params_structured(owner);
    p.scope = vec![params.clone()];
    let mut supers = Vec::new();
    let mut guard = 0;
    while p.peek() == Some(b'L') {
        guard += 1;
        if guard > 64 {
            return None;
        }
        supers.push(p.type_parse()?);
    }
    Some((params.into_iter().map(|(_, tp)| tp).collect(), supers))
}

/// Method signature → (method params, return, parameter types).
/// `class_params` pairs each class type parameter with the name it was
/// declared under, since a `TE;` in this signature carries only a name,
/// never an index.
pub fn parse_method_signature(
    sig: &str,
    class_params: &[(String, TypeParameter)],
    owner: &str,
) -> Option<(Vec<TypeParameter>, TypeRef, Vec<TypeRef>)> {
    let mut p = SigParser::new(sig.as_bytes(), &[]);
    let mparams = p.type_params_structured(owner);
    p.scope = vec![class_params.to_vec(), mparams.clone()];
    p.expect(b'(')?;
    let mut params = Vec::new();
    while p.peek()? != b')' {
        params.push(p.type_parse()?);
    }
    p.expect(b')')?;
    let ret = if p.peek()? == b'V' {
        p.bump();
        TypeRef::Void
    } else {
        p.type_parse()?
    };
    Some((mparams.into_iter().map(|(_, tp)| tp).collect(), ret, params))
}

/// A field's structured type. See [`parse_method_signature`] for why
/// `class_params` must carry names, not just identity.
pub fn parse_field_signature(
    sig: &str,
    class_params: &[(String, TypeParameter)],
) -> Option<TypeRef> {
    let mut p = SigParser::new(sig.as_bytes(), &[]);
    p.scope = vec![class_params.to_vec()];
    p.type_parse()
}

struct SigParser<'a> {
    s: &'a [u8],
    pos: usize,
    params: &'a [String],
    /// Type-parameter names that SHADOW `params` (the method's own type
    /// parameters): a matching variable renders by name, never as a class
    /// `{i}` placeholder.
    shadow: Vec<String>,
    /// Current nested-type recursion depth (bumped/unwound around
    /// [`SigParser::type_render`]) — see [`MAX_SIG_DEPTH`].
    depth: usize,
    /// Nested type-parameter scopes used by structured parsing, innermost last.
    /// Display parsing uses `params` and `shadow` instead.
    scope: Vec<Vec<(String, TypeParameter)>>,
}

impl<'a> SigParser<'a> {
    fn new(s: &'a [u8], params: &'a [String]) -> Self {
        SigParser {
            s,
            pos: 0,
            params,
            shadow: Vec::new(),
            depth: 0,
            scope: Vec::new(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        (self.bump()? == b).then_some(())
    }

    fn read_until(&mut self, stops: &[u8]) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if stops.contains(&c) {
                break;
            }
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()
    }

    /// Parse a leading `<…>` formal-type-parameter list into its names.
    fn type_params(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.peek() != Some(b'<') {
            return out;
        }
        self.bump();
        let mut guard = 0;
        while let Some(c) = self.peek() {
            guard += 1;
            if c == b'>' || guard > 64 {
                self.bump();
                break;
            }
            out.push(self.read_until(b":"));
            // `:` ClassBound? (`:` InterfaceBound)* — skip the bound types.
            while self.peek() == Some(b':') {
                self.bump();
                if !matches!(self.peek(), Some(b':') | Some(b'>') | None) {
                    let _ = self.type_render();
                }
            }
        }
        out
    }

    fn skip_type_params(&mut self) {
        if self.peek() == Some(b'<') {
            let _ = self.type_params();
        }
    }

    /// Depth-capped entry point for rendering one type (see [`MAX_SIG_DEPTH`]).
    fn type_render(&mut self) -> Option<String> {
        self.depth += 1;
        let result = if self.depth > MAX_SIG_DEPTH {
            None
        } else {
            self.type_render_inner()
        };
        self.depth -= 1;
        result
    }

    fn type_render_inner(&mut self) -> Option<String> {
        match self.bump()? {
            b'B' => Some("byte".into()),
            b'C' => Some("char".into()),
            b'D' => Some("double".into()),
            b'F' => Some("float".into()),
            b'I' => Some("int".into()),
            b'J' => Some("long".into()),
            b'S' => Some("short".into()),
            b'Z' => Some("boolean".into()),
            b'[' => {
                let inner = self.type_render()?;
                Some(format!("{inner}[]"))
            }
            b'T' => {
                let name = self.read_until(b";");
                self.expect(b';')?;
                Some(self.var(&name))
            }
            b'L' => self.class_type(),
            _ => None,
        }
    }

    fn class_type(&mut self) -> Option<String> {
        let (name, args) = self.class_type_parts()?;
        if args.is_empty() {
            Some(name)
        } else {
            Some(format!("{name}<{}>", args.join(", ")))
        }
    }

    /// Parse one `ClassTypeSignature` (`L…;` already consumed by the caller)
    /// into its simple display name and type-argument list. For a nested
    /// `Outer<T>.Inner<X>` chain, only the innermost segment is kept.
    fn class_type_parts(&mut self) -> Option<(String, Vec<String>)> {
        let name = self.read_class_name();
        let mut simple_name = simple(&name);
        let mut args = self.maybe_type_arg_list()?;
        // Nested `.Inner<…>` segments.
        while self.peek() == Some(b'.') {
            self.bump();
            simple_name = simple(&self.read_class_name());
            args = self.maybe_type_arg_list()?;
        }
        self.expect(b';')?;
        Some((simple_name, args))
    }

    fn read_class_name(&mut self) -> String {
        self.read_until(b"<;.")
    }

    fn maybe_type_arg_list(&mut self) -> Option<Vec<String>> {
        if self.peek() == Some(b'<') {
            self.type_arg_list()
        } else {
            Some(Vec::new())
        }
    }

    fn type_arg_list(&mut self) -> Option<Vec<String>> {
        self.expect(b'<')?;
        let mut args = Vec::new();
        while self.peek()? != b'>' {
            args.push(self.type_arg()?);
        }
        self.expect(b'>')?;
        Some(args)
    }

    fn type_arg(&mut self) -> Option<String> {
        match self.peek()? {
            b'*' => {
                self.bump();
                Some("?".into())
            }
            b'+' => {
                self.bump();
                Some(format!("? extends {}", self.type_render()?))
            }
            b'-' => {
                self.bump();
                Some(format!("? super {}", self.type_render()?))
            }
            _ => self.type_render(),
        }
    }

    fn var(&self, name: &str) -> String {
        if self.shadow.iter().any(|p| p == name) {
            return name.to_string();
        }
        match self.params.iter().position(|p| p == name) {
            Some(i) => format!("{{{i}}}"),
            None => name.to_string(),
        }
    }

    /// `<T:...>` list → structured parameters (name, `TypeParameter`) owned
    /// by `owner`. A bound may reference an earlier parameter in the same
    /// list, so each parameter is visible to bounds parsed after it.
    fn type_params_structured(&mut self, owner: &str) -> Vec<(String, TypeParameter)> {
        let mut out = Vec::new();
        if self.peek() != Some(b'<') {
            return out;
        }
        self.bump();
        let mut guard = 0;
        while let Some(c) = self.peek() {
            guard += 1;
            if c == b'>' || guard > 64 {
                self.bump();
                break;
            }
            let name = self.read_until(b":");
            let id = TypeVariableId {
                owner: owner.to_string(),
                index: out.len(),
            };
            // Bounds may refer to earlier params of this same list: push a
            // provisional scope entry so `TE;` inside a bound resolves.
            self.scope.push(out.clone());
            let mut bounds = Vec::new();
            while self.peek() == Some(b':') {
                self.bump();
                if !matches!(self.peek(), Some(b':') | Some(b'>') | None) {
                    bounds.push(self.type_parse().unwrap_or(TypeRef::Unknown));
                }
            }
            self.scope.pop();
            out.push((name, TypeParameter { id, bounds }));
        }
        out
    }

    /// Resolve a type-variable name against in-scope parameter lists;
    /// innermost (e.g. method params) wins over an outer class's same name.
    fn lookup_var(&self, name: &str) -> TypeRef {
        for params in self.scope.iter().rev() {
            if let Some((_, tp)) = params.iter().find(|(n, _)| n == name) {
                return TypeRef::Variable(tp.id.clone());
            }
        }
        TypeRef::Unknown
    }

    /// Depth-capped structured twin of [`SigParser::type_render`].
    fn type_parse(&mut self) -> Option<TypeRef> {
        self.depth += 1;
        let r = if self.depth > MAX_SIG_DEPTH {
            None
        } else {
            self.type_parse_inner()
        };
        self.depth -= 1;
        r
    }

    fn type_parse_inner(&mut self) -> Option<TypeRef> {
        Some(match self.bump()? {
            b'B' => TypeRef::Primitive(PrimitiveType::Byte),
            b'C' => TypeRef::Primitive(PrimitiveType::Char),
            b'D' => TypeRef::Primitive(PrimitiveType::Double),
            b'F' => TypeRef::Primitive(PrimitiveType::Float),
            b'I' => TypeRef::Primitive(PrimitiveType::Int),
            b'J' => TypeRef::Primitive(PrimitiveType::Long),
            b'S' => TypeRef::Primitive(PrimitiveType::Short),
            b'Z' => TypeRef::Primitive(PrimitiveType::Boolean),
            b'[' => TypeRef::Array(Box::new(self.type_parse()?)),
            b'T' => {
                let name = self.read_until(b";");
                self.expect(b';')?;
                self.lookup_var(&name)
            }
            b'L' => {
                let mut binary = self.read_until(b"<;.").replace('/', ".");
                let mut args = self.type_args_structured()?;
                // Nested `Outer<..>.Inner<..>` → `Outer$Inner`.
                while self.peek() == Some(b'.') {
                    self.bump();
                    binary.push('$');
                    binary.push_str(&self.read_until(b"<;."));
                    args = self.type_args_structured()?;
                }
                self.expect(b';')?;
                TypeRef::Named {
                    id: TypeId::Named(binary),
                    args,
                }
            }
            _ => return None,
        })
    }

    fn type_args_structured(&mut self) -> Option<Vec<TypeRef>> {
        if self.peek() != Some(b'<') {
            return Some(Vec::new());
        }
        self.expect(b'<')?;
        let mut args = Vec::new();
        while self.peek()? != b'>' {
            args.push(match self.peek()? {
                b'*' => {
                    self.bump();
                    TypeRef::Wildcard {
                        upper: None,
                        lower: None,
                    }
                }
                b'+' => {
                    self.bump();
                    TypeRef::Wildcard {
                        upper: Some(Box::new(self.type_parse()?)),
                        lower: None,
                    }
                }
                b'-' => {
                    self.bump();
                    TypeRef::Wildcard {
                        upper: None,
                        lower: Some(Box::new(self.type_parse()?)),
                    }
                }
                _ => self.type_parse()?,
            });
        }
        self.expect(b'>')?;
        Some(args)
    }
}

fn simple(binary: &str) -> String {
    binary
        .rsplit('/')
        .next()
        .unwrap_or(binary)
        .replace('$', ".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Vec<String> {
        class_type_params(
            "<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;Ljava/util/List<TE;>;",
        )
    }

    #[test]
    fn parses_class_type_params() {
        assert_eq!(params(), vec!["E".to_string()]);
    }

    #[test]
    fn renders_method_templates_with_slots() {
        let tp = params();
        assert_eq!(
            method_template("(TE;)Z", &tp),
            Some((vec![], "boolean".into(), vec!["{0}".into()]))
        );
        assert_eq!(
            method_template("(I)TE;", &tp),
            Some((vec![], "{0}".into(), vec!["int".into()]))
        );
        assert_eq!(
            method_template("()Ljava/util/ListIterator<TE;>;", &tp),
            Some((vec![], "ListIterator<{0}>".into(), vec![]))
        );
        assert_eq!(
            method_template("(Ljava/util/Collection<+TE;>;)Z", &tp),
            Some((
                vec![],
                "boolean".into(),
                vec!["Collection<? extends {0}>".into()]
            ))
        );
    }

    #[test]
    fn renders_field_template() {
        let tp = vec!["K".to_string(), "V".to_string()];
        assert_eq!(field_template("TV;", &tp).as_deref(), Some("{1}"));
    }

    #[test]
    fn renders_map_put_with_two_slots() {
        // Map<K, V>.put(K, V) -> V
        let tp = vec!["K".to_string(), "V".to_string()];
        assert_eq!(
            method_template("(TK;TV;)TV;", &tp),
            Some((vec![], "{1}".into(), vec!["{0}".into(), "{1}".into()]))
        );
    }

    #[test]
    fn renders_nested_generics() {
        // Map<String, List<Integer>>
        assert_eq!(
            field_template(
                "Ljava/util/Map<Ljava/lang/String;Ljava/util/List<Ljava/lang/Integer;>;>;",
                &[]
            )
            .as_deref(),
            Some("Map<String, List<Integer>>")
        );
    }

    #[test]
    fn renders_wildcard_extends_with_concrete_bound() {
        // List<? extends Number>
        assert_eq!(
            field_template("Ljava/util/List<+Ljava/lang/Number;>;", &[]).as_deref(),
            Some("List<? extends Number>")
        );
    }

    #[test]
    fn renders_wildcard_super_and_unbounded() {
        assert_eq!(
            field_template("Ljava/util/List<-Ljava/lang/Number;>;", &[]).as_deref(),
            Some("List<? super Number>")
        );
        assert_eq!(
            field_template("Ljava/util/List<*>;", &[]).as_deref(),
            Some("List<?>")
        );
    }

    #[test]
    fn renders_type_variable_array() {
        // T[] where T is the class's own type parameter.
        let tp = vec!["E".to_string()];
        assert_eq!(field_template("[TE;", &tp).as_deref(), Some("{0}[]"));
    }

    #[test]
    fn renders_method_type_params_in_signature() {
        // <T> T foo(Class<T> c) — method-level T, no class type params.
        assert_eq!(
            method_template("<T:Ljava/lang/Object;>(Ljava/lang/Class<TT;>;)TT;", &[]),
            Some((vec!["T".to_string()], "T".into(), vec!["Class<T>".into()]))
        );
    }

    #[test]
    fn method_type_param_shadows_class_type_param() {
        // class Box<T> { <T> T foo(T x) } — the method's own T shadows the
        // class's T; T must render by name, not as {0}.
        let tp = vec!["T".to_string()];
        assert_eq!(
            method_template("<T:Ljava/lang/Object;>(TT;)TT;", &tp),
            Some((vec!["T".to_string()], "T".into(), vec!["T".into()]))
        );
        // A class param NOT shadowed by the method still substitutes to its
        // placeholder alongside the shadowed one.
        let tp = vec!["T".to_string(), "U".to_string()];
        assert_eq!(
            method_template("<T:Ljava/lang/Object;>(TT;TU;)TT;", &tp),
            Some((
                vec!["T".to_string()],
                "T".into(),
                vec!["T".into(), "{1}".into()]
            ))
        );
    }

    #[test]
    fn multiple_bounds_and_empty_class_bound_parse() {
        // `<T::Ljava/lang/Comparable;>` — empty class bound, one interface
        // bound (javac emits this for `<T extends Comparable>`).
        let sig = "<T::Ljava/lang/Comparable;>Ljava/lang/Object;";
        let tp = class_type_params(sig);
        assert_eq!(tp, vec!["T".to_string()]);
        assert_eq!(field_template("TT;", &tp).as_deref(), Some("{0}"));
        assert_eq!(super_type_args(sig, &tp), vec![Vec::<String>::new()]);

        // `<T:Ljava/lang/Object;:Ljava/lang/Comparable;>` — class bound plus
        // interface bound (`<T extends Object & Comparable>`).
        let sig = "<T:Ljava/lang/Object;:Ljava/lang/Comparable;>Ljava/lang/Object;";
        let tp = class_type_params(sig);
        assert_eq!(tp, vec!["T".to_string()]);
        assert_eq!(field_template("TT;", &tp).as_deref(), Some("{0}"));

        // Same shapes on a method's own type-parameter list.
        assert_eq!(
            method_template("<T::Ljava/lang/Comparable<TT;>;>(TT;)TT;", &[]),
            Some((vec!["T".to_string()], "T".into(), vec!["T".into()]))
        );
    }

    #[test]
    fn throws_clause_after_return_type_is_ignored() {
        let tp = params(); // ["E"]
        assert_eq!(
            method_template("(TE;)V^Ljava/lang/Exception;", &tp),
            Some((vec![], "void".into(), vec!["{0}".into()]))
        );
        // Non-void return, multiple throws entries (including a type-variable
        // throws `^TX;`) — all safely ignored.
        assert_eq!(
            method_template("()TE;^Ljava/io/IOException;^TX;", &tp),
            Some((vec![], "{0}".into(), vec![]))
        );
    }

    #[test]
    fn super_type_args_maps_class_params_across_hierarchy() {
        // class MyList<T> extends AbstractList<T> implements List<T>, Serializable
        let sig = "<T:Ljava/lang/Object;>Ljava/util/AbstractList<TT;>;Ljava/util/List<TT;>;Ljava/io/Serializable;";
        let tp = class_type_params(sig);
        assert_eq!(tp, vec!["T".to_string()]);
        assert_eq!(
            super_type_args(sig, &tp),
            vec![
                vec!["{0}".to_string()],
                vec!["{0}".to_string()],
                Vec::<String>::new(),
            ]
        );
    }

    #[test]
    fn super_type_args_concrete_instantiation() {
        // class StringList extends AbstractList<String>
        let sig = "Ljava/util/AbstractList<Ljava/lang/String;>;";
        assert_eq!(super_type_args(sig, &[]), vec![vec!["String".to_string()]]);
    }

    // --- Malformed / truncated input: never panics, degrades to None/partial. ---

    #[test]
    fn truncated_type_variable_yields_none() {
        // Missing the terminating `;` after the variable name.
        assert_eq!(field_template("TE", &[]), None);
        assert_eq!(method_template("(I)TE", &[]), None);
    }

    #[test]
    fn truncated_class_type_yields_none() {
        assert_eq!(field_template("Ljava/util/List<TE;", &[]), None);
        assert_eq!(field_template("Ljava/util/List", &[]), None);
    }

    #[test]
    fn empty_and_garbage_signatures_yield_none() {
        assert_eq!(field_template("", &[]), None);
        assert_eq!(field_template("???", &[]), None);
        assert_eq!(method_template("", &[]), None);
        assert_eq!(method_template("not a signature at all", &[]), None);
    }

    #[test]
    fn unbalanced_generic_brackets_yield_none() {
        assert_eq!(
            field_template("Ljava/util/List<Ljava/lang/String;", &[]),
            None
        );
        assert_eq!(
            field_template("Ljava/util/Map<Ljava/lang/String;Ljava/util/List<>;", &[]),
            None
        );
    }

    #[test]
    fn super_type_args_on_malformed_signature_stops_cleanly() {
        // Superclass entry is truncated — no panic, just no entries.
        assert_eq!(
            super_type_args("Ljava/util/AbstractList<TE", &[]),
            Vec::<Vec<String>>::new()
        );
    }

    #[test]
    fn deeply_nested_generics_hit_recursion_cap_without_panicking() {
        // 200 levels of array nesting: well past MAX_SIG_DEPTH (32).
        let deep = format!("{}I", "[".repeat(200));
        assert_eq!(field_template(&deep, &[]), None);

        // Same idea via nested `List<List<List<...int...>>>`.
        let mut nested = "I".to_string();
        for _ in 0..200 {
            nested = format!("Ljava/util/List<{nested}>;");
        }
        assert_eq!(field_template(&nested, &[]), None);
    }

    #[test]
    fn moderately_nested_generics_within_cap_still_render() {
        // A handful of levels — well under the cap — must still render fully.
        let mut nested = "Ljava/lang/Integer;".to_string();
        for _ in 0..5 {
            nested = format!("Ljava/util/List<{nested}>;");
        }
        assert_eq!(
            field_template(&nested, &[]).as_deref(),
            Some("List<List<List<List<List<Integer>>>>>")
        );
    }

    // --- structured parsing (`parse_type_params`/`parse_class_signature`/
    // `parse_method_signature`/`parse_field_signature`) ---

    #[test]
    fn parse_method_signature_resolves_class_type_variable() {
        let e = TypeParameter {
            id: TypeVariableId {
                owner: "java.util.List".to_string(),
                index: 0,
            },
            bounds: vec![],
        };
        let (mparams, ret, params) = parse_method_signature(
            "(TE;)Ljava/util/List<TE;>;",
            &[("E".to_string(), e.clone())],
            "java.util.List",
        )
        .expect("parses");
        assert!(mparams.is_empty());
        assert_eq!(params, vec![TypeRef::Variable(e.id.clone())]);
        assert_eq!(
            ret,
            TypeRef::Named {
                id: TypeId::Named("java.util.List".to_string()),
                args: vec![TypeRef::Variable(e.id)],
            }
        );
    }

    #[test]
    fn parse_method_signature_method_type_param_shadows_class_type_param() {
        // class Box<T> { <T> T foo(T x) } — the method's own T must win.
        let class_t = TypeParameter {
            id: TypeVariableId {
                owner: "demo.Box".to_string(),
                index: 0,
            },
            bounds: vec![],
        };
        let (mparams, ret, params) = parse_method_signature(
            "<T:Ljava/lang/Object;>(TT;)TT;",
            &[("T".to_string(), class_t)],
            "demo.Box#foo()",
        )
        .expect("parses");
        assert_eq!(mparams.len(), 1);
        let method_var = mparams[0].id.clone();
        assert_eq!(params, vec![TypeRef::Variable(method_var.clone())]);
        assert_eq!(ret, TypeRef::Variable(method_var));
    }

    #[test]
    fn parse_field_signature_handles_nested_and_dollar_forms() {
        let k = TypeParameter {
            id: TypeVariableId {
                owner: "test.Owner".to_string(),
                index: 0,
            },
            bounds: vec![],
        };
        let v = TypeParameter {
            id: TypeVariableId {
                owner: "test.Owner".to_string(),
                index: 1,
            },
            bounds: vec![],
        };
        let params = vec![("K".to_string(), k.clone()), ("V".to_string(), v.clone())];
        let dollar =
            parse_field_signature("Ljava/util/Map$Entry<TK;TV;>;", &params).expect("parses");
        let dotted = parse_field_signature("Ljava/util/Map<TK;TV;>.Entry<TK;TV;>;", &params)
            .expect("parses");
        for ty in [&dollar, &dotted] {
            match ty {
                TypeRef::Named { id, args } => {
                    assert_eq!(id, &TypeId::Named("java.util.Map$Entry".to_string()));
                    assert_eq!(
                        args,
                        &vec![
                            TypeRef::Variable(k.id.clone()),
                            TypeRef::Variable(v.id.clone())
                        ]
                    );
                }
                other => panic!("expected Named, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_field_signature_truncated_returns_none() {
        assert_eq!(parse_field_signature("Ljava/util/List<TE;", &[]), None);
    }

    #[test]
    fn parse_field_signature_hits_depth_cap() {
        // 40 nested arrays: past MAX_SIG_DEPTH (32).
        let deep = format!("{}I", "[".repeat(40));
        assert_eq!(parse_field_signature(&deep, &[]), None);
    }

    #[test]
    fn parse_type_params_names_are_display_only() {
        let params = parse_type_params(
            "<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;",
            "demo.MyList",
        );
        assert_eq!(
            params,
            vec![TypeParameter {
                id: TypeVariableId {
                    owner: "demo.MyList".to_string(),
                    index: 0
                },
                bounds: vec![TypeRef::named("java.lang.Object")],
            }]
        );
    }

    #[test]
    fn parse_class_signature_structured_supertypes() {
        let (params, supers) = parse_class_signature(
            "<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;Ljava/util/List<TE;>;",
            "demo.MyList",
        )
        .expect("parses");
        assert_eq!(params.len(), 1);
        let e = params[0].id.clone();
        assert_eq!(
            supers,
            vec![
                TypeRef::Named {
                    id: TypeId::Named("java.util.AbstractList".to_string()),
                    args: vec![TypeRef::Variable(e.clone())]
                },
                TypeRef::Named {
                    id: TypeId::Named("java.util.List".to_string()),
                    args: vec![TypeRef::Variable(e)]
                },
            ]
        );
    }
}
