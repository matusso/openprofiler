//! Session management. The [`Engine`] is usable in-process (CLI, MCP, Tauri) and is what the HTTP
//! layer wraps.

use anyhow::{Context, Result, anyhow, bail};
use openprofiler_model::Profile;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;

use crate::decompile::DecompileHandle;
use crate::heap::HeapHandle;
use crate::live::LiveHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Hprof,
    Jfr,
    Snapshot,
    Live,
    /// Code opened in the decompiler: an Android app (`.apk`, `.dex`, ...) or JVM bytecode
    /// (`.jar`, `.class`, ...).
    Dex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Loading,
    Ready,
    Live,
    Detached,
    Failed,
}

/// Wire form of a session (`Session` in the spec).
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub kind: SessionKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub state: SessionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub capabilities: Vec<String>,
    pub generation: u64,
}

/// Server → client notifications (the events WebSocket).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Sessions,
    Session {
        id: String,
        state: SessionState,
        #[serde(skip_serializing_if = "Option::is_none")]
        progress: Option<f64>,
        generation: u64,
    },
    Invalidate {
        id: String,
        kinds: Vec<String>,
        generation: u64,
    },
}

struct Status {
    state: SessionState,
    progress: Option<f64>,
    error: Option<String>,
}

pub type SharedProfile = Arc<RwLock<Profile>>;

pub struct Session {
    pub id: String,
    pub kind: SessionKind,
    pub name: String,
    pub path: Option<PathBuf>,
    status: Mutex<Status>,
    generation: AtomicU64,
    profile: RwLock<Option<SharedProfile>>,
    heap: RwLock<Option<HeapHandle>>,
    live: RwLock<Option<LiveHandle>>,
    decompiler: RwLock<Option<DecompileHandle>>,
    cache: Mutex<(u64, HashMap<String, Arc<dyn Any + Send + Sync>>)>,
    events: broadcast::Sender<ServerEvent>,
}

impl Session {
    pub fn info(&self) -> SessionInfo {
        let st = self.status.lock();
        SessionInfo {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.clone(),
            path: self.path.as_ref().map(|p| p.display().to_string()),
            state: st.state,
            progress: st.progress,
            error: st.error.clone(),
            capabilities: self.capabilities(),
            generation: self.generation(),
        }
    }

