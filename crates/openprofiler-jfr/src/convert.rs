//! Maps JFR events onto the openprofiler-model [`Profile`].
//!
//! Each chunk is converted independently (in parallel) into a chunk-local [`ChunkOut`], and the
//! results are then folded, in chunk order, into one profile by [`Merger`], which remaps symbol,
//! stack and thread ids.
//!
//! Mapping decisions:
//! - `jdk.ExecutionSample` → [`SampleKind::Cpu`], weighted by the sampling period from
//!   `jdk.ActiveSetting` (or the measured inter-sample interval, else 20 ms). When the recording
//!   contains `jdk.CPUTimeSample` (JDK 25+ CPU-time sampler), those samples are used for `Cpu`
//!   instead, weighted by their own `samplingPeriod`.
//! - `jdk.NativeMethodSample` samples threads parked in native code, which is mostly blocking
//!   I/O (socket reads, `poll`, `accept`). Together with the execution samples it forms the
//!   [`SampleKind::Wall`] set: execution samples keep their state (normally `Runnable`) and native
//!   samples get [`ThreadState::NetIo`], so the wall view can be split by state.
//! - Allocation: `jdk.ObjectAllocationSample` (weight = its `weight` field) is preferred; the TLAB
//!   events are used only when no allocation samples exist, weighted by TLAB size (in new TLAB)
//!   or allocation size (outside TLAB).
//! - `jdk.JavaMonitorEnter` → `Monitor`; `jdk.JavaMonitorWait` and `jdk.ThreadPark` → `Wait`;
//!   socket/file read/write → `Io`; `jdk.JavaExceptionThrow` (or `jdk.JavaErrorThrow` when no
//!   exception events exist) → `Exception`. These duration events also produce thread state spans.
//! - Truncated stacks get a synthetic root frame `[truncated]` so they group separately.

use crate::chunk::{Chunk, Raw};
use crate::threaddump;
use openprofiler_model::stacks::ROOT_STACK;
use openprofiler_model::{
    FrameId, FrameKind, GcEvent, JvmInfo, Method, MethodId, MonitorEvent, MonitorEventKind, Nanos, Profile,
    SampleKind, SampleSet, StackId, StrId, ThreadInfo, ThreadState, Unit,
};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

const DEFAULT_PERIOD: Nanos = 20_000_000;
const EXEC: &str = "jdk.ExecutionSample";
const NATIVE: &str = "jdk.NativeMethodSample";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H {
    Exec,
    Native,
    CpuTime,
    AllocSample,
    TlabNew,
    TlabOutside,
    MonitorEnter,
    MonitorWait,
    Park,
    Sleep,
    Io,
    ExceptionThrow,
    ErrorThrow,
    ExceptionStats,
    HeapSummary,
    Metaspace,
    CpuLoad,
    ThreadStats,
    ClassStats,
    PhysMem,
    Gc,
    ThreadStart,
    ThreadEnd,
    JvmInfo,
    OsInfo,
    CpuInfo,
    SysProp,
    HeapConfig,
    ActiveSetting,
    ThreadDump,
}

fn handler_for(name: &str) -> Option<H> {
    Some(match name {
        EXEC => H::Exec,
        NATIVE => H::Native,
        "jdk.CPUTimeSample" => H::CpuTime,
        "jdk.ObjectAllocationSample" => H::AllocSample,
        "jdk.ObjectAllocationInNewTLAB" => H::TlabNew,
        "jdk.ObjectAllocationOutsideTLAB" => H::TlabOutside,
        "jdk.JavaMonitorEnter" => H::MonitorEnter,
        "jdk.JavaMonitorWait" => H::MonitorWait,
        "jdk.ThreadPark" => H::Park,
        "jdk.ThreadSleep" => H::Sleep,
        "jdk.SocketRead" | "jdk.SocketWrite" | "jdk.FileRead" | "jdk.FileWrite" | "jdk.FileForce" => H::Io,
        "jdk.JavaExceptionThrow" => H::ExceptionThrow,
        "jdk.JavaErrorThrow" => H::ErrorThrow,
        "jdk.ExceptionStatistics" => H::ExceptionStats,
        "jdk.GCHeapSummary" => H::HeapSummary,
        "jdk.MetaspaceSummary" => H::Metaspace,
        "jdk.CPULoad" => H::CpuLoad,
        "jdk.JavaThreadStatistics" => H::ThreadStats,
        "jdk.ClassLoadingStatistics" => H::ClassStats,
        "jdk.PhysicalMemory" => H::PhysMem,
        "jdk.GarbageCollection" => H::Gc,
        "jdk.ThreadStart" => H::ThreadStart,
        "jdk.ThreadEnd" => H::ThreadEnd,
        "jdk.JVMInformation" => H::JvmInfo,
        "jdk.OSInformation" => H::OsInfo,
        "jdk.CPUInformation" => H::CpuInfo,
        "jdk.InitialSystemProperty" => H::SysProp,
        "jdk.GCHeapConfiguration" => H::HeapConfig,
        "jdk.ActiveSetting" => H::ActiveSetting,
        "jdk.ThreadDump" => H::ThreadDump,
        _ => return None,
    })
}

