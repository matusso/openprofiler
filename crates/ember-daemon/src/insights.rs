//! Rule-based findings shown first in the UI ("Insights") and by the MCP `insights` tool.

use crate::engine::Session;
use ember_model::views::{SampleFilter, TreeOptions, hot_spots, key_label};
use ember_model::*;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct Insight {
    pub kind: String,
    pub title: String,
    pub severity: Severity,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wasted_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Query API view that shows the evidence (`cpu.hotspots`, `locks`, `heap.inspection:<kind>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
}

fn pct(a: f64, b: f64) -> f64 {
    if b <= 0.0 { 0.0 } else { 100.0 * a / b }
}

pub fn profile_insights(p: &Profile) -> Vec<Insight> {
    let mut out = vec![];
    let duration = (p.meta.end - p.meta.start).max(1) as f64;

    // GC overhead.
    let pause: i64 = p.gc.iter().map(|g| g.pause.unwrap_or(g.duration)).sum();
    let gc_pct = pct(pause as f64, duration);
    if gc_pct >= 2.0 {
        let longest = p.gc.iter().map(|g| g.pause.unwrap_or(g.duration)).max().unwrap_or(0);
        out.push(Insight {
            kind: "gc_overhead".into(),
            title: format!("GC pauses take {gc_pct:.1}% of the time"),
            severity: if gc_pct >= 10.0 { Severity::Critical } else { Severity::Warning },
            summary: format!(
                "{} collections paused the application for {:.0} ms in total (longest {:.0} ms).",
                p.gc.len(),
                pause as f64 / 1e6,
                longest as f64 / 1e6
            ),
            wasted_bytes: None,
            count: Some(p.gc.len() as u64),
            view: Some("overview.gc".into()),
        });
    }

    // Dominant CPU hot spot.
    let opts = TreeOptions::default();
    let (spots, extra) = hot_spots(p, SampleKind::Cpu, &opts);
    let cpu_total = p.samples.get(&SampleKind::Cpu).map(|s| s.total_weight()).unwrap_or(0) as f64;
    if let Some(top) = spots.first() {
        let share = pct(top.self_weight as f64, cpu_total);
        if share >= 15.0 {
            out.push(Insight {
                kind: "cpu_hotspot".into(),
                title: format!(
                    "{} uses {share:.0}% of CPU",
                    key_label(&p.symbols, opts.aggregation, top.key, &extra)
                ),
                severity: if share >= 40.0 { Severity::Warning } else { Severity::Info },
                summary: "A single method accounts for a large share of sampled CPU time. Inspect its back traces to see which callers drive it.".into(),
                wasted_bytes: None,
                count: None,
                view: Some(format!("cpu.hotspots:{}", top.key)),
            });
        }
    }

    // Lock contention.
    let wall_total = p.samples.get(&SampleKind::Wall).map(|s| s.total_weight()).unwrap_or(0) as f64;
    let blocked: u64 = p
        .samples
        .get(&SampleKind::Wall)
        .map(|s| (0..s.len()).filter(|&i| s.state[i] == ThreadState::Blocked).map(|i| s.weight[i]).sum())
        .unwrap_or(0);
    let mut by_class: std::collections::BTreeMap<&str, (u64, i64)> = Default::default();
    for e in &p.monitors {
        if e.kind == MonitorEventKind::Enter {
            let x = by_class.entry(&e.monitor_class).or_default();
            x.0 += 1;
            x.1 += e.duration;
        }
    }
    if let Some((class, (n, dur))) = by_class.iter().max_by_key(|(_, (_, d))| *d) {
        let share = pct(blocked as f64, wall_total);
        if *dur > 0 && (share >= 2.0 || wall_total == 0.0) {
            out.push(Insight {
                kind: "lock_contention".into(),
                title: format!("Lock contention on {class}"),
                severity: if share >= 10.0 { Severity::Warning } else { Severity::Info },
                summary: format!(
                    "{n} contended monitor entries blocked threads for {:.0} ms in total{}.",
                    *dur as f64 / 1e6,
                    if wall_total > 0.0 {
                        format!(" ({share:.1}% of sampled thread time is BLOCKED)")
                    } else {
                        String::new()
                    }
                ),
                wasted_bytes: None,
                count: Some(*n),
                view: Some("locks".into()),
            });
        }
    }

    // Connection-pool / thread-pool starvation: waiting inside a pool acquire.
    if let Some(wall) = p.samples.get(&SampleKind::Wall) {
        let mut frames = vec![];
        let mut waiting = 0u64;
        let mut seen = std::collections::HashMap::new();
        for i in 0..wall.len() {
            if wall.state[i] != ThreadState::Waiting {
                continue;
            }
            let hit = *seen.entry(wall.stack[i]).or_insert_with(|| {
                p.stacks.frames_root_first(wall.stack[i], &mut frames);
                frames.iter().any(|&f| {
                    let m = p.symbols.method(p.symbols.frame(f).method);
                    let (c, n) = (p.symbols.str(m.class), p.symbols.str(m.name));
                    (c.contains("Pool") || c.contains("DataSource")) && n.contains("getConnection")
                })
            });
            if hit {
                waiting += wall.weight[i];
            }
        }
        let share = pct(waiting as f64, wall_total);
        if share >= 1.0 {
            out.push(Insight {
                kind: "pool_starvation".into(),
                title: "Threads wait for database connections".into(),
                severity: if share >= 5.0 { Severity::Warning } else { Severity::Info },
                summary: format!(
                    "{share:.1}% of sampled thread time is spent waiting in a connection pool. The pool may be too small, or connections are held too long."
                ),
                wasted_bytes: None,
                count: None,
                view: Some("cpu.wall".into()),
            });
        }
    }

    // Allocation pressure.
    if let Some(rate) = p.telemetry("alloc.rate") {
        let avg = rate.value.iter().sum::<f64>() / rate.value.len().max(1) as f64;
        if avg >= 512.0 * 1024.0 * 1024.0 {
            out.push(Insight {
                kind: "alloc_rate".into(),
                title: format!("High allocation rate ({:.0} MB/s)", avg / 1048576.0),
                severity: Severity::Warning,
                summary: "Allocation pressure drives GC frequency. See allocation hot spots.".into(),
                wasted_bytes: None,
                count: None,
                view: Some("memory.hotspots".into()),
            });
        }
    }
    let _ = SampleFilter::default();
    out.sort_by_key(|i| std::cmp::Reverse(i.severity));
    out
}

pub fn session_insights(s: &Session) -> anyhow::Result<Vec<Insight>> {
    let mut out = vec![];
    if let Some(p) = s.profile() {
        out.extend(profile_insights(&p.read()));
    }
    if let Some(h) = s.heap() {
        out.extend(crate::heap::insights(&h)?);
    }
    out.sort_by_key(|i| std::cmp::Reverse(i.severity));
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn demo_has_insights() {
        let p = crate::demo::demo_profile();
        let i = super::profile_insights(&p);
        let kinds: Vec<_> = i.iter().map(|i| i.kind.as_str()).collect();
        assert!(kinds.contains(&"lock_contention"), "{kinds:?}");
        assert!(kinds.contains(&"pool_starvation"), "{kinds:?}");
    }
}
