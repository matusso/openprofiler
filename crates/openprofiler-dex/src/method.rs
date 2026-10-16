//! Lifts a method's Dalvik bytecode to IR: basic blocks, variables recovered from register webs,
//! expressions, inferred types, inlined temporaries and merged short-circuit conditions.

use crate::dex::{ACC_STATIC, CodeItem, DebugInfo, DexFile};
use crate::insn::{self, Insn, Payload};
use crate::ir::*;
use crate::types;
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Arc;

/// Methods larger than this (code units) are not decompiled.
pub const MAX_CODE_UNITS: usize = 1 << 16;

#[derive(Debug, Clone)]
pub struct TryRegion {
    /// Blocks covered by this region or a region nested in it.
    pub blocks: Vec<BlockId>,
    /// This region's own `(catch type, handler block)`s; `None` = catch-all.
    pub handlers: Vec<(Option<Arc<str>>, BlockId)>,
    /// Enclosing region: its handlers are a suffix of this region's dex handler list.
    pub parent: Option<usize>,
    pub depth: usize,
}

pub struct Body {
    pub blocks: Vec<Block>,
    pub entry: BlockId,
    pub vars: Vec<VarInfo>,
    /// Parameter variables in declaration order (excluding `this`).
    pub params: Vec<VarId>,
    pub this: Option<VarId>,
    pub tries: Vec<TryRegion>,
    pub ret: String,
}

pub struct MethodInput<'a> {
    pub dex: &'a DexFile,
    pub class: &'a str,
    pub method: u32,
    pub access: u32,
    pub code: &'a CodeItem,
    pub debug: &'a DebugInfo,
}

#[derive(Debug, Clone, Copy)]
struct Def {
    reg: u16,
    wide: bool,
}

type Uses = Vec<(u16, bool)>;

pub fn field_ref(dex: &DexFile, idx: u32) -> FieldRef {
    match dex.field(idx) {
        Some(f) => FieldRef {
            class: dex.type_desc(f.class).into(),
            name: dex.string(f.name).into(),
            ty: dex.type_desc(f.ty).into(),
        },
        None => FieldRef {
            class: "Ljava/lang/Object;".into(),
            name: format!("field_{idx}").into(),
            ty: "I".into(),
        },
    }
}

pub fn method_ref(dex: &DexFile, idx: u32) -> MethodRef {
    match dex.method(idx) {
        Some(m) => {
            let p = dex.proto(m.proto);
            MethodRef {
                class: dex.type_desc(m.class).into(),
                name: dex.string(m.name).into(),
                params: p
                    .map_or_else(Vec::new, |p| {
                        p.params.iter().map(|&t| dex.type_desc(t).to_owned()).collect()
                    })
                    .into(),
                ret: p.map_or("V", |p| dex.type_desc(p.ret)).into(),
            }
        }
        None => MethodRef {
            class: "Ljava/lang/Object;".into(),
            name: format!("method_{idx}").into(),
            params: Vec::new().into(),
            ret: "V".into(),
        },
    }
}

/// Registers defined and used by an instruction (`(reg, wide)`), uses in operand order.
fn def_use(dex: &DexFile, i: &Insn) -> (Option<Def>, Uses) {
    let r = &i.regs;
    let reg = |k: usize| r.get(k).copied().unwrap_or(0);
    let d = |wide: bool| Some(Def { reg: reg(0), wide });
    let n = |k: usize| (reg(k), false);
    let w = |k: usize| (reg(k), true);
    match i.op {
        0x01 | 0x02 | 0x03 | 0x07 | 0x08 | 0x09 => (d(false), vec![n(1)]),
        0x04..=0x06 => (d(true), vec![w(1)]),
        0x0a | 0x0c | 0x0d => (d(false), vec![]),
        0x0b => (d(true), vec![]),
        0x0f | 0x11 => (None, vec![n(0)]),
        0x10 => (None, vec![w(0)]),
        0x12..=0x15 | 0x1a..=0x1c | 0x22 | 0xfe | 0xff => (d(false), vec![]),
        0x16..=0x19 => (d(true), vec![]),
        0x1d | 0x1e | 0x26 | 0x27 | 0x2b | 0x2c => (None, vec![n(0)]),
        0x1f => (d(false), vec![n(0)]),
        0x20 | 0x21 | 0x23 => (d(false), vec![n(1)]),
        0x24 | 0x25 => (None, r.iter().map(|&x| (x, false)).collect()),
        0x2d | 0x2e => (d(false), vec![n(1), n(2)]),
        0x2f..=0x31 => (d(false), vec![w(1), w(2)]),
        0x32..=0x37 => (None, vec![n(0), n(1)]),
        0x38..=0x3d => (None, vec![n(0)]),
        0x45 => (d(true), vec![n(1), n(2)]),
        0x44 | 0x46..=0x4a => (d(false), vec![n(1), n(2)]),
        0x4c => (None, vec![w(0), n(1), n(2)]),
        0x4b | 0x4d..=0x51 => (None, vec![n(0), n(1), n(2)]),
        0x53 => (d(true), vec![n(1)]),
        0x52 | 0x54..=0x58 => (d(false), vec![n(1)]),
        0x5a => (None, vec![w(0), n(1)]),
        0x59 | 0x5b..=0x5f => (None, vec![n(0), n(1)]),
        0x61 => (d(true), vec![]),
        0x60 | 0x62..=0x66 => (d(false), vec![]),
        0x68 => (None, vec![w(0)]),
        0x67 | 0x69..=0x6d => (None, vec![n(0)]),
        0x6e..=0x72 | 0x74..=0x78 | 0xfa | 0xfb => {
            let is_static = i.op == 0x71 || i.op == 0x76;
            let params = if matches!(i.op, 0xfa | 0xfb) {
                dex.proto(i.index2).map(|p| p.params.clone()).unwrap_or_default()
            } else {
                dex.method(i.index)
                    .and_then(|m| dex.proto(m.proto))
                    .map(|p| p.params.clone())
                    .unwrap_or_default()
            };
            let mut uses = vec![];
            let mut k = 0;
            if !is_static {
                uses.push(n(0));
                k = 1;
            }
            for p in params {
                if k >= r.len() {
                    break;
                }
                let wide = types::is_wide(dex.type_desc(p));
                uses.push((r[k], wide));
                k += if wide { 2 } else { 1 };
            }
            (None, uses)
        }
        0xfc | 0xfd => (None, r.iter().map(|&x| (x, false)).collect()),
        0x7b..=0x8f => {
            // (def wide, use wide) per unary op.
            let (dw, uw) = match i.op {
                0x7d | 0x7e | 0x80 | 0x86 | 0x8b => (true, true),
                0x81 | 0x83 | 0x88 | 0x89 => (true, false),
                0x84 | 0x85 | 0x8a | 0x8c => (false, true),
                _ => (false, false),
            };
            (d(dw), vec![(reg(1), uw)])
        }
        0x90..=0xaf => {
            let k = i.op - 0x90;
            let wide = (11..22).contains(&k) || k >= 27;
            let shift_long = (19..22).contains(&k);
            (d(wide), vec![(reg(1), wide), (reg(2), wide && !shift_long)])
        }
        0xb0..=0xcf => {
            let k = i.op - 0xb0;
            let wide = (11..22).contains(&k) || k >= 27;
            let shift_long = (19..22).contains(&k);
            (d(wide), vec![(reg(0), wide), (reg(1), wide && !shift_long)])
        }
        0xd0..=0xe2 => (d(false), vec![n(1)]),
        _ => (None, vec![]),
    }
}

/// Binary operator, operand type and whether it's the 2addr/literal form.
fn binop_of(op: u8) -> Option<(BinOp, &'static str)> {
    use BinOp::*;
    const INT: [BinOp; 11] = [Add, Sub, Mul, Div, Rem, And, Or, Xor, Shl, Shr, Ushr];
    const FLT: [BinOp; 5] = [Add, Sub, Mul, Div, Rem];
    let k = match op {
        0x90..=0xaf => op - 0x90,
        0xb0..=0xcf => op - 0xb0,
        _ => return None,
    } as usize;
    Some(match k {
        0..=10 => (INT[k], "I"),
        11..=21 => (INT[k - 11], "J"),
        22..=26 => (FLT[k - 22], "F"),
        _ => (FLT[k - 27], "D"),
    })
}

fn lit_binop(op: u8) -> Option<BinOp> {
    use BinOp::*;
    Some(match op {
        0xd0 | 0xd8 => Add,
        0xd1 | 0xd9 => Sub, // rsub
        0xd2 | 0xda => Mul,
        0xd3 | 0xdb => Div,
        0xd4 | 0xdc => Rem,
        0xd5 | 0xdd => And,
        0xd6 | 0xde => Or,
        0xd7 | 0xdf => Xor,
        0xe0 => Shl,
        0xe1 => Shr,
        0xe2 => Ushr,
        _ => return None,
    })
}

