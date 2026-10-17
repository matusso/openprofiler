//! Streaming parser for IBM Portable Heap Dumps (`.phd`, written by OpenJ9 / IBM Semeru and the
//! older IBM "Sovereign" JVMs), and conversion to HPROF for the heap engine.
//!
//! The format follows OpenJ9's writer (`runtime/rasdump/heapdump.cpp`) and DTFJ's reader
//! (`com.ibm.dtfj.phd.parser.HeapdumpReader`): a big-endian header (`"portable heap dump"`, version
//! 4–6, flags for 64-bit, all-objects-hashed and J9), then one record per object in address order.
//! Addresses are deltas from the previous object and references are deltas from the object, both
//! scaled by the object alignment (4 bytes on J9 and PHD 5, 8 otherwise). Object records come in
//! short (class from a four-entry cache, up to 3 references), medium (up to 7) and long forms;
//! arrays, classes and long primitive arrays have their own records.
//!
//! A PHD holds the object graph only: classes, object addresses, (non-null) references, array
//! lengths and, from version 6, object sizes. It has no field names or values, no primitive data
//! and no GC roots. See [`convert`] for how that maps onto HPROF.

pub mod convert;

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub use convert::{Conversion, HPROF_MARKER, to_hprof};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a portable heap dump (bad header)")]
    BadHeader,
    #[error("unsupported PHD version {0}")]
    Version(u32),
    #[error("corrupt PHD: {what} at offset {offset}")]
    Corrupt { offset: u64, what: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// The first bytes of every PHD: a Java `writeUTF` of `"portable heap dump"`.
pub const MAGIC: &[u8] = b"\x00\x12portable heap dump";

/// Returns true if `head` (the start of a file, decompressed) is a PHD.
pub fn is_phd(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

/// Returns true if the file is a PHD, optionally gzip-compressed.
pub fn sniff(path: &Path) -> Result<bool> {
    let mut r = open_stream(path)?;
    let mut head = [0u8; MAGIC.len()];
    let mut n = 0;
    while n < head.len() {
        match r.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            // Not gzip after all, or garbage: not a PHD.
            Err(_) => return Ok(false),
        }
    }
    Ok(is_phd(&head[..n]))
}

/// Opens a file for streaming, decompressing gzip transparently.
pub(crate) fn open_stream(path: &Path) -> Result<Box<dyn Read>> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 2];
    let gz = f.read(&mut magic)? == 2 && magic == [0x1f, 0x8b];
    let f = File::open(path)?;
    Ok(if gz {
        Box::new(BufReader::with_capacity(1 << 20, flate2::read::MultiGzDecoder::new(BufReader::new(f))))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, f))
    })
}

/// Top-level record tags.
pub mod tag {
    pub const START_OF_HEADER: u8 = 1;
    pub const START_OF_DUMP: u8 = 2;
    pub const END_OF_DUMP: u8 = 3;
    pub const LONG_OBJECT: u8 = 4;
    pub const OBJECT_ARRAY: u8 = 5;
    pub const CLASS: u8 = 6;
    pub const PRIMITIVE_ARRAY: u8 = 7;
    /// Object array with its true length (references omit nulls).
    pub const NEW_OBJECT_ARRAY: u8 = 8;

    pub const HEADER_TOTALS: u8 = 1;
    pub const HEADER_END: u8 = 2;
    pub const HEADER_HASHCODE_RANDOMS: u8 = 3;
    pub const HEADER_FULL_VERSION: u8 = 4;
}

/// Header flags.
pub mod flag {
    pub const IS_64BIT: u32 = 1;
    pub const ALL_HASHED: u32 = 2;
    pub const IS_J9: u32 = 4;
}

#[derive(Debug, Clone, Default)]
pub struct Header {
    pub version: u32,
    pub flags: u32,
    /// JVM build string (`JRE 21 Linux aarch64-64-Bit Compressed References ...`).
    pub full_version: Option<String>,
    pub total_objects: Option<u32>,
    pub total_refs: Option<u32>,
}

