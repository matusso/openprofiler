//! Rebuilds `res/values*/*.xml` and `public.xml` from a parsed [`ResTable`].

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::arsc::{EntryValue, ResTable};
use super::attrs::{android_attr_name, android_attr_symbol, symbolize};
use super::value::{
    ResValue, TYPE_INT_BOOLEAN, TYPE_INT_DEC, TYPE_INT_HEX, TYPE_STRING, aapt_escape, xml_escape,
};

const MAX_OUTPUT: usize = 256 << 20;
const HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<resources>\n";

// Special `ResTable_map` names (attr metadata and plural quantities).
const ATTR_TYPE: u32 = 0x0100_0000;
const ATTR_MIN: u32 = 0x0100_0001;
const ATTR_MAX: u32 = 0x0100_0002;
const ATTR_L10N: u32 = 0x0100_0003;
const QUANTITIES: [(u32, &str); 6] = [
    (0x0100_0004, "other"),
    (0x0100_0005, "zero"),
    (0x0100_0006, "one"),
    (0x0100_0007, "two"),
    (0x0100_0008, "few"),
    (0x0100_0009, "many"),
];
const ATTR_FORMATS: [(u32, &str); 8] = [
    (0x01, "reference"),
    (0x02, "string"),
    (0x04, "integer"),
    (0x08, "boolean"),
    (0x10, "color"),
    (0x20, "float"),
    (0x40, "dimension"),
    (0x80, "fraction"),
];
const ATTR_ENUM: u32 = 0x1_0000;
const ATTR_FLAGS: u32 = 0x2_0000;

impl ResTable {
    /// Decoded value files like apktool's `res/values*/` output, sorted by path.
    pub fn value_files(&self) -> Vec<(String, String)> {
        let Some(pkg) = self.main_package() else {
            return Vec::new();
        };
        // path -> id -> element text (first definition wins for duplicated ids)
        let mut files: BTreeMap<String, BTreeMap<u32, String>> = BTreeMap::new();
        // Hostile tables can point many entries at one huge string; stop at a sane total.
        let mut total = 0usize;
        for entry in &pkg.entries {
            let typ = pkg.type_name(entry.id);
            let name = xml_escape(&pkg.key_name(entry.id));
            let Some(text) = self.format_entry(&typ, &name, &entry.value) else {
                continue;
            };
            let file = match typ.as_str() {
                "plurals" => "plurals".to_string(),
                t if t.ends_with('s') => t.to_string(),
                t => format!("{t}s"),
            };
            total += text.len();
            if total > MAX_OUTPUT {
                break;
            }
            let path = format!("res/values{}/{file}.xml", entry.qualifiers);
            files.entry(path).or_default().entry(entry.id).or_insert(text);
        }
        files
            .into_iter()
            .map(|(path, items)| {
                let mut xml = String::from(HEADER);
                for text in items.values() {
                    xml.push_str(text);
                }
                xml.push_str("</resources>\n");
                (path, xml)
            })
            .collect()
    }

    /// `public.xml`-like listing of the main package's resources, sorted by id.
    pub fn public_xml(&self) -> String {
        let mut xml = String::from(HEADER);
        if let Some(pkg) = self.main_package() {
            for &id in pkg.names.keys() {
                let _ = writeln!(
                    xml,
                    "    <public type=\"{}\" name=\"{}\" id=\"0x{id:08x}\" />",
                    xml_escape(&pkg.type_name(id)),
                    xml_escape(&pkg.key_name(id)),
                );
            }
        }
        xml.push_str("</resources>\n");
        xml
    }

    fn format_entry(&self, typ: &str, name: &str, value: &EntryValue) -> Option<String> {
        match value {
            EntryValue::Simple(v) => self.format_simple(typ, name, *v),
            EntryValue::Complex { parent, items } => Some(self.format_complex(typ, name, *parent, items)),
        }
    }

    fn format_simple(&self, typ: &str, name: &str, v: ResValue) -> Option<String> {
        if typ == "id" {
            return Some(format!("    <item type=\"id\" name=\"{name}\" />\n"));
        }
        if typ != "string" && v.is_string() && self.strings.get(v.data)?.starts_with("res/") {
            return None; // file-based resource (layout, drawable xml, png...)
        }
        let text = self.value_text(v);
        Some(match typ {
            "string" | "color" | "dimen" | "integer" | "bool" | "fraction" | "drawable" => {
                format!("    <{typ} name=\"{name}\">{text}</{typ}>\n")
            }
            _ => format!("    <item type=\"{typ}\" name=\"{name}\">{text}</item>\n"),
        })
    }

