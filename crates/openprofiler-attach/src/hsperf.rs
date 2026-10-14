//! Parser for HotSpot `hsperfdata` files (the jvmstat shared-memory format).
//!
//! Layout: a 32-byte prologue (`magic 0xcafec0c0` big-endian, byte order, version, ...,
//! `entry_offset`, `num_entries`) followed by variable-length entries:
//! `entry_length i32 | name_offset i32 | vector_length i32 | data_type u8 | flags u8 |
//! data_units u8 | data_variability u8 | data_offset i32`.

use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum PerfValue {
    Long(i64),
    String(String),
}

impl PerfValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PerfValue::String(s) => Some(s),
            PerfValue::Long(_) => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            PerfValue::Long(v) => Some(*v),
            PerfValue::String(_) => None,
        }
    }
}

const MAGIC: u32 = 0xcafe_c0c0;

pub fn parse(data: &[u8]) -> Result<HashMap<String, PerfValue>> {
    if data.len() < 32 {
        bail!("hsperfdata too short");
    }
    if u32::from_be_bytes(data[0..4].try_into()?) != MAGIC {
        bail!("bad hsperfdata magic");
    }
    let little = data[4] == 1;
    let i32_at = |o: usize| -> Option<i32> {
        let b: [u8; 4] = data.get(o..o + 4)?.try_into().ok()?;
        Some(if little { i32::from_le_bytes(b) } else { i32::from_be_bytes(b) })
    };
    let i64_at = |o: usize| -> Option<i64> {
        let b: [u8; 8] = data.get(o..o + 8)?.try_into().ok()?;
        Some(if little { i64::from_le_bytes(b) } else { i64::from_be_bytes(b) })
    };
    let mut out = HashMap::new();
    let mut off = i32_at(24).unwrap_or(0) as usize;
    let n = i32_at(28).unwrap_or(0).max(0);
    for _ in 0..n {
        let (Some(len), Some(name_off), Some(vlen), Some(data_off)) =
            (i32_at(off), i32_at(off + 4), i32_at(off + 8), i32_at(off + 16))
        else {
            break;
        };
        if len <= 0 {
            break;
        }
        let ty = data.get(off + 12).copied().unwrap_or(0);
        let name_start = off + name_off as usize;
        let name = data
            .get(name_start..)
            .and_then(|s| s.iter().position(|&b| b == 0).map(|e| &s[..e]))
            .map(|s| String::from_utf8_lossy(s).into_owned());
        let value_at = off + data_off as usize;
        let value = match ty {
            b'J' if vlen == 0 => i64_at(value_at).map(PerfValue::Long),
            b'B' if vlen > 0 => data.get(value_at..value_at + vlen as usize).map(|s| {
                let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
                PerfValue::String(String::from_utf8_lossy(&s[..end]).into_owned())
            }),
            _ => None,
        };
        if let (Some(name), Some(value)) = (name, value) {
            out.insert(name, value);
        }
        off += len as usize;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ty: u8, value: &[u8], vlen: i32) -> Vec<u8> {
        let name_off = 20;
        let data_off = (name_off + name.len() + 1).next_multiple_of(8);
        let len = (data_off + value.len()).next_multiple_of(8);
        let mut e = vec![0u8; len];
        e[0..4].copy_from_slice(&(len as i32).to_le_bytes());
        e[4..8].copy_from_slice(&(name_off as i32).to_le_bytes());
        e[8..12].copy_from_slice(&vlen.to_le_bytes());
        e[12] = ty;
        e[16..20].copy_from_slice(&(data_off as i32).to_le_bytes());
        e[name_off..name_off + name.len()].copy_from_slice(name.as_bytes());
        e[data_off..data_off + value.len()].copy_from_slice(value);
        e
    }

    #[test]
    fn parses_synthetic_file() {
        let mut d = vec![0u8; 32];
        d[0..4].copy_from_slice(&MAGIC.to_be_bytes());
        d[4] = 1;
        d[24..28].copy_from_slice(&32i32.to_le_bytes());
        d[28..32].copy_from_slice(&2i32.to_le_bytes());
        d.extend(entry("sun.rt.javaCommand", b'B', b"app.Main arg\0\0\0\0", 16));
        d.extend(entry("sun.rt.createVmBeginTime", b'J', &42i64.to_le_bytes(), 0));
        let m = parse(&d).unwrap();
        assert_eq!(m["sun.rt.javaCommand"].as_str(), Some("app.Main arg"));
        assert_eq!(m["sun.rt.createVmBeginTime"].as_i64(), Some(42));
    }
}
