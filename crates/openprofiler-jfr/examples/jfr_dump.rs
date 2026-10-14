//! Prints a summary and the top hot spots of a JFR recording.
//!
//! `cargo run --release -p openprofiler-jfr --example jfr_dump -- recording.jfr [--events]`

use openprofiler_model::views::{TreeOptions, hot_spots, key_label};
use openprofiler_model::{SampleKind, Unit};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or_else(|| anyhow::anyhow!("usage: jfr_dump <file.jfr> [--events]"))?;
    let events = args.any(|a| a == "--events");
    let path = std::path::Path::new(&path);
    let size = std::fs::metadata(path)?.len();

    let t0 = Instant::now();
    let p = openprofiler_jfr::parse_file(path)?;
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "parsed {} ({:.1} MB) in {:.3}s = {:.1} MB/s",
        path.display(),
        size as f64 / 1e6,
        secs,
        size as f64 / 1e6 / secs
    );
    let j = &p.meta.jvm;
    println!(
        "jvm: {} {} ({}) java {} on {} {}",
        j.vm_name, j.vm_version, j.vm_vendor, j.java_version, j.os, j.arch
    );
    println!("main: {}  pid: {:?}  cpus: {:?}  max heap: {:?}", j.main_class, j.pid, j.cpus, j.max_heap);
    println!(
        "duration: {:.1}s  chunks: {}  threads: {}  stacks: {}  frames: {}",
        (p.meta.end - p.meta.start) as f64 / 1e9,
        p.meta.properties.get("jfr.chunks").map(String::as_str).unwrap_or("?"),
        p.threads.len(),
        p.stacks.len(),
        p.symbols.frames().len()
    );
    if let Some(w) = p.meta.properties.get("jfr.warnings") {
        println!("warnings: {w}");
    }
    for (k, s) in &p.samples {
        println!("samples {:<10} rows={:<8} weight={}", k.name(), s.len(), s.total_weight());
    }
    for s in &p.telemetry {
        let last = s.value.last().copied().unwrap_or(0.0);
        let unit = match s.unit {
            Unit::Bytes => "B",
            Unit::Nanos => "ns",
            Unit::Percent => "%",
            Unit::PerSecond => "/s",
            Unit::Count => "",
        };
        println!("telemetry {:<22} points={:<6} last={last:.1}{unit}", s.key, s.time.len());
    }
    println!(
        "gc events: {}  monitor events: {}  thread dumps: {}",
        p.gc.len(),
        p.monitors.len(),
        p.thread_dumps.len()
    );

    for kind in [SampleKind::Cpu, SampleKind::Alloc] {
        if !p.samples.contains_key(&kind) {
            continue;
        }
        let opts = TreeOptions::default();
        let (hs, extra) = hot_spots(&p, kind, &opts);
        let total: u64 = p.samples[&kind].total_weight();
        println!("\ntop {} hot spots:", kind.name());
        for h in hs.iter().take(10) {
            println!(
                "  {:5.1}% self {:5.1}% total  {}",
                h.self_weight as f64 * 100.0 / total.max(1) as f64,
                h.total_weight as f64 * 100.0 / total.max(1) as f64,
                key_label(&p.symbols, opts.aggregation, h.key, &extra)
            );
        }
    }

    if events {
        let reader = openprofiler_jfr::JfrReader::open(path)?;
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for e in reader.events()? {
            *counts.entry(e?.type_name).or_default() += 1;
        }
        println!("\nevents by type:");
        for (k, v) in counts {
            println!("  {k:<45} {v}");
        }
    }
    Ok(())
}
