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
/// Calls at least this slow are always recorded as events.
const SLOW_NS: i64 = 100_000_000;

/// Request label frame per thread index, read by the samplers.
static LABELS: Mutex<Option<HashMap<u32, u32>>> = Mutex::new(None);
/// Events recorded in the current second per probe: `(second, counts)`.
static BUDGET: Mutex<(i64, Vec<u32>)> = Mutex::new((0, Vec::new()));

#[derive(Default)]
struct ThreadLabel {
    thread: Option<u32>,
    frame: Option<u32>,
    text: Option<String>,
}

thread_local! {
    static LABEL: RefCell<ThreadLabel> = RefCell::new(ThreadLabel::default());
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
    LABELS.lock().as_ref().and_then(|m| m.get(&idx).copied())
}

fn label_frame(a: &Agent, text: &str) -> u32 {
    let mut c = a.collector.lock();
    let m = c.symbols.intern_method("", &format!("[{text}]"), "");
    c.symbols.intern_frame(m, -1, FrameKind::Unknown)
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
    let second = now_nanos() / 1_000_000_000;
    let mut b = BUDGET.lock();
    if b.0 != second {
        b.0 = second;
        b.1.clear();
    }
    let i = probe as usize;
    if b.1.len() <= i {
        b.1.resize(i + 1, 0);
    }
    if b.1[i] >= a.opts.probe_events {
        return false;
    }
    b.1[i] += 1;
    true
}

/// Stack of the calling thread without the boot classes' frames on top.
fn probe_stack(a: &Agent, env: *mut JNIEnv) -> u32 {
    let Some(stack) = capture_stack(a, env, ptr::null_mut()) else { return 0 };
    let c = a.collector.lock();
    let mut s = stack;
    while s != 0 {
        let m = c.symbols.method(c.symbols.frame(c.stacks.frame(s)).method);
        if !c.symbols.str(m.class).starts_with("dev.openprofiler.boot.") {
            break;
        }
        s = c.stacks.parent(s);
    }
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
    let Some(thread) = instrument::calls::current_thread_index(env) else { return };
    let payload = jstring_to_string(env, payload).unwrap_or_default();
    let error = jstring_to_string(env, error);
    let stack = probe_stack(a, env);
    let event = take_event_budget(a, probe, duration);
    let mut c = a.collector.lock();
    let pid = c.symbols.intern_str(&payload);
    c.add_probe_call(start, probe, pid, thread, stack, duration, error.is_some());
    if event {
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
    let Some(thread) = instrument::calls::current_thread_index(env) else { return };
    if probe == u32::MAX {
        return;
    }
    let stack = probe_stack(a, env);
    a.collector.lock().add_control_event(v1::ControlEvent {
        time: now_nanos(),
        probe,
        id: id as u64,
        kind: if open != 0 { 0 } else { 1 },
        description: jstring_to_string(env, description).unwrap_or_default(),
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
        let mut m = LABELS.lock();
        let m = m.get_or_insert_with(HashMap::new);
        match l.frame {
            Some(f) => m.insert(idx, f),
            None => m.remove(&idx),
        };
    });
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
        native!("probeCall0", "(ILjava/lang/String;JJLjava/lang/String;JLjava/lang/String;)V", probe_call0),
        native!("control0", "(IJZLjava/lang/String;)V", control0),
        native!("setContext0", "(Ljava/lang/String;)V", set_context0),
    ];
    let ok = jni!(env, RegisterNatives, cls, methods.as_ptr(), methods.len() as jint) == 0;
    if !ok {
        debug(&format!("cannot bind Hooks natives: {:?}", crate::jvm::take_exception(env)));
    }
    jni!(env, DeleteLocalRef, cls);
    ok
}
