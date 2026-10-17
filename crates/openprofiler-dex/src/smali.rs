//! Smali-style disassembly of a class and its nested classes.

use crate::dex::{self, DexFile, EncodedMethod, Value};
use crate::insn::{self, Format, IndexKind, Insn, Payload};
use crate::java::Printed;
use crate::program::Program;
use std::collections::BTreeSet;
use std::fmt::Write;

fn access_str(flags: u32, kind: char) -> String {
    let mut v = vec![];
    let table: &[(u32, &str)] = &[
        (dex::ACC_PUBLIC, "public"),
        (dex::ACC_PRIVATE, "private"),
        (dex::ACC_PROTECTED, "protected"),
        (dex::ACC_STATIC, "static"),
        (dex::ACC_FINAL, "final"),
    ];
    for &(f, n) in table {
        if flags & f != 0 {
            v.push(n);
        }
    }
    let more: &[(u32, &str)] = match kind {
        'm' => &[
            (dex::ACC_SYNCHRONIZED, "synchronized"),
            (dex::ACC_BRIDGE, "bridge"),
            (dex::ACC_VARARGS, "varargs"),
            (dex::ACC_NATIVE, "native"),
            (dex::ACC_ABSTRACT, "abstract"),
            (dex::ACC_STRICT, "strictfp"),
            (dex::ACC_SYNTHETIC, "synthetic"),
            (dex::ACC_CONSTRUCTOR, "constructor"),
        ],
        'f' => &[
            (dex::ACC_VOLATILE, "volatile"),
            (dex::ACC_TRANSIENT, "transient"),
            (dex::ACC_SYNTHETIC, "synthetic"),
            (dex::ACC_ENUM, "enum"),
        ],
        _ => &[
            (dex::ACC_INTERFACE, "interface"),
            (dex::ACC_ABSTRACT, "abstract"),
            (dex::ACC_SYNTHETIC, "synthetic"),
            (dex::ACC_ANNOTATION, "annotation"),
            (dex::ACC_ENUM, "enum"),
        ],
    };
    for &(f, n) in more {
        if flags & f != 0 {
            v.push(n);
        }
    }
    v.join(" ")
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

fn hex(v: i64) -> String {
    if v < 0 { format!("-0x{:x}", v.unsigned_abs()) } else { format!("0x{v:x}") }
}

fn value(d: &DexFile, v: &Value) -> String {
    match v {
        Value::Byte(x) => format!("{}t", hex(*x as i64)),
        Value::Short(x) => format!("{}s", hex(*x as i64)),
        Value::Char(x) => format!("'\\u{x:04x}'"),
        Value::Int(x) => hex(*x as i64),
        Value::Long(x) => format!("{}L", hex(*x)),
        Value::Float(x) => format!("{x}f"),
        Value::Double(x) => format!("{x}"),
        Value::String(s) => format!("\"{}\"", escape(d.string(*s))),
        Value::Type(t) => d.type_desc(*t).to_owned(),
        Value::Field(f) | Value::Enum(f) => field(d, *f),
        Value::Method(m) => method(d, *m),
        Value::MethodType(p) => d.proto_desc(*p),
        Value::MethodHandle(h) => format!("method_handle_{h}"),
        Value::Array(a) => format!("{{{}}}", a.iter().map(|x| value(d, x)).collect::<Vec<_>>().join(", ")),
        Value::Annotation(a) => format!(".subannotation {}", d.type_desc(a.ty)),
        Value::Null => "null".into(),
        Value::Boolean(b) => b.to_string(),
    }
}

fn field(d: &DexFile, idx: u32) -> String {
    match d.field(idx) {
        Some(f) => format!("{}->{}:{}", d.type_desc(f.class), d.string(f.name), d.type_desc(f.ty)),
        None => format!("field@{idx}"),
    }
}

fn method(d: &DexFile, idx: u32) -> String {
    match d.method(idx) {
        Some(m) => format!("{}->{}{}", d.type_desc(m.class), d.string(m.name), d.proto_desc(m.proto)),
        None => format!("method@{idx}"),
    }
}

struct Regs {
    first_param: u16,
}

impl Regs {
    fn name(&self, r: u16) -> String {
        if r >= self.first_param { format!("p{}", r - self.first_param) } else { format!("v{r}") }
    }
}

fn operands(d: &DexFile, i: &Insn, regs: &Regs) -> String {
    let Some(f) = i.format() else { return String::new() };
    let r = |k: usize| regs.name(i.regs.get(k).copied().unwrap_or(0));
    let label = |a: u32| format!(":L{a:04x}");
    let index = || match i.index_kind() {
        IndexKind::String => format!("\"{}\"", escape(d.string(i.index))),
        IndexKind::Type => d.type_desc(i.index).to_owned(),
        IndexKind::Field => field(d, i.index),
        IndexKind::Method => method(d, i.index),
        IndexKind::CallSite => format!("call_site_{}", i.index),
        IndexKind::MethodHandle => format!("method_handle_{}", i.index),
        IndexKind::Proto => d.proto_desc(i.index),
        IndexKind::None => String::new(),
    };
    let reglist = || {
        if matches!(f, Format::F3rc | Format::F4rcc) && !i.regs.is_empty() {
            format!("{{{} .. {}}}", regs.name(i.regs[0]), regs.name(*i.regs.last().unwrap_or(&0)))
        } else {
            format!("{{{}}}", i.regs.iter().map(|&x| regs.name(x)).collect::<Vec<_>>().join(", "))
        }
    };
    match f {
        Format::F10x => String::new(),
        Format::F12x | Format::F22x | Format::F32x => format!("{}, {}", r(0), r(1)),
        Format::F11n | Format::F21s | Format::F31i | Format::F21h | Format::F51l => {
            format!("{}, {}", r(0), hex(i.lit))
        }
        Format::F11x => r(0),
        Format::F10t | Format::F20t | Format::F30t => label(i.target()),
        Format::F21t | Format::F31t => format!("{}, {}", r(0), label(i.target())),
        Format::F21c | Format::F31c => format!("{}, {}", r(0), index()),
        Format::F23x => format!("{}, {}, {}", r(0), r(1), r(2)),
        Format::F22b | Format::F22s => format!("{}, {}, {}", r(0), r(1), hex(i.lit)),
        Format::F22t => format!("{}, {}, {}", r(0), r(1), label(i.target())),
        Format::F22c => format!("{}, {}, {}", r(0), r(1), index()),
        Format::F35c | Format::F3rc => format!("{}, {}", reglist(), index()),
        Format::F45cc | Format::F4rcc => format!("{}, {}, {}", reglist(), index(), d.proto_desc(i.index2)),
    }
}

fn print_method(out: &mut String, d: &DexFile, m: &EncodedMethod) {
    let Some(mi) = d.method(m.method) else { return };
    let _ = writeln!(
        out,
        ".method {} {}{}",
        access_str(m.access, 'm'),
        d.string(mi.name),
        d.proto_desc(mi.proto)
    );
    if m.code_off == 0 {
        out.push_str(".end method\n\n");
        return;
    }
    let code = match d.code(m.code_off) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "    # invalid code item: {e:#}\n.end method\n");
            return;
        }
    };
    let _ = writeln!(out, "    .registers {}", code.registers);
    let regs = Regs { first_param: code.registers.saturating_sub(code.ins) };
    let debug = d.debug_info(&code).unwrap_or_default();
    let (insns, payloads) = match code.decode() {
        Ok(x) => x,
        Err(e) => {
            let _ = writeln!(out, "    # {e:#}\n.end method\n");
            return;
        }
    };
    let mut labels = BTreeSet::new();
    for i in &insns {
        if insn::is_branch(i.op) || i.op == 0x26 {
            labels.insert(i.target());
        }
    }
    for (addr, p) in &payloads {
        if let Payload::PackedSwitch { targets, .. } | Payload::SparseSwitch { targets, .. } = p
            && let Some(sw) = insns.iter().find(|i| matches!(i.op, 0x2b | 0x2c) && i.target() == *addr)
        {
            for &t in targets {
                labels.insert(sw.addr.wrapping_add_signed(t));
            }
        }
    }
    for t in &code.tries {
        labels.insert(t.start);
        labels.insert(t.start + t.count);
        for h in &t.handlers {
            labels.insert(h.addr);
        }
    }
    let mut lines = debug.lines.iter().peekable();
    let mut pi = 0;
    for i in &insns {
        while pi < payloads.len() && payloads[pi].0 < i.addr {
            print_payload(out, &payloads[pi], &insns);
            pi += 1;
        }
        while let Some(&&(a, l)) = lines.peek() {
            if a > i.addr {
                break;
            }
            if a == i.addr {
                let _ = writeln!(out, "    .line {l}");
            }
            lines.next();
        }
        if labels.contains(&i.addr) {
            let _ = writeln!(out, "    :L{:04x}", i.addr);
        }
        let ops = operands(d, i, &regs);
        let _ = writeln!(out, "    {}{}{}", i.name(), if ops.is_empty() { "" } else { " " }, ops);
    }
    let end = code.units() as u32;
    if labels.contains(&end) {
        let _ = writeln!(out, "    :L{end:04x}");
    }
    for p in &payloads[pi..] {
        print_payload(out, p, &insns);
    }
    for t in &code.tries {
        for h in &t.handlers {
            let ty = h.ty.map_or_else(|| ".catchall".to_owned(), |ty| format!(".catch {}", d.type_desc(ty)));
            let _ = writeln!(
                out,
                "    {ty} {{:L{:04x} .. :L{:04x}}} :L{:04x}",
                t.start,
                t.start + t.count,
                h.addr
            );
        }
    }
    out.push_str(".end method\n\n");
}

