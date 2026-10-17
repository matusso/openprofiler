//! End-to-end checks on the corpus (`tests/data/corpus`, built by `regen.sh` with javac + d8, and
//! as plain jars): every class decompiles without falling back, and — when a JDK is available —
//! the output recompiles with javac and behaves exactly like the original classes.

use openprofiler_dex::{Program, View};
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURES: [&str; 4] = ["corpus.dex", "corpus-release.dex", "corpus.jar", "corpus-nodebug.jar"];

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/corpus").join(name)
}

fn decompile_all(p: &Program) -> Vec<(String, String)> {
    p.top_levels
        .iter()
        .map(|t| {
            let code = p.class_code_desc(t, View::Java).expect("decompile");
            (t.trim_start_matches('L').trim_end_matches(';').to_owned(), code.code.clone())
        })
        .collect()
}

#[test]
fn decompiles_without_fallbacks() {
    for f in FIXTURES {
        let p = Program::open(&data(f)).unwrap();
        assert!(p.top_levels.len() >= 4, "{f}: {:?}", p.top_levels);
        for (name, code) in decompile_all(&p) {
            assert!(!code.contains("restructure failed"), "{f} {name}:\n{code}");
            assert!(!code.contains("decompilation failed"), "{f} {name}:\n{code}");
        }
        let smali = p.class_code("corpus.Control", View::Smali).unwrap();
        assert!(smali.code.contains(".method public loops(I)I"), "{}", smali.code);
    }
}

#[test]
fn readable_constructs() {
    let p = Program::open(&data("corpus.dex")).unwrap();
    let all: String = decompile_all(&p).into_iter().map(|(_, c)| c).collect();
    for want in [
        "public static final int LIMIT = 42;",
        "TABLE = new long[]{1L, 2L, 3L};",
        "this.enabled = !this.enabled;",
        "c %= 7;",
        "return \"object \" + String.valueOf(o) + \" #\" + this.total;",
        "for (int v : values) {",
        "for (int i = 0; i < n; i++) {",
        "loop1: for (int r = 0; r < grid.length; r++) {",
        "break loop1;",
        "do {",
        "} while (s > 100);",
        "return a != null && b != null && a.equals(b);",
        "case CIRCLE:",
        "synchronized (obj) {",
        "} catch (NumberFormatException e) {",
        "public static <T extends Comparable<T>> T maxOf(List<T> items) {",
        "for (T t : items) {",
        "CIRCLE(\"c\"),",
        "return Shapes.this.registered;",
        "out.add(new AnonymousClass1());",
        "case \"alpha\":",
    ] {
        assert!(all.contains(want), "missing `{want}` in:\n{all}");
    }
}

#[test]
fn links_and_usages() {
    let p = Program::open(&data("corpus.dex")).unwrap();
    let code = p.class_code("corpus.Control", View::Java).unwrap();
    // The declaration of `classify` and a reference to String.isEmpty (not defined here: no link).
    let classify = code
        .links
        .iter()
        .find(|l| l.3 == 1 && code.nodes[&l.2.to_string()].name == "classify")
        .expect("decl");
    let s = code.code.encode_utf16().skip(classify.0 as usize).take(classify.1 as usize).collect::<Vec<_>>();
    assert_eq!(String::from_utf16_lossy(&s), "classify");
    let (located, pos) = p.locate(classify.2).unwrap();
    assert_eq!(located.name, "corpus.Control");
    assert_eq!(pos, Some(classify.0));
    // Basics.add is used by nobody in the corpus; Shapes.Circle is used by Shapes.make.
    let circle = p.search("circle", false, 10, std::time::Duration::from_secs(5));
    let node = circle
        .results
        .iter()
        .find_map(|r| match r {
            openprofiler_dex::program::SearchHit::Node(n) if n.kind == "class" => Some(n.id),
            _ => None,
        })
        .expect("Circle class");
    let (_, usages, _) = p.usages(node, 50).unwrap();
    assert!(usages.iter().any(|u| u.snippet.contains("new Circle(")), "{usages:?}");
    let text = p.search("basics", true, 10, std::time::Duration::from_secs(5));
    assert!(!text.results.is_empty());
}

fn has(cmd: &str) -> bool {
    Command::new(cmd).arg("-version").output().is_ok_and(|o| o.status.success())
}

/// The JVM's helpful NullPointerException messages name variables; names differ without debug info.
fn normalize(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines() {
        let mut l = line.to_owned();
        while let Some(i) = l.find("because \"") {
            let rest = &l[i + 9..];
            let Some(j) = rest.find('"') else { break };
            l = format!("{}because <var>{}", &l[..i], &rest[j + 1..]);
        }
        out.push_str(&l);
        out.push('\n');
    }
    out
}

#[test]
fn recompiles_and_behaves_identically() {
    if !has("javac") || !has("java") {
        eprintln!("no JDK on PATH; skipping");
        return;
    }
    let expected = normalize(&std::fs::read_to_string(data("expected.txt")).unwrap());
    for f in FIXTURES {
        let dir = std::env::temp_dir().join(format!("op-dex-corpus-{}-{f}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        let p = Program::open(&data(f)).unwrap();
        let mut files = vec![];
        for (name, code) in decompile_all(&p) {
            let path = src.join(format!("{name}.java"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, code).unwrap();
            files.push(path);
        }
        let classes = dir.join("classes");
        let out = Command::new("javac").arg("-nowarn").arg("-d").arg(&classes).args(&files).output().unwrap();
        assert!(out.status.success(), "{f}: javac failed:\n{}", String::from_utf8_lossy(&out.stderr));
        let harness = dir.join("harness");
        let out = Command::new("javac")
            .arg("-d")
            .arg(&harness)
            .arg("-cp")
            .arg(&classes)
            .arg(data("harness/Check.java"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{f}: harness: {}", String::from_utf8_lossy(&out.stderr));
        let cp = std::env::join_paths([&classes, &harness]).unwrap();
        let run = Command::new("java").arg("-cp").arg(cp).arg("Check").output().unwrap();
        let actual = normalize(&String::from_utf8_lossy(&run.stdout));
        assert_eq!(
            actual,
            expected,
            "{f}: behaviour differs; stderr:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