/// Parses JFR setting durations such as `20 ms`, `10ms`, `1 s`.
pub(crate) fn parse_period(s: &str) -> Option<Nanos> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..split].parse().ok()?;
    let mult = match s[split..].trim() {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        "d" => 86_400_000_000_000,
        _ => return None,
    };
    Some(n * mult).filter(|&v| v > 0)
}

/// Chunk-local conversion result.
pub(crate) struct ChunkOut {
    p: Profile,
    exec: SampleSet,
    native: SampleSet,
    cpu_time: SampleSet,
    alloc_sample: SampleSet,
    tlab: SampleSet,
    errors: SampleSet,
    periods: FxHashMap<String, Nanos>,
    /// GC id per entry of `p.gc`.
    gc_ids: Vec<i64>,
    /// `(gc id, before?, heap used)`.
    heap_gc: Vec<(i64, bool, u64)>,
    props: Vec<(String, String)>,
    jvm: JvmInfo,
    start: Nanos,
    end: Nanos,
    warning: Option<String>,
}

/// Accessors over one decoded event.
struct Ev<'c, 'a> {
    c: &'c Chunk<'a>,
    t: u32,
    f: &'c [Raw],
}

impl<'c, 'a> Ev<'c, 'a> {
    fn raw(&self, name: &str) -> Option<&'c Raw> {
        self.c.md.types[self.t as usize].field_index(name).and_then(|i| self.f.get(i))
    }
    fn i64(&self, name: &str) -> Option<i64> {
        self.raw(name).and_then(|r| self.c.resolve(r).as_i64())
    }
    fn f64(&self, name: &str) -> Option<f64> {
        self.raw(name).and_then(|r| self.c.resolve(r).as_f64())
    }
    fn text(&self, name: &str) -> Option<&'c str> {
        self.raw(name).and_then(|r| self.c.any_text(r))
    }
    fn nanos(&self, name: &str) -> Option<Nanos> {
        self.c.field_nanos(self.t, self.f, name)
    }
    fn start(&self) -> Nanos {
        self.nanos("startTime").unwrap_or(self.c.header.start_nanos)
    }
    fn duration(&self) -> Nanos {
        self.nanos("duration").unwrap_or(0).max(0)
    }
    fn thread_raw(&self) -> Option<&'c Raw> {
        self.raw("sampledThread").or_else(|| self.raw("eventThread"))
    }
}

struct Conv<'c, 'a> {
    c: &'c Chunk<'a>,
    out: ChunkOut,
    thread_by_key: FxHashMap<i64, u32>,
    thread_by_id: FxHashMap<u64, u32>,
    stack_by_key: FxHashMap<i64, StackId>,
    method_by_key: FxHashMap<i64, MethodId>,
    class_by_key: FxHashMap<i64, StrId>,
    kind_by_key: FxHashMap<i64, FrameKind>,
    frames: Vec<FrameId>,
    truncated_frame: Option<FrameId>,
}

pub(crate) fn convert_chunk(c: &Chunk<'_>) -> ChunkOut {
    let mut handlers: FxHashMap<u64, (H, u32)> = FxHashMap::default();
    for (i, t) in c.md.types.iter().enumerate() {
        if let Some(h) = handler_for(&t.name) {
            handlers.insert(t.id, (h, i as u32));
        }
    }
    let mut conv = Conv {
        c,
        out: ChunkOut {
            p: Profile::new("jfr"),
            exec: SampleSet::default(),
            native: SampleSet::default(),
            cpu_time: SampleSet::default(),
            alloc_sample: SampleSet::default(),
            tlab: SampleSet::default(),
            errors: SampleSet::default(),
            periods: FxHashMap::default(),
            gc_ids: vec![],
            heap_gc: vec![],
            props: vec![],
            jvm: JvmInfo::default(),
            start: c.header.start_nanos,
            end: c.end_nanos(),
            warning: None,
        },
        thread_by_key: FxHashMap::default(),
        thread_by_id: FxHashMap::default(),
        stack_by_key: FxHashMap::default(),
        method_by_key: FxHashMap::default(),
        class_by_key: FxHashMap::default(),
        kind_by_key: FxHashMap::default(),
        frames: vec![],
        truncated_frame: None,
    };
    for rec in c.event_records() {
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                conv.out.warning = Some(format!("chunk at {}: {e:#}", c.header.offset));
                break;
            }
        };
        let Some(&(h, _)) = handlers.get(&rec.type_id) else { continue };
        match c.decode(&rec) {
            Ok((t, f)) => conv.handle(h, &Ev { c, t, f: &f }),
            Err(e) => {
                conv.out.warning = Some(format!("chunk at {}: {e:#}", c.header.offset));
                break;
            }
        }
    }
    let mut out = conv.out;
    for (set, name) in [(&mut out.exec, EXEC), (&mut out.native, NATIVE)] {
        if let Some(&p) = out.periods.get(name) {
            set.weight.iter_mut().for_each(|w| *w = p as u64);
        }
    }
    out
}

impl<'c, 'a> Conv<'c, 'a> {
    fn p(&mut self) -> &mut Profile {
        &mut self.out.p
    }

