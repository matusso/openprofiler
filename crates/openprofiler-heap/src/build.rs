//! Index construction: HPROF → object index, CSR graphs, dominator tree, retained sizes.
//!
//! Every artifact is written to the cache directory as a flat little-endian array and memory
//! mapped on open; `meta.json` is written last and marks a complete index.

use crate::classes::{ClassInfo, ClassKind, FieldInfo, Sizes, StaticInfo, compute_layouts, kind_from_name};
use crate::dom::{Graph, NONE, semi_nca};
use crate::store::{Appender, Arr, Big, MutArr, SyncPtr, sort_rows_by, write_slice};
use crate::{OpenOptions, Phase};
use anyhow::{Context, Result};
use openprofiler_hprof::{Hprof, RootKind, SubRecord, read_id, tag, ty, type_size};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub(crate) const ENGINE_VERSION: u32 = 2;

/// File offset marker of the boot class loader pseudo-object (object 0, address 0).
pub(crate) const BOOT_LOADER_OFFSET: u64 = u64::MAX;

/// The last out-edge of this object is a `Reference.referent` (weak) edge.
pub(crate) const F_REFERENT_LAST: u8 = 1;
pub(crate) const F_ROOT: u8 = 2;
pub(crate) const F_REACHABLE: u8 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub engine_version: u32,
    pub key: String,
    pub hprof_path: PathBuf,
    pub id_size: u32,
    pub timestamp_ms: u64,
    pub objects: u64,
    pub edges: u64,
    pub classes: u32,
    pub roots: u64,
    pub total_shallow: u64,
    pub reachable_objects: u64,
    pub reachable_shallow: u64,
    pub sizes: Sizes,
    pub exclude_weak: bool,
    pub java_lang_class: u32,
    pub unknown_class: u32,
    pub addr_min: u64,
    pub addr_shift: u32,
    /// Converted from an IBM Portable Heap Dump: sizes come from the records, and there are no
    /// field values or primitive array data.
    #[serde(default)]
    pub phd: bool,
}

/// Address → dense object id lookup: a bucket table over the sorted address array.
pub(crate) struct AddrIndex<'a> {
    pub addr: &'a [u64],
    pub table: &'a [u32],
    pub min: u64,
    pub shift: u32,
}

impl AddrIndex<'_> {
    #[inline]
    pub fn find(&self, a: u64) -> Option<u32> {
        if a < self.min || self.addr.is_empty() {
            return None;
        }
        let b = ((a - self.min) >> self.shift) as usize;
        if b + 1 >= self.table.len() {
            return None;
        }
        let (s, e) = (self.table[b] as usize, self.table[b + 1] as usize);
        self.addr[s..e].binary_search(&a).ok().map(|i| (s + i) as u32)
    }
}

pub(crate) fn build_table(addr: &[u64]) -> (Vec<u32>, u64, u32) {
    if addr.is_empty() {
        return (vec![0, 0], 0, 0);
    }
    let min = addr[0];
    let max = *addr.last().unwrap();
    let buckets = (addr.len() / 4).clamp(1, 1 << 24) as u64;
    let mut shift = 0u32;
    while ((max - min) >> shift) >= buckets {
        shift += 1;
    }
    let nb = (((max - min) >> shift) + 1) as usize;
    let mut table = vec![0u32; nb + 1];
    let mut i = 0usize;
    for (b, slot) in table.iter_mut().enumerate() {
        while i < addr.len() && (((addr[i] - min) >> shift) as usize) < b {
            i += 1;
        }
        *slot = i as u32;
    }
    table[nb] = addr.len() as u32;
    (table, min, shift)
}

struct SegOut {
    objects: usize,
    classes: Vec<u64>,
    /// `(object address, root kind, thread serial or u32::MAX)`
    roots: Vec<(u64, u8, u32)>,
    /// Compressed-reference detection over object arrays in file order (MAT's heuristic): an
    /// array starting inside the previous array's uncompressed extent proves 4-byte refs.
    first_array: Option<u64>,
    last_array: Option<(u64, u64)>,
    compressed: bool,
}

