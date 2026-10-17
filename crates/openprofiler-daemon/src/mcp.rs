//! Model Context Protocol server (JSON-RPC 2.0 over newline-delimited stdio).
//!
//! Tools mirror the Query API with compact, LLM-friendly JSON. Responses carry stable ids
//! (`session`, `node`, `object`) and a `suggested_next` hint.

use crate::engine::{Engine, Session};
use crate::query::*;
use anyhow::{Context, Result, anyhow, bail};
use openprofiler_model::SampleKind;
use openprofiler_model::views::Aggregation;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpServer {
    engine: Arc<Engine>,
}

/// A remote target (docs/spec/query-api.md, `RemoteTarget`): a chain of ssh, docker and kubectl hops.
fn remote_target_schema() -> Value {
    json!({
        "type": "object",
        "description": "Remote target, e.g. {\"hops\":[{\"type\":\"kubectl\",\"namespace\":\"prod\",\"pod\":\"api-0\",\"debug_image\":\"busybox:1.37\"}]}. Hop types: ssh {host, user?, port?, jump?}, docker {container, user?}, kubectl {context?, namespace?, pod, container?, debug_image?}. debug_image runs an ephemeral debug container (distroless images). Omit for this machine.",
        "properties": { "hops": { "type": "array", "items": { "type": "object" } } },
    })
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required },
    })
}

