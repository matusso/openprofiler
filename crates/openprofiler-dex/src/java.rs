//! Java source generation: class declarations, members and structured method bodies, with
//! navigation links (references and declarations) recorded as the text is written.

use crate::dex::{self, Annotation, DexFile, EncodedMethod, Value};
use crate::ir::*;
use crate::method::{self, Body, MethodInput};
use crate::program::{NodeKey, Program};
use crate::structure::{self, Catch, LoopKind, Node, Structured};
use crate::types::{self, Sig, SigArg};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

pub struct Printed {
    pub code: String,
    /// `(byte offset, byte length, node, is declaration)`.
    pub links: Vec<(usize, usize, NodeKey, bool)>,
}

const KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "true",
    "false",
    "null",
    "var",
    "record",
    "yield",
];

/// A valid Java identifier for a dex name (d8 and obfuscators produce names like `a-b` or `if`).
pub fn ident(name: &str) -> std::borrow::Cow<'_, str> {
    if valid_ident(name) {
        return name.into();
    }
    let mut out: String =
        name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '$' { c } else { '_' }).collect();
    if out.is_empty() || out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out.into()
}

pub fn escape_java(s: &str, quote: char) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            '\\' => o.push_str("\\\\"),
            c if c == quote => {
                o.push('\\');
                o.push(c);
            }
            c if (c as u32) < 0x20
                || (0x7f..0xa0).contains(&(c as u32))
                || c == '\u{2028}'
                || c == '\u{2029}' =>
            {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

fn float_lit(v: f64, suffix: &str, class: &str) -> String {
    if v.is_nan() {
        return format!("{class}.NaN");
    }
    if v.is_infinite() {
        return format!("{class}.{}_INFINITY", if v > 0.0 { "POSITIVE" } else { "NEGATIVE" });
    }
    let s = if suffix == "f" { format!("{}", v as f32) } else { format!("{v}") };
    let s = if s.contains('.') || s.contains('e') || s.contains("inf") { s } else { format!("{s}.0") };
    format!("{s}{suffix}")
}

/// Generated variable name for a type.
fn base_name(desc: &str, simple: &dyn Fn(&str) -> String) -> String {
    if let Some(el) = desc.strip_prefix('[') {
        let b = base_name(el, simple);
        return if types::is_primitive(el) {
            format!("{b}Arr")
        } else {
            format!("{}Arr", b.trim_end_matches("Arr"))
        };
    }
    let n = match desc {
        "Z" => "z",
        "B" => "b",
        "S" => "s",
        "C" => "c",
        "I" => "i",
        "J" => "j",
        "F" => "f",
        "D" => "d",
        "Ljava/lang/String;" => "str",
        "Ljava/lang/Object;" => "obj",
        "Ljava/lang/Integer;" => "num",
        "Ljava/lang/Class;" => "cls",
        "Ljava/lang/Throwable;" => "th",
        "Ljava/util/Iterator;" => "it",
        "Ljava/lang/StringBuilder;" => "sb",
        _ => {
            let s = simple(desc);
            // Last name part; for `Outer$method$1` the part before the number.
            let parts: Vec<&str> = s.split(['.', '$']).filter(|p| !p.is_empty()).collect();
            let s = parts
                .iter()
                .rev()
                .find(|p| !p.starts_with(|c: char| c.is_ascii_digit()))
                .copied()
                .unwrap_or("obj")
                .to_owned();
            if s.ends_with("Exception") || s.ends_with("Error") {
                return "e".into();
            }
            let mut c = s.chars();
            let lower = match c.next() {
                Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
                None => "obj".into(),
            };
            return if KEYWORDS.contains(&lower.as_str())
                || lower.is_empty()
                || !lower.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
            {
                format!("{lower}Var").chars().filter(|c| c.is_alphanumeric() || *c == '_').collect()
            } else {
                lower
            };
        }
    };
    n.into()
}

struct Writer<'p> {
    p: &'p Program,
    top: Arc<str>,
    pkg: String,
    out: String,
    links: Vec<(usize, usize, NodeKey, bool)>,
    imports: BTreeMap<String, String>,
    /// Simple names that must not be imported (nested classes of the file, same-package classes).
    reserved: HashSet<String>,
    indent: usize,
    /// Class whose members are being printed.
    cls: Arc<str>,
}

pub fn print_class(p: &Program, top: &str) -> Printed {
    let pkg = types::package_of(top);
    let mut w = Writer {
        p,
        top: top.into(),
        pkg: pkg.clone(),
        out: String::new(),
        links: vec![],
        imports: BTreeMap::new(),
        reserved: HashSet::new(),
        indent: 0,
        cls: top.into(),
    };
    w.reserved.insert(p.simple_name(top));
    let mut stack = vec![Arc::<str>::from(top)];
    while let Some(c) = stack.pop() {
        if let Some(m) = p.members.get(&c) {
            for x in m {
                w.reserved.insert(p.simple_name(x));
                stack.push(x.clone());
            }
        }
    }
    w.class(top);
    // Header: package, imports, then shift link offsets.
    let mut head = String::new();
    if !pkg.is_empty() {
        let _ = writeln!(head, "package {pkg};\n");
    }
    if !w.imports.is_empty() {
        let mut im: Vec<&String> = w.imports.values().collect();
        im.sort();
        for i in im {
            let _ = writeln!(head, "import {i};");
        }
        head.push('\n');
    }
    if let Some((d, c)) = p.class(top) {
        let src = c.source_file.map(|s| d.string(s)).unwrap_or("");
        let _ = writeln!(
            head,
            "/* loaded from: {}{} */",
            d.name,
            if src.is_empty() { String::new() } else { format!(", compiled from: {src}") }
        );
    }
    let shift = head.len();
    head.push_str(&w.out);
    Printed { code: head, links: w.links.into_iter().map(|(s, l, k, d)| (s + shift, l, k, d)).collect() }
}

impl<'p> Writer<'p> {
    fn line(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn w(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn link(&mut self, text: &str, key: Option<NodeKey>, decl: bool) {
        let start = self.out.len();
        self.out.push_str(text);
        if let Some(k) = key {
            self.links.push((start, text.len(), k, decl));
        }
    }

    fn defined(&self, desc: &str) -> Option<NodeKey> {
        self.p.classes.get_key_value(desc).map(|(k, _)| NodeKey::Class(k.clone()))
    }

    // ------------------------------------------------------------ type names

    /// How a top-level (or library) class is named in this file, importing it when possible.
    fn top_name(&mut self, java: &str) -> String {
        let (pkg, simple) = match java.rfind('.') {
            Some(i) => (&java[..i], &java[i + 1..]),
            None => ("", java),
        };
        let clean = ident(simple);
        if clean != simple {
            // Not importable under its own name (Kotlin's `-Foo`): write it qualified.
            return if pkg.is_empty() { clean.into_owned() } else { format!("{pkg}.{clean}") };
        }
        if pkg == self.pkg {
            return simple.to_owned();
        }
        if self.reserved.contains(simple) {
            return java.to_owned();
        }
        if let Some(full) = self.imports.get(simple) {
            return if full == java { simple.to_owned() } else { java.to_owned() };
        }
        // A same-package class with that simple name would shadow an import.
        let same_pkg = if self.pkg.is_empty() {
            format!("L{simple};")
        } else {
            format!("L{}/{simple};", self.pkg.replace('.', "/"))
        };
        if self.p.classes.contains_key(same_pkg.as_str()) {
            return java.to_owned();
        }
        if pkg == "java.lang" {
            return simple.to_owned();
        }
        if pkg.is_empty() {
            return simple.to_owned();
        }
        self.imports.insert(simple.to_owned(), java.to_owned());
        simple.to_owned()
    }

    /// Printed name for a class descriptor (`Foo`, `Outer.Inner`, `java.util.Map.Entry`).
    fn class_name(&mut self, desc: &str) -> String {
        if self.p.classes.contains_key(desc) {
            let mut parts = vec![ident(&self.p.simple_name(desc)).into_owned()];
            let mut cur: Arc<str> = desc.into();
            while let Some(i) = self.p.inner.get(&cur) {
                cur = i.outer.clone();
                if cur == self.top {
                    return parts.into_iter().rev().collect::<Vec<_>>().join(".");
                }
                parts.push(ident(&self.p.simple_name(&cur)).into_owned());
            }
            if cur == self.top && parts.len() == 1 {
                return parts.pop().unwrap_or_default();
            }
            let top = self.top_name(&types::java_name(&cur));
            parts.pop();
            parts.reverse();
            return std::iter::once(top).chain(parts).collect::<Vec<_>>().join(".");
        }
        // Library class: `a/b/Outer$Inner` → import Outer, print Outer.Inner.
        let java = types::java_name(desc);
        let (pkg, simple) = java.rsplit_once('.').unwrap_or(("", &java));
        let mut segs: Vec<&str> = simple.split('$').collect();
        if segs.len() > 1
            && segs[1..]
                .iter()
                .all(|s| !s.is_empty() && !s.chars().next().is_some_and(|c| c.is_ascii_digit()))
        {
            let outer = if pkg.is_empty() { segs[0].to_owned() } else { format!("{pkg}.{}", segs[0]) };
            let first = self.top_name(&outer);
            segs.remove(0);
            return std::iter::once(first.as_str()).chain(segs).collect::<Vec<_>>().join(".");
        }
        self.top_name(&java)
    }

    /// Writes a type from a descriptor, linking class names.
    fn ty(&mut self, desc: &str) {
        let dims = desc.bytes().take_while(|&b| b == b'[').count();
        let base = &desc[dims..];
        if base.starts_with('L') {
            let n = self.class_name(base);
            let k = self.defined(base);
            self.link(&n, k, false);
        } else {
            self.w(base.as_bytes().first().and_then(|&b| types::primitive_name(b)).unwrap_or("Object"));
        }
        for _ in 0..dims {
            self.w("[]");
        }
    }

    fn sig(&mut self, s: &Sig) {
        match s {
            Sig::Prim(b) => self.w(types::primitive_name(*b).unwrap_or("?")),
            Sig::Var(v) => self.w(v),
            Sig::Array(t) => {
                self.sig(t);
                self.w("[]");
            }
            Sig::Class(parts) => {
                let desc = s.erased().unwrap_or_default();
                // Name of the outermost part, then nested parts with their own type arguments.
                let first_desc = format!("L{};", parts[0].0.replace('.', "/"));
                if parts.len() == 1 {
                    let n = self.class_name(&desc);
                    let k = self.defined(&desc);
                    self.link(&n, k, false);
                } else {
                    let n = self.class_name(&first_desc);
                    let k = self.defined(&first_desc);
                    self.link(&n, k, false);
                }
                for (i, (name, args)) in parts.iter().enumerate() {
                    if i > 0 {
                        self.w(".");
                        self.w(name);
                    }
                    if !args.is_empty() {
                        self.w("<");
                        for (j, a) in args.iter().enumerate() {
                            if j > 0 {
                                self.w(", ");
                            }
                            match a {
                                SigArg::Any => self.w("?"),
                                SigArg::Extends(t) => {
                                    self.w("? extends ");
                                    self.sig(t);
                                }
                                SigArg::Super(t) => {
                                    self.w("? super ");
                                    self.sig(t);
                                }
                                SigArg::Exact(t) => self.sig(t),
                            }
                        }
                        self.w(">");
                    }
                }
            }
        }
    }

    fn type_params(&mut self, params: &[types::TypeParam]) {
        if params.is_empty() {
            return;
        }
        self.w("<");
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            self.w(&p.name);
            let bounds: Vec<&Sig> =
                p.bounds.iter().filter(|b| b.erased().as_deref() != Some("Ljava/lang/Object;")).collect();
            for (j, b) in bounds.iter().enumerate() {
                self.w(if j == 0 { " extends " } else { " & " });
                self.sig(b);
            }
        }
        self.w(">");
    }

    // ------------------------------------------------------------ declarations

    fn annotations(&mut self, d: &DexFile, set: &[Annotation], inline: bool) {
        for a in set {
            if a.visibility == dex::VISIBILITY_SYSTEM {
                continue;
            }
            let ty = d.type_desc(a.value.ty).to_owned();
            // Kotlin compiler metadata, usually stripped of its values by R8: just noise.
            if matches!(ty.as_str(), "Lkotlin/Metadata;" | "Lkotlin/coroutines/jvm/internal/DebugMetadata;") {
                continue;
            }
            self.w("@");
            self.ty(&ty);
            if !a.value.elements.is_empty() {
                self.w("(");
                for (i, (n, v)) in a.value.elements.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    let name = d.string(*n);
                    if !(name == "value" && a.value.elements.len() == 1) {
                        self.w(name);
                        self.w(" = ");
                    }
                    self.value(d, v);
                }
                self.w(")");
            }
            if inline {
                self.w(" ");
            } else {
                self.line();
            }
        }
    }

