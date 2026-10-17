//! Native heap dumper and recorded objects.
//!
//! The dumper walks the reachable heap with JVMTI `FollowReferences` in a private JVMTI
//! environment (so its tags never meet the agent's) and writes HPROF 1.0.2, the format the heap
//! engine and `.opsnap` heap sections already read, so retained sizes are computed exactly as for
//! HotSpot's own dumps. It works where `HotSpotDiagnosticMXBean.dumpHeap` does not and can dump
//! only the objects recorded by allocation recording.
//!
//! Object ids are synthetic: each object's tag is its id, assigned on discovery from a running
//! sum of `GetObjectSize`, so ids look like a densely packed heap (the engine infers the object
//! alignment from them). Compressed references are detected from object array sizes and written
//! to the hint record ([`openprofiler_hprof::hints_id`]), as ids are not addresses.
//!
//! HotSpot reports all references of an object (and then its primitive values) in one burst
//! while it visits the object, so instances are written as soon as the next object starts and
//! the walk keeps no per-object state. Classes are collected up front: their fields give the
//! instance layouts and the JVMTI field indices (interface fields first, then `Object`'s down to
//! the class's own, statics included); their statics, constant-pool references, super, loader,
//! signers and protection domain are filled in during the walk and written at the end. Roots map
//! onto HPROF roots; stack and JNI locals carry their thread's serial, so the engine turns them
//! into references from the thread object as it does for HotSpot dumps.
//!
//! Recorded objects are tagged in another private environment when allocation recording samples
//! them (tags die with their objects). A recorded-only dump first walks the heap remembering each
//! object's first referrer (4 bytes per object), keeps the recorded objects and their referrer
//! chains up to the roots, and then walks again writing only those (references to other objects
//! are written as null), plus all classes.

use crate::Agent;
use crate::jvm::{Jvmti, Vm, check, jni, jvmti};
use jvmti_sys::*;
use openprofiler_hprof::{tag as htag, ty as hty, writer::HprofWriter};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::ptr;

/// Environment in which objects sampled by allocation recording carry [`RECORDED`].
static RECORDER: RwLock<Option<Jvmti>> = RwLock::new(None);
const RECORDED: jlong = 1;

/// First synthetic object id.
const BASE: u64 = 0x10_0000;
/// Parent of an object not reached yet in the first recorded-only walk.
const UNSET: u32 = u32::MAX;
const ACC_STATIC: jint = 0x0008;

/// A JVMTI environment with `can_tag_objects`, disposed (with all its tags) on drop.
struct Env(Jvmti);

impl Env {
    fn new(vm: Vm) -> Result<Self, String> {
        let mut env: *mut c_void = ptr::null_mut();
        // SAFETY: valid JavaVM; GetEnv creates a new JVMTI environment.
        let r = unsafe { ((**vm.0).GetEnv.expect("GetEnv"))(vm.0, &mut env, JVMTI_VERSION_1_2) };
        if r != JNI_OK || env.is_null() {
            return Err("cannot create a JVMTI environment".into());
        }
        let env = Env(Jvmti(env as *mut jvmtiEnv));
        env.0
            .add_capabilities(&jvmtiCapabilities::default().with(&[CAN_TAG_OBJECTS]))
            .map_err(|e| format!("object tagging unavailable ({e})"))?;
        Ok(env)
    }

    fn into_raw(self) -> Jvmti {
        let j = self.0;
        std::mem::forget(self);
        j
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        jvmti!(self.0, DisposeEnvironment);
    }
}

/// Starts a new set of recorded objects (allocation recording switched on).
pub fn reset_recorded(vm: Vm) {
    let mut r = RECORDER.write();
    if let Some(old) = r.take() {
        jvmti!(old, DisposeEnvironment);
    }
    match Env::new(vm) {
        Ok(e) => *r = Some(e.into_raw()),
        Err(e) => crate::debug(&format!("recorded objects unavailable: {e}")),
    }
}

