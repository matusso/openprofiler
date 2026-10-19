//! CPU samples on OpenJ9, without `AsyncGetCallTrace`.
//!
//! J9's `AsyncGetCallTrace` walks the interrupted thread's stack inside the signal handler, after
//! pointing the thread's stack roots (`pc`, `sp`, `arg0EA`, `literals`) at a frame it builds on
//! the C stack. Other threads read those roots meanwhile: a thread inside a JVMTI callback runs
//! without VM access, so the GC scans its stack and JVMTI inspects it concurrently, and the
//! sampled thread may itself be in the middle of a stack walk (`GetStackTrace` of an allocation
//! sample). With allocation sampling on, this corrupts the heap.
//!
//! Like async-profiler on OpenJ9, the signal handler instead asks the VM to halt the thread for
//! inspection: it sets `J9_PUBLIC_FLAGS_HALT_THREAD_INSPECTION` and the stack overflow mark that
//! makes the thread check its flags at its next yield point (J9's own way of posting an async
//! event), queues the request and wakes the inspector thread. The inspector takes the stack with
//! `GetStackTrace`, which halts the thread (if it has not halted itself yet), walks it under the
//! VM's own synchronization and clears the flag when resuming it. A sample therefore shows the
//! thread at its first yield point after the timer fired.
//!
//! Every flag the handler sets must be cleared by an inspection, or the thread stays halted:
//! - The ring slot is reserved before the flag is set, so every request is queued.
//! - Only registered threads are flagged; a registered thread has a global reference the
//!   inspector can pass to `GetStackTrace`. A thread unregisters itself at `ThreadEnd` and then
//!   enters the VM once, which waits for a request made before the removal to be served, before
//!   its reference goes away. J9 frees (and reuses) a `J9VMThread` only after its `ThreadEnd`,
//!   so no stale registration can flag another thread. Threads already running at startup are
//!   registered by the starting thread under the lock `ThreadEnd` takes, and skipped if they
//!   ended before.
//! - The inspector only takes locks that no application thread holds across a VM entry (where a
//!   thread halts), so a halted thread cannot keep it from serving requests. A thread inside an
//!   event callback has no VM access, so J9 inspects it without waiting.
//! - A carrier running a virtual thread is not flagged: JVMTI shows only the carrier's own frames,
//!   and walking the virtual thread first waits for a mount or unmount in progress, which a
//!   pending request could hold up. A walker thread takes the virtual thread's stack instead
//!   (J9 halts the carrier for it), soon after the tick, and keeps it only if the thread stayed
//!   mounted throughout (see [`walk_mounted`]). The inspector never waits for the walker.
//! - Before the VM dies, requests stop, handlers in flight are waited for and what is queued is
//!   served.

use crate::jvm::{jni, jvmti};
use crate::{Agent, debug};
use jvmti_sys::*;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// Word offsets into `J9VMThread`, whose address is the thread's `JNIEnv*` (64-bit layout, the
/// same since the first OpenJ9 release).
const SP: usize = 4;
const STACK_OVERFLOW_MARK: usize = 10;
const STACK_OVERFLOW_MARK2: usize = 11;
const PUBLIC_FLAGS: usize = 19;
const J9_PUBLIC_FLAGS_STOPPED: usize = 0x40;
const J9_PUBLIC_FLAGS_HALT_THREAD_INSPECTION: usize = 0x8000;
/// `J9_EVENT_SOM_VALUE`: a stack overflow mark that sends the thread into its async-event check.
const EVENT_SOM: usize = usize::MAX;
/// Registered threads, by `JNIEnv*`: an open-addressed set the signal handler reads.
const KNOWN_SLOTS: usize = 4096;
const KNOWN_PROBE: usize = 64;
const TOMBSTONE: usize = 1;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// Whether the signal handler may request inspections.
static ACCEPT: AtomicBool = AtomicBool::new(false);
/// Signal handlers between checking `ACCEPT` and committing their request.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// eventfd the handler writes to wake the inspector.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);
static KNOWN: [AtomicUsize; KNOWN_SLOTS] = [const { AtomicUsize::new(0) }; KNOWN_SLOTS];
/// Global reference of each registered thread, by `JNIEnv*`. The inspector holds the lock while
/// inspecting, so a reference is not deleted under it.
static REFS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
/// Virtual thread mounted on each carrier, by `JNIEnv*`. JVMTI shows only a carrier's own
/// frames, so the walker takes the mounted virtual thread's. Only the walker holds the lock
/// across a VM entry.
static MOUNTS: Mutex<Option<HashMap<usize, Mount>>> = Mutex::new(None);
static MOUNT_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct Mount {
    /// Global reference of the virtual thread.
    global: usize,
    /// Its thread index.
    idx: u32,
    /// Distinguishes mounts of the same virtual thread on the same carrier.
    generation: u64,
}
/// Serializes registering threads at startup with `ThreadEnd`.
static REGISTRATION: Mutex<Registration> = Mutex::new(Registration { done: false, ended: None });
/// `com.ibm.GetJ9vmThread`.
static GET_J9VMTHREAD: AtomicUsize = AtomicUsize::new(0);

