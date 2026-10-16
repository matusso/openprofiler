//! JVM class files (`.class`, `.jar`, `.war`, `.jmod`) as a program model.
//!
//! Class files are converted into the same in-memory model the DEX reader produces: constant
//! pools become shared string/type/proto/field/method tables, class attributes become the
//! system annotations the printer understands (`Signature`, `InnerClass`, `EnclosingMethod`,
//! `Throws`), and method code is translated on demand from the operand stack to registers
//! ([`code`]). Everything downstream — lifting, structuring, Java printing, navigation, usages
//! and search — is shared with the Android decompiler.

pub mod classfile;
mod code;
pub mod disasm;

use crate::dex::{
    self, Annotation, Annotations, ClassData, ClassDef, CodeItem, DexFile, EncodedAnnotation, EncodedField,
    EncodedMethod, FieldId, MethodId, Proto, Value,
};
use crate::ir::{CallSite, Lambda, MethodRef};
use crate::types;
use anyhow::{Result, anyhow};
use classfile::{Be, ClassFile, Const, Member, desc_of_internal};
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

/// The shared index tables of a program model.
#[derive(Default)]
pub struct Pools {
    pub strings: Vec<Box<str>>,
    pub types: Vec<u32>,
    pub protos: Vec<Proto>,
    pub fields: Vec<FieldId>,
    pub methods: Vec<MethodId>,
}

#[derive(Default)]
struct Interner {
    pools: Pools,
    strings: HashMap<Box<str>, u32>,
    types: HashMap<u32, u32>,
    protos: HashMap<(u32, Vec<u32>), u32>,
    fields: HashMap<(u32, u32, u32), u32>,
    methods: HashMap<(u32, u32, u32), u32>,
}

impl Interner {
    fn string(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.strings.get(s) {
            return i;
        }
        let i = self.pools.strings.len() as u32;
        self.pools.strings.push(s.into());
        self.strings.insert(s.into(), i);
        i
    }

    fn ty(&mut self, desc: &str) -> u32 {
        let s = self.string(desc);
        if let Some(&i) = self.types.get(&s) {
            return i;
        }
        let i = self.pools.types.len() as u32;
        self.pools.types.push(s);
        self.types.insert(s, i);
        i
    }

    fn proto(&mut self, desc: &str) -> u32 {
        let (params, ret) = types::split_method_desc(desc);
        let ret = self.ty(&ret);
        let params: Vec<u32> = params.iter().map(|p| self.ty(p)).collect();
        let key = (ret, params);
        if let Some(&i) = self.protos.get(&key) {
            return i;
        }
        let shorty: String = std::iter::once(&*self.pools.strings[self.pools.types[ret as usize] as usize])
            .chain(key.1.iter().map(|&t| &*self.pools.strings[self.pools.types[t as usize] as usize]))
            .map(|d| if types::is_reference(d) { 'L' } else { d.chars().next().unwrap_or('V') })
            .collect();
        let shorty = self.string(&shorty);
        let i = self.pools.protos.len() as u32;
        self.pools.protos.push(Proto { shorty, ret: key.0, params: key.1.clone() });
        self.protos.insert(key, i);
        i
    }

    fn field(&mut self, class: &str, name: &str, ty: &str) -> u32 {
        let key = (self.ty(class), self.string(name), self.ty(ty));
        if let Some(&i) = self.fields.get(&key) {
            return i;
        }
        let i = self.pools.fields.len() as u32;
        self.pools.fields.push(FieldId { class: key.0, ty: key.2, name: key.1 });
        self.fields.insert(key, i);
        i
    }

    fn method(&mut self, class: &str, name: &str, desc: &str) -> u32 {
        let key = (self.ty(class), self.string(name), self.proto(desc));
        if let Some(&i) = self.methods.get(&key) {
            return i;
        }
        let i = self.pools.methods.len() as u32;
        self.pools.methods.push(MethodId { class: key.0, proto: key.2, name: key.1 });
        self.methods.insert(key, i);
        i
    }
}

