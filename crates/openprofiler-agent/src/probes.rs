//! Native side of the boot classes' probes (`dev.openprofiler.boot.Hooks` natives).
//!
//! The Java probes register themselves and their interceptions, then report calls: every call is
//! aggregated per (second, probe, payload, thread, stack) into probe-call rows, and calls within
//! the per-probe event budget (`probe_events` per second; slow calls always) are also recorded as
//! individual events. Control objects (connections, files, sockets) report open and close.
//!
//! A probe that starts serving a request (HTTP server, gRPC server) sets the thread's request
//! label: it becomes the root frame of the thread's CPU, allocation, monitor, instrumented-call
//! and probe stacks ("URL splitting"), and the context of its probe events. The boot classes carry
//! the label across thread hand-offs (executors, `CompletableFuture`, fork/join, virtual threads).

use crate::collector::now_nanos;
use crate::events::capture_stack;
use crate::instrument::{self, Interception};
use crate::jvm::{jni, jstring_to_string, new_string};
use crate::{Agent, agent, debug};
use jvmti_sys::*;
use openprofiler_model::FrameKind;
use openprofiler_proto::v1;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether probes record anything (`SetRecording probes`).
pub static ON: AtomicBool = AtomicBool::new(true);
/// Interception flags (must match `Interception.java`).
const F_SUBTYPES: i32 = 1;
const F_ARGS: i32 = 2;
const F_EXIT_ONLY: i32 = 4;
const F_ENTRY_ONLY: i32 = 8;
/// Calls at least this slow are always recorded as events.
const SLOW_NS: i64 = 100_000_000;

/// Request label frame + 1 (0: none) per thread index, read by the samplers. Labels change on
/// every request and hand-off, so this is a table of atomics; threads beyond it use
/// [`LABELS_OVERFLOW`].
static LABELS: [std::sync::atomic::AtomicU32; 32768] =
    [const { std::sync::atomic::AtomicU32::new(0) }; 32768];
static LABELS_OVERFLOW: Mutex<Option<HashMap<u32, u32>>> = Mutex::new(None);

fn publish_label(idx: u32, frame: Option<u32>) {
    match LABELS.get(idx as usize) {
        Some(slot) => slot.store(frame.map_or(0, |f| f + 1), Ordering::Relaxed),
        None => {
            let mut m = LABELS_OVERFLOW.lock();
            let m = m.get_or_insert_with(HashMap::new);
            match frame {
                Some(f) => m.insert(idx, f),
                None => m.remove(&idx),
            };
        }
    }
}
/// Events recorded in the current second per collector probe: `second << 32 | count`.
static BUDGET: [std::sync::atomic::AtomicU64; 512] = [const { std::sync::atomic::AtomicU64::new(0) }; 512];

#[derive(Default)]
struct ThreadLabel {
    thread: Option<u32>,
    frame: Option<u32>,
    text: Option<String>,
}

thread_local! {
    static LABEL: RefCell<ThreadLabel> = RefCell::new(ThreadLabel::default());
    /// Label text → frame, so setting a label does not take the collector lock.
    static LABEL_FRAMES: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
    /// Probe stacks without the boot classes' leaf frames, by captured stack.
    static TRIMMED: RefCell<HashMap<u32, u32>> = RefCell::new(HashMap::new());
}

/// The calling thread's request label frame.
pub fn current_label_frame() -> Option<u32> {
    LABEL.try_with(|l| l.borrow().frame).ok().flatten()
}

fn current_label_text() -> Option<String> {
    LABEL.try_with(|l| l.borrow().text.clone()).ok().flatten()
}

/// Request label frame of thread `idx` (for samplers walking other threads).
pub fn label_frame_of(idx: u32) -> Option<u32> {
    match LABELS.get(idx as usize) {
        Some(slot) => slot.load(Ordering::Relaxed).checked_sub(1),
        None => LABELS_OVERFLOW.lock().as_ref().and_then(|m| m.get(&idx).copied()),
    }
}

fn label_frame(a: &Agent, text: &str) -> u32 {
    if let Ok(Some(f)) = LABEL_FRAMES.try_with(|m| m.borrow().get(text).copied()) {
        return f;
    }
    let f = {
        let mut c = a.collector.lock();
        let m = c.symbols.intern_method("", &format!("[{text}]"), "");
        c.symbols.intern_frame(m, -1, FrameKind::Unknown)
    };
    let _ = LABEL_FRAMES.try_with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= 1024 {
            m.clear();
        }
        m.insert(text.to_string(), f);
    });
    f
}

