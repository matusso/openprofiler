//! View exporters: CSV, JSON, HTML, SVG flame graphs and folded ("collapsed") stacks.

use crate::engine::Session;
use crate::query::*;
use anyhow::{Result, bail};
use openprofiler_model::views::{BuiltTree, ROOT_NODE, build_tree, key_label};
use openprofiler_model::{Profile, SampleKind};
use serde::Deserialize;
use std::fmt::Write;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Csv,
    Json,
    Html,
    Svg,
    Collapsed,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Csv => "text/csv; charset=utf-8",
            Format::Json => "application/json",
            Format::Html => "text/html; charset=utf-8",
            Format::Svg => "image/svg+xml",
            Format::Collapsed => "text/plain; charset=utf-8",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Html => "html",
            Format::Svg => "svg",
            Format::Collapsed => "txt",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportRequest {
    pub view: String,
    pub format: Format,
    #[serde(default)]
    pub request: serde_json::Value,
}

pub struct Exported {
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_owned() }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn fmt_value(v: u64, unit: openprofiler_model::Unit) -> String {
    use openprofiler_model::Unit::*;
    match unit {
        Nanos => {
            let ms = v as f64 / 1e6;
            if ms >= 1000.0 { format!("{:.2} s", ms / 1000.0) } else { format!("{ms:.1} ms") }
        }
        Bytes => {
            let b = v as f64;
            if b >= 1073741824.0 {
                format!("{:.2} GB", b / 1073741824.0)
            } else if b >= 1048576.0 {
                format!("{:.1} MB", b / 1048576.0)
            } else if b >= 1024.0 {
                format!("{:.1} kB", b / 1024.0)
            } else {
                format!("{v} B")
            }
        }
        _ => v.to_string(),
    }
}

fn tree_req(req: &serde_json::Value) -> Result<TreeRequest> {
    let mut v = req.clone();
    if v.get("kind").is_none() {
        if let Some(o) = v.as_object_mut() {
            o.insert("kind".into(), "cpu".into());
        } else {
            v = serde_json::json!({ "kind": "cpu" });
        }
    }
    Ok(serde_json::from_value(v)?)
}

fn full_tree(p: &Profile, req: &TreeRequest) -> BuiltTree {
    let opts = openprofiler_model::views::TreeOptions {
        filter: req.filter.to_filter(p),
        aggregation: req.aggregation,
        inverted: req.view == TreeView::Backtraces || req.inverted,
        collapse_recursion: req.collapse_recursion,
        focus: req.focus,
        compact_prefixes: req.compact_prefixes.clone(),
    };
    build_tree(p, req.kind, &opts)
}

