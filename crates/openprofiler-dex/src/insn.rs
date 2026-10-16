//! Dalvik bytecode decoding (https://source.android.com/docs/core/runtime/dalvik-bytecode).

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    F10x,
    F12x,
    F11n,
    F11x,
    F10t,
    F20t,
    F22x,
    F21t,
    F21s,
    F21h,
    F21c,
    F23x,
    F22b,
    F22t,
    F22s,
    F22c,
    F32x,
    F30t,
    F31t,
    F31i,
    F31c,
    F35c,
    F3rc,
    F45cc,
    F4rcc,
    F51l,
}

/// What an instruction's constant-pool index refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    None,
    String,
    Type,
    Field,
    Method,
    CallSite,
    MethodHandle,
    Proto,
}

use Format::*;
use IndexKind as K;

/// `(mnemonic, format, index kind)` per opcode; `None` for unused opcodes.
pub fn opcode(op: u8) -> Option<(&'static str, Format, IndexKind)> {
    let e = |n, f, k| Some((n, f, k));
    let i = op as usize;
    match op {
        0x00 => e("nop", F10x, K::None),
        0x01 => e("move", F12x, K::None),
        0x02 => e("move/from16", F22x, K::None),
        0x03 => e("move/16", F32x, K::None),
        0x04 => e("move-wide", F12x, K::None),
        0x05 => e("move-wide/from16", F22x, K::None),
        0x06 => e("move-wide/16", F32x, K::None),
        0x07 => e("move-object", F12x, K::None),
        0x08 => e("move-object/from16", F22x, K::None),
        0x09 => e("move-object/16", F32x, K::None),
        0x0a => e("move-result", F11x, K::None),
        0x0b => e("move-result-wide", F11x, K::None),
        0x0c => e("move-result-object", F11x, K::None),
        0x0d => e("move-exception", F11x, K::None),
        0x0e => e("return-void", F10x, K::None),
        0x0f => e("return", F11x, K::None),
        0x10 => e("return-wide", F11x, K::None),
        0x11 => e("return-object", F11x, K::None),
        0x12 => e("const/4", F11n, K::None),
        0x13 => e("const/16", F21s, K::None),
        0x14 => e("const", F31i, K::None),
        0x15 => e("const/high16", F21h, K::None),
        0x16 => e("const-wide/16", F21s, K::None),
        0x17 => e("const-wide/32", F31i, K::None),
        0x18 => e("const-wide", F51l, K::None),
        0x19 => e("const-wide/high16", F21h, K::None),
        0x1a => e("const-string", F21c, K::String),
        0x1b => e("const-string/jumbo", F31c, K::String),
        0x1c => e("const-class", F21c, K::Type),
        0x1d => e("monitor-enter", F11x, K::None),
        0x1e => e("monitor-exit", F11x, K::None),
        0x1f => e("check-cast", F21c, K::Type),
        0x20 => e("instance-of", F22c, K::Type),
        0x21 => e("array-length", F12x, K::None),
        0x22 => e("new-instance", F21c, K::Type),
        0x23 => e("new-array", F22c, K::Type),
        0x24 => e("filled-new-array", F35c, K::Type),
        0x25 => e("filled-new-array/range", F3rc, K::Type),
        0x26 => e("fill-array-data", F31t, K::None),
        0x27 => e("throw", F11x, K::None),
        0x28 => e("goto", F10t, K::None),
        0x29 => e("goto/16", F20t, K::None),
        0x2a => e("goto/32", F30t, K::None),
        0x2b => e("packed-switch", F31t, K::None),
        0x2c => e("sparse-switch", F31t, K::None),
        0x2d..=0x31 => {
            e(["cmpl-float", "cmpg-float", "cmpl-double", "cmpg-double", "cmp-long"][i - 0x2d], F23x, K::None)
        }
        0x32..=0x37 => e(["if-eq", "if-ne", "if-lt", "if-ge", "if-gt", "if-le"][i - 0x32], F22t, K::None),
        0x38..=0x3d => {
            e(["if-eqz", "if-nez", "if-ltz", "if-gez", "if-gtz", "if-lez"][i - 0x38], F21t, K::None)
        }
        0x44..=0x51 => e(
            [
                "aget",
                "aget-wide",
                "aget-object",
                "aget-boolean",
                "aget-byte",
                "aget-char",
                "aget-short",
                "aput",
                "aput-wide",
                "aput-object",
                "aput-boolean",
                "aput-byte",
                "aput-char",
                "aput-short",
            ][i - 0x44],
            F23x,
            K::None,
        ),
        0x52..=0x5f => e(
            [
                "iget",
                "iget-wide",
                "iget-object",
                "iget-boolean",
                "iget-byte",
                "iget-char",
                "iget-short",
                "iput",
                "iput-wide",
                "iput-object",
                "iput-boolean",
                "iput-byte",
                "iput-char",
                "iput-short",
            ][i - 0x52],
            F22c,
            K::Field,
        ),
        0x60..=0x6d => e(
            [
                "sget",
                "sget-wide",
                "sget-object",
                "sget-boolean",
                "sget-byte",
                "sget-char",
                "sget-short",
                "sput",
                "sput-wide",
                "sput-object",
                "sput-boolean",
                "sput-byte",
                "sput-char",
                "sput-short",
            ][i - 0x60],
            F21c,
            K::Field,
        ),
        0x6e..=0x72 => e(
            ["invoke-virtual", "invoke-super", "invoke-direct", "invoke-static", "invoke-interface"]
                [i - 0x6e],
            F35c,
            K::Method,
        ),
        0x74..=0x78 => e(
            [
                "invoke-virtual/range",
                "invoke-super/range",
                "invoke-direct/range",
                "invoke-static/range",
                "invoke-interface/range",
            ][i - 0x74],
            F3rc,
            K::Method,
        ),
        0x7b..=0x8f => e(UNOPS[i - 0x7b], F12x, K::None),
        0x90..=0xaf => e(BINOPS[i - 0x90], F23x, K::None),
        0xb0..=0xcf => e(BINOPS_2ADDR[i - 0xb0], F12x, K::None),
        0xd0..=0xd7 => e(
            [
                "add-int/lit16",
                "rsub-int",
                "mul-int/lit16",
                "div-int/lit16",
                "rem-int/lit16",
                "and-int/lit16",
                "or-int/lit16",
                "xor-int/lit16",
            ][i - 0xd0],
            F22s,
            K::None,
        ),
        0xd8..=0xe2 => e(
            [
                "add-int/lit8",
                "rsub-int/lit8",
                "mul-int/lit8",
                "div-int/lit8",
                "rem-int/lit8",
                "and-int/lit8",
                "or-int/lit8",
                "xor-int/lit8",
                "shl-int/lit8",
                "shr-int/lit8",
                "ushr-int/lit8",
            ][i - 0xd8],
            F22b,
            K::None,
        ),
        0xfa => e("invoke-polymorphic", F45cc, K::Method),
        0xfb => e("invoke-polymorphic/range", F4rcc, K::Method),
        0xfc => e("invoke-custom", F35c, K::CallSite),
        0xfd => e("invoke-custom/range", F3rc, K::CallSite),
        0xfe => e("const-method-handle", F21c, K::MethodHandle),
        0xff => e("const-method-type", F21c, K::Proto),
        _ => None,
    }
}