    fn value(&mut self, d: &DexFile, v: &Value) {
        match v {
            Value::Byte(x) => self.w(&x.to_string()),
            Value::Short(x) => self.w(&x.to_string()),
            Value::Char(x) => self.w(&char_lit(*x as i64)),
            Value::Int(x) => self.w(&x.to_string()),
            Value::Long(x) => self.w(&format!("{x}L")),
            Value::Float(x) => self.w(&float_lit(*x as f64, "f", "Float")),
            Value::Double(x) => self.w(&float_lit(*x, "d", "Double")),
            Value::String(s) => self.w(&format!("\"{}\"", escape_java(d.string(*s), '"'))),
            Value::Type(t) => {
                let t = d.type_desc(*t).to_owned();
                self.ty(&t);
                self.w(".class");
            }
            Value::Enum(f) | Value::Field(f) => {
                if let Some(fi) = d.field(*f) {
                    let (c, n) = (d.type_desc(fi.class).to_owned(), d.string(fi.name).to_owned());
                    self.ty(&c);
                    self.w(".");
                    let k = self.p.resolve_field(&c, &n);
                    self.link(&n, k, false);
                }
            }
            Value::Method(m) => {
                let r = method::method_ref(d, *m);
                self.w(&format!("/* {}.{} */ null", types::java_name(&r.class), r.name));
            }
            Value::MethodType(_) | Value::MethodHandle(_) => self.w("null"),
            Value::Array(a) => {
                self.w("{");
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.value(d, x);
                }
                self.w("}");
            }
            Value::Annotation(a) => {
                let wrapped = Annotation { visibility: dex::VISIBILITY_RUNTIME, value: a.clone() };
                let start = self.out.len();
                self.annotations(d, std::slice::from_ref(&wrapped), true);
                if self.out.ends_with(' ') && self.out.len() > start {
                    self.out.pop();
                }
            }
            Value::Null => self.w("null"),
            Value::Boolean(b) => self.w(&b.to_string()),
        }
    }

    fn modifiers(&mut self, access: u32, kind: char, in_interface: bool) {
        let mut m = vec![];
        if access & dex::ACC_PUBLIC != 0 && !(in_interface && kind != 'c') {
            m.push("public");
        }
        if access & dex::ACC_PRIVATE != 0 {
            m.push("private");
        }
        if access & dex::ACC_PROTECTED != 0 {
            m.push("protected");
        }
        let iface = access & dex::ACC_INTERFACE != 0;
        let is_enum = access & dex::ACC_ENUM != 0;
        if access & dex::ACC_ABSTRACT != 0
            && !iface
            && !(in_interface && kind == 'm')
            && !(is_enum && kind == 'c')
        {
            m.push("abstract");
        }
        if access & dex::ACC_STATIC != 0
            && !(in_interface && kind == 'f')
            && !(kind == 'c' && (iface || is_enum))
        {
            m.push("static");
        }
        if access & dex::ACC_FINAL != 0 && !(in_interface && kind == 'f') && !(kind == 'c' && is_enum) {
            m.push("final");
        }
        if kind == 'm' {
            if access & dex::ACC_SYNCHRONIZED != 0 {
                m.push("synchronized");
            }
            if access & dex::ACC_NATIVE != 0 {
                m.push("native");
            }
            if access & dex::ACC_STRICT != 0 {
                m.push("strictfp");
            }
        }
        if kind == 'f' {
            if access & dex::ACC_VOLATILE != 0 {
                m.push("volatile");
            }
            if access & dex::ACC_TRANSIENT != 0 {
                m.push("transient");
            }
        }
        for x in m {
            self.w(x);
            self.w(" ");
        }
    }

    fn class(&mut self, desc: &str) {
        let Some((d, c)) = self.p.class(desc) else { return };
        let prev = std::mem::replace(&mut self.cls, desc.into());
        let ann = d.annotations(c).unwrap_or_default();
        let access = self.p.inner.get(desc).map_or(c.access, |i| i.access | (c.access & dex::ACC_INTERFACE));
        let iface = access & dex::ACC_INTERFACE != 0;
        let is_annotation = access & dex::ACC_ANNOTATION != 0;
        let is_enum =
            access & dex::ACC_ENUM != 0 && c.superclass.is_some_and(|s| d.type_desc(s) == "Ljava/lang/Enum;");
        let anonymous = self.p.inner.get(desc).is_some_and(|i| i.name.is_none());
        if anonymous {
            self.w("/* anonymous class */");
            self.line();
        }
        if access & dex::ACC_SYNTHETIC != 0 {
            self.w("/* synthetic */");
            self.line();
        }
        self.annotations(d, &ann.class, false);
        let in_iface = self
            .p
            .inner
            .get(desc)
            .and_then(|i| self.p.class(&i.outer))
            .is_some_and(|(_, oc)| oc.access & dex::ACC_INTERFACE != 0);
        let mut class_access = access & !(if in_iface { dex::ACC_PUBLIC | dex::ACC_STATIC } else { 0 });
        if self.p.has_outer_instance(desc) {
            class_access &= !dex::ACC_STATIC;
        } else if self.p.inner.contains_key(desc) {
            // Nested without an enclosing instance (e.g. anonymous classes from static code).
            class_access |= dex::ACC_STATIC;
        }
        self.modifiers(class_access, 'c', false);
        self.w(if is_annotation {
            "@interface "
        } else if iface {
            "interface "
        } else if is_enum {
            "enum "
        } else {
            "class "
        });
        let name = ident(&self.p.simple_name(desc)).into_owned();
        self.link(
            &name,
            Some(NodeKey::Class(
                self.p.classes.get_key_value(desc).map(|(k, _)| k.clone()).unwrap_or_else(|| desc.into()),
            )),
            true,
        );
        let sig = signature(d, &ann.class).and_then(|s| types::parse_class_sig(&s));
        if let Some(s) = &sig {
            self.type_params(&s.params);
        }
        let mut supers: Vec<(String, Option<Sig>)> = vec![];
        if let Some(s) = c.superclass {
            supers.push((d.type_desc(s).to_owned(), sig.as_ref().and_then(|s| s.supers.first().cloned())));
        }
        for (k, &i) in c.interfaces.iter().enumerate() {
            supers.push((d.type_desc(i).to_owned(), sig.as_ref().and_then(|s| s.supers.get(k + 1).cloned())));
        }
        let (ext, imp): (Vec<_>, Vec<_>) =
            supers.into_iter().enumerate().partition(|(k, _)| *k == 0 && c.superclass.is_some());
        if let Some((_, (sup, ss))) = ext.into_iter().next()
            && sup != "Ljava/lang/Object;"
            && !is_enum
            && !iface
        {
            self.w(" extends ");
            match ss {
                Some(s) => self.sig(&s),
                None => self.ty(&sup),
            }
        }
        let imp: Vec<_> = imp
            .into_iter()
            .map(|x| x.1)
            .filter(|(t, _)| !(is_annotation && t == "Ljava/lang/annotation/Annotation;"))
            .collect();
        if !imp.is_empty() {
            self.w(if iface { " extends " } else { " implements " });
            for (k, (t, s)) in imp.into_iter().enumerate() {
                if k > 0 {
                    self.w(", ");
                }
                match s {
                    Some(s) => self.sig(&s),
                    None => self.ty(&t),
                }
            }
        }
        self.w(" {");
        self.indent += 1;
        let cd = d.class_data(c).unwrap_or_default();
        let statics = d.encoded_array_at(c.static_values_off).unwrap_or_default();

        // Decompile <clinit> first: enum constants and constant fields come from it.
        let clinit = cd
            .direct_methods
            .iter()
            .find(|m| d.method(m.method).is_some_and(|mi| d.string(mi.name) == "<clinit>"));
        let mut clinit_body = clinit.and_then(|m| self.decompile(d, desc, m).ok());
        let mut enum_consts: Vec<(String, Vec<Expr>)> = vec![];
        if is_enum && let Some((body, st)) = &mut clinit_body {
            enum_consts = take_enum_constants(desc, &mut st.nodes);
            let _ = body;
        }
        let mut any = false;
        let consts_found = !enum_consts.is_empty();
        if is_enum && !consts_found {
            // Constants could not be recovered from `<clinit>`; an enum body still needs `;`.
            self.line();
            self.line();
            self.w(";");
            any = true;
        }
        if is_enum && consts_found {
            self.line();
            let n = enum_consts.len();
            for (k, (name, args)) in std::mem::take(&mut enum_consts).into_iter().enumerate() {
                self.line();
                let key = self.p.resolve_field(desc, &name);
                self.link(&name, key, true);
                if !args.is_empty()
                    && let Some((body, _)) = &clinit_body
                {
                    self.w("(");
                    let mut mc = MethodCtx::new(self, body, None);
                    for (j, a) in args.iter().enumerate() {
                        if j > 0 {
                            self.w(", ");
                        }
                        self.expr(&mut mc, a, 0, None);
                    }
                    self.w(")");
                }
                self.w(if k + 1 == n { ";" } else { "," });
            }
            any = true;
        }

        // Fields.
        let mut first_field = true;
        for (k, f) in cd.static_fields.iter().chain(&cd.instance_fields).enumerate() {
            let Some(fi) = d.field(f.field) else { continue };
            let fname = d.string(fi.name).to_owned();
            let fty = d.type_desc(fi.ty).to_owned();
            if is_enum
                && consts_found
                && (f.access & dex::ACC_ENUM != 0
                    || (fname == "$VALUES" && f.access & dex::ACC_SYNTHETIC != 0))
            {
                continue;
            }
            if fname.starts_with("this$")
                && f.access & dex::ACC_SYNTHETIC != 0
                && self.p.has_outer_instance(desc)
            {
                continue;
            }
            if first_field {
                self.line();
                first_field = false;
            }
            self.line();
            if let Some(a) = ann.fields.get(&f.field) {
                self.annotations(d, a, false);
            }
            if f.access & dex::ACC_SYNTHETIC != 0 {
                self.w("/* synthetic */ ");
            }
            self.modifiers(f.access, 'f', iface);
            match ann
                .fields
                .get(&f.field)
                .and_then(|a| signature(d, a))
                .and_then(|s| types::parse_field_sig(&s))
            {
                Some(s) => self.sig(&s),
                None => self.ty(&fty),
            }
            self.w(" ");
            let key = Some(NodeKey::Field(desc.into(), fname.as_str().into()));
            self.link(&ident(&fname), key, true);
            if k < cd.static_fields.len()
                && let Some(v) = statics.get(k)
                && !matches!(v, Value::Null)
            {
                // Only non-default values are initializers in source.
                let default = matches!(
                    v,
                    Value::Int(0)
                        | Value::Long(0)
                        | Value::Byte(0)
                        | Value::Short(0)
                        | Value::Char(0)
                        | Value::Boolean(false)
                ) || matches!(v, Value::Float(x) if *x == 0.0)
                    || matches!(v, Value::Double(x) if *x == 0.0);
                if !default || f.access & dex::ACC_FINAL != 0 {
                    self.w(" = ");
                    self.typed_value(d, v, &fty);
                }
            }
            self.w(";");
            any = true;
        }

        // Methods: static initializer, constructors, then the rest in definition order.
        let mut methods: Vec<&EncodedMethod> = cd.direct_methods.iter().chain(&cd.virtual_methods).collect();
        methods.sort_by_key(|m| {
            let n = d.method(m.method).map(|mi| d.string(mi.name)).unwrap_or("");
            (
                match n {
                    "<clinit>" => 0,
                    "<init>" => 1,
                    _ => 2,
                },
                0,
            )
        });
        for m in methods {
            let Some(mi) = d.method(m.method) else { continue };
            let mname = d.string(mi.name).to_owned();
            let pdesc = d.proto_desc(mi.proto);
            if m.access & dex::ACC_BRIDGE != 0 && m.access & dex::ACC_SYNTHETIC != 0 {
                continue;
            }
            if is_enum
                && ((mname == "values" && m.access & dex::ACC_STATIC != 0 && pdesc.starts_with("()["))
                    || (mname == "valueOf" && pdesc.starts_with("(Ljava/lang/String;)"))
                    || (mname == "$values" && m.access & dex::ACC_SYNTHETIC != 0))
            {
                continue;
            }
            if mname == "<clinit>" {
                if let Some((body, st)) = clinit_body.take() {
                    if st.nodes.is_empty()
                        || (st.nodes.len() == 1 && matches!(st.nodes[0], Node::Return(None)))
                    {
                        continue;
                    }
                    self.line();
                    self.line();
                    self.w("static {");
                    self.indent += 1;
                    self.body(&body, st, None, None);
                    self.indent -= 1;
                    self.line();
                    self.w("}");
                    any = true;
                }
                continue;
            }
            self.method(d, desc, m, &ann, iface, is_enum, anonymous);
            any = true;
        }

        // Nested classes.
        if let Some(members) = self.p.members.get(desc).cloned() {
            for inner in members {
                self.line();
                self.line();
                self.class(&inner);
                any = true;
            }
        }
        self.indent -= 1;
        if any {
            self.line();
        }
        self.w("}");
        if self.indent == 0 {
            self.w("\n");
        }
        self.cls = prev;
    }

    fn typed_value(&mut self, d: &DexFile, v: &Value, ty: &str) {
        match (v, ty) {
            (Value::Int(x), "Z") => self.w(if *x != 0 { "true" } else { "false" }),
            (Value::Int(x), "C") => self.w(&char_lit(*x as i64)),
            _ => self.value(d, v),
        }
    }

    fn decompile(&self, d: &DexFile, desc: &str, m: &EncodedMethod) -> anyhow::Result<(Body, Structured)> {
        let code = d.code(m.code_off)?;
        let debug = d.debug_info(&code).unwrap_or_default();
        let run = || -> anyhow::Result<(Body, Structured)> {
            let mut body = method::lift(&MethodInput {
                dex: d,
                class: desc,
                method: m.method,
                access: m.access,
                code: &code,
                debug: &debug,
            })?;
            let st = structure::structure(&mut body);
            Ok((body, st))
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
            Ok(r) => r,
            Err(e) => {
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                anyhow::bail!("internal error: {msg}")
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn method(
        &mut self,
        d: &DexFile,
        desc: &str,
        m: &EncodedMethod,
        ann: &dex::Annotations,
        iface: bool,
        is_enum: bool,
        anonymous: bool,
    ) {
        let Some(mi) = d.method(m.method) else { return };
        let name = d.string(mi.name).to_owned();
        let pdesc = d.proto_desc(mi.proto);
        let ctor = name == "<init>";
        let (mut params, ret) = types::split_method_desc(&pdesc);
        let manns = ann.methods.get(&m.method).cloned().unwrap_or_default();
        let msig = signature(d, &manns).and_then(|s| types::parse_method_sig(&s));
        let body = if m.code_off != 0 { Some(self.decompile(d, desc, m)) } else { None };
        // Enum constructors drop the synthetic name/ordinal parameters.
        let skip = if is_enum
            && ctor
            && params.len() >= 2
            && params[0] == "Ljava/lang/String;"
            && params[1] == "I"
        {
            2
        } else if ctor
            && self.p.has_outer_instance(desc)
            && params.first().is_some_and(|p| self.p.inner.get(desc).is_some_and(|i| *i.outer == **p))
        {
            // The enclosing instance is implicit in source.
            1
        } else {
            0
        };
        // Default constructor with nothing in it.
        if ctor
            && params.len() == skip
            && !anonymous
            && let Some(Ok((_, st))) = &body
            && st.warning.is_none()
            && strip_enum_super(strip_ctor_prefix(&st.nodes, desc)).is_empty()
            && (m.access & (dex::ACC_PUBLIC | dex::ACC_PRIVATE | dex::ACC_PROTECTED) == 0
                || m.access & dex::ACC_PUBLIC != 0
                || is_enum)
            && manns.iter().all(|a| a.visibility == dex::VISIBILITY_SYSTEM)
        {
            return;
        }
        self.line();
        self.line();
        if m.access & dex::ACC_SYNTHETIC != 0 {
            self.w("/* synthetic */");
            self.line();
        }
        self.annotations(d, &manns, false);
        let mut access = m.access;
        if iface && m.code_off != 0 && m.access & dex::ACC_STATIC == 0 && m.access & dex::ACC_PRIVATE == 0 {
            self.w("default ");
            access &= !dex::ACC_PUBLIC;
        }
        if is_enum && ctor {
            access &= !(dex::ACC_PRIVATE | dex::ACC_PUBLIC | dex::ACC_PROTECTED);
        }
        self.modifiers(access & !dex::ACC_CONSTRUCTOR, 'm', iface);
        if let Some(s) = &msig {
            self.type_params(&s.params);
            if !s.params.is_empty() {
                self.w(" ");
            }
        }
        let key = NodeKey::Method(desc.into(), name.as_str().into(), pdesc.as_str().into());
        if ctor {
            let n = ident(&self.p.simple_name(desc)).into_owned();
            self.link(&n, Some(key), true);
        } else {
            match msig.as_ref().and_then(|s| s.ret.clone()) {
                Some(r) => self.sig(&r),
                None => self.ty(&ret),
            }
            self.w(" ");
            self.link(&ident(&name), Some(key), true);
        }
        self.w("(");
        let lifted = match &body {
            Some(Ok((b, _))) => Some(b),
            _ => None,
        };
        let names: Vec<String> = param_names(self, lifted, &params);
        let generic_args = msig.as_ref().map(|s| s.args.clone()).filter(|a| a.len() == params.len());
        let varargs = m.access & dex::ACC_VARARGS != 0;
        let n = params.len();
        for (k, p) in params.iter().enumerate().skip(skip) {
            if k > skip {
                self.w(", ");
            }
            if let Some(pa) = ann.params.get(&m.method).and_then(|v| v.get(k)) {
                self.annotations(d, pa, true);
            }
            if varargs && k + 1 == n && p.starts_with('[') {
                match generic_args.as_ref().map(|g| g[k].clone()) {
                    Some(Sig::Array(inner)) => self.sig(&inner),
                    _ => self.ty(&p[1..]),
                }
                self.w("...");
            } else {
                match generic_args.as_ref().map(|g| g[k].clone()) {
                    Some(g) => self.sig(&g),
                    None => self.ty(p),
                }
            }
            self.w(" ");
            self.w(&names[k]);
        }
        self.w(")");
        let throws: Vec<String> = msig
            .as_ref()
            .filter(|s| !s.throws.is_empty())
            .map(|s| s.throws.iter().filter_map(Sig::erased).collect())
            .or_else(|| {
                dex::find_annotation(d, &manns, "Ldalvik/annotation/Throws;")
                    .and_then(|a| dex::annotation_value(d, a, "value"))
                    .map(|v| match v {
                        Value::Array(items) => items
                            .iter()
                            .filter_map(|x| {
                                if let Value::Type(t) = x { Some(d.type_desc(*t).to_owned()) } else { None }
                            })
                            .collect(),
                        _ => vec![],
                    })
            })
            .unwrap_or_default();
        if !throws.is_empty() {
            self.w(" throws ");
            for (k, t) in throws.iter().enumerate() {
                if k > 0 {
                    self.w(", ");
                }
                self.ty(t);
            }
        }
        params.truncate(n);
        match body {
            None => self.w(";"),
            Some(Err(e)) => {
                self.w(" {");
                self.indent += 1;
                self.line();
                self.w("/*");
                for l in format!("Method decompilation failed: {e:#}").lines() {
                    self.line();
                    self.w("    ");
                    self.w(l);
                }
                self.line();
                self.w("*/");
                self.line();
                self.w(&format!(
                    "throw new UnsupportedOperationException(\"Method not decompiled: {}.{}\");",
                    escape_java(&types::java_name(desc), '"'),
                    escape_java(&name, '"')
                ));
                self.indent -= 1;
                self.line();
                self.w("}");
            }
            Some(Ok((body, mut st))) => {
                if ctor {
                    st.nodes = strip_ctor_prefix(&st.nodes, desc);
                    if skip == 2 {
                        st.nodes = strip_enum_super(st.nodes);
                    }
                    if skip == 1 {
                        // `this.this$0 = outer;`
                        st.nodes.retain(|n| !matches!(n, Node::Stmt(Stmt::IPut(_, f, _)) if f.name.starts_with("this$") && &*f.class == desc));
                    }
                }
                self.w(" {");
                self.indent += 1;
                let generic_ret =
                    msig.as_ref().and_then(|s| s.ret.clone()).filter(|r| matches!(r, Sig::Var(_)));
                self.body(&body, st, Some(&names), generic_ret);
                self.indent -= 1;
                self.line();
                self.w("}");
            }
        }
    }

    fn body(
        &mut self,
        body: &Body,
        mut st: Structured,
        param_names: Option<&[String]>,
        generic_ret: Option<Sig>,
    ) {
        if let Some(w) = &st.warning {
            self.line();
            self.w(&format!("/* Code restructure failed: {} */", w.replace("*/", "* /")));
        }
        if std::env::var_os("OPENPROFILER_DEX_DUMP").is_some() {
            self.line();
            self.w(&format!("/*\n{}*/", structure::dump(body).replace("*/", "* /")));
        }
        // Trailing `return;` is implicit.
        if matches!(st.nodes.last(), Some(Node::Return(None))) {
            st.nodes.pop();
        }
        let mut counts = HashMap::new();
        structure::count_refs(&st.nodes, &mut counts);
        loops_to_for(&mut st.nodes, body, &counts);
        if std::env::var_os("OPENPROFILER_DEX_DUMP").is_some() {
            self.line();
            self.w(&format!("/*\n{:#?}\n*/", st.nodes).replace("*/\n*/", "* /\n*/"));
        }
        let mut mc = MethodCtx::new(self, body, param_names);
        if let Some(r) = generic_ret {
            generic_locals(&st.nodes, body, &r, &mut mc.generic);
        }
        mc.labels = st.used_labels.iter().copied().collect();
        plan_declarations(&mut mc, &st.nodes);
        self.nodes(&mut mc, &st.nodes);
    }

    // ------------------------------------------------------------ statements

    fn nodes(&mut self, mc: &mut MethodCtx<'_>, nodes: &[Node]) {
        let scope = mc.next_scope();
        for (i, n) in nodes.iter().enumerate() {
            self.declare_vars(mc, (scope, i));
            mc.pos = (scope, i);
            self.node(mc, n);
        }
    }

    fn declare_vars(&mut self, mc: &mut MethodCtx<'_>, at: (u32, usize)) {
        let Some(vars) = mc.declare_before.remove(&at) else { return };
        for v in vars {
            if mc.declared[v as usize] {
                continue;
            }
            self.line();
            let t = mc.var_type(v);
            self.var_type_decl(mc, v, &t);
            self.w(" ");
            self.w(&mc.names[v as usize].clone());
            self.w(";");
            mc.declared[v as usize] = true;
        }
    }

    fn var_type_decl(&mut self, mc: &MethodCtx<'_>, v: VarId, t: &str) {
        if let Some(s) = mc.var_sig(v) {
            self.sig(&s);
        } else {
            self.ty(t);
        }
    }

    fn block(&mut self, mc: &mut MethodCtx<'_>, nodes: &[Node]) {
        self.w(" {");
        self.indent += 1;
        self.nodes(mc, nodes);
        self.indent -= 1;
        self.line();
        self.w("}");
    }

    fn label(&mut self, mc: &MethodCtx<'_>, l: u32) {
        if mc.labels.contains(&l) {
            self.w(&format!("loop{l}: "));
        }
    }

    fn node(&mut self, mc: &mut MethodCtx<'_>, n: &Node) {
        match n {
            Node::Stmt(s) => {
                if let Stmt::Line(_) = s {
                    return;
                }
                self.line();
                self.stmt(mc, s);
            }
            Node::If { cond, then, els } => {
                self.line();
                self.if_chain(mc, cond, then, els);
            }
            Node::Loop { label, kind, body } => {
                self.line();
                self.label(mc, *label);
                match kind {
                    LoopKind::Forever => {
                        self.w("while (true)");
                        self.block(mc, body);
                    }
                    LoopKind::While(c) => {
                        self.w("while (");
                        self.cond(mc, c);
                        self.w(")");
                        self.block(mc, body);
                    }
                    LoopKind::DoWhile(c) => {
                        self.w("do");
                        self.block(mc, body);
                        self.w(" while (");
                        self.cond(mc, c);
                        self.w(");");
                    }
                }
            }
            Node::For { label, init, cond, update, body } => {
                self.line();
                self.label(mc, *label);
                let header = mc.next_scope();
                mc.pos = (header, 0);
                self.w("for (");
                if let Some(i) = init {
                    self.inline_stmt(mc, i);
                }
                self.w("; ");
                self.cond(mc, cond);
                self.w(";");
                for (k, u) in update.iter().enumerate() {
                    self.w(if k == 0 { " " } else { ", " });
                    self.inline_stmt(mc, u);
                }
                self.w(")");
                self.block(mc, body);
            }
            Node::ForEach { label, var, iterable, body } => {
                self.line();
                self.label(mc, *label);
                let header = mc.next_scope();
                mc.pos = (header, 0);
                self.w("for (");
                let t = mc.var_type(*var);
                self.var_type_decl(mc, *var, &t);
                self.w(" ");
                self.w(&mc.names[*var as usize].clone());
                self.w(" : ");
                self.expr(mc, iterable, 0, None);
                self.w(")");
                self.block(mc, body);
            }
            Node::Switch { label, value, cases } => {
                self.line();
                self.label(mc, *label);
                self.w("switch (");
                let mut vt = mc.type_of(value);
                // `switch (e.ordinal())` on an enum of this program → `switch (e)` with names.
                let mut names: Option<Vec<String>> = None;
                let mut shown = value;
                if let Expr::Invoke(InvokeKind::Virtual, m, args) = value
                    && &*m.name == "ordinal"
                    && args.len() == 1
                    && let Some(t) = mc.type_of(&args[0])
                    && let Some(consts) = self.p.enum_constants(&t)
                    && cases.iter().all(|c| c.keys.iter().all(|&k| (k as usize) < consts.len() && k >= 0))
                {
                    names = Some(consts);
                    shown = &args[0];
                    vt = None;
                }
                self.expr(mc, shown, 0, None);
                self.w(") {");
                self.indent += 1;
                let shared = mc.next_scope();
                for (ci, c) in cases.iter().enumerate() {
                    for &k in &c.keys {
                        self.line();
                        self.w("case ");
                        match &names {
                            Some(n) => self.w(&n[k as usize].clone()),
                            None => self.w(&lit_text(k as i64, false, vt.as_deref())),
                        }
                        self.w(":");
                    }
                    if c.default {
                        self.line();
                        self.w("default:");
                    }
                    self.indent += 1;
                    self.declare_vars(mc, (shared, ci));
                    self.nodes(mc, &c.body);
                    self.indent -= 1;
                }
                self.indent -= 1;
                self.line();
                self.w("}");
            }
            Node::Try { body, catches } => {
                self.line();
                self.w("try");
                self.block(mc, body);
                for Catch { types, var, body } in catches {
                    self.w(" catch (");
                    for (k, t) in types.iter().enumerate() {
                        if k > 0 {
                            self.w(" | ");
                        }
                        self.ty(t.as_deref().unwrap_or("Ljava/lang/Throwable;"));
                    }
                    self.w(" ");
                    let name = match var {
                        Some(v) => {
                            mc.declared[*v as usize] = true;
                            mc.names[*v as usize].clone()
                        }
                        None => mc.fresh_name(if types.iter().any(Option::is_none) { "th" } else { "e" }),
                    };
                    self.w(&name);
                    self.w(")");
                    self.block(mc, body);
                }
                if catches.is_empty() {
                    self.w(" finally {\n}");
                }
            }
            Node::Sync { lock, body } => {
                self.line();
                self.w("synchronized (");
                self.expr(mc, lock, 0, None);
                self.w(")");
                self.block(mc, body);
            }
            Node::Break(l) => {
                self.line();
                match l {
                    Some(l) => self.w(&format!("break loop{l};")),
                    None => self.w("break;"),
                }
            }
            Node::Continue(l) => {
                self.line();
                match l {
                    Some(l) => self.w(&format!("continue loop{l};")),
                    None => self.w("continue;"),
                }
            }
            Node::Return(e) => {
                self.line();
                match e {
                    Some(e) => {
                        self.w("return ");
                        let rt = mc.body.ret.clone();
                        self.assigned(mc, e, 0, &rt);
                        self.w(";");
                    }
                    None => self.w("return;"),
                }
            }
            Node::Throw(e) => {
                self.line();
                self.w("throw ");
                self.expr(mc, e, 0, Some("Ljava/lang/Throwable;"));
                self.w(";");
            }
            Node::Comment(c) => {
                self.line();
                self.w(&format!("/* {} */", c.replace("*/", "* /")));
            }
        }
    }

    fn if_chain(&mut self, mc: &mut MethodCtx<'_>, cond: &Expr, then: &[Node], els: &[Node]) {
        self.w("if (");
        self.cond(mc, cond);
        self.w(")");
        self.block(mc, then);
        if els.is_empty() {
            return;
        }
        self.w(" else");
        if let [Node::If { cond, then, els }] = els {
            // `else if` still consumes the scope ids of the else list for declarations.
            let scope = mc.next_scope();
            if mc.declare_before.contains_key(&(scope, 0)) {
                mc.unscope();
            } else {
                mc.pos = (scope, 0);
                self.w(" ");
                self.if_chain(mc, cond, then, els);
                return;
            }
        }
        self.block(mc, els);
    }

    fn stmt(&mut self, mc: &mut MethodCtx<'_>, s: &Stmt) {
        match s {
            Stmt::Assign(v, e) => {
                let v = *v;
                let decl = !mc.declared[v as usize] && mc.declare_here.remove(&(mc.pos, v));
                let t = mc.var_type(v);
                if decl {
                    mc.declared[v as usize] = true;
                    self.var_type_decl(mc, v, &t);
                    self.w(" ");
                } else if let Some(text) = compound(mc, v, e) {
                    self.w(&mc.names[v as usize].clone());
                    match text {
                        Compound::Inc => self.w("++;"),
                        Compound::Dec => self.w("--;"),
                        Compound::Op(op, rhs) => {
                            self.w(&format!(" {}= ", op.symbol()));
                            self.expr(mc, rhs, 1, Some(&t));
                            self.w(";");
                        }
                    }
                    return;
                }
                let n = mc.names[v as usize].clone();
                self.w(&n);
                self.w(" = ");
                // `T x = (T) it.next()` for generic locals: cast to the declared generic type.
                if let Expr::Cast(ct, inner) = e
                    && let Some(sig) = mc.var_sig(v)
                    && (sig.erased().as_deref() == Some(&**ct) || matches!(sig, Sig::Var(_)))
                {
                    self.w("(");
                    self.sig(&sig);
                    self.w(") ");
                    self.expr(mc, inner, 13, None);
                    self.w(";");
                    return;
                }
                self.assigned(mc, e, 1, &t);
                self.w(";");
            }
            Stmt::Expr(e) => {
                self.expr(mc, e, 0, None);
                self.w(";");
            }
            Stmt::IPut(o, f, val) => {
                if let Some(c) = field_compound(
                    val,
                    |x| matches!(x, Expr::IGet(o2, f2) if **o2 == *o && f2 == f && o.is_pure()),
                ) {
                    self.field_target(mc, Some(o), f, false);
                    self.compound_tail(mc, c, &f.ty.clone());
                    return;
                }
                self.field_target(mc, Some(o), f, false);
                self.w(" = ");
                self.assigned(mc, val, 1, &f.ty.clone());
                self.w(";");
            }
            Stmt::SPut(f, val) => {
                if let Some(c) = field_compound(val, |x| matches!(x, Expr::SGet(f2) if f2 == f)) {
                    self.field_target(mc, None, f, false);
                    self.compound_tail(mc, c, &f.ty.clone());
                    return;
                }
                self.field_target(mc, None, f, false);
                self.w(" = ");
                self.assigned(mc, val, 1, &f.ty.clone());
                self.w(";");
            }
            Stmt::APut(a, i, val, _) => {
                self.expr(mc, a, 14, None);
                self.w("[");
                self.expr(mc, i, 0, Some("I"));
                self.w("] = ");
                let el = mc.type_of(a).and_then(|t| types::element(&t).map(str::to_owned));
                self.expr(mc, val, 1, el.as_deref());
                self.w(";");
            }
            Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
                self.w(if matches!(s, Stmt::MonitorEnter(_)) {
                    "/* monitor-enter("
                } else {
                    "/* monitor-exit("
                });
                self.expr(mc, e, 0, None);
                self.w(") */");
            }
            Stmt::Line(_) => {}
        }
    }

    /// A statement inside a `for` header (no trailing `;`).
    fn inline_stmt(&mut self, mc: &mut MethodCtx<'_>, s: &Stmt) {
        self.stmt(mc, s);
        if self.out.ends_with(';') {
            self.out.pop();
        }
    }

    fn compound_tail(&mut self, mc: &mut MethodCtx<'_>, c: Compound<'_>, ty: &str) {
        match c {
            Compound::Inc => self.w("++;"),
            Compound::Dec => self.w("--;"),
            Compound::Op(op, rhs) => {
                self.w(&format!(" {}= ", op.symbol()));
                self.expr(mc, rhs, 1, Some(ty));
                self.w(";");
            }
        }
    }

    /// An object a member is accessed on; a `null` constant needs a cast (`((T) null).f`).
    fn receiver(&mut self, mc: &mut MethodCtx<'_>, e: &Expr, class: &str) {
        if let Expr::Lit { bits: 0, .. } = e {
            self.w("((");
            self.ty(class);
            self.w(") null)");
        } else {
            self.expr(mc, e, 14, None);
        }
    }

    /// The class whose instance `e` is when it is `this` or an `Outer.this` chain.
    fn enclosing_this(&self, mc: &MethodCtx<'_>, e: &Expr) -> Option<String> {
        match e {
            Expr::Var(v) if mc.body.vars[*v as usize].is_this => Some(self.cls.to_string()),
            Expr::IGet(o, f)
                if f.name.starts_with("this$")
                    && self.enclosing_this(mc, o).as_deref() == Some(&*f.class) =>
            {
                Some(f.ty.to_string())
            }
            _ => None,
        }
    }

    fn field_target(&mut self, mc: &mut MethodCtx<'_>, obj: Option<&Expr>, f: &FieldRef, read: bool) {
        let key = self.p.resolve_field(&f.class, &f.name);
        // `this.this$0` in an inner class is `Outer.this` (also through several levels).
        if read
            && let Some(o) = obj
            && f.name.starts_with("this$")
            && self.p.inner.get(&*f.class).is_some_and(|i| *i.outer == *f.ty)
            && self.enclosing_this(mc, o).as_deref() == Some(&*f.class)
        {
            self.ty(&f.ty);
            self.w(".this");
            return;
        }
        match obj {
            Some(o) => {
                self.receiver(mc, o, &f.class);
                self.w(".");
            }
            None => {
                if *f.class != *self.cls || mc.names.iter().any(|n| **n == *f.name) {
                    self.ty(&f.class);
                    self.w(".");
                }
            }
        }
        self.link(&ident(&f.name), key, false);
    }

    // ------------------------------------------------------------ expressions

    /// An assigned or returned value: implicit widening conversions need no cast.
    fn assigned(&mut self, mc: &mut MethodCtx<'_>, e: &Expr, prec: u8, expect: &str) {
        if let Expr::Cast(to, inner) = e
            && &**to == expect
            && let Some(from) = mc.type_of(inner)
            && widens(&from, to)
        {
            return self.expr(mc, inner, prec, Some(&from));
        }
        self.expr(mc, e, prec, Some(expect));
    }

    fn cond(&mut self, mc: &mut MethodCtx<'_>, c: &Expr) {
        self.expr(mc, c, 0, Some("Z"));
    }

    fn expr(&mut self, mc: &mut MethodCtx<'_>, e: &Expr, prec: u8, expect: Option<&str>) {
        let my = expr_prec(e, mc);
        if my < prec {
            self.w("(");
            self.expr_inner(mc, e, expect);
            self.w(")");
        } else {
            self.expr_inner(mc, e, expect);
        }
    }

    fn expr_inner(&mut self, mc: &mut MethodCtx<'_>, e: &Expr, expect: Option<&str>) {
        match e {
            Expr::Var(v) => {
                let n = mc.names.get(*v as usize).cloned().unwrap_or_else(|| format!("v{v}"));
                self.w(&n);
            }
            Expr::Lit { bits, wide } => self.w(&lit_text(*bits, *wide, expect)),
            Expr::Str(s) => self.w(&format!("\"{}\"", escape_java(s, '"'))),
            Expr::Class(c) => {
                self.ty(c);
                self.w(".class");
            }
            Expr::Raw(r) => self.w(r),
            Expr::Bin(op, a, b, t) => {
                if let Some(zero) = zero_test(mc, *op, a, b) {
                    match zero {
                        ZeroTest::Truthy(x) => self.expr(mc, x, 13, Some("Z")),
                        ZeroTest::Falsy(x) => {
                            self.w("!");
                            self.expr(mc, x, 13, Some("Z"));
                        }
                        ZeroTest::Null(x, eq) => {
                            self.expr(mc, x, 9, None);
                            self.w(if eq { " == null" } else { " != null" });
                        }
                    }
                    return;
                }
                let p = op.precedence();
                let operand_ty = match (op.is_compare() || matches!(op, BinOp::LogAnd | BinOp::LogOr), t) {
                    (_, Some(t)) if !matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) => {
                        Some(t.to_string())
                    }
                    (true, _) if matches!(op, BinOp::LogAnd | BinOp::LogOr) => Some("Z".into()),
                    (true, _) => mc.type_of(a).or_else(|| mc.type_of(b)),
                    _ => mc.type_of(a).or_else(|| expect.map(str::to_owned)),
                };
                let bool_op = matches!(op, BinOp::And | BinOp::Or | BinOp::Xor)
                    && (mc.type_of(a).as_deref() == Some("Z")
                        || mc.type_of(b).as_deref() == Some("Z")
                        || expect == Some("Z"));
                let ot = if bool_op { Some("Z".to_owned()) } else { operand_ty };
                self.expr(mc, a, p, ot.as_deref());
                self.w(&format!(" {} ", op.symbol()));
                let rt = if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) {
                    Some("I".to_owned())
                } else {
                    ot
                };
                self.expr(mc, b, p + 1, rt.as_deref());
            }
            Expr::Neg(a) => {
                self.w("-");
                if matches!(**a, Expr::Neg(_) | Expr::Lit { .. }) {
                    self.w("(");
                    self.expr(mc, a, 0, expect);
                    self.w(")");
                } else {
                    self.expr(mc, a, 13, expect);
                }
            }
            Expr::BitNot(a) => {
                self.w("~");
                self.expr(mc, a, 13, expect);
            }
            Expr::Not(a) => {
                self.w("!");
                self.expr(mc, a, 13, Some("Z"));
            }
            Expr::Cast(t, a) => {
                let from = mc.type_of(a);
                // Widening conversions are implicit; narrowing ones and reference casts are not.
                if from.as_deref() == Some(&**t) {
                    self.expr(mc, a, 13, Some(t));
                    return;
                }
                self.w("(");
                self.ty(t);
                self.w(") ");
                let inner_expect = if types::is_reference(t) { None } else { from.as_deref() };
                self.expr(mc, a, 13, inner_expect);
            }
            Expr::InstanceOf(a, t) => {
                self.expr(mc, a, 9, None);
                self.w(" instanceof ");
                self.ty(t);
            }
            Expr::ArrayLen(a) => {
                self.expr(mc, a, 14, None);
                self.w(".length");
            }
            Expr::ArrayGet(a, i, _) => {
                self.expr(mc, a, 14, None);
                self.w("[");
                self.expr(mc, i, 0, Some("I"));
                self.w("]");
            }
            Expr::IGet(o, f) => self.field_target(mc, Some(o), f, true),
            Expr::SGet(f) => self.field_target(mc, None, f, true),
            Expr::Invoke(kind, m, args) => self.invoke(mc, *kind, m, args),
            Expr::New(m, args) => {
                if let Some(parts) = concat_parts(e) {
                    self.concat(mc, &parts);
                    return;
                }
                let class = m.class.to_string();
                // Inner classes: the enclosing instance becomes `outer.new Inner(...)`.
                if self.p.has_outer_instance(&class)
                    && let Some((outer, rest)) = args.split_first()
                    && m.params
                        .first()
                        .is_some_and(|p| self.p.inner.get(&*class).is_some_and(|i| *i.outer == **p))
                {
                    let implicit = matches!(outer, Expr::Var(v) if mc.body.vars[*v as usize].is_this);
                    if implicit {
                        self.w("new ");
                        self.ty(&class);
                    } else {
                        // `outer.new Inner(...)` takes the simple name.
                        self.expr(mc, outer, 14, None);
                        self.w(".new ");
                        let n = ident(&self.p.simple_name(&class)).into_owned();
                        let k = self.defined(&class);
                        self.link(&n, k, false);
                    }
                    self.args(mc, &m.params[1..], rest);
                    return;
                }
                self.w("new ");
                self.ty(&class);
                self.args(mc, &m.params, args);
            }
            Expr::NewArray(t, n) => {
                self.w("new ");
                let el = types::element(t).unwrap_or("I");
                let base = el.trim_start_matches('[');
                let extra = el.len() - base.len();
                self.ty(base);
                self.w("[");
                self.expr(mc, n, 0, Some("I"));
                self.w("]");
                for _ in 0..extra {
                    self.w("[]");
                }
            }
            Expr::ArrayLit(t, items) => {
                self.w("new ");
                self.ty(t);
                self.w("{");
                let el = types::element(t).map(str::to_owned);
                for (k, x) in items.iter().enumerate() {
                    if k > 0 {
                        self.w(", ");
                    }
                    self.expr(mc, x, 1, el.as_deref());
                }
                self.w("}");
            }
            Expr::Cmp(kind, a, b) => {
                let (cls, ty) = match *kind {
                    "cmp-long" => ("Long", "J"),
                    "cmpl-float" | "cmpg-float" => ("Float", "F"),
                    _ => ("Double", "D"),
                };
                self.w(&format!("{cls}.compare("));
                self.expr(mc, a, 0, Some(ty));
                self.w(", ");
                self.expr(mc, b, 0, Some(ty));
                self.w(")");
            }
            Expr::Ternary(c, a, b) => {
                self.cond_prec(mc, c, 3);
                self.w(" ? ");
                self.expr(mc, a, 3, expect);
                self.w(" : ");
                self.expr(mc, b, 2, expect);
            }
            Expr::CaughtException(_) => self.w("/* caught exception */ null"),
        }
    }

    fn cond_prec(&mut self, mc: &mut MethodCtx<'_>, c: &Expr, prec: u8) {
        self.expr(mc, c, prec, Some("Z"));
    }

    fn args(&mut self, mc: &mut MethodCtx<'_>, params: &[String], args: &[Expr]) {
        self.w("(");
        for (k, a) in args.iter().enumerate() {
            if k > 0 {
                self.w(", ");
            }
            self.expr(mc, a, 1, params.get(k).map(String::as_str));
        }
        self.w(")");
    }

    fn invoke(&mut self, mc: &mut MethodCtx<'_>, kind: InvokeKind, m: &MethodRef, args: &[Expr]) {
        if let Some(parts) = concat_parts(&Expr::Invoke(kind, m.clone(), args.to_vec())) {
            self.concat(mc, &parts);
            return;
        }
        let key = self.p.resolve_method(&m.class, &m.name, &m.desc());
        match kind {
            InvokeKind::Custom => {
                self.w("/* invoke-custom */ ");
                self.w(&m.name);
                self.args(mc, &[], args);
            }
            InvokeKind::Static => {
                if *m.class != *self.cls {
                    self.ty(&m.class);
                    self.w(".");
                }
                self.link(&ident(&m.name), key, false);
                self.args(mc, &m.params, args);
            }
            _ => {
                let Some((recv, rest)) = args.split_first() else {
                    self.w(&m.name);
                    return;
                };
                let is_this = matches!(recv, Expr::Var(v) if mc.body.vars[*v as usize].is_this);
                if &*m.name == "<init>" {
                    if is_this {
                        let n = if *m.class == *self.cls { "this" } else { "super" };
                        self.link(n, key, false);
                    } else {
                        self.w("/* constructor call */ ");
                        self.receiver(mc, recv, &m.class);
                        self.w(".<init>");
                    }
                    self.args(mc, &m.params, rest);
                    return;
                }
                if kind == InvokeKind::Super {
                    self.w("super.");
                } else if !is_this {
                    self.receiver(mc, recv, &m.class);
                    self.w(".");
                }
                self.link(&ident(&m.name), key, false);
                self.args(mc, &m.params, rest);
            }
        }
    }

    fn concat(&mut self, mc: &mut MethodCtx<'_>, parts: &[&Expr]) {
        // `"" + x` keeps string semantics when the first two parts aren't strings.
        let is_str = |mc: &MethodCtx<'_>, e: &Expr| {
            matches!(e, Expr::Str(_)) || mc.type_of(e).as_deref() == Some("Ljava/lang/String;")
        };
        let needs_prefix =
            parts.is_empty() || !(is_str(mc, parts[0]) || parts.get(1).is_some_and(|p| is_str(mc, p)));
        if needs_prefix {
            self.w("\"\"");
            if !parts.is_empty() {
                self.w(" + ");
            }
        }
        for (k, p) in parts.iter().enumerate() {
            if k > 0 {
                self.w(" + ");
            }
            // Right operands of `+` need parentheses for anything binding looser than `*`.
            self.expr(mc, p, if k == 0 { 11 } else { 12 }, None);
        }
    }
}

