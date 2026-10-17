//! Bytecode view of JVM classes: the original instructions with javap-style offsets, in the
//! smali-like syntax of the Android view (`.method`, `Lpkg/Type;->name(desc)`, `#` comments),
//! with member and class references linked to their declarations.

use super::classfile::{Be, ClassFile, Code, Const, Member, desc_of_internal};
use crate::java::Printed;
use crate::program::{NodeKey, Program};
use crate::smali::escape;
use std::fmt::Write;
use std::sync::Arc;

const OPCODES: [&str; 202] = [
    "nop",
    "aconst_null",
    "iconst_m1",
    "iconst_0",
    "iconst_1",
    "iconst_2",
    "iconst_3",
    "iconst_4",
    "iconst_5",
    "lconst_0",
    "lconst_1",
    "fconst_0",
    "fconst_1",
    "fconst_2",
    "dconst_0",
    "dconst_1",
    "bipush",
    "sipush",
    "ldc",
    "ldc_w",
    "ldc2_w",
    "iload",
    "lload",
    "fload",
    "dload",
    "aload",
    "iload_0",
    "iload_1",
    "iload_2",
    "iload_3",
    "lload_0",
    "lload_1",
    "lload_2",
    "lload_3",
    "fload_0",
    "fload_1",
    "fload_2",
    "fload_3",
    "dload_0",
    "dload_1",
    "dload_2",
    "dload_3",
    "aload_0",
    "aload_1",
    "aload_2",
    "aload_3",
    "iaload",
    "laload",
    "faload",
    "daload",
    "aaload",
    "baload",
    "caload",
    "saload",
    "istore",
    "lstore",
    "fstore",
    "dstore",
    "astore",
    "istore_0",
    "istore_1",
    "istore_2",
    "istore_3",
    "lstore_0",
    "lstore_1",
    "lstore_2",
    "lstore_3",
    "fstore_0",
    "fstore_1",
    "fstore_2",
    "fstore_3",
    "dstore_0",
    "dstore_1",
    "dstore_2",
    "dstore_3",
    "astore_0",
    "astore_1",
    "astore_2",
    "astore_3",
    "iastore",
    "lastore",
    "fastore",
    "dastore",
    "aastore",
    "bastore",
    "castore",
    "sastore",
    "pop",
    "pop2",
    "dup",
    "dup_x1",
    "dup_x2",
    "dup2",
    "dup2_x1",
    "dup2_x2",
    "swap",
    "iadd",
    "ladd",
    "fadd",
    "dadd",
    "isub",
    "lsub",
    "fsub",
    "dsub",
    "imul",
    "lmul",
    "fmul",
    "dmul",
    "idiv",
    "ldiv",
    "fdiv",
    "ddiv",
    "irem",
    "lrem",
    "frem",
    "drem",
    "ineg",
    "lneg",
    "fneg",
    "dneg",
    "ishl",
    "lshl",
    "ishr",
    "lshr",
    "iushr",
    "lushr",
    "iand",
    "land",
    "ior",
    "lor",
    "ixor",
    "lxor",
    "iinc",
    "i2l",
    "i2f",
    "i2d",
    "l2i",
    "l2f",
    "l2d",
    "f2i",
    "f2l",
    "f2d",
    "d2i",
    "d2l",
    "d2f",
    "i2b",
    "i2c",
    "i2s",
    "lcmp",
    "fcmpl",
    "fcmpg",
    "dcmpl",
    "dcmpg",
    "ifeq",
    "ifne",
    "iflt",
    "ifge",
    "ifgt",
    "ifle",
    "if_icmpeq",
    "if_icmpne",
    "if_icmplt",
    "if_icmpge",
    "if_icmpgt",
    "if_icmple",
    "if_acmpeq",
    "if_acmpne",
    "goto",
    "jsr",
    "ret",
    "tableswitch",
    "lookupswitch",
    "ireturn",
    "lreturn",
    "freturn",
    "dreturn",
    "areturn",
    "return",
    "getstatic",
    "putstatic",
    "getfield",
    "putfield",
    "invokevirtual",
    "invokespecial",
    "invokestatic",
    "invokeinterface",
    "invokedynamic",
    "new",
    "newarray",
    "anewarray",
    "arraylength",
    "athrow",
    "checkcast",
    "instanceof",
    "monitorenter",
    "monitorexit",
    "wide",
    "multianewarray",
    "ifnull",
    "ifnonnull",
    "goto_w",
    "jsr_w",
];