const UNOPS: [&str; 21] = [
    "neg-int",
    "not-int",
    "neg-long",
    "not-long",
    "neg-float",
    "neg-double",
    "int-to-long",
    "int-to-float",
    "int-to-double",
    "long-to-int",
    "long-to-float",
    "long-to-double",
    "float-to-int",
    "float-to-long",
    "float-to-double",
    "double-to-int",
    "double-to-long",
    "double-to-float",
    "int-to-byte",
    "int-to-char",
    "int-to-short",
];

const BINOPS: [&str; 32] = [
    "add-int",
    "sub-int",
    "mul-int",
    "div-int",
    "rem-int",
    "and-int",
    "or-int",
    "xor-int",
    "shl-int",
    "shr-int",
    "ushr-int",
    "add-long",
    "sub-long",
    "mul-long",
    "div-long",
    "rem-long",
    "and-long",
    "or-long",
    "xor-long",
    "shl-long",
    "shr-long",
    "ushr-long",
    "add-float",
    "sub-float",
    "mul-float",
    "div-float",
    "rem-float",
    "add-double",
    "sub-double",
    "mul-double",
    "div-double",
    "rem-double",
];

const BINOPS_2ADDR: [&str; 32] = [
    "add-int/2addr",
    "sub-int/2addr",
    "mul-int/2addr",
    "div-int/2addr",
    "rem-int/2addr",
    "and-int/2addr",
    "or-int/2addr",
    "xor-int/2addr",
    "shl-int/2addr",
    "shr-int/2addr",
    "ushr-int/2addr",
    "add-long/2addr",
    "sub-long/2addr",
    "mul-long/2addr",
    "div-long/2addr",
    "rem-long/2addr",
    "and-long/2addr",
    "or-long/2addr",
    "xor-long/2addr",
    "shl-long/2addr",
    "shr-long/2addr",
    "ushr-long/2addr",
    "add-float/2addr",
    "sub-float/2addr",
    "mul-float/2addr",
    "div-float/2addr",
    "rem-float/2addr",
    "add-double/2addr",
    "sub-double/2addr",
    "mul-double/2addr",
    "div-double/2addr",
    "rem-double/2addr",
];

