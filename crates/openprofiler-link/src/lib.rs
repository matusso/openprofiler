//! Daemon side of the OpenProfiler agent-link protocol.
//!
//! * [`LinkClient`] connects to a live agent, applies its pushed data batches into a shared
//!   [`Profile`], acknowledges them, exposes a generation counter for UI invalidation, sends
//!   control requests (thread dump, heap dump, recording toggles, GC, bookmarks, MBeans) and
//!   reconnects with watermark resume.
//! * [`replay_recording`] loads an offline `.oplink` recording written by the agent.
//! * [`AgentInfo`] reads the `openprofiler-<pid>.json` file an agent publishes.

mod apply;
mod tls;

pub use apply::{apply_batch, apply_hello, jvm_info, thread_dump};

use anyhow::{Context, Result, anyhow, bail};
use openprofiler_model::Profile;
use openprofiler_proto::{
    FrameType, HEADER_LEN, Message, RECORDING_MAGIC, decode_payload, encode_frame, parse_header, v1,
};
use parking_lot::RwLock;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};

/// Contents of the info file an agent publishes (`$TMPDIR/openprofiler-<pid>.json`, mode 0600).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub pid: u32,
    pub port: u16,
    #[serde(default)]
    pub bind: String,
    pub token: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub instance_id: String,
}

impl AgentInfo {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Default location for an agent running as the current user on this host.
    pub fn default_path(pid: u32) -> PathBuf {
        std::env::temp_dir().join(format!("openprofiler-{pid}.json"))
    }

    /// Address to connect to; wildcard binds are reached via loopback.
    pub fn addr(&self) -> String {
        let host = match self.bind.as_str() {
            "" | "0.0.0.0" => "127.0.0.1",
            "::" => "::1",
            h => h,
        };
        if host.contains(':') { format!("[{host}]:{}", self.port) } else { format!("{host}:{}", self.port) }
    }
}

#[derive(Debug, Clone)]
pub struct LinkOptions {
    /// `host:port`.
    pub addr: String,
    pub token: String,
    /// Pinned certificate fingerprint; `Some` enables TLS.
    pub fingerprint: Option<String>,
    pub client_name: String,
    /// Reconnect after connection loss (gives up after ~30 s of failures).
    pub reconnect: bool,
}

impl LinkOptions {
    pub fn new(addr: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            token: token.into(),
            fingerprint: None,
            client_name: "openprofiler-daemon".into(),
            reconnect: true,
        }
    }

    pub fn from_info(info: &AgentInfo) -> Self {
        let mut o = Self::new(info.addr(), info.token.clone());
        if info.tls {
            o.fingerprint = info.fingerprint.clone();
        }
        o
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Connected,
    Reconnecting {
        attempt: u32,
    },
    /// Final state; the string says why.
    Closed(String),
}

/// How the agent takes a heap dump.
#[derive(Debug, Clone, Default)]
pub struct HeapDumpOptions {
    pub live_only: bool,
    /// `"hotspot"` (or empty: `HotSpotDiagnosticMXBean.dumpHeap`) or `"native"` (JVMTI heap walk).
    pub dumper: String,
    /// Only objects recorded by allocation recording and their paths from the GC roots.
    pub recorded_only: bool,
}

/// What a heap dump request produced.
#[derive(Debug, Clone)]
pub struct HeapDumpOutcome {
    pub path: PathBuf,
    pub size: u64,
}

enum Cmd {
    Request { kind: v1::request::Kind, blob_target: Option<PathBuf>, reply: oneshot::Sender<Result<Reply>> },
    Close,
}

struct Reply {
    response: v1::Response,
    blob: Option<HeapDumpOutcome>,
}

/// A connection to a live agent. Cloning is cheap; all clones share one connection.
#[derive(Clone)]
pub struct LinkClient {
    profile: Arc<RwLock<Profile>>,
    generation: watch::Receiver<u64>,
    state: watch::Receiver<LinkState>,
    hello: Arc<RwLock<v1::Hello>>,
    config: Arc<RwLock<v1::SessionConfig>>,
    cmd: mpsc::Sender<Cmd>,
}

type Reader = ReadHalf<Box<dyn Io>>;
type Writer = WriteHalf<Box<dyn Io>>;

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

async fn read_frame(r: &mut Reader) -> std::io::Result<openprofiler_proto::Frame> {
    let mut h = [0u8; HEADER_LEN];
    r.read_exact(&mut h).await?;
    let (len, ty, flags) = parse_header(&h)?;
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    Ok(openprofiler_proto::Frame { ty, payload: decode_payload(flags, payload)? })
}

