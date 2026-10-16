//! End-to-end tests: a real JVM running `java/testapp/Workload.java` with the OpenProfiler agent, either
//! loaded at startup (`-agentpath`) or attached dynamically. Skipped when `java` is unavailable;
//! dynamic attach and heap dumps are skipped on JVMs that do not support them (e.g. OpenJ9).

use openprofiler_link::{AgentInfo, LinkClient, LinkOptions};
use openprofiler_model::{MonitorEventKind, Profile, SampleKind};
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

/// OpenJ9 does not implement the HotSpot attach protocol that `openprofiler-attach` speaks.
fn is_openj9(java: &Path) -> bool {
    Command::new(java)
        .arg("-version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stderr).contains("OpenJ9"))
}

fn workload() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../java/testapp/Workload.java")
}

/// Path of the agent library, building it if needed.
fn agent_lib() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let name =
        if cfg!(target_os = "macos") { "libopenprofiler_agent.dylib" } else { "libopenprofiler_agent.so" };
    let lib = profile_dir.join(name);
    let target_dir = profile_dir.parent().unwrap();
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "openprofiler-agent"])
        .env("CARGO_TARGET_DIR", target_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("cargo build openprofiler-agent");
    assert!(status.success() || lib.exists(), "building openprofiler-agent failed");
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
    has_frame_on(p, kind, needle, |_| true)
}

/// Like [`has_frame`], restricted to samples of threads matching `thread`.
fn has_frame_on(
    p: &Profile,
    kind: SampleKind,
    needle: &str,
    thread: impl Fn(&openprofiler_model::ThreadInfo) -> bool,
) -> bool {
    let Some(set) = p.samples.get(&kind) else { return false };
    let mut frames = vec![];
    (0..set.len()).any(|i| {
        if !p.threads.get(set.thread[i] as usize).is_some_and(&thread) {
            return false;
        }
        p.stacks.frames_root_first(set.stack[i], &mut frames);
        frames.iter().any(|&f| p.symbols.method_label(p.symbols.frame(f).method).contains(needle))
    })
}

fn thread_name(p: &Profile, idx: Option<u32>) -> &str {
    idx.and_then(|i| p.threads.get(i as usize)).map_or("", |t| t.name.as_str())
}

/// Lock owners, j.u.c park events and virtual threads, where the JVM supports them.
async fn check_locks_and_virtual_threads(client: &LinkClient) {
    let caps = client.hello().capabilities;
    let has = |c: &str| caps.iter().any(|x| x == c);
    if has("monitor_usage") {
        wait_for(client, "monitor events with an owner", |p| {
            p.monitors.iter().any(|m| {
                m.kind == MonitorEventKind::Enter
                    && m.monitor_id.is_some()
                    && thread_name(p, m.owner).starts_with("contender-")
                    && thread_name(p, Some(m.thread)).starts_with("contender-")
            })
        })
        .await;
    }
    if has("park") {
        wait_for(client, "ReentrantLock park events with an owner", |p| {
            p.monitors.iter().any(|m| {
                m.kind == MonitorEventKind::Park
                    && m.monitor_class.contains("ReentrantLock")
                    && thread_name(p, m.owner).starts_with("juc-")
                    && thread_name(p, Some(m.thread)).starts_with("juc-")
            }) && has_frame(p, SampleKind::Monitor, "Workload.contendJuc")
        })
        .await;
    } else {
        eprintln!("no park hook on this JVM; skipping j.u.c checks");
    }
    if has("virtual_threads") {
        wait_for(client, "virtual threads with states and park events", |p| {
            let vt =
                |i: u32| p.threads.get(i as usize).is_some_and(|t| t.is_virtual && t.name.starts_with("vt-"));
            p.threads.iter().any(|t| t.is_virtual && t.name.starts_with("vt-"))
                && (0..p.thread_states.len()).any(|i| vt(p.thread_states.thread[i]))
                && p.monitors.iter().any(|m| m.kind == MonitorEventKind::Park && vt(m.thread))
        })
        .await;
        if has("cpu_async") {
            wait_for(client, "CPU samples attributed to virtual threads", |p| {
                has_frame_on(p, SampleKind::Cpu, "Workload.virtualWork", |t| t.is_virtual)
            })
            .await;
        }
    }
}

