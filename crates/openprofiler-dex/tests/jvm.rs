//! End-to-end checks on JVM class files (`tests/data/jvm`, built by `regen.sh` with javac
//! --release 21, with and without debug info): every class decompiles without falling back,
//! modern constructs read like source, and — when a JDK is available — the output recompiles
//! and behaves exactly like the original classes.

use openprofiler_dex::{Program, View};
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURES: [&str; 2] = ["modern.jar", "modern-nodebug.jar"];

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/jvm").join(name)
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
        assert_eq!(p.top_levels.len(), 1, "{f}: {:?}", p.top_levels);
        for (name, code) in decompile_all(&p) {
            for bad in ["restructure failed", "decompilation failed", "invoke-custom", "lambda$"] {
                assert!(!code.contains(bad), "{f} {name}: `{bad}` in:\n{code}");
            }
        }
        // The register form the decompiler works on is browsable as well.
        let smali = p.class_code("modern.Features", View::Smali).unwrap();
        assert!(smali.code.contains(".method public day(I)Ljava/lang/String;"), "{}", smali.code);
    }
}

#[test]
fn readable_constructs() {
    let p = Program::open(&data("modern.jar")).unwrap();
    let all: String = decompile_all(&p).into_iter().map(|(_, c)| c).collect();
    for want in [
        "public record Point(int x, int y) {",
        "ADD {",
        "IntBinaryOperator op = (a, b) -> a * b + offset + this.counter;",
        "Supplier<List<String>> make = ArrayList::new;",
        "BiFunction<String, Integer, String> rep = String::repeat;",
        "Runnable inner = () -> this.log.add(msg + k);",
        "Collectors.groupingBy(String::length, TreeMap::new, Collectors.counting())",
        ".filter(x -> x % 2 == 0)",
        "Function<Integer, Integer> twice = x -> op.applyAsInt(x, 2);",
        "try (Res r1 = new Res(out, a); Res r2 = new Res(out, b)) {",
        "} catch (IOException e) {",
        "} finally {\n            out.add(\"finally\");",
        "try (Res r = res) {",
        "case \"Aa\":",
        "case \"BB\":",
        "boolean equal = a.intValue() == b.intValue();",
        "copy.remove(Integer.valueOf(7));",
        "this.log = new ArrayList<>();",
        "return \"c=\" + c + \" l=\" + l + \" d=\" + d + \" o=\" + String.valueOf(o) + \" len=\" + arr.length + \"!\";",
        "if (s instanceof Circle) {",
        "r |= ~v;",
        "for (String s : xs) {",
        "String s = a > 0 ? \"pos\" : a < 0 ? \"neg\" : \"zero\";",
        "int u = boxed != null ? boxed : -1;",
        "Integer x = flag ? boxed : Integer.valueOf(n);",
        "out.add(r == null ? \"none\" : \"some\");",
        "m.computeIfAbsent(words.get(i), k -> new ArrayList<>()).add(i);",
        "return in.stream().filter(",
        "return words.stream().collect(Collectors.groupingBy(",
        "return twice.apply(base) + l.size() + rep.apply(\"ab\", 2).length();",
    ] {
        assert!(all.contains(want), "missing `{want}` in:\n{all}");
    }
    for unwanted in [
        "extends Record",
        "addSuppressed",
        "hashCode()) {",
        "Integer.valueOf(base)",
        "(String) words.get",
        "(List) ",
        "(Map) ",
    ] {
        assert!(!all.contains(unwanted), "unexpected `{unwanted}` in:\n{all}");
    }
    // Without debug info, local generics come from where values flow.
    let p = Program::open(&data("modern-nodebug.jar")).unwrap();
    let all: String = decompile_all(&p).into_iter().map(|(_, c)| c).collect();
    assert!(all.contains("ArrayList<String> arrayList = new ArrayList<>();"), "{all}");
    assert!(all.contains("treeMap.computeIfAbsent(list.get(i), "), "{all}");
}

#[test]
fn single_class_files_and_navigation() {
    let jar = std::fs::read(data("modern.jar")).unwrap();
    let p = Program::from_bytes("modern.jar", jar).unwrap();
    // Usages of a method referenced only through a method reference.
    let hits = p.search("Res", false, 20, std::time::Duration::from_secs(5));
    let res = hits
        .results
        .iter()
        .find_map(|r| match r {
            openprofiler_dex::program::SearchHit::Node(n) if n.kind == "class" && n.name == "Res" => {
                Some(n.id)
            }
            _ => None,
        })
        .expect("Res class");
    let (_, usages, _) = p.usages(res, 50).unwrap();
    assert!(usages.iter().any(|u| u.snippet.contains("new Res(out, a)")), "{usages:?}");
    // A bare class file opens on its own; the jar's manifest is a resource.
    let dir = std::env::temp_dir().join(format!("op-jvm-class-{}", std::process::id()));
    let out = Command::new("unzip").arg("-oq").arg(data("modern.jar")).arg("-d").arg(&dir).output();
    if out.is_ok_and(|o| o.status.success()) {
        let bytes = std::fs::read(dir.join("modern/Features$Point.class")).unwrap();
        let single = Program::from_bytes("Features$Point.class", bytes).unwrap();
        let code = single.class_code("modern.Features$Point", View::Java).unwrap();
        assert!(code.code.contains("record Features$Point(int x, int y)"), "{}", code.code);
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert!(p.resource_rows().iter().any(|r| r.name == "META-INF/MANIFEST.MF"));
}

fn has(cmd: &str) -> bool {
    Command::new(cmd).arg("-version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn recompiles_and_behaves_identically() {
    if !has("javac") || !has("java") {
        eprintln!("no JDK on PATH; skipping");
        return;
    }
    let expected = std::fs::read_to_string(data("expected.txt")).unwrap();
    for f in FIXTURES {
        let dir = std::env::temp_dir().join(format!("op-jvm-corpus-{}-{f}", std::process::id()));
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
        assert_eq!(
            String::from_utf8_lossy(&run.stdout),
            expected,
            "{f}: behaviour differs; stderr:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