pub extern "system" fn register_probe0(
    env: *mut JNIEnv,
    _cls: jclass,
    id: jstring,
    name: jstring,
    payload_label: jstring,
    control_label: jstring,
    visible: jboolean,
) -> jint {
    let s = |v: jstring| jstring_to_string(env, v).unwrap_or_default();
    instrument::register_probe(&s(id), &s(name), &s(payload_label), &s(control_label), visible != 0) as jint
}

#[allow(clippy::too_many_arguments)]
pub extern "system" fn register_interception0(
    env: *mut JNIEnv,
    _cls: jclass,
    probe: jint,
    index: jint,
    type_name: jstring,
    method: jstring,
    desc: jstring,
    flags: jint,
) {
    let s = |v: jstring| jstring_to_string(env, v);
    instrument::register_interception(Interception {
        probe: probe as u32,
        index: index as u32,
        type_name: s(type_name).unwrap_or_default().replace('.', "/"),
        subtypes: flags & F_SUBTYPES != 0,
        method: s(method).unwrap_or_default(),
        desc: s(desc).filter(|d| !d.is_empty()),
        args: flags & F_ARGS != 0,
        entry: flags & F_EXIT_ONLY == 0,
        exit: flags & F_ENTRY_ONLY == 0,
    });
}

pub extern "system" fn ready0(env: *mut JNIEnv, _cls: jclass) {
    if let Some(a) = agent() {
        instrument::ready(a, env);
    }
}

/// `probe << 32 | interception` (interception `0xFFFFFFFF` for custom probes), or -1.
pub extern "system" fn site_info0(_env: *mut JNIEnv, _cls: jclass, site: jint) -> jlong {
    match instrument::site(site as u32) {
        Some((probe, Some(i), _)) => ((probe as i64) << 32) | i as i64,
        Some((probe, None, _)) => ((probe as i64) << 32) | 0xFFFF_FFFF,
        None => -1,
    }
}

pub extern "system" fn site_type0(env: *mut JNIEnv, _cls: jclass, site: jint) -> jstring {
    match instrument::site_check(site as u32) {
        Some(t) => new_string(env, &t),
        None => ptr::null_mut(),
    }
}

pub extern "system" fn site_template0(env: *mut JNIEnv, _cls: jclass, site: jint) -> jstring {
    match instrument::site(site as u32) {
        Some((_, _, Some(t))) => new_string(env, &t),
        _ => ptr::null_mut(),
    }
}

/// Whether a call of `probe` (collector index) taking `duration` is recorded as an event.
fn take_event_budget(a: &Agent, probe: u32, duration: i64) -> bool {
    if duration >= SLOW_NS {
        return true;
    }
    if duration < a.opts.probe_threshold.as_nanos() as i64 {
        return false;
    }
    let Some(b) = BUDGET.get(probe as usize) else { return false };
    let second = (now_nanos() / 1_000_000_000) as u64 & 0xffff_ffff;
    // A lost race drops the event (the call is still aggregated).
    let cur = b.load(Ordering::Relaxed);
    let next = if cur >> 32 != second {
        second << 32 | 1
    } else if cur & 0xffff_ffff >= a.opts.probe_events as u64 {
        return false;
    } else {
        cur + 1
    };
    b.compare_exchange(cur, next, Ordering::Relaxed, Ordering::Relaxed).is_ok()
}

/// The calling thread's index, remembered per thread.
fn thread_index(env: *mut JNIEnv) -> Option<u32> {
    if let Ok(Some(i)) = LABEL.try_with(|l| l.borrow().thread) {
        return Some(i);
    }
    let idx = instrument::calls::current_thread_index(env)?;
    let _ = LABEL.try_with(|l| l.borrow_mut().thread = Some(idx));
    Some(idx)
}

/// Probe calls aggregated by the calling thread: `(second, probe, payload, stack)` → totals.
/// The sampler thread moves them into the collector ([`drain`]), so recording a call does not
/// take the collector lock.
#[derive(Default)]
struct CallBuffer {
    thread: u32,
    calls: HashMap<(i64, u32, String, u32), CallTotals>,
}

