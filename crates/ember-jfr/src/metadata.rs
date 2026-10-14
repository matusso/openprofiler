//! The metadata event: a string table followed by an element tree that describes every type
//! (events, constant-pool types, inline structs and primitives) and its fields.

use crate::reader::Reader;
use anyhow::{Result, bail};
use rustc_hash::FxHashMap;

/// How a type's values are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Bool,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    String,
    /// A struct: its fields in declaration order.
    Complex,
}

impl Prim {
    fn from_name(name: &str) -> Self {
        match name {
            "boolean" => Prim::Bool,
            "byte" => Prim::Byte,
            "char" => Prim::Char,
            "short" => Prim::Short,
            "int" => Prim::Int,
            "long" => Prim::Long,
            "float" => Prim::Float,
            "double" => Prim::Double,
            "java.lang.String" => Prim::String,
            _ => Prim::Complex,
        }
    }
}

/// Units declared by `@Timespan` / `@Timestamp` annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Ticks,
    Nanos,
    Micros,
    Millis,
    Seconds,
    EpochMillis,
}

impl TimeUnit {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "TICKS" => TimeUnit::Ticks,
            "NANOSECONDS" => TimeUnit::Nanos,
            "MICROSECONDS" => TimeUnit::Micros,
            "MILLISECONDS" => TimeUnit::Millis,
            "SECONDS" => TimeUnit::Seconds,
            "MILLISECONDS_SINCE_EPOCH" => TimeUnit::EpochMillis,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FieldDesc {
    pub name: String,
    pub label: Option<String>,
    /// Index into [`Metadata::types`].
    pub type_idx: u32,
    /// The value is a key into the constant pool of `type_idx`.
    pub constant_pool: bool,
    pub array: bool,
    pub timespan: Option<TimeUnit>,
    pub timestamp: Option<TimeUnit>,
}

#[derive(Debug, Clone)]
pub struct TypeDesc {
    pub id: u64,
    pub name: String,
    pub label: Option<String>,
    pub super_type: Option<String>,
    pub prim: Prim,
    pub fields: Vec<FieldDesc>,
}

impl TypeDesc {
    pub fn is_event(&self) -> bool {
        self.super_type.as_deref() == Some("jdk.jfr.Event")
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub types: Vec<TypeDesc>,
    pub by_id: FxHashMap<u64, u32>,
    pub by_name: FxHashMap<String, u32>,
}

struct Element {
    name: u32,
    attrs: Vec<(u32, u32)>,
    children: Vec<Element>,
}

fn read_element(r: &mut Reader<'_>, depth: u32) -> Result<Element> {
    if depth > 64 {
        bail!("metadata element tree too deep");
    }
    let name = r.int()? as u32;
    let n_attrs = r.len()?;
    let mut attrs = Vec::with_capacity(n_attrs);
    for _ in 0..n_attrs {
        attrs.push((r.int()? as u32, r.int()? as u32));
    }
    let n_children = r.len()?;
    let mut children = Vec::with_capacity(n_children);
    for _ in 0..n_children {
        children.push(read_element(r, depth + 1)?);
    }
    Ok(Element { name, attrs, children })
}

impl Metadata {
    /// Parses the metadata event whose size field starts at `r.pos`.
    pub fn parse(r: &mut Reader<'_>) -> Result<Metadata> {
        let _size = r.int()?;
        let type_id = r.long()?;
        if type_id != 0 {
            bail!("expected metadata event (type 0), found type {type_id}");
        }
        let _start = r.long()?;
        let _duration = r.long()?;
        let _metadata_id = r.long()?;
        let n = r.len()?;
        let mut strings = Vec::with_capacity(n);
        for _ in 0..n {
            strings.push(r.inline_string()?);
        }
        let root = read_element(r, 0)?;
        let s = |i: u32| strings.get(i as usize).map(String::as_str).unwrap_or("");
        let attr = |e: &Element, key: &str| -> Option<String> {
            e.attrs.iter().find(|(k, _)| s(*k) == key).map(|(_, v)| s(*v).to_owned())
        };

        // Collect class elements wherever they appear (normally root/metadata/class).
        let mut classes: Vec<&Element> = vec![];
        let mut stack = vec![&root];
        while let Some(e) = stack.pop() {
            for c in &e.children {
                if s(c.name) == "class" {
                    classes.push(c);
                } else {
                    stack.push(c);
                }
            }
        }

        let mut md = Metadata::default();
        for c in &classes {
            let id: u64 = attr(c, "id").and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
            let name = attr(c, "name").unwrap_or_default();
            let idx = md.types.len() as u32;
            md.by_id.insert(id, idx);
            md.by_name.insert(name.clone(), idx);
            md.types.push(TypeDesc {
                id,
                prim: Prim::from_name(&name),
                name,
                label: None,
                super_type: attr(c, "superType"),
                fields: vec![],
            });
        }
        // Annotation values: (annotation type id, value) → resolved by name below.
        let annotation = |e: &Element, md: &Metadata| -> Vec<(String, String)> {
            e.children
                .iter()
                .filter(|a| s(a.name) == "annotation")
                .filter_map(|a| {
                    let cid: u64 = attr(a, "class")?.parse().ok()?;
                    let tname = md.types.get(*md.by_id.get(&cid)? as usize)?.name.clone();
                    Some((tname, attr(a, "value").unwrap_or_default()))
                })
                .collect()
        };
        for (i, c) in classes.iter().enumerate() {
            let label = annotation(c, &md).into_iter().find(|(n, _)| n == "jdk.jfr.Label").map(|(_, v)| v);
            let mut fields = vec![];
            for f in c.children.iter().filter(|f| s(f.name) == "field") {
                let tid: u64 = attr(f, "class").and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
                let Some(&type_idx) = md.by_id.get(&tid) else {
                    bail!("field {:?} references unknown type {tid}", attr(f, "name"));
                };
                let anns = annotation(f, &md);
                let find = |n: &str| anns.iter().find(|(a, _)| a == n).map(|(_, v)| v.as_str());
                fields.push(FieldDesc {
                    name: attr(f, "name").unwrap_or_default(),
                    label: find("jdk.jfr.Label").map(str::to_owned),
                    type_idx,
                    constant_pool: attr(f, "constantPool").as_deref() == Some("true"),
                    array: attr(f, "dimension").is_some_and(|d| d != "0"),
                    timespan: find("jdk.jfr.Timespan").and_then(TimeUnit::parse),
                    timestamp: find("jdk.jfr.Timestamp").and_then(TimeUnit::parse),
                });
            }
            let t = &mut md.types[i];
            t.label = label;
            if t.prim == Prim::Complex {
                t.fields = fields;
            }
        }
        Ok(md)
    }

    pub fn type_by_name(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }
}
