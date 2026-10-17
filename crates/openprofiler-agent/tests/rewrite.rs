//! The class-file rewriter on real javac output: every method of the decompiler's `--release 21`
//! corpus (`openprofiler-dex/tests/data/jvm`, with and without debug info) gets call-counting and
//! probe hooks with boxed arguments. The rewritten classes must pass the JVM's verifier and the
//! corpus harness must print exactly what it prints for the original classes, while a stub
//! `Hooks` class checks that every entry is matched by exactly one exit.

use openprofiler_agent::instrument::classfile::{ClassFile, MethodPlan, ProbeSite, rewrite};
use std::path::{Path, PathBuf};
use std::process::Command;

const STUB: &str = r#"
package dev.openprofiler.boot;

import java.util.ArrayDeque;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

public final class Hooks {
    static final AtomicLong CALLS = new AtomicLong(), PROBES = new AtomicLong(), THROWS = new AtomicLong(),
        ERRORS = new AtomicLong();
    static final Map<Thread, ArrayDeque<Integer>> STACKS = new ConcurrentHashMap<>();
    static {
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            long left = STACKS.values().stream().mapToLong(ArrayDeque::size).sum();
            System.err.println("HOOKS calls=" + CALLS + " probes=" + PROBES + " throws=" + THROWS
                + " errors=" + ERRORS + " leftover=" + left);
        }));
    }
    static ArrayDeque<Integer> stack() {
        return STACKS.computeIfAbsent(Thread.currentThread(), t -> new ArrayDeque<>());
    }
    static void pop(int key) {
        Integer top = stack().poll();
        if (top == null || top != key) {
            ERRORS.incrementAndGet();
            System.err.println("unbalanced exit " + key + " top " + top);
        }
    }
    public static void callEnter(int id) { CALLS.incrementAndGet(); stack().push(id); }
    public static void callExit(int id) { pop(id); }
    public static void trigger(int id) {}
    public static void enter(int site, Object self, Object[] args) {
        PROBES.incrementAndGet();
        // Sites divisible by 3 are entry-only: no exit follows.
        if (site % 3 != 0) stack().push(-1 - site);
        if (args == null) ERRORS.incrementAndGet();
    }
    public static void exit(Object result, int site) {
        if (site % 3 == 0) ERRORS.incrementAndGet();
        pop(-1 - site);
    }
    public static void exitThrow(Throwable t, int site) {
        if (site % 3 == 0) ERRORS.incrementAndGet();
        THROWS.incrementAndGet();
        pop(-1 - site);
    }
}
"#;

fn has(cmd: &str) -> bool {
    Command::new(cmd).arg("-version").output().is_ok_and(|o| o.status.success())
}

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../openprofiler-dex/tests/data/jvm").join(name)
}

fn class_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            class_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "class") {
            out.push(p);
        }
    }
}

#[test]
fn rewritten_corpus_verifies_and_behaves_identically() {
    if !has("javac") || !has("java") || !has("unzip") {
        eprintln!("no JDK or unzip on PATH; skipping");
        return;
    }
    // Line endings vary by platform (git autocrlf, println), but a text block always prints a bare \n.
    let expected = std::fs::read_to_string(data("expected.txt")).unwrap().replace("\r\n", "\n");
    for jar in ["modern.jar", "modern-nodebug.jar"] {
        let dir = std::env::temp_dir().join(format!("op-rewrite-{}-{jar}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let classes = dir.join("classes");
        std::fs::create_dir_all(&classes).unwrap();
        let out = Command::new("unzip").arg("-oq").arg(data(jar)).arg("-d").arg(&classes).output().unwrap();
        assert!(out.status.success(), "unzip: {}", String::from_utf8_lossy(&out.stderr));
        let mut files = vec![];
        class_files(&classes, &mut files);
        let (mut methods, mut next) = (0, 0u32);
        for f in &files {
            let bytes = std::fs::read(f).unwrap();
            let cf = ClassFile::parse(&bytes).unwrap();
            let plans: Vec<MethodPlan> = cf
                .methods
                .iter()
                .enumerate()
                .filter(|(_, m)| m.has_code())
                .map(|(i, _)| {
                    next += 1;
                    // Every third method gets an entry-only probe and nothing that needs exits.
                    let entry_only = next % 3 == 0;
                    MethodPlan {
                        method: i,
                        call_id: (!entry_only).then_some(next),
                        probe: Some(ProbeSite { site: next, args: true, entry: true, exit: !entry_only }),
                        trigger: Some(next),
                    }
                })
                .collect();
            let Some(r) = rewrite(&cf, &plans).unwrap() else { continue };
            assert!(r.skipped.is_empty(), "{}: skipped {:?}", f.display(), r.skipped);
            methods += r.applied.len();
            ClassFile::parse(&r.bytes).expect("rewritten class parses");
            std::fs::write(f, &r.bytes).unwrap();
        }
        assert!(methods > 50, "{jar}: only {methods} methods rewritten");
        let stub = dir.join("stub/dev/openprofiler/boot/Hooks.java");
        std::fs::create_dir_all(stub.parent().unwrap()).unwrap();
        std::fs::write(&stub, STUB).unwrap();
        let harness = dir.join("harness");
        let out = Command::new("javac")
            .arg("-nowarn")
            .arg("-d")
            .arg(&harness)
            .arg("-cp")
            .arg(&classes)
            .arg(&stub)
            .arg(data("harness/Check.java"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{jar}: javac: {}", String::from_utf8_lossy(&out.stderr));
        let cp = std::env::join_paths([&classes, &harness]).unwrap();
        let run = Command::new("java").arg("-Xshare:off").arg("-cp").arg(&cp).arg("Check").output().unwrap();
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n"),
            expected,
            "{jar}: behaviour differs; stderr:\n{stderr}"
        );
        let line = stderr.lines().find(|l| l.starts_with("HOOKS")).unwrap_or_else(|| panic!("{stderr}"));
        eprintln!("{jar}: {methods} methods, {line}");
        assert!(line.contains("errors=0 leftover=0"), "{jar}: {line}\n{stderr}");
        assert!(!line.contains("calls=0") && !line.contains("throws=0"), "{jar}: {line}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
