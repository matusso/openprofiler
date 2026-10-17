//! Bytecode instrumentation: exact call counts, probe interceptions and method triggers.
//!
//! The agent's `ClassFileLoadHook` rewrites classes as they load (and again when they are
//! retransformed) with [`classfile`]. What a class gets is decided from:
//!
//! - **call counting**: classes matching the `instrument=` include patterns (and not the
//!   excludes) get `Hooks.callEnter`/`callExit` in every method; see [`calls`];
//! - **probe interceptions** registered by the boot classes' probes (`Hooks.registerInterception0`):
//!   a method of an exact class, or of every subtype of a class or interface;
//! - **custom probes**: methods annotated with `@dev.openprofiler.api.Probe`;
//! - **method triggers**: `MethodCondition`s of the installed triggers.
//!
//! Subtype matching needs the supertypes of a class, which the hook sees only by name. The
//! hook records, per class name, a bit mask of the intercepted types it is a subtype of. A class
//! usually loads before its supertypes (they are resolved while it is defined), so a mask computed
//! in the hook can be incomplete; such classes are re-checked at `ClassPrepare`, when all
//! supertypes have been seen, and retransformed if they turn out to be subtypes after all.
//!
//! Instrumented classes in named modules (the JDK's) get a reads edge to the boot loader's unnamed
//! module, where `Hooks` lives, before their code can run. Classes loaded before the hook was
//! active (bootstrap classes, everything on dynamic attach) are retransformed by the initial scan.
//! A background thread retransforms queued classes and auto-tunes call counting: hot methods
//! whose self time is close to the hook overhead are removed.

pub mod calls;
pub mod classfile;

use crate::jvm::{jni, jvmti};
use crate::{Agent, agent, debug, log};
use classfile::{ClassFile, MethodPlan, ProbeSite};
use jvmti_sys::*;
use openprofiler_model::FrameKind;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Annotation that turns a method into a custom probe.
pub const PROBE_ANNOTATION: &str = "Ldev/openprofiler/api/Probe;";
/// Excluded from call counting even when an include pattern matches.
const ALWAYS_EXCLUDED: &[&str] =
    &["java/", "javax/", "jdk/", "sun/", "com/sun/", "dev/openprofiler/", "kotlin/jvm/internal/"];

/// Whether the hook rewrites anything at all (instrumentation capability granted).
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Classes transformed but not yet prepared (they need module reads).
static UNPREPARED: AtomicUsize = AtomicUsize::new(0);
/// Classes whose subtype mask must be completed at ClassPrepare.
static INCOMPLETE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone)]
pub struct Interception {
    /// Index into [`State::probes`].
    pub probe: u32,
    /// Index of the interception within its probe (passed back to the probe).
    pub index: u32,
    /// Internal name of the class or interface.
    pub type_name: String,
    pub subtypes: bool,
    pub method: String,
    /// Exact descriptor, or `None` for every overload.
    pub desc: Option<String>,
    pub args: bool,
    pub entry: bool,
}

#[derive(Debug, Clone)]
pub enum SiteKind {
    Probe { probe: u32, interception: u32 },
    Custom { probe: u32, payload: String },
}

#[derive(Debug, Clone)]
pub struct Site {
    pub kind: SiteKind,
    pub class: String,
    pub method: String,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub class: String,
    pub name: String,
    pub desc: String,
    pub frame: u32,
}

#[derive(Debug, Clone)]
pub struct ProbeEntry {
    pub id: String,
    /// Collector probe index; `u32::MAX` for internal probes that record nothing.
    pub collector: u32,
}