/// Remembers an object sampled by allocation recording.
pub fn record(object: jobject) {
    if let Some(env) = *RECORDER.read() {
        env.set_tag(object, RECORDED);
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DumpStats {
    /// Instances and arrays written (classes not included).
    pub objects: u64,
    pub classes: u64,
    /// Recorded objects alive at the start of a recorded-only dump.
    pub recorded: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Instance,
    ObjArray,
    PrimArray(u8),
}

struct Field {
    name: u64,
    ty: u8,
    is_static: bool,
}

struct ClassInfo {
    tag: jlong,
    /// UTF8 id of the HPROF class name.
    name: u64,
    kind: Kind,
    is_interface: bool,
    super_tag: jlong,
    interfaces: Vec<jlong>,
    /// Declared fields in `GetClassFields` order.
    fields: Vec<Field>,
    /// JVMTI field index → (offset in the HPROF instance data, type) of instance fields.
    instance: Vec<Option<(u32, u8)>>,
    data_len: u32,
    /// JVMTI field index of the first declared field (statics are `static_base + j`).
    static_base: u32,
}

struct Classes {
    list: Vec<ClassInfo>,
    by_tag: HashMap<jlong, u32>,
}

/// Synthetic ids. Full dumps tag each object with its id. Recorded-only dumps tag objects with a
/// sequence number and give ids only to the objects they keep.
struct Ids {
    recorded: bool,
    next_addr: u64,
    next_seq: jlong,
    addr: HashMap<jlong, u64>,
    keep: Vec<bool>,
    parent: Vec<u32>,
}

fn object_bytes(size: jlong) -> u64 {
    (size.max(8) as u64).next_multiple_of(8)
}

impl Ids {
    fn new(recorded: bool) -> Self {
        Self {
            recorded,
            next_addr: BASE,
            next_seq: 1,
            addr: HashMap::new(),
            keep: vec![],
            parent: vec![UNSET],
        }
    }

    fn new_tag(&mut self, size: jlong) -> jlong {
        if !self.recorded {
            let t = self.next_addr;
            self.next_addr += object_bytes(size);
            return t as jlong;
        }
        let s = self.next_seq;
        self.next_seq += 1;
        if self.keep.is_empty() {
            self.parent.push(UNSET);
        }
        s
    }

    fn kept(&self, tag: jlong) -> bool {
        !self.recorded || self.keep.get(tag as usize).copied().unwrap_or(false)
    }

    /// Id of the object tagged `tag`, 0 if it is not written.
    fn id(&mut self, tag: jlong, size: jlong) -> u64 {
        if !self.recorded {
            return tag as u64;
        }
        if !self.kept(tag) {
            return 0;
        }
        let next = &mut self.next_addr;
        *self.addr.entry(tag).or_insert_with(|| {
            let a = *next;
            *next += object_bytes(size);
            a
        })
    }
}

/// Current object of the emitting walk.
enum Cur {
    None,
    Skip,
    Class(u32),
    Instance { class: Option<u32>, class_tag: jlong, data: Vec<u8> },
    ObjArray { class: u32, elems: Vec<u64> },
    PrimArray { ty: u8, done: bool },
}

#[derive(PartialEq, Eq)]
enum Pass {
    Parents,
    Emit,
}

struct Walk<'a> {
    pass: Pass,
    ids: Ids,
    classes: &'a Classes,
    w: Option<HprofWriter<BufWriter<File>>>,
    cur: Cur,
    cur_tag: jlong,
    /// Lengths of object arrays discovered but not visited yet.
    pending_len: HashMap<jlong, u32>,
    statics: Vec<Vec<u64>>,
    /// `(constant pool index, id)` per class.
    constants: Vec<Vec<(i32, u64)>>,
    /// Superclass, loader, signers and protection domain ids per class.
    refs: Vec<[u64; 4]>,
    serials: HashMap<jlong, u32>,
    compressed: Option<bool>,
    objects: u64,
    err: Option<io::Error>,
}

fn hprof_type(sig: u8) -> u8 {
    match sig {
        b'Z' => hty::BOOLEAN,
        b'C' => hty::CHAR,
        b'F' => hty::FLOAT,
        b'D' => hty::DOUBLE,
        b'B' => hty::BYTE,
        b'S' => hty::SHORT,
        b'I' => hty::INT,
        b'J' => hty::LONG,
        _ => hty::OBJECT,
    }
}

fn type_bytes(ty: u8) -> usize {
    openprofiler_hprof::type_size(ty, 8) as usize
}

/// Raw bits of a primitive value, as HPROF writes them (big-endian, `type_bytes` wide).
fn value_bits(v: jvalue, t: jvmtiPrimitiveType) -> u64 {
    // SAFETY: `t` names the union member the VM filled in.
    unsafe {
        match t {
            JVMTI_PRIMITIVE_TYPE_BOOLEAN => v.z as u64,
            JVMTI_PRIMITIVE_TYPE_BYTE => v.b as u8 as u64,
            JVMTI_PRIMITIVE_TYPE_CHAR => v.c as u64,
            JVMTI_PRIMITIVE_TYPE_SHORT => v.s as u16 as u64,
            JVMTI_PRIMITIVE_TYPE_INT => v.i as u32 as u64,
            JVMTI_PRIMITIVE_TYPE_FLOAT => v.f.to_bits() as u64,
            JVMTI_PRIMITIVE_TYPE_LONG => v.j as u64,
            JVMTI_PRIMITIVE_TYPE_DOUBLE => v.d.to_bits(),
            _ => 0,
        }
    }
}

fn put(data: &mut [u8], off: u32, ty: u8, bits: u64) {
    let n = type_bytes(ty);
    let off = off as usize;
    if let Some(dst) = data.get_mut(off..off + n) {
        dst.copy_from_slice(&bits.to_be_bytes()[8 - n..]);
    }
}

impl ClassInfo {
    fn instance_field(&self, index: jint) -> Option<(u32, u8)> {
        self.instance.get(usize::try_from(index).ok()?).copied().flatten()
    }

    /// Slot among the class's own static fields of JVMTI field `index`.
    fn static_slot(&self, index: jint) -> Option<usize> {
        let j = usize::try_from(index.checked_sub(self.static_base as jint)?).ok()?;
        self.fields.get(j).filter(|f| f.is_static)?;
        Some(self.fields[..j].iter().filter(|f| f.is_static).count())
    }
}

impl Walk<'_> {
    fn write(&mut self, f: impl FnOnce(&mut HprofWriter<BufWriter<File>>) -> io::Result<()>) {
        if let Some(w) = self.w.as_mut()
            && let Err(e) = f(w)
        {
            self.err.get_or_insert(e);
        }
    }

    fn control(&self) -> jint {
        if self.err.is_some() { JVMTI_VISIT_ABORT } else { JVMTI_VISIT_OBJECTS }
    }

    fn serial(&mut self, thread_tag: jlong) -> u32 {
        if thread_tag == 0 {
            return 0;
        }
        let n = self.serials.len() as u32 + 1;
        *self.serials.entry(thread_tag).or_insert(n)
    }

    /// Writes the previous object, if any, and starts `tag`.
    fn enter(&mut self, tag: jlong, class_tag: jlong, kind: jvmtiHeapReferenceKind) {
        if self.cur_tag == tag && !matches!(self.cur, Cur::None) {
            return;
        }
        self.flush();
        self.cur_tag = tag;
        let len = self.pending_len.remove(&tag);
        if !self.ids.kept(tag) {
            self.cur = Cur::Skip;
            return;
        }
        if let Some(&ci) = self.classes.by_tag.get(&tag) {
            self.cur = Cur::Class(ci);
            return;
        }
        let of_class = matches!(
            kind,
            JVMTI_HEAP_REFERENCE_STATIC_FIELD
                | JVMTI_HEAP_REFERENCE_SUPERCLASS
                | JVMTI_HEAP_REFERENCE_CLASS_LOADER
                | JVMTI_HEAP_REFERENCE_SIGNERS
                | JVMTI_HEAP_REFERENCE_PROTECTION_DOMAIN
                | JVMTI_HEAP_REFERENCE_INTERFACE
                | JVMTI_HEAP_REFERENCE_CONSTANT_POOL
        );
        let class = self.classes.by_tag.get(&class_tag).copied();
        self.cur = match class.map(|ci| (ci, &self.classes.list[ci as usize])) {
            // A class loaded after the class list was taken.
            _ if of_class => Cur::Skip,
            Some((ci, c)) => match c.kind {
                Kind::Instance => {
                    Cur::Instance { class: Some(ci), class_tag, data: vec![0; c.data_len as usize] }
                }
                Kind::ObjArray => Cur::ObjArray { class: ci, elems: vec![0; len.unwrap_or(0) as usize] },
                Kind::PrimArray(ty) => Cur::PrimArray { ty, done: false },
            },
            None => Cur::Instance { class: None, class_tag, data: vec![] },
        };
    }

    fn flush(&mut self) {
        let tag = self.cur_tag;
        match std::mem::replace(&mut self.cur, Cur::None) {
            Cur::Instance { class, class_tag, data } => {
                let id = self.ids.id(tag, 16);
                let class_id = match class {
                    Some(ci) => self.ids.id(self.classes.list[ci as usize].tag, 0),
                    None => self.ids.id(class_tag, 0),
                };
                self.objects += 1;
                self.write(|w| w.instance(id, class_id, &data));
            }
            Cur::ObjArray { class, elems } => {
                let id = self.ids.id(tag, 16);
                let class_id = self.ids.id(self.classes.list[class as usize].tag, 0);
                self.objects += 1;
                self.write(|w| w.obj_array(id, class_id, &elems));
            }
            Cur::PrimArray { ty, done: false } => {
                let id = self.ids.id(tag, 16);
                self.objects += 1;
                self.write(|w| w.prim_array(id, ty, 0, &[]));
            }
            _ => {}
        }
    }

    fn root(
        &mut self,
        kind: jvmtiHeapReferenceKind,
        info: *const jvmtiHeapReferenceInfo,
        tag: jlong,
        size: jlong,
    ) {
        if !self.ids.kept(tag) {
            return;
        }
        let id = self.ids.id(tag, size);
        // SAFETY: the VM passes info for the stack and JNI local kinds read below.
        let (t, serial, frame) = unsafe {
            match kind {
                JVMTI_HEAP_REFERENCE_JNI_GLOBAL => (htag::ROOT_JNI_GLOBAL, 0, 0),
                JVMTI_HEAP_REFERENCE_SYSTEM_CLASS => (htag::ROOT_STICKY_CLASS, 0, 0),
                JVMTI_HEAP_REFERENCE_MONITOR => (htag::ROOT_MONITOR_USED, 0, 0),
                JVMTI_HEAP_REFERENCE_STACK_LOCAL if !info.is_null() => {
                    let l = (*info).stack_local;
                    (htag::ROOT_JAVA_FRAME, self.serial(l.thread_tag), l.depth as u32)
                }
                JVMTI_HEAP_REFERENCE_JNI_LOCAL if !info.is_null() => {
                    let l = (*info).jni_local;
                    (htag::ROOT_JNI_LOCAL, self.serial(l.thread_tag), l.depth as u32)
                }
                JVMTI_HEAP_REFERENCE_THREAD => (htag::ROOT_THREAD_OBJECT, self.serial(tag), 0),
                _ => (htag::ROOT_UNKNOWN, 0, 0),
            }
        };
        self.write(|w| w.thread_root(t, id, serial, frame));
    }

    #[allow(clippy::too_many_arguments)]
    fn reference(
        &mut self,
        kind: jvmtiHeapReferenceKind,
        info: *const jvmtiHeapReferenceInfo,
        class_tag: jlong,
        referrer_class_tag: jlong,
        size: jlong,
        tag_ptr: *mut jlong,
        referrer_tag_ptr: *mut jlong,
        length: jint,
    ) -> jint {
        // SAFETY: the VM passes a valid tag pointer for the referee.
        let tag = unsafe {
            if *tag_ptr == 0 {
                *tag_ptr = self.ids.new_tag(size);
            }
            *tag_ptr
        };
        // SAFETY: when not null, the referrer's tag pointer is valid for this callback.
        let referrer = (!referrer_tag_ptr.is_null()).then(|| unsafe { *referrer_tag_ptr });
        if self.pass == Pass::Parents {
            let p = &mut self.ids.parent[tag as usize];
            if *p == UNSET && referrer != Some(tag) {
                *p = referrer.unwrap_or(0) as u32;
            }
            return JVMTI_VISIT_OBJECTS;
        }
        if length >= 0
            && let Some(&ci) = self.classes.by_tag.get(&class_tag)
            && self.classes.list[ci as usize].kind == Kind::ObjArray
        {
            if self.ids.kept(tag) {
                self.pending_len.insert(tag, length as u32);
            }
            if length >= 2 && self.compressed.is_none() {
                self.compressed = Some((size as u64) < 16 + 8 * length as u64);
            }
        }
        let Some(referrer) = referrer else {
            self.root(kind, info, tag, size);
            return self.control();
        };
        self.enter(referrer, referrer_class_tag, kind);
        if matches!(self.cur, Cur::Skip | Cur::None) {
            return self.control();
        }
        let id = self.ids.id(tag, size);
        // SAFETY: field, array and constant-pool kinds carry an index.
        let index = if info.is_null() { -1 } else { unsafe { (*info).field.index } };
        match (&mut self.cur, kind) {
            (Cur::Instance { class: Some(ci), data, .. }, JVMTI_HEAP_REFERENCE_FIELD) => {
                if let Some((off, hty::OBJECT)) = self.classes.list[*ci as usize].instance_field(index) {
                    put(data, off, hty::OBJECT, id);
                }
            }
            (Cur::ObjArray { elems, .. }, JVMTI_HEAP_REFERENCE_ARRAY_ELEMENT) if index >= 0 => {
                let i = index as usize;
                if i >= elems.len() {
                    elems.resize(i + 1, 0);
                }
                elems[i] = id;
            }
            (Cur::Class(ci), _) => {
                let ci = *ci as usize;
                match kind {
                    JVMTI_HEAP_REFERENCE_STATIC_FIELD => {
                        if let Some(slot) = self.classes.list[ci].static_slot(index) {
                            self.statics[ci][slot] = id;
                        }
                    }
                    // Resolved classes too: some (hidden classes) are reachable only this way.
                    JVMTI_HEAP_REFERENCE_CONSTANT_POOL if id != 0 => self.constants[ci].push((index, id)),
                    JVMTI_HEAP_REFERENCE_SUPERCLASS => self.refs[ci][0] = id,
                    JVMTI_HEAP_REFERENCE_CLASS_LOADER => self.refs[ci][1] = id,
                    JVMTI_HEAP_REFERENCE_SIGNERS => self.refs[ci][2] = id,
                    JVMTI_HEAP_REFERENCE_PROTECTION_DOMAIN => self.refs[ci][3] = id,
                    _ => {}
                }
            }
            _ => {}
        }
        self.control()
    }

    fn primitive_field(
        &mut self,
        kind: jvmtiHeapReferenceKind,
        info: *const jvmtiHeapReferenceInfo,
        class_tag: jlong,
        tag_ptr: *mut jlong,
        value: jvalue,
        value_type: jvmtiPrimitiveType,
    ) -> jint {
        if info.is_null() || tag_ptr.is_null() {
            return self.control();
        }
        // SAFETY: valid pointers for this callback; field kinds carry an index.
        let (tag, index) = unsafe { (*tag_ptr, (*info).field.index) };
        self.enter(tag, class_tag, kind);
        let bits = value_bits(value, value_type);
        match (&mut self.cur, kind) {
            (Cur::Instance { class: Some(ci), data, .. }, JVMTI_HEAP_REFERENCE_FIELD) => {
                if let Some((off, ty)) = self.classes.list[*ci as usize].instance_field(index)
                    && ty != hty::OBJECT
                {
                    put(data, off, ty, bits);
                }
            }
            (Cur::Class(ci), JVMTI_HEAP_REFERENCE_STATIC_FIELD) => {
                let ci = *ci as usize;
                if let Some(slot) = self.classes.list[ci].static_slot(index) {
                    self.statics[ci][slot] = bits;
                }
            }
            _ => {}
        }
        self.control()
    }

    fn array_values(
        &mut self,
        class_tag: jlong,
        size: jlong,
        tag_ptr: *mut jlong,
        count: jint,
        element_type: jvmtiPrimitiveType,
        elements: *const c_void,
    ) -> jint {
        if tag_ptr.is_null() {
            return self.control();
        }
        // SAFETY: valid tag pointer for this callback.
        let tag = unsafe { *tag_ptr };
        self.enter(tag, class_tag, JVMTI_HEAP_REFERENCE_CLASS);
        let Cur::PrimArray { done: false, .. } = self.cur else { return self.control() };
        let ty = hprof_type(element_type as u8);
        let n = type_bytes(ty);
        let count = count.max(0) as usize;
        let raw: &[u8] = if elements.is_null() || count == 0 {
            &[]
        } else {
            // SAFETY: the VM passes `count` elements of `element_type`.
            unsafe { std::slice::from_raw_parts(elements as *const u8, count * n) }
        };
        let mut be = raw.to_vec();
        if n > 1 && cfg!(target_endian = "little") {
            for e in be.chunks_exact_mut(n) {
                e.reverse();
            }
        }
        let id = self.ids.id(tag, size);
        self.cur = Cur::PrimArray { ty, done: true };
        self.objects += 1;
        self.write(|w| w.prim_array(id, ty, count as u32, &be));
        self.control()
    }
}