/// Lookups the translator needs after the tables are frozen.
pub(crate) struct Lookup {
    strings: HashMap<Box<str>, u32>,
    types: HashMap<u32, u32>,
}

impl Lookup {
    fn string(&self, s: &str) -> Option<u32> {
        self.strings.get(s).copied()
    }

    fn ty(&self, desc: &str) -> Option<u32> {
        self.types.get(&self.string(desc)?).copied()
    }
}

/// Pool entries the translator synthesizes code with.
pub(crate) struct Globals {
    string_builder: u32,
    sb_init: u32,
    sb_to_string: u32,
    /// `append` overloads by argument descriptor (`Z C I J F D`, String, Object).
    sb_append: HashMap<&'static str, u32>,
    array_new_instance: u32,
    int_array: u32,
    /// `newarray` element types by `atype` (4..=11).
    prim_arrays: [u32; 8],
}

pub(crate) struct Concat {
    /// Argument descriptors of the call site.
    args: Vec<String>,
    pieces: Vec<Piece>,
}

pub(crate) enum Piece {
    Arg(usize),
    Const(u32),
}

pub(crate) enum Site {
    /// Index into the model's call sites.
    Call(u32),
    Concat(Concat),
}

pub(crate) struct Unit {
    cf: ClassFile,
    /// Per constant-pool entry: string (String, Utf8), type (Class), field, method, proto
    /// (MethodType) index; `u32::MAX` otherwise.
    map: Vec<u32>,
    /// `invokedynamic` entries by constant-pool index.
    sites: HashMap<u16, Site>,
}

/// Class files behind a [`DexFile`] built by [`build`]; the model's offsets index these tables
/// (offset = index + 1, 0 = none).
pub(crate) struct Classes {
    units: Vec<Unit>,
    class_data: Vec<ClassData>,
    annotations: Vec<Annotations>,
    statics: Vec<Vec<Value>>,
    /// `(unit, method index in the class file)` per code offset.
    codes: Vec<(u32, u16)>,
    lookup: Lookup,
    globals: Globals,
}

impl Classes {
    pub(crate) fn class_data(&self, off: u32) -> ClassData {
        off.checked_sub(1).and_then(|i| self.class_data.get(i as usize)).cloned().unwrap_or_default()
    }

    pub(crate) fn annotations(&self, off: u32) -> Annotations {
        off.checked_sub(1).and_then(|i| self.annotations.get(i as usize)).cloned().unwrap_or_default()
    }

    pub(crate) fn static_values(&self, off: u32) -> Vec<Value> {
        off.checked_sub(1).and_then(|i| self.statics.get(i as usize)).cloned().unwrap_or_default()
    }

    /// The class file behind class definition `def`.
    pub(crate) fn class_file(&self, def: usize) -> Option<&ClassFile> {
        self.units.get(def).map(|u| &u.cf)
    }

    pub(crate) fn code(&self, off: u32) -> Result<CodeItem> {
        let &(unit, method) = off
            .checked_sub(1)
            .and_then(|i| self.codes.get(i as usize))
            .ok_or_else(|| anyhow!("no code at {off}"))?;
        let u = &self.units[unit as usize];
        let m = &u.cf.methods[method as usize];
        code::translate(self, u, m)
    }
}

/// Whether the bytes are a class file.
pub fn is_class(data: &[u8]) -> bool {
    classfile::is_class(data)
}

const ACC_SUPER: u16 = 0x20;
const ACC_MODULE: u16 = 0x8000;

