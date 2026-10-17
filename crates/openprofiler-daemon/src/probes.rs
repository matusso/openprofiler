//! Probe views of the Query API (`POST /sessions/{id}/probes`), for live sessions and snapshots.
//!
//! Views: `list` (probes with totals), `hotspots` (payloads by total time), `events`, `timeline`
//! (calls and time per second per probe), `controls` (control objects with open/close times and
//! leak flags) and `telemetry` (calls per second and mean duration of one probe). Call trees of a
//! payload come from the tree endpoints with kind `probe` and `filter.classes = [payload]`.

use crate::engine::Session;
use anyhow::{Result, anyhow};
use openprofiler_model::{ControlEventKind, Profile};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Close description the agent uses for control objects collected without close.
const COLLECTED: &str = "garbage collected without close";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProbesRequest {
    /// Probe id (`jdbc`); required except for `list` and `timeline`.
    #[serde(default)]
    pub probe: Option<String>,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    /// Events: only this payload.
    #[serde(default)]
    pub payload: Option<String>,
    /// Events and hot spots: payloads containing this text (case-insensitive).
    #[serde(default)]
    pub search: Option<String>,
    /// Events: only failed calls.
    #[serde(default)]
    pub errors_only: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Controls: objects open longer than this are flagged (default 60 s).
    #[serde(default)]
    pub open_threshold_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HotSpotDto {
    pub payload: String,
    pub calls: u64,
    pub total: i64,
    pub mean: i64,
    pub max: i64,
    pub errors: u64,
    /// Threads the payload ran on.
    pub threads: usize,
}

fn in_range(range: Option<(i64, i64)>, t: i64) -> bool {
    range.is_none_or(|(a, b)| t >= a && t < b)
}

fn probe_index(p: &Profile, id: Option<&str>) -> Result<u32> {
    let id = id.ok_or_else(|| anyhow!("missing probe"))?;
    p.probes
        .probes
        .iter()
        .position(|x| x.id == id)
        .map(|i| i as u32)
        .ok_or_else(|| anyhow!("no such probe: {id}"))
}

fn top_frame(p: &Profile, stack: u32, frames: &mut Vec<u32>) -> String {
    p.stacks.frames_root_first(stack, frames);
    frames
        .iter()
        .rev()
        .map(|&f| p.symbols.method_label(p.symbols.frame(f).method))
        .find(|l| !l.starts_with('['))
        .unwrap_or_default()
}

fn thread_name(p: &Profile, i: u32) -> String {
    p.threads.get(i as usize).map(|t| t.name.clone()).unwrap_or_else(|| format!("#{i}"))
}

pub fn probes(session: &Session, req: &ProbesRequest) -> Result<Value> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let view = req.view.as_deref().unwrap_or("list");
    let limit = req.limit.unwrap_or(1000);
    let search = req.search.as_ref().map(|s| s.to_lowercase());
    let matches = |payload: &str| search.as_ref().is_none_or(|s| payload.to_lowercase().contains(s));
    match view {
        "list" => {
            let mut rows: Vec<Value> = vec![];
            for (i, info) in p.probes.probes.iter().enumerate() {
                let i = i as u32;
                let (mut calls, mut total, mut errors) = (0u64, 0i64, 0u64);
                let mut payloads = std::collections::HashSet::new();
                for c in p.probes.calls.iter().filter(|c| c.probe == i && in_range(req.range, c.time)) {
                    calls += c.count as u64;
                    total += c.duration;
                    errors += c.errors as u64;
                    payloads.insert(c.payload);
                }
                let events = p.probes.events.iter().filter(|e| e.probe == i).count();
                let (open, leaked) = control_counts(&p, i);
                rows.push(json!({
                    "id": info.id, "name": info.name, "payload_label": info.payload_label,
                    "control_label": info.control_label, "calls": calls, "total": total,
                    "errors": errors, "payloads": payloads.len(), "events": events,
                    "open_controls": open, "leaked_controls": leaked,
                }));
            }
            Ok(json!({ "probes": rows }))
        }
        "hotspots" => {
            let probe = probe_index(&p, req.probe.as_deref())?;
            let mut by: HashMap<u32, (HotSpotDto, std::collections::HashSet<u32>)> = HashMap::new();
            for c in p.probes.calls.iter().filter(|c| c.probe == probe && in_range(req.range, c.time)) {
                let payload = p.symbols.str(c.payload);
                if !matches(payload) {
                    continue;
                }
                let e = by.entry(c.payload).or_insert_with(|| {
                    let dto = HotSpotDto {
                        payload: payload.to_string(),
                        calls: 0,
                        total: 0,
                        mean: 0,
                        max: 0,
                        errors: 0,
                        threads: 0,
                    };
                    (dto, Default::default())
                });
                e.0.calls += c.count as u64;
                e.0.total += c.duration;
                e.0.max = e.0.max.max(c.max);
                e.0.errors += c.errors as u64;
                e.1.insert(c.thread);
            }
            let mut rows: Vec<HotSpotDto> = by
                .into_values()
                .map(|(mut d, threads)| {
                    d.mean = d.total / d.calls.max(1) as i64;
                    d.threads = threads.len();
                    d
                })
                .collect();
            rows.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.payload.cmp(&b.payload)));
            let total: i64 = rows.iter().map(|r| r.total).sum();
            rows.truncate(limit);
            Ok(json!({ "hotspots": rows, "total": total }))
        }
        "events" => {
            let probe = probe_index(&p, req.probe.as_deref())?;
            let mut frames = vec![];
            let events: Vec<Value> = p
                .probes
                .events
                .iter()
                .filter(|e| e.probe == probe && in_range(req.range, e.start))
                .filter(|e| !req.errors_only || e.error.is_some())
                .filter(|e| {
                    let payload = p.symbols.str(e.payload);
                    req.payload.as_deref().is_none_or(|x| x == payload) && matches(payload)
                })
                .take(limit)
                .map(|e| {
                    json!({
                        "start": e.start, "duration": e.duration,
                        "payload": p.symbols.str(e.payload), "thread": thread_name(&p, e.thread),
                        "error": e.error, "detail": e.detail, "context": e.context, "control": e.control,
                        "top_frame": top_frame(&p, e.stack, &mut frames),
                    })
                })
                .collect();
            Ok(json!({ "events": events }))
        }
        "timeline" => {
            let mut lanes: BTreeMap<u32, BTreeMap<i64, (u64, i64, u64)>> = BTreeMap::new();
            for c in p.probes.calls.iter().filter(|c| in_range(req.range, c.time)) {
                let e =
                    lanes.entry(c.probe).or_default().entry(c.time.div_euclid(NANOS_PER_SEC)).or_default();
                e.0 += c.count as u64;
                e.1 += c.duration;
                e.2 += c.errors as u64;
            }
            let lanes: Vec<Value> = lanes
                .into_iter()
                .filter_map(|(i, b)| {
                    let info = p.probes.probes.get(i as usize)?;
                    if req.probe.as_ref().is_some_and(|id| *id != info.id) {
                        return None;
                    }
                    Some(json!({
                        "probe": info.id, "name": info.name,
                        "time": b.keys().map(|s| s * NANOS_PER_SEC).collect::<Vec<_>>(),
                        "calls": b.values().map(|v| v.0).collect::<Vec<_>>(),
                        "duration": b.values().map(|v| v.1).collect::<Vec<_>>(),
                        "errors": b.values().map(|v| v.2).collect::<Vec<_>>(),
                    }))
                })
                .collect();
            Ok(json!({ "lanes": lanes }))
        }
        "telemetry" => {
            let probe = probe_index(&p, req.probe.as_deref())?;
            let mut b: BTreeMap<i64, (u64, i64)> = BTreeMap::new();
            for c in p.probes.calls.iter().filter(|c| c.probe == probe && in_range(req.range, c.time)) {
                let e = b.entry(c.time.div_euclid(NANOS_PER_SEC)).or_default();
                e.0 += c.count as u64;
                e.1 += c.duration;
            }
            let time: Vec<i64> = b.keys().map(|s| s * NANOS_PER_SEC).collect();
            let rate: Vec<f64> = b.values().map(|v| v.0 as f64).collect();
            let mean: Vec<f64> = b.values().map(|v| v.1 as f64 / v.0.max(1) as f64).collect();
            Ok(json!({ "series": [
                { "key": "calls", "label": "Calls per second", "unit": "per_second", "time": time, "value": rate },
                { "key": "mean", "label": "Mean duration", "unit": "nanos", "time": time, "value": mean },
            ] }))
        }
        "controls" => {
            let probe = probe_index(&p, req.probe.as_deref())?;
            let threshold = req.open_threshold_ms.unwrap_or(60_000) * 1_000_000;
            Ok(json!({ "controls": controls(&p, probe, threshold, limit) }))
        }
        other => Err(anyhow!("unknown probes view: {other}")),
    }
}

