//! IDE bridge (plan D.12): JSON-RPC 2.0 over the `/api/v1/ide` WebSocket.
//!
//! IDE → daemon requests:
//! - `hello {ide, version}` → `{daemon, api}`
//! - `sessions {}` → session list
//! - `openFile {path}` → session (e.g. a snapshot the IDE just produced)
//! - `gutterData {session?, classes: [..]}` → per-line CPU self/total percentages for the given
//!   top-level classes (nested classes included)
//! - `profiledPackages {session?}` → packages with samples, by weight
//! - `agent {}` → `{path, options}`: the native agent and the options to launch a JVM with
//!   (`-agentpath:<path>=<options>,infofile=<file>`)
//! - `attachLaunched {infofile}` → session, once the launched JVM's agent publishes `infofile`
//!
//! Daemon → IDE notifications:
//! - `navigate {class, method?, line?}` (sent when the UI asks to "go to source")

use crate::engine::{Engine, Session};
use anyhow::{Result, anyhow};
use openprofiler_model::{SampleKind, Unit};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize)]
pub struct LineHeat {
    pub class: String,
    pub method: String,
    pub line: i32,
    pub self_percent: f64,
    pub total_percent: f64,
}

fn pick_session(engine: &Engine, params: &Value) -> Result<Arc<Session>> {
    if let Some(id) = params["session"].as_str() {
        return engine.session(id);
    }
    engine
        .sessions()
        .into_iter()
        .rev()
        .find(|s| s.profile().is_some())
        .ok_or_else(|| anyhow!("no profiling session is open"))
}

pub fn gutter_data(s: &Session, classes: &[String], kind: SampleKind) -> Result<Vec<LineHeat>> {
    let prof = s.require_profile()?;
    let p = prof.read();
    let Some(set) = p.samples.get(&kind) else { return Ok(vec![]) };
    let total: u64 = set.total_weight().max(1);
    let wanted = |c: &str| {
        classes.iter().any(|w| c == w || c.strip_prefix(w.as_str()).is_some_and(|r| r.starts_with('$')))
    };
    let mut per_stack: HashMap<u32, u64> = HashMap::new();
    for i in 0..set.len() {
        *per_stack.entry(set.stack[i]).or_default() += set.weight[i];
    }
    let mut acc: HashMap<u32, (u64, u64)> = HashMap::new(); // frame id -> (self, total)
    let mut frames = vec![];
    let mut seen = vec![];
    for (stack, w) in per_stack {
        p.stacks.frames_root_first(stack, &mut frames);
        seen.clear();
        for (i, &f) in frames.iter().enumerate() {
            let fr = p.symbols.frame(f);
            if fr.line < 0 || !wanted(p.symbols.str(p.symbols.method(fr.method).class)) {
                continue;
            }
            let e = acc.entry(f).or_default();
            if i + 1 == frames.len() {
                e.0 += w;
            }
            if !seen.contains(&f) {
                seen.push(f);
                e.1 += w;
            }
        }
    }
    // Merge frames that differ only by kind (JIT vs interpreted) on the same line.
    let mut lines: HashMap<(String, String, i32), (u64, u64)> = HashMap::new();
    for (f, (sw, tw)) in acc {
        let fr = p.symbols.frame(f);
        let m = p.symbols.method(fr.method);
        let e = lines
            .entry((p.symbols.str(m.class).to_owned(), p.symbols.str(m.name).to_owned(), fr.line))
            .or_default();
        e.0 += sw;
        e.1 += tw;
    }
    let pct = |w: u64| (10000.0 * w as f64 / total as f64).round() / 100.0;
    let mut out: Vec<LineHeat> = lines
        .into_iter()
        .map(|((class, method, line), (sw, tw))| LineHeat {
            class,
            method,
            line,
            self_percent: pct(sw),
            total_percent: pct(tw),
        })
        .collect();
    out.sort_by(|a, b| (&a.class, a.line).cmp(&(&b.class, b.line)));
    Ok(out)
}

