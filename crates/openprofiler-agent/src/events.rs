//! JVMTI event callbacks and agent start-up once the VM is live.

use crate::collector::{MonitorRow, now_nanos};
use crate::jvm::{JResult, jni, jvmti};
use crate::{Agent, Mode, agent, debug, log};
use jvmti_sys::*;
use openprofiler_model::{FrameKind, MonitorEventKind, SampleKind, ThreadState};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::Ordering;

/// Tag value for the agent's own threads (never sampled).
const INTERNAL_TAG: jlong = -1;

static REGISTER: Mutex<()> = Mutex::new(());
/// Fallback thread registry when object tagging is unavailable: Java thread id → index.
static BY_JAVA_ID: Mutex<Option<HashMap<u64, u32>>> = Mutex::new(None);

/// An in-flight monitor enter or wait. JVMTI posts both ends on the waiting thread, so it is kept
/// in a thread-local. Contention can be very frequent, so stacks are only taken for events that
/// are recorded (see [`monitor_end`]).
struct PendingMonitor {
    start: i64,
    kind: MonitorEventKind,
    owner: Option<u32>,
    /// Frames and monitor class, taken at the start when this event completes a sample.
    sample: Option<(Vec<jvmtiFrameInfo>, String)>,
    /// [`CONTENTION_EPOCH`] at the start; an event that started before contended-enter events
    /// were last switched on has no matching start.
    epoch: u32,
}

thread_local! {
    static MONITOR_PENDING: std::cell::Cell<Option<PendingMonitor>> = const { std::cell::Cell::new(None) };
    /// Blocked (0) and waiting (1) time of short monitor events not yet sampled.
    static MONITOR_UNSAMPLED: std::cell::Cell<[i64; 2]> = const { std::cell::Cell::new([0, 0]) };
}

/// Long monitor events (and long parks) recorded individually per second.
const LONG_MONITOR_EVENTS: u64 = 1000;
static LONG_BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Stacks taken per second at the start of contended enters and waits that may turn out long.
const SPECULATIVE_STACKS: u64 = 5000;
/// Stacks taken per second for samples of sampled time (threads mostly parked or waiting would
/// otherwise take one per `monitor_interval` each); time beyond carries over to the next sample.
const SAMPLE_STACKS: u64 = 5000;
static SAMPLE_BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SPECULATIVE_BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Takes one unit of a per-second budget kept as `second << 32 | used`; a lost race takes none.
fn take_budget(budget: &std::sync::atomic::AtomicU64, per_second: u64) -> bool {
    let second = (now_nanos() / 1_000_000_000) as u64 & 0xffff_ffff;
    let cur = budget.load(Ordering::Relaxed);
    let next = if cur >> 32 != second {
        second << 32 | 1
    } else if cur & 0xffff_ffff >= per_second {
        return false;
    } else {
        cur + 1
    };
    budget.compare_exchange(cur, next, Ordering::Relaxed, Ordering::Relaxed).is_ok()
}

fn slot(kind: MonitorEventKind) -> usize {
    usize::from(kind != MonitorEventKind::Enter)
}

/// The current thread's frames and the class of `object`.
fn monitor_site(a: &Agent, jni: *mut JNIEnv, object: jobject) -> Option<(Vec<jvmtiFrameInfo>, String)> {
    let max = a.opts.max_frames.min(512) as usize;
    let mut frames = vec![jvmtiFrameInfo { method: ptr::null_mut(), location: 0 }; max];
    let mut count = 0;
    if jvmti!(a.jvmti, GetStackTrace, ptr::null_mut(), 0, max as jint, frames.as_mut_ptr(), &mut count)
        != JVMTI_ERROR_NONE
    {
        return None;
    }
    frames.truncate(count.max(0) as usize);
    let cls = jni!(jni, GetObjectClass, object);
    let class =
        a.jvmti.class_signature(cls).map(|s| crate::resolve::signature_to_class(&s)).unwrap_or_default();
    jni!(jni, DeleteLocalRef, cls);
    Some((frames, class))
}