pub fn format_units(f: Format) -> u32 {
    match f {
        F10x | F12x | F11n | F11x | F10t => 1,
        F20t | F22x | F21t | F21s | F21h | F21c | F23x | F22b | F22t | F22s | F22c => 2,
        F32x | F30t | F31t | F31i | F31c | F35c | F3rc => 3,
        F45cc | F4rcc => 4,
        F51l => 5,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    PackedSwitch { first_key: i32, targets: Vec<i32> },
    SparseSwitch { keys: Vec<i32>, targets: Vec<i32> },
    FillArray { width: u16, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insn {
    /// Address in 16-bit code units.
    pub addr: u32,
    pub units: u32,
    pub op: u8,
    /// Registers in operand order (destination first).
    pub regs: Vec<u16>,
    /// Literal (`const`, `/lit8`, `/lit16`, `/high16` already shifted).
    pub lit: i64,
    pub index: u32,
    /// Second index (proto of invoke-polymorphic).
    pub index2: u32,
    /// Branch offset relative to `addr` (goto, if, switch / fill-array-data payload).
    pub offset: i32,
}

impl Insn {
    pub fn name(&self) -> &'static str {
        opcode(self.op).map_or("unknown", |o| o.0)
    }

    pub fn format(&self) -> Option<Format> {
        opcode(self.op).map(|o| o.1)
    }

    pub fn index_kind(&self) -> IndexKind {
        opcode(self.op).map_or(IndexKind::None, |o| o.2)
    }

    pub fn target(&self) -> u32 {
        self.addr.wrapping_add_signed(self.offset)
    }
}

fn s4(v: u16) -> i64 {
    (((v as i16) << 12) >> 12) as i64
}

/// Decodes all instructions of a method, skipping switch/array payloads.
pub fn decode_all(code: &[u16]) -> Result<(Vec<Insn>, Vec<(u32, Payload)>)> {
    let mut out = vec![];
    let mut payloads = vec![];
    let mut pc = 0usize;
    while pc < code.len() {
        let u0 = code[pc];
        if u0 & 0xff == 0 && u0 >> 8 != 0 {
            let (p, units) = decode_payload(&code[pc..])?;
            payloads.push((pc as u32, p));
            pc += units;
            continue;
        }
        let insn = decode(code, pc)?;
        pc += insn.units as usize;
        out.push(insn);
    }
    Ok((out, payloads))
}

pub fn decode(code: &[u16], pc: usize) -> Result<Insn> {
    let u0 = code[pc];
    let op = (u0 & 0xff) as u8;
    let Some((_, f, _)) = opcode(op) else { bail!("unknown opcode {op:#04x} at {pc:#x}") };
    let n = format_units(f) as usize;
    if pc + n > code.len() {
        bail!("truncated instruction at {pc:#x}");
    }
    let u = |i: usize| code[pc + i];
    let aa = u0 >> 8;
    let (a, b) = ((u0 >> 8) & 0xf, u0 >> 12);
    let u32_at = |i: usize| u(i) as u32 | (u(i + 1) as u32) << 16;
    let mut insn =
        Insn { addr: pc as u32, units: n as u32, op, regs: vec![], lit: 0, index: 0, index2: 0, offset: 0 };
    match f {
        F10x => {}
        F12x => insn.regs = vec![a, b],
        F11n => {
            insn.regs = vec![a];
            insn.lit = s4(b);
        }
        F11x => insn.regs = vec![aa],
        F10t => insn.offset = (aa as u8 as i8) as i32,
        F20t => insn.offset = u(1) as i16 as i32,
        F22x => insn.regs = vec![aa, u(1)],
        F21t => {
            insn.regs = vec![aa];
            insn.offset = u(1) as i16 as i32;
        }
        F21s => {
            insn.regs = vec![aa];
            insn.lit = u(1) as i16 as i64;
        }
        F21h => {
            insn.regs = vec![aa];
            insn.lit = if op == 0x19 { (u(1) as i64) << 48 } else { ((u(1) as i32) << 16) as i64 };
        }
        F21c => {
            insn.regs = vec![aa];
            insn.index = u(1) as u32;
        }
        F23x => insn.regs = vec![aa, u(1) & 0xff, u(1) >> 8],
        F22b => {
            insn.regs = vec![aa, u(1) & 0xff];
            insn.lit = (u(1) >> 8) as u8 as i8 as i64;
        }
        F22t => {
            insn.regs = vec![a, b];
            insn.offset = u(1) as i16 as i32;
        }
        F22s => {
            insn.regs = vec![a, b];
            insn.lit = u(1) as i16 as i64;
        }
        F22c => {
            insn.regs = vec![a, b];
            insn.index = u(1) as u32;
        }
        F32x => insn.regs = vec![u(1), u(2)],
        F30t => insn.offset = u32_at(1) as i32,
        F31t => {
            insn.regs = vec![aa];
            insn.offset = u32_at(1) as i32;
        }
        F31i => {
            insn.regs = vec![aa];
            insn.lit = u32_at(1) as i32 as i64;
        }
        F31c => {
            insn.regs = vec![aa];
            insn.index = u32_at(1);
        }
        F35c | F45cc => {
            let count = (u0 >> 12) as usize;
            let g = (u0 >> 8) & 0xf;
            let w = u(2);
            let all = [w & 0xf, (w >> 4) & 0xf, (w >> 8) & 0xf, w >> 12, g];
            if count > 5 {
                bail!("bad register count at {pc:#x}");
            }
            insn.regs = all[..count].to_vec();
            insn.index = u(1) as u32;
            if f == F45cc {
                insn.index2 = u(3) as u32;
            }
        }
        F3rc | F4rcc => {
            let first = u(2) as u32;
            insn.regs = (first..first + aa as u32).map(|r| r as u16).collect();
            insn.index = u(1) as u32;
            if f == F4rcc {
                insn.index2 = u(3) as u32;
            }
        }
        F51l => {
            insn.regs = vec![aa];
            insn.lit = (u32_at(1) as u64 | (u32_at(3) as u64) << 32) as i64;
        }
    }
    Ok(insn)
}

/// Decodes a payload at `code[0]`; returns it and its length in units.
pub fn decode_payload(code: &[u16]) -> Result<(Payload, usize)> {
    let get = |i: usize| code.get(i).copied().ok_or_else(|| anyhow::anyhow!("truncated payload"));
    let i32_at = |i: usize| -> Result<i32> { Ok((get(i)? as u32 | (get(i + 1)? as u32) << 16) as i32) };
    match get(0)? {
        0x0100 => {
            let n = get(1)? as usize;
            let first_key = i32_at(2)?;
            let targets = (0..n).map(|k| i32_at(4 + 2 * k)).collect::<Result<_>>()?;
            Ok((Payload::PackedSwitch { first_key, targets }, n * 2 + 4))
        }
        0x0200 => {
            let n = get(1)? as usize;
            let keys = (0..n).map(|k| i32_at(2 + 2 * k)).collect::<Result<_>>()?;
            let targets = (0..n).map(|k| i32_at(2 + 2 * n + 2 * k)).collect::<Result<_>>()?;
            Ok((Payload::SparseSwitch { keys, targets }, n * 4 + 2))
        }
        0x0300 => {
            let width = get(1)?;
            let n = (get(2)? as u32 | (get(3)? as u32) << 16) as usize;
            let bytes = n
                .checked_mul(width as usize)
                .filter(|&b| b <= code.len() * 2)
                .ok_or_else(|| anyhow::anyhow!("bad array payload"))?;
            let mut data = Vec::with_capacity(bytes);
            for k in 0..bytes.div_ceil(2) {
                let w = get(4 + k)?;
                data.push(w as u8);
                data.push((w >> 8) as u8);
            }
            data.truncate(bytes);
            Ok((Payload::FillArray { width, data }, bytes.div_ceil(2) + 4))
        }
        id => bail!("unknown payload {id:#06x}"),
    }
}

/// Instructions after which control does not fall through.
pub fn ends_flow(op: u8) -> bool {
    matches!(op, 0x0e..=0x11 | 0x27 | 0x28..=0x2a)
}

pub fn is_branch(op: u8) -> bool {
    matches!(op, 0x28..=0x2c | 0x32..=0x3d)
}

/// Instructions that may throw (used to place exception edges).
pub fn can_throw(op: u8) -> bool {
    matches!(
        op,
        0x1a..=0x1f // const-string/class, monitors, check-cast
            | 0x21..=0x27 // array-length, allocation, fill-array-data, throw
            | 0x44..=0x6d // array, instance and static field access
            | 0x6e..=0x78 // invokes
            | 0x93 | 0x94 | 0x9e | 0x9f // div/rem int and long
            | 0xb3 | 0xb4 | 0xbe | 0xbf // div/rem /2addr
            | 0xd3 | 0xd4 | 0xdb | 0xdc // div/rem /lit
            | 0xfa..=0xff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_formats() {
        // const/4 v1, -1 ; invoke-virtual {v2, v3}, meth@0x10 ; packed-switch payload
        let code = [0xf112u16, 0x206e, 0x0010, 0x0032, 0x0e];
        let (insns, _) = decode_all(&code).unwrap();
        assert_eq!(insns[0].name(), "const/4");
        assert_eq!(insns[0].regs, vec![1]);
        assert_eq!(insns[0].lit, -1);
        assert_eq!(insns[1].name(), "invoke-virtual");
        assert_eq!(insns[1].regs, vec![2, 3]);
        assert_eq!(insns[1].index, 0x10);
        assert_eq!(insns[2].name(), "return-void");
        let p = [0x0100u16, 2, 5, 0, 10, 0, 20, 0];
        let (pl, n) = decode_payload(&p).unwrap();
        assert_eq!(n, 8);
        assert_eq!(pl, Payload::PackedSwitch { first_key: 5, targets: vec![10, 20] });
        assert!(decode_all(&[0x0073]).is_err());
        for op in 0..=255u8 {
            if let Some((_, f, _)) = opcode(op) {
                let code = vec![op as u16 | 0x1100; format_units(f) as usize];
                let _ = decode(&code, 0);
                let _ = decode(&code[..1], 0);
            }
        }
    }
}