#[derive(Default)]
struct State {
    include: Vec<String>,
    exclude: Vec<String>,
    /// Enabled probe ids; `None` = all.
    enabled_probes: Option<HashSet<String>>,
    probes: Vec<ProbeEntry>,
    interceptions: Vec<Interception>,
    exact: HashMap<String, Vec<usize>>,
    /// Subtype-intercepted type → bit.
    bits: HashMap<String, u32>,
    by_bit: Vec<Vec<usize>>,
    masks: HashMap<Box<str>, u64>,
    /// Classes whose supertypes were not all known at load: name → direct supertypes.
    incomplete: HashMap<Box<str>, Box<[Box<str>]>>,
    sites: Vec<Site>,
    site_keys: HashMap<(String, String, String, i64), u32>,
    methods: Vec<MethodInfo>,
    method_keys: HashMap<(String, String, String), u32>,
    /// Method triggers: (trigger id, class internal name, method name or `name(desc)` or `*`).
    triggers: Vec<(String, String, String)>,
    trigger_sites: Vec<String>,
    trigger_keys: HashMap<(String, String, String, String), u32>,
    unprepared: HashSet<Box<str>>,
    /// Classes currently carrying injected code.
    instrumented: HashSet<Box<str>>,
    instrumented_methods: usize,
    retransform: HashSet<String>,
    /// Methods removed by auto-tuning: (class, name, desc).
    disabled: HashSet<(String, String, String)>,
    ready: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    f(STATE.lock().get_or_insert_with(State::default))
}

/// `com.acme.*` → `com/acme/*`.
pub fn internal_pattern(p: &str) -> String {
    p.trim().replace('.', "/")
}

/// Glob match with `*` for any run of characters.
pub fn glob(pattern: &str, s: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), s.as_bytes());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

impl State {
    fn counts_calls(&self, class: &str) -> bool {
        !self.include.is_empty()
            && !ALWAYS_EXCLUDED.iter().any(|p| class.starts_with(p))
            && self.include.iter().any(|p| glob(p, class))
            && !self.exclude.iter().any(|p| glob(p, class))
    }

    fn probe_enabled(&self, probe: u32) -> bool {
        let Some(p) = self.probes.get(probe as usize) else { return false };
        match &self.enabled_probes {
            None => true,
            Some(set) => set.contains(&p.id) || set.iter().any(|e| p.id.starts_with(&format!("{e}."))),
        }
    }

    /// Mask of intercepted supertypes reachable through `parents`, and whether all were known.
    fn mask_of(&self, parents: &[Box<str>]) -> (u64, bool) {
        let mut mask = 0;
        let mut complete = true;
        for p in parents {
            if let Some(&b) = self.bits.get(&**p) {
                mask |= 1 << b;
            }
            match self.masks.get(p) {
                Some(&m) => mask |= m,
                None if p.starts_with("java/lang/Object") => {}
                None => complete = false,
            }
        }
        (mask, complete)
    }

    fn method_id(&mut self, class: &str, name: &str, desc: &str) -> u32 {
        let key = (class.to_string(), name.to_string(), desc.to_string());
        if let Some(&id) = self.method_keys.get(&key) {
            return id;
        }
        let frame = agent().map_or(0, |a| {
            let mut c = a.collector.lock();
            let c = &mut *c;
            let m = c.symbols.intern_method(&class.replace('/', "."), name, desc);
            c.symbols.intern_frame(m, -1, FrameKind::Unknown)
        });
        let id = self.methods.len() as u32;
        self.methods.push(MethodInfo { class: class.into(), name: name.into(), desc: desc.into(), frame });
        self.method_keys.insert(key, id);
        id
    }

    fn site_id(&mut self, class: &str, method: &str, desc: &str, key: i64, kind: SiteKind) -> u32 {
        let k = (class.to_string(), method.to_string(), desc.to_string(), key);
        if let Some(&id) = self.site_keys.get(&k) {
            return id;
        }
        let id = self.sites.len() as u32;
        self.sites.push(Site { kind, class: class.replace('/', "."), method: method.into() });
        self.site_keys.insert(k, id);
        id
    }

    fn custom_probe(&mut self, name: &str) -> u32 {
        let id = format!("custom.{name}");
        if let Some(i) = self.probes.iter().position(|p| p.id == id) {
            return i as u32;
        }
        let collector = agent().map_or(u32::MAX, |a| {
            a.collector.lock().add_probe(openprofiler_proto::v1::ProbeInfo {
                id: id.clone(),
                name: name.into(),
                payload_label: "Payload".into(),
                control_label: String::new(),
            })
        });
        self.probes.push(ProbeEntry { id, collector });
        (self.probes.len() - 1) as u32
    }