    fn thread(&mut self, v: Option<&Raw>) -> u32 {
        let Some(v) = v else { return self.unknown_thread() };
        let key = if let Raw::Ref(_, k) = v { Some(*k) } else { None };
        if let Some(k) = key
            && let Some(&i) = self.thread_by_key.get(&k)
        {
            return i;
        }
        let Some(tr) = self.c.thread_ref(v) else { return self.unknown_thread() };
        let id = tr.java_id.or(tr.os_id).unwrap_or(0);
        let idx = match self.thread_by_id.get(&id) {
            Some(&i) => i,
            None => {
                let threads = &mut self.out.p.threads;
                threads.push(ThreadInfo {
                    java_id: id,
                    os_id: tr.os_id,
                    name: tr.name,
                    group: tr.group,
                    is_virtual: tr.is_virtual,
                    ..Default::default()
                });
                let i = (threads.len() - 1) as u32;
                self.thread_by_id.insert(id, i);
                i
            }
        };
        if let Some(k) = key {
            self.thread_by_key.insert(k, idx);
        }
        idx
    }

    fn unknown_thread(&mut self) -> u32 {
        // Java id 0 is never assigned to a real thread; use it for events without a thread.
        if let Some(&i) = self.thread_by_id.get(&0) {
            return i;
        }
        self.out.p.threads.push(ThreadInfo {
            java_id: 0,
            name: "<unknown thread>".into(),
            ..Default::default()
        });
        let i = (self.out.p.threads.len() - 1) as u32;
        self.thread_by_id.insert(0, i);
        i
    }

    fn class(&mut self, v: Option<&Raw>) -> Option<StrId> {
        let v = v?;
        let key = if let Raw::Ref(_, k) = v { Some(*k) } else { None };
        if let Some(k) = key
            && let Some(&s) = self.class_by_key.get(&k)
        {
            return Some(s);
        }
        let name = self.c.class_name(v)?;
        let id = self.out.p.symbols.intern_str(&name);
        if let Some(k) = key {
            self.class_by_key.insert(k, id);
        }
        Some(id)
    }

    fn method(&mut self, v: Option<&Raw>) -> MethodId {
        let c = self.c;
        let key = match v {
            Some(Raw::Ref(_, k)) => Some(*k),
            _ => None,
        };
        if let Some(k) = key
            && let Some(&m) = self.method_by_key.get(&k)
        {
            return m;
        }
        let (class, name, desc) = match v {
            Some(m) => (
                c.field_raw(m, "type").and_then(|t| c.class_name(t)).unwrap_or_default(),
                c.field_raw(m, "name").and_then(|n| c.text(n)).unwrap_or("<unknown>"),
                c.field_raw(m, "descriptor").and_then(|n| c.text(n)).unwrap_or(""),
            ),
            None => (String::new(), "<unknown>", ""),
        };
        let id = self.out.p.symbols.intern_method(&class, name, desc);
        if let Some(k) = key {
            self.method_by_key.insert(k, id);
        }
        id
    }

    fn frame_kind(&mut self, v: Option<&Raw>) -> FrameKind {
        let Some(v) = v else { return FrameKind::Unknown };
        if let Raw::Ref(_, k) = v {
            if let Some(&fk) = self.kind_by_key.get(k) {
                return fk;
            }
            let fk = self.c.frame_kind(v);
            self.kind_by_key.insert(*k, fk);
            return fk;
        }
        self.c.frame_kind(v)
    }

    fn stack(&mut self, v: Option<&Raw>) -> StackId {
        let c = self.c;
        let Some(v) = v else { return ROOT_STACK };
        let key = if let Raw::Ref(_, k) = v { Some(*k) } else { None };
        if let Some(k) = key
            && let Some(&s) = self.stack_by_key.get(&k)
        {
            return s;
        }
        let st = c.resolve(v);
        if matches!(st, Raw::Null) {
            return ROOT_STACK;
        }
        let truncated = c.field(st, "truncated").and_then(Raw::as_bool).unwrap_or(false);
        let mut frames = std::mem::take(&mut self.frames);
        frames.clear();
        if let Some(Raw::Arr(items)) = c.field(st, "frames") {
            for fr in items.iter() {
                let m = self.method(c.field_raw(fr, "method"));
                let line = c.field(fr, "lineNumber").and_then(Raw::as_i64).unwrap_or(-1);
                let kind = self.frame_kind(c.field_raw(fr, "type"));
                let line = if line <= 0 { -1 } else { line as i32 };
                frames.push(self.out.p.symbols.intern_frame(m, line, kind));
            }
        }
        if truncated {
            let tf = match self.truncated_frame {
                Some(f) => f,
                None => {
                    let m = self.out.p.symbols.intern_method("", "[truncated]", "");
                    let f = self.out.p.symbols.intern_frame(m, -1, FrameKind::Unknown);
                    self.truncated_frame = Some(f);
                    f
                }
            };
            frames.push(tf);
        }
        let id = self.out.p.stacks.intern_leaf_first(&frames);
        self.frames = frames;
        if let Some(k) = key {
            self.stack_by_key.insert(k, id);
        }
        id
    }

    fn telemetry(&mut self, key: &str, label: &str, unit: Unit, t: Nanos, v: Option<f64>) {
        if let Some(v) = v.filter(|v| v.is_finite()) {
            self.p().telemetry_mut(key, label, unit).push(t, v);
        }
    }

