//! Zero-copy parser for HPROF heap dumps.
//!
//! The file is memory-mapped; every parsed structure borrows from the map. Parsing is split into
//! a cheap sequential scan of top-level records ([`Hprof::scan`], which skips heap dump bodies)
//! and per-segment iteration of heap sub-records ([`SubRecordIter`]), which callers run in
//! parallel. Individual object records can be decoded later by file offset
//! ([`Hprof::record_at`]), which lets analysis indexes store offsets instead of copies.
//!
//! Supported: `JAVA PROFILE 1.0.1` and `1.0.2`, identifier sizes 4 and 8, gzip-compressed dumps
//! (decompressed to a caller-provided path first, see [`decompress_gz`]).

use memmap2::Mmap;
use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not an HPROF file (bad header)")]
    BadHeader,
    #[error("unsupported identifier size {0}")]
    IdSize(u32),
    #[error("truncated or corrupt record at offset {0}")]
    Corrupt(u64),
}

pub type Result<T> = std::result::Result<T, Error>;

pub mod tag {
    pub const UTF8: u8 = 0x01;
    pub const LOAD_CLASS: u8 = 0x02;
    pub const UNLOAD_CLASS: u8 = 0x03;
    pub const STACK_FRAME: u8 = 0x04;
    pub const STACK_TRACE: u8 = 0x05;
    pub const HEAP_DUMP: u8 = 0x0C;
    pub const HEAP_DUMP_SEGMENT: u8 = 0x1C;
    pub const HEAP_DUMP_END: u8 = 0x2C;

    pub const ROOT_UNKNOWN: u8 = 0xFF;
    pub const ROOT_JNI_GLOBAL: u8 = 0x01;
    pub const ROOT_JNI_LOCAL: u8 = 0x02;
    pub const ROOT_JAVA_FRAME: u8 = 0x03;
    pub const ROOT_NATIVE_STACK: u8 = 0x04;
    pub const ROOT_STICKY_CLASS: u8 = 0x05;
    pub const ROOT_THREAD_BLOCK: u8 = 0x06;
    pub const ROOT_MONITOR_USED: u8 = 0x07;
    pub const ROOT_THREAD_OBJECT: u8 = 0x08;
    pub const CLASS_DUMP: u8 = 0x20;
    pub const INSTANCE_DUMP: u8 = 0x21;
    pub const OBJ_ARRAY_DUMP: u8 = 0x22;
    pub const PRIM_ARRAY_DUMP: u8 = 0x23;
}

/// HPROF basic types.
pub mod ty {
    pub const OBJECT: u8 = 2;
    pub const BOOLEAN: u8 = 4;
    pub const CHAR: u8 = 5;
    pub const FLOAT: u8 = 6;
    pub const DOUBLE: u8 = 7;
    pub const BYTE: u8 = 8;
    pub const SHORT: u8 = 9;
    pub const INT: u8 = 10;
    pub const LONG: u8 = 11;
}

/// Size in bytes of a value of HPROF basic type `t` (object = id size).
pub fn type_size(t: u8, id_size: u32) -> u32 {
    match t {
        ty::OBJECT => id_size,
        ty::BOOLEAN | ty::BYTE => 1,
        ty::CHAR | ty::SHORT => 2,
        ty::FLOAT | ty::INT => 4,
        ty::DOUBLE | ty::LONG => 8,
        _ => 0,
    }
}

/// Java source name of a primitive type.
pub fn type_name(t: u8) -> &'static str {
    match t {
        ty::OBJECT => "Object",
        ty::BOOLEAN => "boolean",
        ty::CHAR => "char",
        ty::FLOAT => "float",
        ty::DOUBLE => "double",
        ty::BYTE => "byte",
        ty::SHORT => "short",
        ty::INT => "int",
        ty::LONG => "long",
        _ => "?",
    }
}