pub fn export(session: &Session, req: &ExportRequest) -> Result<Exported> {
    let f = req.format;
    let body: Vec<u8> = match req.view.as_str() {
        "calltree" | "backtraces" | "flame" => {
            let mut tr = tree_req(&req.request)?;
            if req.view == "backtraces" {
                tr.view = TreeView::Backtraces;
            }
            let prof = session.require_profile()?;
            let p = prof.read();
            let bt = full_tree(&p, &tr);
            let min_fraction = tr.min_fraction.unwrap_or(0.0);
            match f {
                Format::Csv => tree_csv(&p, &bt, tr.kind, min_fraction).into_bytes(),
                Format::Json => serde_json::to_vec_pretty(&tree_json(&p, &bt, ROOT_NODE, min_fraction))?,
                Format::Html => tree_html(&p, &bt, tr.kind, &session.name, min_fraction).into_bytes(),
                Format::Collapsed => collapsed(&p, &bt).into_bytes(),
                Format::Svg => flame_svg(&p, &bt, tr.kind, &session.name).into_bytes(),
            }
        }
        "hotspots" => {
            let mut v = req.request.clone();
            if v.get("kind").is_none() {
                v = serde_json::json!({ "kind": "cpu" });
            }
            let hr: HotSpotsRequest = serde_json::from_value(v)?;
            let r = hotspots(session, &hr)?;
            match f {
                Format::Json => serde_json::to_vec_pretty(&r)?,
                Format::Csv | Format::Html => {
                    let mut s = String::from("method,self,self_percent,self_count,total,total_percent\n");
                    for h in &r.hotspots {
                        let _ = writeln!(
                            s,
                            "{},{},{:.2},{},{},{:.2}",
                            csv_escape(&h.label),
                            h.self_,
                            100.0 * h.self_ as f64 / r.total.max(1) as f64,
                            h.self_count,
                            h.total,
                            100.0 * h.total as f64 / r.total.max(1) as f64
                        );
                    }
                    if f == Format::Html {
                        csv_to_html(&s, &session.name).into_bytes()
                    } else {
                        s.into_bytes()
                    }
                }
                _ => bail!("hotspots cannot be exported as {f:?}"),
            }
        }
        "telemetry" => {
            let tr: TelemetryRequest = serde_json::from_value(req.request.clone()).unwrap_or_default();
            let r = telemetry(
                session,
                &TelemetryRequest { points: Some(tr.points.unwrap_or(usize::MAX)), ..tr },
            )?;
            match f {
                Format::Json => serde_json::to_vec_pretty(&r)?,
                Format::Csv => {
                    let mut s = String::from("series,unit,time_ns,value\n");
                    for se in &r.series {
                        for (t, v) in se.t.iter().zip(&se.v) {
                            let _ = writeln!(s, "{},{:?},{t},{v}", se.key, se.unit);
                        }
                    }
                    s.into_bytes()
                }
                _ => bail!("telemetry cannot be exported as {f:?}"),
            }
        }
        "threads" => {
            let r = threads(session)?;
            match f {
                Format::Json => serde_json::to_vec_pretty(&r)?,
                Format::Csv => {
                    let mut s = String::from("index,java_id,name,group,virtual,daemon,cpu_ns\n");
                    for t in &r.threads {
                        let _ = writeln!(
                            s,
                            "{},{},{},{},{},{},{}",
                            t.index,
                            t.info.java_id,
                            csv_escape(&t.info.name),
                            csv_escape(t.info.group.as_deref().unwrap_or("")),
                            t.info.is_virtual,
                            t.info.is_daemon,
                            t.cpu
                        );
                    }
                    s.into_bytes()
                }
                _ => bail!("threads cannot be exported as {f:?}"),
            }
        }
        "insights" => serde_json::to_vec_pretty(&crate::insights::session_insights(session)?)?,
        v if v.starts_with("heap.") || v == "classes" || v == "biggest" => {
            crate::heap::export(session, v.trim_start_matches("heap."), f, &req.request)?
        }
        other => bail!("unknown view: {other}"),
    };
    Ok(Exported { content_type: f.content_type(), body })
}

fn label(p: &Profile, bt: &BuiltTree, id: u32) -> String {
    key_label(&p.symbols, bt.tree.aggregation, bt.tree.nodes[id as usize].key, &bt.extra_strings)
}

fn walk(bt: &BuiltTree, min_fraction: f64, mut f: impl FnMut(u32, u32)) {
    let min = (bt.tree.total_weight() as f64 * min_fraction) as u64;
    let mut stack = vec![(ROOT_NODE, 0u32)];
    while let Some((id, d)) = stack.pop() {
        let n = &bt.tree.nodes[id as usize];
        if id != ROOT_NODE && n.total_weight < min {
            continue;
        }
        f(id, d);
        stack.extend(n.children.iter().rev().map(|&c| (c, d + 1)));
    }
}

fn tree_csv(p: &Profile, bt: &BuiltTree, kind: SampleKind, min_fraction: f64) -> String {
    let total = bt.tree.total_weight().max(1) as f64;
    let mut s =
        format!("depth,label,self,total,total_percent,self_count,total_count,unit={:?}\n", kind.unit());
    walk(bt, min_fraction, |id, d| {
        let n = &bt.tree.nodes[id as usize];
        let _ = writeln!(
            s,
            "{d},{},{},{},{:.2},{},{}",
            csv_escape(&label(p, bt, id)),
            n.self_weight,
            n.total_weight,
            100.0 * n.total_weight as f64 / total,
            n.self_count,
            n.total_count
        );
    });
    s
}

