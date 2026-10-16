//! Android binary XML (`ResXMLTree`) → indented text XML.

use std::fmt::Write as _;

use anyhow::{Result, bail};

use super::ResTable;
use super::attrs::{android_attr_name, android_attr_symbol};
use super::reader::{Chunk, RES_STRING_POOL_TYPE, RES_XML_TYPE, u16_at, u32_at};
use super::strings::StringPool;
use super::value::{ResValue, TYPE_INT_DEC, TYPE_INT_HEX, TYPE_NULL, xml_escape};

const RES_XML_START_NAMESPACE_TYPE: u16 = 0x0100;
const RES_XML_END_NAMESPACE_TYPE: u16 = 0x0101;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;
const RES_XML_CDATA_TYPE: u16 = 0x0104;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;

const NO_INDEX: u32 = 0xffff_ffff;
const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";
/// Refuse to produce absurd output from hostile inputs (e.g. many references to a huge string).
const MAX_OUTPUT: usize = 64 << 20;
/// Deeper elements are attached flat so printing and dropping the tree cannot overflow the stack.
const MAX_DEPTH: usize = 256;

pub(crate) fn is_binary_xml(data: &[u8]) -> bool {
    u16_at(data, 0) == Some(RES_XML_TYPE) && data.len() >= 8
}

#[derive(Default)]
struct Element {
    name: String,
    namespaces: Vec<(String, String)>,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

enum Node {
    Element(Element),
    Text(String),
}

struct Decoder<'t> {
    table: Option<&'t ResTable>,
    pool: StringPool,
    res_map: Vec<u32>,
    /// Active (prefix, uri) declarations, innermost last.
    namespaces: Vec<(String, String)>,
    pending_ns: Vec<(String, String)>,
    /// Open elements; index 0 is a synthetic document node.
    stack: Vec<Element>,
    /// Start elements attached flat beyond `MAX_DEPTH` whose end elements must be skipped.
    overflow: usize,
    /// Bytes of decoded names and values so far, bounded by `MAX_OUTPUT`.
    decoded: usize,
}

pub(crate) fn decode_xml(data: &[u8], table: Option<&ResTable>) -> Result<String> {
    let Some(root) = Chunk::parse(data, 8) else {
        bail!("binary XML: truncated chunk header");
    };
    if root.typ != RES_XML_TYPE {
        bail!("binary XML: unexpected chunk type 0x{:04x}", root.typ);
    }
    let mut dec = Decoder {
        table,
        pool: StringPool::default(),
        res_map: Vec::new(),
        namespaces: Vec::new(),
        pending_ns: Vec::new(),
        stack: vec![Element::default()],
        overflow: 0,
        decoded: 0,
    };
    let mut have_pool = false;
    for chunk in root.children() {
        match chunk.typ {
            RES_STRING_POOL_TYPE if !have_pool => {
                dec.pool = StringPool::parse(&chunk);
                have_pool = true;
            }
            RES_XML_RESOURCE_MAP_TYPE => {
                dec.res_map =
                    chunk.body().as_chunks::<4>().0.iter().map(|&c| u32::from_le_bytes(c)).collect();
            }
            RES_XML_START_NAMESPACE_TYPE => dec.start_namespace(&chunk),
            RES_XML_END_NAMESPACE_TYPE => dec.end_namespace(&chunk),
            RES_XML_START_ELEMENT_TYPE => dec.start_element(&chunk),
            RES_XML_END_ELEMENT_TYPE => dec.end_element(),
            RES_XML_CDATA_TYPE => dec.cdata(&chunk),
            _ => {}
        }
        if dec.decoded > MAX_OUTPUT {
            bail!("binary XML: decoded output exceeds {MAX_OUTPUT} bytes");
        }
    }
    while dec.stack.len() > 1 {
        dec.end_element();
    }
    let doc = dec.stack.pop().unwrap_or_default();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    for node in &doc.children {
        if let Node::Element(e) = node {
            write_element(&mut out, e, 0)?;
        }
    }
    Ok(out)
}