/// JVM descriptor letter for a primitive array type (`I` for int), used for `[I` class names.
pub fn type_descriptor(t: u8) -> char {
    match t {
        ty::BOOLEAN => 'Z',
        ty::CHAR => 'C',
        ty::FLOAT => 'F',
        ty::DOUBLE => 'D',
        ty::BYTE => 'B',
        ty::SHORT => 'S',
        ty::INT => 'I',
        ty::LONG => 'J',
        _ => 'L',
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: &'static str,
    pub id_size: u32,
    pub timestamp_ms: u64,
    /// Offset of the first record.
    pub data_start: usize,
}

/// Big-endian cursor over a byte slice with HPROF id decoding.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
    pub id_size: u32,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8], pos: usize, id_size: u32) -> Self {
        Self { buf, pos, id_size }
    }

    #[inline]
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    #[inline]
    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    #[inline]
    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    #[inline]
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    #[inline]
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    #[inline]
    pub fn id(&mut self) -> Option<u64> {
        if self.id_size == 8 { self.u64() } else { self.u32().map(u64::from) }
    }

    #[inline]
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.take(n)
    }

    #[inline]
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }

    /// Reads a value of basic type `t` as raw bits (ids for objects).
    #[inline]
    pub fn value(&mut self, t: u8) -> Option<u64> {
        match type_size(t, self.id_size) {
            1 => self.u8().map(u64::from),
            2 => self.u16().map(u64::from),
            4 => self.u32().map(u64::from),
            8 => self.u64(),
            _ => None,
        }
    }
}

