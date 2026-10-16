//! Fixtures come from `tests/data/res/regen.sh` (aapt2 from the Android SDK).

use super::config::qualifiers;
use super::value::{ResValue, aapt_escape};
use super::*;

const MANIFEST: &[u8] = include_bytes!("../../tests/data/res/AndroidManifest.xml");
const LAYOUT: &[u8] = include_bytes!("../../tests/data/res/main.xml");
const ARSC: &[u8] = include_bytes!("../../tests/data/res/resources.arsc");
const ARSC_COMPACT: &[u8] = include_bytes!("../../tests/data/res/resources-compact.arsc");
const ARSC_COLLAPSED: &[u8] = include_bytes!("../../tests/data/res/resources-collapsed.arsc");

fn table() -> ResTable {
    ResTable::parse(ARSC).expect("fixture table parses")
}

fn file<'a>(files: &'a [(String, String)], path: &str) -> &'a str {
    files.iter().find(|(p, _)| p == path).map(|(_, x)| x.as_str()).unwrap_or_else(|| {
        panic!("missing {path}; have {:?}", files.iter().map(|(p, _)| p).collect::<Vec<_>>())
    })
}

#[test]
fn manifest_decodes() {
    let t = table();
    assert!(is_binary_xml(MANIFEST));
    assert!(!is_binary_xml(b"<?xml version=\"1.0\"?>"));
    let xml = decode_xml(MANIFEST, Some(&t)).unwrap();
    for needle in [
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<manifest\n",
        "    xmlns:android=\"http://schemas.android.com/apk/res/android\"\n",
        "package=\"dev.openprofiler.demo\"",
        "android:versionCode=\"7\"",
        "android:versionName=\"1.2.3\"",
        "    <uses-permission android:name=\"android.permission.INTERNET\" />\n",
        "android:theme=\"@style/AppTheme\"",
        "android:label=\"@string/app_name\"",
        "android:allowBackup=\"false\"",
        "android:value=\"secret &amp; &lt;value&gt;\"",
        "android:value=\"42\"",
        "android:name=\".MainActivity\"",
        "android:exported=\"true\"",
        "            <intent-filter>\n                <action android:name=\"android.intent.action.MAIN\" />\n",
        "<category android:name=\"android.intent.category.LAUNCHER\" />",
        "        </activity>\n    </application>\n</manifest>\n",
    ] {
        assert!(xml.contains(needle), "{needle:?} not in\n{xml}");
    }
    // Without a table references stay numeric.
    let raw = decode_xml(MANIFEST, None).unwrap();
    assert!(raw.contains("android:label=\"@0x7f0b0000\""), "{raw}");
}

#[test]
fn layout_decodes() {
    let xml = decode_xml(LAYOUT, Some(&table())).unwrap();
    for needle in [
        "xmlns:app=\"http://schemas.android.com/apk/res-auto\"",
        "android:layout_width=\"match_parent\"",
        "android:layout_height=\"wrap_content\"",
        "android:orientation=\"vertical\"",
        "android:gravity=\"end|center_vertical\"",
        "android:padding=\"@dimen/margin\"",
        "android:background=\"@drawable/bg\"",
        "app:mode=\"slow\"",
        "app:customAttr=\"4dp\"",
        "android:id=\"@id/title\"",
        "android:text=\"@string/app_name\"",
        "android:textSize=\"18sp\"",
        "android:textStyle=\"bold|italic\"",
        "android:textColor=\"#ff112233\"",
        "android:alpha=\"0.5\"",
        "android:layout_width=\"100dp\"",
        "android:text=\"Literal &lt;text&gt; &amp; more\" />",
        "    <TextView\n",
        "</LinearLayout>\n",
    ] {
        assert!(xml.contains(needle), "{needle:?} not in\n{xml}");
    }
}

