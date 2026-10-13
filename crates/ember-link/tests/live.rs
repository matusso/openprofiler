//! End-to-end tests: a real JVM running `java/testapp/Workload.java` with the Ember agent, either
//! loaded at startup (`-agentpath`) or attached dynamically. Skipped when `java` is unavailable.

use ember_link::{AgentInfo, LinkClient, LinkOptions};
use ember_model::{Profile, SampleKind};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn java() -> Option<PathBuf> {
    let java = std::env::var_os("JAVA_HOME")
        .map(|h| Path::new(&h).join("bin/java"))
        .unwrap_or_else(|| "java".into());
    Command::new(&java).arg("-version").output().ok().filter(|o| o.status.success()).map(|_| java)
}

fn workload() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../java/testapp/Workload.java")
}

/// Path of the agent library, building it if needed.
fn agent_lib() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let name = if cfg!(target_os = "macos") { "libember_agent.dylib" } else { "libember_agent.so" };
    let lib = profile_dir.join(name);
    let target_dir = profile_dir.parent().unwrap();
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "ember-agent"])
        .env("CARGO_TARGET_DIR", target_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("cargo build ember-agent");
    assert!(status.success() || lib.exists(), "building ember-agent failed");
    lib
}

struct Jvm(Child);

impl Drop for Jvm {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(java: &Path, jvm_args: &[String]) -> (Jvm, u32) {
    let mut child = Command::new(java)
        .args(jvm_args)
        .arg(workload())
        .arg("60")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start java");
    let stdout = child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let pid = line.trim().strip_prefix("PID ").and_then(|p| p.parse().ok()).expect("workload prints its pid");
    (Jvm(child), pid)
}

fn has_frame(p: &Profile, kind: SampleKind, needle: &str) -> bool {
    let Some(set) = p.samples.get(&kind) else { return false };
    let mut frames = vec![];
    set.stack.iter().any(|&s| {
        p.stacks.frames_root_first(s, &mut frames);
        frames.iter().any(|&f| p.symbols.method_label(p.symbols.frame(f).method).contains(needle))
    })
}

async fn wait_for(client: &LinkClient, what: &str, cond: impl Fn(&Profile) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if cond(&client.profile().read()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn exercise(client: &LinkClient, dir: &Path) {
    let hello = client.hello();
    assert!(hello.jvm.as_ref().is_some_and(|j| j.pid != 0), "hello carries JVM info");
    wait_for(client, "CPU samples in Workload.burnCpu", |p| {
        has_frame(p, SampleKind::Cpu, "Workload.burnCpu")
    })
    .await;
    wait_for(client, "heap telemetry", |p| p.telemetry("heap.used").is_some_and(|s| !s.value.is_empty()))
        .await;
    wait_for(client, "wall samples and thread states", |p| {
        p.samples.get(&SampleKind::Wall).is_some_and(|s| !s.is_empty()) && !p.thread_states.is_empty()
    })
    .await;
    wait_for(client, "allocation samples", |p| has_frame(p, SampleKind::Alloc, "Workload.allocate")).await;
    wait_for(client, "monitor contention", |p| has_frame(p, SampleKind::Monitor, "Workload.contend")).await;
    {
        let p = client.profile();
        let p = p.read();
        assert!(p.threads.iter().any(|t| t.name == "burner"));
        assert!(!p.threads.iter().any(|t| t.name.starts_with("Ember ")), "agent threads are hidden");
    }
    let dump = client.thread_dump().await.unwrap();
    let burner = dump.threads.iter().find(|t| t.name == "burner").expect("burner thread in dump");
    assert!(!burner.frames.is_empty());

    let hprof = dir.join("dump.hprof");
    let out = client.heap_dump(&hprof, true, false).await.unwrap();
    assert!(out.size > 1000);
    let head = std::fs::read(&out.path).unwrap();
    assert_eq!(head.len() as u64, out.size);
    assert!(head.starts_with(b"JAVA PROFILE 1.0."), "streamed dump is HPROF");

    client.bookmark("checkpoint", Some("#ff0000")).await.unwrap();
    client.set_recording("cpu", false, 0).await.unwrap();
    client.run_gc().await.unwrap();
    wait_for(client, "bookmark", |p| p.bookmarks.iter().any(|b| b.label == "checkpoint")).await;
    let gen_before = *client.generation().borrow();
    assert!(gen_before > 0);
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agentpath_session() {
    let Some(java) = java() else {
        eprintln!("skipping: java not found");
        return;
    };
    let lib = agent_lib();
    let dir = tempfile::tempdir().unwrap();
    let info = dir.path().join("info.json");
    let opts = format!(
        "-agentpath:{}=port=0,token=secret-token,infofile={},interval=5ms",
        lib.display(),
        info.display()
    );
    let (_jvm, _pid) = spawn(&java, &[opts]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let info = loop {
        if let Ok(i) = AgentInfo::read(&info) {
            break i;
        }
        assert!(Instant::now() < deadline, "agent did not publish its info file");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(info.token, "secret-token");

    // A wrong token is rejected.
    let bad = LinkClient::connect(LinkOptions::new(info.addr(), "wrong")).await;
    assert!(bad.is_err(), "wrong token must be rejected");

    let client = LinkClient::connect(LinkOptions::from_info(&info)).await.unwrap();
    exercise(&client, dir.path()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_attach_with_tls() {
    let Some(java) = java() else {
        eprintln!("skipping: java not found");
        return;
    };
    let lib = agent_lib();
    let dir = tempfile::tempdir().unwrap();
    let (_jvm, pid) = spawn(&java, &["-XX:+EnableDynamicAgentLoading".to_string()]);
    let jvms = ember_attach::list_jvms();
    assert!(jvms.iter().any(|j| j.pid == pid && j.attachable), "list_jvms finds the workload: {jvms:?}");
    let version = ember_attach::jcmd(pid, "VM.version").unwrap();
    assert!(!version.trim().is_empty());

    let r = ember_attach::attach_agent(pid, &lib, "tls,interval=5ms").unwrap();
    assert!(r.tls && r.fingerprint.is_some());
    let mut opts = LinkOptions::new(r.addr(), r.token.clone());
    opts.fingerprint = Some("00".repeat(32));
    assert!(LinkClient::connect(opts.clone()).await.is_err(), "wrong fingerprint must be rejected");
    opts.fingerprint = r.fingerprint.clone();
    let client = LinkClient::connect(opts).await.unwrap();
    assert!(ember_attach::list_jvms().iter().any(|j| j.pid == pid && j.profiled));
    exercise(&client, dir.path()).await;
    let _ = std::fs::remove_file(&r.info_path);
}

#[test]
fn offline_recording_replays() {
    let Some(java) = java() else {
        eprintln!("skipping: java not found");
        return;
    };
    let lib = agent_lib();
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("run.emberlink");
    let status = Command::new(&java)
        .arg(format!("-agentpath:{}=mode=offline,file={}", lib.display(), rec.display()))
        .arg(workload())
        .arg("3")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let p = ember_link::replay_recording(&rec).unwrap();
    assert!(has_frame(&p, SampleKind::Cpu, "Workload.burnCpu"));
    assert!(p.telemetry("heap.used").is_some());
    assert!(p.meta.jvm.pid.is_some());
    assert!(p.meta.end > p.meta.start);
}