const HANDLE_KINDS: [&str; 10] = [
    "",
    "getfield",
    "getstatic",
    "putfield",
    "putstatic",
    "invokevirtual",
    "invokestatic",
    "invokespecial",
    "newinvokespecial",
    "invokeinterface",
];

fn access(flags: u16, kind: char) -> String {
    let table: &[(u16, &str)] = match kind {
        'c' => &[
            (0x0001, "public"),
            (0x0002, "private"),
            (0x0004, "protected"),
            (0x0008, "static"),
            (0x0010, "final"),
            (0x0200, "interface"),
            (0x0400, "abstract"),
            (0x1000, "synthetic"),
            (0x2000, "annotation"),
            (0x4000, "enum"),
            (0x8000, "module"),
        ],
        'f' => &[
            (0x0001, "public"),
            (0x0002, "private"),
            (0x0004, "protected"),
            (0x0008, "static"),
            (0x0010, "final"),
            (0x0040, "volatile"),
            (0x0080, "transient"),
            (0x1000, "synthetic"),
            (0x4000, "enum"),
        ],
        _ => &[
            (0x0001, "public"),
            (0x0002, "private"),
            (0x0004, "protected"),
            (0x0008, "static"),
            (0x0010, "final"),
            (0x0020, "synchronized"),
            (0x0040, "bridge"),
            (0x0080, "varargs"),
            (0x0100, "native"),
            (0x0400, "abstract"),
            (0x0800, "strictfp"),
            (0x1000, "synthetic"),
        ],
    };
    // Followed by a space when not empty.
    table.iter().filter(|(f, _)| flags & f != 0).map(|(_, n)| format!("{n} ")).collect()
}

struct W<'a> {
    p: &'a Program,
    cf: &'a ClassFile,
    out: &'a mut String,
    links: &'a mut Vec<(usize, usize, NodeKey, bool)>,
}

impl W<'_> {
    fn w(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn link(&mut self, text: &str, key: Option<NodeKey>, decl: bool) {
        if let Some(k) = key {
            self.links.push((self.out.len(), text.len(), k, decl));
        }
        self.w(text);
    }

    /// A type descriptor, linked when it names a class of the program.
    fn ty(&mut self, desc: &str) {
        let key = self
            .p
            .classes
            .get_key_value(desc.trim_start_matches('['))
            .map(|(k, _)| NodeKey::Class(k.clone()));
        self.link(desc, key, false);
    }

    fn class_ref(&mut self, i: u16) {
        let d = desc_of_internal(self.cf.class_name(i));
        self.ty(&d);
    }

    /// `Lpkg/Owner;->name(desc)` or `Lpkg/Owner;->name:desc`, linked to the member.
    fn member(&mut self, owner: &str, name: &str, desc: &str) {
        let field = !desc.starts_with('(');
        let key =
            if field { self.p.resolve_field(owner, name) } else { self.p.resolve_method(owner, name, desc) };
        let text = if field { format!("{owner}->{name}:{desc}") } else { format!("{owner}->{name}{desc}") };
        self.link(&text, key, false);
    }

    fn member_ref(&mut self, i: u16) {
        match self.cf.member_ref(i) {
            Some(r) => {
                let owner = desc_of_internal(r.class);
                self.member(&owner, r.name, r.desc);
            }
            None => {
                let _ = write!(self.out, "#{i}");
            }
        }
    }

    fn method_handle(&mut self, i: u16) {
        match self.cf.get(i) {
            Const::MethodHandle(kind, r) => {
                self.w(HANDLE_KINDS.get(*kind as usize).copied().unwrap_or("?"));
                self.w(" ");
                self.member_ref(*r);
            }
            _ => {
                let _ = write!(self.out, "#{i}");
            }
        }
    }

    /// A loadable constant (`ldc`, bootstrap arguments).
    fn constant(&mut self, i: u16) {
        let cf = self.cf;
        match cf.get(i) {
            Const::Int(v) => {
                let _ = write!(self.out, "{v}");
            }
            Const::Float(b) => {
                let _ = write!(self.out, "{:?}f", f32::from_bits(*b));
            }
            Const::Long(v) => {
                let _ = write!(self.out, "{v}L");
            }
            Const::Double(b) => {
                let _ = write!(self.out, "{:?}", f64::from_bits(*b));
            }
            Const::String(s) => {
                let _ = write!(self.out, "\"{}\"", escape(cf.utf8(*s)));
            }
            Const::Class(_) => self.class_ref(i),
            Const::MethodType(d) => {
                let _ = write!(self.out, "methodtype {}", cf.utf8(*d));
            }
            Const::MethodHandle(..) => self.method_handle(i),
            Const::Dynamic(b, nt) => {
                let (n, d) = cf.name_and_type(*nt);
                let _ = write!(self.out, "dynamic #{b} {n}:{d}");
            }
            c => {
                let _ = write!(self.out, "{c:?}");
            }
        }
    }
}