fn array_kind(op: u8) -> &'static str {
    match (op - 0x44) % 7 {
        0 => "I",
        1 => "J",
        2 => "L",
        3 => "Z",
        4 => "B",
        5 => "C",
        _ => "S",
    }
}

struct BitSet(Vec<u64>);

impl BitSet {
    fn new(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }
    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
    fn union_with(&mut self, o: &BitSet) -> bool {
        let mut changed = false;
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            let n = *a | b;
            changed |= n != *a;
            *a = n;
        }
        changed
    }
    fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.0
            .iter()
            .enumerate()
            .flat_map(|(w, &bits)| (0..64).filter(move |b| bits & (1 << b) != 0).map(move |b| w * 64 + b))
    }
}

struct UnionFind(Vec<u32>);

impl UnionFind {
    fn find(&mut self, x: u32) -> u32 {
        let mut r = x;
        while self.0[r as usize] != r {
            r = self.0[r as usize];
        }
        let mut x = x;
        while self.0[x as usize] != r {
            let n = self.0[x as usize];
            self.0[x as usize] = r;
            x = n;
        }
        r
    }
    fn union(&mut self, a: u32, b: u32) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a.max(b) as usize] = a.min(b);
        }
    }
}

// Blocks index several parallel tables (successors, reaching sets, ...).
#[allow(clippy::needless_range_loop)]
pub fn lift(m: &MethodInput<'_>) -> Result<Body> {
    let dex = m.dex;
    if m.code.insns.len() > MAX_CODE_UNITS {
        bail!("method too large ({} code units)", m.code.insns.len());
    }
    let (insns, payloads) = insn::decode_all(&m.code.insns)?;
    if insns.is_empty() {
        bail!("empty method body");
    }
    let payload_at: HashMap<u32, &Payload> = payloads.iter().map(|(a, p)| (*a, p)).collect();
    let mut index_of = vec![u32::MAX; m.code.insns.len() + 1];
    for (k, i) in insns.iter().enumerate() {
        index_of[i.addr as usize] = k as u32;
    }
    let at = |addr: u32| -> Option<usize> {
        index_of.get(addr as usize).copied().filter(|&k| k != u32::MAX).map(|k| k as usize)
    };
    let next_addr = |k: usize| insns.get(k + 1).map_or(m.code.insns.len() as u32, |i| i.addr);

    // ---- basic blocks
    let mut leader = vec![false; insns.len()];
    leader[0] = true;
    let mark = |leader: &mut Vec<bool>, addr: u32| {
        if let Some(k) = at(addr) {
            leader[k] = true;
        }
    };
    for (k, i) in insns.iter().enumerate() {
        if insn::is_branch(i.op) {
            if matches!(i.op, 0x2b | 0x2c) {
                match payload_at.get(&i.target()) {
                    Some(Payload::PackedSwitch { targets, .. } | Payload::SparseSwitch { targets, .. }) => {
                        for &t in targets {
                            mark(&mut leader, i.addr.wrapping_add_signed(t));
                        }
                    }
                    _ => bail!("switch at {:#x} has no payload", i.addr),
                }
            } else {
                mark(&mut leader, i.target());
            }
        }
        if (insn::is_branch(i.op) || insn::ends_flow(i.op)) && k + 1 < insns.len() {
            leader[k + 1] = true;
        }
    }
    for t in &m.code.tries {
        mark(&mut leader, t.start);
        mark(&mut leader, t.start.saturating_add(t.count));
        for h in &t.handlers {
            mark(&mut leader, h.addr);
        }
    }
    // An invoke's move-result stays in its block.
    for (k, i) in insns.iter().enumerate() {
        if matches!(i.op, 0x0a..=0x0c) {
            leader[k] = false;
        }
    }
    leader[0] = true;
    let starts: Vec<usize> = (0..insns.len()).filter(|&k| leader[k]).collect();
    let mut block_of = vec![0usize; insns.len()];
    for (b, w) in starts.iter().enumerate() {
        let end = starts.get(b + 1).copied().unwrap_or(insns.len());
        for x in &mut block_of[*w..end] {
            *x = b;
        }
    }
    let block_at = |addr: u32| -> Result<BlockId> {
        match at(addr) {
            Some(k) => Ok(block_of[k]),
            None => bail!("branch to {addr:#x} is not an instruction"),
        }
    };
    let n_blocks = starts.len();
    let block_range = |b: usize| starts[b]..starts.get(b + 1).copied().unwrap_or(insns.len());

    // Try regions: try items with identical handler lists form one region.
    let mut regions: Vec<(Vec<(Option<u32>, u32)>, TryRegion)> = vec![];
    let mut block_handlers: Vec<Vec<(BlockId, Option<Arc<str>>)>> = vec![vec![]; n_blocks];
    let mut block_region = vec![None; n_blocks];
    for t in &m.code.tries {
        let key: Vec<(Option<u32>, u32)> = t.handlers.iter().map(|h| (h.ty, h.addr)).collect();
        let ri = match regions.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let handlers = t
                    .handlers
                    .iter()
                    .map(|h| Ok((h.ty.map(|ty| Arc::from(dex.type_desc(ty))), block_at(h.addr)?)))
                    .collect::<Result<Vec<_>>>()?;
                regions.push((key, TryRegion { blocks: vec![], handlers, parent: None, depth: 0 }));
                regions.len() - 1
            }
        };
        let end = t.start.saturating_add(t.count);
        for b in 0..n_blocks {
            let a = insns[starts[b]].addr;
            if a >= t.start && a < end {
                // A block belongs to the innermost (first listed) region covering it.
                if block_region[b].is_none() {
                    block_region[b] = Some(ri);
                    regions[ri].1.blocks.push(b);
                    // Only instructions that can throw reach a handler.
                    if block_range(b).any(|k| insn::can_throw(insns[k].op)) {
                        block_handlers[b] =
                            regions[ri].1.handlers.iter().map(|(t, h)| (*h, t.clone())).collect();
                    }
                }
            }
        }
    }
    let tries = nest_regions(regions);

    // ---- registers → variables (webs of reaching definitions)
    let du: Vec<(Option<Def>, Uses)> = insns.iter().map(|i| def_use(dex, i)).collect();
    let regs = m.code.registers as usize;
    let is_static = m.access & ACC_STATIC != 0;
    let proto = dex.method(m.method).and_then(|mi| dex.proto(mi.proto));
    let param_types: Vec<String> =
        proto.map_or_else(Vec::new, |p| p.params.iter().map(|&t| dex.type_desc(t).to_owned()).collect());
    let ret = proto.map_or("V", |p| dex.type_desc(p.ret)).to_owned();
    // Definitions: parameters first, then instructions.
    let mut defs: Vec<Def> = vec![];
    let mut def_site: Vec<Option<usize>> = vec![];
    let mut param_defs = vec![];
    let mut preg = regs.saturating_sub(m.code.ins as usize) as u16;
    let this_def = if !is_static {
        defs.push(Def { reg: preg, wide: false });
        def_site.push(None);
        preg += 1;
        Some(0usize)
    } else {
        None
    };
    for t in &param_types {
        let wide = types::is_wide(t);
        param_defs.push(defs.len());
        defs.push(Def { reg: preg, wide });
        def_site.push(None);
        preg += if wide { 2 } else { 1 };
    }
    let mut insn_def = vec![usize::MAX; insns.len()];
    for (k, (d, _)) in du.iter().enumerate() {
        if let Some(d) = d {
            insn_def[k] = defs.len();
            defs.push(*d);
            def_site.push(Some(k));
        }
    }
    let n_defs = defs.len();
    if regs > 65536 {
        bail!("too many registers");
    }

    // Successors (normal) and exceptional predecessors per block.
    let mut succs: Vec<Vec<BlockId>> = vec![vec![]; n_blocks];
    for b in 0..n_blocks {
        let last = block_range(b).end - 1;
        let i = &insns[last];
        let fall = (last + 1 < insns.len()).then(|| block_of[last + 1]);
        let s = &mut succs[b];
        match i.op {
            0x28..=0x2a => s.push(block_at(i.target())?),
            0x32..=0x3d => {
                s.push(block_at(i.target())?);
                s.extend(fall);
            }
            0x2b | 0x2c => {
                if let Some(Payload::PackedSwitch { targets, .. } | Payload::SparseSwitch { targets, .. }) =
                    payload_at.get(&i.target())
                {
                    for &t in targets {
                        s.push(block_at(i.addr.wrapping_add_signed(t))?);
                    }
                }
                s.extend(fall);
            }
            op if insn::ends_flow(op) => {}
            _ => s.extend(fall),
        }
    }
    let mut preds: Vec<Vec<BlockId>> = vec![vec![]; n_blocks];
    let mut exc_preds: Vec<Vec<BlockId>> = vec![vec![]; n_blocks];
    for b in 0..n_blocks {
        for &s in &succs[b] {
            if !preds[s].contains(&b) {
                preds[s].push(b);
            }
        }
        for (h, _) in &block_handlers[b] {
            if !exc_preds[*h].contains(&b) {
                exc_preds[*h].push(b);
            }
        }
    }

    // Reaching definitions, per register.
    let mut reg_defs: Vec<Vec<usize>> = vec![vec![]; regs + 1];
    for (k, d) in defs.iter().enumerate() {
        if (d.reg as usize) < regs {
            reg_defs[d.reg as usize].push(k);
        }
    }
    let mut gen_sets = vec![];
    let mut kill_regs: Vec<Vec<u16>> = vec![];
    let mut all_defs = vec![];
    for b in 0..n_blocks {
        let mut last: HashMap<u16, usize> = HashMap::new();
        let mut killed = vec![];
        let mut all = BitSet::new(n_defs);
        for k in block_range(b) {
            if let Some(d) = du[k].0 {
                let id = insn_def[k];
                last.insert(d.reg, id);
                killed.push(d.reg);
                all.set(id);
                if d.wide {
                    last.remove(&(d.reg + 1));
                    killed.push(d.reg + 1);
                }
                // A narrow write over the high half of a wide value kills it.
                if d.reg > 0 && last.get(&(d.reg - 1)).is_some_and(|&p| defs[p].wide) {
                    last.remove(&(d.reg - 1));
                }
            }
        }
        let mut g = BitSet::new(n_defs);
        for &id in last.values() {
            g.set(id);
        }
        gen_sets.push(g);
        killed.sort_unstable();
        killed.dedup();
        kill_regs.push(killed);
        all_defs.push(all);
    }
    let mut kill_masks = vec![];
    for killed in &kill_regs {
        let mut mask = BitSet::new(n_defs);
        for &r in killed {
            for &d in reg_defs.get(r as usize).into_iter().flatten() {
                mask.set(d);
            }
            if r > 0 {
                for &d in reg_defs.get(r as usize - 1).into_iter().flatten() {
                    if defs[d].wide {
                        mask.set(d);
                    }
                }
            }
        }
        kill_masks.push(mask);
    }
    let mut ins: Vec<BitSet> = (0..n_blocks).map(|_| BitSet::new(n_defs)).collect();
    let mut outs: Vec<BitSet> = (0..n_blocks).map(|_| BitSet::new(n_defs)).collect();
    {
        let mut entry = BitSet::new(n_defs);
        for (k, site) in def_site.iter().enumerate() {
            if site.is_none() {
                entry.set(k);
            }
        }
        ins[0] = entry;
    }
    let mut changed = true;
    let mut rounds = 0;
    while changed {
        changed = false;
        rounds += 1;
        if rounds > 10_000 {
            bail!("reaching definitions did not converge");
        }
        for b in 0..n_blocks {
            let mut inb = BitSet::new(n_defs);
            if b == 0 {
                inb.union_with(&ins[0]);
            }
            for &p in &preds[b] {
                inb.union_with(&outs[p]);
            }
            for &p in &exc_preds[b] {
                inb.union_with(&ins[p]);
                inb.union_with(&all_defs[p]);
            }
            let mut out = BitSet(inb.0.iter().zip(&kill_masks[b].0).map(|(a, k)| a & !k).collect());
            out.union_with(&gen_sets[b]);
            if out.0 != outs[b].0 {
                outs[b] = out;
                changed = true;
            }
            ins[b] = inb;
        }
    }

    // Walk each block: every use unions the definitions that reach it.
    let mut uf = UnionFind((0..n_defs as u32).collect());
    let mut use_rep: Vec<Vec<u32>> = vec![vec![]; insns.len()];
    // Uses that read the same constant on every path become that constant: d8 shares constant
    // registers between unrelated values (`0` as int, `false` and `null`).
    let mut use_const: Vec<Vec<Option<Expr>>> = vec![vec![]; insns.len()];
    let def_const: Vec<Option<Expr>> = def_site
        .iter()
        .map(|site| {
            let i = &insns[(*site)?];
            match i.op {
                0x12..=0x15 => Some(Expr::lit(i.lit)),
                0x16..=0x19 => Some(Expr::Lit { bits: i.lit, wide: true }),
                0x1a | 0x1b => Some(Expr::Str(dex.string(i.index).into())),
                _ => None,
            }
        })
        .collect();
    let named_local =
        |reg: u16, addr: u32| m.debug.locals.iter().any(|l| l.reg == reg && (l.start..l.end).contains(&addr));
    let mut orphan_defs: Vec<Def> = vec![];
    for b in 0..n_blocks {
        let mut cur: HashMap<u16, Vec<usize>> = HashMap::new();
        for d in ins[b].ones() {
            cur.entry(defs[d].reg).or_default().push(d);
        }
        for k in block_range(b) {
            for &(reg, _) in &du[k].1 {
                let reaching = cur.get(&reg).cloned().unwrap_or_default();
                let constant = reaching.first().and_then(|&f| def_const[f].clone()).filter(|c| {
                    reaching.iter().all(|&d| def_const[d].as_ref() == Some(c))
                        && !named_local(reg, insns[k].addr)
                });
                if let Some(c) = constant {
                    use_const[k].push(Some(c));
                    use_rep[k].push(reaching[0] as u32);
                    continue;
                }
                use_const[k].push(None);
                let rep = match reaching.first() {
                    Some(&f) => {
                        for &o in &reaching[1..] {
                            uf.union(f as u32, o as u32);
                        }
                        f as u32
                    }
                    None => {
                        // Read of an undefined register (invalid but seen in obfuscated code).
                        orphan_defs.push(Def { reg, wide: false });
                        (n_defs + orphan_defs.len() - 1) as u32
                    }
                };
                use_rep[k].push(rep);
            }
            if let Some(d) = du[k].0 {
                let id = insn_def[k];
                cur.insert(d.reg, vec![id]);
                if d.wide {
                    cur.remove(&(d.reg + 1));
                }
                if d.reg > 0
                    && let Some(v) = cur.get_mut(&(d.reg - 1))
                {
                    v.retain(|&p| !defs[p].wide);
                }
            }
        }
    }
    // Definitions and uses inside one debug-info local's range are the same source variable.
    for l in &m.debug.locals {
        let mut first: Option<u32> = None;
        let mut join = |x: u32, uf: &mut UnionFind| match first {
            Some(f) => uf.union(f, x),
            None => first = Some(x),
        };
        for (k, i) in insns.iter().enumerate() {
            if let Some(d) = du[k].0
                && d.reg == l.reg
                && (l.start..l.end.max(l.start + 1)).contains(&next_addr(k))
            {
                join(insn_def[k] as u32, &mut uf);
            }
            if (l.start..l.end).contains(&i.addr) {
                for (u, &(reg, _)) in du[k].1.iter().enumerate() {
                    if reg == l.reg && (use_rep[k][u] as usize) < n_defs {
                        join(use_rep[k][u], &mut uf);
                    }
                }
            }
        }
    }
    let mut var_of_root: HashMap<u32, VarId> = HashMap::new();
    let mut vars: Vec<VarInfo> = vec![];
    let mut var_of = |rep: u32, uf: &mut UnionFind, vars: &mut Vec<VarInfo>| -> VarId {
        let root = if (rep as usize) < n_defs { uf.find(rep) } else { rep };
        *var_of_root.entry(root).or_insert_with(|| {
            let reg = if (root as usize) < n_defs {
                defs[root as usize].reg
            } else {
                orphan_defs[root as usize - n_defs].reg
            };
            vars.push(VarInfo { reg, ..Default::default() });
            (vars.len() - 1) as VarId
        })
    };
    let this = this_def.map(|d| var_of(d as u32, &mut uf, &mut vars));
    if let Some(t) = this {
        vars[t as usize].is_this = true;
        vars[t as usize].ty = Some(m.class.to_owned());
        vars[t as usize].name = Some("this".into());
    }
    let mut params = vec![];
    for (pi, &d) in param_defs.iter().enumerate() {
        let v = var_of(d as u32, &mut uf, &mut vars);
        let vi = &mut vars[v as usize];
        vi.param = Some(pi);
        vi.ty = Some(param_types[pi].clone());
        if let Some(Some(name)) = m.debug.param_names.get(pi) {
            vi.name = Some(dex.string(*name).to_owned());
        }
        params.push(v);
    }
    let def_var: Vec<Option<VarId>> = (0..insns.len())
        .map(|k| (insn_def[k] != usize::MAX).then(|| var_of(insn_def[k] as u32, &mut uf, &mut vars)))
        .collect();
    let use_var: Vec<Vec<VarId>> =
        use_rep.iter().map(|reps| reps.iter().map(|&r| var_of(r, &mut uf, &mut vars)).collect()).collect();

    // Debug-info names and types.
    for l in &m.debug.locals {
        let name = l.name.map(|n| dex.string(n).to_owned());
        let mut apply = |v: VarId| {
            let vi = &mut vars[v as usize];
            if vi.is_this || (vi.param.is_some() && vi.name.is_some()) {
                return;
            }
            if vi.name.is_none() {
                vi.name.clone_from(&name);
            }
            if let Some(t) = l.ty {
                vi.ty.get_or_insert_with(|| dex.type_desc(t).to_owned());
            }
            if let Some(s) = l.signature {
                vi.signature.get_or_insert_with(|| dex.string(s).to_owned());
            }
        };
        for (k, i) in insns.iter().enumerate() {
            if du[k].0.is_some_and(|d| d.reg == l.reg) {
                let na = next_addr(k);
                if l.start <= na
                    && na < l.end.max(l.start + 1)
                    && i.addr < l.end
                    && let Some(v) = def_var[k]
                {
                    apply(v);
                }
            }
            if i.addr >= l.start && i.addr < l.end {
                for (u, &(reg, _)) in du[k].1.iter().enumerate() {
                    if reg == l.reg {
                        apply(use_var[k][u]);
                    }
                }
            }
        }
    }

    // ---- IR
    let mut blocks = Vec::with_capacity(n_blocks);
    for b in 0..n_blocks {
        let range = block_range(b);
        let mut stmts = vec![];
        let mut term = None;
        let mut pending: Option<Expr> = None;
        for k in range.clone() {
            let i = &insns[k];
            let u = |n: usize| match use_const[k].get(n) {
                Some(Some(c)) => c.clone(),
                _ => Expr::Var(use_var[k].get(n).copied().unwrap_or(0)),
            };
            let dv = def_var[k].unwrap_or(0);
            if !matches!(i.op, 0x0a..=0x0c)
                && let Some(p) = pending.take()
                && matches!(p, Expr::Invoke(..))
            {
                stmts.push(Stmt::Expr(p));
            }
            let fall = || block_of.get(k + 1).copied().unwrap_or(b);
            match i.op {
                0x00 => {}
                0x01..=0x09 => stmts.push(Stmt::Assign(dv, u(0))),
                0x0a..=0x0c => {
                    let e = pending.take().unwrap_or_else(|| Expr::Raw("/* missing result */ null".into()));
                    stmts.push(Stmt::Assign(dv, e));
                }
                0x0d => {
                    let ty =
                        tries.iter().find_map(|t| t.handlers.iter().find(|h| h.1 == b).map(|h| h.0.clone()));
                    stmts.push(Stmt::Assign(dv, Expr::CaughtException(ty.flatten())));
                }
                0x0e => term = Some(Term::Return(None)),
                0x0f..=0x11 => term = Some(Term::Return(Some(u(0)))),
                0x12..=0x15 => stmts.push(Stmt::Assign(dv, Expr::lit(i.lit))),
                0x16..=0x19 => stmts.push(Stmt::Assign(dv, Expr::Lit { bits: i.lit, wide: true })),
                0x1a | 0x1b => stmts.push(Stmt::Assign(dv, Expr::Str(dex.string(i.index).into()))),
                0x1c => stmts.push(Stmt::Assign(dv, Expr::Class(dex.type_desc(i.index).into()))),
                0x1d => stmts.push(Stmt::MonitorEnter(u(0))),
                0x1e => stmts.push(Stmt::MonitorExit(u(0))),
                0x1f => {
                    stmts.push(Stmt::Assign(dv, Expr::Cast(dex.type_desc(i.index).into(), Box::new(u(0)))))
                }
                0x20 => stmts
                    .push(Stmt::Assign(dv, Expr::InstanceOf(Box::new(u(0)), dex.type_desc(i.index).into()))),
                0x21 => stmts.push(Stmt::Assign(dv, Expr::ArrayLen(Box::new(u(0))))),
                0x22 => stmts.push(Stmt::Assign(dv, new_placeholder(dex.type_desc(i.index)))),
                0x23 => {
                    let ty = dex.type_desc(i.index);
                    stmts.push(Stmt::Assign(dv, Expr::NewArray(ty.into(), Box::new(u(0)))));
                }
                0x24 | 0x25 => {
                    let args = (0..use_var[k].len()).map(u).collect();
                    pending = Some(Expr::ArrayLit(dex.type_desc(i.index).into(), args));
                }
                0x26 => fill_array(&mut stmts, u(0), use_var[k][0], payload_at.get(&i.target()).copied()),
                0x27 => term = Some(Term::Throw(u(0))),
                0x28..=0x2a => term = Some(Term::Goto(block_at(i.target())?)),
                0x2b | 0x2c => {
                    let mut cases = vec![];
                    match payload_at.get(&i.target()) {
                        Some(Payload::PackedSwitch { first_key, targets }) => {
                            for (n, &t) in targets.iter().enumerate() {
                                cases.push((
                                    first_key.wrapping_add(n as i32),
                                    block_at(i.addr.wrapping_add_signed(t))?,
                                ));
                            }
                        }
                        Some(Payload::SparseSwitch { keys, targets }) => {
                            for (&key, &t) in keys.iter().zip(targets) {
                                cases.push((key, block_at(i.addr.wrapping_add_signed(t))?));
                            }
                        }
                        _ => bail!("bad switch payload"),
                    }
                    term = Some(Term::Switch { value: u(0), cases, default: fall() });
                }
                0x2d..=0x31 => {
                    let name = ["cmpl-float", "cmpg-float", "cmpl-double", "cmpg-double", "cmp-long"]
                        [(i.op - 0x2d) as usize];
                    stmts.push(Stmt::Assign(dv, Expr::Cmp(name, Box::new(u(0)), Box::new(u(1)))));
                }
                0x32..=0x3d => {
                    let ops = [BinOp::Eq, BinOp::Ne, BinOp::Lt, BinOp::Ge, BinOp::Gt, BinOp::Le];
                    let (op, rhs) = if i.op <= 0x37 {
                        (ops[(i.op - 0x32) as usize], u(1))
                    } else {
                        (ops[(i.op - 0x38) as usize], Expr::lit(0))
                    };
                    term = Some(Term::If {
                        cond: Expr::bin(op, u(0), rhs),
                        then: block_at(i.target())?,
                        els: fall(),
                    });
                }
                0x44..=0x4a => stmts
                    .push(Stmt::Assign(dv, Expr::ArrayGet(Box::new(u(0)), Box::new(u(1)), array_kind(i.op)))),
                0x4b..=0x51 => stmts.push(Stmt::APut(u(1), u(2), u(0), array_kind(i.op))),
                0x52..=0x58 => {
                    stmts.push(Stmt::Assign(dv, Expr::IGet(Box::new(u(0)), field_ref(dex, i.index))))
                }
                0x59..=0x5f => stmts.push(Stmt::IPut(u(1), field_ref(dex, i.index), u(0))),
                0x60..=0x66 => stmts.push(Stmt::Assign(dv, Expr::SGet(field_ref(dex, i.index)))),
                0x67..=0x6d => stmts.push(Stmt::SPut(field_ref(dex, i.index), u(0))),
                0x6e..=0x72 | 0x74..=0x78 | 0xfa | 0xfb => {
                    let kind = match i.op {
                        0x6e | 0x74 => InvokeKind::Virtual,
                        0x6f | 0x75 => InvokeKind::Super,
                        0x70 | 0x76 => InvokeKind::Direct,
                        0x71 | 0x77 => InvokeKind::Static,
                        0x72 | 0x78 => InvokeKind::Interface,
                        _ => InvokeKind::Polymorphic,
                    };
                    let args = (0..use_var[k].len()).map(u).collect();
                    pending = Some(Expr::Invoke(kind, method_ref(dex, i.index), args));
                }
                0xfc | 0xfd => {
                    let args = (0..use_var[k].len()).map(u).collect();
                    let mref = MethodRef {
                        class: "Ljava/lang/invoke/CallSite;".into(),
                        name: format!("callSite{}", i.index).into(),
                        params: Vec::new().into(),
                        ret: "Ljava/lang/Object;".into(),
                    };
                    pending = Some(Expr::Invoke(InvokeKind::Custom, mref, args));
                }
                0x7b..=0x8f => {
                    let e = match i.op {
                        0x7b | 0x7d | 0x7f | 0x80 => Expr::Neg(Box::new(u(0))),
                        0x7c | 0x7e => Expr::BitNot(Box::new(u(0))),
                        _ => {
                            let to =
                                ["J", "F", "D", "I", "F", "D", "I", "J", "D", "I", "J", "F", "B", "C", "S"]
                                    [(i.op - 0x81) as usize];
                            Expr::Cast(to.into(), Box::new(u(0)))
                        }
                    };
                    stmts.push(Stmt::Assign(dv, e));
                }
                0x90..=0xcf => {
                    let (op, ty) = binop_of(i.op).unwrap_or((BinOp::Add, "I"));
                    stmts.push(Stmt::Assign(dv, Expr::Bin(op, Box::new(u(0)), Box::new(u(1)), Some(ty))));
                }
                0xd0..=0xe2 => {
                    let op = lit_binop(i.op).unwrap_or(BinOp::Add);
                    let e = if matches!(i.op, 0xd1 | 0xd9) {
                        Expr::Bin(BinOp::Sub, Box::new(Expr::lit(i.lit)), Box::new(u(0)), Some("I"))
                    } else {
                        Expr::Bin(op, Box::new(u(0)), Box::new(Expr::lit(i.lit)), Some("I"))
                    };
                    stmts.push(Stmt::Assign(dv, e));
                }
                0xfe => {
                    stmts.push(Stmt::Assign(dv, Expr::Raw(format!("/* method handle {} */ null", i.index))))
                }
                0xff => {
                    stmts.push(Stmt::Assign(dv, Expr::Raw(format!("/* method type {} */ null", i.index))))
                }
                op => bail!("unsupported opcode {op:#04x} at {:#x}", i.addr),
            }
        }
        if let Some(p) = pending.take()
            && matches!(p, Expr::Invoke(..))
        {
            stmts.push(Stmt::Expr(p));
        }
        let last = range.end - 1;
        let term = term.unwrap_or_else(|| {
            if last + 1 < insns.len() { Term::Goto(block_of[last + 1]) } else { Term::Return(None) }
        });
        blocks.push(Block {
            start: insns[range.start].addr,
            end: next_addr(last),
            stmts,
            term,
            preds: preds[b].clone(),
            handlers: block_handlers[b].clone(),
            try_region: block_region[b],
        });
    }

    let mut body = Body { blocks, entry: 0, vars, params, this, tries, ret };
    merge_constructors(&mut body);
    infer_types(&mut body);
    propagate_constants(&mut body);
    merge_array_init(&mut body);
    merge_builder_calls(&mut body);
    simplify(&mut body);
    inline(&mut body);
    thread_jumps(&mut body);
    merge_conditions(&mut body);
    recompute_preds(&mut body);
    Ok(body)
}

