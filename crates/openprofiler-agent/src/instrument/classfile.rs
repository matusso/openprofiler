//! Minimal class-file reader and method rewriter for the ClassFileLoadHook.
//!
//! Only what instrumentation needs is decoded: the constant pool (to read names and append new
//! entries), the class header, and methods with their `Code` attributes. Everything else is
//! copied byte for byte. A rewritten method gets, depending on its [`MethodPlan`]:
//!
//! - an entry sequence before the first instruction (call counter, method trigger, probe entry
//!   with `this` and the boxed arguments);
//! - an exit sequence before every return instruction (probe exits receive the return value,
//!   boxed, or `this` for constructors);
//! - a catch-all handler at the end of the code that runs the exit hooks for exceptions and
//!   rethrows. In constructors it covers only the code after the `super(...)`/`this(...)` call,
//!   because an exception handler cannot cover code where `this` is uninitialized.
//!
//! Branch offsets, switch padding, the exception table, `LineNumberTable`, `LocalVariable(Type)Table`
//! and `StackMapTable` (including `Uninitialized` offsets) are remapped; the handler gets a full
//! frame with no locals, which is valid for every instruction it covers. Type annotations on code
//! are dropped. A method is left unchanged when it uses `jsr`/`ret`, when a branch no longer fits
//! its 16-bit offset, or when the code would exceed 64 KB.

use std::collections::HashMap;

pub const HOOKS: &str = "dev/openprofiler/boot/Hooks";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, Error>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8], pos: usize) -> Self {
        Self { b, pos }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos + n).ok_or_else(|| Error("truncated class file".into()))?;
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[derive(Debug, Clone, Copy)]
enum Cp {
    /// Byte range of the modified-UTF-8 text.
    Utf8(usize, usize),
    Class(u16),
    Integer(i32),
    Other,
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: u16,
    /// Range of the attribute body (after the length).
    pub body: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct Method {
    pub access: u16,
    pub name: String,
    pub desc: String,
    /// Range of the whole `method_info`.
    pub range: (usize, usize),
    name_idx: u16,
    desc_idx: u16,
    attrs: Vec<Attribute>,
}

impl Method {
    pub fn is_static(&self) -> bool {
        self.access & 0x0008 != 0
    }

    pub fn has_code(&self) -> bool {
        self.access & (0x0100 | 0x0400) == 0
    }

    pub fn is_constructor(&self) -> bool {
        self.name == "<init>"
    }
}

/// A runtime-visible or -invisible annotation with its constant element values as strings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Annotation {
    /// Type descriptor, e.g. `Ldev/openprofiler/api/Probe;`.
    pub desc: String,
    pub values: Vec<(String, String)>,
}

impl Annotation {
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

pub struct ClassFile<'a> {
    bytes: &'a [u8],
    pub major: u16,
    cp: Vec<Cp>,
    /// Byte offset of each constant's tag.
    cp_off: Vec<usize>,
    cp_end: usize,
    pub access: u16,
    pub name: String,
    pub super_name: Option<String>,
    pub interfaces: Vec<String>,
    /// Offset of the `methods_count` field.
    methods_start: usize,
    pub methods: Vec<Method>,
    methods_end: usize,
}