fn print_payload(out: &mut String, (addr, p): &(u32, Payload), insns: &[Insn]) {
    let _ = writeln!(out, "    :L{addr:04x}");
    let base =
        insns.iter().find(|i| matches!(i.op, 0x2b | 0x2c) && i.target() == *addr).map_or(0, |i| i.addr);
    match p {
        Payload::PackedSwitch { first_key, targets } => {
            let _ = writeln!(out, "    .packed-switch {}", hex(*first_key as i64));
            for t in targets {
                let _ = writeln!(out, "        :L{:04x}", base.wrapping_add_signed(*t));
            }
            out.push_str("    .end packed-switch\n");
        }
        Payload::SparseSwitch { keys, targets } => {
            out.push_str("    .sparse-switch\n");
            for (k, t) in keys.iter().zip(targets) {
                let _ = writeln!(out, "        {} -> :L{:04x}", hex(*k as i64), base.wrapping_add_signed(*t));
            }
            out.push_str("    .end sparse-switch\n");
        }
        Payload::FillArray { width, data } => {
            let _ = writeln!(out, "    .array-data {width}");
            for c in data.chunks((*width as usize).max(1)) {
                let mut v: u64 = 0;
                for (k, &b) in c.iter().enumerate() {
                    v |= (b as u64) << (8 * k);
                }
                let _ = writeln!(out, "        0x{v:x}");
            }
            out.push_str("    .end array-data\n");
        }
    }
}