/// Owner lookups used in the current second: `(second, count)`.
static OWNER_BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Owner lookups are possible and enabled (`monitor_usage` capability, `owner_rate` > 0).
static OWNER_LOOKUPS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn install(a: &Agent) -> JResult<()> {
    let cb = jvmtiEventCallbacks {
        VMInit: Some(vm_init),
        VMDeath: Some(vm_death),
        ThreadStart: Some(thread_start),
        ThreadEnd: Some(thread_end),
        ClassLoad: Some(class_load),
        ClassPrepare: Some(class_prepare),
        SampledObjectAlloc: Some(sampled_object_alloc),
        MonitorContendedEnter: Some(monitor_contended_enter),
        MonitorContendedEntered: Some(monitor_contended_entered),
        MonitorWait: Some(monitor_wait),
        MonitorWaited: Some(monitor_waited),
        ResourceExhausted: Some(resource_exhausted),
        VirtualThreadEnd: Some(crate::vthreads::virtual_thread_end),
        ClassFileLoadHook: Some(crate::instrument::class_file_load_hook),
        ..Default::default()
    };
    a.jvmti.set_callbacks(&cb)?;
    let j = a.jvmti;
    j.enable_event(JVMTI_EVENT_VM_INIT, true)?;
    j.enable_event(JVMTI_EVENT_VM_DEATH, true)?;
    j.enable_event(JVMTI_EVENT_THREAD_START, true)?;
    j.enable_event(JVMTI_EVENT_THREAD_END, true)?;
    if cfg!(target_os = "linux") && a.opts.sampler != crate::SamplerChoice::Sync {
        // AsyncGetCallTrace needs jmethodIDs to exist before a method is sampled, and HotSpot's
        // implementation refuses to walk stacks (`ticks_no_class_load`) unless ClassLoad events
        // are enabled.
        let _ = j.enable_event(JVMTI_EVENT_CLASS_LOAD, true);
        let _ = j.enable_event(JVMTI_EVENT_CLASS_PREPARE, true);
    }
    if has_cap(a, "alloc") {
        jvmti!(j, SetHeapSamplingInterval, a.opts.alloc_interval as jint);
        let _ = j.enable_event(JVMTI_EVENT_SAMPLED_OBJECT_ALLOC, a.alloc_on.load(Ordering::SeqCst));
    }
    if has_cap(a, "monitor") {
        OWNER_LOOKUPS.store(has_cap(a, "monitor_usage") && a.opts.owner_rate > 0, Ordering::Relaxed);
        set_monitor_events(a, a.monitor_on.load(Ordering::SeqCst));
    }
    if has_cap(a, "oom") {
        let _ = j.enable_event(JVMTI_EVENT_RESOURCE_EXHAUSTED, true);
    }
    if has_cap(a, "virtual_threads") && !crate::vthreads::install(a) {
        a.caps.lock().retain(|c| c != "virtual_threads");
    }
    if !crate::instrument::install(a) {
        a.caps.lock().retain(|c| !matches!(c.as_str(), "instrument" | "all_class_hooks" | "retransform_any"));
    }
    Ok(())
}

/// Whether threads are tagged with their index (`tagging` capability); cached, as it is asked
/// for on every event.
fn tagging(a: &Agent) -> bool {
    static TAGGING: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    match TAGGING.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let t = has_cap(a, "tagging");
            TAGGING.store(if t { 1 } else { 2 }, Ordering::Relaxed);
            t
        }
    }
}

pub fn has_cap(a: &Agent, name: &str) -> bool {
    a.caps.lock().iter().any(|c| c == name)
}

fn set_monitor_events(a: &Agent, on: bool) {
    *CYCLE.lock() = Cycle::default();
    CONTENTION_SCALE.store(1, Ordering::Relaxed);
    CONTENTION_EPOCH.fetch_add(1, Ordering::Relaxed);
    for ev in [
        JVMTI_EVENT_MONITOR_CONTENDED_ENTER,
        JVMTI_EVENT_MONITOR_CONTENDED_ENTERED,
        JVMTI_EVENT_MONITOR_WAIT,
        JVMTI_EVENT_MONITOR_WAITED,
    ] {
        let _ = a.jvmti.enable_event(ev, on);
    }
}

/// Contended enters per second above which contended-enter events are duty-cycled.
const HOT_CONTENTION: u64 = 20_000;
/// Duty cycle under hot contention: events on for `CYCLE_ON`, off for `CYCLE_OFF`.
const CYCLE_ON: i64 = 1_000_000_000;
const CYCLE_OFF: i64 = 4_000_000_000;