/// Decodes modified UTF-8 (lossy for invalid sequences).
fn mutf8(b: &[u8]) -> String {
    if b.is_ascii() {
        return String::from_utf8_lossy(b).into_owned();
    }
    let mut units: Vec<u16> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as u16;
        if c & 0x80 == 0 {
            units.push(c);
            i += 1;
        } else if c & 0xe0 == 0xc0 && i + 1 < b.len() {
            units.push(((c & 0x1f) << 6) | (b[i + 1] as u16 & 0x3f));
            i += 2;
        } else if c & 0xf0 == 0xe0 && i + 2 < b.len() {
            units.push(((c & 0x0f) << 12) | ((b[i + 1] as u16 & 0x3f) << 6) | (b[i + 2] as u16 & 0x3f));
            i += 3;
        } else {
            units.push(0xfffd);
            i += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

fn read_attrs(r: &mut Reader<'_>) -> Result<Vec<Attribute>> {
    let n = r.u16()?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let name = r.u16()?;
        let len = r.u32()? as usize;
        let start = r.pos;
        r.bytes(len)?;
        out.push(Attribute { name, body: (start, start + len) });
    }
    Ok(out)
}

impl<'a> ClassFile<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut r = Reader::new(bytes, 0);
        if r.u32()? != 0xCAFE_BABE {
            return err("bad magic");
        }
        let _minor = r.u16()?;
        let major = r.u16()?;
        let count = r.u16()? as usize;
        let mut cp = vec![Cp::Other; count.max(1)];
        let mut cp_off = vec![0; count.max(1)];
        let mut i = 1;
        while i < count {
            cp_off[i] = r.pos;
            let tag = r.u8()?;
            match tag {
                1 => {
                    let len = r.u16()? as usize;
                    let s = r.pos;
                    r.bytes(len)?;
                    cp[i] = Cp::Utf8(s, s + len);
                }
                3 => cp[i] = Cp::Integer(r.u32()? as i32),
                4 => {
                    r.bytes(4)?;
                }
                5 | 6 => {
                    r.bytes(8)?;
                    i += 1;
                }
                7 => cp[i] = Cp::Class(r.u16()?),
                8 | 16 | 19 | 20 => {
                    r.bytes(2)?;
                }
                9 | 10 | 11 | 12 | 17 | 18 => {
                    r.bytes(4)?;
                }
                15 => {
                    r.bytes(3)?;
                }
                t => return err(format!("bad constant pool tag {t}")),
            }
            i += 1;
        }
        let cp_end = r.pos;
        let access = r.u16()?;
        let this_class = r.u16()?;
        let super_class = r.u16()?;
        let n_if = r.u16()?;
        let mut ifs = vec![];
        for _ in 0..n_if {
            ifs.push(r.u16()?);
        }
        let n_fields = r.u16()?;
        for _ in 0..n_fields {
            r.bytes(6)?;
            read_attrs(&mut r)?;
        }
        let methods_start = r.pos;
        let n_methods = r.u16()?;
        let mut methods = Vec::with_capacity(n_methods as usize);
        let mut cf = ClassFile {
            bytes,
            major,
            cp,
            cp_off,
            cp_end,
            access,
            name: String::new(),
            super_name: None,
            interfaces: vec![],
            methods_start,
            methods: vec![],
            methods_end: 0,
        };
        for _ in 0..n_methods {
            let start = r.pos;
            let access = r.u16()?;
            let name_idx = r.u16()?;
            let desc_idx = r.u16()?;
            let attrs = read_attrs(&mut r)?;
            methods.push(Method {
                access,
                name: cf.utf8(name_idx)?,
                desc: cf.utf8(desc_idx)?,
                range: (start, r.pos),
                name_idx,
                desc_idx,
                attrs,
            });
        }
        cf.methods_end = r.pos;
        cf.name = cf.class_name(this_class)?;
        cf.super_name = if super_class == 0 { None } else { Some(cf.class_name(super_class)?) };
        cf.interfaces = ifs.iter().map(|&i| cf.class_name(i)).collect::<Result<_>>()?;
        cf.methods = methods;
        Ok(cf)
    }

    pub fn is_interface(&self) -> bool {
        self.access & 0x0200 != 0
    }

    fn utf8(&self, idx: u16) -> Result<String> {
        match self.cp.get(idx as usize) {
            Some(Cp::Utf8(s, e)) => Ok(mutf8(&self.bytes[*s..*e])),
            _ => err(format!("constant {idx} is not Utf8")),
        }
    }

    fn class_name(&self, idx: u16) -> Result<String> {
        match self.cp.get(idx as usize) {
            Some(Cp::Class(n)) => self.utf8(*n),
            _ => err(format!("constant {idx} is not a class")),
        }
    }

    fn attr_name(&self, a: &Attribute) -> String {
        self.utf8(a.name).unwrap_or_default()
    }

    /// Annotations (visible and invisible) declared on method `m`.
    pub fn method_annotations(&self, m: &Method) -> Vec<Annotation> {
        let mut out = vec![];
        for a in &m.attrs {
            let n = self.attr_name(a);
            if n != "RuntimeVisibleAnnotations" && n != "RuntimeInvisibleAnnotations" {
                continue;
            }
            let mut r = Reader::new(self.bytes, a.body.0);
            let Ok(count) = r.u16() else { continue };
            for _ in 0..count {
                match self.annotation(&mut r) {
                    Ok(an) => out.push(an),
                    Err(_) => break,
                }
            }
        }
        out
    }

    fn annotation(&self, r: &mut Reader<'_>) -> Result<Annotation> {
        let desc = self.utf8(r.u16()?)?;
        let n = r.u16()?;
        let mut values = vec![];
        for _ in 0..n {
            let name = self.utf8(r.u16()?)?;
            if let Some(v) = self.element_value(r)? {
                values.push((name, v));
            }
        }
        Ok(Annotation { desc, values })
    }

    /// Reads an element value; constants are returned as strings, everything else skipped.
    fn element_value(&self, r: &mut Reader<'_>) -> Result<Option<String>> {
        let tag = r.u8()?;
        Ok(match tag {
            b's' => Some(self.utf8(r.u16()?)?),
            b'I' | b'S' | b'B' | b'C' => match self.cp.get(r.u16()? as usize) {
                Some(Cp::Integer(v)) => Some(v.to_string()),
                _ => None,
            },
            b'Z' => match self.cp.get(r.u16()? as usize) {
                Some(Cp::Integer(v)) => Some((*v != 0).to_string()),
                _ => None,
            },
            b'J' | b'F' | b'D' | b'c' => {
                r.u16()?;
                None
            }
            b'e' => {
                r.u16()?;
                Some(self.utf8(r.u16()?)?)
            }
            b'@' => {
                self.annotation(r)?;
                None
            }
            b'[' => {
                let n = r.u16()?;
                for _ in 0..n {
                    self.element_value(r)?;
                }
                None
            }
            t => return err(format!("bad element value tag {t}")),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Constant pool additions

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Utf8(String),
    Class(String),
    Nat(String, String),
    Method(String, String, String),
    Int(i32),
}

struct Pool {
    count: usize,
    extra: Vec<u8>,
    added: HashMap<Key, u16>,
}

fn mutf8_encode(s: &str) -> Vec<u8> {
    let mut out = vec![];
    for u in s.encode_utf16() {
        match u {
            1..=0x7f => out.push(u as u8),
            0 | 0x80..=0x7ff => {
                out.push(0xc0 | (u >> 6) as u8);
                out.push(0x80 | (u & 0x3f) as u8);
            }
            _ => {
                out.push(0xe0 | (u >> 12) as u8);
                out.push(0x80 | ((u >> 6) & 0x3f) as u8);
                out.push(0x80 | (u & 0x3f) as u8);
            }
        }
    }
    out
}

impl Pool {
    fn get(&mut self, key: Key) -> Result<u16> {
        if let Some(&i) = self.added.get(&key) {
            return Ok(i);
        }
        let mut body = vec![];
        match &key {
            Key::Utf8(s) => {
                let b = mutf8_encode(s);
                body.push(1);
                body.extend_from_slice(&(b.len() as u16).to_be_bytes());
                body.extend_from_slice(&b);
            }
            Key::Class(n) => {
                let u = self.get(Key::Utf8(n.clone()))?;
                body.push(7);
                body.extend_from_slice(&u.to_be_bytes());
            }
            Key::Nat(n, d) => {
                let a = self.get(Key::Utf8(n.clone()))?;
                let b = self.get(Key::Utf8(d.clone()))?;
                body.push(12);
                body.extend_from_slice(&a.to_be_bytes());
                body.extend_from_slice(&b.to_be_bytes());
            }
            Key::Method(c, n, d) => {
                let a = self.get(Key::Class(c.clone()))?;
                let b = self.get(Key::Nat(n.clone(), d.clone()))?;
                body.push(10);
                body.extend_from_slice(&a.to_be_bytes());
                body.extend_from_slice(&b.to_be_bytes());
            }
            Key::Int(v) => {
                body.push(3);
                body.extend_from_slice(&v.to_be_bytes());
            }
        }
        if self.count >= 0xffff {
            return err("constant pool full");
        }
        let idx = self.count as u16;
        self.count += 1;
        self.extra.extend_from_slice(&body);
        self.added.insert(key, idx);
        Ok(idx)
    }

    fn method(&mut self, class: &str, name: &str, desc: &str) -> Result<u16> {
        self.get(Key::Method(class.into(), name.into(), desc.into()))
    }
}

// ---------------------------------------------------------------------------------------------
// Plans

/// A probe interception on one method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSite {
    pub site: u32,
    /// Pass the arguments as an `Object[]` (boxed) to the entry hook.
    pub args: bool,
    /// Call the entry hook (otherwise only exits are reported).
    pub entry: bool,
}

/// What to inject into one method.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MethodPlan {
    pub method: usize,
    /// Instrumented call counting (`Hooks.callEnter`/`callExit`).
    pub call_id: Option<u32>,
    pub probe: Option<ProbeSite>,
    /// Method trigger (`Hooks.trigger`) at entry.
    pub trigger: Option<u32>,
}

