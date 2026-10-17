//! Remote and container transports (plan D.9).
//!
//! A [`Target`] is a chain of hops (`ssh` → `docker exec` → …) ending where the JVM runs.
//! Everything is driven through exec channels: OpenProfiler uploads `openprofiler-relay` and the agent bundle
//! through stdin, runs `openprofiler-relay list|attach|dump` there, and reaches the agent's loopback port
//! through `openprofiler-relay relay` over the exec's stdio ([`Tunnel`]). No port is ever exposed.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

pub const READY: &str = "OPENPROFILER_RELAY_READY";
pub const RELAY_ERROR: &str = "OPENPROFILER_RELAY_ERROR";
/// Default directory for uploaded files on the remote side.
pub const REMOTE_DIR: &str = "/tmp/openprofiler";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hop {
    /// External OpenSSH client (supports ProxyJump, agent forwarding and the user's ssh config).
    Ssh {
        host: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        /// ProxyJump hosts, in order.
        #[serde(default)]
        jump: Vec<String>,
        #[serde(default)]
        identity: Option<PathBuf>,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Docker {
        container: String,
        #[serde(default)]
        user: Option<String>,
    },
    Kubectl {
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        namespace: Option<String>,
        pod: String,
        #[serde(default)]
        container: Option<String>,
        /// Run in an ephemeral debug container with this image that shares the container's
        /// process namespace (for distroless images without a shell). [`Target::resolve`]
        /// replaces the hop with one that execs into the debug container.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        debug_image: Option<String>,
    },
}

/// Image of the ephemeral debug container: it only needs `sh`, `uname` and `sha256sum`.
pub const DEFAULT_DEBUG_IMAGE: &str = "busybox:1.37";

/// A chain of hops. An empty chain is the local machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub hops: Vec<Hop>,
}

/// POSIX shell quoting.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@%+".contains(&b)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

impl Hop {
    /// Wraps `inner` (argv to run inside this hop) into the argv that runs it from outside.
    pub fn wrap(&self, inner: Vec<String>) -> Vec<String> {
        match self {
            Hop::Ssh { host, user, port, jump, identity, extra_args } => {
                let mut v = vec!["ssh".into(), "-o".into(), "BatchMode=yes".into(), "-T".into()];
                if let Some(p) = port {
                    v.extend(["-p".into(), p.to_string()]);
                }
                if !jump.is_empty() {
                    v.extend(["-J".into(), jump.join(",")]);
                }
                if let Some(i) = identity {
                    v.extend(["-i".into(), i.display().to_string()]);
                }
                v.extend(extra_args.iter().cloned());
                v.push(match user {
                    Some(u) => format!("{u}@{host}"),
                    None => host.clone(),
                });
                v.push("--".into());
                // The remote side runs this through the login shell, so quote once.
                v.push(shell_join(&inner));
                v
            }
            Hop::Docker { container, user } => {
                let mut v = vec!["docker".into(), "exec".into(), "-i".into()];
                if let Some(u) = user {
                    v.extend(["-u".into(), u.clone()]);
                }
                v.push(container.clone());
                v.extend(inner);
                v
            }
            Hop::Kubectl { context, namespace, pod, container, .. } => {
                let mut v = vec!["kubectl".into()];
                if let Some(c) = context {
                    v.extend(["--context".into(), c.clone()]);
                }
                if let Some(n) = namespace {
                    v.extend(["-n".into(), n.clone()]);
                }
                v.extend(["exec".into(), "-i".into(), pod.clone()]);
                if let Some(c) = container {
                    v.extend(["-c".into(), c.clone()]);
                }
                v.push("--".into());
                v.extend(inner);
                v
            }
        }
    }
}