struct Registration {
    /// Whether threads running at startup were registered (or J9 sampling is not used).
    done: bool,
    /// Thread indexes of the threads that ended before.
    ended: Option<HashSet<u32>>,
}

type GetJ9vmThreadFn = unsafe extern "C" fn(*mut jvmtiEnv, jthread, *mut *mut c_void) -> jvmtiError;

/// Whether the VM is OpenJ9 and samples are taken by inspection.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub enum Init {
    NotJ9,
    Ready,
    Unsupported(String),
}

/// Detects OpenJ9 and checks the `J9VMThread` layout on the current thread. When ready, threads
/// already running are registered; requests are accepted after [`accept`]. Either way the
/// startup registration is over afterwards.
pub fn init(a: &Agent, jni: *mut JNIEnv) -> Init {
    let r = try_init(a, jni);
    end_registration();
    r
}

/// Ends the startup registration (also when the async sampler does not start at all).
pub fn end_registration() {
    let mut reg = REGISTRATION.lock();
    reg.done = true;
    reg.ended = None;
}

fn try_init(a: &Agent, jni: *mut JNIEnv) -> Init {
    let Some(f) = extension_function(a, c"com.ibm.GetJ9vmThread") else { return Init::NotJ9 };
    if !cfg!(target_pointer_width = "64") {
        return Init::Unsupported("the J9VMThread layout is only known on 64-bit".into());
    }
    GET_J9VMTHREAD.store(f, Ordering::SeqCst);
    let mut current: jthread = std::ptr::null_mut();
    if jvmti!(a.jvmti, GetCurrentThread, &mut current) != JVMTI_ERROR_NONE || current.is_null() {
        return Init::Unsupported("no current thread".into());
    }
    let env = vm_thread(a, current);
    jni!(jni, DeleteLocalRef, current);
    if env != jni as usize {
        return Init::Unsupported("a thread's J9VMThread is not its JNIEnv".into());
    }
    // SAFETY: `jni` is the current thread's J9VMThread, which is at least this large.
    if !unsafe { layout_plausible(jni) } {
        return Init::Unsupported("unexpected J9VMThread layout".into());
    }
    // SAFETY: plain syscall.
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Init::Unsupported("cannot create an eventfd".into());
    }
    WAKE_FD.store(fd, Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
    // Threads started from now on register at ThreadStart; register the others.
    let mut n = 0;
    let mut threads: *mut jthread = std::ptr::null_mut();
    if jvmti!(a.jvmti, GetAllThreads, &mut n, &mut threads) == JVMTI_ERROR_NONE {
        // SAFETY: JVMTI returned `n` thread references.
        for &t in unsafe { std::slice::from_raw_parts(threads, n.max(0) as usize) } {
            if let Some(idx) = crate::events::thread_index(a, jni, t) {
                // A thread that has not reached `ThreadEnd` is alive and keeps its J9VMThread
                // until it unregisters, which waits for this lock.
                let reg = REGISTRATION.lock();
                if !reg.ended.as_ref().is_some_and(|e| e.contains(&idx)) {
                    register(jni, vm_thread(a, t), t);
                }
            }
            jni!(jni, DeleteLocalRef, t);
        }
        a.jvmti.dealloc(threads);
    }
    Init::Ready
}

