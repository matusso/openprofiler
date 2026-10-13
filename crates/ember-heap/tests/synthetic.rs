//! End-to-end checks on a small hand-built HPROF with known dominators and retained sizes.

use ember_heap::{ClassQuery, Direction, GroupBy, HeapAnalysis, OpenOptions, SelectStep};
use ember_hprof::writer::HprofWriter;
use ember_hprof::{tag, ty};
use std::io::BufWriter;
use std::path::Path;

const OBJECT: u64 = 0x1000;
const CLASS: u64 = 0x1010;
const STRING: u64 = 0x1020;
const BYTES: u64 = 0x1030;
const NODE: u64 = 0x1040;
const REFERENCE: u64 = 0x1050;
const WEAK: u64 = 0x1060;

const A: u64 = 0x10;
const B: u64 = 0x20;
const C: u64 = 0x30;
const D: u64 = 0x40;
const S1: u64 = 0x50;
const V1: u64 = 0x60;
const S2: u64 = 0x70;
const V2: u64 = 0x80;
const W: u64 = 0x90;
const E: u64 = 0xa0;
// 8 bytes off the 16-byte grid so the inferred object alignment (GCD of address gaps) is 8.
const U: u64 = 0xb8;

fn node(next: u64, data: u64) -> Vec<u8> {
    [next.to_be_bytes(), data.to_be_bytes()].concat()
}

fn write_dump(path: &Path) {
    let f = std::fs::File::create(path).unwrap();
    let mut w = HprofWriter::new(BufWriter::new(f), 8, 1_700_000_000_000).unwrap();
    let names = [
        (1, "java/lang/Object"),
        (2, "java/lang/Class"),
        (3, "java/lang/String"),
        (4, "[B"),
        (5, "app/Node"),
        (6, "java/lang/ref/Reference"),
        (7, "java/lang/ref/WeakReference"),
        (10, "value"),
        (11, "coder"),
        (12, "next"),
        (13, "data"),
        (14, "referent"),
        (15, "INSTANCE"),
    ];
    for (id, s) in names {
        w.utf8(id, s).unwrap();
    }
    for (i, (cls, name)) in
        [(OBJECT, 1), (CLASS, 2), (STRING, 3), (BYTES, 4), (NODE, 5), (REFERENCE, 6), (WEAK, 7)]
            .iter()
            .enumerate()
    {
        w.load_class(i as u32 + 1, *cls, *name).unwrap();
    }
    w.class_dump(OBJECT, 0, 0, 0, &[], &[]).unwrap();
    w.class_dump(CLASS, OBJECT, 0, 0, &[], &[]).unwrap();
    w.class_dump(STRING, OBJECT, 0, 0, &[], &[(10, ty::OBJECT), (11, ty::BYTE)]).unwrap();
    w.class_dump(BYTES, OBJECT, 0, 0, &[], &[]).unwrap();
    // Node has a static field pointing at D.
    w.class_dump(NODE, OBJECT, 0, 0, &[(15, ty::OBJECT, 0)], &[(12, ty::OBJECT), (13, ty::OBJECT)]).unwrap();
    w.class_dump(REFERENCE, OBJECT, 0, 0, &[], &[(14, ty::OBJECT)]).unwrap();
    w.class_dump(WEAK, REFERENCE, 0, 0, &[], &[]).unwrap();
    for c in [OBJECT, CLASS, STRING, BYTES, NODE, REFERENCE, WEAK] {
        w.root(tag::ROOT_STICKY_CLASS, c).unwrap();
    }
    w.root(tag::ROOT_JNI_GLOBAL, A).unwrap();
    w.root(tag::ROOT_JAVA_FRAME, D).unwrap();
    w.root(tag::ROOT_JNI_GLOBAL, W).unwrap();
    w.instance(A, NODE, &node(B, S1)).unwrap();
    w.instance(B, NODE, &node(C, S2)).unwrap();
    w.instance(C, NODE, &node(0, 0)).unwrap();
    w.instance(D, NODE, &node(C, 0)).unwrap();
    w.instance(S1, STRING, &[V1.to_be_bytes().as_slice(), &[0]].concat()).unwrap();
    w.prim_array(V1, ty::BYTE, 5, b"hello").unwrap();
    w.instance(S2, STRING, &[V2.to_be_bytes().as_slice(), &[0]].concat()).unwrap();
    w.prim_array(V2, ty::BYTE, 5, b"hello").unwrap();
    w.instance(W, WEAK, &E.to_be_bytes()).unwrap();
    w.instance(E, NODE, &node(0, 0)).unwrap();
    w.instance(U, NODE, &node(A, 0)).unwrap();
    w.finish().unwrap();
}