impl Hop {
    /// `[user@]host[:port]`; earlier `jump` hosts become ProxyJump hops.
    pub fn ssh(spec: &str, jump: Vec<String>) -> Result<Self> {
        let (user, rest) = match spec.rsplit_once('@') {
            Some((u, r)) => (Some(u.to_owned()), r),
            None => (None, spec),
        };
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => {
                (h.to_owned(), Some(p.parse().with_context(|| format!("bad ssh port in {spec}"))?))
            }
            _ => (rest.to_owned(), None),
        };
        ensure!(!host.is_empty(), "empty ssh host in {spec:?}");
        Ok(Hop::Ssh { host, user, port, jump, identity: None, extra_args: vec![] })
    }

    /// `[namespace/]pod[/container]`.
    pub fn kubectl(spec: &str, context: Option<String>, debug_image: Option<String>) -> Result<Self> {
        let parts: Vec<&str> = spec.split('/').collect();
        let (namespace, pod, container) = match parts.as_slice() {
            [pod] => (None, *pod, None),
            [ns, pod] => (Some(*ns), *pod, None),
            [ns, pod, c] => (Some(*ns), *pod, Some(*c)),
            _ => bail!("expected [namespace/]pod[/container], got {spec:?}"),
        };
        ensure!(!pod.is_empty(), "empty pod name in {spec:?}");
        Ok(Hop::Kubectl {
            context,
            namespace: namespace.filter(|n| !n.is_empty()).map(str::to_owned),
            pod: pod.to_owned(),
            container: container.filter(|c| !c.is_empty()).map(str::to_owned),
            debug_image,
        })
    }
}

impl Target {
    pub fn local() -> Self {
        Self::default()
    }

    pub fn then(mut self, hop: Hop) -> Self {
        self.hops.push(hop);
        self
    }

    /// Local argv that runs `argv` at the end of the chain.
    pub fn command(&self, argv: &[String]) -> Vec<String> {
        self.hops.iter().rev().fold(argv.to_vec(), |inner, hop| hop.wrap(inner))
    }

    /// Like [`Target::command`] but runs the final command through `sh -c`.
    pub fn shell_command(&self, script: &str) -> Vec<String> {
        self.command(&["sh".into(), "-c".into(), script.into()])
    }