const NEW_MARK: &str = "<new>";

fn new_placeholder(desc: &str) -> Expr {
    Expr::New(
        MethodRef { class: desc.into(), name: NEW_MARK.into(), params: Vec::new().into(), ret: "V".into() },
        vec![],
    )
}

fn is_placeholder(e: &Expr) -> Option<&Arc<str>> {
    match e {
        Expr::New(m, _) if &*m.name == NEW_MARK => Some(&m.class),
        _ => None,
    }
}

fn fill_array(stmts: &mut Vec<Stmt>, arr: Expr, var: VarId, p: Option<&Payload>) {
    let Some(Payload::FillArray { width, data }) = p else { return };
    let w = (*width as usize).clamp(1, 8);
    let values: Vec<i64> = data
        .chunks(w)
        .map(|c| {
            let mut v: u64 = 0;
            for (i, &b) in c.iter().enumerate() {
                v |= (b as u64) << (8 * i);
            }
            let bits = 8 * c.len() as u32;
            if bits < 64 && v & (1 << (bits - 1)) != 0 { (v | (u64::MAX << bits)) as i64 } else { v as i64 }
        })
        .collect();
    let wide = w == 8;
    if let Some(Stmt::Assign(v, Expr::NewArray(ty, len))) = stmts.last()
        && *v == var
        && matches!(**len, Expr::Lit { bits, .. } if bits as usize == values.len())
    {
        let ty = ty.clone();
        let elems = values.iter().map(|&bits| Expr::Lit { bits, wide }).collect();
        *stmts.last_mut().unwrap() = Stmt::Assign(var, Expr::ArrayLit(ty, elems));
        return;
    }
    for (i, &bits) in values.iter().enumerate() {
        stmts.push(Stmt::APut(arr.clone(), Expr::lit(i as i64), Expr::Lit { bits, wide }, ""));
    }
}