fn id_of(h: &HeapAnalysis, addr: u64) -> u32 {
    h.find_object(addr).expect("object")
}

/// The synthetic dump has no object arrays to infer reference size from; use 4-byte refs.
fn opts() -> OpenOptions {
    OpenOptions { compressed_oops: Some(true), ..Default::default() }
}

#[test]
fn synthetic_dump() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("syn.hprof");
    write_dump(&path);
    let h = HeapAnalysis::open(&path, &opts(), |_, _| {}).unwrap();
    let s = h.summary();
    // 11 instances/arrays, 7 classes and the bootstrap class loader pseudo-object.
    assert_eq!(s.objects, 11 + 7 + 1);
    assert_eq!(s.reachable_objects, 18, "everything but U");
    assert_eq!(s.id_size, 8);
    assert_eq!(s.dump_time, Some(1_700_000_000_000 * 1_000_000));

    let (a, b, c, d) = (id_of(&h, A), id_of(&h, B), id_of(&h, C), id_of(&h, D));
    let (s1, v2, e, u) = (id_of(&h, S1), id_of(&h, V2), id_of(&h, E), id_of(&h, U));
    assert_eq!(h.shallow_size(a), 24);
    assert_eq!(h.retained_size(b), 72);
    assert_eq!(h.retained_size(a), 144);
    assert_eq!(h.retained_size(d), 24);
    assert_eq!(h.retained_size(c), 24);
    assert_eq!(h.string_value(s1).as_deref(), Some("hello"));

    // Weak referents count for reachability and retained sizes, as in MAT.
    let w = id_of(&h, W);
    assert!(h.object(e).unwrap().reachable);
    assert_eq!(h.object(e).unwrap().dominator, Some(w));
    assert_eq!(h.retained_size(w), 16 + 24);
    // Class objects are as big as their statics: Node has one reference static.
    assert_eq!(h.shallow_size(id_of(&h, NODE)), 8);
    assert_eq!(h.shallow_size(id_of(&h, STRING)), 0);
    assert!(!h.object(u).unwrap().reachable);
    assert_eq!(h.object(b).unwrap().dominator, Some(a));
    assert_eq!(h.object(c).unwrap().dominator, None);

    // Retained-size invariant: super root retains every reachable object.
    let reachable: u64 =
        (0..s.objects as u32).filter(|&o| h.object(o).unwrap().reachable).map(|o| h.shallow_size(o)).sum();
    assert_eq!(reachable, s.reachable_shallow);
    let top: u64 = h.biggest(None, 1000).unwrap().iter().map(|r| r.retained.unwrap()).sum();
    assert_eq!(top, s.reachable_shallow);

    // References.
    let inc = h.refs(c, Direction::Incoming, 0, 10).unwrap();
    let mut fields: Vec<_> = inc.rows.iter().map(|r| (r.object.id, r.field.clone())).collect();
    fields.sort();
    let mut expect = vec![(b, "next".to_owned()), (d, "next".to_owned())];
    expect.sort();
    assert_eq!(fields, expect);
    let out = h.refs(a, Direction::Outgoing, 0, 10).unwrap();
    assert!(out.rows.iter().any(|r| r.field == "data" && r.object.id == s1));
    assert!(out.rows.iter().any(|r| r.field == "<class>"));
    let root_rows = h.refs(a, Direction::Incoming, 0, 10).unwrap();
    assert!(root_rows.rows.iter().any(|r| r.field == "<root: JNI global>"));

    // Paths to roots.
    let paths = h.paths_to_roots(v2, 5, true).unwrap();
    assert_eq!(paths.len(), 1);
    let p = &paths[0];
    assert_eq!(p.len(), 4);
    assert_eq!(p[0].object.id, v2);
    assert_eq!(p[0].field.as_deref(), Some("value"));
    assert_eq!(p[1].field.as_deref(), Some("data"));
    assert_eq!(p[2].field.as_deref(), Some("next"));
    assert_eq!(p[3].object.id, a);
    assert_eq!(p[3].root_kind.as_deref(), Some("JNI global"));
    assert!(h.paths_to_roots(e, 5, true).unwrap().is_empty());
    let weak = h.paths_to_roots(e, 5, false).unwrap();
    assert_eq!(weak[0][0].field.as_deref(), Some("referent"));

    // Classes and sets cover reachable objects only (U is garbage), like MAT.
    let rows = h.classes(&ClassQuery { filter: Some("node".into()), ..Default::default() }).unwrap().rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].instances, 5);
    let pkgs = h.classes(&ClassQuery { group_by: GroupBy::Package, ..Default::default() }).unwrap();
    assert!(pkgs.rows.iter().any(|r| r.name == "app" && r.instances == 5));
    let set = h.select(None, &SelectStep::ClassName("app.Node".into())).unwrap();
    assert_eq!(set.objects, 5);
    let only_a = h.select(None, &SelectStep::Objects(vec![a])).unwrap();
    assert_eq!(h.set_retained(only_a.id).unwrap(), 144);
    let retained = h.select(None, &SelectStep::RetainedBy(a)).unwrap();
    assert_eq!(retained.objects, 6);
    assert_eq!(retained.shallow, 144);
    let referenced = h.select(Some(set.id), &SelectStep::ReferencedBySet(true)).unwrap();
    assert!(referenced.objects >= 4);
    let merged = h.merged_refs(set.id, Direction::Outgoing).unwrap();
    assert!(merged.iter().any(|m| m.field == "data" && m.class_name == "java.lang.String" && m.objects == 2));

    // Inspections.
    let dup = h.inspection("duplicate_strings", 10).unwrap();
    assert_eq!(dup.insight.count, Some(1));
    assert_eq!(dup.insight.wasted_bytes, Some(48));
    assert_eq!(dup.rows[0].label, "\"hello\"");
    assert_eq!(h.inspections().len(), 10);
    let detail = h.object(a).unwrap();
    assert!(detail.fields.iter().any(|f| f.name == "next" && f.reference == Some(b)));
    assert!(detail.gc_root_kinds.contains(&"JNI global".to_owned()));

    // Warm reopen uses the cache and gives identical answers.
    drop(h);
    let h2 = HeapAnalysis::open(&path, &opts(), |p, _| assert_ne!(p, ember_heap::Phase::Graph)).unwrap();
    assert_eq!(h2.retained_size(a), 144);
    assert_eq!(h2.histogram_diff(&h2).iter().map(|r| r.a_instances).sum::<u64>(), 18);

    // Optional: ignore weak referents.
    let strong_only = OpenOptions { exclude_weak: true, cache_dir: Some(dir.path().join("weak")), ..opts() };
    let h3 = HeapAnalysis::open(&path, &strong_only, |_, _| {}).unwrap();
    assert!(!h3.object(id_of(&h3, E)).unwrap().reachable);
    assert_eq!(h3.summary().reachable_objects, 17);
}

#[test]
fn gzip_and_readonly_fallback() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("raw.hprof");
    write_dump(&raw);
    let gz = dir.path().join("dump.hprof.gz");
    let mut enc = flate2_like_gzip(&std::fs::read(&raw).unwrap());
    std::fs::File::create(&gz).unwrap().write_all(&enc).unwrap();
    enc.clear();
    let cache = dir.path().join("cache");
    let opts = OpenOptions { cache_dir: Some(cache.clone()), ..opts() };
    let h = HeapAnalysis::open(&gz, &opts, |_, _| {}).unwrap();
    assert_eq!(h.summary().objects, 19);
    assert!(cache.join("meta.json").exists());
}

/// Minimal gzip container with stored (uncompressed) deflate blocks.
fn flate2_like_gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let chunks: Vec<&[u8]> = data.chunks(65535).collect();
    for (i, c) in chunks.iter().enumerate() {
        out.push(if i + 1 == chunks.len() { 1 } else { 0 });
        let len = c.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}
