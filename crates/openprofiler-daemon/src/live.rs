//! Live sessions: attach to (or connect to) a JVM agent and keep its profile up to date.

use crate::engine::{Engine, Session, SessionKind, SessionState};
use anyhow::{Context, Result, anyhow, bail};
use openprofiler_link::{AgentInfo, LinkClient, LinkOptions, LinkState};
use openprofiler_remote::{Target, Tunnel};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Inner {
    client: LinkClient,
    pid: Option<u32>,
    remote: bool,
    _tunnel: Option<Tunnel>,
    watcher: tokio::task::JoinHandle<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

#[derive(Clone)]
pub struct LiveHandle(Arc<Inner>);

impl LiveHandle {
    pub fn client(&self) -> &LinkClient {
        &self.0.client
    }

    pub fn close(&self) {
        let c = self.0.client.clone();
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move { c.close().await });
        }
    }
}

/// Explicit agent endpoint (e.g. reached through a tunnel or a published info file).
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectSpec {
    pub addr: String,
    pub token: String,
    #[serde(default)]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AttachOptions {
    /// TLS 1.3 with a pinned self-signed certificate (default true for attach).
    #[serde(default)]
    pub tls: Option<bool>,
    /// CPU sampling interval, e.g. `"10ms"`.
    #[serde(default)]
    pub interval: Option<String>,
    /// Extra raw agent options (`key=value,...`).
    #[serde(default)]
    pub agent_options: Option<String>,
    #[serde(default)]
    pub agent_path: Option<PathBuf>,
    /// Connect to an already running agent instead of attaching.
    #[serde(default)]
    pub connect: Option<ConnectSpec>,
    /// Attach on a remote host / container through `openprofiler-relay`.
    #[serde(default)]
    pub remote: Option<Target>,
    /// Local bundle directory with `<triple>/{openprofiler-relay, libopenprofiler_agent.so}` for remote attach.
    #[serde(default)]
    pub bundle_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AttachRequest {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub options: Option<AttachOptions>,
    /// Connect to an agent that was started with `infofile=<path>` (e.g. by an IDE launching a JVM
    /// with `-agentpath`), waiting until the agent publishes the file.
    #[serde(default)]
    pub infofile: Option<PathBuf>,
}

fn lib_name() -> &'static str {
    if cfg!(windows) {
        "openprofiler_agent.dll"
    } else if cfg!(target_os = "macos") {
        "libopenprofiler_agent.dylib"
    } else {
        "libopenprofiler_agent.so"
    }
}

/// Locates the native agent: `$OPENPROFILER_AGENT_PATH`, next to the running executable, `../lib/`,
/// or the cargo target directory (development).
pub fn find_agent() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("OPENPROFILER_AGENT_PATH") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe.parent().unwrap_or(Path::new("."));
    let mut candidates = vec![dir.join(lib_name()), dir.join("../lib").join(lib_name())];
    for profile in ["release", "debug"] {
        candidates.push(dir.join("..").join(profile).join(lib_name()));
        candidates.push(PathBuf::from("target").join(profile).join(lib_name()));
    }
    candidates.into_iter().find(|p| p.exists()).map(|p| p.canonicalize().unwrap_or(p)).ok_or_else(|| {
        anyhow!("cannot find {} (set OPENPROFILER_AGENT_PATH or build openprofiler-agent)", lib_name())
    })
}

/// How long to wait for a freshly launched JVM to publish its agent info file.
const INFOFILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Polls `path` until the agent has published it (it writes a temp file and renames it).
async fn wait_for_info(path: &Path, timeout: std::time::Duration) -> Result<AgentInfo> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(info) = AgentInfo::read(path) {
            return Ok(info);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the agent did not publish {} within {}s", path.display(), timeout.as_secs());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Agent options for a JVM launched with `-agentpath` by an IDE, before its `infofile=`.
pub fn launch_options() -> &'static str {
    "tls"
}

pub fn bundle_dir(opts: &AttachOptions) -> PathBuf {
    opts.bundle_dir
        .clone()
        .or_else(|| std::env::var_os("OPENPROFILER_BUNDLE_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::current_exe()
                .and_then(|e| e.canonicalize())
                .ok()
                .and_then(|e| e.parent().map(|d| d.join("bundle")))
                .unwrap_or_else(|| PathBuf::from("dist/bundle"))
        })
}

