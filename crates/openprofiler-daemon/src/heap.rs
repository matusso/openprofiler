//! Heap sessions: HPROF dumps (or snapshots with an embedded dump) analysed by `openprofiler-heap`.

use crate::engine::{Engine, Session, SessionState};
use crate::export::Format;
use crate::insights::{Insight, Severity};
use anyhow::{Context, Result, anyhow, bail};
use openprofiler_heap::{ClassQuery, Direction, HeapAnalysis, ObjectQuery, OpenOptions, Phase, SelectStep};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct HeapHandle {
    pub analysis: Arc<HeapAnalysis>,
    /// The raw HPROF backing the analysis.
    pub path: PathBuf,
}

/// Indexes (or reopens) a heap dump and attaches it to `session`, reporting progress.
pub fn load_hprof(path: &Path, session: &Session) -> Result<()> {
    let phases = [
        Phase::Decompressing,
        Phase::Scanning,
        Phase::Indexing,
        Phase::Graph,
        Phase::Incoming,
        Phase::Dominators,
        Phase::Retained,
        Phase::Finishing,
    ];
    let last = std::sync::Mutex::new((usize::MAX, -1.0f64));
    let analysis = HeapAnalysis::open(path, &OpenOptions::default(), |phase, frac| {
        let i = phases.iter().position(|p| *p == phase).unwrap_or(phases.len() - 1);
        let overall = (i as f64 + frac.clamp(0.0, 1.0)) / phases.len() as f64;
        let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
        // Throttle: report phase changes and every 2%.
        if l.0 != i || overall - l.1 >= 0.02 {
            *l = (i, overall);
            session.set_state(SessionState::Loading, Some(overall), None);
        }
    })
    .with_context(|| format!("analysing {}", path.display()))?;
    session.set_heap(HeapHandle { analysis: Arc::new(analysis), path: path.to_owned() });
    Ok(())
}

pub fn summary_json(h: &HeapHandle) -> Result<Value> {
    Ok(serde_json::to_value(h.analysis.summary())?)
}

pub fn hprof_path(h: &HeapHandle) -> Option<PathBuf> {
    // Gzipped dumps are decompressed by the engine; re-embed the original bytes either way.
    Some(h.path.clone())
}

pub fn insights(h: &HeapHandle) -> Result<Vec<Insight>> {
    Ok(h.analysis
        .inspections()
        .into_iter()
        .map(|i| Insight {
            view: Some(format!("heap.inspection:{}", i.kind)),
            kind: i.kind,
            title: i.title,
            severity: match i.severity {
                openprofiler_heap::Severity::Info => Severity::Info,
                openprofiler_heap::Severity::Warning => Severity::Warning,
                openprofiler_heap::Severity::Critical => Severity::Critical,
            },
            summary: i.summary,
            wasted_bytes: i.wasted_bytes,
            count: i.count,
        })
        .collect())
}

#[derive(Deserialize, Default)]
struct ObjReq {
    #[serde(default)]
    object: Option<u32>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    set: Option<u32>,
    #[serde(default)]
    max_paths: Option<usize>,
    #[serde(default)]
    exclude_weak: Option<bool>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    other_session: Option<String>,
    #[serde(default)]
    from: Option<u32>,
    #[serde(default)]
    step: Option<SelectStep>,
}

fn obj(r: &ObjReq) -> Result<u32> {
    r.object.ok_or_else(|| anyhow!("missing `object`"))
}