    fn handle(&mut self, h: H, e: &Ev<'_, '_>) {
        let c = self.c;
        let start = e.start();
        match h {
            H::Exec | H::Native => {
                let thread = self.thread(e.thread_raw());
                let stack = self.stack(e.raw("stackTrace"));
                let state = if h == H::Native {
                    ThreadState::NetIo
                } else {
                    e.text("state").map_or(ThreadState::Runnable, map_thread_state)
                };
                let set = if h == H::Exec { &mut self.out.exec } else { &mut self.out.native };
                set.push(start, thread, state, stack, 0, 1, None);
            }
            H::CpuTime => {
                if e.raw("failed").and_then(|r| c.resolve(r).as_bool()).unwrap_or(false) {
                    return;
                }
                let thread = self.thread(e.thread_raw());
                let stack = self.stack(e.raw("stackTrace"));
                let w = e.nanos("samplingPeriod").filter(|&p| p > 0).unwrap_or(DEFAULT_PERIOD);
                self.out.cpu_time.push(start, thread, ThreadState::Runnable, stack, w as u64, 1, None);
            }
            H::AllocSample | H::TlabNew | H::TlabOutside => {
                let thread = self.thread(e.thread_raw());
                let stack = self.stack(e.raw("stackTrace"));
                let class = self.class(e.raw("objectClass"));
                let w = match h {
                    H::AllocSample => e.i64("weight"),
                    H::TlabNew => e.i64("tlabSize"),
                    _ => e.i64("allocationSize"),
                }
                .unwrap_or(0)
                .max(0) as u64;
                let set = if h == H::AllocSample { &mut self.out.alloc_sample } else { &mut self.out.tlab };
                set.push(start, thread, ThreadState::Runnable, stack, w, 1, class);
            }
            H::MonitorEnter | H::MonitorWait | H::Park => {
                let thread = self.thread(e.thread_raw());
                let stack = self.stack(e.raw("stackTrace"));
                let dur = e.duration();
                let class_raw = if h == H::Park { e.raw("parkedClass") } else { e.raw("monitorClass") };
                let class = self.class(class_raw);
                let owner_raw = match h {
                    H::MonitorEnter => e.raw("previousOwner"),
                    H::MonitorWait => e.raw("notifier"),
                    _ => None,
                };
                let owner =
                    owner_raw.filter(|r| !matches!(c.resolve(r), Raw::Null)).map(|r| self.thread(Some(r)));
                let (kind, state, mk) = match h {
                    H::MonitorEnter => (SampleKind::Monitor, ThreadState::Blocked, MonitorEventKind::Enter),
                    H::MonitorWait => (SampleKind::Wait, ThreadState::Waiting, MonitorEventKind::Wait),
                    _ => (SampleKind::Wait, ThreadState::Waiting, MonitorEventKind::Park),
                };
                let monitor_class = class.map(|s| self.out.p.symbols.str(s).to_owned()).unwrap_or_default();
                let p = self.p();
                p.samples_mut(kind).push(start, thread, state, stack, dur as u64, 1, class);
                p.thread_states.push(thread, start, start + dur, state);
                p.monitors.push(MonitorEvent {
                    start,
                    duration: dur,
                    kind: mk,
                    thread,
                    owner,
                    monitor_class,
                    monitor_id: e.i64("address").filter(|&a| a != 0).map(|a| a as u64),
                    stack,
                });
            }
            H::Sleep | H::Io => {
                let thread = self.thread(e.thread_raw());
                let dur = e.duration();
                let state = if h == H::Sleep { ThreadState::Sleeping } else { ThreadState::NetIo };
                if h == H::Io {
                    let stack = self.stack(e.raw("stackTrace"));
                    self.p()
                        .samples_mut(SampleKind::Io)
                        .push(start, thread, state, stack, dur as u64, 1, None);
                }
                self.p().thread_states.push(thread, start, start + dur, state);
            }
            H::ExceptionThrow | H::ErrorThrow => {
                let thread = self.thread(e.thread_raw());
                let stack = self.stack(e.raw("stackTrace"));
                let class = self.class(e.raw("thrownClass"));
                let set = if h == H::ExceptionThrow {
                    self.out.p.samples_mut(SampleKind::Exception)
                } else {
                    &mut self.out.errors
                };
                set.push(start, thread, ThreadState::Runnable, stack, 1, 1, class);
            }
            H::ExceptionStats => {
                let v = e.i64("throwables").map(|v| v as f64);
                self.telemetry("exceptions.total", "Throwables created", Unit::Count, start, v);
            }
            H::HeapSummary => {
                let used = e.i64("heapUsed");
                self.telemetry("heap.used", "Heap used", Unit::Bytes, start, used.map(|v| v as f64));
                let space = e.raw("heapSpace");
                let committed = space.and_then(|s| c.field(s, "committedSize")).and_then(Raw::as_i64);
                let reserved = space.and_then(|s| c.field(s, "reservedSize")).and_then(Raw::as_i64);
                self.telemetry(
                    "heap.committed",
                    "Heap committed",
                    Unit::Bytes,
                    start,
                    committed.map(|v| v as f64),
                );
                self.telemetry("heap.max", "Heap reserved", Unit::Bytes, start, reserved.map(|v| v as f64));
                if let (Some(id), Some(used)) = (e.i64("gcId"), used) {
                    let before = e.text("when").is_some_and(|w| w.starts_with("Before"));
                    self.out.heap_gc.push((id, before, used.max(0) as u64));
                }
            }
            H::Metaspace => {
                let used = e.raw("metaspace").and_then(|m| c.field(m, "used")).and_then(Raw::as_i64);
                self.telemetry("nonheap.used", "Metaspace used", Unit::Bytes, start, used.map(|v| v as f64));
            }
            H::CpuLoad => {
                let jvm = match (e.f64("jvmUser"), e.f64("jvmSystem")) {
                    (Some(u), Some(s)) => Some((u + s) * 100.0),
                    _ => None,
                };
                self.telemetry("cpu.process", "Process CPU", Unit::Percent, start, jvm);
                let machine = e.f64("machineTotal").map(|v| v * 100.0);
                self.telemetry("cpu.system", "System CPU", Unit::Percent, start, machine);
            }
            H::ThreadStats => {
                let live = e.f64("activeCount");
                let daemon = e.f64("daemonCount");
                self.telemetry("threads.live", "Live threads", Unit::Count, start, live);
                self.telemetry("threads.daemon", "Daemon threads", Unit::Count, start, daemon);
            }
            H::ClassStats => {
                let loaded = match (e.i64("loadedClassCount"), e.i64("unloadedClassCount")) {
                    (Some(l), u) => Some((l - u.unwrap_or(0)) as f64),
                    _ => None,
                };
                self.telemetry("classes.loaded", "Loaded classes", Unit::Count, start, loaded);
            }
            H::PhysMem => {
                let used = e.f64("usedSize");
                self.telemetry("memory.physical.used", "Physical memory used", Unit::Bytes, start, used);
            }
            H::Gc => {
                let sum = e.nanos("sumOfPauses");
                let ev = GcEvent {
                    start,
                    duration: e.duration(),
                    name: e.text("name").unwrap_or("").to_owned(),
                    cause: e.text("cause").unwrap_or("").to_owned(),
                    heap_before: None,
                    heap_after: None,
                    pause: e.nanos("longestPause").or(sum),
                };
                self.telemetry("gc.pause", "GC pause", Unit::Nanos, start, sum.map(|v| v as f64));
                self.out.gc_ids.push(e.i64("gcId").unwrap_or(-1));
                self.out.p.gc.push(ev);
            }
            H::ThreadStart | H::ThreadEnd => {
                let r = e.raw("thread");
                if r.is_some_and(|r| !matches!(c.resolve(r), Raw::Null)) {
                    let t = self.thread(r) as usize;
                    let info = &mut self.out.p.threads[t];
                    if h == H::ThreadStart {
                        info.start = Some(info.start.map_or(start, |s| s.min(start)));
                    } else {
                        info.end = Some(info.end.map_or(start, |s| s.max(start)));
                    }
                }
            }
            H::JvmInfo => {
                let j = &mut self.out.jvm;
                j.vm_name = e.text("jvmName").unwrap_or("").to_owned();
                j.vm_version = e.text("jvmVersion").unwrap_or("").to_owned();
                j.jvm_args =
                    e.text("jvmArguments").unwrap_or("").split_whitespace().map(str::to_owned).collect();
                let java_args = e.text("javaArguments").unwrap_or("");
                j.main_class = java_args.split_whitespace().next().unwrap_or("").to_owned();
                j.start_time = e.nanos("jvmStartTime");
                j.pid = e.i64("pid").map(|p| p as u32);
                let flags = e.text("jvmFlags").unwrap_or("").to_owned();
                if !flags.is_empty() {
                    self.out.props.push(("jvm.flags".into(), flags));
                }
                if !java_args.is_empty() {
                    self.out.props.push(("java.arguments".into(), java_args.to_owned()));
                }
            }
            H::OsInfo => {
                let os = e.text("osVersion").unwrap_or("").lines().next().unwrap_or("").to_owned();
                self.out.props.push(("os.version.full".into(), os));
            }
            H::CpuInfo => {
                self.out.jvm.cpus = e.i64("hwThreads").filter(|&n| n > 0).map(|n| n as u32);
                if let Some(cpu) = e.text("cpu") {
                    self.out.props.push(("cpu.model".into(), cpu.to_owned()));
                }
            }
            H::SysProp => {
                if let (Some(k), Some(v)) = (e.text("key"), e.text("value")) {
                    self.out.props.push((k.to_owned(), v.to_owned()));
                }
            }
            H::HeapConfig => {
                self.out.jvm.max_heap = e.i64("maxSize").filter(|&v| v > 0).map(|v| v as u64);
            }
            H::ActiveSetting => {
                if e.text("name") != Some("period") {
                    return;
                }
                let Some(id) = e.i64("id") else { return };
                let Some(&t) = c.md.by_id.get(&(id as u64)) else { return };
                let name = c.md.types[t as usize].name.as_str();
                if (name == EXEC || name == NATIVE)
                    && let Some(p) = e.text("value").and_then(parse_period)
                {
                    self.out.periods.insert(name.to_owned(), p);
                }
            }
            H::ThreadDump => {
                if let Some(text) = e.text("result") {
                    self.out.p.thread_dumps.push(threaddump::parse(text, start));
                }
            }
        }
    }
}