fn agent_option_string(opts: &AttachOptions) -> String {
    let mut parts = vec![];
    if opts.tls.unwrap_or(true) {
        parts.push("tls".to_owned());
    }
    if let Some(i) = &opts.interval {
        parts.push(format!("interval={i}"));
    }
    if let Some(extra) = &opts.agent_options {
        parts.push(extra.clone());
    }
    parts.join(",")
}

pub fn list_jvms() -> Result<Vec<openprofiler_attach::LocalJvm>> {
    Ok(openprofiler_attach::list_jvms())
}

/// Attaches to a local or remote JVM (or connects to a running agent) and registers a live
/// session.
pub async fn attach(engine: &Arc<Engine>, req: AttachRequest) -> Result<Arc<Session>> {
    let opts = req.options.clone().unwrap_or_default();
    let mut tunnel = None;
    let mut remote = false;
    let link_opts = if let Some(c) = &opts.connect {
        LinkOptions {
            fingerprint: c.fingerprint.clone(),
            ..LinkOptions::new(c.addr.clone(), c.token.clone())
        }
    } else if let Some(path) = &req.infofile {
        LinkOptions::from_info(&wait_for_info(path, INFOFILE_TIMEOUT).await?)
    } else if let Some(target) = &opts.remote {
        let pid = req.pid.ok_or_else(|| anyhow!("missing pid"))?;
        remote = true;
        let target = target.resolve().await?;
        let bundle = openprofiler_remote::ensure_bundle(&target, &bundle_dir(&opts)).await?;
        let agent = bundle.agent_for(&target, pid).await?;
        let res =
            openprofiler_remote::attach(&target, &bundle.relay, pid, &agent, &agent_option_string(&opts))
                .await?;
        let port = res["port"].as_u64().ok_or_else(|| anyhow!("attach result has no port: {res}"))? as u16;
        let token = res["token"].as_str().ok_or_else(|| anyhow!("attach result has no token"))?.to_owned();
        let t = Tunnel::open(target.clone(), bundle.relay.clone(), port).await?;
        let mut o = LinkOptions::new(t.local_addr.to_string(), token);
        if res["tls"].as_bool().unwrap_or(false) {
            o.fingerprint = res["fingerprint"].as_str().map(str::to_owned);
        }
        tunnel = Some(t);
        o
    } else {
        let pid = req.pid.ok_or_else(|| anyhow!("missing pid"))?;
        let info_path = AgentInfo::default_path(pid);
        if let Ok(info) = AgentInfo::read(&info_path).filter_ok(|i| i.pid == pid) {
            // Already profiled: reuse the running agent.
            LinkOptions::from_info(&info)
        } else {
            let agent = match &opts.agent_path {
                Some(p) => p.clone(),
                None => find_agent()?,
            };
            let options = agent_option_string(&opts);
            let res =
                tokio::task::spawn_blocking(move || openprofiler_attach::attach_agent(pid, &agent, &options))
                    .await?
                    .with_context(|| format!("attaching to {pid}"))?;
            let mut o = LinkOptions::new(res.addr(), res.token.clone());
            if res.tls {
                o.fingerprint = res.fingerprint.clone();
            }
            o
        }
    };
    let client = LinkClient::connect(link_opts).await?;
    let hello = client.hello();
    let jvm_name = {
        let p = client.profile();
        let p = p.read();
        if p.meta.jvm.main_class.is_empty() { p.meta.jvm.name.clone() } else { p.meta.jvm.main_class.clone() }
    };
    let pid = req.pid.or_else(|| client.profile().read().meta.jvm.pid);
    let name = match (pid, jvm_name.is_empty()) {
        (Some(pid), false) => format!("{jvm_name} ({pid})"),
        (Some(pid), true) => format!("JVM {pid}"),
        (None, _) => format!("JVM {}", hello.agent_version),
    };
    let name = match &opts.remote {
        Some(t) if !t.hops.is_empty() => format!("{name} @ {}", t.describe()),
        _ => name,
    };
    let session = engine.new_session(SessionKind::Live, &name, None);
    session.set_profile(client.profile());
    let watcher = tokio::spawn(watch_link(session.clone(), client.clone()));
    session.set_live(LiveHandle(Arc::new(Inner { client, pid, remote, _tunnel: tunnel, watcher })));
    session.set_state(SessionState::Live, None, None);
    Ok(session)
}

