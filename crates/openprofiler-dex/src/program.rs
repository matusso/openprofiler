//! A loaded program (one APK or DEX file): class index, inner-class nesting, navigation nodes,
//! cross references, decompiled-code cache, search and resources.

use crate::dex::{self, ClassDef, DexFile, Value};
use crate::jvm;
use crate::types;
use crate::zip::{self, Zip};
use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// A navigable declaration: class, method or field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeKey {
    Class(Arc<str>),
    /// Class descriptor, name, method descriptor.
    Method(Arc<str>, Arc<str>, Arc<str>),
    /// Class descriptor, name.
    Field(Arc<str>, Arc<str>),
}

impl NodeKey {
    pub fn class(&self) -> &str {
        match self {
            NodeKey::Class(c) | NodeKey::Method(c, ..) | NodeKey::Field(c, _) => c,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub id: u32,
    pub kind: &'static str,
    pub name: String,
    pub full: String,
    /// Top-level class whose source declares the node.
    pub cls: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    Java,
    Smali,
}

/// Decompiled source of a top-level class with navigation metadata. Offsets are UTF-16 code
/// units, matching JavaScript strings.
#[derive(Debug, Clone, Serialize)]
pub struct ClassCode {
    pub name: String,
    pub package: String,
    pub view: View,
    pub code: String,
    /// `[start, length, node id, 1 if declaration]`, sorted by start.
    pub links: Vec<(u32, u32, u32, u8)>,
    pub nodes: BTreeMap<String, NodeInfo>,
    /// Declaration offsets by node id.
    #[serde(skip)]
    pub decls: HashMap<u32, u32>,
}

#[derive(Debug, Clone)]
pub struct InnerInfo {
    pub outer: Arc<str>,
    /// `None` for anonymous classes.
    pub name: Option<String>,
    pub access: u32,
}

pub struct ClassEntry {
    pub dex: usize,
    pub def: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageRow {
    pub name: String,
    pub full: String,
    pub classes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClassRow {
    pub name: String,
    pub full: String,
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Packages {
    pub packages: Vec<PackageRow>,
    pub classes: Vec<ClassRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub cls: String,
    pub pos: u32,
    pub line: u32,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#in: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SearchHit {
    Node(NodeInfo),
    Text { kind: &'static str, cls: String, pos: u32, line: u32, snippet: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub results: Vec<SearchHit>,
    pub truncated: bool,
    pub scanned: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceRow {
    pub name: String,
    pub r#type: &'static str,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ResourceContent {
    pub name: String,
    pub r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    pub files: Vec<String>,
}

#[derive(Default)]
struct NodeTable {
    ids: HashMap<NodeKey, u32>,
    keys: Vec<NodeKey>,
}

/// Files of the container other than code.
struct Container {
    data: Vec<u8>,
    entries: Vec<zip::Entry>,
}

pub struct Program {
    pub name: String,
    pub dexes: Vec<DexFile>,
    pub classes: HashMap<Arc<str>, ClassEntry>,
    pub inner: HashMap<Arc<str>, InnerInfo>,
    /// Inner classes per outer class, in definition order.
    pub members: HashMap<Arc<str>, Vec<Arc<str>>>,
    /// Top-level class descriptors, sorted.
    pub top_levels: Vec<Arc<str>>,
    nodes: Mutex<NodeTable>,
    cache: Mutex<HashMap<(Arc<str>, View), Arc<ClassCode>>>,
    xrefs: OnceLock<HashMap<NodeKey, Vec<Arc<str>>>>,
    container: Option<Container>,
    res_table: OnceLock<Option<crate::res::ResTable>>,
    enums: Mutex<HashMap<Arc<str>, Option<Vec<String>>>>,
    /// Problems found while loading (unreadable dex files, ...).
    pub warnings: Vec<String>,
}

const CACHE_LIMIT: usize = 2048;

fn is_code_entry(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    // classes.dex, classes2.dex … at the root; base/dex/classes.dex in app bundles.
    base.starts_with("classes") && base.ends_with(".dex") && (!name.contains('/') || name.contains("/dex/"))
}

impl Program {
    /// Opens an `.apk` (or `.xapk`/`.apks`/`.aab`/zip with dex files), a `.dex` file, or JVM
    /// bytecode: a `.class` file or a `.jar`/`.war`/`.ear`/`.jmod`/zip with class files.
    pub fn open(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Self::from_bytes(&name, data)
    }

    pub fn from_bytes(name: &str, mut data: Vec<u8>) -> Result<Self> {
        if dex::is_dex(&data) {
            return Self::build(name, vec![DexFile::parse(name, data)?], None);
        }
        if jvm::is_class(&data) {
            let (d, warnings) = jvm::build(name, vec![(name.to_owned(), data)])?;
            let mut p = Self::build(name, vec![d], None)?;
            p.warnings = warnings;
            return Ok(p);
        }
        // A .jmod is a zip behind a 4-byte header.
        if data.starts_with(b"JM\x01\x00") && zip::is_zip(&data[4..]) {
            data.drain(..4);
        }
        if !zip::is_zip(&data) {
            bail!("{name}: not an APK, DEX, JAR or class file");
        }
        let mut warnings = vec![];
        let (dexes, container) = {
            let zip = Zip::parse(&data)?;
            let mut dexes = vec![];
            let mut code: Vec<&zip::Entry> = zip.entries.iter().filter(|e| is_code_entry(&e.name)).collect();
            code.sort_by_key(|e| dex_order(&e.name));
            for e in code {
                let bytes = zip.read(e)?;
                match DexFile::parse(&e.name, bytes) {
                    Ok(d) => dexes.push(d),
                    Err(err) => warnings.push(format!("skipped {}: {err:#}", e.name)),
                }
            }
            // Split-APK containers (.xapk/.apks): code lives in nested APKs.
            if dexes.is_empty() {
                for e in zip.entries.iter().filter(|e| e.name.ends_with(".apk")) {
                    let inner = zip.read(e)?;
                    if let Ok(z) = Zip::parse(&inner) {
                        let mut code: Vec<&zip::Entry> =
                            z.entries.iter().filter(|x| is_code_entry(&x.name)).collect();
                        code.sort_by_key(|x| dex_order(&x.name));
                        for x in code {
                            if let Ok(d) = DexFile::parse(&format!("{}!{}", e.name, x.name), z.read(x)?) {
                                dexes.push(d);
                            }
                        }
                    }
                }
            }
            // JVM archives: class files at any depth, or inside nested jars (.ear, fat jars
            // without their own classes).
            let jvm_code = dexes.is_empty()
                && zip.entries.iter().any(|e| is_class_entry(&e.name) || is_jvm_archive(&e.name));
            if jvm_code {
                let mut files = vec![];
                let mut direct: Vec<&zip::Entry> =
                    zip.entries.iter().filter(|e| is_class_entry(&e.name)).collect();
                // Multi-release jars: the base version first (the first definition wins).
                direct.sort_by_key(|e| e.name.starts_with("META-INF/versions/"));
                for e in direct {
                    match zip.read(e) {
                        Ok(b) => files.push((e.name.clone(), b)),
                        Err(err) => warnings.push(format!("skipped {}: {err:#}", e.name)),
                    }
                }
                if files.is_empty() {
                    for e in zip.entries.iter().filter(|e| is_jvm_archive(&e.name)) {
                        let Ok(inner) = zip.read(e) else { continue };
                        let Ok(z) = Zip::parse(&inner) else { continue };
                        for x in z.entries.iter().filter(|x| is_class_entry(&x.name)) {
                            if let Ok(b) = z.read(x) {
                                files.push((format!("{}!{}", e.name, x.name), b));
                            }
                        }
                    }
                }
                if !files.is_empty() {
                    let (d, w) = jvm::build(name, files)?;
                    warnings.extend(w);
                    dexes.push(d);
                }
            }
            let entries = zip
                .entries
                .into_iter()
                .filter(|e| {
                    !e.name.ends_with('/')
                        && !(if jvm_code { is_class_entry(&e.name) } else { is_code_entry(&e.name) })
                })
                .collect();
            (dexes, Container { data: vec![], entries })
        };
        if dexes.is_empty() {
            bail!("{name}: no classes*.dex or .class files inside");
        }
        let mut p = Self::build(name, dexes, Some(Container { data, ..container }))?;
        p.warnings = warnings;
        Ok(p)
    }

    fn build(name: &str, dexes: Vec<DexFile>, container: Option<Container>) -> Result<Self> {
        let mut classes: HashMap<Arc<str>, ClassEntry> = HashMap::new();
        for (di, d) in dexes.iter().enumerate() {
            for (ci, c) in d.classes.iter().enumerate() {
                let desc: Arc<str> = d.type_desc(c.class).into();
                // Multidex: the first definition wins, as on Android.
                classes.entry(desc).or_insert(ClassEntry { dex: di, def: ci });
            }
        }
        let mut inner = HashMap::new();
        for (desc, e) in &classes {
            let d = &dexes[e.dex];
            let c = &d.classes[e.def];
            let Ok(ann) = d.annotations(c) else { continue };
            let outer = dex::find_annotation(d, &ann.class, "Ldalvik/annotation/EnclosingClass;")
                .and_then(|a| dex::annotation_value(d, a, "value"))
                .and_then(|v| if let Value::Type(t) = v { Some(d.type_desc(*t).to_owned()) } else { None })
                .or_else(|| {
                    dex::find_annotation(d, &ann.class, "Ldalvik/annotation/EnclosingMethod;")
                        .and_then(|a| dex::annotation_value(d, a, "value"))
                        .and_then(|v| {
                            if let Value::Method(m) = v {
                                d.method(*m).map(|m| d.type_desc(m.class).to_owned())
                            } else {
                                None
                            }
                        })
                });
            let Some(outer) = outer else { continue };
            if outer == **desc || !classes.contains_key(outer.as_str()) {
                continue;
            }
            let ic = dex::find_annotation(d, &ann.class, "Ldalvik/annotation/InnerClass;");
            let name = ic.and_then(|a| dex::annotation_value(d, a, "name")).and_then(|v| match v {
                Value::String(s) => Some(d.string(*s).to_owned()),
                _ => None,
            });
            let access = ic
                .and_then(|a| dex::annotation_value(d, a, "accessFlags"))
                .and_then(|v| if let Value::Int(i) = v { Some(*i as u32) } else { None })
                .unwrap_or(c.access);
            inner.insert(desc.clone(), InnerInfo { outer: outer.into(), name, access });
        }
        // Break nesting cycles (obfuscators produce them).
        let keys: Vec<Arc<str>> = inner.keys().cloned().collect();
        for k in keys {
            let mut seen = HashSet::new();
            let mut cur = k.clone();
            while let Some(i) = inner.get(&cur) {
                if !seen.insert(cur.clone()) {
                    inner.remove(&k);
                    break;
                }
                cur = i.outer.clone();
            }
        }
        let mut members: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();
        for (desc, i) in &inner {
            members.entry(i.outer.clone()).or_default().push(desc.clone());
        }
        for v in members.values_mut() {
            v.sort();
        }
        let mut top_levels: Vec<Arc<str>> =
            classes.keys().filter(|d| !inner.contains_key(*d)).cloned().collect();
        top_levels.sort();
        Ok(Self {
            name: name.to_owned(),
            dexes,
            classes,
            inner,
            members,
            top_levels,
            nodes: Mutex::default(),
            cache: Mutex::default(),
            xrefs: OnceLock::new(),
            container,
            res_table: OnceLock::new(),
            enums: Mutex::default(),
            warnings: vec![],
        })
    }

    /// The class file a class was loaded from (JVM programs).
    pub fn class_file(&self, desc: &str) -> Option<&jvm::classfile::ClassFile> {
        let e = self.classes.get(desc)?;
        self.dexes[e.dex].jvm.as_ref()?.class_file(e.def)
    }

    /// Whether the program was loaded from JVM class files.
    pub fn is_jvm(&self) -> bool {
        self.dexes.iter().any(DexFile::is_jvm)
    }

    pub fn class(&self, desc: &str) -> Option<(&DexFile, &ClassDef)> {
        let e = self.classes.get(desc)?;
        let d = &self.dexes[e.dex];
        Some((d, &d.classes[e.def]))
    }

    /// The top-level class containing `desc` (itself when not nested).
    pub fn top_of(&self, desc: &str) -> Arc<str> {
        let mut cur: Arc<str> = desc.into();
        let mut depth = 0;
        while let Some(i) = self.inner.get(&cur) {
            cur = i.outer.clone();
            depth += 1;
            if depth > 64 {
                break;
            }
        }
        cur
    }

    /// `com.example.Foo` / `com.example.Foo$Bar` → descriptor.
    pub fn desc_of(&self, java: &str) -> Option<Arc<str>> {
        let d = format!("L{};", java.replace('.', "/"));
        if let Some((k, _)) = self.classes.get_key_value(d.as_str()) {
            return Some(k.clone());
        }
        // `Outer.Inner` written with dots: try turning trailing dots into `$`.
        let mut s = java.to_owned();
        while let Some(i) = s.rfind('.') {
            s.replace_range(i..i + 1, "$");
            let d = format!("L{};", s.replace('.', "/"));
            if let Some((k, _)) = self.classes.get_key_value(d.as_str()) {
                return Some(k.clone());
            }
        }
        None
    }

    pub fn node_id(&self, key: &NodeKey) -> u32 {
        let mut t = self.nodes.lock();
        if let Some(&id) = t.ids.get(key) {
            return id;
        }
        let id = t.keys.len() as u32;
        t.keys.push(key.clone());
        t.ids.insert(key.clone(), id);
        id
    }

    pub fn node_key(&self, id: u32) -> Option<NodeKey> {
        self.nodes.lock().keys.get(id as usize).cloned()
    }

    pub fn node_info(&self, id: u32) -> Option<NodeInfo> {
        let key = self.node_key(id)?;
        let defined = self.classes.contains_key(key.class());
        let cls = defined.then(|| types::java_name(&self.top_of(key.class())));
        let owner = types::java_name(key.class());
        Some(match &key {
            NodeKey::Class(c) => NodeInfo { id, kind: "class", name: self.simple_name(c), full: owner, cls },
            NodeKey::Method(_, n, _) => {
                NodeInfo { id, kind: "method", name: n.to_string(), full: format!("{owner}.{n}"), cls }
            }
            NodeKey::Field(_, n) => {
                NodeInfo { id, kind: "field", name: n.to_string(), full: format!("{owner}.{n}"), cls }
            }
        })
    }

    /// Source-level simple name of a class (`AnonymousClass1` for anonymous ones).
    pub fn simple_name(&self, desc: &str) -> String {
        match self.inner.get(desc) {
            Some(InnerInfo { name: Some(n), .. }) => n.clone(),
            Some(InnerInfo { name: None, outer, .. }) => {
                // `Outer$1` → `AnonymousClass1`; Kotlin's `Outer$method$1` → `method$1`.
                let b = types::binary_simple_name(desc);
                let o = types::binary_simple_name(outer);
                let suffix =
                    b.strip_prefix(&format!("{o}$")).unwrap_or_else(|| b.rsplit('$').next().unwrap_or(&b));
                if suffix.chars().all(|c| c.is_ascii_digit()) {
                    format!("AnonymousClass{suffix}")
                } else {
                    suffix.to_owned()
                }
            }
            None => types::binary_simple_name(desc),
        }
    }

    /// Finds the class declaring a method (walking superclasses inside the program).
    pub fn resolve_method(&self, class: &str, name: &str, desc: &str) -> Option<NodeKey> {
        let mut cur = class.to_owned();
        for _ in 0..32 {
            let (d, c) = self.class(&cur)?;
            let cd = d.class_data(c).ok()?;
            for m in cd.direct_methods.iter().chain(&cd.virtual_methods) {
                if let Some(mi) = d.method(m.method)
                    && d.string(mi.name) == name
                    && d.proto_desc(mi.proto) == desc
                {
                    return Some(NodeKey::Method(cur.as_str().into(), name.into(), desc.into()));
                }
            }
            cur = d.type_desc(c.superclass?).to_owned();
        }
        None
    }

    pub fn resolve_field(&self, class: &str, name: &str) -> Option<NodeKey> {
        let mut cur = class.to_owned();
        for _ in 0..32 {
            let (d, c) = self.class(&cur)?;
            let cd = d.class_data(c).ok()?;
            for f in cd.static_fields.iter().chain(&cd.instance_fields) {
                if d.field(f.field).is_some_and(|fi| d.string(fi.name) == name) {
                    return Some(NodeKey::Field(cur.as_str().into(), name.into()));
                }
            }
            cur = d.type_desc(c.superclass?).to_owned();
        }
        None
    }

    /// Whether instances of `desc` carry an enclosing instance (a non-static inner or anonymous
    /// class): its constructors then take the outer object first.
    pub fn has_outer_instance(&self, desc: &str) -> bool {
        let Some(info) = self.inner.get(desc) else { return false };
        let Some((d, c)) = self.class(desc) else { return false };
        if c.access & (dex::ACC_INTERFACE | dex::ACC_ENUM) != 0 {
            return false;
        }
        let outer_field = d.class_data(c).is_ok_and(|cd| {
            cd.instance_fields.iter().any(|f| {
                d.field(f.field).is_some_and(|fi| {
                    d.string(fi.name).starts_with("this$") && d.type_desc(fi.ty) == &*info.outer
                })
            })
        });
        outer_field || (info.name.is_some() && info.access & dex::ACC_STATIC == 0)
    }

    /// Enum constant names by ordinal, read from the enum's `<clinit>`
    /// (`new E("NAME", ordinal, …)` stored into the constant's field).
    pub fn enum_constants(&self, desc: &str) -> Option<Vec<String>> {
        if let Some(v) = self.enums.lock().get(desc) {
            return v.clone();
        }
        let found = self.scan_enum(desc);
        self.enums.lock().insert(desc.into(), found.clone());
        found
    }

    fn scan_enum(&self, desc: &str) -> Option<Vec<String>> {
        let (d, c) = self.class(desc)?;
        if c.access & dex::ACC_ENUM == 0 {
            return None;
        }
        let cd = d.class_data(c).ok()?;
        let clinit = cd
            .direct_methods
            .iter()
            .find(|m| d.method(m.method).is_some_and(|mi| d.string(mi.name) == "<clinit>"))?;
        let code = d.code(clinit.code_off).ok()?;
        let (insns, _) = code.decode().ok()?;
        let mut strs: HashMap<u16, String> = HashMap::new();
        let mut ints: HashMap<u16, i64> = HashMap::new();
        let mut out: BTreeMap<i64, String> = BTreeMap::new();
        for i in &insns {
            let r0 = i.regs.first().copied().unwrap_or(0);
            match i.op {
                0x1a | 0x1b => {
                    strs.insert(r0, d.string(i.index).to_owned());
                }
                0x12..=0x14 => {
                    ints.insert(r0, i.lit);
                }
                0x70 | 0x76
                    if d.method(i.index)
                        .is_some_and(|m| d.type_desc(m.class) == desc && d.string(m.name) == "<init>") =>
                {
                    if let (Some(n), Some(o)) =
                        (i.regs.get(1).and_then(|r| strs.get(r)), i.regs.get(2).and_then(|r| ints.get(r)))
                    {
                        out.insert(*o, n.clone());
                    }
                }
                _ => {}
            }
        }
        let names: Vec<String> = out.values().cloned().collect();
        (!names.is_empty() && out.keys().copied().eq(0..names.len() as i64)).then_some(names)
    }

    pub fn summary(&self) -> serde_json::Value {
        let mut pkgs = HashSet::new();
        for t in &self.top_levels {
            pkgs.insert(types::package_of(t));
        }
        serde_json::json!({
            "engine": concat!("openprofiler-dex ", env!("CARGO_PKG_VERSION")),
            // `jvm` for class files (the low-level view is JVM bytecode), `dex` otherwise.
            "format": if self.is_jvm() { "jvm" } else { "dex" },
            "classes": self.top_levels.len(),
            "packages": pkgs.len(),
            "resources": self.resource_rows().len(),
            "warnings": self.warnings,
            "dex_files": self.dexes.iter().map(|d| serde_json::json!({ "name": d.name, "version": d.version, "classes": d.classes.len() })).collect::<Vec<_>>(),
        })
    }

    pub fn class_kind(&self, desc: &str) -> &'static str {
        let Some((_, c)) = self.class(desc) else { return "class" };
        let a = c.access;
        if a & dex::ACC_ANNOTATION != 0 {
            "annotation"
        } else if a & dex::ACC_INTERFACE != 0 {
            "interface"
        } else if a & dex::ACC_ENUM != 0 {
            "enum"
        } else if a & dex::ACC_ABSTRACT != 0 {
            "abstract"
        } else {
            "class"
        }
    }

    /// Packages and top-level classes directly under `parent` (`""` = root). Single-child chains
    /// without classes are compacted (`com.example.app`).
    pub fn packages(&self, parent: &str) -> Packages {
        let prefix = if parent.is_empty() { String::new() } else { format!("{parent}.") };
        let mut subs: BTreeMap<String, usize> = BTreeMap::new();
        let mut classes = vec![];
        let mut all_pkgs: HashMap<String, usize> = HashMap::new();
        for t in &self.top_levels {
            let pkg = types::package_of(t);
            *all_pkgs.entry(pkg.clone()).or_default() += 1;
            if pkg == parent {
                classes.push(ClassRow {
                    name: types::binary_simple_name(t),
                    full: types::java_name(t),
                    kind: self.class_kind(t),
                });
            } else if (parent.is_empty() || pkg.starts_with(&prefix)) && !pkg.is_empty() {
                let rest = &pkg[prefix.len()..];
                let first = rest.split('.').next().unwrap_or(rest);
                *subs.entry(format!("{prefix}{first}")).or_default() += 1;
            }
        }
        let packages = subs
            .into_iter()
            .map(|(full, count)| {
                // Compact: descend while the package has no classes and exactly one subpackage.
                let mut cur = full.clone();
                loop {
                    if all_pkgs.contains_key(&cur) {
                        break;
                    }
                    let p = format!("{cur}.");
                    let mut children: Vec<&str> = all_pkgs
                        .keys()
                        .filter(|k| k.starts_with(&p))
                        .map(|k| k[p.len()..].split('.').next().unwrap_or(""))
                        .collect();
                    children.sort_unstable();
                    children.dedup();
                    if children.len() != 1 {
                        break;
                    }
                    cur = format!("{p}{}", children[0]);
                }
                let name = cur[prefix.len()..].to_owned();
                PackageRow { name, full: cur, classes: count }
            })
            .collect();
        classes.sort_by_key(|a| a.name.to_lowercase());
        Packages { packages, classes }
    }

    /// Decompiled (or disassembled) source of the top-level class containing `java_name`.
    pub fn class_code(&self, java_name: &str, view: View) -> Result<Arc<ClassCode>> {
        let desc = self.desc_of(java_name).ok_or_else(|| anyhow!("no such class: {java_name}"))?;
        self.class_code_desc(&self.top_of(&desc), view)
    }

    pub fn class_code_desc(&self, top: &Arc<str>, view: View) -> Result<Arc<ClassCode>> {
        if let Some(c) = self.cache.lock().get(&(top.clone(), view)) {
            return Ok(c.clone());
        }
        let raw = match view {
            View::Java => crate::java::print_class(self, top),
            View::Smali if self.is_jvm() => jvm::disasm::print_class(self, top),
            View::Smali => crate::smali::print_class(self, top),
        };
        let code = Arc::new(self.finish(top, view, raw));
        let mut c = self.cache.lock();
        if c.len() >= CACHE_LIMIT {
            c.clear();
        }
        c.insert((top.clone(), view), code.clone());
        Ok(code)
    }

    fn finish(&self, top: &str, view: View, raw: crate::java::Printed) -> ClassCode {
        // Byte offsets → UTF-16 offsets.
        let code = raw.code;
        let ascii = code.is_ascii();
        let mut map: Vec<u32> = vec![];
        if !ascii {
            map = Vec::with_capacity(code.len() + 1);
            let mut u = 0u32;
            for ch in code.chars() {
                for _ in 0..ch.len_utf8() {
                    map.push(u);
                }
                u += ch.len_utf16() as u32;
            }
            map.push(u);
        }
        let conv = |b: usize| if ascii { b as u32 } else { map.get(b).copied().unwrap_or(0) };
        let mut links = vec![];
        let mut nodes = BTreeMap::new();
        let mut decls = HashMap::new();
        for (start, len, key, decl) in raw.links {
            let id = self.node_id(&key);
            let (s, e) = (conv(start), conv(start + len));
            links.push((s, e - s, id, decl as u8));
            if decl {
                decls.entry(id).or_insert(s);
            }
            nodes.entry(id.to_string()).or_insert_with(|| self.node_info(id));
        }
        links.sort_unstable();
        let nodes = nodes.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect();
        ClassCode {
            name: types::java_name(top),
            package: types::package_of(top),
            view,
            code,
            links,
            nodes,
            decls,
        }
    }

    /// The class to show for a node and the offset of its declaration.
    pub fn locate(&self, id: u32) -> Result<(Arc<ClassCode>, Option<u32>)> {
        let key = self.node_key(id).ok_or_else(|| anyhow!("no such node: {id}"))?;
        if !self.classes.contains_key(key.class()) {
            bail!("{} is not defined in this program", types::java_name(key.class()));
        }
        let code = self.class_code_desc(&self.top_of(key.class()), View::Java)?;
        let pos = code.decls.get(&id).copied();
        Ok((code, pos))
    }

    fn xrefs(&self) -> &HashMap<NodeKey, Vec<Arc<str>>> {
        self.xrefs.get_or_init(|| {
            let per_class: Vec<(Arc<str>, Vec<NodeKey>)> = self
                .top_levels
                .par_iter()
                .map(|top| {
                    let mut refs = HashSet::new();
                    let mut stack = vec![top.clone()];
                    while let Some(c) = stack.pop() {
                        crate::xref::class_refs(self, &c, &mut refs);
                        stack.extend(self.members.get(&c).into_iter().flatten().cloned());
                    }
                    (top.clone(), refs.into_iter().collect())
                })
                .collect();
            let mut map: HashMap<NodeKey, Vec<Arc<str>>> = HashMap::new();
            for (top, refs) in per_class {
                for r in refs {
                    map.entry(r).or_default().push(top.clone());
                }
            }
            for v in map.values_mut() {
                v.sort();
            }
            map
        })
    }

    pub fn usages(&self, id: u32, limit: usize) -> Result<(NodeInfo, Vec<Usage>, bool)> {
        let key = self.node_key(id).ok_or_else(|| anyhow!("no such node: {id}"))?;
        let info = self.node_info(id).ok_or_else(|| anyhow!("no such node: {id}"))?;
        let users = self.xrefs().get(&key).cloned().unwrap_or_default();
        let mut out = vec![];
        let mut truncated = false;
        'outer: for top in users {
            let Ok(code) = self.class_code_desc(&top, View::Java) else { continue };
            let starts = line_starts(&code.code);
            for &(s, _, n, d) in &code.links {
                if n != id || d == 1 {
                    continue;
                }
                if out.len() >= limit {
                    truncated = true;
                    break 'outer;
                }
                out.push(usage_at(&code, &starts, s));
            }
        }
        Ok((info, out, truncated))
    }

    pub fn search(&self, query: &str, in_code: bool, limit: usize, budget: Duration) -> SearchResult {
        let q = query.to_lowercase();
        let total = self.top_levels.len();
        if in_code {
            let deadline = Instant::now() + budget;
            let stop = std::sync::atomic::AtomicBool::new(false);
            let scanned = std::sync::atomic::AtomicUsize::new(0);
            let found = Mutex::new(vec![]);
            self.top_levels.par_iter().for_each(|top| {
                if stop.load(std::sync::atomic::Ordering::Relaxed) || Instant::now() > deadline {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
                let Ok(code) = self.class_code_desc(top, View::Java) else { return };
                scanned.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // ASCII case folding on UTF-16 units keeps offsets exact.
                let fold = |u: u16| if (b'A' as u16..=b'Z' as u16).contains(&u) { u + 32 } else { u };
                let hay: Vec<u16> = code.code.encode_utf16().map(fold).collect();
                let needle: Vec<u16> = q.encode_utf16().map(fold).collect();
                if needle.is_empty() || needle.len() > hay.len() {
                    return;
                }
                let starts = line_starts(&code.code);
                let mut i = 0;
                while i + needle.len() <= hay.len() {
                    if hay[i..i + needle.len()] == needle[..] {
                        let mut f = found.lock();
                        if f.len() >= limit {
                            stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        f.push((code.name.clone(), i as u32, usage_at(&code, &starts, i as u32)));
                        i += needle.len();
                    } else {
                        i += 1;
                    }
                }
            });
            let mut found = found.into_inner();
            found.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
            return SearchResult {
                results: found
                    .into_iter()
                    .map(|(cls, pos, u)| SearchHit::Text {
                        kind: "code",
                        cls,
                        pos,
                        line: u.line,
                        snippet: u.snippet,
                    })
                    .collect(),
                truncated: stop.into_inner(),
                scanned: scanned.into_inner(),
                total,
            };
        }
        let mut results = vec![];
        let mut truncated = false;
        let push = |key: NodeKey, results: &mut Vec<SearchHit>| -> bool {
            if results.len() >= limit {
                return false;
            }
            let id = self.node_id(&key);
            if let Some(i) = self.node_info(id) {
                results.push(SearchHit::Node(i));
            }
            true
        };
        let mut all: Vec<&Arc<str>> = self.classes.keys().collect();
        all.sort();
        'outer: for desc in all {
            let (d, c) = self.class(desc).expect("indexed class");
            if types::java_name(desc).to_lowercase().contains(&q)
                && !push(NodeKey::Class(desc.clone()), &mut results)
            {
                truncated = true;
                break;
            }
            let Ok(cd) = d.class_data(c) else { continue };
            for m in cd.direct_methods.iter().chain(&cd.virtual_methods) {
                let Some(mi) = d.method(m.method) else { continue };
                let name = d.string(mi.name);
                if name.to_lowercase().contains(&q)
                    && !push(
                        NodeKey::Method(desc.clone(), name.into(), d.proto_desc(mi.proto).into()),
                        &mut results,
                    )
                {
                    truncated = true;
                    break 'outer;
                }
            }
            for f in cd.static_fields.iter().chain(&cd.instance_fields) {
                let Some(fi) = d.field(f.field) else { continue };
                let name = d.string(fi.name);
                if name.to_lowercase().contains(&q)
                    && !push(NodeKey::Field(desc.clone(), name.into()), &mut results)
                {
                    truncated = true;
                    break 'outer;
                }
            }
        }
        SearchResult { results, truncated, scanned: total, total }
    }

    // ---------------------------------------------------------------- resources

    fn res_table(&self) -> Option<&crate::res::ResTable> {
        self.res_table
            .get_or_init(|| {
                let c = self.container.as_ref()?;
                let zip = Zip::parse(&c.data).ok()?;
                let e = zip.find("resources.arsc")?;
                crate::res::ResTable::parse(&zip.read(e).ok()?).ok()
            })
            .as_ref()
    }

    pub fn resource_rows(&self) -> Vec<ResourceRow> {
        let Some(c) = &self.container else { return vec![] };
        let mut rows: Vec<ResourceRow> = c
            .entries
            .iter()
            .map(|e| ResourceRow { name: e.name.clone(), r#type: resource_type(&e.name), size: e.size })
            .collect();
        rows.sort_by(|a, b| {
            let rank = |n: &str| {
                if n == "AndroidManifest.xml" {
                    0
                } else if n == "resources.arsc" {
                    1
                } else {
                    2
                }
            };
            (rank(&a.name), &a.name).cmp(&(rank(&b.name), &b.name))
        });
        rows
    }

    /// `name` or `resources.arsc!res/values/strings.xml`.
    pub fn resource(&self, name: &str) -> Result<ResourceContent> {
        let c = self.container.as_ref().ok_or_else(|| anyhow!("no resources in a plain dex file"))?;
        let (base, sub) = match name.split_once('!') {
            Some((b, s)) => (b, Some(s)),
            None => (name, None),
        };
        let zip = Zip::parse(&c.data)?;
        let entry = zip.find(base).ok_or_else(|| anyhow!("no such resource: {base}"))?;
        let ty = resource_type(base);
        let mut out = ResourceContent { name: name.to_owned(), r#type: ty, ..Default::default() };
        if base == "resources.arsc" {
            let table = self.res_table().ok_or_else(|| anyhow!("resources.arsc could not be decoded"))?;
            let files = table.value_files();
            match sub {
                Some(s) => {
                    let (_, text) = files
                        .iter()
                        .find(|(p, _)| p == s)
                        .ok_or_else(|| anyhow!("no such file in {base}: {s}"))?;
                    out.text = Some(text.clone());
                }
                None => {
                    out.text = Some(table.public_xml());
                    out.files = files.into_iter().map(|(p, _)| p).collect();
                }
            }
            return Ok(out);
        }
        if entry.size > 64 << 20 {
            out.binary = true;
            return Ok(out);
        }
        let bytes = zip.read(entry)?;
        if ty == "img" {
            if bytes.len() <= 4 << 20 {
                out.image = Some(base64(&bytes));
            }
            return Ok(out);
        }
        if crate::res::is_binary_xml(&bytes) {
            out.text = Some(crate::res::decode_xml(&bytes, self.res_table())?);
        } else if let Ok(s) = std::str::from_utf8(&bytes)
            && !s.contains('\0')
        {
            out.text = Some(s.to_owned());
        } else {
            out.binary = true;
        }
        Ok(out)
    }
}

fn is_class_entry(name: &str) -> bool {
    name.ends_with(".class") && !name.ends_with("module-info.class")
}

fn is_jvm_archive(name: &str) -> bool {
    [".jar", ".war"].iter().any(|x| name.ends_with(x))
}

fn dex_order(name: &str) -> (usize, u32) {
    let base = name.rsplit('/').next().unwrap_or(name);
    let n = base.trim_start_matches("classes").trim_end_matches(".dex").parse().unwrap_or(1);
    (name.matches('/').count(), n)
}

fn resource_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if name == "AndroidManifest.xml" {
        return "manifest";
    }
    match ext.as_str() {
        "arsc" => "arsc",
        "xml" => "xml",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => "img",
        "so" => "lib",
        "json" => "json",
        "txt" | "properties" | "pro" | "cfg" | "version" | "md" => "text",
        "html" | "htm" | "js" | "css" => "html",
        "ttf" | "otf" => "font",
        "mp3" | "ogg" | "wav" => "sound",
        "mp4" | "webm" => "video",
        "jar" | "war" | "zip" | "apk" | "dex" | "jmod" => "archive",
        "yml" | "yaml" | "mf" | "sql" | "conf" => "text",
        "kotlin_builtins" | "kotlin_module" => "kotlin",
        _ if name.starts_with("META-INF/") => "meta",
        _ => "other",
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Line start offsets in UTF-16 units.
fn line_starts(code: &str) -> Vec<u32> {
    let mut starts = vec![0];
    let mut u = 0u32;
    for ch in code.chars() {
        u += ch.len_utf16() as u32;
        if ch == '\n' {
            starts.push(u);
        }
    }
    starts
}

fn usage_at(code: &ClassCode, starts: &[u32], pos: u32) -> Usage {
    let line = starts.partition_point(|&s| s <= pos).max(1);
    let utf16: Vec<u16> = code.code.encode_utf16().collect();
    let s = starts[line - 1] as usize;
    let e = starts.get(line).map_or(utf16.len(), |&e| e as usize - 1);
    let mut snippet =
        String::from_utf16_lossy(&utf16[s.min(utf16.len())..e.min(utf16.len())]).trim().to_owned();
    if snippet.chars().count() > 240 {
        snippet = snippet.chars().take(240).collect::<String>() + "…";
    }
    // Enclosing member: the last declaration link before the position.
    let enclosing = code
        .links
        .iter()
        .filter(|l| l.3 == 1 && l.0 <= pos)
        .filter_map(|l| code.nodes.get(&l.2.to_string()))
        .rfind(|n| n.kind == "method")
        .map(|n| n.name.clone());
    Usage { cls: code.name.clone(), pos, line: line as u32, snippet, r#in: enclosing }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn code_entries() {
        assert!(is_code_entry("classes.dex"));
        assert!(is_code_entry("classes12.dex"));
        assert!(is_code_entry("base/dex/classes2.dex"));
        assert!(!is_code_entry("assets/classes.dex"));
        assert!(dex_order("classes2.dex") < dex_order("classes10.dex"));
    }
}
