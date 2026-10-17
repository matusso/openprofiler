//! Exact call counts and times for instrumented methods (`Hooks.callEnter`/`callExit`).
//!
//! Each thread keeps, in native thread-local storage, a shadow stack of instrumented frames and a
//! small call trie of the paths it has seen. Entering a method looks up (parent, method) in the
//! trie; leaving it adds the call and its self time (duration minus instrumented callees) to the
//! node. An exit pops frames up to the matching method, so frames left behind by exceptions that
//! no handler covered (constructors before `super(...)`) are closed by their caller's exit. About
//! once a second, at thread end and when the thread's outermost frame returns, the trie is
//! flushed into the collector as [`SampleKind::Calls`] rows (weight = self nanoseconds, count =
//! invocations) on the global stack of the node's path. A thread's current request label, if a
//! probe set one, becomes the root frame of its paths.
//!
//! Each node also keeps the inclusive time and the slowest call of its path (call site), flushed
//! as call-site rows for outlier detection. While the call tracer runs ([`start_trace`]), every
//! entry and exit is also appended to a per-thread buffer and flushed as trace events, until the
//! trace's deadline or event cap.

use crate::agent;
use crate::events::thread_index;
use crate::jvm::jvmti;
use jvmti_sys::*;
use openprofiler_model::{CallSiteStat, SampleKind, ThreadState, TraceEvent};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Whether call counting records anything (`SetRecording calls`).
pub static ON: AtomicBool = AtomicBool::new(true);
/// Auto-tuning window.
pub const TUNE_WINDOW: Duration = Duration::from_secs(5);
/// A method called at least this often in a window...
const TUNE_MIN_CALLS: u64 = 50_000;
/// ...with a mean self time below this is dominated by the hook overhead.
const TUNE_MAX_SELF_NS: u64 = 400;
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Label nodes use method ids with this bit set (the low bits are the label's frame id).
const LABEL: u32 = 1 << 31;

/// Default call-tracer limits.
pub const TRACE_DURATION: Duration = Duration::from_secs(10);
pub const TRACE_EVENTS: u64 = 1_000_000;
/// A thread's trace buffer is flushed once it holds this many events.
const TRACE_FLUSH: usize = 4096;

/// Whether the call tracer is running (checked on every call; the rest only while it runs).
static TRACING: AtomicBool = AtomicBool::new(false);
/// Trace deadline, nanoseconds since [`clock`]'s base.
static TRACE_UNTIL: AtomicU64 = AtomicU64::new(0);
/// Events the running trace may still record.
static TRACE_LEFT: AtomicI64 = AtomicI64::new(0);
/// Events recorded by the current (or last) trace.
static TRACE_RECORDED: AtomicU64 = AtomicU64::new(0);

/// `(Instant, epoch nanos)` taken once, to turn instants into wall-clock times.
fn clock() -> (Instant, i64) {
    static BASE: OnceLock<(Instant, i64)> = OnceLock::new();
    *BASE.get_or_init(|| (Instant::now(), crate::collector::now_nanos()))
}

fn epoch(t: Instant) -> i64 {
    let (base, epoch) = clock();
    epoch + t.saturating_duration_since(base).as_nanos() as i64
}

/// Starts the call tracer for `duration` or `max_events` events, whichever comes first.
pub fn start_trace(duration: Duration, max_events: u64) {
    let (base, _) = clock();
    let until = Instant::now().saturating_duration_since(base) + duration;
    TRACE_UNTIL.store(until.as_nanos() as u64, Ordering::SeqCst);
    TRACE_LEFT.store(max_events.min(i64::MAX as u64) as i64, Ordering::SeqCst);
    TRACE_RECORDED.store(0, Ordering::SeqCst);
    TRACING.store(true, Ordering::SeqCst);
}

pub fn stop_trace() {
    TRACING.store(false, Ordering::SeqCst);
}

/// `(running, events recorded)` of the current or last trace.
pub fn trace_status() -> (bool, u64) {
    let running = TRACING.load(Ordering::Relaxed)
        && TRACE_LEFT.load(Ordering::Relaxed) > 0
        && clock().0.elapsed().as_nanos() as u64 <= TRACE_UNTIL.load(Ordering::Relaxed);
    (running, TRACE_RECORDED.load(Ordering::Relaxed))
}

/// Whether an event at `now` may be traced; consumes one event of the budget.
fn trace_take(now: Instant) -> bool {
    if !TRACING.load(Ordering::Relaxed) {
        return false;
    }
    if now.saturating_duration_since(clock().0).as_nanos() as u64 > TRACE_UNTIL.load(Ordering::Relaxed)
        || TRACE_LEFT.fetch_sub(1, Ordering::Relaxed) <= 0
    {
        TRACING.store(false, Ordering::Relaxed);
        return false;
    }
    TRACE_RECORDED.fetch_add(1, Ordering::Relaxed);
    true
}

/// Methods disabled by auto-tuning (bit per method id) until their class is retransformed.
static DISABLED: [AtomicU64; 4096] = [const { AtomicU64::new(0) }; 4096];
/// Per-method `(calls, self nanos)` since the last tuning pass.
static STATS: Mutex<Option<HashMap<u32, (u64, u64)>>> = Mutex::new(None);

fn disabled(id: u32) -> bool {
    DISABLED.get((id / 64) as usize).is_some_and(|w| w.load(Ordering::Relaxed) & (1 << (id % 64)) != 0)
}

#[derive(Clone, Copy)]
struct Node {
    parent: u32,
    method: u32,
    /// Frame id, resolved when the node is created (never while the collector is locked: the
    /// class-load hook takes the instrumentation lock before the collector's).
    frame: u32,
    count: u32,
    self_ns: u64,
    /// Inclusive time and slowest call since the last flush.
    total_ns: u64,
    max_ns: u64,
    /// Global stack id, 0 until first flushed.
    stack: u32,
}

#[derive(Clone, Copy)]
struct Frame {
    node: u32,
    method: u32,
    start: Instant,
    child_ns: u64,
}

struct ThreadCalls {
    /// `None` for the agent's own threads.
    thread: Option<u32>,
    nodes: Vec<Node>,
    index: HashMap<(u32, u32), u32>,
    stack: Vec<Frame>,
    last_flush: Instant,
    /// Call tracer events not yet flushed: (time, frame, exit).
    trace: Vec<(Instant, u32, bool)>,
}

impl ThreadCalls {
    fn new(thread: Option<u32>) -> Self {
        let root = Node {
            parent: 0,
            method: u32::MAX,
            frame: 0,
            count: 0,
            self_ns: 0,
            total_ns: 0,
            max_ns: 0,
            stack: 0,
        };
        Self {
            thread,
            nodes: vec![root],
            index: HashMap::new(),
            stack: vec![],
            last_flush: Instant::now(),
            trace: vec![],
        }
    }

    fn child(&mut self, parent: u32, method: u32) -> u32 {
        let next = self.nodes.len() as u32;
        let id = *self.index.entry((parent, method)).or_insert(next);
        if id == next {
            let frame =
                if method & LABEL != 0 { method & !LABEL } else { super::method_frame(method).unwrap_or(0) };
            self.nodes.push(Node {
                parent,
                method,
                frame,
                count: 0,
                self_ns: 0,
                total_ns: 0,
                max_ns: 0,
                stack: 0,
            });
        }
        id
    }

    fn enter(&mut self, method: u32) {
        let parent = match self.stack.last() {
            Some(f) => f.node,
            None => match crate::probes::current_label_frame() {
                Some(label) => self.child(0, LABEL | label),
                None => 0,
            },
        };
        let node = self.child(parent, method);
        let start = Instant::now();
        if trace_take(start) {
            self.trace.push((start, self.nodes[node as usize].frame, false));
        }
        self.stack.push(Frame { node, method, start, child_ns: 0 });
    }

    fn exit(&mut self, method: u32) {
        let Some(pos) = self.stack.iter().rposition(|f| f.method == method) else { return };
        let now = Instant::now();
        while self.stack.len() > pos {
            let f = self.stack.pop().unwrap();
            let dur = now.duration_since(f.start).as_nanos() as u64;
            let n = &mut self.nodes[f.node as usize];
            n.count += 1;
            n.self_ns += dur.saturating_sub(f.child_ns);
            n.total_ns += dur;
            n.max_ns = n.max_ns.max(dur);
            let frame = n.frame;
            if let Some(p) = self.stack.last_mut() {
                p.child_ns += dur;
            }
            if trace_take(now) {
                self.trace.push((now, frame, true));
            }
        }
        let since = now.duration_since(self.last_flush);
        if since >= FLUSH_EVERY
            || (self.stack.is_empty() && since >= FLUSH_EVERY / 10)
            || self.trace.len() >= TRACE_FLUSH
        {
            self.flush();
        }
    }

    /// Global stack id of `node`, interning its path.
    fn global_stack(&mut self, c: &mut crate::Collector, node: u32) -> u32 {
        if node == 0 {
            return 0;
        }
        let n = self.nodes[node as usize];
        if n.stack != 0 {
            return n.stack;
        }
        let parent = self.global_stack(c, n.parent);
        let id = c.stacks.push(parent, n.frame);
        self.nodes[node as usize].stack = id;
        id
    }

    fn flush(&mut self) {
        self.last_flush = Instant::now();
        let Some(thread) = self.thread else { return };
        let Some(a) = agent() else { return };
        let now = crate::collector::now_nanos();
        let mut stats = vec![];
        {
            let mut c = a.collector.lock();
            if !self.trace.is_empty() {
                c.add_trace_events(self.trace.drain(..).map(|(t, frame, exit)| TraceEvent {
                    time: epoch(t),
                    thread,
                    frame,
                    exit,
                }));
            }
            for i in 1..self.nodes.len() {
                let n = self.nodes[i];
                if n.count == 0 {
                    continue;
                }
                let stack = self.global_stack(&mut c, i as u32);
                c.add_samples(
                    now,
                    SampleKind::Calls,
                    thread,
                    ThreadState::Runnable,
                    stack,
                    n.self_ns,
                    n.count,
                );
                c.add_call_site(CallSiteStat {
                    time: now,
                    thread,
                    stack,
                    count: n.count,
                    total: n.total_ns as i64,
                    max: n.max_ns as i64,
                });
                stats.push((n.method, n.count as u64, n.self_ns));
                let n = &mut self.nodes[i];
                n.count = 0;
                n.self_ns = 0;
                n.total_ns = 0;
                n.max_ns = 0;
            }
        }
        let mut s = STATS.lock();
        let s = s.get_or_insert_with(HashMap::new);
        for (m, count, ns) in stats {
            if m & LABEL == 0 {
                let e = s.entry(m).or_insert((0, 0));
                e.0 += count;
                e.1 += ns;
            }
        }
    }
}

impl Drop for ThreadCalls {
    fn drop(&mut self) {
        self.flush();
    }
}

thread_local! {
    static CALLS: RefCell<Option<ThreadCalls>> = const { RefCell::new(None) };
}

fn with_thread(env: *mut JNIEnv, f: impl FnOnce(&mut ThreadCalls)) {
    let _ = CALLS.try_with(|c| {
        let Ok(mut c) = c.try_borrow_mut() else { return };
        let tc = c.get_or_insert_with(|| ThreadCalls::new(current_thread_index(env)));
        if tc.thread.is_some() {
            f(tc);
        }
    });
}

/// Thread index of the calling Java thread (`None` for the agent's threads).
pub fn current_thread_index(env: *mut JNIEnv) -> Option<u32> {
    let a = agent()?;
    let mut t: jthread = std::ptr::null_mut();
    if jvmti!(a.jvmti, GetCurrentThread, &mut t) != JVMTI_ERROR_NONE || t.is_null() {
        return None;
    }
    let idx = thread_index(a, env, t);
    crate::jvm::jni!(env, DeleteLocalRef, t);
    idx
}

pub extern "system" fn call_enter(env: *mut JNIEnv, _cls: jclass, id: jint) {
    let id = id as u32;
    if !ON.load(Ordering::Relaxed) || disabled(id) {
        return;
    }
    with_thread(env, |t| t.enter(id));
}

pub extern "system" fn call_exit(env: *mut JNIEnv, _cls: jclass, id: jint) {
    let id = id as u32;
    if disabled(id) {
        return;
    }
    with_thread(env, |t| t.exit(id));
}

/// Flushes the calling thread's counts (thread end).
pub fn flush_current() {
    let _ = CALLS.try_with(|c| {
        if let Ok(mut c) = c.try_borrow_mut()
            && let Some(t) = c.as_mut()
        {
            t.flush();
        }
    });
}

/// Hot methods dominated by the hook overhead in the last window; they are disabled at once.
pub fn take_tuning_candidates() -> Vec<u32> {
    let stats = STATS.lock().take().unwrap_or_default();
    let hot: Vec<u32> = stats
        .into_iter()
        .filter(|&(_, (calls, ns))| calls >= TUNE_MIN_CALLS && ns / calls.max(1) < TUNE_MAX_SELF_NS)
        .map(|(m, _)| m)
        .collect();
    for &m in &hot {
        if let Some(w) = DISABLED.get((m / 64) as usize) {
            w.fetch_or(1 << (m % 64), Ordering::Relaxed);
        }
    }
    hot
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tracer's state is global; tests that enter and exit methods must not overlap.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn self_times_and_unwinding() {
        let _serial = SERIAL.lock();
        let mut t = ThreadCalls::new(None);
        t.enter(1);
        t.enter(2);
        t.enter(3);
        // 3 threw past an uninstrumented handler; 2's exit closes it.
        t.exit(2);
        t.enter(2);
        t.exit(2);
        t.exit(1);
        let path = |t: &ThreadCalls, ms: &[u32]| {
            ms.iter().try_fold(0u32, |p, &m| t.index.get(&(p, m)).copied()).map(|n| t.nodes[n as usize])
        };
        assert_eq!(path(&t, &[1]).unwrap().count, 1);
        assert_eq!(path(&t, &[1, 2]).unwrap().count, 2);
        assert_eq!(path(&t, &[1, 2, 3]).unwrap().count, 1);
        assert!(t.stack.is_empty());
        // An exit without a matching entry is ignored.
        t.exit(9);
        assert!(t.stack.is_empty());
        let site = path(&t, &[1, 2]).unwrap();
        assert!(site.total_ns >= site.max_ns && site.max_ns > 0);
    }

    #[test]
    fn tracer_records_in_order_until_its_cap() {
        let _serial = SERIAL.lock();
        let mut t = ThreadCalls::new(None);
        start_trace(Duration::from_secs(60), 5);
        t.enter(1);
        t.enter(2);
        t.exit(2);
        t.exit(1);
        t.enter(3);
        t.exit(3);
        let kinds: Vec<bool> = t.trace.iter().map(|e| e.2).collect();
        assert_eq!(kinds, vec![false, false, true, true, false]);
        assert!(t.trace.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(trace_status(), (false, 5));
        stop_trace();
    }
}
