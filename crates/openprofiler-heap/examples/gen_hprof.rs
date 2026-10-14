//! Writes a synthetic HPROF of roughly the requested size for benchmarking.
//!
//! `cargo run --release -p openprofiler-heap --example gen_hprof -- out.hprof 4.0`
//!
//! The heap is made of clusters: an `ArrayList` (JNI global root) holding an `Object[64]` of
//! `app.Node`s chained via `next`, cross-linked to earlier clusters via `left`, each with a
//! `byte[]` payload; every fourth node has a `String` name drawn from 1000 values (duplicates).

use openprofiler_hprof::writer::HprofWriter;
use openprofiler_hprof::{tag, ty};
use std::io::BufWriter;

const PER_CLUSTER: u64 = 162;
const BASE: u64 = 0x7_0000_0000;

fn addr(cluster: u64, slot: u64) -> u64 {
    BASE + (cluster * PER_CLUSTER + slot) * 32
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).expect("usage: gen_hprof <out> <gigabytes>");
    let gb: f64 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(1.0);
    let target = (gb * (1u64 << 30) as f64) as u64;
    let f = std::fs::File::create(out)?;
    let mut w = HprofWriter::new(BufWriter::with_capacity(16 << 20, f), 8, 1_700_000_000_000)?;
    w.set_segment_limit(256 << 20);
    let names = [
        "java/lang/Object",
        "java/lang/Class",
        "java/lang/String",
        "[B",
        "[Ljava/lang/Object;",
        "app/Node",
        "java/util/ArrayList",
        "value",
        "coder",
        "hash",
        "next",
        "left",
        "payload",
        "name",
        "id",
        "elementData",
        "size",
    ];
    for (i, n) in names.iter().enumerate() {
        w.utf8(i as u64 + 1, n)?;
    }
    let class = |i: u64| 0x1000 + i * 0x10;
    for i in 0..7u64 {
        w.load_class(i as u32 + 1, class(i), i + 1)?;
    }
    w.class_dump(class(0), 0, 0, 0, &[], &[])?;
    w.class_dump(class(1), class(0), 0, 0, &[], &[])?;
    w.class_dump(class(2), class(0), 0, 0, &[], &[(8, ty::OBJECT), (9, ty::BYTE), (10, ty::INT)])?;
    w.class_dump(class(3), class(0), 0, 0, &[], &[])?;
    w.class_dump(class(4), class(0), 0, 0, &[], &[])?;
    w.class_dump(
        class(5),
        class(0),
        0,
        0,
        &[],
        &[(11, ty::OBJECT), (12, ty::OBJECT), (13, ty::OBJECT), (14, ty::OBJECT), (15, ty::LONG)],
    )?;
    w.class_dump(class(6), class(0), 0, 0, &[], &[(16, ty::OBJECT), (17, ty::INT)])?;
    for i in 0..7 {
        w.root(tag::ROOT_STICKY_CLASS, class(i))?;
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut written = 0u64;
    let mut c = 0u64;
    let mut data = Vec::with_capacity(256);
    let mut objects = 0u64;
    while written < target {
        // slots: 0 list, 1 array, 2..66 nodes, 66..130 payloads, 130..146 strings, 146..162 values
        w.root(tag::ROOT_JNI_GLOBAL, addr(c, 0))?;
        data.clear();
        data.extend_from_slice(&addr(c, 1).to_be_bytes());
        data.extend_from_slice(&64u32.to_be_bytes());
        w.instance(addr(c, 0), class(6), &data)?;
        let elems: Vec<u64> = (0..64).map(|k| addr(c, 2 + k)).collect();
        w.obj_array(addr(c, 1), class(4), &elems)?;
        written += 40 + 30 + 64 * 8;
        for k in 0..64u64 {
            let next = if k > 0 { addr(c, 2 + k - 1) } else { 0 };
            let left = if c > 0 && rng.next().is_multiple_of(3) {
                let oc = rng.next() % c;
                addr(oc, 2 + rng.next() % 64)
            } else {
                0
            };
            let name = if k % 4 == 0 { addr(c, 130 + k / 4) } else { 0 };
            data.clear();
            for v in [next, left, addr(c, 66 + k), name, c * 64 + k] {
                data.extend_from_slice(&v.to_be_bytes());
            }
            w.instance(addr(c, 2 + k), class(5), &data)?;
            let len = 16 + rng.next() % 184;
            data.clear();
            data.extend((0..len).map(|i| (i as u8) ^ (c as u8) | 1));
            w.prim_array(addr(c, 66 + k), ty::BYTE, len as u32, &data)?;
            written += 62 + 18 + len;
        }
        for s in 0..16u64 {
            let text = format!("name-{}", rng.next() % 1000);
            data.clear();
            data.extend_from_slice(&addr(c, 146 + s).to_be_bytes());
            data.push(0);
            data.extend_from_slice(&0u32.to_be_bytes());
            w.instance(addr(c, 130 + s), class(2), &data)?;
            w.prim_array(addr(c, 146 + s), ty::BYTE, text.len() as u32, text.as_bytes())?;
            written += 38 + 18 + text.len() as u64;
        }
        objects += PER_CLUSTER;
        c += 1;
    }
    w.finish()?;
    eprintln!(
        "wrote {out}: {c} clusters, {objects} objects, ~{:.2} GB",
        written as f64 / (1u64 << 30) as f64
    );
    Ok(())
}