    /// Decides what to inject into `cf`. Records the class's subtype mask.
    fn plan(&mut self, cf: &ClassFile<'_>) -> Vec<MethodPlan> {
        let mut parents: Vec<Box<str>> = cf.interfaces.iter().map(|s| s.as_str().into()).collect();
        if let Some(s) = &cf.super_name {
            parents.push(s.as_str().into());
        }
        let (mask, complete) = self.mask_of(&parents);
        let name: Box<str> = cf.name.as_str().into();
        if complete {
            if self.incomplete.remove(&name).is_some() {
                INCOMPLETE.fetch_sub(1, Ordering::Relaxed);
            }
        } else if self.incomplete.insert(name.clone(), parents.into_boxed_slice()).is_none() {
            INCOMPLETE.fetch_add(1, Ordering::Relaxed);
        }
        self.masks.insert(name, mask);
        if !self.ready {
            return vec![];
        }

        let class = cf.name.as_str();
        let count_calls = self.counts_calls(class) && !cf.is_interface();
        let mut candidates: Vec<usize> = self.exact.get(class).cloned().unwrap_or_default();
        for (bit, list) in self.by_bit.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                candidates.extend(list);
            }
        }
        candidates.retain(|&i| self.probe_enabled(self.interceptions[i].probe));
        let triggers: Vec<(String, String)> = self
            .triggers
            .iter()
            .filter(|(_, c, _)| glob(c, class))
            .map(|(id, _, m)| (id.clone(), m.clone()))
            .collect();
        let customs = self.enabled_probes.as_ref().is_none_or(|s| s.contains("custom"));

        let mut plans = vec![];
        for (mi, m) in cf.methods.iter().enumerate() {
            if !m.has_code() {
                continue;
            }
            let mut plan = MethodPlan { method: mi, ..Default::default() };
            let synthetic_bridge = m.access & 0x0040 != 0;
            if count_calls
                && !synthetic_bridge
                && m.name != "<clinit>"
                && !self.disabled.contains(&(class.to_string(), m.name.clone(), m.desc.clone()))
            {
                plan.call_id = Some(self.method_id(class, &m.name, &m.desc));
            }
            for &ii in &candidates {
                let it = &self.interceptions[ii];
                if it.method != m.name || it.desc.as_ref().is_some_and(|d| *d != m.desc) {
                    continue;
                }
                let (probe, index, args, entry) = (it.probe, it.index, it.args, it.entry);
                let site = self.site_id(
                    class,
                    &m.name,
                    &m.desc,
                    ii as i64,
                    SiteKind::Probe { probe, interception: index },
                );
                plan.probe = Some(ProbeSite { site, args, entry });
                break;
            }
            if plan.probe.is_none() && customs {
                for an in cf.method_annotations(m) {
                    if an.desc != PROBE_ANNOTATION {
                        continue;
                    }
                    let pname = an.value("value").filter(|v| !v.is_empty()).unwrap_or("custom").to_string();
                    let payload = match an.value("payload") {
                        Some(p) if !p.is_empty() => p.to_string(),
                        _ => format!("{}.{}", class.rsplit('/').next().unwrap_or(class), m.name),
                    };
                    let probe = self.custom_probe(&pname);
                    let site = self.site_id(
                        class,
                        &m.name,
                        &m.desc,
                        -1,
                        SiteKind::Custom { probe, payload: payload.clone() },
                    );
                    plan.probe = Some(ProbeSite { site, args: payload.contains('{'), entry: true });
                }
            }
            for (tid, spec) in &triggers {
                let matches = spec == "*"
                    || *spec == m.name
                    || spec.strip_prefix(m.name.as_str()).is_some_and(|d| d == m.desc);
                if !matches {
                    continue;
                }
                let key = (class.to_string(), m.name.clone(), m.desc.clone(), tid.clone());
                let next = self.trigger_sites.len() as u32;
                let id = *self.trigger_keys.entry(key).or_insert(next);
                if id == next {
                    self.trigger_sites.push(tid.clone());
                }
                plan.trigger = Some(id);
                break;
            }
            if plan.call_id.is_some() || plan.probe.is_some() || plan.trigger.is_some() {
                plans.push(plan);
            }
        }
        plans
    }
}

