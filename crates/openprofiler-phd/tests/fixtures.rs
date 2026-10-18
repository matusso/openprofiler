//! Real OpenJ9 dumps of `tests/data/PhdFixture.java` (regenerate with `tests/data/regenerate.sh`).

use openprofiler_hprof::{Hprof, SubRecord, ty};
use openprofiler_phd::{HPROF_MARKER, Record};
use std::collections::HashMap;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

#[derive(Default, Debug)]
struct Stats {
    classes: HashMap<u64, String>,
    instances: HashMap<u64, (u64, u64)>,
    obj_arrays: u64,
    prim_arrays: HashMap<u8, u64>,
    big_byte_arrays: u64,
    sized: u64,
    unsorted: u64,
}

fn stats(name: &str) -> (openprofiler_phd::Header, Stats) {
    let mut r = openprofiler_phd::open(&fixture(name)).unwrap();
    let mut s = Stats::default();
    let mut last = 0;
    while let Some(rec) = r.next_record().unwrap() {
        if rec.address() < last {
            s.unsorted += 1;
        }
        last = rec.address();
        match rec {
            Record::Class { address, name, .. } => {
                s.classes.insert(address, name.to_owned());
            }
            Record::Object { class, refs, .. } => {
                let e = s.instances.entry(class).or_default();
                e.0 += 1;
                e.1 += refs.len() as u64;
            }
            Record::ObjArray { size, .. } => {
                s.obj_arrays += 1;
                s.sized += size.is_some() as u64;
            }
            Record::PrimArray { ty: t, length, size, .. } => {
                *s.prim_arrays.entry(t).or_default() += 1;
                s.sized += size.is_some() as u64;
                if t == ty::BYTE && length == 3 << 20 {
                    s.big_byte_arrays += 1;
                    assert!(size.unwrap() >= 3 << 20);
                }
            }
        }
    }
    (r.header.clone(), s)
}

fn count_of(s: &Stats, class: &str) -> (u64, u64) {
    let addr = s.classes.iter().find(|(_, n)| *n == class).map(|(a, _)| *a).unwrap();
    s.instances.get(&addr).copied().unwrap_or_default()
}

#[test]
fn parses_openj9_dumps() {
    for name in ["compressed.phd.gz", "uncompressed.phd.gz"] {
        let (h, s) = stats(name);
        eprintln!("{name}: {h:?} classes={} unsorted={}", s.classes.len(), s.unsorted);
        assert!(h.is_64bit() && h.is_j9());
        assert_eq!(h.version, 6);
        assert!(h.full_version.as_deref().unwrap().contains("JRE 21"));
        assert!(s.classes.len() > 400);
        // 1000 nodes, each referencing the next (but the last) and every tenth a long[4].
        assert_eq!(count_of(&s, "PhdFixture$Node"), (1000, 999 + 100));
        // 100 Wide objects with 9 non-null fields each.
        assert_eq!(count_of(&s, "PhdFixture$Wide"), (100, 900));
        assert_eq!(s.big_byte_arrays, 1);
        assert!(s.prim_arrays[&ty::LONG] >= 100);
        assert!(s.prim_arrays[&ty::DOUBLE] >= 100);
        assert!(s.classes.values().any(|n| n == "[[I"));
        assert!(s.classes.values().any(|n| n == "[Ljava/lang/Object;"));
    }
}

#[test]
fn converts_to_hprof() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("c.hprof");
    let conv = openprofiler_phd::to_hprof(&fixture("compressed.phd.gz"), &out, 1234, &|_| {}).unwrap();
    eprintln!("{conv:?}");
    assert_eq!(conv.id_size, 8);
    assert!(conv.roots > 0 && conv.dangling_references == 0);
    let h = Hprof::open(&out).unwrap();
    assert_eq!(h.header.timestamp_ms, 1234);
    let top = h.scan().unwrap();
    assert!(h.string(&top, 1).starts_with(HPROF_MARKER));
    let mut objects = 0u64;
    let mut big = 0;
    for &seg in &top.segments {
        for r in h.sub_records(seg) {
            let r = r.unwrap();
            if r.object_id().is_some() {
                objects += 1;
            }
            if let SubRecord::PrimArray { ty: t, len, data, stack_serial, .. } = r
                && t == ty::BYTE
                && len == 3 << 20
            {
                big += 1;
                assert_eq!(data.len(), 3 << 20);
                assert!(data.iter().all(|&b| b == 0));
                assert!(stack_serial as u64 * 4 >= 3 << 20);
            }
        }
    }
    assert_eq!(objects, conv.objects);
    assert_eq!(big, 1);
}