/// Reads a big-endian id of `id_size` bytes at `pos` of `buf`.
#[inline]
pub fn read_id(buf: &[u8], pos: usize, id_size: u32) -> u64 {
    if id_size == 8 {
        u64::from_be_bytes(buf[pos..pos + 8].try_into().unwrap())
    } else {
        u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as u64
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LoadClass {
    pub serial: u32,
    pub class_id: u64,
    pub stack_serial: u32,
    pub name_id: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct StackFrame {
    pub frame_id: u64,
    pub method_name: u64,
    pub signature: u64,
    pub source_file: u64,
    pub class_serial: u32,
    pub line: i32,
}

#[derive(Debug, Clone)]
pub struct StackTrace {
    pub serial: u32,
    pub thread_serial: u32,
    pub frames: Vec<u64>,
}

/// Result of the top-level scan.
#[derive(Debug, Default)]
pub struct TopLevel {
    /// UTF8 string id → byte range within the file.
    pub strings: FxHashMap<u64, (usize, usize)>,
    pub classes: Vec<LoadClass>,
    pub frames: FxHashMap<u64, StackFrame>,
    pub traces: FxHashMap<u32, StackTrace>,
    /// Body ranges of `HEAP_DUMP` / `HEAP_DUMP_SEGMENT` records.
    pub segments: Vec<(usize, usize)>,
}

/// A memory-mapped HPROF file.
pub struct Hprof {
    map: Mmap,
    pub header: Header,
}

impl Hprof {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the heap dump is treated as immutable for the lifetime of the map.
        let map = unsafe { Mmap::map(&file)? };
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Sequential);
        let header = parse_header(&map)?;
        Ok(Self { map, header })
    }

    pub fn data(&self) -> &[u8] {
        &self.map
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn id_size(&self) -> u32 {
        self.header.id_size
    }

    /// Resolves a UTF8 record (lossy for modified-UTF8 surrogates).
    pub fn string<'a>(&'a self, top: &TopLevel, id: u64) -> std::borrow::Cow<'a, str> {
        match top.strings.get(&id) {
            Some(&(s, e)) => String::from_utf8_lossy(&self.map[s..e]),
            None => std::borrow::Cow::Borrowed(""),
        }
    }

    /// Scans all top-level records. Heap dump bodies are only located, not parsed.
    pub fn scan(&self) -> Result<TopLevel> {
        let id = self.header.id_size;
        let buf = &self.map[..];
        let mut top = TopLevel::default();
        let mut pos = self.header.data_start;
        while pos + 9 <= buf.len() {
            let t = buf[pos];
            let len = u32::from_be_bytes(buf[pos + 5..pos + 9].try_into().unwrap()) as usize;
            let body = pos + 9;
            let end = body.checked_add(len).ok_or(Error::Corrupt(pos as u64))?;
            // HotSpot can leave a truncated final segment when a dump is interrupted; clamp it.
            let end = if end > buf.len() {
                if t == tag::HEAP_DUMP || t == tag::HEAP_DUMP_SEGMENT {
                    buf.len()
                } else {
                    return Err(Error::Corrupt(pos as u64));
                }
            } else {
                end
            };
            let mut c = Cursor::new(&buf[..end], body, id);
            let corrupt = || Error::Corrupt(pos as u64);
            match t {
                tag::UTF8 => {
                    let sid = c.id().ok_or_else(corrupt)?;
                    top.strings.insert(sid, (c.pos, end));
                }
                tag::LOAD_CLASS => {
                    let serial = c.u32().ok_or_else(corrupt)?;
                    let class_id = c.id().ok_or_else(corrupt)?;
                    let stack_serial = c.u32().ok_or_else(corrupt)?;
                    let name_id = c.id().ok_or_else(corrupt)?;
                    top.classes.push(LoadClass { serial, class_id, stack_serial, name_id });
                }
                tag::STACK_FRAME => {
                    let f = StackFrame {
                        frame_id: c.id().ok_or_else(corrupt)?,
                        method_name: c.id().ok_or_else(corrupt)?,
                        signature: c.id().ok_or_else(corrupt)?,
                        source_file: c.id().ok_or_else(corrupt)?,
                        class_serial: c.u32().ok_or_else(corrupt)?,
                        line: c.u32().ok_or_else(corrupt)? as i32,
                    };
                    top.frames.insert(f.frame_id, f);
                }
                tag::STACK_TRACE => {
                    let serial = c.u32().ok_or_else(corrupt)?;
                    let thread_serial = c.u32().ok_or_else(corrupt)?;
                    let n = c.u32().ok_or_else(corrupt)?;
                    let mut frames = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        frames.push(c.id().ok_or_else(corrupt)?);
                    }
                    top.traces.insert(serial, StackTrace { serial, thread_serial, frames });
                }
                tag::HEAP_DUMP | tag::HEAP_DUMP_SEGMENT => top.segments.push((body, end)),
                _ => {}
            }
            pos = end;
        }
        Ok(top)
    }

    /// Iterates the sub-records of a heap dump segment body.
    pub fn sub_records(&self, segment: (usize, usize)) -> SubRecordIter<'_> {
        SubRecordIter { buf: &self.map[..segment.1], pos: segment.0, id_size: self.header.id_size }
    }

    /// Decodes the single heap sub-record starting at file offset `offset`.
    pub fn record_at(&self, offset: u64) -> Option<SubRecord<'_>> {
        let mut it = SubRecordIter { buf: &self.map[..], pos: offset as usize, id_size: self.header.id_size };
        it.next().and_then(|r| r.ok())
    }
}

fn parse_header(buf: &[u8]) -> Result<Header> {
    let nul = buf.iter().take(64).position(|&b| b == 0).ok_or(Error::BadHeader)?;
    let version = match &buf[..nul] {
        b"JAVA PROFILE 1.0.1" => "1.0.1",
        b"JAVA PROFILE 1.0.2" => "1.0.2",
        b"JAVA PROFILE 1.0" => "1.0",
        _ => return Err(Error::BadHeader),
    };
    let mut c = Cursor::new(buf, nul + 1, 4);
    let id_size = c.u32().ok_or(Error::BadHeader)?;
    if id_size != 4 && id_size != 8 {
        return Err(Error::IdSize(id_size));
    }
    let timestamp_ms = c.u64().ok_or(Error::BadHeader)?;
    Ok(Header { version, id_size, timestamp_ms, data_start: c.pos })
}