unsafe extern "C" fn on_reference(
    kind: jvmtiHeapReferenceKind,
    info: *const jvmtiHeapReferenceInfo,
    class_tag: jlong,
    referrer_class_tag: jlong,
    size: jlong,
    tag_ptr: *mut jlong,
    referrer_tag_ptr: *mut jlong,
    length: jint,
    user_data: *mut c_void,
) -> jint {
    // SAFETY: `user_data` is the `Walk` that `follow` passes, exclusively borrowed for the walk.
    let w = unsafe { &mut *(user_data as *mut Walk<'_>) };
    w.reference(kind, info, class_tag, referrer_class_tag, size, tag_ptr, referrer_tag_ptr, length)
}

unsafe extern "C" fn on_primitive_field(
    kind: jvmtiHeapReferenceKind,
    info: *const jvmtiHeapReferenceInfo,
    object_class_tag: jlong,
    object_tag_ptr: *mut jlong,
    value: jvalue,
    value_type: jvmtiPrimitiveType,
    user_data: *mut c_void,
) -> jint {
    // SAFETY: see `on_reference`.
    let w = unsafe { &mut *(user_data as *mut Walk<'_>) };
    w.primitive_field(kind, info, object_class_tag, object_tag_ptr, value, value_type)
}

unsafe extern "C" fn on_array_values(
    class_tag: jlong,
    size: jlong,
    tag_ptr: *mut jlong,
    element_count: jint,
    element_type: jvmtiPrimitiveType,
    elements: *const c_void,
    user_data: *mut c_void,
) -> jint {
    // SAFETY: see `on_reference`.
    let w = unsafe { &mut *(user_data as *mut Walk<'_>) };
    w.array_values(class_tag, size, tag_ptr, element_count, element_type, elements)
}

fn follow(env: Jvmti, walk: &mut Walk<'_>) -> Result<(), String> {
    let mut cb = jvmtiHeapCallbacks { heap_reference_callback: Some(on_reference), ..Default::default() };
    if walk.pass == Pass::Emit {
        cb.primitive_field_callback = Some(on_primitive_field);
        cb.array_primitive_value_callback = Some(on_array_values);
    }
    let user = walk as *mut Walk<'_> as *const c_void;
    let e = jvmti!(env, FollowReferences, 0, ptr::null_mut(), ptr::null_mut(), &cb, user);
    if let Some(err) = walk.err.take() {
        return Err(format!("writing heap dump: {err}"));
    }
    check(e).map_err(|e| format!("FollowReferences failed ({e})"))
}

/// Copies a JVMTI-allocated array and frees it.
fn take_array<T: Copy>(env: Jvmti, p: *mut T, n: jint) -> Vec<T> {
    let v = if p.is_null() || n <= 0 {
        vec![]
    } else {
        // SAFETY: JVMTI returned `n` elements at `p`.
        unsafe { std::slice::from_raw_parts(p, n as usize) }.to_vec()
    };
    env.dealloc(p);
    v
}

fn object_size(env: Jvmti, obj: jobject) -> jlong {
    let mut s = 0;
    jvmti!(env, GetObjectSize, obj, &mut s);
    s
}

/// Tags `obj` if it has no tag yet; returns its tag.
fn pretag(env: Jvmti, ids: &mut Ids, obj: jobject) -> jlong {
    let t = env.get_tag(obj);
    if t != 0 {
        return t;
    }
    let size = object_size(env, obj);
    let t = ids.new_tag(size);
    env.set_tag(obj, t);
    if ids.recorded {
        ids.addr.insert(t, ids.next_addr);
        ids.next_addr += object_bytes(size);
    }
    t
}

/// HPROF class name (`java/lang/String`, `[I`, `[Ljava/lang/String;`) of a class signature.
fn internal_name(sig: &str) -> &str {
    match sig.strip_prefix('L') {
        Some(s) => s.strip_suffix(';').unwrap_or(s),
        None => sig,
    }
}

struct Strings {
    ids: HashMap<String, u64>,
    new: Vec<(u64, String)>,
}

impl Strings {
    fn id(&mut self, s: &str) -> u64 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.ids.len() as u64 + 1;
        self.ids.insert(s.to_string(), id);
        self.new.push((id, s.to_string()));
        id
    }

    fn write(&mut self, w: &mut HprofWriter<BufWriter<File>>) -> io::Result<()> {
        for (id, s) in self.new.drain(..) {
            w.utf8(id, &s)?;
        }
        Ok(())
    }
}

/// Collects all loaded classes, tags their mirrors and computes layouts and field indices.
fn collect_classes(
    env: Jvmti,
    jni: *mut JNIEnv,
    ids: &mut Ids,
    strings: &mut Strings,
) -> Result<Classes, String> {
    let (mut n, mut arr) = (0, ptr::null_mut());
    check(jvmti!(env, GetLoadedClasses, &mut n, &mut arr))
        .map_err(|e| format!("GetLoadedClasses failed ({e})"))?;
    let handles = take_array(env, arr, n);
    let mut list = Vec::with_capacity(handles.len());
    let mut by_tag = HashMap::new();
    for &k in &handles {
        let sig = env.class_signature(k).unwrap_or_default();
        let tag = pretag(env, ids, k);
        let name = strings.id(internal_name(&sig));
        let kind = match sig.as_bytes() {
            [b'[', b'L' | b'[', ..] => Kind::ObjArray,
            [b'[', t] => Kind::PrimArray(hprof_type(*t)),
            _ => Kind::Instance,
        };
        by_tag.insert(tag, list.len() as u32);
        list.push(ClassInfo {
            tag,
            name,
            kind,
            is_interface: false,
            super_tag: 0,
            interfaces: vec![],
            fields: vec![],
            instance: vec![],
            data_len: 0,
            static_base: 0,
        });
    }
    for (c, &k) in list.iter_mut().zip(&handles) {
        let sup = jni!(jni, GetSuperclass, k);
        if !sup.is_null() {
            c.super_tag = env.get_tag(sup);
            jni!(jni, DeleteLocalRef, sup);
        }
        let mut is_if = 0;
        jvmti!(env, IsInterface, k, &mut is_if);
        c.is_interface = is_if != 0;
        let (mut ni, mut ifs) = (0, ptr::null_mut());
        if jvmti!(env, GetImplementedInterfaces, k, &mut ni, &mut ifs) == JVMTI_ERROR_NONE {
            for i in take_array(env, ifs, ni) {
                c.interfaces.push(env.get_tag(i));
                jni!(jni, DeleteLocalRef, i);
            }
        }
        let mut status = 0;
        jvmti!(env, GetClassStatus, k, &mut status);
        if c.kind != Kind::Instance || status & JVMTI_CLASS_STATUS_PREPARED == 0 {
            continue;
        }
        let (mut nf, mut fs) = (0, ptr::null_mut());
        if jvmti!(env, GetClassFields, k, &mut nf, &mut fs) != JVMTI_ERROR_NONE {
            continue;
        }
        for f in take_array(env, fs, nf) {
            let (mut name, mut sig) = (ptr::null_mut(), ptr::null_mut());
            if jvmti!(env, GetFieldName, k, f, &mut name, &mut sig, ptr::null_mut()) != JVMTI_ERROR_NONE {
                // Keep the indices of the remaining fields right.
                c.fields.push(Field { name: 0, ty: hty::INT, is_static: true });
                continue;
            }
            let (name, sig) = (env.take_string(name), env.take_string(sig));
            let mut mods = 0;
            jvmti!(env, GetFieldModifiers, k, f, &mut mods);
            c.fields.push(Field {
                name: strings.id(&name),
                ty: hprof_type(sig.as_bytes().first().copied().unwrap_or(b'L')),
                is_static: mods & ACC_STATIC != 0,
            });
        }
    }
    for k in handles {
        jni!(jni, DeleteLocalRef, k);
    }
    let mut classes = Classes { list, by_tag };
    layouts(&mut classes);
    Ok(classes)
}

/// Instance layouts and JVMTI field indices: interface fields come first (counted, never
/// reported for instances), then the fields of `Object` down to the class's own, statics
/// included. HPROF instance data holds the class's own instance fields first, then its
/// superclass's.
fn layouts(classes: &mut Classes) {
    let n = classes.list.len();
    let chain = |classes: &Classes, i: usize| -> Vec<usize> {
        let mut v = vec![i];
        let mut t = classes.list[i].super_tag;
        while let Some(&s) = classes.by_tag.get(&t) {
            if v.len() > 256 || v.contains(&(s as usize)) {
                break;
            }
            v.push(s as usize);
            t = classes.list[s as usize].super_tag;
        }
        v
    };
    let mut computed = Vec::with_capacity(n);
    for i in 0..n {
        let chain = chain(classes, i);
        // Transitive interfaces of the class and its superclasses.
        let mut seen = HashSet::new();
        let mut stack: Vec<jlong> = chain.iter().flat_map(|&c| classes.list[c].interfaces.clone()).collect();
        let mut iface_fields = 0usize;
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            if let Some(&ic) = classes.by_tag.get(&t) {
                let ic = &classes.list[ic as usize];
                iface_fields += ic.fields.len();
                stack.extend(ic.interfaces.iter().copied());
            }
        }
        let total = iface_fields + chain.iter().map(|&c| classes.list[c].fields.len()).sum::<usize>();
        let own = classes.list[i].fields.len();
        let mut instance = vec![None; total];
        let mut start = total;
        let mut off = 0u32;
        for &k in &chain {
            let fields = &classes.list[k].fields;
            start -= fields.len();
            for (j, f) in fields.iter().enumerate() {
                if !f.is_static {
                    instance[start + j] = Some((off, f.ty));
                    off += type_bytes(f.ty) as u32;
                }
            }
        }
        computed.push((instance, off, (total - own) as u32));
    }
    for (c, (instance, data_len, static_base)) in classes.list.iter_mut().zip(computed) {
        if c.kind == Kind::Instance && !c.is_interface {
            c.instance = instance;
            c.data_len = data_len;
        }
        c.static_base = static_base;
    }
}

/// Writes an HPROF heap dump with the requested dumper (`"native"`, or HotSpot's by default).
pub fn dump_with(
    a: &Agent,
    java: &crate::java::JavaHelpers,
    jni: *mut JNIEnv,
    path: &Path,
    dumper: &str,
    recorded_only: bool,
    live_only: bool,
) -> Result<(), String> {
    match dumper {
        "native" => dump(a, jni, path, recorded_only).map(|_| ()),
        "" | "hotspot" if !recorded_only => java.dump_heap(jni, &path.display().to_string(), live_only),
        "" | "hotspot" => dump(a, jni, path, true).map(|_| ()),
        other => Err(format!("unknown heap dumper {other:?} (hotspot, native)")),
    }
}

/// Writes an HPROF dump of the reachable heap (or of the recorded objects and their paths from
/// the GC roots) to `path`.
pub fn dump(a: &Agent, jni: *mut JNIEnv, path: &Path, recorded_only: bool) -> Result<DumpStats, String> {
    let env = Env::new(a.vm)?;
    let e = env.0;
    let mut ids = Ids::new(recorded_only);
    let mut strings = Strings { ids: HashMap::new(), new: vec![] };
    let mut stats = DumpStats::default();

    // Recorded objects first, so they get the lowest sequence numbers.
    let mut recorded = vec![];
    if recorded_only {
        let guard = RECORDER.read();
        let Some(rec) = *guard else {
            return Err("no recorded objects (allocation recording is off)".into());
        };
        let (mut n, mut objs) = (0, ptr::null_mut());
        check(jvmti!(rec, GetObjectsWithTags, 1, &RECORDED, &mut n, &mut objs, ptr::null_mut()))
            .map_err(|e| format!("GetObjectsWithTags failed ({e})"))?;
        for o in take_array(rec, objs, n) {
            recorded.push(pretag(e, &mut ids, o));
            jni!(jni, DeleteLocalRef, o);
        }
        stats.recorded = recorded.len() as u64;
        if recorded.is_empty() {
            return Err("no recorded objects alive (record allocations first)".into());
        }
    }

    let classes = collect_classes(e, jni, &mut ids, &mut strings)?;
    stats.classes = classes.list.len() as u64;
    // Thread objects need tags before the walk: stack roots name their thread by tag.
    let (mut n, mut threads) = (0, ptr::null_mut());
    if jvmti!(e, GetAllThreads, &mut n, &mut threads) == JVMTI_ERROR_NONE {
        for t in take_array(e, threads, n) {
            pretag(e, &mut ids, t);
            jni!(jni, DeleteLocalRef, t);
        }
    }

    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut w = HprofWriter::new(BufWriter::with_capacity(1 << 20, file), 8, now.as_millis() as u64)
        .map_err(|e| e.to_string())?;
    w.set_segment_limit(16 << 20);
    let io = |e: io::Error| format!("writing heap dump: {e}");
    strings.write(&mut w).map_err(io)?;
    for (serial, c) in classes.list.iter().enumerate() {
        let id = if recorded_only { ids.addr[&c.tag] } else { c.tag as u64 };
        w.load_class(serial as u32 + 1, id, c.name).map_err(io)?;
    }

    let n_classes = classes.list.len();
    let mut walk = Walk {
        pass: if recorded_only { Pass::Parents } else { Pass::Emit },
        ids,
        classes: &classes,
        w: None,
        cur: Cur::None,
        cur_tag: 0,
        pending_len: HashMap::new(),
        statics: classes
            .list
            .iter()
            .map(|c| vec![0; c.fields.iter().filter(|f| f.is_static).count()])
            .collect(),
        constants: vec![vec![]; n_classes],
        refs: vec![[0; 4]; n_classes],
        serials: HashMap::new(),
        compressed: None,
        objects: 0,
        err: None,
    };
    if recorded_only {
        follow(e, &mut walk)?;
        let ids = &mut walk.ids;
        ids.keep = vec![false; ids.next_seq as usize];
        for c in &classes.list {
            ids.keep[c.tag as usize] = true;
        }
        for &r in &recorded {
            let mut s = r as u32;
            while s != 0 && s != UNSET && !ids.keep[s as usize] {
                ids.keep[s as usize] = true;
                s = ids.parent[s as usize];
            }
        }
        ids.parent = vec![];
        walk.pass = Pass::Emit;
    }
    walk.w = Some(w);
    follow(e, &mut walk)?;
    walk.flush();
    let mut w = walk.w.take().expect("writer");
    stats.objects = walk.objects;

    // Classes, with their statics. Constant-pool references go into a synthetic
    // `<resolved_references>` Object[] per class, as in HotSpot's dumps (JVMTI reports them as
    // direct references from the class), so class and array sizes compare.
    let compressed = walk.compressed.unwrap_or(true);
    let object_array = strings
        .ids
        .get("[Ljava/lang/Object;")
        .and_then(|n| classes.list.iter().find(|c| c.name == *n))
        .map_or(0, |c| walk.ids.id(c.tag, 0));
    let resolved = strings.id("<resolved_references>");
    for (ci, c) in classes.list.iter().enumerate() {
        let id = walk.ids.id(c.tag, 0);
        let mut refs = walk.refs[ci];
        if refs[0] == 0 && c.super_tag != 0 {
            refs[0] = walk.ids.id(c.super_tag, 0);
        }
        let mut statics: Vec<(u64, u8, u64)> = c
            .fields
            .iter()
            .filter(|f| f.is_static)
            .zip(&walk.statics[ci])
            .map(|(f, &v)| (f.name, f.ty, v))
            .collect();
        let constants = &mut walk.constants[ci];
        if !constants.is_empty() && object_array != 0 {
            constants.sort_unstable();
            let elems: Vec<u64> = constants.iter().map(|&(_, v)| v).collect();
            let arr = walk.ids.next_addr;
            walk.ids.next_addr +=
                (16 + elems.len() as u64 * if compressed { 4 } else { 8 }).next_multiple_of(8);
            w.obj_array(arr, object_array, &elems).map_err(io)?;
            statics.push((resolved, hty::OBJECT, arr));
        }
        statics.truncate(u16::MAX as usize);
        let fields: Vec<(u64, u8)> =
            c.fields.iter().filter(|f| !f.is_static).map(|f| (f.name, f.ty)).collect();
        w.class_dump_with([id, refs[0], refs[1], refs[2], refs[3]], c.data_len, &statics, &fields)
            .map_err(io)?;
    }
    strings.write(&mut w).map_err(io)?;
    w.hints(&format!(
        "compressedRefs={};dumper=native;recordedOnly={recorded_only}",
        walk.compressed.unwrap_or(true)
    ))
    .map_err(io)?;
    w.finish().map_err(io)?;
    drop(env);
    Ok(stats)
}