#[test]
fn value_files_match_sources() {
    let t = table();
    let files = t.value_files();
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted);
    assert!(!paths.iter().any(|p| p.contains("layout") || p.contains("drawable")), "{paths:?}");

    let strings = file(&files, "res/values/strings.xml");
    assert!(strings.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<resources>\n"));
    for needle in [
        "    <string name=\"app_name\">Demo</string>\n",
        "<string name=\"greeting\">Hello &amp; welcome</string>",
        r#"<string name="quote">It\'s \"quoted\"\nnext line</string>"#,
        r#"<string name="at_sign">\@not_a_ref</string>"#,
    ] {
        assert!(strings.contains(needle), "{needle:?} not in\n{strings}");
    }
    assert!(strings.ends_with("</resources>\n"));
    assert!(file(&files, "res/values-de/strings.xml").contains(">Demo DE</string>"));
    assert!(file(&files, "res/values-pt-rBR/strings.xml").contains(">Demonstração</string>"));
    assert!(
        file(&files, "res/values-night/colors.xml").contains("<color name=\"primary\">#ffbb86fc</color>")
    );
    let colors = file(&files, "res/values/colors.xml");
    assert!(colors.contains("<color name=\"accent\">#03dac5</color>"), "{colors}");
    let dimens = file(&files, "res/values/dimens.xml");
    assert!(dimens.contains("<dimen name=\"margin\">16dp</dimen>"), "{dimens}");
    assert!(dimens.contains("<dimen name=\"text_size\">14sp</dimen>"));
    assert!(dimens.contains("<dimen name=\"frac\">1.5px</dimen>"));
    assert!(file(&files, "res/values-sw600dp-v31/dimens.xml").contains(">32dp<"));
    assert!(file(&files, "res/values-land-xxhdpi/dimens.xml").contains(">24dp<"));
    assert!(file(&files, "res/values/integers.xml").contains("<integer name=\"max_items\">10</integer>"));
    assert!(file(&files, "res/values/bools.xml").contains("<bool name=\"enabled\">true</bool>"));
    assert!(file(&files, "res/values/ids.xml").contains("<item type=\"id\" name=\"extra_id\" />"));

    let arrays = file(&files, "res/values/arrays.xml");
    assert!(
        arrays.contains(
            "    <string-array name=\"planets\">\n        <item>Mercury</item>\n        <item>Venus</item>\n    </string-array>\n"
        ),
        "{arrays}"
    );
    assert!(arrays.contains("<integer-array name=\"numbers\">"));
    let plurals = file(&files, "res/values/plurals.xml");
    assert!(plurals.contains("<item quantity=\"one\">%d apple</item>"), "{plurals}");
    assert!(plurals.contains("<item quantity=\"other\">%d apples</item>"));
    let styles = file(&files, "res/values/styles.xml");
    for needle in [
        "<style name=\"AppTheme\" parent=\"@android:style/Theme.Material.Light\">",
        "<item name=\"android:colorPrimary\">@color/primary</item>",
        "<item name=\"android:windowNoTitle\">true</item>",
        "<item name=\"customAttr\">8dp</item>",
        "</style>",
    ] {
        assert!(styles.contains(needle), "{needle:?} not in\n{styles}");
    }
    let attrs = file(&files, "res/values/attrs.xml");
    assert!(attrs.contains("<attr name=\"customAttr\" format=\"dimension\" />"), "{attrs}");
    assert!(attrs.contains("<attr name=\"mode\">\n        <enum name=\"fast\" value=\"0\" />"), "{attrs}");
}

#[test]
fn public_xml_and_names() {
    let t = table();
    let public = t.public_xml();
    assert!(
        public.contains("    <public type=\"string\" name=\"app_name\" id=\"0x7f0b0000\" />\n"),
        "{public}"
    );
    assert!(public.contains("<public type=\"layout\" name=\"main\" id=\"0x7f090000\" />"));
    assert!(public.contains("<public type=\"array\" name=\"numbers\" id=\"0x7f010000\" />"));
    let first = public.find("0x7f010000").unwrap();
    let last = public.find("0x7f0c0000").unwrap();
    assert!(first < last);
    assert_eq!(t.name_of(0x7f0b0000).as_deref(), Some("@string/app_name"));
    assert_eq!(t.name_of(0x01010003).as_deref(), Some("@android:attr/name"));
    assert_eq!(t.name_of(0x01030237).as_deref(), Some("@android:style/Theme.Material.Light"));
    assert_eq!(t.name_of(0x0106000b).as_deref(), Some("@android:color/white"));
    assert_eq!(t.name_of(0x7f0bffff), None);
    assert_eq!(t.name_of(0x12345678), None);
}

#[test]
fn sparse_table_matches_dense() {
    let compact = ResTable::parse(ARSC_COMPACT).unwrap();
    assert_eq!(compact.value_files(), table().value_files());
    assert_eq!(compact.public_xml(), table().public_xml());
}

