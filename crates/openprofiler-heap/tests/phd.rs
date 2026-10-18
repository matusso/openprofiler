//! IBM Portable Heap Dumps from OpenJ9 (fixtures in `crates/openprofiler-phd/tests/data`).

use openprofiler_heap::{ClassQuery, HeapAnalysis, ObjectQuery, OpenOptions};
use std::path::PathBuf;

fn open(name: &str) -> (tempfile::TempDir, HeapAnalysis) {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../openprofiler-phd/tests/data").join(name);
    let cache = tempfile::tempdir().unwrap();
    let opts = OpenOptions { cache_dir: Some(cache.path().to_path_buf()), ..Default::default() };
    let h = HeapAnalysis::open(&src, &opts, |_, _| {}).unwrap();
    (cache, h)
}

fn class_row(h: &HeapAnalysis, name: &str) -> openprofiler_heap::ClassRow {
    let rows = h.classes(&ClassQuery { filter: Some(name.into()), ..Default::default() }).unwrap().rows;
    rows.into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no class {name}"))
}

#[test]
fn openj9_dumps() {
    for (name, node_size) in [("compressed.phd.gz", 24), ("uncompressed.phd.gz", 40)] {
        let (_cache, h) = open(name);
        let s = h.summary();
        assert_eq!(s.format, "phd");
        assert!(h.is_phd());
        assert!(s.objects > 10_000, "{s:?}");
        // GC roots are inferred so that every object is reachable.
        assert_eq!(s.reachable_objects, s.objects, "{name}: {s:?}");
        assert_eq!(s.reachable_shallow, s.total_shallow);

        // Sizes come from the dump: class instance sizes and PHD 6 array sizes.
        let node = class_row(&h, "PhdFixture$Node");
        assert_eq!((node.instances, node.shallow), (1000, 1000 * node_size), "{name}");
        let longs = class_row(&h, "long[]");
        assert!(longs.instances >= 100);
        let bytes = class_row(&h, "byte[]");
        assert!(bytes.shallow >= 3 << 20);
        assert_eq!(class_row(&h, "PhdFixture$Wide").instances, 100);
        assert!(class_row(&h, "int[][]").instances >= 1);

        // The static chain head retains all 1000 nodes and their 100 long[4] payloads.
        let id = node.class_id.unwrap();
        let nodes =
            h.objects(&ObjectQuery { class_id: Some(id), limit: Some(1), ..Default::default() }).unwrap();
        let head = &nodes.rows[0];
        let long4 = h
            .objects(&ObjectQuery {
                class_id: class_row(&h, "long[]").class_id,
                limit: Some(10_000),
                ..Default::default()
            })
            .unwrap()
            .rows
            .iter()
            .find(|r| h.object(r.id).unwrap().array_length == Some(4))
            .unwrap()
            .shallow;
        assert!(long4 >= 32);
        assert_eq!(head.retained.unwrap(), 1000 * node_size + 100 * long4, "{name}");

        // References are unnamed fields; primitive arrays have a length but no values.
        let d = h.object(head.id).unwrap();
        assert!(d.fields.iter().any(|f| f.name == "ref0" && f.reference.is_some()), "{:?}", d.fields);
        let paths = h.paths_to_roots(head.id, 1, false).unwrap();
        assert!(paths[0].last().unwrap().root_kind.is_some());
        let big = h
            .objects(&ObjectQuery { class_id: bytes.class_id, limit: Some(1), ..Default::default() })
            .unwrap()
            .rows[0]
            .id;
        let d = h.object(big).unwrap();
        assert_eq!(d.array_length, Some(3 << 20));
        assert!(d.elements.is_none());

        // No field values, so no strings or inspections.
        assert!(h.inspections().is_empty());
        assert!(h.inspection("duplicate_strings", 10).is_err());

        let top: u64 = h.biggest(None, usize::MAX).unwrap().iter().map(|r| r.retained.unwrap()).sum();
        assert_eq!(top, s.reachable_shallow);
    }
}
