//! In-agent aggregation and delta shipping.
//!
//! The collector owns append-only tables (symbols, stack trie, threads, sample rows, telemetry,
//! thread-state spans, monitor/GC events, bookmarks, thread dumps). Samples are first aggregated per
//! `(second, kind, thread, state, stack, class)` and turned into rows when a batch is cut. A
//! [`Watermark`] is a vector of absolute table positions; a [`DataBatch`] carries everything
//! between two watermarks. In live mode, rows the daemon acknowledged may be discarded.

use crate::resolve::Resolver;
use openprofiler_model::{
    Bookmark, FrameKind, GcEvent, MonitorEventKind, Nanos, SampleKind, StackTrie, Symbols, ThreadState,
};
use openprofiler_proto::v1;
use std::collections::HashMap;

/// Once retained sample rows exceed this, acknowledged rows are dropped (live mode).
const RETAIN_ROWS: usize = 200_000;
const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Open thread-state spans are cut at most this often so live views see current states.
const SPAN_CUT: Nanos = NANOS_PER_SEC;

#[derive(Debug, Clone, Default)]
pub struct AgentThread {
    pub java_id: u64,
    pub os_id: u64,
    pub name: String,
    pub is_daemon: bool,
    pub is_virtual: bool,
    pub start: Nanos,
    pub end: Nanos,
    /// Current open state span `(state, since)`.
    open: Option<(ThreadState, Nanos)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BucketKey {
    second: i64,
    kind: SampleKind,
    thread: u32,
    state: ThreadState,
    stack: u32,
    class: u32,
}

#[derive(Debug, Clone, Copy)]
struct SampleRow {
    time: Nanos,
    kind: SampleKind,
    thread: u32,
    state: ThreadState,
    stack: u32,
    weight: u64,
    count: u32,
    class: u32,
}

#[derive(Debug, Clone)]
pub struct MonitorRow {
    pub start: Nanos,
    pub duration: Nanos,
    pub kind: MonitorEventKind,
    pub thread: u32,
    /// Thread index of the lock owner, if known.
    pub owner: Option<u32>,
    pub monitor_class: String,
    /// Identity hash of the monitor/blocker object, 0 if unknown.
    pub monitor_id: u64,
    pub stack: u32,
}

/// An append-only table that can drop a prefix.
#[derive(Debug)]
struct Table<T> {
    base: u64,
    rows: Vec<T>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self { base: 0, rows: vec![] }
    }
}

impl<T: Clone> Table<T> {
    fn end(&self) -> u64 {
        self.base + self.rows.len() as u64
    }

    fn since(&self, pos: u64) -> &[T] {
        let start = pos.saturating_sub(self.base) as usize;
        &self.rows[start.min(self.rows.len())..]
    }

    fn drop_until(&mut self, pos: u64) {
        let n = pos.saturating_sub(self.base).min(self.rows.len() as u64) as usize;
        self.rows.drain(..n);
        self.base += n as u64;
    }
}

pub struct Collector {
    pub symbols: Symbols,
    pub stacks: StackTrie,
    pub resolver: Resolver,
    pub threads: Vec<AgentThread>,
    thread_updates: Vec<v1::ThreadUpdate>,
    os_threads: HashMap<u64, u32>,
    pending: HashMap<BucketKey, (u64, u32)>,
    samples: Table<SampleRow>,
    telemetry_series: Vec<(String, String, u32)>,
    telemetry: Table<(u32, Nanos, f64)>,
    states: Table<(u32, Nanos, Nanos, ThreadState)>,
    monitors: Table<MonitorRow>,
    gc: Table<GcEvent>,
    bookmarks: Table<Bookmark>,
    thread_dumps: Table<v1::ThreadDump>,
    seq: u64,
    /// Whether acknowledged rows may be dropped.
    pub live: bool,
    last_span_cut: Nanos,
    pub dropped_samples: u64,
}

pub fn now_nanos() -> Nanos {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as Nanos)
        .unwrap_or(0)
}