/// GC root kinds, in the order of their HPROF tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum RootKind {
    Unknown = 0,
    JniGlobal = 1,
    JniLocal = 2,
    JavaFrame = 3,
    NativeStack = 4,
    StickyClass = 5,
    ThreadBlock = 6,
    MonitorUsed = 7,
    ThreadObject = 8,
}

impl RootKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::JniGlobal,
            2 => Self::JniLocal,
            3 => Self::JavaFrame,
            4 => Self::NativeStack,
            5 => Self::StickyClass,
            6 => Self::ThreadBlock,
            7 => Self::MonitorUsed,
            8 => Self::ThreadObject,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::JniGlobal => "JNI global",
            Self::JniLocal => "JNI local",
            Self::JavaFrame => "Java frame",
            Self::NativeStack => "native stack",
            Self::StickyClass => "sticky class",
            Self::ThreadBlock => "thread block",
            Self::MonitorUsed => "monitor used",
            Self::ThreadObject => "thread object",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Root {
    pub kind: RootKind,
    pub id: u64,
    pub thread_serial: Option<u32>,
    pub frame: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldDecl {
    pub name: u64,
    pub ty: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct StaticField {
    pub name: u64,
    pub ty: u8,
    pub value: u64,
}

#[derive(Debug, Clone)]
pub struct ClassDump {
    pub class_id: u64,
    pub stack_serial: u32,
    pub super_id: u64,
    pub loader_id: u64,
    pub signers_id: u64,
    pub protection_domain_id: u64,
    pub instance_size: u32,
    pub statics: Vec<StaticField>,
    pub fields: Vec<FieldDecl>,
}

#[derive(Debug, Clone)]
pub enum SubRecord<'a> {
    Root(Root),
    Class(ClassDump),
    Instance { id: u64, stack_serial: u32, class_id: u64, data: &'a [u8] },
    ObjArray { id: u64, stack_serial: u32, class_id: u64, len: u32, elems: &'a [u8] },
    PrimArray { id: u64, stack_serial: u32, ty: u8, len: u32, data: &'a [u8] },
}

impl SubRecord<'_> {
    /// The object id of object records (class, instance, arrays).
    pub fn object_id(&self) -> Option<u64> {
        match self {
            SubRecord::Root(_) => None,
            SubRecord::Class(c) => Some(c.class_id),
            SubRecord::Instance { id, .. }
            | SubRecord::ObjArray { id, .. }
            | SubRecord::PrimArray { id, .. } => Some(*id),
        }
    }
}

/// Iterator over heap dump sub-records. Yields `(file offset, record)`.
pub struct SubRecordIter<'a> {
    buf: &'a [u8],
    pos: usize,
    id_size: u32,
}

impl<'a> SubRecordIter<'a> {
    /// File offset of the next record.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Advances over the next record, returning `(offset, tag, object id)` for object records
    /// and `(offset, tag, 0)` for roots without materializing class dumps. Faster than `next`.
    pub fn next_header(&mut self) -> Option<std::result::Result<(usize, u8, u64), Error>> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let start = self.pos;
        let r = self.skip_one();
        Some(r.ok_or(Error::Corrupt(start as u64)))
    }

    fn skip_one(&mut self) -> Option<(usize, u8, u64)> {
        let start = self.pos;
        let ids = self.id_size as usize;
        let mut c = Cursor::new(self.buf, self.pos, self.id_size);
        let t = c.u8()?;
        let mut oid = 0;
        match t {
            tag::ROOT_UNKNOWN | tag::ROOT_STICKY_CLASS | tag::ROOT_MONITOR_USED => c.skip(ids)?,
            tag::ROOT_JNI_GLOBAL => c.skip(ids * 2)?,
            tag::ROOT_JNI_LOCAL | tag::ROOT_JAVA_FRAME | tag::ROOT_THREAD_OBJECT => c.skip(ids + 8)?,
            tag::ROOT_NATIVE_STACK | tag::ROOT_THREAD_BLOCK => c.skip(ids + 4)?,
            tag::CLASS_DUMP => {
                oid = c.id()?;
                c.skip(4 + ids * 6 + 4)?;
                let ncp = c.u16()?;
                for _ in 0..ncp {
                    c.skip(2)?;
                    let t = c.u8()?;
                    c.skip(type_size(t, self.id_size) as usize)?;
                }
                let nst = c.u16()?;
                for _ in 0..nst {
                    c.skip(ids)?;
                    let t = c.u8()?;
                    c.skip(type_size(t, self.id_size) as usize)?;
                }
                let nf = c.u16()?;
                c.skip(nf as usize * (ids + 1))?;
            }
            tag::INSTANCE_DUMP => {
                oid = c.id()?;
                c.skip(4 + ids)?;
                let n = c.u32()?;
                c.skip(n as usize)?;
            }
            tag::OBJ_ARRAY_DUMP => {
                oid = c.id()?;
                c.skip(4)?;
                let n = c.u32()?;
                c.skip(ids + n as usize * ids)?;
            }
            tag::PRIM_ARRAY_DUMP => {
                oid = c.id()?;
                c.skip(4)?;
                let n = c.u32()?;
                let t = c.u8()?;
                c.skip(n as usize * type_size(t, self.id_size) as usize)?;
            }
            _ => return None,
        }
        self.pos = c.pos;
        Some((start, t, oid))
    }

    fn parse_one(&mut self) -> Option<SubRecord<'a>> {
        let ids = self.id_size as usize;
        let mut c = Cursor::new(self.buf, self.pos, self.id_size);
        let t = c.u8()?;
        let rec = match t {
            tag::ROOT_UNKNOWN
            | tag::ROOT_STICKY_CLASS
            | tag::ROOT_MONITOR_USED
            | tag::ROOT_JNI_GLOBAL
            | tag::ROOT_JNI_LOCAL
            | tag::ROOT_JAVA_FRAME
            | tag::ROOT_NATIVE_STACK
            | tag::ROOT_THREAD_BLOCK
            | tag::ROOT_THREAD_OBJECT => {
                let id = c.id()?;
                let (mut thread_serial, mut frame) = (None, None);
                match t {
                    tag::ROOT_JNI_GLOBAL => c.skip(ids)?,
                    tag::ROOT_JNI_LOCAL | tag::ROOT_JAVA_FRAME => {
                        thread_serial = Some(c.u32()?);
                        frame = Some(c.u32()?);
                    }
                    tag::ROOT_THREAD_OBJECT => {
                        thread_serial = Some(c.u32()?);
                        c.skip(4)?;
                    }
                    tag::ROOT_NATIVE_STACK | tag::ROOT_THREAD_BLOCK => thread_serial = Some(c.u32()?),
                    _ => {}
                }
                let kind = if t == tag::ROOT_UNKNOWN { RootKind::Unknown } else { RootKind::from_u8(t) };
                SubRecord::Root(Root { kind, id, thread_serial, frame })
            }
            tag::CLASS_DUMP => {
                let class_id = c.id()?;
                let stack_serial = c.u32()?;
                let super_id = c.id()?;
                let loader_id = c.id()?;
                let signers_id = c.id()?;
                let protection_domain_id = c.id()?;
                c.skip(ids * 2)?;
                let instance_size = c.u32()?;
                let ncp = c.u16()?;
                for _ in 0..ncp {
                    c.skip(2)?;
                    let t = c.u8()?;
                    c.skip(type_size(t, self.id_size) as usize)?;
                }
                let nst = c.u16()?;
                let mut statics = Vec::with_capacity(nst as usize);
                for _ in 0..nst {
                    let name = c.id()?;
                    let ty = c.u8()?;
                    let value = c.value(ty)?;
                    statics.push(StaticField { name, ty, value });
                }
                let nf = c.u16()?;
                let mut fields = Vec::with_capacity(nf as usize);
                for _ in 0..nf {
                    let name = c.id()?;
                    let ty = c.u8()?;
                    fields.push(FieldDecl { name, ty });
                }
                SubRecord::Class(ClassDump {
                    class_id,
                    stack_serial,
                    super_id,
                    loader_id,
                    signers_id,
                    protection_domain_id,
                    instance_size,
                    statics,
                    fields,
                })
            }
            tag::INSTANCE_DUMP => {
                let id = c.id()?;
                let stack_serial = c.u32()?;
                let class_id = c.id()?;
                let n = c.u32()?;
                SubRecord::Instance { id, stack_serial, class_id, data: c.bytes(n as usize)? }
            }
            tag::OBJ_ARRAY_DUMP => {
                let id = c.id()?;
                let stack_serial = c.u32()?;
                let len = c.u32()?;
                let class_id = c.id()?;
                SubRecord::ObjArray { id, stack_serial, class_id, len, elems: c.bytes(len as usize * ids)? }
            }
            tag::PRIM_ARRAY_DUMP => {
                let id = c.id()?;
                let stack_serial = c.u32()?;
                let len = c.u32()?;
                let ty = c.u8()?;
                let data = c.bytes(len as usize * type_size(ty, self.id_size) as usize)?;
                SubRecord::PrimArray { id, stack_serial, ty, len, data }
            }
            _ => return None,
        };
        self.pos = c.pos;
        Some(rec)
    }
}

