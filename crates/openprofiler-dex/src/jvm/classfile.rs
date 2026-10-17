//! Java class file parser (JVMS §4): constant pool, members and the attributes the decompiler
//! uses. Inputs are untrusted, so every read is bounds-checked and errors instead of panicking.

use anyhow::{Result, bail};
use std::sync::Arc;

pub const MAGIC: [u8; 4] = [0xca, 0xfe, 0xba, 0xbe];

/// Class file magic with a plausible major version (Mach-O universal binaries share the magic,
/// followed by a small architecture count).
pub fn is_class(data: &[u8]) -> bool {
    data.len() >= 10
        && data.starts_with(&MAGIC)
        && (45..200).contains(&u16::from_be_bytes([data[6], data[7]]))
}

/// Big-endian bounds-checked reader.
#[derive(Clone)]
pub struct Be<'a> {
    data: &'a [u8],
    pub pos: usize,
}

impl<'a> Be<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len());
        let Some(end) = end else { bail!("read of {n} bytes at {:#x} is out of bounds", self.pos) };
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok((self.u32()? as u64) << 32 | self.u32()? as u64)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    /// Index 0 and the unusable slot after a long or double.
    None,
    Utf8(Arc<str>),
    Int(i32),
    Float(u32),
    Long(i64),
    Double(u64),
    Class(u16),
    String(u16),
    Field(u16, u16),
    Method(u16, u16),
    IMethod(u16, u16),
    NameAndType(u16, u16),
    MethodHandle(u8, u16),
    MethodType(u16),
    Dynamic(u16, u16),
    InvokeDynamic(u16, u16),
    Module(u16),
    Package(u16),
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub name: u16,
    pub data: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub access: u16,
    pub name: u16,
    pub desc: u16,
    pub attrs: Vec<Attr>,
}

#[derive(Debug, Clone)]
pub struct ClassFile {
    pub major: u16,
    pub cp: Vec<Const>,
    pub access: u16,
    pub this: u16,
    pub superclass: u16,
    pub interfaces: Vec<u16>,
    pub fields: Vec<Member>,
    pub methods: Vec<Member>,
    pub attrs: Vec<Attr>,
}

/// A resolved field or method reference.
pub struct MemberRef<'a> {
    pub class: &'a str,
    pub name: &'a str,
    pub desc: &'a str,
    pub interface: bool,
}

pub struct Handler {
    pub start: u32,
    pub end: u32,
    pub handler: u32,
    /// Constant-pool class index; 0 = any.
    pub catch: u16,
}

pub struct Code {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Arc<[u8]>,
    pub handlers: Vec<Handler>,
    pub attrs: Vec<Attr>,
}

pub struct LocalEntry {
    pub start: u32,
    pub len: u32,
    pub name: u16,
    pub desc: u16,
    pub index: u16,
}

pub struct InnerEntry {
    pub inner: u16,
    pub outer: u16,
    pub name: u16,
    pub access: u16,
}

pub struct Bootstrap {
    pub method: u16,
    pub args: Vec<u16>,
}

/// Decodes modified UTF-8 (JVMS §4.4.7): `C0 80` for NUL and surrogates encoded separately.
pub fn mutf8(b: &[u8]) -> Arc<str> {
    if b.iter().all(|&c| c != 0 && c < 0x80) {
        return Arc::from(std::str::from_utf8(b).unwrap_or_default());
    }
    let mut units: Vec<u16> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let a = b[i] as u16;
        let at = |k: usize| b.get(i + k).map_or(0, |&x| x as u16 & 0x3f);
        if a < 0x80 {
            units.push(a);
            i += 1;
        } else if a & 0xe0 == 0xc0 {
            units.push(((a & 0x1f) << 6) | at(1));
            i += 2;
        } else if a & 0xf0 == 0xe0 {
            units.push(((a & 0x0f) << 12) | (at(1) << 6) | at(2));
            i += 3;
        } else {
            units.push(0xfffd);
            i += 1;
        }
    }
    Arc::from(String::from_utf16_lossy(&units))
}

fn attrs(r: &mut Be<'_>) -> Result<Vec<Attr>> {
    let n = r.u16()?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let name = r.u16()?;
        let len = r.u32()? as usize;
        out.push(Attr { name, data: Arc::from(r.bytes(len)?) });
    }
    Ok(out)
}

fn members(r: &mut Be<'_>) -> Result<Vec<Member>> {
    let n = r.u16()?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let (access, name, desc) = (r.u16()?, r.u16()?, r.u16()?);
        out.push(Member { access, name, desc, attrs: attrs(r)? });
    }
    Ok(out)
}

