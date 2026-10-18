//! Arrow IPC encoding of the columnar sections (codec 3, format 1.1).
//!
//! Each table is one Arrow IPC *file* (`ARROW1` … footer), so a section's payload can be handed
//! to any Arrow reader as is. Buffers are zstd-compressed by Arrow itself. Times are absolute
//! nanoseconds. Enumerations (thread state, units, …), probe ids and strings (allocated classes,
//! probe payloads) are string view columns (`Utf8View`) with deduplicated bytes, which compress
//! about as well as dictionaries but keep the values readable without a lookup. Thread and stack
//! columns are ids into the Threads and Stacks sections; [`dimension_tables`] resolves them for
//! export. [`portable`] turns views into plain strings for readers without view support (DuckDB's
//! `read_arrow`).

use crate::SectionKind;
use anyhow::{Context, Result, bail};
use arrow_array::{
    Array, ArrayRef, ArrowPrimitiveType, BooleanArray, Float64Array, Int32Array, Int64Array, PrimitiveArray,
    RecordBatch, StringViewArray as StringArray, UInt32Array, UInt64Array, builder::StringViewBuilder,
};
use arrow_ipc::CompressionType;
use arrow_ipc::reader::FileReader;
use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
use arrow_schema::{DataType, Schema};
use openprofiler_model::*;
use std::collections::HashMap;
use std::sync::Arc;

/// Rows per record batch. Smaller batches let readers stream large sample tables.
const BATCH_ROWS: usize = 1 << 20;

#[cfg(test)]
pub(crate) fn tests_batch_rows() -> usize {
    BATCH_ROWS
}

/// Version written into every table's schema metadata.
const FORMAT: &str = "opsnap 1.1";

/// One table: the section it is stored in and its index entry name.
pub struct Table {
    pub kind: SectionKind,
    pub name: String,
    pub batch: RecordBatch,
}

/// Encodes a table as an Arrow IPC file with zstd-compressed buffers.
pub fn encode(batch: &RecordBatch) -> Result<Vec<u8>> {
    let opts = IpcWriteOptions::default().try_with_compression(Some(CompressionType::ZSTD))?;
    let mut w = FileWriter::try_new_with_options(Vec::new(), &batch.schema(), opts)?;
    let mut off = 0;
    while off < batch.num_rows() {
        let len = BATCH_ROWS.min(batch.num_rows() - off);
        w.write(&batch.slice(off, len))?;
        off += len;
    }
    w.finish()?;
    Ok(w.into_inner()?)
}

/// Decodes an Arrow IPC file: its schema metadata and record batches.
pub fn decode(bytes: &[u8]) -> Result<(HashMap<String, String>, Vec<RecordBatch>)> {
    let r = FileReader::try_new(std::io::Cursor::new(bytes), None)?;
    let md = r.schema().metadata().clone();
    Ok((md, r.collect::<Result<Vec<_>, _>>()?))
}

fn batches(bytes: &[u8]) -> Result<Vec<RecordBatch>> {
    Ok(decode(bytes)?.1)
}

/// `batch` with every string view column converted to a plain UTF-8 column.
pub fn portable(batch: &RecordBatch) -> Result<RecordBatch> {
    let schema = batch.schema();
    let mut fields = vec![];
    let mut cols = vec![];
    for (f, c) in schema.fields().iter().zip(batch.columns()) {
        match c.as_any().downcast_ref::<StringArray>() {
            Some(v) => {
                fields.push(f.as_ref().clone().with_data_type(DataType::Utf8));
                cols.push(arr(arrow_array::StringArray::from_iter(v.iter())));
            }
            None => {
                fields.push(f.as_ref().clone());
                cols.push(c.clone());
            }
        }
    }
    let schema = Schema::new(fields).with_metadata(schema.metadata().clone());
    Ok(RecordBatch::try_new(Arc::new(schema), cols)?)
}

// ---- writing ----