/// Rewrites `v = new-instance T; ...; v.<init>(args)` into `v = new T(args)`: each constructor
/// call is matched with the closest allocation of its variable earlier in the same block, or with
/// the variable's only allocation.
fn merge_constructors(body: &mut Body) {
    // Variables whose only definition is one allocation.
    let mut defs: HashMap<VarId, Vec<(BlockId, usize, bool)>> = HashMap::new();
    for (b, blk) in body.blocks.iter().enumerate() {
        for (s, st) in blk.stmts.iter().enumerate() {
            if let Stmt::Assign(v, e) = st {
                defs.entry(*v).or_default().push((b, s, is_placeholder(e).is_some()));
            }
        }
    }
    let only: HashMap<VarId, Option<(BlockId, usize)>> =
        defs.into_iter().map(|(v, d)| (v, (d.len() == 1 && d[0].2).then(|| (d[0].0, d[0].1)))).collect();
    let mut remove = vec![];
    for b in 0..body.blocks.len() {
        for s in 0..body.blocks[b].stmts.len() {
            let Stmt::Expr(Expr::Invoke(InvokeKind::Direct, m, args)) = &body.blocks[b].stmts[s] else {
                continue;
            };
            if &*m.name != "<init>" {
                continue;
            }
            let Some(Expr::Var(v)) = args.first() else { continue };
            let v = *v;
            // The closest earlier allocation of v in this block, with no other write of v between.
            let mut site = None;
            for k in (0..s).rev() {
                if let Stmt::Assign(w, e) = &body.blocks[b].stmts[k]
                    && *w == v
                {
                    if is_placeholder(e).is_some() {
                        site = Some((b, k));
                    }
                    break;
                }
            }
            // One allocation may feed constructor calls on different paths (`x = c ? new T() :
            // new T(a)` compiled with a shared new-instance); each call becomes a `new`.
            let Some((pb, ps)) = site.or_else(|| only.get(&v).copied().flatten()) else { continue };
            let (m, rest) = (m.clone(), args[1..].to_vec());
            body.blocks[b].stmts[s] = Stmt::Assign(v, Expr::New(m, rest));
            if !remove.contains(&(pb, ps)) {
                remove.push((pb, ps));
            }
        }
    }
    remove.sort_unstable_by(|a, b| b.cmp(a));
    for (b, s) in remove {
        body.blocks[b].stmts.remove(s);
    }
}