impl Collector {
    pub fn new(live: bool) -> Self {
        Self {
            symbols: Symbols::new(),
            stacks: StackTrie::new(),
            resolver: Resolver::default(),
            threads: vec![],
            thread_updates: vec![],
            os_threads: HashMap::new(),
            pending: HashMap::new(),
            samples: Table::default(),
            telemetry_series: vec![],
            telemetry: Table::default(),
            states: Table::default(),
            monitors: Table::default(),
            gc: Table::default(),
            bookmarks: Table::default(),
            thread_dumps: Table::default(),
            seq: 0,
            live,
            last_span_cut: 0,
            dropped_samples: 0,
        }
    }

    pub fn add_thread(&mut self, t: AgentThread) -> u32 {
        let idx = self.threads.len() as u32;
        if t.os_id != 0 {
            self.os_threads.insert(t.os_id, idx);
        }
        self.threads.push(t);
        idx
    }

    pub fn new_thread(&mut self, java_id: u64, name: String, is_daemon: bool, now: Nanos) -> u32 {
        self.add_thread(AgentThread { java_id, name, is_daemon, start: now, ..Default::default() })
    }

    pub fn new_virtual_thread(&mut self, java_id: u64, name: String, now: Nanos) -> u32 {
        self.add_thread(AgentThread {
            java_id,
            name,
            is_daemon: true,
            is_virtual: true,
            start: now,
            ..Default::default()
        })
    }

    pub fn set_os_id(&mut self, idx: u32, os_id: u64) {
        if let Some(t) = self.threads.get_mut(idx as usize) {
            t.os_id = os_id;
            self.os_threads.insert(os_id, idx);
        }
    }

    pub fn thread_by_os_id(&self, os_id: u64) -> Option<u32> {
        self.os_threads.get(&os_id).copied()
    }

    /// Finds a thread whose name starts with `prefix` (Linux `comm` names are truncated to 15
    /// bytes) and has no OS id yet. Only unique matches are returned.
    pub fn thread_by_comm(&self, comm: &str) -> Option<u32> {
        let mut found = None;
        for (i, t) in self.threads.iter().enumerate() {
            if t.os_id == 0 && t.end == 0 && !comm.is_empty() && t.name.starts_with(comm) {
                if found.is_some() {
                    return None;
                }
                found = Some(i as u32);
            }
        }
        found
    }

    pub fn end_thread(&mut self, idx: u32, now: Nanos) {
        let Some(t) = self.threads.get_mut(idx as usize) else { return };
        t.end = now;
        if let Some((st, since)) = t.open.take() {
            self.states.rows.push((idx, since, now, st));
        }
        self.thread_updates.push(v1::ThreadUpdate { index: idx, name: String::new(), end: now });
    }