    pub fn describe(&self) -> String {
        if self.hops.is_empty() {
            return "local".into();
        }
        self.hops
            .iter()
            .map(|h| match h {
                Hop::Ssh { host, .. } => format!("ssh:{host}"),
                Hop::Docker { container, .. } => format!("docker:{container}"),
                Hop::Kubectl { namespace, pod, container, debug_image, .. } => format!(
                    "k8s:{}/{}{}{}",
                    namespace.as_deref().unwrap_or("default"),
                    pod,
                    container.as_ref().map(|c| format!("/{c}")).unwrap_or_default(),
                    if debug_image.is_some() { " (debug)" } else { "" }
                ),
            })
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// Replaces Kubernetes hops that ask for a debug container with hops into an ephemeral
    /// debug container (created, or reused if one is already running for that container).
    pub async fn resolve(&self) -> Result<Target> {
        let mut out = Target::local();
        for hop in &self.hops {
            let hop = match hop {
                Hop::Kubectl { context, namespace, pod, container, debug_image: Some(image) } => {
                    let ns = namespace.as_deref().unwrap_or("default");
                    let target = match container {
                        Some(c) => c.clone(),
                        None => kube_default_container(&out, context.as_deref(), ns, pod).await?,
                    };
                    kube_debug_container(&out, context.as_deref(), ns, pod, &target, image).await?
                }
                h => h.clone(),
            };
            out.hops.push(hop);
        }
        Ok(out)
    }

    fn tokio_command(&self, argv: &[String]) -> Command {
        let full = self.command(argv);
        let mut c = Command::new(&full[0]);
        c.args(&full[1..]).kill_on_drop(true);
        c
    }

    /// Runs `argv` remotely, feeding `stdin`, and returns stdout. Fails on non-zero exit.
    pub async fn exec(&self, argv: &[String], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut c = self.tokio_command(argv);
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().with_context(|| format!("cannot run {}", self.command(argv)[0]))?;
        if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
            si.write_all(data).await?;
            drop(si);
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!(
                "{} failed on {} ({}): {}",
                argv.first().map(String::as_str).unwrap_or(""),
                self.describe(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    pub async fn sh(&self, script: &str) -> Result<String> {
        let out = self.exec(&["sh".into(), "-c".into(), script.into()], None).await?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub fn spawn(&self, argv: &[String]) -> Result<Child> {
        let mut c = self.tokio_command(argv);
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(c.spawn()?)
    }

    pub async fn platform(&self) -> Result<Platform> {
        let s = self.sh("uname -sm; (ldd --version 2>&1 || true) | head -1").await?;
        Platform::parse(&s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub musl: bool,
}

impl Platform {
    fn parse(s: &str) -> Result<Self> {
        let mut lines = s.lines();
        let first = lines.next().ok_or_else(|| anyhow!("empty uname output"))?;
        let mut it = first.split_whitespace();
        let os = it.next().unwrap_or_default().to_lowercase();
        let arch = match it.next().unwrap_or_default() {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            a if a.starts_with("armv7") || a == "armhf" => "armv7",
            other => other,
        }
        .to_owned();
        let musl = lines.next().is_some_and(|l| l.to_lowercase().contains("musl"));
        Ok(Self { os, arch, musl })
    }

    /// `(relay triple, agent triple)`: the relay is always the static musl build; the agent must
    /// match the JVM's C library.
    pub fn bundle_triples(&self) -> Result<(String, String)> {
        let relay = self.triple()?;
        let agent =
            if self.os == "linux" && !self.musl { relay.replace("-musl", "-gnu") } else { relay.clone() };
        Ok((relay, agent))
    }

    /// Rust target triple of the relay. Linux always uses the static musl build.
    pub fn triple(&self) -> Result<String> {
        Ok(match (self.os.as_str(), self.arch.as_str()) {
            ("linux", "x86_64") => "x86_64-unknown-linux-musl".into(),
            ("linux", "aarch64") => "aarch64-unknown-linux-musl".into(),
            ("linux", "armv7") => "armv7-unknown-linux-musleabihf".into(),
            ("darwin", "aarch64") => "aarch64-apple-darwin".into(),
            ("darwin", "x86_64") => "x86_64-apple-darwin".into(),
            (os, arch) => bail!("unsupported remote platform {os}/{arch}"),
        })
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&data)))
}

/// Uploads `local` to `remote` through the exec channel's stdin and verifies its SHA-256.
/// Skips the transfer when the remote file already has the right hash.
pub async fn upload(target: &Target, local: &Path, remote: &str, executable: bool) -> Result<()> {
    let want = sha256_file(local)?;
    let q = shell_quote(remote);
    let hash_cmd = format!(
        "if command -v sha256sum >/dev/null 2>&1; then sha256sum {q}; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 {q}; fi 2>/dev/null | cut -d' ' -f1"
    );
    let have = target.sh(&format!("[ -f {q} ] && {{ {hash_cmd}; }} || true")).await?;
    if have.trim() == want {
        return Ok(());
    }
    let dir = Path::new(remote).parent().map(|p| p.display().to_string()).unwrap_or_else(|| ".".into());
    let mode = if executable { "755" } else { "644" };
    let script = format!(
        "umask 077 && mkdir -p {d} && cat > {q}.part && chmod {mode} {q}.part && mv {q}.part {q} && {{ {hash_cmd}; }}",
        d = shell_quote(&dir)
    );
    let data = tokio::fs::read(local).await?;
    let out = target.exec(&["sh".into(), "-c".into(), script], Some(&data)).await?;
    let got = String::from_utf8_lossy(&out).trim().to_owned();
    if got.is_empty() {
        tracing::warn!("no sha256 tool on {}; skipped upload verification", target.describe());
    } else {
        ensure!(got == want, "upload verification failed for {remote}: expected {want}, got {got}");
    }
    Ok(())
}

/// Files OpenProfiler needs on the remote side, from a local bundle directory laid out as
/// `<bundle>/<triple>/{openprofiler-relay, libopenprofiler_agent.so}`.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteBundle {
    pub relay: String,
    pub platform: Platform,
    #[serde(skip)]
    local: PathBuf,
}

fn remote_dir() -> String {
    format!("{REMOTE_DIR}/{}", env!("CARGO_PKG_VERSION"))
}

/// Uploads the relay for the target's platform.
pub async fn ensure_bundle(target: &Target, bundle_dir: &Path) -> Result<RemoteBundle> {
    let platform = target.platform().await?;
    let relay_triple = platform.triple()?;
    let relay_local = bundle_dir.join(&relay_triple).join("openprofiler-relay");
    ensure!(
        relay_local.exists(),
        "no openprofiler-relay for {relay_triple} in {} (run scripts/build-bundle.sh)",
        bundle_dir.display()
    );
    let relay = format!("{}/openprofiler-relay", remote_dir());
    upload(target, &relay_local, &relay, true).await?;
    Ok(RemoteBundle { relay, platform, local: bundle_dir.to_owned() })
}

impl RemoteBundle {
    /// Uploads the agent build that matches the C library of JVM `pid` and returns its remote
    /// path. The JVM's libc can differ from the shell's (an Alpine debug container next to a
    /// Debian-based JVM), so it is read from the JVM's root, falling back to the shell's.
    pub async fn agent_for(&self, target: &Target, pid: u32) -> Result<String> {
        let mut platform = self.platform.clone();
        if let Some(musl) = jvm_musl(target, pid).await {
            platform.musl = musl;
        }
        let (_, agent_triple) = platform.bundle_triples()?;
        let name =
            if platform.os == "darwin" { "libopenprofiler_agent.dylib" } else { "libopenprofiler_agent.so" };
        let local = self.local.join(&agent_triple).join(name);
        ensure!(
            local.exists(),
            "no agent for {agent_triple} in {} (run scripts/build-bundle.sh)",
            self.local.display()
        );
        // Both Linux builds may be needed on one host (Alpine and Debian containers).
        let remote = match (platform.os.as_str(), platform.musl) {
            ("linux", true) => format!("{}/libopenprofiler_agent-musl.so", remote_dir()),
            _ => format!("{}/{name}", remote_dir()),
        };
        upload(target, &local, &remote, false).await?;
        Ok(remote)
    }
}

/// Whether JVM `pid` links against musl, from the dynamic loader in its root filesystem.
/// `None` when that can't be told (no `/proc`, no access).
pub async fn jvm_musl(target: &Target, pid: u32) -> Option<bool> {
    let script = format!(
        "r=/proc/{pid}/root; for d in lib lib64 usr/lib; do ls \"$r/$d\" 2>/dev/null; done | grep -E '^ld-(musl|linux)' | head -1"
    );
    let out = target.sh(&script).await.ok()?;
    let out = out.trim();
    if out.starts_with("ld-musl") {
        Some(true)
    } else if out.starts_with("ld-linux") {
        Some(false)
    } else {
        None
    }
}

/// Runs a relay subcommand and parses its JSON output.
pub async fn relay_json(target: &Target, relay: &str, args: &[&str]) -> Result<serde_json::Value> {
    let mut argv = vec![relay.to_owned()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let out = target.exec(&argv, None).await?;
    let text = String::from_utf8_lossy(&out);
    if let Some(err) = text.lines().find_map(|l| l.strip_prefix(RELAY_ERROR)) {
        bail!("openprofiler-relay: {}", err.trim());
    }
    Ok(serde_json::from_str(text.trim())?)
}

pub async fn list_jvms(target: &Target, relay: &str) -> Result<serde_json::Value> {
    relay_json(target, relay, &["list"]).await
}

pub async fn attach(
    target: &Target,
    relay: &str,
    pid: u32,
    agent: &str,
    options: &str,
) -> Result<serde_json::Value> {
    relay_json(target, relay, &["attach", &pid.to_string(), agent, options]).await
}

/// Streams a heap dump of `pid` into `dest` (via `openprofiler-relay dump`).
pub async fn dump_heap(target: &Target, relay: &str, pid: u32, dest: &Path) -> Result<u64> {
    let mut child = target.spawn(&[relay.to_owned(), "dump".into(), pid.to_string()])?;
    drop(child.stdin.take());
    let mut out = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut file = tokio::fs::File::create(dest).await?;
    let n = tokio::io::copy(&mut out, &mut file).await?;
    let status = child.wait().await?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err).await;
        }
        bail!("remote heap dump failed: {}", err.trim());
    }
    Ok(n)
}

/// A local TCP endpoint that forwards each connection through a fresh `openprofiler-relay relay` process
/// on the target. Connect the agent-link client to [`Tunnel::local_addr`].
pub struct Tunnel {
    pub local_addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Tunnel {
    pub async fn open(target: Target, relay: String, remote_port: u16) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (t, r) = (target.clone(), relay.clone());
                tokio::spawn(async move {
                    if let Err(e) = splice(&t, &r, remote_port, sock).await {
                        tracing::warn!("tunnel to {} failed: {e:#}", t.describe());
                    }
                });
            }
        });
        Ok(Self { local_addr, task })
    }
}