/// Transforms one class. `None` leaves it unchanged.
fn transform(bytes: &[u8]) -> Option<Vec<u8>> {
    let cf = ClassFile::parse(bytes).ok()?;
    if cf.name.starts_with("dev/openprofiler/") {
        return None;
    }
    let mut guard = STATE.lock();
    let st = guard.get_or_insert_with(State::default);
    let plans = st.plan(&cf);
    let name: Box<str> = cf.name.as_str().into();
    if plans.is_empty() {
        st.instrumented.remove(&name);
        return None;
    }
    match classfile::rewrite(&cf, &plans) {
        Ok(Some(r)) => {
            for (mi, why) in &r.skipped {
                debug(&format!("not instrumenting {}.{}: {why}", cf.name, cf.methods[*mi].name));
            }
            if r.applied.is_empty() {
                st.instrumented.remove(&name);
                return None;
            }
            st.instrumented_methods += r.applied.len();
            debug(&format!("instrumented {} ({} methods)", cf.name, r.applied.len()));
            st.instrumented.insert(name.clone());
            if st.unprepared.insert(name) {
                UNPREPARED.fetch_add(1, Ordering::Relaxed);
            }
            Some(r.bytes)
        }
        Ok(None) => None,
        Err(e) => {
            debug(&format!("cannot instrument {}: {e}", cf.name));
            None
        }
    }
}

/// JVMTI `ClassFileLoadHook`.
pub unsafe extern "C" fn class_file_load_hook(
    jvmti_env: *mut jvmtiEnv,
    _jni: *mut JNIEnv,
    _class_being_redefined: jclass,
    _loader: jobject,
    name: *const c_char,
    _protection_domain: jobject,
    len: jint,
    data: *const u8,
    new_len: *mut jint,
    new_data: *mut *mut u8,
) {
    if !ENABLED.load(Ordering::Relaxed) || name.is_null() || data.is_null() || len <= 0 {
        return;
    }
    // SAFETY: the VM passes a NUL-terminated class name.
    let n = unsafe { CStr::from_ptr(name) }.to_bytes();
    if n.starts_with(b"dev/openprofiler/") || n.windows(9).any(|w| w == b"$$Lambda$") {
        return;
    }
    // SAFETY: `data` holds `len` bytes of class file for the duration of the callback.
    let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
    let Some(out) = transform(bytes) else { return };
    let mut mem: *mut u8 = ptr::null_mut();
    // SAFETY: `jvmti_env` is the callback's environment; Allocate writes a pointer to `mem`.
    let r = unsafe { ((**jvmti_env).Allocate.expect("Allocate"))(jvmti_env, out.len() as jlong, &mut mem) };
    if r != JVMTI_ERROR_NONE || mem.is_null() {
        return;
    }
    // SAFETY: `mem` has room for `out.len()` bytes; the VM takes ownership of it.
    unsafe {
        ptr::copy_nonoverlapping(out.as_ptr(), mem, out.len());
        *new_len = out.len() as jint;
        *new_data = mem;
    }
}

/// `ClassPrepare` work: module reads for transformed classes and completion of subtype masks.
pub fn on_class_prepare(a: &Agent, jni: *mut JNIEnv, klass: jclass) {
    if UNPREPARED.load(Ordering::Relaxed) == 0 && INCOMPLETE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Ok(sig) = a.jvmti.class_signature(klass) else { return };
    let Some(name) = sig.strip_prefix('L').and_then(|s| s.strip_suffix(';')) else { return };
    let (needs_reads, retransform) = with_state(|st| {
        let needs_reads = st.unprepared.remove(name);
        if needs_reads {
            UNPREPARED.fetch_sub(1, Ordering::Relaxed);
        }
        let mut retransform = false;
        if let Some(parents) = st.incomplete.remove(name) {
            INCOMPLETE.fetch_sub(1, Ordering::Relaxed);
            let (mask, _) = st.mask_of(&parents);
            let old = st.masks.insert(name.into(), mask).unwrap_or(0);
            if mask & !old != 0 && st.ready {
                retransform = true;
            }
        }
        (needs_reads, retransform)
    });
    if needs_reads {
        add_reads(a, jni, klass);
    }
    if retransform {
        with_state(|st| st.retransform.insert(name.to_string()));
    }
}