/// Contended enters seen (monitor events on).
static CONTENDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Weight factor of contended-enter samples while duty-cycling (1 otherwise).
static CONTENTION_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
static CONTENTION_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[derive(Default)]
struct Cycle {
    /// Start of the current measurement window or phase, and the count at its start.
    since: i64,
    count: u64,
    cycling: bool,
    off: bool,
}

static CYCLE: Mutex<Cycle> = Mutex::new(Cycle { since: 0, count: 0, cycling: false, off: false });

/// Called by the sampler thread every wall tick. Even when the agent does nothing in its
/// callbacks, JVMTI's delivery of contended-enter events costs a few percent of throughput when a
/// monitor is contended tens of thousands of times a second. Above [`HOT_CONTENTION`] the events
/// are therefore switched on for one second in five, and the contended-enter samples recorded
/// meanwhile weigh five times their duration. Monitor waits are not affected.
pub fn regulate_contention(a: &Agent) {
    if !a.monitor_on.load(Ordering::Relaxed) || !has_cap(a, "monitor") {
        return;
    }
    let now = now_nanos();
    let count = CONTENDED.load(Ordering::Relaxed);
    let mut c = CYCLE.lock();
    if c.since == 0 {
        (c.since, c.count) = (now, count);
        return;
    }
    let elapsed = now - c.since;
    let enters = |on: bool| {
        for ev in [JVMTI_EVENT_MONITOR_CONTENDED_ENTER, JVMTI_EVENT_MONITOR_CONTENDED_ENTERED] {
            let _ = a.jvmti.enable_event(ev, on);
        }
    };
    if c.off {
        if elapsed >= CYCLE_OFF {
            CONTENTION_EPOCH.fetch_add(1, Ordering::Relaxed);
            enters(true);
            (c.off, c.since, c.count) = (false, now, count);
        }
        return;
    }
    let window = if c.cycling { CYCLE_ON } else { 1_000_000_000 };
    if elapsed < window {
        return;
    }
    let rate = (count - c.count) * 1_000_000_000 / elapsed.max(1) as u64;
    (c.since, c.count) = (now, count);
    if rate > HOT_CONTENTION {
        if !c.cycling {
            debug(&format!("{rate} contended enters/s: sampling contention one second in five"));
        }
        c.cycling = true;
        c.off = true;
        CONTENTION_SCALE.store(((CYCLE_ON + CYCLE_OFF) / CYCLE_ON) as u32, Ordering::Relaxed);
        enters(false);
    } else if c.cycling {
        c.cycling = false;
        CONTENTION_SCALE.store(1, Ordering::Relaxed);
    }
}

/// Turns a recording kind on or off (`cpu`, `alloc`, `monitor`); `interval_us` > 0 also changes
/// the CPU sampling interval.
pub fn set_recording(a: &Agent, kind: &str, on: bool, interval_us: u32) {
    match kind {
        "cpu" => {
            a.cpu_on.store(on, Ordering::SeqCst);
            if interval_us > 0 {
                a.interval_ns.store(interval_us as u64 * 1000, Ordering::SeqCst);
            }
        }
        "alloc" if has_cap(a, "alloc") => {
            if on && !a.alloc_on.swap(true, Ordering::SeqCst) {
                crate::heapdump::reset_recorded(a.vm);
            }
            a.alloc_on.store(on, Ordering::SeqCst);
            let _ = a.jvmti.enable_event(JVMTI_EVENT_SAMPLED_OBJECT_ALLOC, on);
        }
        "monitor" if has_cap(a, "monitor") => {
            a.monitor_on.store(on, Ordering::SeqCst);
            set_monitor_events(a, on);
        }
        "calls" => crate::instrument::calls::ON.store(on, Ordering::SeqCst),
        "probes" => crate::probes::ON.store(on, Ordering::SeqCst),
        _ => debug(&format!("ignoring recording toggle {kind}")),
    }
}

