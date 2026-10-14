//! Linux asynchronous CPU sampler.
//!
//! Each thread gets a POSIX timer on its own CPU-time clock (`timer_create` with
//! `SIGEV_THREAD_ID`), so only threads that actually burn CPU are interrupted and no safepoint
//! bias is introduced. The `SIGPROF` handler calls HotSpot's `AsyncGetCallTrace` and copies the
//! raw frames into a lock-free bounded ring (Vyukov MPMC); a drainer thread resolves and
//! aggregates them. Signals that are not ours are forwarded to the previously installed handler.

use crate::collector::now_nanos;
use crate::jvm::jni;
use crate::{Agent, agent, log};
use ember_model::{FrameKind, SampleKind, ThreadState};
use jvmti_sys::*;
use parking_lot::Mutex;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

const MAX_FRAMES: usize = 128;
const RING_SIZE: usize = 2048;
/// `sigev_value` marker identifying our timers.
const TIMER_MAGIC: usize = 0x454d_4252;

struct Slot {
    seq: AtomicUsize,
    tid: UnsafeCell<i32>,
    time: UnsafeCell<i64>,
    n: UnsafeCell<i32>,
    frames: UnsafeCell<[ASGCT_CallFrame; MAX_FRAMES]>,
}

struct Ring {
    slots: Box<[Slot]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: slot payloads are only accessed by the producer that reserved the slot (between reserve
// and commit) or by the single consumer after commit; `seq` orders these accesses.
unsafe impl Sync for Ring {}
// SAFETY: the raw jmethodID pointers inside slots are opaque VM handles valid on any thread.
unsafe impl Send for Ring {}

impl Ring {
    fn new() -> Self {
        let slots = (0..RING_SIZE)
            .map(|i| Slot {
                seq: AtomicUsize::new(i),
                tid: UnsafeCell::new(0),
                time: UnsafeCell::new(0),
                n: UnsafeCell::new(0),
                frames: UnsafeCell::new(
                    [ASGCT_CallFrame { lineno: 0, method_id: std::ptr::null_mut() }; MAX_FRAMES],
                ),
            })
            .collect();
        Self { slots, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    /// Reserves a slot (async-signal-safe: atomics only). `None` if the ring is full.
    fn reserve(&self) -> Option<(&Slot, usize)> {
        let mut pos = self.head.load(Ordering::Relaxed);
        loop {
            let slot = &self.slots[pos % RING_SIZE];
            let seq = slot.seq.load(Ordering::Acquire);
            let diff = seq as isize - pos as isize;
            if diff == 0 {
                match self.head.compare_exchange_weak(pos, pos + 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return Some((slot, pos)),
                    Err(cur) => pos = cur,
                }
            } else if diff < 0 {
                return None;
            } else {
                pos = self.head.load(Ordering::Relaxed);
            }
        }
    }

    fn commit(slot: &Slot, pos: usize) {
        slot.seq.store(pos + 1, Ordering::Release);
    }

    /// Single consumer: pops one committed sample.
    fn pop(&self, out: &mut Vec<ASGCT_CallFrame>) -> Option<(i32, i64, i32)> {
        let pos = self.tail.load(Ordering::Relaxed);
        let slot = &self.slots[pos % RING_SIZE];
        if slot.seq.load(Ordering::Acquire) != pos + 1 {
            return None;
        }
        // SAFETY: the slot was committed by its producer and is owned by the consumer until `seq`
        // is advanced below.
        let r = unsafe {
            let n = *slot.n.get();
            out.clear();
            if n > 0 {
                let frames = &*slot.frames.get();
                out.extend_from_slice(&frames[..n as usize]);
            }
            (*slot.tid.get(), *slot.time.get(), n)
        };
        slot.seq.store(pos + RING_SIZE, Ordering::Release);
        self.tail.store(pos + 1, Ordering::Relaxed);
        Some(r)
    }
}

static RING: OnceLock<Ring> = OnceLock::new();
static ASGCT: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static INTERVAL_NS: AtomicU64 = AtomicU64::new(10_000_000);
static PREV: OnceLock<PrevHandler> = OnceLock::new();
static TIMERS: Mutex<Option<HashMap<i32, usize>>> = Mutex::new(None);

struct PrevHandler(libc::sigaction);
// SAFETY: a copied sigaction struct is plain data.
unsafe impl Send for PrevHandler {}
// SAFETY: read-only after initialization.
unsafe impl Sync for PrevHandler {}

fn gettid() -> i32 {
    // SAFETY: gettid has no preconditions and is async-signal-safe.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

fn realtime_nanos() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime is async-signal-safe; `ts` is a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    // `time_t`/`c_long` are 32-bit on armhf, hence the (otherwise redundant) widening casts.
    #[allow(clippy::unnecessary_cast)]
    let nanos = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
    nanos
}

extern "C" fn on_sigprof(sig: libc::c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    // SAFETY: errno location is thread-local and valid for the current thread.
    let saved_errno = unsafe { *libc::__errno_location() };
    // SAFETY: `info` is provided by the kernel for SA_SIGINFO handlers.
    let ours = !info.is_null()
        && unsafe {
            (*info).si_code == libc::SI_TIMER && (*info).si_value().sival_ptr as usize == TIMER_MAGIC
        };
    if !ours {
        chain(sig, info, uc);
    } else {
        sample(uc);
    }
    // SAFETY: as above.
    unsafe { *libc::__errno_location() = saved_errno };
}

fn chain(sig: libc::c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    let Some(prev) = PREV.get() else { return };
    let h = prev.0.sa_sigaction;
    if h == libc::SIG_DFL || h == libc::SIG_IGN {
        return;
    }
    if prev.0.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: the previous handler was installed with SA_SIGINFO and expects these arguments.
        let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut c_void) =
            unsafe { std::mem::transmute(h) };
        f(sig, info, uc);
    } else {
        // SAFETY: plain signal handler signature.
        let f: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(h) };
        f(sig);
    }
}

fn sample(uc: *mut c_void) {
    let (Some(ring), Some(a)) = (RING.get(), agent()) else { return };
    let f = ASGCT.load(Ordering::Relaxed);
    if f == 0 || !a.cpu_on.load(Ordering::Relaxed) {
        return;
    }
    let Some((slot, pos)) = ring.reserve() else {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let env = a.vm.env().unwrap_or(std::ptr::null_mut());
    // SAFETY: `f` is HotSpot's AsyncGetCallTrace resolved via dlsym; the slot is reserved for us
    // until commit; `uc` is the signal context.
    unsafe {
        let asgct: AsyncGetCallTraceFn = std::mem::transmute(f);
        let mut trace = ASGCT_CallTrace { env, num_frames: 0, frames: (*slot.frames.get()).as_mut_ptr() };
        if env.is_null() {
            trace.num_frames = ASGCT_UNKNOWN_NOT_JAVA;
        } else {
            asgct(&mut trace, MAX_FRAMES as jint, uc);
        }
        *slot.n.get() = trace.num_frames;
        *slot.tid.get() = gettid();
        *slot.time.get() = realtime_nanos();
    }
    Ring::commit(slot, pos);
}

/// Starts the async sampler if possible; on failure the sync sampler keeps producing CPU samples.
pub fn start(a: &'static Agent) {
    if !a.async_possible() {
        return;
    }
    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name.
    let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"AsyncGetCallTrace".as_ptr()) };
    if sym.is_null() {
        if a.opts.sampler == crate::SamplerChoice::Async {
            log("AsyncGetCallTrace not found; using the sync sampler");
        }
        return;
    }
    let _ = RING.set(Ring::new());
    ASGCT.store(sym as usize, Ordering::SeqCst);
    INTERVAL_NS.store(a.interval_ns.load(Ordering::SeqCst), Ordering::SeqCst);
    // SAFETY: installs a SA_SIGINFO handler; the previous action is saved for chaining.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigprof as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGPROF, &sa, &mut old) != 0 {
            log("cannot install SIGPROF handler; using the sync sampler");
            return;
        }
        let _ = PREV.set(PrevHandler(old));
    }
    if old_handler_is_foreign() {
        log("another SIGPROF handler was installed (another profiler?); chaining to it");
    }
    ACTIVE.store(true, Ordering::SeqCst);
    a.async_active.store(true, Ordering::SeqCst);
    // Existing threads: arm a timer per task.
    if let Ok(dir) = std::fs::read_dir("/proc/self/task") {
        for e in dir.flatten() {
            if let Some(tid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
                arm(tid);
            }
        }
    }
    let r = std::thread::Builder::new().name("ember-async-drain".into()).spawn(move || drain_loop(a));
    if let Err(e) = r {
        log(&format!("cannot start async drainer: {e}"));
    }
}

