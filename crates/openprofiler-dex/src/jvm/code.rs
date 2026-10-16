//! JVM bytecode → register instructions (the [`Insn`] form the Dalvik lifter consumes).
//!
//! Local slot `n` is register `n`; operand-stack word `d` is register `locals + d`; a few scratch
//! registers above the stack hold values of synthesized code (string concatenation,
//! multi-dimensional arrays, stack shuffles). The stack shape at every instruction comes from a
//! forward data-flow pass (the verifier guarantees one shape per instruction). Finally registers
//! are rotated so parameters sit at the top of the frame, as Dalvik expects.
//!
//! Stack temporaries become single-use variables that the lifter inlines back into expressions.
//! `new` + `dup` + `<init>` keeps the allocation and the constructor call on one register, and
//! `invokedynamic` string concatenation becomes a `StringBuilder` chain, so the shared passes
//! recover `new T(...)` and `a + b`.

use super::classfile::{ClassFile, Code, Const, Member, desc_of_internal};
use super::{Classes, Piece, Site, Unit};
use crate::dex::{ACC_STATIC, CodeItem, DebugInfo, Handler, LocalVar, Prebuilt, TryItem};
use crate::insn::{Insn, Payload};
use crate::types;
use anyhow::{Result, bail};
use std::sync::Arc;

/// Largest frame (locals + stack + scratch) translated.
const MAX_REGS: u32 = 65000;
const SCRATCH: u32 = 4;

/// A decoded JVM instruction.
struct J {
    pc: u32,
    op: u8,
    /// Local index, constant, constant-pool index or `newarray` type.
    a: i32,
    /// `iinc` increment, `multianewarray` dimensions.
    b: i32,
    /// Branch target; for switches the default.
    target: u32,
    /// Switch `(key, target)` pairs.
    cases: Vec<(i32, u32)>,
}

fn decode(code: &[u8]) -> Result<Vec<J>> {
    let mut out = vec![];
    let mut pc = 0usize;
    let s8 = |i: usize| code.get(i).map(|&b| b as i8 as i32).ok_or_else(|| anyhow::anyhow!("truncated"));
    let u8_ = |i: usize| code.get(i).map(|&b| b as i32).ok_or_else(|| anyhow::anyhow!("truncated"));
    let u16_ = |i: usize| -> Result<i32> { Ok(u8_(i)? << 8 | u8_(i + 1)?) };
    let s16 = |i: usize| -> Result<i32> { Ok(u16_(i)? as i16 as i32) };
    let s32 = |i: usize| -> Result<i32> { Ok(u16_(i)? << 16 | u16_(i + 2)?) };
    let rel = |pc: usize, off: i32| -> Result<u32> {
        let t = pc as i64 + off as i64;
        if t < 0 || t >= code.len() as i64 {
            bail!("branch at {pc} to {t} is outside the code");
        }
        Ok(t as u32)
    };
    while pc < code.len() {
        let op = code[pc];
        let mut j = J { pc: pc as u32, op, a: 0, b: 0, target: 0, cases: vec![] };
        let len = match op {
            0x10 => {
                j.a = s8(pc + 1)?;
                2
            }
            0x11 => {
                j.a = s16(pc + 1)?;
                3
            }
            0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => {
                j.a = u8_(pc + 1)?;
                2
            }
            0x13 | 0x14 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 => {
                j.a = u16_(pc + 1)?;
                3
            }
            0x84 => {
                j.a = u8_(pc + 1)?;
                j.b = s8(pc + 2)?;
                3
            }
            0x99..=0xa8 | 0xc6 | 0xc7 => {
                j.target = rel(pc, s16(pc + 1)?)?;
                3
            }
            0xc8 | 0xc9 => {
                j.target = rel(pc, s32(pc + 1)?)?;
                5
            }
            0xb9 | 0xba => {
                j.a = u16_(pc + 1)?;
                5
            }
            0xc5 => {
                j.a = u16_(pc + 1)?;
                j.b = u8_(pc + 3)?;
                4
            }
            0xc4 => {
                j.op = u8_(pc + 1)? as u8;
                j.a = u16_(pc + 2)?;
                match j.op {
                    0x84 => {
                        j.b = s16(pc + 4)?;
                        6
                    }
                    0x15..=0x19 | 0x36..=0x3a | 0xa9 => 4,
                    o => bail!("bad wide opcode {o:#04x} at {pc}"),
                }
            }
            0xaa | 0xab => {
                let p = (pc + 4) & !3;
                j.target = rel(pc, s32(p)?)?;
                if op == 0xaa {
                    let (low, high) = (s32(p + 4)?, s32(p + 8)?);
                    let n = high as i64 - low as i64 + 1;
                    if !(0..=65536).contains(&n) {
                        bail!("bad tableswitch at {pc}");
                    }
                    for k in 0..n as usize {
                        j.cases.push((low.wrapping_add(k as i32), rel(pc, s32(p + 12 + 4 * k)?)?));
                    }
                    p + 12 + 4 * n as usize - pc
                } else {
                    let n = s32(p + 4)?;
                    if !(0..=65536).contains(&n) {
                        bail!("bad lookupswitch at {pc}");
                    }
                    for k in 0..n as usize {
                        j.cases.push((s32(p + 8 + 8 * k)?, rel(pc, s32(p + 12 + 8 * k)?)?));
                    }
                    p + 8 + 8 * n as usize - pc
                }
            }
            0x00..=0xc3 => 1,
            o => bail!("unknown opcode {o:#04x} at {pc}"),
        };
        out.push(j);
        pc += len;
    }
    Ok(out)
}