impl ClassFile {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if !is_class(data) {
            bail!("not a class file");
        }
        let mut r = Be::new(data);
        r.bytes(4)?;
        let _minor = r.u16()?;
        let major = r.u16()?;
        let n = r.u16()? as usize;
        let mut cp = Vec::with_capacity(n);
        cp.push(Const::None);
        while cp.len() < n {
            let tag = r.u8()?;
            let c = match tag {
                1 => {
                    let len = r.u16()? as usize;
                    Const::Utf8(mutf8(r.bytes(len)?))
                }
                3 => Const::Int(r.u32()? as i32),
                4 => Const::Float(r.u32()?),
                5 => Const::Long(r.u64()? as i64),
                6 => Const::Double(r.u64()?),
                7 => Const::Class(r.u16()?),
                8 => Const::String(r.u16()?),
                9 => Const::Field(r.u16()?, r.u16()?),
                10 => Const::Method(r.u16()?, r.u16()?),
                11 => Const::IMethod(r.u16()?, r.u16()?),
                12 => Const::NameAndType(r.u16()?, r.u16()?),
                15 => Const::MethodHandle(r.u8()?, r.u16()?),
                16 => Const::MethodType(r.u16()?),
                17 => Const::Dynamic(r.u16()?, r.u16()?),
                18 => Const::InvokeDynamic(r.u16()?, r.u16()?),
                19 => Const::Module(r.u16()?),
                20 => Const::Package(r.u16()?),
                t => bail!("unknown constant pool tag {t} at entry {}", cp.len()),
            };
            let wide = matches!(c, Const::Long(_) | Const::Double(_));
            cp.push(c);
            if wide {
                cp.push(Const::None);
            }
        }
        cp.truncate(n.max(1));
        let access = r.u16()?;
        let this = r.u16()?;
        let superclass = r.u16()?;
        let ni = r.u16()?;
        let interfaces = (0..ni).map(|_| r.u16()).collect::<Result<_>>()?;
        let fields = members(&mut r)?;
        let methods = members(&mut r)?;
        let attrs = attrs(&mut r)?;
        Ok(Self { major, cp, access, this, superclass, interfaces, fields, methods, attrs })
    }

    pub fn get(&self, i: u16) -> &Const {
        self.cp.get(i as usize).unwrap_or(&Const::None)
    }

    pub fn utf8(&self, i: u16) -> &str {
        match self.get(i) {
            Const::Utf8(s) => s,
            _ => "",
        }
    }

    /// Internal name (`java/lang/String`) or array descriptor (`[I`) of a class constant.
    pub fn class_name(&self, i: u16) -> &str {
        match self.get(i) {
            Const::Class(n) => self.utf8(*n),
            _ => "",
        }
    }

    pub fn this_name(&self) -> &str {
        self.class_name(self.this)
    }

    pub fn name_and_type(&self, i: u16) -> (&str, &str) {
        match self.get(i) {
            Const::NameAndType(n, d) => (self.utf8(*n), self.utf8(*d)),
            _ => ("", ""),
        }
    }

    pub fn member_ref(&self, i: u16) -> Option<MemberRef<'_>> {
        let (c, nt, interface) = match self.get(i) {
            Const::Field(c, nt) | Const::Method(c, nt) => (*c, *nt, false),
            Const::IMethod(c, nt) => (*c, *nt, true),
            _ => return None,
        };
        let (name, desc) = self.name_and_type(nt);
        Some(MemberRef { class: self.class_name(c), name, desc, interface })
    }

    pub fn attr<'s>(&self, list: &'s [Attr], name: &str) -> Option<&'s [u8]> {
        list.iter().find(|a| self.utf8(a.name) == name).map(|a| &*a.data)
    }

    pub fn code(&self, m: &Member) -> Result<Option<Code>> {
        let Some(a) = m.attrs.iter().find(|a| self.utf8(a.name) == "Code") else { return Ok(None) };
        let mut r = Be::new(&a.data);
        let max_stack = r.u16()?;
        let max_locals = r.u16()?;
        let len = r.u32()? as usize;
        let code: Arc<[u8]> = Arc::from(r.bytes(len)?);
        let n = r.u16()?;
        let mut handlers = Vec::with_capacity(n as usize);
        for _ in 0..n {
            handlers.push(Handler {
                start: r.u16()? as u32,
                end: r.u16()? as u32,
                handler: r.u16()? as u32,
                catch: r.u16()?,
            });
        }
        let attrs = attrs(&mut r)?;
        Ok(Some(Code { max_stack, max_locals, code, handlers, attrs }))
    }

    /// `LocalVariableTable` or `LocalVariableTypeTable` entries (the `desc` is a signature there).
    pub fn locals(&self, code: &Code, table: &str) -> Vec<LocalEntry> {
        let mut out = vec![];
        for a in code.attrs.iter().filter(|a| self.utf8(a.name) == table) {
            let mut r = Be::new(&a.data);
            let Ok(n) = r.u16() else { continue };
            for _ in 0..n {
                let e = (|| -> Result<LocalEntry> {
                    Ok(LocalEntry {
                        start: r.u16()? as u32,
                        len: r.u16()? as u32,
                        name: r.u16()?,
                        desc: r.u16()?,
                        index: r.u16()?,
                    })
                })();
                match e {
                    Ok(e) => out.push(e),
                    Err(_) => break,
                }
            }
        }
        out
    }

    /// `(start pc, line)` pairs.
    pub fn lines(&self, code: &Code) -> Vec<(u32, u32)> {
        let mut out = vec![];
        for a in code.attrs.iter().filter(|a| self.utf8(a.name) == "LineNumberTable") {
            let mut r = Be::new(&a.data);
            let Ok(n) = r.u16() else { continue };
            for _ in 0..n {
                match (r.u16(), r.u16()) {
                    (Ok(pc), Ok(line)) => out.push((pc as u32, line as u32)),
                    _ => break,
                }
            }
        }
        out.sort_unstable();
        out
    }

    pub fn inner_classes(&self) -> Vec<InnerEntry> {
        let Some(d) = self.attr(&self.attrs, "InnerClasses") else { return vec![] };
        let mut r = Be::new(d);
        let n = r.u16().unwrap_or(0);
        let mut out = vec![];
        for _ in 0..n {
            match (r.u16(), r.u16(), r.u16(), r.u16()) {
                (Ok(inner), Ok(outer), Ok(name), Ok(access)) => {
                    out.push(InnerEntry { inner, outer, name, access })
                }
                _ => break,
            }
        }
        out
    }

    pub fn bootstraps(&self) -> Vec<Bootstrap> {
        let Some(d) = self.attr(&self.attrs, "BootstrapMethods") else { return vec![] };
        let mut r = Be::new(d);
        let n = r.u16().unwrap_or(0);
        let mut out = vec![];
        for _ in 0..n {
            let (Ok(method), Ok(k)) = (r.u16(), r.u16()) else { break };
            let args: Result<Vec<u16>> = (0..k).map(|_| r.u16()).collect();
            let Ok(args) = args else { break };
            out.push(Bootstrap { method, args });
        }
        out
    }

    /// A `u16` constant-pool index attribute (`Signature`, `SourceFile`, `ConstantValue`).
    pub fn index_attr(&self, list: &[Attr], name: &str) -> Option<u16> {
        let d = self.attr(list, name)?;
        (d.len() >= 2).then(|| u16::from_be_bytes([d[0], d[1]]))
    }

    /// A list of `u16` indices (`Exceptions`, `PermittedSubclasses`).
    pub fn index_list(&self, list: &[Attr], name: &str) -> Vec<u16> {
        let Some(d) = self.attr(list, name) else { return vec![] };
        let mut r = Be::new(d);
        let n = r.u16().unwrap_or(0);
        (0..n).map_while(|_| r.u16().ok()).collect()
    }
}

/// Converts an internal name or array descriptor to a field descriptor.
pub fn desc_of_internal(name: &str) -> String {
    if name.starts_with('[') { name.to_owned() } else { format!("L{name};") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_modified_utf8() {
        assert_eq!(&*mutf8(b"abc"), "abc");
        assert_eq!(&*mutf8(&[0xc0, 0x80]), "\0");
        assert_eq!(&*mutf8("é€".as_bytes()), "é€");
        // U+1F600 as a CESU-8 surrogate pair.
        assert_eq!(&*mutf8(&[0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80]), "😀");
        // Truncated sequences don't panic.
        assert_eq!(&*mutf8(&[0xe2]), "\u{2000}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(ClassFile::parse(b"nope").is_err());
        let mut d = MAGIC.to_vec();
        d.extend([0, 0, 0, 52, 0, 5, 99]);
        assert!(ClassFile::parse(&d).is_err());
        for n in 0..d.len() {
            let _ = ClassFile::parse(&d[..n]);
        }
    }
}
