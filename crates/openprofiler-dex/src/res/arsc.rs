//! `resources.arsc` (`ResTable`): global string pool, packages, type specs and typed entries.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};

use super::attrs::framework_name;
use super::config::qualifiers;
use super::reader::{Chunk, RES_STRING_POOL_TYPE, RES_TABLE_TYPE, slice_at, u8_at, u16_at, u32_at};
use super::strings::StringPool;
use super::value::ResValue;

const RES_TABLE_PACKAGE_TYPE: u16 = 0x0200;
const RES_TABLE_TYPE_TYPE: u16 = 0x0201;

const NO_ENTRY: u32 = 0xffff_ffff;
const TYPE_FLAG_SPARSE: u8 = 0x01;
const TYPE_FLAG_OFFSET16: u8 = 0x02;
const ENTRY_FLAG_COMPLEX: u16 = 0x0001;
const ENTRY_FLAG_COMPACT: u16 = 0x0008;

/// Parsed `resources.arsc`.
pub struct ResTable {
    pub(crate) strings: StringPool,
    pub(crate) packages: Vec<Package>,
    pub(crate) main: usize,
}

pub(crate) struct Package {
    pub id: u32,
    pub name: String,
    type_names: StringPool,
    type_id_offset: u32,
    keys: StringPool,
    /// Resource id -> key string index, from the first entry seen for that id.
    pub names: BTreeMap<u32, u32>,
    /// (type id, key index) pairs used by several ids, e.g. after name collapsing.
    shared_keys: HashSet<(u32, u32)>,
    pub entries: Vec<Entry>,
}

pub(crate) struct Entry {
    pub id: u32,
    /// Folder qualifiers including the leading dash (`-de`, `-night-v8`), or empty.
    pub qualifiers: String,
    pub value: EntryValue,
}

pub(crate) enum EntryValue {
    Simple(ResValue),
    Complex { parent: u32, items: Vec<(u32, ResValue)> },
}

impl ResTable {
    pub fn parse(data: &[u8]) -> Result<ResTable> {
        let Some(root) = Chunk::parse(data, 12) else {
            bail!("resources.arsc: truncated chunk header");
        };
        if root.typ != RES_TABLE_TYPE {
            bail!("resources.arsc: unexpected chunk type 0x{:04x}", root.typ);
        }
        let mut strings = None;
        let mut packages = Vec::new();
        for chunk in root.children() {
            match chunk.typ {
                RES_STRING_POOL_TYPE if strings.is_none() => {
                    strings = Some(StringPool::parse(&chunk));
                }
                RES_TABLE_PACKAGE_TYPE => {
                    if let Some(pkg) = Package::parse(&chunk) {
                        packages.push(pkg);
                    }
                }
                _ => {}
            }
        }
        let main = packages.iter().position(|p| p.id == 0x7f).unwrap_or(0);
        Ok(ResTable { strings: strings.unwrap_or_default(), packages, main })
    }

    pub(crate) fn main_package(&self) -> Option<&Package> {
        self.packages.get(self.main)
    }

    /// `@type/name`, prefixed with `package:` when not in the main package.
    pub fn name_of(&self, id: u32) -> Option<String> {
        let pkg_id = id >> 24;
        let Some(pkg) = self.packages.iter().find(|p| p.id == pkg_id) else {
            return framework_name(id).map(|(t, n)| format!("@android:{t}/{n}"));
        };
        if !pkg.names.contains_key(&id) {
            return None;
        }
        let (typ, key) = (pkg.type_name(id), pkg.key_name(id));
        if self.main_package().is_some_and(|m| m.id == pkg_id) {
            Some(format!("@{typ}/{key}"))
        } else {
            Some(format!("@{}:{typ}/{key}", pkg.name))
        }
    }
}

