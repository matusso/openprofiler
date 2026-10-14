//! Chunks, constant pools and metadata-driven value decoding.
//!
//! A JFR file is a sequence of self-contained chunks. Each chunk starts with a fixed 68-byte
//! header, followed by size-prefixed records: the metadata record (type id 0), constant-pool
//! records (type id 1, there can be several per chunk in streaming "flush" mode) and events
//! (every other type id). Events refer to constant-pool entries (threads, stack traces, classes,
//! …) by key, and those keys are scoped to the chunk.

use crate::metadata::{FieldDesc, Metadata, Prim, TimeUnit};
use crate::reader::{Reader, WireStr};
use anyhow::{Context, Result, bail};
use openprofiler_model::{FrameKind, Nanos};
use rustc_hash::FxHashMap;
use std::path::Path;

pub(crate) const HEADER_SIZE: usize = 68;
const MAGIC: &[u8; 4] = b"FLR\0";
const METADATA_TYPE: u64 = 0;
const CONSTANT_POOL_TYPE: u64 = 1;
/// Element nesting bound for inline structs; guards against malformed recursive metadata.
const MAX_DEPTH: u32 = 32;

#[derive(Debug, Clone)]
pub struct ChunkHeader {
    /// Absolute offset of the chunk in the file.
    pub offset: usize,
    pub size: usize,
    pub major: u16,
    pub minor: u16,
    pub constant_pool_offset: i64,
    pub metadata_offset: i64,
    pub start_nanos: Nanos,
    pub duration_nanos: Nanos,
    pub start_ticks: i64,
    pub ticks_per_second: i64,
    pub compressed: bool,
}

impl ChunkHeader {
    pub fn parse(data: &[u8], offset: usize) -> Result<ChunkHeader> {
        let mut r = Reader::new(data, offset, false);
        if r.bytes(4)? != MAGIC {
            bail!("not a JFR chunk at offset {offset} (bad magic)");
        }
        let major = r.be_u16()?;
        let minor = r.be_u16()?;
        if major != 2 {
            bail!("unsupported JFR format version {major}.{minor} (need 2.x, JDK 11+)");
        }
        let size = r.be_i64()?;
        let constant_pool_offset = r.be_i64()?;
        let metadata_offset = r.be_i64()?;
        let start_nanos = r.be_i64()?;
        let duration_nanos = r.be_i64()?;
        let start_ticks = r.be_i64()?;
        let ticks_per_second = r.be_i64()?;
        let flags = *data.get(offset + 67).context("truncated chunk header")?;
        let remaining = data.len() - offset;
        // A chunk still being written may carry a zero or stale size; clamp to the file.
        let size =
            if size <= HEADER_SIZE as i64 || size as usize > remaining { remaining } else { size as usize };
        Ok(ChunkHeader {
            offset,
            size,
            major,
            minor,
            constant_pool_offset,
            metadata_offset,
            start_nanos,
            duration_nanos,
            start_ticks,
            ticks_per_second: ticks_per_second.max(1),
            compressed: flags & 1 != 0,
        })
    }
}

/// Splits a file into chunk headers.
pub fn chunk_headers(data: &[u8]) -> Result<Vec<ChunkHeader>> {
    let mut out = vec![];
    let mut off = 0;
    while off + HEADER_SIZE <= data.len() {
        let h = ChunkHeader::parse(data, off)?;
        off += h.size;
        out.push(h);
    }
    if out.is_empty() {
        bail!("no JFR chunks found ({} bytes)", data.len());
    }
    Ok(out)
}

/// A decoded value before constant-pool resolution.
#[derive(Debug, Clone)]
pub(crate) enum Raw {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Box<str>),
    /// `(type index, key)` into the chunk's constant pools.
    Ref(u32, i64),
    /// `(type index, field values)`.
    Obj(u32, Box<[Raw]>),
    Arr(Box<[Raw]>),
}

static NULL: Raw = Raw::Null;

impl Raw {
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Raw::Int(v) => Some(v),
            Raw::Float(f) => Some(f as i64),
            Raw::Bool(b) => Some(b as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Raw::Int(v) => Some(v as f64),
            Raw::Float(f) => Some(f),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Raw::Bool(b) => Some(b),
            Raw::Int(v) => Some(v != 0),
            _ => None,
        }
    }
}