fn batch(
    table: &str,
    extra: Vec<(String, String)>,
    cols: Vec<(&str, ArrayRef, bool)>,
) -> Result<RecordBatch> {
    let b = RecordBatch::try_from_iter_with_nullable(cols)?;
    let mut md: HashMap<String, String> = extra.into_iter().collect();
    md.insert("openprofiler.table".into(), table.into());
    md.insert("openprofiler.format".into(), FORMAT.into());
    let schema = Arc::new(b.schema().as_ref().clone().with_metadata(md));
    Ok(RecordBatch::try_new(schema, b.columns().to_vec())?)
}

fn arr<A: Array + 'static>(a: A) -> ArrayRef {
    Arc::new(a)
}

fn strs<'a>(v: impl Iterator<Item = &'a str>) -> ArrayRef {
    opt_strs(v.map(Some))
}

/// A string view column. Repeated strings share their bytes, so their views are identical and
/// compress to almost nothing (unlike the ever-growing offsets of a plain string column).
fn opt_strs<'a>(v: impl Iterator<Item = Option<&'a str>>) -> ArrayRef {
    let mut b = StringViewBuilder::new().with_deduplicate_strings();
    for s in v {
        b.append_option(s);
    }
    arr(b.finish())
}

fn unit_name(u: Unit) -> &'static str {
    match u {
        Unit::Nanos => "nanos",
        Unit::Bytes => "bytes",
        Unit::Count => "count",
        Unit::Percent => "percent",
        Unit::PerSecond => "per_second",
    }
}

fn unit_parse(s: &str) -> Unit {
    match s {
        "nanos" => Unit::Nanos,
        "bytes" => Unit::Bytes,
        "percent" => Unit::Percent,
        "per_second" => Unit::PerSecond,
        _ => Unit::Count,
    }
}

fn monitor_kind_name(k: MonitorEventKind) -> &'static str {
    match k {
        MonitorEventKind::Enter => "enter",
        MonitorEventKind::Wait => "wait",
        MonitorEventKind::Park => "park",
    }
}

fn control_kind_name(k: ControlEventKind) -> &'static str {
    match k {
        ControlEventKind::Open => "open",
        ControlEventKind::Close => "close",
    }
}

fn probe_ids(p: &Profile) -> Vec<&str> {
    p.probes.probes.iter().map(|i| i.id.as_str()).collect()
}

pub fn samples(p: &Profile, kind: SampleKind, s: &SampleSet) -> Result<RecordBatch> {
    let sym = &p.symbols;
    batch(
        "samples",
        vec![("openprofiler.sample_kind".into(), kind.name().into())],
        vec![
            ("time", arr(Int64Array::from(s.time.clone())), false),
            ("thread", arr(UInt32Array::from(s.thread.clone())), false),
            ("state", strs(s.state.iter().map(|x| x.name())), false),
            ("stack", arr(UInt32Array::from(s.stack.clone())), false),
            ("weight", arr(UInt64Array::from(s.weight.clone())), false),
            ("count", arr(UInt32Array::from(s.count.clone())), false),
            ("class", opt_strs(s.class.iter().map(|&c| (c != u32::MAX).then(|| sym.str(c)))), true),
        ],
    )
}

pub fn thread_states(p: &Profile) -> Result<RecordBatch> {
    let ts = &p.thread_states;
    batch(
        "thread_states",
        vec![],
        vec![
            ("thread", arr(UInt32Array::from(ts.thread.clone())), false),
            ("start", arr(Int64Array::from(ts.start.clone())), false),
            ("end", arr(Int64Array::from(ts.end.clone())), false),
            ("state", strs(ts.state.iter().map(|x| x.name())), false),
        ],
    )
}