    /// Records the thread's state at `now`, extending or closing its open span.
    pub fn observe_state(&mut self, idx: u32, state: ThreadState, now: Nanos) {
        let Some(t) = self.threads.get_mut(idx as usize) else { return };
        match t.open {
            Some((st, _)) if st == state => {}
            Some((st, since)) => {
                self.states.rows.push((idx, since, now, st));
                t.open = Some((state, now));
            }
            None => t.open = Some((state, now)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_sample(
        &mut self,
        time: Nanos,
        kind: SampleKind,
        thread: u32,
        state: ThreadState,
        stack: u32,
        weight: u64,
        class: Option<u32>,
    ) {
        let key = BucketKey {
            second: time.div_euclid(NANOS_PER_SEC),
            kind,
            thread,
            state,
            stack,
            class: class.unwrap_or(u32::MAX),
        };
        let e = self.pending.entry(key).or_insert((0, 0));
        e.0 += weight;
        e.1 += 1;
    }

    /// Interns a synthetic frame such as `[GC active]`.
    pub fn synthetic_frame(&mut self, label: &str) -> u32 {
        let m = self.symbols.intern_method("", label, "");
        self.symbols.intern_frame(m, -1, FrameKind::Cpp)
    }

    pub fn set_telemetry_keys(&mut self, keys: Vec<(String, String, u32)>) {
        self.telemetry_series = keys;
    }

    pub fn telemetry_keys(&self) -> Vec<String> {
        self.telemetry_series.iter().map(|(k, _, _)| k.clone()).collect()
    }

    pub fn add_telemetry_row(&mut self, time: Nanos, values: &[f64]) {
        for (i, v) in values.iter().enumerate().take(self.telemetry_series.len()) {
            if v.is_finite() {
                self.telemetry.rows.push((i as u32, time, *v));
            }
        }
    }

    pub fn add_gc(&mut self, ev: GcEvent) {
        self.gc.rows.push(ev);
    }

    pub fn add_bookmark(&mut self, b: Bookmark) {
        self.bookmarks.rows.push(b);
    }

    pub fn add_monitor_event(&mut self, row: MonitorRow) {
        self.monitors.rows.push(row);
    }

    pub fn add_thread_dump(&mut self, dump: v1::ThreadDump) {
        self.thread_dumps.rows.push(dump);
    }

    pub fn sample_rows(&self) -> usize {
        self.samples.rows.len() + self.pending.len()
    }

    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut rows: Vec<_> = self.pending.drain().collect();
        rows.sort_unstable_by_key(|(k, _)| (k.second, k.kind, k.thread, k.stack));
        for (k, (weight, count)) in rows {
            self.samples.rows.push(SampleRow {
                time: k.second * NANOS_PER_SEC,
                kind: k.kind,
                thread: k.thread,
                state: k.state,
                stack: k.stack,
                weight,
                count,
                class: k.class,
            });
        }
    }

    fn cut_spans(&mut self, now: Nanos, force: bool) {
        if !force && now - self.last_span_cut < SPAN_CUT {
            return;
        }
        self.last_span_cut = now;
        for (i, t) in self.threads.iter_mut().enumerate() {
            if let Some((st, since)) = t.open
                && now > since
            {
                self.states.rows.push((i as u32, since, now, st));
                t.open = Some((st, now));
            }
        }
    }

    pub fn watermark(&self) -> v1::Watermark {
        v1::Watermark {
            strings: self.symbols.strings().len() as u64,
            methods: self.symbols.methods().len() as u64,
            frames: self.symbols.frames().len() as u64,
            stacks: self.stacks.len() as u64,
            threads: self.threads.len() as u64,
            samples: self.samples.end(),
            telemetry: self.telemetry.end(),
            thread_states: self.states.end(),
            monitors: self.monitors.end(),
            gc: self.gc.end(),
            bookmarks: self.bookmarks.end(),
            seq: self.seq,
            thread_dumps: self.thread_dumps.end(),
        }
    }

    /// The oldest position the collector can still resend from.
    pub fn retained_from(&self) -> v1::Watermark {
        v1::Watermark {
            samples: self.samples.base,
            telemetry: self.telemetry.base,
            thread_states: self.states.base,
            monitors: self.monitors.base,
            gc: self.gc.base,
            bookmarks: self.bookmarks.base,
            thread_dumps: self.thread_dumps.base,
            ..Default::default()
        }
    }

    /// Clamps a resume position to what is still retained and to what exists.
    pub fn clamp_resume(&self, w: &v1::Watermark) -> v1::Watermark {
        let r = self.retained_from();
        let e = self.watermark();
        v1::Watermark {
            strings: w.strings.min(e.strings),
            methods: w.methods.min(e.methods),
            frames: w.frames.min(e.frames),
            stacks: w.stacks.min(e.stacks).max(1),
            threads: w.threads.min(e.threads),
            samples: w.samples.clamp(r.samples, e.samples),
            telemetry: w.telemetry.clamp(r.telemetry, e.telemetry),
            thread_states: w.thread_states.clamp(r.thread_states, e.thread_states),
            monitors: w.monitors.clamp(r.monitors, e.monitors),
            gc: w.gc.clamp(r.gc, e.gc),
            bookmarks: w.bookmarks.clamp(r.bookmarks, e.bookmarks),
            seq: w.seq,
            thread_dumps: w.thread_dumps.clamp(r.thread_dumps, e.thread_dumps),
        }
    }

    /// Cuts a batch with everything since `from`. Returns `None` when nothing changed.
    /// `force_spans` closes open thread-state spans now; `stream` consumes pending thread updates
    /// (a full recording carries end times in the thread table instead).
    pub fn cut_batch(
        &mut self,
        from: &v1::Watermark,
        force_spans: bool,
        stream: bool,
    ) -> Option<v1::DataBatch> {
        let now = now_nanos();
        self.flush_pending();
        self.cut_spans(now, force_spans);
        let mut to = self.watermark();
        let from = self.clamp_resume(from);
        let unchanged = {
            let mut a = from;
            a.seq = 0;
            let mut b = to;
            b.seq = 0;
            a == b && (!stream || self.thread_updates.is_empty())
        };
        if unchanged {
            return None;
        }
        self.seq += 1;
        to.seq = self.seq;

        let symbols = {
            let sb = from.strings as usize;
            let mb = from.methods as usize;
            let fb = from.frames as usize;
            let methods = self.symbols.methods_since(mb);
            let frames = self.symbols.frames_since(fb);
            v1::SymbolDelta {
                string_base: sb as u32,
                strings: self.symbols.strings_since(sb).iter().map(|s| s.to_string()).collect(),
                method_base: mb as u32,
                method_class: methods.iter().map(|m| m.class).collect(),
                method_name: methods.iter().map(|m| m.name).collect(),
                method_signature: methods.iter().map(|m| m.signature).collect(),
                frame_base: fb as u32,
                frame_method: frames.iter().map(|f| f.method).collect(),
                frame_line: frames.iter().map(|f| f.line).collect(),
                frame_kind: frames.iter().map(|f| f.kind as u32).collect(),
            }
        };
        let stacks = {
            let base = (from.stacks as usize).max(1);
            let (parent, frame) = self.stacks.raw();
            v1::StackDelta {
                base: base as u32,
                parent: parent.get(base..).unwrap_or_default().to_vec(),
                frame: frame.get(base..).unwrap_or_default().to_vec(),
            }
        };
        let threads = v1::ThreadInfoBatch {
            base: from.threads as u32,
            threads: self.threads[(from.threads as usize).min(self.threads.len())..]
                .iter()
                .map(|t| v1::ThreadInfo {
                    java_id: t.java_id,
                    os_id: t.os_id,
                    name: t.name.clone(),
                    group: String::new(),
                    is_virtual: t.is_virtual,
                    is_daemon: t.is_daemon,
                    start: t.start,
                    end: t.end,
                })
                .collect(),
            updates: if stream { std::mem::take(&mut self.thread_updates) } else { vec![] },
        };
        let mut by_kind: Vec<v1::SampleBatch> = vec![];
        for r in self.samples.since(from.samples) {
            let kind = r.kind as u32;
            let b = match by_kind.iter_mut().position(|b| b.kind == kind) {
                Some(i) => &mut by_kind[i],
                None => {
                    by_kind.push(v1::SampleBatch { kind, ..Default::default() });
                    by_kind.last_mut().unwrap()
                }
            };
            b.time.push(r.time);
            b.thread.push(r.thread);
            b.state.push(r.state as u32);
            b.stack.push(r.stack);
            b.weight.push(r.weight);
            b.count.push(r.count);
            b.class_id.push(r.class);
        }
        let mut telemetry = v1::TelemetryBatch::default();
        for &(series, t, v) in self.telemetry.since(from.telemetry) {
            let Some((key, label, unit)) = self.telemetry_series.get(series as usize) else { continue };
            let s = match telemetry.series.iter().position(|s| &s.key == key) {
                Some(i) => &mut telemetry.series[i],
                None => {
                    telemetry.series.push(v1::TelemetrySeriesDelta {
                        key: key.clone(),
                        label: label.clone(),
                        unit: *unit,
                        ..Default::default()
                    });
                    telemetry.series.last_mut().unwrap()
                }
            };
            s.time.push(t);
            s.value.push(v);
        }
        let mut thread_states = v1::ThreadStateBatch::default();
        for &(th, s, e, st) in self.states.since(from.thread_states) {
            thread_states.thread.push(th);
            thread_states.start.push(s);
            thread_states.end.push(e);
            thread_states.state.push(st as u32);
        }
        let monitors = v1::MonitorEventBatch {
            events: self
                .monitors
                .since(from.monitors)
                .iter()
                .map(|m| v1::MonitorEvent {
                    start: m.start,
                    duration: m.duration,
                    kind: match m.kind {
                        MonitorEventKind::Enter => 0,
                        MonitorEventKind::Wait => 1,
                        MonitorEventKind::Park => 2,
                    },
                    thread: m.thread,
                    owner: m.owner.map_or(0, |o| o + 1),
                    monitor_class: m.monitor_class.clone(),
                    monitor_id: m.monitor_id,
                    stack: m.stack,
                })
                .collect(),
        };
        let gc = v1::GcEventBatch {
            events: self
                .gc
                .since(from.gc)
                .iter()
                .map(|g| v1::GcEvent {
                    start: g.start,
                    duration: g.duration,
                    name: g.name.clone(),
                    cause: g.cause.clone(),
                    heap_before: g.heap_before.unwrap_or(0),
                    heap_after: g.heap_after.unwrap_or(0),
                    pause: g.pause.unwrap_or(0),
                })
                .collect(),
        };
        let bookmarks = self
            .bookmarks
            .since(from.bookmarks)
            .iter()
            .map(|b| v1::Bookmark {
                time: b.time,
                label: b.label.clone(),
                color: b.color.clone().unwrap_or_default(),
            })
            .collect();
        Some(v1::DataBatch {
            from: Some(from),
            to: Some(to),
            symbols: Some(symbols),
            stacks: Some(stacks),
            threads: Some(threads),
            samples: by_kind,
            telemetry: Some(telemetry),
            thread_states: Some(thread_states),
            monitors: Some(monitors),
            gc: Some(gc),
            bookmarks,
            time: now,
            thread_dumps: self.thread_dumps.since(from.thread_dumps).to_vec(),
        })
    }

    /// The daemon has applied everything up to `w`.
    pub fn ack(&mut self, w: &v1::Watermark) {
        if !self.live {
            return;
        }
        if self.samples.rows.len() > RETAIN_ROWS {
            self.samples.drop_until(w.samples);
        }
        if self.telemetry.rows.len() > RETAIN_ROWS {
            self.telemetry.drop_until(w.telemetry);
        }
        if self.states.rows.len() > RETAIN_ROWS {
            self.states.drop_until(w.thread_states);
        }
        if self.monitors.rows.len() > RETAIN_ROWS / 4 {
            self.monitors.drop_until(w.monitors);
        }
        // Dumps are large; once the daemon has them they are not kept.
        self.thread_dumps.drop_until(w.thread_dumps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_are_incremental() {
        let mut c = Collector::new(true);
        let t = c.new_thread(1, "main".into(), false, 0);
        let m = c.symbols.intern_method("app.Main", "run", "()V");
        let f = c.symbols.intern_frame(m, 3, FrameKind::Jit);
        let s = c.stacks.intern_root_first([f]);
        c.add_sample(1_500_000_000, SampleKind::Cpu, t, ThreadState::Runnable, s, 10, None);
        c.add_sample(1_600_000_000, SampleKind::Cpu, t, ThreadState::Runnable, s, 10, None);
        c.observe_state(t, ThreadState::Runnable, 0);
        c.set_telemetry_keys(vec![("heap.used".into(), "Heap".into(), 1)]);
        c.add_telemetry_row(5, &[1.0]);
        let b = c.cut_batch(&v1::Watermark::default(), false, true).unwrap();
        assert_eq!(b.samples.len(), 1);
        assert_eq!(b.samples[0].count, vec![2]);
        assert_eq!(b.samples[0].weight, vec![20]);
        assert_eq!(b.stacks.as_ref().unwrap().parent, vec![0]);
        assert_eq!(b.threads.as_ref().unwrap().threads.len(), 1);
        assert_eq!(b.telemetry.as_ref().unwrap().series[0].value, vec![1.0]);
        let to = b.to.unwrap();
        assert!(c.cut_batch(&to, false, true).is_none());
        c.add_sample(2_000_000_000, SampleKind::Cpu, t, ThreadState::Runnable, s, 10, None);
        let b2 = c.cut_batch(&to, false, true).unwrap();
        assert_eq!(b2.samples[0].time, vec![2_000_000_000]);
        assert!(b2.symbols.unwrap().strings.is_empty());
        c.ack(&b2.to.unwrap());
    }
}
