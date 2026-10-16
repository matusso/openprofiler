//! Decompiler sessions: Android apps (`.apk`, `.xapk`, `.apks`, `.aab`), `.dex` files and JVM
//! bytecode (`.jar`, `.war`, `.ear`, `.jmod`, `.class`) turned back into Java source by
//! `openprofiler-dex`, in process. Classes are decompiled on demand and
//! cached, so opening a large app only indexes it.

use crate::engine::Session;
use anyhow::{Result, anyhow, bail};
use openprofiler_dex::{Program, View};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct DecompileHandle {
    pub program: Arc<Program>,
}

/// Indexes `path` and attaches the decompiler to `session`.
pub fn load(path: &Path, session: &Session) -> Result<()> {
    let program = Program::open(path)?;
    session.set_decompiler(DecompileHandle { program: Arc::new(program) });
    Ok(())
}

fn view(req: &Value) -> Result<View> {
    match req["view"].as_str().unwrap_or("java") {
        "java" => Ok(View::Java),
        "smali" => Ok(View::Smali),
        v => bail!("unknown view: {v} (java or smali)"),
    }
}

fn node_arg(req: &Value) -> Result<u32> {
    req["node"].as_u64().map(|n| n as u32).ok_or_else(|| anyhow!("missing node"))
}

/// Query API: `/sessions/{id}/code/{rest}` (docs/spec/query-api.md).
pub fn handle(session: &Session, rest: &str, req: Value) -> Result<Value> {
    let d = session.decompiler().ok_or_else(|| match session.state() {
        crate::engine::SessionState::Loading => anyhow!("session {} is still loading", session.id),
        _ => anyhow!("session {} has no decompiled code", session.id),
    })?;
    let p = &d.program;
    Ok(match rest.trim_matches('/') {
        "summary" => p.summary(),
        "packages" => serde_json::to_value(p.packages(req["parent"].as_str().unwrap_or_default()))?,
        "class" => {
            if req.get("node").is_some_and(|n| !n.is_null()) {
                let (code, focus) = p.locate(node_arg(&req)?)?;
                let mut v = serde_json::to_value(&*code)?;
                v["focus"] = json!(focus);
                v
            } else {
                let name = req["name"].as_str().ok_or_else(|| anyhow!("missing name"))?;
                serde_json::to_value(&*p.class_code(name, view(&req)?)?)?
            }
        }
        "usages" => {
            let limit = req["limit"].as_u64().unwrap_or(500) as usize;
            let (target, usages, truncated) = p.usages(node_arg(&req)?, limit)?;
            json!({ "target": target, "usages": usages, "truncated": truncated })
        }
        "search" => {
            let query = req["query"].as_str().unwrap_or_default().trim();
            if query.is_empty() {
                bail!("empty query");
            }
            let in_code = req["scope"].as_str() == Some("code");
            let limit = req["limit"].as_u64().unwrap_or(200) as usize;
            let budget = Duration::from_millis(req["budget_ms"].as_u64().unwrap_or(20_000));
            serde_json::to_value(p.search(query, in_code, limit, budget))?
        }
        "resources" => json!({ "resources": p.resource_rows() }),
        "resource" => {
            let name = req["name"].as_str().ok_or_else(|| anyhow!("missing name"))?;
            serde_json::to_value(p.resource(name)?)?
        }
        op => bail!("no such code query: {op}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../openprofiler-dex/tests/data/corpus/corpus.dex")
    }

    #[test]
    fn serves_code_queries() {
        let engine = crate::Engine::new();
        let s = engine.open_blocking(&corpus()).unwrap();
        assert_eq!(s.kind, crate::SessionKind::Dex);
        assert!(s.capabilities().contains(&"decompile".to_owned()));

        let summary = handle(&s, "summary", json!({})).unwrap();
        assert!(summary["classes"].as_u64().unwrap() >= 5, "{summary}");
        let tree = handle(&s, "packages", json!({})).unwrap();
        assert_eq!(tree["packages"][0]["full"], "corpus", "{tree}");
        let cls = handle(&s, "class", json!({ "name": "corpus.Control" })).unwrap();
        let code = cls["code"].as_str().unwrap();
        assert!(code.contains("public class Control"), "{code}");
        // A link is [start, length, node, decl]; following a declaration's node finds it again.
        let link = cls["links"].as_array().unwrap().iter().find(|l| l[3] == 1).unwrap().clone();
        let located = handle(&s, "class", json!({ "node": link[2] })).unwrap();
        assert_eq!(located["focus"], link[0]);
        let found = handle(&s, "search", json!({ "query": "classify" })).unwrap();
        let node = found["results"][0]["id"].clone();
        let usages = handle(&s, "usages", json!({ "node": node })).unwrap();
        assert!(usages["usages"].is_array(), "{usages}");
        let smali = handle(&s, "class", json!({ "name": "corpus.Control", "view": "smali" })).unwrap();
        assert!(smali["code"].as_str().unwrap().contains(".method"));
        assert!(handle(&s, "class", json!({ "name": "corpus.Nope" })).is_err());
        assert_eq!(handle(&s, "resources", json!({})).unwrap()["resources"], json!([]));
    }

    #[test]
    fn opens_jvm_bytecode() {
        let jar = corpus().with_file_name("corpus.jar");
        assert_eq!(crate::engine::detect_format(&jar).unwrap(), crate::engine::FileFormat::Archive);
        let engine = crate::Engine::new();
        let s = engine.open_blocking(&jar).unwrap();
        assert_eq!(s.kind, crate::SessionKind::Dex);
        let cls = handle(&s, "class", json!({ "name": "corpus.Shapes" })).unwrap();
        let code = cls["code"].as_str().unwrap();
        assert!(code.contains("return () -> System.out.println(msg + this.registered);"), "{code}");
        let res = handle(&s, "resources", json!({})).unwrap();
        assert!(
            res["resources"].as_array().unwrap().iter().any(|r| r["name"] == "META-INF/MANIFEST.MF"),
            "{res}"
        );

        // A bare class file.
        let dir = std::env::temp_dir().join(format!("op-class-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let class = dir.join("Hello.class");
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52];
        bytes.extend([0, 0]);
        std::fs::write(&class, &bytes).unwrap();
        assert_eq!(crate::engine::detect_format(&class).unwrap(), crate::engine::FileFormat::Class);
        // Mach-O universal binaries share the magic.
        std::fs::write(&class, [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2, 0, 0]).unwrap();
        assert!(crate::engine::detect_format(&class).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