impl Package {
    fn parse(chunk: &Chunk<'_>) -> Option<Package> {
        let d = chunk.data;
        let id = u32_at(d, 8)?;
        let name = slice_at(d, 12, 256)
            .map(|b| {
                let units: Vec<u16> = b
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_le_bytes(c))
                    .take_while(|&u| u != 0)
                    .collect();
                String::from_utf16_lossy(&units)
            })
            .unwrap_or_default();
        let type_strings = u32_at(d, 268).unwrap_or(0) as usize;
        let key_strings = u32_at(d, 276).unwrap_or(0) as usize;
        let type_id_offset = if chunk.header_size >= 288 { u32_at(d, 284).unwrap_or(0) } else { 0 };
        let pool_at = |off: usize| {
            let c = Chunk::parse(d.get(off..)?, 28)?;
            (c.typ == RES_STRING_POOL_TYPE).then(|| StringPool::parse(&c))
        };
        let mut type_names = pool_at(type_strings);
        let mut keys = pool_at(key_strings);
        let mut pkg_pools = Vec::new();
        let mut types = Vec::new();
        for c in chunk.children() {
            match c.typ {
                RES_STRING_POOL_TYPE => pkg_pools.push(c),
                RES_TABLE_TYPE_TYPE => types.push(c),
                _ => {}
            }
        }
        // Fall back to pool order when the header offsets are bogus.
        let mut pools = pkg_pools.iter();
        if type_names.is_none() {
            type_names = pools.next().map(StringPool::parse);
        }
        if keys.is_none() {
            keys = pools.next_back().map(StringPool::parse);
        }
        let mut pkg = Package {
            id,
            name,
            type_names: type_names.unwrap_or_default(),
            type_id_offset,
            keys: keys.unwrap_or_default(),
            names: BTreeMap::new(),
            shared_keys: HashSet::new(),
            entries: Vec::new(),
        };
        for t in &types {
            pkg.parse_type(t);
        }
        let mut seen = HashSet::new();
        for (&id, &key) in &pkg.names {
            if !seen.insert(((id >> 16) & 0xff, key)) {
                pkg.shared_keys.insert(((id >> 16) & 0xff, key));
            }
        }
        Some(pkg)
    }

    fn parse_type(&mut self, chunk: &Chunk<'_>) {
        let d = chunk.data;
        let (Some(type_id), Some(flags), Some(count), Some(entries_start)) =
            (u8_at(d, 8), u8_at(d, 9), u32_at(d, 12), u32_at(d, 16))
        else {
            return;
        };
        if type_id == 0 {
            return;
        }
        let config_len = (u32_at(d, 20).unwrap_or(0) as usize).min(chunk.header_size.saturating_sub(20));
        let qualifiers = qualifiers(slice_at(d, 20, config_len).unwrap_or_default());
        let entries_start = entries_start as usize;
        let table = chunk.header_size;
        let avail = d.len().saturating_sub(table);
        let count = count as usize;

        // (entry index, byte offset from entries_start)
        let mut slots: Vec<(u32, usize)> = Vec::new();
        if flags & TYPE_FLAG_SPARSE != 0 {
            for i in 0..count.min(avail / 4) {
                let (Some(idx), Some(off)) = (u16_at(d, table + i * 4), u16_at(d, table + i * 4 + 2)) else {
                    break;
                };
                slots.push((u32::from(idx), usize::from(off) * 4));
            }
        } else if flags & TYPE_FLAG_OFFSET16 != 0 {
            for i in 0..count.min(avail / 2) {
                match u16_at(d, table + i * 2) {
                    Some(0xffff) => {}
                    Some(off) => slots.push((i as u32, usize::from(off) * 4)),
                    None => break,
                }
            }
        } else {
            for i in 0..count.min(avail / 4) {
                match u32_at(d, table + i * 4) {
                    Some(NO_ENTRY) => {}
                    Some(off) => slots.push((i as u32, off as usize)),
                    None => break,
                }
            }
        }

        let base = (self.id << 24) | (u32::from(type_id) << 16);
        for (idx, off) in slots {
            if idx > 0xffff {
                continue;
            }
            let Some(at) = entries_start.checked_add(off) else {
                continue;
            };
            let Some((key, value)) = parse_entry(d, at) else {
                continue;
            };
            let id = base | idx;
            self.names.entry(id).or_insert(key);
            self.entries.push(Entry { id, qualifiers: qualifiers.clone(), value });
        }
    }

    pub fn type_name(&self, id: u32) -> String {
        let type_id = (id >> 16) & 0xff;
        let name = type_id
            .checked_sub(1 + self.type_id_offset)
            .and_then(|i| self.type_names.get(i))
            .unwrap_or_default();
        if is_name(&name) { name } else { format!("type{type_id:02x}") }
    }

    /// Key name, or `res_0x7f010000` when it is unusable or shared with other ids.
    pub fn key_name(&self, id: u32) -> String {
        let key = self.names.get(&id).copied();
        let name = key
            .filter(|&k| !self.shared_keys.contains(&((id >> 16) & 0xff, k)))
            .and_then(|k| self.keys.get(k))
            .unwrap_or_default();
        if is_name(&name) { name } else { format!("res_0x{id:08x}") }
    }
}

/// Parses a `ResTable_entry` (full or compact) at `at`; returns (key index, value).
fn parse_entry(d: &[u8], at: usize) -> Option<(u32, EntryValue)> {
    if at >= d.len() {
        return None;
    }
    let size = u16_at(d, at)?;
    let flags = u16_at(d, at + 2)?;
    if flags & ENTRY_FLAG_COMPACT != 0 {
        let value = ResValue { typ: (flags >> 8) as u8, data: u32_at(d, at + 4)? };
        return Some((u32::from(size), EntryValue::Simple(value)));
    }
    let key = u32_at(d, at + 4)?;
    let size = usize::from(size);
    if flags & ENTRY_FLAG_COMPLEX != 0 {
        let parent = u32_at(d, at + 8)?;
        let count = u32_at(d, at + 12)? as usize;
        let maps = at + size.max(16);
        let count = count.min(d.len().saturating_sub(maps) / 12);
        let items = (0..count)
            .map_while(|i| {
                let m = maps + i * 12;
                Some((u32_at(d, m)?, ResValue::read(d, m + 4)?))
            })
            .collect();
        Some((key, EntryValue::Complex { parent, items }))
    } else {
        let value = ResValue::read(d, at + size.max(8))?;
        Some((key, EntryValue::Simple(value)))
    }
}

/// Whether `s` is usable as a resource type/key name in generated XML.
fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '$'))
}