impl MethodPlan {
    fn has_exit(&self) -> bool {
        self.call_id.is_some() || self.probe.is_some()
    }
}

// ---------------------------------------------------------------------------------------------
// Bytecode

const RETURNS: std::ops::RangeInclusive<u8> = 0xac..=0xb1;

fn is_branch16(op: u8) -> bool {
    matches!(op, 0x99..=0xa8 | 0xc6 | 0xc7)
}

fn switch_pad(pc: usize) -> usize {
    (4 - (pc + 1) % 4) % 4
}

fn i32_at(code: &[u8], at: usize) -> Result<i32> {
    code.get(at..at + 4)
        .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| Error("truncated switch".into()))
}

/// Length of the instruction at `pc` (old layout).
fn insn_len(code: &[u8], pc: usize) -> Result<usize> {
    let op = code[pc];
    Ok(match op {
        0x00..=0x0f | 0x1a..=0x35 | 0x3b..=0x83 | 0x85..=0x98 | 0xac..=0xb1 | 0xbe | 0xbf | 0xc2 | 0xc3 => 1,
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11 | 0x13 | 0x14 | 0x84 | 0x99..=0xa8 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc6 | 0xc7 => 3,
        0xc5 => 4,
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        0xc4 => {
            if code.get(pc + 1) == Some(&0x84) {
                6
            } else {
                4
            }
        }
        0xaa => {
            let p = pc + 1 + switch_pad(pc);
            let lo = i32_at(code, p + 4)?;
            let hi = i32_at(code, p + 8)?;
            if hi < lo {
                return err("bad tableswitch");
            }
            p + 12 + 4 * ((hi as i64 - lo as i64 + 1) as usize) - pc
        }
        0xab => {
            let p = pc + 1 + switch_pad(pc);
            let n = i32_at(code, p + 4)?;
            if n < 0 {
                return err("bad lookupswitch");
            }
            p + 8 + 8 * n as usize - pc
        }
        _ => return err(format!("bad opcode {op:#x}")),
    })
}

