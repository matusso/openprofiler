//! JVM discovery, attach and heap dumps inside the remote host or container (via `ember-attach`).

pub fn list() -> Result<String, String> {
    serde_json::to_string(&ember_attach::list_jvms()).map_err(|e| e.to_string())
}

pub fn attach(pid: u32, agent: &str, options: &str) -> Result<String, String> {
    let r = ember_attach::attach_agent(pid, std::path::Path::new(agent), options)
        .map_err(|e| format!("{e:#}"))?;
    serde_json::to_string(&r).map_err(|e| e.to_string())
}

/// Asks the JVM to dump its heap into its own /tmp, then streams the file to `out`.
pub fn dump(pid: u32, out: &mut dyn std::io::Write) -> Result<(), String> {
    let name = format!("ember-dump-{pid}-{}.hprof", std::process::id());
    let jvm_path = format!("/tmp/{name}");
    ember_attach::jcmd(pid, &format!("GC.heap_dump {jvm_path}")).map_err(|e| format!("{e:#}"))?;
    // The JVM may live in another mount namespace (container / debug container).
    let candidates = [format!("/proc/{pid}/root/tmp/{name}"), jvm_path.clone()];
    let path = candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or_else(|| format!("heap dump {jvm_path} not found"))?;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let r = std::io::copy(&mut f, out).map(|_| ()).map_err(|e| e.to_string());
    let _ = std::fs::remove_file(path);
    r
}