fn s16(code: &[u8], i: usize) -> i32 {
    i16::from_be_bytes([code.get(i).copied().unwrap_or(0), code.get(i + 1).copied().unwrap_or(0)]) as i32
}

fn s32(code: &[u8], i: usize) -> i32 {
    let b = |k| code.get(i + k).copied().unwrap_or(0);
    i32::from_be_bytes([b(0), b(1), b(2), b(3)])
}

fn u16_at(code: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([code.get(i).copied().unwrap_or(0), code.get(i + 1).copied().unwrap_or(0)])
}

fn print_code(w: &mut W<'_>, code: &Code) {
    let cf = w.cf;
    let bytes = &code.code;
    let _ = writeln!(w.out, "    .limit stack {} locals {}", code.max_stack, code.max_locals);
    let lines = cf.lines(code);
    let mut li = 0;
    let boots = cf.bootstraps();
    let mut pc = 0usize;
    while pc < bytes.len() {
        while li < lines.len() && lines[li].0 as usize <= pc {
            if lines[li].0 as usize == pc {
                let _ = writeln!(w.out, "    .line {}", lines[li].1);
            }
            li += 1;
        }
        let mut op = bytes[pc];
        let _ = write!(w.out, "    {pc:5}: ");
        let start = pc;
        let wide = op == 0xc4;
        if wide {
            w.w("wide ");
            op = bytes.get(pc + 1).copied().unwrap_or(0);
            pc += 1;
        }
        let Some(name) = OPCODES.get(op as usize) else {
            let _ = writeln!(w.out, "<unknown opcode {op:#04x}>");
            break;
        };
        w.w(name);
        let rel = |off: i32| (start as i64 + off as i64).to_string();
        let len = match op {
            0x10 => {
                let _ = write!(w.out, " {}", bytes.get(pc + 1).map_or(0, |&b| b as i8 as i32));
                2
            }
            0x11 => {
                let _ = write!(w.out, " {}", s16(bytes, pc + 1));
                3
            }
            0x12 => {
                w.w(" ");
                w.constant(bytes.get(pc + 1).copied().unwrap_or(0) as u16);
                2
            }
            0x13 | 0x14 => {
                w.w(" ");
                w.constant(u16_at(bytes, pc + 1));
                3
            }
            0x15..=0x19 | 0x36..=0x3a | 0xa9 => {
                if wide {
                    let _ = write!(w.out, " {}", u16_at(bytes, pc + 1));
                    3
                } else {
                    let _ = write!(w.out, " {}", bytes.get(pc + 1).copied().unwrap_or(0));
                    2
                }
            }
            0x84 => {
                if wide {
                    let _ = write!(w.out, " {}, {}", u16_at(bytes, pc + 1), s16(bytes, pc + 3));
                    5
                } else {
                    let v = bytes.get(pc + 2).map_or(0, |&b| b as i8 as i32);
                    let _ = write!(w.out, " {}, {v}", bytes.get(pc + 1).copied().unwrap_or(0));
                    3
                }
            }
            0x99..=0xa8 | 0xc6 | 0xc7 => {
                let _ = write!(w.out, " {}", rel(s16(bytes, pc + 1)));
                3
            }
            0xc8 | 0xc9 => {
                let _ = write!(w.out, " {}", rel(s32(bytes, pc + 1)));
                5
            }
            0xaa | 0xab => {
                let p = (pc + 4) & !3;
                let default = s32(bytes, p);
                let mut cases = vec![];
                let end = if op == 0xaa {
                    let (low, high) = (s32(bytes, p + 4), s32(bytes, p + 8));
                    let n = (high as i64 - low as i64 + 1).clamp(0, 65536) as usize;
                    for k in 0..n {
                        cases.push((low.wrapping_add(k as i32), s32(bytes, p + 12 + 4 * k)));
                    }
                    p + 12 + 4 * n
                } else {
                    let n = s32(bytes, p + 4).clamp(0, 65536) as usize;
                    for k in 0..n {
                        cases.push((s32(bytes, p + 8 + 8 * k), s32(bytes, p + 12 + 8 * k)));
                    }
                    p + 8 + 8 * n
                };
                w.w("\n");
                for (key, off) in cases {
                    let _ = writeln!(w.out, "               {key}: {}", rel(off));
                }
                let _ = write!(w.out, "               default: {}", rel(default));
                end - pc
            }
            0xb2..=0xb8 => {
                w.w(" ");
                w.member_ref(u16_at(bytes, pc + 1));
                3
            }
            0xb9 => {
                w.w(" ");
                w.member_ref(u16_at(bytes, pc + 1));
                5
            }
            0xba => {
                let i = u16_at(bytes, pc + 1);
                if let Const::InvokeDynamic(b, nt) = cf.get(i) {
                    let (n, d) = cf.name_and_type(*nt);
                    let _ = write!(w.out, " {n}{d}");
                    if let Some(bm) = boots.get(*b as usize) {
                        // The bootstrap method by owner and name (its descriptor is fixed).
                        w.w("  # ");
                        match cf.get(bm.method) {
                            Const::MethodHandle(_, r) => match cf.member_ref(*r) {
                                Some(m) => {
                                    let _ = write!(w.out, "{}->{}", desc_of_internal(m.class), m.name);
                                }
                                None => w.method_handle(bm.method),
                            },
                            _ => w.method_handle(bm.method),
                        }
                        for (k, &a) in bm.args.iter().enumerate() {
                            w.w(if k == 0 { " [" } else { ", " });
                            w.constant(a);
                        }
                        if !bm.args.is_empty() {
                            w.w("]");
                        }
                    }
                }
                5
            }
            0xbb | 0xbd | 0xc0 | 0xc1 => {
                w.w(" ");
                w.class_ref(u16_at(bytes, pc + 1));
                3
            }
            0xbc => {
                let t = bytes.get(pc + 1).copied().unwrap_or(0);
                let names = ["boolean", "char", "float", "double", "byte", "short", "int", "long"];
                let _ = write!(w.out, " {}", names.get((t as usize).wrapping_sub(4)).unwrap_or(&"?"));
                2
            }
            0xc5 => {
                w.w(" ");
                w.class_ref(u16_at(bytes, pc + 1));
                let _ = write!(w.out, ", {}", bytes.get(pc + 3).copied().unwrap_or(0));
                4
            }
            _ => 1,
        };
        w.w("\n");
        pc += len;
    }
    for h in &code.handlers {
        w.w("    .catch ");
        if h.catch == 0 {
            w.w("all");
        } else {
            w.class_ref(h.catch);
        }
        let _ = writeln!(w.out, " from {} to {} using {}", h.start, h.end, h.handler);
    }
    let lvtt = cf.locals(code, "LocalVariableTypeTable");
    for e in cf.locals(code, "LocalVariableTable") {
        let _ = write!(w.out, "    .local {} {} ", e.index, cf.utf8(e.name));
        w.ty(cf.utf8(e.desc));
        if let Some(t) = lvtt.iter().find(|t| t.index == e.index && t.start == e.start) {
            let _ = write!(w.out, " \"{}\"", escape(cf.utf8(t.desc)));
        }
        let _ = writeln!(w.out, " from {} to {}", e.start, e.start + e.len);
    }
}

