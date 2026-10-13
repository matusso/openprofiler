//! Agent-link server: one daemon connection at a time, push-based data plane plus
//! request/response control. See `docs/spec/agent-link.md`.

use crate::collector::now_nanos;
use crate::jvm::jvmti;
use crate::{AGENT_VERSION, Agent, debug, log};
use ember_proto::{Frame, FrameType, HEADER_LEN, Message, decode_payload, encode_frame, parse_header, v1};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FLUSH_EVERY: Duration = Duration::from_millis(200);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const BLOB_CHUNK: usize = 1 << 20;

static FINAL_REQUESTED: AtomicBool = AtomicBool::new(false);
static FINAL_DONE: AtomicBool = AtomicBool::new(false);
static SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn start(a: &'static Agent) {
    let r = std::thread::Builder::new().name("ember-link".into()).spawn(move || run(a));
    if let Err(e) = r {
        log(&format!("cannot start agent-link server: {e}"));
    }
}

/// Called at VM death: lets the active session send its last batch (bounded wait).
pub fn final_flush(_a: &Agent) {
    if !SESSION_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    FINAL_REQUESTED.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !FINAL_DONE.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn hello(a: &Agent) -> v1::Hello {
    v1::Hello {
        protocol_semver: ember_proto::PROTOCOL_SEMVER.into(),
        agent_version: AGENT_VERSION.into(),
        jvm: Some(a.jvm_info.lock().clone()),
        capabilities: {
            let mut c = a.caps.lock().clone();
            if a.async_active.load(Ordering::SeqCst) {
                c.push("cpu_async".into());
            }
            c
        },
        token: String::new(),
        instance_id: a.instance_id,
        mode: match a.opts.mode {
            crate::Mode::Live => "live".into(),
            crate::Mode::Offline => "offline".into(),
        },
    }
}

fn config(a: &Agent) -> v1::SessionConfig {
    v1::SessionConfig {
        cpu: a.cpu_on.load(Ordering::SeqCst),
        cpu_interval_us: (a.interval_ns.load(Ordering::SeqCst) / 1000) as u32,
        alloc: a.alloc_on.load(Ordering::SeqCst),
        alloc_interval_bytes: a.opts.alloc_interval,
        monitors: a.monitor_on.load(Ordering::SeqCst),
        monitor_threshold_us: a.opts.monitor_threshold.as_micros() as u32,
        telemetry: true,
        thread_states: true,
        sampler: if a.async_active.load(Ordering::SeqCst) { "async".into() } else { "sync".into() },
    }
}

fn publish_info(a: &Agent, port: u16) {
    let path = a
        .opts
        .infofile
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("ember-{}.json", std::process::id())));
    let info = serde_json::json!({
        "pid": std::process::id(),
        "port": port,
        "bind": a.opts.bind,
        "token": a.token,
        "tls": a.tls.is_some(),
        "fingerprint": a.tls.as_ref().map(|t| t.fingerprint.clone()),
        "agent_version": AGENT_VERSION,
        "instance_id": a.instance_id.to_string(),
    });
    let tmp = path.with_extension("json.tmp");
    let write = || -> io::Result<()> {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp)?;
        f.write_all(info.to_string().as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    };
    match write() {
        Ok(()) => *a.info_file.lock() = Some(path),
        Err(e) => log(&format!("cannot write info file {}: {e}", path.display())),
    }
}

fn run(a: &'static Agent) {
    let Some(jni) = a.vm.attach_daemon("Ember Link") else {
        log("agent-link: cannot attach to the VM");
        return;
    };
    let listener = match TcpListener::bind((a.opts.bind.as_str(), a.opts.port)) {
        Ok(l) => l,
        Err(e) => {
            log(&format!("cannot listen on {}:{}: {e}", a.opts.bind, a.opts.port));
            return;
        }
    };
    let port = listener.local_addr().map(|s| s.port()).unwrap_or(0);
    publish_info(a, port);
    debug(&format!("agent-link listening on {}:{port}", a.opts.bind));
    for conn in listener.incoming() {
        if a.shutting_down.load(Ordering::SeqCst) {
            break;
        }
        let Ok(tcp) = conn else { continue };
        let peer = tcp.peer_addr().map(|p| p.to_string()).unwrap_or_default();
        let r = serve(a, jni, tcp);
        SESSION_ACTIVE.store(false, Ordering::SeqCst);
        if let Err(e) = r {
            debug(&format!("session with {peer} ended: {e}"));
        }
    }
}

