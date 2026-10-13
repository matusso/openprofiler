//! Remote and container transports (plan D.9).
//!
//! A [`Target`] is a chain of hops (`ssh` → `docker exec` → …) ending where the JVM runs.
//! Everything is driven through exec channels: Ember uploads `ember-relay` and the agent bundle
//! through stdin, runs `ember-relay list|attach|dump` there, and reaches the agent's loopback port
//! through `ember-relay relay` over the exec's stdio ([`Tunnel`]). No port is ever exposed.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

pub const READY: &str = "EMBER_RELAY_READY";
pub const RELAY_ERROR: &str = "EMBER_RELAY_ERROR";
/// Default directory for uploaded files on the remote side.
pub const REMOTE_DIR: &str = "/tmp/ember";

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
    },
}

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
            Hop::Kubectl { context, namespace, pod, container } => {
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
                Hop::Kubectl { namespace, pod, container, .. } => format!(
                    "k8s:{}/{}{}",
                    namespace.as_deref().unwrap_or("default"),
                    pod,
                    container.as_ref().map(|c| format!("/{c}")).unwrap_or_default()
                ),
            })
            .collect::<Vec<_>>()
            .join(" → ")
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

/// Files Ember needs on the remote side, from a local bundle directory laid out as
/// `<bundle>/<triple>/{ember-relay, libember_agent.so, ember-boot.jar}`.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteBundle {
    pub relay: String,
    pub agent: String,
    pub platform: Platform,
}

pub async fn ensure_bundle(target: &Target, bundle_dir: &Path) -> Result<RemoteBundle> {
    let platform = target.platform().await?;
    let (relay_triple, agent_triple) = platform.bundle_triples()?;
    let agent_name = if platform.os == "darwin" { "libember_agent.dylib" } else { "libember_agent.so" };
    let relay_local = bundle_dir.join(&relay_triple).join("ember-relay");
    let agent_local = bundle_dir.join(&agent_triple).join(agent_name);
    ensure!(
        relay_local.exists(),
        "no ember-relay for {relay_triple} in {} (run scripts/build-bundle.sh)",
        bundle_dir.display()
    );
    let dir = format!("{REMOTE_DIR}/{}", env!("CARGO_PKG_VERSION"));
    let relay = format!("{dir}/ember-relay");
    let agent = format!("{dir}/{agent_name}");
    upload(target, &relay_local, &relay, true).await?;
    if agent_local.exists() {
        upload(target, &agent_local, &agent, false).await?;
    } else {
        tracing::warn!("no agent for {agent_triple} in {}; attach will fail", bundle_dir.display());
    }
    Ok(RemoteBundle { relay, agent, platform })
}

/// Runs a relay subcommand and parses its JSON output.
pub async fn relay_json(target: &Target, relay: &str, args: &[&str]) -> Result<serde_json::Value> {
    let mut argv = vec![relay.to_owned()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let out = target.exec(&argv, None).await?;
    let text = String::from_utf8_lossy(&out);
    if let Some(err) = text.lines().find_map(|l| l.strip_prefix(RELAY_ERROR)) {
        bail!("ember-relay: {}", err.trim());
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

/// Streams a heap dump of `pid` into `dest` (via `ember-relay dump`).
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

/// A local TCP endpoint that forwards each connection through a fresh `ember-relay relay` process
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
                    node: p["spec"]["nodeName"].as_str().map(str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Adds an ephemeral debug container sharing the target container's process namespace (for
/// distroless images without a shell) and returns a hop that execs into it. The JVM's files are
/// then reachable under `/proc/<pid>/root`, which `ember-relay attach` handles.
pub async fn kube_debug_container(
    host: &Target,
    context: Option<&str>,
    namespace: &str,
    pod: &str,
    target_container: &str,
    image: &str,
) -> Result<Hop> {
    let name = format!("ember-debug-{}", std::process::id());
    let target_arg = format!("--target={target_container}");
    let image_arg = format!("--image={image}");
    let container_arg = format!("--container={name}");
    host.exec(
        &kubectl(
            context,
            &["debug", "-n", namespace, pod, &image_arg, &target_arg, &container_arg, "--", "sleep", "86400"],
        ),
        None,
    )
    .await?;
    // Wait until the ephemeral container runs.
    for _ in 0..60 {
        let out = host
            .exec(
                &kubectl(
                    context,
                    &[
                        "get",
                        "pod",
                        "-n",
                        namespace,
                        pod,
                        "-o",
                        &format!("jsonpath={{.status.ephemeralContainerStatuses[?(@.name==\"{name}\")].state.running}}"),
                    ],
                ),
                None,
            )
            .await?;
        if !out.is_empty() {
            return Ok(Hop::Kubectl {
                context: context.map(str::to_owned),
                namespace: Some(namespace.to_owned()),
                pod: pod.to_owned(),
                container: Some(name),
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    bail!("ephemeral debug container {name} did not start")
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
        let argv = t.command(&["/tmp/ember/ember-relay".into(), "relay".into(), "127.0.0.1:9000".into()]);
        assert_eq!(&argv[..4], &["ssh", "-o", "BatchMode=yes", "-T"]);
        assert!(argv.contains(&"me@box".to_owned()));
        assert!(argv.contains(&"bastion".to_owned()));
        assert_eq!(argv.last().unwrap(), "docker exec -i app /tmp/ember/ember-relay relay 127.0.0.1:9000");
        let k = Target::local().then(Hop::Kubectl {
            context: None,
            namespace: Some("prod".into()),
            pod: "api-1".into(),
            container: Some("app".into()),
        });
        assert_eq!(
            k.command(&["ls".into()]),
            ["kubectl", "-n", "prod", "exec", "-i", "api-1", "-c", "app", "--", "ls"]
        );
        assert_eq!(k.describe(), "k8s:prod/api-1/app");
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
        let dir = std::env::temp_dir().join(format!("ember-remote-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, b"hello ember").unwrap();
        let dest = dir.join("sub/dest.bin");
        let t = Target::local();
        upload(&t, &src, dest.to_str().unwrap(), true).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello ember");
        // Second upload is a no-op (hash matches).
        upload(&t, &src, dest.to_str().unwrap(), true).await.unwrap();
        let p = t.platform().await.unwrap();
        assert!(p.triple().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