// ---------------------------------------------------------------- types

#[derive(Default)]
struct Hints {
    exact: Vec<String>,
    /// Narrow ints used as such (arithmetic, array index, switch).
    int_use: bool,
    any_ref: bool,
    wide_const: bool,
}

/// Type an expression produces, when determined by the expression itself.
pub fn expr_type(e: &Expr, vars: &[VarInfo]) -> Option<String> {
    Some(match e {
        Expr::Var(v) => return vars.get(*v as usize).and_then(|v| v.ty.clone()),
        Expr::Lit { .. } | Expr::Raw(_) | Expr::Ternary(..) => return None,
        Expr::Str(_) => "Ljava/lang/String;".into(),
        Expr::Class(_) => "Ljava/lang/Class;".into(),
        Expr::Bin(op, a, b, t) => {
            if op.is_compare() || matches!(op, BinOp::LogAnd | BinOp::LogOr) {
                "Z".into()
            } else if let Some(t) = t {
                if *t == "I" && matches!(op, BinOp::And | BinOp::Or | BinOp::Xor) {
                    // Booleans use int bit ops.
                    let (ta, tb) = (expr_type(a, vars), expr_type(b, vars));
                    if ta.as_deref() == Some("Z") || tb.as_deref() == Some("Z") {
                        return Some("Z".into());
                    }
                }
                (*t).into()
            } else {
                return expr_type(a, vars);
            }
        }
        Expr::Neg(a) | Expr::BitNot(a) => return expr_type(a, vars),
        Expr::Not(_) | Expr::InstanceOf(..) => "Z".into(),
        Expr::Cast(t, _) => t.to_string(),
        Expr::ArrayLen(_) | Expr::Cmp(..) => "I".into(),
        Expr::ArrayGet(a, _, k) => {
            if let Some(t) = expr_type(a, vars).as_deref().and_then(types::element) {
                return Some(t.to_owned());
            }
            match *k {
                "L" | "I" | "J" => return None,
                k => k.into(),
            }
        }
        Expr::IGet(_, f) | Expr::SGet(f) => f.ty.to_string(),
        Expr::Invoke(_, m, _) => {
            if &*m.ret == "V" {
                return None;
            }
            m.ret.to_string()
        }
        Expr::New(m, _) => m.class.to_string(),
        Expr::NewArray(t, _) | Expr::ArrayLit(t, _) => t.to_string(),
        Expr::CaughtException(t) => t.as_deref().unwrap_or("Ljava/lang/Throwable;").to_owned(),
    })
}

