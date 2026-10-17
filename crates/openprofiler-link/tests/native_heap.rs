//! The agent's native heap dumper (JVMTI heap walk) against HotSpot's own HPROF dumps of the same
//! JVM, and recorded-objects-only dumps. Skipped when `java` is unavailable or cannot write HPROF.

use openprofiler_heap::{ClassQuery, HeapAnalysis, ObjectQuery, OpenOptions};
use openprofiler_link::{AgentInfo, HeapDumpOptions, LinkClient, LinkOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROGRAM: &str = r#"
public class Shape {
    static final class Node {
        Node next; byte[] payload; String name;
        int id; long stamp; double weight; boolean flag; char c; short s; float f; byte b;
    }
    static class Base { Object baseRef; int baseValue = 7; }
    static final class Holder extends Base implements Runnable {
        Node head; Object[] slots = new Object[10]; int count;
        public void run() {}
    }
    static final class Recorded { final long[] data = new long[256]; final int n; Recorded(int n) { this.n = n; } }

    static final Holder HOLDER = new Holder();
    static final Recorded[] RING = new Recorded[400];
    static long counter = 42;

    public static void main(String[] args) throws Exception {
        build();
        System.out.println("PID " + ProcessHandle.current().pid());
        System.out.flush();
        // Recorded objects are allocated only once allocation recording is switched on.
        java.io.BufferedReader in = new java.io.BufferedReader(new java.io.InputStreamReader(System.in));
        in.readLine();
        for (int i = 0; ; i++) {
            RING[i % RING.length] = new Recorded(i);
            if (i == 4000) { System.out.println("ALLOCATED"); System.out.flush(); Thread.sleep(600_000); }
            if (i % 50 == 0) Thread.sleep(1);
        }
    }

    static void build() {
        Node prev = null;
        for (int i = 0; i < 1000; i++) {
            Node n = new Node();
            n.payload = new byte[100]; n.name = "node-" + i; n.id = i; n.stamp = i * 3L;
            n.weight = i / 2.0; n.flag = i % 2 == 0; n.c = 'x'; n.s = (short) i; n.f = i; n.b = (byte) i;
            n.next = prev;
            prev = n;
        }
        HOLDER.head = prev;
        HOLDER.baseRef = "base";
        for (int i = 0; i < 10; i++) HOLDER.slots[i] = i % 3 == 0 ? null : new int[i];
        HOLDER.count = 1000;
    }
}
"#;

fn java() -> Option<PathBuf> {
    let java = std::env::var_os("JAVA_HOME")
        .map(|h| Path::new(&h).join("bin/java"))
        .unwrap_or_else(|| "java".into());
    Command::new(&java).arg("-version").output().ok().filter(|o| o.status.success()).map(|_| java)
}

fn agent_lib() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let name =
        if cfg!(target_os = "macos") { "libopenprofiler_agent.dylib" } else { "libopenprofiler_agent.so" };
    let lib = profile_dir.join(name);
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "openprofiler-agent"])
        .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap())
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

fn open(path: &Path) -> HeapAnalysis {
    HeapAnalysis::open(path, &OpenOptions::default(), |_, _| {}).unwrap()
}

fn class_row(h: &HeapAnalysis, name: &str) -> Option<openprofiler_heap::ClassRow> {
    let q = ClassQuery { filter: Some(name.into()), ..Default::default() };
    h.classes(&q).unwrap().rows.into_iter().find(|r| r.name == name)
}

/// The single instance of `class`.
fn only_instance(h: &HeapAnalysis, class: &str) -> u32 {
    let c = class_row(h, class).unwrap_or_else(|| panic!("class {class}"));
    assert_eq!(c.instances, 1, "{class}");
    let q = ObjectQuery { class_id: c.class_id, limit: Some(1), ..Default::default() };
    h.objects(&q).unwrap().rows[0].id
}

/// Primitive fields as `name=value`, sorted.
fn primitives(h: &HeapAnalysis, obj: u32) -> Vec<String> {
    let mut v: Vec<String> = h
        .object(obj)
        .unwrap()
        .fields
        .into_iter()
        .filter(|f| f.reference.is_none() && f.ty != "object")
        .map(|f| format!("{}={}", f.name, f.value))
        .collect();
    v.sort();
    v
}

