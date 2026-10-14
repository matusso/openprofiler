//! Interned strings, methods and frames.

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

pub type StrId = u32;
pub type MethodId = u32;
pub type FrameId = u32;

/// How a frame was executing when it was sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum FrameKind {
    Interpreted = 0,
    Jit = 1,
    Inlined = 2,
    Native = 3,
    Kernel = 4,
    Cpp = 5,
    #[default]
    Unknown = 6,
}

impl FrameKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Interpreted,
            1 => Self::Jit,
            2 => Self::Inlined,
            3 => Self::Native,
            4 => Self::Kernel,
            5 => Self::Cpp,
            _ => Self::Unknown,
        }
    }
}

/// A method, identified by its declaring class (dotted binary name), name and JVM descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Method {
    pub class: StrId,
    pub name: StrId,
    pub signature: StrId,
}

/// One stack frame: a method plus the line being executed (`-1` if unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Frame {
    pub method: MethodId,
    pub line: i32,
    pub kind: FrameKind,
}

/// Interning tables for strings, methods and frames. Ids are dense indices and stay stable as the
/// tables grow, which lets live sessions ship them incrementally (see [`Symbols::strings_since`]).
#[derive(Debug, Default, Clone)]
pub struct Symbols {
    strings: Vec<Box<str>>,
    string_ids: FxHashMap<Box<str>, StrId>,
    methods: Vec<Method>,
    method_ids: FxHashMap<Method, MethodId>,
    frames: Vec<Frame>,
    frame_ids: FxHashMap<Frame, FrameId>,
    /// Lazily computed package name per class string.
    packages: FxHashMap<StrId, StrId>,
}

impl Symbols {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.intern_str("");
        s
    }

    pub fn intern_str(&mut self, s: &str) -> StrId {
        if let Some(&id) = self.string_ids.get(s) {
            return id;
        }
        let id = self.strings.len() as StrId;
        let boxed: Box<str> = s.into();
        self.strings.push(boxed.clone());
        self.string_ids.insert(boxed, id);
        id
    }

    pub fn lookup_str(&self, s: &str) -> Option<StrId> {
        self.string_ids.get(s).copied()
    }

    pub fn str(&self, id: StrId) -> &str {
        self.strings.get(id as usize).map(|s| &**s).unwrap_or("")
    }

    pub fn intern_method(&mut self, class: &str, name: &str, signature: &str) -> MethodId {
        let m = Method {
            class: self.intern_str(class),
            name: self.intern_str(name),
            signature: self.intern_str(signature),
        };
        self.intern_method_ids(m)
    }

    pub fn intern_method_ids(&mut self, m: Method) -> MethodId {
        if let Some(&id) = self.method_ids.get(&m) {
            return id;
        }
        let id = self.methods.len() as MethodId;
        self.methods.push(m);
        self.method_ids.insert(m, id);
        id
    }

    pub fn intern_frame(&mut self, method: MethodId, line: i32, kind: FrameKind) -> FrameId {
        let f = Frame { method, line, kind };
        if let Some(&id) = self.frame_ids.get(&f) {
            return id;
        }
        let id = self.frames.len() as FrameId;
        self.frames.push(f);
        self.frame_ids.insert(f, id);
        id
    }

    pub fn method(&self, id: MethodId) -> Method {
        self.methods[id as usize]
    }

    pub fn frame(&self, id: FrameId) -> Frame {
        self.frames[id as usize]
    }

    pub fn strings(&self) -> &[Box<str>] {
        &self.strings
    }

    pub fn methods(&self) -> &[Method] {
        &self.methods
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn strings_since(&self, watermark: usize) -> &[Box<str>] {
        &self.strings[watermark.min(self.strings.len())..]
    }

    pub fn methods_since(&self, watermark: usize) -> &[Method] {
        &self.methods[watermark.min(self.methods.len())..]
    }

    pub fn frames_since(&self, watermark: usize) -> &[Frame] {
        &self.frames[watermark.min(self.frames.len())..]
    }

    /// Package (`java.util`) of a dotted class name string; `""` for the default package.
    pub fn package_of_class(&mut self, class: StrId) -> StrId {
        if let Some(&p) = self.packages.get(&class) {
            return p;
        }
        let name = self.str(class);
        let pkg = match name.rfind('.') {
            Some(i) => name[..i].to_owned(),
            None => String::new(),
        };
        let id = self.intern_str(&pkg);
        self.packages.insert(class, id);
        id
    }

    /// Package lookup that does not intern (for `&self` contexts); returns the package text.
    pub fn package_name(&self, class: StrId) -> &str {
        let name = self.str(class);
        match name.rfind('.') {
            Some(i) => &name[..i],
            None => "",
        }
    }

    /// `java.lang.String.indexOf`
    pub fn method_label(&self, id: MethodId) -> String {
        let m = self.method(id);
        let class = self.str(m.class);
        let name = self.str(m.name);
        if class.is_empty() { name.to_owned() } else { format!("{class}.{name}") }
    }

    /// `java.lang.String.indexOf(int, int)` style label with a human-readable signature.
    pub fn method_label_with_signature(&self, id: MethodId) -> String {
        let m = self.method(id);
        let mut out = self.method_label(id);
        let sig = self.str(m.signature);
        if !sig.is_empty() {
            out.push_str(&pretty_descriptor(sig));
        }
        out
    }
}

