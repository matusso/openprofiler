//! PHD → HPROF conversion, so the heap engine (which indexes HPROF) can analyse PHD dumps.
//!
//! What a PHD lacks is filled in as follows:
//!
//! - Classes become `LOAD_CLASS` and `CLASS_DUMP` records. A class's references (its fields as a
//!   `java.lang.Class` and its statics; the PHD does not say which is which) become static
//!   reference fields `ref0`, `ref1`, ...
//! - Instance references become reference fields named the same way: each class declares as many
//!   as its instance with the most references needs, minus what its superclasses declare. The
//!   values are the instance's non-null references in dump order, then nulls.
//! - Object arrays get the array class of their element class (created when the dump has no
//!   such class); the elements are the non-null references followed by nulls up to the length.
//! - Primitive arrays keep their length; their data is a sparse run of zeros.
//! - Shallow sizes travel in the (otherwise unused) stack trace serial of every object record, in
//!   4-byte units: PHD 6 array sizes, the class's instance size for instances and
//!   `java.lang.Class`'s for class objects. `0` means unknown.
//! - GC roots: classes become sticky-class roots, and objects that nothing references become
//!   unknown roots, as does the lowest-address object of every cycle still unreachable after
//!   that. OpenJ9 collects garbage before writing a heap dump, so every object stays live.
//!
//! The output starts with a UTF8 record (id 1) holding [`HPROF_MARKER`], by which the engine
//! recognizes converted dumps: sizes come from the serials, and field values, primitive data
//! and the inspections that need them are unavailable.

use crate::{Error, Header, PhdReader, Record, Result};
use openprofiler_hprof::{tag as htag, ty, type_size};
use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Prefix of the UTF8 record with id 1 in HPROF files converted from PHD.
pub const HPROF_MARKER: &str = "openprofiler:phd";

const K_INSTANCE: u8 = 0;
const K_OBJ_ARRAY: u8 = 1;
const K_PRIM_ARRAY: u8 = 2;
const K_CLASS: u8 = 3;

/// Heap dump segments are closed at this size (the HPROF length field is 32 bits).
const SEGMENT_LIMIT: u64 = 1 << 30;

/// What a conversion found.
#[derive(Debug, Clone, Default)]
pub struct Conversion {
    pub header: Header,
    pub id_size: u32,
    /// Object records written, classes included.
    pub objects: u64,
    pub classes: u64,
    /// Array classes (and classes of instances) the dump references but does not contain.
    pub synthetic_classes: u64,
    pub references: u64,
    /// References to addresses that are not objects in the dump.
    pub dangling_references: u64,
    /// Inferred GC roots (classes included).
    pub roots: u64,
}

struct ClassRec {
    address: u64,
    super_class: u64,
    name: String,
    instance_size: u32,
}

/// Objects in dump order, as parallel columns.
#[derive(Default)]
struct Objects {
    addr: Vec<u64>,
    /// Class address (instances), element class address (object arrays), HPROF type
    /// (primitive arrays) or class index (classes).
    aux: Vec<u64>,
    kind: Vec<u8>,
    len: Vec<u32>,
    /// Shallow size in 4-byte units, 0 if unknown.
    size4: Vec<u32>,
    ref_end: Vec<u64>,
    refs: Vec<u64>,
}

impl Objects {
    fn push(&mut self, addr: u64, aux: u64, kind: u8, len: u32, size: Option<u64>, refs: &[u64]) {
        self.addr.push(addr);
        self.aux.push(aux);
        self.kind.push(kind);
        self.len.push(len);
        self.size4.push(size.map(|s| (s / 4).min(u32::MAX as u64) as u32).unwrap_or(0));
        self.refs.extend_from_slice(refs);
        self.ref_end.push(self.refs.len() as u64);
    }

    fn refs_of(&self, i: usize) -> &[u64] {
        let s = if i == 0 { 0 } else { self.ref_end[i - 1] as usize };
        &self.refs[s..self.ref_end[i] as usize]
    }
}

struct Counting<R> {
    inner: R,
    n: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.n.fetch_add(k as u64, Ordering::Relaxed);
        Ok(k)
    }
}

