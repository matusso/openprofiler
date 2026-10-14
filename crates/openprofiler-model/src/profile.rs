//! The [`Profile`] container and its columnar tables.

use crate::stacks::{StackId, StackTrie};
use crate::symbols::{StrId, Symbols};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Nanoseconds since the Unix epoch.
pub type Nanos = i64;

/// What a sample set measures. Each kind becomes its own tree family in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SampleKind {
    /// CPU / execution samples. Weight: estimated nanoseconds.
    Cpu = 0,
    /// Wall-clock samples (all threads regardless of state). Weight: nanoseconds.
    Wall = 1,
    /// Allocation samples. Weight: bytes; `class` column holds the allocated class.
    Alloc = 2,
    /// Contended monitor enter. Weight: blocked nanoseconds.
    Monitor = 3,
    /// `Object.wait` / `LockSupport.park`. Weight: waiting nanoseconds.
    Wait = 4,
    /// Blocking socket / file I/O. Weight: nanoseconds.
    Io = 5,
    /// Thrown exceptions. Weight: count.
    Exception = 6,
}

impl SampleKind {
    pub const ALL: [SampleKind; 7] = [
        SampleKind::Cpu,
        SampleKind::Wall,
        SampleKind::Alloc,
        SampleKind::Monitor,
        SampleKind::Wait,
        SampleKind::Io,
        SampleKind::Exception,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.get(v as usize).copied()
    }