/// Starts everything that needs a live VM. Idempotent.
pub fn on_live(a: &'static Agent, jni: *mut JNIEnv) {
    if a.live_started.swap(true, Ordering::SeqCst) {
        return;
    }
    match crate::java::init(jni) {
        Ok(h) => {
            if has_cap(a, "instrument") {
                crate::instrument::init_live(jni);
                if crate::probes::bind(jni) {
                    a.caps.lock().push("probes".into());
                }
            }
            *a.jvm_info.lock() = h.jvm_info(jni);
            let _ = a.java.set(h);
            if let Some(h) = a.java.get() {
                h.start(jni);
            }
        }
        Err(e) => {
            log(&format!("boot classes unavailable: {e}"));
            a.jvm_info.lock().pid = std::process::id();
        }
    }
    preload_methods(a);
    // Installed even with monitors off so `SetRecording monitor` works later; the hook passes
    // straight through while monitor recording is off.
    if crate::park::install(a, jni) {
        a.caps.lock().push("park".into());
    }
    #[cfg(target_os = "linux")]
    crate::sampler_async::start(a, jni);
    crate::sampler::start(a);
    crate::triggers::start(a);
    if a.opts.mode == Mode::Live {
        crate::server::start(a);
        if a.opts.wait {
            let (lock, cv) = &a.connected;
            let mut c = lock.lock().unwrap();
            log("waiting for a daemon to connect (wait option)");
            while !*c {
                c = cv.wait(c).unwrap();
            }
        }
    }
}

/// Creates jmethodIDs for every loaded class (needed by AsyncGetCallTrace on attach).
fn preload_methods(a: &Agent) {
    if !a.async_possible() {
        return;
    }
    let mut n = 0;
    let mut classes: *mut jclass = ptr::null_mut();
    if jvmti!(a.jvmti, GetLoadedClasses, &mut n, &mut classes) != JVMTI_ERROR_NONE {
        return;
    }
    if classes.is_null() || n <= 0 {
        return;
    }
    // SAFETY: JVMTI returned `n` class references.
    let list = unsafe { std::slice::from_raw_parts(classes, n as usize) };
    for &k in list {
        load_methods(a, k);
    }
    a.jvmti.dealloc(classes);
}

fn load_methods(a: &Agent, klass: jclass) {
    let mut n = 0;
    let mut methods: *mut jmethodID = ptr::null_mut();
    if jvmti!(a.jvmti, GetClassMethods, klass, &mut n, &mut methods) == JVMTI_ERROR_NONE {
        a.jvmti.dealloc(methods);
    }
}

impl Agent {
    pub fn async_possible(&self) -> bool {
        cfg!(target_os = "linux") && self.opts.sampler != crate::SamplerChoice::Sync
    }
}

/// Thread index for `thread`, registering it on first sight. `None` for agent-internal threads.
pub fn thread_index(a: &Agent, jni: *mut JNIEnv, thread: jthread) -> Option<u32> {
    let tagging = tagging(a);
    if tagging {
        let tag = a.jvmti.get_tag(thread);
        if tag > 0 {
            return Some((tag - 1) as u32);
        }
        if tag == INTERNAL_TAG {
            return None;
        }
    }
    let _g = REGISTER.lock();
    if tagging {
        let tag = a.jvmti.get_tag(thread);
        if tag != 0 {
            return (tag > 0).then(|| (tag - 1) as u32);
        }
    }
    let java_id = crate::java::thread_java_id(jni, thread);
    if !tagging {
        let map = BY_JAVA_ID.lock();
        if let Some(&i) = map.as_ref().and_then(|m| m.get(&java_id)) {
            return Some(i);
        }
    }
    let (name, daemon) = a.jvmti.thread_name(thread);
    if name.starts_with("OpenProfiler ") {
        if tagging {
            a.jvmti.set_tag(thread, INTERNAL_TAG);
        }
        return None;
    }
    let idx = a.collector.lock().new_thread(java_id, name, daemon, now_nanos());
    if tagging {
        a.jvmti.set_tag(thread, idx as jlong + 1);
    } else {
        BY_JAVA_ID.lock().get_or_insert_with(HashMap::new).insert(java_id, idx);
    }
    Some(idx)
}