impl<'a> Iterator for SubRecordIter<'a> {
    type Item = std::result::Result<SubRecord<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let start = self.pos;
        match self.parse_one() {
            Some(r) => Some(Ok(r)),
            None => {
                // Stop iterating after a corrupt record.
                self.pos = self.buf.len();
                Some(Err(Error::Corrupt(start as u64)))
            }
        }
    }
}

/// Returns true if the file starts with the gzip magic.
pub fn is_gzip(path: &Path) -> Result<bool> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 2];
    Ok(f.read(&mut magic)? == 2 && magic == [0x1f, 0x8b])
}

/// Decompresses a gzip HPROF to `out`.
pub fn decompress_gz(src: &Path, out: &Path) -> Result<()> {
    let f = File::open(src)?;
    let mut dec = flate2::read::MultiGzDecoder::new(std::io::BufReader::with_capacity(1 << 20, f));
    let tmp = out.with_extension("partial");
    let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
    std::io::copy(&mut dec, &mut w)?;
    w.flush()?;
    drop(w);
    std::fs::rename(tmp, out)?;
    Ok(())
}

/// A minimal HPROF writer, used by tests and the synthetic dump generator.
pub mod writer {
    use super::{tag, type_size};
    use std::io::{self, Write};

    pub struct HprofWriter<W: Write> {
        w: W,
        id_size: u32,
        seg: Vec<u8>,
        seg_limit: usize,
    }

