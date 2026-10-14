//! `ember compare` reports and CI regression gates (`--fail-if "cpu.total > +10%"`).

use anyhow::{Result, bail};
use ember_daemon::Session;
use ember_model::SampleKind;
use ember_model::views::{TreeOptions, hot_spots, key_label};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct MethodChange {
    pub method: String,
    pub a_self: u64,
    pub b_self: u64,
    pub a_total: u64,
    pub b_total: u64,
    /// Change of the method's share of all samples, in percentage points (self time).
    pub self_share_delta: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareReport {
    pub kind: String,
    pub a: String,
    pub b: String,
    pub a_total: u64,
    pub b_total: u64,
    /// Relative change of the total, in percent.
    pub total_change: f64,
    pub methods: Vec<MethodChange>,
    #[serde(skip)]
    all: HashMap<String, MethodChange>,
}

fn per_method(s: &Session, kind: SampleKind) -> Result<(u64, HashMap<String, (u64, u64)>)> {
    let prof = s.require_profile()?;
    let p = prof.read();
    let opts = TreeOptions::default();
    let (spots, extra) = hot_spots(&p, kind, &opts);
    let total = p.samples.get(&kind).map(|s| s.total_weight()).unwrap_or(0);
    Ok((
        total,
        spots
            .iter()
            .map(|h| {
                (key_label(&p.symbols, opts.aggregation, h.key, &extra), (h.self_weight, h.total_weight))
            })
            .collect(),
    ))
}

fn rel(a: u64, b: u64) -> f64 {
    if a == 0 { if b == 0 { 0.0 } else { f64::INFINITY } } else { 100.0 * (b as f64 - a as f64) / a as f64 }
}

pub fn compare_report(a: &Session, b: &Session, kind: SampleKind, limit: usize) -> Result<CompareReport> {
    let (at, am) = per_method(a, kind)?;
    let (bt, bm) = per_method(b, kind)?;
    let share = |w: u64, t: u64| if t == 0 { 0.0 } else { 100.0 * w as f64 / t as f64 };
    let mut all = HashMap::new();
    for name in am.keys().chain(bm.keys()) {
        if all.contains_key(name) {
            continue;
        }
        let (a_self, a_total) = am.get(name).copied().unwrap_or_default();
        let (b_self, b_total) = bm.get(name).copied().unwrap_or_default();
        all.insert(
            name.clone(),
            MethodChange {
                method: name.clone(),
                a_self,
                b_self,
                a_total,
                b_total,
                self_share_delta: share(b_self, bt) - share(a_self, at),
            },
        );
    }
    let mut methods: Vec<MethodChange> = all.values().cloned().collect();
    methods.sort_by(|x, y| {
        y.self_share_delta.abs().total_cmp(&x.self_share_delta.abs()).then(x.method.cmp(&y.method))
    });
    methods.truncate(limit);
    Ok(CompareReport {
        kind: kind.name().into(),
        a: a.name.clone(),
        b: b.name.clone(),
        a_total: at,
        b_total: bt,
        total_change: rel(at, bt),
        methods,
        all,
    })
}

pub fn print_report(r: &CompareReport) {
    println!("{} → {}  ({})", r.a, r.b, r.kind);
    println!("total: {} → {} ({:+.1}%)", r.a_total, r.b_total, r.total_change);
    println!("{:>9}  {:>8} {:>8}  method", "Δ self pp", "a self%", "b self%");
    for m in &r.methods {
        let pa = 100.0 * m.a_self as f64 / r.a_total.max(1) as f64;
        let pb = 100.0 * m.b_self as f64 / r.b_total.max(1) as f64;
        println!("{:>+9.2}  {:>7.2}% {:>7.2}%  {}", m.self_share_delta, pa, pb, m.method);
    }
}

/// Evaluates gate expressions; returns the ones that tripped (as human-readable messages).
///
/// Grammar: `<metric> <op> <number><unit>` where
/// - metric: `<kind>.total` (relative change of the total, `%`),
///   `method:<label>` (relative change of the method's inherent total, `%`) or
///   `method:<label>.share` (change of its self share, percentage points `pp`);
/// - op: `>`, `>=`, `<`, `<=`;
/// - number: signed, e.g. `+10%`, `-5%`, `2pp`.
pub fn evaluate(r: &CompareReport, gates: &[String]) -> Result<Vec<String>> {
    let mut failed = vec![];
    for g in gates {
        let (metric, op, threshold) = parse_gate(g)?;
        let value = if let Some(name) = metric.strip_prefix("method:") {
            let (name, share) = match name.strip_suffix(".share") {
                Some(n) => (n, true),
                None => (name, false),
            };
            match r.all.get(name) {
                Some(m) if share => m.self_share_delta,
                Some(m) => rel(m.a_total, m.b_total),
                None => 0.0,
            }
        } else if let Some(k) = metric.strip_suffix(".total") {
            if k != r.kind {
                bail!("gate `{g}` refers to {k} but the comparison is for {} (use --kind {k})", r.kind);
            }
            r.total_change
        } else {
            bail!("unknown gate metric `{metric}` in `{g}`");
        };
        let trip = match op {
            ">" => value > threshold,
            ">=" => value >= threshold,
            "<" => value < threshold,
            "<=" => value <= threshold,
            _ => unreachable!(),
        };
        if trip {
            failed.push(format!("{g} (actual {value:+.2})"));
        }
    }
    Ok(failed)
}

fn parse_gate(g: &str) -> Result<(String, &'static str, f64)> {
    for op in [">=", "<=", ">", "<"] {
        if let Some((m, v)) = g.split_once(op) {
            let v = v.trim().trim_end_matches('%').trim_end_matches("pp").trim();
            let threshold: f64 =
                v.trim_start_matches('+').parse().map_err(|_| anyhow::anyhow!("bad number in gate `{g}`"))?;
            return Ok((m.trim().to_owned(), op, threshold));
        }
    }
    bail!("gate `{g}` needs an operator (>, >=, <, <=)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates() {
        assert_eq!(parse_gate("cpu.total > +10%").unwrap(), ("cpu.total".into(), ">", 10.0));
        assert_eq!(
            parse_gate("method:a.B.c.share >= -2pp").unwrap(),
            ("method:a.B.c.share".into(), ">=", -2.0)
        );
        let engine = ember_daemon::Engine::new();
        let a = engine.add_profile(ember_daemon::SessionKind::Ember, "a", ember_daemon::demo::demo_profile());
        let b = engine.add_profile(ember_daemon::SessionKind::Ember, "b", ember_daemon::demo::demo_profile());
        let r = compare_report(&a, &b, SampleKind::Cpu, 10).unwrap();
        assert_eq!(r.total_change, 0.0);
        assert!(evaluate(&r, &["cpu.total > +10%".into()]).unwrap().is_empty());
        assert_eq!(evaluate(&r, &["cpu.total >= 0%".into()]).unwrap().len(), 1);
        assert!(evaluate(&r, &["alloc.total > 1%".into()]).is_err());
    }
}