/// Stack of `thread` (null: the current thread), with the current thread's request label as the
/// root frame, interned in the collector.
///
/// Most stacks repeat (the same code paths run over and over), and resolving one takes the
/// collector lock that every recording thread contends on. Each thread therefore remembers the
/// stack ids of the raw stacks it captured, by a 128-bit hash of the frames and the label, and
/// takes the lock only for stacks it has not seen. Stack ids are never reused.
pub fn capture_stack(a: &Agent, jni: *mut JNIEnv, thread: jthread) -> Option<u32> {
    let max = a.opts.max_frames.min(512) as usize;
    STACK_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let cache = &mut *cache;
        cache.frames.resize(max, jvmtiFrameInfo { method: ptr::null_mut(), location: 0 });
        let mut count = 0;
        if jvmti!(a.jvmti, GetStackTrace, thread, 0, max as jint, cache.frames.as_mut_ptr(), &mut count)
            != JVMTI_ERROR_NONE
        {
            return None;
        }
        let frames = &cache.frames[..count.max(0) as usize];
        let label = crate::probes::current_label_frame();
        let key = stack_key(frames, label);
        if let Some(&id) = cache.ids.get(&key) {
            return Some(id);
        }
        jni!(jni, PushLocalFrame, 64);
        let stack = {
            let mut c = a.collector.lock();
            let c = &mut *c;
            let mut ids: Vec<u32> = frames
                .iter()
                .map(|f| c.resolver.frame(a.jvmti, &mut c.symbols, f.method, f.location, FrameKind::Unknown))
                .collect();
            ids.extend(label);
            c.stacks.intern_leaf_first(&ids)
        };
        jni!(jni, PopLocalFrame, ptr::null_mut());
        if cache.ids.len() >= STACK_CACHE_SIZE {
            cache.ids.clear();
        }
        cache.ids.insert(key, stack);
        Some(stack)
    })
}

#[derive(Default)]
struct StackCache {
    /// Reused frame buffer.
    frames: Vec<jvmtiFrameInfo>,
    ids: HashMap<u128, u32>,
}

thread_local! {
    static STACK_CACHE: std::cell::RefCell<StackCache> = std::cell::RefCell::new(StackCache::default());
}

/// Stacks a thread remembers; the cache starts over when it is full.
const STACK_CACHE_SIZE: usize = 2048;

/// Two keyed 64-bit hashes of the frames and the label.
fn stack_key(frames: &[jvmtiFrameInfo], label: Option<u32>) -> u128 {
    use std::hash::{BuildHasher, Hasher};
    static KEYS: std::sync::OnceLock<(
        std::collections::hash_map::RandomState,
        std::collections::hash_map::RandomState,
    )> = std::sync::OnceLock::new();
    let (k1, k2) = KEYS.get_or_init(Default::default);
    let mut h1 = k1.build_hasher();
    let mut h2 = k2.build_hasher();
    for f in frames {
        for h in [&mut h1, &mut h2] {
            h.write_usize(f.method as usize);
            h.write_i64(f.location);
        }
    }
    let l = label.map_or(u64::MAX, u64::from);
    h1.write_u64(l);
    h2.write_u64(l);
    (u128::from(h1.finish()) << 64) | u128::from(h2.finish())
}

unsafe extern "C" fn vm_init(_jvmti: *mut jvmtiEnv, jni: *mut JNIEnv, _thread: jthread) {
    if let Some(a) = agent() {
        on_live(a, jni);
    }
}

unsafe extern "C" fn vm_death(_jvmti: *mut jvmtiEnv, jni: *mut JNIEnv) {
    let Some(a) = agent() else { return };
    crate::triggers::on_exit(a, jni);
    a.shutting_down.store(true, Ordering::SeqCst);
    if a.opts.mode == Mode::Offline {
        let path = a.opts.file.clone().unwrap_or_else(|| {
            std::path::PathBuf::from(format!("openprofiler-{}.oplink", std::process::id()))
        });
        match crate::recording::write(a, &path) {
            Ok(()) => log(&format!("recording written to {}", path.display())),
            Err(e) => log(&format!("cannot write recording {}: {e}", path.display())),
        }
    } else {
        crate::server::final_flush(a);
    }
    if let Some(p) = a.info_file.lock().take() {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("openprofiler-agent-{}", std::process::id())),
    );
}

unsafe extern "C" fn thread_start(_jvmti: *mut jvmtiEnv, jni: *mut JNIEnv, thread: jthread) {
    let Some(a) = agent() else { return };
    if !a.live_started.load(Ordering::Relaxed) {
        return;
    }
    let idx = thread_index(a, jni, thread);
    #[cfg(target_os = "linux")]
    crate::sampler_async::on_thread_start(a, idx);
    #[cfg(not(target_os = "linux"))]
    let _ = idx;
}