fn old_handler_is_foreign() -> bool {
    PREV.get().is_some_and(|p| p.0.sa_sigaction != libc::SIG_DFL && p.0.sa_sigaction != libc::SIG_IGN)
}

fn arm(tid: i32) {
    let interval = INTERVAL_NS.load(Ordering::Relaxed) as i64;
    // CPU-time clock of thread `tid`: MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED).
    let clock = (((!(tid as u32)) << 3) | 6) as libc::clockid_t;
    // SAFETY: zeroed sigevent is valid; fields set below match SIGEV_THREAD_ID semantics.
    unsafe {
        let mut sev: libc::sigevent = std::mem::zeroed();
        sev.sigev_notify = libc::SIGEV_THREAD_ID;
        sev.sigev_signo = libc::SIGPROF;
        sev.sigev_value = libc::sigval { sival_ptr: TIMER_MAGIC as *mut c_void };
        sev.sigev_notify_thread_id = tid;
        let mut timer: libc::timer_t = std::ptr::null_mut();
        if libc::timer_create(clock, &mut sev, &mut timer) != 0 {
            return;
        }
        let ts = libc::timespec { tv_sec: interval / 1_000_000_000, tv_nsec: interval % 1_000_000_000 };
        let spec = libc::itimerspec { it_interval: ts, it_value: ts };
        if libc::timer_settime(timer, 0, &spec, std::ptr::null_mut()) != 0 {
            libc::timer_delete(timer);
            return;
        }
        TIMERS.lock().get_or_insert_with(HashMap::new).insert(tid, timer as usize);
    }
}

