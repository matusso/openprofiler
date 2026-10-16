//! Cross references: which classes, methods and fields a class's code mentions, gathered from
//! bytecode and declarations without decompiling.

use crate::insn::{self, IndexKind};
use crate::program::{NodeKey, Program};
use std::collections::HashSet;
use std::sync::Arc;

fn base_type(desc: &str) -> &str {
    desc.trim_start_matches('[')
}

fn add_type(p: &Program, desc: &str, out: &mut HashSet<NodeKey>) {
    let b = base_type(desc);
    if b.starts_with('L')
        && let Some((k, _)) = p.classes.get_key_value(b)
    {
        out.insert(NodeKey::Class(k.clone()));
    }
}

/// Adds every node referenced by class `desc` (not its nested classes) to `out`.
pub fn class_refs(p: &Program, desc: &Arc<str>, out: &mut HashSet<NodeKey>) {
    let Some((d, c)) = p.class(desc) else { return };
    if let Some(s) = c.superclass {
        add_type(p, d.type_desc(s), out);
    }
    for &i in &c.interfaces {
        add_type(p, d.type_desc(i), out);
    }
    let Ok(cd) = d.class_data(c) else { return };
    for f in cd.static_fields.iter().chain(&cd.instance_fields) {
        if let Some(fi) = d.field(f.field) {
            add_type(p, d.type_desc(fi.ty), out);
        }
    }
    for m in cd.direct_methods.iter().chain(&cd.virtual_methods) {
        if let Some(mi) = d.method(m.method)
            && let Some(pr) = d.proto(mi.proto)
        {
            add_type(p, d.type_desc(pr.ret), out);
            for &t in &pr.params {
                add_type(p, d.type_desc(t), out);
            }
        }
        if m.code_off == 0 {
            continue;
        }
        let Ok(code) = d.code(m.code_off) else { continue };
        let Ok((insns, _)) = insn::decode_all(&code.insns) else { continue };
        for i in insns {
            match i.index_kind() {
                IndexKind::Type => add_type(p, d.type_desc(i.index), out),
                IndexKind::Method => {
                    if let Some(mi) = d.method(i.index) {
                        let class = d.type_desc(mi.class);
                        add_type(p, class, out);
                        let name = d.string(mi.name);
                        let pd = d.proto_desc(mi.proto);
                        if let Some(k) = p.resolve_method(class, name, &pd) {
                            out.insert(k);
                        }
                    }
                }
                IndexKind::Field => {
                    if let Some(fi) = d.field(i.index) {
                        let class = d.type_desc(fi.class);
                        add_type(p, class, out);
                        if let Some(k) = p.resolve_field(class, d.string(fi.name)) {
                            out.insert(k);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