async fn write_frame<M: Message>(w: &mut Writer, ty: FrameType, m: &M) -> std::io::Result<()> {
    w.write_all(&encode_frame(ty, m)).await?;
    w.flush().await
}

async fn expect<M: Message + Default>(r: &mut Reader, ty: FrameType) -> Result<M> {
    loop {
        let f = read_frame(r).await?;
        match f.frame_type() {
            Some(t) if t == ty => return Ok(f.decode()?),
            Some(FrameType::Error) => {
                let e: v1::Error = f.decode()?;
                bail!("agent error: {}", e.message);
            }
            _ => continue,
        }
    }
}

struct Session {
    reader: Reader,
    writer: Writer,
    hello: v1::Hello,
    welcome: v1::Welcome,
}

async fn open(opts: &LinkOptions, resume: v1::Resume) -> Result<Session> {
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&opts.addr))
        .await
        .map_err(|_| anyhow!("connect to {} timed out", opts.addr))??;
    tcp.set_nodelay(true)?;
    let io: Box<dyn Io> = match &opts.fingerprint {
        Some(fp) => {
            let connector = tokio_rustls::TlsConnector::from(tls::client_config(fp)?);
            let name = rustls::pki_types::ServerName::try_from("openprofiler-agent")?;
            Box::new(connector.connect(name, tcp).await?)
        }
        None => Box::new(tcp),
    };
    let (mut reader, mut writer) = tokio::io::split(io);
    let fut = async {
        let hello: v1::Hello = expect(&mut reader, FrameType::Hello).await?;
        write_frame(
            &mut writer,
            FrameType::Auth,
            &v1::Auth { token: opts.token.clone(), client: opts.client_name.clone() },
        )
        .await?;
        write_frame(&mut writer, FrameType::Resume, &resume).await?;
        let welcome: v1::Welcome = expect(&mut reader, FrameType::Welcome).await?;
        Ok::<_, anyhow::Error>((hello, welcome))
    };
    let (hello, welcome) = tokio::time::timeout(Duration::from_secs(15), fut)
        .await
        .map_err(|_| anyhow!("handshake timed out"))??;
    Ok(Session { reader, writer, hello, welcome })
}

struct Shared {
    profile: Arc<RwLock<Profile>>,
    generation: watch::Sender<u64>,
    state: watch::Sender<LinkState>,
    hello: Arc<RwLock<v1::Hello>>,
    config: Arc<RwLock<v1::SessionConfig>>,
}

impl LinkClient {
    /// Connects and completes the handshake. The returned client keeps receiving data in a
    /// background task until [`LinkClient::close`] or the agent goes away.
    pub async fn connect(opts: LinkOptions) -> Result<Self> {
        let session = open(&opts, v1::Resume::default()).await?;
        let mut profile = Profile::new("live");
        apply_hello(&mut profile, &session.hello);
        let profile = Arc::new(RwLock::new(profile));
        let (gen_tx, gen_rx) = watch::channel(0u64);
        let (state_tx, state_rx) = watch::channel(LinkState::Connected);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let shared = Shared {
            profile: profile.clone(),
            generation: gen_tx,
            state: state_tx,
            hello: Arc::new(RwLock::new(session.hello.clone())),
            config: Arc::new(RwLock::new(session.welcome.config.clone().unwrap_or_default())),
        };
        let client = LinkClient {
            profile,
            generation: gen_rx,
            state: state_rx,
            hello: shared.hello.clone(),
            config: shared.config.clone(),
            cmd: cmd_tx,
        };
        tokio::spawn(run(opts, shared, session, cmd_rx));
        Ok(client)
    }

    /// Convenience: reads the agent info file for `pid` and connects.
    pub async fn connect_pid(pid: u32) -> Result<Self> {
        let info = AgentInfo::read(&AgentInfo::default_path(pid))?;
        Self::connect(LinkOptions::from_info(&info)).await
    }

    pub fn profile(&self) -> Arc<RwLock<Profile>> {
        self.profile.clone()
    }

    /// Increments every time new data was applied.
    pub fn generation(&self) -> watch::Receiver<u64> {
        self.generation.clone()
    }

    pub fn state(&self) -> watch::Receiver<LinkState> {
        self.state.clone()
    }

    pub fn hello(&self) -> v1::Hello {
        self.hello.read().clone()
    }