/// Long format: one row per point. The schema metadata lists every series in order
/// (`openprofiler.series.<i>.key/label/unit`), also those without points.
pub fn telemetry(p: &Profile) -> Result<RecordBatch> {
    let tel = &p.telemetry;
    let mut md = vec![("openprofiler.series".to_string(), tel.len().to_string())];
    for (i, s) in tel.iter().enumerate() {
        md.push((format!("openprofiler.series.{i}.key"), s.key.clone()));
        md.push((format!("openprofiler.series.{i}.label"), s.label.clone()));
        md.push((format!("openprofiler.series.{i}.unit"), unit_name(s.unit).into()));
    }
    let rows = || tel.iter().flat_map(|s| s.time.iter().zip(&s.value).map(move |(t, v)| (s, *t, *v)));
    batch(
        "telemetry",
        md,
        vec![
            ("key", strs(rows().map(|(s, _, _)| s.key.as_str())), false),
            ("time", arr(Int64Array::from_iter_values(rows().map(|(_, t, _)| t))), false),
            ("value", arr(Float64Array::from_iter_values(rows().map(|(_, _, v)| v))), false),
        ],
    )
}

pub fn gc(p: &Profile) -> Result<RecordBatch> {
    let g = &p.gc;
    batch(
        "gc",
        vec![],
        vec![
            ("start", arr(Int64Array::from_iter_values(g.iter().map(|e| e.start))), false),
            ("duration", arr(Int64Array::from_iter_values(g.iter().map(|e| e.duration))), false),
            ("name", strs(g.iter().map(|e| e.name.as_str())), false),
            ("cause", strs(g.iter().map(|e| e.cause.as_str())), false),
            ("heap_before", arr(UInt64Array::from_iter(g.iter().map(|e| e.heap_before))), true),
            ("heap_after", arr(UInt64Array::from_iter(g.iter().map(|e| e.heap_after))), true),
            ("pause", arr(Int64Array::from_iter(g.iter().map(|e| e.pause))), true),
        ],
    )
}

pub fn monitors(p: &Profile) -> Result<RecordBatch> {
    let m = &p.monitors;
    batch(
        "monitors",
        vec![],
        vec![
            ("start", arr(Int64Array::from_iter_values(m.iter().map(|e| e.start))), false),
            ("duration", arr(Int64Array::from_iter_values(m.iter().map(|e| e.duration))), false),
            ("kind", strs(m.iter().map(|e| monitor_kind_name(e.kind))), false),
            ("thread", arr(UInt32Array::from_iter_values(m.iter().map(|e| e.thread))), false),
            ("owner", arr(UInt32Array::from_iter(m.iter().map(|e| e.owner))), true),
            ("monitor_class", strs(m.iter().map(|e| e.monitor_class.as_str())), false),
            ("monitor_id", arr(UInt64Array::from_iter(m.iter().map(|e| e.monitor_id))), true),
            ("stack", arr(UInt32Array::from_iter_values(m.iter().map(|e| e.stack))), false),
        ],
    )
}

pub fn probe_calls(p: &Profile) -> Result<RecordBatch> {
    let (c, ids, sym) = (&p.probes.calls, probe_ids(p), &p.symbols);
    batch(
        "probe_calls",
        vec![],
        vec![
            ("time", arr(Int64Array::from_iter_values(c.iter().map(|c| c.time))), false),
            ("probe", strs(c.iter().map(|c| ids.get(c.probe as usize).copied().unwrap_or(""))), false),
            ("payload", strs(c.iter().map(|c| sym.str(c.payload))), false),
            ("thread", arr(UInt32Array::from_iter_values(c.iter().map(|c| c.thread))), false),
            ("stack", arr(UInt32Array::from_iter_values(c.iter().map(|c| c.stack))), false),
            ("count", arr(UInt32Array::from_iter_values(c.iter().map(|c| c.count))), false),
            ("duration", arr(Int64Array::from_iter_values(c.iter().map(|c| c.duration))), false),
            ("max", arr(Int64Array::from_iter_values(c.iter().map(|c| c.max))), false),
            ("errors", arr(UInt32Array::from_iter_values(c.iter().map(|c| c.errors))), false),
        ],
    )
}