/// Builds a program model from class files (`(entry name, bytes)`). The first definition of a
/// class wins. Returns the model and warnings about skipped entries.
pub fn build(name: &str, files: Vec<(String, Vec<u8>)>) -> Result<(DexFile, Vec<String>)> {
    let parsed: Vec<(String, Result<ClassFile>)> =
        files.into_par_iter().map(|(n, data)| (n, ClassFile::parse(&data))).collect();
    let mut warnings = vec![];
    let mut seen = std::collections::HashSet::new();
    let mut cfs = vec![];
    for (entry, cf) in parsed {
        match cf {
            Ok(cf) => {
                if cf.access & ACC_MODULE != 0 || cf.this_name().ends_with("package-info") {
                    continue;
                }
                if seen.insert(cf.this_name().to_owned()) {
                    cfs.push(cf);
                }
            }
            Err(e) => warnings.push(format!("skipped {entry}: {e:#}")),
        }
    }
    let mut it = Interner::default();
    let globals = globals(&mut it);
    let mut classes = Classes {
        units: Vec::with_capacity(cfs.len()),
        class_data: vec![],
        annotations: vec![],
        statics: vec![],
        codes: vec![],
        lookup: Lookup { strings: HashMap::new(), types: HashMap::new() },
        globals,
    };
    let mut defs = Vec::with_capacity(cfs.len());
    let mut call_sites = vec![];
    for cf in cfs {
        let unit_index = classes.units.len() as u32;
        let map = map_pool(&cf, &mut it);
        let sites = map_sites(&cf, &mut it, &mut call_sites);
        let desc = desc_of_internal(cf.this_name());
        let class = it.ty(&desc);
        let superclass = (cf.superclass != 0).then(|| it.ty(&desc_of_internal(cf.class_name(cf.superclass))));
        let interfaces = cf.interfaces.iter().map(|&i| it.ty(&desc_of_internal(cf.class_name(i)))).collect();
        let source_file = cf.index_attr(&cf.attrs, "SourceFile").map(|i| it.string(cf.utf8(i)));
        let mut access = (cf.access & !ACC_SUPER) as u32;
        if cf.attr(&cf.attrs, "Synthetic").is_some() {
            access |= dex::ACC_SYNTHETIC;
        }

        let mut ann = Annotations { class: class_annotations(&cf, &mut it), ..Default::default() };
        let mut data = ClassData::default();
        let mut statics = vec![];
        for f in &cf.fields {
            let (fname, fdesc) = (cf.utf8(f.name), cf.utf8(f.desc));
            let idx = it.field(&desc, fname, fdesc);
            let mut access = f.access as u32;
            if cf.attr(&f.attrs, "Synthetic").is_some() {
                access |= dex::ACC_SYNTHETIC;
            }
            let set = member_annotations(&cf, f, &mut it);
            if !set.is_empty() {
                ann.fields.insert(idx, set);
            }
            let ef = EncodedField { field: idx, access };
            if access & dex::ACC_STATIC != 0 {
                let v = cf
                    .index_attr(&f.attrs, "ConstantValue")
                    .and_then(|i| constant_value(&cf, i, fdesc, &mut it))
                    .unwrap_or(Value::Null);
                statics.push(v);
                data.static_fields.push(ef);
            } else {
                data.instance_fields.push(ef);
            }
        }
        while statics.last() == Some(&Value::Null) {
            statics.pop();
        }
        for (mi, m) in cf.methods.iter().enumerate() {
            let (mname, mdesc) = (cf.utf8(m.name), cf.utf8(m.desc));
            let idx = it.method(&desc, mname, mdesc);
            let mut access = m.access as u32;
            if cf.attr(&m.attrs, "Synthetic").is_some() {
                access |= dex::ACC_SYNTHETIC;
            }
            if mname == "<init>" || mname == "<clinit>" {
                access |= dex::ACC_CONSTRUCTOR;
            }
            let mut set = member_annotations(&cf, m, &mut it);
            let throws = cf.index_list(&m.attrs, "Exceptions");
            if !throws.is_empty() {
                let list =
                    throws.iter().map(|&t| Value::Type(it.ty(&desc_of_internal(cf.class_name(t))))).collect();
                set.push(system(&mut it, "Ldalvik/annotation/Throws;", vec![("value", Value::Array(list))]));
            }
            if !set.is_empty() {
                ann.methods.insert(idx, set);
            }
            let params = param_annotations(&cf, m, &mut it);
            if params.iter().any(|p| !p.is_empty()) {
                ann.params.insert(idx, params);
            }
            let code_off = if m.attrs.iter().any(|a| cf.utf8(a.name) == "Code") {
                classes.codes.push((unit_index, mi as u16));
                classes.codes.len() as u32
            } else {
                0
            };
            let em = EncodedMethod { method: idx, access, code_off };
            let direct = access & (dex::ACC_STATIC | dex::ACC_PRIVATE | dex::ACC_CONSTRUCTOR) != 0;
            if direct { data.direct_methods.push(em) } else { data.virtual_methods.push(em) }
        }
        classes.class_data.push(data);
        let annotations_off = if ann.class.is_empty() && ann.fields.is_empty() && ann.methods.is_empty() {
            0
        } else {
            classes.annotations.push(ann);
            classes.annotations.len() as u32
        };
        let static_values_off = if statics.is_empty() {
            0
        } else {
            classes.statics.push(statics);
            classes.statics.len() as u32
        };
        defs.push(ClassDef {
            class,
            access,
            superclass,
            interfaces,
            source_file,
            annotations_off,
            class_data_off: classes.class_data.len() as u32,
            static_values_off,
        });
        classes.units.push(Unit { cf, map, sites });
    }
    let Interner { pools, strings, types, .. } = it;
    classes.lookup = Lookup { strings, types };
    Ok((DexFile::from_jvm(name, pools, defs, call_sites, classes), warnings))
}

