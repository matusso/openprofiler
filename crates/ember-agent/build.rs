//! Compiles the Java boot classes (`java/ember-boot`) and embeds them in the agent library as a
//! simple blob (`u32 name_len | name | u32 len | class bytes`, little-endian, repeated), so the
//! agent is a single self-contained file. The agent defines these classes directly in the
//! bootstrap loader. If no JDK is available at build time the blob is empty and the agent needs
//! the `bootjar=` option at runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

fn java_tool(name: &str) -> PathBuf {
    match std::env::var_os("JAVA_HOME") {
        Some(home) => Path::new(&home).join("bin").join(name),
        None => PathBuf::from(name),
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest.join("../../java/ember-boot/src");
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-env-changed=JAVA_HOME");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let blob = out.join("ember-boot.bin");
    let classes = out.join("boot-classes");
    let _ = std::fs::remove_dir_all(&classes);
    std::fs::create_dir_all(&classes).unwrap();

    let mut sources = vec![];
    if let Ok(entries) = std::fs::read_dir(src.join("dev/ember/boot")) {
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "java") {
                sources.push(e.path());
                println!("cargo:rerun-if-changed={}", e.path().display());
            }
        }
    }
    let ok = !sources.is_empty()
        && Command::new(java_tool("javac"))
            .args(["--release", "11", "-nowarn", "-d"])
            .arg(&classes)
            .args(&sources)
            .status()
            .is_ok_and(|s| s.success());
    let mut out_bytes = vec![];
    if ok {
        let mut files = vec![];
        collect(&classes, &mut files);
        files.sort();
        for f in files {
            let rel = f.strip_prefix(&classes).unwrap().with_extension("");
            let name = rel.to_string_lossy().replace('\\', "/");
            let bytes = std::fs::read(&f).unwrap();
            out_bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out_bytes.extend_from_slice(name.as_bytes());
            out_bytes.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out_bytes.extend_from_slice(&bytes);
        }
    } else {
        println!("cargo:warning=javac unavailable: embedding no boot classes (use bootjar= at runtime)");
    }
    std::fs::write(&blob, out_bytes).unwrap();
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "class") {
            out.push(p);
        }
    }
}
