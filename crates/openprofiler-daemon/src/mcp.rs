//! Model Context Protocol server (JSON-RPC 2.0 over newline-delimited stdio).
//!
//! Tools mirror the Query API with compact, LLM-friendly JSON. Responses carry stable ids
//! (`session`, `node`, `object`) and a `suggested_next` hint.

use crate::engine::{Engine, Session};
use crate::query::*;
use anyhow::{Result, anyhow, bail};
use openprofiler_model::SampleKind;
use openprofiler_model::views::Aggregation;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpServer {
    engine: Arc<Engine>,
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required },
    })
}

pub fn tools() -> Vec<Value> {
    let kind = json!({ "type": "string", "enum": ["cpu", "wall", "alloc", "monitor", "wait", "io", "exception"], "default": "cpu" });
    let session = json!({ "type": "string", "description": "Session id from load_snapshot/attach/status" });
    vec![
        tool(
            "list_jvms",
            "List JVMs running on this machine that OpenProfiler can attach to.",
            json!({}),
            &[],
        ),
        tool(
            "prepare_profiling",
            "Explain how to start a JVM with the OpenProfiler agent (returns the -agentpath option to add).",
            json!({ "mode": { "type": "string", "enum": ["live", "offline"], "default": "live" },
                    "output": { "type": "string", "description": "Offline mode: snapshot file to write at exit" } }),
            &[],
        ),
        tool(
            "attach",
            "Attach the OpenProfiler agent to a running local JVM and start a live session.",
            json!({ "pid": { "type": "integer" } }),
            &["pid"],
        ),
        tool("status", "List open sessions with their state and capabilities.", json!({}), &[]),
        tool(
            "load_snapshot",
            "Open a profiling file (.jfr, .hprof, .hprof.gz, .opsnap). Optionally open a baseline for comparison.",
            json!({ "path": { "type": "string" }, "baseline": { "type": "string" } }),
            &["path"],
        ),
        tool(
            "hotspots",
            "Top methods by self time (CPU) or self weight (allocation, monitor, ...). Each result has a node id expandable with `expand` (back traces).",
            json!({ "session": session, "kind": kind, "limit": { "type": "integer", "default": 15 },
                    "aggregation": { "type": "string", "enum": ["method", "class", "package"] },
                    "thread": { "type": "string", "description": "Only threads whose name contains this text" } }),
            &["session"],
        ),
        tool(
            "expand",
            "Expand a node of the call tree (view=calltree) or a hot spot's back traces (view=backtraces). Without node, returns the top of the tree. Children below 1% are omitted.",
            json!({ "session": session, "kind": kind, "view": { "type": "string", "enum": ["calltree", "backtraces"], "default": "calltree" },
                    "node": { "type": "integer" }, "depth": { "type": "integer", "default": 3 } }),
            &["session"],
        ),
        tool(
            "heap_dump",
            "Trigger a heap dump in a live session; returns the new heap session id.",
            json!({ "session": session }),
            &["session"],
        ),
        tool(
            "heap_data",
            "Query a heap session: summary, classes (histogram), biggest (dominator tree, pass object to drill down), refs, path_to_roots, object, inspection.",
            json!({ "session": session,
                    "view": { "type": "string", "enum": ["summary", "classes", "biggest", "refs", "path_to_roots", "object", "inspection"] },
                    "object": { "type": "integer" }, "direction": { "type": "string", "enum": ["incoming", "outgoing"] },
                    "kind": { "type": "string", "description": "Inspection kind, e.g. duplicate_strings" },
                    "limit": { "type": "integer", "default": 20 } }),
            &["session", "view"],
        ),
        tool(
            "insights",
            "Rule-based findings (GC overhead, lock contention, pool starvation, heap waste, ...), most severe first.",
            json!({ "session": session }),
            &["session"],
        ),
        tool(
            "compare",
            "Compare two sessions: the biggest changes in total time/weight per method.",
            json!({ "a": session, "b": session, "kind": kind, "limit": { "type": "integer", "default": 20 } }),
            &["a", "b"],
        ),
        tool(
            "thread_dump",
            "Thread dumps of a session (live sessions capture a new one).",
            json!({ "session": session }),
            &["session"],
        ),
        tool(
            "set_triggers",
            "Replace a live agent's triggers. Each trigger has an id, one condition (timer {delay, period}, threshold {metric, above|below, for}, out_of_memory, jvm_start, jvm_exit) and actions (heap_dump {path}, thread_dump, save_snapshot <path>, bookmark <label>, start_recording/stop_recording cpu|alloc|monitor, run_gc), plus optional cooldown and max_fires.",
            json!({ "session": session, "triggers": { "type": "array", "items": { "type": "object" } } }),
            &["session", "triggers"],
        ),
        tool(
            "probe_events",
            "Probe events (JDBC, HTTP, ...) of a session. Requires a live agent with probes enabled.",
            json!({ "session": session, "probe": { "type": "string" } }),
            &["session"],
        ),
    ]
}