#[derive(Default)]
struct CallTotals {
    count: u32,
    duration: i64,
    max: i64,
    errors: u32,
}

/// Every thread's buffer; a buffer only its registry entry holds belongs to a finished thread.
static CALL_BUFFERS: Mutex<Vec<std::sync::Arc<Mutex<CallBuffer>>>> = Mutex::new(Vec::new());

thread_local! {
    static CALLS: RefCell<Option<std::sync::Arc<Mutex<CallBuffer>>>> = const { RefCell::new(None) };
}

fn buffer_call(
    thread: u32,
    second: i64,
    probe: u32,
    payload: String,
    stack: u32,
    duration: i64,
    error: bool,
) {
    let _ = CALLS.try_with(|b| {
        let mut b = b.borrow_mut();
        let buf = b.get_or_insert_with(|| {
            let buf = std::sync::Arc::new(Mutex::new(CallBuffer { thread, calls: HashMap::new() }));
            CALL_BUFFERS.lock().push(buf.clone());
            buf
        });
        let mut buf = buf.lock();
        let t = buf.calls.entry((second, probe, payload, stack)).or_default();
        t.count += 1;
        t.duration += duration;
        t.max = t.max.max(duration);
        t.errors += u32::from(error);
    });
}

/// Moves the threads' aggregated probe calls into the collector. Called periodically by the
/// sampler thread and before batches are cut.
pub fn drain(a: &Agent) {
    let buffers: Vec<_> = {
        let mut all = CALL_BUFFERS.lock();
        let list = all.clone();
        all.retain(|b| std::sync::Arc::strong_count(b) > 2);
        list
    };
    let mut c = a.collector.lock();
    for b in buffers {
        let (thread, calls) = {
            let mut b = b.lock();
            (b.thread, std::mem::take(&mut b.calls))
        };
        for ((second, probe, payload, stack), t) in calls {
            let pid = c.symbols.intern_str(&payload);
            c.add_probe_calls(second, probe, pid, thread, stack, t.count, t.duration, t.max, t.errors);
        }
    }
}

/// Last full stack per (probe, label frame, payload): `(time, stack)`, sharded by hash.
type RecentStacks = HashMap<(u32, u32), HashMap<String, (i64, u32)>>;
static RECENT_STACKS: [Mutex<Option<RecentStacks>>; 16] = [const { Mutex::new(None) }; 16];

/// Stack for a probe call or control object of `probe` with `payload`. Walking the stack is the
/// main cost of a probe call (it grows with the depth: tens of nanoseconds per frame), so a full
/// stack for a (probe, request label, payload) is taken at most once per `probe_stack_interval`
/// (by any thread), and the calls in between are attributed to it: like CPU samples, call trees of
/// probe calls are then exact in total and sampled in their split across call paths. `exact`
/// (events, which are listed with their stacks) always takes a full stack.
fn sampled_probe_stack(a: &Agent, env: *mut JNIEnv, probe: u32, payload: &str, exact: bool) -> u32 {
    use std::hash::{Hash, Hasher};
    let interval = a.opts.probe_stack_interval.as_nanos() as i64;
    if interval == 0 {
        return probe_stack(a, env);
    }
    let label = current_label_frame().unwrap_or(u32::MAX);
    let now = now_nanos();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (probe, label, payload).hash(&mut h);
    let shard = &RECENT_STACKS[h.finish() as usize % RECENT_STACKS.len()];
    if !exact
        && let Some(s) = shard
            .lock()
            .as_ref()
            .and_then(|r| r.get(&(probe, label)))
            .and_then(|m| m.get(payload))
            .filter(|(t, _)| now - t < interval)
            .map(|x| x.1)
    {
        return s;
    }
    let stack = probe_stack(a, env);
    let mut r = shard.lock();
    let r = r.get_or_insert_with(HashMap::new);
    if r.len() >= 1024 {
        r.clear();
    }
    let m = r.entry((probe, label)).or_default();
    if m.len() >= 1024 {
        m.clear();
    }
    match m.get_mut(payload) {
        Some(e) => *e = (now, stack),
        None => {
            m.insert(payload.to_string(), (now, stack));
        }
    }
    stack
}