/// A stack value: its size in words and, while uninitialized, the `new` that created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct V {
    w: u8,
    new: Option<u32>,
}

const W1: V = V { w: 1, new: None };
const W2: V = V { w: 2, new: None };

fn words(st: &[V]) -> u32 {
    st.iter().map(|v| v.w as u32).sum()
}

/// Descriptor size in words (0 for void).
fn size(desc: &str) -> u8 {
    match desc {
        "V" => 0,
        "J" | "D" => 2,
        _ => 1,
    }
}

struct Ctx<'a> {
    classes: &'a Classes,
    unit: &'a Unit,
    cf: &'a ClassFile,
    /// First stack register.
    locals: u32,
    /// First scratch register.
    scratch: u32,
    /// `jsr` call sites: `(key, pc after the jsr)`. A `jsr` stores its key as the return
    /// address; `ret` switches on it.
    returns: Vec<(i32, u32)>,
}

struct Out {
    insns: Vec<Insn>,
    /// `(instruction index, target pc)` for branches.
    fixups: Vec<(usize, u32)>,
    /// `(switch instruction index, payload)` with targets as pcs.
    switches: Vec<(usize, Payload)>,
}

impl Out {
    fn push(&mut self, op: u8, regs: Vec<u32>) -> &mut Insn {
        let addr = self.insns.len() as u32;
        self.insns.push(Insn {
            addr,
            units: 1,
            op,
            regs: regs.into_iter().map(|r| r as u16).collect(),
            lit: 0,
            index: 0,
            index2: 0,
            offset: 0,
        });
        self.insns.last_mut().unwrap()
    }

    fn lit(&mut self, op: u8, reg: u32, lit: i64) {
        self.push(op, vec![reg]).lit = lit;
    }

    fn idx(&mut self, op: u8, regs: Vec<u32>, index: u32) {
        self.push(op, regs).index = index;
    }

    fn branch(&mut self, op: u8, regs: Vec<u32>, target: u32) {
        self.push(op, regs);
        self.fixups.push((self.insns.len() - 1, target));
    }

    fn mov(&mut self, w: u8, dst: u32, src: u32) {
        if dst != src {
            self.push(if w == 2 { 0x04 } else { 0x01 }, vec![dst, src]);
        }
    }
}

/// Moves the values at `from` (register, width) to `to` simultaneously.
fn parallel_move(out: &mut Out, moves: &[(u32, u32, u8)], scratch: u32) {
    let mut pending: Vec<(u32, u32, u8)> = moves.iter().copied().filter(|m| m.0 != m.1).collect();
    let overlaps = |a: u32, aw: u8, b: u32, bw: u8| a < b + bw as u32 && b < a + aw as u32;
    // Where copies of a source already live, to break cycles without scratch.
    let mut copies: Vec<(u32, u32, u8)> = vec![];
    let mut guard = 0;
    while !pending.is_empty() && guard < 64 {
        guard += 1;
        let ready = pending.iter().position(|&(dst, _, w)| {
            !pending.iter().any(|&(d2, s2, w2)| d2 != dst && overlaps(dst, w, s2, w2))
        });
        match ready {
            Some(k) => {
                let (dst, src, w) = pending.remove(k);
                out.mov(w, dst, src);
                copies.push((src, dst, w));
            }
            None => {
                // A cycle: read one source from an existing copy or a scratch register.
                let (dst, src, w) = pending[0];
                let alt = copies
                    .iter()
                    .find(|c| c.0 == src && c.2 == w && !pending.iter().any(|p| overlaps(c.1, w, p.0, p.2)))
                    .map(|c| c.1);
                let tmp = alt.unwrap_or_else(|| {
                    out.mov(w, scratch, src);
                    scratch
                });
                pending[0] = (dst, tmp, w);
            }
        }
    }
}

