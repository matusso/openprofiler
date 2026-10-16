//! Bounds-checked little-endian reader. Inputs are untrusted (often deliberately malformed), so
//! every access returns an error instead of panicking.

use anyhow::{Result, bail};

#[derive(Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    #[cfg(test)]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn at(data: &'a [u8], pos: usize) -> Result<Self> {
        if pos > data.len() {
            bail!("offset {pos:#x} is past the end ({:#x})", data.len());
        }
        Ok(Self { data, pos })
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len());
        let Some(end) = end else { bail!("read of {n} bytes at {:#x} is out of bounds", self.pos) };
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn uleb(&mut self) -> Result<u32> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!("invalid uleb128 at {:#x}", self.pos)
    }

    pub fn sleb(&mut self) -> Result<i32> {
        let mut v: i32 = 0;
        let mut shift = 0;
        for _ in 0..5 {
            let b = self.u8()?;
            v |= ((b & 0x7f) as i32).wrapping_shl(shift);
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 32 && b & 0x40 != 0 {
                    v |= -1i32 << shift;
                }
                return Ok(v);
            }
        }
        bail!("invalid sleb128 at {:#x}", self.pos)
    }

    /// `uleb128p1`: value + 1, so `0` encodes "none" (`-1`).
    pub fn uleb_p1(&mut self) -> Result<Option<u32>> {
        Ok(self.uleb()?.checked_sub(1))
    }
}

pub fn u16_at(data: &[u8], pos: usize) -> Result<u16> {
    Reader::at(data, pos)?.u16()
}

pub fn u32_at(data: &[u8], pos: usize) -> Result<u32> {
    Reader::at(data, pos)?.u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128() {
        let mut r = Reader::new(&[0x00, 0x7f, 0x80, 0x01, 0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(r.uleb().unwrap(), 0);
        assert_eq!(r.uleb().unwrap(), 127);
        assert_eq!(r.uleb().unwrap(), 128);
        assert_eq!(r.uleb().unwrap(), u32::MAX);
        let mut r = Reader::new(&[0x7f, 0x80, 0x7f, 0x3f]);
        assert_eq!(r.sleb().unwrap(), -1);
        assert_eq!(r.sleb().unwrap(), -128);
        assert_eq!(r.sleb().unwrap(), 63);
        assert!(Reader::new(&[0x80]).uleb().is_err());
        assert_eq!(Reader::new(&[0]).uleb_p1().unwrap(), None);
    }
}