#[derive(Clone, Copy)]
enum ZeroTest<'a> {
    Truthy(&'a Expr),
    Falsy(&'a Expr),
    Null(&'a Expr, bool),
}

fn zero_test<'a>(mc: &MethodCtx<'_>, op: BinOp, a: &'a Expr, b: &'a Expr) -> Option<ZeroTest<'a>> {
    let (x, lit) = match (a, b) {
        (x, Expr::Lit { bits, .. }) => (x, *bits),
        _ => return None,
    };
    if !matches!(op, BinOp::Eq | BinOp::Ne) {
        return None;
    }
    let t = mc.type_of(x);
    match t.as_deref() {
        Some("Z") if lit == 0 || lit == 1 => {
            Some(if (op == BinOp::Ne) == (lit == 0) { ZeroTest::Truthy(x) } else { ZeroTest::Falsy(x) })
        }
        Some(t) if types::is_reference(t) && lit == 0 => Some(ZeroTest::Null(x, op == BinOp::Eq)),
        _ => None,
    }
}

enum Compound<'a> {
    Inc,
    Dec,
    Op(BinOp, &'a Expr),
}

fn compound<'a>(mc: &MethodCtx<'_>, v: VarId, e: &'a Expr) -> Option<Compound<'a>> {
    let Expr::Bin(op, a, b, Some(_)) = e else { return None };
    if **a != Expr::Var(v) || op.is_compare() {
        return None;
    }
    let t = mc.var_type(v);
    if matches!(**b, Expr::Lit { bits: 1, .. })
        && matches!(op, BinOp::Add | BinOp::Sub)
        && matches!(t.as_str(), "I" | "J" | "S" | "B" | "C")
    {
        return Some(if *op == BinOp::Add { Compound::Inc } else { Compound::Dec });
    }
    if t == "Z" {
        return None;
    }
    Some(Compound::Op(*op, b))
}

