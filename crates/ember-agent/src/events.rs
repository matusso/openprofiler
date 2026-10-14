//! JVMTI event callbacks and agent start-up once the VM is live.

use crate::collector::now_nanos;
use crate::jvm::{JResult, jni, jvmti};
use crate::{Agent, Mode, agent, debug, log};
use ember_model::{FrameKind, MonitorEventKind, SampleKind, ThreadState};
use jvmti_sys::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::Ordering;

/// Tag value for the agent's own threads (never sampled).
const INTERNAL_TAG: jlong = -1;

static REGISTER: Mutex<()> = Mutex::new(());
/// Fallback thread registry when object tagging is unavailable: Java thread id → index.
static BY_JAVA_ID: Mutex<Option<HashMap<u64, u32>>> = Mutex::new(None);
/// An in-flight monitor enter/wait: (start, kind, monitor class, stack).
type PendingMonitor = (i64, MonitorEventKind, String, u32);
/// In-flight monitor waits per thread index.
static MONITOR_PENDING: Mutex<Option<HashMap<u32, PendingMonitor>>> = Mutex::new(None);

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
        set_monitor_events(a, a.monitor_on.load(Ordering::SeqCst));
    }
    Ok(())
}

pub fn has_cap(a: &Agent, name: &str) -> bool {
    a.caps.lock().iter().any(|c| c == name)
}

