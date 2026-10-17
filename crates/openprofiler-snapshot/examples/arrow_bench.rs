//! Compares the 1.0 (protobuf) and 1.1 (Arrow) encodings of a snapshot: size per section kind
//! and write/read time. Usage: `cargo run --release --example arrow_bench -- file.opsnap`.

use openprofiler_snapshot::*;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

fn sizes(path: &Path) -> BTreeMap<u32, u64> {
    let r = SnapshotReader::open(path).unwrap();
    let mut m = BTreeMap::new();
    for e in &r.info.sections {
        *m.entry(e.kind).or_default() += e.length;
    }
    m
}

fn main() {
    let input = std::env::args().nth(1).expect("usage: arrow_bench <file.opsnap>");
    let (p, _) = read_snapshot(Path::new(&input)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("v10.opsnap"), dir.path().join("v11.opsnap"));
    let n = 5;
    let time = |f: &dyn Fn()| {
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        t.elapsed().as_secs_f64() * 1000.0 / n as f64
    };
    let w10 = time(&|| write_snapshot_v1_0(&a, &p, None).unwrap());
    let w11 = time(&|| write_snapshot(&b, &p, None).unwrap());
    let r10 = time(&|| drop(read_snapshot(&a).unwrap()));
    let r11 = time(&|| drop(read_snapshot(&b).unwrap()));
    let (s10, s11) = (sizes(&a), sizes(&b));
    println!("{:<10} {:>12} {:>12}", "kind", "1.0 bytes", "1.1 bytes");
    for k in s10.keys() {
        println!("{k:<10} {:>12} {:>12}", s10[k], s11.get(k).copied().unwrap_or(0));
    }
    let len = |p: &Path| std::fs::metadata(p).unwrap().len();
    println!("{:<10} {:>12} {:>12}", "file", len(&a), len(&b));
    println!("write ms   {w10:>12.1} {w11:>12.1}");
    println!("read ms    {r10:>12.1} {r11:>12.1}");
}
