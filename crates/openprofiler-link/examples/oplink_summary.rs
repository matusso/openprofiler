//! Prints a summary of an `.oplink` recording: sample counts, top leaf frames, telemetry keys,
//! probe hot spots, events and control objects.
//! Usage: cargo run -p openprofiler-link --example oplink_summary -- <file.oplink>

use std::collections::HashMap;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: oplink_summary <file>");
    let p = openprofiler_link::replay_recording(std::path::Path::new(&path))?;
    println!("jvm: {:?} {} {}", p.meta.jvm.pid, p.meta.jvm.vm_name, p.meta.jvm.java_version);
    println!("caps: {:?}", p.meta.properties.get("agent_capabilities"));
    println!("threads: {}", p.threads.len());
    for (kind, set) in &p.samples {
        let mut leaves: HashMap<String, u64> = HashMap::new();
        let mut frames = vec![];
        for i in 0..set.len() {
            p.stacks.frames_root_first(set.stack[i], &mut frames);
            let label = frames
                .last()
                .map(|&f| p.symbols.method_label(p.symbols.frame(f).method))
                .unwrap_or_else(|| "<empty>".into());
            *leaves.entry(label).or_default() += set.count[i] as u64;
        }
        let mut top: Vec<_> = leaves.into_iter().collect();
        top.sort_by_key(|e| std::cmp::Reverse(e.1));
        println!(
            "{kind:?}: {} rows, {} samples",
            set.len(),
            set.count.iter().map(|&c| c as u64).sum::<u64>()
        );
        for (l, n) in top.iter().take(8) {
            println!("    {n:>7}  {l}");
        }
    }
    let keys: Vec<_> = p.telemetry.iter().map(|s| format!("{}({})", s.key, s.value.len())).collect();
    println!("telemetry: {}", keys.join(", "));
    println!(
        "gc events: {}, monitor events: {}, state spans: {}",
        p.gc.len(),
        p.monitors.len(),
        p.thread_states.len()
    );
    let pr = &p.probes;
    for (i, info) in pr.probes.iter().enumerate() {
        let mut by_payload: HashMap<&str, (u64, i64)> = HashMap::new();
        for c in pr.calls.iter().filter(|c| c.probe as usize == i) {
            let e = by_payload.entry(p.symbols.str(c.payload)).or_default();
            e.0 += c.count as u64;
            e.1 += c.duration;
        }
        let events = pr.events.iter().filter(|e| e.probe as usize == i).count();
        let controls = pr.controls.iter().filter(|e| e.probe as usize == i).count();
        println!(
            "probe {} ({}): {} payloads, {events} events, {controls} control events",
            info.id,
            info.name,
            by_payload.len()
        );
        let mut top: Vec<_> = by_payload.into_iter().collect();
        top.sort_by_key(|e| std::cmp::Reverse(e.1.1));
        for (payload, (n, d)) in top.iter().take(5) {
            println!("    {n:>6} calls {:>9.3} ms  {payload}", *d as f64 / 1e6);
        }
        if let Some(e) =
            pr.events.iter().find(|e| e.probe as usize == i && (e.context.is_some() || e.detail.is_some()))
        {
            println!(
                "    event: {} detail={:?} context={:?} error={:?}",
                p.symbols.str(e.payload),
                e.detail,
                e.context,
                e.error
            );
        }
    }
    println!("bookmarks: {}", p.bookmarks.iter().map(|b| b.label.as_str()).collect::<Vec<_>>().join(", "));
    Ok(())
}
