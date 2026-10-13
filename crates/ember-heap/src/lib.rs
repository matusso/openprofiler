//! Out-of-core heap dump analysis.
//!
//! [`HeapAnalysis::open`] indexes an HPROF file (or reuses a persisted index) and answers the
//! Heap queries of the Query API. All query methods take `&self`; the type is `Send + Sync`.
//!
//! Public API:
//!
//! - [`HeapAnalysis::open`]`(path, &OpenOptions, progress)` — build or reuse the index in
//!   `<file>.ember-idx/` (falls back to the user cache dir when the dump's directory is read-only).
//! - [`HeapAnalysis::summary`] → [`HeapSummary`]
//! - [`HeapAnalysis::classes`]`(&ClassQuery)` → `Page<ClassRow>` (group by class/package/loader)
//! - [`HeapAnalysis::objects`]`(&ObjectQuery)` → `Page<ObjectRow>`
//! - [`HeapAnalysis::biggest`]`(Option<object>, limit)` → dominator-tree children by retained size
//! - [`HeapAnalysis::dominator_classes`]`(Option<object>)` → dominator children grouped by class
//! - [`HeapAnalysis::refs`]`(object, Direction, offset, limit)` → `Page<RefRow>`
//! - [`HeapAnalysis::merged_refs`]`(set, Direction)` → `Vec<MergedRefRow>`
//! - [`HeapAnalysis::paths_to_roots`]`(object, max_paths, exclude_weak)` → `Vec<Vec<PathStep>>`
//! - [`HeapAnalysis::object`]`(object)` → [`ObjectDetail`]; [`HeapAnalysis::string_value`]
//! - [`HeapAnalysis::select`]`(from, SelectStep)` / [`HeapAnalysis::sets`] /
//!   [`HeapAnalysis::set_retained`] — object sets ("selection steps"); set `0` = all objects
//! - [`HeapAnalysis::inspections`] → `Vec<Insight>`; [`HeapAnalysis::inspection`]`(kind, limit)`
//! - [`HeapAnalysis::histogram_diff`]`(&other)` → `Vec<HistogramDiffRow>`
//! - [`HeapAnalysis::object_row`], [`HeapAnalysis::find_object`], [`HeapAnalysis::is_reachable`],
//!   [`HeapAnalysis::class_name`], [`HeapAnalysis::object_count`]
//!
//! Sizes, references, GC roots and reachability follow Eclipse MAT's HPROF model, so object
//! counts, shallow and retained sizes match MAT exactly (see `scripts/mat-parity.sh`).

mod build;
pub mod classes;
pub mod dom;
mod dto;
mod inspect;
#[doc(hidden)]
pub mod store;

pub use dto::*;

use anyhow::{Context, Result};
use build::{AddrIndex, F_REACHABLE, F_ROOT, HeapGraph, Meta, group_retained, path};
use classes::{ClassInfo, ClassKind, Layout, compute_layouts, package_of};
use ember_hprof::{Hprof, RootKind, SubRecord, read_id, ty, type_name, type_size};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use store::Arr;

/// Index build phases reported through the progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Decompressing,
    Scanning,
    Indexing,
    Graph,
    Incoming,
    Dominators,
    Retained,
    Finishing,
    Loading,
    Ready,
}

#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Explicit index directory. Default: `<dump>.ember-idx/`, falling back to the user cache.
    pub cache_dir: Option<PathBuf>,
    /// Ignore `java.lang.ref.Reference.referent` edges for reachability and dominators.
    /// Default false, matching Eclipse MAT's retained sizes.
    pub exclude_weak: bool,
    /// Force 4-byte (`true`) or 8-byte references on 64-bit dumps. Default: detected from
    /// object array spacing, like MAT.
    pub compressed_oops: Option<bool>,
    /// Rebuild even if a valid index exists.
    pub force_rebuild: bool,
}

pub(crate) enum SetData {
    All,
    List(Vec<u32>),
}

pub(crate) struct ObjectSet {
    dto: ObjectSetDto,
    data: Arc<SetData>,
}

/// An indexed heap dump.
pub struct HeapAnalysis {
    meta: Meta,
    dir: PathBuf,
    hprof: Hprof,
    classes: Vec<ClassInfo>,
    layouts: Vec<Layout>,
    class_by_addr: FxHashMap<u64, u32>,
    addr: Arr<u64>,
    addr_table: Arr<u32>,
    offset: Arr<u64>,
    class: Arr<u32>,
    shallow: Arr<u32>,
    flags: Arr<u8>,
    out_off: Arr<u64>,
    out: Arr<u32>,
    in_off: Arr<u64>,
    inc: Arr<u32>,
    idom: Arr<u32>,
    retained: Arr<u64>,
    dom_off: Arr<u64>,
    dom: Arr<u32>,
    class_count: Arr<u64>,
    class_shallow: Arr<u64>,
    class_retained: Arr<u64>,
    /// `(object, root kind)` sorted.
    roots: Vec<(u32, u8)>,
    /// `[thread, local object, root kind]` sorted: stack locals referenced by thread objects.
    locals: Vec<[u32; 3]>,
    sets: RwLock<Vec<ObjectSet>>,
    group_cache: Mutex<FxHashMap<GroupBy, Arc<Vec<u64>>>>,
    inspection_cache: Mutex<FxHashMap<String, Arc<InspectionResult>>>,
}

fn cache_key(path: &Path, opts: &OpenOptions) -> Result<String> {
    use std::io::Read;
    let md = std::fs::metadata(path)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut head = vec![0u8; 1 << 20];
    let mut f = std::fs::File::open(path)?;
    let mut read = 0;
    while read < head.len() {
        let k = f.read(&mut head[read..])?;
        if k == 0 {
            break;
        }
        read += k;
    }
    let h = xxhash_rust::xxh3::xxh3_64(&head[..read]);
    Ok(format!(
        "{}-{}-{:016x}-e{}-w{}-c{:?}",
        md.len(),
        mtime,
        h,
        build::ENGINE_VERSION,
        opts.exclude_weak,
        opts.compressed_oops
    ))
}