/// Records what type each operand of `e` is expected to have.
fn use_hints(e: &Expr, hints: &mut [Hints]) {
    let want = |e: &Expr, ty: &str, hints: &mut [Hints]| {
        if let Expr::Var(v) = e
            && let Some(h) = hints.get_mut(*v as usize)
        {
            h.exact.push(ty.to_owned());
        }
    };
    match e {
        Expr::Bin(op, a, b, t) => {
            if let Some(t) = t {
                let shift = matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr);
                if matches!(op, BinOp::And | BinOp::Or | BinOp::Xor) && *t == "I" {
                    // int or boolean: decided by the other hints
                } else {
                    for (x, is_rhs) in [(a, false), (b, true)] {
                        if let Expr::Var(v) = **x {
                            let ty = if shift && is_rhs { "I" } else { t };
                            if ty == "I" {
                                hints[v as usize].int_use = true;
                            } else {
                                want(x, ty, hints);
                            }
                        }
                    }
                }
            } else if op.is_compare() {
                // `x == 0`: no hint; `x == y` links nothing here.
                if let (Expr::Var(v), Expr::Lit { bits: 0, .. }) = (&**a, &**b)
                    && matches!(op, BinOp::Lt | BinOp::Ge | BinOp::Gt | BinOp::Le)
                {
                    hints[*v as usize].int_use = true;
                }
            }
        }
        Expr::ArrayGet(a, i, _) => {
            if let Expr::Var(v) = **i {
                hints[v as usize].int_use = true;
            }
            if let Expr::Var(v) = **a {
                hints[v as usize].any_ref = true;
            }
        }
        Expr::ArrayLen(a) | Expr::InstanceOf(a, _) => {
            if let Expr::Var(v) = **a {
                hints[v as usize].any_ref = true;
            }
        }
        Expr::Cast(t, a) => {
            if let Expr::Var(v) = **a
                && types::is_reference(t)
            {
                hints[v as usize].any_ref = true;
            }
        }
        Expr::IGet(o, f) => want(o, &f.class, hints),
        Expr::New(m, args) => {
            for (a, p) in args.iter().zip(m.params.iter()) {
                want(a, p, hints);
            }
        }
        Expr::Invoke(kind, m, args) if *kind != InvokeKind::Custom => {
            let mut params: Vec<&str> = m.params.iter().map(String::as_str).collect();
            if *kind != InvokeKind::Static {
                params.insert(0, &m.class);
            }
            for (a, p) in args.iter().zip(params) {
                want(a, p, hints);
            }
        }
        Expr::NewArray(_, n) => {
            if let Expr::Var(v) = **n {
                hints[v as usize].int_use = true;
            }
        }
        Expr::ArrayLit(t, items) => {
            if let Some(el) = types::element(t) {
                for i in items {
                    want(i, el, hints);
                }
            }
        }
        _ => {}
    }
}

fn infer_types(body: &mut Body) {
    let n = body.vars.len();
    let ret = body.ret.clone();
    for _round in 0..3 {
        let mut hints: Vec<Hints> = (0..n).map(|_| Hints::default()).collect();
        for blk in &body.blocks {
            for st in &blk.stmts {
                st.visit_exprs(&mut |e| e.visit(&mut |x| use_hints(x, &mut hints)));
                match st {
                    Stmt::Assign(v, e) => {
                        if let Some(t) = expr_type(e, &body.vars) {
                            hints[*v as usize].exact.push(t);
                        } else if let Expr::Lit { wide: true, .. } = e {
                            hints[*v as usize].wide_const = true;
                        }
                        // A move links both sides.
                        if let Expr::Var(src) = e
                            && let Some(t) = body.vars[*v as usize].ty.clone()
                        {
                            hints[*src as usize].exact.push(t);
                        }
                    }
                    Stmt::IPut(o, f, val) => {
                        hint_var(&mut hints, o, &f.class);
                        hint_var(&mut hints, val, &f.ty);
                    }
                    Stmt::SPut(f, val) => hint_var(&mut hints, val, &f.ty),
                    Stmt::APut(a, i, val, k) => {
                        if let Expr::Var(v) = i {
                            hints[*v as usize].int_use = true;
                        }
                        if let Some(at) = expr_type(a, &body.vars)
                            && let Some(el) = types::element(&at)
                        {
                            hint_var(&mut hints, val, el);
                        } else if !matches!(*k, "I" | "J" | "L" | "") {
                            hint_var(&mut hints, val, k);
                        }
                        if let Expr::Var(v) = a {
                            hints[*v as usize].any_ref = true;
                        }
                    }
                    Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
                        hint_var(&mut hints, e, "Ljava/lang/Object;")
                    }
                    _ => {}
                }
            }
            blk.term.visit_exprs(&mut |e| e.visit(&mut |x| use_hints(x, &mut hints)));
            match &blk.term {
                Term::Return(Some(e)) => hint_var(&mut hints, e, &ret),
                Term::Throw(e) => hint_var(&mut hints, e, "Ljava/lang/Throwable;"),
                Term::Switch { value: Expr::Var(v), .. } => hints[*v as usize].int_use = true,
                _ => {}
            }
        }
        for (v, h) in hints.into_iter().enumerate() {
            let vi = &mut body.vars[v];
            if vi.is_this || vi.param.is_some() {
                continue;
            }
            if let Some(t) = choose_type(&h, vi.ty.as_deref()) {
                vi.ty = Some(t);
            }
        }
    }
}

fn hint_var(hints: &mut [Hints], e: &Expr, ty: &str) {
    if let Expr::Var(v) = e
        && let Some(h) = hints.get_mut(*v as usize)
    {
        h.exact.push(ty.to_owned());
    }
}

fn choose_type(h: &Hints, current: Option<&str>) -> Option<String> {
    // Debug-info types are authoritative.
    if let Some(c) = current
        && c != "I"
        && c != "Ljava/lang/Object;"
    {
        return Some(c.to_owned());
    }
    let refs: Vec<&str> = h.exact.iter().map(String::as_str).filter(|t| types::is_reference(t)).collect();
    let prims: Vec<&str> = h.exact.iter().map(String::as_str).filter(|t| types::is_primitive(t)).collect();
    if !refs.is_empty() {
        // Prefer the most specific: a non-Object type seen most often.
        let mut best = refs[0];
        let mut best_n = 0;
        for &t in &refs {
            let n =
                refs.iter().filter(|&&x| x == t).count() + if t == "Ljava/lang/Object;" { 0 } else { 1000 };
            if n > best_n {
                best = t;
                best_n = n;
            }
        }
        return Some(best.to_owned());
    }
    if !prims.is_empty() {
        for want in ["J", "D", "F"] {
            if prims.contains(&want) {
                return Some(want.into());
            }
        }
        if prims.contains(&"Z") && !h.int_use && prims.iter().all(|&p| p == "Z") {
            return Some("Z".into());
        }
        for want in ["C", "B", "S"] {
            if prims.contains(&want) && !h.int_use && prims.iter().all(|&p| p == want) {
                return Some(want.into());
            }
        }
        return Some("I".into());
    }
    if h.any_ref {
        return Some("Ljava/lang/Object;".into());
    }
    if h.wide_const {
        return Some("J".into());
    }
    current.map(str::to_owned).or_else(|| h.int_use.then(|| "I".into()))
}

// ---------------------------------------------------------------- inlining

#[derive(Default, Clone, Copy)]
struct VarStats {
    defs: u32,
    uses: u32,
    def_at: Option<(BlockId, usize)>,
    use_at: Option<(BlockId, usize)>,
}