/// Stack of the calling thread without the boot classes' frames on top.
fn probe_stack(a: &Agent, env: *mut JNIEnv) -> u32 {
    let Some(stack) = capture_stack(a, env, ptr::null_mut()) else { return 0 };
    // The trimmed stack of a captured one never changes; remember it per thread.
    if let Ok(Some(s)) = TRIMMED.try_with(|t| t.borrow().get(&stack).copied()) {
        return s;
    }
    let c = a.collector.lock();
    let mut s = stack;
    while s != 0 {
        let m = c.symbols.method(c.symbols.frame(c.stacks.frame(s)).method);
        if !c.symbols.str(m.class).starts_with("dev.openprofiler.boot.") {
            break;
        }
        s = c.stacks.parent(s);
    }
    drop(c);
    let _ = TRIMMED.try_with(|t| {
        let mut t = t.borrow_mut();
        if t.len() >= 4096 {
            t.clear();
        }
        t.insert(stack, s);
    });
    // Popping only removes leaf frames, so the root-side label (if any) stays.
    s
}

#[allow(clippy::too_many_arguments)]
pub extern "system" fn probe_call0(
    env: *mut JNIEnv,
    _cls: jclass,
    probe: jint,
    payload: jstring,
    start: jlong,
    duration: jlong,
    error: jstring,
    control: jlong,
    detail: jstring,
) {
    let Some(a) = agent() else { return };
    if !ON.load(Ordering::Relaxed) || a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    let probe = instrument::collector_probe(probe as u32);
    if probe == u32::MAX {
        return;
    }
    let Some(thread) = thread_index(env) else { return };
    let payload = jstring_to_string(env, payload).unwrap_or_default();
    let error = jstring_to_string(env, error);
    let event = take_event_budget(a, probe, duration);
    let stack = sampled_probe_stack(a, env, probe, &payload, event);
    if !event {
        buffer_call(
            thread,
            start.div_euclid(1_000_000_000),
            probe,
            payload,
            stack,
            duration,
            error.is_some(),
        );
        return;
    }
    let mut c = a.collector.lock();
    let pid = c.symbols.intern_str(&payload);
    c.add_probe_call(start, probe, pid, thread, stack, duration, error.is_some());
    {
        c.add_probe_event(v1::ProbeEvent {
            start,
            duration,
            probe,
            payload: pid,
            thread,
            stack,
            error: error.unwrap_or_default(),
            control: control as u64,
            detail: jstring_to_string(env, detail).unwrap_or_default(),
            context: current_label_text().unwrap_or_default(),
        });
    }
}

pub extern "system" fn control0(
    env: *mut JNIEnv,
    _cls: jclass,
    probe: jint,
    id: jlong,
    open: jboolean,
    description: jstring,
) {
    let Some(a) = agent() else { return };
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let probe = instrument::collector_probe(probe as u32);
    let Some(thread) = thread_index(env) else { return };
    if probe == u32::MAX {
        return;
    }
    // Where an object was opened matters (leaks); closes carry no stack.
    let description = jstring_to_string(env, description).unwrap_or_default();
    let stack = if open != 0 { sampled_probe_stack(a, env, probe | 1 << 31, &description, false) } else { 0 };
    a.collector.lock().add_control_event(v1::ControlEvent {
        time: now_nanos(),
        probe,
        id: id as u64,
        kind: if open != 0 { 0 } else { 1 },
        description,
        thread,
        stack,
    });
}

/// Sets (or, with null, clears) the calling thread's request label.
pub extern "system" fn set_context0(env: *mut JNIEnv, _cls: jclass, label: jstring) {
    let Some(a) = agent() else { return };
    let text = jstring_to_string(env, label);
    let _ = LABEL.try_with(|l| {
        let mut l = l.borrow_mut();
        if l.thread.is_none() {
            l.thread = instrument::calls::current_thread_index(env);
        }
        let Some(idx) = l.thread else { return };
        l.frame = text.as_deref().map(|t| label_frame(a, t));
        l.text = text;
        publish_label(idx, l.frame);
    });
}