fn map_thread_state(s: &str) -> ThreadState {
    let s = s.to_ascii_uppercase();
    if s.contains("RUNNABLE") {
        ThreadState::Runnable
    } else if s.contains("SLEEP") {
        ThreadState::Sleeping
    } else if s.contains("BLOCKED") || s.contains("MONITOR_ENTER") {
        ThreadState::Blocked
    } else if s.contains("WAIT") || s.contains("PARK") {
        ThreadState::Waiting
    } else if s.contains("NATIVE") {
        ThreadState::NetIo
    } else if s.contains("NEW") {
        ThreadState::New
    } else if s.contains("TERMINATED") {
        ThreadState::Terminated
    } else {
        ThreadState::Unknown
    }
}

/// Folds chunk results into one profile.
pub(crate) struct Merger {
    p: Profile,
    exec: SampleSet,
    native: SampleSet,
    cpu_time: SampleSet,
    alloc_sample: SampleSet,
    tlab: SampleSet,
    errors: SampleSet,
    exec_pending: Vec<(usize, usize)>,
    native_pending: Vec<(usize, usize)>,
    periods: FxHashMap<String, Nanos>,
    gc_ids: Vec<i64>,
    heap_gc: FxHashMap<i64, (Option<u64>, Option<u64>)>,
    thread_ids: FxHashMap<u64, u32>,
    jvm: JvmInfo,
    props: BTreeMap<String, String>,
    chunks: usize,
    warnings: Vec<String>,
    start: Nanos,
    end: Nanos,
}

