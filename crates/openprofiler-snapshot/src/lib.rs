//! The `.opsnap` snapshot container (docs/spec/snapshot-format.md).
//!
//! ```text
//! [Header]  "OPRFSNAP" | u16 major | u16 minor | u32 flags | i64 created_ns | u32 len | JvmInfo(protobuf)
//! [Section]* (zero padding) | u32 kind | u64 len | u8 codec | payload
//! [Index]   protobuf Index (section table with offsets and xxh3 checksums)
//! [Trailer] u64 index_offset | u64 index_len | "OPRFSEND"
//! ```
//!
//! All integers are little-endian. Every section is compressed independently, so readers can
//! load only the sections they need. The heap dump section uses independent zstd frames so it
//! can be extracted (or randomly accessed) without decompressing everything. Since 1.1 the
//! columnar event and telemetry tables are Arrow IPC files (codec 3, see [`arrow`]) that other
//! tools can read in place.

use anyhow::{Context, Result, bail, ensure};
use openprofiler_model::*;
use prost::Message;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

pub mod arrow;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/openprofiler.snapshot.v1.rs"));
}

pub const MAGIC: &[u8; 8] = b"OPRFSNAP";
pub const END_MAGIC: &[u8; 8] = b"OPRFSEND";
pub const MAJOR: u16 = 1;
pub const MINOR: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SectionKind {
    Meta = 1,
    Symbols = 2,
    Stacks = 3,
    Samples = 4,
    Threads = 5,
    ThreadStates = 6,
    Telemetry = 7,
    Gc = 8,
    Monitors = 9,
    Bookmarks = 10,
    ThreadDumps = 11,
    HeapDump = 12,
    Config = 13,
    Extension = 14,
    Probes = 15,
    /// Since 1.1.
    Calls = 16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    None = 0,
    Zstd = 1,
    /// Independent zstd frames: `u32 raw_frame_size | u32 n | n × u64 compressed_len | frames`.
    ZstdFrames = 2,
    /// An Arrow IPC file (with its own buffer compression), starting 64-byte aligned.
    Arrow = 3,
}

/// Alignment of Arrow section payloads in the file, so they can be memory-mapped zero-copy.
const ARROW_ALIGN: u64 = 64;

const HEAP_FRAME: usize = 8 << 20;

/// Streaming writer. Sections are appended in call order; [`SnapshotWriter::finish`] writes the
/// index and trailer.
pub struct SnapshotWriter {
    out: BufWriter<File>,
    pos: u64,
    index: pb::Index,
    level: i32,
}

impl SnapshotWriter {
    pub fn create(path: &Path, jvm: &JvmInfo) -> Result<Self> {
        Self::create_version(path, jvm, MINOR)
    }

    fn create_version(path: &Path, jvm: &JvmInfo, minor: u16) -> Result<Self> {
        let f = File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        let mut w =
            Self { out: BufWriter::with_capacity(1 << 20, f), pos: 0, index: pb::Index::default(), level: 3 };
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let jvm = jvm_to_pb(jvm).encode_to_vec();
        let mut header = Vec::with_capacity(32 + jvm.len());
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&MAJOR.to_le_bytes());
        header.extend_from_slice(&minor.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&created.to_le_bytes());
        header.extend_from_slice(&(jvm.len() as u32).to_le_bytes());
        header.extend_from_slice(&jvm);
        w.write_raw(&header)?;
        Ok(w)
    }

    fn write_raw(&mut self, b: &[u8]) -> Result<()> {
        self.out.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    /// Writes a zstd-compressed section.
    pub fn section(&mut self, kind: SectionKind, name: &str, payload: &[u8]) -> Result<()> {
        let stored = zstd::bulk::compress(payload, self.level)?;
        self.section_raw(kind, name, Codec::Zstd, &stored, payload.len() as u64)
    }

    /// Writes a table as an Arrow IPC section (codec 3).
    pub fn arrow(&mut self, kind: SectionKind, name: &str, batch: &arrow_array::RecordBatch) -> Result<()> {
        let stored = arrow::encode(batch)?;
        // Readers find sections through the index, so padding between sections is allowed.
        let pad = (ARROW_ALIGN - (self.pos + 13) % ARROW_ALIGN) % ARROW_ALIGN;
        self.write_raw(&[0u8; ARROW_ALIGN as usize][..pad as usize])?;
        self.section_raw(kind, name, Codec::Arrow, &stored, stored.len() as u64)
    }

    fn section_raw(
        &mut self,
        kind: SectionKind,
        name: &str,
        codec: Codec,
        stored: &[u8],
        raw_len: u64,
    ) -> Result<()> {
        let offset = self.pos;
        let mut h = [0u8; 13];
        h[..4].copy_from_slice(&(kind as u32).to_le_bytes());
        h[4..12].copy_from_slice(&(stored.len() as u64).to_le_bytes());
        h[12] = codec as u8;
        self.write_raw(&h)?;
        self.write_raw(stored)?;
        self.index.sections.push(pb::SectionEntry {
            kind: kind as u32,
            offset,
            length: stored.len() as u64,
            codec: codec as u32,
            raw_length: raw_len,
            xxh3: xxhash_rust::xxh3::xxh3_64(stored),
            name: name.to_owned(),
        });
        Ok(())
    }

    pub fn message<M: Message>(&mut self, kind: SectionKind, name: &str, m: &M) -> Result<()> {
        self.section(kind, name, &m.encode_to_vec())
    }

    /// Embeds an HPROF heap dump as independent zstd frames, compressed in parallel.
    pub fn heap_dump(&mut self, hprof: &Path) -> Result<()> {
        let mut input = BufReader::with_capacity(HEAP_FRAME, File::open(hprof)?);
        let raw_len = std::fs::metadata(hprof)?.len();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
        let mut frames: Vec<Vec<u8>> = vec![];
        loop {
            let mut batch = vec![];
            for _ in 0..threads {
                let mut buf = vec![0u8; HEAP_FRAME];
                let mut n = 0;
                while n < buf.len() {
                    let r = input.read(&mut buf[n..])?;
                    if r == 0 {
                        break;
                    }
                    n += r;
                }
                if n == 0 {
                    break;
                }
                buf.truncate(n);
                batch.push(buf);
            }
            if batch.is_empty() {
                break;
            }
            let level = self.level;
            let done = batch.len() < threads || batch.last().is_some_and(|b| b.len() < HEAP_FRAME);
            let compressed: Vec<Result<Vec<u8>>> = std::thread::scope(|s| {
                let hs: Vec<_> =
                    batch.iter().map(|b| s.spawn(move || Ok(zstd::bulk::compress(b, level)?))).collect();
                hs.into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| bail!("compression thread panicked")))
                    .collect()
            });
            for c in compressed {
                frames.push(c?);
            }
            if done {
                break;
            }
        }
        let mut stored =
            Vec::with_capacity(8 + frames.len() * 8 + frames.iter().map(Vec::len).sum::<usize>());
        stored.extend_from_slice(&(HEAP_FRAME as u32).to_le_bytes());
        stored.extend_from_slice(&(frames.len() as u32).to_le_bytes());
        for f in &frames {
            stored.extend_from_slice(&(f.len() as u64).to_le_bytes());
        }
        for f in &frames {
            stored.extend_from_slice(f);
        }
        self.section_raw(SectionKind::HeapDump, "hprof", Codec::ZstdFrames, &stored, raw_len)
    }

    pub fn finish(mut self) -> Result<()> {
        let index_offset = self.pos;
        let idx = self.index.encode_to_vec();
        self.write_raw(&idx)?;
        let mut t = [0u8; 24];
        t[..8].copy_from_slice(&index_offset.to_le_bytes());
        t[8..16].copy_from_slice(&(idx.len() as u64).to_le_bytes());
        t[16..].copy_from_slice(END_MAGIC);
        self.write_raw(&t)?;
        self.out.flush()?;
        Ok(())
    }
}