/// A parsed chunk: metadata and constant pools are loaded, events are decoded on demand.
pub struct Chunk<'a> {
    pub header: ChunkHeader,
    /// The chunk's bytes (offsets in the header are relative to this slice).
    data: &'a [u8],
    pub(crate) md: Metadata,
    pools: Vec<FxHashMap<i64, Raw>>,
    string_type: u32,
}

/// Location of one record inside a chunk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordPos {
    pub type_id: u64,
    pub body: usize,
    pub end: usize,
}

impl<'a> Chunk<'a> {
    pub fn parse(file: &'a [u8], header: ChunkHeader) -> Result<Chunk<'a>> {
        let data = &file[header.offset..header.offset + header.size];
        if header.metadata_offset <= 0 || header.metadata_offset as usize >= data.len() {
            bail!("chunk at offset {} has no metadata (incomplete recording?)", header.offset);
        }
        let mut r = Reader::new(data, header.metadata_offset as usize, header.compressed);
        let md =
            Metadata::parse(&mut r).with_context(|| format!("metadata of chunk at {}", header.offset))?;
        let string_type = md.type_by_name("java.lang.String").context("metadata lacks java.lang.String")?;
        let mut chunk =
            Chunk { header, data, pools: vec![FxHashMap::default(); md.types.len()], md, string_type };
        chunk.load_constant_pools()?;
        Ok(chunk)
    }

    fn reader(&self, pos: usize) -> Reader<'a> {
        Reader::new(self.data, pos, self.header.compressed)
    }

    /// Reads the record header at `pos`.
    pub(crate) fn record_at(&self, pos: usize) -> Result<RecordPos> {
        let mut r = self.reader(pos);
        let size = r.int()?;
        if size <= 0 || pos + size as usize > self.data.len() {
            bail!("invalid record size {size} at chunk offset {pos}");
        }
        let type_id = r.long()? as u64;
        Ok(RecordPos { type_id, body: r.pos, end: pos + size as usize })
    }

