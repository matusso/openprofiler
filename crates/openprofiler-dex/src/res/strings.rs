//! `ResStringPool` (chunk type 0x0001): UTF-8 or UTF-16 strings, decoded on demand.

use super::reader::{Chunk, u8_at, u16_at, u32_at};

const UTF8_FLAG: u32 = 0x100;

/// An owned string pool. Strings are decoded lazily so a hostile pool whose offsets all
/// point at one huge string costs nothing until a string is actually used.
#[derive(Default, Clone)]
pub(crate) struct StringPool {
    data: Vec<u8>,
    offsets: Vec<u32>,
    strings_start: usize,
    utf8: bool,
}

impl StringPool {
    pub fn parse(chunk: &Chunk<'_>) -> StringPool {
        let d = chunk.data;
        let count = u32_at(d, 8).unwrap_or(0) as usize;
        let flags = u32_at(d, 16).unwrap_or(0);
        let strings_start = u32_at(d, 20).unwrap_or(0) as usize;
        // Bogus counts are bounded by what the chunk can actually hold.
        let max = d.len().saturating_sub(chunk.header_size) / 4;
        let offsets = (0..count.min(max)).map_while(|i| u32_at(d, chunk.header_size + i * 4)).collect();
        StringPool { data: d.to_vec(), offsets, strings_start, utf8: flags & UTF8_FLAG != 0 }
    }

    /// The string at `index`, or `None` when the index or its data is invalid.
    pub fn get(&self, index: u32) -> Option<String> {
        let off = *self.offsets.get(index as usize)? as usize;
        let start = self.strings_start.checked_add(off)?;
        if self.utf8 { self.utf8_at(start) } else { self.utf16_at(start) }
    }

    /// Like [`get`](Self::get) but yields an empty string for invalid indices.
    pub fn get_or_empty(&self, index: u32) -> String {
        self.get(index).unwrap_or_default()
    }

    fn utf8_at(&self, pos: usize) -> Option<String> {
        let d = &self.data;
        // UTF-16 length (unused), then UTF-8 byte length.
        let (_, pos) = utf8_len(d, pos)?;
        let (n, pos) = utf8_len(d, pos)?;
        let end = pos.checked_add(n)?.min(d.len());
        Some(String::from_utf8_lossy(d.get(pos..end)?).into_owned())
    }

    fn utf16_at(&self, pos: usize) -> Option<String> {
        let d = &self.data;
        let l0 = u16_at(d, pos)?;
        let (n, mut pos) = if l0 & 0x8000 != 0 {
            let l1 = u16_at(d, pos + 2)?;
            ((usize::from(l0 & 0x7fff) << 16) | usize::from(l1), pos + 4)
        } else {
            (usize::from(l0), pos + 2)
        };
        let n = n.min(d.len().saturating_sub(pos) / 2);
        let mut units = Vec::with_capacity(n);
        for _ in 0..n {
            units.push(u16_at(d, pos)?);
            pos += 2;
        }
        Some(String::from_utf16_lossy(&units))
    }
}

/// A 1- or 2-byte UTF-8 pool length; returns (length, position after it).
fn utf8_len(d: &[u8], pos: usize) -> Option<(usize, usize)> {
    let b0 = u8_at(d, pos)?;
    if b0 & 0x80 == 0 {
        return Some((usize::from(b0), pos + 1));
    }
    let b1 = u8_at(d, pos + 1)?;
    Some(((usize::from(b0 & 0x7f) << 8) | usize::from(b1), pos + 2))
}