/// The `next` chain from `obj`.
fn chain(h: &HeapAnalysis, mut obj: u32) -> Vec<String> {
    let mut out = vec![];
    for _ in 0..2000 {
        let d = h.object(obj).unwrap();
        let name = d.fields.iter().find(|f| f.name == "name").and_then(|f| f.reference);
        out.push(name.and_then(|n| h.string_value(n)).unwrap_or_default());
        match d.fields.iter().find(|f| f.name == "next").and_then(|f| f.reference) {
            Some(n) => obj = n,
            None => break,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn native_dump_matches_hotspot() {
    let Some(java) = java() else {
        eprintln!("skipping: java not found");
        return;
    };
    let lib = agent_lib();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("Shape.java");
    std::fs::write(&src, PROGRAM).unwrap();
    let info = dir.path().join("info.json");
    let agent = format!(
        "-agentpath:{}=port=0,token=t,infofile={},alloc=off,alloc_interval=16k",
        lib.display(),
        info.display()
    );
    let mut child = Command::new(&java)
        .args([&agent, "-Xmx512m"])
        .arg(&src)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start java");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let _jvm = Jvm(child);
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert!(line.starts_with("PID "), "{line}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let info = loop {
        if let Ok(i) = AgentInfo::read(&info) {
            break i;
        }
        assert!(Instant::now() < deadline, "agent did not publish its info file");
        std::thread::sleep(Duration::from_millis(50));
    };
    let client = LinkClient::connect(LinkOptions::from_info(&info)).await.unwrap();
    assert!(client.hello().capabilities.iter().any(|c| c == "native_heap_dump"));

    let hotspot = dir.path().join("hotspot.hprof");
    match client.heap_dump(&hotspot, true, true).await {
        Err(e) if e.to_string().contains("UnsupportedOperationException") => {
            eprintln!("JVM cannot write HPROF heap dumps; skipping");
            return;
        }
        r => r.unwrap(),
    };
    let native = dir.path().join("native.hprof");
    let opts = HeapDumpOptions { dumper: "native".into(), ..Default::default() };
    let t = Instant::now();
    let n = client.heap_dump_with(&native, &opts, true).await.unwrap();
    eprintln!("native dump: {} bytes in {:?}", n.size, t.elapsed());

    let (hs, nv) = (open(&hotspot), open(&native));
    let (s1, s2) = (hs.summary(), nv.summary());
    eprintln!("hotspot {s1:?}\nnative  {s2:?}");
    for d in hs.histogram_diff(&nv).iter().take(15) {
        eprintln!("  {d:?}");
    }
    let near = |a: u64, b: u64, pct: u64| a.abs_diff(b) * 100 <= a.max(b) * pct;
    assert!(near(s1.reachable_objects, s2.reachable_objects, 5), "reachable objects");
    assert!(near(s1.reachable_shallow, s2.reachable_shallow, 5), "reachable shallow size");
    assert!(near(s1.classes as u64, s2.classes as u64, 5), "classes");
    assert!(s2.gc_roots > 100);

    for class in ["Shape$Node", "Shape$Holder", "byte[]", "java.lang.String"] {
        let (a, b) = (class_row(&hs, class).unwrap(), class_row(&nv, class).unwrap());
        if class.starts_with("Shape") {
            assert_eq!((a.instances, a.shallow), (b.instances, b.shallow), "{class}");
        } else {
            assert!(near(a.instances, b.instances, 5), "{class}: {a:?} vs {b:?}");
        }
    }
    let node = class_row(&nv, "Shape$Node").unwrap();
    assert_eq!(node.instances, 1000);

    // The holder's retained size covers the node chain, payloads, names and slots.
    let (h1, h2) = (only_instance(&hs, "Shape$Holder"), only_instance(&nv, "Shape$Holder"));
    assert_eq!(hs.retained_size(h1), nv.retained_size(h2), "holder retained size");
    assert!(nv.retained_size(h2) > 1000 * (32 + 100));
    assert_eq!(primitives(&hs, h1), primitives(&nv, h2));
    let head = |h: &HeapAnalysis, o: u32| {
        h.object(o).unwrap().fields.iter().find(|f| f.name == "head").unwrap().reference.unwrap()
    };
    let (n1, n2) = (head(&hs, h1), head(&nv, h2));
    assert_eq!(primitives(&hs, n1), primitives(&nv, n2), "node fields");
    let c2 = chain(&nv, n2);
    assert_eq!(c2.len(), 1000);
    assert_eq!(c2[0], "node-999");
    assert_eq!(chain(&hs, n1), c2);

    // Recorded objects: allocated after recording starts, kept alive by `RING`.
    let rec = dir.path().join("recorded.hprof");
    let opts = HeapDumpOptions { recorded_only: true, ..Default::default() };
    let err = client.heap_dump_with(&rec, &opts, true).await.unwrap_err();
    assert!(err.to_string().contains("recorded"), "{err}");
    client.set_recording("alloc", true, 0).await.unwrap();
    use std::io::Write;
    writeln!(stdin, "go").unwrap();
    stdin.flush().unwrap();
    loop {
        line.clear();
        out.read_line(&mut line).unwrap();
        if line.trim() == "ALLOCATED" {
            break;
        }
    }
    client.heap_dump_with(&rec, &opts, true).await.unwrap();
    let r = open(&rec);
    let s = r.summary();
    eprintln!("recorded {s:?}");
    assert!(s.objects < s2.objects / 5, "recorded dump is small");
    let recorded = ["Shape$Recorded", "long[]"]
        .iter()
        .filter_map(|c| class_row(&r, c))
        .map(|c| c.instances)
        .sum::<u64>();
    assert!(recorded > 0, "recorded objects in the dump");
    assert!(
        class_row(&r, "Shape$Node").is_none_or(|c| c.instances == 0),
        "nodes were allocated before recording"
    );
    // Every recorded object comes with its path from the GC roots.
    let ring = class_row(&r, "Shape$Recorded[]").expect("the ring on the path to the recorded objects");
    assert_eq!(ring.instances, 1);
    let q = ObjectQuery {
        class_id: class_row(&r, "Shape$Recorded").and_then(|c| c.class_id),
        limit: Some(50),
        ..Default::default()
    };
    if q.class_id.is_some() {
        for o in r.objects(&q).unwrap().rows {
            assert!(r.is_reachable(o.id), "recorded object {} reachable", o.address);
        }
    }
    client.close().await;
}