    pub fn config(&self) -> v1::SessionConfig {
        self.config.read().clone()
    }

    async fn request(&self, kind: v1::request::Kind, blob_target: Option<PathBuf>) -> Result<Reply> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Request { kind, blob_target, reply: tx })
            .await
            .map_err(|_| anyhow!("link closed"))?;
        let reply = rx.await.map_err(|_| anyhow!("link closed"))??;
        if !reply.response.ok {
            bail!("agent: {}", reply.response.error);
        }
        Ok(reply)
    }

    pub async fn thread_dump(&self) -> Result<openprofiler_model::ThreadDump> {
        let r = self.request(v1::request::Kind::ThreadDump(v1::ThreadDumpRequest {}), None).await?;
        match r.response.result {
            Some(v1::response::Result::ThreadDump(d)) => {
                let dump = thread_dump(&d);
                self.profile.write().thread_dumps.push(dump.clone());
                Ok(dump)
            }
            _ => bail!("unexpected thread dump response"),
        }
    }

    /// Requests an HPROF heap dump and stores it at `path`. With `agent_writes_locally` the agent
    /// writes the file itself (same host / filesystem); otherwise it streams the dump.
    pub async fn heap_dump(
        &self,
        path: &Path,
        live_only: bool,
        agent_writes_locally: bool,
    ) -> Result<HeapDumpOutcome> {
        let opts = HeapDumpOptions { live_only, ..Default::default() };
        self.heap_dump_with(path, &opts, agent_writes_locally).await
    }

    /// Like [`Self::heap_dump`], with a choice of dumper.
    pub async fn heap_dump_with(
        &self,
        path: &Path,
        opts: &HeapDumpOptions,
        agent_writes_locally: bool,
    ) -> Result<HeapDumpOutcome> {
        let abs = std::path::absolute(path)?;
        let req = v1::HeapDumpRequest {
            format: "hprof".into(),
            live_only: opts.live_only,
            local_path: if agent_writes_locally { abs.display().to_string() } else { String::new() },
            dumper: opts.dumper.clone(),
            recorded_only: opts.recorded_only,
        };
        let r = self
            .request(v1::request::Kind::HeapDump(req), (!agent_writes_locally).then(|| abs.clone()))
            .await?;
        match (r.blob, r.response.result) {
            (Some(b), _) => Ok(b),
            (None, Some(v1::response::Result::HeapDump(h))) => {
                Ok(HeapDumpOutcome { path: abs, size: h.size })
            }
            _ => bail!("unexpected heap dump response"),
        }
    }

    pub async fn set_recording(&self, kind: &str, enabled: bool, interval_us: u32) -> Result<()> {
        let k = v1::SetRecording { kind: kind.into(), enabled, interval_us, ..Default::default() };
        self.request(v1::request::Kind::SetRecording(k), None).await.map(|_| ())
    }

    /// Starts the call tracer for `duration_ms` (0 = 10 s) or `max_events` (0 = the agent's
    /// default), or stops it.
    pub async fn call_trace(&self, enabled: bool, duration_ms: u32, max_events: u32) -> Result<()> {
        let k = v1::SetRecording { kind: "trace".into(), enabled, interval_us: 0, duration_ms, max_events };
        self.request(v1::request::Kind::SetRecording(k), None).await.map(|_| ())
    }

    pub async fn run_gc(&self) -> Result<()> {
        self.request(v1::request::Kind::RunGc(v1::RunGc {}), None).await.map(|_| ())
    }

    pub async fn bookmark(&self, label: &str, color: Option<&str>) -> Result<()> {
        let b = v1::AddBookmark { label: label.into(), color: color.unwrap_or("").into() };
        self.request(v1::request::Kind::Bookmark(b), None).await.map(|_| ())
    }

    /// Replaces the agent's triggers (see `openprofiler_proto::triggers` for the JSON form).
    pub async fn set_triggers(&self, triggers: Vec<v1::Trigger>) -> Result<()> {
        self.request(v1::request::Kind::SetTriggers(v1::SetTriggers { triggers }), None).await.map(|_| ())
    }

    /// Replaces the instrumentation settings (call-count patterns, enabled probes, complexity
    /// methods); the agent retransforms the affected classes. Empty `probes` enables all,
    /// `["none"]` disables all.
    pub async fn set_instrumentation(
        &self,
        include: Vec<String>,
        exclude: Vec<String>,
        probes: Vec<String>,
        complexity: Vec<String>,
    ) -> Result<v1::InstrumentationInfo> {
        let kind = v1::request::Kind::SetInstrumentation(v1::SetInstrumentation {
            include,
            exclude,
            probes,
            complexity,
        });
        match self.request(kind, None).await?.response.result {
            Some(v1::response::Result::Instrumentation(i)) => Ok(i),
            _ => bail!("unexpected instrumentation response"),
        }
    }

    /// The current instrumentation settings and statistics.
    pub async fn instrumentation(&self) -> Result<v1::InstrumentationInfo> {
        let kind = v1::request::Kind::Instrumentation(v1::InstrumentationRequest {});
        match self.request(kind, None).await?.response.result {
            Some(v1::response::Result::Instrumentation(i)) => Ok(i),
            _ => bail!("unexpected instrumentation response"),
        }
    }

    /// Asks the agent to write an `.oplink` recording at `path` (on the agent's filesystem).
    pub async fn save_recording(&self, path: &str) -> Result<()> {
        let s = v1::SaveSnapshot { path: path.into() };
        self.request(v1::request::Kind::SaveSnapshot(s), None).await.map(|_| ())
    }

    /// The MBean servers: platform, others in the JVM, connected remote JMX servers.
    pub async fn mbean_servers(&self) -> Result<Vec<v1::MBeanServerInfo>> {
        let kind = v1::request::Kind::MbeanServers(v1::MBeanServersRequest {});
        match self.request(kind, None).await?.response.result {
            Some(v1::response::Result::MbeanServers(l)) => Ok(l.servers),
            _ => bail!("unexpected MBean servers response"),
        }
    }

    /// Connects the JVM to a remote JMX server, or disconnects it; returns the servers.
    pub async fn mbean_connect(&self, req: v1::MBeanConnectRequest) -> Result<Vec<v1::MBeanServerInfo>> {
        match self.request(v1::request::Kind::MbeanConnect(req), None).await?.response.result {
            Some(v1::response::Result::MbeanServers(l)) => Ok(l.servers),
            _ => bail!("unexpected MBean connect response"),
        }
    }

    /// Lists MBeans of `server` (empty = platform) matching an ObjectName `pattern` (empty = all)
    /// and the tracked attributes.
    pub async fn mbean_list(&self, server: &str, pattern: &str) -> Result<v1::MBeanList> {
        let r = v1::MBeanListRequest { pattern: pattern.into(), server: server.into() };
        match self.request(v1::request::Kind::MbeanList(r), None).await?.response.result {
            Some(v1::response::Result::MbeanList(l)) => Ok(l),
            _ => bail!("unexpected MBean list response"),
        }
    }

    /// An MBean's attributes (with current values) and operations.
    pub async fn mbean_get(&self, server: &str, name: &str) -> Result<v1::MBeanInfo> {
        let r = v1::MBeanGetRequest { name: name.into(), server: server.into() };
        match self.request(v1::request::Kind::MbeanGet(r), None).await?.response.result {
            Some(v1::response::Result::MbeanInfo(i)) => Ok(i),
            _ => bail!("unexpected MBean response"),
        }
    }

    /// Invokes an operation; returns its result as JSON.
    pub async fn mbean_invoke(&self, req: v1::MBeanInvokeRequest) -> Result<String> {
        match self.request(v1::request::Kind::MbeanInvoke(req), None).await?.response.result {
            Some(v1::response::Result::MbeanInvoke(r)) => Ok(r.value_json),
            _ => bail!("unexpected MBean invoke response"),
        }
    }

    pub async fn mbean_set(&self, req: v1::MBeanSetRequest) -> Result<()> {
        self.request(v1::request::Kind::MbeanSet(req), None).await.map(|_| ())
    }

    /// Starts or stops sampling an attribute as telemetry; returns the tracked attributes.
    pub async fn mbean_track(&self, req: v1::MBeanTrackRequest) -> Result<Vec<v1::TrackedAttribute>> {
        match self.request(v1::request::Kind::MbeanTrack(req), None).await?.response.result {
            Some(v1::response::Result::MbeanList(l)) => Ok(l.tracked),
            _ => bail!("unexpected MBean track response"),
        }
    }

    /// Adds or removes a notification listener; returns the subscriptions.
    pub async fn mbean_subscribe(
        &self,
        req: v1::MBeanSubscribeRequest,
    ) -> Result<Vec<v1::MBeanSubscription>> {
        match self.request(v1::request::Kind::MbeanSubscribe(req), None).await?.response.result {
            Some(v1::response::Result::MbeanNotifications(n)) => Ok(n.subscriptions),
            _ => bail!("unexpected MBean subscribe response"),
        }
    }

    /// Buffered notifications with a sequence number above `after`.
    pub async fn mbean_notifications(&self, after: u64) -> Result<v1::MBeanNotifications> {
        let kind = v1::request::Kind::MbeanNotifications(v1::MBeanNotificationsRequest { after });
        match self.request(kind, None).await?.response.result {
            Some(v1::response::Result::MbeanNotifications(n)) => Ok(n),
            _ => bail!("unexpected MBean notifications response"),
        }
    }

    pub async fn close(&self) {
        let _ = self.cmd.send(Cmd::Close).await;
    }
}

