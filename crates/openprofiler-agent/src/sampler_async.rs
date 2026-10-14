//! Linux asynchronous CPU sampler.
//!
//! Each thread gets a POSIX timer on its own CPU-time clock (`timer_create` with
//! `SIGEV_THREAD_ID`), so only threads that actually burn CPU are interrupted and no safepoint
//! bias is introduced. The `SIGPROF` handler calls HotSpot's `AsyncGetCallTrace` and copies the
//! raw frames into a lock-free bounded ring (Vyukov MPMC); a drainer thread resolves and
//! aggregates them. Signals that are not ours are forwarded to the previously installed handler.
//!
//! `AsyncGetCallTrace` gives up on two common situations, which the handler repairs and retries:
//! - The thread is in Java code but the PC is in a frameless stub (array zeroing/copying,
//!   itable/vtable stubs) or in a method prologue/epilogue (`ticks_unknown_Java`). The handler
//!   unwinds one frame on a copy of the signal context and calls it again.
//! - The thread is in the VM, called from Java through a path that leaves the last-Java-frame
//!   anchor without a PC (`ticks_unknown_not_Java`). Like HotSpot's own `make_walkable()`, the
//!   handler fills in the PC from the anchor's SP for the duration of the call (offsets come from
//!   `gHotSpotVMStructs`).
//!
//! Threads without Java frames (JIT compiler, GC, VM thread) are labelled by role.

use crate::collector::now_nanos;
use crate::jvm::jni;
use crate::{Agent, agent, log};
use jvmti_sys::*;
use openprofiler_model::{FrameKind, SampleKind, ThreadState};
use parking_lot::Mutex;
use std::cell::UnsafeCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

const MAX_FRAMES: usize = 128;
const RING_SIZE: usize = 2048;
/// `sigev_value` marker identifying our timers.
const TIMER_MAGIC: usize = 0x454d_4252;
/// Sample code (beside the `ASGCT_*` codes) for a Java thread that has no Java frames at all,
/// such as a JIT compiler thread.
const NO_JAVA_FRAMES: i32 = -100;
/// Upper bound for the distance between SP and a frame pointer worth following.
const MAX_FRAME_SPAN: usize = 8 << 20;

