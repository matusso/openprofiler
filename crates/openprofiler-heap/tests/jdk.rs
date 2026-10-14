//! Integration test against a real HotSpot heap dump produced by the local JDK.
//! Skipped (passes trivially) when `java` is not on the PATH or the JVM cannot write
//! HPROF dumps (e.g. OpenJ9).

use openprofiler_heap::{ClassQuery, HeapAnalysis, ObjectQuery, OpenOptions, SelectStep};
use std::process::Command;

const PROGRAM: &str = r#"
import com.sun.management.HotSpotDiagnosticMXBean;
import java.lang.management.ManagementFactory;
import java.util.*;

public class Dumper {
    static final List<Object> KEEP = new ArrayList<>();
    static final int UNSUPPORTED = 3;

    static final class Marker {
        String name;
        int id;
        long[] payload = new long[16];
        Marker(String name, int id) { this.name = name; this.id = id; }
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 1000; i++) {
            KEEP.add(new Marker(new String("openprofiler-marker-" + (i % 10)), i));
        }
        KEEP.add("ünïcødé-openprofiler-ŝtring");
        for (int i = 0; i < 50; i++) KEEP.add(new ArrayList<>(64));
        KEEP.add(new ThreadLocal<String>());
        HotSpotDiagnosticMXBean bean = ManagementFactory.getPlatformMXBean(HotSpotDiagnosticMXBean.class);
        try {
            bean.dumpHeap(args[0], true);
        } catch (UnsupportedOperationException e) {
            System.exit(UNSUPPORTED);
        }
        System.out.println(KEEP.size());
    }
}
"#;

/// Exit code `Dumper` uses when `dumpHeap` is unsupported (matches `Dumper.UNSUPPORTED`).
const EXIT_UNSUPPORTED: i32 = 3;

#[test]
fn real_hotspot_dump() {
    if Command::new("java").arg("-version").output().is_err() {
        eprintln!("java not found; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("Dumper.java");
    std::fs::write(&src, PROGRAM).unwrap();
    let dump = dir.path().join("dump.hprof");
    let st = Command::new("java").arg("-Xmx256m").arg(&src).arg(&dump).status().unwrap();
    if st.code() == Some(EXIT_UNSUPPORTED) {
        eprintln!("JVM cannot write HPROF heap dumps; skipping");
        return;
    }
    assert!(st.success());

    let h = HeapAnalysis::open(&dump, &OpenOptions::default(), |_, _| {}).unwrap();
    let s = h.summary();
    assert!(s.objects > 10_000, "{s:?}");
    assert!(s.gc_roots > 100);

    let markers =
        h.classes(&ClassQuery { filter: Some("Dumper$Marker".into()), ..Default::default() }).unwrap();
    assert_eq!(markers.rows.len(), 1);
    let m = &markers.rows[0];
    assert_eq!(m.instances, 1000);
    assert_eq!(m.shallow, 1000 * 24, "compressed-oops instance size");
    // Retained size per marker: itself + long[16] (144 bytes); the name strings are unique too.
    let class_id = m.class_id.unwrap();
    let objs =
        h.objects(&ObjectQuery { class_id: Some(class_id), limit: Some(5), ..Default::default() }).unwrap();
    let r = objs.rows[0].retained.unwrap();
    assert!(r >= 24 + 144, "retained {r}");

    // UTF-16 compact string decoding.
    let strings = h.select(None, &SelectStep::ClassName("java.lang.String".into())).unwrap();
    let page = h
        .objects(&ObjectQuery { set: Some(strings.id), limit: Some(1_000_000), ..Default::default() })
        .unwrap();
    assert!(page.rows.iter().any(|r| h.string_value(r.id).as_deref() == Some("ünïcødé-openprofiler-ŝtring")));
    assert!(page.rows.iter().any(|r| h.string_value(r.id).as_deref() == Some("openprofiler-marker-3")));

    // Dominator invariant: the GC-root level retains exactly the reachable heap.
    let top: u64 = h.biggest(None, usize::MAX).unwrap().iter().map(|r| r.retained.unwrap()).sum();
    assert_eq!(top, s.reachable_shallow);
    assert!(s.reachable_shallow <= s.total_shallow);

    // Inspections see the duplicated marker names (100 copies each of 10 values).
    let dup = h.inspection("duplicate_strings", 1000).unwrap();
    let row =
        dup.rows.iter().find(|r| r.label == "\"openprofiler-marker-7\"").expect("duplicate marker string");
    assert!(row.count >= 100);
    let empty = h.inspection("empty_collections", 100).unwrap();
    assert!(empty.rows.iter().any(|r| r.label == "java.util.ArrayList" && r.count >= 50));

    // Path to roots for a marker ends at a GC root through the static KEEP list.
    let paths = h.paths_to_roots(objs.rows[0].id, 3, true).unwrap();
    assert!(!paths.is_empty());
    assert!(paths[0].last().unwrap().root_kind.is_some());
}