/// Lets the module of `klass` read the boot loader's unnamed module (where `Hooks` lives).
fn add_reads(a: &Agent, jni: *mut JNIEnv, klass: jclass) {
    let Some(ids) = MODULE_IDS.get() else { return };
    let module = jni!(jni, CallObjectMethodA, klass, ids.get_module, ptr::null());
    if module.is_null() {
        jni!(jni, ExceptionClear);
        return;
    }
    if jni!(jni, IsSameObject, module, ids.boot_unnamed) == 0 {
        jvmti!(a.jvmti, AddModuleReads, module, ids.boot_unnamed);
    }
    jni!(jni, DeleteLocalRef, module);
}

struct ModuleIds {
    get_module: jmethodID,
    boot_unnamed: jobject,
}

// SAFETY: a method id and a global reference, valid on every attached thread.
unsafe impl Send for ModuleIds {}
// SAFETY: see above.
unsafe impl Sync for ModuleIds {}

static MODULE_IDS: std::sync::OnceLock<ModuleIds> = std::sync::OnceLock::new();

/// Capabilities and the hook are set up at load; returns whether instrumentation is possible.
pub fn install(a: &Agent) -> bool {
    let caps = a.caps.lock().iter().any(|c| c == "instrument");
    if !caps {
        return false;
    }
    with_state(|st| {
        st.include = a.opts.instrument.iter().map(|p| internal_pattern(p)).collect();
        st.exclude = a.opts.instrument_exclude.iter().map(|p| internal_pattern(p)).collect();
        st.enabled_probes = a.opts.probes.as_ref().map(|v| v.iter().cloned().collect());
    });
    ENABLED.store(true, Ordering::SeqCst);
    let _ = a.jvmti.enable_event(JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, true);
    let _ = a.jvmti.enable_event(JVMTI_EVENT_CLASS_PREPARE, true);
    true
}

/// Resolves what module reads need; called once the boot classes are defined.
pub fn init_live(jni: *mut JNIEnv) {
    let class_class = jni!(jni, FindClass, c"java/lang/Class".as_ptr());
    let hooks = jni!(jni, FindClass, c"dev/openprofiler/boot/Hooks".as_ptr());
    if class_class.is_null() || hooks.is_null() {
        jni!(jni, ExceptionClear);
        ENABLED.store(false, Ordering::SeqCst);
        return;
    }
    let get_module =
        jni!(jni, GetMethodID, class_class, c"getModule".as_ptr(), c"()Ljava/lang/Module;".as_ptr());
    if get_module.is_null() {
        jni!(jni, ExceptionClear);
        return;
    }
    let m = jni!(jni, CallObjectMethodA, hooks, get_module, ptr::null());
    if !m.is_null() {
        let boot_unnamed = jni!(jni, NewGlobalRef, m);
        let _ = MODULE_IDS.set(ModuleIds { get_module, boot_unnamed });
        jni!(jni, DeleteLocalRef, m);
    }
    jni!(jni, DeleteLocalRef, class_class);
    jni!(jni, DeleteLocalRef, hooks);
}

// ---------------------------------------------------------------------------------------------
// Registration (from the boot classes' probes)

pub fn register_probe(id: &str, name: &str, payload_label: &str, control_label: &str, visible: bool) -> u32 {
    with_state(|st| {
        if let Some(i) = st.probes.iter().position(|p| p.id == id) {
            return i as u32;
        }
        let collector = match (visible, agent()) {
            (true, Some(a)) => a.collector.lock().add_probe(openprofiler_proto::v1::ProbeInfo {
                id: id.into(),
                name: name.into(),
                payload_label: payload_label.into(),
                control_label: control_label.into(),
            }),
            _ => u32::MAX,
        };
        st.probes.push(ProbeEntry { id: id.into(), collector });
        (st.probes.len() - 1) as u32
    })
}

pub fn register_interception(it: Interception) {
    with_state(|st| {
        let idx = st.interceptions.len();
        if it.subtypes {
            let next = st.bits.len() as u32;
            if next >= 64 && !st.bits.contains_key(&it.type_name) {
                log(&format!("too many intercepted types; ignoring {}", it.type_name));
                return;
            }
            let bit = *st.bits.entry(it.type_name.clone()).or_insert(next);
            if st.by_bit.len() <= bit as usize {
                st.by_bit.resize(bit as usize + 1, vec![]);
            }
            st.by_bit[bit as usize].push(idx);
        } else {
            st.exact.entry(it.type_name.clone()).or_default().push(idx);
        }
        st.interceptions.push(it);
    });
}