/// Summary of a snapshot file.
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub major: u16,
    pub minor: u16,
    pub created: i64,
    pub jvm: JvmInfo,
    pub sections: Vec<pb::SectionEntry>,
}

impl SnapshotInfo {
    pub fn has_heap(&self) -> bool {
        self.sections.iter().any(|s| s.kind == SectionKind::HeapDump as u32)
    }
}

pub struct SnapshotReader {
    map: memmap2::Mmap,
    pub info: SnapshotInfo,
}

impl SnapshotReader {
    pub fn open(path: &Path) -> Result<Self> {
        let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        // SAFETY: the file is opened read-only; concurrent truncation by another process would
        // be a user error, as for every mmap-based reader.
        let map = unsafe { memmap2::Mmap::map(&f)? };
        ensure!(map.len() >= 32 + 24 && &map[..8] == MAGIC, "not an .opsnap snapshot");
        let major = u16::from_le_bytes([map[8], map[9]]);
        let minor = u16::from_le_bytes([map[10], map[11]]);
        ensure!(major == MAJOR, "unsupported .opsnap major version {major}");
        let created = i64::from_le_bytes(map[16..24].try_into()?);
        let jlen = u32::from_le_bytes(map[24..28].try_into()?) as usize;
        let jvm = pb::JvmInfo::decode(map.get(28..28 + jlen).context("truncated header")?)?;
        let t = &map[map.len() - 24..];
        ensure!(&t[16..] == END_MAGIC, "truncated snapshot (missing trailer)");
        let io = u64::from_le_bytes(t[..8].try_into()?) as usize;
        let il = u64::from_le_bytes(t[8..16].try_into()?) as usize;
        let idx = pb::Index::decode(map.get(io..io + il).context("bad index offset")?)?;
        Ok(Self {
            info: SnapshotInfo { major, minor, created, jvm: jvm_from_pb(jvm), sections: idx.sections },
            map,
        })
    }

    /// The stored bytes of a section, after checking its checksum. For an Arrow section (codec 3)
    /// this is a complete Arrow IPC file.
    pub fn stored(&self, e: &pb::SectionEntry) -> Result<&[u8]> {
        let start = e.offset as usize + 13;
        let b = self.map.get(start..start + e.length as usize).context("section out of bounds")?;
        ensure!(xxhash_rust::xxh3::xxh3_64(b) == e.xxh3, "checksum mismatch in section {}", e.kind);
        Ok(b)
    }

    pub fn payload(&self, e: &pb::SectionEntry) -> Result<Vec<u8>> {
        let b = self.stored(e)?;
        Ok(match e.codec {
            0 => b.to_vec(),
            1 => zstd::bulk::decompress(b, e.raw_length as usize)?,
            c => bail!("section {} has codec {c}; use stored or extract_heap", e.kind),
        })
    }

    /// The Arrow sections of `kind`.
    fn arrows(&self, kind: SectionKind) -> impl Iterator<Item = &pb::SectionEntry> {
        self.sections(kind).filter(|e| e.codec == Codec::Arrow as u32)
    }

    /// The Arrow section of `kind` named `name`.
    fn arrow(&self, kind: SectionKind, name: &str) -> Result<Option<&[u8]>> {
        self.arrows(kind).find(|e| e.name == name).map(|e| self.stored(e)).transpose()
    }

    fn sections(&self, kind: SectionKind) -> impl Iterator<Item = &pb::SectionEntry> {
        self.info.sections.iter().filter(move |s| s.kind == kind as u32)
    }

    /// The first protobuf section of `kind`.
    fn one<M: Message + Default>(&self, kind: SectionKind) -> Result<Option<M>> {
        match self.sections(kind).find(|e| e.codec != Codec::Arrow as u32) {
            Some(e) => Ok(Some(M::decode(&*self.payload(e)?)?)),
            None => Ok(None),
        }
    }