pub(crate) struct HeapGraph<'a> {
    pub n: usize,
    pub out_off: &'a [u64],
    pub out: &'a [u32],
    pub in_off: &'a [u64],
    pub inc: &'a [u32],
    pub flags: &'a [u8],
    pub roots: &'a [u32],
    pub exclude_weak: bool,
}

impl HeapGraph<'_> {
    /// Non-weak successors of an object.
    #[inline]
    pub fn strong_out(&self, v: u32) -> &[u32] {
        let s = &self.out[self.out_off[v as usize] as usize..self.out_off[v as usize + 1] as usize];
        if self.exclude_weak && self.flags[v as usize] & F_REFERENT_LAST != 0 && !s.is_empty() {
            &s[..s.len() - 1]
        } else {
            s
        }
    }

    /// Distinct predecessors of `v`, excluding weak referent edges when configured.
    pub fn for_each_pred(&self, v: u32, f: &mut dyn FnMut(u32)) {
        let s = &self.inc[self.in_off[v as usize] as usize..self.in_off[v as usize + 1] as usize];
        let mut i = 0;
        while i < s.len() {
            let u = s[i];
            let mut j = i + 1;
            while j < s.len() && s[j] == u {
                j += 1;
            }
            let weak_only =
                j - i == 1 && self.exclude_weak && self.flags[u as usize] & F_REFERENT_LAST != 0 && {
                    let e = self.out_off[u as usize + 1] as usize;
                    e > 0 && self.out[e - 1] == v
                };
            if !weak_only {
                f(u);
            }
            i = j;
        }
    }
}

impl Graph for HeapGraph<'_> {
    fn node_count(&self) -> usize {
        self.n + 1
    }

    fn succ(&self, v: u32) -> &[u32] {
        if v as usize == self.n { self.roots } else { self.strong_out(v) }
    }

    fn pred(&self, v: u32, f: &mut dyn FnMut(u32)) {
        if v as usize == self.n {
            return;
        }
        if self.flags[v as usize] & F_ROOT != 0 {
            f(self.n as u32);
        }
        self.for_each_pred(v, f);
    }
}

