//! Linux asynchronous CPU sampler.
//!
//! Each thread gets a POSIX timer on its own CPU-time clock (`timer_create` with
//! `SIGEV_THREAD_ID`), so only threads that actually burn CPU are interrupted and no safepoint
//! bias is introduced. The `SIGPROF` handler calls HotSpot's `AsyncGetCallTrace` and copies the
//! raw frames into a lock-free bounded ring (Vyukov MPMC); a drainer thread resolves and
//! aggregates them. Signals that are not ours are forwarded to the previously installed handler.
//! On OpenJ9 the handler instead requests an inspection of the thread (see `sampler_j9`).
//!
//! `AsyncGetCallTrace` gives up on two common situations, which the handler repairs and retries:
//! - The thread is in Java code but the PC is in a frameless stub (array zeroing/copying,
//!   itable/vtable stubs), in a method prologue/epilogue, or in VM/libc code reached by a leaf
//!   call that does not set the anchor (`ticks_unknown_Java`). The handler unwinds one frame on a
//!   copy of the signal context and calls it again; failing that, it scans the stack of a stub
//!   whose SP is not where its frame size says, or follows the frame-pointer chain until a return
//!   address lands in the code cache (bounds from `gHotSpotVMStructs`).
//! - The thread is in the VM, called from Java through a path that leaves the last-Java-frame
//!   anchor without a PC (`ticks_unknown_not_Java`). Like HotSpot's own `make_walkable()`, the
//!   handler fills in the PC from the anchor's SP for the duration of the call (offsets come from
//!   `gHotSpotVMStructs`).
//!
//! Threads without Java frames (JIT compiler, GC, VM thread) are labelled by role, other native
//! threads by name, and walks that still fail on a Java thread by its VM thread state. Native
//! threads the VM starts later (GC workers) are picked up by rescanning `/proc/self/task`.

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
/// Sample code for a thread the VM does not know as a Java thread (no `JNIEnv`).
const NOT_A_JAVA_THREAD: i32 = -101;
/// Sample code for an OpenJ9 thread whose stack the inspector takes (see `sampler_j9`).
const INSPECT: i32 = -102;
/// Sample code for an OpenJ9 carrier running a virtual thread, whose stack the walker takes.
const WALK: i32 = -103;
/// Sample code for a virtual thread that unmounted before the walker took its stack.
const UNMOUNTED: i32 = -104;
/// Walks queued for the walker at most.
const MAX_WALKS: usize = 1024;
/// `JavaThreadState` values (stable since JDK 8); the `_trans` variants are one higher.
const THREAD_IN_NATIVE: i32 = 4;
const THREAD_IN_VM: i32 = 6;
const THREAD_BLOCKED: i32 = 10;
/// How many native frames the code-cache-guided unwinding follows.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const MAX_NATIVE_FRAMES: usize = 16;
/// How many stack words above SP are scanned for a return address out of a stub.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const MAX_SCAN_WORDS: usize = 32;
/// How often `/proc/self/task` is rescanned for threads to arm.
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
/// Upper bound for the distance between SP and a frame pointer worth following.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const MAX_FRAME_SPAN: usize = 8 << 20;

struct Slot {
    seq: AtomicUsize,
    tid: UnsafeCell<i32>,
    time: UnsafeCell<i64>,
    n: UnsafeCell<i32>,
    /// Thread index + 1 of the virtual thread mounted on the sampled carrier, 0 if none.
    vthread: UnsafeCell<u32>,
    /// `JavaThreadState` when the walk failed, -1 if unknown.
    state: UnsafeCell<i32>,
    /// `JNIEnv*` of the thread, for an OpenJ9 inspection request.
    env: UnsafeCell<usize>,
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
                state: UnsafeCell::new(-1),
                env: UnsafeCell::new(0),
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

    /// Single consumer: pops one committed sample, appending its frames to `out`.
    fn pop(&self, out: &mut Vec<ASGCT_CallFrame>) -> Option<Popped> {
        let pos = self.tail.load(Ordering::Relaxed);
        let slot = &self.slots[pos % RING_SIZE];
        if slot.seq.load(Ordering::Acquire) != pos + 1 {
            return None;
        }
        // SAFETY: the slot was committed by its producer and is owned by the consumer until `seq`
        // is advanced below.
        let r = unsafe {
            let n = *slot.n.get();
            let first = out.len();
            if n > 0 {
                let frames = &*slot.frames.get();
                out.extend_from_slice(&frames[..n as usize]);
            }
            Popped {
                tid: *slot.tid.get(),
                time: *slot.time.get(),
                n,
                vthread: *slot.vthread.get(),
                state: *slot.state.get(),
                env: *slot.env.get(),
                first,
            }
        };
        slot.seq.store(pos + RING_SIZE, Ordering::Release);
        self.tail.store(pos + 1, Ordering::Relaxed);
        Some(r)
    }
}