    pub fn read_profile(&self) -> Result<Profile> {
        let mut p = Profile::new("openprofiler");
        if let Some(m) = self.one::<pb::Meta>(SectionKind::Meta)? {
            p.meta = ProfileMeta {
                source: m.source,
                jvm: m.jvm.map(jvm_from_pb).unwrap_or_default(),
                start: m.start,
                end: m.end,
                cpu_interval: m.cpu_interval,
                properties: m.properties.into_iter().collect(),
            };
        }
        if let Some(s) = self.one::<pb::Symbols>(SectionKind::Symbols)? {
            let mut sym = Symbols::default();
            for (i, st) in s.strings.iter().enumerate() {
                let id = sym.intern_str(st);
                ensure!(id as usize == i, "duplicate string in symbol table");
            }
            for i in 0..s.method_class.len() {
                sym.intern_method_ids(Method {
                    class: s.method_class[i],
                    name: *s.method_name.get(i).context("short method table")?,
                    signature: *s.method_signature.get(i).context("short method table")?,
                });
            }
            for i in 0..s.frame_method.len() {
                sym.intern_frame(
                    s.frame_method[i],
                    *s.frame_line.get(i).context("short frame table")?,
                    FrameKind::from_u8(*s.frame_kind.get(i).context("short frame table")? as u8),
                );
            }
            p.symbols = sym;
        }
        if let Some(s) = self.one::<pb::Stacks>(SectionKind::Stacks)? {
            let mut parent = vec![0];
            let mut frame = vec![u32::MAX];
            parent.extend(s.parent);
            frame.extend(s.frame);
            p.stacks = StackTrie::from_raw(&parent, &frame);
        }
        for e in self.arrows(SectionKind::Samples) {
            let kind = SampleKind::parse(&e.name).context("unknown sample kind")?;
            p.samples.insert(kind, arrow::read_samples(self.stored(e)?, &mut p.symbols)?);
        }
        for e in self.sections(SectionKind::Samples).filter(|e| e.codec != Codec::Arrow as u32) {
            let s = pb::Samples::decode(&*self.payload(e)?)?;
            let kind = SampleKind::from_u8(s.kind as u8).context("unknown sample kind")?;
            let n = s.time_delta.len();
            let mut set = SampleSet::default();
            let mut t = 0i64;
            for i in 0..n {
                t += s.time_delta[i];
                set.push(
                    t,
                    s.thread[i],
                    ThreadState::from_u8(s.state[i] as u8),
                    s.stack[i],
                    s.weight[i],
                    s.count.get(i).copied().unwrap_or(1),
                    s.class.get(i).copied().filter(|&c| c != u32::MAX),
                );
            }
            p.samples.insert(kind, set);
        }
        if let Some(t) = self.one::<pb::Threads>(SectionKind::Threads)? {
            p.threads = t
                .threads
                .into_iter()
                .map(|t| ThreadInfo {
                    java_id: t.java_id,
                    os_id: t.os_id,
                    name: t.name,
                    group: t.group,
                    is_virtual: t.is_virtual,
                    is_daemon: t.is_daemon,
                    start: t.start,
                    end: t.end,
                })
                .collect();
        }
        if let Some(b) = self.arrow(SectionKind::ThreadStates, "thread_states")? {
            arrow::read_thread_states(b, &mut p.thread_states)?;
        }
        if let Some(s) = self.one::<pb::ThreadStates>(SectionKind::ThreadStates)? {
            let mut t = 0i64;
            for i in 0..s.thread.len() {
                t += s.start_delta[i];
                p.thread_states.push(
                    s.thread[i],
                    t,
                    t + s.duration[i],
                    ThreadState::from_u8(s.state[i] as u8),
                );
            }
        }
        if let Some(b) = self.arrow(SectionKind::Telemetry, "telemetry")? {
            p.telemetry = arrow::read_telemetry(b)?;
        }
        if let Some(tel) = self.one::<pb::Telemetry>(SectionKind::Telemetry)? {
            for s in tel.series {
                let mut ts = TelemetrySeries::new(&s.key, &s.label, unit_from(s.unit));
                let mut t = 0;
                for (d, v) in s.time_delta.iter().zip(&s.value) {
                    t += d;
                    ts.push(t, *v);
                }
                p.telemetry.push(ts);
            }
        }
        if let Some(b) = self.arrow(SectionKind::Gc, "gc")? {
            p.gc = arrow::read_gc(b)?;
        }
        if let Some(g) = self.one::<pb::Gc>(SectionKind::Gc)? {
            p.gc = g
                .events
                .into_iter()
                .map(|g| GcEvent {
                    start: g.start,
                    duration: g.duration,
                    name: g.name,
                    cause: g.cause,
                    heap_before: g.heap_before,
                    heap_after: g.heap_after,
                    pause: g.pause,
                })
                .collect();
        }
        if let Some(b) = self.arrow(SectionKind::Monitors, "monitors")? {
            p.monitors = arrow::read_monitors(b)?;
        }
        if let Some(m) = self.one::<pb::Monitors>(SectionKind::Monitors)? {
            p.monitors = m
                .events
                .into_iter()
                .map(|e| MonitorEvent {
                    start: e.start,
                    duration: e.duration,
                    kind: match e.kind {
                        1 => MonitorEventKind::Wait,
                        2 => MonitorEventKind::Park,
                        _ => MonitorEventKind::Enter,
                    },
                    thread: e.thread,
                    owner: e.owner,
                    monitor_class: e.monitor_class,
                    monitor_id: e.monitor_id,
                    stack: e.stack,
                })
                .collect();
        }
        if let Some(pr) = self.one::<pb::Probes>(SectionKind::Probes)? {
            p.probes.probes = pr
                .probes
                .into_iter()
                .map(|i| ProbeInfo {
                    id: i.id,
                    name: i.name,
                    payload_label: i.payload_label,
                    control_label: i.control_label,
                })
                .collect();
            if let Some(c) = pr.calls {
                let mut t = 0i64;
                for i in 0..c.probe.len() {
                    t += c.time_delta.get(i).copied().unwrap_or(0);
                    let at = |v: &Vec<u32>| v.get(i).copied().unwrap_or(0);
                    let at64 = |v: &Vec<i64>| v.get(i).copied().unwrap_or(0);
                    p.push_probe_call(ProbeCall {
                        time: t,
                        probe: c.probe[i],
                        payload: at(&c.payload),
                        thread: at(&c.thread),
                        stack: at(&c.stack),
                        count: at(&c.count),
                        duration: at64(&c.duration),
                        max: at64(&c.max),
                        errors: at(&c.errors),
                    });
                }
            }
            p.probes.events = pr
                .events
                .into_iter()
                .map(|e| ProbeEvent {
                    start: e.start,
                    duration: e.duration,
                    probe: e.probe,
                    payload: e.payload,
                    thread: e.thread,
                    stack: e.stack,
                    error: e.error,
                    control: e.control,
                    detail: e.detail,
                    context: e.context,
                })
                .collect();
            p.probes.controls = pr
                .controls
                .into_iter()
                .map(|c| ControlEvent {
                    time: c.time,
                    probe: c.probe,
                    id: c.id,
                    kind: if c.kind == 1 { ControlEventKind::Close } else { ControlEventKind::Open },
                    description: c.description,
                    thread: c.thread,
                    stack: c.stack,
                })
                .collect();
        }
        if let Some(b) = self.arrow(SectionKind::Probes, "probe_calls")? {
            for c in arrow::read_probe_calls(b, &p.probes.probes, &mut p.symbols)? {
                p.push_probe_call(c);
            }
        }
        if let Some(b) = self.arrow(SectionKind::Probes, "probe_events")? {
            p.probes.events = arrow::read_probe_events(b, &p.probes.probes, &mut p.symbols)?;
        }
        if let Some(b) = self.arrow(SectionKind::Probes, "probe_controls")? {
            p.probes.controls = arrow::read_probe_controls(b, &p.probes.probes)?;
        }
        if let Some(c) = self.one::<pb::Calls>(SectionKind::Calls)? {
            if let Some(t) = c.trace {
                let mut time = 0i64;
                for i in 0..t.thread.len() {
                    time += t.time_delta.get(i).copied().unwrap_or(0);
                    p.calls.trace.push(TraceEvent {
                        time,
                        thread: t.thread[i],
                        frame: t.frame.get(i).copied().unwrap_or(0),
                        exit: t.exit.get(i).copied().unwrap_or(false),
                    });
                }
            }
            if let Some(c) = c.sites {
                let mut time = 0i64;
                for i in 0..c.thread.len() {
                    time += c.time_delta.get(i).copied().unwrap_or(0);
                    p.calls.sites.push(CallSiteStat {
                        time,
                        thread: c.thread[i],
                        stack: c.stack.get(i).copied().unwrap_or(0),
                        count: c.count.get(i).copied().unwrap_or(0),
                        total: c.total.get(i).copied().unwrap_or(0),
                        max: c.max.get(i).copied().unwrap_or(0),
                    });
                }
            }
            if let Some(c) = c.complexity {
                let mut time = 0i64;
                for i in 0..c.thread.len() {
                    time += c.time_delta.get(i).copied().unwrap_or(0);
                    p.calls.complexity.push(ComplexityPoint {
                        time,
                        thread: c.thread[i],
                        frame: c.frame.get(i).copied().unwrap_or(0),
                        size: c.size.get(i).copied().unwrap_or(0),
                        duration: c.duration.get(i).copied().unwrap_or(0),
                    });
                }
            }
        }
        if let Some(b) = self.one::<pb::Bookmarks>(SectionKind::Bookmarks)? {
            p.bookmarks = b
                .bookmarks
                .into_iter()
                .map(|b| Bookmark { time: b.time, label: b.label, color: b.color })
                .collect();
        }
        if let Some(d) = self.one::<pb::ThreadDumps>(SectionKind::ThreadDumps)? {
            p.thread_dumps = d
                .dumps
                .into_iter()
                .map(|d| ThreadDump {
                    time: d.time,
                    label: d.label,
                    threads: d
                        .threads
                        .into_iter()
                        .map(|t| DumpedThread {
                            name: t.name,
                            java_id: t.java_id,
                            state: t.state,
                            is_virtual: t.is_virtual,
                            frames: t.frames,
                            locked: t.locked,
                            waiting_on: t.waiting_on,
                        })
                        .collect(),
                })
                .collect();
        }
        Ok(p)
    }