struct Pending {
    reply: oneshot::Sender<Result<Reply>>,
    blob_target: Option<PathBuf>,
}

struct BlobSink {
    blob_id: u64,
    file: tokio::fs::File,
    path: PathBuf,
    size: u64,
    response: v1::Response,
    reply: oneshot::Sender<Result<Reply>>,
}

enum End {
    Closed,
    Lost(anyhow::Error),
}

async fn run(opts: LinkOptions, shared: Shared, first: Session, mut cmd_rx: mpsc::Receiver<Cmd>) {
    let mut watermark = first.welcome.resume_from.unwrap_or_default();
    let mut instance = first.hello.instance_id;
    let mut session = Some(first);
    let mut attempt = 0u32;
    loop {
        let s = match session.take() {
            Some(s) => s,
            None => {
                attempt += 1;
                let _ = shared.state.send(LinkState::Reconnecting { attempt });
                tokio::time::sleep(Duration::from_secs(1)).await;
                let resume = v1::Resume { instance_id: instance, watermark: Some(watermark) };
                match open(&opts, resume).await {
                    Ok(s) => s,
                    Err(e) => {
                        if attempt >= 30 {
                            let _ = shared.state.send(LinkState::Closed(format!("agent unreachable: {e}")));
                            return;
                        }
                        continue;
                    }
                }
            }
        };
        attempt = 0;
        if s.hello.instance_id != instance {
            // The agent restarted (new JVM): start a fresh profile.
            let mut p = Profile::new("live");
            apply_hello(&mut p, &s.hello);
            *shared.profile.write() = p;
            instance = s.hello.instance_id;
        }
        watermark = s.welcome.resume_from.unwrap_or_default();
        *shared.hello.write() = s.hello.clone();
        *shared.config.write() = s.welcome.config.clone().unwrap_or_default();
        let _ = shared.state.send(LinkState::Connected);
        match serve(s, &shared, &mut cmd_rx, &mut watermark).await {
            End::Closed => {
                let _ = shared.state.send(LinkState::Closed("closed by client".into()));
                return;
            }
            End::Lost(e) => {
                tracing::debug!("agent link lost: {e:#}");
                if !opts.reconnect {
                    let _ = shared.state.send(LinkState::Closed(format!("connection lost: {e}")));
                    return;
                }
            }
        }
    }
}