fn print_member_attrs(w: &mut W<'_>, m: &Member) {
    let cf = w.cf;
    if let Some(s) = cf.index_attr(&m.attrs, "Signature") {
        let _ = writeln!(w.out, "    .signature \"{}\"", escape(cf.utf8(s)));
    }
    for t in cf.index_list(&m.attrs, "Exceptions") {
        w.w("    .throws ");
        w.class_ref(t);
        w.w("\n");
    }
    for a in &m.attrs {
        let n = cf.utf8(a.name);
        if n.contains("Annotations") || n == "AnnotationDefault" || n == "MethodParameters" {
            let _ = writeln!(w.out, "    # {n} ({} bytes)", a.data.len());
        }
    }
}

fn print_one(w: &mut W<'_>, desc: &Arc<str>, cf: &ClassFile) {
    let src = cf.index_attr(&cf.attrs, "SourceFile").map(|i| cf.utf8(i).to_owned());
    let _ = writeln!(
        w.out,
        "# class file {}{}",
        cf.major,
        src.map(|s| format!(", compiled from {s}")).unwrap_or_default()
    );
    let _ = write!(w.out, ".class {}", access(cf.access & !0x20, 'c'));
    w.link(desc, Some(NodeKey::Class(desc.clone())), true);
    w.w("\n");
    if cf.superclass != 0 {
        w.w(".super ");
        w.class_ref(cf.superclass);
        w.w("\n");
    }
    for &i in &cf.interfaces {
        w.w(".implements ");
        w.class_ref(i);
        w.w("\n");
    }
    if let Some(s) = cf.index_attr(&cf.attrs, "Signature") {
        let _ = writeln!(w.out, ".signature \"{}\"", escape(cf.utf8(s)));
    }
    for e in cf.inner_classes() {
        let inner = desc_of_internal(cf.class_name(e.inner));
        let _ = write!(w.out, ".inner {}", access(e.access, 'c'));
        if e.name != 0 {
            let _ = write!(w.out, "{} ", cf.utf8(e.name));
        }
        w.ty(&inner);
        if e.outer != 0 {
            w.w(" in ");
            w.class_ref(e.outer);
        }
        w.w("\n");
    }
    if let Some(d) = cf.attr(&cf.attrs, "EnclosingMethod") {
        let mut r = Be::new(d);
        if let (Ok(c), Ok(nt)) = (r.u16(), r.u16()) {
            w.w(".enclosing method ");
            if nt == 0 {
                w.class_ref(c);
            } else {
                let (n, md) = cf.name_and_type(nt);
                let owner = desc_of_internal(cf.class_name(c));
                w.member(&owner, n, md);
            }
            w.w("\n");
        }
    }
    w.w("\n");
    for f in &cf.fields {
        let (name, fd) = (cf.utf8(f.name), cf.utf8(f.desc));
        let _ = write!(w.out, ".field {}", access(f.access, 'f'));
        let key = w.p.resolve_field(desc, name);
        w.link(name, key, true);
        w.w(" ");
        w.ty(fd);
        if let Some(c) = cf.index_attr(&f.attrs, "ConstantValue") {
            w.w(" = ");
            w.constant(c);
        }
        w.w("\n");
        print_member_attrs(w, f);
    }
    if !cf.fields.is_empty() {
        w.w("\n");
    }
    for m in &cf.methods {
        let (name, md) = (cf.utf8(m.name), cf.utf8(m.desc));
        let _ = write!(w.out, ".method {}", access(m.access, 'm'));
        let key = w.p.resolve_method(desc, name, md);
        w.link(name, key, true);
        let _ = writeln!(w.out, "{md}");
        print_member_attrs(w, m);
        match cf.code(m) {
            Ok(Some(code)) => print_code(w, &code),
            Ok(None) => {}
            Err(e) => {
                let _ = writeln!(w.out, "    # invalid Code attribute: {e:#}");
            }
        }
        w.w(".end method\n\n");
    }
}

/// The bytecode of a top-level class and its nested classes.
pub fn print_class(p: &Program, top: &str) -> Printed {
    let (mut out, mut links) = (String::new(), vec![]);
    let mut stack = vec![Arc::<str>::from(top)];
    while let Some(c) = stack.pop() {
        if let Some(cf) = p.class_file(&c) {
            if !out.is_empty() {
                out.push_str(
                    "# ------------------------------------------------------------ nested class\n\n",
                );
            }
            let key = p.classes.get_key_value(&*c).map_or_else(|| c.clone(), |(k, _)| k.clone());
            print_one(&mut W { p, cf, out: &mut out, links: &mut links }, &key, cf);
        }
        if let Some(m) = p.members.get(&c) {
            stack.extend(m.iter().rev().cloned());
        }
    }
    Printed { code: out, links }
}