struct Emit<'p> {
    code: Vec<u8>,
    pool: &'p mut Pool,
}

impl Emit<'_> {
    fn op(&mut self, b: u8) {
        self.code.push(b);
    }

    fn u16(&mut self, v: u16) {
        self.code.extend_from_slice(&v.to_be_bytes());
    }

    fn int(&mut self, v: i32) -> Result<()> {
        match v {
            -1..=5 => self.op((0x03 + v) as u8),
            -128..=127 => {
                self.op(0x10);
                self.op(v as u8);
            }
            -32768..=32767 => {
                self.op(0x11);
                self.u16(v as i16 as u16);
            }
            _ => {
                let i = self.pool.get(Key::Int(v))?;
                self.op(0x13);
                self.u16(i);
            }
        }
        Ok(())
    }

    fn invokestatic(&mut self, class: &str, name: &str, desc: &str) -> Result<()> {
        let m = self.pool.method(class, name, desc)?;
        self.op(0xb8);
        self.u16(m);
        Ok(())
    }

    fn hook(&mut self, name: &str, desc: &str) -> Result<()> {
        self.invokestatic(HOOKS, name, desc)
    }

    fn load(&mut self, short: u8, long: u8, slot: u16) {
        if slot <= 3 {
            self.op(short + slot as u8);
        } else if slot <= 255 {
            self.op(long);
            self.op(slot as u8);
        } else {
            self.op(0xc4);
            self.op(long);
            self.u16(slot);
        }
    }

    /// Boxes the primitive of descriptor char `t` on top of the stack.
    fn box_value(&mut self, t: u8) -> Result<()> {
        let (class, desc) = match t {
            b'Z' => ("java/lang/Boolean", "(Z)Ljava/lang/Boolean;"),
            b'B' => ("java/lang/Byte", "(B)Ljava/lang/Byte;"),
            b'C' => ("java/lang/Character", "(C)Ljava/lang/Character;"),
            b'S' => ("java/lang/Short", "(S)Ljava/lang/Short;"),
            b'I' => ("java/lang/Integer", "(I)Ljava/lang/Integer;"),
            b'J' => ("java/lang/Long", "(J)Ljava/lang/Long;"),
            b'F' => ("java/lang/Float", "(F)Ljava/lang/Float;"),
            b'D' => ("java/lang/Double", "(D)Ljava/lang/Double;"),
            _ => return Ok(()),
        };
        self.invokestatic(class, "valueOf", desc)
    }
}

/// Parameter type tags of a method descriptor: the first byte of each parameter type (`L`
/// and `[` for references).
fn param_tags(desc: &str) -> Result<Vec<u8>> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return err("bad descriptor");
    }
    let mut out = vec![];
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        let start = i;
        while b[i] == b'[' {
            i += 1;
        }
        if b[i] == b'L' {
            while i < b.len() && b[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        out.push(b[start]);
        if i > b.len() {
            return err("bad descriptor");
        }
    }
    Ok(out)
}

fn entry_code(e: &mut Emit<'_>, m: &Method, plan: &MethodPlan) -> Result<()> {
    if let Some(t) = plan.trigger {
        e.int(t as i32)?;
        e.hook("trigger", "(I)V")?;
    }
    if let Some(id) = plan.call_id {
        e.int(id as i32)?;
        e.hook("callEnter", "(I)V")?;
    }
    if let Some(p) = plan.probe
        && p.entry
    {
        e.int(p.site as i32)?;
        if m.is_static() || m.is_constructor() {
            e.op(0x01);
        } else {
            e.op(0x2a);
        }
        if p.args {
            let tags = param_tags(&m.desc)?;
            e.int(tags.len() as i32)?;
            let obj = e.pool.get(Key::Class("java/lang/Object".into()))?;
            e.op(0xbd);
            e.u16(obj);
            let mut slot: u16 = if m.is_static() { 0 } else { 1 };
            for (i, &t) in tags.iter().enumerate() {
                e.op(0x59);
                e.int(i as i32)?;
                match t {
                    b'J' => e.load(0x1e, 0x16, slot),
                    b'F' => e.load(0x22, 0x17, slot),
                    b'D' => e.load(0x26, 0x18, slot),
                    b'L' | b'[' => e.load(0x2a, 0x19, slot),
                    _ => e.load(0x1a, 0x15, slot),
                }
                e.box_value(t)?;
                e.op(0x53);
                slot += if matches!(t, b'J' | b'D') { 2 } else { 1 };
            }
        } else {
            e.op(0x01);
        }
        e.hook("enter", "(ILjava/lang/Object;[Ljava/lang/Object;)V")?;
    }
    Ok(())
}

/// Exit hooks before a return instruction `op`. Probe exits get the return value.
fn exit_code(e: &mut Emit<'_>, m: &Method, plan: &MethodPlan, op: u8) -> Result<()> {
    if let Some(p) = plan.probe {
        match op {
            0xb1 if m.is_constructor() => e.op(0x2a),
            0xb1 => e.op(0x01),
            0xb0 => e.op(0x59),
            0xad | 0xaf => {
                e.op(0x5c);
                e.box_value(if op == 0xad { b'J' } else { b'D' })?;
            }
            _ => {
                e.op(0x59);
                let ret = m.desc.rsplit(')').next().and_then(|r| r.bytes().next()).unwrap_or(b'I');
                e.box_value(if op == 0xae { b'F' } else { ret })?;
            }
        }
        e.int(p.site as i32)?;
        e.hook("exit", "(Ljava/lang/Object;I)V")?;
    }
    if let Some(id) = plan.call_id {
        e.int(id as i32)?;
        e.hook("callExit", "(I)V")?;
    }
    Ok(())
}

