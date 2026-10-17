//! Virtual threads (JDK 21+), from HotSpot's `VirtualThreadMount` / `VirtualThreadUnmount`
//! extension events and the standard `VirtualThreadEnd` event.
//!
//! - A virtual thread gets its own thread row on its first mount. After [`MAX_ROWS`] distinct
//!   virtual threads the rest share one row, so request-per-thread servers do not grow the thread
//!   table without bound.
//! - Mount/unmount tell the async sampler which virtual thread runs on a carrier, so CPU samples
//!   taken on the carrier are attributed to the virtual thread. They also keep a mounted flag
//!   that the wall-clock tick turns into state history (mounted = runnable, unmounted =
//!   waiting), at the same resolution as platform threads.
//! - An unmount/mount pair is a park: unmounted on a blocker for at least `monitor_threshold`
//!   produces a park event with the blocker and, for locks, its owner (read at unmount), like
//!   `park` does for platform threads. Unmounts without a blocker (sleep, I/O, yield) only show in
//!   the state history.
//!
//! The callbacks run on every mount and unmount, so they avoid the collector lock: everything
//! beyond flag updates happens only for parks that cross the threshold.

use crate::collector::{Collector, now_nanos};
use crate::jvm::jvmti;
use crate::{Agent, agent, debug};
use jvmti_sys::*;
use openprofiler_model::ThreadState;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

/// Distinct virtual threads that get their own thread row.
pub const MAX_ROWS: usize = 4096;

/// A virtual thread with its own row.
#[derive(Default)]
struct VThread {
    mounted: bool,
    /// Unmounted on a blocker since this time, with the lock owner at that point.
    parked: Option<(i64, Option<u32>)>,
}

#[derive(Default)]
struct State {
    rows: usize,
    threads: HashMap<u32, VThread>,
}

/// Lock order: the collector before `STATE`; never the other way round.
static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Shared row for virtual threads beyond [`MAX_ROWS`].
static OVERFLOW: OnceLock<u32> = OnceLock::new();

/// Index of an extension event by id, freeing JVMTI's descriptions.
fn extension_event(a: &Agent, id: &CStr) -> Option<jint> {
    let mut n: jint = 0;
    let mut infos: *mut jvmtiExtensionEventInfo = std::ptr::null_mut();
    if jvmti!(a.jvmti, GetExtensionEvents, &mut n, &mut infos) != JVMTI_ERROR_NONE || infos.is_null() {
        return None;
    }
    // SAFETY: JVMTI returned `n` entries.
    let list = unsafe { std::slice::from_raw_parts(infos, n.max(0) as usize) };
    let mut found = None;
    for e in list {
        // SAFETY: ids are NUL-terminated strings.
        if !e.id.is_null() && unsafe { CStr::from_ptr(e.id) } == id {
            found = Some(e.extension_event_index);
        }
        if !e.params.is_null() {
            // SAFETY: `params` has `param_count` entries.
            for p in unsafe { std::slice::from_raw_parts(e.params, e.param_count.max(0) as usize) } {
                a.jvmti.dealloc(p.name);
            }
        }
        a.jvmti.dealloc(e.params);
        a.jvmti.dealloc(e.id);
        a.jvmti.dealloc(e.short_description);
    }
    a.jvmti.dealloc(infos);
    found
}

/// Subscribes to the mount/unmount events. Needs `can_support_virtual_threads` and tagging.
pub fn install(a: &Agent) -> bool {
    if !crate::events::has_cap(a, "tagging") {
        return false;
    }
    let (Some(mount), Some(unmount)) = (
        extension_event(a, c"com.sun.hotspot.events.VirtualThreadMount"),
        extension_event(a, c"com.sun.hotspot.events.VirtualThreadUnmount"),
    ) else {
        debug("virtual thread mount events unavailable");
        return false;
    };
    *STATE.lock() = Some(State::default());
    // The VM calls extension callbacks through a variadic pointer type. Only the fixed first
    // argument is read (the JNIEnv and thread are re-obtained), which is ABI-safe everywhere,
    // including Apple arm64 where variadic arguments travel on the stack.
    // SAFETY: transmuting between C function pointer types; see above.
    let (m, u): (jvmtiExtensionEvent, jvmtiExtensionEvent) = unsafe {
        (
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut jvmtiEnv),
                unsafe extern "C" fn(*mut jvmtiEnv, ...),
            >(on_mount)),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut jvmtiEnv),
                unsafe extern "C" fn(*mut jvmtiEnv, ...),
            >(on_unmount)),
        )
    };
    // Like standard events, extension events need a callback and to be enabled.
    let ok = jvmti!(a.jvmti, SetExtensionEventCallback, mount, m) == JVMTI_ERROR_NONE
        && jvmti!(a.jvmti, SetExtensionEventCallback, unmount, u) == JVMTI_ERROR_NONE
        && a.jvmti.enable_event(mount as jvmtiEvent, true).is_ok()
        && a.jvmti.enable_event(unmount as jvmtiEvent, true).is_ok();
    if ok {
        let _ = a.jvmti.enable_event(JVMTI_EVENT_VIRTUAL_THREAD_END, true);
    }
    ok
}