async fn serve(
    s: Session,
    shared: &Shared,
    cmd_rx: &mut mpsc::Receiver<Cmd>,
    watermark: &mut v1::Watermark,
) -> End {
    let Session { mut reader, mut writer, .. } = s;
    let (ftx, mut frx) = mpsc::channel::<std::io::Result<openprofiler_proto::Frame>>(64);
    let reader_task = tokio::spawn(async move {
        loop {
            let f = read_frame(&mut reader).await;
            let stop = f.is_err();
            if ftx.send(f).await.is_err() || stop {
                break;
            }
        }
    });
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut blob: Option<BlobSink> = None;
    let mut next_id = 1u64;
    let mut ping = tokio::time::interval(Duration::from_secs(5));
    let result = loop {
        tokio::select! {
            f = frx.recv() => {
                let f = match f {
                    Some(Ok(f)) => f,
                    Some(Err(e)) => break End::Lost(e.into()),
                    None => break End::Lost(anyhow!("reader stopped")),
                };
                if let Err(e) = on_frame(f, shared, &mut writer, watermark, &mut pending, &mut blob).await {
                    break End::Lost(e);
                }
            }
            c = cmd_rx.recv() => match c {
                None | Some(Cmd::Close) => break End::Closed,
                Some(Cmd::Request { kind, blob_target, reply }) => {
                    let id = next_id;
                    next_id += 1;
                    let req = v1::Request { id, kind: Some(kind) };
                    if let Err(e) = write_frame(&mut writer, FrameType::Request, &req).await {
                        let _ = reply.send(Err(anyhow!("send failed: {e}")));
                        break End::Lost(e.into());
                    }
                    pending.insert(id, Pending { reply, blob_target });
                }
            },
            _ = ping.tick() => {
                if let Err(e) = write_frame(&mut writer, FrameType::Ping, &v1::Ping { time: 0 }).await {
                    break End::Lost(e.into());
                }
            }
        }
    };
    reader_task.abort();
    for (_, p) in pending {
        let _ = p.reply.send(Err(anyhow!("connection lost")));
    }
    if let Some(b) = blob {
        let _ = b.reply.send(Err(anyhow!("connection lost during heap dump transfer")));
    }
    result
}