pub(super) fn translate(classes: &Classes, unit: &Unit, m: &Member) -> Result<CodeItem> {
    let cf = &unit.cf;
    let Some(code) = cf.code(m)? else { bail!("no Code attribute") };
    let is_static = m.access as u32 & ACC_STATIC != 0;
    let (params, _) = types::split_method_desc(cf.utf8(m.desc));
    let ins: u32 = u32::from(!is_static) + params.iter().map(|p| size(p) as u32).sum::<u32>();
    let js = decode(&code.code)?;
    if js.is_empty() {
        bail!("empty method body");
    }
    let mut at = vec![u32::MAX; code.code.len() + 1];
    for (k, j) in js.iter().enumerate() {
        at[j.pc as usize] = k as u32;
    }
    let index = |pc: u32| -> Result<usize> {
        match at.get(pc as usize) {
            Some(&k) if k != u32::MAX => Ok(k as usize),
            _ => bail!("jump to {pc}, which is not an instruction"),
        }
    };
    for h in &code.handlers {
        index(h.handler)?;
    }

    // ---- stack shapes
    let locals = (code.max_locals as u32).max(ins);
    let returns: Vec<(i32, u32)> = js
        .iter()
        .enumerate()
        .filter(|(_, j)| matches!(j.op, 0xa8 | 0xc9))
        .filter_map(|(k, _)| js.get(k + 1).map(|n| n.pc))
        .enumerate()
        .map(|(n, pc)| (n as i32, pc))
        .collect();
    let mut ctx = Ctx { classes, unit, cf, locals, scratch: 0, returns };
    let mut states: Vec<Option<Vec<V>>> = vec![None; js.len()];
    states[0] = Some(vec![]);
    let mut work = vec![0usize];
    let mut max_words = 0u32;
    let mut probe = Out { insns: vec![], fixups: vec![], switches: vec![] };
    let mut rounds = 0usize;
    while let Some(k) = work.pop() {
        rounds += 1;
        if rounds > js.len() * 64 + 1024 {
            bail!("stack analysis did not converge");
        }
        let j = &js[k];
        let mut st = states[k].clone().unwrap_or_default();
        max_words = max_words.max(words(&st));
        // Exception handlers covering this instruction start with just the exception.
        let mut succ: Vec<(usize, Vec<V>)> = vec![];
        for h in code.handlers.iter().filter(|h| (h.start..h.end).contains(&j.pc)) {
            succ.push((index(h.handler)?, vec![W1]));
        }
        probe.insns.clear();
        probe.fixups.clear();
        probe.switches.clear();
        let before = st.clone();
        let flow = step(&ctx, j, &mut st, &mut probe)?;
        max_words = max_words.max(words(&st));
        if flow.after_jsr.is_some() {
            if k + 1 >= js.len() {
                bail!("jsr at the end of the code");
            }
            succ.push((k + 1, before));
        }
        if flow.falls {
            if k + 1 >= js.len() {
                bail!("code falls off the end");
            }
            succ.push((k + 1, st.clone()));
        }
        for t in flow.targets {
            succ.push((index(t)?, st.clone()));
        }
        for (s, shape) in succ {
            match &mut states[s] {
                None => {
                    states[s] = Some(shape);
                    work.push(s);
                }
                Some(old) => {
                    if old.len() != shape.len() || old.iter().zip(&shape).any(|(a, b)| a.w != b.w) {
                        bail!("inconsistent stack at {}", js[s].pc);
                    }
                    let mut changed = false;
                    for (a, b) in old.iter_mut().zip(&shape) {
                        if a.new != b.new && a.new.is_some() {
                            a.new = None;
                            changed = true;
                        }
                    }
                    if changed {
                        work.push(s);
                    }
                }
            }
        }
    }
    ctx.scratch = locals + max_words + 2;
    let total = ctx.scratch + SCRATCH;
    if total > MAX_REGS {
        bail!("frame too large ({total} registers)");
    }

    // ---- emission
    let handler_pcs: std::collections::HashSet<u32> = code.handlers.iter().map(|h| h.handler).collect();
    let mut out = Out { insns: vec![], fixups: vec![], switches: vec![] };
    let mut addr_of = vec![u32::MAX; code.code.len() + 1];
    for (k, j) in js.iter().enumerate() {
        let Some(mut st) = states[k].clone() else { continue };
        addr_of[j.pc as usize] = out.insns.len() as u32;
        if handler_pcs.contains(&j.pc) {
            // `catch (E e)` stores the exception right away: catch it into the local directly.
            let store = match j.op {
                0x3a => Some(j.a as u32),
                0x4b..=0x4e => Some((j.op - 0x4b) as u32),
                _ => None,
            };
            if let Some(n) = store.filter(|_| st.len() == 1) {
                out.push(0x0d, vec![n]);
                continue;
            }
            out.push(0x0d, vec![locals]);
        }
        step(&ctx, j, &mut st, &mut out)?;
    }
    let end = out.insns.len() as u32;
    addr_of[code.code.len()] = end;
    for pc in (0..code.code.len()).rev() {
        if addr_of[pc] == u32::MAX {
            addr_of[pc] = addr_of[pc + 1];
        }
    }
    let addr = |pc: u32| addr_of.get(pc as usize).copied().unwrap_or(end);
    for &(k, t) in &out.fixups {
        let i = &mut out.insns[k];
        i.offset = addr(t) as i32 - i.addr as i32;
    }
    let mut payloads = vec![];
    for (n, (k, p)) in std::mem::take(&mut out.switches).into_iter().enumerate() {
        let paddr = end + n as u32;
        let from = out.insns[k].addr;
        let rel = |t: &i32| addr(*t as u32) as i32 - from as i32;
        let p = match p {
            Payload::PackedSwitch { first_key, targets } => {
                Payload::PackedSwitch { first_key, targets: targets.iter().map(rel).collect() }
            }
            Payload::SparseSwitch { keys, targets } => {
                Payload::SparseSwitch { keys, targets: targets.iter().map(rel).collect() }
            }
            p => p,
        };
        out.insns[k].offset = paddr as i32 - from as i32;
        payloads.push((paddr, p));
    }

    // ---- registers: parameters at the top of the frame
    let remap = |r: u32| if r < ins { total - ins + r } else { r - ins };
    for i in &mut out.insns {
        for r in &mut i.regs {
            *r = remap(*r as u32) as u16;
        }
    }

    let tries = tries(&ctx, &code, &addr);
    let debug = debug_info(&ctx, &code, m, &params, is_static, &addr, &remap);
    let units = end + payloads.len() as u32;
    Ok(CodeItem {
        registers: total as u16,
        ins: ins as u16,
        outs: 0,
        insns: vec![],
        tries,
        debug_off: 0,
        prebuilt: Some(Arc::new(Prebuilt { insns: out.insns, payloads, debug, units })),
    })
}