fn serve(a: &'static Agent, jni: *mut jvmti_sys::JNIEnv, tcp: TcpStream) -> io::Result<()> {
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_millis(50)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    match &a.tls {
        Some(t) => {
            let conn = rustls::ServerConnection::new(t.config.clone()).map_err(io::Error::other)?;
            let mut s = rustls::StreamOwned::new(conn, tcp);
            session(a, jni, &mut s)
        }
        None => {
            let mut s = tcp;
            session(a, jni, &mut s)
        }
    }
}

/// Incremental frame reader over a stream with a read timeout.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    /// Reads whatever is available (blocking at most the stream's read timeout) and returns the
    /// complete frames.
    pub fn poll<S: Read>(&mut self, s: &mut S) -> io::Result<Vec<Frame>> {
        let mut tmp = [0u8; 64 * 1024];
        match s.read(&mut tmp) {
            Ok(0) => return Err(io::ErrorKind::ConnectionAborted.into()),
            Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
        let mut out = vec![];
        loop {
            if self.buf.len() < HEADER_LEN {
                break;
            }
            let h: [u8; HEADER_LEN] = self.buf[..HEADER_LEN].try_into().unwrap();
            let (len, ty, flags) = parse_header(&h)?;
            let end = HEADER_LEN + len as usize;
            if self.buf.len() < end {
                break;
            }
            let payload = self.buf[HEADER_LEN..end].to_vec();
            self.buf.drain(..end);
            out.push(Frame { ty, payload: decode_payload(flags, payload)? });
        }
        Ok(out)
    }
}

fn send<S: Write, M: Message>(s: &mut S, ty: FrameType, m: &M) -> io::Result<()> {
    s.write_all(&encode_frame(ty, m))?;
    s.flush()
}

fn expect<S: Read, M: Message + Default>(
    r: &mut FrameReader,
    s: &mut S,
    ty: FrameType,
    pending: &mut Vec<Frame>,
) -> io::Result<M> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if let Some(i) = pending.iter().position(|f| f.ty == ty as u16) {
            return pending.remove(i).decode();
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("handshake: no {ty:?}")));
        }
        pending.extend(r.poll(s)?);
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn session<S: Read + Write>(a: &'static Agent, jni: *mut jvmti_sys::JNIEnv, s: &mut S) -> io::Result<()> {
    let mut reader = FrameReader::default();
    let mut pending = vec![];
    send(s, FrameType::Hello, &hello(a))?;
    let auth: v1::Auth = expect(&mut reader, s, FrameType::Auth, &mut pending)?;
    if !ct_eq(auth.token.as_bytes(), a.token.as_bytes()) {
        let _ = send(s, FrameType::Error, &v1::Error { message: "invalid token".into(), fatal: true });
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "invalid token"));
    }
    let resume: v1::Resume = expect(&mut reader, s, FrameType::Resume, &mut pending)?;
    let resume_from = {
        let c = a.collector.lock();
        if resume.instance_id == a.instance_id {
            c.clamp_resume(&resume.watermark.unwrap_or_default())
        } else {
            c.clamp_resume(&v1::Watermark::default())
        }
    };
    send(s, FrameType::Welcome, &v1::Welcome { config: Some(config(a)), resume_from: Some(resume_from) })?;
    debug(&format!("daemon connected ({})", auth.client));
    SESSION_ACTIVE.store(true, Ordering::SeqCst);
    {
        let (lock, cv) = &a.connected;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    let mut sent = resume_from;
    let mut last_flush = Instant::now() - FLUSH_EVERY;
    loop {
        let frames: Vec<Frame> = std::mem::take(&mut pending).into_iter().chain(reader.poll(s)?).collect();
        for f in frames {
            match f.frame_type() {
                Some(FrameType::Ack) => {
                    let ack: v1::Ack = f.decode()?;
                    if let Some(w) = ack.watermark {
                        a.collector.lock().ack(&w);
                    }
                }
                Some(FrameType::Request) => handle_request(a, jni, s, f.decode()?)?,
                Some(FrameType::Ping) => {
                    send(s, FrameType::Pong, &v1::Ping { time: now_nanos() })?;
                }
                _ => {}
            }
        }
        let final_req = FINAL_REQUESTED.load(Ordering::SeqCst);
        if final_req || last_flush.elapsed() >= FLUSH_EVERY {
            last_flush = Instant::now();
            let batch = a.collector.lock().cut_batch(&sent, final_req, true);
            if let Some(b) = batch {
                let to = b.to.unwrap_or_default();
                send(s, FrameType::DataBatch, &b)?;
                sent = to;
            }
            if final_req {
                FINAL_DONE.store(true, Ordering::SeqCst);
                return Ok(());
            }
        }
    }
}