/// A sample taken off the ring; its `n` frames start at `first` in the caller's buffer.
struct Popped {
    tid: i32,
    time: i64,
    n: i32,
    vthread: u32,
    state: i32,
    env: usize,
    first: usize,
}

/// Samples taken off the ring, with their frames.
#[derive(Default)]
struct Batch {
    samples: Vec<Popped>,
    frames: Vec<ASGCT_CallFrame>,
}

// SAFETY: the raw jmethodID pointers are opaque VM handles valid on any thread.
unsafe impl Send for Batch {}

static RING: OnceLock<Ring> = OnceLock::new();
static ASGCT: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static INTERVAL_NS: AtomicU64 = AtomicU64::new(10_000_000);
static PREV: OnceLock<PrevHandler> = OnceLock::new();
/// OpenJ9: samples whose stacks the inspector took, for the drainer to record.
static INSPECTED: Mutex<Option<Batch>> = Mutex::new(None);
static INSPECTOR: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
static INSPECTOR_STOP: AtomicBool = AtomicBool::new(false);
static WALKER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// OpenJ9: whether the walker runs; carriers are otherwise inspected like other threads.
static WALKER_ON: AtomicBool = AtomicBool::new(false);
/// OpenJ9: samples of carriers waiting for the walker, and whether it should stop.
static WALKS: Mutex<(Vec<Popped>, bool)> = Mutex::new((Vec::new(), false));
static WALKS_CV: parking_lot::Condvar = parking_lot::Condvar::new();
static TIMERS: Mutex<Option<HashMap<i32, usize>>> = Mutex::new(None);
/// The agent's own threads: never armed.
static OWN_TIDS: Mutex<Option<HashSet<i32>>> = Mutex::new(None);
/// Whether failed walks are repaired and retried (`recover` option).
static RECOVER: AtomicBool = AtomicBool::new(false);
/// Offsets from a thread's `JNIEnv*` to its anchor's `_last_Java_sp` / `_last_Java_pc`; 0 when
/// unknown.
static ENV_TO_SP: AtomicIsize = AtomicIsize::new(0);
static ENV_TO_PC: AtomicIsize = AtomicIsize::new(0);
/// Offset from a thread's `JNIEnv*` to its `JavaThread::_thread_state`; 0 when unknown.
static ENV_TO_STATE: AtomicIsize = AtomicIsize::new(0);
static PID: AtomicI32 = AtomicI32::new(0);
static CODE_CACHE: OnceLock<crate::vmstructs::CodeCache> = OnceLock::new();
/// Samples repaired by unwinding a stub frame / by making the anchor walkable.
static RECOVERED_STUB: AtomicU64 = AtomicU64::new(0);
static RECOVERED_ANCHOR: AtomicU64 = AtomicU64::new(0);
/// Samples repaired by following native frames back into the code cache.
static RECOVERED_NATIVE: AtomicU64 = AtomicU64::new(0);

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
    let j9 = crate::sampler_j9::enabled();
    if (f == 0 && !j9) || !a.cpu_on.load(Ordering::Relaxed) {
        return;
    }
    // Counted from before a request can be made until it is committed.
    let _in_flight = j9.then(crate::sampler_j9::InFlight::enter);
    let Some((slot, pos)) = ring.reserve() else {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let env = a.vm.env().unwrap_or(std::ptr::null_mut());
    let tid = gettid();
    if j9 {
        let vthread = mounted(tid);
        let n = if env.is_null() {
            NOT_A_JAVA_THREAD
        } else if vthread != 0 && WALKER_ON.load(Ordering::Relaxed) {
            WALK
        // SAFETY: `env` is the current thread's; we are in flight with the slot reserved.
        } else if unsafe { crate::sampler_j9::request(env) } {
            INSPECT
        } else {
            NO_JAVA_FRAMES
        };
        // SAFETY: the slot is reserved for us until commit.
        unsafe {
            *slot.n.get() = n;
            *slot.state.get() = -1;
            *slot.env.get() = env as usize;
            *slot.tid.get() = tid;
            *slot.time.get() = realtime_nanos();
            *slot.vthread.get() = vthread;
        }
        Ring::commit(slot, pos);
        if n == INSPECT || n == WALK {
            crate::sampler_j9::wake();
        }
        return;
    }
    // SAFETY: `f` is HotSpot's AsyncGetCallTrace resolved via dlsym; the slot is reserved for us
    // until commit; `uc` is the signal context.
    unsafe {
        let asgct: AsyncGetCallTraceFn = std::mem::transmute(f);
        let mut trace = ASGCT_CallTrace { env, num_frames: 0, frames: (*slot.frames.get()).as_mut_ptr() };
        let mut state = -1;
        if env.is_null() {
            trace.num_frames = NOT_A_JAVA_THREAD;
        } else {
            asgct(&mut trace, MAX_FRAMES as jint, uc);
            if trace.num_frames < 0 && RECOVER.load(Ordering::Relaxed) {
                recover(asgct, &mut trace, uc);
            }
            let to_state = ENV_TO_STATE.load(Ordering::Relaxed);
            if trace.num_frames < 0 && to_state != 0 {
                // A field of the current thread's JavaThread (offset from VMStructs).
                state = std::ptr::read_volatile((env as isize + to_state) as *const i32);
            }
        }
        *slot.n.get() = trace.num_frames;
        *slot.state.get() = state;
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
        // SAFETY: forwarded from the caller.
        ASGCT_UNKNOWN_JAVA | ASGCT_NOT_WALKABLE_JAVA if !unsafe { unwind_and_retry(asgct, trace, uc) } => {
            trace.num_frames = code;
        }
        _ => {}
    }
}

