//! DEX file parser: the index tables, class data, code items, debug info, encoded values and
//! annotations (https://source.android.com/docs/core/runtime/dex-format).

use crate::insn::{Insn, Payload};
use crate::ir::CallSite;
use crate::read::{Reader, u32_at};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::sync::Arc;

pub const NO_INDEX: u32 = 0xffff_ffff;

pub const ACC_PUBLIC: u32 = 0x1;
pub const ACC_PRIVATE: u32 = 0x2;
pub const ACC_PROTECTED: u32 = 0x4;
pub const ACC_STATIC: u32 = 0x8;
pub const ACC_FINAL: u32 = 0x10;
pub const ACC_SYNCHRONIZED: u32 = 0x20;
pub const ACC_VOLATILE: u32 = 0x40;
pub const ACC_BRIDGE: u32 = 0x40;
pub const ACC_TRANSIENT: u32 = 0x80;
pub const ACC_VARARGS: u32 = 0x80;
pub const ACC_NATIVE: u32 = 0x100;
pub const ACC_INTERFACE: u32 = 0x200;
pub const ACC_ABSTRACT: u32 = 0x400;
pub const ACC_STRICT: u32 = 0x800;
pub const ACC_SYNTHETIC: u32 = 0x1000;
pub const ACC_ANNOTATION: u32 = 0x2000;
pub const ACC_ENUM: u32 = 0x4000;
pub const ACC_CONSTRUCTOR: u32 = 0x10000;