trait FilterOk<T> {
    fn filter_ok(self, f: impl FnOnce(&T) -> bool) -> Result<T>;
}

impl<T> FilterOk<T> for Result<T> {
    fn filter_ok(self, f: impl FnOnce(&T) -> bool) -> Result<T> {
        match self {
            Ok(v) if f(&v) => Ok(v),
            Ok(_) => Err(anyhow!("filtered")),
            Err(e) => Err(e),
        }
    }
}

/// Forwards data generations as (throttled) invalidations and tracks the link state.
async fn watch_link(session: Arc<Session>, client: LinkClient) {
    let mut generation = client.generation();
    let mut state = client.state();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(1000));
    let mut dirty = false;
    loop {
        tokio::select! {
            r = generation.changed() => {
                if r.is_err() { break; }
                dirty = true;
            }
            r = state.changed() => {
                if r.is_err() { break; }
                let s = state.borrow().clone();
                match s {
                    LinkState::Connected => session.set_state(SessionState::Live, None, None),
                    LinkState::Reconnecting { .. } => session.set_state(SessionState::Live, None, Some("reconnecting".into())),
                    LinkState::Closed(why) => {
                        session.set_state(SessionState::Detached, None, Some(why));
                        session.bump(vec!["all".into()]);
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                if dirty {
                    dirty = false;
                    session.bump(vec!["all".into()]);
                }
            }
        }
    }
}

fn dump_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".openprofiler"))
        .unwrap_or_else(std::env::temp_dir);
    let d = base.join("dumps");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub async fn control(engine: &Arc<Engine>, s: &Arc<Session>, action: &str, req: Value) -> Result<Value> {
    if action == "snapshot" {
        let path = PathBuf::from(req["path"].as_str().unwrap_or_default());
        if path.as_os_str().is_empty() {
            bail!("snapshot: missing path");
        }
        let s = s.clone();
        tokio::task::spawn_blocking(move || crate::save::save_snapshot(&s, &path)).await??;
        return Ok(json!({}));
    }
    let Some(live) = s.live() else { bail!("{action}: not a live session") };
    let c = live.client();
    match action {
        "recording" => {
            let kind = req["kind"].as_str().ok_or_else(|| anyhow!("missing kind"))?;
            let enabled = req["enabled"].as_bool().unwrap_or(true);
            let interval_us = req["interval_ms"].as_f64().map(|ms| (ms * 1000.0) as u32).unwrap_or(0);
            c.set_recording(kind, enabled, interval_us).await?;
            Ok(json!({}))
        }
        "gc" => {
            c.run_gc().await?;
            Ok(json!({}))
        }
        "bookmark" => {
            c.bookmark(req["label"].as_str().unwrap_or("Bookmark"), req["color"].as_str()).await?;
            s.bump(vec!["telemetry".into()]);
            Ok(json!({}))
        }
        "thread-dumps" => {
            let d = c.thread_dump().await?;
            if let Some(p) = s.profile() {
                p.write().thread_dumps.push(d.clone());
            }
            s.bump(vec!["thread_dumps".into()]);
            Ok(serde_json::to_value(d)?)
        }
        "heap-dump" => {
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
            let path = dump_dir().join(format!("{}-{ts}.hprof", live.0.pid.unwrap_or(0)));
            let live_only = req["live_only"].as_bool().unwrap_or(true);
            let out = c.heap_dump(&path, live_only, !live.0.remote).await?;
            let hs = engine.open(&out.path)?;
            Ok(json!({ "session": hs.info(), "path": out.path, "size": out.size }))
        }
        "triggers" => {
            // `{"triggers": [...]}` or a bare array; see `openprofiler_proto::triggers`.
            let triggers =
                openprofiler_proto::triggers::parse_json(&req.to_string()).map_err(|e| anyhow!(e))?;
            let n = triggers.len();
            c.set_triggers(triggers).await?;
            Ok(json!({ "installed": n }))
        }
        "mbeans"
        | "mbean"
        | "mbean-invoke"
        | "mbean-set"
        | "mbean-track"
        | "mbean-servers"
        | "mbean-connect"
        | "mbean-subscribe"
        | "mbean-notifications" => {
            let r = crate::mbeans::handle(c, action, &req).await?;
            if action == "mbean-track" {
                s.bump(vec!["telemetry".into()]);
            }
            Ok(r)
        }
        "instrumentation" => {
            // An empty body reads the settings; `include`/`exclude`/`probes` replace them.
            let list = |k: &str| -> Vec<String> {
                req[k]
                    .as_array()
                    .map_or(vec![], |a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            };
            let i = if req.get("include").is_some()
                || req.get("exclude").is_some()
                || req.get("probes").is_some()
            {
                c.set_instrumentation(list("include"), list("exclude"), list("probes")).await?
            } else {
                c.instrumentation().await?
            };
            Ok(json!({
                "include": i.include, "exclude": i.exclude,
                "available_probes": i.available_probes.iter().map(|p| json!({
                    "id": p.id, "name": p.name, "payload_label": p.payload_label,
                    "control_label": (!p.control_label.is_empty()).then_some(&p.control_label),
                })).collect::<Vec<_>>(),
                "enabled_probes": i.enabled_probes,
                "instrumented_classes": i.instrumented_classes,
                "instrumented_methods": i.instrumented_methods,
                "disabled_methods": i.disabled_methods,
            }))
        }
        other => bail!("unknown live action: {other}"),
    }
}

/// Instructions for starting a JVM with the agent (MCP `prepare_profiling`).
pub fn prepare_profiling(args: &Value) -> Value {
    let offline = args["mode"].as_str() == Some("offline");
    let out = args["output"].as_str().unwrap_or("app.oplink");
    let lib =
        find_agent().map(|p| p.display().to_string()).unwrap_or_else(|_| format!("/path/to/{}", lib_name()));
    let opts = if offline { format!("mode=offline,file={out}") } else { "port=0".to_owned() };
    json!({
        "jvm_option": format!("-agentpath:{lib}={opts}"),
        "notes": if offline {
            "The JVM writes the recording when it exits. Open it with load_snapshot."
        } else {
            "The agent listens on loopback with a random token published in $TMPDIR/openprofiler-<pid>.json. Call attach with the pid; attach reuses an already loaded agent."
        },
    })
}

/// Remote discovery for the UI pickers: `{target, what: "jvms"|"containers"|"pods"|"contexts"}`.
pub async fn remote_discover(req: Value) -> Result<Value> {
    let target: Target = match req.get("target") {
        Some(t) if !t.is_null() => serde_json::from_value(t.clone()).context("invalid target")?,
        _ => Target::local(),
    };
    match req["what"].as_str().unwrap_or("jvms") {
        "containers" => Ok(serde_json::to_value(openprofiler_remote::docker_containers(&target).await?)?),
        "contexts" => Ok(serde_json::to_value(openprofiler_remote::kube_contexts(&target).await?)?),
        "pods" => Ok(serde_json::to_value(
            openprofiler_remote::kube_pods(&target, req["context"].as_str(), req["namespace"].as_str())
                .await?,
        )?),
        "jvms" => {
            let opts: AttachOptions = serde_json::from_value(req["options"].clone()).unwrap_or_default();
            let target = target.resolve().await?;
            let bundle = openprofiler_remote::ensure_bundle(&target, &bundle_dir(&opts)).await?;
            openprofiler_remote::list_jvms(&target, &bundle.relay).await
        }
        w => bail!("unknown discovery kind {w}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn waits_for_infofile() {
        let path = std::env::temp_dir().join(format!("openprofiler-ide-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(wait_for_info(&path, Duration::from_millis(150)).await.is_err());
        let writer = {
            let path = path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                std::fs::write(&path, r#"{"pid":42,"port":1234,"bind":"127.0.0.1","token":"t"}"#).unwrap();
            })
        };
        let info = wait_for_info(&path, Duration::from_secs(5)).await.unwrap();
        writer.await.unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!((info.pid, info.addr().as_str()), (42, "127.0.0.1:1234"));
    }
}
