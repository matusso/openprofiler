//! Prints a summary of an `.oplink` recording: sample counts, top leaf frames, telemetry keys.
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
    Ok(())
}