    /// Decompresses the embedded heap dump to `dest`. Returns `false` if there is none.
    pub fn extract_heap(&self, dest: &Path) -> Result<bool> {
        let Some(e) = self.sections(SectionKind::HeapDump).next() else { return Ok(false) };
        let b = self.stored(e)?;
        ensure!(e.codec == Codec::ZstdFrames as u32, "unexpected heap dump codec");
        let frame_raw = u32::from_le_bytes(b[..4].try_into()?) as usize;
        let n = u32::from_le_bytes(b[4..8].try_into()?) as usize;
        let mut lens = Vec::with_capacity(n);
        for i in 0..n {
            lens.push(u64::from_le_bytes(b[8 + i * 8..16 + i * 8].try_into()?) as usize);
        }
        let mut offsets = Vec::with_capacity(n);
        let mut off = 8 + n * 8;
        for l in &lens {
            offsets.push(off);
            off += l;
        }
        let tmp = dest.with_extension("partial");
        let out = File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        out.set_len(e.raw_length)?;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| -> Result<()> {
            let hs: Vec<_> = (0..threads)
                .map(|_| {
                    let out = out.try_clone();
                    let (next, lens, offsets) = (&next, &lens, &offsets);
                    s.spawn(move || -> Result<()> {
                        let out = out?;
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if i >= n {
                                return Ok(());
                            }
                            let data =
                                zstd::bulk::decompress(&b[offsets[i]..offsets[i] + lens[i]], frame_raw)?;
                            // Cloned handles share one file offset, so use positional writes.
                            write_all_at(&out, &data, (i * frame_raw) as u64)?;
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().map_err(|_| anyhow::anyhow!("decompression thread panicked"))??;
            }
            Ok(())
        })?;
        drop(out);
        std::fs::rename(&tmp, dest)?;
        Ok(true)
    }
}

fn unit_to(u: Unit) -> u32 {
    match u {
        Unit::Nanos => 0,
        Unit::Bytes => 1,
        Unit::Count => 2,
        Unit::Percent => 3,
        Unit::PerSecond => 4,
    }
}

fn unit_from(u: u32) -> Unit {
    match u {
        0 => Unit::Nanos,
        1 => Unit::Bytes,
        3 => Unit::Percent,
        4 => Unit::PerSecond,
        _ => Unit::Count,
    }
}

fn jvm_to_pb(j: &JvmInfo) -> pb::JvmInfo {
    pb::JvmInfo {
        pid: j.pid,
        name: j.name.clone(),
        vm_name: j.vm_name.clone(),
        vm_version: j.vm_version.clone(),
        vm_vendor: j.vm_vendor.clone(),
        java_version: j.java_version.clone(),
        jvm_args: j.jvm_args.clone(),
        main_class: j.main_class.clone(),
        os: j.os.clone(),
        arch: j.arch.clone(),
        cpus: j.cpus,
        max_heap: j.max_heap,
        start_time: j.start_time,
    }
}

fn jvm_from_pb(j: pb::JvmInfo) -> JvmInfo {
    JvmInfo {
        pid: j.pid,
        name: j.name,
        vm_name: j.vm_name,
        vm_version: j.vm_version,
        vm_vendor: j.vm_vendor,
        java_version: j.java_version,
        jvm_args: j.jvm_args,
        main_class: j.main_class,
        os: j.os,
        arch: j.arch,
        cpus: j.cpus,
        max_heap: j.max_heap,
        start_time: j.start_time,
    }
}

fn deltas(v: impl Iterator<Item = i64>) -> Vec<i64> {
    let mut prev = 0;
    v.map(|t| {
        let d = t - prev;
        prev = t;
        d
    })
    .collect()
}

/// Writes every part of `p` as sections (no heap dump).
pub fn write_profile_sections(w: &mut SnapshotWriter, p: &Profile) -> Result<()> {
    write_sections(w, p, true)
}

/// Writes the sections of `p`, the columnar tables as Arrow (1.1) or as protobuf (1.0).
fn write_sections(w: &mut SnapshotWriter, p: &Profile, use_arrow: bool) -> Result<()> {
    w.message(
        SectionKind::Meta,
        "meta",
        &pb::Meta {
            source: p.meta.source.clone(),
            jvm: Some(jvm_to_pb(&p.meta.jvm)),
            start: p.meta.start,
            end: p.meta.end,
            cpu_interval: p.meta.cpu_interval,
            properties: p.meta.properties.clone().into_iter().collect(),
            producer: format!("openprofiler {}", env!("CARGO_PKG_VERSION")),
        },
    )?;
    let sym = &p.symbols;
    w.message(
        SectionKind::Symbols,
        "symbols",
        &pb::Symbols {
            strings: sym.strings().iter().map(|s| s.to_string()).collect(),
            method_class: sym.methods().iter().map(|m| m.class).collect(),
            method_name: sym.methods().iter().map(|m| m.name).collect(),
            method_signature: sym.methods().iter().map(|m| m.signature).collect(),
            frame_method: sym.frames().iter().map(|f| f.method).collect(),
            frame_line: sym.frames().iter().map(|f| f.line).collect(),
            frame_kind: sym.frames().iter().map(|f| f.kind as u32).collect(),
        },
    )?;
    let (parent, frame) = p.stacks.raw();
    w.message(
        SectionKind::Stacks,
        "stacks",
        &pb::Stacks { parent: parent[1..].to_vec(), frame: frame[1..].to_vec() },
    )?;
    for (kind, s) in &p.samples {
        if *kind == SampleKind::Probe {
            continue;
        }
        if use_arrow {
            w.arrow(SectionKind::Samples, kind.name(), &arrow::samples(p, *kind, s)?)?;
            continue;
        }
        let all_one = s.count.iter().all(|&c| c == 1);
        let no_class = s.class.iter().all(|&c| c == u32::MAX);
        w.message(
            SectionKind::Samples,
            kind.name(),
            &pb::Samples {
                kind: *kind as u32,
                time_delta: deltas(s.time.iter().copied()),
                thread: s.thread.clone(),
                state: s.state.iter().map(|&x| x as u32).collect(),
                stack: s.stack.clone(),
                weight: s.weight.clone(),
                count: if all_one { vec![] } else { s.count.clone() },
                class: if no_class { vec![] } else { s.class.clone() },
            },
        )?;
    }
    w.message(
        SectionKind::Threads,
        "threads",
        &pb::Threads {
            threads: p
                .threads
                .iter()
                .map(|t| pb::Thread {
                    java_id: t.java_id,
                    os_id: t.os_id,
                    name: t.name.clone(),
                    group: t.group.clone(),
                    is_virtual: t.is_virtual,
                    is_daemon: t.is_daemon,
                    start: t.start,
                    end: t.end,
                })
                .collect(),
        },
    )?;
    let ts = &p.thread_states;
    if use_arrow {
        if !ts.is_empty() {
            w.arrow(SectionKind::ThreadStates, "thread_states", &arrow::thread_states(p)?)?;
        }
        w.arrow(SectionKind::Telemetry, "telemetry", &arrow::telemetry(p)?)?;
        w.arrow(SectionKind::Gc, "gc", &arrow::gc(p)?)?;
        w.arrow(SectionKind::Monitors, "monitors", &arrow::monitors(p)?)?;
    } else {
        write_protobuf_events(w, p)?;
    }
    if !p.probes.probes.is_empty() {
        let pr = &p.probes;
        let probes = pr
            .probes
            .iter()
            .map(|i| pb::ProbeInfo {
                id: i.id.clone(),
                name: i.name.clone(),
                payload_label: i.payload_label.clone(),
                control_label: i.control_label.clone(),
            })
            .collect();
        if use_arrow {
            w.message(SectionKind::Probes, "probes", &pb::Probes { probes, ..Default::default() })?;
            w.arrow(SectionKind::Probes, "probe_calls", &arrow::probe_calls(p)?)?;
            w.arrow(SectionKind::Probes, "probe_events", &arrow::probe_events(p)?)?;
            w.arrow(SectionKind::Probes, "probe_controls", &arrow::probe_controls(p)?)?;
        } else {
            write_protobuf_probes(w, p, probes)?;
        }
    }
    if use_arrow && !p.calls.is_empty() {
        let c = &p.calls;
        w.message(
            SectionKind::Calls,
            "calls",
            &pb::Calls {
                trace: Some(pb::CallTrace {
                    time_delta: deltas(c.trace.iter().map(|e| e.time)),
                    thread: c.trace.iter().map(|e| e.thread).collect(),
                    frame: c.trace.iter().map(|e| e.frame).collect(),
                    exit: c.trace.iter().map(|e| e.exit).collect(),
                }),
                sites: Some(pb::CallSites {
                    time_delta: deltas(c.sites.iter().map(|e| e.time)),
                    thread: c.sites.iter().map(|e| e.thread).collect(),
                    stack: c.sites.iter().map(|e| e.stack).collect(),
                    count: c.sites.iter().map(|e| e.count).collect(),
                    total: c.sites.iter().map(|e| e.total).collect(),
                    max: c.sites.iter().map(|e| e.max).collect(),
                }),
                complexity: Some(pb::Complexity {
                    time_delta: deltas(c.complexity.iter().map(|e| e.time)),
                    thread: c.complexity.iter().map(|e| e.thread).collect(),
                    frame: c.complexity.iter().map(|e| e.frame).collect(),
                    size: c.complexity.iter().map(|e| e.size).collect(),
                    duration: c.complexity.iter().map(|e| e.duration).collect(),
                }),
            },
        )?;
    }
    w.message(
        SectionKind::Bookmarks,
        "bookmarks",
        &pb::Bookmarks {
            bookmarks: p
                .bookmarks
                .iter()
                .map(|b| pb::Bookmark { time: b.time, label: b.label.clone(), color: b.color.clone() })
                .collect(),
        },
    )?;
    w.message(
        SectionKind::ThreadDumps,
        "thread_dumps",
        &pb::ThreadDumps {
            dumps: p
                .thread_dumps
                .iter()
                .map(|d| pb::ThreadDump {
                    time: d.time,
                    label: d.label.clone(),
                    threads: d
                        .threads
                        .iter()
                        .map(|t| pb::DumpedThread {
                            name: t.name.clone(),
                            java_id: t.java_id,
                            state: t.state.clone(),
                            is_virtual: t.is_virtual,
                            frames: t.frames.clone(),
                            locked: t.locked.clone(),
                            waiting_on: t.waiting_on.clone(),
                        })
                        .collect(),
                })
                .collect(),
        },
    )?;
    Ok(())
}

/// Thread states, telemetry, GC and monitor events as 1.0 protobuf sections.
fn write_protobuf_events(w: &mut SnapshotWriter, p: &Profile) -> Result<()> {
    let ts = &p.thread_states;
    if !ts.is_empty() {
        w.message(
            SectionKind::ThreadStates,
            "thread_states",
            &pb::ThreadStates {
                thread: ts.thread.clone(),
                start_delta: deltas(ts.start.iter().copied()),
                duration: ts.start.iter().zip(&ts.end).map(|(a, b)| b - a).collect(),
                state: ts.state.iter().map(|&s| s as u32).collect(),
            },
        )?;
    }
    w.message(
        SectionKind::Telemetry,
        "telemetry",
        &pb::Telemetry {
            series: p
                .telemetry
                .iter()
                .map(|s| pb::Series {
                    key: s.key.clone(),
                    label: s.label.clone(),
                    unit: unit_to(s.unit),
                    time_delta: deltas(s.time.iter().copied()),
                    value: s.value.clone(),
                })
                .collect(),
        },
    )?;
    w.message(
        SectionKind::Gc,
        "gc",
        &pb::Gc {
            events: p
                .gc
                .iter()
                .map(|g| pb::GcEvent {
                    start: g.start,
                    duration: g.duration,
                    name: g.name.clone(),
                    cause: g.cause.clone(),
                    heap_before: g.heap_before,
                    heap_after: g.heap_after,
                    pause: g.pause,
                })
                .collect(),
        },
    )?;
    w.message(
        SectionKind::Monitors,
        "monitors",
        &pb::Monitors {
            events: p
                .monitors
                .iter()
                .map(|e| pb::MonitorEvent {
                    start: e.start,
                    duration: e.duration,
                    kind: match e.kind {
                        MonitorEventKind::Enter => 0,
                        MonitorEventKind::Wait => 1,
                        MonitorEventKind::Park => 2,
                    },
                    thread: e.thread,
                    owner: e.owner,
                    monitor_class: e.monitor_class.clone(),
                    monitor_id: e.monitor_id,
                    stack: e.stack,
                })
                .collect(),
        },
    )?;
    Ok(())
}

/// Probe calls, events and control events as one 1.0 protobuf section.
fn write_protobuf_probes(w: &mut SnapshotWriter, p: &Profile, probes: Vec<pb::ProbeInfo>) -> Result<()> {
    let pr = &p.probes;
    w.message(
        SectionKind::Probes,
        "probes",
        &pb::Probes {
            probes,
            calls: Some(pb::ProbeCalls {
                time_delta: deltas(pr.calls.iter().map(|c| c.time)),
                probe: pr.calls.iter().map(|c| c.probe).collect(),
                payload: pr.calls.iter().map(|c| c.payload).collect(),
                thread: pr.calls.iter().map(|c| c.thread).collect(),
                stack: pr.calls.iter().map(|c| c.stack).collect(),
                count: pr.calls.iter().map(|c| c.count).collect(),
                duration: pr.calls.iter().map(|c| c.duration).collect(),
                max: pr.calls.iter().map(|c| c.max).collect(),
                errors: pr.calls.iter().map(|c| c.errors).collect(),
            }),
            events: pr
                .events
                .iter()
                .map(|e| pb::ProbeEvent {
                    start: e.start,
                    duration: e.duration,
                    probe: e.probe,
                    payload: e.payload,
                    thread: e.thread,
                    stack: e.stack,
                    error: e.error.clone(),
                    control: e.control,
                    detail: e.detail.clone(),
                    context: e.context.clone(),
                })
                .collect(),
            controls: pr
                .controls
                .iter()
                .map(|c| pb::ControlEvent {
                    time: c.time,
                    probe: c.probe,
                    id: c.id,
                    kind: match c.kind {
                        ControlEventKind::Open => 0,
                        ControlEventKind::Close => 1,
                    },
                    description: c.description.clone(),
                    thread: c.thread,
                    stack: c.stack,
                })
                .collect(),
        },
    )
}

/// Writes a complete snapshot of `p`, optionally embedding an HPROF heap dump.
pub fn write_snapshot(path: &Path, p: &Profile, heap: Option<&Path>) -> Result<()> {
    let tmp = path.with_extension("openprofiler.partial");
    let mut w = SnapshotWriter::create(&tmp, &p.meta.jvm)?;
    write_profile_sections(&mut w, p)?;
    if let Some(h) = heap {
        w.heap_dump(h)?;
    }
    w.finish()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Writes a 1.0 snapshot (protobuf only), readable by openprofiler 0.4 and older.
pub fn write_snapshot_v1_0(path: &Path, p: &Profile, heap: Option<&Path>) -> Result<()> {
    let tmp = path.with_extension("openprofiler.partial");
    let mut w = SnapshotWriter::create_version(&tmp, &p.meta.jvm, 0)?;
    write_sections(&mut w, p, false)?;
    if let Some(h) = heap {
        w.heap_dump(h)?;
    }
    w.finish()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read_snapshot(path: &Path) -> Result<(Profile, SnapshotInfo)> {
    let r = SnapshotReader::open(path)?;
    Ok((r.read_profile()?, r.info.clone()))
}

#[cfg(unix)]
fn write_all_at(f: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, offset)
}

#[cfg(windows)]
fn write_all_at(f: &std::fs::File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(f, buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> Profile {
        let mut p = Profile::new("test");
        p.meta.jvm.vm_name = "TestVM".into();
        p.meta.start = 1_000;
        p.meta.end = 9_000;
        let m = p.symbols.intern_method("a.B", "c", "()V");
        let f = p.symbols.intern_frame(m, 12, FrameKind::Jit);
        let m2 = p.symbols.intern_method("a.B", "d", "(I)V");
        let f2 = p.symbols.intern_frame(m2, -1, FrameKind::Native);
        let st = p.stacks.intern_root_first([f, f2]);
        let t = p.thread_index(7, || "worker".into());
        p.samples_mut(SampleKind::Cpu).push(1_500, t, ThreadState::Runnable, st, 10, 1, None);
        p.samples_mut(SampleKind::Cpu).push(2_500, t, ThreadState::Runnable, st, 10, 3, None);
        let cls = p.symbols.intern_str("byte[]");
        p.samples_mut(SampleKind::Alloc).push(3_000, t, ThreadState::Runnable, st, 4096, 1, Some(cls));
        p.thread_states.push(t, 1_000, 5_000, ThreadState::Runnable);
        p.telemetry_mut("heap.used", "Heap", Unit::Bytes).push(1_000, 42.0);
        p.gc.push(GcEvent { start: 2_000, duration: 5, name: "G1".into(), ..Default::default() });
        p.monitors.push(MonitorEvent {
            start: 3,
            duration: 4,
            kind: MonitorEventKind::Park,
            thread: t,
            owner: None,
            monitor_class: "X".into(),
            monitor_id: Some(1),
            stack: st,
        });
        p.bookmarks.push(Bookmark { time: 4, label: "b".into(), color: None });
        let sql = p.symbols.intern_str("SELECT 1");
        p.probes.probes.push(ProbeInfo {
            id: "jdbc".into(),
            name: "JDBC".into(),
            payload_label: "SQL statement".into(),
            control_label: Some("Connection".into()),
        });
        p.push_probe_call(ProbeCall {
            time: 1_000_000_000,
            probe: 0,
            payload: sql,
            thread: t,
            stack: st,
            count: 3,
            duration: 300,
            max: 200,
            errors: 1,
        });
        p.probes.events.push(ProbeEvent {
            start: 7,
            duration: 200,
            probe: 0,
            payload: sql,
            thread: t,
            stack: st,
            error: Some("java.sql.SQLException: x".into()),
            control: Some(4),
            detail: None,
            context: Some("GET /a".into()),
        });
        p.probes.controls.push(ControlEvent {
            time: 6,
            probe: 0,
            id: 4,
            kind: ControlEventKind::Open,
            description: "jdbc:h2:mem".into(),
            thread: t,
            stack: st,
        });
        p.thread_dumps.push(ThreadDump {
            time: 5,
            label: "d".into(),
            threads: vec![DumpedThread::default()],
        });
        p.calls.trace.push(TraceEvent { time: 10, thread: t, frame: 1, exit: false });
        p.calls.trace.push(TraceEvent { time: 12, thread: t, frame: 1, exit: true });
        p.calls.sites.push(CallSiteStat { time: 20, thread: t, stack: st, count: 4, total: 400, max: 250 });
        p.calls.complexity.push(ComplexityPoint { time: 30, thread: t, frame: 1, size: 1000, duration: 77 });
        p
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.opsnap");
        let heap = dir.path().join("h.hprof");
        let heap_bytes: Vec<u8> = (0..(HEAP_FRAME * 6 + 1234)).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&heap, &heap_bytes).unwrap();
        let p = sample_profile();
        write_snapshot(&path, &p, Some(&heap)).unwrap();
        let (q, info) = read_snapshot(&path).unwrap();
        assert!(info.has_heap());
        assert_eq!(info.jvm.vm_name, "TestVM");
        assert_eq!(q.meta, p.meta);
        assert_eq!(q.symbols.method_label(1), p.symbols.method_label(1));
        assert_eq!(q.stacks.len(), p.stacks.len());
        let (a, b) = (&p.samples[&SampleKind::Cpu], &q.samples[&SampleKind::Cpu]);
        assert_eq!(a.time, b.time);
        assert_eq!(a.count, b.count);
        assert_eq!(q.samples[&SampleKind::Alloc].class, p.samples[&SampleKind::Alloc].class);
        assert_eq!(q.threads, p.threads);
        assert_eq!(q.thread_states.end, p.thread_states.end);
        assert_eq!(q.telemetry, p.telemetry);
        assert_eq!(q.gc, p.gc);
        assert_eq!(q.monitors, p.monitors);
        assert_eq!(q.bookmarks, p.bookmarks);
        assert_eq!(q.thread_dumps, p.thread_dumps);
        assert_eq!(q.probes.probes, p.probes.probes);
        assert_eq!(q.probes.calls, p.probes.calls);
        assert_eq!(q.probes.events, p.probes.events);
        assert_eq!(q.probes.controls, p.probes.controls);
        assert_eq!(q.samples[&SampleKind::Probe].weight, vec![300]);
        assert_eq!(q.calls.trace, p.calls.trace);
        assert_eq!(q.calls.sites, p.calls.sites);
        assert_eq!(q.calls.complexity, p.calls.complexity);
        assert_eq!(info.minor, MINOR);
        let out = dir.path().join("out.hprof");
        assert!(SnapshotReader::open(&path).unwrap().extract_heap(&out).unwrap());
        assert_eq!(std::fs::read(&out).unwrap(), heap_bytes);
    }

    fn assert_same(q: &Profile, p: &Profile) {
        assert_eq!(q.meta, p.meta);
        assert_eq!(q.symbols.strings(), p.symbols.strings());
        assert_eq!(q.stacks.raw(), p.stacks.raw());
        for (k, a) in &p.samples {
            let b = &q.samples[k];
            assert_eq!(
                (&a.time, &a.thread, &a.state, &a.stack),
                (&b.time, &b.thread, &b.state, &b.stack),
                "{k:?}"
            );
            assert_eq!((&a.weight, &a.count, &a.class), (&b.weight, &b.count, &b.class), "{k:?}");
        }
        assert_eq!(q.samples.len(), p.samples.len());
        assert_eq!(q.threads, p.threads);
        assert_eq!(
            (&q.thread_states.thread, &q.thread_states.start, &q.thread_states.end, &q.thread_states.state),
            (&p.thread_states.thread, &p.thread_states.start, &p.thread_states.end, &p.thread_states.state)
        );
        assert_eq!(q.telemetry, p.telemetry);
        assert_eq!(q.gc, p.gc);
        assert_eq!(q.monitors, p.monitors);
        assert_eq!(q.bookmarks, p.bookmarks);
        assert_eq!(q.thread_dumps, p.thread_dumps);
        assert_eq!(q.probes.probes, p.probes.probes);
        assert_eq!(q.probes.calls, p.probes.calls);
        assert_eq!(q.probes.events, p.probes.events);
        assert_eq!(q.probes.controls, p.probes.controls);
    }

    /// `tests/fixtures/v1_0.opsnap` was written by the 1.0 writer (openprofiler 0.4).
    #[test]
    fn reads_1_0_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1_0.opsnap");
        let r = SnapshotReader::open(&path).unwrap();
        assert_eq!((r.info.major, r.info.minor), (1, 0));
        assert_same(&r.read_profile().unwrap(), &sample_profile());
    }

    #[test]
    fn writes_1_0() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.opsnap");
        write_snapshot_v1_0(&path, &sample_profile(), None).unwrap();
        let r = SnapshotReader::open(&path).unwrap();
        assert_eq!(r.info.minor, 0);
        assert!(r.info.sections.iter().all(|e| e.codec != Codec::Arrow as u32));
        assert_same(&r.read_profile().unwrap(), &sample_profile());
    }

    #[test]
    fn arrow_sections_are_aligned_standalone_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.opsnap");
        let mut p = sample_profile();
        p.telemetry_mut("cpu.process", "Process CPU", Unit::Percent);
        write_snapshot(&path, &p, None).unwrap();
        let r = SnapshotReader::open(&path).unwrap();
        assert_eq!(r.info.minor, MINOR);
        let names: Vec<_> = r
            .info
            .sections
            .iter()
            .filter(|e| e.codec == Codec::Arrow as u32)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "cpu",
                "alloc",
                "thread_states",
                "telemetry",
                "gc",
                "monitors",
                "probe_calls",
                "probe_events",
                "probe_controls"
            ]
        );
        for e in r.info.sections.iter().filter(|e| e.codec == Codec::Arrow as u32) {
            assert_eq!((e.offset + 13) % ARROW_ALIGN, 0, "{}", e.name);
            let b = r.stored(e).unwrap();
            assert!(b.starts_with(b"ARROW1") && b.ends_with(b"ARROW1"));
            let (md, _) = arrow::decode(b).unwrap();
            assert!(md["openprofiler.table"].len() > 1);
        }
        let q = r.read_profile().unwrap();
        // A series without points survives through the dictionaries.
        assert_eq!(q.telemetry[1].key, "cpu.process");
        assert_same(&q, &p);
    }

    #[test]
    fn arrow_many_batches_and_empty_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.opsnap");
        let mut p = sample_profile();
        let cls = p.symbols.intern_str("int[]");
        let s = p.samples_mut(SampleKind::Wall);
        for i in 0..(super::arrow::tests_batch_rows() * 2 + 17) {
            s.push(
                i as i64,
                0,
                ThreadState::from_u8((i % 8) as u8),
                1,
                i as u64,
                1,
                (i % 3 == 0).then_some(cls),
            );
        }
        write_snapshot(&path, &p, None).unwrap();
        assert_same(&read_snapshot(&path).unwrap().0, &p);
        let empty = Profile::new("x");
        write_snapshot(&path, &empty, None).unwrap();
        assert_same(&read_snapshot(&path).unwrap().0, &empty);
    }

    #[test]
    fn detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.opsnap");
        write_snapshot(&path, &sample_profile(), None).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[60] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let r = SnapshotReader::open(&path);
        assert!(r.is_err() || r.unwrap().read_profile().is_err());
    }
}