/// Converts the PHD at `src` (optionally gzip-compressed) to an HPROF file at `dst`. `progress`
/// receives the fraction of the input read, then 1.0 when the output is written.
pub fn to_hprof(src: &Path, dst: &Path, timestamp_ms: u64, progress: &dyn Fn(f64)) -> Result<Conversion> {
    let total = std::fs::metadata(src)?.len().max(1);
    let read = Arc::new(AtomicU64::new(0));
    let gz = {
        let mut magic = [0u8; 2];
        File::open(src)?.read(&mut magic)? == 2 && magic == [0x1f, 0x8b]
    };
    let file = Counting { inner: File::open(src)?, n: read.clone() };
    let stream: Box<dyn Read> = if gz {
        Box::new(BufReader::with_capacity(1 << 20, flate2::read::MultiGzDecoder::new(BufReader::new(file))))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, file))
    };
    let mut r = PhdReader::new(stream)?;

    // ---- Read every record ----
    let mut objs = Objects::default();
    let mut classes: Vec<ClassRec> = vec![];
    let mut last_report = 0u64;
    while let Some(rec) = r.next_record()? {
        match rec {
            Record::Object { address, class, refs } => objs.push(address, class, K_INSTANCE, 0, None, refs),
            Record::ObjArray { address, class, refs, length, size } => {
                objs.push(address, class, K_OBJ_ARRAY, length.max(refs.len() as u32), size, refs)
            }
            Record::PrimArray { address, ty: t, length, size } => {
                objs.push(address, t as u64, K_PRIM_ARRAY, length.min(i32::MAX as u64) as u32, size, &[])
            }
            Record::Class { address, super_class, name, instance_size, refs } => {
                objs.push(address, classes.len() as u64, K_CLASS, 0, None, refs);
                classes.push(ClassRec { address, super_class, name: name.to_owned(), instance_size });
            }
        }
        let n = read.load(Ordering::Relaxed);
        if n - last_report > 16 << 20 {
            last_report = n;
            progress(0.8 * n as f64 / total as f64);
        }
    }
    let header = r.header.clone();
    drop(r);
    let id_size = if header.is_64bit() { 8 } else { 4 };
    let mut conv = Conversion { header, id_size, ..Default::default() };

    // ---- Classes the dump references but lacks ----
    let mut class_by_addr: FxHashMap<u64, u32> =
        classes.iter().enumerate().map(|(i, c)| (c.address, i as u32)).collect();
    let mut class_by_name: FxHashMap<String, u32> =
        classes.iter().enumerate().map(|(i, c)| (c.name.clone(), i as u32)).collect();
    let object_class =
        class_by_name.get("java/lang/Object").map(|&i| classes[i as usize].address).unwrap_or(0);
    let mut sorted_addr = objs.addr.clone();
    sorted_addr.sort_unstable();
    let mut fake = sorted_addr.last().copied().unwrap_or(0).saturating_add(0x1000) & !7;
    let synth = |classes: &mut Vec<ClassRec>, objs: &mut Objects, address: u64, name: String| -> u32 {
        let i = classes.len() as u32;
        objs.push(address, i as u64, K_CLASS, 0, None, &[]);
        classes.push(ClassRec { address, super_class: object_class, name, instance_size: 0 });
        i
    };
    let n_dumped = objs.addr.len();
    let mut array_class_of: FxHashMap<u64, u32> = FxHashMap::default();
    for i in 0..n_dumped {
        let aux = objs.aux[i];
        match objs.kind[i] {
            K_INSTANCE if !class_by_addr.contains_key(&aux) && sorted_addr.binary_search(&aux).is_err() => {
                let c = synth(&mut classes, &mut objs, aux, format!("<unknown class 0x{aux:x}>"));
                class_by_addr.insert(aux, c);
                conv.synthetic_classes += 1;
            }
            K_OBJ_ARRAY if !array_class_of.contains_key(&aux) => {
                let name = match class_by_addr.get(&aux) {
                    Some(&e) if classes[e as usize].name.starts_with('[') => {
                        format!("[{}", classes[e as usize].name)
                    }
                    Some(&e) => format!("[L{};", classes[e as usize].name),
                    None => "[Ljava/lang/Object;".to_owned(),
                };
                let c = match class_by_name.get(&name) {
                    Some(&c) => c,
                    None => {
                        let c = synth(&mut classes, &mut objs, fake, name.clone());
                        fake += 8;
                        class_by_addr.insert(classes[c as usize].address, c);
                        class_by_name.insert(name, c);
                        conv.synthetic_classes += 1;
                        c
                    }
                };
                array_class_of.insert(aux, c);
            }
            _ => {}
        }
    }
    drop(sorted_addr);

    // ---- Address order (dumps are mostly sorted already); duplicates keep their first record ----
    let n = objs.addr.len();
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&i| objs.addr[i as usize]);
    order.dedup_by_key(|&mut i| objs.addr[i as usize]);
    let addr: Vec<u64> = order.iter().map(|&i| objs.addr[i as usize]).collect();
    let find = |a: u64| addr.binary_search(&a).ok();

    // ---- Instance field slots per class ----
    let nc = classes.len();
    let mut need = vec![0u32; nc];
    for &o in &order {
        let o = o as usize;
        if objs.kind[o] == K_INSTANCE
            && let Some(&c) = class_by_addr.get(&objs.aux[o])
        {
            need[c as usize] = need[c as usize].max(objs.refs_of(o).len() as u32);
        }
    }
    let super_of: Vec<Option<u32>> = classes
        .iter()
        .map(|c| class_by_addr.get(&c.super_class).copied().filter(|_| c.super_class != 0))
        .collect();
    let mut slots: Vec<Option<u32>> = vec![None; nc];
    fn total_slots(
        c: usize,
        need: &[u32],
        super_of: &[Option<u32>],
        memo: &mut [Option<u32>],
        depth: u32,
    ) -> u32 {
        if let Some(v) = memo[c] {
            return v;
        }
        let inherited = match super_of[c] {
            Some(s) if depth < 256 => total_slots(s as usize, need, super_of, memo, depth + 1),
            _ => 0,
        };
        let v = need[c].max(inherited).min(u16::MAX as u32);
        memo[c] = Some(v);
        v
    }
    let total: Vec<u32> = (0..nc).map(|c| total_slots(c, &need, &super_of, &mut slots, 0)).collect();
    let inherited = |c: usize| super_of[c].map(|s| total[s as usize]).unwrap_or(0);

    // ---- Roots ----
    progress(0.85);
    let mut indeg = vec![0u32; addr.len()];
    for (i, &o) in order.iter().enumerate() {
        for &r in objs.refs_of(o as usize) {
            match find(r) {
                Some(t) if t != i => indeg[t] = indeg[t].saturating_add(1),
                Some(_) => {}
                None => conv.dangling_references += 1,
            }
            conv.references += 1;
        }
    }
    let mut root_kind = vec![0u8; addr.len()];
    let mut seen = vec![false; addr.len()];
    let mut stack: Vec<usize> = vec![];
    let visit = |start: usize, seen: &mut Vec<bool>, stack: &mut Vec<usize>| {
        seen[start] = true;
        stack.push(start);
        while let Some(v) = stack.pop() {
            for &r in objs.refs_of(order[v] as usize) {
                if let Some(t) = find(r)
                    && !seen[t]
                {
                    seen[t] = true;
                    stack.push(t);
                }
            }
        }
    };
    for (i, &o) in order.iter().enumerate() {
        if objs.kind[o as usize] == K_CLASS {
            root_kind[i] = htag::ROOT_STICKY_CLASS;
        } else if indeg[i] == 0 {
            root_kind[i] = htag::ROOT_UNKNOWN;
        }
    }
    drop(indeg);
    for i in 0..addr.len() {
        if root_kind[i] != 0 && !seen[i] {
            visit(i, &mut seen, &mut stack);
        }
    }
    for i in 0..addr.len() {
        if !seen[i] {
            root_kind[i] = htag::ROOT_UNKNOWN;
            visit(i, &mut seen, &mut stack);
        }
    }
    drop(seen);

    // ---- Write ----
    progress(0.9);
    let mut out = Out::create(dst, id_size, timestamp_ms)?;
    let marker = format!(
        "{HPROF_MARKER} version={} flags=0x{:x} jvm={}",
        conv.header.version,
        conv.header.flags,
        conv.header.full_version.as_deref().unwrap_or("unknown")
    );
    out.utf8(1, &marker)?;
    let max_slots = total.iter().copied().max().unwrap_or(0) as usize;
    let max_statics =
        (0..n).filter(|&o| objs.kind[o] == K_CLASS).map(|o| objs.refs_of(o).len()).max().unwrap_or(0);
    let field_names = 2 + nc as u64;
    for k in 0..max_slots.max(max_statics).min(u16::MAX as usize) {
        out.utf8(field_names + k as u64, &format!("ref{k}"))?;
    }
    for (i, c) in classes.iter().enumerate() {
        out.utf8(2 + i as u64, &c.name)?;
        out.load_class(i as u32 + 1, c.address, 2 + i as u64)?;
    }
    let class_object_size = class_by_name
        .get("java/lang/Class")
        .map(|&c| classes[c as usize].instance_size.div_ceil(4))
        .unwrap_or(0);
    let mut b: Vec<u8> = Vec::with_capacity(1 << 16);
    for (i, &k) in root_kind.iter().enumerate() {
        if k != 0 {
            b.clear();
            b.push(k);
            out.push_id(&mut b, addr[i]);
            out.sub(&b)?;
            conv.roots += 1;
        }
    }
    let id = id_size as usize;
    for (i, &o) in order.iter().enumerate() {
        let o = o as usize;
        let a = addr[i];
        let refs = objs.refs_of(o);
        b.clear();
        match objs.kind[o] {
            K_CLASS => {
                let ci = objs.aux[o] as usize;
                let c = &classes[ci];
                b.push(htag::CLASS_DUMP);
                out.push_id(&mut b, a);
                b.extend_from_slice(&class_object_size.to_be_bytes());
                out.push_id(&mut b, c.super_class);
                for _ in 0..5 {
                    out.push_id(&mut b, 0);
                }
                b.extend_from_slice(&c.instance_size.to_be_bytes());
                b.extend_from_slice(&0u16.to_be_bytes());
                let statics = refs.len().min(u16::MAX as usize);
                b.extend_from_slice(&(statics as u16).to_be_bytes());
                for (k, &r) in refs[..statics].iter().enumerate() {
                    out.push_id(&mut b, field_names + k as u64);
                    b.push(ty::OBJECT);
                    out.push_id(&mut b, r);
                }
                let (from, to) = (inherited(ci), total[ci]);
                b.extend_from_slice(&((to - from.min(to)) as u16).to_be_bytes());
                for k in from..to {
                    out.push_id(&mut b, field_names + k as u64);
                    b.push(ty::OBJECT);
                }
                out.sub(&b)?;
                conv.classes += 1;
            }
            K_INSTANCE => {
                let c = class_by_addr.get(&objs.aux[o]).map(|&c| c as usize);
                let slots = c.map(|c| total[c] as usize).unwrap_or(0);
                let size4 = c.map(|c| classes[c].instance_size.next_multiple_of(8) / 4).unwrap_or(0);
                b.push(htag::INSTANCE_DUMP);
                out.push_id(&mut b, a);
                b.extend_from_slice(&size4.to_be_bytes());
                out.push_id(&mut b, objs.aux[o]);
                b.extend_from_slice(&((slots * id) as u32).to_be_bytes());
                for k in 0..slots {
                    out.push_id(&mut b, refs.get(k).copied().unwrap_or(0));
                }
                out.sub(&b)?;
            }
            K_OBJ_ARRAY => {
                let class =
                    array_class_of.get(&objs.aux[o]).map(|&c| classes[c as usize].address).unwrap_or(0);
                // A record must fit the 32-bit HPROF length: longer arrays are truncated.
                let len = (objs.len[o] as usize).min((u32::MAX as usize - 64) / id);
                let refs = &refs[..refs.len().min(len)];
                b.push(htag::OBJ_ARRAY_DUMP);
                out.push_id(&mut b, a);
                b.extend_from_slice(&objs.size4[o].to_be_bytes());
                b.extend_from_slice(&(len as u32).to_be_bytes());
                out.push_id(&mut b, class);
                for &r in refs {
                    out.push_id(&mut b, r);
                }
                out.sub_with_zeros(&b, ((len - refs.len()) * id) as u64)?;
            }
            _ => {
                let t = objs.aux[o] as u8;
                let elem = type_size(t, id_size) as u64;
                let max_len = ((u32::MAX as u64 - 64) / elem) as u32;
                let len = objs.len[o].min(max_len);
                b.push(htag::PRIM_ARRAY_DUMP);
                out.push_id(&mut b, a);
                b.extend_from_slice(&objs.size4[o].to_be_bytes());
                b.extend_from_slice(&len.to_be_bytes());
                b.push(t);
                out.sub_with_zeros(&b, len as u64 * elem)?;
            }
        }
        conv.objects += 1;
    }
    out.finish()?;
    progress(1.0);
    Ok(conv)
}

