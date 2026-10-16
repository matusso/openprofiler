//! Minimal zip reader (central directory, stored and deflated entries) for APK containers.
//!
//! APK protectors corrupt local headers and flags, so entries are located through the central
//! directory and only the local header's variable lengths are trusted.

use crate::read::{Reader, u16_at, u32_at};
use anyhow::{Context, Result, bail};
use std::io::Read;

const EOCD_SIG: u32 = 0x0605_4b50;
const CEN_SIG: u32 = 0x0201_4b50;
const LOC_SIG: u32 = 0x0403_4b50;
/// Refuse to inflate single entries beyond this (zip bombs).
const MAX_ENTRY: u64 = 1 << 31;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub method: u16,
    pub compressed_size: u64,
    pub size: u64,
    local_offset: u64,
}

pub struct Zip<'a> {
    data: &'a [u8],
    pub entries: Vec<Entry>,
}

pub fn is_zip(data: &[u8]) -> bool {
    data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06")
}

impl<'a> Zip<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let eocd = find_eocd(data).context("not a zip file (no end of central directory)")?;
        let count = u16_at(data, eocd + 10)? as usize;
        let cd_off = u32_at(data, eocd + 16)? as usize;
        let mut r = Reader::at(data, cd_off).context("central directory offset out of range")?;
        let mut entries = Vec::with_capacity(count);
        // The count may be truncated (>65535 entries); read until the signature stops matching.
        while let Ok(sig) = r.clone().u32() {
            if sig != CEN_SIG {
                break;
            }
            let head = r.bytes(46)?;
            let le16 = |o: usize| u16::from_le_bytes([head[o], head[o + 1]]);
            let le32 = |o: usize| u32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]);
            let method = le16(10);
            let (name_len, extra_len, comment_len) =
                (le16(28) as usize, le16(30) as usize, le16(32) as usize);
            let name = String::from_utf8_lossy(r.bytes(name_len)?).into_owned();
            r.bytes(extra_len + comment_len)?;
            entries.push(Entry {
                name,
                method,
                compressed_size: le32(20) as u64,
                size: le32(24) as u64,
                local_offset: le32(42) as u64,
            });
        }
        if entries.is_empty() && count > 0 {
            bail!("corrupt central directory");
        }
        Ok(Self { data, entries })
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn read(&self, e: &Entry) -> Result<Vec<u8>> {
        let off = e.local_offset as usize;
        if u32_at(self.data, off)? != LOC_SIG {
            bail!("{}: bad local header", e.name);
        }
        let name_len = u16_at(self.data, off + 26)? as usize;
        let extra_len = u16_at(self.data, off + 28)? as usize;
        let start = off + 30 + name_len + extra_len;
        let mut r = Reader::at(self.data, start)?;
        if e.size > MAX_ENTRY {
            bail!("{}: entry too large", e.name);
        }
        match e.method {
            0 => Ok(r.bytes(e.size as usize)?.to_vec()),
            8 => {
                // Some protectors lie about the compressed size; inflate from the start instead.
                let avail = &self.data[start.min(self.data.len())..];
                let mut out = Vec::with_capacity(e.size.min(1 << 26) as usize);
                flate2::read::DeflateDecoder::new(avail)
                    .take(e.size.max(1) * 2 + 1024)
                    .read_to_end(&mut out)
                    .with_context(|| format!("{}: inflate failed", e.name))?;
                if e.size > 0 && out.len() as u64 > e.size {
                    out.truncate(e.size as usize);
                }
                Ok(out)
            }
            // Protectors set bogus methods on stored entries; trust the sizes when they agree.
            _ if e.compressed_size == e.size => Ok(r.bytes(e.size as usize)?.to_vec()),
            m => bail!("{}: unsupported compression method {m}", e.name),
        }
    }
}

fn find_eocd(data: &[u8]) -> Option<usize> {
    let min = data.len().saturating_sub(22 + 0xffff);
    (min..=data.len().checked_sub(22)?).rev().find(|&i| u32_at(data, i).ok() == Some(EOCD_SIG))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a zip with one stored and one deflated entry.
    pub fn make_zip(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        use std::io::Write;
        let mut out = vec![];
        let mut cen = vec![];
        for (name, data, deflate) in files {
            let body = if *deflate {
                let mut e = flate2::write::DeflateEncoder::new(vec![], flate2::Compression::default());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let off = out.len() as u32;
            out.extend(LOC_SIG.to_le_bytes());
            out.extend([20, 0, 0, 0]);
            out.extend(method.to_le_bytes());
            out.extend([0; 8]); // time, date, crc
            out.extend((body.len() as u32).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(&body);
            cen.extend(CEN_SIG.to_le_bytes());
            cen.extend([20, 0, 20, 0, 0, 0]);
            cen.extend(method.to_le_bytes());
            cen.extend([0; 8]);
            cen.extend((body.len() as u32).to_le_bytes());
            cen.extend((data.len() as u32).to_le_bytes());
            cen.extend((name.len() as u16).to_le_bytes());
            cen.extend([0; 12]);
            cen.extend(off.to_le_bytes());
            cen.extend(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend(&cen);
        out.extend(EOCD_SIG.to_le_bytes());
        out.extend([0; 4]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((cen.len() as u32).to_le_bytes());
        out.extend(cd_off.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    #[test]
    fn reads_stored_and_deflated() {
        let z = make_zip(&[("a.txt", b"hello", false), ("b/c.bin", &[7u8; 1000], true)]);
        let zip = Zip::parse(&z).unwrap();
        assert_eq!(zip.entries.len(), 2);
        assert_eq!(zip.read(zip.find("a.txt").unwrap()).unwrap(), b"hello");
        assert_eq!(zip.read(zip.find("b/c.bin").unwrap()).unwrap(), vec![7u8; 1000]);
        for n in 0..z.len() {
            if let Ok(zip) = Zip::parse(&z[..n]) {
                for e in &zip.entries {
                    let _ = zip.read(e);
                }
            }
        }
    }
}