async fn splice(target: &Target, relay: &str, port: u16, sock: tokio::net::TcpStream) -> Result<()> {
    let mut child = target.spawn(&[relay.to_owned(), "relay".into(), format!("127.0.0.1:{port}")])?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?);
    // Wait for the READY line; anything before it (e.g. login banners) is skipped.
    let mut line = String::new();
    loop {
        line.clear();
        if stdout.read_line(&mut line).await? == 0 {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err).await;
            }
            bail!("relay exited before READY: {}", err.trim());
        }
        if line.starts_with(READY) {
            break;
        }
        if let Some(e) = line.strip_prefix(RELAY_ERROR) {
            bail!("relay: {}", e.trim());
        }
    }
    let (mut rd, mut wr) = sock.into_split();
    let up = async move {
        let _ = tokio::io::copy(&mut rd, &mut stdin).await;
        // `shutdown` on a child pipe only flushes; dropping it delivers EOF to the relay.
        drop(stdin);
    };
    let down = async {
        let _ = tokio::io::copy(&mut stdout, &mut wr).await;
        let _ = wr.shutdown().await;
    };
    tokio::join!(up, down);
    let _ = child.kill().await;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Discovery helpers for the UI pickers.

#[derive(Debug, Clone, Serialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
}

/// `docker ps` on the target (which may be a remote host reached over SSH).
pub async fn docker_containers(host: &Target) -> Result<Vec<Container>> {
    let out =
        host.exec(&["docker".into(), "ps".into(), "--format".into(), "{{json .}}".into()], None).await?;
    let mut v = vec![];
    for line in String::from_utf8_lossy(&out).lines().filter(|l| !l.trim().is_empty()) {
        let j: serde_json::Value = serde_json::from_str(line)?;
        let s = |k: &str| j[k].as_str().unwrap_or_default().to_owned();
        v.push(Container { id: s("ID"), name: s("Names"), image: s("Image"), status: s("Status") });
    }
    Ok(v)
}