/// Streaming HPROF writer with large, patched heap dump segments and sparse zero runs.
struct Out {
    w: BufWriter<File>,
    id_size: u32,
    pos: u64,
    /// Start of the open heap dump segment record.
    segment: Option<u64>,
}

impl Out {
    fn create(path: &Path, id_size: u32, timestamp_ms: u64) -> Result<Self> {
        let mut o = Self {
            w: BufWriter::with_capacity(1 << 20, File::create(path)?),
            id_size,
            pos: 0,
            segment: None,
        };
        o.raw(b"JAVA PROFILE 1.0.2\0")?;
        o.raw(&id_size.to_be_bytes())?;
        o.raw(&timestamp_ms.to_be_bytes())?;
        Ok(o)
    }

    fn raw(&mut self, b: &[u8]) -> Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    fn push_id(&self, b: &mut Vec<u8>, v: u64) {
        if self.id_size == 8 {
            b.extend_from_slice(&v.to_be_bytes());
        } else {
            b.extend_from_slice(&(v as u32).to_be_bytes());
        }
    }

    fn top(&mut self, t: u8, body: &[u8]) -> Result<()> {
        self.close_segment()?;
        self.raw(&[t])?;
        self.raw(&0u32.to_be_bytes())?;
        self.raw(&(body.len() as u32).to_be_bytes())?;
        self.raw(body)
    }