#[derive(Debug, Clone)]
pub struct Proto {
    pub shorty: u32,
    pub ret: u32,
    pub params: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldId {
    pub class: u32,
    pub ty: u32,
    pub name: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct MethodId {
    pub class: u32,
    pub proto: u32,
    pub name: u32,
}

#[derive(Debug, Clone)]
pub struct ClassDef {
    pub class: u32,
    pub access: u32,
    pub superclass: Option<u32>,
    pub interfaces: Vec<u32>,
    pub source_file: Option<u32>,
    pub annotations_off: u32,
    pub class_data_off: u32,
    pub static_values_off: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodedField {
    pub field: u32,
    pub access: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodedMethod {
    pub method: u32,
    pub access: u32,
    pub code_off: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ClassData {
    pub static_fields: Vec<EncodedField>,
    pub instance_fields: Vec<EncodedField>,
    pub direct_methods: Vec<EncodedMethod>,
    pub virtual_methods: Vec<EncodedMethod>,
}

#[derive(Debug, Clone)]
pub struct Handler {
    /// `None` = catch-all.
    pub ty: Option<u32>,
    pub addr: u32,
}

#[derive(Debug, Clone)]
pub struct TryItem {
    pub start: u32,
    pub count: u32,
    pub handlers: Vec<Handler>,
}

#[derive(Debug, Clone)]
pub struct CodeItem {
    pub registers: u16,
    pub ins: u16,
    pub outs: u16,
    pub insns: Vec<u16>,
    pub tries: Vec<TryItem>,
    pub debug_off: u32,
    /// Code translated from another bytecode (JVM class files): already decoded instructions
    /// with their debug info, used instead of `insns`.
    pub prebuilt: Option<Arc<Prebuilt>>,
}

#[derive(Debug, Default)]
pub struct Prebuilt {
    pub insns: Vec<Insn>,
    pub payloads: Vec<(u32, Payload)>,
    pub debug: DebugInfo,
    /// Code size in units (one per instruction and payload).
    pub units: u32,
}

impl CodeItem {
    pub fn decode(&self) -> Result<(Vec<Insn>, Vec<(u32, Payload)>)> {
        match &self.prebuilt {
            Some(p) => Ok((p.insns.clone(), p.payloads.clone())),
            None => crate::insn::decode_all(&self.insns),
        }
    }

    /// Code size in units; addresses are below it.
    pub fn units(&self) -> usize {
        self.prebuilt.as_ref().map_or(self.insns.len(), |p| p.units as usize)
    }
}

#[derive(Debug, Clone)]
pub struct LocalVar {
    pub reg: u16,
    pub start: u32,
    pub end: u32,
    pub name: Option<u32>,
    pub ty: Option<u32>,
    pub signature: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct DebugInfo {
    pub param_names: Vec<Option<u32>>,
    /// `(address, line)`, sorted by address.
    pub lines: Vec<(u32, u32)>,
    pub locals: Vec<LocalVar>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Byte(i8),
    Short(i16),
    Char(u16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    MethodType(u32),
    MethodHandle(u32),
    String(u32),
    Type(u32),
    Field(u32),
    Method(u32),
    Enum(u32),
    Array(Vec<Value>),
    Annotation(EncodedAnnotation),
    Null,
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncodedAnnotation {
    pub ty: u32,
    pub elements: Vec<(u32, Value)>,
}

pub const VISIBILITY_BUILD: u8 = 0;
pub const VISIBILITY_RUNTIME: u8 = 1;
pub const VISIBILITY_SYSTEM: u8 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub visibility: u8,
    pub value: EncodedAnnotation,
}

#[derive(Debug, Clone, Default)]
pub struct Annotations {
    pub class: Vec<Annotation>,
    pub fields: HashMap<u32, Vec<Annotation>>,
    pub methods: HashMap<u32, Vec<Annotation>>,
    pub params: HashMap<u32, Vec<Vec<Annotation>>>,
}

pub struct DexFile {
    data: Vec<u8>,
    /// Name of the file inside its container (`classes2.dex`).
    pub name: String,
    pub version: u32,
    pub strings: Vec<Box<str>>,
    pub types: Vec<u32>,
    pub protos: Vec<Proto>,
    pub fields: Vec<FieldId>,
    pub methods: Vec<MethodId>,
    pub classes: Vec<ClassDef>,
    /// Resolved `invokedynamic` call sites (JVM class files only).
    pub call_sites: Vec<CallSite>,
    /// Class files this program model was built from; offsets then index its tables.
    pub(crate) jvm: Option<Box<crate::jvm::Classes>>,
}

fn table(data: &[u8], header_off: usize) -> Result<(usize, usize)> {
    Ok((u32_at(data, header_off)? as usize, u32_at(data, header_off + 4)? as usize))
}

/// Upper bound for table sizes so a corrupt header can't make us allocate gigabytes.
fn check_table(data: &[u8], name: &str, size: usize, off: usize, item: usize) -> Result<()> {
    if size.checked_mul(item).and_then(|n| n.checked_add(off)).is_none_or(|end| end > data.len()) {
        bail!("{name} table ({size} items at {off:#x}) is out of bounds");
    }
    Ok(())
}

pub fn is_dex(data: &[u8]) -> bool {
    data.len() >= 0x70 && data.starts_with(b"dex\n")
}

impl DexFile {
    pub fn parse(name: &str, data: Vec<u8>) -> Result<Self> {
        if !is_dex(&data) {
            bail!("{name}: not a dex file");
        }
        let version = std::str::from_utf8(&data[4..7]).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        if u32_at(&data, 40)? != 0x1234_5678 {
            bail!("{name}: big-endian dex files are not supported");
        }
        let (n_strings, strings_off) = table(&data, 56)?;
        let (n_types, types_off) = table(&data, 64)?;
        let (n_protos, protos_off) = table(&data, 72)?;
        let (n_fields, fields_off) = table(&data, 80)?;
        let (n_methods, methods_off) = table(&data, 88)?;
        let (n_classes, classes_off) = table(&data, 96)?;
        check_table(&data, "string_ids", n_strings, strings_off, 4)?;
        check_table(&data, "type_ids", n_types, types_off, 4)?;
        check_table(&data, "proto_ids", n_protos, protos_off, 12)?;
        check_table(&data, "field_ids", n_fields, fields_off, 8)?;
        check_table(&data, "method_ids", n_methods, methods_off, 8)?;
        check_table(&data, "class_defs", n_classes, classes_off, 32)?;

        let mut strings = Vec::with_capacity(n_strings);
        let mut r = Reader::at(&data, strings_off)?;
        for _ in 0..n_strings {
            let off = r.u32()? as usize;
            strings.push(read_mutf8(&data, off).unwrap_or_else(|_| "<bad string>".into()));
        }
        let mut r = Reader::at(&data, types_off)?;
        let types = (0..n_types).map(|_| r.u32()).collect::<Result<Vec<_>>>()?;
        let mut r = Reader::at(&data, protos_off)?;
        let mut protos = Vec::with_capacity(n_protos);
        for _ in 0..n_protos {
            let (shorty, ret, params_off) = (r.u32()?, r.u32()?, r.u32()?);
            protos.push(Proto { shorty, ret, params: type_list(&data, params_off)? });
        }
        let mut r = Reader::at(&data, fields_off)?;
        let mut fields = Vec::with_capacity(n_fields);
        for _ in 0..n_fields {
            fields.push(FieldId { class: r.u16()? as u32, ty: r.u16()? as u32, name: r.u32()? });
        }
        let mut r = Reader::at(&data, methods_off)?;
        let mut methods = Vec::with_capacity(n_methods);
        for _ in 0..n_methods {
            methods.push(MethodId { class: r.u16()? as u32, proto: r.u16()? as u32, name: r.u32()? });
        }
        let mut r = Reader::at(&data, classes_off)?;
        let mut classes = Vec::with_capacity(n_classes);
        for _ in 0..n_classes {
            let class = r.u32()?;
            let access = r.u32()?;
            let superclass = Some(r.u32()?).filter(|&s| s != NO_INDEX);
            let interfaces_off = r.u32()?;
            let source_file = Some(r.u32()?).filter(|&s| s != NO_INDEX);
            classes.push(ClassDef {
                class,
                access,
                superclass,
                interfaces: type_list(&data, interfaces_off).unwrap_or_default(),
                source_file,
                annotations_off: r.u32()?,
                class_data_off: r.u32()?,
                static_values_off: r.u32()?,
            });
        }
        Ok(Self {
            data,
            name: name.to_owned(),
            version,
            strings,
            types,
            protos,
            fields,
            methods,
            classes,
            call_sites: vec![],
            jvm: None,
        })
    }

    /// A program model built from JVM class files (see [`crate::jvm`]).
    pub(crate) fn from_jvm(
        name: &str,
        pools: crate::jvm::Pools,
        classes: Vec<ClassDef>,
        call_sites: Vec<CallSite>,
        jvm: crate::jvm::Classes,
    ) -> Self {
        Self {
            data: vec![],
            name: name.to_owned(),
            version: 0,
            strings: pools.strings,
            types: pools.types,
            protos: pools.protos,
            fields: pools.fields,
            methods: pools.methods,
            classes,
            call_sites,
            jvm: Some(Box::new(jvm)),
        }
    }

    /// Whether this model was built from JVM class files.
    pub fn is_jvm(&self) -> bool {
        self.jvm.is_some()
    }

    pub fn string(&self, idx: u32) -> &str {
        self.strings.get(idx as usize).map_or("<bad string>", |s| s)
    }

    /// Type descriptor (`Lcom/example/Foo;`, `I`, `[J`).
    pub fn type_desc(&self, idx: u32) -> &str {
        self.types.get(idx as usize).map_or("<bad type>", |&s| self.string(s))
    }

    pub fn field(&self, idx: u32) -> Option<&FieldId> {
        self.fields.get(idx as usize)
    }

    pub fn method(&self, idx: u32) -> Option<&MethodId> {
        self.methods.get(idx as usize)
    }

    pub fn proto(&self, idx: u32) -> Option<&Proto> {
        self.protos.get(idx as usize)
    }

    /// `(I[Ljava/lang/String;)V` for a proto.
    pub fn proto_desc(&self, idx: u32) -> String {
        let Some(p) = self.proto(idx) else { return "()V".into() };
        let mut s = String::from("(");
        for &t in &p.params {
            s.push_str(self.type_desc(t));
        }
        s.push(')');
        s.push_str(self.type_desc(p.ret));
        s
    }

    pub fn call_site(&self, idx: u32) -> Option<&CallSite> {
        self.call_sites.get(idx as usize)
    }

    pub fn class_data(&self, c: &ClassDef) -> Result<ClassData> {
        if let Some(j) = &self.jvm {
            return Ok(j.class_data(c.class_data_off));
        }
        if c.class_data_off == 0 {
            return Ok(ClassData::default());
        }
        let mut r = Reader::at(&self.data, c.class_data_off as usize)?;
        let sizes = [r.uleb()?, r.uleb()?, r.uleb()?, r.uleb()?];
        if sizes.iter().map(|&n| n as usize).sum::<usize>() > self.data.len() {
            bail!("class data sizes out of range");
        }
        let fields = |n: u32, r: &mut Reader<'_>| -> Result<Vec<EncodedField>> {
            let mut idx = 0u32;
            (0..n)
                .map(|_| {
                    idx = idx.wrapping_add(r.uleb()?);
                    Ok(EncodedField { field: idx, access: r.uleb()? })
                })
                .collect()
        };
        let static_fields = fields(sizes[0], &mut r)?;
        let instance_fields = fields(sizes[1], &mut r)?;
        let methods = |n: u32, r: &mut Reader<'_>| -> Result<Vec<EncodedMethod>> {
            let mut idx = 0u32;
            (0..n)
                .map(|_| {
                    idx = idx.wrapping_add(r.uleb()?);
                    Ok(EncodedMethod { method: idx, access: r.uleb()?, code_off: r.uleb()? })
                })
                .collect()
        };
        let direct_methods = methods(sizes[2], &mut r)?;
        let virtual_methods = methods(sizes[3], &mut r)?;
        Ok(ClassData { static_fields, instance_fields, direct_methods, virtual_methods })
    }

    pub fn code(&self, off: u32) -> Result<CodeItem> {
        if let Some(j) = &self.jvm {
            return j.code(off);
        }
        let mut r = Reader::at(&self.data, off as usize)?;
        let registers = r.u16()?;
        let ins = r.u16()?;
        let outs = r.u16()?;
        let tries_size = r.u16()?;
        let debug_off = r.u32()?;
        let n = r.u32()? as usize;
        let raw = r.bytes(n.checked_mul(2).context("insns size overflow")?)?;
        let insns = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let mut tries = vec![];
        if tries_size > 0 {
            if n % 2 == 1 {
                r.u16()?;
            }
            let mut items = vec![];
            for _ in 0..tries_size {
                items.push((r.u32()?, r.u16()? as u32, r.u16()? as usize));
            }
            let handlers_base = r.pos;
            for (start, count, handler_off) in items {
                let mut h = Reader::at(&self.data, handlers_base + handler_off)?;
                let size = h.sleb()?;
                let mut handlers = vec![];
                for _ in 0..size.unsigned_abs().min(1024) {
                    handlers.push(Handler { ty: Some(h.uleb()?), addr: h.uleb()? });
                }
                if size <= 0 {
                    handlers.push(Handler { ty: None, addr: h.uleb()? });
                }
                tries.push(TryItem { start, count, handlers });
            }
        }
        Ok(CodeItem { registers, ins, outs, insns, tries, debug_off, prebuilt: None })
    }

    pub fn debug_info(&self, code: &CodeItem) -> Result<DebugInfo> {
        if let Some(p) = &code.prebuilt {
            return Ok(p.debug.clone());
        }
        if code.debug_off == 0 {
            return Ok(DebugInfo::default());
        }
        let mut r = Reader::at(&self.data, code.debug_off as usize)?;
        let mut line = r.uleb()?;
        let n_params = r.uleb()?;
        if n_params > 1024 {
            bail!("bad debug info");
        }
        let param_names = (0..n_params).map(|_| r.uleb_p1()).collect::<Result<Vec<_>>>()?;
        let mut info = DebugInfo { param_names, ..Default::default() };
        let mut addr = 0u32;
        let mut open: HashMap<u16, LocalVar> = HashMap::new();
        // The last ended local per register, for DBG_RESTART_LOCAL.
        let mut ended: HashMap<u16, LocalVar> = HashMap::new();
        let end = code.insns.len() as u32;
        let close = |v: LocalVar, at: u32, info: &mut DebugInfo, ended: &mut HashMap<u16, LocalVar>| {
            ended.insert(v.reg, v.clone());
            if at > v.start {
                info.locals.push(LocalVar { end: at, ..v });
            }
        };
        loop {
            let op = r.u8()?;
            match op {
                0 => break,
                1 => addr = addr.saturating_add(r.uleb()?),
                2 => line = line.wrapping_add_signed(r.sleb()?),
                3 | 4 => {
                    let reg = r.uleb()? as u16;
                    let name = r.uleb_p1()?;
                    let ty = r.uleb_p1()?;
                    let signature = if op == 4 { r.uleb_p1()? } else { None };
                    if let Some(v) = open.remove(&reg) {
                        close(v, addr, &mut info, &mut ended);
                    }
                    open.insert(reg, LocalVar { reg, start: addr, end, name, ty, signature });
                }
                5 => {
                    let reg = r.uleb()? as u16;
                    if let Some(v) = open.remove(&reg) {
                        close(v, addr, &mut info, &mut ended);
                    }
                }
                6 => {
                    let reg = r.uleb()? as u16;
                    if let Some(v) = ended.get(&reg).cloned() {
                        open.insert(reg, LocalVar { start: addr, end, ..v });
                    }
                }
                7 | 8 => {}
                9 => {
                    r.uleb_p1()?;
                }
                _ => {
                    let adj = (op - 0x0a) as i32;
                    line = line.wrapping_add_signed(adj % 15 - 4);
                    addr = addr.saturating_add((adj / 15) as u32);
                    info.lines.push((addr, line));
                }
            }
        }
        for (_, v) in open {
            close(v, end, &mut info, &mut ended);
        }
        info.locals.sort_by_key(|v| (v.start, v.reg));
        Ok(info)
    }

    pub fn encoded_array_at(&self, off: u32) -> Result<Vec<Value>> {
        if off == 0 {
            return Ok(vec![]);
        }
        if let Some(j) = &self.jvm {
            return Ok(j.static_values(off));
        }
        encoded_array(&mut Reader::at(&self.data, off as usize)?, 0)
    }

    pub fn annotations(&self, c: &ClassDef) -> Result<Annotations> {
        let mut out = Annotations::default();
        if c.annotations_off == 0 {
            return Ok(out);
        }
        if let Some(j) = &self.jvm {
            return Ok(j.annotations(c.annotations_off));
        }
        let mut r = Reader::at(&self.data, c.annotations_off as usize)?;
        let class_off = r.u32()?;
        let (nf, nm, np) = (r.u32()?, r.u32()?, r.u32()?);
        if (nf as usize + nm as usize + np as usize) * 8 > self.data.len() {
            bail!("annotation directory out of range");
        }
        out.class = self.annotation_set(class_off)?;
        for _ in 0..nf {
            let (idx, off) = (r.u32()?, r.u32()?);
            out.fields.insert(idx, self.annotation_set(off)?);
        }
        for _ in 0..nm {
            let (idx, off) = (r.u32()?, r.u32()?);
            out.methods.insert(idx, self.annotation_set(off)?);
        }
        for _ in 0..np {
            let (idx, off) = (r.u32()?, r.u32()?);
            let mut l = Reader::at(&self.data, off as usize)?;
            let n = l.u32()?.min(255);
            let sets = (0..n).map(|_| self.annotation_set(l.u32()?)).collect::<Result<Vec<_>>>()?;
            out.params.insert(idx, sets);
        }
        Ok(out)
    }

    fn annotation_set(&self, off: u32) -> Result<Vec<Annotation>> {
        if off == 0 {
            return Ok(vec![]);
        }
        let mut r = Reader::at(&self.data, off as usize)?;
        let n = r.u32()?;
        if n as usize * 4 > self.data.len() {
            bail!("annotation set out of range");
        }
        let mut out = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let item = r.u32()? as usize;
            let mut a = Reader::at(&self.data, item)?;
            let visibility = a.u8()?;
            out.push(Annotation { visibility, value: encoded_annotation(&mut a, 0)? });
        }
        Ok(out)
    }
}

fn type_list(data: &[u8], off: u32) -> Result<Vec<u32>> {
    if off == 0 {
        return Ok(vec![]);
    }
    let mut r = Reader::at(data, off as usize)?;
    let n = r.u32()?;
    if n > 65535 {
        bail!("type list too long");
    }
    (0..n).map(|_| Ok(r.u16()? as u32)).collect()
}

/// Decodes a MUTF-8 string (`uleb128 utf16_size`, bytes, NUL).
fn read_mutf8(data: &[u8], off: usize) -> Result<Box<str>> {
    let mut r = Reader::at(data, off)?;
    let n = r.uleb()? as usize;
    let mut units: Vec<u16> = Vec::with_capacity(n.min(1 << 16));
    loop {
        let a = r.u8()? as u16;
        if a == 0 {
            break;
        }
        if a < 0x80 {
            units.push(a);
        } else if a & 0xe0 == 0xc0 {
            let b = r.u8()? as u16;
            units.push(((a & 0x1f) << 6) | (b & 0x3f));
        } else if a & 0xf0 == 0xe0 {
            let b = r.u8()? as u16;
            let c = r.u8()? as u16;
            units.push(((a & 0x0f) << 12) | ((b & 0x3f) << 6) | (c & 0x3f));
        } else {
            units.push(0xfffd);
        }
    }
    Ok(String::from_utf16_lossy(&units).into_boxed_str())
}

fn encoded_value(r: &mut Reader<'_>, depth: u32) -> Result<Value> {
    if depth > 32 {
        bail!("encoded value nested too deeply");
    }
    let h = r.u8()?;
    let (ty, arg) = (h & 0x1f, (h >> 5) as usize);
    let mut int = |signed: bool| -> Result<u64> {
        let b = r.bytes(arg + 1)?;
        let mut v = 0u64;
        for (i, &x) in b.iter().enumerate() {
            v |= (x as u64) << (8 * i);
        }
        let bits = 8 * (arg as u32 + 1);
        if signed && bits < 64 && v & (1 << (bits - 1)) != 0 {
            v |= u64::MAX << bits;
        }
        Ok(v)
    };
    Ok(match ty {
        0x00 => Value::Byte(int(true)? as i8),
        0x02 => Value::Short(int(true)? as i16),
        0x03 => Value::Char(int(false)? as u16),
        0x04 => Value::Int(int(true)? as i32),
        0x06 => Value::Long(int(true)? as i64),
        // Floats are zero-extended to the right.
        0x10 => Value::Float(f32::from_bits((int(false)? << (8 * (3 - arg.min(3)))) as u32)),
        0x11 => Value::Double(f64::from_bits(int(false)? << (8 * (7 - arg.min(7))))),
        0x15 => Value::MethodType(int(false)? as u32),
        0x16 => Value::MethodHandle(int(false)? as u32),
        0x17 => Value::String(int(false)? as u32),
        0x18 => Value::Type(int(false)? as u32),
        0x19 => Value::Field(int(false)? as u32),
        0x1a => Value::Method(int(false)? as u32),
        0x1b => Value::Enum(int(false)? as u32),
        0x1c => Value::Array(encoded_array(r, depth + 1)?),
        0x1d => Value::Annotation(encoded_annotation(r, depth + 1)?),
        0x1e => Value::Null,
        0x1f => Value::Boolean(arg != 0),
        t => bail!("unknown encoded value type {t:#x}"),
    })
}

fn encoded_array(r: &mut Reader<'_>, depth: u32) -> Result<Vec<Value>> {
    let n = r.uleb()?;
    // Every value takes at least one byte.
    if n as usize > r.remaining() {
        bail!("encoded array too long");
    }
    (0..n).map(|_| encoded_value(r, depth)).collect()
}

fn encoded_annotation(r: &mut Reader<'_>, depth: u32) -> Result<EncodedAnnotation> {
    let ty = r.uleb()?;
    let n = r.uleb()?.min(4096);
    let mut elements = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let name = r.uleb()?;
        elements.push((name, encoded_value(r, depth)?));
    }
    Ok(EncodedAnnotation { ty, elements })
}

/// Finds a system annotation (`Ldalvik/annotation/Signature;`, ...) in a set.
pub fn find_annotation<'a>(dex: &DexFile, set: &'a [Annotation], ty: &str) -> Option<&'a EncodedAnnotation> {
    set.iter().map(|a| &a.value).find(|a| dex.type_desc(a.ty) == ty)
}

/// The `value` element of an annotation.
pub fn annotation_value<'a>(dex: &DexFile, a: &'a EncodedAnnotation, name: &str) -> Option<&'a Value> {
    a.elements.iter().find(|(n, _)| dex.string(*n) == name).map(|(_, v)| v)
}