impl Decoder<'_> {
    fn string(&self, idx: Option<u32>) -> String {
        idx.filter(|&i| i != NO_INDEX).and_then(|i| self.pool.get(i)).unwrap_or_default()
    }

    fn start_namespace(&mut self, c: &Chunk<'_>) {
        let hs = c.header_size;
        let prefix = self.string(u32_at(c.data, hs));
        let uri = self.string(u32_at(c.data, hs + 4));
        let prefix = if is_xml_name(&prefix) {
            prefix
        } else if uri == ANDROID_NS {
            "android".into()
        } else {
            format!("ns{}", self.namespaces.len())
        };
        self.namespaces.push((prefix.clone(), uri.clone()));
        self.pending_ns.push((prefix, uri));
    }

    fn end_namespace(&mut self, c: &Chunk<'_>) {
        let uri = self.string(u32_at(c.data, c.header_size + 4));
        if let Some(i) = self.namespaces.iter().rposition(|(_, u)| *u == uri) {
            self.namespaces.remove(i);
        }
    }

    fn prefix_for(&self, ns: Option<u32>) -> Option<String> {
        let uri = self.string(ns);
        if uri.is_empty() {
            return None;
        }
        let found = self.namespaces.iter().rev().find(|(_, u)| *u == uri);
        Some(match found {
            Some((p, _)) => p.clone(),
            None if uri == ANDROID_NS => "android".into(),
            None => "ns".into(),
        })
    }

    fn qualified(&self, ns: Option<u32>, name: String) -> String {
        match self.prefix_for(ns) {
            Some(p) => format!("{p}:{name}"),
            None => name,
        }
    }

    fn start_element(&mut self, c: &Chunk<'_>) {
        let (d, hs) = (c.data, c.header_size);
        let mut name = self.string(u32_at(d, hs + 4));
        if !is_xml_name(&name) {
            name = "element".into();
        }
        let mut el = Element {
            name: self.qualified(u32_at(d, hs), name),
            namespaces: std::mem::take(&mut self.pending_ns),
            ..Element::default()
        };
        let start = usize::from(u16_at(d, hs + 8).unwrap_or(20));
        let stride = usize::from(u16_at(d, hs + 10).unwrap_or(20)).max(20);
        let count = usize::from(u16_at(d, hs + 12).unwrap_or(0));
        let first = hs + start;
        let count = count.min(d.len().saturating_sub(first) / stride);
        for i in 0..count {
            let a = first + i * stride;
            if let Some(attr) = self.attribute(d, a) {
                self.decoded += attr.0.len() + attr.1.len();
                el.attrs.push(attr);
            }
        }
        if self.stack.len() < MAX_DEPTH {
            self.stack.push(el);
        } else if let Some(parent) = self.stack.last_mut() {
            parent.children.push(Node::Element(el));
            self.overflow += 1;
        }
    }

    fn attribute(&self, d: &[u8], a: usize) -> Option<(String, String)> {
        let ns = u32_at(d, a)?;
        let name_idx = u32_at(d, a + 4)?;
        let raw = u32_at(d, a + 8)?;
        let value = ResValue::read(d, a + 12)?;
        let res_id = self.res_map.get(name_idx as usize).copied().filter(|&id| id != 0);

        let raw_name = self.string(Some(name_idx));
        // The platform resolves attributes by resource id, so prefer the id's real name over a
        // possibly renamed string; fall back to the id itself when the string is unusable.
        let name = match (res_id, res_id.and_then(android_attr_name)) {
            (_, Some(n)) => n.to_string(),
            (Some(id), None) if !is_xml_name(&raw_name) => self
                .table
                .and_then(|t| t.name_of(id))
                .and_then(|n| n.rsplit('/').next().map(str::to_string))
                .unwrap_or_else(|| format!("attr_0x{id:08x}")),
            _ if is_xml_name(&raw_name) => raw_name,
            _ => format!("attr{name_idx}"),
        };
        let symbol = match res_id {
            Some(id) if id >> 24 == 0x01 => matches!(value.typ, TYPE_INT_DEC | TYPE_INT_HEX)
                .then(|| android_attr_symbol(&name, value.data))
                .flatten(),
            Some(id) => self.table.and_then(|t| t.attr_symbol(id, value)),
            None => None,
        };
        let name = self.qualified(Some(ns), name);
        if let Some(symbol) = symbol {
            return Some((name, symbol));
        }

        let text = if value.is_string() {
            self.pool.get(value.data).unwrap_or_else(|| self.string(Some(raw)))
        } else if value.typ == TYPE_NULL && raw != NO_INDEX {
            self.string(Some(raw))
        } else {
            value.format(&self.pool, self.table)
        };
        Some((name, text))
    }

    fn end_element(&mut self) {
        if self.overflow > 0 {
            self.overflow -= 1;
        } else if self.stack.len() > 1
            && let Some(el) = self.stack.pop()
            && let Some(parent) = self.stack.last_mut()
        {
            parent.children.push(Node::Element(el));
        }
    }

    fn cdata(&mut self, c: &Chunk<'_>) {
        let text = self.string(u32_at(c.data, c.header_size));
        self.decoded += text.len();
        if text.trim().is_empty() || self.stack.len() < 2 {
            return;
        }
        if let Some(el) = self.stack.last_mut() {
            el.children.push(Node::Text(text.trim().to_string()));
        }
    }
}

fn write_element(out: &mut String, el: &Element, depth: usize) -> Result<()> {
    if out.len() > MAX_OUTPUT {
        bail!("binary XML: decoded output exceeds {MAX_OUTPUT} bytes");
    }
    let pad = "    ".repeat(depth);
    let _ = write!(out, "{pad}<{}", el.name);
    let attrs: Vec<String> = el
        .namespaces
        .iter()
        .map(|(p, u)| format!("xmlns:{p}=\"{}\"", xml_escape(u)))
        .chain(el.attrs.iter().map(|(n, v)| format!("{n}=\"{}\"", xml_escape(v))))
        .collect();
    if let [a] = attrs.as_slice() {
        let _ = write!(out, " {a}");
    } else {
        for a in &attrs {
            let _ = write!(out, "\n{pad}    {a}");
        }
    }
    if el.children.is_empty() {
        out.push_str(" />\n");
        return Ok(());
    }
    if let [Node::Text(t)] = el.children.as_slice() {
        let _ = writeln!(out, ">{}</{}>", escape_text(t), el.name);
        return Ok(());
    }
    out.push_str(">\n");
    for child in &el.children {
        match child {
            Node::Element(e) => write_element(out, e, depth + 1)?,
            Node::Text(t) => {
                let _ = writeln!(out, "{pad}    {}", escape_text(t));
            }
        }
    }
    let _ = writeln!(out, "{pad}</{}>", el.name);
    Ok(())
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn is_xml_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}