#[test]
fn collapsed_names_stay_distinct() {
    let t = ResTable::parse(ARSC_COLLAPSED).unwrap();
    assert_eq!(t.name_of(0x7f0b0000).as_deref(), Some("@string/res_0x7f0b0000"));
    let files = t.value_files();
    let strings = file(&files, "res/values/strings.xml");
    assert!(strings.contains("<string name=\"res_0x7f0b0000\">Demo</string>"), "{strings}");
    let xml = decode_xml(LAYOUT, Some(&t)).unwrap();
    assert!(xml.contains("android:text=\"@string/res_0x7f0b0000\""), "{xml}");
}

/// UTF-8 string pool chunk.
fn pool(strings: &[&str]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    for s in strings {
        offsets.push(data.len() as u32);
        data.extend([s.chars().count() as u8, s.len() as u8]);
        data.extend(s.as_bytes());
        data.push(0);
    }
    while data.len() % 4 != 0 {
        data.push(0);
    }
    let header = 28u32;
    let start = header + 4 * strings.len() as u32;
    let mut c = Vec::new();
    c.extend(1u16.to_le_bytes());
    c.extend((header as u16).to_le_bytes());
    c.extend((start + data.len() as u32).to_le_bytes());
    c.extend((strings.len() as u32).to_le_bytes());
    c.extend(0u32.to_le_bytes());
    c.extend(0x100u32.to_le_bytes());
    c.extend(start.to_le_bytes());
    c.extend(0u32.to_le_bytes());
    for o in offsets {
        c.extend(o.to_le_bytes());
    }
    c.extend(data);
    c
}

fn chunk(typ: u16, header: &[u8], body: &[u8]) -> Vec<u8> {
    let mut c = Vec::new();
    c.extend(typ.to_le_bytes());
    c.extend((8 + header.len() as u16).to_le_bytes());
    c.extend((8 + header.len() as u32 + body.len() as u32).to_le_bytes());
    c.extend(header);
    c.extend(body);
    c
}