fn user_cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("EMBER_CACHE_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(d).join("ember");
    }
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    if cfg!(target_os = "macos") { home.join("Library/Caches/ember") } else { home.join(".cache/ember") }
}

fn resolve_cache_dir(path: &Path, opts: &OpenOptions) -> Result<PathBuf> {
    if let Some(d) = &opts.cache_dir {
        std::fs::create_dir_all(d)?;
        return Ok(d.clone());
    }
    let mut sibling = path.as_os_str().to_owned();
    sibling.push(".ember-idx");
    let sibling = PathBuf::from(sibling);
    if std::fs::create_dir_all(&sibling).is_ok() {
        let probe = sibling.join(".write-test");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(probe);
            return Ok(sibling);
        }
    }
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let h = xxhash_rust::xxh3::xxh3_64(canon.to_string_lossy().as_bytes());
    let d = user_cache_dir().join("heap").join(format!("{h:016x}"));
    std::fs::create_dir_all(&d).with_context(|| format!("creating cache dir {}", d.display()))?;
    Ok(d)
}

fn hex(a: u64) -> String {
    format!("0x{a:x}")
}

impl HeapAnalysis {
    /// Opens (indexing on first use) an HPROF heap dump, optionally gzip-compressed.
    pub fn open(path: &Path, opts: &OpenOptions, progress: impl Fn(Phase, f64) + Sync) -> Result<Self> {
        let dir = resolve_cache_dir(path, opts)?;
        let key = cache_key(path, opts)?;
        let existing: Option<Meta> =
            std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(
                |m: &Meta| m.key == key && m.engine_version == build::ENGINE_VERSION && m.hprof_path.exists(),
            );
        let meta = match existing {
            Some(m) if !opts.force_rebuild => {
                progress(Phase::Loading, 0.0);
                m
            }
            _ => {
                let src = if ember_hprof::is_gzip(path)? {
                    progress(Phase::Decompressing, 0.0);
                    let out = dir.join("dump.hprof");
                    ember_hprof::decompress_gz(path, &out)?;
                    out
                } else {
                    std::fs::canonicalize(path)?
                };
                build::build(&src, &dir, key, opts, &progress)?
            }
        };
        let a = Self::load(dir, meta)?;
        progress(Phase::Ready, 1.0);
        Ok(a)
    }

    fn load(dir: PathBuf, meta: Meta) -> Result<Self> {
        let hprof = Hprof::open(&meta.hprof_path)?;
        let classes: Vec<ClassInfo> = serde_json::from_slice(&std::fs::read(path(&dir, "classes.json"))?)?;
        let layouts = compute_layouts(&classes, meta.id_size, meta.sizes);
        let class_by_addr = classes
            .iter()
            .enumerate()
            .filter(|(_, c)| c.address != 0)
            .map(|(i, c)| (c.address, i as u32))
            .collect();
        let packed: Arr<u64> = Arr::open(&path(&dir, "roots.u64"))?;
        let roots = packed.iter().map(|&p| ((p >> 8) as u32, (p & 0xff) as u8)).collect();
        let locals: Arr<[u32; 3]> = Arr::open(&path(&dir, "locals.u32x3"))?;
        let locals = locals.to_vec();
        let o = |n: &str| path(&dir, n);
        Ok(Self {
            addr: Arr::open(&o("addr.u64"))?,
            addr_table: Arr::open(&o("addr_table.u32"))?,
            offset: Arr::open(&o("offset.u64"))?,
            class: Arr::open(&o("class.u32"))?,
            shallow: Arr::open(&o("shallow.u32"))?,
            flags: Arr::open(&o("flags.u8"))?,
            out_off: Arr::open(&o("out_off.u64"))?,
            out: Arr::open(&o("out.u32"))?,
            in_off: Arr::open(&o("in_off.u64"))?,
            inc: Arr::open(&o("in.u32"))?,
            idom: Arr::open(&o("idom.u32"))?,
            retained: Arr::open(&o("retained.u64"))?,
            dom_off: Arr::open(&o("dom_off.u64"))?,
            dom: Arr::open(&o("dom.u32"))?,
            class_count: Arr::open(&o("class_count.u64"))?,
            class_shallow: Arr::open(&o("class_shallow.u64"))?,
            class_retained: Arr::open(&o("class_retained.u64"))?,
            roots,
            locals,
            meta,
            dir,
            hprof,
            classes,
            layouts,
            class_by_addr,
            sets: RwLock::new(vec![]),
            group_cache: Mutex::new(FxHashMap::default()),
            inspection_cache: Mutex::new(FxHashMap::default()),
        })
        .inspect(|a| {
            let all = ObjectSetDto {
                id: 0,
                parent: None,
                label: "All objects".into(),
                objects: a.meta.reachable_objects,
                shallow: a.meta.reachable_shallow,
                retained: Some(a.meta.reachable_shallow),
            };
            a.sets.write().unwrap().push(ObjectSet { dto: all, data: Arc::new(SetData::All) });
        })
    }

    /// Directory holding the persisted index.
    pub fn cache_dir(&self) -> &Path {
        &self.dir
    }

    pub fn object_count(&self) -> u64 {
        self.meta.objects
    }

    pub fn summary(&self) -> HeapSummary {
        HeapSummary {
            objects: self.meta.objects,
            classes: self.classes.len() as u32,
            total_shallow: self.meta.total_shallow,
            gc_roots: self.meta.roots,
            id_size: self.meta.id_size,
            dump_time: (self.meta.timestamp_ms > 0).then(|| self.meta.timestamp_ms as i64 * 1_000_000),
            analysis_state: "ready".into(),
            reachable_objects: self.meta.reachable_objects,
            reachable_shallow: self.meta.reachable_shallow,
            edges: self.meta.edges,
        }
    }

    // ---------------------------------------------------------------- object basics

