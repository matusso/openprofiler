//! Bounds-checked little-endian readers and chunk iteration (`ResChunk_header`).

pub(crate) fn u8_at(d: &[u8], off: usize) -> Option<u8> {
    d.get(off).copied()
}

pub(crate) fn u16_at(d: &[u8], off: usize) -> Option<u16> {
    let b = d.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

pub(crate) fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

pub(crate) fn slice_at(d: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    d.get(off..off.checked_add(len)?)
}

pub(crate) const RES_STRING_POOL_TYPE: u16 = 0x0001;
pub(crate) const RES_TABLE_TYPE: u16 = 0x0002;
pub(crate) const RES_XML_TYPE: u16 = 0x0003;

/// One chunk: `data` spans the whole chunk (header included), clamped to the input.
#[derive(Clone, Copy)]
pub(crate) struct Chunk<'a> {
    pub typ: u16,
    pub header_size: usize,
    pub data: &'a [u8],
}

impl<'a> Chunk<'a> {
    /// Parses the chunk at the start of `d`. Oversized chunk sizes are clamped to the input;
    /// a header size outside `8..=size` is replaced by `default_header`.
    pub fn parse(d: &'a [u8], default_header: usize) -> Option<Chunk<'a>> {
        let typ = u16_at(d, 0)?;
        let header_size = usize::from(u16_at(d, 2)?);
        let size = u32_at(d, 4)? as usize;
        if size < 8 {
            return None;
        }
        let data = d.get(..size.min(d.len()))?;
        let header_size = if (8..=data.len()).contains(&header_size) {
            header_size
        } else {
            default_header.min(data.len())
        };
        Some(Chunk { typ, header_size, data })
    }

    /// Bytes after the header.
    pub fn body(&self) -> &'a [u8] {
        self.data.get(self.header_size..).unwrap_or_default()
    }

    /// Iterates the child chunks that follow the header.
    pub fn children(&self) -> Chunks<'a> {
        Chunks { rest: self.body() }
    }
}

pub(crate) struct Chunks<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        let chunk = Chunk::parse(self.rest, 8)?;
        self.rest = self.rest.get(chunk.data.len()..).unwrap_or_default();
        Some(chunk)
    }
}