unsafe extern "C" fn thread_end(_jvmti: *mut jvmtiEnv, jni: *mut JNIEnv, thread: jthread) {
    let Some(a) = agent() else { return };
    crate::instrument::calls::flush_current();
    #[cfg(target_os = "linux")]
    crate::sampler_async::on_thread_end();
    if let Some(idx) = thread_index(a, jni, thread) {
        a.collector.lock().end_thread(idx, now_nanos());
    }
}

unsafe extern "C" fn class_load(_jvmti: *mut jvmtiEnv, _jni: *mut JNIEnv, _thread: jthread, _klass: jclass) {}

unsafe extern "C" fn class_prepare(_jvmti: *mut jvmtiEnv, jni: *mut JNIEnv, _thread: jthread, klass: jclass) {
    if let Some(a) = agent() {
        if a.async_possible() {
            load_methods(a, klass);
        }
        crate::instrument::on_class_prepare(a, jni, klass);
    }
}

/// Unbiased estimate of the bytes a sampled allocation represents.
fn alloc_weight(size: i64, interval: u32) -> u64 {
    let size = size.max(1) as f64;
    let p = 1.0 - (-size / interval as f64).exp();
    (size / p.max(1e-9)) as u64
}

unsafe extern "C" fn sampled_object_alloc(
    _jvmti: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    thread: jthread,
    object: jobject,
    klass: jclass,
    size: jlong,
) {
    let Some(a) = agent() else { return };
    if !a.alloc_on.load(Ordering::Relaxed) || a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    crate::heapdump::record(object);
    let Some(idx) = thread_index(a, jni, thread) else { return };
    let Some(stack) = capture_stack(a, jni, ptr::null_mut()) else { return };
    let mut c = a.collector.lock();
    let class = c.resolver.class_name(a.jvmti, klass);
    let class_id = c.symbols.intern_str(&class);
    let w = alloc_weight(size, a.opts.alloc_interval);
    c.add_sample(now_nanos(), SampleKind::Alloc, idx, ThreadState::Runnable, stack, w, Some(class_id));
}

/// Contended monitor enter or `Object.wait`: remembers the start, the owner of a contended
/// monitor (only known now) and the stack, if the thread's unsampled time of short events reached
/// `monitor_interval` or, within [`SPECULATIVE_STACKS`] per second, in case the event is long. The
/// thread does not own the monitor yet, so the work does not hold up others; at the end it does,
/// and no stack is taken there.
fn monitor_begin(jni: *mut JNIEnv, object: jobject, kind: MonitorEventKind) {
    let Some(a) = agent() else { return };
    if !a.monitor_on.load(Ordering::Relaxed) || a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    if kind == MonitorEventKind::Enter {
        CONTENDED.fetch_add(1, Ordering::Relaxed);
    }
    let owner = if kind == MonitorEventKind::Enter { monitor_owner(a, jni, object) } else { None };
    let due = MONITOR_UNSAMPLED.get()[slot(kind)] as u128 >= a.opts.monitor_interval.as_nanos()
        && take_budget(&SAMPLE_BUDGET, SAMPLE_STACKS);
    let sample = if due || take_budget(&SPECULATIVE_BUDGET, SPECULATIVE_STACKS) {
        monitor_site(a, jni, object)
    } else {
        None
    };
    let epoch = CONTENTION_EPOCH.load(Ordering::Relaxed);
    MONITOR_PENDING.set(Some(PendingMonitor { start: now_nanos(), kind, owner, sample, epoch }));
}

/// Identity hash code of `object` as an id (0 if unavailable).
pub fn identity_hash(a: &Agent, object: jobject) -> u64 {
    let mut h: jint = 0;
    if object.is_null() || jvmti!(a.jvmti, GetObjectHashCode, object, &mut h) != JVMTI_ERROR_NONE {
        return 0;
    }
    h as u32 as u64
}