    pub fn state(&self) -> SessionState {
        self.status.lock().state
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn capabilities(&self) -> Vec<String> {
        let mut caps = vec![];
        if let Some(p) = self.profile() {
            let p = p.read();
            for k in p.available_kinds() {
                caps.push(k.name().to_owned());
            }
            if !p.telemetry.is_empty() || !p.gc.is_empty() {
                caps.push("telemetry".into());
            }
            if !p.gc.is_empty() {
                caps.push("gc".into());
            }
            if !p.threads.is_empty() {
                caps.push("threads".into());
            }
            if !p.thread_dumps.is_empty() || self.live().is_some() {
                caps.push("thread_dumps".into());
            }
            if p.probes
                .probes
                .iter()
                .enumerate()
                .any(|(i, _)| p.probes.calls.iter().any(|c| c.probe as usize == i))
            {
                caps.push("probes".into());
            }
            if !p.calls.is_empty() {
                caps.push("call_analysis".into());
            }
        }
        if self.heap().is_some() {
            caps.push("heap".into());
        }
        if let Some(l) = self.live() {
            caps.push("recording".into());
            let agent = l.client().hello().capabilities;
            if agent.iter().any(|c| c == "mbeans") {
                caps.push("mbeans".into());
            }
            if agent.iter().any(|c| c == "probes") && !caps.iter().any(|c| c == "probes") {
                caps.push("probes".into());
            }
            if agent.iter().any(|c| c == "instrument") {
                caps.push("instrument".into());
                if !caps.iter().any(|c| c == "call_analysis") {
                    caps.push("call_analysis".into());
                }
            }
        }
        if self.decompiler().is_some() {
            caps.push("decompile".into());
        }
        caps
    }

    pub fn profile(&self) -> Option<SharedProfile> {
        self.profile.read().clone()
    }

    pub fn heap(&self) -> Option<HeapHandle> {
        self.heap.read().clone()
    }

    pub fn live(&self) -> Option<LiveHandle> {
        self.live.read().clone()
    }

    pub fn decompiler(&self) -> Option<DecompileHandle> {
        self.decompiler.read().clone()
    }

    pub fn require_profile(&self) -> Result<SharedProfile> {
        self.profile().ok_or_else(|| match self.state() {
            SessionState::Loading => anyhow!("session {} is still loading", self.id),
            SessionState::Failed => anyhow!("session {} failed to load", self.id),
            _ => anyhow!("session {} has no profiling data (heap-only session)", self.id),
        })
    }

    pub fn require_heap(&self) -> Result<HeapHandle> {
        self.heap().ok_or_else(|| match self.state() {
            SessionState::Loading => anyhow!("session {} is still loading", self.id),
            _ => anyhow!("session {} has no heap dump", self.id),
        })
    }

    pub fn set_profile(&self, p: SharedProfile) {
        *self.profile.write() = Some(p);
        self.bump(vec!["all".into()]);
    }

    pub fn set_heap(&self, h: HeapHandle) {
        *self.heap.write() = Some(h);
        self.bump(vec!["heap".into()]);
    }

    pub fn set_decompiler(&self, d: DecompileHandle) {
        *self.decompiler.write() = Some(d);
        self.bump(vec!["code".into()]);
    }

    pub fn set_live(&self, l: LiveHandle) {
        *self.live.write() = Some(l);
    }

    pub fn set_state(&self, state: SessionState, progress: Option<f64>, error: Option<String>) {
        {
            let mut st = self.status.lock();
            st.state = state;
            st.progress = progress;
            if error.is_some() {
                st.error = error;
            }
        }
        let _ = self.events.send(ServerEvent::Session {
            id: self.id.clone(),
            state,
            progress,
            generation: self.generation(),
        });
    }

    /// Marks the data as changed: clears caches and tells clients to refetch.
    pub fn bump(&self, kinds: Vec<String>) {
        let g = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.events.send(ServerEvent::Invalidate { id: self.id.clone(), kinds, generation: g });
    }

    /// Memoizes `f` under `key` for the current generation.
    pub fn cached<T: Send + Sync + 'static>(
        &self,
        key: &str,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        let gen_now = self.generation();
        {
            let mut c = self.cache.lock();
            if c.0 != gen_now {
                c.0 = gen_now;
                c.1.clear();
            }
            if let Some(v) = c.1.get(key)
                && let Ok(v) = v.clone().downcast::<T>()
            {
                return Ok(v);
            }
        }
        let v = Arc::new(f()?);
        let mut c = self.cache.lock();
        if c.0 == gen_now {
            if c.1.len() >= 128 {
                c.1.clear();
            }
            c.1.insert(key.to_owned(), v.clone());
        }
        Ok(v)
    }
}

/// Recognized input file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Hprof,
    HprofGz,
    /// IBM Portable Heap Dump (OpenJ9 / Semeru), optionally gzip-compressed.
    Phd,
    Jfr,
    Snapshot,
    Recording,
    /// Dalvik executable.
    Dex,
    /// JVM class file.
    Class,
    /// Directory tree of JVM class files (`target/classes`).
    ClassDir,
    /// Zip container of code for the decompiler: Android apps with `classes*.dex` (`.apk`,
    /// `.xapk`, `.apks`, `.aab`, ...) or JVM archives with class files (`.jar`, `.war`, `.ear`,
    /// `.jmod`, `.aar`).
    Archive,
}

/// Zip-based containers the decompiler accepts.
const ARCHIVE_EXTENSIONS: &[&str] =
    &["apk", "apks", "apkm", "xapk", "aab", "zip", "jar", "war", "ear", "aar", "jmod"];