fn globals(it: &mut Interner) -> Globals {
    const SB: &str = "Ljava/lang/StringBuilder;";
    let mut sb_append = HashMap::new();
    for t in ["Z", "C", "I", "J", "F", "D", "Ljava/lang/String;", "Ljava/lang/Object;"] {
        sb_append.insert(t, it.method(SB, "append", &format!("({t}){SB}")));
    }
    let mut prim_arrays = [0; 8];
    for (k, t) in ["[Z", "[C", "[F", "[D", "[B", "[S", "[I", "[J"].iter().enumerate() {
        prim_arrays[k] = it.ty(t);
    }
    Globals {
        string_builder: it.ty(SB),
        sb_init: it.method(SB, "<init>", "()V"),
        sb_to_string: it.method(SB, "toString", "()Ljava/lang/String;"),
        sb_append,
        array_new_instance: it.method(
            "Ljava/lang/reflect/Array;",
            "newInstance",
            "(Ljava/lang/Class;[I)Ljava/lang/Object;",
        ),
        int_array: it.ty("[I"),
        prim_arrays,
    }
}

/// Interns every constant-pool entry the code can refer to.
fn map_pool(cf: &ClassFile, it: &mut Interner) -> Vec<u32> {
    let mut map = vec![u32::MAX; cf.cp.len()];
    for (i, c) in cf.cp.iter().enumerate() {
        map[i] = match c {
            Const::Utf8(s) => it.string(s),
            Const::String(s) => it.string(cf.utf8(*s)),
            Const::Class(_) => {
                let d = desc_of_internal(cf.class_name(i as u16));
                // Array types for `anewarray` and component types for `multianewarray`.
                it.ty(&format!("[{d}"));
                let mut rest = d.as_str();
                while let Some(r) = rest.strip_prefix('[') {
                    it.ty(r);
                    rest = r;
                }
                it.ty(&d)
            }
            Const::Field(..) => {
                let Some(r) = cf.member_ref(i as u16) else { continue };
                it.field(&desc_of_internal(r.class), r.name, r.desc)
            }
            Const::Method(..) | Const::IMethod(..) => {
                let Some(r) = cf.member_ref(i as u16) else { continue };
                it.method(&desc_of_internal(r.class), r.name, r.desc)
            }
            Const::MethodType(d) => it.proto(cf.utf8(*d)),
            _ => continue,
        };
    }
    map
}

/// The target of a method handle constant: `(kind, class descriptor, name, descriptor)`.
fn method_handle(cf: &ClassFile, i: u16) -> Option<(u8, String, &str, &str)> {
    let Const::MethodHandle(kind, r) = cf.get(i) else { return None };
    let m = cf.member_ref(*r)?;
    Some((*kind, desc_of_internal(m.class), m.name, m.desc))
}

