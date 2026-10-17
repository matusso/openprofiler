//! Applies agent-link data batches to an [`openprofiler_model::Profile`].

use anyhow::{Context, Result, bail};
use openprofiler_model::{
    Bookmark, ControlEvent, ControlEventKind, FrameKind, GcEvent, JvmInfo, Method, MonitorEvent,
    MonitorEventKind, ProbeCall, ProbeEvent, ProbeInfo, Profile, SampleKind, ThreadInfo, ThreadState, Unit,
};
use openprofiler_proto::v1;

pub fn unit_from_u32(u: u32) -> Unit {
    match u {
        0 => Unit::Nanos,
        1 => Unit::Bytes,
        3 => Unit::Percent,
        4 => Unit::PerSecond,
        _ => Unit::Count,
    }
}

pub fn jvm_info(j: &v1::JvmInfo) -> JvmInfo {
    JvmInfo {
        pid: (j.pid != 0).then_some(j.pid),
        name: j.name.clone(),
        vm_name: j.vm_name.clone(),
        vm_version: j.vm_version.clone(),
        vm_vendor: j.vm_vendor.clone(),
        java_version: j.java_version.clone(),
        jvm_args: j.jvm_args.clone(),
        main_class: j.main_class.clone(),
        os: j.os.clone(),
        arch: j.arch.clone(),
        cpus: (j.cpus != 0).then_some(j.cpus),
        max_heap: (j.max_heap != 0).then_some(j.max_heap),
        start_time: (j.start_time != 0).then_some(j.start_time),
    }
}

pub fn thread_dump(d: &v1::ThreadDump) -> openprofiler_model::ThreadDump {
    openprofiler_model::ThreadDump {
        time: d.time,
        label: d.label.clone(),
        threads: d
            .threads
            .iter()
            .map(|t| openprofiler_model::DumpedThread {
                name: t.name.clone(),
                java_id: t.java_id,
                state: t.state.clone(),
                is_virtual: t.is_virtual,
                frames: t.frames.clone(),
                locked: t.locked.clone(),
                waiting_on: (!t.waiting_on.is_empty()).then(|| t.waiting_on.clone()),
            })
            .collect(),
    }
}

