//! Instrumented method analysis views of the Query API (`POST /sessions/{id}/calls`), for live
//! sessions and snapshots.
//!
//! Views: `trace` (the call tracer's ordered calls of one thread, rebuilt from entry and exit
//! events, with nesting depth and durations; `threads` lists the traced threads), `outliers`
//! (call sites, i.e. stacks of instrumented calls, with their slowest call against the mean,
//! sorted by that ratio) and `complexity` (methods configured for complexity analysis; for one
//! method its (size, duration) points and least-squares fits of constant, logarithmic, linear,
//! n log n and quadratic models). Starting the tracer and choosing complexity methods are live
//! actions (`recording` with kind `trace`, `instrumentation` with `complexity`).

use crate::engine::Session;
use anyhow::{Result, anyhow};
use openprofiler_model::{Profile, StackId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CallsRequest {
    /// `trace`, `outliers` or `complexity`.
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    /// Trace: only this thread index.
    #[serde(default)]
    pub thread: Option<u32>,
    /// Trace: skip this many calls (paging).
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Outliers: call sites with fewer calls are left out (default 10).
    #[serde(default)]
    pub min_calls: Option<u64>,
    /// Outliers: methods containing this text (case-insensitive).
    #[serde(default)]
    pub search: Option<String>,
    /// Complexity: the method's frame id (from the method list).
    #[serde(default)]
    pub frame: Option<u32>,
}

fn in_range(range: Option<(i64, i64)>, t: i64) -> bool {
    range.is_none_or(|(a, b)| t >= a && t < b)
}

fn frame_label(p: &Profile, frame: u32) -> String {
    if (frame as usize) >= p.symbols.frames().len() {
        return format!("#{frame}");
    }
    p.symbols.method_label(p.symbols.frame(frame).method)
}

fn thread_name(p: &Profile, i: u32) -> String {
    p.threads.get(i as usize).map(|t| t.name.clone()).unwrap_or_else(|| format!("#{i}"))
}

pub fn calls(session: &Session, req: &CallsRequest) -> Result<Value> {
    let prof = session.require_profile()?;
    let p = prof.read();
    match req.view.as_deref().unwrap_or("outliers") {
        "trace" => Ok(trace(&p, req)),
        "outliers" => Ok(outliers(&p, req)),
        "complexity" => complexity(&p, req),
        v => Err(anyhow!("unknown calls view: {v}")),
    }
}

/// One call rebuilt from the trace.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TracedCall {
    pub thread: u32,
    pub depth: u32,
    pub frame: u32,
    pub method: String,
    pub start: i64,
    /// `None` if the trace ended before the call returned.
    pub end: Option<i64>,
}

/// Rebuilds calls from one thread's entry/exit events (in order). An exit closes the innermost
/// open call of its method and everything above it (frames left by exceptions); exits without an
/// entry (calls that began before the trace) are dropped.
pub fn rebuild(events: &[(i64, u32, bool)]) -> Vec<(u32, u32, i64, Option<i64>)> {
    let mut out: Vec<(u32, u32, i64, Option<i64>)> = vec![];
    // Indexes into `out` of the open calls.
    let mut open: Vec<usize> = vec![];
    for &(t, frame, exit) in events {
        if !exit {
            out.push((open.len() as u32, frame, t, None));
            open.push(out.len() - 1);
            continue;
        }
        let Some(pos) = open.iter().rposition(|&i| out[i].1 == frame) else { continue };
        for &i in &open[pos..] {
            out[i].3 = Some(t);
        }
        open.truncate(pos);
    }
    out
}