fn map_sites(cf: &ClassFile, it: &mut Interner, call_sites: &mut Vec<CallSite>) -> HashMap<u16, Site> {
    let boots = cf.bootstraps();
    let mut out = HashMap::new();
    for (i, c) in cf.cp.iter().enumerate() {
        let Const::InvokeDynamic(b, nt) = c else { continue };
        let (name, desc) = cf.name_and_type(*nt);
        let (params, ret) = types::split_method_desc(desc);
        let boot = boots.get(*b as usize);
        let bsm = boot.and_then(|b| method_handle(cf, b.method));
        let bsm_owner = bsm.as_ref().map_or("", |b| b.1.as_str());
        let bsm_name = bsm.as_ref().map_or("", |b| b.2);
        let site = match (bsm_owner, bsm_name, boot) {
            ("Ljava/lang/invoke/LambdaMetafactory;", "metafactory" | "altMetafactory", Some(b))
                if b.args.len() >= 3 =>
            {
                let target = method_handle(cf, b.args[1]);
                let inst = match cf.get(b.args[2]) {
                    Const::MethodType(d) => cf.utf8(*d),
                    _ => "",
                };
                target.map(|(kind, class, tname, tdesc)| {
                    let (tparams, tret) = types::split_method_desc(tdesc);
                    let (sam_params, sam_ret) = types::split_method_desc(inst);
                    // Make sure the printer can resolve the target as a node.
                    it.method(&class, tname, tdesc);
                    CallSite::Lambda(Arc::new(Lambda {
                        iface: ret.as_str().into(),
                        method: name.into(),
                        params: sam_params.into(),
                        ret: sam_ret.into(),
                        target: MethodRef {
                            class: class.into(),
                            name: tname.into(),
                            params: tparams.into(),
                            ret: tret.into(),
                        },
                        kind,
                    }))
                })
            }
            ("Ljava/lang/invoke/StringConcatFactory;", "makeConcatWithConstants", Some(b))
                if !b.args.is_empty() =>
            {
                let recipe = match cf.get(b.args[0]) {
                    Const::String(s) => cf.utf8(*s).to_owned(),
                    _ => String::new(),
                };
                let consts: Vec<String> = b.args[1..].iter().map(|&a| const_text(cf, a)).collect();
                out.insert(i as u16, Site::Concat(concat(&recipe, &consts, params, it)));
                continue;
            }
            ("Ljava/lang/invoke/StringConcatFactory;", "makeConcat", _) => {
                let pieces = (0..params.len()).map(Piece::Arg).collect();
                out.insert(i as u16, Site::Concat(Concat { args: params, pieces }));
                continue;
            }
            _ => None,
        };
        let site = site.unwrap_or_else(|| CallSite::Dynamic {
            name: name.into(),
            owner: bsm_owner.into(),
            bootstrap: format!("{}.{bsm_name}", types::java_name(bsm_owner)).into(),
            params: params.into(),
            ret: ret.into(),
        });
        call_sites.push(site);
        out.insert(i as u16, Site::Call(call_sites.len() as u32 - 1));
    }
    out
}

/// Text of a loadable constant used as a concatenation constant.
fn const_text(cf: &ClassFile, i: u16) -> String {
    match cf.get(i) {
        Const::String(s) => cf.utf8(*s).to_owned(),
        Const::Int(v) => v.to_string(),
        Const::Long(v) => v.to_string(),
        Const::Float(v) => f32::from_bits(*v).to_string(),
        Const::Double(v) => f64::from_bits(*v).to_string(),
        Const::Class(_) => types::java_name(&desc_of_internal(cf.class_name(i))),
        _ => String::new(),
    }
}

/// Splits a `makeConcatWithConstants` recipe: `\u{1}` is the next argument, `\u{2}` the next
/// constant, anything else literal text.
fn concat(recipe: &str, consts: &[String], args: Vec<String>, it: &mut Interner) -> Concat {
    let mut pieces = vec![];
    let mut text = String::new();
    let (mut arg, mut cst) = (0, 0);
    let flush = |text: &mut String, pieces: &mut Vec<Piece>, it: &mut Interner| {
        if !text.is_empty() {
            pieces.push(Piece::Const(it.string(text)));
            text.clear();
        }
    };
    for ch in recipe.chars() {
        match ch {
            '\u{1}' => {
                flush(&mut text, &mut pieces, it);
                pieces.push(Piece::Arg(arg));
                arg += 1;
            }
            '\u{2}' => {
                text.push_str(consts.get(cst).map_or("", String::as_str));
                cst += 1;
            }
            c => text.push(c),
        }
    }
    flush(&mut text, &mut pieces, it);
    Concat { args, pieces }
}