/// Thread index of the thread that owns the (contended) monitor of `object`.
///
/// `GetObjectMonitorUsage` runs as a safepoint operation, so lookups are limited to `owner_rate`
/// per second; contention beyond that is recorded without an owner.
fn monitor_owner(a: &Agent, jni: *mut JNIEnv, object: jobject) -> Option<u32> {
    if !OWNER_LOOKUPS.load(Ordering::Relaxed) {
        return None;
    }
    if !take_budget(&OWNER_BUDGET, a.opts.owner_rate as u64) {
        return None;
    }
    // SAFETY: plain-old-data out struct; zero is a valid initial value.
    let mut u: jvmtiMonitorUsage = unsafe { std::mem::zeroed() };
    if jvmti!(a.jvmti, GetObjectMonitorUsage, object, &mut u) != JVMTI_ERROR_NONE {
        return None;
    }
    a.jvmti.dealloc(u.waiters);
    a.jvmti.dealloc(u.notify_waiters);
    if u.owner.is_null() {
        return None;
    }
    let idx = thread_index(a, jni, u.owner);
    jni!(jni, DeleteLocalRef, u.owner);
    idx
}

/// The monitor was entered or the wait ended. Events of at least `monitor_threshold` that have a
/// stack from their start are recorded as an event and a sample of their duration, within
/// [`LONG_MONITOR_EVENTS`] per second. The others are sampled: once a thread's summed time of them
/// reaches `monitor_interval`, an event with a stack records a sample of that weight.
///
/// After a contended enter the thread owns the monitor, so this takes no agent lock that other
/// threads hold for long: the raw frames are queued and resolved later by [`drain_monitors`].
fn monitor_end(jni: *mut JNIEnv, thread: jthread, object: jobject, kind: MonitorEventKind) {
    let Some(m) = MONITOR_PENDING.take() else { return };
    if m.kind != kind || m.epoch != CONTENTION_EPOCH.load(Ordering::Relaxed) {
        return;
    }
    let Some(a) = agent() else { return };
    let dur = (now_nanos() - m.start).max(0);
    let long = dur as u128 >= a.opts.monitor_threshold.as_nanos()
        && m.sample.is_some()
        && take_budget(&LONG_BUDGET, LONG_MONITOR_EVENTS);
    let (site, weight) = if long {
        (m.sample, dur)
    } else {
        let mut unsampled = MONITOR_UNSAMPLED.get();
        let s = slot(m.kind);
        unsampled[s] += dur;
        let w = unsampled[s];
        let record = m.sample.is_some() && w as u128 >= a.opts.monitor_interval.as_nanos();
        if record {
            unsampled[s] = 0;
        }
        MONITOR_UNSAMPLED.set(unsampled);
        (m.sample.filter(|_| record), w)
    };
    let Some((frames, class)) = site else { return };
    let Some(thread) = thread_index(a, jni, thread) else { return };
    let scale = if m.kind == MonitorEventKind::Enter { CONTENTION_SCALE.load(Ordering::Relaxed) } else { 1 };
    let d = DeferredMonitor {
        start: m.start,
        duration: dur,
        weight: weight as u64 * u64::from(scale),
        kind: m.kind,
        blocked: m.kind == MonitorEventKind::Enter,
        thread,
        owner: m.owner,
        long,
        class,
        id: if long { identity_hash(a, object) } else { 0 },
        frames,
        label: crate::probes::current_label_frame(),
    };
    defer(d);
}

fn defer(d: DeferredMonitor) {
    let mut q = DEFERRED_MONITORS.lock();
    if q.len() < MAX_DEFERRED_MONITORS {
        q.push(d);
    }
}

static LONG_PARKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A `j.u.c` park on `blocker` of thread `idx` that took `dur` (see `park`). Parks of at least
/// `monitor_threshold` are recorded like long monitor events, within a budget of
/// [`LONG_MONITOR_EVENTS`] per second; the time of the parks beyond it is sampled per thread
/// (every `monitor_interval` yields a sample). Shorter parks are not recorded.
pub fn record_park(
    a: &Agent,
    jni: *mut JNIEnv,
    idx: u32,
    start: i64,
    dur: i64,
    blocker: jobject,
    owner: Option<u32>,
) {
    if (dur as u128) < a.opts.monitor_threshold.as_nanos() {
        return;
    }
    let blocked = owner.is_some();
    let long = take_budget(&LONG_PARKS, LONG_MONITOR_EVENTS);
    let weight = if long {
        dur
    } else {
        let slot = usize::from(!blocked);
        let mut unsampled = MONITOR_UNSAMPLED.get();
        unsampled[slot] += dur;
        if (unsampled[slot] as u128) < a.opts.monitor_interval.as_nanos()
            || !take_budget(&SAMPLE_BUDGET, SAMPLE_STACKS)
        {
            MONITOR_UNSAMPLED.set(unsampled);
            return;
        }
        let w = unsampled[slot];
        unsampled[slot] = 0;
        MONITOR_UNSAMPLED.set(unsampled);
        w
    };
    let Some((frames, class)) = monitor_site(a, jni, blocker) else { return };
    defer(DeferredMonitor {
        start,
        duration: dur,
        weight: weight as u64,
        kind: MonitorEventKind::Park,
        blocked,
        thread: idx,
        owner,
        long,
        class,
        id: if long { identity_hash(a, blocker) } else { 0 },
        frames,
        label: crate::probes::current_label_frame(),
    });
}