fn trace(p: &Profile, req: &CallsRequest) -> Value {
    let mut by_thread: BTreeMap<u32, Vec<(i64, u32, bool)>> = BTreeMap::new();
    for e in p.calls.trace.iter().filter(|e| in_range(req.range, e.time)) {
        by_thread.entry(e.thread).or_default().push((e.time, e.frame, e.exit));
    }
    let threads: Vec<Value> = by_thread
        .iter()
        .map(|(&t, ev)| {
            let first = ev.first().map_or(0, |e| e.0);
            let last = ev.last().map_or(0, |e| e.0);
            json!({ "thread": t, "name": thread_name(p, t), "events": ev.len(), "start": first, "end": last })
        })
        .collect();
    let thread = req.thread.or_else(|| by_thread.iter().max_by_key(|(_, v)| v.len()).map(|(&t, _)| t));
    let rebuilt = thread.and_then(|t| by_thread.get(&t).map(|ev| rebuild(ev))).unwrap_or_default();
    let offset = req.offset.unwrap_or(0);
    let limit = req.limit.unwrap_or(5000);
    let mut labels: HashMap<u32, String> = HashMap::new();
    let calls: Vec<TracedCall> = rebuilt
        .iter()
        .skip(offset)
        .take(limit)
        .map(|&(depth, frame, start, end)| TracedCall {
            thread: thread.unwrap_or(0),
            depth,
            frame,
            method: labels.entry(frame).or_insert_with(|| frame_label(p, frame)).clone(),
            start,
            end,
        })
        .collect();
    json!({
        "events": p.calls.trace.len(),
        "threads": threads,
        "thread": thread,
        "total": rebuilt.len(),
        "offset": offset,
        "calls": calls,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct OutlierDto {
    pub stack: StackId,
    pub method: String,
    /// The calling frame, if any (the method's call site).
    pub caller: Option<String>,
    pub calls: u64,
    pub total: i64,
    pub mean: i64,
    pub max: i64,
    /// `max / mean`.
    pub ratio: f64,
    pub threads: usize,
}

fn outliers(p: &Profile, req: &CallsRequest) -> Value {
    let min_calls = req.min_calls.unwrap_or(10);
    let search = req.search.as_ref().map(|s| s.to_lowercase());
    let mut by: HashMap<StackId, (u64, i64, i64, std::collections::HashSet<u32>)> = HashMap::new();
    for s in p.calls.sites.iter().filter(|s| in_range(req.range, s.time)) {
        let e = by.entry(s.stack).or_default();
        e.0 += s.count as u64;
        e.1 += s.total;
        e.2 = e.2.max(s.max);
        e.3.insert(s.thread);
    }
    let mut frames = vec![];
    let mut rows: Vec<OutlierDto> = by
        .into_iter()
        .filter(|(_, (calls, ..))| *calls >= min_calls.max(1))
        .filter_map(|(stack, (calls, total, max, threads))| {
            p.stacks.frames_root_first(stack, &mut frames);
            let leaf = *frames.last()?;
            let method = frame_label(p, leaf);
            if search.as_ref().is_some_and(|s| !method.to_lowercase().contains(s)) {
                return None;
            }
            let caller = frames.len().checked_sub(2).map(|i| frame_label(p, frames[i]));
            let mean = total / calls as i64;
            let ratio = if mean > 0 { max as f64 / mean as f64 } else { 1.0 };
            Some(OutlierDto { stack, method, caller, calls, total, mean, max, ratio, threads: threads.len() })
        })
        .collect();
    rows.sort_by(|a, b| b.ratio.total_cmp(&a.ratio).then(b.max.cmp(&a.max)));
    let n = rows.len();
    rows.truncate(req.limit.unwrap_or(500));
    json!({ "sites": rows, "total": n, "min_calls": min_calls })
}

/// A least-squares fit `t = a + b·f(n)`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Fit {
    /// `O(1)`, `O(log n)`, `O(n)`, `O(n log n)` or `O(n^2)`.
    pub model: &'static str,
    pub a: f64,
    pub b: f64,
    /// Coefficient of determination.
    pub r2: f64,
}

const MODELS: [(&str, fn(f64) -> f64); 5] = [
    ("O(1)", |_| 0.0),
    ("O(log n)", |n| (n.max(1.0)).ln()),
    ("O(n)", |n| n),
    ("O(n log n)", |n| n * n.max(1.0).ln()),
    ("O(n^2)", |n| n * n),
];

/// The model's growth function, for drawing a fit.
pub fn model_fn(model: &str) -> Option<fn(f64) -> f64> {
    MODELS.iter().find(|(m, _)| *m == model).map(|(_, f)| *f)
}

/// Fits every model to `(size, nanos)` points. Slopes are kept non-negative (a model that would
/// need a falling curve fits as a constant).
pub fn fits(points: &[(f64, f64)]) -> Vec<Fit> {
    let n = points.len() as f64;
    if points.is_empty() {
        return vec![];
    }
    let mean_t = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sst: f64 = points.iter().map(|p| (p.1 - mean_t).powi(2)).sum();
    MODELS
        .iter()
        .map(|&(model, f)| {
            let xs: Vec<f64> = points.iter().map(|p| f(p.0)).collect();
            let mean_x = xs.iter().sum::<f64>() / n;
            let sxx: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
            let sxy: f64 = xs.iter().zip(points).map(|(x, p)| (x - mean_x) * (p.1 - mean_t)).sum();
            let b = if sxx > 0.0 { (sxy / sxx).max(0.0) } else { 0.0 };
            let a = mean_t - b * mean_x;
            let sse: f64 = xs.iter().zip(points).map(|(x, p)| (p.1 - (a + b * x)).powi(2)).sum();
            let r2 = if sst > 0.0 { 1.0 - sse / sst } else { 1.0 };
            Fit { model, a, b, r2 }
        })
        .collect()
}

/// The simplest model whose fit is within 0.01 of the best; `O(1)` when no model explains much
/// of the variance (or the sizes barely vary).
pub fn best_fit(fits: &[Fit], distinct_sizes: usize) -> Option<&Fit> {
    let best = fits.iter().map(|f| f.r2).fold(f64::NEG_INFINITY, f64::max);
    if distinct_sizes < 3 || best < 0.3 {
        return fits.iter().find(|f| f.model == "O(1)");
    }
    fits.iter().find(|f| f.r2 >= best - 0.01)
}

fn complexity(p: &Profile, req: &CallsRequest) -> Result<Value> {
    let mut by: BTreeMap<u32, Vec<(f64, f64)>> = BTreeMap::new();
    for c in p.calls.complexity.iter().filter(|c| in_range(req.range, c.time)) {
        by.entry(c.frame).or_default().push((c.size as f64, c.duration as f64));
    }
    let summarize = |frame: u32, pts: &[(f64, f64)]| {
        let fs = fits(pts);
        let distinct = pts.iter().map(|p| p.0 as u64).collect::<std::collections::HashSet<_>>().len();
        let best = best_fit(&fs, distinct).cloned();
        let max_size = pts.iter().map(|p| p.0).fold(0.0, f64::max);
        (
            json!({
                "frame": frame, "method": frame_label(p, frame), "points": pts.len(), "sizes": distinct,
                "max_size": max_size, "best": best.as_ref().map(|b| b.model),
            }),
            fs,
            best,
        )
    };
    let methods: Vec<Value> = by.iter().map(|(&f, pts)| summarize(f, pts).0).collect();
    let mut out = json!({ "methods": methods });
    let frame = req.frame.or_else(|| by.iter().max_by_key(|(_, v)| v.len()).map(|(&f, _)| f));
    if let Some(frame) = frame {
        let pts = by.get(&frame).ok_or_else(|| anyhow!("no complexity points for frame {frame}"))?;
        let (summary, fs, best) = summarize(frame, pts);
        // Evenly thinned for drawing.
        let limit = req.limit.unwrap_or(2000).max(1);
        let step = pts.len().div_ceil(limit).max(1);
        let shown: Vec<[f64; 2]> = pts.iter().step_by(step).map(|p| [p.0, p.1]).collect();
        out["method"] = json!({ "summary": summary, "fits": fs, "best": best, "points": shown });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilds_nested_calls() {
        // a { b { c (exception) } b' } and an exit of a call that began before the trace.
        let ev = [
            (0, 9, true),
            (1, 1, false),
            (2, 2, false),
            (3, 3, false),
            (5, 2, true),
            (6, 2, false),
            (7, 2, true),
            (8, 1, true),
            (9, 4, false),
        ];
        let r = rebuild(&ev);
        assert_eq!(
            r,
            vec![
                (0, 1, 1, Some(8)),
                (1, 2, 2, Some(5)),
                (2, 3, 3, Some(5)),
                (1, 2, 6, Some(7)),
                (0, 4, 9, None),
            ]
        );
    }

    #[test]
    fn views_on_the_demo_profile() {
        let engine = crate::engine::Engine::new();
        let s = engine.add_profile(crate::SessionKind::Snapshot, "demo", crate::demo::demo_profile());
        let r =
            |view: &str| calls(&s, &CallsRequest { view: Some(view.into()), ..Default::default() }).unwrap();
        let o = r("outliers");
        assert_eq!(o["sites"][0]["method"], "dev.openprofiler.shop.ProductCache.get");
        assert_eq!(o["sites"][0]["caller"], "dev.openprofiler.shop.CatalogController.search");
        assert!(o["sites"][0]["ratio"].as_f64().unwrap() > 10.0);
        let t = r("trace");
        assert_eq!(t["total"], 200);
        assert_eq!(t["calls"][0]["depth"], 0);
        assert_eq!(t["calls"][1]["depth"], 1);
        // The outer call ends after its last child.
        assert!(t["calls"][0]["end"].as_i64().unwrap() > t["calls"][4]["end"].as_i64().unwrap());
        let c = r("complexity");
        let best = |name: &str| {
            c["methods"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["method"].as_str().unwrap().ends_with(name))
                .unwrap()["best"]
                .clone()
        };
        assert_eq!(best("PriceCalculator.total"), "O(n)");
        assert_eq!(best("SearchRanker.rank"), "O(n^2)");
        assert_eq!(c["method"]["fits"].as_array().unwrap().len(), 5);
    }

    fn points(f: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
        let mut seed = 7u64;
        (1..200)
            .map(|i| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let noise = 1.0 + ((seed >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 0.1;
                let n = (i * 50) as f64;
                (n, f(n) * noise + 1000.0)
            })
            .collect()
    }

    fn best(pts: &[(f64, f64)]) -> &'static str {
        let fs = fits(pts);
        best_fit(&fs, pts.len()).unwrap().model
    }

    #[test]
    fn picks_the_growth_model() {
        assert_eq!(best(&points(|n| 3.0 * n)), "O(n)");
        assert_eq!(best(&points(|n| 0.01 * n * n)), "O(n^2)");
        assert_eq!(best(&points(|_| 50_000.0)), "O(1)");
        assert_eq!(best(&points(|n| 2000.0 * n.ln())), "O(log n)");
        // Too few distinct sizes to say anything.
        assert_eq!(best_fit(&fits(&[(1.0, 5.0), (2.0, 9.0)]), 2).unwrap().model, "O(1)");
    }
}
