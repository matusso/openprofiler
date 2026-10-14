//! Query API request/response types and their evaluation against a session's [`Profile`].
//! Shared by the HTTP layer, the CLI exporters and the MCP server.

use crate::engine::Session;
use anyhow::{Result, anyhow};
use openprofiler_model::lttb::lttb_indices;
use openprofiler_model::symbols::pretty_descriptor;
use openprofiler_model::views::*;
use openprofiler_model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Requests

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct FilterDto {
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    #[serde(default)]
    pub threads: Option<Vec<u32>>,
    #[serde(default)]
    pub states: Option<Vec<ThreadState>>,
    #[serde(default)]
    pub classes: Option<Vec<String>>,
}

impl FilterDto {
    pub fn to_filter(&self, p: &Profile) -> SampleFilter {
        SampleFilter {
            range: self.range,
            threads: self.threads.clone(),
            states: self.states.clone(),
            classes: self
                .classes
                .as_ref()
                .map(|cs| cs.iter().map(|c| p.symbols.lookup_str(c).unwrap_or(u32::MAX - 1)).collect()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TreeView {
    #[default]
    Calltree,
    Backtraces,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct TreeRequest {
    pub kind: SampleKind,
    #[serde(default)]
    pub view: TreeView,
    #[serde(default)]
    pub filter: FilterDto,
    #[serde(default)]
    pub aggregation: Aggregation,
    #[serde(default)]
    pub collapse_recursion: bool,
    #[serde(default)]
    pub compact_prefixes: Vec<String>,
    #[serde(default)]
    pub focus: Option<Focus>,
    #[serde(default)]
    pub node: Option<u32>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub min_fraction: Option<f64>,
    /// Flame only: build the inverted (icicle of callers) layout.
    #[serde(default)]
    pub inverted: bool,
}

impl TreeRequest {
    pub fn new(kind: SampleKind) -> Self {
        Self {
            kind,
            view: TreeView::Calltree,
            filter: FilterDto::default(),
            aggregation: Aggregation::Method,
            collapse_recursion: false,
            compact_prefixes: vec![],
            focus: None,
            node: None,
            depth: None,
            min_fraction: None,
            inverted: false,
        }
    }

    fn options(&self, p: &Profile) -> TreeOptions {
        TreeOptions {
            filter: self.filter.to_filter(p),
            aggregation: self.aggregation,
            inverted: self.view == TreeView::Backtraces || self.inverted,
            collapse_recursion: self.collapse_recursion,
            focus: self.focus,
            compact_prefixes: self.compact_prefixes.clone(),
        }
    }

    /// Cache key for the tree shape (independent of node/depth).
    fn tree_key(&self) -> String {
        let mut r = self.clone();
        r.node = None;
        r.depth = None;
        r.min_fraction = None;
        format!("tree:{}", serde_json::to_string(&r).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------------------------
// Responses

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TreeNodeDto {
    pub id: u32,
    pub parent: Option<u32>,
    pub key: u32,
    pub depth: u32,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub kind: FrameKind,
    #[serde(rename = "self")]
    pub self_: u64,
    pub total: u64,
    pub self_count: u64,
    pub total_count: u64,
    pub child_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeResponse {
    pub total: u64,
    pub total_count: u64,
    pub unit: Unit,
    pub generation: u64,
    pub nodes: Vec<TreeNodeDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HotSpotDto {
    pub key: u32,
    pub label: String,
    #[serde(rename = "self")]
    pub self_: u64,
    pub self_count: u64,
    pub total: u64,
    pub total_count: u64,
    /// Node id of this hot spot in the `backtraces` tree with the same parameters.
    pub node: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HotSpotsResponse {
    pub total: u64,
    pub unit: Unit,
    pub hotspots: Vec<HotSpotDto>,
}

const MAX_NODES: usize = 20_000;

fn build(session: &Session, p: &Profile, req: &TreeRequest) -> Result<Arc<BuiltTree>> {
    session.cached(&req.tree_key(), || Ok(build_tree(p, req.kind, &req.options(p))))
}

fn node_dto(p: &Profile, bt: &BuiltTree, id: u32, depth: u32) -> TreeNodeDto {
    let n = &bt.tree.nodes[id as usize];
    let label = key_label(&p.symbols, bt.tree.aggregation, n.key, &bt.extra_strings);
    let (class, method, signature) = if bt.tree.aggregation == Aggregation::Method && n.key != u32::MAX {
        let m = p.symbols.method(n.key);
        let sig = p.symbols.str(m.signature);
        (
            Some(p.symbols.str(m.class).to_owned()),
            Some(p.symbols.str(m.name).to_owned()),
            (!sig.is_empty()).then(|| pretty_descriptor(sig)),
        )
    } else {
        (None, None, None)
    };
    TreeNodeDto {
        id,
        parent: (id != ROOT_NODE).then_some(n.parent),
        key: n.key,
        depth,
        label,
        class,
        method,
        signature,
        kind: n.kind,
        self_: n.self_weight,
        total: n.total_weight,
        self_count: n.self_count,
        total_count: n.total_count,
        child_count: n.children.len() as u32,
    }
}

pub fn tree(session: &Session, req: &TreeRequest) -> Result<TreeResponse> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let bt = build(session, &p, req)?;
    let start = req.node.unwrap_or(ROOT_NODE);
    let start_node = bt.tree.node(start).ok_or_else(|| anyhow!("no such node: {start}"))?;
    let max_depth = req.depth.unwrap_or(1);
    let root_total = bt.tree.total_weight().max(1) as f64;
    let threshold = req.min_fraction.map(|f| (f * root_total) as u64);
    let base_depth = depth_of(&bt.tree, start);
    let mut nodes = vec![node_dto(&p, &bt, start, base_depth)];
    // Pre-order walk.
    let mut stack: Vec<(u32, u32)> = start_node.children.iter().rev().map(|&c| (c, 1)).collect();
    while let Some((id, rel)) = stack.pop() {
        if nodes.len() >= MAX_NODES {
            break;
        }
        let n = &bt.tree.nodes[id as usize];
        let within = rel <= max_depth || threshold.is_some_and(|t| n.total_weight >= t && rel <= 256);
        if !within {
            continue;
        }
        nodes.push(node_dto(&p, &bt, id, base_depth + rel));
        stack.extend(n.children.iter().rev().map(|&c| (c, rel + 1)));
    }
    Ok(TreeResponse {
        total: bt.tree.total_weight(),
        total_count: bt.tree.root().total_count,
        unit: req.kind.unit(),
        generation: session.generation(),
        nodes,
    })
}

fn depth_of(t: &CallTree, mut id: u32) -> u32 {
    let mut d = 0;
    while id != ROOT_NODE {
        id = t.nodes[id as usize].parent;
        d += 1;
    }
    d
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HotSpotsRequest {
    pub kind: SampleKind,
    #[serde(default)]
    pub filter: FilterDto,
    #[serde(default)]
    pub aggregation: Aggregation,
    #[serde(default)]
    pub compact_prefixes: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

pub fn hotspots(session: &Session, req: &HotSpotsRequest) -> Result<HotSpotsResponse> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let tree_req = TreeRequest {
        view: TreeView::Backtraces,
        filter: req.filter.clone(),
        aggregation: req.aggregation,
        compact_prefixes: req.compact_prefixes.clone(),
        ..TreeRequest::new(req.kind)
    };
    let opts = tree_req.options(&p);
    let key = format!("hotspots:{}", serde_json::to_string(req).unwrap_or_default());
    let (spots, extra) = &*session.cached(&key, || Ok(hot_spots(&p, req.kind, &opts)))?;
    let bt = build(session, &p, &tree_req)?;
    let index: std::collections::HashMap<u32, u32> =
        bt.tree.root().children.iter().map(|&c| (bt.tree.nodes[c as usize].key, c)).collect();
    let total = bt.tree.total_weight();
    let limit = req.limit.unwrap_or(500);
    Ok(HotSpotsResponse {
        total,
        unit: req.kind.unit(),
        hotspots: spots
            .iter()
            .filter(|h| h.self_weight > 0)
            .take(limit)
            .map(|h| HotSpotDto {
                key: h.key,
                label: key_label(&p.symbols, req.aggregation, h.key, extra),
                self_: h.self_weight,
                self_count: h.self_count,
                total: h.total_weight,
                total_count: h.total_count,
                node: index.get(&h.key).copied(),
            })
            .collect(),
    })
}

pub fn flame(session: &Session, req: &TreeRequest) -> Result<FlameLayout> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let bt = build(session, &p, req)?;
    let key = format!("flame:{}:{:?}", req.tree_key(), req.min_fraction);
    let f = session.cached(&key, || {
        Ok(flame_layout(&bt.tree, &p.symbols, &bt.extra_strings, req.min_fraction.unwrap_or(0.0005)))
    })?;
    Ok((*f).clone())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CallGraphRequest {
    pub kind: SampleKind,
    #[serde(default)]
    pub filter: FilterDto,
    #[serde(default)]
    pub aggregation: Aggregation,
    #[serde(default)]
    pub focus_key: Option<u32>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallGraphNodeDto {
    pub key: u32,
    pub label: String,
    #[serde(rename = "self")]
    pub self_: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallGraphEdgeDto {
    pub from: u32,
    pub to: u32,
    pub weight: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallGraphResponse {
    pub total: u64,
    pub nodes: Vec<CallGraphNodeDto>,
    pub edges: Vec<CallGraphEdgeDto>,
}

pub fn callgraph(session: &Session, req: &CallGraphRequest) -> Result<CallGraphResponse> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let opts =
        TreeOptions { filter: req.filter.to_filter(&p), aggregation: req.aggregation, ..Default::default() };
    let (g, extra) = call_graph(&p, req.kind, &opts);
    let total = aggregate_stacks(&p, req.kind, &opts.filter).iter().map(|s| s.1).sum();
    let limit = req.limit.unwrap_or(60);
    // Choose the node subset: the focus neighbourhood (2 levels each way) or the top nodes by
    // total weight.
    let mut keep: Vec<u32> = match req.focus_key.and_then(|k| g.nodes.iter().position(|n| n.key == k)) {
        Some(center) => {
            let center = center as u32;
            let mut set = vec![center];
            for _ in 0..2 {
                let frontier = set.clone();
                for &(a, b, _, _) in &g.edges {
                    if frontier.contains(&a) && !set.contains(&b) {
                        set.push(b);
                    }
                    if frontier.contains(&b) && !set.contains(&a) {
                        set.push(a);
                    }
                }
            }
            set
        }
        None => {
            let mut idx: Vec<u32> = (0..g.nodes.len() as u32).collect();
            idx.sort_by_key(|&i| std::cmp::Reverse(g.nodes[i as usize].total_weight));
            idx
        }
    };
    keep.truncate(limit);
    let pos: std::collections::HashMap<u32, u32> =
        keep.iter().enumerate().map(|(i, &n)| (n, i as u32)).collect();
    Ok(CallGraphResponse {
        total,
        nodes: keep
            .iter()
            .map(|&i| {
                let h = &g.nodes[i as usize];
                CallGraphNodeDto {
                    key: h.key,
                    label: key_label(&p.symbols, req.aggregation, h.key, &extra),
                    self_: h.self_weight,
                    total: h.total_weight,
                }
            })
            .collect(),
        edges: g
            .edges
            .iter()
            .filter_map(|&(a, b, w, _)| {
                Some(CallGraphEdgeDto { from: *pos.get(&a)?, to: *pos.get(&b)?, weight: w })
            })
            .collect(),
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct HistogramRequest {
    pub kind: SampleKind,
    #[serde(default)]
    pub filter: FilterDto,
    #[serde(default)]
    pub buckets: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistogramResponse {
    pub start: i64,
    pub end: i64,
    pub values: Vec<u64>,
}

pub fn histogram(session: &Session, req: &HistogramRequest) -> Result<HistogramResponse> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let f = req.filter.to_filter(&p);
    let (start, end) = f.range.unwrap_or((p.meta.start, p.meta.end + 1));
    Ok(HistogramResponse {
        start,
        end,
        values: sample_histogram(&p, req.kind, &f, req.buckets.unwrap_or(200)),
    })
}

// ---------------------------------------------------------------------------------------------
// Summary & telemetry

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub session: crate::engine::SessionInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jvm: Option<JvmInfo>,
    pub start: i64,
    pub end: i64,
    pub sample_counts: BTreeMap<String, u64>,
    pub threads: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heap: Option<serde_json::Value>,
}

pub fn summary(session: &Session) -> Result<Summary> {
    let info = session.info();
    let heap = session.heap().and_then(|h| crate::heap::summary_json(&h).ok());
    let Some(prof) = session.profile() else {
        return Ok(Summary {
            session: info,
            jvm: None,
            start: 0,
            end: 0,
            sample_counts: BTreeMap::new(),
            threads: 0,
            heap,
        });
    };
    let p = prof.read();
    Ok(Summary {
        session: info,
        jvm: Some(p.meta.jvm.clone()),
        start: p.meta.start,
        end: p.meta.end,
        sample_counts: p
            .samples
            .iter()
            .map(|(k, s)| (k.name().to_owned(), s.count.iter().map(|&c| c as u64).sum()))
            .collect(),
        threads: p.threads.len(),
        heap,
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TelemetryRequest {
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    #[serde(default)]
    pub points: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeriesDto {
    pub key: String,
    pub label: String,
    pub unit: Unit,
    pub t: Vec<i64>,
    pub v: Vec<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetryResponse {
    pub series: Vec<SeriesDto>,
    pub gc: Vec<GcEvent>,
    pub bookmarks: Vec<Bookmark>,
}

/// Telemetry series derived from samples when the source has no explicit series.
fn derived_series(p: &Profile) -> Vec<TelemetrySeries> {
    let mut out = vec![];
    for kind in [SampleKind::Cpu, SampleKind::Alloc] {
        let Some(set) = p.samples.get(&kind) else { continue };
        if set.is_empty() || p.meta.end <= p.meta.start {
            continue;
        }
        let secs = ((p.meta.end - p.meta.start) / 1_000_000_000 + 1).clamp(1, 1_000_000) as usize;
        let mut buckets = vec![0f64; secs];
        for i in 0..set.len() {
            let b = ((set.time[i] - p.meta.start) / 1_000_000_000).clamp(0, secs as i64 - 1) as usize;
            buckets[b] += set.weight[i] as f64;
        }
        let (key, label, unit) = match kind {
            SampleKind::Cpu => ("samples.cpu", "Sampled CPU time", Unit::Nanos),
            _ => ("samples.alloc", "Sampled allocations", Unit::Bytes),
        };
        let mut s = TelemetrySeries::new(key, label, unit);
        for (i, v) in buckets.into_iter().enumerate() {
            s.push(p.meta.start + i as i64 * 1_000_000_000, v);
        }
        out.push(s);
    }
    out
}

pub fn telemetry(session: &Session, req: &TelemetryRequest) -> Result<TelemetryResponse> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let points = req.points.unwrap_or(800).max(3);
    let derived =
        if p.telemetry.iter().any(|s| s.key.starts_with("cpu.")) { vec![] } else { derived_series(&p) };
    let series = p
        .telemetry
        .iter()
        .chain(derived.iter())
        .filter(|s| req.keys.as_ref().is_none_or(|k| k.iter().any(|k| k == &s.key)))
        .map(|s| {
            let (lo, hi) = match req.range {
                Some((a, b)) => (s.time.partition_point(|&t| t < a), s.time.partition_point(|&t| t < b)),
                None => (0, s.time.len()),
            };
            let t = &s.time[lo..hi];
            let v = &s.value[lo..hi];
            let x: Vec<f64> = t.iter().map(|&t| t as f64).collect();
            let idx = lttb_indices(&x, v, points);
            SeriesDto {
                key: s.key.clone(),
                label: s.label.clone(),
                unit: s.unit,
                t: idx.iter().map(|&i| t[i]).collect(),
                v: idx.iter().map(|&i| v[i]).collect(),
            }
        })
        .collect();
    let in_range = |t: i64| req.range.is_none_or(|(a, b)| t >= a && t < b);
    Ok(TelemetryResponse {
        series,
        gc: p.gc.iter().filter(|g| in_range(g.start)).cloned().collect(),
        bookmarks: p.bookmarks.iter().filter(|b| in_range(b.time)).cloned().collect(),
    })
}

// ---------------------------------------------------------------------------------------------
// Threads & monitors

#[derive(Debug, Clone, Serialize)]
pub struct ThreadDto {
    pub index: u32,
    #[serde(flatten)]
    pub info: ThreadInfo,
    pub cpu: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadsResponse {
    pub threads: Vec<ThreadDto>,
}

pub fn threads(session: &Session) -> Result<ThreadsResponse> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let cpu: std::collections::HashMap<u32, u64> =
        thread_totals(&p, SampleKind::Cpu, &SampleFilter::default())
            .into_iter()
            .map(|(t, w, _)| (t, w))
            .collect();
    Ok(ThreadsResponse {
        threads: p
            .threads
            .iter()
            .enumerate()
            .map(|(i, t)| ThreadDto {
                index: i as u32,
                info: t.clone(),
                cpu: cpu.get(&(i as u32)).copied().unwrap_or(0),
            })
            .collect(),
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThreadStatesRequest {
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    #[serde(default)]
    pub threads: Option<Vec<u32>>,
    #[serde(default)]
    pub max_spans: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpanDto {
    pub thread: u32,
    pub start: i64,
    pub end: i64,
    pub state: ThreadState,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadStatesResponse {
    pub spans: Vec<SpanDto>,
    pub truncated: bool,
}

pub fn thread_states(session: &Session, req: &ThreadStatesRequest) -> Result<ThreadStatesResponse> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let s = &p.thread_states;
    let max = req.max_spans.unwrap_or(200_000);
    let mut spans = vec![];
    let mut truncated = false;
    for i in 0..s.len() {
        if req.threads.as_ref().is_some_and(|t| !t.contains(&s.thread[i])) {
            continue;
        }
        let (mut a, mut b) = (s.start[i], s.end[i]);
        if let Some((ra, rb)) = req.range {
            if b <= ra || a >= rb {
                continue;
            }
            a = a.max(ra);
            b = b.min(rb);
        }
        if spans.len() >= max {
            truncated = true;
            break;
        }
        spans.push(SpanDto { thread: s.thread[i], start: a, end: b, state: s.state[i] });
    }
    // Sources without explicit state spans: derive coarse spans from wall/cpu samples.
    if s.is_empty() {
        let set = p.samples.get(&SampleKind::Wall).or_else(|| p.samples.get(&SampleKind::Cpu));
        if let Some(set) = set {
            let interval = p.meta.cpu_interval.unwrap_or(20_000_000);
            let mut order: Vec<usize> = (0..set.len()).collect();
            order.sort_by_key(|&i| (set.thread[i], set.time[i]));
            let mut cur: Option<SpanDto> = None;
            for i in order {
                let (th, t, st) = (set.thread[i], set.time[i], set.state[i]);
                if req.threads.as_ref().is_some_and(|x| !x.contains(&th)) {
                    continue;
                }
                if req.range.is_some_and(|(a, b)| t < a || t >= b) {
                    continue;
                }
                match &mut cur {
                    Some(c) if c.thread == th && c.state == st && t - c.end <= interval * 2 => {
                        c.end = t + interval
                    }
                    _ => {
                        if let Some(c) = cur.take() {
                            spans.push(c);
                        }
                        cur = Some(SpanDto { thread: th, start: t, end: t + interval, state: st });
                    }
                }
                if spans.len() >= max {
                    truncated = true;
                    break;
                }
            }
            spans.extend(cur);
        }
    }
    Ok(ThreadStatesResponse { spans, truncated })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorsRequest {
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorEventDto {
    pub start: i64,
    pub duration: i64,
    pub kind: MonitorEventKind,
    pub thread: String,
    pub owner: Option<String>,
    pub monitor_class: String,
    pub top_frame: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorStatDto {
    pub monitor_class: String,
    pub count: u64,
    pub total_duration: i64,
    pub max_duration: i64,
}

pub fn monitors(session: &Session, req: &MonitorsRequest) -> Result<serde_json::Value> {
    let prof = session.require_profile()?;
    let p = prof.read();
    let evs = p.monitors.iter().filter(|e| req.range.is_none_or(|(a, b)| e.start >= a && e.start < b));
    let tname = |i: u32| p.threads.get(i as usize).map(|t| t.name.clone()).unwrap_or_else(|| format!("#{i}"));
    if req.view.as_deref() == Some("stats") {
        let mut m: BTreeMap<String, MonitorStatDto> = BTreeMap::new();
        for e in evs {
            let s = m.entry(e.monitor_class.clone()).or_insert_with(|| MonitorStatDto {
                monitor_class: e.monitor_class.clone(),
                count: 0,
                total_duration: 0,
                max_duration: 0,
            });
            s.count += 1;
            s.total_duration += e.duration;
            s.max_duration = s.max_duration.max(e.duration);
        }
        let mut v: Vec<_> = m.into_values().collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.total_duration));
        return Ok(serde_json::json!({ "stats": v }));
    }
    let mut frames = vec![];
    let events: Vec<MonitorEventDto> = evs
        .take(req.limit.unwrap_or(10_000))
        .map(|e| {
            p.stacks.frames_root_first(e.stack, &mut frames);
            let top =
                frames.last().map(|&f| p.symbols.method_label(p.symbols.frame(f).method)).unwrap_or_default();
            MonitorEventDto {
                start: e.start,
                duration: e.duration,
                kind: e.kind,
                thread: tname(e.thread),
                owner: e.owner.map(tname),
                monitor_class: e.monitor_class.clone(),
                top_frame: top,
            }
        })
        .collect();
    Ok(serde_json::json!({ "events": events }))
}

pub fn thread_dumps(session: &Session) -> Result<Vec<ThreadDump>> {
    let prof = session.require_profile()?;
    Ok(prof.read().thread_dumps.clone())
}

// ---------------------------------------------------------------------------------------------
// Compare

#[derive(Debug, Clone, Deserialize)]
pub struct CompareRequest {
    pub a: String,
    pub b: String,
    pub kind: String,
    #[serde(default)]
    pub aggregation: Aggregation,
    #[serde(default)]
    pub filter: FilterDto,
    #[serde(default)]
    pub node: Option<u32>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub min_fraction: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffNodeDto {
    pub id: u32,
    pub parent: Option<u32>,
    pub depth: u32,
    pub label: String,
    pub a_self: u64,
    pub a_total: u64,
    pub b_self: u64,
    pub b_total: u64,
    pub child_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareResponse {
    pub a_total: u64,
    pub b_total: u64,
    pub unit: Unit,
    pub nodes: Vec<DiffNodeDto>,
}

pub fn diff_tree(a: &Session, b: &Session, req: &CompareRequest) -> Result<(DiffTree, Unit)> {
    let kind =
        SampleKind::parse(&req.kind).ok_or_else(|| anyhow!("unsupported compare kind: {}", req.kind))?;
    let tr =
        TreeRequest { filter: req.filter.clone(), aggregation: req.aggregation, ..TreeRequest::new(kind) };
    let (pa, pb) = (a.require_profile()?, b.require_profile()?);
    let (pa, pb) = (pa.read(), pb.read());
    // Time ranges are per-profile; a range filter only makes sense within one session.
    let mut tr_nr = tr.clone();
    tr_nr.filter.range = None;
    let ta = build(a, &pa, &tr_nr)?;
    let tb = build(b, &pb, &tr_nr)?;
    Ok((diff_trees(&ta, &pa.symbols, &tb, &pb.symbols), kind.unit()))
}

pub fn compare(a: &Session, b: &Session, req: &CompareRequest) -> Result<CompareResponse> {
    let (d, unit) = diff_tree(a, b, req)?;
    let start = req.node.unwrap_or(0);
    if start as usize >= d.nodes.len() {
        return Err(anyhow!("no such node: {start}"));
    }
    let max_depth = req.depth.unwrap_or(1);
    let root = &d.nodes[0];
    let threshold = req.min_fraction.map(|f| (f * root.a_total.max(root.b_total) as f64) as u64);
    let mut base = 0;
    let mut x = start;
    while x != 0 {
        x = d.nodes[x as usize].parent;
        base += 1;
    }
    let dto = |id: u32, depth: u32| {
        let n = &d.nodes[id as usize];
        DiffNodeDto {
            id,
            parent: (id != 0).then_some(n.parent),
            depth,
            label: n.label.clone(),
            a_self: n.a_self,
            a_total: n.a_total,
            b_self: n.b_self,
            b_total: n.b_total,
            child_count: n.children.len() as u32,
        }
    };
    let mut nodes = vec![dto(start, base)];
    let mut stack: Vec<(u32, u32)> = d.nodes[start as usize].children.iter().rev().map(|&c| (c, 1)).collect();
    while let Some((id, rel)) = stack.pop() {
        if nodes.len() >= MAX_NODES {
            break;
        }
        let n = &d.nodes[id as usize];
        if rel > max_depth && !threshold.is_some_and(|t| n.a_total.max(n.b_total) >= t) {
            continue;
        }
        nodes.push(dto(id, base + rel));
        stack.extend(n.children.iter().rev().map(|&c| (c, rel + 1)));
    }
    Ok(CompareResponse { a_total: root.a_total, b_total: root.b_total, unit, nodes })
}

/// Differential flame graph: laid out by `b`, `delta = b_total - a_total`.
pub fn compare_flame(a: &Session, b: &Session, req: &CompareRequest) -> Result<FlameLayout> {
    let (d, _) = diff_tree(a, b, req)?;
    let total = d.nodes[0].b_total;
    let min_w = ((total as f64) * req.min_fraction.unwrap_or(0.0005)).ceil().max(1.0) as u64;
    let mut out = FlameLayout { total, delta: Some(vec![]), ..Default::default() };
    let mut names: std::collections::HashMap<String, u32> = Default::default();
    let mut stack = vec![(0u32, 0u64, 0u32)];
    while let Some((id, x, depth)) = stack.pop() {
        let n = &d.nodes[id as usize];
        if id != 0 && n.b_total < min_w {
            continue;
        }
        let name = *names.entry(n.label.clone()).or_insert_with(|| {
            out.names.push(n.label.clone());
            (out.names.len() - 1) as u32
        });
        out.node.push(id);
        out.key.push(u32::MAX);
        out.x.push(x);
        out.width.push(n.b_total);
        out.depth.push(depth);
        out.name.push(name);
        out.self_weight.push(n.b_self);
        out.kind.push(FrameKind::Unknown);
        out.delta.as_mut().unwrap().push(n.b_total as i64 - n.a_total as i64);
        out.max_depth = out.max_depth.max(depth);
        let mut cx = x;
        let mut kids = vec![];
        for &c in &n.children {
            kids.push((c, cx, depth + 1));
            cx += d.nodes[c as usize].b_total;
        }
        stack.extend(kids.into_iter().rev());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, SessionKind};

    #[test]
    fn demo_queries() {
        let e = Engine::new();
        let s = e.add_profile(SessionKind::Snapshot, "demo", crate::demo::demo_profile());
        let t = tree(&s, &TreeRequest { depth: Some(3), ..TreeRequest::new(SampleKind::Cpu) }).unwrap();
        assert!(t.total > 0);
        assert!(t.nodes.len() > 3);
        assert_eq!(t.nodes[0].label, "All threads");
        let h = hotspots(
            &s,
            &HotSpotsRequest {
                kind: SampleKind::Cpu,
                filter: Default::default(),
                aggregation: Aggregation::Method,
                compact_prefixes: vec![],
                limit: Some(5),
            },
        )
        .unwrap();
        assert_eq!(h.hotspots[0].label, "java.util.regex.Pattern$Start.match");
        let bt = tree(
            &s,
            &TreeRequest {
                view: TreeView::Backtraces,
                node: h.hotspots[0].node,
                depth: Some(2),
                ..TreeRequest::new(SampleKind::Cpu)
            },
        )
        .unwrap();
        assert_eq!(bt.nodes[1].label, "dev.openprofiler.shop.PricingEngine.matchRule");
        let f = flame(&s, &TreeRequest::new(SampleKind::Cpu)).unwrap();
        assert!(f.node.len() > 10);
        let tel = telemetry(&s, &TelemetryRequest { points: Some(20), ..Default::default() }).unwrap();
        assert!(tel.series.iter().all(|s| s.t.len() <= 20));
        assert!(!tel.gc.is_empty());
        let th = threads(&s).unwrap();
        assert_eq!(th.threads.len(), 9);
        let spans = thread_states(&s, &Default::default()).unwrap();
        assert!(!spans.spans.is_empty());
        let cg = callgraph(
            &s,
            &CallGraphRequest {
                kind: SampleKind::Cpu,
                filter: Default::default(),
                aggregation: Aggregation::Method,
                focus_key: Some(h.hotspots[0].key),
                limit: None,
            },
        )
        .unwrap();
        assert!(cg.edges.len() >= 2);
        let s2 = e.add_profile(SessionKind::Snapshot, "demo2", crate::demo::demo_profile());
        let req = CompareRequest {
            a: s.id.clone(),
            b: s2.id.clone(),
            kind: "cpu".into(),
            aggregation: Aggregation::Method,
            filter: Default::default(),
            node: None,
            depth: Some(2),
            min_fraction: None,
        };
        let c = compare(&s, &s2, &req).unwrap();
        assert_eq!(c.a_total, c.b_total);
        let cf = compare_flame(&s, &s2, &req).unwrap();
        assert!(cf.delta.unwrap().iter().all(|&d| d == 0));
        let m = monitors(&s, &MonitorsRequest { view: Some("stats".into()), ..Default::default() }).unwrap();
        assert_eq!(m["stats"][0]["monitor_class"], "dev.openprofiler.shop.ProductCache");
    }
}