    fn index(&self) -> AddrIndex<'_> {
        AddrIndex {
            addr: &self.addr,
            table: &self.addr_table,
            min: self.meta.addr_min,
            shift: self.meta.addr_shift,
        }
    }

    fn n(&self) -> usize {
        self.meta.objects as usize
    }

    fn valid(&self, obj: u32) -> Result<()> {
        anyhow::ensure!((obj as usize) < self.n(), "no object with id {obj}");
        Ok(())
    }

    /// Object id of the object at `address`, if the dump contains it.
    pub fn find_object(&self, address: u64) -> Option<u32> {
        self.index().find(address)
    }

    pub fn class_name(&self, class_id: u32) -> &str {
        self.classes.get(class_id as usize).map(|c| c.name.as_str()).unwrap_or("?")
    }

    pub fn class_of(&self, obj: u32) -> u32 {
        self.class[obj as usize]
    }

    pub fn shallow_size(&self, obj: u32) -> u64 {
        self.shallow[obj as usize] as u64
    }

    pub fn retained_size(&self, obj: u32) -> u64 {
        self.retained[obj as usize]
    }

    /// Whether the object is reachable from GC roots (unreachable objects have no retained size).
    pub fn is_reachable(&self, obj: u32) -> bool {
        self.reachable(obj)
    }

    fn reachable(&self, obj: u32) -> bool {
        self.flags[obj as usize] & F_REACHABLE != 0
    }

    fn record(&self, obj: u32) -> Option<SubRecord<'_>> {
        self.hprof.record_at(self.offset[obj as usize])
    }

    fn graph(&self, exclude_weak: bool) -> HeapGraph<'_> {
        HeapGraph {
            n: self.n(),
            out_off: &self.out_off,
            out: &self.out,
            in_off: &self.in_off,
            inc: &self.inc,
            flags: &self.flags,
            roots: &[],
            exclude_weak,
        }
    }

    fn out_edges(&self, obj: u32) -> &[u32] {
        &self.out[self.out_off[obj as usize] as usize..self.out_off[obj as usize + 1] as usize]
    }

    fn in_edges(&self, obj: u32) -> &[u32] {
        &self.inc[self.in_off[obj as usize] as usize..self.in_off[obj as usize + 1] as usize]
    }

    fn dom_children(&self, node: usize) -> &[u32] {
        &self.dom[self.dom_off[node] as usize..self.dom_off[node + 1] as usize]
    }

    pub fn object_row(&self, obj: u32) -> ObjectRow {
        let reachable = self.reachable(obj);
        ObjectRow {
            id: obj,
            address: hex(self.addr[obj as usize]),
            class_name: self.class_name(self.class[obj as usize]).to_owned(),
            shallow: self.shallow[obj as usize] as u64,
            retained: Some(if reachable { self.retained[obj as usize] } else { 0 }),
            preview: self.preview(obj),
            child_count: Some(self.dom_off[obj as usize + 1] - self.dom_off[obj as usize]),
        }
    }

    /// Reads an instance field by name as raw bits (refs are addresses).
    pub(crate) fn field_raw(&self, obj: u32, name: &str) -> Option<(u8, u64)> {
        let SubRecord::Instance { class_id, data, .. } = self.record(obj)? else { return None };
        let ci = *self.class_by_addr.get(&class_id)?;
        let f = self.layouts[ci as usize].field(name)?;
        let v = read_value(data, f.offset as usize, f.ty, self.meta.id_size)?;
        Some((f.ty, v))
    }

    pub(crate) fn field_ref(&self, obj: u32, name: &str) -> Option<u32> {
        match self.field_raw(obj, name)? {
            (ty::OBJECT, a) if a != 0 => self.index().find(a),
            _ => None,
        }
    }

    pub(crate) fn field_int(&self, obj: u32, name: &str) -> Option<i64> {
        let (t, v) = self.field_raw(obj, name)?;
        Some(match t {
            ty::INT => v as u32 as i32 as i64,
            ty::LONG => v as i64,
            ty::SHORT => v as u16 as i16 as i64,
            ty::BYTE => v as u8 as i8 as i64,
            ty::BOOLEAN => v as i64,
            ty::CHAR => v as i64,
            _ => return None,
        })
    }

    /// Array length of an array object.
    pub(crate) fn array_len(&self, obj: u32) -> Option<u32> {
        match self.record(obj)? {
            SubRecord::ObjArray { len, .. } | SubRecord::PrimArray { len, .. } => Some(len),
            _ => None,
        }
    }

    /// The text of a `java.lang.String` (JDK 9+ compact strings and older `char[]` strings).
    pub fn string_value(&self, obj: u32) -> Option<String> {
        if self.class_name(self.class[obj as usize]) != "java.lang.String" {
            return None;
        }
        let value = self.field_ref(obj, "value")?;
        let coder = self.field_int(obj, "coder").unwrap_or(0);
        let SubRecord::PrimArray { ty: t, data, .. } = self.record(value)? else { return None };
        Some(decode_string(t, data, coder))
    }

    fn preview(&self, obj: u32) -> Option<String> {
        if self.offset[obj as usize] == build::BOOT_LOADER_OFFSET {
            return Some("<bootstrap class loader>".into());
        }
        let ci = self.class[obj as usize];
        let c = &self.classes[ci as usize];
        if ci == self.meta.java_lang_class {
            let a = self.addr[obj as usize];
            return self.class_by_addr.get(&a).map(|&k| format!("class {}", self.classes[k as usize].name));
        }
        match c.kind {
            ClassKind::ObjArray | ClassKind::PrimArray(_) => {
                return self.array_len(obj).map(|l| format!("length {l}"));
            }
            ClassKind::Instance => {}
        }
        match c.name.as_str() {
            "java.lang.String" => self.string_value(obj).map(|s| quote(&s, 120)),
            "java.lang.Integer" | "java.lang.Long" | "java.lang.Short" | "java.lang.Byte" => {
                self.field_int(obj, "value").map(|v| v.to_string())
            }
            "java.lang.Boolean" => self.field_int(obj, "value").map(|v| (v != 0).to_string()),
            "java.lang.Character" => {
                self.field_int(obj, "value").map(|v| format!("'{}'", char::from_u32(v as u32).unwrap_or('?')))
            }
            "java.lang.Double" => self.field_raw(obj, "value").map(|(_, v)| f64::from_bits(v).to_string()),
            "java.lang.Float" => {
                self.field_raw(obj, "value").map(|(_, v)| f32::from_bits(v as u32).to_string())
            }
            "java.lang.Thread" | "java.lang.VirtualThread" => {
                self.field_ref(obj, "name").and_then(|s| self.string_value(s)).map(|s| quote(&s, 80))
            }
            _ => {
                // Enum constants show their name.
                let is_enum =
                    self.superclasses(ci).any(|s| self.classes[s as usize].name == "java.lang.Enum");
                if is_enum { self.field_ref(obj, "name").and_then(|s| self.string_value(s)) } else { None }
            }
        }
    }

    fn superclasses(&self, ci: u32) -> impl Iterator<Item = u32> + '_ {
        let mut cur = Some(ci);
        let mut guard = 0;
        std::iter::from_fn(move || {
            let c = cur?;
            guard += 1;
            if guard > 256 {
                return None;
            }
            cur = self.classes[c as usize].super_class;
            Some(c)
        })
    }

    /// Outgoing references with field names: `(field, target address)`, non-null only.
    fn named_out(&self, obj: u32) -> Vec<(String, u64)> {
        let mut v = vec![];
        let id = self.meta.id_size;
        if self.offset[obj as usize] == build::BOOT_LOADER_OFFSET {
            let cl = self.classes[self.class[obj as usize] as usize].address;
            if cl != 0 {
                v.push(("<class>".into(), cl));
            }
            return v;
        }
        match self.record(obj) {
            Some(SubRecord::Instance { class_id, data, .. }) => {
                if let Some(&ci) = self.class_by_addr.get(&class_id) {
                    for f in &self.layouts[ci as usize].fields {
                        if f.ty == ty::OBJECT && (f.offset + id) as usize <= data.len() {
                            let a = read_id(data, f.offset as usize, id);
                            if a != 0 {
                                v.push((f.name.clone(), a));
                            }
                        }
                    }
                }
                v.push(("<class>".into(), class_id));
                let i = self.locals.partition_point(|l| l[0] < obj);
                for l in self.locals[i..].iter().take_while(|l| l[0] == obj) {
                    v.push((
                        format!("<{}>", RootKind::from_u8(l[2] as u8).label()),
                        self.addr[l[1] as usize],
                    ));
                }
            }
            Some(SubRecord::ObjArray { class_id, len, elems, .. }) => {
                for i in 0..len as usize {
                    let a = read_id(elems, i * id as usize, id);
                    if a != 0 {
                        v.push((format!("[{i}]"), a));
                    }
                }
                v.push(("<class>".into(), class_id));
            }
            Some(SubRecord::PrimArray { .. }) => {
                let m = self.classes[self.class[obj as usize] as usize].address;
                if m != 0 {
                    v.push(("<class>".into(), m));
                }
            }
            Some(SubRecord::Class(c)) => {
                let jlc = self.classes[self.meta.java_lang_class as usize].address;
                if jlc != 0 {
                    v.push(("<class>".into(), jlc));
                }
                if c.super_id != 0 {
                    v.push(("<super>".into(), c.super_id));
                }
                // Loader 0 is the bootstrap loader pseudo-object at address 0.
                v.push(("<classloader>".into(), c.loader_id));
                if let Some(&ci) = self.class_by_addr.get(&c.class_id) {
                    for s in &self.classes[ci as usize].statics {
                        if s.ty == ty::OBJECT && s.value != 0 {
                            v.push((format!("static {}", s.name), s.value));
                        }
                    }
                }
            }
            _ => {}
        }
        v
    }

    fn root_kinds(&self, obj: u32) -> Vec<String> {
        let i = self.roots.partition_point(|r| r.0 < obj);
        self.roots[i..]
            .iter()
            .take_while(|r| r.0 == obj)
            .map(|r| RootKind::from_u8(r.1).label().to_owned())
            .collect()
    }

    // ---------------------------------------------------------------- classes & objects

    fn set_data(&self, set: Option<u32>) -> Result<Arc<SetData>> {
        let sets = self.sets.read().unwrap();
        let s = sets
            .get(set.unwrap_or(0) as usize)
            .with_context(|| format!("no object set {}", set.unwrap_or(0)))?;
        Ok(s.data.clone())
    }

    fn for_each_in_set(&self, data: &SetData, mut f: impl FnMut(u32)) {
        match data {
            SetData::All => (0..self.n() as u32).filter(|&o| self.reachable(o)).for_each(f),
            SetData::List(v) => v.iter().for_each(|&o| f(o)),
        }
    }

    fn group_key(
        &self,
        g: GroupBy,
        ci: u32,
        groups: &mut FxHashMap<String, u32>,
        names: &mut Vec<String>,
    ) -> u32 {
        let name = match g {
            GroupBy::Class => return ci,
            GroupBy::Package => {
                let p = package_of(&self.classes[ci as usize].name);
                if p.is_empty() { "<default package>".to_owned() } else { p.to_owned() }
            }
            GroupBy::Classloader => self.loader_label(self.classes[ci as usize].loader),
        };
        *groups.entry(name.clone()).or_insert_with(|| {
            names.push(name);
            (names.len() - 1) as u32
        })
    }

    fn loader_label(&self, loader: u64) -> String {
        if loader == 0 {
            return "<bootstrap>".into();
        }
        match self.index().find(loader) {
            Some(o) => format!("{}@{}", self.class_name(self.class[o as usize]), hex(loader)),
            None => format!("<loader {}>", hex(loader)),
        }
    }

    /// Class → group mapping plus group names for package/classloader grouping.
    fn class_groups(&self, g: GroupBy) -> (Vec<u32>, Vec<String>) {
        let mut groups = FxHashMap::default();
        let mut names = vec![];
        let map =
            (0..self.classes.len() as u32).map(|ci| self.group_key(g, ci, &mut groups, &mut names)).collect();
        if g == GroupBy::Class {
            names = self.classes.iter().map(|c| c.name.clone()).collect();
        }
        (map, names)
    }

    fn group_retained_cached(&self, g: GroupBy, class_group: &[u32], groups: usize) -> Arc<Vec<u64>> {
        if g == GroupBy::Class {
            return Arc::new(self.class_retained.to_vec());
        }
        if let Some(v) = self.group_cache.lock().unwrap().get(&g) {
            return v.clone();
        }
        let v = Arc::new(group_retained(self.n(), &self.dom_off, &self.dom, &self.retained, groups, |o| {
            class_group[self.class[o as usize] as usize]
        }));
        self.group_cache.lock().unwrap().insert(g, v.clone());
        v
    }

    pub fn classes(&self, q: &ClassQuery) -> Result<Page<ClassRow>> {
        let data = self.set_data(q.set)?;
        let (class_group, names) = self.class_groups(q.group_by);
        let ng = names.len();
        let (count, shallow, retained): (Vec<u64>, Vec<u64>, Option<Arc<Vec<u64>>>) = match &*data {
            SetData::All => {
                let mut c = vec![0u64; ng];
                let mut s = vec![0u64; ng];
                for ci in 0..self.classes.len() {
                    c[class_group[ci] as usize] += self.class_count[ci];
                    s[class_group[ci] as usize] += self.class_shallow[ci];
                }
                (c, s, Some(self.group_retained_cached(q.group_by, &class_group, ng)))
            }
            SetData::List(v) => {
                let mut c = vec![0u64; ng];
                let mut s = vec![0u64; ng];
                for &o in v {
                    let g = class_group[self.class[o as usize] as usize] as usize;
                    c[g] += 1;
                    s[g] += self.shallow[o as usize] as u64;
                }
                (c, s, None)
            }
        };
        let filter = q.filter.as_ref().map(|f| f.to_lowercase());
        let mut rows: Vec<ClassRow> = (0..ng)
            .filter(|&g| count[g] > 0)
            .filter(|&g| filter.as_ref().is_none_or(|f| names[g].to_lowercase().contains(f)))
            .map(|g| ClassRow {
                class_id: (q.group_by == GroupBy::Class).then_some(g as u32),
                name: names[g].clone(),
                instances: count[g],
                shallow: shallow[g],
                retained: retained.as_ref().map(|r| r[g]),
            })
            .collect();
        match q.sort {
            ClassSort::Instances => {
                rows.sort_by(|a, b| b.instances.cmp(&a.instances).then(a.name.cmp(&b.name)))
            }
            ClassSort::Shallow => rows.sort_by(|a, b| b.shallow.cmp(&a.shallow).then(a.name.cmp(&b.name))),
            ClassSort::Retained => rows.sort_by(|a, b| b.retained.cmp(&a.retained).then(a.name.cmp(&b.name))),
            ClassSort::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        }
        Ok(page(rows, q.offset, q.limit))
    }

    pub fn objects(&self, q: &ObjectQuery) -> Result<Page<ObjectRow>> {
        let data = self.set_data(q.set)?;
        let mut ids: Vec<u32> = match (&*data, q.class_id) {
            (SetData::All, Some(c)) => {
                use rayon::prelude::*;
                (0..self.n() as u32)
                    .into_par_iter()
                    .filter(|&o| self.class[o as usize] == c && self.reachable(o))
                    .collect()
            }
            (d, c) => {
                let mut v = vec![];
                self.for_each_in_set(d, |o| {
                    if c.is_none_or(|c| self.class[o as usize] == c) {
                        v.push(o)
                    }
                });
                v
            }
        };
        let total = ids.len() as u64;
        let (off, lim) = (q.offset.unwrap_or(0), q.limit.unwrap_or(100));
        let key = |o: &u32| match q.sort {
            ObjectSort::Shallow => self.shallow[*o as usize] as u64,
            ObjectSort::Retained => self.retained[*o as usize],
        };
        let k = (off + lim).min(ids.len());
        if k < ids.len() && k > 0 {
            ids.select_nth_unstable_by(k - 1, |a, b| key(b).cmp(&key(a)).then(a.cmp(b)));
            ids.truncate(k);
        }
        ids.sort_by(|a, b| key(b).cmp(&key(a)).then(a.cmp(b)));
        Ok(Page {
            total_rows: total,
            rows: ids.into_iter().skip(off).take(lim).map(|o| self.object_row(o)).collect(),
        })
    }

    /// Dominator-tree children of `object` (or the GC-root level when `None`), by retained size.
    pub fn biggest(&self, object: Option<u32>, limit: usize) -> Result<Vec<ObjectRow>> {
        let node = match object {
            Some(o) => {
                self.valid(o)?;
                o as usize
            }
            None => self.n(),
        };
        Ok(self.dom_children(node).iter().take(limit).map(|&o| self.object_row(o)).collect())
    }

    /// Dominator-tree children of `object` grouped by class (for sunburst / treemap views).
    pub fn dominator_classes(&self, object: Option<u32>) -> Result<Vec<DominatorClassRow>> {
        let node = match object {
            Some(o) => {
                self.valid(o)?;
                o as usize
            }
            None => self.n(),
        };
        let mut acc: FxHashMap<u32, (u64, u64)> = FxHashMap::default();
        for &c in self.dom_children(node) {
            let e = acc.entry(self.class[c as usize]).or_default();
            e.0 += 1;
            e.1 += self.retained[c as usize];
        }
        let mut rows: Vec<_> = acc
            .into_iter()
            .map(|(ci, (n, r))| DominatorClassRow {
                name: self.class_name(ci).to_owned(),
                objects: n,
                retained: r,
            })
            .collect();
        rows.sort_by(|a, b| b.retained.cmp(&a.retained).then(a.name.cmp(&b.name)));
        Ok(rows)
    }

    // ---------------------------------------------------------------- references

    pub fn refs(&self, object: u32, dir: Direction, offset: usize, limit: usize) -> Result<Page<RefRow>> {
        self.valid(object)?;
        let mut rows = vec![];
        match dir {
            Direction::Outgoing => {
                let idx = self.index();
                for (field, a) in self.named_out(object) {
                    if let Some(t) = idx.find(a) {
                        rows.push((field, t));
                    }
                }
            }
            Direction::Incoming => {
                for k in self.root_kinds(object) {
                    rows.push((format!("<root: {k}>"), object));
                }
                let target = self.addr[object as usize];
                let mut srcs: Vec<u32> = self.in_edges(object).to_vec();
                srcs.dedup();
                let total_src = srcs.len();
                // Field names require decoding the referrer; only decode the requested page.
                let mut produced = 0usize;
                for (i, &s) in srcs.iter().enumerate() {
                    if produced >= offset + limit {
                        // Count the rest without decoding.
                        let rest = total_src - i;
                        let total = rows.len() + rest;
                        let rows = rows
                            .into_iter()
                            .skip(offset)
                            .take(limit)
                            .map(|(field, o)| RefRow { object: self.object_row(o), field })
                            .collect();
                        return Ok(Page { total_rows: total as u64, rows });
                    }
                    for (field, a) in self.named_out(s) {
                        if a == target {
                            rows.push((field, s));
                            produced += 1;
                        }
                    }
                }
            }
        }
        let total = rows.len() as u64;
        let rows = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(field, o)| RefRow { object: self.object_row(o), field })
            .collect();
        Ok(Page { total_rows: total, rows })
    }

    /// References of all objects in a set, grouped by `(class, field)`.
    pub fn merged_refs(&self, set: u32, dir: Direction) -> Result<Vec<MergedRefRow>> {
        let data = self.set_data(Some(set))?;
        let idx = self.index();
        let mut acc: FxHashMap<(u32, String), (FxHashSet<u32>, u64)> = FxHashMap::default();
        let mut budget = 2_000_000usize;
        self.for_each_in_set(&data, |o| {
            if budget == 0 {
                return;
            }
            budget -= 1;
            match dir {
                Direction::Outgoing => {
                    for (field, a) in self.named_out(o) {
                        if let Some(t) = idx.find(a) {
                            let e = acc.entry((self.class[t as usize], strip_index(&field))).or_default();
                            e.0.insert(t);
                            e.1 += 1;
                        }
                    }
                }
                Direction::Incoming => {
                    let target = self.addr[o as usize];
                    let mut srcs = self.in_edges(o).to_vec();
                    srcs.dedup();
                    for s in srcs {
                        for (field, a) in self.named_out(s) {
                            if a == target {
                                let e = acc.entry((self.class[s as usize], strip_index(&field))).or_default();
                                e.0.insert(s);
                                e.1 += 1;
                            }
                        }
                    }
                }
            }
        });
        let mut rows: Vec<_> = acc
            .into_iter()
            .map(|((ci, field), (objs, refs))| MergedRefRow {
                class_name: self.class_name(ci).to_owned(),
                field,
                objects: objs.len() as u64,
                refs,
            })
            .collect();
        rows.sort_by(|a, b| b.refs.cmp(&a.refs).then(a.class_name.cmp(&b.class_name)));
        Ok(rows)
    }

    /// Shortest reference chains from GC roots to `object` (one per distinct root reached).
    /// Each path starts at the object and ends at a GC root.
    pub fn paths_to_roots(
        &self,
        object: u32,
        max_paths: usize,
        exclude_weak: bool,
    ) -> Result<Vec<Vec<PathStep>>> {
        self.valid(object)?;
        let g = self.graph(exclude_weak);
        // next[u] = the neighbour of u that is one step closer to `object`.
        let mut next: FxHashMap<u32, u32> = FxHashMap::default();
        next.insert(object, u32::MAX);
        let mut queue = VecDeque::from([object]);
        let mut found = vec![];
        let budget = 20_000_000usize;
        while let Some(v) = queue.pop_front() {
            if self.flags[v as usize] & F_ROOT != 0 {
                found.push(v);
                if found.len() >= max_paths.max(1) {
                    break;
                }
                continue;
            }
            if next.len() > budget {
                break;
            }
            g.for_each_pred(v, &mut |u| {
                if let std::collections::hash_map::Entry::Vacant(e) = next.entry(u) {
                    e.insert(v);
                    queue.push_back(u);
                }
            });
        }
        let mut paths = vec![];
        for root in found {
            // root → … → object
            let mut chain = vec![root];
            let mut cur = root;
            while cur != object {
                cur = next[&cur];
                chain.push(cur);
            }
            chain.reverse();
            let mut steps = Vec::with_capacity(chain.len());
            for (i, &o) in chain.iter().enumerate() {
                let field = chain.get(i + 1).and_then(|&holder| {
                    let target = self.addr[o as usize];
                    self.named_out(holder).into_iter().find(|(_, a)| *a == target).map(|(f, _)| f)
                });
                let root_kind = (i == chain.len() - 1).then(|| self.root_kinds(o).join(", "));
                steps.push(PathStep { object: self.object_row(o), field, root_kind });
            }
            paths.push(steps);
        }
        Ok(paths)
    }

    pub fn object(&self, obj: u32) -> Result<ObjectDetail> {
        self.object_page(obj, 0, 1000)
    }

    /// Object details with array elements `[elem_offset, elem_offset + elem_limit)`.
    pub fn object_page(&self, obj: u32, elem_offset: usize, elem_limit: usize) -> Result<ObjectDetail> {
        self.valid(obj)?;
        let id = self.meta.id_size;
        let idx = self.index();
        let ref_value = |a: u64| -> (String, Option<u32>) {
            if a == 0 {
                return ("null".into(), None);
            }
            match idx.find(a) {
                Some(t) => {
                    let mut s = format!("{}@{}", self.class_name(self.class[t as usize]), hex(a));
                    if let Some(p) = self.preview(t) {
                        s.push(' ');
                        s.push_str(&p);
                    }
                    (s, Some(t))
                }
                None => (format!("<unresolved {}>", hex(a)), None),
            }
        };
        let mut fields = vec![];
        let mut array_length = None;
        let mut elements = None;
        match self.record(obj) {
            Some(SubRecord::Instance { class_id, data, .. }) => {
                if let Some(&ci) = self.class_by_addr.get(&class_id) {
                    for f in &self.layouts[ci as usize].fields {
                        let Some(raw) = read_value(data, f.offset as usize, f.ty, id) else { continue };
                        let (value, reference) =
                            if f.ty == ty::OBJECT { ref_value(raw) } else { (format_prim(f.ty, raw), None) };
                        let ty_name =
                            if f.ty == ty::OBJECT { "ref".to_owned() } else { type_name(f.ty).to_owned() };
                        fields.push(FieldValue {
                            name: f.name.clone(),
                            ty: ty_name,
                            value,
                            reference,
                            is_static: false,
                        });
                    }
                }
            }
            Some(SubRecord::ObjArray { len, elems, .. }) => {
                array_length = Some(len);
                elements = Some(
                    (elem_offset..(elem_offset + elem_limit).min(len as usize))
                        .map(|i| {
                            let (value, reference) = ref_value(read_id(elems, i * id as usize, id));
                            ElementValue { index: i as u32, value, reference }
                        })
                        .collect(),
                );
            }
            Some(SubRecord::PrimArray { ty: t, len, data, .. }) => {
                array_length = Some(len);
                let sz = type_size(t, id) as usize;
                elements = Some(
                    (elem_offset..(elem_offset + elem_limit).min(len as usize))
                        .filter_map(|i| {
                            let raw = read_value(data, i * sz, t, id)?;
                            Some(ElementValue {
                                index: i as u32,
                                value: format_prim(t, raw),
                                reference: None,
                            })
                        })
                        .collect(),
                );
            }
            Some(SubRecord::Class(c)) => {
                for (n, a) in [
                    ("<super>", c.super_id),
                    ("<classloader>", c.loader_id),
                    ("<protection domain>", c.protection_domain_id),
                ] {
                    let (value, reference) = ref_value(a);
                    fields.push(FieldValue {
                        name: n.into(),
                        ty: "ref".into(),
                        value,
                        reference,
                        is_static: false,
                    });
                }
                let statics =
                    self.class_by_addr.get(&c.class_id).map(|&ci| &self.classes[ci as usize].statics);
                for s in statics.into_iter().flatten() {
                    let (value, reference) = if s.ty == ty::OBJECT {
                        ref_value(s.value)
                    } else {
                        (format_prim(s.ty, s.value), None)
                    };
                    let ty_name =
                        if s.ty == ty::OBJECT { "ref".to_owned() } else { type_name(s.ty).to_owned() };
                    fields.push(FieldValue {
                        name: s.name.clone(),
                        ty: ty_name,
                        value,
                        reference,
                        is_static: true,
                    });
                }
            }
            _ => {}
        }
        let d = self.idom[obj as usize];
        Ok(ObjectDetail {
            object: self.object_row(obj),
            fields,
            array_length,
            elements,
            gc_root_kinds: self.root_kinds(obj),
            dominator: (d != dom::NONE && (d as usize) < self.n()).then_some(d),
            reachable: self.reachable(obj),
        })
    }

    // ---------------------------------------------------------------- object sets

    pub fn sets(&self) -> Vec<ObjectSetDto> {
        self.sets.read().unwrap().iter().map(|s| s.dto.clone()).collect()
    }

    /// Applies a selection step to set `from` (default: all objects) and registers the result.
    pub fn select(&self, from: Option<u32>, step: &SelectStep) -> Result<ObjectSetDto> {
        let from_id = from.unwrap_or(0);
        let data = self.set_data(Some(from_id))?;
        let members = |f: &mut dyn FnMut(u32)| self.for_each_in_set(&data, f);
        let mut out: Vec<u32> = vec![];
        let label;
        match step {
            SelectStep::ClassId(c) => {
                label = format!("Instances of {}", self.class_name(*c));
                members(&mut |o| {
                    if self.class[o as usize] == *c {
                        out.push(o)
                    }
                });
            }
            SelectStep::ClassName(name) => {
                label = format!("Instances of {name}");
                let ids: FxHashSet<u32> = self
                    .classes
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| &c.name == name)
                    .map(|(i, _)| i as u32)
                    .collect();
                members(&mut |o| {
                    if ids.contains(&self.class[o as usize]) {
                        out.push(o)
                    }
                });
            }
            SelectStep::Package(p) => {
                label = format!("Objects in package {p}");
                let ids: Vec<bool> = self.classes.iter().map(|c| package_of(&c.name) == p).collect();
                members(&mut |o| {
                    if ids[self.class[o as usize] as usize] {
                        out.push(o)
                    }
                });
            }
            SelectStep::RetainedBy(obj) => {
                self.valid(*obj)?;
                label = format!(
                    "Retained by {}@{}",
                    self.class_name(self.class[*obj as usize]),
                    hex(self.addr[*obj as usize])
                );
                self.dom_subtree(*obj, &mut out);
            }
            SelectStep::DominatedBySet(_) => {
                label = "Retained objects".into();
                members(&mut |o| self.dom_subtree(o, &mut out));
            }
            SelectStep::ReferencedBySet(_) => {
                label = "Referenced objects".into();
                members(&mut |o| out.extend_from_slice(self.out_edges(o)));
            }
            SelectStep::ReferencingSet(_) => {
                label = "Referencing objects".into();
                members(&mut |o| out.extend_from_slice(self.in_edges(o)));
            }
            SelectStep::Objects(v) => {
                label = format!("{} selected objects", v.len());
                out.extend(v.iter().copied().filter(|&o| (o as usize) < self.n()));
            }
        }
        out.sort_unstable();
        out.dedup();
        let shallow = out.iter().map(|&o| self.shallow[o as usize] as u64).sum();
        let mut sets = self.sets.write().unwrap();
        let dto = ObjectSetDto {
            id: sets.len() as u32,
            parent: Some(from_id),
            label,
            objects: out.len() as u64,
            shallow,
            retained: None,
        };
        sets.push(ObjectSet { dto: dto.clone(), data: Arc::new(SetData::List(out)) });
        Ok(dto)
    }

    fn dom_subtree(&self, root: u32, out: &mut Vec<u32>) {
        let mut stack = vec![root];
        while let Some(v) = stack.pop() {
            out.push(v);
            stack.extend_from_slice(self.dom_children(v as usize));
        }
    }

    /// Exact retained size of a set: objects that become unreachable when the set is removed.
    /// O(objects + edges); the result is cached on the set.
    pub fn set_retained(&self, set: u32) -> Result<u64> {
        let data = self.set_data(Some(set))?;
        if let Some(r) = self.sets.read().unwrap()[set as usize].dto.retained {
            return Ok(r);
        }
        let n = self.n();
        let SetData::List(members) = &*data else { return Ok(self.meta.reachable_shallow) };
        let mut excluded = vec![false; n];
        for &m in members {
            excluded[m as usize] = true;
        }
        let g = self.graph(self.meta.exclude_weak);
        let mut seen = vec![false; n];
        let mut stack: Vec<u32> = vec![];
        let mut i = 0;
        while i < self.roots.len() {
            let r = self.roots[i].0;
            if !excluded[r as usize] && !seen[r as usize] {
                seen[r as usize] = true;
                stack.push(r);
            }
            i += 1;
        }
        while let Some(v) = stack.pop() {
            for &w in g.strong_out(v) {
                if !excluded[w as usize] && !seen[w as usize] {
                    seen[w as usize] = true;
                    stack.push(w);
                }
            }
        }
        let retained: u64 = (0..n)
            .filter(|&o| self.flags[o] & F_REACHABLE != 0 && !seen[o])
            .map(|o| self.shallow[o] as u64)
            .sum();
        self.sets.write().unwrap()[set as usize].dto.retained = Some(retained);
        Ok(retained)
    }

    // ---------------------------------------------------------------- comparisons

    /// Class histogram difference (`self` = a, `other` = b), matched by class name.
    pub fn histogram_diff(&self, other: &HeapAnalysis) -> Vec<HistogramDiffRow> {
        let mut acc: FxHashMap<&str, [u64; 4]> = FxHashMap::default();
        for (ci, c) in self.classes.iter().enumerate() {
            let e = acc.entry(&c.name).or_default();
            e[0] += self.class_count[ci];
            e[2] += self.class_shallow[ci];
        }
        for (ci, c) in other.classes.iter().enumerate() {
            let e = acc.entry(&c.name).or_default();
            e[1] += other.class_count[ci];
            e[3] += other.class_shallow[ci];
        }
        let mut rows: Vec<_> = acc
            .into_iter()
            .filter(|(_, v)| v[0] + v[1] > 0)
            .map(|(name, v)| HistogramDiffRow {
                name: name.to_owned(),
                a_instances: v[0],
                b_instances: v[1],
                a_shallow: v[2],
                b_shallow: v[3],
            })
            .collect();
        rows.sort_by(|a, b| {
            let da = (a.b_shallow as i128 - a.a_shallow as i128).abs();
            let db = (b.b_shallow as i128 - b.a_shallow as i128).abs();
            db.cmp(&da).then(a.name.cmp(&b.name))
        });
        rows
    }
}