async fn on_frame(
    f: openprofiler_proto::Frame,
    shared: &Shared,
    writer: &mut Writer,
    watermark: &mut v1::Watermark,
    pending: &mut HashMap<u64, Pending>,
    blob: &mut Option<BlobSink>,
) -> Result<()> {
    match f.frame_type() {
        Some(FrameType::DataBatch) => {
            let b: v1::DataBatch = f.decode()?;
            {
                let mut p = shared.profile.write();
                apply_batch(&mut p, &b)?;
            }
            if let Some(to) = b.to {
                *watermark = to;
            }
            shared.generation.send_modify(|g| *g += 1);
            write_frame(writer, FrameType::Ack, &v1::Ack { watermark: Some(*watermark) }).await?;
        }
        Some(FrameType::Response) => {
            let r: v1::Response = f.decode()?;
            let Some(p) = pending.remove(&r.id) else { return Ok(()) };
            let blob_id = match &r.result {
                Some(v1::response::Result::HeapDump(h)) if r.ok && h.blob_id != 0 => Some(h.blob_id),
                _ => None,
            };
            match (blob_id, p.blob_target) {
                (Some(id), Some(path)) => {
                    if let Some(dir) = path.parent() {
                        tokio::fs::create_dir_all(dir).await.ok();
                    }
                    let file = tokio::fs::File::create(&path).await?;
                    *blob = Some(BlobSink { blob_id: id, file, path, size: 0, response: r, reply: p.reply });
                }
                _ => {
                    let _ = p.reply.send(Ok(Reply { response: r, blob: None }));
                }
            }
        }
        Some(FrameType::BlobChunk) => {
            let c: v1::BlobChunk = f.decode()?;
            let done = match blob.as_mut() {
                Some(b) if b.blob_id == c.blob_id => {
                    b.file.write_all(&c.data).await?;
                    b.size += c.data.len() as u64;
                    c.last
                }
                _ => false,
            };
            if done && let Some(mut b) = blob.take() {
                b.file.flush().await?;
                let out = HeapDumpOutcome { path: b.path, size: b.size };
                let _ = b.reply.send(Ok(Reply { response: b.response, blob: Some(out) }));
            }
        }
        Some(FrameType::Error) => {
            let e: v1::Error = f.decode()?;
            if e.fatal {
                bail!("agent error: {}", e.message);
            }
            tracing::warn!("agent error: {}", e.message);
        }
        _ => {}
    }
    Ok(())
}

/// Loads an offline `.oplink` recording.
pub fn replay_recording(path: &Path) -> Result<Profile> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    replay_bytes(&data)
}

/// Whether `bytes` start like an `.oplink` recording.
pub fn is_recording(bytes: &[u8]) -> bool {
    bytes.starts_with(RECORDING_MAGIC)
}

pub fn replay_bytes(data: &[u8]) -> Result<Profile> {
    if !is_recording(data) {
        bail!("not an .oplink recording");
    }
    let mut r = &data[RECORDING_MAGIC.len()..];
    let mut p = Profile::new("oplink");
    while let Some(f) = openprofiler_proto::read_frame(&mut r)? {
        match f.frame_type() {
            Some(FrameType::RecordingHeader) => {
                let h: v1::RecordingHeader = f.decode()?;
                if let Some(hello) = &h.hello {
                    apply_hello(&mut p, hello);
                }
            }
            Some(FrameType::DataBatch) => apply_batch(&mut p, &f.decode()?)?,
            _ => {}
        }
    }
    Ok(p)
}