fn stats(body: &Body) -> Vec<VarStats> {
    let mut s = vec![VarStats::default(); body.vars.len()];
    for (b, blk) in body.blocks.iter().enumerate() {
        for (k, st) in blk.stmts.iter().enumerate() {
            st.visit_exprs(&mut |e| {
                e.visit_vars(&mut |v| {
                    let x = &mut s[v as usize];
                    x.uses += 1;
                    x.use_at = Some((b, k));
                })
            });
            if let Stmt::Assign(v, _) = st {
                let x = &mut s[*v as usize];
                x.defs += 1;
                x.def_at = Some((b, k));
            }
        }
        let k = blk.stmts.len();
        blk.term.visit_exprs(&mut |e| {
            e.visit_vars(&mut |v| {
                let x = &mut s[v as usize];
                x.uses += 1;
                x.use_at = Some((b, k));
            })
        });
    }
    s
}

/// Position in evaluation order: the final statement, then operand indices down the tree.
type EvalPath = Vec<usize>;

/// Whether `a` is evaluated after `b`. Operands are evaluated left to right, before their parent.
fn after(a: &EvalPath, b: &EvalPath) -> bool {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return x > y;
        }
    }
    // A prefix is an ancestor, evaluated after its descendants.
    a.len() < b.len()
}