/// `(collector probe index, interception)` of a probe site, or the custom payload template.
pub fn site(site: u32) -> Option<(u32, Option<u32>, Option<String>)> {
    with_state(|st| {
        let s = st.sites.get(site as usize)?;
        Some(match &s.kind {
            SiteKind::Probe { probe, interception } => (*probe, Some(*interception), None),
            SiteKind::Custom { probe, payload } => (*probe, None, Some(payload.clone())),
        })
    })
}

/// Collector index of probe `probe` (registration index).
pub fn collector_probe(probe: u32) -> u32 {
    with_state(|st| st.probes.get(probe as usize).map_or(u32::MAX, |p| p.collector))
}

pub fn method_frame(id: u32) -> Option<u32> {
    with_state(|st| st.methods.get(id as usize).map(|m| m.frame))
}

pub fn trigger_of_site(site: u32) -> Option<String> {
    with_state(|st| st.trigger_sites.get(site as usize).cloned())
}

/// Installs the method conditions of a trigger set; retransforms the affected classes.
pub fn set_method_triggers(list: Vec<(String, String, String)>) {
    with_state(|st| {
        let classes: Vec<String> = st.triggers.iter().chain(list.iter()).map(|(_, c, _)| c.clone()).collect();
        st.triggers = list;
        st.retransform.extend(classes.into_iter().map(|c| format!("glob:{c}")));
    });
}

/// Replaces include/exclude patterns and enabled probes; retransforms what changes.
pub fn configure(include: Vec<String>, exclude: Vec<String>, probes: Option<Vec<String>>) {
    with_state(|st| {
        st.include = include.iter().map(|p| internal_pattern(p)).collect();
        st.exclude = exclude.iter().map(|p| internal_pattern(p)).collect();
        st.enabled_probes = probes.map(|v| v.into_iter().collect());
        st.retransform.insert("scan:".into());
    });
}

pub struct Info {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub probes: Vec<String>,
    pub enabled_probes: Vec<String>,
    pub classes: usize,
    pub methods: usize,
    pub disabled: Vec<String>,
}