pub fn detect_format(path: &Path) -> Result<FileFormat> {
    use std::io::Read;
    if path.is_dir() {
        if openprofiler_dex::jvm::has_class_files(path) {
            return Ok(FileFormat::ClassDir);
        }
        bail!(
            "{}: a directory without .class files (open a file, or a directory of classes)",
            path.display()
        );
    }
    let mut f = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut head = [0u8; 20];
    let n = f.read(&mut head)?;
    let head = &head[..n];
    if head.starts_with(b"JAVA PROFILE 1.0") {
        return Ok(FileFormat::Hprof);
    }
    if openprofiler_phd::is_phd(head) {
        return Ok(FileFormat::Phd);
    }
    if head.starts_with(b"FLR\0") {
        return Ok(FileFormat::Jfr);
    }
    if head.starts_with(b"OPRFSNAP") {
        return Ok(FileFormat::Snapshot);
    }
    if head.starts_with(b"OPRFLINK") {
        return Ok(FileFormat::Recording);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        if openprofiler_phd::sniff(path).unwrap_or(false) {
            return Ok(FileFormat::Phd);
        }
        return Ok(FileFormat::HprofGz);
    }
    if head.starts_with(b"dex\n") {
        return Ok(FileFormat::Dex);
    }
    if openprofiler_dex::jvm::is_class(head) {
        return Ok(FileFormat::Class);
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    // A .jmod is a zip behind a `JM` header.
    if (head.starts_with(b"PK\x03\x04") || head.starts_with(b"JM\x01\x00"))
        && ARCHIVE_EXTENSIONS.contains(&ext.as_str())
    {
        return Ok(FileFormat::Archive);
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or_default();
    bail!(
        "unrecognized file format: {name} (expected .hprof, .hprof.gz, .phd, .jfr, .opsnap, \
         or for the decompiler .apk / .dex / .jar / .class)"
    )
}

/// Pluggable loaders so the engine can be built and tested without every format crate.
pub trait Loader: Send + Sync {
    /// Loads the file into the session (calling `set_profile` / `set_heap` and reporting
    /// progress via `set_state`). Runs on a blocking thread.
    fn load(&self, format: FileFormat, path: &Path, session: &Session) -> Result<()>;
}

pub struct Engine {
    sessions: RwLock<Vec<Arc<Session>>>,
    events: broadcast::Sender<ServerEvent>,
    ide: broadcast::Sender<serde_json::Value>,
    next_id: AtomicU64,
    loader: RwLock<Option<Arc<dyn Loader>>>,
}

impl Default for Engine {
    fn default() -> Self {
        let (events, _) = broadcast::channel(1024);
        let (ide, _) = broadcast::channel(64);
        Self {
            sessions: RwLock::new(vec![]),
            events,
            ide,
            next_id: AtomicU64::new(1),
            loader: RwLock::new(None),
        }
    }
}

impl Engine {
    pub fn new() -> Arc<Self> {
        let e = Arc::new(Self::default());
        e.set_loader(Arc::new(crate::loaders::DefaultLoader));
        e
    }

    pub fn set_loader(&self, loader: Arc<dyn Loader>) {
        *self.loader.write() = Some(loader);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }

    /// Notifications for connected IDE plugins (see [`crate::ide`]).
    pub fn subscribe_ide(&self) -> broadcast::Receiver<serde_json::Value> {
        self.ide.subscribe()
    }

    /// Sends a JSON-RPC notification to every connected IDE; returns how many received it.
    pub fn notify_ide(&self, method: &str, params: serde_json::Value) -> usize {
        self.ide
            .send(serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .unwrap_or(0)
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.read().clone()
    }

    pub fn session(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions
            .read()
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("no such session: {id}"))
    }

    pub fn new_session(&self, kind: SessionKind, name: &str, path: Option<PathBuf>) -> Arc<Session> {
        let id = format!("s{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let s = Arc::new(Session {
            id,
            kind,
            name: name.to_owned(),
            path,
            status: Mutex::new(Status { state: SessionState::Loading, progress: None, error: None }),
            generation: AtomicU64::new(1),
            profile: RwLock::new(None),
            heap: RwLock::new(None),
            live: RwLock::new(None),
            decompiler: RwLock::new(None),
            cache: Mutex::new((0, HashMap::new())),
            events: self.events.clone(),
        });
        self.sessions.write().push(s.clone());
        let _ = self.events.send(ServerEvent::Sessions);
        s
    }

    pub fn add_profile(&self, kind: SessionKind, name: &str, profile: Profile) -> Arc<Session> {
        let s = self.new_session(kind, name, None);
        s.set_profile(Arc::new(RwLock::new(profile)));
        s.set_state(SessionState::Ready, None, None);
        s
    }

    pub fn close(&self, id: &str) -> Result<()> {
        let removed = {
            let mut v = self.sessions.write();
            let before = v.len();
            let mut removed = None;
            v.retain(|s| {
                if s.id == id {
                    removed = Some(s.clone());
                    false
                } else {
                    true
                }
            });
            if v.len() == before {
                bail!("no such session: {id}");
            }
            removed
        };
        if let Some(live) = removed.and_then(|s| s.live()) {
            live.close();
        }
        let _ = self.events.send(ServerEvent::Sessions);
        Ok(())
    }

    /// Opens a file. Returns immediately with a `loading` session; loading runs on a blocking
    /// thread when a tokio runtime is available, inline otherwise.
    pub fn open(&self, path: &Path) -> Result<Arc<Session>> {
        let path = std::fs::canonicalize(path).with_context(|| format!("cannot open {}", path.display()))?;
        let format = detect_format(&path)?;
        let kind = match format {
            FileFormat::Hprof | FileFormat::HprofGz | FileFormat::Phd => SessionKind::Hprof,
            FileFormat::Jfr => SessionKind::Jfr,
            FileFormat::Snapshot | FileFormat::Recording => SessionKind::Snapshot,
            FileFormat::Dex | FileFormat::Class | FileFormat::ClassDir | FileFormat::Archive => {
                SessionKind::Dex
            }
        };
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let session = self.new_session(kind, &name, Some(path.clone()));
        let loader = self.loader.read().clone().ok_or_else(|| anyhow!("no loader configured"))?;
        let s2 = session.clone();
        let job = move || {
            let started = std::time::Instant::now();
            match loader.load(format, &path, &s2) {
                Ok(()) => {
                    tracing::info!("loaded {} in {:.2?}", path.display(), started.elapsed());
                    s2.set_state(SessionState::Ready, None, None)
                }
                Err(e) => {
                    tracing::warn!("failed to load {}: {e:#}", path.display());
                    s2.set_state(SessionState::Failed, None, Some(format!("{e:#}")))
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(job);
            }
            Err(_) => job(),
        }
        Ok(session)
    }

    /// Opens a file and blocks until it has loaded (CLI use).
    pub fn open_blocking(&self, path: &Path) -> Result<Arc<Session>> {
        let s = self.open(path)?;
        self.wait_ready_blocking(&s)?;
        Ok(s)
    }

    pub fn wait_ready_blocking(&self, s: &Session) -> Result<()> {
        loop {
            match s.state() {
                SessionState::Loading => std::thread::sleep(std::time::Duration::from_millis(20)),
                SessionState::Failed => {
                    bail!("{}", s.info().error.unwrap_or_else(|| "failed to load".into()))
                }
                _ => return Ok(()),
            }
        }
    }

    pub async fn wait_ready(&self, s: &Session) -> Result<()> {
        loop {
            match s.state() {
                SessionState::Loading => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                SessionState::Failed => {
                    bail!("{}", s.info().error.unwrap_or_else(|| "failed to load".into()))
                }
                _ => return Ok(()),
            }
        }
    }
}