impl Header {
    pub fn is_64bit(&self) -> bool {
        self.flags & flag::IS_64BIT != 0
    }

    pub fn all_hashed(&self) -> bool {
        self.flags & flag::ALL_HASHED != 0
    }

    pub fn is_j9(&self) -> bool {
        self.flags & flag::IS_J9 != 0
    }
}

/// A heap record. Primitive array types use the HPROF basic type codes
/// (`openprofiler_hprof::ty`); sizes are in bytes (PHD 6 and later).
#[derive(Debug, Clone, PartialEq)]
pub enum Record<'a> {
    Object {
        address: u64,
        class: u64,
        refs: &'a [u64],
    },
    /// `class` is the element class, not the array class. `refs` are the non-null elements.
    ObjArray {
        address: u64,
        class: u64,
        refs: &'a [u64],
        length: u32,
        size: Option<u64>,
    },
    PrimArray {
        address: u64,
        ty: u8,
        length: u64,
        size: Option<u64>,
    },
    /// `refs` are the class object's references: its fields as a `java.lang.Class` and statics.
    Class {
        address: u64,
        super_class: u64,
        name: &'a str,
        instance_size: u32,
        refs: &'a [u64],
    },
}

impl Record<'_> {
    pub fn address(&self) -> u64 {
        match *self {
            Record::Object { address, .. }
            | Record::ObjArray { address, .. }
            | Record::PrimArray { address, .. }
            | Record::Class { address, .. } => address,
        }
    }
}

/// Sequential reader over a PHD stream.
pub struct PhdReader<R: Read> {
    r: R,
    pub header: Header,
    pos: u64,
    gap_shift: u32,
    last: u64,
    class_cache: [u64; 4],
    cache_next: usize,
    refs: Vec<u64>,
    name: String,
    /// Builds before IBM defect 78432 wrote short primitive array records with int fields.
    pre78432: bool,
    done: bool,
}

/// Opens a `.phd` or gzip-compressed `.phd.gz` file.
pub fn open(path: &Path) -> Result<PhdReader<Box<dyn Read>>> {
    PhdReader::new(open_stream(path)?)
}

impl<R: Read> PhdReader<R> {
    pub fn new(r: R) -> Result<Self> {
        let mut p = Self {
            r,
            header: Header::default(),
            pos: 0,
            gap_shift: 3,
            last: 0,
            class_cache: [0; 4],
            cache_next: 0,
            refs: Vec::new(),
            name: String::new(),
            pre78432: false,
            done: false,
        };
        let magic = p.utf().map_err(|_| Error::BadHeader)?;
        if magic != "portable heap dump" {
            return Err(Error::BadHeader);
        }
        let version = p.u32()?;
        if !(4..=6).contains(&version) {
            return Err(Error::Version(version));
        }
        p.header.version = version;
        p.header.flags = p.u32()?;
        if version == 5 || p.header.is_j9() {
            p.gap_shift = 2;
        }
        if p.u8()? != tag::START_OF_HEADER {
            return Err(p.corrupt("missing start of header"));
        }
        loop {
            match p.u8()? {
                tag::HEADER_END => break,
                tag::HEADER_TOTALS => {
                    p.header.total_objects = Some(p.u32()?);
                    p.header.total_refs = Some(p.u32()?);
                }
                tag::HEADER_HASHCODE_RANDOMS => {
                    p.u32()?;
                    p.u32()?;
                }
                tag::HEADER_FULL_VERSION => {
                    let v = p.utf()?;
                    p.pre78432 = v.ends_with("0917");
                    p.header.full_version = Some(v);
                }
                t => return Err(p.corrupt(&format!("unknown header tag {t}"))),
            }
        }
        if p.u8()? != tag::START_OF_DUMP {
            return Err(p.corrupt("missing start of dump"));
        }
        Ok(p)
    }

    /// Bytes consumed so far (of the decompressed stream).
    pub fn position(&self) -> u64 {
        self.pos
    }