/// JVM exception table → Dalvik try items: non-overlapping ranges, each with the handlers that
/// apply there in table order. Handlers that can never fire (after one for the same type) are
/// kept: an outer `try`'s handlers stay a suffix of the inner one's list, which is how regions
/// nest.
fn tries(ctx: &Ctx<'_>, code: &Code, addr: &dyn Fn(u32) -> u32) -> Vec<TryItem> {
    let mut cuts: Vec<u32> = code.handlers.iter().flat_map(|h| [h.start, h.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<TryItem> = vec![];
    for w in cuts.windows(2) {
        let (s, e) = (addr(w[0]), addr(w[1]));
        if s >= e {
            continue;
        }
        let mut handlers: Vec<Handler> = vec![];
        for h in code.handlers.iter().filter(|h| h.start <= w[0] && w[1] <= h.end) {
            let ty = (h.catch != 0).then(|| ctx.unit.map[h.catch as usize]).filter(|&t| t != u32::MAX);
            if h.catch != 0 && ty.is_none() {
                continue;
            }
            handlers.push(Handler { ty, addr: addr(h.handler) });
        }
        if handlers.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut()
            && last.start + last.count == s
            && last.handlers.len() == handlers.len()
            && last.handlers.iter().zip(&handlers).all(|(a, b)| a.ty == b.ty && a.addr == b.addr)
        {
            last.count += e - s;
            continue;
        }
        out.push(TryItem { start: s, count: e - s, handlers });
    }
    out
}

fn debug_info(
    ctx: &Ctx<'_>,
    code: &Code,
    m: &Member,
    params: &[String],
    is_static: bool,
    addr: &dyn Fn(u32) -> u32,
    remap: &dyn Fn(u32) -> u32,
) -> DebugInfo {
    let cf = ctx.cf;
    let map = &ctx.unit.map;
    let string = |i: u16| map.get(i as usize).copied().filter(|&s| s != u32::MAX);
    let lvt = cf.locals(code, "LocalVariableTable");
    let lvtt = cf.locals(code, "LocalVariableTypeTable");
    let mut info = DebugInfo::default();
    let mut slot = u32::from(!is_static);
    let names = method_parameter_names(cf, m);
    for (k, p) in params.iter().enumerate() {
        let from_lvt =
            lvt.iter().find(|e| e.start == 0 && e.index as u32 == slot).and_then(|e| string(e.name));
        info.param_names.push(from_lvt.or_else(|| names.get(k).copied().flatten().and_then(string)));
        slot += size(p) as u32;
    }
    for e in &lvt {
        let (start, end) = (addr(e.start), addr(e.start.saturating_add(e.len)));
        if start >= end || e.index as u32 >= ctx.locals {
            continue;
        }
        let signature =
            lvtt.iter().find(|t| t.index == e.index && t.start == e.start).and_then(|t| string(t.desc));
        info.locals.push(LocalVar {
            reg: remap(e.index as u32) as u16,
            start,
            end,
            name: string(e.name),
            ty: ctx.classes.lookup.ty(cf.utf8(e.desc)),
            signature,
        });
    }
    info.locals.sort_by_key(|v| (v.start, v.reg));
    info.lines = cf.lines(code).into_iter().map(|(pc, line)| (addr(pc), line)).collect();
    info
}

fn method_parameter_names(cf: &ClassFile, m: &Member) -> Vec<Option<u16>> {
    let Some(d) = cf.attr(&m.attrs, "MethodParameters") else { return vec![] };
    let mut r = super::classfile::Be::new(d);
    let n = r.u8().unwrap_or(0);
    (0..n)
        .map_while(|_| {
            let name = r.u16().ok()?;
            r.u16().ok()?;
            Some((name != 0).then_some(name))
        })
        .collect()
}

struct Flow {
    falls: bool,
    targets: Vec<u32>,
    /// A `jsr` returns to the next instruction with the stack it had before the call.
    after_jsr: Option<Vec<V>>,
}

/// Applies one instruction to the stack shape and emits its register form.
fn step(ctx: &Ctx<'_>, j: &J, st: &mut Vec<V>, out: &mut Out) -> Result<Flow> {
    let next_pc = ctx.returns.iter().map(|r| r.1).find(|&pc| pc > j.pc);
    let cf = ctx.cf;
    let map = &ctx.unit.map;
    let g = &ctx.classes.globals;
    let d = words(st);
    let reg = |w: u32| ctx.locals + w;
    let local = |n: i32| n as u32;
    let s0 = ctx.scratch;
    let mut flow = Flow { falls: true, targets: vec![], after_jsr: None };
    let pop = |st: &mut Vec<V>, n: usize| -> Result<()> {
        if st.len() < n {
            bail!("stack underflow at {}", j.pc);
        }
        st.truncate(st.len() - n);
        Ok(())
    };
    // Word offset of the value `n` from the top (1 = top).
    let base = |st: &[V], n: usize| -> Result<u32> {
        if st.len() < n {
            bail!("stack underflow at {}", j.pc);
        }
        Ok(words(&st[..st.len() - n]))
    };
    let cp = |i: i32| map.get(i as usize).copied().unwrap_or(u32::MAX);
    let ty_of = |desc: &str| ctx.classes.lookup.ty(desc).unwrap_or(u32::MAX);
    match j.op {
        0x00 => {}
        0x01 => {
            out.lit(0x12, reg(d), 0);
            st.push(W1);
        }
        0x02..=0x08 => {
            out.lit(0x14, reg(d), j.op as i64 - 3);
            st.push(W1);
        }
        0x09 | 0x0a => {
            out.lit(0x18, reg(d), (j.op - 0x09) as i64);
            st.push(W2);
        }
        0x0b..=0x0d => {
            out.lit(0x14, reg(d), ((j.op - 0x0b) as f32).to_bits() as i32 as i64);
            st.push(W1);
        }
        0x0e | 0x0f => {
            out.lit(0x18, reg(d), ((j.op - 0x0e) as f64).to_bits() as i64);
            st.push(W2);
        }
        0x10 | 0x11 => {
            out.lit(0x14, reg(d), j.a as i64);
            st.push(W1);
        }
        0x12..=0x14 => match cf.get(j.a as u16) {
            Const::Int(v) => {
                out.lit(0x14, reg(d), *v as i64);
                st.push(W1);
            }
            Const::Float(b) => {
                out.lit(0x14, reg(d), *b as i32 as i64);
                st.push(W1);
            }
            Const::Long(v) => {
                out.lit(0x18, reg(d), *v);
                st.push(W2);
            }
            Const::Double(b) => {
                out.lit(0x18, reg(d), *b as i64);
                st.push(W2);
            }
            Const::String(_) => {
                out.idx(0x1a, vec![reg(d)], cp(j.a));
                st.push(W1);
            }
            Const::Class(_) => {
                out.idx(0x1c, vec![reg(d)], cp(j.a));
                st.push(W1);
            }
            Const::MethodType(_) => {
                out.idx(0xff, vec![reg(d)], cp(j.a));
                st.push(W1);
            }
            Const::MethodHandle(..) => {
                out.idx(0xfe, vec![reg(d)], j.a as u32);
                st.push(W1);
            }
            Const::Dynamic(_, nt) => {
                let (_, desc) = cf.name_and_type(*nt);
                if size(desc) == 2 {
                    out.lit(0x18, reg(d), 0);
                    st.push(W2);
                } else {
                    out.idx(0xfe, vec![reg(d)], j.a as u32);
                    st.push(W1);
                }
            }
            c => bail!("ldc of {c:?} at {}", j.pc),
        },
        0x15..=0x2d => {
            let (kind, n) = if j.op <= 0x19 {
                (j.op - 0x15, j.a)
            } else {
                ((j.op - 0x1a) / 4, ((j.op - 0x1a) % 4) as i32)
            };
            let w = if matches!(kind, 1 | 3) { 2 } else { 1 };
            let op = match kind {
                1 | 3 => 0x04,
                4 => 0x07,
                _ => 0x01,
            };
            out.push(op, vec![reg(d), local(n)]);
            st.push(V { w, new: None });
        }
        0x2e..=0x35 => {
            let k = (j.op - 0x2e) as usize;
            let op = [0x44, 0x45, 0x44, 0x45, 0x46, 0x48, 0x49, 0x4a][k];
            let w = [1, 2, 1, 2, 1, 1, 1, 1][k];
            let b = base(st, 2)?;
            out.push(op, vec![reg(b), reg(b), reg(b + 1)]);
            pop(st, 2)?;
            st.push(V { w, new: None });
        }
        0x36..=0x4e => {
            let (kind, n) = if j.op <= 0x3a {
                (j.op - 0x36, j.a)
            } else {
                ((j.op - 0x3b) / 4, ((j.op - 0x3b) % 4) as i32)
            };
            let op = match kind {
                1 | 3 => 0x04,
                4 => 0x07,
                _ => 0x01,
            };
            let b = base(st, 1)?;
            out.push(op, vec![local(n), reg(b)]);
            pop(st, 1)?;
        }
        0x4f..=0x56 => {
            let k = (j.op - 0x4f) as usize;
            let op = [0x4b, 0x4c, 0x4b, 0x4c, 0x4d, 0x4f, 0x50, 0x51][k];
            let b = base(st, 3)?;
            out.push(op, vec![reg(b + 2), reg(b), reg(b + 1)]);
            pop(st, 3)?;
        }
        0x57 => pop(st, 1)?,
        0x58 => {
            let top = st.last().map_or(1, |v| v.w);
            pop(st, if top == 2 { 1 } else { 2 })?;
        }
        0x59..=0x5f => shuffle(ctx, j, st, out)?,
        0x60..=0x73 => {
            let (t, gidx) = (((j.op - 0x60) % 4) as usize, (j.op - 0x60) / 4);
            let w = [1u8, 2, 1, 2][t];
            let b = base(st, 2)?;
            let op = 0x90 + [0u8, 11, 22, 27][t] + gidx;
            out.push(op, vec![reg(b), reg(b), reg(b + w as u32)]);
            pop(st, 2)?;
            st.push(V { w, new: None });
        }
        0x74..=0x77 => {
            let t = (j.op - 0x74) as usize;
            let b = base(st, 1)?;
            out.push([0x7b, 0x7d, 0x7f, 0x80][t], vec![reg(b), reg(b)]);
        }
        0x78..=0x83 => {
            let (k, idx) = if j.op <= 0x7d {
                let k = (j.op - 0x78) as usize;
                (k, [8u8, 19, 9, 20, 10, 21][k])
            } else {
                let k = (j.op - 0x7e) as usize;
                (k, [5u8, 16, 6, 17, 7, 18][k])
            };
            let w: u8 = if k % 2 == 1 { 2 } else { 1 };
            let b = base(st, 2)?;
            out.push(0x90 + idx, vec![reg(b), reg(b), reg(b + w as u32)]);
            pop(st, 2)?;
            st.push(V { w, new: None });
        }
        0x84 => {
            out.push(0xd0, vec![local(j.a), local(j.a)]).lit = j.b as i64;
        }
        0x85..=0x93 => {
            let k = (j.op - 0x85) as usize;
            let wout = [2u8, 1, 2, 1, 1, 2, 1, 2, 2, 1, 2, 1, 1, 1, 1][k];
            let b = base(st, 1)?;
            out.push(0x81 + k as u8, vec![reg(b), reg(b)]);
            pop(st, 1)?;
            st.push(V { w: wout, new: None });
        }
        0x94..=0x98 => {
            let (op, w) =
                [(0x31u8, 2u32), (0x2d, 1), (0x2e, 1), (0x2f, 2), (0x30, 2)][(j.op - 0x94) as usize];
            let b = base(st, 2)?;
            out.push(op, vec![reg(b), reg(b), reg(b + w)]);
            pop(st, 2)?;
            st.push(W1);
        }
        0x99..=0x9e | 0xc6 | 0xc7 => {
            let op = match j.op {
                0xc6 => 0x38,
                0xc7 => 0x39,
                o => 0x38 + (o - 0x99),
            };
            let b = base(st, 1)?;
            out.branch(op, vec![reg(b)], j.target);
            pop(st, 1)?;
            flow.targets.push(j.target);
        }
        0x9f..=0xa6 => {
            let op = if j.op <= 0xa4 { 0x32 + (j.op - 0x9f) } else { 0x32 + (j.op - 0xa5) };
            let b = base(st, 2)?;
            out.branch(op, vec![reg(b), reg(b + 1)], j.target);
            pop(st, 2)?;
            flow.targets.push(j.target);
        }
        0xa7 | 0xc8 => {
            out.branch(0x28, vec![], j.target);
            flow.falls = false;
            flow.targets.push(j.target);
        }
        0xa8 | 0xc9 => {
            // The return address is the call site's key; the subroutine stores it in a local.
            let key = ctx.returns.iter().find(|r| Some(r.1) == next_pc).map_or(0, |r| r.0);
            out.lit(0x14, reg(d), key as i64);
            out.branch(0x28, vec![], j.target);
            flow.after_jsr = Some(st.clone());
            st.push(W1);
            flow.falls = false;
            flow.targets.push(j.target);
        }
        0xa9 => {
            // `ret`: back to whichever call site's key the local holds.
            let Some(&(_, first)) = ctx.returns.first() else { bail!("ret without jsr at {}", j.pc) };
            let k = out.insns.len();
            out.push(0x2c, vec![local(j.a)]);
            let payload = Payload::SparseSwitch {
                keys: ctx.returns.iter().map(|r| r.0).collect(),
                targets: ctx.returns.iter().map(|r| r.1 as i32).collect(),
            };
            out.switches.push((k, payload));
            out.branch(0x28, vec![], first);
            flow.falls = false;
        }
        0xaa | 0xab => {
            let b = base(st, 1)?;
            let k = out.insns.len();
            let packed = j.op == 0xaa;
            out.push(if packed { 0x2b } else { 0x2c }, vec![reg(b)]);
            let targets: Vec<i32> = j.cases.iter().map(|c| c.1 as i32).collect();
            let payload = if packed {
                Payload::PackedSwitch { first_key: j.cases.first().map_or(0, |c| c.0), targets }
            } else {
                Payload::SparseSwitch { keys: j.cases.iter().map(|c| c.0).collect(), targets }
            };
            out.switches.push((k, payload));
            out.branch(0x28, vec![], j.target);
            pop(st, 1)?;
            flow.falls = false;
            flow.targets.push(j.target);
            flow.targets.extend(j.cases.iter().map(|c| c.1));
        }
        0xac..=0xb0 => {
            let k = (j.op - 0xac) as usize;
            let b = base(st, 1)?;
            out.push([0x0f, 0x10, 0x0f, 0x10, 0x11][k], vec![reg(b)]);
            flow.falls = false;
        }
        0xb1 => {
            out.push(0x0e, vec![]);
            flow.falls = false;
        }
        0xb2..=0xb5 => {
            let Some(r) = cf.member_ref(j.a as u16) else { bail!("bad field reference at {}", j.pc) };
            let w = size(r.desc).max(1);
            let k = match r.desc.as_bytes().first() {
                Some(b'J' | b'D') => 1,
                Some(b'L' | b'[') => 2,
                Some(b'Z') => 3,
                Some(b'B') => 4,
                Some(b'C') => 5,
                Some(b'S') => 6,
                _ => 0,
            };
            let f = cp(j.a);
            match j.op {
                0xb2 => {
                    out.idx(0x60 + k, vec![reg(d)], f);
                    st.push(V { w, new: None });
                }
                0xb3 => {
                    let b = base(st, 1)?;
                    out.idx(0x67 + k, vec![reg(b)], f);
                    pop(st, 1)?;
                }
                0xb4 => {
                    let b = base(st, 1)?;
                    out.idx(0x52 + k, vec![reg(b), reg(b)], f);
                    pop(st, 1)?;
                    st.push(V { w, new: None });
                }
                _ => {
                    let b = base(st, 2)?;
                    out.idx(0x59 + k, vec![reg(b + 1), reg(b)], f);
                    pop(st, 2)?;
                }
            }
        }
        0xb6..=0xb9 => {
            let Some(r) = cf.member_ref(j.a as u16) else { bail!("bad method reference at {}", j.pc) };
            let (params, ret) = types::split_method_desc(r.desc);
            let receiver = j.op != 0xb8;
            let n = params.len() + usize::from(receiver);
            let b = base(st, n)?;
            let mut regs: Vec<u32> = (b..d).map(reg).collect();
            let op = match j.op {
                0xb6 => 0x6e,
                0xb8 => 0x71,
                0xb9 => 0x72,
                _ if r.name == "<init>" || r.class == cf.this_name() => 0x70,
                _ => 0x6f,
            };
            if r.name == "<init>" {
                // Construct the allocation itself, not its `dup` copy.
                let ri = st.len() - n;
                if let Some(p) = st[ri].new {
                    if let Some(orig) = (0..ri).rev().find(|&i| st[i].new == Some(p)) {
                        regs[0] = reg(words(&st[..orig]));
                    }
                    for v in st.iter_mut() {
                        if v.new == Some(p) {
                            v.new = None;
                        }
                    }
                }
            }
            out.idx(op, regs, cp(j.a));
            pop(st, n)?;
            if ret != "V" {
                let op = match ret.as_bytes()[0] {
                    b'J' | b'D' => 0x0b,
                    b'L' | b'[' => 0x0c,
                    _ => 0x0a,
                };
                out.push(op, vec![reg(b)]);
                st.push(V { w: size(&ret), new: None });
            }
        }
        0xba => {
            let (_, desc) = match cf.get(j.a as u16) {
                Const::InvokeDynamic(_, nt) => cf.name_and_type(*nt),
                _ => bail!("bad invokedynamic at {}", j.pc),
            };
            let (params, ret) = types::split_method_desc(desc);
            let b = base(st, params.len())?;
            match ctx.unit.sites.get(&(j.a as u16)) {
                Some(Site::Concat(c)) => {
                    // A StringBuilder chain the shared passes turn back into `a + b`.
                    let mut offs = vec![];
                    let mut o = b;
                    for p in &c.args {
                        offs.push(o);
                        o += size(p) as u32;
                    }
                    let sb = s0;
                    let tmp = s0 + 1;
                    out.idx(0x22, vec![sb], g.string_builder);
                    out.idx(0x70, vec![sb], g.sb_init);
                    for piece in &c.pieces {
                        match piece {
                            Piece::Arg(k) => {
                                let t = c.args.get(*k).map_or("Ljava/lang/Object;", String::as_str);
                                let r = reg(offs.get(*k).copied().unwrap_or(b));
                                let key = match t {
                                    "B" | "S" | "I" => "I",
                                    "Z" | "C" | "J" | "F" | "D" | "Ljava/lang/String;" => t,
                                    _ => "Ljava/lang/Object;",
                                };
                                let mut regs = vec![sb, r];
                                if size(t) == 2 {
                                    regs.push(r + 1);
                                }
                                out.idx(0x6e, regs, g.sb_append[key]);
                            }
                            Piece::Const(s) => {
                                out.idx(0x1a, vec![tmp], *s);
                                out.idx(0x6e, vec![sb, tmp], g.sb_append["Ljava/lang/String;"]);
                            }
                        }
                    }
                    out.idx(0x6e, vec![sb], g.sb_to_string);
                    out.push(0x0c, vec![reg(b)]);
                }
                Some(Site::Call(idx)) => {
                    out.idx(0xfc, (b..d).map(reg).collect(), *idx);
                    if ret != "V" {
                        let op = match ret.as_bytes()[0] {
                            b'J' | b'D' => 0x0b,
                            b'L' | b'[' => 0x0c,
                            _ => 0x0a,
                        };
                        out.push(op, vec![reg(b)]);
                    }
                }
                None => bail!("unresolved invokedynamic at {}", j.pc),
            }
            pop(st, params.len())?;
            if ret != "V" {
                st.push(V { w: size(&ret), new: None });
            }
        }
        0xbb => {
            out.idx(0x22, vec![reg(d)], cp(j.a));
            st.push(V { w: 1, new: Some(j.pc) });
        }
        0xbc => {
            let t = match j.a {
                4..=11 => g.prim_arrays[(j.a - 4) as usize],
                a => bail!("bad newarray type {a} at {}", j.pc),
            };
            let b = base(st, 1)?;
            out.idx(0x23, vec![reg(b), reg(b)], t);
        }
        0xbd => {
            let b = base(st, 1)?;
            let elem = desc_of_internal(cf.class_name(j.a as u16));
            out.idx(0x23, vec![reg(b), reg(b)], ty_of(&format!("[{elem}")));
        }
        0xbe => {
            let b = base(st, 1)?;
            out.push(0x21, vec![reg(b), reg(b)]);
        }
        0xbf => {
            let b = base(st, 1)?;
            out.push(0x27, vec![reg(b)]);
            flow.falls = false;
        }
        0xc0 => {
            let b = base(st, 1)?;
            out.idx(0x1f, vec![reg(b)], cp(j.a));
        }
        0xc1 => {
            let b = base(st, 1)?;
            out.idx(0x20, vec![reg(b), reg(b)], cp(j.a));
        }
        0xc2 | 0xc3 => {
            let b = base(st, 1)?;
            out.push(if j.op == 0xc2 { 0x1d } else { 0x1e }, vec![reg(b)]);
            pop(st, 1)?;
        }
        0xc5 => {
            let dims = j.b.max(1) as usize;
            let b = base(st, dims)?;
            let desc = desc_of_internal(cf.class_name(j.a as u16));
            let component = desc.get(dims.min(desc.len())..).unwrap_or("I");
            out.idx(0x24, (b..d).map(reg).collect(), g.int_array);
            out.push(0x0c, vec![s0]);
            out.idx(0x1c, vec![s0 + 1], ty_of(component));
            out.idx(0x71, vec![s0 + 1, s0], g.array_new_instance);
            out.push(0x0c, vec![reg(b)]);
            out.idx(0x1f, vec![reg(b)], cp(j.a));
            pop(st, dims)?;
            st.push(W1);
        }
        o => bail!("unsupported opcode {o:#04x} at {}", j.pc),
    }
    Ok(flow)
}

/// `dup`, `dup_x1`, `dup_x2`, `dup2`, `dup2_x1`, `dup2_x2` and `swap` as register moves.
fn shuffle(ctx: &Ctx<'_>, j: &J, st: &mut Vec<V>, out: &mut Out) -> Result<()> {
    let n = st.len();
    let w = |k: usize| if k <= n && k > 0 { st[n - k].w } else { 0 };
    // `(values involved, new order as indices into them, 0 = deepest)`.
    let (k, order): (usize, &[usize]) = match (j.op, w(1), w(2), w(3)) {
        (0x59, 1, ..) => (1, &[0, 0]),
        (0x5a, 1, 1, _) => (2, &[1, 0, 1]),
        (0x5b, 1, 1, 1) => (3, &[2, 0, 1, 2]),
        (0x5b, 1, 2, _) => (2, &[1, 0, 1]),
        (0x5c, 1, 1, _) => (2, &[0, 1, 0, 1]),
        (0x5c, 2, ..) => (1, &[0, 0]),
        (0x5d, 1, 1, 1) => (3, &[1, 2, 0, 1, 2]),
        (0x5d, 2, 1, _) => (2, &[1, 0, 1]),
        (0x5e, 1, 1, 1) if w(4) == 1 => (4, &[2, 3, 0, 1, 2, 3]),
        (0x5e, 2, 1, 1) => (3, &[2, 0, 1, 2]),
        (0x5e, 1, 1, 2) => (3, &[1, 2, 0, 1, 2]),
        (0x5e, 2, 2, _) => (2, &[1, 0, 1]),
        (0x5f, 1, 1, _) => (2, &[1, 0]),
        _ => bail!("bad stack shape for opcode {:#04x} at {}", j.op, j.pc),
    };
    let old: Vec<V> = st[n - k..].to_vec();
    let start = words(&st[..n - k]);
    let mut old_at = vec![];
    let mut o = start;
    for v in &old {
        old_at.push(o);
        o += v.w as u32;
    }
    // A plain `dup` of a fresh allocation: the copy is only the `<init>` receiver, which is
    // redirected to the original, so no move is needed.
    let skip_copy = j.op == 0x59 && old[0].new.is_some();
    let mut moves = vec![];
    let mut at = start;
    for &i in order {
        if !skip_copy || at == old_at[i] {
            moves.push((ctx.locals + at, ctx.locals + old_at[i], old[i].w));
        }
        at += old[i].w as u32;
    }
    parallel_move(out, &moves, ctx.scratch + 2);
    st.truncate(n - k);
    st.extend(order.iter().map(|&i| old[i]));
    Ok(())
}