fn append(dst: &mut SampleSet, src: &SampleSet, stacks: &[StackId], threads: &[u32], strs: &[StrId]) {
    dst.time.extend_from_slice(&src.time);
    dst.thread.extend(src.thread.iter().map(|&t| threads[t as usize]));
    dst.state.extend_from_slice(&src.state);
    dst.stack.extend(src.stack.iter().map(|&s| stacks[s as usize]));
    dst.weight.extend_from_slice(&src.weight);
    dst.count.extend_from_slice(&src.count);
    dst.class.extend(src.class.iter().map(|&c| if c == u32::MAX { c } else { strs[c as usize] }));
}

impl Merger {
    pub fn new() -> Self {
        Merger {
            p: Profile::new("jfr"),
            exec: SampleSet::default(),
            native: SampleSet::default(),
            cpu_time: SampleSet::default(),
            alloc_sample: SampleSet::default(),
            tlab: SampleSet::default(),
            errors: SampleSet::default(),
            exec_pending: vec![],
            native_pending: vec![],
            periods: FxHashMap::default(),
            gc_ids: vec![],
            heap_gc: FxHashMap::default(),
            thread_ids: FxHashMap::default(),
            jvm: JvmInfo::default(),
            props: BTreeMap::new(),
            chunks: 0,
            warnings: vec![],
            start: i64::MAX,
            end: i64::MIN,
        }
    }

    pub fn warn(&mut self, w: String) {
        self.warnings.push(w);
    }

    pub fn add(&mut self, out: ChunkOut) {
        self.chunks += 1;
        self.start = self.start.min(out.start);
        self.end = self.end.max(out.end);
        if let Some(w) = out.warning {
            self.warnings.push(w);
        }
        let src = out.p;
        let dst = &mut self.p;
        let strs: Vec<StrId> = src.symbols.strings().iter().map(|s| dst.symbols.intern_str(s)).collect();
        let methods: Vec<MethodId> = src
            .symbols
            .methods()
            .iter()
            .map(|m| {
                dst.symbols.intern_method_ids(Method {
                    class: strs[m.class as usize],
                    name: strs[m.name as usize],
                    signature: strs[m.signature as usize],
                })
            })
            .collect();
        let frames: Vec<FrameId> = src
            .symbols
            .frames()
            .iter()
            .map(|f| dst.symbols.intern_frame(methods[f.method as usize], f.line, f.kind))
            .collect();
        let (parents, leafs) = src.stacks.raw();
        let mut stacks = vec![ROOT_STACK; parents.len()];
        for i in 1..parents.len() {
            stacks[i] = dst.stacks.push(stacks[parents[i] as usize], frames[leafs[i] as usize]);
        }
        let threads: Vec<u32> = src
            .threads
            .into_iter()
            .map(|t| match self.thread_ids.get(&t.java_id) {
                Some(&i) => {
                    let d = &mut dst.threads[i as usize];
                    if let Some(s) = t.start {
                        d.start = Some(d.start.map_or(s, |x| x.min(s)));
                    }
                    if let Some(e) = t.end {
                        d.end = Some(d.end.map_or(e, |x| x.max(e)));
                    }
                    if d.name.is_empty() || d.name.starts_with('<') {
                        d.name = t.name;
                    }
                    i
                }
                None => {
                    dst.threads.push(t.clone());
                    let i = (dst.threads.len() - 1) as u32;
                    self.thread_ids.insert(t.java_id, i);
                    i
                }
            })
            .collect();

        for (kind, set) in &src.samples {
            append(dst.samples_mut(*kind), set, &stacks, &threads, &strs);
        }
        for (dst_set, src_set, pending, name) in [
            (&mut self.exec, &out.exec, Some(&mut self.exec_pending), EXEC),
            (&mut self.native, &out.native, Some(&mut self.native_pending), NATIVE),
            (&mut self.cpu_time, &out.cpu_time, None, ""),
            (&mut self.alloc_sample, &out.alloc_sample, None, ""),
            (&mut self.tlab, &out.tlab, None, ""),
            (&mut self.errors, &out.errors, None, ""),
        ] {
            let before = dst_set.len();
            append(dst_set, src_set, &stacks, &threads, &strs);
            if let Some(pending) = pending
                && !out.periods.contains_key(name)
                && dst_set.len() > before
            {
                pending.push((before, dst_set.len()));
            }
        }
        for (k, v) in out.periods {
            self.periods.entry(k).or_insert(v);
        }
        for s in src.telemetry {
            let d = dst.telemetry_mut(&s.key, &s.label, s.unit);
            d.time.extend(s.time);
            d.value.extend(s.value);
        }
        dst.gc.extend(src.gc);
        self.gc_ids.extend(out.gc_ids);
        for (id, before, used) in out.heap_gc {
            let e = self.heap_gc.entry(id).or_default();
            if before {
                e.0 = Some(used);
            } else {
                e.1 = Some(used);
            }
        }
        dst.monitors.extend(src.monitors.into_iter().map(|mut m| {
            m.thread = threads[m.thread as usize];
            m.owner = m.owner.map(|o| threads[o as usize]);
            m.stack = stacks[m.stack as usize];
            m
        }));
        let ts = &src.thread_states;
        for i in 0..ts.len() {
            dst.thread_states.push(threads[ts.thread[i] as usize], ts.start[i], ts.end[i], ts.state[i]);
        }
        dst.thread_dumps.extend(src.thread_dumps);
        merge_jvm(&mut self.jvm, out.jvm);
        for (k, v) in out.props {
            self.props.insert(k, v);
        }
    }

