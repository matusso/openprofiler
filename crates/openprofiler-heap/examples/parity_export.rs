//! Exports what `scripts/mat-parity.sh` compares against Eclipse MAT: per-object sizes, the
//! class histogram and the top of the dominator tree.
//!
//! `cargo run --release -p openprofiler-heap --example parity_export -- dump.hprof out_dir`
//!
//! Writes `objects.csv` (`address,shallow,retained` for reachable objects), `histogram.csv`
//! (`class,objects,shallow`) and `dominators.csv` (`address,class,shallow,retained` for the
//! top-level dominator tree entries, largest first).

use openprofiler_heap::{ClassQuery, HeapAnalysis, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dump = PathBuf::from(args.next().expect("usage: parity_export <dump> <out_dir>"));
    let out = PathBuf::from(args.next().expect("usage: parity_export <dump> <out_dir>"));
    std::fs::create_dir_all(&out)?;
    let h = HeapAnalysis::open(&dump, &OpenOptions::default(), |_, _| {})?;

    let mut w = std::io::BufWriter::new(std::fs::File::create(out.join("objects.csv"))?);
    for o in 0..h.object_count() as u32 {
        if h.is_reachable(o) {
            let r = h.object_row(o);
            writeln!(w, "{},{},{}", r.address, r.shallow, h.retained_size(o))?;
        }
    }
    w.flush()?;

    let mut w = std::io::BufWriter::new(std::fs::File::create(out.join("histogram.csv"))?);
    for r in h.classes(&ClassQuery::default())?.rows {
        writeln!(w, "{},{},{}", r.name, r.instances, r.shallow)?;
    }
    w.flush()?;

    let mut w = std::io::BufWriter::new(std::fs::File::create(out.join("dominators.csv"))?);
    for r in h.biggest(None, usize::MAX)? {
        writeln!(w, "{},{},{},{}", r.address, r.class_name, r.shallow, r.retained.unwrap_or(0))?;
    }
    w.flush()?;
    let s = h.summary();
    eprintln!(
        "openprofiler: {} reachable objects, {} bytes reachable shallow",
        s.reachable_objects, s.reachable_shallow
    );
    Ok(())
}
