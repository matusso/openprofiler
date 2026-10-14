//! Exact parity with Eclipse MAT on freshly generated JDK heap dumps.
//!
//! Runs only when `MAT_HOME` points at an unpacked Memory Analyzer and `docker` and `java`
//! are available; otherwise it passes trivially. It drives `scripts/mat-parity.sh`, which
//! compares object counts, every object's shallow and retained size, the class histogram and
//! the top of the dominator tree.

use std::path::Path;
use std::process::Command;

const PROGRAM: &str = r#"
import com.sun.management.HotSpotDiagnosticMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.*;
import java.util.concurrent.CountDownLatch;

public class Parity {
    static final Map<String, Object> MAP = new HashMap<>();
    static final ThreadLocal<long[]> TL = ThreadLocal.withInitial(() -> new long[512]);
    static final List<WeakReference<Object>> WEAK = new ArrayList<>();
    class Inner { int[] data = new int[32]; }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 5000; i++) MAP.put("k" + i, i % 2 == 0 ? new String("dup" + (i % 20)) : new Parity().new Inner());
        for (int i = 0; i < 50; i++) WEAK.add(new WeakReference<>(i % 2 == 0 ? MAP : new byte[100]));
        CountDownLatch ready = new CountDownLatch(2), done = new CountDownLatch(1);
        for (int t = 0; t < 2; t++) {
            new Thread(() -> {
                long[] mine = TL.get();
                List<String> local = new ArrayList<>();
                for (int i = 0; i < 200; i++) local.add("t" + i);
                ready.countDown();
                try { done.await(); } catch (InterruptedException e) { }
                System.out.println(mine.length + local.size());
            }).start();
        }
        ready.await();
        for (int i = 0; i < 1000; i++) new StringBuilder("garbage").append(i).toString();
        ManagementFactory.getPlatformMXBean(HotSpotDiagnosticMXBean.class).dumpHeap(args[0], false);
        done.countDown();
    }
}
"#;

fn available() -> Option<String> {
    let mat = std::env::var("MAT_HOME").ok()?;
    if !Path::new(&mat).join("ParseHeapDump.sh").exists() {
        return None;
    }
    let ok = |cmd: &str, arg: &str| Command::new(cmd).arg(arg).output().is_ok_and(|o| o.status.success());
    (ok("docker", "info") && ok("java", "-version") && ok("python3", "--version")).then_some(mat)
}

#[test]
fn matches_eclipse_mat() {
    let Some(mat) = available() else {
        eprintln!("MAT_HOME, docker, java or python3 unavailable; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("Parity.java");
    std::fs::write(&src, PROGRAM).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/mat-parity.sh");
    for (name, flag) in [("compressed", "-XX:+UseCompressedOops"), ("uncompressed", "-XX:-UseCompressedOops")]
    {
        let dump = dir.path().join(format!("{name}.hprof"));
        let st = Command::new("java").arg(flag).arg(&src).arg(&dump).status().unwrap();
        assert!(st.success());
        // A separate target dir keeps the script's `cargo run` from waiting on this test's lock.
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/heap-parity");
        let out =
            Command::new(&script).arg(&dump).arg(&mat).env("CARGO_TARGET_DIR", target).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        eprintln!("{name}:\n{text}{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && text.contains("PARITY OK"), "{name} dump differs from MAT");
    }
}
