//! End-to-end tests against a recording produced by the local JDK.
//!
//! A small multi-threaded workload (`tests/workload/Workload.java`) is run once with JFR enabled;
//! every test shares that recording. Tests are skipped (pass trivially) when `java` is missing.

use ember_jfr::{JfrReader, parse_bytes, parse_file};
use ember_model::views::{TreeOptions, hot_spots, key_label};
use ember_model::{Profile, SampleKind};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

struct Recording {
    _dir: tempfile::TempDir,
    path: PathBuf,
    profile: Profile,
}

fn java_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("JAVA_HOME") {
        return Some(PathBuf::from(h));
    }
    if let Ok(out) = Command::new("java").args(["-XshowSettings:properties", "-version"]).output() {
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        if let Some(h) = text.lines().find_map(|l| l.trim().strip_prefix("java.home = ").map(PathBuf::from)) {
            return Some(h);
        }
    }
    // macOS: /usr/bin/java is a stub; ask the launcher helper.
    let out = Command::new("/usr/libexec/java_home").output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn recording() -> Option<&'static Recording> {
    static REC: OnceLock<Option<Recording>> = OnceLock::new();
    REC.get_or_init(|| {
        let java = java_home().map(|h| h.join("bin/java")).filter(|p| p.exists())?;
        let dir = tempfile::tempdir().ok()?;
        let path = dir.path().join("workload.jfr");
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/workload/Workload.java");
        let status = Command::new(java)
            .arg(format!(
                "-XX:StartFlightRecording=filename={},settings=profile,jdk.JavaMonitorEnter#threshold=0ms",
                path.display()
            ))
            .arg(src)
            .arg("3")
            .current_dir(dir.path())
            .output()
            .expect("run java");
        assert!(status.status.success(), "workload failed: {}", String::from_utf8_lossy(&status.stderr));
        let profile = parse_file(&path).expect("parse recording");
        Some(Recording { _dir: dir, path, profile })
    })
    .as_ref()
}

macro_rules! rec_or_skip {
    () => {
        match recording() {
            Some(r) => r,
            None => {
                eprintln!("java not found; skipping");
                return;
            }
        }
    };
}

#[test]
fn cpu_hot_method_is_found() {
    let r = rec_or_skip!();
    let p = &r.profile;
    let cpu = p.samples.get(&SampleKind::Cpu).expect("cpu samples");
    assert!(cpu.len() > 10, "too few cpu samples: {}", cpu.len());
    assert!(p.meta.cpu_interval.is_some());
    let opts = TreeOptions::default();
    let (hs, extra) = hot_spots(p, SampleKind::Cpu, &opts);
    let top: Vec<String> =
        hs.iter().take(3).map(|h| key_label(&p.symbols, opts.aggregation, h.key, &extra)).collect();
    assert!(top.iter().any(|l| l == "Workload.hotLoop"), "hot spots: {top:?}");
}

#[test]
fn allocations_monitors_and_waits() {
    let r = rec_or_skip!();
    let p = &r.profile;
    let alloc = p.samples.get(&SampleKind::Alloc).expect("alloc samples");
    assert!(!alloc.is_empty());
    let classes: Vec<&str> =
        alloc.class.iter().filter(|&&c| c != u32::MAX).map(|&c| p.symbols.str(c)).collect();
    assert!(classes.contains(&"byte[]"), "allocated classes lack byte[]");
    assert!(p.samples.get(&SampleKind::Wait).is_some_and(|s| !s.is_empty()), "no park/wait samples");
    assert!(!p.monitors.is_empty());
    assert!(!p.thread_states.is_empty());
}

#[test]
fn telemetry_and_gc() {
    let r = rec_or_skip!();
    let p = &r.profile;
    for key in ["heap.used", "heap.committed", "classes.loaded", "threads.live", "gc.pause", "alloc.rate"] {
        let s = p.telemetry(key).unwrap_or_else(|| panic!("missing telemetry {key}"));
        assert!(!s.time.is_empty(), "{key} empty");
        assert!(s.time.windows(2).all(|w| w[0] <= w[1]), "{key} not sorted");
    }
    assert!(!p.gc.is_empty(), "System.gc() should produce GC events");
    assert!(p.gc.iter().any(|g| g.heap_before.is_some() && g.heap_after.is_some()));
    assert!(
        p.gc.iter().any(|g| g.cause.contains("System.gc")),
        "causes: {:?}",
        p.gc.iter().map(|g| &g.cause).collect::<Vec<_>>()
    );
}