/// Object alignment as the GCD of address differences (at least 8, at most 256), as MAT does.
pub(crate) fn object_alignment(addr: &[u64]) -> u32 {
    let mut align = 0u64;
    let mut prev = 0u64;
    for &a in addr {
        if a == 0 {
            continue;
        }
        let diff = a - prev;
        prev = a;
        if a == diff {
            continue;
        }
        align = if align == 0 { diff } else { gcd(align, diff) };
        if align <= 8 {
            break;
        }
    }
    align.clamp(8, 256) as u32
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

pub(crate) fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

pub(crate) fn build(
    src: &Path,
    dir: &Path,
    key: String,
    opts: &OpenOptions,
    progress: &(dyn Fn(Phase, f64) + Sync),
) -> Result<Meta> {
    let _ = std::fs::remove_file(dir.join("meta.json"));
    // Derived results (inspections) belong to the previous index generation.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("inspection-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let h = Hprof::open(src).with_context(|| format!("opening {}", src.display()))?;
    let id_size = h.id_size();
    progress(Phase::Scanning, 0.0);
    let top = h.scan()?;
    let phd = h.string(&top, 1).starts_with(openprofiler_phd::HPROF_MARKER);
    // Converted PHDs carry each object's size (in 4-byte units) in its stack trace serial.
    let sized =
        |serial: u32, computed: u32| if phd && serial != 0 { serial.saturating_mul(4) } else { computed };

    // ---- Pass A: locate objects, class dumps and roots (parallel per segment) ----
    // A1 counts objects and collects the (few) class dumps and roots; A2 writes `(address,
    // offset)` pairs straight into one pre-sized array. Header skipping is cheap, so scanning
    // twice costs far less than growable per-segment buffers.
    let total_bytes: usize = top.segments.iter().map(|s| s.1 - s.0).sum::<usize>().max(1);
    let done = AtomicU64::new(0);
    let segs: Vec<SegOut> = top
        .segments
        .par_iter()
        .map(|&seg| -> Result<SegOut> {
            let mut out = SegOut {
                objects: 0,
                classes: vec![],
                roots: vec![],
                first_array: None,
                last_array: None,
                compressed: false,
            };
            let data = h.data();
            let mut it = h.sub_records(seg);
            let mut last = seg.0;
            while let Some(r) = it.next_header() {
                let (off, t, oid) = r?;
                match t {
                    tag::INSTANCE_DUMP | tag::PRIM_ARRAY_DUMP => out.objects += 1,
                    tag::OBJ_ARRAY_DUMP => {
                        out.objects += 1;
                        let lp = off + 1 + id_size as usize + 4;
                        let len = u32::from_be_bytes(data[lp..lp + 4].try_into().unwrap()) as u64;
                        if let Some((start, end)) = out.last_array
                            && oid > start
                            && oid < end
                        {
                            out.compressed = true;
                        }
                        out.first_array.get_or_insert(oid);
                        out.last_array = Some((oid, oid + 16 + len * 8));
                    }
                    tag::CLASS_DUMP => {
                        out.objects += 1;
                        out.classes.push(off as u64);
                    }
                    _ => {
                        if let Some(SubRecord::Root(root)) = h.record_at(off as u64) {
                            out.roots.push((
                                root.id,
                                root.kind as u8,
                                root.thread_serial.unwrap_or(u32::MAX),
                            ));
                        }
                    }
                }
                if off - last > 64 << 20 {
                    let d = done.fetch_add((off - last) as u64, Ordering::Relaxed) + (off - last) as u64;
                    progress(Phase::Scanning, d as f64 / total_bytes as f64);
                    last = off;
                }
            }
            done.fetch_add((seg.1 - last) as u64, Ordering::Relaxed);
            Ok(out)
        })
        .collect::<Result<_>>()?;

    progress(Phase::Indexing, 0.0);
    // Slot 0 is a pseudo-object at address 0 standing for the bootstrap class loader, which
    // HotSpot dumps reference (as loader 0) but do not contain. MAT models it the same way.
    let mut starts = Vec::with_capacity(segs.len());
    let mut n_objs = 1usize;
    for s in &segs {
        starts.push(n_objs);
        n_objs += s.objects;
    }
    let mut compressed_detected = false;
    let mut prev_array: Option<(u64, u64)> = None;
    for s in &segs {
        if let (Some(first), Some((start, end))) = (s.first_array, prev_array)
            && first > start
            && first < end
        {
            compressed_detected = true;
        }
        compressed_detected |= s.compressed;
        if s.last_array.is_some() {
            prev_array = s.last_array;
        }
    }
    let compressed_detected = compressed_detected && id_size == 8;
    let mut pair_buf = Big::<[u64; 2]>::zeroed(n_objs);
    pair_buf[0] = [0, BOOT_LOADER_OFFSET];
    {
        let ptr = SyncPtr(pair_buf.as_mut_ptr());
        top.segments.par_iter().zip(starts.par_iter()).for_each(|(&seg, &start)| {
            let mut i = start;
            let mut it = h.sub_records(seg);
            while let Some(Ok((off, t, oid))) = it.next_header() {
                if matches!(
                    t,
                    tag::INSTANCE_DUMP | tag::OBJ_ARRAY_DUMP | tag::PRIM_ARRAY_DUMP | tag::CLASS_DUMP
                ) {
                    // SAFETY: each segment owns the disjoint range [start, start + objects).
                    unsafe { ptr.write(i, [oid, off as u64]) };
                    i += 1;
                }
            }
        });
    }
    let mut class_offsets = vec![];
    let mut raw_roots = vec![];
    for s in segs {
        class_offsets.extend(s.classes);
        raw_roots.extend(s.roots);
    }
    let pairs_sorted = pair_buf.windows(2).all(|w| w[0][0] < w[1][0]);
    if !pairs_sorted {
        pair_buf.par_sort_unstable_by_key(|p| p[0]);
    }
    // Duplicate addresses (corrupt dumps) keep their first record.
    let n = if pairs_sorted {
        n_objs
    } else {
        let mut w = 0usize;
        for r in 0..n_objs {
            if w == 0 || pair_buf[w - 1][0] != pair_buf[r][0] {
                pair_buf[w] = pair_buf[r];
                w += 1;
            }
        }
        w
    };
    let pairs = &pair_buf[..n];
    anyhow::ensure!(n < u32::MAX as usize - 2, "too many objects ({n}) for 32-bit object ids");
    {
        let mut a = Appender::<u64>::create(&path(dir, "addr.u64"))?;
        let mut o = Appender::<u64>::create(&path(dir, "offset.u64"))?;
        let mut buf = Vec::with_capacity(1 << 20);
        for c in pairs.chunks(1 << 20) {
            buf.clear();
            buf.extend(c.iter().map(|p| p[0]));
            a.extend(&buf)?;
            buf.clear();
            buf.extend(c.iter().map(|p| p[1]));
            o.extend(&buf)?;
        }
        a.finish()?;
        o.finish()?;
    }
    drop(pair_buf);
    let addr: Arr<u64> = Arr::open(&path(dir, "addr.u64"))?;
    let offset: Arr<u64> = Arr::open(&path(dir, "offset.u64"))?;
    progress(Phase::Indexing, 0.3);
    let align = object_alignment(&addr);

    // ---- Classes ----
    let mut names: FxHashMap<u64, String> = FxHashMap::default();
    for lc in &top.classes {
        names.insert(lc.class_id, openprofiler_model::symbols::java_class_name(&h.string(&top, lc.name_id)));
    }
    let mut dumps: Vec<_> = class_offsets
        .par_iter()
        .filter_map(|&off| match h.record_at(off) {
            Some(SubRecord::Class(c)) => Some(c),
            _ => None,
        })
        .collect();
    dumps.sort_by_key(|c| c.class_id);
    dumps.dedup_by_key(|c| c.class_id);
    let class_by_addr: FxHashMap<u64, u32> =
        dumps.iter().enumerate().map(|(i, c)| (c.class_id, i as u32)).collect();
    let find_obj = |a: u64| addr.binary_search(&a).ok().map(|i| i as u32).unwrap_or(u32::MAX);
    let mut classes: Vec<ClassInfo> = dumps
        .iter()
        .map(|c| {
            let name =
                names.get(&c.class_id).cloned().unwrap_or_else(|| format!("<class 0x{:x}>", c.class_id));
            ClassInfo {
                address: c.class_id,
                kind: kind_from_name(&name),
                name,
                super_class: class_by_addr.get(&c.super_id).copied(),
                loader: c.loader_id,
                signers: c.signers_id,
                protection_domain: c.protection_domain_id,
                object: find_obj(c.class_id),
                fields: c
                    .fields
                    .iter()
                    .map(|f| FieldInfo { name: h.string(&top, f.name).into_owned(), ty: f.ty })
                    .collect(),
                statics: c
                    .statics
                    .iter()
                    .map(|s| StaticInfo {
                        name: h.string(&top, s.name).into_owned(),
                        ty: s.ty,
                        value: s.value,
                    })
                    .collect(),
            }
        })
        .collect();
    drop(dumps);
    let synth = |classes: &mut Vec<ClassInfo>, name: &str, kind: ClassKind| -> u32 {
        if let Some(i) = classes.iter().position(|c| c.name == name) {
            return i as u32;
        }
        classes.push(ClassInfo {
            address: 0,
            name: name.into(),
            super_class: None,
            loader: 0,
            signers: 0,
            protection_domain: 0,
            object: u32::MAX,
            kind,
            fields: vec![],
            statics: vec![],
        });
        (classes.len() - 1) as u32
    };
    let mut prim = [u32::MAX; 12];
    for t in [ty::BOOLEAN, ty::CHAR, ty::FLOAT, ty::DOUBLE, ty::BYTE, ty::SHORT, ty::INT, ty::LONG] {
        prim[t as usize] =
            synth(&mut classes, &format!("{}[]", openprofiler_hprof::type_name(t)), ClassKind::PrimArray(t));
    }
    let java_lang_class = synth(&mut classes, "java.lang.Class", ClassKind::Instance);
    let object_array = synth(&mut classes, "java.lang.Object[]", ClassKind::ObjArray);
    let unknown_class = synth(&mut classes, "<unknown class>", ClassKind::Instance);
    let sizes = Sizes::new(id_size, opts.compressed_oops.unwrap_or(compressed_detected), align);
    let layouts = compute_layouts(&classes, id_size, sizes);
    let class_mirror: Vec<u32> = classes.iter().map(|c| c.object).collect();
    let class_loader_class = classes.iter().position(|c| c.name == "java.lang.ClassLoader").map(|i| i as u32);

    let (table, addr_min, addr_shift) = build_table(&addr);
    let index = AddrIndex { addr: &addr, table: &table, min: addr_min, shift: addr_shift };
    progress(Phase::Indexing, 0.5);

    // ---- Roots ----
    // Stack locals (Java frame, JNI local, native stack, thread block) of a known thread are
    // not GC roots: they become references from the thread object, so the thread dominates
    // what only its stack keeps alive. Locals of unknown threads stay roots.
    let threads: FxHashMap<u32, u64> = raw_roots
        .iter()
        .filter(|r| r.1 == RootKind::ThreadObject as u8 && r.2 != u32::MAX)
        .map(|r| (r.2, r.0))
        .collect();
    let mut roots: Vec<(u32, u8)> = vec![];
    let mut locals: Vec<[u32; 3]> = vec![];
    for &(a, k, serial) in &raw_roots {
        let Some(obj) = index.find(a) else { continue };
        let thread_context = matches!(
            RootKind::from_u8(k),
            RootKind::JniLocal | RootKind::JavaFrame | RootKind::NativeStack | RootKind::ThreadBlock
        );
        match threads.get(&serial).filter(|_| thread_context) {
            Some(&thread_addr) => {
                if let Some(t) = index.find(thread_addr) {
                    locals.push([t, obj, k as u32]);
                }
            }
            None => roots.push((obj, k)),
        }
    }
    drop(raw_roots);
    roots.par_sort_unstable();
    roots.dedup();
    let mut root_objs: Vec<u32> = roots.iter().map(|r| r.0).collect();
    root_objs.dedup();
    locals.sort_unstable();
    locals.dedup();
    write_slice(&path(dir, "locals.u32x3"), &locals)?;
    let mut locals_by_thread: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    for l in &locals {
        let v = locals_by_thread.entry(l[0]).or_default();
        if v.last() != Some(&l[1]) {
            v.push(l[1]);
        }
    }
    drop(locals);

    // ---- Pass B: classes, shallow sizes, outgoing CSR (parallel over id chunks) ----
    struct ChunkOut {
        class: Vec<u32>,
        shallow: Vec<u32>,
        flags: Vec<u8>,
        deg: Vec<u32>,
        targets: Vec<u32>,
    }
    let decode = |id: usize, out: &mut ChunkOut| {
        let push = |out: &mut ChunkOut, a: u64| -> bool {
            if a == 0 {
                return false;
            }
            match index.find(a) {
                Some(t) => {
                    out.targets.push(t);
                    true
                }
                None => false,
            }
        };
        let before = out.targets.len();
        let mut flags = 0u8;
        let rec = if offset[id] == BOOT_LOADER_OFFSET { None } else { h.record_at(offset[id]) };
        let (class, shallow) = match rec {
            None if offset[id] == BOOT_LOADER_OFFSET => match class_loader_class {
                Some(cl) => {
                    if class_mirror[cl as usize] != u32::MAX {
                        out.targets.push(class_mirror[cl as usize]);
                    }
                    (cl, layouts[cl as usize].shallow)
                }
                None => (unknown_class, sizes.pointer + sizes.ref_size),
            },
            Some(SubRecord::Instance { class_id, data, stack_serial, .. }) => {
                let ci = class_by_addr.get(&class_id).copied().unwrap_or(unknown_class);
                let l = &layouts[ci as usize];
                for &o in &l.ref_offsets {
                    if Some(o) != l.referent_offset && (o + id_size) as usize <= data.len() {
                        push(out, read_id(data, o as usize, id_size));
                    }
                }
                let m = class_mirror[ci as usize];
                if m != u32::MAX {
                    out.targets.push(m);
                }
                if let Some(l) = locals_by_thread.get(&(id as u32)) {
                    out.targets.extend_from_slice(l);
                }
                if let Some(o) = l.referent_offset
                    && (o + id_size) as usize <= data.len()
                    && push(out, read_id(data, o as usize, id_size))
                {
                    flags |= F_REFERENT_LAST;
                }
                (ci, sized(stack_serial, l.shallow))
            }
            Some(SubRecord::ObjArray { class_id, len, elems, stack_serial, .. }) => {
                let ci = class_by_addr.get(&class_id).copied().unwrap_or(object_array);
                for i in 0..len as usize {
                    push(out, read_id(elems, i * id_size as usize, id_size));
                }
                let m = class_mirror[ci as usize];
                if m != u32::MAX {
                    out.targets.push(m);
                }
                (ci, sized(stack_serial, sizes.object_array(len)))
            }
            Some(SubRecord::PrimArray { ty: t, len, stack_serial, .. }) => {
                let ci = prim.get(t as usize).copied().filter(|&c| c != u32::MAX).unwrap_or(unknown_class);
                let m = class_mirror[ci as usize];
                if m != u32::MAX {
                    out.targets.push(m);
                }
                (ci, sized(stack_serial, sizes.primitive_array(len, type_size(t, 8))))
            }
            Some(SubRecord::Class(c)) => {
                // Same edges as MAT: <class>, <super>, <classloader> (0 = boot loader pseudo
                // object) and static references. Signers and protection domain are not edges.
                let m = class_mirror[java_lang_class as usize];
                if m != u32::MAX {
                    out.targets.push(m);
                }
                push(out, c.super_id);
                if c.loader_id == 0 {
                    out.targets.push(0);
                } else {
                    push(out, c.loader_id);
                }
                for s in &c.statics {
                    if s.ty == ty::OBJECT {
                        push(out, s.value);
                    }
                }
                let computed = if phd { 0 } else { sizes.class_object(c.statics.iter().map(|s| s.ty)) };
                (java_lang_class, sized(c.stack_serial, computed))
            }
            _ => (unknown_class, 0),
        };
        out.class.push(class);
        out.shallow.push(shallow);
        out.flags.push(flags);
        out.deg.push((out.targets.len() - before) as u32);
    };

    let chunk = 1usize << 15;
    let batch_chunks = 512usize;
    let n_chunks = n.div_ceil(chunk);
    let mut class_file = Appender::<u32>::create(&path(dir, "class.u32"))?;
    let mut shallow_file = Appender::<u32>::create(&path(dir, "shallow.u32"))?;
    let mut off_file = Appender::<u64>::create(&path(dir, "out_off.u64"))?;
    off_file.extend(&[0])?;
    let mut last_off = 0u64;
    let mut flags: Vec<u8> = Vec::with_capacity(n);
    let mut out_file = Appender::<u32>::create(&path(dir, "out.u32"))?;
    let mut b = 0;
    while b < n_chunks {
        let e = (b + batch_chunks).min(n_chunks);
        let outs: Vec<ChunkOut> = (b..e)
            .into_par_iter()
            .map(|c| {
                let (s, t) = (c * chunk, ((c + 1) * chunk).min(n));
                let mut o = ChunkOut {
                    class: Vec::with_capacity(t - s),
                    shallow: Vec::with_capacity(t - s),
                    flags: Vec::with_capacity(t - s),
                    deg: Vec::with_capacity(t - s),
                    targets: Vec::with_capacity((t - s) * 4),
                };
                for id in s..t {
                    decode(id, &mut o);
                }
                o
            })
            .collect();
        for o in outs {
            class_file.extend(&o.class)?;
            shallow_file.extend(&o.shallow)?;
            flags.extend_from_slice(&o.flags);
            let offs: Vec<u64> = o
                .deg
                .iter()
                .map(|&d| {
                    last_off += d as u64;
                    last_off
                })
                .collect();
            off_file.extend(&offs)?;
            out_file.extend(&o.targets)?;
        }
        progress(Phase::Graph, e as f64 / n_chunks.max(1) as f64);
        b = e;
    }
    let n_edges = out_file.len;
    out_file.finish()?;
    class_file.finish()?;
    shallow_file.finish()?;
    off_file.finish()?;
    // Everything finished so far is re-mapped: the page cache, not the heap, holds it.
    write_slice(&path(dir, "addr_table.u32"), &table)?;
    drop((addr, offset, table));
    let class_of: Arr<u32> = Arr::open(&path(dir, "class.u32"))?;
    let shallow: Arr<u32> = Arr::open(&path(dir, "shallow.u32"))?;
    let out_off: Arr<u64> = Arr::open(&path(dir, "out_off.u64"))?;
    let out: Arr<u32> = Arr::open(&path(dir, "out.u32"))?;

    // ---- Incoming CSR ----
    progress(Phase::Incoming, 0.0);
    let in_deg_raw = Big::<u32>::zeroed(n);
    // SAFETY: `AtomicU32` has the same size and alignment as `u32`, and the mapping is only
    // accessed through this view until it is dropped.
    let in_deg: &[AtomicU32] = unsafe { &*(&*in_deg_raw as *const [u32] as *const [AtomicU32]) };
    out.par_chunks(1 << 16).for_each(|c| {
        for &t in c {
            in_deg[t as usize].fetch_add(1, Ordering::Relaxed);
        }
    });
    {
        let mut f = Appender::<u64>::create(&path(dir, "in_off.u64"))?;
        let mut acc = 0u64;
        let mut buf = Vec::with_capacity(1 << 20);
        buf.push(0);
        for d in in_deg {
            acc += d.swap(0, Ordering::Relaxed) as u64;
            buf.push(acc);
            if buf.len() == 1 << 20 {
                f.extend(&buf)?;
                buf.clear();
            }
        }
        f.extend(&buf)?;
        f.finish()?;
    }
    let in_off: Arr<u64> = Arr::open(&path(dir, "in_off.u64"))?;
    {
        let mut inc: MutArr<u32> = MutArr::create(&path(dir, "in.u32"), n_edges as usize)?;
        // `in_deg` is reused as the per-row fill cursor.
        let cursor = in_deg;
        let ptr = SyncPtr(inc.as_mut_ptr());
        (0..n.div_ceil(1 << 14)).into_par_iter().for_each(|c| {
            for src in c << 14..((c + 1) << 14).min(n) {
                for &t in &out[out_off[src] as usize..out_off[src + 1] as usize] {
                    let p = in_off[t as usize] + cursor[t as usize].fetch_add(1, Ordering::Relaxed) as u64;
                    // SAFETY: each position is handed out exactly once by the atomic cursor.
                    unsafe { ptr.write(p as usize, src as u32) };
                }
            }
        });
        sort_rows_by(&in_off, &mut inc, |a, b| a.cmp(b));
        inc.flush()?;
    }
    drop(in_deg_raw);
    let inc: Arr<u32> = Arr::open(&path(dir, "in.u32"))?;
    progress(Phase::Incoming, 1.0);

    for &r in &root_objs {
        flags[r as usize] |= F_ROOT;
    }
    let packed: Vec<u64> = roots.iter().map(|&(o, k)| ((o as u64) << 8) | k as u64).collect();
    write_slice(&path(dir, "roots.u64"), &packed)?;

    // ---- Dominators ----
    progress(Phase::Dominators, 0.0);
    let g = HeapGraph {
        n,
        out_off: &out_off,
        out: &out,
        in_off: &in_off,
        inc: &inc,
        flags: &flags,
        roots: &root_objs,
        exclude_weak: opts.exclude_weak,
    };
    let d = semi_nca(&g, n as u32);
    drop(inc);
    drop(in_off);
    progress(Phase::Retained, 0.0);
    write_slice(&path(dir, "idom.u32"), &d.idom[..n])?;
    let mut retained: MutArr<u64> = MutArr::create(&path(dir, "retained.u64"), n + 1)?;
    for &v in d.preorder() {
        if (v as usize) < n {
            retained[v as usize] = shallow[v as usize] as u64;
            flags[v as usize] |= F_REACHABLE;
        }
    }
    for &v in d.preorder().iter().skip(1).rev() {
        let p = d.idom[v as usize];
        retained[p as usize] += retained[v as usize];
    }
    let reachable_objects = d.reachable as u64 - 1;
    let reachable_shallow = retained[n];
    // Dominator children CSR (row n = super root). Counting sort with the shifted-offsets trick:
    // after filling, `dom_off[i]` is the start of row `i`.
    // One spare trailing entry (equal to the total) keeps the trick in bounds for row `n`.
    let mut dom_off: MutArr<u64> = MutArr::create(&path(dir, "dom_off.u64"), n + 3)?;
    for &v in d.preorder().iter().skip(1) {
        dom_off[d.idom[v as usize] as usize + 2] += 1;
    }
    for i in 2..n + 3 {
        dom_off[i] += dom_off[i - 1];
    }
    let mut dom: MutArr<u32> = MutArr::create(&path(dir, "dom.u32"), reachable_objects as usize)?;
    for &v in d.preorder().iter().skip(1) {
        let p = d.idom[v as usize] as usize;
        let pos = dom_off[p + 1];
        dom[pos as usize] = v;
        dom_off[p + 1] = pos + 1;
    }
    drop(d);
    sort_rows_by(&dom_off, &mut dom, |a, b| retained[*b as usize].cmp(&retained[*a as usize]).then(a.cmp(b)));
    retained.flush()?;
    dom_off.flush()?;
    dom.flush()?;
    write_slice(&path(dir, "flags.u8"), &flags)?;

    // ---- Class aggregates ----
    progress(Phase::Finishing, 0.0);
    let nc = classes.len();
    // Like MAT, the histogram counts reachable objects only.
    let (count, shal) = class_of
        .par_chunks(1 << 16)
        .zip(shallow.par_chunks(1 << 16))
        .zip(flags.par_chunks(1 << 16))
        .fold(
            || (vec![0u64; nc], vec![0u64; nc]),
            |(mut c, mut s), ((cls, sh), fl)| {
                for ((&k, &z), &f) in cls.iter().zip(sh).zip(fl) {
                    if f & F_REACHABLE != 0 {
                        c[k as usize] += 1;
                        s[k as usize] += z as u64;
                    }
                }
                (c, s)
            },
        )
        .reduce(
            || (vec![0u64; nc], vec![0u64; nc]),
            |(mut a, mut b), (c, d)| {
                for i in 0..nc {
                    a[i] += c[i];
                    b[i] += d[i];
                }
                (a, b)
            },
        );
    let class_retained = group_retained(n, &dom_off, &dom, &retained, nc, |v| class_of[v as usize]);
    write_slice(&path(dir, "class_count.u64"), &count)?;
    write_slice(&path(dir, "class_shallow.u64"), &shal)?;
    write_slice(&path(dir, "class_retained.u64"), &class_retained)?;
    std::fs::write(path(dir, "classes.json"), serde_json::to_vec(&classes)?)?;

    let total_shallow = shallow.par_iter().map(|&s| s as u64).sum();
    let meta = Meta {
        engine_version: ENGINE_VERSION,
        key,
        hprof_path: src.to_path_buf(),
        id_size,
        timestamp_ms: h.header.timestamp_ms,
        objects: n as u64,
        edges: n_edges,
        classes: nc as u32,
        roots: root_objs.len() as u64,
        total_shallow,
        reachable_objects,
        reachable_shallow,
        sizes,
        exclude_weak: opts.exclude_weak,
        java_lang_class,
        unknown_class,
        addr_min,
        addr_shift,
        phd,
    };
    std::fs::write(path(dir, "meta.json"), serde_json::to_vec_pretty(&meta)?)?;
    progress(Phase::Finishing, 1.0);
    Ok(meta)
}

/// Retained size per group: the sum of retained sizes of objects that have no dominator-tree
/// ancestor in the same group (i.e. the dominator-based retained size of each group's objects).
pub(crate) fn group_retained(
    n: usize,
    dom_off: &[u64],
    dom: &[u32],
    retained: &[u64],
    groups: usize,
    group_of: impl Fn(u32) -> u32,
) -> Vec<u64> {
    let mut out = vec![0u64; groups];
    let mut active = vec![0u32; groups];
    // (node, next child position)
    let mut stack: Vec<(u32, u64)> = vec![(n as u32, dom_off[n])];
    while let Some(top) = stack.last_mut() {
        let (v, pos) = *top;
        if pos < dom_off[v as usize + 1] {
            top.1 += 1;
            let c = dom[pos as usize];
            let gidx = group_of(c);
            if gidx != NONE {
                let g = gidx as usize;
                if active[g] == 0 {
                    out[g] += retained[c as usize];
                }
                active[g] += 1;
            }
            stack.push((c, dom_off[c as usize]));
        } else {
            stack.pop();
            if (v as usize) < n {
                let gidx = group_of(v);
                if gidx != NONE {
                    active[gidx as usize] -= 1;
                }
            }
        }
    }
    out
}