pub fn info() -> Info {
    with_state(|st| {
        let java = |p: &String| p.replace('/', ".");
        let visible: Vec<String> =
            st.probes.iter().filter(|p| p.collector != u32::MAX).map(|p| p.id.clone()).collect();
        Info {
            include: st.include.iter().map(java).collect(),
            exclude: st.exclude.iter().map(java).collect(),
            enabled_probes: st
                .probes
                .iter()
                .enumerate()
                .filter(|(i, _)| st.probe_enabled(*i as u32))
                .map(|(_, p)| p.id.clone())
                .collect(),
            probes: visible,
            classes: st.instrumented.len(),
            methods: st.instrumented_methods,
            disabled: st.disabled.iter().map(|(c, n, _)| format!("{}.{n}", c.replace('/', "."))).collect(),
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Retransformation

/// Called when the boot classes registered their probes: instruments already-loaded classes and
/// starts the background thread.
pub fn ready(a: &'static Agent, jni: *mut JNIEnv) {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    with_state(|st| st.ready = true);
    scan(a, jni);
    let r = std::thread::Builder::new().name("openprofiler-instrument".into()).spawn(move || run(a));
    if let Err(e) = r {
        log(&format!("cannot start instrumentation thread: {e}"));
    }
}

fn run(a: &'static Agent) {
    crate::own_thread();
    let Some(jni) = a.vm.attach_daemon("OpenProfiler Instrumentation") else { return };
    let mut last_tune = Instant::now();
    while !a.shutting_down.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        let queue = with_state(|st| std::mem::take(&mut st.retransform));
        if queue.contains("scan:") {
            scan(a, jni);
        } else if !queue.is_empty() {
            retransform_matching(a, jni, |name| {
                queue.iter().any(|q| match q.strip_prefix("glob:") {
                    Some(g) => glob(g, name),
                    None => q == name,
                })
            });
        }
        if a.opts.autotune && last_tune.elapsed() >= calls::TUNE_WINDOW {
            last_tune = Instant::now();
            let hot = calls::take_tuning_candidates();
            if !hot.is_empty() {
                let removed: Vec<String> = with_state(|st| {
                    hot.iter()
                        .filter_map(|&id| st.methods.get(id as usize).cloned())
                        .map(|m| {
                            st.disabled.insert((m.class.clone(), m.name.clone(), m.desc.clone()));
                            m.class
                        })
                        .collect()
                });
                debug(&format!("auto-tuning removed {} hot method(s)", hot.len()));
                retransform_matching(a, jni, |n| removed.iter().any(|c| c == n));
            }
        }
    }
}

/// Retransforms every loaded class that matches the current settings or carries injected code.
fn scan(a: &Agent, jni: *mut JNIEnv) {
    jni!(jni, PushLocalFrame, 64);
    // Subtype masks of classes loaded before the hook saw them, from the loaded target types.
    let loaded = loaded_classes(a);
    let targets: Vec<(u32, jclass)> = with_state(|st| {
        loaded.iter().filter_map(|(n, k)| st.bits.get(n.as_str()).map(|&b| (b, *k))).collect::<Vec<_>>()
    });
    let mut masks = vec![];
    for (name, k) in &loaded {
        let mut m = 0u64;
        for &(b, t) in &targets {
            if *k != t && jni!(jni, IsAssignableFrom, *k, t) != 0 {
                m |= 1 << b;
            }
        }
        masks.push((name.clone(), m));
    }
    let wanted: HashSet<String> = with_state(|st| {
        for (n, m) in &masks {
            let e = st.masks.entry(n.as_str().into()).or_insert(0);
            *e |= m;
        }
        masks
            .iter()
            .filter(|(n, m)| {
                let n = n.as_str();
                st.instrumented.contains(n)
                    || st.counts_calls(n)
                    || st
                        .exact
                        .get(n)
                        .is_some_and(|l| l.iter().any(|&i| st.probe_enabled(st.interceptions[i].probe)))
                    || st.by_bit.iter().enumerate().any(|(b, l)| {
                        m & (1 << b) != 0 && l.iter().any(|&i| st.probe_enabled(st.interceptions[i].probe))
                    })
                    || st.triggers.iter().any(|(_, c, _)| glob(c, n))
            })
            .map(|(n, _)| n.clone())
            .collect()
    });
    jni!(jni, PopLocalFrame, ptr::null_mut());
    retransform_matching(a, jni, |n| wanted.contains(n));
}

/// Loaded classes as `(internal name, local ref)`; callers release the references with a local
/// frame. Arrays and primitives are skipped.
fn loaded_classes(a: &Agent) -> Vec<(String, jclass)> {
    let mut n = 0;
    let mut classes: *mut jclass = ptr::null_mut();
    if jvmti!(a.jvmti, GetLoadedClasses, &mut n, &mut classes) != JVMTI_ERROR_NONE || classes.is_null() {
        return vec![];
    }
    // SAFETY: JVMTI returned `n` class references.
    let list = unsafe { std::slice::from_raw_parts(classes, n.max(0) as usize) }.to_vec();
    a.jvmti.dealloc(classes);
    list.into_iter()
        .filter_map(|k| {
            let sig = a.jvmti.class_signature(k).ok()?;
            let name = sig.strip_prefix('L')?.strip_suffix(';')?.to_string();
            Some((name, k))
        })
        .collect()
}

/// Retransforms loaded classes whose internal name satisfies `pred`, adding module reads first.
fn retransform_matching(a: &Agent, jni: *mut JNIEnv, pred: impl Fn(&str) -> bool) {
    jni!(jni, PushLocalFrame, 64);
    let loaded = loaded_classes(a);
    let mut batch: Vec<jclass> = vec![];
    for (name, k) in &loaded {
        if name.starts_with("dev/openprofiler/") || name.contains("$$Lambda") || !pred(name) {
            continue;
        }
        let mut modifiable: jboolean = 0;
        if jvmti!(a.jvmti, IsModifiableClass, *k, &mut modifiable) != JVMTI_ERROR_NONE || modifiable == 0 {
            continue;
        }
        add_reads(a, jni, *k);
        batch.push(*k);
    }
    if !batch.is_empty() {
        let r = jvmti!(a.jvmti, RetransformClasses, batch.len() as jint, batch.as_ptr());
        if r != JVMTI_ERROR_NONE {
            // One bad class fails the batch; retry one by one.
            let mut failed = 0;
            for k in &batch {
                if jvmti!(a.jvmti, RetransformClasses, 1, k) != JVMTI_ERROR_NONE {
                    failed += 1;
                }
            }
            if failed > 0 {
                debug(&format!("retransform failed for {failed} class(es)"));
            }
        }
        debug(&format!("retransformed {} class(es)", batch.len()));
    }
    // The local refs from GetLoadedClasses are released with the frame.
    jni!(jni, PopLocalFrame, ptr::null_mut());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("com/acme/*", "com/acme/shop/Cart"));
        assert!(glob("*Service", "com/acme/OrderService"));
        assert!(!glob("com/acme/*", "com/acmex/Foo"));
        assert!(glob("com/acme/Foo", "com/acme/Foo"));
        assert!(!glob("com/acme/Foo", "com/acme/Foo$1"));
        assert_eq!(internal_pattern("com.acme.*"), "com/acme/*");
    }

    #[test]
    fn plans_subtypes_and_calls() {
        let mut st = State { ready: true, include: vec!["app/*".into()], ..Default::default() };
        st.probes.push(ProbeEntry { id: "jdbc".into(), collector: 0 });
        let it = Interception {
            probe: 0,
            index: 3,
            type_name: "java/sql/Statement".into(),
            subtypes: true,
            method: "run".into(),
            desc: None,
            args: true,
            entry: true,
        };
        st.bits.insert(it.type_name.clone(), 0);
        st.by_bit.push(vec![0]);
        st.interceptions.push(it);
        st.masks.insert("app/Base".into(), 1);
        // A minimal class `app/Impl extends app/Base` with `void run()` and `void other()`.
        let bytes = test_class("app/Impl", "app/Base", &["run", "other"]);
        let cf = ClassFile::parse(&bytes).unwrap();
        let plans = st.plan(&cf);
        assert_eq!(plans.len(), 2);
        assert!(plans[0].probe.is_some() && plans[0].call_id.is_some());
        assert!(plans[1].probe.is_none() && plans[1].call_id.is_some());
        assert!(matches!(st.sites[0].kind, SiteKind::Probe { probe: 0, interception: 3 }));
        // Unknown supertype: incomplete until ClassPrepare.
        let bytes = test_class("other/X", "other/Unseen", &["run"]);
        st.plan(&ClassFile::parse(&bytes).unwrap());
        assert!(st.incomplete.contains_key("other/X"));
    }

    /// Builds a class with `void name() { return; }` methods.
    fn test_class(name: &str, sup: &str, methods: &[&str]) -> Vec<u8> {
        let mut cp: Vec<Vec<u8>> = vec![];
        let utf = |s: &str, cp: &mut Vec<Vec<u8>>| {
            let mut e = vec![1];
            e.extend_from_slice(&(s.len() as u16).to_be_bytes());
            e.extend_from_slice(s.as_bytes());
            cp.push(e);
            cp.len() as u16
        };
        let n = utf(name, &mut cp);
        cp.push(vec![7, (n >> 8) as u8, n as u8]);
        let this = cp.len() as u16;
        let s = utf(sup, &mut cp);
        cp.push(vec![7, (s >> 8) as u8, s as u8]);
        let sup_i = cp.len() as u16;
        let code = utf("Code", &mut cp);
        let v = utf("()V", &mut cp);
        let names: Vec<u16> = methods.iter().map(|m| utf(m, &mut cp)).collect();
        let mut b = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52];
        b.extend_from_slice(&((cp.len() + 1) as u16).to_be_bytes());
        for e in &cp {
            b.extend_from_slice(e);
        }
        for x in [0x21u16, this, sup_i, 0, 0, methods.len() as u16] {
            b.extend_from_slice(&x.to_be_bytes());
        }
        for &mn in &names {
            for x in [1u16, mn, v, 1, code] {
                b.extend_from_slice(&x.to_be_bytes());
            }
            b.extend_from_slice(&13u32.to_be_bytes());
            b.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 1, 0xb1, 0, 0, 0, 0]);
        }
        b.extend_from_slice(&[0, 0]);
        b
    }
}
