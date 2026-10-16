//! Decompiles every class of an APK/DEX: `cargo run -p openprofiler-dex --example decompile -- app.apk [out-dir] [--smali]`.
//! `ONLY=<substring>` limits it to matching class descriptors.
use openprofiler_dex::{Program, View};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let p = Program::open(std::path::Path::new(&args[0]))?;
    let view = if args.iter().any(|a| a == "--smali") { View::Smali } else { View::Java };
    let out = args.get(1).filter(|a| !a.starts_with("--"));
    let only = std::env::var("ONLY").ok();
    for top in &p.top_levels {
        if only.as_ref().is_some_and(|o| !top.contains(o.as_str())) {
            continue;
        }
        let code = p.class_code_desc(top, view)?;
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
    Ok(())
}