#[test]
fn offset16_and_compact_entries() {
    // One string type with entries 0 (absent) and 1 (compact, key "b" = "hello").
    let mut config = vec![0u8; 64];
    config[..4].copy_from_slice(&64u32.to_le_bytes());
    config[8..10].copy_from_slice(b"fr");
    let offsets_len = 4u32; // two u16 offsets
    let mut type_header = vec![1u8, 0x02, 0, 0];
    type_header.extend(2u32.to_le_bytes());
    type_header.extend((8 + 12 + 64 + offsets_len).to_le_bytes());
    type_header.extend(&config);
    let mut body = Vec::new();
    body.extend(0xffffu16.to_le_bytes());
    body.extend(0u16.to_le_bytes());
    body.extend(1u16.to_le_bytes()); // compact: key index
    body.extend((0x0008u16 | (0x03 << 8)).to_le_bytes()); // FLAG_COMPACT, TYPE_STRING
    body.extend(0u32.to_le_bytes()); // global string 0
    let type_chunk = chunk(0x0201, &type_header, &body);

    let types = pool(&["string"]);
    let keys = pool(&["a", "b"]);
    let mut pkg_header = Vec::new();
    pkg_header.extend(0x7fu32.to_le_bytes());
    let mut name = [0u8; 256];
    for (i, u) in "x.y".encode_utf16().enumerate() {
        name[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
    pkg_header.extend(name);
    let header_len = 8 + 4 + 256 + 16;
    pkg_header.extend((header_len as u32).to_le_bytes());
    pkg_header.extend(0u32.to_le_bytes());
    pkg_header.extend((header_len as u32 + types.len() as u32).to_le_bytes());
    pkg_header.extend(0u32.to_le_bytes());
    let pkg = chunk(0x0200, &pkg_header, &[types, keys, type_chunk].concat());
    let arsc = chunk(0x0002, &1u32.to_le_bytes(), &[pool(&["hello"]), pkg].concat());

    let t = ResTable::parse(&arsc).unwrap();
    assert_eq!(t.name_of(0x7f010001).as_deref(), Some("@string/b"));
    assert_eq!(t.name_of(0x7f010000), None);
    let files = t.value_files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "res/values-fr/strings.xml");
    assert!(files[0].1.contains("<string name=\"b\">hello</string>"), "{}", files[0].1);
}

#[test]
fn config_qualifiers() {
    let mut c = vec![0u8; 64];
    c[..4].copy_from_slice(&64u32.to_le_bytes());
    assert_eq!(qualifiers(&c), "");
    c[8..10].copy_from_slice(b"pt");
    c[10..12].copy_from_slice(b"BR");
    c[12] = 2; // land
    c[14..16].copy_from_slice(&480u16.to_le_bytes());
    c[24..26].copy_from_slice(&21u16.to_le_bytes());
    c[28] = 0x80; // ldrtl
    c[29] = 0x20; // night
    c[30..32].copy_from_slice(&600u16.to_le_bytes());
    c[32..34].copy_from_slice(&720u16.to_le_bytes());
    assert_eq!(qualifiers(&c), "-pt-rBR-ldrtl-sw600dp-w720dp-land-night-xxhdpi-v21");
    c[36..40].copy_from_slice(b"Latn");
    assert!(qualifiers(&c).starts_with("-b+pt+Latn+BR-ldrtl"));
    // Packed three-letter language ("fil") and nodpi; truncated configs read as unset.
    let mut c = vec![0u8; 16];
    c[8..10].copy_from_slice(&[0xad, 0x05]);
    c[14..16].copy_from_slice(&0xffffu16.to_le_bytes());
    assert_eq!(qualifiers(&c), "-fil-nodpi");
    assert_eq!(qualifiers(&[]), "");
}

#[test]
fn value_formats() {
    let pool = Default::default();
    let fmt = |typ, data| ResValue { typ, data }.format(&pool, None);
    assert_eq!(fmt(0x05, (16 << 8) | 1), "16dp");
    assert_eq!(fmt(0x05, (14 << 8) | 2), "14sp");
    assert_eq!(fmt(0x05, (-3i32 << 8) as u32), "-3px");
    assert_eq!(fmt(0x06, (0x40_0000 << 8) | (3 << 4)), "50%");
    assert_eq!(fmt(0x06, (0x40_0000 << 8) | (3 << 4) | 1), "50%p");
    assert_eq!(fmt(0x04, 1.25f32.to_bits()), "1.25");
    assert_eq!(fmt(0x10, -2i32 as u32), "-2");
    assert_eq!(fmt(0x11, 0xff), "0x000000ff");
    assert_eq!(fmt(0x12, 0xffff_ffff), "true");
    assert_eq!(fmt(0x1c, 0x80ff0000), "#80ff0000");
    assert_eq!(fmt(0x1d, 0xff00ff00), "#00ff00");
    assert_eq!(fmt(0x1f, 0xffaabbcc), "#abc");
    assert_eq!(fmt(0x01, 0x7f010000), "@0x7f010000");
    assert_eq!(fmt(0x02, 0x7f010000), "?0x7f010000");
    assert_eq!(fmt(0x01, 0), "@null");
    assert_eq!(fmt(0x00, 1), "");
    assert_eq!(aapt_escape("?a & <b>"), "\\?a &amp; &lt;b&gt;");
    assert_eq!(aapt_escape(" padded "), "\" padded \"");
    assert_eq!(aapt_escape("a\\b\tc"), "a\\\\b\\tc");
}

/// xorshift64*, enough to generate junk deterministically.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn exercise(data: &[u8]) {
    if let Ok(t) = ResTable::parse(data) {
        let _ = t.value_files();
        let _ = t.public_xml();
        let _ = decode_xml(LAYOUT, Some(&t));
    }
    let _ = decode_xml(data, None);
    let _ = is_binary_xml(data);
}

#[test]
fn prefixes_never_panic() {
    for fixture in [MANIFEST, LAYOUT, ARSC, ARSC_COMPACT, ARSC_COLLAPSED] {
        for len in 0..=fixture.len() {
            exercise(&fixture[..len]);
        }
    }
}

#[test]
fn random_and_mutated_input_never_panics() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..500 {
        let len = (rng.next() % 2048) as usize;
        let mut junk: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        exercise(&junk);
        // Same junk behind valid chunk headers so parsing gets past the first check.
        if junk.len() >= 8 {
            junk[..2].copy_from_slice(&[3, 0]);
            exercise(&junk);
            junk[..2].copy_from_slice(&[2, 0]);
            exercise(&junk);
        }
    }
    for fixture in [MANIFEST, LAYOUT, ARSC, ARSC_COMPACT] {
        for _ in 0..400 {
            let mut data = fixture.to_vec();
            for _ in 0..1 + rng.next() % 8 {
                let i = (rng.next() as usize) % data.len();
                data[i] = match rng.next() % 3 {
                    0 => 0xff,
                    1 => 0,
                    _ => rng.next() as u8,
                };
            }
            exercise(&data);
        }
    }
}
