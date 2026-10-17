//! Decompiles every class of an APK/DEX/JAR:
//! `cargo run -p openprofiler-dex --example decompile -- app.apk [out-dir] [--smali] [--stats]`.
//! `ONLY=<substring>` limits it to matching class descriptors. `--stats` prints how many methods
//! fell back (failed or unstructured) and the most common reasons instead of the source.
use openprofiler_dex::{Program, View};
use std::collections::HashMap;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let start = std::time::Instant::now();
    let p = Program::open(std::path::Path::new(&args[0]))?;
    let loaded = start.elapsed();
    let view = if args.iter().any(|a| a == "--smali") { View::Smali } else { View::Java };
    let stats = args.iter().any(|a| a == "--stats");
    let out = args.get(1).filter(|a| !a.starts_with("--"));
    let only = std::env::var("ONLY").ok();
    let (mut classes, mut methods, mut failed, mut unstructured) = (0usize, 0usize, 0usize, 0usize);
    let mut reasons: HashMap<String, usize> = HashMap::new();
    for top in &p.top_levels {
        if only.as_ref().is_some_and(|o| !top.contains(o.as_str())) {
            continue;
        }
        let code = p.class_code_desc(top, view)?;
        classes += 1;
        if stats {
            let mut lines = code.code.lines().peekable();
            while let Some(l) = lines.next() {
                let t = l.trim();
                if t.ends_with('{') && t.contains('(') && !t.starts_with("if ") {
                    methods += 1;
                }
                // Reasons without addresses, so equal problems count together.
                let key =
                    |s: &str| s.split(' ').filter(|w| !w.starts_with("0x")).collect::<Vec<_>>().join(" ");
                if t.starts_with("Method decompilation failed:") {
                    failed += 1;
                    let why = lines.peek().map(|n| key(n.trim())).unwrap_or_default();
                    *reasons.entry(why).or_default() += 1;
                }
                if let Some(r) = t.strip_prefix("/* Code restructure failed: ") {
                    unstructured += 1;
                    *reasons
                        .entry(format!("restructure: {}", key(r.trim_end_matches(" */"))))
                        .or_default() += 1;
                }
            }
            continue;
        }
        match out {
            Some(dir) => {
                let rel = top.trim_start_matches('L').trim_end_matches(';');
                let path = std::path::Path::new(dir)
                    .join(format!("{rel}.{}", if view == View::Java { "java" } else { "smali" }));
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(path, &code.code)?;
            }
            None => println!("{}", code.code),
        }
    }
    if stats {
        println!(
            "{classes} classes, ~{methods} methods, {failed} failed, {unstructured} unstructured; \
             loaded in {loaded:.2?}, total {:.2?}",
            start.elapsed()
        );
        let mut r: Vec<_> = reasons.into_iter().collect();
        r.sort_by_key(|x| std::cmp::Reverse(x.1));
        for (why, n) in r.into_iter().take(25) {
            println!("{n:6}  {why}");
        }
    }
    Ok(())
}