fn field_compound(val: &Expr, same: impl Fn(&Expr) -> bool) -> Option<Compound<'_>> {
    let Expr::Bin(op, a, b, Some(t)) = val else { return None };
    if !same(a) || op.is_compare() {
        return None;
    }
    if matches!(**b, Expr::Lit { bits: 1, .. })
        && matches!(op, BinOp::Add | BinOp::Sub)
        && matches!(*t, "I" | "J")
    {
        return Some(if *op == BinOp::Add { Compound::Inc } else { Compound::Dec });
    }
    Some(Compound::Op(*op, b))
}

/// Parts of a `new StringBuilder().append(a).append(b).toString()` chain.
fn concat_parts(e: &Expr) -> Option<Vec<&Expr>> {
    let Expr::Invoke(InvokeKind::Virtual, m, args) = e else { return None };
    if &*m.class != "Ljava/lang/StringBuilder;" || &*m.name != "toString" || args.len() != 1 {
        return None;
    }
    let mut parts = vec![];
    let mut cur = &args[0];
    loop {
        match cur {
            Expr::Invoke(InvokeKind::Virtual, m, a)
                if &*m.class == "Ljava/lang/StringBuilder;" && &*m.name == "append" && a.len() == 2 =>
            {
                // `append(char)` with a literal would print as a number; only plain types.
                if m.params.first().is_some_and(|p| p == "C") && matches!(a[1], Expr::Lit { .. }) {
                    return None;
                }
                parts.push(&a[1]);
                cur = &a[0];
            }
            Expr::New(m, a) if &*m.class == "Ljava/lang/StringBuilder;" => {
                match (&m.params[..], a.as_slice()) {
                    ([], []) => {}
                    ([p], [x]) if p == "Ljava/lang/String;" => parts.push(x),
                    _ => return None,
                }
                break;
            }
            _ => return None,
        }
    }
    parts.reverse();
    (!parts.is_empty()).then_some(parts)
}