    fn corrupt(&self, what: &str) -> Error {
        Error::Corrupt { offset: self.pos, what: what.into() }
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut b = [0u8; N];
        self.r.read_exact(&mut b).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => self.corrupt("unexpected end of file"),
            _ => Error::Io(e),
        })?;
        self.pos += N as u64;
        Ok(b)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.bytes()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.bytes()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.bytes()?))
    }

    /// Signed number of `1 << enc` bytes (encoding 0–3: byte, short, int, long).
    fn signed(&mut self, enc: u8) -> Result<i64> {
        Ok(match enc & 3 {
            0 => self.u8()? as i8 as i64,
            1 => self.u16()? as i16 as i64,
            2 => self.u32()? as i32 as i64,
            _ => self.u64()? as i64,
        })
    }

    /// A pointer-sized word, unsigned.
    fn word(&mut self) -> Result<u64> {
        if self.header.is_64bit() { self.u64() } else { Ok(self.u32()? as u64) }
    }

    /// A pointer-sized word, signed.
    fn signed_word(&mut self) -> Result<i64> {
        if self.header.is_64bit() { Ok(self.u64()? as i64) } else { Ok(self.u32()? as i32 as i64) }
    }

    fn utf(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        let mut b = vec![0u8; n];
        self.r.read_exact(&mut b).map_err(|_| self.corrupt("truncated string"))?;
        self.pos += n as u64;
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    fn mask(&self, a: u64) -> u64 {
        if self.header.is_64bit() { a } else { a & 0xffff_ffff }
    }

    fn relative(&mut self, enc: u8) -> Result<u64> {
        let gap = self.signed(enc)? << self.gap_shift;
        Ok(self.mask(self.last.wrapping_add(gap as u64)))
    }

    /// Hash codes are skipped: the analysis has no use for them.
    fn skip_hash(&mut self, stored: bool) -> Result<()> {
        if self.header.is_j9() && self.header.all_hashed() {
            self.u16()?;
        } else if stored {
            self.u32()?;
        }
        Ok(())
    }

    fn instance_size(&mut self) -> Result<Option<u64>> {
        if self.header.version >= 6 { Ok(Some(self.u32()? as u64 * 4)) } else { Ok(None) }
    }

    fn read_refs(&mut self, address: u64, class: Option<u64>, count: u32, enc: u8) -> Result<()> {
        self.refs.clear();
        for i in 0..count {
            let gap = self.signed(enc)? << self.gap_shift;
            let r = self.mask(address.wrapping_add(gap as u64));
            // J9 writes the class as the first reference of instances.
            if self.header.is_j9() && i == 0 && Some(r) == class {
                continue;
            }
            self.refs.push(r);
        }
        Ok(())
    }

    fn cache_class(&mut self, class: u64) {
        self.class_cache[self.cache_next] = class;
        self.cache_next = (self.cache_next + 1) % 4;
    }

    /// The next record, or `None` at the end of the dump.
    pub fn next_record(&mut self) -> Result<Option<Record<'_>>> {
        if self.done {
            return Ok(None);
        }
        let t = self.u8()?;
        let rec = if t & 0x80 != 0 {
            // Short object: class from the cache, up to 3 references.
            let class = self.class_cache[((t >> 5) & 3) as usize];
            let address = self.relative((t >> 2) & 1)?;
            self.skip_hash(false)?;
            self.read_refs(address, Some(class), ((t >> 3) & 3) as u32, t & 3)?;
            self.last = address;
            Record::Object { address, class, refs: &self.refs }
        } else if t & 0x40 != 0 {
            // Medium object: explicit class, up to 7 references.
            let address = self.relative((t >> 2) & 1)?;
            let class = self.word()?;
            self.cache_class(class);
            self.skip_hash(false)?;
            self.read_refs(address, Some(class), ((t >> 3) & 7) as u32, t & 3)?;
            self.last = address;
            Record::Object { address, class, refs: &self.refs }
        } else if t & 0x20 != 0 {
            // Short primitive array: type, and one encoding for the gap and the length.
            let ty = (t >> 2) & 7;
            let enc = t & 3;
            let (address, length) = if self.pre78432 && enc == 3 {
                let gap = (self.u32()? as i32 as i64) << self.gap_shift;
                (self.mask(self.last.wrapping_add(gap as u64)), self.u32()? as u64)
            } else {
                let address = self.relative(enc)?;
                let length = match enc {
                    0 => self.u8()? as u64,
                    1 => self.u16()? as u64,
                    2 => self.u32()? as u64,
                    _ => self.u64()?,
                };
                (address, length)
            };
            self.skip_hash(false)?;
            let size = self.instance_size()?;
            self.last = address;
            Record::PrimArray { address, ty: ty + 4, length, size }
        } else {
            match t {
                tag::END_OF_DUMP => {
                    self.done = true;
                    return Ok(None);
                }
                tag::LONG_OBJECT => {
                    let f = self.u8()?;
                    let address = self.relative((f >> 6) & 3)?;
                    let class = self.word()?;
                    self.cache_class(class);
                    self.skip_hash(f & 2 != 0)?;
                    let n = self.u32()?;
                    self.read_refs(address, Some(class), n, (f >> 4) & 3)?;
                    self.last = address;
                    Record::Object { address, class, refs: &self.refs }
                }
                tag::OBJECT_ARRAY | tag::NEW_OBJECT_ARRAY => {
                    let f = self.u8()?;
                    let address = self.relative((f >> 6) & 3)?;
                    let class = self.word()?;
                    self.skip_hash(f & 2 != 0)?;
                    let n = self.u32()?;
                    self.read_refs(address, Some(class), n, (f >> 4) & 3)?;
                    // Elements are written last to first.
                    self.refs.reverse();
                    let length = if t == tag::NEW_OBJECT_ARRAY { self.u32()? } else { n };
                    let size = self.instance_size()?;
                    self.last = address;
                    Record::ObjArray { address, class, refs: &self.refs, length, size }
                }
                tag::CLASS => {
                    let f = self.u8()?;
                    let address = self.relative((f >> 6) & 3)?;
                    let instance_size = self.u32()?;
                    self.skip_hash(f & 8 != 0)?;
                    let super_class = self.word()?;
                    self.name = self.utf()?;
                    let n = self.u32()?;
                    self.read_refs(address, None, n, (f >> 4) & 3)?;
                    self.last = address;
                    Record::Class { address, super_class, name: &self.name, instance_size, refs: &self.refs }
                }
                tag::PRIMITIVE_ARRAY => {
                    let f = self.u8()?;
                    let ty = f >> 5;
                    let (gap, length) = if f & 0x10 == 0 {
                        (self.u8()? as i8 as i64, self.u8()? as u64)
                    } else {
                        (self.signed_word()?, self.word()?)
                    };
                    let address = self.mask(self.last.wrapping_add((gap << self.gap_shift) as u64));
                    self.skip_hash(f & 2 != 0)?;
                    let size = self.instance_size()?;
                    self.last = address;
                    Record::PrimArray { address, ty: ty + 4, length, size }
                }
                t => return Err(self.corrupt(&format!("unknown record tag {t}"))),
            }
        };
        Ok(Some(rec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes PHD records the way OpenJ9 does, for decoding tests.
    struct W {
        b: Vec<u8>,
    }

    impl W {
        fn new(flags: u32) -> Self {
            let mut b = MAGIC.to_vec();
            b.extend_from_slice(&6u32.to_be_bytes());
            b.extend_from_slice(&flags.to_be_bytes());
            b.push(tag::START_OF_HEADER);
            b.push(tag::HEADER_FULL_VERSION);
            b.extend_from_slice(&4u16.to_be_bytes());
            b.extend_from_slice(b"test");
            b.push(tag::HEADER_END);
            b.push(tag::START_OF_DUMP);
            Self { b }
        }
    }

    #[test]
    fn decodes_each_record_kind() {
        // 64-bit J9: gaps and reference offsets are in 4-byte units.
        let mut w = W::new(flag::IS_64BIT | flag::IS_J9);
        // Class record at 0x1000 (gap 0x400 units, int encoding), super 0, two static refs.
        w.b.extend_from_slice(&[tag::CLASS, (2 << 6) | (1 << 4)]);
        w.b.extend_from_slice(&0x400i32.to_be_bytes());
        w.b.extend_from_slice(&24u32.to_be_bytes());
        w.b.extend_from_slice(&0u64.to_be_bytes());
        w.b.extend_from_slice(&3u16.to_be_bytes());
        w.b.extend_from_slice(b"A/B");
        w.b.extend_from_slice(&2u32.to_be_bytes());
        w.b.extend_from_slice(&4i16.to_be_bytes());
        w.b.extend_from_slice(&(-2i16).to_be_bytes());
        // Medium object at 0x1010 of class 0x1000: first ref is the class (dropped), then +8.
        w.b.push(0x40 | (2 << 3));
        w.b.push(4);
        w.b.extend_from_slice(&0x1000u64.to_be_bytes());
        w.b.extend_from_slice(&[(-4i8) as u8, 2]);
        // Short object at 0x1020, class from cache slot 0, one byte ref -> 0x1010.
        w.b.push(0x80 | (1 << 3));
        w.b.push(4);
        w.b.push((-4i8) as u8);
        // Short primitive int array at 0x1030, length 5, size 40.
        w.b.push(0x20 | (6 << 2));
        w.b.push(4);
        w.b.push(5);
        w.b.extend_from_slice(&10u32.to_be_bytes());
        // Object array at 0x1060 of element class 0x1000: 2 refs, length 10, size 56.
        w.b.extend_from_slice(&[tag::NEW_OBJECT_ARRAY, 1]);
        w.b.push(12);
        w.b.extend_from_slice(&0x1000u64.to_be_bytes());
        w.b.extend_from_slice(&2u32.to_be_bytes());
        w.b.extend_from_slice(&[(-8i8) as u8, (-12i8) as u8]);
        w.b.extend_from_slice(&10u32.to_be_bytes());
        w.b.extend_from_slice(&14u32.to_be_bytes());
        // Long primitive byte array at 0x2000, word-sized gap and length.
        w.b.extend_from_slice(&[tag::PRIMITIVE_ARRAY, (4 << 5) | 0x10]);
        w.b.extend_from_slice(&((0x2000u64 - 0x1060) / 4).to_be_bytes());
        w.b.extend_from_slice(&70000u64.to_be_bytes());
        w.b.extend_from_slice(&17504u32.to_be_bytes());
        w.b.push(tag::END_OF_DUMP);

        let mut r = PhdReader::new(&w.b[..]).unwrap();
        assert!(r.header.is_64bit() && r.header.is_j9());
        assert_eq!(r.header.full_version.as_deref(), Some("test"));
        let mut out = vec![];
        while let Some(rec) = r.next_record().unwrap() {
            out.push(format!("{rec:?}"));
        }
        let expected = [
            Record::Class {
                address: 0x1000,
                super_class: 0,
                name: "A/B",
                instance_size: 24,
                refs: &[0x1010, 0xff8],
            },
            Record::Object { address: 0x1010, class: 0x1000, refs: &[0x1018] },
            Record::Object { address: 0x1020, class: 0x1000, refs: &[0x1010] },
            Record::PrimArray { address: 0x1030, ty: 10, length: 5, size: Some(40) },
            Record::ObjArray {
                address: 0x1060,
                class: 0x1000,
                refs: &[0x1030, 0x1040],
                length: 10,
                size: Some(56),
            },
            Record::PrimArray { address: 0x2000, ty: 8, length: 70000, size: Some(70016) },
        ];
        assert_eq!(out, expected.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>());
        assert!(r.next_record().unwrap().is_none());
    }

    #[test]
    fn rejects_other_files() {
        assert!(matches!(PhdReader::new(&b"JAVA PROFILE 1.0.2\0"[..]), Err(Error::BadHeader)));
        let mut b = MAGIC.to_vec();
        b.extend_from_slice(&9u32.to_be_bytes());
        assert!(matches!(PhdReader::new(&b[..]), Err(Error::Version(9))));
    }
}