fn print_one(out: &mut String, p: &Program, desc: &str) {
    let Some((d, c)) = p.class(desc) else { return };
    let _ = writeln!(out, ".class {} {}", access_str(c.access, 'c'), desc);
    if let Some(s) = c.superclass {
        let _ = writeln!(out, ".super {}", d.type_desc(s));
    }
    if let Some(s) = c.source_file {
        let _ = writeln!(out, ".source \"{}\"", escape(d.string(s)));
    }
    for &i in &c.interfaces {
        let _ = writeln!(out, ".implements {}", d.type_desc(i));
    }
    if let Ok(ann) = d.annotations(c) {
        for a in &ann.class {
            let vis = ["build", "runtime", "system"].get(a.visibility as usize).unwrap_or(&"system");
            let _ = writeln!(out, "\n.annotation {vis} {}", d.type_desc(a.value.ty));
            for (n, v) in &a.value.elements {
                let _ = writeln!(out, "    {} = {}", d.string(*n), value(d, v));
            }
            out.push_str(".end annotation\n");
        }
    }
    out.push('\n');
    let Ok(cd) = d.class_data(c) else { return };
    let statics = d.encoded_array_at(c.static_values_off).unwrap_or_default();
    for (k, f) in cd.static_fields.iter().enumerate() {
        let Some(fi) = d.field(f.field) else { continue };
        let init = statics.get(k).map(|v| format!(" = {}", value(d, v))).unwrap_or_default();
        let _ = writeln!(
            out,
            ".field {} {}:{}{}",
            access_str(f.access, 'f'),
            d.string(fi.name),
            d.type_desc(fi.ty),
            init
        );
    }
    for f in &cd.instance_fields {
        let Some(fi) = d.field(f.field) else { continue };
        let _ = writeln!(
            out,
            ".field {} {}:{}",
            access_str(f.access, 'f'),
            d.string(fi.name),
            d.type_desc(fi.ty)
        );
    }
    if !cd.static_fields.is_empty() || !cd.instance_fields.is_empty() {
        out.push('\n');
    }
    for m in cd.direct_methods.iter().chain(&cd.virtual_methods) {
        print_method(out, d, m);
    }
}

pub fn print_class(p: &Program, top: &str) -> Printed {
    let mut out = String::new();
    let mut stack = vec![std::sync::Arc::<str>::from(top)];
    let mut first = true;
    while let Some(c) = stack.pop() {
        if !first {
            out.push_str("\n# ------------------------------------------------------------ nested class\n\n");
        }
        first = false;
        print_one(&mut out, p, &c);
        if let Some(m) = p.members.get(&c) {
            stack.extend(m.iter().rev().cloned());
        }
    }
    Printed { code: out, links: vec![] }
}
