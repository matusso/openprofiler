//! Type descriptors (`Lcom/example/Foo;`, `[I`) and generic signatures
//! (`Ljava/util/List<Ljava/lang/String;>;`).

/// `Lcom/example/Foo$Bar;` → `com.example.Foo$Bar`; primitives and arrays as Java source.
pub fn java_name(desc: &str) -> String {
    let dims = desc.bytes().take_while(|&b| b == b'[').count();
    let base = &desc[dims..];
    let mut s = match base.as_bytes().first() {
        Some(b'L') => base[1..].trim_end_matches(';').replace('/', "."),
        Some(&b) => primitive_name(b).unwrap_or("?").to_owned(),
        None => "?".to_owned(),
    };
    for _ in 0..dims {
        s.push_str("[]");
    }
    s
}

pub fn primitive_name(b: u8) -> Option<&'static str> {
    Some(match b {
        b'V' => "void",
        b'Z' => "boolean",
        b'B' => "byte",
        b'S' => "short",
        b'C' => "char",
        b'I' => "int",
        b'J' => "long",
        b'F' => "float",
        b'D' => "double",
        _ => return None,
    })
}

pub fn is_primitive(desc: &str) -> bool {
    desc.len() == 1 && primitive_name(desc.as_bytes()[0]).is_some()
}

pub fn is_reference(desc: &str) -> bool {
    desc.starts_with('L') || desc.starts_with('[')
}

pub fn is_wide(desc: &str) -> bool {
    desc == "J" || desc == "D"
}

/// Element type of an array descriptor.
pub fn element(desc: &str) -> Option<&str> {
    desc.strip_prefix('[')
}

/// Package of a class descriptor in dotted form (`""` for the default package).
pub fn package_of(desc: &str) -> String {
    let n = java_name(desc);
    n.rfind('.').map_or_else(String::new, |i| n[..i].to_owned())
}

/// Simple binary name: `com.example.Foo$Bar` → `Foo$Bar`.
pub fn binary_simple_name(desc: &str) -> String {
    let n = java_name(desc);
    n.rfind('.').map_or(n.clone(), |i| n[i + 1..].to_owned())
}

