//! Local JVM discovery and HotSpot dynamic attach.
//!
//! * [`list_jvms`] finds JVMs through their `hsperfdata` files (and, on Linux, `/proc` so JVMs in
//!   containers or with `-XX:-UsePerfData` are found too).
//! * [`attach_agent`] loads the OpenProfiler agent into a running JVM and returns how to reach it.
//! * [`jcmd`] runs diagnostic commands over the same attach socket.
//!
//! The HotSpot attach protocol: create `.attach_pid<pid>`, send `SIGQUIT`, wait for the target to
//! open the `.java_pid<pid>` UNIX socket in its temp directory, then send
//! `1\0<cmd>\0<arg1>\0<arg2>\0<arg3>\0` and read `<status>\n<output>`. In containers the target's
//! namespace PID (`NSpid`) and its root (`/proc/<pid>/root`) are used.

pub mod hsperf;

#[cfg(unix)]
use anyhow::anyhow;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A JVM running on this host (fields match the Query API `LocalJvm`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalJvm {
    pub pid: u32,
    pub main_class: String,
    pub args: String,
    pub vm_version: Option<String>,
    pub attachable: bool,
    pub profiled: bool,
    pub user: Option<String>,
}

/// How to reach an agent after attaching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachResult {
    pub pid: u32,
    pub port: u16,
    pub bind: String,
    pub token: String,
    pub tls: bool,
    pub fingerprint: Option<String>,
    /// Where the agent's info file was read from (host path).
    pub info_path: PathBuf,
}