/// The current (virtual) thread and its JNIEnv.
fn current(a: &Agent) -> Option<(*mut JNIEnv, jthread)> {
    let jni = a.vm.env()?;
    let mut t: jthread = std::ptr::null_mut();
    (jvmti!(a.jvmti, GetCurrentThread, &mut t) == JVMTI_ERROR_NONE && !t.is_null()).then_some((jni, t))
}

/// Row of a virtual thread, registering it on first sight. Returns `(index, shared)`.
fn row(a: &Agent, jni: *mut JNIEnv, vthread: jthread) -> Option<(u32, bool)> {
    let tag = a.jvmti.get_tag(vthread);
    if tag > 0 {
        let idx = (tag - 1) as u32;
        return Some((idx, OVERFLOW.get() == Some(&idx)));
    }
    let own_row = {
        let mut st = STATE.lock();
        let st = st.as_mut()?;
        st.rows < MAX_ROWS && {
            st.rows += 1;
            true
        }
    };
    let now = now_nanos();
    let (idx, shared) = if own_row {
        let java_id = crate::java::thread_java_id(jni, vthread);
        let (name, _) = a.jvmti.thread_name(vthread);
        let name = if name.is_empty() { format!("virtual-{java_id}") } else { name };
        let idx = a.collector.lock().new_virtual_thread(java_id, name, now);
        if let Some(st) = STATE.lock().as_mut() {
            st.threads.insert(idx, VThread::default());
        }
        (idx, false)
    } else {
        let idx = *OVERFLOW
            .get_or_init(|| a.collector.lock().new_virtual_thread(0, "[other virtual threads]".into(), now));
        (idx, true)
    };
    a.jvmti.set_tag(vthread, idx as jlong + 1);
    Some((idx, shared))
}

unsafe extern "C" fn on_mount(_jvmti: *mut jvmtiEnv) {
    let Some(a) = agent() else { return };
    if a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    let Some((jni, vthread)) = current(a) else { return };
    let Some((idx, shared)) = row(a, jni, vthread) else { return };
    #[cfg(target_os = "linux")]
    crate::sampler_async::set_mounted(Some(idx));
    if shared {
        return;
    }
    let parked = {
        let mut st = STATE.lock();
        let Some(t) = st.as_mut().and_then(|s| s.threads.get_mut(&idx)) else { return };
        t.mounted = true;
        t.parked.take()
    };
    let Some((since, owner)) = parked else { return };
    if !a.monitor_on.load(Ordering::Relaxed)
        || ((now_nanos() - since) as u128) < a.opts.monitor_threshold.as_nanos()
    {
        return;
    }
    // The blocker is still set: LockSupport.park clears it only after the thread continues.
    let b = crate::park::blocker(a, jni, vthread);
    if !b.object.is_null() {
        crate::events::record_park(a, jni, idx, since, now_nanos() - since, b.object, owner);
    }
}

unsafe extern "C" fn on_unmount(_jvmti: *mut jvmtiEnv) {
    let Some(a) = agent() else { return };
    #[cfg(target_os = "linux")]
    crate::sampler_async::set_mounted(None);
    if a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    let Some((jni, vthread)) = current(a) else { return };
    let tag = a.jvmti.get_tag(vthread);
    if tag <= 0 {
        return;
    }
    let idx = (tag - 1) as u32;
    let parked = if a.monitor_on.load(Ordering::Relaxed) {
        let b = crate::park::blocker(a, jni, vthread);
        (!b.object.is_null()).then(|| (now_nanos(), b.owner))
    } else {
        None
    };
    // Threads that already ended (the final unmount follows VirtualThreadEnd) are gone from the
    // map, as are those in the shared row.
    if let Some(t) = STATE.lock().as_mut().and_then(|s| s.threads.get_mut(&idx)) {
        t.mounted = false;
        t.parked = parked;
    }
}

pub unsafe extern "C" fn virtual_thread_end(_jvmti: *mut jvmtiEnv, _jni: *mut JNIEnv, vthread: jthread) {
    let Some(a) = agent() else { return };
    let tag = a.jvmti.get_tag(vthread);
    if tag <= 0 {
        return;
    }
    let idx = (tag - 1) as u32;
    let known = STATE.lock().as_mut().is_some_and(|s| s.threads.remove(&idx).is_some());
    if known {
        a.collector.lock().end_thread(idx, now_nanos());
    }
}

/// Records the current state of every live virtual thread (called on the wall-clock tick with
/// the collector locked).
pub fn observe_states(c: &mut Collector, now: i64) {
    if let Some(s) = STATE.lock().as_ref() {
        for (&idx, t) in &s.threads {
            c.observe_state(idx, if t.mounted { ThreadState::Runnable } else { ThreadState::Waiting }, now);
        }
    }
}