/// Splits a method descriptor `(IJ)V` into parameter descriptors and the return descriptor.
pub fn split_method_desc(desc: &str) -> (Vec<String>, String) {
    let Some(rest) = desc.strip_prefix('(') else { return (vec![], "V".into()) };
    let Some(close) = rest.find(')') else { return (vec![], "V".into()) };
    let (params, ret) = (&rest[..close], &rest[close + 1..]);
    let mut out = vec![];
    let mut i = 0;
    let b = params.as_bytes();
    while i < b.len() {
        let start = i;
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        if i < b.len() && b[i] == b'L' {
            while i < b.len() && b[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        out.push(params[start..i.min(b.len())].to_owned());
    }
    (out, ret.to_owned())
}

/// A parsed generic signature type, rendered through a class-name mapper so imports apply.
#[derive(Debug, Clone, PartialEq)]
pub enum Sig {
    Prim(u8),
    /// Class with nested parts: `Outer<A>.Inner<B>`; each part is (binary name, type args).
    Class(Vec<(String, Vec<SigArg>)>),
    Var(String),
    Array(Box<Sig>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SigArg {
    Any,
    Extends(Sig),
    Super(Sig),
    Exact(Sig),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TypeParam {
    pub name: String,
    pub bounds: Vec<Sig>,
}

/// Class signature: type parameters, superclass, interfaces.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClassSig {
    pub params: Vec<TypeParam>,
    pub supers: Vec<Sig>,
}

/// Method signature: type parameters, parameters, return type, throws.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MethodSig {
    pub params: Vec<TypeParam>,
    pub args: Vec<Sig>,
    pub ret: Option<Sig>,
    pub throws: Vec<Sig>,
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        (self.peek()? == b).then(|| self.i += 1)
    }

    fn ident(&mut self, stops: &[u8]) -> Option<String> {
        let start = self.i;
        while !stops.contains(&self.peek()?) {
            self.i += 1;
        }
        Some(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    fn ty(&mut self) -> Option<Sig> {
        match self.peek()? {
            b'[' => {
                self.i += 1;
                Some(Sig::Array(Box::new(self.ty()?)))
            }
            b'T' => {
                self.i += 1;
                let n = self.ident(b";")?;
                self.eat(b';')?;
                Some(Sig::Var(n))
            }
            b'L' => {
                self.i += 1;
                let mut parts = vec![];
                let mut name = self.ident(b"<;.")?;
                loop {
                    let args = if self.peek()? == b'<' { self.args()? } else { vec![] };
                    parts.push((name.replace('/', "."), args));
                    match self.peek()? {
                        b'.' => {
                            self.i += 1;
                            name = self.ident(b"<;.")?;
                        }
                        b';' => {
                            self.i += 1;
                            return Some(Sig::Class(parts));
                        }
                        _ => return None,
                    }
                }
            }
            b => {
                primitive_name(b)?;
                self.i += 1;
                Some(Sig::Prim(b))
            }
        }
    }

    fn args(&mut self) -> Option<Vec<SigArg>> {
        self.eat(b'<')?;
        let mut out = vec![];
        while self.peek()? != b'>' {
            out.push(match self.peek()? {
                b'*' => {
                    self.i += 1;
                    SigArg::Any
                }
                b'+' => {
                    self.i += 1;
                    SigArg::Extends(self.ty()?)
                }
                b'-' => {
                    self.i += 1;
                    SigArg::Super(self.ty()?)
                }
                _ => SigArg::Exact(self.ty()?),
            });
        }
        self.i += 1;
        Some(out)
    }

    fn type_params(&mut self) -> Option<Vec<TypeParam>> {
        if self.peek() != Some(b'<') {
            return Some(vec![]);
        }
        self.i += 1;
        let mut out = vec![];
        while self.peek()? != b'>' {
            let name = self.ident(b":")?;
            let mut bounds = vec![];
            while self.peek() == Some(b':') {
                self.i += 1;
                if matches!(self.peek()?, b'L' | b'T' | b'[') {
                    bounds.push(self.ty()?);
                }
            }
            out.push(TypeParam { name, bounds });
        }
        self.i += 1;
        Some(out)
    }
}

pub fn parse_field_sig(s: &str) -> Option<Sig> {
    let mut p = P { s: s.as_bytes(), i: 0 };
    let t = p.ty()?;
    (p.i == s.len()).then_some(t)
}

pub fn parse_class_sig(s: &str) -> Option<ClassSig> {
    let mut p = P { s: s.as_bytes(), i: 0 };
    let params = p.type_params()?;
    let mut supers = vec![];
    while p.peek().is_some() {
        supers.push(p.ty()?);
    }
    Some(ClassSig { params, supers })
}

pub fn parse_method_sig(s: &str) -> Option<MethodSig> {
    let mut p = P { s: s.as_bytes(), i: 0 };
    let params = p.type_params()?;
    p.eat(b'(')?;
    let mut args = vec![];
    while p.peek()? != b')' {
        args.push(p.ty()?);
    }
    p.i += 1;
    let ret = if p.peek()? == b'V' {
        p.i += 1;
        None
    } else {
        Some(p.ty()?)
    };
    let mut throws = vec![];
    while p.eat(b'^').is_some() {
        throws.push(p.ty()?);
    }
    Some(MethodSig { params, args, ret, throws })
}

impl Sig {
    /// Renders as Java source; `class` maps a dotted binary class name to how it should be printed.
    pub fn render(&self, class: &mut dyn FnMut(&str) -> String) -> String {
        match self {
            Sig::Prim(b) => primitive_name(*b).unwrap_or("?").to_owned(),
            Sig::Var(v) => v.clone(),
            Sig::Array(t) => format!("{}[]", t.render(class)),
            Sig::Class(parts) => {
                let mut out = String::new();
                let mut binary = String::new();
                for (i, (name, args)) in parts.iter().enumerate() {
                    if i == 0 {
                        binary = name.clone();
                        out = class(&binary);
                    } else {
                        binary = format!("{binary}${name}");
                        out = format!("{out}.{name}");
                    }
                    if !args.is_empty() {
                        let a: Vec<String> = args.iter().map(|a| a.render(class)).collect();
                        out = format!("{out}<{}>", a.join(", "));
                    }
                }
                out
            }
        }
    }

    /// The erased class descriptor, for imports and links.
    pub fn erased(&self) -> Option<String> {
        match self {
            Sig::Class(parts) => {
                let mut n = parts[0].0.clone();
                for (p, _) in &parts[1..] {
                    n = format!("{n}${p}");
                }
                Some(format!("L{};", n.replace('.', "/")))
            }
            Sig::Array(t) => t.erased().map(|e| format!("[{e}")),
            _ => None,
        }
    }

    /// Every class descriptor mentioned (for imports).
    pub fn classes(&self, out: &mut Vec<String>) {
        match self {
            Sig::Class(parts) => {
                out.extend(self.erased());
                for (_, args) in parts {
                    for a in args {
                        if let SigArg::Exact(t) | SigArg::Extends(t) | SigArg::Super(t) = a {
                            t.classes(out);
                        }
                    }
                }
            }
            Sig::Array(t) => t.classes(out),
            _ => {}
        }
    }
}

impl SigArg {
    fn render(&self, class: &mut dyn FnMut(&str) -> String) -> String {
        match self {
            SigArg::Any => "?".into(),
            SigArg::Extends(t) => format!("? extends {}", t.render(class)),
            SigArg::Super(t) => format!("? super {}", t.render(class)),
            SigArg::Exact(t) => t.render(class),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors() {
        assert_eq!(java_name("Lcom/a/B$C;"), "com.a.B$C");
        assert_eq!(java_name("[[I"), "int[][]");
        assert_eq!(
            split_method_desc("(I[JLjava/lang/String;)V"),
            (vec!["I".into(), "[J".into(), "Ljava/lang/String;".into()], "V".into())
        );
        assert_eq!(package_of("Lcom/a/B;"), "com.a");
        assert_eq!(package_of("LB;"), "");
    }

    #[test]
    fn signatures() {
        let mut plain = |n: &str| n.rsplit('.').next().unwrap().to_owned();
        let t = parse_field_sig("Ljava/util/Map<Ljava/lang/String;+Ljava/util/List<*>;>;").unwrap();
        assert_eq!(t.render(&mut plain), "Map<String, ? extends List<?>>");
        let m = parse_method_sig("<T::Ljava/lang/Comparable<TT;>;>(Ljava/util/List<TT;>;)TT;").unwrap();
        assert_eq!(m.params[0].name, "T");
        assert_eq!(m.params[0].bounds[0].render(&mut plain), "Comparable<T>");
        assert_eq!(m.ret.unwrap().render(&mut plain), "T");
        let c = parse_class_sig("Ljava/lang/Object;Ljava/lang/Comparable<Lcom/a/B;>;").unwrap();
        assert_eq!(c.supers.len(), 2);
        let inner = parse_field_sig("Lcom/a/Outer<TT;>.Inner<Ljava/lang/String;>;").unwrap();
        assert_eq!(inner.erased().unwrap(), "Lcom/a/Outer$Inner;");
        assert!(parse_field_sig("Ljava/util/List<").is_none());
    }
}