    /// All records after the header, in file order.
    pub(crate) fn records(&self) -> impl Iterator<Item = Result<RecordPos>> + '_ {
        let mut pos = HEADER_SIZE;
        let mut failed = false;
        std::iter::from_fn(move || {
            if failed || pos >= self.data.len() {
                return None;
            }
            match self.record_at(pos) {
                Ok(rec) => {
                    pos = rec.end;
                    Some(Ok(rec))
                }
                Err(e) => {
                    failed = true;
                    Some(Err(e))
                }
            }
        })
    }

    fn load_constant_pools(&mut self) -> Result<()> {
        let recs: Vec<RecordPos> = self
            .records()
            .filter(|r| r.as_ref().map_or(true, |r| r.type_id == CONSTANT_POOL_TYPE))
            .collect::<Result<_>>()?;
        for rec in recs {
            let mut r = self.reader(rec.body);
            let _start = r.long()?;
            let _duration = r.long()?;
            let _delta = r.long()?;
            let _checkpoint_type = r.u8()?;
            let pool_count = r.len()?;
            for _ in 0..pool_count {
                let type_id = r.long()? as u64;
                let Some(&t) = self.md.by_id.get(&type_id) else {
                    bail!("constant pool for unknown type {type_id}");
                };
                let count = r.len()?;
                for _ in 0..count {
                    let key = r.long()?;
                    let v = self.read_value(&mut r, t, 0)?;
                    self.pools[t as usize].insert(key, v);
                }
            }
        }
        Ok(())
    }

    fn read_value(&self, r: &mut Reader<'_>, t: u32, depth: u32) -> Result<Raw> {
        let ty = &self.md.types[t as usize];
        Ok(match ty.prim {
            Prim::Bool => Raw::Bool(r.u8()? != 0),
            Prim::Byte => Raw::Int(r.u8()? as i8 as i64),
            Prim::Char => Raw::Int(r.char()? as i64),
            Prim::Short => Raw::Int(r.short()? as i64),
            Prim::Int => Raw::Int(r.int()? as i64),
            Prim::Long => Raw::Int(r.long()?),
            Prim::Float => Raw::Float(r.float()? as f64),
            Prim::Double => Raw::Float(r.double()?),
            Prim::String => match r.string()? {
                WireStr::Pool(k) => Raw::Ref(self.string_type, k),
                WireStr::Null => Raw::Null,
                s => Raw::Str(s.into_owned().unwrap_or_default()),
            },
            Prim::Complex => {
                if depth > MAX_DEPTH {
                    bail!("value nesting too deep for type {}", ty.name);
                }
                let mut fields = Vec::with_capacity(ty.fields.len());
                for f in &ty.fields {
                    fields.push(self.read_field(r, f, depth + 1)?);
                }
                Raw::Obj(t, fields.into_boxed_slice())
            }
        })
    }

    fn read_field(&self, r: &mut Reader<'_>, f: &FieldDesc, depth: u32) -> Result<Raw> {
        if f.array {
            let n = r.len()?;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(self.read_single(r, f, depth)?);
            }
            Ok(Raw::Arr(items.into_boxed_slice()))
        } else {
            self.read_single(r, f, depth)
        }
    }

    #[inline]
    fn read_single(&self, r: &mut Reader<'_>, f: &FieldDesc, depth: u32) -> Result<Raw> {
        if f.constant_pool {
            Ok(Raw::Ref(f.type_idx, r.long()?))
        } else {
            self.read_value(r, f.type_idx, depth)
        }
    }

    /// Decodes the fields of an event record.
    pub(crate) fn decode(&self, rec: &RecordPos) -> Result<(u32, Vec<Raw>)> {
        let Some(&t) = self.md.by_id.get(&rec.type_id) else {
            bail!("event of unknown type {}", rec.type_id);
        };
        let mut r = self.reader(rec.body);
        let ty = &self.md.types[t as usize];
        let mut fields = Vec::with_capacity(ty.fields.len());
        for f in &ty.fields {
            if r.pos >= rec.end {
                // Older writers may omit trailing fields; treat them as absent.
                fields.push(Raw::Null);
                continue;
            }
            fields.push(self.read_field(&mut r, f, 0)?);
        }
        Ok((t, fields))
    }

    /// Follows constant-pool references.
    pub(crate) fn resolve<'s>(&'s self, mut v: &'s Raw) -> &'s Raw {
        for _ in 0..8 {
            match v {
                Raw::Ref(t, k) => v = self.pools.get(*t as usize).and_then(|p| p.get(k)).unwrap_or(&NULL),
                _ => return v,
            }
        }
        v
    }

    /// A field of a (possibly referenced) struct, unresolved.
    pub(crate) fn field_raw<'s>(&'s self, v: &'s Raw, name: &str) -> Option<&'s Raw> {
        match self.resolve(v) {
            Raw::Obj(t, fields) => self.md.types[*t as usize].field_index(name).and_then(|i| fields.get(i)),
            _ => None,
        }
    }

    pub(crate) fn field<'s>(&'s self, v: &'s Raw, name: &str) -> Option<&'s Raw> {
        self.field_raw(v, name).map(|f| self.resolve(f))
    }

    /// Text of a string, a `Symbol` (`{string}`), or nested references to either.
    pub(crate) fn text<'s>(&'s self, v: &'s Raw) -> Option<&'s str> {
        match self.resolve(v) {
            Raw::Str(s) => Some(s),
            obj @ Raw::Obj(..) => self.field_raw(obj, "string").and_then(|f| self.text(f)),
            _ => None,
        }
    }

    /// Text of a string, or of the first string-typed field of a struct (`GCName`, `GCWhen`,
    /// `ThreadState`, `FrameType`, `Symbol`, …).
    pub(crate) fn any_text<'s>(&'s self, v: &'s Raw) -> Option<&'s str> {
        match self.resolve(v) {
            Raw::Str(s) => Some(s),
            Raw::Obj(t, fields) => self.md.types[*t as usize]
                .fields
                .iter()
                .zip(fields.iter())
                .find(|(fd, _)| self.md.types[fd.type_idx as usize].prim == Prim::String)
                .and_then(|(_, fv)| self.text(fv)),
            _ => None,
        }
    }

    pub fn ticks_to_nanos(&self, ticks: i64) -> Nanos {
        let h = &self.header;
        h.start_nanos + ((ticks - h.start_ticks) as i128 * 1_000_000_000 / h.ticks_per_second as i128) as i64
    }

    pub fn span_to_nanos(&self, v: i64, unit: TimeUnit) -> Nanos {
        match unit {
            TimeUnit::Ticks => (v as i128 * 1_000_000_000 / self.header.ticks_per_second as i128) as i64,
            TimeUnit::Nanos => v,
            TimeUnit::Micros => v.saturating_mul(1_000),
            TimeUnit::Millis | TimeUnit::EpochMillis => v.saturating_mul(1_000_000),
            TimeUnit::Seconds => v.saturating_mul(1_000_000_000),
        }
    }

    pub fn timestamp_to_nanos(&self, v: i64, unit: TimeUnit) -> Nanos {
        match unit {
            TimeUnit::Ticks => self.ticks_to_nanos(v),
            TimeUnit::EpochMillis | TimeUnit::Millis => v.saturating_mul(1_000_000),
            other => self.span_to_nanos(v, other),
        }
    }

    /// Converts a numeric field honoring its `@Timespan`/`@Timestamp` unit.
    pub(crate) fn field_nanos(&self, t: u32, fields: &[Raw], name: &str) -> Option<Nanos> {
        let ty = &self.md.types[t as usize];
        let i = ty.field_index(name)?;
        let f = &ty.fields[i];
        let v = self.resolve(fields.get(i)?).as_i64()?;
        Some(match (f.timestamp, f.timespan) {
            (Some(u), _) => self.timestamp_to_nanos(v, u),
            (_, Some(u)) => self.span_to_nanos(v, u),
            _ => v,
        })
    }

    pub fn end_nanos(&self) -> Nanos {
        self.header.start_nanos + self.header.duration_nanos
    }

    /// Event types declared by this chunk's metadata.
    pub fn event_types(&self) -> Vec<EventType> {
        self.md
            .types
            .iter()
            .filter(|t| t.is_event())
            .map(|t| EventType {
                id: t.id,
                name: t.name.clone(),
                label: t.label.clone(),
                fields: t
                    .fields
                    .iter()
                    .map(|f| EventField {
                        name: f.name.clone(),
                        label: f.label.clone(),
                        type_name: self.md.types[f.type_idx as usize].name.clone(),
                        array: f.array,
                    })
                    .collect(),
            })
            .collect()
    }

    /// Iterates all events of the chunk, fully resolved.
    pub fn events(&self) -> impl Iterator<Item = Result<Event>> + '_ {
        self.records().filter_map(move |rec| match rec {
            Ok(rec) if rec.type_id == METADATA_TYPE || rec.type_id == CONSTANT_POOL_TYPE => None,
            Ok(rec) => Some(self.decode(&rec).map(|(t, f)| self.to_event(t, &f))),
            Err(e) => Some(Err(e)),
        })
    }

    pub(crate) fn event_records(&self) -> impl Iterator<Item = Result<RecordPos>> + '_ {
        self.records().filter(|r| r.as_ref().map_or(true, |r| r.type_id > CONSTANT_POOL_TYPE))
    }

    fn to_event(&self, t: u32, raw: &[Raw]) -> Event {
        let ty = &self.md.types[t as usize];
        let mut ev = Event {
            type_name: ty.name.clone(),
            type_id: ty.id,
            start: self.field_nanos(t, raw, "startTime").unwrap_or(self.header.start_nanos),
            duration: self.field_nanos(t, raw, "duration").unwrap_or(0),
            thread: None,
            stack: None,
            fields: vec![],
        };
        for (f, v) in ty.fields.iter().zip(raw) {
            match f.name.as_str() {
                "startTime" | "duration" => {}
                "eventThread" | "sampledThread" if ev.thread.is_none() => ev.thread = self.thread_ref(v),
                "stackTrace" => ev.stack = self.stack_trace(v),
                _ => ev.fields.push((f.name.clone(), self.to_value(v, Some(f), 4))),
            }
        }
        ev
    }

    pub(crate) fn thread_ref(&self, v: &Raw) -> Option<ThreadRef> {
        let r = self.resolve(v);
        if matches!(r, Raw::Null) {
            return None;
        }
        let java_id =
            self.field(r, "javaThreadId").and_then(Raw::as_i64).filter(|&v| v > 0).map(|v| v as u64);
        let os_id = self.field(r, "osThreadId").and_then(Raw::as_i64).filter(|&v| v > 0).map(|v| v as u64);
        let name = self
            .field_raw(r, "javaName")
            .and_then(|f| self.text(f))
            .or_else(|| self.field_raw(r, "osName").and_then(|f| self.text(f)))
            .unwrap_or("")
            .to_owned();
        let group =
            self.field_raw(r, "group").and_then(|g| self.field_raw(g, "name")).and_then(|n| self.text(n));
        Some(ThreadRef {
            name,
            java_id,
            os_id,
            group: group.map(str::to_owned),
            is_virtual: self.field(r, "virtual").and_then(Raw::as_bool).unwrap_or(false),
        })
    }

    pub(crate) fn class_name(&self, class: &Raw) -> Option<String> {
        self.field_raw(class, "name")
            .and_then(|n| self.text(n))
            .map(openprofiler_model::symbols::java_class_name)
    }

    pub(crate) fn frame_kind(&self, v: &Raw) -> FrameKind {
        let desc = self.field_raw(v, "description").and_then(|d| self.text(d)).or_else(|| self.text(v));
        match desc {
            Some("Interpreted") => FrameKind::Interpreted,
            Some("Inlined") => FrameKind::Inlined,
            Some("Native") => FrameKind::Native,
            Some(s) if s.contains("compiled") || s.contains("JIT") => FrameKind::Jit,
            _ => FrameKind::Unknown,
        }
    }

    pub(crate) fn stack_trace(&self, v: &Raw) -> Option<StackTrace> {
        let st = self.resolve(v);
        if matches!(st, Raw::Null) {
            return None;
        }
        let truncated = self.field(st, "truncated").and_then(Raw::as_bool).unwrap_or(false);
        let mut frames = vec![];
        if let Some(Raw::Arr(items)) = self.field(st, "frames") {
            for fr in items.iter() {
                let method = self.field_raw(fr, "method");
                let class = method.and_then(|m| self.field_raw(m, "type")).and_then(|c| self.class_name(c));
                frames.push(StackFrame {
                    class: class.unwrap_or_default(),
                    method: method
                        .and_then(|m| self.field_raw(m, "name"))
                        .and_then(|n| self.text(n))
                        .unwrap_or("")
                        .to_owned(),
                    descriptor: method
                        .and_then(|m| self.field_raw(m, "descriptor"))
                        .and_then(|n| self.text(n))
                        .unwrap_or("")
                        .to_owned(),
                    line: self.field(fr, "lineNumber").and_then(Raw::as_i64).unwrap_or(-1) as i32,
                    kind: self.field_raw(fr, "type").map_or(FrameKind::Unknown, |t| self.frame_kind(t)),
                });
            }
        }
        Some(StackTrace { truncated, frames })
    }

    fn to_value(&self, v: &Raw, f: Option<&FieldDesc>, depth: u32) -> Value {
        match self.resolve(v) {
            Raw::Null => Value::Null,
            Raw::Bool(b) => Value::Bool(*b),
            Raw::Int(i) => match f.map(|f| (f.timestamp, f.timespan)) {
                Some((Some(u), _)) => Value::Timestamp(self.timestamp_to_nanos(*i, u)),
                Some((_, Some(u))) => Value::Duration(self.span_to_nanos(*i, u)),
                _ => Value::Int(*i),
            },
            Raw::Float(x) => Value::Float(*x),
            Raw::Str(s) => Value::String(s.to_string()),
            Raw::Arr(items) => Value::Array(items.iter().map(|i| self.to_value(i, f, depth)).collect()),
            Raw::Obj(t, fields) => {
                let ty = &self.md.types[*t as usize];
                if depth == 0 {
                    return Value::String(format!("<{}>", ty.name));
                }
                // Symbols, classes and similar wrappers read better flattened.
                match ty.name.as_str() {
                    "jdk.types.Symbol" => {
                        return self.text(v).map_or(Value::Null, |s| Value::String(s.to_owned()));
                    }
                    "java.lang.Class" => {
                        return self.class_name(v).map_or(Value::Null, Value::String);
                    }
                    "java.lang.Thread" => {
                        return self.thread_ref(v).map_or(Value::Null, |t| Value::String(t.name));
                    }
                    _ => {}
                }
                // Single-string wrappers (GCName, GCCause, GCWhen, ThreadState, …) become strings.
                if let [only] = ty.fields.as_slice()
                    && self.md.types[only.type_idx as usize].prim == Prim::String
                    && !only.array
                {
                    return self.text(&fields[0]).map_or(Value::Null, |s| Value::String(s.to_owned()));
                }
                Value::Object {
                    type_name: ty.name.clone(),
                    fields: ty
                        .fields
                        .iter()
                        .zip(fields.iter())
                        .map(|(fd, fv)| (fd.name.clone(), self.to_value(fv, Some(fd), depth - 1)))
                        .collect(),
                }
            }
            Raw::Ref(..) => Value::Null,
        }
    }
}