pub fn probe_events(p: &Profile) -> Result<RecordBatch> {
    let (e, ids, sym) = (&p.probes.events, probe_ids(p), &p.symbols);
    batch(
        "probe_events",
        vec![],
        vec![
            ("start", arr(Int64Array::from_iter_values(e.iter().map(|e| e.start))), false),
            ("duration", arr(Int64Array::from_iter_values(e.iter().map(|e| e.duration))), false),
            ("probe", strs(e.iter().map(|e| ids.get(e.probe as usize).copied().unwrap_or(""))), false),
            ("payload", strs(e.iter().map(|e| sym.str(e.payload))), false),
            ("thread", arr(UInt32Array::from_iter_values(e.iter().map(|e| e.thread))), false),
            ("stack", arr(UInt32Array::from_iter_values(e.iter().map(|e| e.stack))), false),
            ("error", opt_strs(e.iter().map(|e| e.error.as_deref())), true),
            ("control", arr(UInt64Array::from_iter(e.iter().map(|e| e.control))), true),
            ("detail", opt_strs(e.iter().map(|e| e.detail.as_deref())), true),
            ("context", opt_strs(e.iter().map(|e| e.context.as_deref())), true),
        ],
    )
}

pub fn probe_controls(p: &Profile) -> Result<RecordBatch> {
    let (c, ids) = (&p.probes.controls, probe_ids(p));
    batch(
        "probe_controls",
        vec![],
        vec![
            ("time", arr(Int64Array::from_iter_values(c.iter().map(|c| c.time))), false),
            ("probe", strs(c.iter().map(|c| ids.get(c.probe as usize).copied().unwrap_or(""))), false),
            ("id", arr(UInt64Array::from_iter_values(c.iter().map(|c| c.id))), false),
            ("kind", strs(c.iter().map(|c| control_kind_name(c.kind))), false),
            ("description", strs(c.iter().map(|c| c.description.as_str())), false),
            ("thread", arr(UInt32Array::from_iter_values(c.iter().map(|c| c.thread))), false),
            ("stack", arr(UInt32Array::from_iter_values(c.iter().map(|c| c.stack))), false),
        ],
    )
}

/// Every table that a 1.1 snapshot of `p` stores as Arrow, in file order.
pub fn section_tables(p: &Profile) -> Result<Vec<Table>> {
    let mut out = vec![];
    let mut t = |kind, name: &str, batch| out.push(Table { kind, name: name.to_owned(), batch });
    for (kind, s) in &p.samples {
        if *kind != SampleKind::Probe {
            t(SectionKind::Samples, kind.name(), samples(p, *kind, s)?);
        }
    }
    if !p.thread_states.is_empty() {
        t(SectionKind::ThreadStates, "thread_states", thread_states(p)?);
    }
    t(SectionKind::Telemetry, "telemetry", telemetry(p)?);
    t(SectionKind::Gc, "gc", gc(p)?);
    t(SectionKind::Monitors, "monitors", monitors(p)?);
    if !p.probes.probes.is_empty() {
        t(SectionKind::Probes, "probe_calls", probe_calls(p)?);
        t(SectionKind::Probes, "probe_events", probe_events(p)?);
        t(SectionKind::Probes, "probe_controls", probe_controls(p)?);
    }
    Ok(out)
}