    pub fn chunks(&self) -> usize {
        self.chunks
    }

    pub fn finish(mut self) -> Profile {
        let exec_period =
            self.periods.get(EXEC).copied().or_else(|| estimate_period(&self.exec)).unwrap_or(DEFAULT_PERIOD);
        let native_period = self.periods.get(NATIVE).copied().unwrap_or(exec_period);
        for &(a, b) in &self.exec_pending {
            self.exec.weight[a..b].iter_mut().for_each(|w| *w = exec_period as u64);
        }
        for &(a, b) in &self.native_pending {
            self.native.weight[a..b].iter_mut().for_each(|w| *w = native_period as u64);
        }
        let p = &mut self.p;
        let has_cpu_time = !self.cpu_time.is_empty();
        if !self.native.is_empty() {
            let wall = p.samples_mut(SampleKind::Wall);
            for s in [&self.exec, &self.native] {
                wall.time.extend_from_slice(&s.time);
                wall.thread.extend_from_slice(&s.thread);
                wall.state.extend_from_slice(&s.state);
                wall.stack.extend_from_slice(&s.stack);
                wall.weight.extend_from_slice(&s.weight);
                wall.count.extend_from_slice(&s.count);
                wall.class.extend_from_slice(&s.class);
            }
        }
        let cpu =
            if has_cpu_time { std::mem::take(&mut self.cpu_time) } else { std::mem::take(&mut self.exec) };
        if !cpu.is_empty() {
            p.samples.insert(SampleKind::Cpu, cpu);
        }
        let alloc = if self.alloc_sample.is_empty() {
            std::mem::take(&mut self.tlab)
        } else {
            std::mem::take(&mut self.alloc_sample)
        };
        if !alloc.is_empty() {
            p.samples.insert(SampleKind::Alloc, alloc);
        }
        if p.samples.get(&SampleKind::Exception).is_none_or(|s| s.is_empty()) && !self.errors.is_empty() {
            p.samples.insert(SampleKind::Exception, std::mem::take(&mut self.errors));
        }
        p.samples.retain(|_, s| !s.is_empty());

        for (ev, id) in p.gc.iter_mut().zip(&self.gc_ids) {
            if let Some(&(before, after)) = self.heap_gc.get(id) {
                ev.heap_before = before;
                ev.heap_after = after;
            }
        }
        for s in &mut p.telemetry {
            if !s.time.windows(2).all(|w| w[0] <= w[1]) {
                let mut idx: Vec<usize> = (0..s.time.len()).collect();
                idx.sort_by_key(|&i| s.time[i]);
                s.time = idx.iter().map(|&i| s.time[i]).collect();
                s.value = idx.iter().map(|&i| s.value[i]).collect();
            }
        }
        if self.start == i64::MAX {
            self.start = 0;
            self.end = 0;
        }
        p.meta.start = self.start;
        p.meta.end = self.end;
        derive_rates(p);
        p.gc.sort_by_key(|g| g.start);

        p.meta.source = "jfr".into();
        p.meta.cpu_interval = (!has_cpu_time).then_some(exec_period);
        let mut jvm = std::mem::take(&mut self.jvm);
        let prop = |k: &str| self.props.get(k).cloned().unwrap_or_default();
        jvm.java_version = prop("java.version");
        jvm.vm_vendor = prop("java.vm.vendor");
        jvm.os = prop("os.name");
        jvm.arch = prop("os.arch");
        // Recordings without the full system-property set: fall back to the VM version string,
        // "<vm> (27+35) for bsd-aarch64 JRE (27+35), built on …", and the OS banner.
        if jvm.java_version.is_empty() {
            jvm.java_version = between(&jvm.vm_version, "JRE (", ")").unwrap_or_default().to_owned();
        }
        if jvm.vm_vendor.is_empty() {
            jvm.vm_vendor = prop("java.vendor");
        }
        let platform = between(&jvm.vm_version, " for ", " ").unwrap_or_default().to_owned();
        if jvm.arch.is_empty() {
            jvm.arch = platform.split_once('-').map(|(_, a)| a.to_owned()).unwrap_or_default();
        }
        if jvm.os.is_empty() {
            let banner = prop("os.version.full");
            let banner = banner.trim_start_matches("uname:").trim();
            jvm.os = banner.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        }
        jvm.name = if jvm.main_class.is_empty() { "JFR recording".into() } else { jvm.main_class.clone() };
        p.meta.jvm = jvm;
        p.meta.properties = std::mem::take(&mut self.props);
        p.meta.properties.insert("jfr.chunks".into(), self.chunks.to_string());
        if !self.warnings.is_empty() {
            p.meta.properties.insert("jfr.warnings".into(), self.warnings.join("\n"));
        }
        self.p
    }
}