struct Slot {
    seq: AtomicUsize,
    tid: UnsafeCell<i32>,
    time: UnsafeCell<i64>,
    n: UnsafeCell<i32>,
    /// Thread index + 1 of the virtual thread mounted on the sampled carrier, 0 if none.
    vthread: UnsafeCell<u32>,
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
                vthread: UnsafeCell::new(0),
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
    fn pop(&self, out: &mut Vec<ASGCT_CallFrame>) -> Option<(i32, i64, i32, u32)> {
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
            (*slot.tid.get(), *slot.time.get(), n, *slot.vthread.get())
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
/// The agent's own threads: never armed.
static OWN_TIDS: Mutex<Option<HashSet<i32>>> = Mutex::new(None);
/// Whether failed walks are repaired and retried (`recover` option).
static RECOVER: AtomicBool = AtomicBool::new(false);
/// Offsets from a thread's `JNIEnv*` to its anchor's `_last_Java_sp` / `_last_Java_pc`; 0 when
/// unknown.
static ENV_TO_SP: AtomicIsize = AtomicIsize::new(0);
static ENV_TO_PC: AtomicIsize = AtomicIsize::new(0);
static PID: AtomicI32 = AtomicI32::new(0);
static CODE_CACHE: OnceLock<crate::vmstructs::CodeCache> = OnceLock::new();
/// Samples repaired by unwinding a stub frame / by making the anchor walkable.
static RECOVERED_STUB: AtomicU64 = AtomicU64::new(0);
static RECOVERED_ANCHOR: AtomicU64 = AtomicU64::new(0);

/// Virtual thread mounted on each carrier as `tid << 32 | (thread index + 1)`. Slots are found by
/// probing from `tid % MOUNT_SLOTS`; a carrier writes only its own entry (mount callbacks run on
/// the carrier) and its signal handler reads it.
const MOUNT_SLOTS: usize = 1024;
const MOUNT_PROBE: usize = 16;
static MOUNTED: [AtomicU64; MOUNT_SLOTS] = [const { AtomicU64::new(0) }; MOUNT_SLOTS];

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
    let tid = gettid();
    // SAFETY: `f` is HotSpot's AsyncGetCallTrace resolved via dlsym; the slot is reserved for us
    // until commit; `uc` is the signal context.
    unsafe {
        let asgct: AsyncGetCallTraceFn = std::mem::transmute(f);
        let mut trace = ASGCT_CallTrace { env, num_frames: 0, frames: (*slot.frames.get()).as_mut_ptr() };
        if env.is_null() {
            trace.num_frames = ASGCT_UNKNOWN_NOT_JAVA;
        } else {
            asgct(&mut trace, MAX_FRAMES as jint, uc);
            if trace.num_frames < 0 && RECOVER.load(Ordering::Relaxed) {
                recover(asgct, &mut trace, uc);
            }
        }
        *slot.n.get() = trace.num_frames;
        *slot.tid.get() = tid;
        *slot.time.get() = realtime_nanos();
        *slot.vthread.get() = mounted(tid);
    }
    Ring::commit(slot, pos);
}

/// Repairs the two walk failures described in the module docs and retries. Leaves the original
/// error code in `trace` if that does not help.
///
/// # Safety
/// Signal-handler context of a Java thread; `trace.env` is that thread's `JNIEnv*`.
unsafe fn recover(asgct: AsyncGetCallTraceFn, trace: &mut ASGCT_CallTrace, uc: *mut c_void) {
    let code = trace.num_frames;
    match code {
        ASGCT_UNKNOWN_NOT_JAVA | ASGCT_NOT_WALKABLE_NOT_JAVA => {
            let (to_sp, to_pc) = (ENV_TO_SP.load(Ordering::Relaxed), ENV_TO_PC.load(Ordering::Relaxed));
            if to_sp == 0 {
                return;
            }
            let env = trace.env as isize;
            let sp_slot = (env + to_sp) as *const usize;
            let pc_slot = (env + to_pc) as *mut usize;
            // SAFETY: both slots are fields of the current thread's JavaThread (offsets from
            // VMStructs). The anchor is only modified by this thread, which is interrupted, and
            // other threads that walk it compute the same PC from SP.
            unsafe {
                let sp = std::ptr::read_volatile(sp_slot);
                if sp == 0 {
                    if code == ASGCT_UNKNOWN_NOT_JAVA {
                        trace.num_frames = NO_JAVA_FRAMES;
                    }
                    return;
                }
                let mut pc = std::ptr::read_volatile(pc_slot);
                let patched = pc == 0;
                if patched {
                    // What JavaFrameAnchor::make_walkable() does: the return address into the
                    // last Java frame is stored just below its SP.
                    pc = *(sp as *const usize).sub(1);
                    std::ptr::write_volatile(pc_slot, pc);
                    asgct(trace, MAX_FRAMES as jint, uc);
                }
                // The last Java frame is a C2 runtime stub whose frame HotSpot marks as never
                // safe to walk, although it is complete whenever the anchor points into it
                // (async-profiler applies the same fix).
                if trace.num_frames == ASGCT_NOT_WALKABLE_NOT_JAVA
                    && let Some(cc) = CODE_CACHE.get()
                    && let Some(blob) = cc.find_blob(pc)
                {
                    let (complete, size) = cc.frame_info(blob);
                    if complete == -1 && size > 0 {
                        cc.set_frame_complete(blob, 0);
                        asgct(trace, MAX_FRAMES as jint, uc);
                    }
                }
                if patched {
                    std::ptr::write_volatile(pc_slot, 0);
                }
            }
            if trace.num_frames > 0 {
                RECOVERED_ANCHOR.fetch_add(1, Ordering::Relaxed);
            } else {
                trace.num_frames = code;
            }
        }
        ASGCT_UNKNOWN_JAVA | ASGCT_NOT_WALKABLE_JAVA => {
            // SAFETY: forwarded from the caller.
            if unsafe { unwind_and_retry(asgct, trace, uc) } {
                RECOVERED_STUB.fetch_add(1, Ordering::Relaxed);
            } else {
                trace.num_frames = code;
            }
        }
        _ => {}
    }
}

/// Reads a word that may not be mapped, without faulting (`process_vm_readv` on ourselves).
fn safe_read(addr: usize) -> Option<usize> {
    let mut v = 0usize;
    let local = libc::iovec { iov_base: &mut v as *mut usize as *mut c_void, iov_len: size_of::<usize>() };
    let remote = libc::iovec { iov_base: addr as *mut c_void, iov_len: size_of::<usize>() };
    // SAFETY: the kernel validates `remote`; `local` is our own variable. Async-signal-safe.
    let n = unsafe { libc::process_vm_readv(PID.load(Ordering::Relaxed), &local, 1, &remote, 1, 0) };
    (n == size_of::<usize>() as isize).then_some(v)
}

/// The frame record `(saved fp, return address)` at `fp`, if `fp` plausibly points into the
/// stack above `sp`.
fn frame_record(sp: usize, fp: usize) -> Option<(usize, usize)> {
    if fp <= sp || fp - sp > MAX_FRAME_SPAN || !fp.is_multiple_of(size_of::<usize>()) {
        return None;
    }
    Some((safe_read(fp)?, safe_read(fp + size_of::<usize>())?))
}

/// Unwinds the top frame of the interrupted context in a few plausible ways and retries the walk
/// from each until one succeeds. `AsyncGetCallTrace` validates the frame it is given against the
/// code cache and the thread's stack, so a wrong guess fails rather than producing a bogus trace.
///
/// # Safety
/// Signal-handler context; `uc` is the handler's `ucontext_t`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe fn unwind_and_retry(asgct: AsyncGetCallTraceFn, trace: &mut ASGCT_CallTrace, uc: *mut c_void) -> bool {
    // SAFETY: `uc` points to the kernel-provided ucontext_t; a bitwise copy is a valid context.
    let mut ctx: libc::ucontext_t = unsafe { std::ptr::read(uc as *const libc::ucontext_t) };
    let mut candidates: [Option<(usize, usize, usize)>; 3] = [None; 3];
    #[cfg(target_arch = "x86_64")]
    {
        let g = &ctx.uc_mcontext.gregs;
        let (sp, fp) = (g[libc::REG_RSP as usize] as usize, g[libc::REG_RBP as usize] as usize);
        // Frameless stub or prologue/epilogue: the return address is on top of the stack.
        // SAFETY: SP of the interrupted code points into this thread's mapped stack.
        let ret = unsafe { *(sp as *const usize) };
        candidates[0] = Some((ret, sp + 8, fp));
        // Stub that set up an RBP frame (`enter`).
        candidates[1] = frame_record(sp, fp).map(|(saved, ret)| (ret, fp + 16, saved));
    }
    #[cfg(target_arch = "aarch64")]
    {
        let m = &ctx.uc_mcontext;
        let (sp, fp, lr) = (m.sp as usize, m.regs[29] as usize, m.regs[30] as usize);
        let record = frame_record(sp, fp);
        match record {
            // Stub that pushed a frame record (`enter`) and has not called anything since.
            Some((saved, ret)) if ret == lr => {
                candidates[0] = Some((ret, fp + 16, saved));
                candidates[1] = Some((lr, sp, fp));
            }
            _ => {
                // Leaf stub without a frame: the return address is still in LR.
                candidates[0] = Some((lr, sp, fp));
                candidates[1] = record.map(|(saved, ret)| (ret, fp + 16, saved));
            }
        }
    }
    for (pc, sp, fp) in candidates.into_iter().flatten() {
        if pc == 0 {
            continue;
        }
        #[cfg(target_arch = "x86_64")]
        {
            let g = &mut ctx.uc_mcontext.gregs;
            g[libc::REG_RIP as usize] = pc as i64;
            g[libc::REG_RSP as usize] = sp as i64;
            g[libc::REG_RBP as usize] = fp as i64;
        }
        #[cfg(target_arch = "aarch64")]
        {
            let m = &mut ctx.uc_mcontext;
            m.pc = pc as u64;
            m.sp = sp as u64;
            m.regs[29] = fp as u64;
        }
        // SAFETY: a valid context for the current thread with adjusted registers.
        unsafe { asgct(trace, MAX_FRAMES as jint, &mut ctx as *mut libc::ucontext_t as *mut c_void) };
        if trace.num_frames > 0 {
            return true;
        }
    }
    false
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
unsafe fn unwind_and_retry(_: AsyncGetCallTraceFn, _: &mut ASGCT_CallTrace, _: *mut c_void) -> bool {
    false
}

/// Records the virtual thread (thread index) mounted on the calling carrier, `None` on unmount.
pub fn set_mounted(vthread: Option<u32>) {
    let tid = gettid() as u32 as u64;
    let value = (tid << 32) | vthread.map_or(0, |v| v as u64 + 1);
    let start = tid as usize % MOUNT_SLOTS;
    for i in 0..MOUNT_PROBE {
        let s = &MOUNTED[(start + i) % MOUNT_SLOTS];
        let cur = s.load(Ordering::Relaxed);
        if cur >> 32 == tid {
            s.store(value, Ordering::Relaxed);
            return;
        }
        if cur == 0 && s.compare_exchange(0, value, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            return;
        }
    }
}

/// Thread index + 1 of the virtual thread mounted on carrier `tid`, 0 if none (async-signal-safe).
fn mounted(tid: i32) -> u32 {
    let tid = tid as u32 as u64;
    let start = tid as usize % MOUNT_SLOTS;
    (0..MOUNT_PROBE)
        .map(|i| MOUNTED[(start + i) % MOUNT_SLOTS].load(Ordering::Relaxed))
        .find(|v| v >> 32 == tid)
        .map_or(0, |v| v as u32)
}

fn clear_mounted(tid: i32) {
    let tid = tid as u32 as u64;
    let start = tid as usize % MOUNT_SLOTS;
    for i in 0..MOUNT_PROBE {
        let s = &MOUNTED[(start + i) % MOUNT_SLOTS];
        if s.load(Ordering::Relaxed) >> 32 == tid {
            s.store(0, Ordering::Relaxed);
        }
    }
}

/// Starts the async sampler if possible; on failure the sync sampler keeps producing CPU samples.
pub fn start(a: &'static Agent, jni: *mut JNIEnv) {
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
    PID.store(std::process::id() as i32, Ordering::SeqCst);
    if a.opts.recover {
        init_anchor_offsets(jni);
        RECOVER.store(true, Ordering::SeqCst);
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
    let r = std::thread::Builder::new().name("openprofiler-async-drain".into()).spawn(move || drain_loop(a));
    if let Err(e) = r {
        log(&format!("cannot start async drainer: {e}"));
    }
}

/// Finds a thread's last-Java-frame anchor relative to its `JNIEnv*`: `java.lang.Thread.eetop`
/// holds the current `JavaThread*`, VMStructs the anchor's field offsets.
fn init_anchor_offsets(jni: *mut JNIEnv) {
    match crate::vmstructs::CodeCache::load() {
        Some(c) => {
            let _ = CODE_CACHE.set(c);
        }
        None => crate::debug("VMStructs: code cache layout unknown; runtime-stub recovery disabled"),
    }
    let v = crate::vmstructs::VmStructs::load(&["JavaThread", "JavaFrameAnchor"]);
    let get = |t: &str, n: &str| v.offset(t, n).map(|o| o as isize);
    let (Some(anchor), Some(sp), Some(pc)) = (
        get("JavaThread", "_anchor"),
        get("JavaFrameAnchor", "_last_Java_sp"),
        get("JavaFrameAnchor", "_last_Java_pc"),
    ) else {
        crate::debug("VMStructs: no JavaFrameAnchor offsets; anchor recovery disabled");
        return;
    };
    let Some(thread) = current_java_thread(jni) else {
        crate::debug("cannot read Thread.eetop; anchor recovery disabled");
        return;
    };
    let env = jni as isize - thread as isize;
    if !(1..1 << 16).contains(&env)
        || !(0..1 << 16).contains(&anchor)
        || !(0..64).contains(&sp)
        || !(0..64).contains(&pc)
    {
        crate::debug(&format!(
            "implausible JavaThread layout (env {env}, anchor {anchor}); anchor recovery disabled"
        ));
        return;
    }
    ENV_TO_SP.store(anchor + sp - env, Ordering::SeqCst);
    ENV_TO_PC.store(anchor + pc - env, Ordering::SeqCst);
}

/// The current thread's `JavaThread*` (`Thread.currentThread().eetop`).
fn current_java_thread(jni: *mut JNIEnv) -> Option<usize> {
    let cls = jni!(jni, FindClass, c"java/lang/Thread".as_ptr());
    if cls.is_null() {
        jni!(jni, ExceptionClear);
        return None;
    }
    let current =
        jni!(jni, GetStaticMethodID, cls, c"currentThread".as_ptr(), c"()Ljava/lang/Thread;".as_ptr());
    let eetop = jni!(jni, GetFieldID, cls, c"eetop".as_ptr(), c"J".as_ptr());
    let r = if current.is_null() || eetop.is_null() {
        jni!(jni, ExceptionClear);
        None
    } else {
        let t = jni!(jni, CallStaticObjectMethodA, cls, current, std::ptr::null());
        let v = if t.is_null() { 0 } else { jni!(jni, GetLongField, t, eetop) };
        jni!(jni, DeleteLocalRef, t);
        (v != 0).then_some(v as usize)
    };
    jni!(jni, DeleteLocalRef, cls);
    r
}

fn old_handler_is_foreign() -> bool {
    PREV.get().is_some_and(|p| p.0.sa_sigaction != libc::SIG_DFL && p.0.sa_sigaction != libc::SIG_IGN)
}

fn arm(tid: i32) {
    if OWN_TIDS.lock().as_ref().is_some_and(|s| s.contains(&tid)) {
        return;
    }
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

/// Excludes the calling thread (one of the agent's own) from CPU sampling.
pub fn register_own_thread() {
    let tid = gettid();
    OWN_TIDS.lock().get_or_insert_with(HashSet::new).insert(tid);
    disarm(tid);
}

/// ThreadStart runs on the new thread itself, so its tid is ours to read. `idx` is `None` for the
/// agent's own Java threads, which are not sampled.
pub fn on_thread_start(a: &Agent, idx: Option<u32>) {
    let tid = gettid();
    let Some(i) = idx else {
        register_own_thread();
        return;
    };
    a.collector.lock().set_os_id(i, tid as u64);
    if ACTIVE.load(Ordering::Relaxed) {
        arm(tid);
    }
}

pub fn on_thread_end() {
    let tid = gettid();
    clear_mounted(tid);
    if ACTIVE.load(Ordering::Relaxed) {
        disarm(tid);
    }
}

fn comm(tid: i32) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Label for a sample without Java frames, by what the thread does.
fn role_label(thread_name: &str, code: i32) -> &'static str {
    // JDK 21 calls the non-generational ZGC threads X*.
    const GC: &[&str] = &[
        "GC Thread",
        "G1 ",
        "GC ",
        "Conc#",
        "ParGC",
        "ZDirector",
        "ZDriver",
        "ZWorker",
        "ZStat",
        "ZUnmapper",
        "ZUncommitter",
        "XDirector",
        "XDriver",
        "XWorker",
        "XStat",
        "XUnmapper",
        "XUncommitter",
        "Shenandoah",
    ];
    if thread_name.contains("CompilerThre") || thread_name.starts_with("JVMCI") {
        "[JIT compiler]"
    } else if GC.iter().any(|p| thread_name.starts_with(p)) {
        "[GC]"
    } else if thread_name == "VM Thread" {
        "[VM operation]"
    } else if thread_name.starts_with("VM Periodic") {
        "[VM periodic task]"
    } else if code == ASGCT_UNKNOWN_NOT_JAVA {
        "[non-Java thread]"
    } else {
        "[no Java frames]"
    }
}

fn error_label(code: i32) -> &'static str {
    match code {
        ASGCT_NO_CLASS_LOAD => "[no class load]",
        ASGCT_GC_ACTIVE => "[GC active]",
        ASGCT_NOT_WALKABLE_NOT_JAVA => "[not walkable, in VM]",
        ASGCT_UNKNOWN_JAVA => "[unknown Java]",
        ASGCT_NOT_WALKABLE_JAVA => "[not walkable Java]",
        ASGCT_THREAD_EXIT => "[thread exit]",
        ASGCT_DEOPT => "[deoptimization]",
        ASGCT_SAFEPOINT => "[safepoint]",
        _ => "[unknown]",
    }
}

fn drain_loop(a: &'static Agent) {
    register_own_thread();
    let Some(jni) = a.vm.attach_daemon("OpenProfiler Async Drainer") else { return };
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
        while let Some((tid, time, n, vthread)) = ring.pop(&mut frames) {
            let thread = match c.thread_by_os_id(tid as u64) {
                Some(i) => i,
                None => {
                    let name = comm(tid);
                    if name.to_ascii_lowercase().starts_with("openprofiler") {
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
                let label = match n {
                    NO_JAVA_FRAMES | ASGCT_NO_JAVA_FRAME | ASGCT_UNKNOWN_NOT_JAVA => {
                        role_label(c.threads.get(thread as usize).map_or("", |t| t.name.as_str()), n)
                    }
                    _ => error_label(n),
                };
                ids.push(c.synthetic_frame(label));
            }
            let stack = c.stacks.intern_leaf_first(&ids);
            // CPU used while a virtual thread is mounted belongs to the virtual thread.
            let thread = if vthread > 0 { vthread - 1 } else { thread };
            c.add_sample(time, SampleKind::Cpu, thread, ThreadState::Runnable, stack, weight, None);
        }
        c.dropped_samples = DROPPED.load(Ordering::Relaxed);
        drop(guard);
        jni!(jni, PopLocalFrame, std::ptr::null_mut());
    }
    crate::debug(&format!(
        "async sampler: {} dropped, {} repaired by stub unwinding, {} by anchor recovery",
        DROPPED.load(Ordering::Relaxed),
        RECOVERED_STUB.load(Ordering::Relaxed),
        RECOVERED_ANCHOR.load(Ordering::Relaxed)
    ));
    a.vm.detach();
}