/// Stack pointer above the overflow marks, flags of a live thread that is not being inspected.
///
/// # Safety
/// `env` is the current thread's `J9VMThread`.
unsafe fn layout_plausible(env: *mut JNIEnv) -> bool {
    let w = env as *const usize;
    // SAFETY: reads of the current thread's own J9VMThread fields.
    let (sp, som, som2, flags) = unsafe {
        (
            std::ptr::read_volatile(w.add(SP)),
            std::ptr::read_volatile(w.add(STACK_OVERFLOW_MARK)),
            std::ptr::read_volatile(w.add(STACK_OVERFLOW_MARK2)),
            std::ptr::read_volatile(w.add(PUBLIC_FLAGS)),
        )
    };
    debug(&format!("J9VMThread: sp {sp:#x}, overflow marks {som:#x} {som2:#x}, flags {flags:#x}"));
    som2 != 0
        && (som == som2 || som == EVENT_SOM)
        && som2 < sp
        && sp - som2 < 1 << 30
        && flags & (J9_PUBLIC_FLAGS_STOPPED | J9_PUBLIC_FLAGS_HALT_THREAD_INSPECTION) == 0
}

/// Lets the signal handler request inspections (once the inspector runs).
pub fn accept() {
    ACCEPT.store(true, Ordering::SeqCst);
}

/// Stops new requests and waits until no signal handler is about to queue one. Requests queued
/// so far must still be served.
pub fn stop_requests() {
    ACCEPT.store(false, Ordering::SeqCst);
    while IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        std::thread::yield_now();
    }
}

/// `JNIEnv*` (= `J9VMThread*`) of `thread`, 0 if unknown.
fn vm_thread(a: &Agent, thread: jthread) -> usize {
    let f = GET_J9VMTHREAD.load(Ordering::Relaxed);
    if f == 0 {
        return 0;
    }
    let mut env: *mut c_void = std::ptr::null_mut();
    // SAFETY: `f` is `com.ibm.GetJ9vmThread`, implemented as a plain (non-variadic) C function
    // taking these arguments.
    let r = unsafe { std::mem::transmute::<usize, GetJ9vmThreadFn>(f)(a.jvmti.raw(), thread, &mut env) };
    if r == JVMTI_ERROR_NONE { env as usize } else { 0 }
}

/// Makes `thread`, whose `JNIEnv*` is `env`, eligible for inspection.
pub fn register(jni: *mut JNIEnv, env: usize, thread: jthread) {
    if !enabled() || env == 0 {
        return;
    }
    let global = jni!(jni, NewGlobalRef, thread) as usize;
    if global == 0 {
        return;
    }
    let old = REFS.lock().get_or_insert_with(HashMap::new).insert(env, global);
    if let Some(old) = old {
        jni!(jni, DeleteGlobalRef, old as jobject);
    }
    if !known_insert(env) {
        debug("too many threads to register for J9 sampling; some are labelled without stacks");
    }
}

/// `ThreadEnd` of the current thread (index `idx`): no more inspections of it.
pub fn thread_end(a: &Agent, jni: *mut JNIEnv, thread: jthread, idx: Option<u32>) {
    let env = jni as usize;
    {
        let mut reg = REGISTRATION.lock();
        if !reg.done
            && let Some(i) = idx
        {
            reg.ended.get_or_insert_with(HashSet::new).insert(i);
        }
        known_remove(env);
    }
    if !enabled() {
        return;
    }
    // Entering the VM waits while a request made before the removal is pending, and the
    // inspector serves it with the reference still registered.
    let mut state: jint = 0;
    jvmti!(a.jvmti, GetThreadState, thread, &mut state);
    let old = REFS.lock().as_mut().and_then(|m| m.remove(&env));
    if let Some(old) = old {
        jni!(jni, DeleteGlobalRef, old as jobject);
    }
    set_mounted(jni, None);
}