    impl<W: Write> HprofWriter<W> {
        pub fn new(mut w: W, id_size: u32, timestamp_ms: u64) -> io::Result<Self> {
            w.write_all(b"JAVA PROFILE 1.0.2\0")?;
            w.write_all(&id_size.to_be_bytes())?;
            w.write_all(&timestamp_ms.to_be_bytes())?;
            Ok(Self { w, id_size, seg: Vec::new(), seg_limit: 64 << 20 })
        }

        pub fn set_segment_limit(&mut self, bytes: usize) {
            self.seg_limit = bytes;
        }

        fn id_bytes(&self, id: u64, out: &mut Vec<u8>) {
            if self.id_size == 8 {
                out.extend_from_slice(&id.to_be_bytes());
            } else {
                out.extend_from_slice(&(id as u32).to_be_bytes());
            }
        }

        fn record(&mut self, t: u8, body: &[u8]) -> io::Result<()> {
            self.w.write_all(&[t])?;
            self.w.write_all(&0u32.to_be_bytes())?;
            self.w.write_all(&(body.len() as u32).to_be_bytes())?;
            self.w.write_all(body)
        }

        pub fn utf8(&mut self, id: u64, s: &str) -> io::Result<()> {
            let mut b = Vec::new();
            self.id_bytes(id, &mut b);
            b.extend_from_slice(s.as_bytes());
            self.record(tag::UTF8, &b)
        }