#[test]
fn jvm_info_and_threads() {
    let r = rec_or_skip!();
    let p = &r.profile;
    let j = &p.meta.jvm;
    assert!(j.pid.is_some());
    assert!(!j.vm_name.is_empty());
    assert!(!j.java_version.is_empty());
    assert!(p.meta.start > 0 && p.meta.end > p.meta.start);
    assert!(
        p.threads.iter().any(|t| t.name == "ember-worker"),
        "threads: {:?}",
        p.threads.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    // Every sample refers to a valid thread and stack.
    for s in p.samples.values() {
        assert!(s.thread.iter().all(|&t| (t as usize) < p.threads.len()));
        assert!(s.stack.iter().all(|&st| (st as usize) < p.stacks.len()));
    }
}

#[test]
fn event_counts_match_jfr_summary() {
    let r = rec_or_skip!();
    let Some(jfr) = java_home().map(|h| h.join("bin/jfr")).filter(|p| p.exists()) else {
        eprintln!("jfr tool not found; skipping");
        return;
    };
    let out = Command::new(jfr).arg("summary").arg(&r.path).output().expect("jfr summary");
    let text = String::from_utf8_lossy(&out.stdout);
    let expected: BTreeMap<String, usize> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?;
            let count: usize = it.next()?.parse().ok()?;
            // Metadata and constant-pool records are not events.
            (count > 0 && name.starts_with("jdk.") && name != "jdk.Metadata" && name != "jdk.Checkpoint")
                .then(|| (name.to_owned(), count))
        })
        .collect();
    assert!(expected.len() > 20, "unexpected jfr summary output:\n{text}");

    let reader = JfrReader::open(&r.path).unwrap();
    let mut actual: BTreeMap<String, usize> = BTreeMap::new();
    for e in reader.events().unwrap() {
        *actual.entry(e.unwrap().type_name).or_default() += 1;
    }
    assert_eq!(actual, expected);

    let p = &r.profile;
    let n = |k| p.samples.get(&k).map_or(0, |s| s.len());
    let e = |k: &str| expected.get(k).copied().unwrap_or(0);
    if e("jdk.CPUTimeSample") == 0 {
        assert_eq!(n(SampleKind::Cpu), e("jdk.ExecutionSample"));
    }
    assert_eq!(n(SampleKind::Alloc), e("jdk.ObjectAllocationSample"));
    assert_eq!(n(SampleKind::Monitor), e("jdk.JavaMonitorEnter"));
    assert_eq!(n(SampleKind::Wait), e("jdk.ThreadPark") + e("jdk.JavaMonitorWait"));
    assert_eq!(p.gc.len(), e("jdk.GarbageCollection"));
}

#[test]
fn generic_events_are_resolved() {
    let r = rec_or_skip!();
    let reader = JfrReader::open(&r.path).unwrap();
    let chunk = reader.chunks().unwrap().next().unwrap().unwrap();
    let types = chunk.event_types();
    let exec = types.iter().find(|t| t.name == "jdk.ExecutionSample").expect("ExecutionSample type");
    assert!(exec.fields.iter().any(|f| f.name == "stackTrace"));
    let sample = chunk
        .events()
        .map(Result::unwrap)
        .find(|e| e.type_name == "jdk.ExecutionSample" && e.stack.is_some())
        .expect("an execution sample");
    let stack = sample.stack.unwrap();
    assert!(!stack.frames.is_empty());
    assert!(stack.frames.iter().all(|f| !f.method.is_empty()));
    assert!(sample.thread.is_some());
    assert!(sample.start >= chunk.header.start_nanos);
    let gc = chunk.events().map(Result::unwrap).find(|e| e.type_name == "jdk.GarbageCollection").unwrap();
    assert!(matches!(gc.field("sumOfPauses"), Some(ember_jfr::Value::Duration(d)) if *d > 0));
    assert!(matches!(gc.field("name"), Some(ember_jfr::Value::String(_))));
}

#[test]
fn bytes_match_file_and_truncation_is_safe() {
    let r = rec_or_skip!();
    let data = std::fs::read(&r.path).unwrap();
    let p = parse_bytes(&data).unwrap();
    assert_eq!(p.samples.len(), r.profile.samples.len());
    assert_eq!(p.stacks.len(), r.profile.stacks.len());
    // Truncated files must not panic: either an error or a partial profile with a warning.
    for frac in [0.1, 0.5, 0.9, 0.999] {
        let cut = &data[..(data.len() as f64 * frac) as usize];
        if let Ok(p) = parse_bytes(cut) {
            assert!(p.meta.properties.contains_key("jfr.warnings"));
        }
    }
    assert!(parse_bytes(b"not a jfr file at all, definitely not").is_err());
}
