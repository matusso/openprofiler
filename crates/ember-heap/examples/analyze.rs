//! Indexes a heap dump and times the phases, a warm reopen and a few queries.
//!
//! `cargo run --release -p ember-heap --example analyze -- dump.hprof [--rebuild]`

use ember_heap::{ClassQuery, ClassSort, HeapAnalysis, OpenOptions, Phase};
use std::sync::Mutex;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = std::path::PathBuf::from(args.get(1).expect("usage: analyze <dump> [--rebuild]"));
    let rebuild = args.iter().any(|a| a == "--rebuild");
    let size = std::fs::metadata(&path)?.len();
    let start = Instant::now();
    let last: Mutex<Option<(Phase, Instant)>> = Mutex::new(None);
    let progress = |p: Phase, _f: f64| {
        let mut l = last.lock().unwrap();
        if l.map(|x| x.0) != Some(p) {
            if let Some((prev, t)) = *l {
                eprintln!("  {prev:?}: {:.2}s", t.elapsed().as_secs_f64());
            }
            *l = Some((p, Instant::now()));
        }
    };
    let opts = OpenOptions { force_rebuild: rebuild, ..Default::default() };
    let h = HeapAnalysis::open(&path, &opts, progress)?;
    let cold = start.elapsed().as_secs_f64();
    let s = h.summary();
    println!(
        "open: {cold:.2}s  ({:.2} GB, {} objects, {} edges, {:.1} M objects/s, {:.0} MB/s)",
        size as f64 / (1u64 << 30) as f64,
        s.objects,
        s.edges,
        s.objects as f64 / cold / 1e6,
        size as f64 / cold / (1 << 20) as f64
    );
    println!("summary: {}", serde_json::to_string(&s)?);
    drop(h);

    let t = Instant::now();
    let h = HeapAnalysis::open(&path, &OpenOptions::default(), |_, _| {})?;
    println!("warm reopen: {:.3}s", t.elapsed().as_secs_f64());

    let t = Instant::now();
    let rows = h.classes(&ClassQuery { sort: ClassSort::Retained, limit: Some(5), ..Default::default() })?;
    println!("classes (top 5 by retained) in {:.3}s:", t.elapsed().as_secs_f64());
    for r in rows.rows {
        println!(
            "  {:<40} {:>12} inst {:>14} shallow {:>14?} retained",
            r.name, r.instances, r.shallow, r.retained
        );
    }
    let t = Instant::now();
    let big = h.biggest(None, 3)?;
    println!(
        "biggest in {:.3}s: {:?}",
        t.elapsed().as_secs_f64(),
        big.iter().map(|b| b.retained).collect::<Vec<_>>()
    );
    if let Some(b) = big.first() {
        let t = Instant::now();
        let p = h.paths_to_roots(b.id, 3, true)?;
        println!("paths to roots in {:.3}s ({} paths)", t.elapsed().as_secs_f64(), p.len());
    }
    let t = Instant::now();
    let dup = h.inspection("duplicate_strings", 3)?;
    println!("duplicate_strings in {:.2}s: {}", t.elapsed().as_secs_f64(), dup.insight.summary);
    Ok(())
}