fn tree_json(p: &Profile, bt: &BuiltTree, id: u32, min_fraction: f64) -> serde_json::Value {
    let n = &bt.tree.nodes[id as usize];
    let min = (bt.tree.total_weight() as f64 * min_fraction) as u64;
    serde_json::json!({
        "label": label(p, bt, id),
        "self": n.self_weight,
        "total": n.total_weight,
        "self_count": n.self_count,
        "total_count": n.total_count,
        "children": n.children.iter()
            .filter(|&&c| bt.tree.nodes[c as usize].total_weight >= min)
            .map(|&c| tree_json(p, bt, c, min_fraction)).collect::<Vec<_>>(),
    })
}

const HTML_HEAD: &str = r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<style>
:root{color-scheme:light dark;--bar:#e8590c33;--fg:CanvasText}
body{font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;margin:24px}
h1{font:600 18px system-ui;margin:0 0 4px}.sub{color:GrayText;margin-bottom:16px;font-family:system-ui}
details{margin-left:16px}summary{cursor:pointer;white-space:nowrap;padding:1px 0}
.p{display:inline-block;width:64px;text-align:right;margin-right:8px;background:linear-gradient(to right,var(--bar) var(--w),transparent var(--w))}
.v{color:GrayText;margin-left:8px}table{border-collapse:collapse}td,th{padding:2px 10px;border-bottom:1px solid #8883;text-align:left}
</style></head><body>"#;

fn tree_html(p: &Profile, bt: &BuiltTree, kind: SampleKind, name: &str, min_fraction: f64) -> String {
    let total = bt.tree.total_weight().max(1) as f64;
    let mut s = String::from(HTML_HEAD);
    let _ = write!(
        s,
        "<h1>{} {}</h1><div class=sub>Exported by OpenProfiler · total {}</div>",
        html_escape(name),
        if bt.tree.inverted { "back traces" } else { "call tree" },
        fmt_value(bt.tree.total_weight(), kind.unit())
    );
    let min = (total * min_fraction.max(0.0005)) as u64;
    fn rec(
        p: &Profile,
        bt: &BuiltTree,
        id: u32,
        total: f64,
        min: u64,
        kind: SampleKind,
        s: &mut String,
        depth: u32,
    ) {
        let n = &bt.tree.nodes[id as usize];
        let pct = 100.0 * n.total_weight as f64 / total;
        let open = if depth < 4 { " open" } else { "" };
        let _ = write!(
            s,
            "<details{open}><summary><span class=p style=\"--w:{pct:.1}%\">{pct:.1}%</span>{}<span class=v>{} · self {}</span></summary>",
            html_escape(&label(p, bt, id)),
            fmt_value(n.total_weight, kind.unit()),
            fmt_value(n.self_weight, kind.unit())
        );
        for &c in &n.children {
            if bt.tree.nodes[c as usize].total_weight >= min {
                rec(p, bt, c, total, min, kind, s, depth + 1);
            }
        }
        s.push_str("</details>");
    }
    rec(p, bt, ROOT_NODE, total, min, kind, &mut s, 0);
    s.push_str("</body></html>");
    s
}

fn csv_to_html(csv: &str, name: &str) -> String {
    let mut s = String::from(HTML_HEAD);
    let _ = write!(s, "<h1>{}</h1><table>", html_escape(name));
    for (i, line) in csv.lines().enumerate() {
        let tag = if i == 0 { "th" } else { "td" };
        s.push_str("<tr>");
        for cell in split_csv(line) {
            let _ = write!(s, "<{tag}>{}</{tag}>", html_escape(&cell));
        }
        s.push_str("</tr>");
    }
    s.push_str("</table></body></html>");
    s
}

fn split_csv(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Folded stacks (`a;b;c 123`), the input format of flamegraph.pl, speedscope and others.
pub fn collapsed(p: &Profile, bt: &BuiltTree) -> String {
    let mut s = String::new();
    let mut path: Vec<String> = vec![];
    let mut stack = vec![(ROOT_NODE, 0usize)];
    while let Some((id, d)) = stack.pop() {
        path.truncate(d);
        let n = &bt.tree.nodes[id as usize];
        if id != ROOT_NODE {
            path.push(label(p, bt, id).replace(';', ":"));
            if n.self_weight > 0 {
                let _ = writeln!(s, "{} {}", path.join(";"), n.self_weight);
            }
        }
        let next = if id == ROOT_NODE { 0 } else { d + 1 };
        stack.extend(n.children.iter().rev().map(|&c| (c, next)));
    }
    s
}

fn flame_svg(p: &Profile, bt: &BuiltTree, kind: SampleKind, name: &str) -> String {
    let fl = openprofiler_model::views::flame_layout(&bt.tree, &p.symbols, &bt.extra_strings, 0.001);
    let width = 1600.0;
    let row = 17.0;
    let height = (fl.max_depth as f64 + 1.0) * row + 40.0;
    let total = fl.total.max(1) as f64;
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" font-family="Menlo,monospace" font-size="11"><rect width="100%" height="100%" fill="#fbfaf8"/><text x="10" y="20" font-family="system-ui" font-size="14" font-weight="600">{} · {} {}</text>"##,
        html_escape(name),
        fmt_value(fl.total, kind.unit()),
        kind.name()
    );
    for i in 0..fl.node.len() {
        let x = fl.x[i] as f64 / total * width;
        let w = fl.width[i] as f64 / total * width;
        let y = height - (fl.depth[i] as f64 + 1.0) * row;
        let label = &fl.names[fl.name[i] as usize];
        // Warm palette keyed by name for stable colours.
        let h = label.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        let (r, g, b) = (205 + (h % 50), 80 + (h / 50 % 120), 40 + (h / 6000 % 50));
        let text = if w > 30.0 {
            let max_chars = ((w - 6.0) / 6.6) as usize;
            let t: String = label.chars().take(max_chars).collect();
            format!(r#"<text x="{:.1}" y="{:.1}">{}</text>"#, x + 3.0, y + 12.0, html_escape(&t))
        } else {
            String::new()
        };
        let _ = write!(
            s,
            r#"<g><title>{} ({}, {:.2}%)</title><rect x="{x:.1}" y="{y:.1}" width="{:.1}" height="{}" rx="2" fill="rgb({r},{g},{b})"/>{text}</g>"#,
            html_escape(label),
            fmt_value(fl.width[i], kind.unit()),
            100.0 * fl.width[i] as f64 / total,
            (w - 0.5).max(0.1),
            row - 1.0
        );
    }
    s.push_str("</svg>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, SessionKind};

    #[test]
    fn exports() {
        let e = Engine::new();
        let s = e.add_profile(SessionKind::Snapshot, "demo", crate::demo::demo_profile());
        for (view, fmt) in [
            ("calltree", Format::Csv),
            ("calltree", Format::Html),
            ("calltree", Format::Json),
            ("flame", Format::Svg),
            ("flame", Format::Collapsed),
            ("hotspots", Format::Csv),
            ("hotspots", Format::Html),
            ("telemetry", Format::Csv),
            ("threads", Format::Csv),
        ] {
            let out = export(
                &s,
                &ExportRequest { view: view.into(), format: fmt, request: serde_json::Value::Null },
            )
            .unwrap_or_else(|e| panic!("{view} {fmt:?}: {e}"));
            assert!(out.body.len() > 100, "{view} {fmt:?}");
        }
        let c = export(
            &s,
            &ExportRequest {
                view: "flame".into(),
                format: Format::Collapsed,
                request: serde_json::Value::Null,
            },
        )
        .unwrap();
        let text = String::from_utf8(c.body).unwrap();
        assert!(text.lines().all(|l| l.rsplit_once(' ').unwrap().1.parse::<u64>().is_ok()));
    }
}