fn constant_value(cf: &ClassFile, i: u16, ty: &str, it: &mut Interner) -> Option<Value> {
    Some(match (cf.get(i), ty) {
        (Const::Int(v), "Z") => Value::Boolean(*v != 0),
        (Const::Int(v), "B") => Value::Byte(*v as i8),
        (Const::Int(v), "S") => Value::Short(*v as i16),
        (Const::Int(v), "C") => Value::Char(*v as u16),
        (Const::Int(v), _) => Value::Int(*v),
        (Const::Long(v), _) => Value::Long(*v),
        (Const::Float(v), _) => Value::Float(f32::from_bits(*v)),
        (Const::Double(v), _) => Value::Double(f64::from_bits(*v)),
        (Const::String(s), _) => Value::String(it.string(cf.utf8(*s))),
        _ => return None,
    })
}

fn system(it: &mut Interner, ty: &str, elements: Vec<(&str, Value)>) -> Annotation {
    Annotation {
        visibility: dex::VISIBILITY_SYSTEM,
        value: EncodedAnnotation {
            ty: it.ty(ty),
            elements: elements.into_iter().map(|(n, v)| (it.string(n), v)).collect(),
        },
    }
}

fn class_annotations(cf: &ClassFile, it: &mut Interner) -> Vec<Annotation> {
    let mut out = runtime_annotations(cf, &cf.attrs, it);
    if let Some(s) = cf.index_attr(&cf.attrs, "Signature") {
        let v = Value::Array(vec![Value::String(it.string(cf.utf8(s)))]);
        out.push(system(it, "Ldalvik/annotation/Signature;", vec![("value", v)]));
    }
    let this = cf.this_name();
    let own = cf.inner_classes().into_iter().find(|e| e.inner != 0 && cf.class_name(e.inner) == this);
    if let Some(e) = own {
        let name = if e.name == 0 { Value::Null } else { Value::String(it.string(cf.utf8(e.name))) };
        let access = Value::Int((e.access & !ACC_SUPER) as i32);
        out.push(system(it, "Ldalvik/annotation/InnerClass;", vec![("accessFlags", access), ("name", name)]));
        let mut enclosing = None;
        if e.outer != 0 {
            let t = it.ty(&desc_of_internal(cf.class_name(e.outer)));
            enclosing =
                Some(system(it, "Ldalvik/annotation/EnclosingClass;", vec![("value", Value::Type(t))]));
        } else if let Some(d) = cf.attr(&cf.attrs, "EnclosingMethod") {
            let mut r = Be::new(d);
            if let (Ok(c), Ok(nt)) = (r.u16(), r.u16()) {
                let owner = desc_of_internal(cf.class_name(c));
                enclosing = Some(if nt != 0 {
                    let (n, md) = cf.name_and_type(nt);
                    let m = it.method(&owner, n, md);
                    system(it, "Ldalvik/annotation/EnclosingMethod;", vec![("value", Value::Method(m))])
                } else {
                    let t = it.ty(&owner);
                    system(it, "Ldalvik/annotation/EnclosingClass;", vec![("value", Value::Type(t))])
                });
            }
        }
        out.extend(enclosing);
    }
    out
}

fn member_annotations(cf: &ClassFile, m: &Member, it: &mut Interner) -> Vec<Annotation> {
    let mut out = runtime_annotations(cf, &m.attrs, it);
    if let Some(s) = cf.index_attr(&m.attrs, "Signature") {
        let v = Value::Array(vec![Value::String(it.string(cf.utf8(s)))]);
        out.push(system(it, "Ldalvik/annotation/Signature;", vec![("value", v)]));
    }
    out
}