#[derive(Debug, Clone, Serialize)]
pub struct Pod {
    pub namespace: String,
    pub name: String,
    pub phase: String,
    pub containers: Vec<String>,
    /// Image of each container, in the order of `containers`.
    pub images: Vec<String>,
    pub node: Option<String>,
}

fn kubectl(context: Option<&str>, args: &[&str]) -> Vec<String> {
    let mut v = vec!["kubectl".to_owned()];
    if let Some(c) = context {
        v.extend(["--context".into(), c.into()]);
    }
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

pub async fn kube_contexts(host: &Target) -> Result<Vec<String>> {
    let out = host.exec(&kubectl(None, &["config", "get-contexts", "-o", "name"]), None).await?;
    Ok(String::from_utf8_lossy(&out).lines().map(str::to_owned).filter(|s| !s.is_empty()).collect())
}

pub async fn kube_pods(host: &Target, context: Option<&str>, namespace: Option<&str>) -> Result<Vec<Pod>> {
    let mut args = vec!["get", "pods", "-o", "json"];
    match namespace {
        Some(n) => args.extend(["-n", n]),
        None => args.push("-A"),
    }
    let out = host.exec(&kubectl(context, &args), None).await?;
    let j: serde_json::Value = serde_json::from_slice(&out)?;
    Ok(j["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|p| Pod {
                    namespace: p["metadata"]["namespace"].as_str().unwrap_or_default().into(),
                    name: p["metadata"]["name"].as_str().unwrap_or_default().into(),
                    phase: p["status"]["phase"].as_str().unwrap_or_default().into(),
                    containers: p["spec"]["containers"]
                        .as_array()
                        .map(|c| c.iter().filter_map(|c| c["name"].as_str().map(str::to_owned)).collect())
                        .unwrap_or_default(),
                    images: p["spec"]["containers"]
                        .as_array()
                        .map(|c| {
                            c.iter().map(|c| c["image"].as_str().unwrap_or_default().to_owned()).collect()
                        })
                        .unwrap_or_default(),
                    node: p["spec"]["nodeName"].as_str().map(str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn kube_pod_json(
    host: &Target,
    context: Option<&str>,
    namespace: &str,
    pod: &str,
) -> Result<serde_json::Value> {
    let out = host.exec(&kubectl(context, &["get", "pod", "-n", namespace, pod, "-o", "json"]), None).await?;
    Ok(serde_json::from_slice(&out)?)
}

/// The pod's first container (what `kubectl exec` picks without `-c`).
async fn kube_default_container(
    host: &Target,
    context: Option<&str>,
    namespace: &str,
    pod: &str,
) -> Result<String> {
    let p = kube_pod_json(host, context, namespace, pod).await?;
    p["spec"]["containers"][0]["name"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("pod {namespace}/{pod} has no containers"))
}

/// A running OpenProfiler debug container of `pod` that targets `target_container`.
fn running_debug_container(pod: &serde_json::Value, target_container: &str) -> Option<String> {
    let running = |name: &str| {
        pod["status"]["ephemeralContainerStatuses"]
            .as_array()
            .is_some_and(|st| st.iter().any(|s| s["name"] == name && s["state"]["running"].is_object()))
    };
    pod["spec"]["ephemeralContainers"]
        .as_array()?
        .iter()
        .filter(|c| c["targetContainerName"] == target_container)
        .filter_map(|c| c["name"].as_str())
        .filter(|n| n.starts_with("openprofiler-debug"))
        .find(|n| running(n))
        .map(str::to_owned)
}

/// Adds an ephemeral debug container sharing the target container's process namespace (for
/// distroless images without a shell) and returns a hop that execs into it. A running one left by
/// an earlier attach is reused, since ephemeral containers can't be removed from a pod. The JVM's
/// files are reachable under `/proc/<pid>/root`, which `openprofiler-relay attach` handles.
///
/// The container gets the `general` profile (`CAP_SYS_PTRACE`), which reading another user's
/// `/proc/<pid>/root` needs.
pub async fn kube_debug_container(
    host: &Target,
    context: Option<&str>,
    namespace: &str,
    pod: &str,
    target_container: &str,
    image: &str,
) -> Result<Hop> {
    let hop = |name: String| Hop::Kubectl {
        context: context.map(str::to_owned),
        namespace: Some(namespace.to_owned()),
        pod: pod.to_owned(),
        container: Some(name),
        debug_image: None,
    };
    let p = kube_pod_json(host, context, namespace, pod).await?;
    if let Some(name) = running_debug_container(&p, target_container) {
        return Ok(hop(name));
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let name = format!("openprofiler-debug-{secs}");
    let target_arg = format!("--target={target_container}");
    let image_arg = format!("--image={image}");
    let container_arg = format!("--container={name}");
    host.exec(
        &kubectl(
            context,
            &[
                "debug",
                "-n",
                namespace,
                pod,
                &image_arg,
                &target_arg,
                &container_arg,
                "--profile=general",
                "--",
                "sleep",
                "2147483647",
            ],
        ),
        None,
    )
    .await?;
    // Wait until the ephemeral container runs (the image may need to be pulled).
    for _ in 0..240 {
        let p = kube_pod_json(host, context, namespace, pod).await?;
        if running_debug_container(&p, target_container).as_deref() == Some(name.as_str()) {
            return Ok(hop(name));
        }
        let waiting = p["status"]["ephemeralContainerStatuses"]
            .as_array()
            .and_then(|st| st.iter().find(|s| s["name"] == name.as_str()))
            .and_then(|s| s["state"]["waiting"]["reason"].as_str().map(str::to_owned));
        if let Some(r) =
            waiting.filter(|r| r.contains("ErrImage") || r.contains("BackOff") || r.contains("ConfigError"))
        {
            bail!("debug container {name} cannot start: {r}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    bail!("ephemeral debug container {name} did not start within 2 minutes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc-1.2/x"), "abc-1.2/x");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn hop_chains() {
        let t = Target::local()
            .then(Hop::Ssh {
                host: "box".into(),
                user: Some("me".into()),
                port: Some(2222),
                jump: vec!["bastion".into()],
                identity: None,
                extra_args: vec![],
            })
            .then(Hop::Docker { container: "app".into(), user: None });
        let argv = t.command(&[
            "/tmp/openprofiler/openprofiler-relay".into(),
            "relay".into(),
            "127.0.0.1:9000".into(),
        ]);
        assert_eq!(&argv[..4], &["ssh", "-o", "BatchMode=yes", "-T"]);
        assert!(argv.contains(&"me@box".to_owned()));
        assert!(argv.contains(&"bastion".to_owned()));
        assert_eq!(
            argv.last().unwrap(),
            "docker exec -i app /tmp/openprofiler/openprofiler-relay relay 127.0.0.1:9000"
        );
        let k = Target::local().then(Hop::Kubectl {
            context: None,
            namespace: Some("prod".into()),
            pod: "api-1".into(),
            container: Some("app".into()),
            debug_image: None,
        });
        assert_eq!(
            k.command(&["ls".into()]),
            ["kubectl", "-n", "prod", "exec", "-i", "api-1", "-c", "app", "--", "ls"]
        );
        assert_eq!(k.describe(), "k8s:prod/api-1/app");
    }

    #[test]
    fn hop_specs() {
        assert_eq!(
            Hop::ssh("me@box:2222", vec!["bastion".into()]).unwrap(),
            Hop::Ssh {
                host: "box".into(),
                user: Some("me".into()),
                port: Some(2222),
                jump: vec!["bastion".into()],
                identity: None,
                extra_args: vec![],
            }
        );
        assert!(matches!(Hop::ssh("box", vec![]).unwrap(), Hop::Ssh { user: None, port: None, .. }));
        assert!(Hop::ssh("box:x", vec![]).is_err());
        let k = Hop::kubectl("prod/api-1/app", None, Some(DEFAULT_DEBUG_IMAGE.into())).unwrap();
        assert!(
            matches!(&k, Hop::Kubectl { namespace: Some(n), pod, container: Some(c), debug_image: Some(_), .. }
            if n == "prod" && pod == "api-1" && c == "app")
        );
        assert!(matches!(Hop::kubectl("api-1", None, None).unwrap(), Hop::Kubectl { namespace: None, .. }));
        assert!(Hop::kubectl("a/b/c/d", None, None).is_err());
        assert_eq!(Target::local().then(k).describe(), "k8s:prod/api-1/app (debug)");
    }

    #[test]
    fn finds_running_debug_container() {
        let pod = serde_json::json!({
            "spec": {"ephemeralContainers": [
                {"name": "openprofiler-debug-1", "targetContainerName": "app"},
                {"name": "openprofiler-debug-2", "targetContainerName": "app"},
                {"name": "openprofiler-debug-3", "targetContainerName": "sidecar"},
            ]},
            "status": {"ephemeralContainerStatuses": [
                {"name": "openprofiler-debug-1", "state": {"terminated": {}}},
                {"name": "openprofiler-debug-2", "state": {"running": {}}},
                {"name": "openprofiler-debug-3", "state": {"running": {}}},
            ]},
        });
        assert_eq!(running_debug_container(&pod, "app").as_deref(), Some("openprofiler-debug-2"));
        assert_eq!(running_debug_container(&pod, "other"), None);
    }

    #[test]
    fn platforms() {
        let p = Platform::parse("Linux x86_64\nmusl libc (x86_64)\n").unwrap();
        assert!(p.musl);
        assert_eq!(p.triple().unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(Platform::parse("Darwin arm64\n").unwrap().triple().unwrap(), "aarch64-apple-darwin");
    }

    #[tokio::test]
    async fn local_upload_and_exec() {
        let dir = std::env::temp_dir().join(format!("openprofiler-remote-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, b"hello openprofiler").unwrap();
        let dest = dir.join("sub/dest.bin");
        let t = Target::local();
        upload(&t, &src, dest.to_str().unwrap(), true).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello openprofiler");
        // Second upload is a no-op (hash matches).
        upload(&t, &src, dest.to_str().unwrap(), true).await.unwrap();
        let p = t.platform().await.unwrap();
        assert!(p.triple().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