/// Java widening primitive conversion `from` → `to`.
fn widens(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("B", "S" | "I" | "J" | "F" | "D")
            | ("S" | "C", "I" | "J" | "F" | "D")
            | ("I", "J" | "F" | "D")
            | ("J", "F" | "D")
            | ("F", "D")
    )
}

fn char_lit(v: i64) -> String {
    let c = v as u32;
    match char::from_u32(c) {
        Some('\'') => "'\\''".into(),
        Some(ch) if (0x20..0x7f).contains(&c) || (ch.is_alphanumeric() && c > 0xa0) => {
            if ch == '\\' {
                "'\\\\'".into()
            } else {
                format!("'{ch}'")
            }
        }
        Some('\n') => "'\\n'".into(),
        Some('\t') => "'\\t'".into(),
        Some('\r') => "'\\r'".into(),
        _ => format!("'\\u{:04x}'", c & 0xffff),
    }
}

pub fn lit_text(bits: i64, wide: bool, expect: Option<&str>) -> String {
    match expect {
        Some("Z") => (if bits == 0 { "false" } else { "true" }).into(),
        Some("C") if (0..=0xffff).contains(&bits) => char_lit(bits),
        Some("F") => float_lit(f32::from_bits(bits as u32) as f64, "f", "Float"),
        Some("D") => float_lit(f64::from_bits(bits as u64), "d", "Double"),
        Some("J") => format!("{bits}L"),
        Some(t) if types::is_reference(t) && bits == 0 => "null".into(),
        Some("B") if !(-128..=127).contains(&bits) => format!("(byte) {bits}"),
        Some("S") if !(-32768..=32767).contains(&bits) => format!("(short) {bits}"),
        _ if wide => format!("{bits}L"),
        _ => {
            let v = bits as i32;
            if v == i32::MIN { "Integer.MIN_VALUE".into() } else { v.to_string() }
        }
    }
}