fn page<T>(rows: Vec<T>, offset: Option<usize>, limit: Option<usize>) -> Page<T> {
    let total = rows.len() as u64;
    Page {
        total_rows: total,
        rows: rows.into_iter().skip(offset.unwrap_or(0)).take(limit.unwrap_or(usize::MAX)).collect(),
    }
}

fn strip_index(field: &str) -> String {
    if field.starts_with('[') { "[]".into() } else { field.to_owned() }
}

pub(crate) fn read_value(data: &[u8], off: usize, t: u8, id_size: u32) -> Option<u64> {
    let sz = type_size(t, id_size) as usize;
    let b = data.get(off..off + sz)?;
    Some(match sz {
        1 => b[0] as u64,
        2 => u16::from_be_bytes([b[0], b[1]]) as u64,
        4 => u32::from_be_bytes(b.try_into().ok()?) as u64,
        8 => u64::from_be_bytes(b.try_into().ok()?),
        _ => return None,
    })
}

fn format_prim(t: u8, v: u64) -> String {
    match t {
        ty::BOOLEAN => (v != 0).to_string(),
        ty::CHAR => format!("'{}'", char::from_u32(v as u32).unwrap_or('?').escape_default()),
        ty::FLOAT => f32::from_bits(v as u32).to_string(),
        ty::DOUBLE => f64::from_bits(v).to_string(),
        ty::BYTE => (v as u8 as i8).to_string(),
        ty::SHORT => (v as u16 as i16).to_string(),
        ty::INT => (v as u32 as i32).to_string(),
        ty::LONG => (v as i64).to_string(),
        _ => format!("0x{v:x}"),
    }
}

/// Decodes `String.value` contents. `coder` 0 = Latin-1, 1 = UTF-16 (platform order, assumed
/// little-endian); `char[]` values (pre-JDK 9) are big-endian in HPROF.
pub(crate) fn decode_string(t: u8, data: &[u8], coder: i64) -> String {
    match t {
        ty::CHAR => {
            let units: Vec<u16> =
                data.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        }
        ty::BYTE if coder == 1 => {
            let units: Vec<u16> =
                data.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        }
        _ => data.iter().map(|&b| b as char).collect(),
    }
}

fn quote(s: &str, max: usize) -> String {
    let mut out = String::from("\"");
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.extend(c.escape_debug());
    }
    out.push('"');
    out
}