fn between<'s>(s: &'s str, start: &str, end: &str) -> Option<&'s str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

fn merge_jvm(dst: &mut JvmInfo, src: JvmInfo) {
    let take = |d: &mut String, s: String| {
        if d.is_empty() {
            *d = s;
        }
    };
    take(&mut dst.vm_name, src.vm_name);
    take(&mut dst.vm_version, src.vm_version);
    take(&mut dst.main_class, src.main_class);
    if dst.jvm_args.is_empty() {
        dst.jvm_args = src.jvm_args;
    }
    dst.pid = dst.pid.or(src.pid);
    dst.cpus = dst.cpus.or(src.cpus);
    dst.max_heap = dst.max_heap.or(src.max_heap);
    dst.start_time = dst.start_time.or(src.start_time);
}

/// Median of per-thread minimum positive inter-sample deltas, clamped to [1 ms, 1 s].
fn estimate_period(s: &SampleSet) -> Option<Nanos> {
    let mut by_thread: FxHashMap<u32, Vec<Nanos>> = FxHashMap::default();
    for i in 0..s.len() {
        by_thread.entry(s.thread[i]).or_default().push(s.time[i]);
    }
    let mut mins: Vec<Nanos> = by_thread
        .into_values()
        .filter_map(|mut ts| {
            ts.sort_unstable();
            ts.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0).min()
        })
        .collect();
    if mins.is_empty() {
        return None;
    }
    mins.sort_unstable();
    Some(mins[mins.len() / 2].clamp(1_000_000, 1_000_000_000))
}

/// Derived per-second series: allocation rate from allocation samples and GC activity (share of
/// wall time spent in GC pauses).
fn derive_rates(p: &mut Profile) {
    const SEC: Nanos = 1_000_000_000;
    let start = p.meta.start;
    let buckets = (((p.meta.end - start) / SEC) + 1).clamp(1, 10_000_000) as usize;
    if let Some(alloc) = p.samples.get(&SampleKind::Alloc) {
        let mut b = vec![0f64; buckets];
        for i in 0..alloc.len() {
            let k = ((alloc.time[i] - start).max(0) / SEC) as usize;
            b[k.min(buckets - 1)] += alloc.weight[i] as f64;
        }
        let s = p.telemetry_mut("alloc.rate", "Allocation rate (bytes/s)", Unit::PerSecond);
        for (i, v) in b.into_iter().enumerate() {
            s.push(start + i as Nanos * SEC, v);
        }
    }
    if let Some(pause) = p.telemetry("gc.pause").cloned() {
        let mut b = vec![0f64; buckets];
        for (t, v) in pause.time.iter().zip(&pause.value) {
            let k = ((t - start).max(0) / SEC) as usize;
            b[k.min(buckets - 1)] += v;
        }
        let s = p.telemetry_mut("gc.activity", "GC activity", Unit::Percent);
        for (i, v) in b.into_iter().enumerate() {
            s.push(start + i as Nanos * SEC, (v / SEC as f64 * 100.0).min(100.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods() {
        assert_eq!(parse_period("20 ms"), Some(20_000_000));
        assert_eq!(parse_period("10ms"), Some(10_000_000));
        assert_eq!(parse_period("1 s"), Some(1_000_000_000));
        assert_eq!(parse_period("everyChunk"), None);
        assert_eq!(parse_period("0 ms"), None);
    }

    #[test]
    fn states() {
        assert_eq!(map_thread_state("STATE_RUNNABLE"), ThreadState::Runnable);
        assert_eq!(map_thread_state("STATE_SLEEPING"), ThreadState::Sleeping);
        assert_eq!(map_thread_state("STATE_IN_OBJECT_WAIT"), ThreadState::Waiting);
    }
}