/// A recorded monitor event whose frames are not resolved yet.
struct DeferredMonitor {
    start: i64,
    duration: i64,
    weight: u64,
    kind: MonitorEventKind,
    /// Blocked on a lock (a monitor sample) rather than waiting (a wait sample).
    blocked: bool,
    thread: u32,
    owner: Option<u32>,
    long: bool,
    class: String,
    id: u64,
    frames: Vec<jvmtiFrameInfo>,
    label: Option<u32>,
}

// SAFETY: jmethodIDs are process-wide identifiers, valid on any thread.
unsafe impl Send for DeferredMonitor {}

static DEFERRED_MONITORS: Mutex<Vec<DeferredMonitor>> = Mutex::new(Vec::new());
/// Events beyond this many between two drains are dropped.
const MAX_DEFERRED_MONITORS: usize = 200_000;

/// Resolves queued monitor events into samples and events. Called periodically by the sampler
/// thread and before batches are cut.
pub fn drain_monitors(a: &Agent) {
    let queued = std::mem::take(&mut *DEFERRED_MONITORS.lock());
    if queued.is_empty() {
        return;
    }
    let mut c = a.collector.lock();
    let c = &mut *c;
    for d in queued {
        let mut ids: Vec<u32> = d
            .frames
            .iter()
            .map(|f| c.resolver.frame(a.jvmti, &mut c.symbols, f.method, f.location, FrameKind::Unknown))
            .collect();
        ids.extend(d.label);
        let stack = c.stacks.intern_leaf_first(&ids);
        let (sample_kind, state) = if d.blocked {
            (SampleKind::Monitor, ThreadState::Blocked)
        } else {
            (SampleKind::Wait, ThreadState::Waiting)
        };
        let class_id = c.symbols.intern_str(&d.class);
        c.add_sample(d.start, sample_kind, d.thread, state, stack, d.weight, Some(class_id));
        if d.long {
            c.add_monitor_event(MonitorRow {
                start: d.start,
                duration: d.duration,
                kind: d.kind,
                thread: d.thread,
                owner: d.owner,
                monitor_class: d.class,
                monitor_id: d.id,
                stack,
            });
        }
    }
}

unsafe extern "C" fn monitor_contended_enter(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    _thread: jthread,
    object: jobject,
) {
    monitor_begin(jni, object, MonitorEventKind::Enter);
}

unsafe extern "C" fn monitor_contended_entered(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    thread: jthread,
    object: jobject,
) {
    monitor_end(jni, thread, object, MonitorEventKind::Enter);
}

unsafe extern "C" fn monitor_wait(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    _thread: jthread,
    object: jobject,
    _t: jlong,
) {
    monitor_begin(jni, object, MonitorEventKind::Wait);
}

unsafe extern "C" fn monitor_waited(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    thread: jthread,
    object: jobject,
    _to: jboolean,
) {
    monitor_end(jni, thread, object, MonitorEventKind::Wait);
}

unsafe extern "C" fn resource_exhausted(
    _j: *mut jvmtiEnv,
    _jni: *mut JNIEnv,
    flags: jint,
    _reserved: *const std::ffi::c_void,
    description: *const std::ffi::c_char,
) {
    if agent().is_none() {
        return;
    }
    let what = if description.is_null() {
        String::new()
    } else {
        // SAFETY: the VM passes a NUL-terminated description.
        unsafe { std::ffi::CStr::from_ptr(description) }.to_string_lossy().into_owned()
    };
    crate::triggers::on_out_of_memory(flags, &what);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_weight_estimates() {
        assert!(alloc_weight(16, 512 * 1024) > 500_000);
        assert_eq!(alloc_weight(100 << 20, 512 * 1024), 100 << 20);
    }
}