/// Tables that resolve the ids used by [`section_tables`]: `threads`, `stacks`, `frames` and
/// `probes`. They are not stored as Arrow in the snapshot (the Symbols, Stacks, Threads and
/// Probes sections stay protobuf) but written next to exported sections.
pub fn dimension_tables(p: &Profile) -> Result<Vec<(String, RecordBatch)>> {
    let th = &p.threads;
    let threads = batch(
        "threads",
        vec![],
        vec![
            ("thread", arr(UInt32Array::from_iter_values(0..th.len() as u32)), false),
            ("java_id", arr(UInt64Array::from_iter_values(th.iter().map(|t| t.java_id))), false),
            ("os_id", arr(UInt64Array::from_iter(th.iter().map(|t| t.os_id))), true),
            ("name", strs(th.iter().map(|t| t.name.as_str())), false),
            ("group", opt_strs(th.iter().map(|t| t.group.as_deref())), true),
            ("is_virtual", arr(BooleanArray::from_iter(th.iter().map(|t| Some(t.is_virtual)))), false),
            ("is_daemon", arr(BooleanArray::from_iter(th.iter().map(|t| Some(t.is_daemon)))), false),
            ("start", arr(Int64Array::from_iter(th.iter().map(|t| t.start))), true),
            ("end", arr(Int64Array::from_iter(th.iter().map(|t| t.end))), true),
        ],
    )?;
    // Stack 0 is the empty stack; a stack's leaf frame is `frame`, the rest of it is `parent`.
    let (parent, frame) = p.stacks.raw();
    let stacks = batch(
        "stacks",
        vec![],
        vec![
            ("stack", arr(UInt32Array::from_iter_values(1..parent.len() as u32)), false),
            ("parent", arr(UInt32Array::from(parent[1..].to_vec())), false),
            ("frame", arr(UInt32Array::from(frame[1..].to_vec())), false),
        ],
    )?;
    let sym = &p.symbols;
    let fr = sym.frames();
    let method = |f: &Frame| sym.methods()[f.method as usize];
    let kinds = ["interpreted", "jit", "inlined", "native", "kernel", "cpp", "unknown"];
    let frames = batch(
        "frames",
        vec![],
        vec![
            ("frame", arr(UInt32Array::from_iter_values(0..fr.len() as u32)), false),
            ("class", strs(fr.iter().map(|f| sym.str(method(f).class))), false),
            ("method", strs(fr.iter().map(|f| sym.str(method(f).name))), false),
            ("signature", strs(fr.iter().map(|f| sym.str(method(f).signature))), false),
            ("line", arr(Int32Array::from_iter_values(fr.iter().map(|f| f.line))), false),
            ("kind", strs(fr.iter().map(|f| kinds[(f.kind as usize).min(kinds.len() - 1)])), false),
        ],
    )?;
    let pr = &p.probes.probes;
    let probes = batch(
        "probes",
        vec![],
        vec![
            ("probe", strs(pr.iter().map(|i| i.id.as_str())), false),
            ("name", strs(pr.iter().map(|i| i.name.as_str())), false),
            ("payload_label", strs(pr.iter().map(|i| i.payload_label.as_str())), false),
            ("control_label", opt_strs(pr.iter().map(|i| i.control_label.as_deref())), true),
        ],
    )?;
    Ok(vec![
        ("threads".into(), threads),
        ("stacks".into(), stacks),
        ("frames".into(), frames),
        ("probes".into(), probes),
    ])
}

// ---- reading ----