fn handle_request<S: Write>(
    a: &'static Agent,
    jni: *mut jvmti_sys::JNIEnv,
    s: &mut S,
    req: v1::Request,
) -> io::Result<()> {
    use v1::request::Kind;
    let mut resp = v1::Response { id: req.id, ok: true, ..Default::default() };
    let mut blob: Option<(u64, std::path::PathBuf)> = None;
    match req.kind {
        Some(Kind::ThreadDump(_)) => match a.java.get() {
            Some(h) => resp.result = Some(v1::response::Result::ThreadDump(h.thread_dump(jni))),
            None => fail(&mut resp, "thread dumps unavailable (no boot classes)"),
        },
        Some(Kind::HeapDump(r)) => match a.java.get() {
            Some(h) => {
                let local = !r.local_path.is_empty();
                let path = if local {
                    std::path::PathBuf::from(&r.local_path)
                } else {
                    std::env::temp_dir().join(format!("ember-heap-{}-{}.hprof", std::process::id(), req.id))
                };
                match h.dump_heap(jni, &path.display().to_string(), r.live_only) {
                    Ok(()) => {
                        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        let blob_id = if local { 0 } else { req.id.max(1) };
                        resp.result = Some(v1::response::Result::HeapDump(v1::HeapDumpResult {
                            blob_id,
                            size,
                            format: "hprof".into(),
                            local_path: if local { r.local_path.clone() } else { String::new() },
                        }));
                        if !local {
                            blob = Some((blob_id, path));
                        }
                    }
                    Err(e) => fail(&mut resp, &e),
                }
            }
            None => fail(&mut resp, "heap dumps unavailable (no boot classes)"),
        },
        Some(Kind::SetRecording(r)) => crate::events::set_recording(a, &r.kind, r.enabled, r.interval_us),
        Some(Kind::UpdateConfig(u)) => {
            if let Some(c) = u.config {
                crate::events::set_recording(a, "cpu", c.cpu, c.cpu_interval_us);
                crate::events::set_recording(a, "alloc", c.alloc, 0);
                crate::events::set_recording(a, "monitor", c.monitors, 0);
            }
        }
        Some(Kind::SaveSnapshot(r)) => match crate::recording::write(a, std::path::Path::new(&r.path)) {
            Ok(()) => resp.result = Some(v1::response::Result::Snapshot(v1::SnapshotResult { path: r.path })),
            Err(e) => fail(&mut resp, &e.to_string()),
        },
        Some(Kind::RunGc(_)) => {
            let e = jvmti!(a.jvmti, ForceGarbageCollection);
            if e != 0 {
                fail(&mut resp, &format!("ForceGarbageCollection failed: {e}"));
            }
        }
        Some(Kind::Bookmark(b)) => a.collector.lock().add_bookmark(ember_model::Bookmark {
            time: now_nanos(),
            label: b.label,
            color: (!b.color.is_empty()).then_some(b.color),
        }),
        None => fail(&mut resp, "empty request"),
    }
    send(s, FrameType::Response, &resp)?;
    if let Some((blob_id, path)) = blob {
        let r = stream_blob(s, blob_id, &path);
        let _ = std::fs::remove_file(&path);
        r?;
    }
    Ok(())
}

fn fail(resp: &mut v1::Response, msg: &str) {
    resp.ok = false;
    resp.error = msg.into();
}

fn stream_blob<S: Write>(s: &mut S, blob_id: u64, path: &std::path::Path) -> io::Result<()> {
    let mut f = std::fs::File::open(path)?;
    let mut offset = 0u64;
    let mut buf = vec![0u8; BLOB_CHUNK];
    loop {
        let n = read_full(&mut f, &mut buf)?;
        let last = n < buf.len();
        send(s, FrameType::BlobChunk, &v1::BlobChunk { blob_id, offset, data: buf[..n].to_vec(), last })?;
        offset += n as u64;
        if last {
            return Ok(());
        }
    }
}

fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_handles_partial_frames() {
        let bytes = encode_frame(FrameType::Ping, &v1::Ping { time: 7 });
        let mut r = FrameReader::default();
        let (a, b) = bytes.split_at(3);
        assert!(r.poll(&mut &a[..]).unwrap().is_empty());
        let f = r.poll(&mut &b[..]).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].decode::<v1::Ping>().unwrap().time, 7);
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd"));
    }
}