pub fn tools() -> Vec<Value> {
    let kind = json!({ "type": "string", "enum": ["cpu", "wall", "alloc", "monitor", "wait", "io", "exception", "calls", "probe"], "default": "cpu" });
    let session = json!({ "type": "string", "description": "Session id from load_snapshot/attach/status" });
    let mbean_server = json!({ "type": "string", "description": "MBean server id from list_mbean_servers; omit for the platform server" });
    vec![
        tool(
            "list_jvms",
            "List JVMs that OpenProfiler can attach to, on this machine or on a remote target.",
            json!({ "target": remote_target_schema() }),
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
            "Attach the OpenProfiler agent to a running JVM, on this machine or on a remote target, and start a live session.",
            json!({ "pid": { "type": "integer" }, "target": remote_target_schema() }),
            &["pid"],
        ),
        tool("status", "List open sessions with their state and capabilities.", json!({}), &[]),
        tool(
            "load_snapshot",
            "Open a profiling file (.jfr, .hprof, .hprof.gz, .phd, .opsnap), or code for the decompiler: an Android app (.apk, .dex, .xapk, .apks, .aab) or JVM bytecode (.jar, .war, .ear, .jmod, .class). Optionally open a baseline for comparison.",
            json!({ "path": { "type": "string" }, "baseline": { "type": "string" } }),
            &["path"],
        ),
        tool(
            "hotspots",
            "Top methods by self time (CPU) or self weight (allocation, monitor, ...). Each result has a node id expandable with `expand` (back traces).",
            json!({ "session": session, "kind": kind, "limit": { "type": "integer", "default": 15 },
                    "aggregation": { "type": "string", "enum": ["method", "class", "package"] },
                    "thread": { "type": "string", "description": "Only threads whose name contains this text" },
                    "payload": { "type": "string", "description": "kind=probe: only calls with this probe payload (an SQL statement, a URL)" } }),
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
            "Trigger a heap dump in a live session; returns the new heap session id. dumper=native walks the heap in the agent (JVMTI) instead of HotSpot's dumpHeap; recorded_only dumps only the objects recorded by allocation recording and their paths from the GC roots.",
            json!({ "session": session,
                    "dumper": { "type": "string", "enum": ["hotspot", "native"], "default": "hotspot" },
                    "recorded_only": { "type": "boolean", "default": false } }),
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
            "Replace a live agent's triggers. Each trigger has an id, one condition (timer {delay, period}, threshold {metric, above|below, for}, out_of_memory, jvm_start, jvm_exit, method {class, method}) and actions (heap_dump {path, dumper: hotspot|native, recorded_only}, thread_dump, save_snapshot <path>, bookmark <label>, start_recording/stop_recording cpu|alloc|monitor|calls|probes|trace, run_gc), plus optional cooldown and max_fires.",
            json!({ "session": session, "triggers": { "type": "array", "items": { "type": "object" } } }),
            &["session", "triggers"],
        ),
        tool(
            "list_mbean_servers",
            "MBean servers reachable through a live JVM: the platform server (id \"\"), other MBean servers in the JVM, and remote JMX servers connected with connect_jmx. Pass the id as `server` to the other MBean tools.",
            json!({ "session": session }),
            &["session"],
        ),
        tool(
            "connect_jmx",
            "Connect a live JVM to a remote JMX server (from inside that JVM), or disconnect it with enabled=false. The service URL becomes the server id.",
            json!({ "session": session, "url": { "type": "string", "description": "e.g. service:jmx:rmi:///jndi/rmi://host:9010/jmxrmi" },
                    "username": { "type": "string" }, "password": { "type": "string" },
                    "enabled": { "type": "boolean", "default": true } }),
            &["session", "url"],
        ),
        tool(
            "list_mbeans",
            "List the MBeans of a live JVM (platform MBean server unless `server` is given), optionally filtered by an ObjectName pattern such as java.lang:* or *:type=GarbageCollector,*. Also returns the attributes sampled as telemetry.",
            json!({ "session": session, "server": mbean_server, "pattern": { "type": "string" } }),
            &["session"],
        ),
        tool(
            "get_mbean",
            "Attributes (with current values), operations and declared notifications of one MBean in a live JVM.",
            json!({ "session": session, "server": mbean_server, "name": { "type": "string", "description": "ObjectName, e.g. java.lang:type=Memory" } }),
            &["session", "name"],
        ),
        tool(
            "invoke_mbean",
            "Invoke an MBean operation in a live JVM. signature lists the parameter types (from get_mbean), args the values as strings.",
            json!({ "session": session, "server": mbean_server, "name": { "type": "string" }, "operation": { "type": "string" },
                    "signature": { "type": "array", "items": { "type": "string" } },
                    "args": { "type": "array", "items": { "type": "string" } } }),
            &["session", "name", "operation"],
        ),
        tool(
            "track_mbean",
            "Sample a numeric MBean attribute (or an item of a composite attribute, e.g. HeapMemoryUsage.used) once per second as a telemetry series; enabled=false stops it. The series key can be used in telemetry queries and threshold triggers.",
            json!({ "session": session, "server": mbean_server, "name": { "type": "string" }, "attribute": { "type": "string" },
                    "item": { "type": "string" }, "label": { "type": "string" },
                    "unit": { "type": "string", "enum": ["count", "bytes", "nanos", "percent", "per_second"] },
                    "enabled": { "type": "boolean", "default": true } }),
            &["session", "name", "attribute"],
        ),
        tool(
            "subscribe_mbean",
            "Listen to the notifications of one MBean in a live JVM (e.g. java.lang:type=GarbageCollector,name=G1 Young Generation); enabled=false stops. Read them with mbean_notifications.",
            json!({ "session": session, "server": mbean_server, "name": { "type": "string" },
                    "enabled": { "type": "boolean", "default": true } }),
            &["session", "name"],
        ),
        tool(
            "mbean_notifications",
            "Notifications received from subscribed MBeans (the agent keeps the last 1000), oldest first, with sequence numbers above `after`. Returns `last` to pass as the next `after`.",
            json!({ "session": session, "after": { "type": "integer", "default": 0 } }),
            &["session"],
        ),
        tool(
            "decompile",
            "Decompiled Java source (or the low-level view: smali for Android code, JVM bytecode for class files) of a class in a decompiler session (.apk/.dex/.jar/.class). Without `class`, lists the packages and classes directly under `package` (default: top level).",
            json!({ "session": session, "class": { "type": "string", "description": "Fully qualified class name" },
                    "package": { "type": "string" },
                    "view": { "type": "string", "enum": ["java", "smali"], "default": "java" },
                    "max_chars": { "type": "integer", "default": 60000 } }),
            &["session"],
        ),
        tool(
            "search_code",
            "Search a decompiler session. scope=names matches class, method and field names (fast); scope=code searches decompiled source text, e.g. for strings, URLs or API keys (slower, time-bounded). Name results carry a node id for `find_usages`.",
            json!({ "session": session, "query": { "type": "string" },
                    "scope": { "type": "string", "enum": ["names", "code"], "default": "names" },
                    "limit": { "type": "integer", "default": 50 } }),
            &["session", "query"],
        ),
        tool(
            "find_usages",
            "Places in the decompiled code that use a class, method or field (node id from search_code).",
            json!({ "session": session, "node": { "type": "integer" }, "limit": { "type": "integer", "default": 100 } }),
            &["session", "node"],
        ),
        tool(
            "probes",
            "Probe data (JDBC SQL, HTTP server/client URLs, JPA, Kafka, gRPC, exceptions, files, sockets, custom probes). view=list (probes with totals; start here), hotspots (payloads by total time for one probe), events (individual calls with detail, error and request context), controls (connections/files/sockets with leak flags), timeline, telemetry. For the code paths behind a payload use hotspots with kind=probe and payload=<payload>.",
            json!({ "session": session, "probe": { "type": "string", "description": "Probe id from view=list, e.g. jdbc" },
                    "view": { "type": "string", "enum": ["list", "hotspots", "events", "controls", "timeline", "telemetry"], "default": "list" },
                    "search": { "type": "string", "description": "Only payloads containing this text" },
                    "errors_only": { "type": "boolean" },
                    "limit": { "type": "integer", "default": 50 } }),
            &["session"],
        ),
        tool(
            "instrumentation",
            "Read or change a live agent's bytecode instrumentation: include/exclude class patterns (e.g. com.acme.*) get exact call counts (kind=calls trees), probes lists the enabled probe ids (empty = all, [\"none\"] = none), complexity lists methods for complexity analysis (`com.acme.Sorter.sort#0`: class.method, optional #argument index or #this; by default the first argument with a size). Without arguments returns the current settings.",
            json!({ "session": session,
                    "include": { "type": "array", "items": { "type": "string" } },
                    "exclude": { "type": "array", "items": { "type": "string" } },
                    "probes": { "type": "array", "items": { "type": "string" } },
                    "complexity": { "type": "array", "items": { "type": "string" } } }),
            &["session"],
        ),
        tool(
            "call_trace",
            "Start (or with enabled=false stop) a live agent's call tracer: an ordered trace of every call of instrumented methods (the include patterns of instrumentation) per thread, for duration_ms (default 10000) or max_events (default 1,000,000). Read it with call_analysis view=trace.",
            json!({ "session": session, "enabled": { "type": "boolean", "default": true },
                    "duration_ms": { "type": "integer" }, "max_events": { "type": "integer" } }),
            &["session"],
        ),
        tool(
            "call_analysis",
            "Instrumented method analysis. view=outliers (call sites of instrumented methods whose slowest call is far above their mean, sorted by max/mean), trace (the call tracer's calls of one thread in order, with depth and durations; `threads` lists traced threads), complexity (methods configured for complexity analysis with their best-fitting growth model; with frame, the (size, nanos) points and all fits).",
            json!({ "session": session,
                    "view": { "type": "string", "enum": ["outliers", "trace", "complexity"], "default": "outliers" },
                    "thread": { "type": "integer", "description": "trace: thread index from `threads`" },
                    "frame": { "type": "integer", "description": "complexity: frame id from `methods`" },
                    "search": { "type": "string", "description": "outliers: methods containing this text" },
                    "min_calls": { "type": "integer", "default": 10 },
                    "offset": { "type": "integer" },
                    "limit": { "type": "integer", "default": 50 } }),
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
                "instructions": "OpenProfiler is a JVM profiler. Start with load_snapshot (files) or list_jvms + attach (live), then insights, hotspots and expand. Node and object ids are stable within a session. Android apps (.apk, .dex) and JVM bytecode (.jar, .class) opened with load_snapshot are browsed with decompile, search_code and find_usages.",
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
            "list_jvms" => {
                let jvms = match args.get("target").filter(|t| !t.is_null()) {
                    Some(t) => crate::live::remote_discover(json!({ "target": t, "what": "jvms" })).await?,
                    None => serde_json::to_value(crate::live::list_jvms()?)?,
                };
                Ok(json!({ "jvms": jvms, "suggested_next": "attach with a pid (and the same target)" }))
            }
            "prepare_profiling" => Ok(crate::live::prepare_profiling(&args)),
            "attach" => {
                let pid = args["pid"].as_u64().ok_or_else(|| anyhow!("missing pid"))? as u32;
                let options = match args.get("target").filter(|t| !t.is_null()) {
                    Some(t) => Some(crate::live::AttachOptions {
                        remote: Some(serde_json::from_value(t.clone()).context("invalid target")?),
                        ..Default::default()
                    }),
                    None => None,
                };
                let s = crate::live::attach(
                    &engine,
                    crate::live::AttachRequest { pid: Some(pid), options, infofile: None },
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
                    } else if s.decompiler().is_some() {
                        out["code"] = crate::decompile::handle(&s, "summary", json!({}))?;
                        out["suggested_next"] =
                            json!("decompile (lists packages), search_code, then decompile class=...");
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
                if let Some(payload) = args["payload"].as_str() {
                    filter.classes = Some(vec![payload.to_string()]);
                }
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
            "decompile" => {
                let s = self.session(args, "session")?;
                let Some(class) = args["class"].as_str() else {
                    let parent = args["package"].as_str().unwrap_or_default();
                    let mut v = crate::decompile::handle(&s, "packages", json!({ "parent": parent }))?;
                    v["suggested_next"] = json!("decompile with package=<full> to descend, or class=<full>");
                    return Ok(v);
                };
                let view = args["view"].as_str().unwrap_or("java");
                let v = crate::decompile::handle(&s, "class", json!({ "name": class, "view": view }))?;
                let max = args["max_chars"].as_u64().unwrap_or(60_000) as usize;
                let code = v["code"].as_str().unwrap_or_default();
                let cut = code.char_indices().nth(max).map(|(i, _)| i);
                Ok(json!({
                    "class": v["name"],
                    "view": view,
                    "code": &code[..cut.unwrap_or(code.len())],
                    "truncated": cut.is_some(),
                    "suggested_next": "search_code for names or strings, find_usages for a node id",
                }))
            }
            "search_code" => {
                let s = self.session(args, "session")?;
                let req = json!({
                    "query": args["query"],
                    "scope": args["scope"].as_str().unwrap_or("names"),
                    "limit": args["limit"].as_u64().unwrap_or(50),
                });
                crate::decompile::handle(&s, "search", req)
            }
            "find_usages" => {
                let s = self.session(args, "session")?;
                let req = json!({ "node": args["node"], "limit": args["limit"].as_u64().unwrap_or(100) });
                crate::decompile::handle(&s, "usages", req)
            }
            "probes" => {
                let s = self.session(args, "session")?;
                let mut req: crate::probes::ProbesRequest = serde_json::from_value(args.clone())?;
                req.limit = req.limit.or(Some(50));
                crate::probes::probes(&s, &req)
            }
            "call_analysis" => {
                let s = self.session(args, "session")?;
                let mut req: crate::calls::CallsRequest = serde_json::from_value(args.clone())?;
                req.limit = req.limit.or(Some(50));
                crate::calls::calls(&s, &req)
            }
            "call_trace" => {
                let s = self.session(args, "session")?;
                let engine = self.engine.clone();
                let mut req = args.clone();
                req["kind"] = json!("trace");
                req["enabled"] = json!(args["enabled"].as_bool().unwrap_or(true));
                tokio::runtime::Handle::current()
                    .block_on(async move { crate::live::control(&engine, &s, "recording", req).await })
            }
            "heap_dump"
            | "instrumentation"
            | "set_triggers"
            | "list_mbeans"
            | "get_mbean"
            | "invoke_mbean"
            | "track_mbean"
            | "list_mbean_servers"
            | "connect_jmx"
            | "subscribe_mbean"
            | "mbean_notifications" => {
                let s = self.session(args, "session")?;
                let engine = self.engine.clone();
                let action = match name {
                    "heap_dump" => "heap-dump",
                    "set_triggers" => "triggers",
                    "list_mbeans" => "mbeans",
                    "get_mbean" => "mbean",
                    "invoke_mbean" => "mbean-invoke",
                    "track_mbean" => "mbean-track",
                    "list_mbean_servers" => "mbean-servers",
                    "connect_jmx" => "mbean-connect",
                    "subscribe_mbean" => "mbean-subscribe",
                    "mbean_notifications" => "mbean-notifications",
                    _ => "instrumentation",
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