fn runtime_annotations(cf: &ClassFile, attrs: &[classfile::Attr], it: &mut Interner) -> Vec<Annotation> {
    let mut out = vec![];
    for (name, visibility) in [
        ("RuntimeVisibleAnnotations", dex::VISIBILITY_RUNTIME),
        ("RuntimeInvisibleAnnotations", dex::VISIBILITY_BUILD),
    ] {
        let Some(d) = cf.attr(attrs, name) else { continue };
        let mut r = Be::new(d);
        let n = r.u16().unwrap_or(0);
        for _ in 0..n {
            match annotation(cf, &mut r, it, 0) {
                Ok(value) => out.push(Annotation { visibility, value }),
                Err(_) => break,
            }
        }
    }
    out
}

fn param_annotations(cf: &ClassFile, m: &Member, it: &mut Interner) -> Vec<Vec<Annotation>> {
    let mut out: Vec<Vec<Annotation>> = vec![];
    for (name, visibility) in [
        ("RuntimeVisibleParameterAnnotations", dex::VISIBILITY_RUNTIME),
        ("RuntimeInvisibleParameterAnnotations", dex::VISIBILITY_BUILD),
    ] {
        let Some(d) = cf.attr(&m.attrs, name) else { continue };
        let mut r = Be::new(d);
        let n = r.u8().unwrap_or(0) as usize;
        if out.len() < n {
            out.resize_with(n, Vec::new);
        }
        'params: for p in out.iter_mut().take(n) {
            let k = r.u16().unwrap_or(0);
            for _ in 0..k {
                match annotation(cf, &mut r, it, 0) {
                    Ok(value) => p.push(Annotation { visibility, value }),
                    Err(_) => break 'params,
                }
            }
        }
    }
    out
}

fn annotation(cf: &ClassFile, r: &mut Be<'_>, it: &mut Interner, depth: u32) -> Result<EncodedAnnotation> {
    let ty = it.ty(cf.utf8(r.u16()?));
    let n = r.u16()?;
    let mut elements = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let name = it.string(cf.utf8(r.u16()?));
        elements.push((name, element_value(cf, r, it, depth)?));
    }
    Ok(EncodedAnnotation { ty, elements })
}

fn element_value(cf: &ClassFile, r: &mut Be<'_>, it: &mut Interner, depth: u32) -> Result<Value> {
    if depth > 32 {
        anyhow::bail!("annotation nested too deeply");
    }
    let tag = r.u8()?;
    let int = |i: u16| if let Const::Int(v) = cf.get(i) { *v } else { 0 };
    Ok(match tag {
        b'B' => Value::Byte(int(r.u16()?) as i8),
        b'C' => Value::Char(int(r.u16()?) as u16),
        b'S' => Value::Short(int(r.u16()?) as i16),
        b'Z' => Value::Boolean(int(r.u16()?) != 0),
        b'I' => Value::Int(int(r.u16()?)),
        b'J' => Value::Long(if let Const::Long(v) = cf.get(r.u16()?) { *v } else { 0 }),
        b'F' => Value::Float(if let Const::Float(v) = cf.get(r.u16()?) { f32::from_bits(*v) } else { 0.0 }),
        b'D' => Value::Double(if let Const::Double(v) = cf.get(r.u16()?) { f64::from_bits(*v) } else { 0.0 }),
        b's' => Value::String(it.string(cf.utf8(r.u16()?))),
        b'e' => {
            let (t, n) = (r.u16()?, r.u16()?);
            let ty = cf.utf8(t).to_owned();
            Value::Enum(it.field(&ty, cf.utf8(n), &ty))
        }
        b'c' => Value::Type(it.ty(cf.utf8(r.u16()?))),
        b'@' => Value::Annotation(annotation(cf, r, it, depth + 1)?),
        b'[' => {
            let n = r.u16()?;
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                items.push(element_value(cf, r, it, depth + 1)?);
            }
            Value::Array(items)
        }
        t => anyhow::bail!("unknown element value tag {t}"),
    })
}