fn expr_prec(e: &Expr, mc: &MethodCtx<'_>) -> u8 {
    match e {
        Expr::Bin(op, a, b, _) => {
            if let Some(z) = zero_test(mc, *op, a, b) {
                return match z {
                    ZeroTest::Truthy(_) => 15,
                    ZeroTest::Falsy(_) => 13,
                    ZeroTest::Null(..) => 8,
                };
            }
            op.precedence()
        }
        Expr::Ternary(..) => 2,
        Expr::InstanceOf(..) => 9,
        Expr::Neg(_) | Expr::BitNot(_) | Expr::Not(_) | Expr::Cast(..) => 13,
        Expr::Lit { bits, .. } if *bits < 0 => 13,
        Expr::Invoke(..) | Expr::New(..) if concat_parts(e).is_some() => 11,
        Expr::New(..) | Expr::NewArray(..) | Expr::ArrayLit(..) => 14,
        _ => 15,
    }
}

fn signature(d: &DexFile, set: &[Annotation]) -> Option<String> {
    let a = dex::find_annotation(d, set, "Ldalvik/annotation/Signature;")?;
    match dex::annotation_value(d, a, "value")? {
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|p| if let Value::String(s) = p { Some(d.string(*s)) } else { None })
                .collect(),
        ),
        _ => None,
    }
}

/// Drops the implicit `super()` call of a constructor and its trailing `return;`.
fn strip_ctor_prefix(nodes: &[Node], desc: &str) -> Vec<Node> {
    let mut out = nodes.to_vec();
    let implicit = |n: &Node| {
        matches!(n, Node::Stmt(Stmt::Expr(Expr::Invoke(InvokeKind::Direct, m, args)))
            if &*m.name == "<init>" && &*m.class != desc && m.params.is_empty() && args.len() == 1)
    };
    if let Some(pos) = out.iter().position(implicit) {
        out.remove(pos);
    }
    if matches!(out.last(), Some(Node::Return(None))) {
        out.pop();
    }
    out
}

fn strip_enum_super(mut nodes: Vec<Node>) -> Vec<Node> {
    nodes.retain(|n| !matches!(n, Node::Stmt(Stmt::Expr(Expr::Invoke(InvokeKind::Direct, m, _))) if &*m.class == "Ljava/lang/Enum;" && &*m.name == "<init>"));
    nodes
}

/// Removes `CONST = new E("CONST", n, args…)` and `$VALUES = …` from an enum's `<clinit>`
/// (directly or through a temporary); returns the constants with their extra constructor
/// arguments.
fn take_enum_constants(desc: &str, nodes: &mut Vec<Node>) -> Vec<(String, Vec<Expr>)> {
    let ctor_args = |e: &Expr, name: &str| -> Option<Vec<Expr>> {
        match e {
            Expr::New(m, args) if &*m.class == desc => match args.as_slice() {
                [Expr::Str(n), Expr::Lit { .. }, rest @ ..] if **n == *name => Some(rest.to_vec()),
                _ => None,
            },
            _ => None,
        }
    };
    let temps: HashMap<VarId, Expr> = nodes
        .iter()
        .filter_map(|n| match n {
            Node::Stmt(Stmt::Assign(v, e @ Expr::New(m, _))) if &*m.class == desc => Some((*v, e.clone())),
            _ => None,
        })
        .collect();
    let mut out = vec![];
    let mut used_temps = HashSet::new();
    for n in nodes.iter() {
        if let Node::Stmt(Stmt::SPut(f, val)) = n
            && &*f.class == desc
        {
            let (e, temp) = match val {
                Expr::Var(v) => (temps.get(v), Some(*v)),
                e => (Some(e), None),
            };
            if let Some(args) = e.and_then(|e| ctor_args(e, &f.name)) {
                out.push((f.name.to_string(), args));
                used_temps.extend(temp);
            }
        }
    }
    let names: HashSet<String> = out.iter().map(|(n, _)| n.clone()).collect();
    nodes.retain(|n| match n {
        Node::Stmt(Stmt::SPut(f, _)) if &*f.class == desc => {
            !names.contains(&*f.name) && &*f.name != "$VALUES"
        }
        Node::Stmt(Stmt::Assign(v, _)) => !used_temps.contains(v),
        _ => true,
    });
    if matches!(nodes.last(), Some(Node::Return(None))) {
        nodes.pop();
    }
    out
}