/// `probeCall0` for a call of request `label`, recorded on another thread (asynchronous
/// completions): the label is the call's context and the root of its stack.
#[allow(clippy::too_many_arguments)]
pub extern "system" fn probe_call_labelled0(
    env: *mut JNIEnv,
    cls: jclass,
    probe: jint,
    payload: jstring,
    start: jlong,
    duration: jlong,
    error: jstring,
    control: jlong,
    detail: jstring,
    label: jstring,
) {
    let Some(a) = agent() else { return };
    let text = jstring_to_string(env, label);
    let frame = text.as_deref().map(|t| label_frame(a, t));
    let Ok(previous) = LABEL.try_with(|l| {
        let mut l = l.borrow_mut();
        (std::mem::replace(&mut l.frame, frame), std::mem::replace(&mut l.text, text))
    }) else {
        return;
    };
    probe_call0(env, cls, probe, payload, start, duration, error, control, detail);
    let _ = LABEL.try_with(|l| {
        let mut l = l.borrow_mut();
        (l.frame, l.text) = previous;
    });
}

thread_local! {
    /// Complexity site → method frame, so recording a point does not take the instrumentation lock.
    static COMPLEXITY_FRAMES: RefCell<HashMap<u32, u32>> = RefCell::new(HashMap::new());
}

/// `Hooks.complexity0(site, size, nanos)`: a call of a method configured for complexity analysis.
pub extern "system" fn complexity0(env: *mut JNIEnv, _cls: jclass, site: jint, size: jlong, nanos: jlong) {
    let Some(a) = agent() else { return };
    if size < 0 || a.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    let site = site as u32;
    let frame = COMPLEXITY_FRAMES.try_with(|m| {
        if let Some(&f) = m.borrow().get(&site) {
            return Some(f);
        }
        let f = instrument::complexity_frame(site)?;
        m.borrow_mut().insert(site, f);
        Some(f)
    });
    let Ok(Some(frame)) = frame else { return };
    let Some(thread) = thread_index(env) else { return };
    let time = now_nanos() - nanos;
    a.collector.lock().add_complexity(
        openprofiler_model::ComplexityPoint { time, thread, frame, size: size as u64, duration: nanos },
        a.opts.complexity_rate,
    );
}

/// `Hooks.trigger(site)`: a method trigger fired.
pub extern "system" fn trigger(_env: *mut JNIEnv, _cls: jclass, site: jint) {
    if let Some(id) = instrument::trigger_of_site(site as u32) {
        crate::triggers::on_method(&id);
    }
}

/// Binds the natives of `dev.openprofiler.boot.Hooks`.
pub fn bind(env: *mut JNIEnv) -> bool {
    macro_rules! native {
        ($name:literal, $sig:literal, $f:expr) => {
            JNINativeMethod {
                name: concat!($name, "\0").as_ptr() as *mut _,
                signature: concat!($sig, "\0").as_ptr() as *mut _,
                fnPtr: $f as *mut std::ffi::c_void,
            }
        };
    }
    let cls = jni!(env, FindClass, c"dev/openprofiler/boot/Hooks".as_ptr());
    if cls.is_null() {
        jni!(env, ExceptionClear);
        return false;
    }
    let methods = [
        native!("callEnter", "(I)V", instrument::calls::call_enter),
        native!("callExit", "(I)V", instrument::calls::call_exit),
        native!("trigger", "(I)V", trigger),
        native!(
            "registerProbe0",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Z)I",
            register_probe0
        ),
        native!(
            "registerInterception0",
            "(IILjava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V",
            register_interception0
        ),
        native!("ready0", "()V", ready0),
        native!("siteInfo0", "(I)J", site_info0),
        native!("siteTemplate0", "(I)Ljava/lang/String;", site_template0),
        native!("siteType0", "(I)Ljava/lang/String;", site_type0),
        native!("probeCall0", "(ILjava/lang/String;JJLjava/lang/String;JLjava/lang/String;)V", probe_call0),
        native!(
            "probeCallLabelled0",
            "(ILjava/lang/String;JJLjava/lang/String;JLjava/lang/String;Ljava/lang/String;)V",
            probe_call_labelled0
        ),
        native!("control0", "(IJZLjava/lang/String;)V", control0),
        native!("setContext0", "(Ljava/lang/String;)V", set_context0),
        native!("complexity0", "(IJJ)V", complexity0),
    ];
    let ok = jni!(env, RegisterNatives, cls, methods.as_ptr(), methods.len() as jint) == 0;
    if !ok {
        debug(&format!("cannot bind Hooks natives: {:?}", crate::jvm::take_exception(env)));
    }
    jni!(env, DeleteLocalRef, cls);
    ok
}