impl AttachResult {
    pub fn addr(&self) -> String {
        let host = match self.bind.as_str() {
            "" | "0.0.0.0" => "127.0.0.1",
            h => h,
        };
        format!("{host}:{}", self.port)
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence/permission.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// Candidate directories holding `hsperfdata_<user>` directories.
fn perf_roots() -> Vec<PathBuf> {
    let mut v = vec![std::env::temp_dir()];
    let tmp = PathBuf::from("/tmp");
    if !v.contains(&tmp) {
        v.push(tmp);
    }
    v
}

fn split_command(cmd: &str) -> (String, String) {
    match cmd.split_once(' ') {
        Some((m, a)) => (m.to_string(), a.to_string()),
        None => (cmd.to_string(), String::new()),
    }
}

fn jvm_from_perf(pid: u32, data: &[u8], user: Option<String>) -> Option<LocalJvm> {
    let m = hsperf::parse(data).ok()?;
    let get = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (main_class, args) = split_command(&get("sun.rt.javaCommand"));
    let vm_args = get("java.rt.vmArgs");
    let vm_flags = get("java.rt.vmFlags");
    let attachable =
        !vm_args.contains("DisableAttachMechanism") && !vm_flags.contains("+DisableAttachMechanism");
    let vm_version = Some(get("java.property.java.vm.version")).filter(|s| !s.is_empty());
    Some(LocalJvm {
        pid,
        main_class,
        args,
        vm_version,
        attachable,
        profiled: AgentInfoFile::default_path(pid).exists() || vm_args.contains("openprofiler_agent"),
        user,
    })
}

/// Lists JVMs visible to the current user.
pub fn list_jvms() -> Vec<LocalJvm> {
    let mut out: Vec<LocalJvm> = vec![];
    let me = std::process::id();
    for root in perf_roots() {
        let Ok(dirs) = std::fs::read_dir(&root) else { continue };
        for d in dirs.flatten() {
            let name = d.file_name().to_string_lossy().into_owned();
            let Some(user) = name.strip_prefix("hsperfdata_") else { continue };
            let Ok(files) = std::fs::read_dir(d.path()) else { continue };
            for f in files.flatten() {
                let Some(pid) = f.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
                if pid == me || out.iter().any(|j| j.pid == pid) || !process_alive(pid) {
                    continue;
                }
                if let Ok(data) = std::fs::read(f.path())
                    && let Some(j) = jvm_from_perf(pid, &data, Some(user.to_string()))
                {
                    out.push(j);
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    linux::scan_proc(&mut out);
    out.sort_by_key(|j| j.pid);
    out
}

/// The agent's published `{port, token, ...}` file.
#[derive(Debug, Clone, Deserialize)]
struct AgentInfoFile {
    port: u16,
    #[serde(default)]
    bind: String,
    token: String,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    fingerprint: Option<String>,
}

impl AgentInfoFile {
    fn into_result(self, pid: u32, info_path: PathBuf) -> AttachResult {
        AttachResult {
            pid,
            port: self.port,
            bind: self.bind,
            token: self.token,
            tls: self.tls,
            fingerprint: self.fingerprint,
            info_path,
        }
    }

    fn default_path(pid: u32) -> PathBuf {
        std::env::temp_dir().join(format!("openprofiler-{pid}.json"))
    }
}

/// Where the target sees its temp dir, as a host path, plus the PID inside its namespace.
struct Target {
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: u32,
    ns_pid: u32,
    /// Host path of the target's `/tmp` (or `$TMPDIR` on macOS).
    tmp: PathBuf,
    /// Host path of the target's root, if it differs from ours (container).
    root: Option<PathBuf>,
}

fn target(pid: u32) -> Target {
    #[cfg(target_os = "linux")]
    {
        linux::target(pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Target { pid, ns_pid: pid, tmp: std::env::temp_dir(), root: None }
    }
}

#[cfg(unix)]
fn connect_attach_socket(t: &Target, timeout: Duration) -> Result<UnixStream> {
    let sock = t.tmp.join(format!(".java_pid{}", t.ns_pid));
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    // Trigger the attach listener: .attach_pid file + SIGQUIT.
    let mut candidates = vec![];
    #[cfg(target_os = "linux")]
    candidates.push(PathBuf::from(format!("/proc/{}/cwd/.attach_pid{}", t.pid, t.ns_pid)));
    candidates.push(t.tmp.join(format!(".attach_pid{}", t.ns_pid)));
    let attach_file = candidates.into_iter().find(|p| std::fs::File::create(p).is_ok());
    // SAFETY: sending SIGQUIT to the target JVM makes it start its attach listener.
    if unsafe { libc::kill(t.pid as libc::pid_t, libc::SIGQUIT) } != 0 {
        if let Some(f) = &attach_file {
            let _ = std::fs::remove_file(f);
        }
        bail!("cannot signal process {}: {}", t.pid, std::io::Error::last_os_error());
    }
    let deadline = Instant::now() + timeout;
    let mut wait = Duration::from_millis(20);
    let result = loop {
        if let Ok(s) = UnixStream::connect(&sock) {
            break Ok(s);
        }
        if Instant::now() > deadline {
            break Err(anyhow!("JVM {} did not open its attach socket ({})", t.pid, sock.display()));
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(200));
    };
    if let Some(f) = &attach_file {
        let _ = std::fs::remove_file(f);
    }
    result
}

/// Sends one attach-protocol command and returns `(status, output)`.
#[cfg(unix)]
pub fn attach_command(pid: u32, cmd: &str, args: &[&str]) -> Result<(i32, String)> {
    let t = target(pid);
    let mut s = {
        #[cfg(target_os = "linux")]
        let _creds = linux::AsTargetUser::switch(pid);
        connect_attach_socket(&t, Duration::from_secs(10))?
    };
    let mut req = Vec::new();
    req.extend_from_slice(b"1\0");
    req.extend_from_slice(cmd.as_bytes());
    req.push(0);
    for i in 0..3 {
        req.extend_from_slice(args.get(i).copied().unwrap_or("").as_bytes());
        req.push(0);
    }
    s.write_all(&req)?;
    s.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    let (code, rest) = resp.split_once('\n').unwrap_or((resp.as_str(), ""));
    let code = code.trim().parse().with_context(|| format!("bad attach response: {resp:?}"))?;
    Ok((code, rest.to_string()))
}

/// HotSpot attach on Windows goes through remote thread injection, which is not implemented.
#[cfg(not(unix))]
pub fn attach_command(_pid: u32, _cmd: &str, _args: &[&str]) -> Result<(i32, String)> {
    bail!("attaching to local JVMs is not supported on this platform")
}

/// Runs a diagnostic command (`Thread.print`, `GC.heap_dump /path`, `VM.version`, …).
pub fn jcmd(pid: u32, command: &str) -> Result<String> {
    let (code, out) = attach_command(pid, "jcmd", &[command])?;
    if code != 0 {
        bail!("jcmd failed ({code}): {}", out.trim());
    }
    Ok(out)
}

/// Loads a native agent library (`load <path> true <options>`). Returns the agent's
/// `Agent_OnAttach` result.
pub fn load_agent(pid: u32, agent_path: &Path, options: &str) -> Result<i32> {
    let path = agent_path.to_str().context("agent path is not UTF-8")?;
    let (code, out) = attach_command(pid, "load", &[path, "true", options])?;
    if code != 0 {
        bail!("attach failed ({code}): {}", out.trim());
    }
    // JDK 9+: "return code: N"; older: "N".
    let rc = out.trim().rsplit([' ', '\n']).next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok(rc)
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    hex::encode(b)
}

/// Copies the agent into the target's mount namespace when it runs in a container.
fn agent_path_for(t: &Target, agent: &Path) -> Result<(PathBuf, String)> {
    match &t.root {
        None => {
            let abs = std::fs::canonicalize(agent).with_context(|| format!("agent {}", agent.display()))?;
            let s = abs.display().to_string();
            Ok((abs, s))
        }
        Some(_) => {
            let dir = t.tmp.join("openprofiler");
            std::fs::create_dir_all(&dir)?;
            let name = agent.file_name().context("agent file name")?;
            let host = dir.join(name);
            std::fs::copy(agent, &host)?;
            Ok((host, format!("/tmp/openprofiler/{}", name.to_string_lossy())))
        }
    }
}

fn read_info(path: &Path) -> Option<AgentInfoFile> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The agent already running in `pid`, if its info file exists and its port accepts connections
/// (a stale file left by an earlier JVM with the same pid has no listener).
fn running_agent(pid: u32, info_path: &Path) -> Option<AttachResult> {
    let info = read_info(info_path)?;
    let host = if info.bind.is_empty() || info.bind == "0.0.0.0" { "127.0.0.1" } else { info.bind.as_str() };
    let addr: std::net::SocketAddr = format!("{host}:{}", info.port).parse().ok()?;
    std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()?;
    Some(info.into_result(pid, info_path.to_owned()))
}

/// Attaches the OpenProfiler agent at `agent_path` to `pid` with extra agent `options`
/// (e.g. `"interval=5ms,tls"`). A random token is generated unless `options` contains one.
pub fn attach_agent(pid: u32, agent_path: &Path, options: &str) -> Result<AttachResult> {
    let t = target(pid);
    let info_name = format!("openprofiler-{}.json", t.ns_pid);
    let info_host = t.tmp.join(&info_name);
    // An agent loaded by an earlier attach keeps serving; a second one can't be loaded.
    if let Some(r) = running_agent(pid, &info_host) {
        return Ok(r);
    }
    let (_, in_target) = agent_path_for(&t, agent_path)?;
    let token = options
        .split(',')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(String::from)
        .unwrap_or_else(random_token);
    let info_target =
        if t.root.is_some() { format!("/tmp/{info_name}") } else { info_host.display().to_string() };
    let _ = std::fs::remove_file(&info_host);
    let mut opts = vec![format!("token={token}"), format!("infofile={info_target}")];
    opts.extend(options.split(',').filter(|s| !s.is_empty() && !s.starts_with("token=")).map(String::from));
    let rc = load_agent(pid, Path::new(&in_target), &opts.join(","))?;
    if rc != 0 {
        bail!("Agent_OnAttach returned {rc}");
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(info) = read_info(&info_host) {
            return Ok(info.into_result(pid, info_host));
        }
        if Instant::now() > deadline {
            bail!(
                "agent loaded but did not publish {} (already profiled by another agent instance?)",
                info_host.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Last `NSpid` entry: the PID inside the innermost PID namespace.
    pub fn ns_pid(pid: u32) -> u32 {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("NSpid:"))
                    .and_then(|l| l.split_whitespace().last()?.parse().ok())
            })
            .unwrap_or(pid)
    }

    fn same_root(pid: u32) -> bool {
        match (std::fs::metadata(format!("/proc/{pid}/root")), std::fs::metadata("/proc/self/root")) {
            (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
            _ => true,
        }
    }

    pub fn target(pid: u32) -> Target {
        let ns = ns_pid(pid);
        if same_root(pid) {
            Target { pid, ns_pid: ns, tmp: PathBuf::from("/tmp"), root: None }
        } else {
            let root = PathBuf::from(format!("/proc/{pid}/root"));
            Target { pid, ns_pid: ns, tmp: root.join("tmp"), root: Some(root) }
        }
    }

    /// While alive, the effective uid/gid are the target JVM's, so the `.attach_pid` file and the
    /// socket connection pass HotSpot's owner checks when we run as root (e.g. in a Kubernetes
    /// debug container next to a JVM running as a non-root user). A no-op unless we are root.
    pub struct AsTargetUser {
        restore: Option<(libc::uid_t, libc::gid_t)>,
    }

    impl AsTargetUser {
        pub fn switch(pid: u32) -> Self {
            // SAFETY: plain libc credential calls; on failure the original ids stay in place.
            unsafe {
                let (euid, egid) = (libc::geteuid(), libc::getegid());
                let Some((uid, gid)) = owner(pid).filter(|&(u, g)| euid == 0 && (u, g) != (euid, egid))
                else {
                    return Self { restore: None };
                };
                if libc::setegid(gid) != 0 {
                    return Self { restore: None };
                }
                if libc::seteuid(uid) != 0 {
                    libc::setegid(egid);
                    return Self { restore: None };
                }
                Self { restore: Some((euid, egid)) }
            }
        }
    }

    impl Drop for AsTargetUser {
        fn drop(&mut self) {
            if let Some((uid, gid)) = self.restore {
                // SAFETY: restores the ids saved in `switch` (uid first, to regain the right to set gid).
                unsafe {
                    libc::seteuid(uid);
                    libc::setegid(gid);
                }
            }
        }
    }

    /// Effective uid and gid of `pid`.
    fn owner(pid: u32) -> Option<(libc::uid_t, libc::gid_t)> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let field = |name: &str| -> Option<u32> {
            status.lines().find(|l| l.starts_with(name))?.split_whitespace().nth(2)?.parse().ok()
        };
        Some((field("Uid:")?, field("Gid:")?))
    }

    /// Finds JVMs that hsperfdata in our /tmp did not reveal (containers, -XX:-UsePerfData).
    pub fn scan_proc(out: &mut Vec<LocalJvm>) {
        let Ok(dir) = std::fs::read_dir("/proc") else { return };
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if out.iter().any(|j| j.pid == pid) || pid == std::process::id() {
                continue;
            }
            let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else { continue };
            if exe.file_name().is_none_or(|n| n != "java") {
                continue;
            }
            let t = target(pid);
            let mut found = None;
            if let Ok(dirs) = std::fs::read_dir(&t.tmp) {
                for d in dirs.flatten() {
                    let name = d.file_name().to_string_lossy().into_owned();
                    if let Some(user) = name.strip_prefix("hsperfdata_")
                        && let Ok(data) = std::fs::read(d.path().join(t.ns_pid.to_string()))
                    {
                        found = jvm_from_perf(pid, &data, Some(user.to_string()));
                        break;
                    }
                }
            }
            let mut jvm = found.unwrap_or_else(|| {
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let parts: Vec<String> = cmdline
                    .split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect();
                let main = parts.iter().skip(1).find(|p| !p.starts_with('-')).cloned().unwrap_or_default();
                LocalJvm {
                    pid,
                    main_class: main,
                    args: parts.join(" "),
                    vm_version: None,
                    attachable: true,
                    profiled: false,
                    user: None,
                }
            });
            // The agent publishes its info file in the JVM's own /tmp.
            jvm.profiled |= t.tmp.join(format!("openprofiler-{}.json", t.ns_pid)).exists();
            out.push(jvm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_java_command() {
        assert_eq!(split_command("app.Main a b"), ("app.Main".into(), "a b".into()));
        assert_eq!(split_command("app.jar"), ("app.jar".into(), String::new()));
    }

    #[test]
    fn listing_does_not_panic() {
        let _ = list_jvms();
    }
}