    pub fn unit(self) -> Unit {
        match self {
            SampleKind::Alloc => Unit::Bytes,
            SampleKind::Exception => Unit::Count,
            _ => Unit::Nanos,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SampleKind::Cpu => "cpu",
            SampleKind::Wall => "wall",
            SampleKind::Alloc => "alloc",
            SampleKind::Monitor => "monitor",
            SampleKind::Wait => "wait",
            SampleKind::Io => "io",
            SampleKind::Exception => "exception",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// Thread state at sample time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ThreadState {
    Runnable = 0,
    Waiting = 1,
    Blocked = 2,
    NetIo = 3,
    Sleeping = 4,
    New = 5,
    Terminated = 6,
    #[default]
    Unknown = 7,
}

impl ThreadState {
    pub const ALL: [ThreadState; 8] = [
        ThreadState::Runnable,
        ThreadState::Waiting,
        ThreadState::Blocked,
        ThreadState::NetIo,
        ThreadState::Sleeping,
        ThreadState::New,
        ThreadState::Terminated,
        ThreadState::Unknown,
    ];

    pub fn from_u8(v: u8) -> Self {
        Self::ALL.get(v as usize).copied().unwrap_or(ThreadState::Unknown)
    }

    pub fn name(self) -> &'static str {
        match self {
            ThreadState::Runnable => "runnable",
            ThreadState::Waiting => "waiting",
            ThreadState::Blocked => "blocked",
            ThreadState::NetIo => "net_io",
            ThreadState::Sleeping => "sleeping",
            ThreadState::New => "new",
            ThreadState::Terminated => "terminated",
            ThreadState::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Nanos,
    Bytes,
    #[default]
    Count,
    Percent,
    PerSecond,
}

/// Columnar sample table. All columns have the same length.
///
/// A row may represent several identical samples (`count > 1`), which is how live sessions store
/// per-second pre-aggregated buckets.
#[derive(Debug, Clone, Default)]
pub struct SampleSet {
    pub time: Vec<Nanos>,
    /// Index into [`Profile::threads`].
    pub thread: Vec<u32>,
    pub state: Vec<ThreadState>,
    pub stack: Vec<StackId>,
    pub weight: Vec<u64>,
    pub count: Vec<u32>,
    /// Allocated / exception / monitor class (string id), `u32::MAX` when not applicable.
    pub class: Vec<StrId>,
}

impl SampleSet {
    pub fn len(&self) -> usize {
        self.time.len()
    }

    pub fn is_empty(&self) -> bool {
        self.time.is_empty()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        time: Nanos,
        thread: u32,
        state: ThreadState,
        stack: StackId,
        weight: u64,
        count: u32,
        class: Option<StrId>,
    ) {
        self.time.push(time);
        self.thread.push(thread);
        self.state.push(state);
        self.stack.push(stack);
        self.weight.push(weight);
        self.count.push(count);
        self.class.push(class.unwrap_or(u32::MAX));
    }

    pub fn total_weight(&self) -> u64 {
        self.weight.iter().sum()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThreadInfo {
    /// Java thread id (`Thread.threadId()`), or the OS id when no Java id is known.
    pub java_id: u64,
    pub os_id: Option<u64>,
    pub name: String,
    pub group: Option<String>,
    pub is_virtual: bool,
    pub is_daemon: bool,
    pub start: Option<Nanos>,
    pub end: Option<Nanos>,
}

/// Thread state history as intervals, columnar.
#[derive(Debug, Clone, Default)]
pub struct ThreadStateSpans {
    pub thread: Vec<u32>,
    pub start: Vec<Nanos>,
    pub end: Vec<Nanos>,
    pub state: Vec<ThreadState>,
}

impl ThreadStateSpans {
    pub fn push(&mut self, thread: u32, start: Nanos, end: Nanos, state: ThreadState) {
        self.thread.push(thread);
        self.start.push(start);
        self.end.push(end);
        self.state.push(state);
    }

    pub fn len(&self) -> usize {
        self.thread.len()
    }

    pub fn is_empty(&self) -> bool {
        self.thread.is_empty()
    }
}

/// A named time series. `key` is a stable dotted identifier such as `heap.used`, `gc.pause`,
/// `cpu.process`, `threads.live`, `classes.loaded`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TelemetrySeries {
    pub key: String,
    pub label: String,
    pub unit: Unit,
    pub time: Vec<Nanos>,
    pub value: Vec<f64>,
}

impl TelemetrySeries {
    pub fn new(key: &str, label: &str, unit: Unit) -> Self {
        Self { key: key.into(), label: label.into(), unit, time: vec![], value: vec![] }
    }

    pub fn push(&mut self, t: Nanos, v: f64) {
        self.time.push(t);
        self.value.push(v);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GcEvent {
    pub start: Nanos,
    pub duration: Nanos,
    pub name: String,
    pub cause: String,
    pub heap_before: Option<u64>,
    pub heap_after: Option<u64>,
    /// Longest stop-the-world pause within this collection.
    pub pause: Option<Nanos>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorEventKind {
    /// Blocked entering a contended monitor.
    Enter,
    /// `Object.wait`.
    Wait,
    /// `LockSupport.park` (j.u.c locks).
    Park,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorEvent {
    pub start: Nanos,
    pub duration: Nanos,
    pub kind: MonitorEventKind,
    pub thread: u32,
    /// Thread index of the owner at the time, if known.
    pub owner: Option<u32>,
    pub monitor_class: String,
    /// Identity hash or address of the monitor, if known.
    pub monitor_id: Option<u64>,
    pub stack: StackId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Bookmark {
    pub time: Nanos,
    pub label: String,
    pub color: Option<String>,
}

/// A captured thread dump.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThreadDump {
    pub time: Nanos,
    pub label: String,
    pub threads: Vec<DumpedThread>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DumpedThread {
    pub name: String,
    pub java_id: u64,
    pub state: String,
    pub is_virtual: bool,
    /// Rendered frames, leaf first (`java.lang.Thread.sleep(Native Method)`).
    pub frames: Vec<String>,
    pub locked: Vec<String>,
    pub waiting_on: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JvmInfo {
    pub pid: Option<u32>,
    pub name: String,
    pub vm_name: String,
    pub vm_version: String,
    pub vm_vendor: String,
    pub java_version: String,
    pub jvm_args: Vec<String>,
    pub main_class: String,
    pub os: String,
    pub arch: String,
    pub cpus: Option<u32>,
    pub max_heap: Option<u64>,
    pub start_time: Option<Nanos>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfileMeta {
    /// `jfr`, `openprofiler`, `live`, …
    pub source: String,
    pub jvm: JvmInfo,
    pub start: Nanos,
    pub end: Nanos,
    /// Nominal CPU sampling interval, if known.
    pub cpu_interval: Option<Nanos>,
    pub properties: BTreeMap<String, String>,
}

/// Everything known about one profiled JVM over a time window.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub meta: ProfileMeta,
    pub symbols: Symbols,
    pub stacks: StackTrie,
    pub samples: BTreeMap<SampleKind, SampleSet>,
    pub threads: Vec<ThreadInfo>,
    pub thread_states: ThreadStateSpans,
    pub telemetry: Vec<TelemetrySeries>,
    pub gc: Vec<GcEvent>,
    pub monitors: Vec<MonitorEvent>,
    pub bookmarks: Vec<Bookmark>,
    pub thread_dumps: Vec<ThreadDump>,
}

impl Profile {
    pub fn new(source: &str) -> Self {
        Self {
            meta: ProfileMeta { source: source.into(), ..Default::default() },
            symbols: Symbols::new(),
            ..Default::default()
        }
    }

    pub fn samples_mut(&mut self, kind: SampleKind) -> &mut SampleSet {
        self.samples.entry(kind).or_default()
    }

    pub fn telemetry(&self, key: &str) -> Option<&TelemetrySeries> {
        self.telemetry.iter().find(|s| s.key == key)
    }

    pub fn telemetry_mut(&mut self, key: &str, label: &str, unit: Unit) -> &mut TelemetrySeries {
        if let Some(i) = self.telemetry.iter().position(|s| s.key == key) {
            return &mut self.telemetry[i];
        }
        self.telemetry.push(TelemetrySeries::new(key, label, unit));
        self.telemetry.last_mut().unwrap()
    }

    /// Returns the index of the thread with this Java id, registering it if new.
    pub fn thread_index(&mut self, java_id: u64, name: impl FnOnce() -> String) -> u32 {
        if let Some(i) = self.threads.iter().position(|t| t.java_id == java_id) {
            return i as u32;
        }
        self.threads.push(ThreadInfo { java_id, name: name(), ..Default::default() });
        (self.threads.len() - 1) as u32
    }

    /// Widens `meta.start`/`meta.end` to cover `t`.
    pub fn extend_range(&mut self, t: Nanos) {
        if self.meta.start == 0 || t < self.meta.start {
            self.meta.start = t;
        }
        if t > self.meta.end {
            self.meta.end = t;
        }
    }

    /// Sample kinds that have at least one sample.
    pub fn available_kinds(&self) -> Vec<SampleKind> {
        self.samples.iter().filter(|(_, s)| !s.is_empty()).map(|(k, _)| *k).collect()
    }
}