fn disarm(tid: i32) {
    if let Some(t) = TIMERS.lock().as_mut().and_then(|m| m.remove(&tid)) {
        // SAFETY: `t` is a timer created by `arm` and not yet deleted.
        unsafe { libc::timer_delete(t as libc::timer_t) };
    }
}

/// ThreadStart runs on the new thread itself, so its tid is ours to read.
pub fn on_thread_start(a: &Agent, idx: Option<u32>) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let tid = gettid();
    if let Some(i) = idx {
        a.collector.lock().set_os_id(i, tid as u64);
    }
    arm(tid);
}

pub fn on_thread_end() {
    if ACTIVE.load(Ordering::Relaxed) {
        disarm(gettid());
    }
}

fn comm(tid: i32) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn error_label(code: i32) -> &'static str {
    match code {
        ASGCT_NO_JAVA_FRAME => "[no Java frame]",
        ASGCT_NO_CLASS_LOAD => "[no class load]",
        ASGCT_GC_ACTIVE => "[GC active]",
        ASGCT_UNKNOWN_NOT_JAVA | ASGCT_NOT_WALKABLE_NOT_JAVA => "[non-Java thread]",
        ASGCT_UNKNOWN_JAVA => "[unknown Java]",
        ASGCT_NOT_WALKABLE_JAVA => "[not walkable Java]",
        ASGCT_THREAD_EXIT => "[thread exit]",
        ASGCT_DEOPT => "[deoptimization]",
        ASGCT_SAFEPOINT => "[safepoint]",
        _ => "[unknown]",
    }
}

fn drain_loop(a: &'static Agent) {
    let Some(jni) = a.vm.attach_daemon("Ember Async Drainer") else { return };
    let ring = RING.get().unwrap();
    let mut frames = Vec::with_capacity(MAX_FRAMES);
    let mut ids = Vec::with_capacity(MAX_FRAMES);
    while !a.shutting_down.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
        if jni!(jni, PushLocalFrame, 256) != 0 {
            jni!(jni, ExceptionClear);
            continue;
        }
        let weight = INTERVAL_NS.load(Ordering::Relaxed);
        let mut guard = a.collector.lock();
        let c = &mut *guard;
        while let Some((tid, time, n)) = ring.pop(&mut frames) {
            let thread = match c.thread_by_os_id(tid as u64) {
                Some(i) => i,
                None => {
                    let name = comm(tid);
                    if name.starts_with("Ember") {
                        continue;
                    }
                    match c.thread_by_comm(&name) {
                        Some(i) => {
                            c.set_os_id(i, tid as u64);
                            i
                        }
                        None => {
                            let i = c.new_thread(0, name, true, now_nanos());
                            c.set_os_id(i, tid as u64);
                            i
                        }
                    }
                }
            };
            ids.clear();
            if n > 0 {
                for f in &frames {
                    let kind = if f.lineno == -3 { FrameKind::Native } else { FrameKind::Unknown };
                    let bci = if f.lineno >= 0 { f.lineno as i64 } else { -1 };
                    ids.push(c.resolver.frame(a.jvmti, &mut c.symbols, f.method_id, bci, kind));
                }
            } else {
                ids.push(c.synthetic_frame(error_label(n)));
            }
            let stack = c.stacks.intern_leaf_first(&ids);
            c.add_sample(time, SampleKind::Cpu, thread, ThreadState::Runnable, stack, weight, None);
        }
        c.dropped_samples = DROPPED.load(Ordering::Relaxed);
        drop(guard);
        jni!(jni, PopLocalFrame, std::ptr::null_mut());
    }
    a.vm.detach();
}