/// A timer trigger installed over the link fires and its thread dump reaches the profile.
async fn check_triggers(client: &LinkClient) {
    let t = openprofiler_proto::triggers::parse_json(
        r#"[{"id": "soon", "timer": {"delay": "100ms"}, "actions": [{"bookmark": "timer-fired"}, "thread_dump"]}]"#,
    )
    .unwrap();
    client.set_triggers(t).await.unwrap();
    wait_for(client, "trigger bookmark and thread dump", |p| {
        p.bookmarks.iter().any(|b| b.label == "timer-fired")
            && p.bookmarks.iter().any(|b| b.label == "Trigger soon")
            && p.thread_dumps
                .iter()
                .any(|d| d.label == "Trigger soon" && d.threads.iter().any(|t| t.name == "burner"))
    })
    .await;
    let bad = openprofiler_proto::v1::Trigger { id: "broken".into(), ..Default::default() };
    assert!(client.set_triggers(vec![bad]).await.is_err(), "a trigger without condition is rejected");
}

/// MBean list/get/invoke/set, and an attribute tracked as telemetry.
async fn check_mbeans(client: &LinkClient) {
    use openprofiler_proto::v1;
    assert!(client.hello().capabilities.iter().any(|c| c == "mbeans"));
    let l = client.mbean_list("", "java.lang:*").await.unwrap();
    assert!(l.beans.iter().any(|b| b.name == "java.lang:type=Memory"), "platform beans are listed");
    assert!(l.beans.iter().all(|b| b.name.starts_with("java.lang:")), "pattern filters");
    assert!(client.mbean_list("", "not a pattern").await.is_err());

    let mem = client.mbean_get("", "java.lang:type=Memory").await.unwrap();
    let heap = mem.attributes.iter().find(|a| a.name == "HeapMemoryUsage").expect("HeapMemoryUsage");
    let v: serde_json::Value = serde_json::from_str(&heap.value_json).unwrap();
    assert!(v["used"].as_f64().is_some_and(|u| u > 0.0), "composite value as JSON: {v}");
    assert!(mem.attributes.iter().any(|a| a.name == "Verbose" && a.writable));
    assert!(mem.operations.iter().any(|o| o.name == "gc" && o.params.is_empty()));

    let threading = client.mbean_get("", "java.lang:type=Threading").await.unwrap();
    let op = threading
        .operations
        .iter()
        .find(|o| o.name == "getThreadCpuTime" && o.params.len() == 1 && o.params[0].r#type == "long")
        .expect("getThreadCpuTime(long)");
    let r = v1::MBeanInvokeRequest {
        name: "java.lang:type=Threading".into(),
        operation: op.name.clone(),
        signature: vec!["long".into()],
        args: vec!["1".into()],
        ..Default::default()
    };
    let out: serde_json::Value = serde_json::from_str(&client.mbean_invoke(r).await.unwrap()).unwrap();
    assert!(out.is_number(), "invoke returns JSON: {out}");
    let bad = v1::MBeanInvokeRequest {
        name: "java.lang:type=Threading".into(),
        operation: "getThreadCpuTime".into(),
        signature: vec!["long".into()],
        args: vec!["nope".into()],
        ..Default::default()
    };
    assert!(client.mbean_invoke(bad).await.unwrap_err().to_string().contains("NumberFormatException"));

    let set = |value: &str| v1::MBeanSetRequest {
        name: "java.lang:type=Memory".into(),
        attribute: "Verbose".into(),
        value: value.into(),
        ..Default::default()
    };
    client.mbean_set(set("false")).await.unwrap();
    assert!(client.mbean_set(set("maybe")).await.is_err());

    let track = |enabled| v1::MBeanTrackRequest {
        name: "java.lang:type=Memory".into(),
        attribute: "HeapMemoryUsage".into(),
        item: "used".into(),
        label: String::new(),
        unit: 1,
        enabled,
        ..Default::default()
    };
    let tracked = client.mbean_track(track(true)).await.unwrap();
    let key = "mbean:java.lang:type=Memory#HeapMemoryUsage.used";
    assert!(tracked.iter().any(|t| t.key == key && t.label == "HeapMemoryUsage.used (Memory)"));
    wait_for(client, "tracked MBean telemetry", |p| p.telemetry(key).is_some_and(|s| !s.value.is_empty()))
        .await;
    assert!(client.mbean_track(track(false)).await.unwrap().is_empty());
    assert!(client.mbean_list("", "").await.unwrap().tracked.is_empty());
}

/// Other MBean servers, a remote JMX connection, notifications, and a slow operation that must not
/// hold up the agent link.
async fn check_mbean_servers(client: &LinkClient) {
    use openprofiler_proto::v1;
    let servers = client.mbean_servers().await.unwrap();
    assert_eq!(servers[0].kind, "platform");
    let local = servers.iter().find(|s| s.kind == "local").expect("the workload's second MBean server");
    let l = client.mbean_list(&local.id, "").await.unwrap();
    assert!(l.beans.iter().any(|b| b.name == "java.lang:type=Memory"), "beans of the second server");
    assert!(client.mbean_list("no such server", "").await.is_err());

    // The workload's JMX connector, as a remote server.
    let rt = client.mbean_get("", "java.lang:type=Runtime").await.unwrap();
    let props = &rt.attributes.iter().find(|a| a.name == "SystemProperties").unwrap().value_json;
    let props: serde_json::Value = serde_json::from_str(props).unwrap();
    let url = props
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "openprofiler.test.jmx")
        .and_then(|p| p["value"].as_str())
        .expect("the workload's JMX connector URL")
        .to_string();
    let connect = |enabled| v1::MBeanConnectRequest { url: url.clone(), enabled, ..Default::default() };
    let servers = client.mbean_connect(connect(true)).await.unwrap();
    let remote = servers.iter().find(|s| s.kind == "remote").expect("remote server");
    assert_eq!(remote.id, url);
    assert!(remote.error.is_empty() && remote.bean_count >= 2, "{remote:?}");
    let mem = client.mbean_get(&url, "java.lang:type=Memory").await.unwrap();
    assert!(mem.attributes.iter().any(|a| a.name == "HeapMemoryUsage" && a.value_json.contains("used")));
    let track = |enabled| v1::MBeanTrackRequest {
        server: url.clone(),
        name: "java.lang:type=Memory".into(),
        attribute: "HeapMemoryUsage".into(),
        item: "committed".into(),
        unit: 1,
        enabled,
        ..Default::default()
    };
    let tracked = client.mbean_track(track(true)).await.unwrap();
    let key = format!("mbean:[{url}]java.lang:type=Memory#HeapMemoryUsage.committed");
    assert!(tracked.iter().any(|t| t.key == key && t.server == url));
    wait_for(client, "remote tracked telemetry", |p| {
        p.telemetry(&key).is_some_and(|s| s.value.iter().any(|v| v.is_finite()))
    })
    .await;
    client.mbean_track(track(false)).await.unwrap();
    let servers = client.mbean_connect(connect(false)).await.unwrap();
    assert!(servers.iter().all(|s| s.kind != "remote"));
    assert!(client.mbean_get(&url, "java.lang:type=Memory").await.is_err());

    // GC notifications.
    let gcs = client.mbean_list("", "java.lang:type=GarbageCollector,*").await.unwrap().beans;
    assert!(!gcs.is_empty());
    let after = client.mbean_notifications(0).await.unwrap().last_seq;
    for b in &gcs {
        let r = v1::MBeanSubscribeRequest { name: b.name.clone(), enabled: true, ..Default::default() };
        client.mbean_subscribe(r).await.unwrap();
    }
    let bad = v1::MBeanSubscribeRequest {
        name: "java.lang:type=Runtime".into(),
        enabled: true,
        ..Default::default()
    };
    assert!(client.mbean_subscribe(bad).await.is_err(), "Runtime emits no notifications");
    let gc = v1::MBeanInvokeRequest {
        name: "java.lang:type=Memory".into(),
        operation: "gc".into(),
        ..Default::default()
    };
    client.mbean_invoke(gc).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let n = client.mbean_notifications(after).await.unwrap();
        if let Some(n) = n.notifications.iter().find(|n| n.r#type == "com.sun.management.gc.notification") {
            let data: serde_json::Value = serde_json::from_str(&n.data_json).unwrap();
            assert!(data["gcName"].is_string(), "GC info as JSON: {data}");
            assert!(gcs.iter().any(|b| b.name == n.source));
            break;
        }
        assert!(Instant::now() < deadline, "no GC notification");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for b in &gcs {
        let r = v1::MBeanSubscribeRequest { name: b.name.clone(), enabled: false, ..Default::default() };
        client.mbean_subscribe(r).await.unwrap();
    }
    assert!(client.mbean_notifications(u64::MAX).await.unwrap().subscriptions.is_empty());

    // A slow operation runs off the link thread: other requests are answered meanwhile.
    let slow = v1::MBeanInvokeRequest {
        name: "openprofiler.test:type=Slow".into(),
        operation: "sleep".into(),
        signature: vec!["long".into()],
        args: vec!["3000".into()],
        ..Default::default()
    };
    let started = Instant::now();
    let (r, dump_after) = tokio::join!(client.mbean_invoke(slow), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        client.thread_dump().await.unwrap();
        started.elapsed()
    });
    r.unwrap();
    assert!(
        dump_after < Duration::from_millis(2500),
        "thread dump waited for the MBean call: {dump_after:?}"
    );
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
    check_locks_and_virtual_threads(client).await;
    check_triggers(client).await;
    check_mbeans(client).await;
    check_mbean_servers(client).await;
    {
        let p = client.profile();
        let p = p.read();
        assert!(p.threads.iter().any(|t| t.name == "burner"));
        assert!(!p.threads.iter().any(|t| t.name.starts_with("OpenProfiler ")), "agent threads are hidden");
        assert!(
            !p.threads.iter().any(|t| t.name.to_ascii_lowercase().starts_with("openprofiler")),
            "agent threads are not sampled: {:?}",
            p.threads.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }
    let dump = client.thread_dump().await.unwrap();
    let burner = dump.threads.iter().find(|t| t.name == "burner").expect("burner thread in dump");
    assert!(!burner.frames.is_empty());

    let hprof = dir.join("dump.hprof");
    match client.heap_dump(&hprof, true, false).await {
        Err(e) if e.to_string().contains("UnsupportedOperationException") => {
            eprintln!("JVM cannot write HPROF heap dumps; skipping heap dump checks");
        }
        res => {
            let out = res.unwrap();
            assert!(out.size > 1000);
            let head = std::fs::read(&out.path).unwrap();
            assert_eq!(head.len() as u64, out.size);
            assert!(head.starts_with(b"JAVA PROFILE 1.0."), "streamed dump is HPROF");
        }
    }

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
    if is_openj9(&java) {
        eprintln!("skipping: dynamic attach is not supported on OpenJ9");
        return;
    }
    let lib = agent_lib();
    let dir = tempfile::tempdir().unwrap();
    let (_jvm, pid) = spawn(&java, &["-XX:+EnableDynamicAgentLoading".to_string()]);
    let jvms = openprofiler_attach::list_jvms();
    assert!(jvms.iter().any(|j| j.pid == pid && j.attachable), "list_jvms finds the workload: {jvms:?}");
    let version = openprofiler_attach::jcmd(pid, "VM.version").unwrap();
    assert!(!version.trim().is_empty());

    let r = openprofiler_attach::attach_agent(pid, &lib, "tls,interval=5ms").unwrap();
    assert!(r.tls && r.fingerprint.is_some());
    let mut opts = LinkOptions::new(r.addr(), r.token.clone());
    opts.fingerprint = Some("00".repeat(32));
    assert!(LinkClient::connect(opts.clone()).await.is_err(), "wrong fingerprint must be rejected");
    opts.fingerprint = r.fingerprint.clone();
    let client = LinkClient::connect(opts).await.unwrap();
    assert!(openprofiler_attach::list_jvms().iter().any(|j| j.pid == pid && j.profiled));
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
    let rec = dir.path().join("run.oplink");
    let triggers = dir.path().join("triggers.json");
    std::fs::write(
        &triggers,
        r#"{"triggers": [
            {"id": "start", "jvm_start": {}, "actions": [{"bookmark": "started"}]},
            {"id": "exit", "jvm_exit": {}, "actions": ["thread_dump"]}
        ]}"#,
    )
    .unwrap();
    let status = Command::new(&java)
        .arg(format!(
            "-agentpath:{}=mode=offline,file={},triggers={}",
            lib.display(),
            rec.display(),
            triggers.display()
        ))
        .arg(workload())
        .arg("3")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let p = openprofiler_link::replay_recording(&rec).unwrap();
    assert!(has_frame(&p, SampleKind::Cpu, "Workload.burnCpu"));
    assert!(p.telemetry("heap.used").is_some());
    assert!(p.meta.jvm.pid.is_some());
    assert!(p.meta.end > p.meta.start);
    assert!(p.bookmarks.iter().any(|b| b.label == "started"), "JVM-start trigger ran");
    assert!(
        p.thread_dumps.iter().any(|d| d.label == "Trigger exit" && !d.threads.is_empty()),
        "JVM-exit trigger's thread dump is in the recording"
    );
}