        pub fn load_class(&mut self, serial: u32, class_id: u64, name_id: u64) -> io::Result<()> {
            let mut b = Vec::new();
            b.extend_from_slice(&serial.to_be_bytes());
            self.id_bytes(class_id, &mut b);
            b.extend_from_slice(&0u32.to_be_bytes());
            self.id_bytes(name_id, &mut b);
            self.record(tag::LOAD_CLASS, &b)
        }

        fn maybe_flush(&mut self) -> io::Result<()> {
            if self.seg.len() >= self.seg_limit {
                self.flush_segment()?;
            }
            Ok(())
        }

        pub fn flush_segment(&mut self) -> io::Result<()> {
            if !self.seg.is_empty() {
                let seg = std::mem::take(&mut self.seg);
                self.record(tag::HEAP_DUMP_SEGMENT, &seg)?;
                self.seg = seg;
                self.seg.clear();
            }
            Ok(())
        }

        pub fn root(&mut self, kind: u8, id: u64) -> io::Result<()> {
            let mut b = vec![kind];
            self.id_bytes(id, &mut b);
            match kind {
                tag::ROOT_JNI_GLOBAL => self.id_bytes(0, &mut b),
                tag::ROOT_JNI_LOCAL | tag::ROOT_JAVA_FRAME | tag::ROOT_THREAD_OBJECT => {
                    b.extend_from_slice(&[0; 8]);
                }
                tag::ROOT_NATIVE_STACK | tag::ROOT_THREAD_BLOCK => b.extend_from_slice(&[0; 4]),
                _ => {}
            }
            self.seg.extend_from_slice(&b);
            self.maybe_flush()
        }

        /// `statics`: `(name id, type, raw value)`, `fields`: `(name id, type)`.
        #[allow(clippy::too_many_arguments)]
        pub fn class_dump(
            &mut self,
            class_id: u64,
            super_id: u64,
            loader_id: u64,
            instance_size: u32,
            statics: &[(u64, u8, u64)],
            fields: &[(u64, u8)],
        ) -> io::Result<()> {
            let mut b = vec![tag::CLASS_DUMP];
            self.id_bytes(class_id, &mut b);
            b.extend_from_slice(&0u32.to_be_bytes());
            for id in [super_id, loader_id, 0, 0, 0, 0] {
                self.id_bytes(id, &mut b);
            }
            b.extend_from_slice(&instance_size.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&(statics.len() as u16).to_be_bytes());
            for &(name, ty, value) in statics {
                self.id_bytes(name, &mut b);
                b.push(ty);
                let n = type_size(ty, self.id_size) as usize;
                b.extend_from_slice(&value.to_be_bytes()[8 - n..]);
            }
            b.extend_from_slice(&(fields.len() as u16).to_be_bytes());
            for &(name, ty) in fields {
                self.id_bytes(name, &mut b);
                b.push(ty);
            }
            self.seg.extend_from_slice(&b);
            self.maybe_flush()
        }

        pub fn instance(&mut self, id: u64, class_id: u64, data: &[u8]) -> io::Result<()> {
            let mut b = Vec::with_capacity(32 + data.len());
            b.push(tag::INSTANCE_DUMP);
            self.id_bytes(id, &mut b);
            b.extend_from_slice(&0u32.to_be_bytes());
            self.id_bytes(class_id, &mut b);
            b.extend_from_slice(&(data.len() as u32).to_be_bytes());
            b.extend_from_slice(data);
            self.seg.extend_from_slice(&b);
            self.maybe_flush()
        }