fn param_names(w: &Writer<'_>, body: Option<&Body>, params: &[String]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    let simple = |d: &str| w.p.simple_name(d);
    params
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let from_body = body.and_then(|b| b.params.get(k).and_then(|&v| b.vars[v as usize].name.clone()));
            let base = from_body.filter(|n| valid_ident(n)).unwrap_or_else(|| base_name(p, &simple));
            unique(&base, &mut used)
        })
        .collect()
}

fn valid_ident(n: &str) -> bool {
    !n.is_empty()
        && !KEYWORDS.contains(&n)
        && n.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && n.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

fn unique(base: &str, used: &mut HashSet<String>) -> String {
    let mut n = base.to_owned();
    let mut k = 2;
    while used.contains(&n) || KEYWORDS.contains(&n.as_str()) {
        n = format!("{base}{k}");
        k += 1;
    }
    used.insert(n.clone());
    n
}

// ---------------------------------------------------------------- per-method context

struct MethodCtx<'b> {
    body: &'b Body,
    names: Vec<String>,
    declared: Vec<bool>,
    used_names: HashSet<String>,
    labels: HashSet<u32>,
    scope_counter: u32,
    pos: (u32, usize),
    /// Variables declared on their own line before a child of a scope.
    declare_before: HashMap<(u32, usize), Vec<VarId>>,
    /// Assignments that declare their variable: `((scope, child), var)`.
    declare_here: HashSet<((u32, usize), VarId)>,
    /// Generic types inferred for locals without debug signatures.
    generic: HashMap<VarId, Sig>,
}

impl<'b> MethodCtx<'b> {
    fn new(w: &Writer<'_>, body: &'b Body, param_names: Option<&[String]>) -> Self {
        let mut used = HashSet::new();
        let n = body.vars.len();
        let mut names = vec![String::new(); n];
        let mut declared = vec![false; n];
        for (k, &v) in body.params.iter().enumerate() {
            let name = param_names.and_then(|p| p.get(k).cloned()).unwrap_or_else(|| format!("p{k}"));
            used.insert(name.clone());
            names[v as usize] = name;
            declared[v as usize] = true;
        }
        if let Some(t) = body.this {
            names[t as usize] = "this".into();
            declared[t as usize] = true;
        }
        let simple = |d: &str| w.p.simple_name(d);
        for (v, vi) in body.vars.iter().enumerate() {
            if !names[v].is_empty() {
                continue;
            }
            let base = vi
                .name
                .clone()
                .filter(|n| valid_ident(n))
                .unwrap_or_else(|| base_name(vi.ty.as_deref().unwrap_or("I"), &simple));
            names[v] = unique(&base, &mut used);
        }
        Self {
            body,
            names,
            declared,
            used_names: used,
            labels: HashSet::new(),
            scope_counter: 0,
            pos: (0, 0),
            declare_before: HashMap::new(),
            declare_here: HashSet::new(),
            generic: HashMap::new(),
        }
    }

    fn next_scope(&mut self) -> u32 {
        self.scope_counter += 1;
        self.scope_counter
    }

    /// Undoes `next_scope` (the scope will be consumed again by `nodes`).
    fn unscope(&mut self) {
        self.scope_counter -= 1;
    }

    fn fresh_name(&mut self, base: &str) -> String {
        unique(base, &mut self.used_names)
    }

    fn var_sig(&self, v: VarId) -> Option<Sig> {
        self.body.vars[v as usize]
            .signature
            .as_deref()
            .and_then(types::parse_field_sig)
            .or_else(|| self.generic.get(&v).cloned())
    }

    fn var_type(&self, v: VarId) -> String {
        self.body.vars.get(v as usize).and_then(|x| x.ty.clone()).unwrap_or_else(|| "I".into())
    }

    fn type_of(&self, e: &Expr) -> Option<String> {
        method::expr_type(e, &self.body.vars)
    }
}

/// Decides where each local variable is declared: at its first assignment when that assignment
/// is in the innermost scope containing every reference, otherwise on its own line.
fn plan_declarations(mc: &mut MethodCtx<'_>, nodes: &[Node]) {
    mark_catch_vars(nodes, &mut mc.declared);
    let mut refs: HashMap<VarId, Vec<Vec<(u32, usize)>>> = HashMap::new();
    let mut first_assign: HashMap<VarId, Vec<(u32, usize)>> = HashMap::new();
    let mut counter = 0u32;
    let mut path: Vec<(u32, usize)> = vec![];
    let mut headers: HashMap<u32, (u32, usize)> = HashMap::new();
    walk_refs(nodes, &mut counter, &mut path, &mut refs, &mut first_assign, &mut headers);
    for (v, paths) in refs {
        if mc.declared[v as usize] {
            continue;
        }
        // Longest common prefix of all reference paths.
        let mut common: Vec<(u32, usize)> = paths[0].clone();
        for p in &paths[1..] {
            let mut n = 0;
            while n < common.len() && n < p.len() && common[n].0 == p[n].0 {
                if common[n].1 != p[n].1 {
                    n += 1;
                    break;
                }
                n += 1;
            }
            common.truncate(n);
        }
        if common.is_empty() {
            continue;
        }
        let (scope, _) = *common.last().expect("non-empty");
        // The first child of `scope` that references v.
        let first_child =
            paths.iter().filter_map(|p| p.get(common.len() - 1).map(|x| x.1)).min().unwrap_or(0);
        if let Some(fa) = first_assign.get(&v)
            && fa.len() == common.len()
            && fa.last() == Some(&(scope, first_child))
            && paths.iter().all(|p| p.get(common.len() - 1).map(|x| x.1) >= Some(first_child))
        {
            mc.declare_here.insert(((scope, first_child), v));
        } else if let Some(&at) = headers.get(&scope) {
            // A `for` header only declares in its init: declare before the loop instead.
            mc.declare_before.entry(at).or_default().push(v);
        } else {
            mc.declare_before.entry((scope, first_child)).or_default().push(v);
        }
    }
    for v in mc.declare_before.values_mut() {
        v.sort_unstable();
    }
}

fn walk_refs(
    nodes: &[Node],
    counter: &mut u32,
    path: &mut Vec<(u32, usize)>,
    refs: &mut HashMap<VarId, Vec<Vec<(u32, usize)>>>,
    first_assign: &mut HashMap<VarId, Vec<(u32, usize)>>,
    headers: &mut HashMap<u32, (u32, usize)>,
) {
    *counter += 1;
    let scope = *counter;
    for (i, n) in nodes.iter().enumerate() {
        path.push((scope, i));
        let add = |v: VarId, refs: &mut HashMap<VarId, Vec<Vec<(u32, usize)>>>| {
            refs.entry(v).or_default().push(path.clone())
        };
        let exprs =
            |e: &Expr, refs: &mut HashMap<VarId, Vec<Vec<(u32, usize)>>>| e.visit_vars(&mut |v| add(v, refs));
        match n {
            Node::Stmt(s) => {
                // Uses are evaluated before the assignment target is defined.
                let mut used_here = vec![];
                s.visit_exprs(&mut |e| e.visit_vars(&mut |v| used_here.push(v)));
                if let Stmt::Assign(v, _) = s {
                    if !refs.contains_key(v) && !used_here.contains(v) {
                        first_assign.insert(*v, path.clone());
                    }
                    refs.entry(*v).or_default().push(path.clone());
                }
                for v in used_here {
                    refs.entry(v).or_default().push(path.clone());
                }
            }
            Node::If { cond, then, els } => {
                exprs(cond, refs);
                walk_refs(then, counter, path, refs, first_assign, headers);
                // An empty `else` is not printed and takes no scope.
                if !els.is_empty() {
                    walk_refs(els, counter, path, refs, first_assign, headers);
                }
            }
            Node::Loop { kind, body, .. } => {
                if let LoopKind::While(c) = kind {
                    exprs(c, refs);
                }
                walk_refs(body, counter, path, refs, first_assign, headers);
                if let LoopKind::DoWhile(c) = kind {
                    // The condition sits outside the body's scope.
                    c.visit_vars(&mut |v| refs.entry(v).or_default().push(path.clone()));
                }
            }
            Node::For { init, cond, update, body, .. } => {
                *counter += 1;
                headers.insert(*counter, (scope, i));
                path.push((*counter, 0));
                if let Some(Stmt::Assign(v, e)) = init {
                    let mut used_here = vec![];
                    e.visit_vars(&mut |x| used_here.push(x));
                    if !refs.contains_key(v) && !used_here.contains(v) {
                        first_assign.insert(*v, path.clone());
                    }
                    refs.entry(*v).or_default().push(path.clone());
                    for x in used_here {
                        refs.entry(x).or_default().push(path.clone());
                    }
                } else if let Some(i) = init {
                    i.visit_exprs(&mut |e| {
                        e.visit_vars(&mut |x| refs.entry(x).or_default().push(path.clone()))
                    });
                }
                cond.visit_vars(&mut |x| refs.entry(x).or_default().push(path.clone()));
                for u in update {
                    if let Stmt::Assign(v, _) = u {
                        refs.entry(*v).or_default().push(path.clone());
                    }
                    u.visit_exprs(&mut |e| {
                        e.visit_vars(&mut |x| refs.entry(x).or_default().push(path.clone()))
                    });
                }
                walk_refs(body, counter, path, refs, first_assign, headers);
                path.pop();
            }
            Node::ForEach { iterable, body, .. } => {
                exprs(iterable, refs);
                *counter += 1;
                headers.insert(*counter, (scope, i));
                path.push((*counter, 0));
                walk_refs(body, counter, path, refs, first_assign, headers);
                path.pop();
            }
            Node::Switch { value, cases, .. } => {
                exprs(value, refs);
                // All cases share one scope in Java; give them one id.
                *counter += 1;
                let shared = *counter;
                let base = path.len();
                for (k, c) in cases.iter().enumerate() {
                    path.push((shared, k));
                    walk_refs(&c.body, counter, path, refs, first_assign, headers);
                    path.truncate(base);
                }
            }
            Node::Try { body, catches } => {
                walk_refs(body, counter, path, refs, first_assign, headers);
                for c in catches {
                    walk_refs(&c.body, counter, path, refs, first_assign, headers);
                }
            }
            Node::Sync { lock, body } => {
                exprs(lock, refs);
                walk_refs(body, counter, path, refs, first_assign, headers);
            }
            Node::Return(Some(e)) | Node::Throw(e) => exprs(e, refs),
            _ => {}
        }
        path.pop();
    }
}