/// Catch-all handler body: exception on the stack, exit hooks, rethrow.
fn handler_code(e: &mut Emit<'_>, plan: &MethodPlan) -> Result<()> {
    if let Some(p) = plan.probe {
        e.op(0x59);
        e.int(p.site as i32)?;
        e.hook("exitThrow", "(Ljava/lang/Throwable;I)V")?;
    }
    if let Some(id) = plan.call_id {
        e.int(id as i32)?;
        e.hook("callExit", "(I)V")?;
    }
    e.op(0xbf);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// StackMapTable

#[derive(Debug, Clone)]
enum Vt {
    Simple(u8),
    Object(u16),
    Uninit(usize),
}

#[derive(Debug, Clone)]
enum FrameBody {
    Same,
    Same1(Vt),
    Chop(u8),
    Append(Vec<Vt>),
    Full(Vec<Vt>, Vec<Vt>),
}

fn read_vt(r: &mut Reader<'_>) -> Result<Vt> {
    Ok(match r.u8()? {
        7 => Vt::Object(r.u16()?),
        8 => Vt::Uninit(r.u16()? as usize),
        t @ 0..=6 => Vt::Simple(t),
        t => return err(format!("bad verification type {t}")),
    })
}

fn read_frames(b: &[u8]) -> Result<Vec<(usize, FrameBody)>> {
    let mut r = Reader::new(b, 0);
    let n = r.u16()?;
    let mut out = Vec::with_capacity(n as usize);
    let mut prev: Option<usize> = None;
    for _ in 0..n {
        let t = r.u8()?;
        let (delta, body) = match t {
            0..=63 => (t as usize, FrameBody::Same),
            64..=127 => ((t - 64) as usize, FrameBody::Same1(read_vt(&mut r)?)),
            247 => {
                let d = r.u16()? as usize;
                (d, FrameBody::Same1(read_vt(&mut r)?))
            }
            248..=250 => (r.u16()? as usize, FrameBody::Chop(251 - t)),
            251 => (r.u16()? as usize, FrameBody::Same),
            252..=254 => {
                let d = r.u16()? as usize;
                let vts = (0..t - 251).map(|_| read_vt(&mut r)).collect::<Result<_>>()?;
                (d, FrameBody::Append(vts))
            }
            255 => {
                let d = r.u16()? as usize;
                let nl = r.u16()?;
                let locals = (0..nl).map(|_| read_vt(&mut r)).collect::<Result<_>>()?;
                let ns = r.u16()?;
                let stack = (0..ns).map(|_| read_vt(&mut r)).collect::<Result<_>>()?;
                (d, FrameBody::Full(locals, stack))
            }
            t => return err(format!("bad frame type {t}")),
        };
        let off = match prev {
            None => delta,
            Some(p) => p + delta + 1,
        };
        prev = Some(off);
        out.push((off, body));
    }
    Ok(out)
}

fn write_vt(out: &mut Vec<u8>, vt: &Vt, map_new: &dyn Fn(usize) -> usize) {
    match vt {
        Vt::Simple(t) => out.push(*t),
        Vt::Object(c) => {
            out.push(7);
            out.extend_from_slice(&c.to_be_bytes());
        }
        Vt::Uninit(o) => {
            out.push(8);
            out.extend_from_slice(&(map_new(*o) as u16).to_be_bytes());
        }
    }
}

fn write_frames(frames: &[(usize, FrameBody)], map_new: &dyn Fn(usize) -> usize) -> Result<Vec<u8>> {
    let mut out = (frames.len() as u16).to_be_bytes().to_vec();
    let mut prev: Option<usize> = None;
    for (off, body) in frames {
        let delta = match prev {
            None => *off,
            Some(p) if *off > p => off - p - 1,
            Some(_) => return err("frames out of order"),
        };
        if delta > 0xffff {
            return err("frame offset too large");
        }
        prev = Some(*off);
        let d = (delta as u16).to_be_bytes();
        match body {
            FrameBody::Same if delta <= 63 => out.push(delta as u8),
            FrameBody::Same => {
                out.push(251);
                out.extend_from_slice(&d);
            }
            FrameBody::Same1(vt) => {
                if delta <= 63 {
                    out.push(64 + delta as u8);
                } else {
                    out.push(247);
                    out.extend_from_slice(&d);
                }
                write_vt(&mut out, vt, map_new);
            }
            FrameBody::Chop(k) => {
                out.push(251 - k);
                out.extend_from_slice(&d);
            }
            FrameBody::Append(vts) => {
                out.push(251 + vts.len() as u8);
                out.extend_from_slice(&d);
                for vt in vts {
                    write_vt(&mut out, vt, map_new);
                }
            }
            FrameBody::Full(locals, stack) => {
                out.push(255);
                out.extend_from_slice(&d);
                out.extend_from_slice(&(locals.len() as u16).to_be_bytes());
                for vt in locals {
                    write_vt(&mut out, vt, map_new);
                }
                out.extend_from_slice(&(stack.len() as u16).to_be_bytes());
                for vt in stack {
                    write_vt(&mut out, vt, map_new);
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Method rewriting

/// `pc` just past the instruction that initializes `this` in a constructor (`super(...)` or
/// `this(...)`): the first `invokespecial <init>` not matched by an earlier `new`.
fn this_init_end(cf: &ClassFile<'_>, code: &[u8], insns: &[(usize, usize)]) -> Option<usize> {
    let mut pending_new = 0usize;
    for &(pc, len) in insns {
        match code[pc] {
            0xbb => pending_new += 1,
            0xb7 => {
                let idx = u16::from_be_bytes([code[pc + 1], code[pc + 2]]);
                if !cf.is_init_ref(idx) {
                    continue;
                }
                if pending_new == 0 {
                    return Some(pc + len);
                }
                pending_new -= 1;
            }
            _ => {}
        }
    }
    None
}

impl ClassFile<'_> {
    /// Whether constant `idx` is a method reference named `<init>`.
    fn is_init_ref(&self, idx: u16) -> bool {
        // Methodref: tag 10 | class u16 | nat u16; NameAndType: tag 12 | name u16 | desc u16.
        let Some(off) = self.cp_offset(idx) else { return false };
        let b = self.bytes;
        if b[off] != 10 && b[off] != 11 {
            return false;
        }
        let nat = u16::from_be_bytes([b[off + 3], b[off + 4]]);
        let Some(n) = self.cp_offset(nat) else { return false };
        let name = u16::from_be_bytes([b[n + 1], b[n + 2]]);
        self.utf8(name).is_ok_and(|s| s == "<init>")
    }

    fn cp_offset(&self, idx: u16) -> Option<usize> {
        self.cp_off.get(idx as usize).copied().filter(|&o| o != 0)
    }
}

/// Rewrites the `Code` attribute body of `m`. Returns the new attribute body.
fn rewrite_code(
    cf: &ClassFile<'_>,
    m: &Method,
    body: &[u8],
    plan: &MethodPlan,
    pool: &mut Pool,
) -> Result<Vec<u8>> {
    let mut r = Reader::new(body, 0);
    let max_stack = r.u16()?;
    let max_locals = r.u16()?;
    let code_len = r.u32()? as usize;
    let code = r.bytes(code_len)?;
    let n_ex = r.u16()?;
    let mut extab = vec![];
    for _ in 0..n_ex {
        extab.push((r.u16()? as usize, r.u16()? as usize, r.u16()? as usize, r.u16()?));
    }
    let attrs = read_attrs(&mut r)?;

    // Decode instruction boundaries.
    let mut insns = vec![];
    let mut pc = 0;
    while pc < code_len {
        let len = insn_len(code, pc)?;
        if pc + len > code_len {
            return err("instruction overruns code");
        }
        if matches!(code[pc], 0xa8 | 0xa9 | 0xc9) || (code[pc] == 0xc4 && code[pc + 1] == 0xa9) {
            return err("jsr/ret");
        }
        insns.push((pc, len));
        pc += len;
    }

    // Pre-generate the injected sequences (their lengths don't depend on position).
    let mut e = Emit { code: vec![], pool };
    entry_code(&mut e, m, plan)?;
    let entry = std::mem::take(&mut e.code);
    let mut exits: HashMap<u8, Vec<u8>> = HashMap::new();
    if plan.has_exit() {
        for &(pc, _) in &insns {
            let op = code[pc];
            if RETURNS.contains(&op) && !exits.contains_key(&op) {
                exit_code(&mut e, m, plan, op)?;
                exits.insert(op, std::mem::take(&mut e.code));
            }
        }
    }

    // Layout: `target[i]` is where branches to old instruction i land (before its injected
    // exit code), `start[i]` where the instruction itself starts.
    let mut target = HashMap::with_capacity(insns.len() + 1);
    let mut start = HashMap::with_capacity(insns.len());
    let mut npc = entry.len();
    for &(pc, len) in &insns {
        target.insert(pc, npc);
        if let Some(x) = exits.get(&code[pc]) {
            npc += x.len();
        }
        start.insert(pc, npc);
        npc += match code[pc] {
            0xaa | 0xab => len - switch_pad(pc) + switch_pad(npc),
            _ => len,
        };
    }
    target.insert(code_len, npc);
    let body_end = npc;
    let map = |old: usize| -> Result<usize> {
        target.get(&old).copied().ok_or_else(|| Error(format!("offset {old} is not an instruction")))
    };

    // Emit.
    let mut out = entry;
    for &(pc, len) in &insns {
        let op = code[pc];
        if let Some(x) = exits.get(&op) {
            out.extend_from_slice(x);
        }
        let here = start[&pc];
        debug_assert_eq!(out.len(), here);
        let rel =
            |old_off: i64| -> Result<i64> { Ok(map((pc as i64 + old_off) as usize)? as i64 - here as i64) };
        if is_branch16(op) {
            let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i64;
            let n = rel(off)?;
            if n < i16::MIN as i64 || n > i16::MAX as i64 {
                return err("branch offset overflow");
            }
            out.push(op);
            out.extend_from_slice(&(n as i16).to_be_bytes());
        } else if op == 0xc8 {
            let off = i32_at(code, pc + 1)? as i64;
            out.push(op);
            out.extend_from_slice(&(rel(off)? as i32).to_be_bytes());
        } else if op == 0xaa || op == 0xab {
            let p = pc + 1 + switch_pad(pc);
            out.push(op);
            out.extend(std::iter::repeat_n(0u8, switch_pad(here)));
            let default = i32_at(code, p)? as i64;
            out.extend_from_slice(&(rel(default)? as i32).to_be_bytes());
            if op == 0xaa {
                let lo = i32_at(code, p + 4)?;
                let hi = i32_at(code, p + 8)?;
                out.extend_from_slice(&lo.to_be_bytes());
                out.extend_from_slice(&hi.to_be_bytes());
                for k in 0..(hi as i64 - lo as i64 + 1) as usize {
                    let o = i32_at(code, p + 12 + 4 * k)? as i64;
                    out.extend_from_slice(&(rel(o)? as i32).to_be_bytes());
                }
            } else {
                let n = i32_at(code, p + 4)?;
                out.extend_from_slice(&n.to_be_bytes());
                for k in 0..n as usize {
                    let key = i32_at(code, p + 8 + 8 * k)?;
                    let o = i32_at(code, p + 12 + 8 * k)? as i64;
                    out.extend_from_slice(&key.to_be_bytes());
                    out.extend_from_slice(&(rel(o)? as i32).to_be_bytes());
                }
            }
        } else {
            out.extend_from_slice(&code[pc..pc + len]);
        }
    }
    debug_assert_eq!(out.len(), body_end);

    // Catch-all handler.
    let mut new_extab: Vec<(usize, usize, usize, u16)> =
        extab.iter().map(|&(s, en, h, t)| Ok((map(s)?, map(en)?, map(h)?, t))).collect::<Result<_>>()?;
    let mut handler_pc = None;
    if plan.has_exit() {
        let range_start = if m.is_constructor() {
            this_init_end(cf, code, &insns).map(map).transpose()?
        } else {
            Some(map(0)?)
        };
        if let Some(rs) = range_start.filter(|&rs| rs < body_end) {
            let mut e = Emit { code: vec![], pool };
            handler_code(&mut e, plan)?;
            handler_pc = Some(out.len());
            new_extab.push((rs, body_end, out.len(), 0));
            out.extend_from_slice(&e.code);
        }
    }
    if out.len() > 0xffff {
        return err("method too large");
    }

    // Attributes.
    let new_of = |old: usize| start.get(&old).copied().unwrap_or(old);
    let mut new_attrs: Vec<(u16, Vec<u8>)> = vec![];
    let mut had_frames = false;
    for a in &attrs {
        let b = &body[a.body.0..a.body.1];
        match cf.attr_name(a).as_str() {
            "LineNumberTable" => {
                let mut r = Reader::new(b, 0);
                let n = r.u16()?;
                let mut o = n.to_be_bytes().to_vec();
                for _ in 0..n {
                    let s = map(r.u16()? as usize)?;
                    o.extend_from_slice(&(s as u16).to_be_bytes());
                    o.extend_from_slice(&r.u16()?.to_be_bytes());
                }
                new_attrs.push((a.name, o));
            }
            "LocalVariableTable" | "LocalVariableTypeTable" => {
                let mut r = Reader::new(b, 0);
                let n = r.u16()?;
                let mut o = n.to_be_bytes().to_vec();
                for _ in 0..n {
                    let s = r.u16()? as usize;
                    let l = r.u16()? as usize;
                    let (ns, ne) = (map(s)?, map(s + l)?);
                    o.extend_from_slice(&(ns as u16).to_be_bytes());
                    o.extend_from_slice(&((ne - ns) as u16).to_be_bytes());
                    o.extend_from_slice(r.bytes(6)?);
                }
                new_attrs.push((a.name, o));
            }
            "StackMapTable" => {
                had_frames = true;
                let mut frames = read_frames(b)?
                    .into_iter()
                    .map(|(off, f)| Ok((map(off)?, f)))
                    .collect::<Result<Vec<_>>>()?;
                if let Some(h) = handler_pc {
                    let t = pool_throwable(pool)?;
                    frames.push((h, FrameBody::Full(vec![], vec![Vt::Object(t)])));
                }
                new_attrs.push((a.name, write_frames(&frames, &new_of)?));
            }
            // Type annotations carry code offsets we don't remap.
            _ => {}
        }
    }
    if !had_frames
        && cf.major >= 50
        && let Some(h) = handler_pc
    {
        let t = pool_throwable(pool)?;
        let name = pool.get(Key::Utf8("StackMapTable".into()))?;
        new_attrs.push((name, write_frames(&[(h, FrameBody::Full(vec![], vec![Vt::Object(t)]))], &new_of)?));
    }

    let mut o = vec![];
    o.extend_from_slice(&max_stack.saturating_add(8).to_be_bytes());
    o.extend_from_slice(&max_locals.to_be_bytes());
    o.extend_from_slice(&(out.len() as u32).to_be_bytes());
    o.extend_from_slice(&out);
    o.extend_from_slice(&(new_extab.len() as u16).to_be_bytes());
    for (s, e, h, t) in new_extab {
        for v in [s as u16, e as u16, h as u16, t] {
            o.extend_from_slice(&v.to_be_bytes());
        }
    }
    o.extend_from_slice(&(new_attrs.len() as u16).to_be_bytes());
    for (name, b) in new_attrs {
        o.extend_from_slice(&name.to_be_bytes());
        o.extend_from_slice(&(b.len() as u32).to_be_bytes());
        o.extend_from_slice(&b);
    }
    Ok(o)
}

fn pool_throwable(pool: &mut Pool) -> Result<u16> {
    pool.get(Key::Class("java/lang/Throwable".into()))
}

/// Result of [`rewrite`].
pub struct Rewritten {
    pub bytes: Vec<u8>,
    /// Plans that were applied (methods that could not be rewritten are left out).
    pub applied: Vec<MethodPlan>,
    /// Methods skipped, with the reason.
    pub skipped: Vec<(usize, String)>,
}

/// Applies `plans` to the class. Returns `None` if no method could be rewritten.
pub fn rewrite(cf: &ClassFile<'_>, plans: &[MethodPlan]) -> Result<Option<Rewritten>> {
    let mut pool = Pool { count: cf.cp.len(), extra: vec![], added: HashMap::new() };
    let mut new_methods: HashMap<usize, Vec<u8>> = HashMap::new();
    let mut applied = vec![];
    let mut skipped = vec![];
    for plan in plans {
        let Some(m) = cf.methods.get(plan.method) else { continue };
        if !m.has_code() {
            continue;
        }
        let code_attr = m.attrs.iter().position(|a| cf.attr_name(a) == "Code");
        let Some(ci) = code_attr else { continue };
        // Keep the pool consistent if this method fails half-way.
        let saved = (pool.count, pool.extra.len(), pool.added.clone());
        let a = &m.attrs[ci];
        match rewrite_code(cf, m, &cf.bytes[a.body.0..a.body.1], plan, &mut pool) {
            Ok(code) => {
                let mut o = vec![];
                o.extend_from_slice(&m.access.to_be_bytes());
                o.extend_from_slice(&m.name_idx.to_be_bytes());
                o.extend_from_slice(&m.desc_idx.to_be_bytes());
                o.extend_from_slice(&(m.attrs.len() as u16).to_be_bytes());
                for (i, at) in m.attrs.iter().enumerate() {
                    o.extend_from_slice(&at.name.to_be_bytes());
                    if i == ci {
                        o.extend_from_slice(&(code.len() as u32).to_be_bytes());
                        o.extend_from_slice(&code);
                    } else {
                        o.extend_from_slice(&((at.body.1 - at.body.0) as u32).to_be_bytes());
                        o.extend_from_slice(&cf.bytes[at.body.0..at.body.1]);
                    }
                }
                new_methods.insert(plan.method, o);
                applied.push(plan.clone());
            }
            Err(e) => {
                pool.count = saved.0;
                pool.extra.truncate(saved.1);
                pool.added = saved.2;
                skipped.push((plan.method, e.0));
            }
        }
    }
    if new_methods.is_empty() {
        return Ok(if skipped.is_empty() {
            None
        } else {
            Some(Rewritten { bytes: cf.bytes.to_vec(), applied, skipped })
        });
    }
    let b = cf.bytes;
    let mut out = Vec::with_capacity(b.len() + pool.extra.len() + 256 * new_methods.len());
    out.extend_from_slice(&b[..8]);
    out.extend_from_slice(&(pool.count as u16).to_be_bytes());
    out.extend_from_slice(&b[10..cf.cp_end]);
    out.extend_from_slice(&pool.extra);
    out.extend_from_slice(&b[cf.cp_end..cf.methods_start + 2]);
    for (i, m) in cf.methods.iter().enumerate() {
        match new_methods.get(&i) {
            Some(nm) => out.extend_from_slice(nm),
            None => out.extend_from_slice(&b[m.range.0..m.range.1]),
        }
    }
    out.extend_from_slice(&b[cf.methods_end..]);
    Ok(Some(Rewritten { bytes: out, applied, skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors() {
        assert_eq!(param_tags("(I[JLjava/lang/String;D[[Ljava/lang/Object;)V").unwrap(), b"I[LD[".to_vec());
        assert!(param_tags("()V").unwrap().is_empty());
    }

    #[test]
    fn modified_utf8_round_trips() {
        for s in ["abc", "é\u{0}x", "日本", "😀"] {
            assert_eq!(mutf8(&mutf8_encode(s)), s);
        }
    }

    #[test]
    fn switch_padding() {
        assert_eq!(switch_pad(0), 3);
        assert_eq!(switch_pad(3), 0);
        assert_eq!(switch_pad(5), 2);
    }
}