fn pct(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { (1000.0 * a as f64 / b as f64).round() / 10.0 }
}

fn kind_arg(args: &Value) -> Result<SampleKind> {
    let k = args["kind"].as_str().unwrap_or("cpu");
    SampleKind::parse(k).ok_or_else(|| anyhow!("unknown kind {k}"))
}

impl McpServer {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    fn session(&self, args: &Value, key: &str) -> Result<Arc<Session>> {
        let id = args[key].as_str().ok_or_else(|| anyhow!("missing argument `{key}`"))?;
        let s = self.engine.session(id)?;
        self.engine.wait_ready_blocking(&s)?;
        Ok(s)
    }

    /// Handles one JSON-RPC message; returns the response (None for notifications).
    pub async fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or_default().to_owned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Result<Value> = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "openprofiler", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "OpenProfiler is a JVM profiler. Start with load_snapshot (files) or list_jvms + attach (live), then insights, hotspots and expand. Node and object ids are stable within a session.",
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_owned();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let r = self.call(&name, args).await;
                Ok(match r {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                        "structuredContent": v,
                        "isError": false,
                    }),
                    Err(e) => {
                        json!({ "content": [{ "type": "text", "text": format!("Error: {e:#}") }], "isError": true })
                    }
                })
            }
            m if m.starts_with("notifications/") => return None,
            m => Err(anyhow!("method not found: {m}")),
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": e.to_string() } })
            }
        })
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let engine = self.engine.clone();
        match name {
            "list_jvms" => Ok(json!({ "jvms": crate::live::list_jvms()?,
                "suggested_next": "attach with a pid" })),
            "prepare_profiling" => Ok(crate::live::prepare_profiling(&args)),
            "attach" => {
                let pid = args["pid"].as_u64().ok_or_else(|| anyhow!("missing pid"))? as u32;
                let s = crate::live::attach(
                    &engine,
                    crate::live::AttachRequest { pid: Some(pid), options: None, infofile: None },
                )
                .await?;
                Ok(json!({ "session": s.id, "info": s.info(),
                    "suggested_next": "wait a few seconds, then hotspots or insights" }))
            }
            "status" => {
                Ok(json!({ "sessions": engine.sessions().iter().map(|s| s.info()).collect::<Vec<_>>() }))
            }
            "load_snapshot" => {
                let path = args["path"].as_str().ok_or_else(|| anyhow!("missing path"))?.to_owned();
                let baseline = args["baseline"].as_str().map(str::to_owned);
                tokio::task::spawn_blocking(move || {
                    let s = engine.open_blocking(std::path::Path::new(&path))?;
                    let mut out = json!({ "session": s.id, "summary": summary(&s)? });
                    if let Some(b) = baseline {
                        let bs = engine.open_blocking(std::path::Path::new(&b))?;
                        out["baseline_session"] = json!(bs.id);
                        out["suggested_next"] = json!(format!("compare a={} b={}", bs.id, s.id));
                    } else {
                        out["suggested_next"] = json!("insights, then hotspots");
                    }
                    Ok(out)
                })
                .await?
            }
            _ => {
                let name = name.to_owned();
                tokio::task::spawn_blocking(move || McpServer { engine }.call_blocking(&name, &args)).await?
            }
        }
    }

    fn call_blocking(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "hotspots" => {
                let s = self.session(args, "session")?;
                let kind = kind_arg(args)?;
                let mut filter = FilterDto::default();
                if let Some(t) = args["thread"].as_str() {
                    let th = threads(&s)?;
                    filter.threads = Some(
                        th.threads.iter().filter(|x| x.info.name.contains(t)).map(|x| x.index).collect(),
                    );
                }
                let aggregation = match args["aggregation"].as_str() {
                    Some("class") => Aggregation::Class,
                    Some("package") => Aggregation::Package,
                    _ => Aggregation::Method,
                };
                let limit = args["limit"].as_u64().unwrap_or(15) as usize;
                let r = hotspots(
                    &s,
                    &HotSpotsRequest {
                        kind,
                        filter,
                        aggregation,
                        compact_prefixes: vec![],
                        limit: Some(limit),
                    },
                )?;
                Ok(json!({
                    "session": s.id, "kind": kind.name(), "unit": r.unit, "total": r.total,
                    "hotspots": r.hotspots.iter().map(|h| json!({
                        "node": h.node, "method": h.label, "self_percent": pct(h.self_, r.total),
                        "total_percent": pct(h.total, r.total), "self": h.self_, "samples": h.self_count,
                    })).collect::<Vec<_>>(),
                    "suggested_next": "expand view=backtraces node=<node> to see who calls a hot spot",
                }))
            }
            "expand" => {
                let s = self.session(args, "session")?;
                let kind = kind_arg(args)?;
                let view = if args["view"].as_str() == Some("backtraces") {
                    TreeView::Backtraces
                } else {
                    TreeView::Calltree
                };
                let req = TreeRequest {
                    view,
                    node: args["node"].as_u64().map(|n| n as u32),
                    depth: Some(args["depth"].as_u64().unwrap_or(3) as u32),
                    ..TreeRequest::new(kind)
                };
                let r = tree(&s, &req)?;
                let min = r.total / 100;
                let nodes: Vec<Value> = r
                    .nodes
                    .iter()
                    .filter(|n| n.total >= min || n.parent.is_none())
                    .map(|n| {
                        json!({ "node": n.id, "parent": n.parent, "depth": n.depth, "method": n.label,
                            "total_percent": pct(n.total, r.total), "self_percent": pct(n.self_, r.total),
                            "children": n.child_count })
                    })
                    .collect();
                Ok(json!({ "session": s.id, "view": view, "unit": r.unit, "total": r.total, "nodes": nodes,
                    "suggested_next": "expand a deeper node id, or hotspots for a flat view" }))
            }
            "insights" => {
                let s = self.session(args, "session")?;
                Ok(json!({ "session": s.id, "insights": crate::insights::session_insights(&s)? }))
            }
            "compare" => {
                let a = self.session(args, "a")?;
                let b = self.session(args, "b")?;
                let kind = kind_arg(args)?;
                let req = CompareRequest {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    kind: kind.name().into(),
                    aggregation: Aggregation::Method,
                    filter: Default::default(),
                    node: None,
                    depth: None,
                    min_fraction: None,
                };
                let (d, unit) = diff_tree(&a, &b, &req)?;
                let (at, bt) = (d.nodes[0].a_total, d.nodes[0].b_total);
                // Aggregate per label (method) across the tree: total = inherent time.
                let mut per: std::collections::HashMap<&str, (u64, u64)> = Default::default();
                for n in d.nodes.iter().skip(1) {
                    let e = per.entry(&n.label).or_default();
                    e.0 += n.a_self;
                    e.1 += n.b_self;
                }
                let mut rows: Vec<_> = per
                    .into_iter()
                    .map(|(l, (a, b))| (l, pct(a, at), pct(b, bt), pct(b, bt) - pct(a, at)))
                    .collect();
                rows.sort_by(|x, y| y.3.abs().total_cmp(&x.3.abs()));
                rows.truncate(args["limit"].as_u64().unwrap_or(20) as usize);
                Ok(json!({
                    "a": a.id, "b": b.id, "unit": unit, "a_total": at, "b_total": bt,
                    "total_change_percent": if at > 0 { (1000.0 * (bt as f64 - at as f64) / at as f64).round() / 10.0 } else { 0.0 },
                    "biggest_self_time_changes": rows.iter().map(|(l, a, b, d)| json!({
                        "method": l, "a_self_percent": a, "b_self_percent": b, "delta_points": (d * 10.0).round() / 10.0
                    })).collect::<Vec<_>>(),
                }))
            }
            "thread_dump" => {
                let s = self.session(args, "session")?;
                Ok(json!({ "session": s.id, "dumps": thread_dumps(&s)? }))
            }
            "heap_data" => {
                let s = self.session(args, "session")?;
                let view = args["view"].as_str().unwrap_or("summary");
                let limit = args["limit"].as_u64().unwrap_or(20);
                let (endpoint, post, req) = match view {
                    "summary" => ("summary", false, json!({})),
                    "classes" => ("classes", true, json!({ "sort": "retained", "limit": limit })),
                    "biggest" => ("biggest", true, json!({ "object": args.get("object"), "limit": limit })),
                    "refs" => (
                        "refs",
                        true,
                        json!({ "object": args["object"], "direction": args["direction"].as_str().unwrap_or("incoming"), "limit": limit }),
                    ),
                    "path_to_roots" => {
                        ("path-to-roots", true, json!({ "object": args["object"], "max_paths": 3 }))
                    }
                    "object" => {
                        let o = args["object"].as_u64().ok_or_else(|| anyhow!("missing object"))?;
                        return crate::heap::handle(
                            &self.engine,
                            &s,
                            &format!("object/{o}"),
                            false,
                            json!({}),
                        );
                    }
                    "inspection" => (
                        "inspection",
                        true,
                        json!({ "kind": args["kind"].as_str().unwrap_or("duplicate_strings"), "limit": limit }),
                    ),
                    v => bail!("unknown heap view {v}"),
                };
                let mut v = crate::heap::handle(&self.engine, &s, endpoint, post, req)?;
                if let Some(o) = v.as_object_mut() {
                    o.insert("suggested_next".into(), json!("heap_data view=biggest object=<id> to drill into the dominator tree; path_to_roots to see why an object is alive"));
                }
                Ok(v)
            }
            "heap_dump" | "probe_events" | "set_triggers" => {
                let s = self.session(args, "session")?;
                let engine = self.engine.clone();
                let action = match name {
                    "heap_dump" => "heap-dump",
                    "set_triggers" => "triggers",
                    _ => "probe-events",
                };
                tokio::runtime::Handle::current()
                    .block_on(async move { crate::live::control(&engine, &s, action, args.clone()).await })
            }
            other => bail!("unknown tool: {other}"),
        }
    }

    /// Serves MCP on stdin/stdout until EOF.
    pub async fn serve_stdio(self) -> Result<()> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut out = tokio::io::stdout();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Value>(&line) {
                Ok(Value::Array(batch)) => {
                    let mut v = vec![];
                    for m in batch {
                        v.extend(self.handle(m).await);
                    }
                    (!v.is_empty()).then_some(Value::Array(v))
                }
                Ok(m) => self.handle(m).await,
                Err(e) => Some(
                    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } }),
                ),
            };
            if let Some(r) = resp {
                out.write_all(serde_json::to_string(&r)?.as_bytes()).await?;
                out.write_all(b"\n").await?;
                out.flush().await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_flow() {
        let engine = Engine::new();
        let s = engine.add_profile(crate::SessionKind::Snapshot, "demo", crate::demo::demo_profile());
        let srv = McpServer::new(engine);
        let init =
            srv.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).await.unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "openprofiler");
        let list = srv.handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await.unwrap();
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 10);
        let r = srv
            .handle(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"hotspots","arguments":{"session": s.id}}}))
            .await
            .unwrap();
        let hs = &r["result"]["structuredContent"]["hotspots"];
        assert_eq!(hs[0]["method"], "java.util.regex.Pattern$Start.match");
        let node = hs[0]["node"].as_u64().unwrap();
        let r = srv
            .handle(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"expand","arguments":{"session": s.id, "view":"backtraces","node":node}}}))
            .await
            .unwrap();
        assert_eq!(r["result"]["isError"], false, "{r}");
        let r = srv
            .handle(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"insights","arguments":{"session": s.id}}}))
            .await
            .unwrap();
        assert!(!r["result"]["structuredContent"]["insights"].as_array().unwrap().is_empty());
        assert!(srv.handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await.is_none());
    }
}
