//! Minimal parser for JVM generic signatures (JVMS §4.7.9.1).
//!
//! It renders types to readable Java, replacing references to the **class's**
//! formal type parameters with `{0}`, `{1}`, … placeholders, so a caller can
//! substitute the actual type arguments from a use site (`ArrayList<String>`).
//! Method-level type parameters and unresolved variables render by name.

/// Names of a class signature's formal type parameters, e.g. `["E"]` for
/// `<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;…`.
pub(crate) fn class_type_params(sig: &str) -> Vec<String> {
    SigParser::new(sig.as_bytes(), &[]).type_params()
}

/// A method's `(return, [param, …])` rendered with `{i}` placeholders.
pub(crate) fn method_template(sig: &str, class_params: &[String]) -> Option<(String, Vec<String>)> {
    let mut p = SigParser::new(sig.as_bytes(), class_params);
    p.skip_type_params(); // method-level type params — not substituted
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
    Some((ret, params))
}

/// A field's type rendered with `{i}` placeholders.
pub(crate) fn field_template(sig: &str, class_params: &[String]) -> Option<String> {
    SigParser::new(sig.as_bytes(), class_params).type_render()
}

struct SigParser<'a> {
    s: &'a [u8],
    pos: usize,
    params: &'a [String],
}

impl<'a> SigParser<'a> {
    fn new(s: &'a [u8], params: &'a [String]) -> Self {
        SigParser { s, pos: 0, params }
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

    fn type_render(&mut self) -> Option<String> {
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
        let name = self.read_class_name();
        let mut out = simple(&name);
        if self.peek() == Some(b'<') {
            out = format!("{out}<{}>", self.type_args()?);
        }
        // Nested `.Inner<…>` segments.
        while self.peek() == Some(b'.') {
            self.bump();
            out = simple(&self.read_class_name());
            if self.peek() == Some(b'<') {
                out = format!("{out}<{}>", self.type_args()?);
            }
        }
        self.expect(b';')?;
        Some(out)
    }

    fn read_class_name(&mut self) -> String {
        self.read_until(b"<;.")
    }

    fn type_args(&mut self) -> Option<String> {
        self.expect(b'<')?;
        let mut args = Vec::new();
        while self.peek()? != b'>' {
            args.push(self.type_arg()?);
        }
        self.expect(b'>')?;
        Some(args.join(", "))
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
        match self.params.iter().position(|p| p == name) {
            Some(i) => format!("{{{i}}}"),
            None => name.to_string(),
        }
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
            Some(("boolean".into(), vec!["{0}".into()]))
        );
        assert_eq!(
            method_template("(I)TE;", &tp),
            Some(("{0}".into(), vec!["int".into()]))
        );
        assert_eq!(
            method_template("()Ljava/util/ListIterator<TE;>;", &tp),
            Some(("ListIterator<{0}>".into(), vec![]))
        );
        assert_eq!(
            method_template("(Ljava/util/Collection<+TE;>;)Z", &tp),
            Some(("boolean".into(), vec!["Collection<? extends {0}>".into()]))
        );
    }

    #[test]
    fn renders_field_template() {
        let tp = vec!["K".to_string(), "V".to_string()];
        assert_eq!(field_template("TV;", &tp).as_deref(), Some("{1}"));
    }
}