fn set_monitor_events(a: &Agent, on: bool) {
    for ev in [
        JVMTI_EVENT_MONITOR_CONTENDED_ENTER,
        JVMTI_EVENT_MONITOR_CONTENDED_ENTERED,
        JVMTI_EVENT_MONITOR_WAIT,
        JVMTI_EVENT_MONITOR_WAITED,
    ] {
        let _ = a.jvmti.enable_event(ev, on);
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
            a.alloc_on.store(on, Ordering::SeqCst);
            let _ = a.jvmti.enable_event(JVMTI_EVENT_SAMPLED_OBJECT_ALLOC, on);
        }
        "monitor" if has_cap(a, "monitor") => {
            a.monitor_on.store(on, Ordering::SeqCst);
            set_monitor_events(a, on);
        }
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
    #[cfg(target_os = "linux")]
    crate::sampler_async::start(a);
    crate::sampler::start(a);
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
    let tagging = has_cap(a, "tagging");
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
    if name.starts_with("Ember ") {
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

/// Captures the current stack of `thread` (null = current thread) as a stack id.
pub fn capture_stack(a: &Agent, jni: *mut JNIEnv, thread: jthread) -> Option<u32> {
    let max = a.opts.max_frames.min(512) as usize;
    let mut frames = vec![jvmtiFrameInfo { method: ptr::null_mut(), location: 0 }; max];
    let mut count = 0;
    if jvmti!(a.jvmti, GetStackTrace, thread, 0, max as jint, frames.as_mut_ptr(), &mut count)
        != JVMTI_ERROR_NONE
    {
        return None;
    }
    frames.truncate(count.max(0) as usize);
    jni!(jni, PushLocalFrame, 64);
    let stack = {
        let mut c = a.collector.lock();
        let c = &mut *c;
        let ids: Vec<u32> = frames
            .iter()
            .map(|f| c.resolver.frame(a.jvmti, &mut c.symbols, f.method, f.location, FrameKind::Unknown))
            .collect();
        c.stacks.intern_leaf_first(&ids)
    };
    jni!(jni, PopLocalFrame, ptr::null_mut());
    Some(stack)
}

unsafe extern "C" fn vm_init(_jvmti: *mut jvmtiEnv, jni: *mut JNIEnv, _thread: jthread) {
    if let Some(a) = agent() {
        on_live(a, jni);
    }
}

unsafe extern "C" fn vm_death(_jvmti: *mut jvmtiEnv, _jni: *mut JNIEnv) {
    let Some(a) = agent() else { return };
    a.shutting_down.store(true, Ordering::SeqCst);
    if a.opts.mode == Mode::Offline {
        let path =
            a.opts.file.clone().unwrap_or_else(|| {
                std::path::PathBuf::from(format!("ember-{}.emberlink", std::process::id()))
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
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("ember-agent-{}", std::process::id())));
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
    #[cfg(target_os = "linux")]
    crate::sampler_async::on_thread_end();
    if let Some(idx) = thread_index(a, jni, thread) {
        a.collector.lock().end_thread(idx, now_nanos());
    }
}

unsafe extern "C" fn class_load(_jvmti: *mut jvmtiEnv, _jni: *mut JNIEnv, _thread: jthread, _klass: jclass) {}

unsafe extern "C" fn class_prepare(
    _jvmti: *mut jvmtiEnv,
    _jni: *mut JNIEnv,
    _thread: jthread,
    klass: jclass,
) {
    if let Some(a) = agent() {
        load_methods(a, klass);
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
    _object: jobject,
    klass: jclass,
    size: jlong,
) {
    let Some(a) = agent() else { return };
    if !a.alloc_on.load(Ordering::Relaxed) || a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    let Some(idx) = thread_index(a, jni, thread) else { return };
    let Some(stack) = capture_stack(a, jni, ptr::null_mut()) else { return };
    let mut c = a.collector.lock();
    let class = c.resolver.class_name(a.jvmti, klass);
    let class_id = c.symbols.intern_str(&class);
    let w = alloc_weight(size, a.opts.alloc_interval);
    c.add_sample(now_nanos(), SampleKind::Alloc, idx, ThreadState::Runnable, stack, w, Some(class_id));
}

fn monitor_begin(jni: *mut JNIEnv, thread: jthread, object: jobject, kind: MonitorEventKind) {
    let Some(a) = agent() else { return };
    if !a.monitor_on.load(Ordering::Relaxed) || a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    let Some(idx) = thread_index(a, jni, thread) else { return };
    let Some(stack) = capture_stack(a, jni, ptr::null_mut()) else { return };
    let cls = jni!(jni, GetObjectClass, object);
    let class = a.collector.lock().resolver.class_name(a.jvmti, cls);
    jni!(jni, DeleteLocalRef, cls);
    MONITOR_PENDING.lock().get_or_insert_with(HashMap::new).insert(idx, (now_nanos(), kind, class, stack));
}

fn monitor_end(jni: *mut JNIEnv, thread: jthread) {
    let Some(a) = agent() else { return };
    let Some(idx) = thread_index(a, jni, thread) else { return };
    let Some((start, kind, class, stack)) = MONITOR_PENDING.lock().as_mut().and_then(|m| m.remove(&idx))
    else {
        return;
    };
    let now = now_nanos();
    let dur = (now - start).max(0);
    let (sample_kind, state) = match kind {
        MonitorEventKind::Enter => (SampleKind::Monitor, ThreadState::Blocked),
        _ => (SampleKind::Wait, ThreadState::Waiting),
    };
    let mut c = a.collector.lock();
    let class_id = c.symbols.intern_str(&class);
    c.add_sample(start, sample_kind, idx, state, stack, dur as u64, Some(class_id));
    if dur as u128 >= a.opts.monitor_threshold.as_nanos() {
        c.add_monitor_event(start, dur, kind, idx, class, stack);
    }
}

unsafe extern "C" fn monitor_contended_enter(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    thread: jthread,
    object: jobject,
) {
    monitor_begin(jni, thread, object, MonitorEventKind::Enter);
}

unsafe extern "C" fn monitor_contended_entered(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    thread: jthread,
    _o: jobject,
) {
    monitor_end(jni, thread);
}

unsafe extern "C" fn monitor_wait(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    thread: jthread,
    object: jobject,
    _t: jlong,
) {
    monitor_begin(jni, thread, object, MonitorEventKind::Wait);
}

unsafe extern "C" fn monitor_waited(
    _j: *mut jvmtiEnv,
    jni: *mut JNIEnv,
    thread: jthread,
    _o: jobject,
    _to: jboolean,
) {
    monitor_end(jni, thread);
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