/// Turns a JVM method descriptor `(ILjava/lang/String;)V` into `(int, String)`.
/// Descriptors that are already human-readable are returned unchanged.
pub fn pretty_descriptor(desc: &str) -> String {
    if !desc.starts_with('(') || !desc.contains(')') {
        return desc.to_owned();
    }
    let params = &desc[1..desc.find(')').unwrap()];
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let mut dims = 0;
        while i < bytes.len() && bytes[i] == b'[' {
            dims += 1;
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let ty = match bytes[i] {
            b'L' => {
                let end = params[i..].find(';').map(|e| i + e).unwrap_or(bytes.len());
                let full = &params[i + 1..end];
                i = end;
                full.rsplit('/').next().unwrap_or(full).to_owned()
            }
            b'Z' => "boolean".into(),
            b'B' => "byte".into(),
            b'C' => "char".into(),
            b'S' => "short".into(),
            b'I' => "int".into(),
            b'J' => "long".into(),
            b'F' => "float".into(),
            b'D' => "double".into(),
            other => (other as char).to_string(),
        };
        i += 1;
        out.push(format!("{ty}{}", "[]".repeat(dims)));
    }
    format!("({})", out.join(", "))
}

/// Converts an internal class name (`java/lang/String`, `[Ljava/lang/Object;`, `[I`) into the
/// Java source form (`java.lang.String`, `java.lang.Object[]`, `int[]`).
pub fn java_class_name(internal: &str) -> String {
    let dims = internal.bytes().take_while(|&b| b == b'[').count();
    if dims == 0 {
        return internal.replace('/', ".");
    }
    let rest = &internal[dims..];
    let base = match rest.as_bytes().first() {
        Some(b'L') => rest[1..].trim_end_matches(';').replace('/', "."),
        Some(b'Z') => "boolean".into(),
        Some(b'B') => "byte".into(),
        Some(b'C') => "char".into(),
        Some(b'S') => "short".into(),
        Some(b'I') => "int".into(),
        Some(b'J') => "long".into(),
        Some(b'F') => "float".into(),
        Some(b'D') => "double".into(),
        _ => rest.replace('/', "."),
    };
    format!("{base}{}", "[]".repeat(dims))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable() {
        let mut s = Symbols::new();
        let a = s.intern_method("java.lang.String", "indexOf", "(II)I");
        let b = s.intern_method("java.lang.String", "indexOf", "(II)I");
        assert_eq!(a, b);
        let f = s.intern_frame(a, 10, FrameKind::Jit);
        assert_eq!(f, s.intern_frame(a, 10, FrameKind::Jit));
        assert_ne!(f, s.intern_frame(a, 11, FrameKind::Jit));
        assert_eq!(s.method_label(a), "java.lang.String.indexOf");
        let cls = s.method(a).class;
        let pkg = s.package_of_class(cls);
        assert_eq!(s.str(pkg), "java.lang");
    }

    #[test]
    fn descriptors() {
        assert_eq!(pretty_descriptor("(ILjava/lang/String;[J)V"), "(int, String, long[])");
        assert_eq!(pretty_descriptor("()V"), "()");
        assert_eq!(java_class_name("[[Ljava/lang/Object;"), "java.lang.Object[][]");
        assert_eq!(java_class_name("[I"), "int[]");
        assert_eq!(java_class_name("java/util/HashMap$Node"), "java.util.HashMap$Node");
    }
}