fn mark_catch_vars(nodes: &[Node], declared: &mut [bool]) {
    for n in nodes {
        match n {
            Node::If { then, els, .. } => {
                mark_catch_vars(then, declared);
                mark_catch_vars(els, declared);
            }
            Node::Loop { body, .. } | Node::For { body, .. } | Node::Sync { body, .. } => {
                mark_catch_vars(body, declared)
            }
            Node::ForEach { var, body, .. } => {
                declared[*var as usize] = true;
                mark_catch_vars(body, declared);
            }
            Node::Switch { cases, .. } => cases.iter().for_each(|c| mark_catch_vars(&c.body, declared)),
            Node::Try { body, catches } => {
                mark_catch_vars(body, declared);
                for c in catches {
                    if let Some(v) = c.var {
                        declared[v as usize] = true;
                    }
                    mark_catch_vars(&c.body, declared);
                }
            }
            _ => {}
        }
    }
}

/// `x` for `x != 0` (a boolean tested against zero).
fn truthy(e: &Expr) -> &Expr {
    match e {
        Expr::Bin(BinOp::Ne, x, z, _) if matches!(**z, Expr::Lit { bits: 0, .. }) => x,
        e => e,
    }
}

/// `while (c) { ...; i++; }` without `continue` → `for (; c; i++) { ... }`.
fn while_to_for(n: &mut Node) {
    let Node::Loop { label, kind: LoopKind::While(cond), body } = n else { return };
    let Some(Node::Stmt(Stmt::Assign(v, Expr::Bin(op, a, _, _)))) = body.last() else { return };
    if !matches!(op, BinOp::Add | BinOp::Sub) || **a != Expr::Var(*v) {
        return;
    }
    let mut in_cond = false;
    cond.visit_vars(&mut |x| in_cond |= x == *v);
    if !in_cond || has_continue(body, *label, true) {
        return;
    }
    let Some(Node::Stmt(update)) = body.pop() else { return };
    let (label, cond, body) = (*label, cond.clone(), std::mem::take(body));
    *n = Node::For { label, init: None, cond, update: vec![update], body };
}

/// Whether `nodes` contain a `continue` that targets the loop labelled `label`.
fn has_continue(nodes: &[Node], label: u32, innermost: bool) -> bool {
    nodes.iter().any(|n| match n {
        Node::Continue(None) => innermost,
        Node::Continue(Some(l)) => *l == label,
        Node::If { then, els, .. } => {
            has_continue(then, label, innermost) || has_continue(els, label, innermost)
        }
        Node::Loop { body, .. } | Node::For { body, .. } | Node::ForEach { body, .. } => {
            has_continue(body, label, false)
        }
        Node::Sync { body, .. } => has_continue(body, label, innermost),
        Node::Switch { cases, .. } => cases.iter().any(|c| has_continue(&c.body, label, innermost)),
        Node::Try { body, catches } => {
            has_continue(body, label, innermost)
                || catches.iter().any(|c| has_continue(&c.body, label, innermost))
        }
        _ => false,
    })
}

/// `it.next()` or `(T) it.next()`.
fn is_next(e: &Expr, it: VarId) -> bool {
    match e {
        Expr::Cast(_, inner) => is_next(inner, it),
        Expr::Invoke(_, m, a) => &*m.name == "next" && a == &[Expr::Var(it)],
        _ => false,
    }
}

/// Folds `i = 0; for (; i < n; i++)` into the header and turns iterator and array loops into
/// for-each loops.
fn loops_to_for(nodes: &mut Vec<Node>, body: &Body, counts: &HashMap<VarId, usize>) {
    for n in nodes.iter_mut() {
        match n {
            Node::If { then, els, .. } => {
                loops_to_for(then, body, counts);
                loops_to_for(els, body, counts);
            }
            Node::Loop { body: b, .. }
            | Node::For { body: b, .. }
            | Node::ForEach { body: b, .. }
            | Node::Sync { body: b, .. } => loops_to_for(b, body, counts),
            Node::Switch { cases, .. } => {
                cases.iter_mut().for_each(|c| loops_to_for(&mut c.body, body, counts))
            }
            Node::Try { body: b, catches } => {
                loops_to_for(b, body, counts);
                catches.iter_mut().for_each(|c| loops_to_for(&mut c.body, body, counts));
            }
            _ => {}
        }
    }
    let unnamed = |v: VarId| {
        let vi = &body.vars[v as usize];
        vi.name.is_none() && !vi.is_this && vi.param.is_none()
    };
    let count = |v: VarId| counts.get(&v).copied().unwrap_or(0);
    for n in nodes.iter_mut() {
        while_to_for(n);
    }
    let mut i = 0;
    while i < nodes.len() {
        // `it = c.iterator(); j = 0; while (it.hasNext())` → move the pure `j = 0` up first.
        if i + 2 < nodes.len()
            && let Node::Stmt(Stmt::Assign(it, Expr::Invoke(_, m, _))) = &nodes[i]
            && &*m.name == "iterator"
            && let Node::Stmt(Stmt::Assign(_, between)) = &nodes[i + 1]
            && between.is_pure()
            && matches!(&nodes[i + 2], Node::Loop { .. })
        {
            let mut reads_it = false;
            between.visit_vars(&mut |x| reads_it |= x == *it);
            if !reads_it {
                nodes.swap(i, i + 1);
                i += 1;
                continue;
            }
        }
        // Iterator: `it = c.iterator(); while (it.hasNext()) { x = (T) it.next(); ... }`
        if i + 1 < nodes.len()
            && let Node::Stmt(Stmt::Assign(it, Expr::Invoke(_, m, args))) = &nodes[i]
            && &*m.name == "iterator"
            && m.params.is_empty()
            && args.len() == 1
            && let Node::Loop { label, kind: LoopKind::While(wc), body: lb } = &nodes[i + 1]
            && let Expr::Invoke(_, hn, ha) = truthy(wc)
            && &*hn.name == "hasNext"
            && ha == &[Expr::Var(*it)]
            && let Some(Node::Stmt(Stmt::Assign(_, first))) = lb.first()
            && is_next(first, *it)
            && count(*it) == 3
            && unnamed(*it)
            && i + 1 < nodes.len()
        {
            let (label, it_coll) = (*label, args[0].clone());
            let Node::Loop { body: mut lb, .. } = nodes.remove(i + 1) else { unreachable!() };
            let Node::Stmt(Stmt::Assign(x, _)) = lb.remove(0) else { unreachable!() };
            nodes[i] = Node::ForEach { label, var: x, iterable: it_coll, body: lb };
            i += 1;
            continue;
        }
        // `i = 0; for (; i < n; i++)` → `for (i = 0; ...)`
        if i + 1 < nodes.len()
            && let Node::Stmt(Stmt::Assign(v, _)) = &nodes[i]
            && let Node::For { init: None, cond, .. } = &nodes[i + 1]
        {
            let v = *v;
            let mut in_cond = false;
            cond.visit_vars(&mut |x| in_cond |= x == v);
            if in_cond {
                let Node::Stmt(st) = nodes.remove(i) else { unreachable!() };
                if let Node::For { init, .. } = &mut nodes[i] {
                    *init = Some(st);
                }
                continue;
            }
        }
        // Array: `a = E; n = a.length; for (i = 0; i < n; i++) { x = a[i]; ... }`
        if let Node::For {
            label,
            init: Some(Stmt::Assign(idx, Expr::Lit { bits: 0, .. })),
            cond: Expr::Bin(BinOp::Lt, ci, cn, _),
            update,
            body: fb,
        } = &nodes[i]
            && **ci == Expr::Var(*idx)
            && let Expr::Var(len) = **cn
            && update.len() == 1
            && matches!(&update[0], Stmt::Assign(u, Expr::Bin(BinOp::Add, a, b, _)) if u == idx && **a == Expr::Var(*idx) && matches!(**b, Expr::Lit { bits: 1, .. }))
            && let Some(Node::Stmt(Stmt::Assign(x, Expr::ArrayGet(arr, gi, _)))) = fb.first()
            && **gi == Expr::Var(*idx)
            && let Expr::Var(arr) = **arr
            && i >= 1
            && matches!(&nodes[i - 1], Node::Stmt(Stmt::Assign(l, Expr::ArrayLen(a))) if *l == len && **a == Expr::Var(arr))
            && count(*idx) == 5
            && count(len) == 2
            && unnamed(*idx)
            && unnamed(len)
        {
            let (label, x) = (*label, *x);
            // The array itself: a temporary assigned just before, or an existing variable.
            let from_temp = i >= 2
                && matches!(&nodes[i - 2], Node::Stmt(Stmt::Assign(a, _)) if *a == arr)
                && count(arr) == 3
                && unnamed(arr);
            let Node::For { body: mut fb, .. } = nodes.remove(i) else { unreachable!() };
            fb.remove(0);
            nodes.remove(i - 1);
            let iterable = if from_temp {
                let Node::Stmt(Stmt::Assign(_, e)) = nodes.remove(i - 2) else { unreachable!() };
                i -= 2;
                e
            } else {
                i -= 1;
                Expr::Var(arr)
            };
            nodes.insert(i, Node::ForEach { label, var: x, iterable, body: fb });
            i += 1;
            continue;
        }
        i += 1;
    }
}

/// Locals returned from a method whose return type is a type variable get that type, and so do
/// locals copied (or cast) into them.
fn generic_locals(nodes: &[Node], body: &Body, ret: &Sig, out: &mut HashMap<VarId, Sig>) {
    let mut moves: Vec<(VarId, VarId)> = vec![];
    let mut returned: Vec<VarId> = vec![];
    fn walk(nodes: &[Node], moves: &mut Vec<(VarId, VarId)>, returned: &mut Vec<VarId>) {
        for n in nodes {
            match n {
                Node::Stmt(Stmt::Assign(a, Expr::Var(b))) => moves.push((*a, *b)),
                Node::Stmt(Stmt::Assign(a, Expr::Cast(_, inner))) => {
                    if let Expr::Var(b) = **inner {
                        moves.push((*a, b));
                    }
                }
                Node::Return(Some(Expr::Var(v))) => returned.push(*v),
                Node::If { then, els, .. } => {
                    walk(then, moves, returned);
                    walk(els, moves, returned);
                }
                Node::Loop { body, .. }
                | Node::For { body, .. }
                | Node::ForEach { body, .. }
                | Node::Sync { body, .. } => walk(body, moves, returned),
                Node::Switch { cases, .. } => cases.iter().for_each(|c| walk(&c.body, moves, returned)),
                Node::Try { body, catches } => {
                    walk(body, moves, returned);
                    catches.iter().for_each(|c| walk(&c.body, moves, returned));
                }
                _ => {}
            }
        }
    }
    walk(nodes, &mut moves, &mut returned);
    let mut work = returned;
    while let Some(v) = work.pop() {
        let vi = &body.vars[v as usize];
        if vi.is_this
            || vi.param.is_some()
            || vi.signature.is_some()
            || !vi.ty.as_deref().is_some_and(types::is_reference)
            || out.contains_key(&v)
        {
            continue;
        }
        out.insert(v, ret.clone());
        for &(a, b) in &moves {
            if a == v {
                work.push(b);
            }
            if b == v {
                work.push(a);
            }
        }
    }
}