        pub fn obj_array(&mut self, id: u64, class_id: u64, elems: &[u64]) -> io::Result<()> {
            let mut b = Vec::with_capacity(32 + elems.len() * 8);
            b.push(tag::OBJ_ARRAY_DUMP);
            self.id_bytes(id, &mut b);
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&(elems.len() as u32).to_be_bytes());
            self.id_bytes(class_id, &mut b);
            for &e in elems {
                self.id_bytes(e, &mut b);
            }
            self.seg.extend_from_slice(&b);
            self.maybe_flush()
        }

        /// `data` must already be big-endian encoded elements of type `ty`.
        pub fn prim_array(&mut self, id: u64, ty: u8, len: u32, data: &[u8]) -> io::Result<()> {
            let mut b = Vec::with_capacity(32 + data.len());
            b.push(tag::PRIM_ARRAY_DUMP);
            self.id_bytes(id, &mut b);
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&len.to_be_bytes());
            b.push(ty);
            b.extend_from_slice(data);
            self.seg.extend_from_slice(&b);
            self.maybe_flush()
        }

        pub fn finish(mut self) -> io::Result<W> {
            self.flush_segment()?;
            self.record(tag::HEAP_DUMP_END, &[])?;
            self.w.flush()?;
            Ok(self.w)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::writer::HprofWriter;
    use super::*;

    #[test]
    fn roundtrip_small() {
        for id_size in [4u32, 8] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("t.hprof");
            let f = std::fs::File::create(&path).unwrap();
            let mut w = HprofWriter::new(BufWriter::new(f), id_size, 1234).unwrap();
            w.utf8(1, "java/lang/Object").unwrap();
            w.utf8(2, "value").unwrap();
            w.load_class(1, 100, 1).unwrap();
            w.class_dump(100, 0, 0, 8, &[], &[(2, ty::OBJECT)]).unwrap();
            w.root(tag::ROOT_STICKY_CLASS, 100).unwrap();
            let mut data = vec![];
            if id_size == 8 {
                data.extend_from_slice(&300u64.to_be_bytes());
            } else {
                data.extend_from_slice(&300u32.to_be_bytes());
            }
            w.instance(200, 100, &data).unwrap();
            w.prim_array(300, ty::INT, 2, &[0, 0, 0, 1, 0, 0, 0, 2]).unwrap();
            w.finish().unwrap();

            let h = Hprof::open(&path).unwrap();
            assert_eq!(h.header.id_size, id_size);
            assert_eq!(h.header.timestamp_ms, 1234);
            let top = h.scan().unwrap();
            assert_eq!(top.classes.len(), 1);
            assert_eq!(h.string(&top, 1), "java/lang/Object");
            assert_eq!(top.segments.len(), 1);
            let recs: Vec<_> = h.sub_records(top.segments[0]).map(|r| r.unwrap()).collect();
            assert_eq!(recs.len(), 4);
            match &recs[2] {
                SubRecord::Instance { id, class_id, data, .. } => {
                    assert_eq!((*id, *class_id), (200, 100));
                    assert_eq!(read_id(data, 0, id_size), 300);
                }
                r => panic!("unexpected {r:?}"),
            }
            let mut it = h.sub_records(top.segments[0]);
            let mut offsets = vec![];
            while let Some(r) = it.next_header() {
                offsets.push(r.unwrap());
            }
            assert_eq!(offsets.len(), 4);
            let (off, t, oid) = offsets[3];
            assert_eq!((t, oid), (tag::PRIM_ARRAY_DUMP, 300));
            match h.record_at(off as u64).unwrap() {
                SubRecord::PrimArray { len, ty: t, .. } => assert_eq!((len, t), (2, ty::INT)),
                r => panic!("unexpected {r:?}"),
            }
        }
    }
}