    fn utf8(&mut self, id: u64, s: &str) -> Result<()> {
        let mut b = Vec::with_capacity(8 + s.len());
        self.push_id(&mut b, id);
        b.extend_from_slice(s.as_bytes());
        self.top(htag::UTF8, &b)
    }

    fn load_class(&mut self, serial: u32, class: u64, name: u64) -> Result<()> {
        let mut b = serial.to_be_bytes().to_vec();
        self.push_id(&mut b, class);
        b.extend_from_slice(&0u32.to_be_bytes());
        self.push_id(&mut b, name);
        self.top(htag::LOAD_CLASS, &b)
    }

    /// Opens a segment, or a new one if the open one cannot take `len` more bytes.
    fn room(&mut self, len: u64) -> Result<()> {
        if let Some(s) = self.segment
            && self.pos - (s + 9) + len > SEGMENT_LIMIT.max(len)
        {
            self.close_segment()?;
        }
        if self.segment.is_none() {
            self.segment = Some(self.pos);
            self.raw(&[htag::HEAP_DUMP_SEGMENT])?;
            self.raw(&[0; 8])?;
        }
        Ok(())
    }

    fn sub(&mut self, b: &[u8]) -> Result<()> {
        self.room(b.len() as u64)?;
        self.raw(b)
    }

    /// A sub-record of `b` followed by `zeros` zero bytes; long runs become file holes.
    fn sub_with_zeros(&mut self, b: &[u8], zeros: u64) -> Result<()> {
        static ZERO: [u8; 1 << 16] = [0; 1 << 16];
        self.room(b.len() as u64 + zeros)?;
        self.raw(b)?;
        if zeros > ZERO.len() as u64 {
            self.w.seek(SeekFrom::Current(zeros as i64))?;
            self.pos += zeros;
        } else {
            self.raw(&ZERO[..zeros as usize])?;
        }
        Ok(())
    }

    fn close_segment(&mut self) -> Result<()> {
        if let Some(s) = self.segment.take() {
            let len = self.pos - (s + 9);
            let len = u32::try_from(len)
                .map_err(|_| Error::Corrupt { offset: s, what: "segment too large".into() })?;
            self.w.seek(SeekFrom::Start(s + 5))?;
            self.w.write_all(&len.to_be_bytes())?;
            self.w.seek(SeekFrom::Start(self.pos))?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.top(htag::HEAP_DUMP_END, &[])?;
        self.w.flush()?;
        Ok(())
    }
}