    fn format_complex(&self, typ: &str, name: &str, parent: u32, items: &[(u32, ResValue)]) -> String {
        let mut out = String::new();
        match typ {
            "array" => {
                let tag = if !items.is_empty() && items.iter().all(|(_, v)| v.is_string()) {
                    "string-array"
                } else if !items.is_empty() && items.iter().all(|(_, v)| v.typ == TYPE_INT_DEC) {
                    "integer-array"
                } else {
                    "array"
                };
                let _ = writeln!(out, "    <{tag} name=\"{name}\">");
                for (_, v) in items {
                    let _ = writeln!(out, "        <item>{}</item>", self.value_text(*v));
                }
                let _ = writeln!(out, "    </{tag}>");
            }
            "plurals" => {
                let _ = writeln!(out, "    <plurals name=\"{name}\">");
                for (key, v) in items {
                    let q = QUANTITIES.iter().find(|(k, _)| k == key).map_or("other", |(_, q)| q);
                    let _ = writeln!(out, "        <item quantity=\"{q}\">{}</item>", self.value_text(*v));
                }
                let _ = writeln!(out, "    </plurals>");
            }
            "attr" => self.format_attr(&mut out, name, items),
            _ => {
                let is_style = typ == "style";
                let parent = if parent == 0 {
                    String::new()
                } else {
                    format!(" parent=\"{}\"", xml_escape(&self.reference(parent)))
                };
                let open = if is_style {
                    format!("    <style name=\"{name}\"{parent}")
                } else {
                    format!("    <item type=\"{typ}\" name=\"{name}\"{parent}")
                };
                let close = if is_style { "style" } else { "item" };
                if items.is_empty() {
                    let _ = writeln!(out, "{open} />");
                } else {
                    let _ = writeln!(out, "{open}>");
                    for (key, v) in items {
                        let _ = writeln!(
                            out,
                            "        <item name=\"{}\">{}</item>",
                            xml_escape(&self.attr_name(*key)),
                            self.attr_symbol(*key, *v).unwrap_or_else(|| self.value_text(*v))
                        );
                    }
                    let _ = writeln!(out, "    </{close}>");
                }
            }
        }
        out
    }

    fn format_attr(&self, out: &mut String, name: &str, items: &[(u32, ResValue)]) {
        let format_bits = items.iter().find(|(k, _)| *k == ATTR_TYPE).map_or(0, |(_, v)| v.data);
        let mut header = format!("    <attr name=\"{name}\"");
        let formats: Vec<&str> =
            ATTR_FORMATS.iter().filter(|(bit, _)| format_bits & bit != 0).map(|(_, n)| *n).collect();
        if !formats.is_empty() && format_bits != 0xffff {
            let _ = write!(header, " format=\"{}\"", formats.join("|"));
        }
        for (key, attr) in [(ATTR_MIN, "min"), (ATTR_MAX, "max")] {
            if let Some((_, v)) = items.iter().find(|(k, _)| *k == key) {
                let _ = write!(header, " {attr}=\"{}\"", v.data as i32);
            }
        }
        let child = if format_bits & ATTR_FLAGS != 0 {
            "flag"
        } else if format_bits & ATTR_ENUM != 0 {
            "enum"
        } else {
            ""
        };
        let symbols: Vec<_> = items
            .iter()
            .filter(|(k, _)| !matches!(*k, ATTR_TYPE | ATTR_MIN | ATTR_MAX | ATTR_L10N))
            .collect();
        if symbols.is_empty() || child.is_empty() {
            let _ = writeln!(out, "{header} />");
            return;
        }
        let _ = writeln!(out, "{header}>");
        for (key, v) in symbols {
            let sym = self.name_of(*key).map_or_else(
                || format!("0x{key:08x}"),
                |n| n.rsplit('/').next().unwrap_or_default().to_string(),
            );
            let value =
                if child == "flag" { format!("0x{:08x}", v.data) } else { (v.data as i32).to_string() };
            let _ = writeln!(out, "        <{child} name=\"{}\" value=\"{value}\" />", xml_escape(&sym));
        }
        let _ = writeln!(out, "    </attr>");
    }

    /// Symbolic form (`fast`, `bold|italic`) of an integer value of an enum/flag attribute.
    pub(crate) fn attr_symbol(&self, attr: u32, value: ResValue) -> Option<String> {
        if !matches!(value.typ, TYPE_INT_DEC | TYPE_INT_HEX) {
            return None;
        }
        if attr >> 24 == 0x01 {
            return android_attr_symbol(android_attr_name(attr)?, value.data);
        }
        let pkg = self.packages.iter().find(|p| p.id == attr >> 24)?;
        let items = pkg.entries.iter().find_map(|e| match &e.value {
            EntryValue::Complex { items, .. } if e.id == attr => Some(items),
            _ => None,
        })?;
        let format_bits = items.iter().find(|(k, _)| *k == ATTR_TYPE)?.1.data;
        if format_bits & (ATTR_ENUM | ATTR_FLAGS) == 0 {
            return None;
        }
        let syms: Vec<(u32, String)> = items
            .iter()
            .filter(|(k, _)| !matches!(*k, ATTR_TYPE | ATTR_MIN | ATTR_MAX | ATTR_L10N))
            .filter_map(|(k, v)| Some((v.data, self.name_of(*k)?.rsplit('/').next()?.to_string())))
            .collect();
        let flags = format_bits & ATTR_FLAGS != 0;
        symbolize(value.data, flags, syms.iter().map(|(v, n)| (*v, n.as_str())))
    }

    /// Element text for a value: aapt-escaped strings, otherwise the escaped typed form.
    fn value_text(&self, v: ResValue) -> String {
        match v.typ {
            TYPE_STRING => aapt_escape(&self.strings.get_or_empty(v.data)),
            TYPE_INT_BOOLEAN | TYPE_INT_DEC => v.format(&self.strings, Some(self)),
            _ => xml_escape(&v.format(&self.strings, Some(self))),
        }
    }

    fn reference(&self, id: u32) -> String {
        self.name_of(id).unwrap_or_else(|| format!("@0x{id:08x}"))
    }

    /// Style item name: `colorPrimary` in the main package, `android:windowNoTitle` elsewhere.
    fn attr_name(&self, id: u32) -> String {
        if let Some(pkg) = self.main_package().filter(|p| p.names.contains_key(&id)) {
            return pkg.key_name(id);
        }
        if let Some(n) = android_attr_name(id) {
            return format!("android:{n}");
        }
        match self.name_of(id) {
            Some(n) => n.trim_start_matches('@').replacen("attr/", "", 1),
            None => format!("0x{id:08x}"),
        }
    }
}