/// Initializes profile metadata from the agent's `Hello`.
pub fn apply_hello(p: &mut Profile, hello: &v1::Hello) {
    if let Some(j) = &hello.jvm {
        p.meta.jvm = jvm_info(j);
    }
    p.meta.properties.insert("agent_version".into(), hello.agent_version.clone());
    p.meta.properties.insert("agent_capabilities".into(), hello.capabilities.join(","));
    p.meta.properties.insert("agent_mode".into(), hello.mode.clone());
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn apply_probes(p: &mut Profile, b: &v1::DataBatch) {
    let base = b.from.as_ref().map_or(0, |w| w.probes) as usize;
    for (i, info) in b.probes.iter().enumerate() {
        if base + i < p.probes.probes.len() {
            continue;
        }
        p.probes.probes.push(ProbeInfo {
            id: info.id.clone(),
            name: info.name.clone(),
            payload_label: info.payload_label.clone(),
            control_label: non_empty(&info.control_label),
        });
    }
    if let Some(c) = &b.probe_calls {
        for i in 0..c.time.len() {
            let at = |v: &Vec<u32>| v.get(i).copied().unwrap_or(0);
            let at64 = |v: &Vec<i64>| v.get(i).copied().unwrap_or(0);
            p.push_probe_call(ProbeCall {
                time: c.time[i],
                probe: at(&c.probe),
                payload: at(&c.payload),
                thread: at(&c.thread),
                stack: at(&c.stack),
                count: at(&c.count),
                duration: at64(&c.duration),
                max: at64(&c.max),
                errors: at(&c.errors),
            });
        }
        if let (Some(&first), Some(&last)) = (c.time.first(), c.time.last()) {
            p.extend_range(first);
            p.extend_range(last + 1_000_000_000);
        }
    }
    for e in &b.probe_events {
        p.probes.events.push(ProbeEvent {
            start: e.start,
            duration: e.duration,
            probe: e.probe,
            payload: e.payload,
            thread: e.thread,
            stack: e.stack,
            error: non_empty(&e.error),
            control: (e.control != 0).then_some(e.control),
            detail: non_empty(&e.detail),
            context: non_empty(&e.context),
        });
    }
    for c in &b.probe_controls {
        p.probes.controls.push(ControlEvent {
            time: c.time,
            probe: c.probe,
            id: c.id,
            kind: if c.kind == 1 { ControlEventKind::Close } else { ControlEventKind::Open },
            description: c.description.clone(),
            thread: c.thread,
            stack: c.stack,
        });
    }
}

/// Applies one batch. Tables are positional: elements before the profile's current length are
/// assumed to be already applied and are skipped, so re-sent batches are idempotent for
/// symbols, stacks and threads.
pub fn apply_batch(p: &mut Profile, b: &v1::DataBatch) -> Result<()> {
    if let Some(s) = &b.symbols {
        let have = p.symbols.strings().len();
        for (i, text) in s.strings.iter().enumerate() {
            let pos = s.string_base as usize + i;
            if pos < have {
                continue;
            }
            let id = p.symbols.intern_str(text);
            if id as usize != pos {
                bail!("string table out of sync at {pos} (got {id})");
            }
        }
        let have = p.symbols.methods().len();
        for i in 0..s.method_class.len() {
            let pos = s.method_base as usize + i;
            if pos < have {
                continue;
            }
            let m = Method {
                class: s.method_class[i],
                name: *s.method_name.get(i).context("method_name column")?,
                signature: *s.method_signature.get(i).context("method_signature column")?,
            };
            let id = p.symbols.intern_method_ids(m);
            if id as usize != pos {
                bail!("method table out of sync at {pos}");
            }
        }
        let have = p.symbols.frames().len();
        for i in 0..s.frame_method.len() {
            let pos = s.frame_base as usize + i;
            if pos < have {
                continue;
            }
            let kind = FrameKind::from_u8(*s.frame_kind.get(i).unwrap_or(&6) as u8);
            let id = p.symbols.intern_frame(s.frame_method[i], *s.frame_line.get(i).unwrap_or(&-1), kind);
            if id as usize != pos {
                bail!("frame table out of sync at {pos}");
            }
        }
    }
    if let Some(s) = &b.stacks {
        let have = p.stacks.len();
        for i in 0..s.parent.len() {
            let pos = s.base as usize + i;
            if pos < have {
                continue;
            }
            let id = p.stacks.push(s.parent[i], *s.frame.get(i).context("stack frame column")?);
            if id as usize != pos {
                bail!("stack trie out of sync at {pos}");
            }
        }
    }
    if let Some(t) = &b.threads {
        let have = p.threads.len();
        for (i, th) in t.threads.iter().enumerate() {
            if (t.base as usize + i) < have {
                continue;
            }
            p.threads.push(ThreadInfo {
                java_id: th.java_id,
                os_id: (th.os_id != 0).then_some(th.os_id),
                name: th.name.clone(),
                group: (!th.group.is_empty()).then(|| th.group.clone()),
                is_virtual: th.is_virtual,
                is_daemon: th.is_daemon,
                start: (th.start != 0).then_some(th.start),
                end: (th.end != 0).then_some(th.end),
            });
        }
        for u in &t.updates {
            if let Some(th) = p.threads.get_mut(u.index as usize) {
                if !u.name.is_empty() {
                    th.name = u.name.clone();
                }
                if u.end != 0 {
                    th.end = Some(u.end);
                }
            }
        }
    }
    for s in &b.samples {
        let Some(kind) = SampleKind::from_u8(s.kind as u8) else { continue };
        if kind == SampleKind::Probe {
            // Derived from probe calls.
            continue;
        }
        let n = s.time.len();
        let set = p.samples_mut(kind);
        for i in 0..n {
            let class = s.class_id.get(i).copied().filter(|&c| c != u32::MAX);
            set.push(
                s.time[i],
                s.thread.get(i).copied().unwrap_or(0),
                ThreadState::from_u8(s.state.get(i).copied().unwrap_or(7) as u8),
                s.stack.get(i).copied().unwrap_or(0),
                s.weight.get(i).copied().unwrap_or(0),
                s.count.get(i).copied().unwrap_or(1),
                class,
            );
        }
        if let (Some(&first), Some(&last)) = (s.time.first(), s.time.last()) {
            p.extend_range(first);
            p.extend_range(last + 1_000_000_000);
        }
    }
    if let Some(t) = &b.telemetry {
        for s in &t.series {
            let series = p.telemetry_mut(&s.key, &s.label, unit_from_u32(s.unit));
            for (t, v) in s.time.iter().zip(&s.value) {
                series.push(*t, *v);
            }
            if let (Some(&f), Some(&l)) = (s.time.first(), s.time.last()) {
                p.extend_range(f);
                p.extend_range(l);
            }
        }
    }
    if let Some(s) = &b.thread_states {
        for i in 0..s.thread.len() {
            p.thread_states.push(
                s.thread[i],
                *s.start.get(i).unwrap_or(&0),
                *s.end.get(i).unwrap_or(&0),
                ThreadState::from_u8(*s.state.get(i).unwrap_or(&7) as u8),
            );
        }
    }
    if let Some(m) = &b.monitors {
        for e in &m.events {
            p.monitors.push(MonitorEvent {
                start: e.start,
                duration: e.duration,
                kind: match e.kind {
                    0 => MonitorEventKind::Enter,
                    1 => MonitorEventKind::Wait,
                    _ => MonitorEventKind::Park,
                },
                thread: e.thread,
                owner: e.owner.checked_sub(1),
                monitor_class: e.monitor_class.clone(),
                monitor_id: (e.monitor_id != 0).then_some(e.monitor_id),
                stack: e.stack,
            });
        }
    }
    if let Some(g) = &b.gc {
        for e in &g.events {
            p.gc.push(GcEvent {
                start: e.start,
                duration: e.duration,
                name: e.name.clone(),
                cause: e.cause.clone(),
                heap_before: (e.heap_before != 0).then_some(e.heap_before),
                heap_after: (e.heap_after != 0).then_some(e.heap_after),
                pause: (e.pause != 0).then_some(e.pause),
            });
        }
    }
    for bm in &b.bookmarks {
        p.bookmarks.push(Bookmark {
            time: bm.time,
            label: bm.label.clone(),
            color: (!bm.color.is_empty()).then(|| bm.color.clone()),
        });
    }
    for d in &b.thread_dumps {
        p.thread_dumps.push(thread_dump(d));
    }
    apply_probes(p, b);
    if b.time != 0 {
        p.extend_range(b.time);
    }
    Ok(())
}