pub fn profiled_packages(s: &Session) -> Result<Vec<(String, u64)>> {
    let prof = s.require_profile()?;
    let p = prof.read();
    let mut acc: HashMap<String, u64> = HashMap::new();
    if let Some(set) = p.samples.get(&SampleKind::Cpu) {
        for i in 0..set.len() {
            let f = p.stacks.frame(set.stack[i]);
            if f == u32::MAX {
                continue;
            }
            let class = p.symbols.method(p.symbols.frame(f).method).class;
            *acc.entry(p.symbols.package_name(class).to_owned()).or_default() += set.weight[i];
        }
    }
    let mut v: Vec<_> = acc.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(v)
}

/// Handles one JSON-RPC request from an IDE, including the ones that wait on a JVM.
pub async fn handle_async(engine: Arc<Engine>, msg: Value) -> Option<Value> {
    if msg["method"] != "attachLaunched" {
        return tokio::task::spawn_blocking(move || handle(&engine, &msg)).await.ok().flatten();
    }
    let id = msg.get("id").cloned()?;
    let r = async {
        let infofile = msg["params"]["infofile"].as_str().ok_or_else(|| anyhow!("missing infofile"))?;
        let req = crate::live::AttachRequest { pid: None, options: None, infofile: Some(infofile.into()) };
        Ok::<_, anyhow::Error>(serde_json::to_value(crate::live::attach(&engine, req).await?.info())?)
    }
    .await;
    Some(rpc_response(id, r))
}

fn rpc_response(id: Value, r: Result<Value>) -> Value {
    match r {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(e) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": format!("{e:#}") } })
        }
    }
}

/// Handles one JSON-RPC request from an IDE.
pub fn handle(engine: &Arc<Engine>, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let r: Result<Value> = (|| match msg["method"].as_str().unwrap_or_default() {
        "hello" => Ok(json!({ "daemon": "openprofiler", "version": env!("CARGO_PKG_VERSION"), "api": 1 })),
        "sessions" => Ok(json!(engine.sessions().iter().map(|s| s.info()).collect::<Vec<_>>())),
        "openFile" => {
            let path = params["path"].as_str().ok_or_else(|| anyhow!("missing path"))?;
            Ok(serde_json::to_value(engine.open(std::path::Path::new(path))?.info())?)
        }
        "gutterData" => {
            let s = pick_session(engine, &params)?;
            let classes: Vec<String> = serde_json::from_value(params["classes"].clone()).unwrap_or_default();
            let kind = params["kind"].as_str().and_then(SampleKind::parse).unwrap_or(SampleKind::Cpu);
            Ok(json!({ "session": s.id, "unit": Unit::Percent, "lines": gutter_data(&s, &classes, kind)? }))
        }
        "agent" => Ok(json!({
            "path": crate::live::find_agent()?,
            "options": crate::live::launch_options(),
        })),
        "profiledPackages" => {
            let s = pick_session(engine, &params)?;
            Ok(json!({ "session": s.id, "packages": profiled_packages(&s)? }))
        }
        m => Err(anyhow!("method not found: {m}")),
    })();
    Some(rpc_response(id, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gutter_and_packages() {
        let e = Engine::new();
        let s = e.add_profile(crate::SessionKind::Snapshot, "demo", crate::demo::demo_profile());
        let lines =
            gutter_data(&s, &["dev.openprofiler.shop.PricingEngine".into()], SampleKind::Cpu).unwrap();
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| l.class == "dev.openprofiler.shop.PricingEngine"));
        assert!(lines.iter().any(|l| l.total_percent > 10.0));
        let pk = profiled_packages(&s).unwrap();
        assert_eq!(pk[0].0, "java.util.regex");
        let r = handle(&e, &json!({"jsonrpc":"2.0","id":1,"method":"gutterData","params":{"classes":["dev.openprofiler.shop.OrderService"]}}))
            .unwrap();
        assert!(r["result"]["lines"].as_array().is_some(), "{r}");
    }
}