/// A fully resolved event value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// Epoch nanoseconds.
    Timestamp(Nanos),
    /// Nanoseconds.
    Duration(Nanos),
    Object {
        type_name: String,
        fields: Vec<(String, Value)>,
    },
    Array(Vec<Value>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRef {
    pub name: String,
    pub java_id: Option<u64>,
    pub os_id: Option<u64>,
    pub group: Option<String>,
    pub is_virtual: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StackFrame {
    /// Dotted class name.
    pub class: String,
    pub method: String,
    pub descriptor: String,
    pub line: i32,
    pub kind: FrameKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StackTrace {
    pub truncated: bool,
    /// Leaf first.
    pub frames: Vec<StackFrame>,
}

/// One decoded event, for event browsers.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub type_name: String,
    pub type_id: u64,
    pub start: Nanos,
    pub duration: Nanos,
    pub thread: Option<ThreadRef>,
    pub stack: Option<StackTrace>,
    /// Remaining fields in declaration order.
    pub fields: Vec<(String, Value)>,
}

impl Event {
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventType {
    pub id: u64,
    pub name: String,
    pub label: Option<String>,
    pub fields: Vec<EventField>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventField {
    pub name: String,
    pub label: Option<String>,
    pub type_name: String,
    pub array: bool,
}

enum Data {
    Mmap(memmap2::Mmap),
    Owned(Vec<u8>),
}

/// Generic access to a recording's chunks and events.
pub struct JfrReader {
    data: Data,
}

impl JfrReader {
    /// Memory-maps a recording.
    pub fn open(path: &Path) -> Result<JfrReader> {
        let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        // SAFETY: the mapping is read-only; concurrent truncation by another process would be a
        // caller error, as with every mmap-based reader.
        let map = unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mmap {}", path.display()))?;
        Ok(JfrReader { data: Data::Mmap(map) })
    }

    pub fn from_bytes(bytes: Vec<u8>) -> JfrReader {
        JfrReader { data: Data::Owned(bytes) }
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.data {
            Data::Mmap(m) => m,
            Data::Owned(v) => v,
        }
    }

    pub fn chunk_headers(&self) -> Result<Vec<ChunkHeader>> {
        chunk_headers(self.bytes())
    }

    /// Parses chunks lazily, in file order.
    pub fn chunks(&self) -> Result<impl Iterator<Item = Result<Chunk<'_>>>> {
        let headers = self.chunk_headers()?;
        Ok(headers.into_iter().map(move |h| Chunk::parse(self.bytes(), h)))
    }

    /// All events of all chunks, in file order.
    pub fn events(&self) -> Result<impl Iterator<Item = Result<Event>> + '_> {
        let chunks = self.chunks()?;
        Ok(chunks.flat_map(|c| -> Box<dyn Iterator<Item = Result<Event>> + '_> {
            match c {
                Ok(c) => Box::new(c.events().collect::<Vec<_>>().into_iter()),
                Err(e) => Box::new(std::iter::once(Err(e))),
            }
        }))
    }
}