/// `(open, leaked)` control objects of a probe.
fn control_counts(p: &Profile, probe: u32) -> (usize, usize) {
    let mut open: HashMap<u64, bool> = HashMap::new();
    let mut leaked = 0;
    for c in p.probes.controls.iter().filter(|c| c.probe == probe) {
        match c.kind {
            ControlEventKind::Open => {
                open.insert(c.id, true);
            }
            ControlEventKind::Close => {
                open.remove(&c.id);
                leaked += (c.description == COLLECTED) as usize;
            }
        }
    }
    (open.len(), leaked)
}

fn controls(p: &Profile, probe: u32, threshold: i64, limit: usize) -> Vec<Value> {
    struct Row {
        id: u64,
        description: String,
        opened: i64,
        closed: Option<i64>,
        thread: u32,
        stack: u32,
        collected: bool,
    }
    let mut rows: Vec<Row> = vec![];
    let mut by_id: HashMap<u64, usize> = HashMap::new();
    for c in p.probes.controls.iter().filter(|c| c.probe == probe) {
        match c.kind {
            ControlEventKind::Open => {
                by_id.insert(c.id, rows.len());
                rows.push(Row {
                    id: c.id,
                    description: c.description.clone(),
                    opened: c.time,
                    closed: None,
                    thread: c.thread,
                    stack: c.stack,
                    collected: false,
                });
            }
            ControlEventKind::Close => {
                if let Some(&i) = by_id.get(&c.id) {
                    rows[i].closed = Some(c.time);
                    rows[i].collected = c.description == COLLECTED;
                }
            }
        }
    }
    let end = p.meta.end;
    let mut frames = vec![];
    // Open and leaked first, then by open time.
    rows.sort_by_key(|r| (r.closed.is_some() && !r.collected, std::cmp::Reverse(r.opened)));
    rows.into_iter()
        .take(limit)
        .map(|r| {
            let open_for = r.closed.unwrap_or(end) - r.opened;
            json!({
                "id": r.id, "description": r.description, "opened": r.opened, "closed": r.closed,
                "open_for": open_for, "thread": thread_name(p, r.thread),
                "top_frame": top_frame(p, r.stack, &mut frames),
                "state": if r.collected { "leaked" } else if r.closed.is_some() { "closed" }
                         else if open_for >= threshold { "long_open" } else { "open" },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use openprofiler_model::*;

    fn profile() -> Profile {
        let mut p = Profile::new("test");
        let t = p.thread_index(1, || "main".into());
        let m = p.symbols.intern_method("app.Dao", "find", "()V");
        let f = p.symbols.intern_frame(m, 3, FrameKind::Jit);
        let st = p.stacks.intern_root_first([f]);
        p.probes.probes.push(ProbeInfo {
            id: "jdbc".into(),
            name: "JDBC".into(),
            payload_label: "SQL statement".into(),
            control_label: Some("Connection".into()),
        });
        for (sql, d, n) in [("SELECT a", 300, 3), ("SELECT b", 900, 1)] {
            let payload = p.symbols.intern_str(sql);
            p.push_probe_call(ProbeCall {
                time: 2_000_000_000,
                probe: 0,
                payload,
                thread: t,
                stack: st,
                count: n,
                duration: d,
                max: d,
                errors: 0,
            });
            p.probes.events.push(ProbeEvent {
                start: 2_100_000_000,
                duration: d,
                probe: 0,
                payload,
                thread: t,
                stack: st,
                error: None,
                control: Some(1),
                detail: None,
                context: Some("GET /x".into()),
            });
        }
        for (id, kind, desc) in [
            (1, ControlEventKind::Open, "db"),
            (2, ControlEventKind::Open, "db"),
            (2, ControlEventKind::Close, COLLECTED),
        ] {
            p.probes.controls.push(ControlEvent {
                time: 1_000_000_000,
                probe: 0,
                id,
                kind,
                description: desc.into(),
                thread: t,
                stack: st,
            });
        }
        p.meta.end = 3_000_000_000;
        p
    }

    #[test]
    fn views() {
        let engine = Engine::new();
        let s = engine.add_profile(crate::SessionKind::Snapshot, "t", profile());
        let r = |view: &str| {
            probes(
                &s,
                &ProbesRequest { probe: Some("jdbc".into()), view: Some(view.into()), ..Default::default() },
            )
            .unwrap()
        };
        let list = probes(&s, &ProbesRequest::default()).unwrap();
        assert_eq!(list["probes"][0]["calls"], 4);
        assert_eq!(list["probes"][0]["open_controls"], 1);
        assert_eq!(list["probes"][0]["leaked_controls"], 1);
        let hs = r("hotspots");
        assert_eq!(hs["hotspots"][0]["payload"], "SELECT b");
        assert_eq!(hs["hotspots"][1]["mean"], 100);
        let ev = r("events");
        assert_eq!(ev["events"][0]["top_frame"], "app.Dao.find");
        assert_eq!(ev["events"][0]["context"], "GET /x");
        assert_eq!(r("timeline")["lanes"][0]["calls"][0], 4);
        assert_eq!(r("telemetry")["series"][0]["value"][0], 4.0);
        let c = r("controls");
        let state = |id: u64| {
            c["controls"].as_array().unwrap().iter().find(|x| x["id"] == id).unwrap()["state"].clone()
        };
        assert_eq!(state(1), "open");
        assert_eq!(state(2), "leaked");
        let c = probes(
            &s,
            &ProbesRequest {
                probe: Some("jdbc".into()),
                view: Some("controls".into()),
                open_threshold_ms: Some(1000),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(c["controls"].as_array().unwrap().iter().any(|x| x["state"] == "long_open"));
        // Payload call trees come from the `probe` sample kind.
        assert_eq!(s.require_profile().unwrap().read().samples[&SampleKind::Probe].len(), 2);
    }
}
