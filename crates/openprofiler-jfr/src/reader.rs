//! Low-level cursor over chunk bytes.
//!
//! JFR (format 2.x) is big-endian. When a chunk has the "compressed integers" flag, every
//! `short`/`char`/`int`/`long` is written as a little-endian base-128 varint of up to 9 bytes
//! (8 groups of 7 bits, then one full byte); floats, doubles, bytes and booleans are always
//! written raw.

use anyhow::{Result, bail};

#[derive(Clone)]
pub(crate) struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
    pub compressed: bool,
}

/// A string as encoded on the wire, before constant-pool resolution.
pub(crate) enum WireStr<'a> {
    Null,
    Empty,
    /// Key into the `java.lang.String` constant pool.
    Pool(i64),
    Utf8(&'a [u8]),
    Chars(String),
    Latin1(&'a [u8]),
}

impl<'a> WireStr<'a> {
    pub fn into_owned(self) -> Option<Box<str>> {
        match self {
            WireStr::Null | WireStr::Pool(_) => None,
            WireStr::Empty => Some("".into()),
            WireStr::Utf8(b) => Some(String::from_utf8_lossy(b).into()),
            WireStr::Chars(s) => Some(s.into()),
            WireStr::Latin1(b) => Some(b.iter().map(|&c| c as char).collect::<String>().into()),
        }
    }
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8], pos: usize, compressed: bool) -> Self {
        Self { data, pos, compressed }
    }

    #[inline]
    fn eof<T>(&self) -> Result<T> {
        bail!("unexpected end of data at offset {}", self.pos)
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        match self.data.get(self.pos) {
            Some(&b) => {
                self.pos += 1;
                Ok(b)
            }
            None => self.eof(),
        }
    }

    #[inline]
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len());
        match end {
            Some(end) => {
                let s = &self.data[self.pos..end];
                self.pos = end;
                Ok(s)
            }
            None => self.eof(),
        }
    }

    #[inline]
    fn be<const N: usize>(&mut self) -> Result<[u8; N]> {
        let b = self.bytes(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(b);
        Ok(out)
    }

    #[inline]
    pub fn varlong(&mut self) -> Result<u64> {
        let mut ret = 0u64;
        for i in 0..8 {
            let b = self.u8()?;
            ret |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(ret);
            }
        }
        let b = self.u8()?;
        Ok(ret | ((b as u64) << 56))
    }

    #[inline]
    pub fn long(&mut self) -> Result<i64> {
        if self.compressed { Ok(self.varlong()? as i64) } else { Ok(i64::from_be_bytes(self.be()?)) }
    }

    #[inline]
    pub fn int(&mut self) -> Result<i32> {
        if self.compressed { Ok(self.varlong()? as i32) } else { Ok(i32::from_be_bytes(self.be()?)) }
    }

    #[inline]
    pub fn short(&mut self) -> Result<i16> {
        if self.compressed { Ok(self.varlong()? as i16) } else { Ok(i16::from_be_bytes(self.be()?)) }
    }

    #[inline]
    pub fn char(&mut self) -> Result<u16> {
        if self.compressed { Ok(self.varlong()? as u16) } else { Ok(u16::from_be_bytes(self.be()?)) }
    }

    pub fn float(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.be()?))
    }

    pub fn double(&mut self) -> Result<f64> {
        Ok(f64::from_be_bytes(self.be()?))
    }

    /// Big-endian fixed-width values, used by the chunk header regardless of compression.
    pub fn be_i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.be()?))
    }

    pub fn be_u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.be()?))
    }

    /// Non-negative length prefix with a sanity bound against the remaining bytes.
    pub fn len(&mut self) -> Result<usize> {
        let n = self.int()?;
        if n < 0 || n as usize > self.data.len() - self.pos.min(self.data.len()) {
            bail!("invalid length {n} at offset {}", self.pos);
        }
        Ok(n as usize)
    }

    pub fn string(&mut self) -> Result<WireStr<'a>> {
        Ok(match self.u8()? {
            0 => WireStr::Null,
            1 => WireStr::Empty,
            2 => WireStr::Pool(self.long()?),
            3 => {
                let n = self.len()?;
                WireStr::Utf8(self.bytes(n)?)
            }
            4 => {
                let n = self.len()?;
                let mut units = Vec::with_capacity(n);
                for _ in 0..n {
                    units.push(self.char()?);
                }
                WireStr::Chars(String::from_utf16_lossy(&units))
            }
            5 => {
                let n = self.len()?;
                WireStr::Latin1(self.bytes(n)?)
            }
            t => bail!("unknown string encoding {t} at offset {}", self.pos - 1),
        })
    }

    /// Reads a string that must be inline (metadata string table).
    pub fn inline_string(&mut self) -> Result<String> {
        match self.string()? {
            WireStr::Pool(_) => bail!("unexpected pooled string in metadata"),
            s => Ok(s.into_owned().map(String::from).unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        let mut r = Reader::new(&[0x05, 0x80, 0x01, 0xff, 0xff, 0xff, 0xff, 0x0f], 0, true);
        assert_eq!(r.long().unwrap(), 5);
        assert_eq!(r.long().unwrap(), 128);
        assert_eq!(r.int().unwrap(), -1);
        let nine = [0xffu8; 9];
        let mut r = Reader::new(&nine, 0, true);
        assert_eq!(r.long().unwrap(), -1);
        let mut r = Reader::new(&[0, 0, 0, 7], 0, false);
        assert_eq!(r.int().unwrap(), 7);
    }

    #[test]
    fn strings() {
        let data = [3u8, 2, b'h', b'i', 5, 1, 0xe9, 4, 2, 0x41, 0x42, 0, 1, 2, 9];
        let mut r = Reader::new(&data, 0, true);
        assert_eq!(&*r.string().unwrap().into_owned().unwrap(), "hi");
        assert_eq!(&*r.string().unwrap().into_owned().unwrap(), "é");
        assert_eq!(&*r.string().unwrap().into_owned().unwrap(), "AB");
        assert!(matches!(r.string().unwrap(), WireStr::Null));
        assert!(matches!(r.string().unwrap(), WireStr::Empty));
        assert!(matches!(r.string().unwrap(), WireStr::Pool(9)));
    }
}