/// Dispatches `.../heap/<endpoint>`.
pub fn handle(engine: &Engine, s: &Session, endpoint: &str, post: bool, req: Value) -> Result<Value> {
    let h = s.require_heap()?;
    let a = &h.analysis;
    let r: ObjReq =
        if post { serde_json::from_value(req.clone()).unwrap_or_default() } else { ObjReq::default() };
    let v = match endpoint {
        "summary" => serde_json::to_value(a.summary())?,
        "classes" => serde_json::to_value(a.classes(&serde_json::from_value::<ClassQuery>(req)?)?)?,
        "objects" => serde_json::to_value(a.objects(&serde_json::from_value::<ObjectQuery>(req)?)?)?,
        "biggest" => json!({ "rows": a.biggest(r.object, r.limit.unwrap_or(100))? }),
        "dominator-classes" => json!({ "rows": a.dominator_classes(r.object)? }),
        "refs" => serde_json::to_value(a.refs(
            obj(&r)?,
            r.direction.unwrap_or(Direction::Incoming),
            r.offset.unwrap_or(0),
            r.limit.unwrap_or(100),
        )?)?,
        "merged-refs" => {
            json!({ "rows": a.merged_refs(r.set.unwrap_or(0), r.direction.unwrap_or(Direction::Incoming))? })
        }
        "path-to-roots" => json!({
            "paths": a.paths_to_roots(obj(&r)?, r.max_paths.unwrap_or(5), r.exclude_weak.unwrap_or(true))?
        }),
        "select" => {
            let step = r.step.as_ref().ok_or_else(|| anyhow!("missing `step`"))?;
            serde_json::to_value(a.select(r.from, step)?)?
        }
        "sets" => serde_json::to_value(a.sets())?,
        "set-retained" => json!({ "retained": a.set_retained(r.set.unwrap_or(0))? }),
        "inspections" => json!({ "inspections": a.inspections() }),
        "inspection" => {
            let kind = r.kind.as_deref().ok_or_else(|| anyhow!("missing `kind`"))?;
            serde_json::to_value(a.inspection(kind, r.limit.unwrap_or(100))?)?
        }
        "histogram-diff" => {
            let other_id = r.other_session.as_deref().ok_or_else(|| anyhow!("missing `other_session`"))?;
            let other = engine.session(other_id)?.require_heap()?;
            // Rows describe `other` relative to this session (a = this, b = other).
            json!({ "rows": a.histogram_diff(&other.analysis) })
        }
        e if e.starts_with("object/") => {
            let id: u32 = e["object/".len()..].parse().map_err(|_| anyhow!("bad object id"))?;
            match (r.offset, r.limit) {
                (None, None) => serde_json::to_value(a.object(id)?)?,
                (o, l) => serde_json::to_value(a.object_page(id, o.unwrap_or(0), l.unwrap_or(1000))?)?,
            }
        }
        other => bail!("no such heap endpoint: {other}"),
    };
    Ok(v)
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_owned() }
}

pub fn export(s: &Session, view: &str, f: Format, req: &Value) -> Result<Vec<u8>> {
    let h = s.require_heap()?;
    let a = &h.analysis;
    match (view, f) {
        ("classes", Format::Csv) => {
            let mut q: ClassQuery = serde_json::from_value(req.clone()).unwrap_or_default();
            q.limit = Some(q.limit.unwrap_or(usize::MAX / 2));
            let page = a.classes(&q)?;
            let mut out = String::from("name,instances,shallow,retained\n");
            for row in &page.rows {
                let v = serde_json::to_value(row)?;
                out.push_str(&format!(
                    "{},{},{},{}\n",
                    csv_escape(v["name"].as_str().unwrap_or_default()),
                    v["instances"],
                    v["shallow"],
                    v["retained"].as_u64().map(|r| r.to_string()).unwrap_or_default()
                ));
            }
            Ok(out.into_bytes())
        }
        ("classes", Format::Json) => {
            let q: ClassQuery = serde_json::from_value(req.clone()).unwrap_or_default();
            Ok(serde_json::to_vec_pretty(&a.classes(&q)?)?)
        }
        ("biggest", Format::Csv | Format::Json) => {
            let object = req["object"].as_u64().map(|o| o as u32);
            let rows = a.biggest(object, req["limit"].as_u64().unwrap_or(1000) as usize)?;
            if f == Format::Json {
                return Ok(serde_json::to_vec_pretty(&rows)?);
            }
            let mut out = String::from("id,address,class,shallow,retained\n");
            for r in rows {
                let v = serde_json::to_value(&r)?;
                out.push_str(&format!(
                    "{},{},{},{},{}\n",
                    v["id"],
                    v["address"].as_str().unwrap_or_default(),
                    csv_escape(v["class_name"].as_str().unwrap_or_default()),
                    v["shallow"],
                    v["retained"].as_u64().map(|r| r.to_string()).unwrap_or_default()
                ));
            }
            Ok(out.into_bytes())
        }
        ("inspections", _) => Ok(serde_json::to_vec_pretty(&a.inspections())?),
        (v, f) => bail!("heap view {v} cannot be exported as {f:?}"),
    }
}