/// Reads a word that may not be mapped, without faulting (`process_vm_readv` on ourselves).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn safe_read(addr: usize) -> Option<usize> {
    let mut v = 0usize;
    let local = libc::iovec { iov_base: &mut v as *mut usize as *mut c_void, iov_len: size_of::<usize>() };
    let remote = libc::iovec { iov_base: addr as *mut c_void, iov_len: size_of::<usize>() };
    // SAFETY: the kernel validates `remote`; `local` is our own variable. Async-signal-safe.
    let n = unsafe { libc::process_vm_readv(PID.load(Ordering::Relaxed), &local, 1, &remote, 1, 0) };
    (n == size_of::<usize>() as isize).then_some(v)
}

/// The frame record `(saved fp, return address)` at `fp`, if `fp` plausibly points into the
/// stack at or above `sp` (equal right after a stub's `enter`, when its callees are frameless).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn frame_record(sp: usize, fp: usize) -> Option<(usize, usize)> {
    if fp < sp || fp - sp > MAX_FRAME_SPAN || !fp.is_multiple_of(size_of::<usize>()) {
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
    #[cfg(target_arch = "x86_64")]
    let (sp, fp) = {
        let g = &ctx.uc_mcontext.gregs;
        (g[libc::REG_RSP as usize] as usize, g[libc::REG_RBP as usize] as usize)
    };
    #[cfg(target_arch = "aarch64")]
    let (sp, fp) = (ctx.uc_mcontext.sp as usize, ctx.uc_mcontext.regs[29] as usize);
    let mut candidates: [Option<(usize, usize, usize)>; 4] = [None; 4];
    #[cfg(target_arch = "x86_64")]
    let (pc, lr) = (ctx.uc_mcontext.gregs[libc::REG_RIP as usize] as usize, 0);
    #[cfg(target_arch = "aarch64")]
    let (pc, lr) = (ctx.uc_mcontext.pc as usize, ctx.uc_mcontext.regs[30] as usize);
    #[cfg(target_arch = "x86_64")]
    {
        // Frameless stub or prologue/epilogue: the return address is on top of the stack.
        // SAFETY: SP of the interrupted code points into this thread's mapped stack.
        let ret = unsafe { *(sp as *const usize) };
        candidates[0] = Some((ret, sp + 8, fp));
        // Stub that set up an RBP frame (`enter`).
        candidates[1] = frame_record(sp, fp).map(|(saved, ret)| (ret, fp + 16, saved));
    }
    #[cfg(target_arch = "aarch64")]
    {
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
    // Compiled method between pushing its frame and `frame_complete_offset` (nmethod entry
    // barrier, stack banging) or in its epilogue, where HotSpot does not consider the frame
    // walkable: the blob's frame size gives the caller's SP, with the frame record just below.
    let blob = CODE_CACHE.get().and_then(|cc| cc.find_blob(pc));
    if let Some(cc) = CODE_CACHE.get()
        && let Some(blob) = blob
    {
        // SAFETY: `blob` comes from `find_blob`.
        let (_, words) = unsafe { cc.frame_info(blob) };
        // The blob may be freed concurrently: no arithmetic on the raw size.
        let size = usize::try_from(words).ok().and_then(|w| w.checked_mul(size_of::<usize>()));
        if let Some(size) = size.filter(|&s| s >= 2 * size_of::<usize>() && s <= MAX_FRAME_SPAN) {
            let caller_sp = sp + size;
            candidates[2] = safe_read(caller_sp - 8)
                .zip(safe_read(caller_sp - 16))
                .map(|(ret, saved)| (ret, caller_sp, saved));
            // aarch64: frame allocated but the record not stored yet, or already reloaded.
            if lr != 0 {
                candidates[3] = Some((lr, caller_sp, fp));
            }
        }
    }
    for (cpc, csp, cfp) in candidates.into_iter().flatten() {
        if cpc == 0 {
            continue;
        }
        // SAFETY: a valid context for the current thread with adjusted registers.
        if unsafe { retry_at(asgct, trace, &mut ctx, cpc, csp, cfp) } {
            RECOVERED_STUB.fetch_add(1, Ordering::Relaxed);
            return true;
        }
    }
    // Stub that moves SP past its declared frame (Graal's copies of the libm intrinsics reserve
    // scratch space with `sub rsp`): the first word above SP that points into another blob is
    // taken as the return address.
    if let Some(cc) = CODE_CACHE.get()
        && let Some(blob) = blob
    {
        for slot in (1..MAX_SCAN_WORDS).map(|i| sp + i * size_of::<usize>()) {
            let Some(ret) = safe_read(slot) else { break };
            if cc.find_blob(ret).is_some_and(|b| b != blob) {
                // SAFETY: as above.
                if unsafe { retry_at(asgct, trace, &mut ctx, ret, slot + size_of::<usize>(), fp) } {
                    RECOVERED_STUB.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                break;
            }
        }
    }
    // VM or libc code called from compiled code without setting the anchor (runtime leaf calls,
    // math and crypto intrinsics, GC barriers): follow frame records until a return address is
    // in the code cache, i.e. back in Java code.
    let Some(cc) = CODE_CACHE.get() else { return false };
    let (mut sp, mut fp) = (sp, fp);
    for _ in 0..MAX_NATIVE_FRAMES {
        let Some((saved, ret)) = frame_record(sp, fp) else { break };
        let caller_sp = fp + 2 * size_of::<usize>();
        if cc.find_blob(ret).is_some() {
            // SAFETY: as above.
            if unsafe { retry_at(asgct, trace, &mut ctx, ret, caller_sp, saved) } {
                RECOVERED_NATIVE.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            break;
        }
        (sp, fp) = (caller_sp, saved);
    }
    false
}

/// Calls `AsyncGetCallTrace` on `ctx` with PC, SP and FP replaced; true if the walk succeeded.
///
/// # Safety
/// Signal-handler context; `ctx` is a copy of the handler's `ucontext_t`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe fn retry_at(
    asgct: AsyncGetCallTraceFn,
    trace: &mut ASGCT_CallTrace,
    ctx: &mut libc::ucontext_t,
    pc: usize,
    sp: usize,
    fp: usize,
) -> bool {
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
    unsafe { asgct(trace, MAX_FRAMES as jint, ctx as *mut libc::ucontext_t as *mut c_void) };
    trace.num_frames > 0
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
        crate::sampler_j9::end_registration();
        return;
    }
    let j9 = match crate::sampler_j9::init(a, jni) {
        crate::sampler_j9::Init::Ready => true,
        crate::sampler_j9::Init::NotJ9 => false,
        crate::sampler_j9::Init::Unsupported(why) => {
            log(&format!("OpenJ9 stack sampling unavailable ({why}); using the sync sampler"));
            return;
        }
    };
    PID.store(std::process::id() as i32, Ordering::SeqCst);
    if !j9 {
        // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name.
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"AsyncGetCallTrace".as_ptr()) };
        if sym.is_null() {
            if a.opts.sampler == crate::SamplerChoice::Async {
                log("AsyncGetCallTrace not found; using the sync sampler");
            }
            return;
        }
        if a.opts.recover {
            init_anchor_offsets(jni);
            RECOVER.store(true, Ordering::SeqCst);
        }
        ASGCT.store(sym as usize, Ordering::SeqCst);
    }
    let _ = RING.set(Ring::new());
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
    if j9 {
        match std::thread::Builder::new()
            .name("openprofiler-j9-inspector".into())
            .spawn(move || inspect_loop(a))
        {
            Ok(h) => *INSPECTOR.lock() = Some(h),
            Err(e) => {
                log(&format!("cannot start the J9 stack inspector ({e}); using the sync sampler"));
                return;
            }
        }
        match std::thread::Builder::new().name("openprofiler-j9-walker".into()).spawn(move || walker_loop(a))
        {
            Ok(h) => *WALKER.lock() = Some(h),
            Err(e) => {
                log(&format!(
                    "cannot start the J9 virtual thread walker ({e}); carriers show their own frames"
                ));
            }
        }
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

/// Finds a thread's last-Java-frame anchor and thread state relative to its `JNIEnv*`:
/// `java.lang.Thread.eetop` holds the current `JavaThread*`, VMStructs the field offsets.
fn init_anchor_offsets(jni: *mut JNIEnv) {
    match crate::vmstructs::CodeCache::load() {
        Some(c) => {
            let _ = CODE_CACHE.set(c);
        }
        None => crate::debug(
            "VMStructs: code cache layout unknown; runtime-stub and native-frame recovery disabled",
        ),
    }
    let v = crate::vmstructs::VmStructs::load(&["JavaThread", "JavaFrameAnchor"]);
    let get = |t: &str, n: &str| v.offset(t, n).map(|o| o as isize);
    let Some(thread) = current_java_thread(jni) else {
        crate::debug("cannot read Thread.eetop; anchor recovery disabled");
        return;
    };
    let env = jni as isize - thread as isize;
    if !(1..1 << 16).contains(&env) {
        crate::debug(&format!("implausible JavaThread layout (env {env}); anchor recovery disabled"));
        return;
    }
    match get("JavaThread", "_thread_state") {
        Some(state) if (0..1 << 16).contains(&state) => ENV_TO_STATE.store(state - env, Ordering::SeqCst),
        _ => crate::debug("VMStructs: no JavaThread::_thread_state; failed walks not labelled by state"),
    }
    let (Some(anchor), Some(sp), Some(pc)) = (
        get("JavaThread", "_anchor"),
        get("JavaFrameAnchor", "_last_Java_sp"),
        get("JavaFrameAnchor", "_last_Java_pc"),
    ) else {
        crate::debug("VMStructs: no JavaFrameAnchor offsets; anchor recovery disabled");
        return;
    };
    if !(0..1 << 16).contains(&anchor) || !(0..64).contains(&sp) || !(0..64).contains(&pc) {
        crate::debug(&format!(
            "implausible JavaFrameAnchor layout (anchor {anchor}); anchor recovery disabled"
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

/// Arms a CPU-time timer for `tid` unless it has one or is the agent's own. The timer map stays
/// locked throughout, so the drainer's rescan and `ThreadStart` cannot arm a thread twice, and
/// `register_own_thread` (which disarms after recording the tid) cannot be overtaken.
fn arm(tid: i32) {
    let mut timers = TIMERS.lock();
    let timers = timers.get_or_insert_with(HashMap::new);
    if timers.contains_key(&tid) || OWN_TIDS.lock().as_ref().is_some_and(|s| s.contains(&tid)) {
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
        // The fields are 32-bit on armhf.
        let ts = libc::timespec {
            tv_sec: (interval / 1_000_000_000) as _,
            tv_nsec: (interval % 1_000_000_000) as _,
        };
        let spec = libc::itimerspec { it_interval: ts, it_value: ts };
        if libc::timer_settime(timer, 0, &spec, std::ptr::null_mut()) != 0 {
            libc::timer_delete(timer);
            return;
        }
        timers.insert(tid, timer as usize);
    }
}

/// Arms threads started since the last scan that `ThreadStart` does not report (GC workers the VM
/// adds on demand, threads of native libraries) and drops the timers of threads that are gone.
fn rescan() {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else { return };
    let live: HashSet<i32> =
        dir.flatten().filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok())).collect();
    let (gone, new): (Vec<i32>, Vec<i32>) = {
        let timers = TIMERS.lock();
        let armed = |t: &i32| timers.as_ref().is_some_and(|m| m.contains_key(t));
        let gone =
            timers.as_ref().map_or(vec![], |m| m.keys().copied().filter(|t| !live.contains(t)).collect());
        (gone, live.iter().copied().filter(|t| !armed(t)).collect())
    };
    for tid in gone {
        disarm(tid);
    }
    let own = OWN_TIDS.lock().clone().unwrap_or_default();
    for tid in new {
        // The agent's native threads mark themselves as their first action; skip any caught
        // before that.
        if !own.contains(&tid) && !comm(tid).to_ascii_lowercase().starts_with("openprofiler") {
            arm(tid);
        }
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
pub fn on_thread_start(a: &Agent, jni: *mut JNIEnv, thread: jthread, idx: Option<u32>) {
    let tid = gettid();
    let Some(i) = idx else {
        register_own_thread();
        return;
    };
    a.collector.lock().set_os_id(i, tid as u64);
    crate::sampler_j9::register(jni, jni as usize, thread);
    if ACTIVE.load(Ordering::Relaxed) {
        arm(tid);
    }
}

pub fn on_thread_end(a: &Agent, jni: *mut JNIEnv, thread: jthread, idx: Option<u32>) {
    let tid = gettid();
    clear_mounted(tid);
    crate::sampler_j9::thread_end(a, jni, thread, idx);
    if ACTIVE.load(Ordering::Relaxed) {
        disarm(tid);
    }
}

fn comm(tid: i32) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// What a thread without Java frames does, by its name.
fn role(thread_name: &str) -> Option<&'static str> {
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
        // OpenJ9 (`GC Worker` matches `GC `).
        "Concurrent Mark",
    ];
    // Java threads that run VM-internal work (comm names are truncated to 15 bytes).
    const SERVICE: &[&str] = &[
        "Service Thread",
        "Monitor Deflati",
        "Notification Th",
        "Signal Dispatch",
        "Attach Listener",
        "Sweeper thread",
    ];
    // OpenJ9: `JIT Compilation Thread-000`, `JIT-SamplerThread`, `JIT IProfiler`.
    let jit = thread_name.starts_with("JVMCI") || thread_name.starts_with("JIT");
    Some(if thread_name.contains("CompilerThre") || jit {
        "[JIT compiler]"
    } else if GC.iter().any(|p| thread_name.starts_with(p)) {
        "[GC]"
    } else if thread_name == "VM Thread" {
        "[VM operation]"
    } else if thread_name.starts_with("VM Periodic") {
        "[VM periodic task]"
    } else if thread_name.starts_with("JFR ") {
        "[JFR]"
    } else if SERVICE.iter().any(|p| thread_name.starts_with(p)) {
        "[VM service]"
    } else {
        return None;
    })
}

/// Label for a sample of a thread the VM does not know as a Java thread: its role, or its name
/// with the instance number dropped (`pool-3-worker-12` becomes `[native: pool-worker]`), so that
/// threads of one pool share a frame.
fn native_thread_label(thread_name: &str) -> String {
    if let Some(r) = role(thread_name) {
        return r.to_string();
    }
    let mut out = String::new();
    for part in thread_name.split(['-', '_', ' ', '#']) {
        let part = part.trim_end_matches(|c: char| c.is_ascii_digit());
        if !part.is_empty() {
            if !out.is_empty() {
                out.push('-');
            }
            out.push_str(part);
        }
    }
    if out.is_empty() { "[native thread]".to_string() } else { format!("[native: {out}]") }
}

/// Label for a Java thread's failed walk, by its `JavaThreadState`.
fn state_label(state: i32) -> &'static str {
    match state & !1 {
        THREAD_IN_NATIVE => "[in native code]",
        THREAD_IN_VM => "[in VM]",
        THREAD_BLOCKED => "[blocked in VM]",
        _ => "[unknown, not Java]",
    }
}

fn error_label(code: i32) -> &'static str {
    match code {
        UNMOUNTED => "[virtual thread, unmounted when walked]",
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

/// Takes the committed samples off the ring into `batch`. The stacks of OpenJ9 inspection
/// requests are taken now, which resumes the requesting threads; walks of virtual threads go to
/// `walks`.
fn take(a: &Agent, ring: &Ring, batch: &mut Batch, walks: &mut Vec<Popped>, buf: &mut [jvmtiFrameInfo]) {
    while let Some(mut s) = ring.pop(&mut batch.frames) {
        match s.n {
            INSPECT => {
                // The frames are the thread's own, also on a carrier that mounted a virtual
                // thread since.
                s.vthread = 0;
                let r = crate::sampler_j9::inspect(a, s.env, buf);
                s.n = add_frames(batch, &mut s, r, buf);
            }
            WALK => {
                walks.push(s);
                continue;
            }
            _ => {}
        }
        batch.samples.push(s);
    }
}

/// Appends the frames of a stack taken with JVMTI to `batch` for sample `s`; its sample code.
fn add_frames(
    batch: &mut Batch,
    s: &mut Popped,
    r: Result<usize, jvmtiError>,
    buf: &[jvmtiFrameInfo],
) -> i32 {
    s.first = batch.frames.len();
    match r {
        Ok(0) => NO_JAVA_FRAMES,
        Ok(n) => {
            let frames =
                buf[..n].iter().map(|f| ASGCT_CallFrame { lineno: f.location as jint, method_id: f.method });
            batch.frames.extend(frames);
            n as i32
        }
        Err(JVMTI_ERROR_THREAD_NOT_ALIVE) => ASGCT_THREAD_EXIT,
        Err(_) => ASGCT_UNKNOWN_STATE,
    }
}

/// Hands samples with their stacks to the drainer.
fn publish(batch: &mut Batch) {
    if batch.samples.is_empty() {
        return;
    }
    let mut out = INSPECTED.lock();
    let out = out.get_or_insert_with(Batch::default);
    let base = out.frames.len();
    out.frames.append(&mut batch.frames);
    out.samples.extend(batch.samples.drain(..).map(|s| Popped { first: s.first + base, ..s }));
}

/// OpenJ9: serves inspection requests as they come, holding no agent lock (see `sampler_j9`),
/// passes walks of virtual threads to the walker and hands the samples to the drainer. Stopped by
/// [`stop`] once no requests can be made, after serving the last ones.
fn inspect_loop(a: &'static Agent) {
    register_own_thread();
    let Some(_jni) = a.vm.attach_daemon("OpenProfiler J9 Inspector") else {
        log("cannot attach the J9 stack inspector; OpenJ9 samples have no stacks");
        return;
    };
    crate::sampler_j9::accept();
    let ring = RING.get().unwrap();
    let mut buf = vec![jvmtiFrameInfo { method: std::ptr::null_mut(), location: 0 }; MAX_FRAMES];
    let mut batch = Batch::default();
    let mut walks = Vec::new();
    loop {
        let last = INSPECTOR_STOP.load(Ordering::SeqCst);
        take(a, ring, &mut batch, &mut walks, &mut buf);
        publish(&mut batch);
        if !walks.is_empty() {
            let mut q = WALKS.lock();
            let room = MAX_WALKS.saturating_sub(q.0.len());
            if walks.len() > room {
                DROPPED.fetch_add((walks.len() - room) as u64, Ordering::Relaxed);
                walks.truncate(room);
            }
            q.0.append(&mut walks);
            WALKS_CV.notify_one();
        }
        if last {
            break;
        }
        crate::sampler_j9::wait(10);
    }
    a.vm.detach();
}

/// OpenJ9: takes the stacks of the virtual threads that carriers ran when sampled (see
/// `sampler_j9::walk_mounted`) and hands the samples to the drainer.
fn walker_loop(a: &'static Agent) {
    register_own_thread();
    let Some(jni) = a.vm.attach_daemon("OpenProfiler J9 Walker") else {
        log("cannot attach the J9 virtual thread walker; carriers show their own frames");
        return;
    };
    WALKER_ON.store(true, Ordering::SeqCst);
    let mut buf = vec![jvmtiFrameInfo { method: std::ptr::null_mut(), location: 0 }; MAX_FRAMES];
    let mut batch = Batch::default();
    loop {
        let jobs = {
            let mut q = WALKS.lock();
            while q.0.is_empty() && !q.1 {
                WALKS_CV.wait(&mut q);
            }
            if q.0.is_empty() {
                break;
            }
            std::mem::take(&mut q.0)
        };
        for mut s in jobs {
            let walked = crate::sampler_j9::walk_mounted(a, jni, s.env, s.vthread - 1, &mut buf);
            s.n = match walked {
                Some(n) => add_frames(&mut batch, &mut s, Ok(n), &buf),
                None => UNMOUNTED,
            };
            batch.samples.push(s);
        }
        publish(&mut batch);
    }
    a.vm.detach();
}

/// Before the VM dies: on OpenJ9, stops inspection requests and waits until the inspector served
/// the queued ones, while JVMTI still works; then stops the walker.
pub fn stop() {
    if !crate::sampler_j9::enabled() {
        return;
    }
    crate::sampler_j9::stop_requests();
    INSPECTOR_STOP.store(true, Ordering::SeqCst);
    crate::sampler_j9::wake();
    if let Some(h) = INSPECTOR.lock().take() {
        let _ = h.join();
    }
    WALKER_ON.store(false, Ordering::SeqCst);
    WALKS.lock().1 = true;
    WALKS_CV.notify_one();
    if let Some(h) = WALKER.lock().take() {
        let _ = h.join();
    }
}

fn drain_loop(a: &'static Agent) {
    register_own_thread();
    let Some(jni) = a.vm.attach_daemon("OpenProfiler Async Drainer") else { return };
    let ring = RING.get().unwrap();
    let j9 = crate::sampler_j9::enabled();
    let mut batch = Batch::default();
    let mut ids = Vec::with_capacity(MAX_FRAMES);
    let mut last_scan = std::time::Instant::now();
    while !a.shutting_down.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
        if jni!(jni, PushLocalFrame, 256) != 0 {
            jni!(jni, ExceptionClear);
            continue;
        }
        if j9 {
            if let Some(b) = INSPECTED.lock().as_mut() {
                std::mem::swap(b, &mut batch);
            }
        } else {
            take(a, ring, &mut batch, &mut Vec::new(), &mut []);
        }
        let weight = INTERVAL_NS.load(Ordering::Relaxed);
        let mut guard = a.collector.lock();
        let c = &mut *guard;
        if last_scan.elapsed() >= RESCAN_INTERVAL {
            rescan();
            last_scan = std::time::Instant::now();
        }
        for &Popped { tid, time, n, vthread, state, first, .. } in &batch.samples {
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
                for f in &batch.frames[first..first + n as usize] {
                    let kind = if f.lineno == -3 { FrameKind::Native } else { FrameKind::Unknown };
                    let bci = if f.lineno >= 0 { f.lineno as i64 } else { -1 };
                    ids.push(c.resolver.frame(a.jvmti, &mut c.symbols, f.method_id, bci, kind));
                }
            } else {
                let name = c.threads.get(thread as usize).map_or("", |t| t.name.as_str());
                let label = match n {
                    NOT_A_JAVA_THREAD => native_thread_label(name),
                    NO_JAVA_FRAMES | ASGCT_NO_JAVA_FRAME => {
                        role(name).unwrap_or("[no Java frames]").to_string()
                    }
                    ASGCT_UNKNOWN_NOT_JAVA => role(name).unwrap_or_else(|| state_label(state)).to_string(),
                    _ => error_label(n).to_string(),
                };
                ids.push(c.synthetic_frame(&label));
            }
            // CPU used while a virtual thread is mounted belongs to the virtual thread.
            let thread = if vthread > 0 { vthread - 1 } else { thread };
            if n > 0
                && let Some(label) = crate::probes::label_frame_of(thread)
            {
                ids.push(label);
            }
            let stack = c.stacks.intern_leaf_first(&ids);
            c.add_sample(time, SampleKind::Cpu, thread, ThreadState::Runnable, stack, weight, None);
        }
        batch.samples.clear();
        batch.frames.clear();
        c.dropped_samples = DROPPED.load(Ordering::Relaxed);
        drop(guard);
        jni!(jni, PopLocalFrame, std::ptr::null_mut());
    }
    crate::debug(&format!(
        "async sampler: {} dropped, {} repaired by stub unwinding, {} by native-frame unwinding, {} by anchor recovery",
        DROPPED.load(Ordering::Relaxed),
        RECOVERED_STUB.load(Ordering::Relaxed),
        RECOVERED_NATIVE.load(Ordering::Relaxed),
        RECOVERED_ANCHOR.load(Ordering::Relaxed)
    ));
    a.vm.detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_threads_are_labelled_by_role_or_name() {
        assert_eq!(native_thread_label("GC Thread#3"), "[GC]");
        assert_eq!(native_thread_label("G1 Conc#1"), "[GC]");
        assert_eq!(native_thread_label("VM Thread"), "[VM operation]");
        assert_eq!(native_thread_label("C2 CompilerThre"), "[JIT compiler]");
        assert_eq!(role("JIT Compilation"), Some("[JIT compiler]"));
        assert_eq!(role("GC Worker"), Some("[GC]"));
        assert_eq!(role("Concurrent Mark Helper"), Some("[GC]"));
        assert_eq!(native_thread_label("pool-3-worker-12"), "[native: pool-worker]");
        assert_eq!(native_thread_label("grpc_epoll#2"), "[native: grpc-epoll]");
        assert_eq!(native_thread_label("12"), "[native thread]");
        assert_eq!(role("worker"), None);
    }

    #[test]
    fn failed_walks_are_labelled_by_thread_state() {
        assert_eq!(state_label(THREAD_IN_NATIVE), "[in native code]");
        assert_eq!(state_label(THREAD_IN_VM + 1), "[in VM]");
        assert_eq!(state_label(THREAD_BLOCKED), "[blocked in VM]");
        assert_eq!(state_label(-1), "[unknown, not Java]");
    }
}
