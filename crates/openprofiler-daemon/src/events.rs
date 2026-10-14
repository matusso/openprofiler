//! Generic event browser (`POST .../events`), currently for JFR sessions: every event type in the
//! recording, with decoded fields.

use crate::engine::{Session, SessionKind};
use anyhow::{Result, bail};
use openprofiler_jfr::{JfrReader, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventsRequest {
    /// Event type name (`jdk.GarbageCollection`); omit to list types only.
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    #[serde(default)]
    pub range: Option<(i64, i64)>,
    /// Case-insensitive substring match over rendered fields and thread name.
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventTypeCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    #[serde(rename = "type")]
    pub type_name: String,
    pub start: i64,
    pub duration: i64,
    pub thread: Option<String>,
    pub top_frame: Option<String>,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventsResponse {
    pub types: Vec<EventTypeCount>,
    pub total_rows: usize,
    pub rows: Vec<EventRow>,
}

fn to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::String(s) => json!(s),
        Value::Timestamp(t) => json!({ "timestamp": t }),
        Value::Duration(d) => json!({ "duration": d }),
        Value::Object { fields, .. } => {
            serde_json::Value::Object(fields.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
        }
        Value::Array(a) => serde_json::Value::Array(a.iter().map(to_json).collect()),
    }
}

pub fn events(s: &Session, req: &EventsRequest) -> Result<EventsResponse> {
    let Some(path) = s.path.clone().filter(|_| s.kind == SessionKind::Jfr) else {
        bail!("the event browser is available for JFR recordings only");
    };
    let types = s.cached("events:types", || {
        let r = JfrReader::open(&path)?;
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for e in r.events()? {
            *counts.entry(e?.type_name).or_default() += 1;
        }
        let mut v: Vec<EventTypeCount> =
            counts.into_iter().map(|(name, count)| EventTypeCount { name, count }).collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        Ok(v)
    })?;
    let Some(ty) = &req.type_name else {
        return Ok(EventsResponse { types: (*types).clone(), total_rows: 0, rows: vec![] });
    };
    let offset = req.offset.unwrap_or(0);
    let limit = req.limit.unwrap_or(200).min(5000);
    let needle = req.search.as_ref().map(|s| s.to_lowercase());
    let r = JfrReader::open(&path)?;
    let mut rows = vec![];
    let mut total = 0usize;
    for e in r.events()? {
        let e = e?;
        if &e.type_name != ty || req.range.is_some_and(|(a, b)| e.start < a || e.start >= b) {
            continue;
        }
        let fields: serde_json::Map<String, serde_json::Value> =
            e.fields.iter().map(|(k, v)| (k.clone(), to_json(v))).collect();
        let thread = e.thread.as_ref().map(|t| t.name.clone());
        if let Some(n) = &needle {
            let hay =
                format!("{} {}", thread.as_deref().unwrap_or(""), serde_json::Value::Object(fields.clone()))
                    .to_lowercase();
            if !hay.contains(n.as_str()) {
                continue;
            }
        }
        total += 1;
        if total <= offset || rows.len() >= limit {
            continue;
        }
        rows.push(EventRow {
            type_name: e.type_name.clone(),
            start: e.start,
            duration: e.duration,
            thread,
            top_frame: e
                .stack
                .as_ref()
                .and_then(|s| s.frames.first())
                .map(|f| format!("{}.{}:{}", f.class, f.method, f.line)),
            fields,
        });
    }
    Ok(EventsResponse { types: (*types).clone(), total_rows: total, rows })
}