/// Replaces single-use temporaries with their defining expression where evaluation order allows.
/// Statements are decided last to first, so each user's final position is known; moving a
/// definition is allowed when every conflicting statement in between still runs after it.
fn inline(body: &mut Body) {
    let st = stats(body);
    let keep = |v: VarId, body: &Body| {
        let vi = &body.vars[v as usize];
        vi.name.is_some() || vi.is_this || vi.param.is_some()
    };
    for b in 0..body.blocks.len() {
        let n = body.blocks[b].stmts.len();
        let mut ops: Vec<Vec<VarId>> = Vec::with_capacity(n + 1);
        let mut impure = Vec::with_capacity(n + 1);
        for st_ in &body.blocks[b].stmts {
            let mut o = vec![];
            st_.visit_exprs(&mut |e| e.visit_vars(&mut |v| o.push(v)));
            ops.push(o);
            impure.push(st_.orders());
        }
        let mut o = vec![];
        body.blocks[b].term.visit_exprs(&mut |e| e.visit_vars(&mut |v| o.push(v)));
        ops.push(o);
        impure.push(true);
        let writes: Vec<Option<VarId>> = body.blocks[b]
            .stmts
            .iter()
            .map(|s| if let Stmt::Assign(v, _) = s { Some(*v) } else { None })
            .chain([None])
            .collect();
        let mut path: Vec<EvalPath> = (0..=n).map(|k| vec![k]).collect();
        let mut into: Vec<Option<usize>> = vec![None; n + 1];
        let mut children: Vec<Vec<usize>> = vec![vec![]; n + 1];
        // Decisions depend on where other statements end up, so iterate until nothing changes.
        for _round in 0..8 {
            let mut changed = false;
            for u in (0..=n).rev() {
                for (i, &v) in ops[u].iter().enumerate().rev() {
                    let s = st[v as usize];
                    if s.defs != 1 || s.uses != 1 || keep(v, body) {
                        continue;
                    }
                    let Some((db, d)) = s.def_at else { continue };
                    if db != b || d >= u || into[d].is_some() {
                        continue;
                    }
                    let Stmt::Assign(_, e) = &body.blocks[b].stmts[d] else { continue };
                    // `it.next()` may only merge into a cast of it (`x = (T) it.next()`).
                    let into_cast = matches!(body.blocks[b].stmts.get(u), Some(Stmt::Assign(_, Expr::Cast(_, x))) if **x == Expr::Var(v));
                    if matches!(e, Expr::CaughtException(_))
                        || is_placeholder(e).is_some()
                        || (is_iterator_next(e) && !into_cast)
                    {
                        continue;
                    }
                    // Effects and reads of d together with what is already inlined into it.
                    let mut sub_impure = false;
                    let mut sub_reads: Vec<VarId> = vec![];
                    let mut stack = vec![d];
                    while let Some(x) = stack.pop() {
                        sub_impure |= impure[x];
                        sub_reads.extend(&ops[x]);
                        stack.extend(&children[x]);
                    }
                    let target: EvalPath = path[u].iter().copied().chain([i]).collect();
                    let last = path[u][0];
                    let ok = (d + 1..last).all(|x| {
                        let conflict =
                            (sub_impure && impure[x]) || writes[x].is_some_and(|w| sub_reads.contains(&w));
                        !conflict || after(&path[x], &target)
                    });
                    if ok {
                        into[d] = Some(u);
                        children[u].push(d);
                        // d and everything inlined into it move together.
                        let mut stack = vec![(d, target)];
                        while let Some((x, p)) = stack.pop() {
                            for &c in &children[x] {
                                let slot = *path[c].last().unwrap_or(&0);
                                stack.push((c, p.iter().copied().chain([slot]).collect()));
                            }
                            path[x] = p;
                        }
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // Materialize: substitute definitions into their users, front to back.
        let mut taken: Vec<Option<Expr>> = vec![None; n];
        let def_of: HashMap<VarId, usize> =
            (0..n).filter(|&d| into[d].is_some()).filter_map(|d| writes[d].map(|v| (v, d))).collect();
        for k in 0..=n {
            let mut sub = |x: &mut Expr| {
                if let Expr::Var(v) = x
                    && let Some(&d) = def_of.get(v)
                    && into[d] == Some(k)
                    && let Some(e) = taken[d].take()
                {
                    *x = e;
                }
            };
            if k < n {
                body.blocks[b].stmts[k].visit_exprs_mut(&mut |x| x.visit_mut(&mut sub));
                if into[k].is_some()
                    && let Stmt::Assign(_, e) = std::mem::replace(&mut body.blocks[b].stmts[k], Stmt::Line(0))
                {
                    taken[k] = Some(e);
                }
            } else {
                body.blocks[b].term.visit_exprs_mut(&mut |x| x.visit_mut(&mut sub));
            }
        }
        body.blocks[b].stmts.retain(|s| !matches!(s, Stmt::Line(_)));
    }
    // Drop pure assignments that are never read.
    let st = stats(body);
    for blk in &mut body.blocks {
        blk.stmts.retain(|s| match s {
            Stmt::Assign(v, e) => {
                st[*v as usize].uses > 0 || !e.is_pure() || body.vars[*v as usize].name.is_some()
            }
            _ => true,
        });
    }
}

/// `it.next()` or `(T) it.next()`: kept as its own statement so for-each loops can be rebuilt.
fn is_iterator_next(e: &Expr) -> bool {
    match e {
        Expr::Cast(_, inner) => is_iterator_next(inner),
        Expr::Invoke(InvokeKind::Interface, m, _) => {
            &*m.name == "next" && m.params.is_empty() && &*m.class == "Ljava/util/Iterator;"
        }
        _ => false,
    }
}

/// Replaces reads of single-definition constants with the constant (registers holding constants
/// are freely reused by d8).
fn propagate_constants(body: &mut Body) {
    let st = stats(body);
    let mut consts: HashMap<VarId, Expr> = HashMap::new();
    for blk in &body.blocks {
        for s in &blk.stmts {
            if let Stmt::Assign(v, e) = s
                && matches!(e, Expr::Lit { .. } | Expr::Str(_) | Expr::Class(_))
                && st[*v as usize].defs == 1
                && body.vars[*v as usize].name.is_none()
            {
                consts.insert(*v, e.clone());
            }
        }
    }
    if consts.is_empty() {
        return;
    }
    let mut sub = |x: &mut Expr| {
        if let Expr::Var(v) = x
            && let Some(c) = consts.get(v)
        {
            *x = c.clone();
        }
    };
    for blk in &mut body.blocks {
        blk.stmts.retain(|s| !matches!(s, Stmt::Assign(v, _) if consts.contains_key(v)));
        for s in &mut blk.stmts {
            s.visit_exprs_mut(&mut |x| x.visit_mut(&mut sub));
        }
        blk.term.visit_exprs_mut(&mut |x| x.visit_mut(&mut sub));
    }
}

/// `sb = new StringBuilder(); sb.append(a); sb.append(b); … sb.toString()` → one append chain,
/// so string concatenation can be recovered.
fn merge_builder_calls(body: &mut Body) {
    let st = stats(body);
    for blk in &mut body.blocks {
        let mut i = 0;
        while i < blk.stmts.len() {
            if let Stmt::Assign(sb, e @ Expr::New(m, _)) = &blk.stmts[i]
                && &*m.class == "Ljava/lang/StringBuilder;"
                && body.vars[*sb as usize].name.is_none()
            {
                let sb = *sb;
                let mut chain = e.clone();
                let mut j = i + 1;
                while let Some(Stmt::Expr(Expr::Invoke(InvokeKind::Virtual, am, args))) = blk.stmts.get(j)
                    && &*am.name == "append"
                    && &*am.class == "Ljava/lang/StringBuilder;"
                    && args.len() == 2
                    && args[0] == Expr::Var(sb)
                {
                    chain = Expr::Invoke(InvokeKind::Virtual, am.clone(), vec![chain, args[1].clone()]);
                    j += 1;
                }
                // Every other use must be the final toString().
                if j > i + 1 && st[sb as usize].uses as usize == j - i {
                    blk.stmts.splice(i..j, [Stmt::Assign(sb, chain)]);
                }
            }
            i += 1;
        }
    }
}

/// `a = new T[n]; a[0] = x; …; a[n-1] = z;` → `a = new T[]{x, …, z};`
fn merge_array_init(body: &mut Body) {
    for blk in &mut body.blocks {
        let mut i = 0;
        while i < blk.stmts.len() {
            if let Stmt::Assign(v, Expr::NewArray(ty, len)) = &blk.stmts[i]
                && let Expr::Lit { bits: n, .. } = **len
                && (1..=4096).contains(&n)
                && i + (n as usize) < blk.stmts.len()
            {
                let (v, ty, n) = (*v, ty.clone(), n as usize);
                let vals: Option<Vec<Expr>> = (0..n)
                    .map(|k| match &blk.stmts[i + 1 + k] {
                        Stmt::APut(Expr::Var(a), Expr::Lit { bits, .. }, val, _)
                            if *a == v && *bits == k as i64 =>
                        {
                            let mut uses_v = false;
                            val.visit_vars(&mut |x| uses_v |= x == v);
                            (!uses_v).then(|| val.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if let Some(vals) = vals {
                    blk.stmts.splice(i..=i + n, [Stmt::Assign(v, Expr::ArrayLit(ty, vals))]);
                }
            }
            i += 1;
        }
    }
}

/// Local rewrites that need types: `b ^ 1` → `!b`, `x + -1` → `x - 1`.
fn simplify(body: &mut Body) {
    let vars = body.vars.clone();
    let mut f = |e: &mut Expr| {
        if let Expr::Bin(op, a, b, t) = e {
            match (*op, &**b) {
                (BinOp::Xor, Expr::Lit { bits: 1, .. }) if expr_type(a, &vars).as_deref() == Some("Z") => {
                    *e = Expr::Not(a.clone());
                }
                (BinOp::Add | BinOp::Sub, Expr::Lit { bits, wide })
                    if *bits < 0 && *bits != i64::MIN && matches!(t, Some("I" | "J")) =>
                {
                    let (bits, wide) = (-*bits, *wide);
                    if !(*t == Some("I") && bits > i32::MAX as i64) {
                        *op = if *op == BinOp::Add { BinOp::Sub } else { BinOp::Add };
                        **b = Expr::Lit { bits, wide };
                    }
                }
                _ => {}
            }
        }
    };
    for blk in &mut body.blocks {
        for s in &mut blk.stmts {
            s.visit_exprs_mut(&mut |x| x.visit_mut(&mut f));
        }
        blk.term.visit_exprs_mut(&mut |x| x.visit_mut(&mut f));
    }
}

/// Builds region nesting: a region whose dex handler list ends with another region's list is
/// nested in it. Nested regions keep only their own handlers; parents cover their blocks too.
fn nest_regions(regions: Vec<(Vec<(Option<u32>, u32)>, TryRegion)>) -> Vec<TryRegion> {
    let keys: Vec<Vec<(Option<u32>, u32)>> = regions.iter().map(|(k, _)| k.clone()).collect();
    let mut out: Vec<TryRegion> = regions.into_iter().map(|(_, r)| r).collect();
    for a in 0..out.len() {
        let parent = (0..out.len())
            .filter(|&b| b != a && keys[b].len() < keys[a].len() && keys[a].ends_with(&keys[b]))
            .max_by_key(|&b| keys[b].len());
        if let Some(p) = parent {
            out[a].parent = Some(p);
            let own = keys[a].len() - keys[p].len();
            out[a].handlers.truncate(own);
        }
    }
    for a in 0..out.len() {
        let mut depth = 0;
        let mut cur = out[a].parent;
        let blocks = out[a].blocks.clone();
        while let Some(p) = cur {
            depth += 1;
            if depth > out.len() {
                break;
            }
            for &b in &blocks {
                if !out[p].blocks.contains(&b) {
                    out[p].blocks.push(b);
                }
            }
            cur = out[p].parent;
        }
        out[a].depth = depth;
    }
    for r in &mut out {
        r.blocks.sort_unstable();
    }
    out
}

/// Retargets edges that go through empty `goto` blocks.
fn thread_jumps(body: &mut Body) {
    let n = body.blocks.len();
    let forward: Vec<Option<BlockId>> = (0..n)
        .map(|b| match body.blocks[b].term {
            // Blocks at try boundaries carry exception structure; keep them.
            Term::Goto(t)
                if body.blocks[b].stmts.is_empty()
                    && t != b
                    && b != body.entry
                    && body.blocks[b].try_region == body.blocks[t].try_region =>
            {
                Some(t)
            }
            _ => None,
        })
        .collect();
    let resolve = |mut b: BlockId| {
        for _ in 0..n {
            match forward[b] {
                Some(t) => b = t,
                None => break,
            }
        }
        b
    };
    for blk in &mut body.blocks {
        match &mut blk.term {
            Term::Goto(t) => *t = resolve(*t),
            Term::If { then, els, .. } => {
                *then = resolve(*then);
                *els = resolve(*els);
            }
            Term::Switch { cases, default, .. } => {
                for c in cases {
                    c.1 = resolve(c.1);
                }
                *default = resolve(*default);
            }
            _ => {}
        }
    }
    recompute_preds(body);
}

fn recompute_preds(body: &mut Body) {
    for b in &mut body.blocks {
        b.preds.clear();
    }
    for b in 0..body.blocks.len() {
        for s in body.blocks[b].term.succs() {
            if !body.blocks[s].preds.contains(&b) {
                body.blocks[s].preds.push(b);
            }
        }
    }
}

// ---------------------------------------------------------------- conditions

/// Turns `cmp` results compared with zero into direct comparisons and merges `if` chains that
/// implement `&&` / `||` into single conditions.
fn merge_conditions(body: &mut Body) {
    for blk in &mut body.blocks {
        if let Term::If { cond, .. } = &mut blk.term {
            simplify_cond(cond);
        }
    }
    loop {
        let mut changed = false;
        for a in 0..body.blocks.len() {
            let Term::If { then: at, els: ae, .. } = body.blocks[a].term.clone() else { continue };
            for (second, other, a_jumps_to_other_on_true) in [(ae, at, true), (at, ae, false)] {
                if second == a || !body.blocks[second].stmts.is_empty() || body.blocks[second].preds != [a] {
                    continue;
                }
                if body.blocks[second].try_region != body.blocks[a].try_region {
                    continue;
                }
                let Term::If { cond: c2, then: bt, els: be } = body.blocks[second].term.clone() else {
                    continue;
                };
                let Term::If { cond: c1, .. } = body.blocks[a].term.clone() else { continue };
                // a: if c1 → other else second ; second: if c2 → other / else
                let merged = if a_jumps_to_other_on_true {
                    if bt == other {
                        Some((Expr::bin(BinOp::LogOr, c1, c2), other, be))
                    } else if be == other {
                        Some((Expr::bin(BinOp::LogOr, c1, c2.negate()), other, bt))
                    } else {
                        None
                    }
                } else if be == other {
                    Some((Expr::bin(BinOp::LogAnd, c1, c2), bt, other))
                } else if bt == other {
                    Some((Expr::bin(BinOp::LogAnd, c1, c2.negate()), be, other))
                } else {
                    None
                };
                if let Some((cond, then, els)) = merged {
                    body.blocks[a].term = Term::If { cond, then, els };
                    // `second` becomes unreachable.
                    body.blocks[second].preds.clear();
                    for s in [bt, be] {
                        let p = &mut body.blocks[s].preds;
                        p.retain(|&x| x != second);
                        if !p.contains(&a) {
                            p.push(a);
                        }
                    }
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

fn simplify_cond(cond: &mut Expr) {
    // (cmp a b) OP 0  →  a OP b
    if let Expr::Bin(op, l, r, _) = cond
        && op.is_compare()
        && matches!(**r, Expr::Lit { bits: 0, .. })
        && let Expr::Cmp(kind, a, b) = &**l
    {
        let ty = match *kind {
            "cmp-long" => "J",
            "cmpl-float" | "cmpg-float" => "F",
            _ => "D",
        };
        *cond = Expr::Bin(*op, a.clone(), b.clone(), Some(ty));
    }
}