/// Mount (with the virtual thread and its thread index) or unmount callback on the carrier whose
/// `JNIEnv*` is `jni`.
pub fn set_mounted(jni: *mut JNIEnv, vthread: Option<(jthread, u32)>) {
    if !enabled() {
        return;
    }
    let new = vthread.and_then(|(t, idx)| {
        let global = jni!(jni, NewGlobalRef, t) as usize;
        let generation = MOUNT_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        (global != 0).then_some(Mount { global, idx, generation })
    });
    let old = {
        let mut m = MOUNTS.lock();
        let m = m.get_or_insert_with(HashMap::new);
        match new {
            Some(v) => m.insert(jni as usize, v),
            None => m.remove(&(jni as usize)),
        }
    };
    if let Some(old) = old {
        jni!(jni, DeleteGlobalRef, old.global as jobject);
    }
}

/// Counts a signal handler in flight until dropped, which must happen after its request is
/// committed to the ring.
pub struct InFlight(());

impl InFlight {
    pub fn enter() -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        InFlight(())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Signal handler: asks the VM to halt the current thread for inspection. True if the request
/// must be queued for the inspector (and is then served exactly by one `GetStackTrace`); false
/// for threads that are not registered or exiting, which are left untouched.
///
/// # Safety
/// `env` is the current thread's `JNIEnv*`; the caller holds an [`InFlight`] and has reserved
/// the ring slot.
pub unsafe fn request(env: *mut JNIEnv) -> bool {
    if !ACCEPT.load(Ordering::SeqCst) || !known(env as usize) {
        return false;
    }
    let w = env as *const AtomicUsize;
    // SAFETY: `publicFlags` and `stackOverflowMark` are word-sized, word-aligned fields of the
    // current thread's J9VMThread; the VM updates the flags atomically too.
    unsafe {
        let flags = &*w.add(PUBLIC_FLAGS);
        let prev = flags.fetch_or(J9_PUBLIC_FLAGS_HALT_THREAD_INSPECTION, Ordering::SeqCst);
        if prev & J9_PUBLIC_FLAGS_STOPPED != 0 {
            if prev & J9_PUBLIC_FLAGS_HALT_THREAD_INSPECTION == 0 {
                flags.fetch_and(!J9_PUBLIC_FLAGS_HALT_THREAD_INSPECTION, Ordering::SeqCst);
            }
            return false;
        }
        (*w.add(STACK_OVERFLOW_MARK)).store(EVENT_SOM, Ordering::Release);
    }
    true
}

/// Signal handler, after committing a request: wakes the inspector (async-signal-safe).
pub fn wake() {
    let fd = WAKE_FD.load(Ordering::Relaxed);
    let one: u64 = 1;
    // SAFETY: write(2) of 8 bytes to an eventfd is async-signal-safe.
    unsafe { libc::write(fd, &one as *const u64 as *const c_void, 8) };
}

/// Inspector: waits up to `timeout_ms` for a request.
pub fn wait(timeout_ms: i32) {
    let fd = WAKE_FD.load(Ordering::Relaxed);
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let mut count: u64 = 0;
    // SAFETY: valid pollfd and buffer.
    unsafe {
        if libc::poll(&mut p, 1, timeout_ms) > 0 {
            libc::read(fd, &mut count as *mut u64 as *mut c_void, 8);
        }
    }
}

/// Inspector: the stack of the thread whose `JNIEnv*` is `env`, which serves its request.
pub fn inspect(a: &Agent, env: usize, frames: &mut [jvmtiFrameInfo]) -> Result<usize, jvmtiError> {
    let refs = REFS.lock();
    let Some(&thread) = refs.as_ref().and_then(|m| m.get(&env)) else {
        // Not possible by construction: requests are only made for registered threads, which
        // are unregistered after their last request was served.
        debug("J9 inspection request of an unregistered thread");
        return Err(JVMTI_ERROR_THREAD_NOT_ALIVE);
    };
    stack_trace(a, thread as jthread, frames)
}

/// Walker (`jni` is its own `JNIEnv`): the stack of virtual thread `idx`, if it is mounted on the
/// carrier whose `JNIEnv*` is `env` both before and after the walk, so the walk halted the
/// carrier while it ran the virtual thread. J9 first waits for a mount or unmount in progress;
/// a request pending for the carrier (made before the mount was seen) is served by the inspector,
/// which never waits for the walker.
pub fn walk_mounted(
    a: &Agent,
    jni: *mut JNIEnv,
    env: usize,
    idx: u32,
    frames: &mut [jvmtiFrameInfo],
) -> Option<usize> {
    let (vthread, generation) = {
        let m = MOUNTS.lock();
        let m = m.as_ref().and_then(|m| m.get(&env)).filter(|m| m.idx == idx)?;
        (jni!(jni, NewLocalRef, m.global as jobject), m.generation)
    };
    if vthread.is_null() {
        return None;
    }
    let r = stack_trace(a, vthread, frames);
    jni!(jni, DeleteLocalRef, vthread);
    let still = MOUNTS.lock().as_ref().and_then(|m| m.get(&env)).is_some_and(|m| m.generation == generation);
    r.ok().filter(|_| still)
}

fn stack_trace(a: &Agent, thread: jthread, frames: &mut [jvmtiFrameInfo]) -> Result<usize, jvmtiError> {
    let mut count: jint = 0;
    let r = jvmti!(a.jvmti, GetStackTrace, thread, 0, frames.len() as jint, frames.as_mut_ptr(), &mut count);
    if r == JVMTI_ERROR_NONE { Ok(count.max(0) as usize) } else { Err(r) }
}

fn slot_of(env: usize) -> usize {
    ((env as u64 >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 52) as usize
}

/// Async-signal-safe membership test.
fn known(env: usize) -> bool {
    let s = slot_of(env);
    (0..KNOWN_PROBE).any(|i| KNOWN[(s + i) % KNOWN_SLOTS].load(Ordering::Acquire) == env)
}

fn known_insert(env: usize) -> bool {
    if known(env) {
        return true;
    }
    let s = slot_of(env);
    (0..KNOWN_PROBE).any(|i| {
        let slot = &KNOWN[(s + i) % KNOWN_SLOTS];
        [0, TOMBSTONE]
            .into_iter()
            .any(|free| slot.compare_exchange(free, env, Ordering::AcqRel, Ordering::Acquire).is_ok())
    })
}

/// Removes every occurrence (concurrent inserts of one thread may have added two).
fn known_remove(env: usize) {
    let s = slot_of(env);
    for i in 0..KNOWN_PROBE {
        let _ = KNOWN[(s + i) % KNOWN_SLOTS].compare_exchange(
            env,
            TOMBSTONE,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}

/// An extension function by id, freeing JVMTI's descriptions.
fn extension_function(a: &Agent, id: &CStr) -> Option<usize> {
    let mut n: jint = 0;
    let mut infos: *mut jvmtiExtensionFunctionInfo = std::ptr::null_mut();
    if jvmti!(a.jvmti, GetExtensionFunctions, &mut n, &mut infos) != JVMTI_ERROR_NONE || infos.is_null() {
        return None;
    }
    // SAFETY: JVMTI returned `n` entries.
    let list = unsafe { std::slice::from_raw_parts(infos, n.max(0) as usize) };
    let mut found = None;
    for e in list {
        // SAFETY: ids are NUL-terminated strings.
        if !e.id.is_null() && unsafe { CStr::from_ptr(e.id) } == id {
            found = e.func.map(|f| f as usize);
        }
        if !e.params.is_null() {
            // SAFETY: `params` has `param_count` entries.
            for p in unsafe { std::slice::from_raw_parts(e.params, e.param_count.max(0) as usize) } {
                a.jvmti.dealloc(p.name);
            }
        }
        a.jvmti.dealloc(e.params);
        a.jvmti.dealloc(e.errors);
        a.jvmti.dealloc(e.id);
        a.jvmti.dealloc(e.short_description);
    }
    a.jvmti.dealloc(infos);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_set_inserts_and_removes() {
        let (a, b) = (0x7f00_1234_5600, 0x7f00_1234_5600 + (KNOWN_SLOTS << 4));
        assert!(!known(a));
        assert!(known_insert(a) && known_insert(b) && known_insert(a));
        assert!(known(a) && known(b));
        known_remove(a);
        assert!(!known(a) && known(b));
        assert!(known_insert(a) && known(a));
        known_remove(a);
        known_remove(b);
        assert!(!known(a) && !known(b));
    }
}
