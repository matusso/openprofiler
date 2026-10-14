//! Class metadata and instance field layouts.

use ember_hprof::{ty, type_size};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassKind {
    Instance,
    ObjArray,
    /// Primitive array with the HPROF basic type of its elements.
    PrimArray(u8),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldInfo {
    pub name: String,
    pub ty: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticInfo {
    pub name: String,
    pub ty: u8,
    pub value: u64,
}

/// A class from the dump (or a synthetic class for primitive arrays without a class dump).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassInfo {
    /// Address of the class object (`0` for synthetic classes).
    pub address: u64,
    /// Java source name (`java.util.HashMap$Node`, `int[]`).
    pub name: String,
    pub super_class: Option<u32>,
    pub loader: u64,
    pub signers: u64,
    pub protection_domain: u64,
    /// Object id of the class mirror (`u32::MAX` if not in the dump).
    pub object: u32,
    pub kind: ClassKind,
    /// Instance fields declared by this class (not inherited), in record order.
    pub fields: Vec<FieldInfo>,
    pub statics: Vec<StaticInfo>,
}

/// Object geometry used to compute shallow sizes, which HPROF does not record.
///
/// The rules follow Eclipse MAT's HPROF parser so retained sizes are directly comparable:
/// an object header is `pointer + ref` bytes, every class level of an instance is padded to
/// the reference size, and objects are padded to `align` (the GCD of object address
/// differences, at least 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sizes {
    /// Uncompressed pointer size (the dump's identifier size).
    pub pointer: u32,
    /// Size of a (possibly compressed) reference field.
    pub ref_size: u32,
    /// Object alignment.
    pub align: u32,
}

impl Sizes {
    pub fn new(id_size: u32, compressed_refs: bool, align: u32) -> Self {
        let ref_size = if id_size == 8 && compressed_refs { 4 } else { id_size };
        Self { pointer: id_size, ref_size, align: align.max(8) }
    }

    pub fn field_size(&self, t: u8) -> u32 {
        if t == ty::OBJECT { self.ref_size } else { type_size(t, 8) }
    }

    pub fn object_array(&self, len: u32) -> u32 {
        align_to(
            self.pointer as u64 + self.ref_size as u64 + 4 + len as u64 * self.ref_size as u64,
            self.align,
        )
    }

    pub fn primitive_array(&self, len: u32, elem: u32) -> u32 {
        let header = align_to((self.pointer + self.ref_size + 4) as u64, self.ref_size) as u64;
        align_to(header + len as u64 * elem as u64, self.align)
    }

    /// Size of a class object: its static fields only.
    pub fn class_object(&self, statics: impl Iterator<Item = u8>) -> u32 {
        align_to(statics.map(|t| self.field_size(t) as u64).sum(), self.align)
    }
}

pub fn align_to(v: u64, a: u32) -> u32 {
    v.next_multiple_of(a.max(1) as u64).min(u32::MAX as u64) as u32
}

#[derive(Debug, Clone)]
pub struct LayoutField {
    pub name: String,
    pub ty: u8,
    /// Byte offset within the instance record data.
    pub offset: u32,
    /// Declaring class.
    pub declaring: u32,
}

/// Flattened instance layout: own fields first, then superclass fields (HPROF record order).
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub fields: Vec<LayoutField>,
    pub ref_offsets: Vec<u32>,
    /// Offset of `java.lang.ref.Reference.referent`, for Reference subclasses.
    pub referent_offset: Option<u32>,
    pub shallow: u32,
}

impl Layout {
    pub fn field(&self, name: &str) -> Option<&LayoutField> {
        self.fields.iter().find(|f| f.name == name)
    }
}

pub fn compute_layouts(classes: &[ClassInfo], id_size: u32, sizes: Sizes) -> Vec<Layout> {
    let reference = classes.iter().position(|c| c.name == "java.lang.ref.Reference").map(|i| i as u32);
    // Unpadded instance size per class: header for root classes, then each level's own fields
    // added to the superclass size and padded to the reference size.
    let mut level_size: Vec<Option<u32>> = vec![None; classes.len()];
    fn level(classes: &[ClassInfo], sizes: Sizes, memo: &mut [Option<u32>], i: usize, depth: u32) -> u32 {
        if let Some(v) = memo[i] {
            return v;
        }
        let c = &classes[i];
        let v = match c.super_class {
            Some(sup) if depth < 256 => {
                let own: u32 = c.fields.iter().map(|f| sizes.field_size(f.ty)).sum();
                align_to((own + level(classes, sizes, memo, sup as usize, depth + 1)) as u64, sizes.ref_size)
            }
            _ => sizes.pointer + sizes.ref_size,
        };
        memo[i] = Some(v);
        v
    }
    (0..classes.len())
        .map(|i| {
            let mut l = Layout::default();
            let mut off = 0u32;
            let mut cur = Some(i as u32);
            let mut guard = 0;
            while let Some(ci) = cur {
                let c = &classes[ci as usize];
                for f in &c.fields {
                    if f.ty == ty::OBJECT {
                        l.ref_offsets.push(off);
                        if Some(ci) == reference && f.name == "referent" {
                            l.referent_offset = Some(off);
                        }
                    }
                    l.fields.push(LayoutField { name: f.name.clone(), ty: f.ty, offset: off, declaring: ci });
                    off += type_size(f.ty, id_size);
                }
                cur = c.super_class;
                guard += 1;
                if guard > 256 {
                    break;
                }
            }
            l.shallow = align_to(level(classes, sizes, &mut level_size, i, 0) as u64, sizes.align);
            l
        })
        .collect()
}

/// Element type of an array class name in Java form (`int[]` → INT, `String[]` → OBJECT).
pub fn kind_from_name(name: &str) -> ClassKind {
    let Some(base) = name.strip_suffix("[]") else { return ClassKind::Instance };
    if base.ends_with("[]") {
        return ClassKind::ObjArray;
    }
    match base {
        "boolean" => ClassKind::PrimArray(ty::BOOLEAN),
        "char" => ClassKind::PrimArray(ty::CHAR),
        "float" => ClassKind::PrimArray(ty::FLOAT),
        "double" => ClassKind::PrimArray(ty::DOUBLE),
        "byte" => ClassKind::PrimArray(ty::BYTE),
        "short" => ClassKind::PrimArray(ty::SHORT),
        "int" => ClassKind::PrimArray(ty::INT),
        "long" => ClassKind::PrimArray(ty::LONG),
        _ => ClassKind::ObjArray,
    }
}

/// Package of a Java class name (`""` for the default package; arrays use their element type).
pub fn package_of(name: &str) -> &str {
    let base = name.trim_end_matches("[]");
    match base.rfind('.') {
        Some(i) => &base[..i],
        None => "",
    }
}