fn col<'a, T: 'static>(b: &'a RecordBatch, name: &str) -> Result<&'a T> {
    b.column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<T>()
        .with_context(|| format!("column {name} has an unexpected type"))
}

fn opt<T: ArrowPrimitiveType>(a: &PrimitiveArray<T>, i: usize) -> Option<T::Native> {
    a.is_valid(i).then(|| a.value(i))
}

fn opt_str(a: &StringArray, i: usize) -> Option<String> {
    a.is_valid(i).then(|| a.value(i).to_owned())
}

fn state(s: &str) -> ThreadState {
    ThreadState::parse(s).unwrap_or(ThreadState::Unknown)
}

/// Maps probe ids back to indices into the Probes section.
struct ProbeIndex(HashMap<String, u32>);

impl ProbeIndex {
    fn new(probes: &[ProbeInfo]) -> Self {
        Self(probes.iter().enumerate().map(|(i, p)| (p.id.clone(), i as u32)).collect())
    }

    fn get(&self, id: &str) -> Result<u32> {
        match self.0.get(id) {
            Some(&i) => Ok(i),
            None => bail!("unknown probe {id}"),
        }
    }
}

pub fn read_samples(bytes: &[u8], sym: &mut Symbols) -> Result<SampleSet> {
    let mut s = SampleSet::default();
    for b in batches(bytes)? {
        let (time, thread, stack) = (
            col::<Int64Array>(&b, "time")?,
            col::<UInt32Array>(&b, "thread")?,
            col::<UInt32Array>(&b, "stack")?,
        );
        let (weight, count) = (col::<UInt64Array>(&b, "weight")?, col::<UInt32Array>(&b, "count")?);
        let (st, class) = (col::<StringArray>(&b, "state")?, col::<StringArray>(&b, "class")?);
        for i in 0..b.num_rows() {
            s.push(
                time.value(i),
                thread.value(i),
                state(st.value(i)),
                stack.value(i),
                weight.value(i),
                count.value(i),
                class.is_valid(i).then(|| sym.intern_str(class.value(i))),
            );
        }
    }
    Ok(s)
}

pub fn read_thread_states(bytes: &[u8], out: &mut ThreadStateSpans) -> Result<()> {
    for b in batches(bytes)? {
        let (thread, start, end) = (
            col::<UInt32Array>(&b, "thread")?,
            col::<Int64Array>(&b, "start")?,
            col::<Int64Array>(&b, "end")?,
        );
        let st = col::<StringArray>(&b, "state")?;
        for i in 0..b.num_rows() {
            out.push(thread.value(i), start.value(i), end.value(i), state(st.value(i)));
        }
    }
    Ok(())
}

pub fn read_telemetry(bytes: &[u8]) -> Result<Vec<TelemetrySeries>> {
    let (md, batches) = decode(bytes)?;
    let n: usize = md.get("openprofiler.series").context("telemetry without series list")?.parse()?;
    let get =
        |i: usize, f: &str| md.get(&format!("openprofiler.series.{i}.{f}")).map(String::as_str).unwrap_or("");
    let mut out: Vec<TelemetrySeries> = (0..n)
        .map(|i| TelemetrySeries::new(get(i, "key"), get(i, "label"), unit_parse(get(i, "unit"))))
        .collect();
    let index: HashMap<String, usize> = out.iter().enumerate().map(|(i, s)| (s.key.clone(), i)).collect();
    for b in batches {
        let (key, time, value) = (
            col::<StringArray>(&b, "key")?,
            col::<Int64Array>(&b, "time")?,
            col::<Float64Array>(&b, "value")?,
        );
        let mut last: Option<(&str, usize)> = None;
        for i in 0..b.num_rows() {
            let k = key.value(i);
            let s = match last {
                Some((lk, s)) if lk == k => s,
                _ => *index.get(k).with_context(|| format!("unknown telemetry series {k}"))?,
            };
            last = Some((k, s));
            out[s].push(time.value(i), value.value(i));
        }
    }
    Ok(out)
}

pub fn read_gc(bytes: &[u8]) -> Result<Vec<GcEvent>> {
    let mut out = vec![];
    for b in batches(bytes)? {
        let (start, duration) = (col::<Int64Array>(&b, "start")?, col::<Int64Array>(&b, "duration")?);
        let (name, cause) = (col::<StringArray>(&b, "name")?, col::<StringArray>(&b, "cause")?);
        let (before, after) = (col::<UInt64Array>(&b, "heap_before")?, col::<UInt64Array>(&b, "heap_after")?);
        let pause = col::<Int64Array>(&b, "pause")?;
        for i in 0..b.num_rows() {
            out.push(GcEvent {
                start: start.value(i),
                duration: duration.value(i),
                name: name.value(i).to_owned(),
                cause: cause.value(i).to_owned(),
                heap_before: opt(before, i),
                heap_after: opt(after, i),
                pause: opt(pause, i),
            });
        }
    }
    Ok(out)
}

pub fn read_monitors(bytes: &[u8]) -> Result<Vec<MonitorEvent>> {
    let mut out = vec![];
    for b in batches(bytes)? {
        let (start, duration) = (col::<Int64Array>(&b, "start")?, col::<Int64Array>(&b, "duration")?);
        let (kind, class) = (col::<StringArray>(&b, "kind")?, col::<StringArray>(&b, "monitor_class")?);
        let (thread, owner) = (col::<UInt32Array>(&b, "thread")?, col::<UInt32Array>(&b, "owner")?);
        let (id, stack) = (col::<UInt64Array>(&b, "monitor_id")?, col::<UInt32Array>(&b, "stack")?);
        for i in 0..b.num_rows() {
            out.push(MonitorEvent {
                start: start.value(i),
                duration: duration.value(i),
                kind: match kind.value(i) {
                    "wait" => MonitorEventKind::Wait,
                    "park" => MonitorEventKind::Park,
                    _ => MonitorEventKind::Enter,
                },
                thread: thread.value(i),
                owner: opt(owner, i),
                monitor_class: class.value(i).to_owned(),
                monitor_id: opt(id, i),
                stack: stack.value(i),
            });
        }
    }
    Ok(out)
}

pub fn read_probe_calls(bytes: &[u8], probes: &[ProbeInfo], sym: &mut Symbols) -> Result<Vec<ProbeCall>> {
    let index = ProbeIndex::new(probes);
    let mut out = vec![];
    for b in batches(bytes)? {
        let time = col::<Int64Array>(&b, "time")?;
        let (probe, payload) = (col::<StringArray>(&b, "probe")?, col::<StringArray>(&b, "payload")?);
        let (thread, stack, count) = (
            col::<UInt32Array>(&b, "thread")?,
            col::<UInt32Array>(&b, "stack")?,
            col::<UInt32Array>(&b, "count")?,
        );
        let (duration, max, errors) = (
            col::<Int64Array>(&b, "duration")?,
            col::<Int64Array>(&b, "max")?,
            col::<UInt32Array>(&b, "errors")?,
        );
        for i in 0..b.num_rows() {
            out.push(ProbeCall {
                time: time.value(i),
                probe: index.get(probe.value(i))?,
                payload: sym.intern_str(payload.value(i)),
                thread: thread.value(i),
                stack: stack.value(i),
                count: count.value(i),
                duration: duration.value(i),
                max: max.value(i),
                errors: errors.value(i),
            });
        }
    }
    Ok(out)
}

pub fn read_probe_events(bytes: &[u8], probes: &[ProbeInfo], sym: &mut Symbols) -> Result<Vec<ProbeEvent>> {
    let index = ProbeIndex::new(probes);
    let mut out = vec![];
    for b in batches(bytes)? {
        let (start, duration) = (col::<Int64Array>(&b, "start")?, col::<Int64Array>(&b, "duration")?);
        let (probe, payload) = (col::<StringArray>(&b, "probe")?, col::<StringArray>(&b, "payload")?);
        let (thread, stack) = (col::<UInt32Array>(&b, "thread")?, col::<UInt32Array>(&b, "stack")?);
        let (error, detail, context) = (
            col::<StringArray>(&b, "error")?,
            col::<StringArray>(&b, "detail")?,
            col::<StringArray>(&b, "context")?,
        );
        let control = col::<UInt64Array>(&b, "control")?;
        for i in 0..b.num_rows() {
            out.push(ProbeEvent {
                start: start.value(i),
                duration: duration.value(i),
                probe: index.get(probe.value(i))?,
                payload: sym.intern_str(payload.value(i)),
                thread: thread.value(i),
                stack: stack.value(i),
                error: opt_str(error, i),
                control: opt(control, i),
                detail: opt_str(detail, i),
                context: opt_str(context, i),
            });
        }
    }
    Ok(out)
}

pub fn read_probe_controls(bytes: &[u8], probes: &[ProbeInfo]) -> Result<Vec<ControlEvent>> {
    let index = ProbeIndex::new(probes);
    let mut out = vec![];
    for b in batches(bytes)? {
        let time = col::<Int64Array>(&b, "time")?;
        let (probe, kind) = (col::<StringArray>(&b, "probe")?, col::<StringArray>(&b, "kind")?);
        let (id, description) = (col::<UInt64Array>(&b, "id")?, col::<StringArray>(&b, "description")?);
        let (thread, stack) = (col::<UInt32Array>(&b, "thread")?, col::<UInt32Array>(&b, "stack")?);
        for i in 0..b.num_rows() {
            out.push(ControlEvent {
                time: time.value(i),
                probe: index.get(probe.value(i))?,
                id: id.value(i),
                kind: if kind.value(i) == "close" { ControlEventKind::Close } else { ControlEventKind::Open },
                description: description.value(i).to_owned(),
                thread: thread.value(i),
                stack: stack.value(i),
            });
        }
    }
    Ok(out)
}
