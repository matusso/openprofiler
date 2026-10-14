//! Heap inspections ("Insights"): duplicate strings and arrays, empty and sparse collections,
//! bad hash distribution, ThreadLocal leaks, classloader leaks, big collections, inner-class
//! retention. Results are cached in memory and persisted next to the index.

use crate::classes::ClassKind;
use crate::{HeapAnalysis, Insight, InspectionResult, InspectionRow, Severity};
use anyhow::Result;
use openprofiler_hprof::{SubRecord, ty};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

pub const KINDS: &[&str] = &[
    "duplicate_strings",
    "duplicate_arrays",
    "empty_collections",
    "sparse_arrays",
    "zero_arrays",
    "bad_hash_distribution",
    "thread_locals",
    "classloader_leaks",
    "big_collections",
    "inner_class_retention",
];

const MAX_ROWS: usize = 1000;

/// `(class name, size field, backing storage field)`
const COLLECTIONS: &[(&str, &str, &str)] = &[
    ("java.util.ArrayList", "size", "elementData"),
    ("java.util.HashMap", "size", "table"),
    ("java.util.Hashtable", "count", "table"),
    ("java.util.Vector", "elementCount", "elementData"),
    ("java.util.IdentityHashMap", "size", "table"),
    ("java.util.WeakHashMap", "size", "table"),
    ("java.util.TreeMap", "size", ""),
    ("java.util.concurrent.ConcurrentHashMap", "baseCount", "table"),
];

fn severity(wasted: u64, total: u64) -> Severity {
    let f = wasted as f64 / total.max(1) as f64;
    if f >= 0.2 {
        Severity::Critical
    } else if f >= 0.05 {
        Severity::Warning
    } else {
        Severity::Info
    }
}

fn mb(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GB", b as f64 / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.1} KB", b as f64 / 1024.0)
    }
}

impl HeapAnalysis {
    /// Runs (or loads) every inspection and returns their headline findings.
    pub fn inspections(&self) -> Vec<Insight> {
        KINDS.iter().filter_map(|k| self.inspection_full(k).ok()).map(|r| r.insight.clone()).collect()
    }

    /// One inspection with up to `limit` detail rows.
    pub fn inspection(&self, kind: &str, limit: usize) -> Result<InspectionResult> {
        let r = self.inspection_full(kind)?;
        Ok(InspectionResult {
            insight: r.insight.clone(),
            rows: r.rows.iter().take(limit).cloned().collect(),
        })
    }

    fn inspection_full(&self, kind: &str) -> Result<Arc<InspectionResult>> {
        anyhow::ensure!(KINDS.contains(&kind), "unknown inspection {kind}");
        if let Some(r) = self.inspection_cache.lock().unwrap().get(kind) {
            return Ok(r.clone());
        }
        let file = self.dir.join(format!("inspection-{kind}.json"));
        let r =
            match std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<InspectionResult>(&b).ok())
            {
                Some(r) => r,
                None => {
                    let mut r = self.run_inspection(kind);
                    r.rows.truncate(MAX_ROWS);
                    let _ = std::fs::write(&file, serde_json::to_vec(&r)?);
                    r
                }
            };
        let r = Arc::new(r);
        self.inspection_cache.lock().unwrap().insert(kind.to_owned(), r.clone());
        Ok(r)
    }

    fn classes_where(&self, pred: impl Fn(u32) -> bool) -> Vec<bool> {
        (0..self.classes.len() as u32).map(pred).collect()
    }

    fn instances_of(&self, mask: &[bool]) -> Vec<u32> {
        (0..self.n() as u32)
            .into_par_iter()
            .filter(|&o| mask[self.class[o as usize] as usize] && self.reachable(o))
            .collect()
    }

    fn total(&self) -> u64 {
        self.meta.total_shallow
    }

    /// Non-null element count of an object array (from the outgoing CSR).
    fn non_null_elements(&self, arr: u32) -> u64 {
        let m = self.classes[self.class[arr as usize] as usize].object;
        let d = self.out_edges(arr).len() as u64;
        if m != u32::MAX { d.saturating_sub(1) } else { d }
    }

    fn run_inspection(&self, kind: &str) -> InspectionResult {
        match kind {
            "duplicate_strings" => self.dup_strings(),
            "duplicate_arrays" => self.dup_arrays(),
            "empty_collections" => self.empty_collections(),
            "sparse_arrays" => self.sparse_arrays(),
            "zero_arrays" => self.zero_arrays(),
            "bad_hash_distribution" => self.bad_hash(),
            "thread_locals" => self.thread_locals(),
            "classloader_leaks" => self.classloader_leaks(),
            "big_collections" => self.big_collections(),
            _ => self.inner_classes(),
        }
    }

    fn string_mask(&self) -> Vec<bool> {
        self.classes_where(|c| self.classes[c as usize].name == "java.lang.String")
    }

    fn dup_strings(&self) -> InspectionResult {
        let strings = self.instances_of(&self.string_mask());
        // (content hash, string, value array)
        let mut keyed: Vec<(u128, u32, u32)> = strings
            .par_iter()
            .filter_map(|&s| {
                let v = self.field_ref(s, "value")?;
                let coder = self.field_int(s, "coder").unwrap_or(0);
                let SubRecord::PrimArray { data, .. } = self.record(v)? else { return None };
                let mut h = xxhash_rust::xxh3::Xxh3::new();
                h.update(&[coder as u8]);
                h.update(data);
                Some((h.digest128(), s, v))
            })
            .collect();
        keyed.par_sort_unstable();
        let mut rows = vec![];
        let (mut wasted_total, mut dup_total) = (0u64, 0u64);
        for group in keyed.chunk_by(|a, b| a.0 == b.0) {
            if group.len() < 2 {
                continue;
            }
            let mut arrays: Vec<u32> = group.iter().map(|g| g.2).collect();
            arrays.sort_unstable();
            arrays.dedup();
            let s0 = group[0].1;
            let wasted = (group.len() as u64 - 1) * self.shallow_size(s0)
                + (arrays.len() as u64).saturating_sub(1) * self.shallow_size(group[0].2);
            if wasted == 0 {
                continue;
            }
            wasted_total += wasted;
            dup_total += group.len() as u64 - 1;
            let text = self.string_value(s0).unwrap_or_default();
            rows.push(InspectionRow {
                label: crate::quote(&text, 100),
                count: group.len() as u64,
                bytes: wasted,
                object: Some(self.object_row(s0)),
            });
        }
        rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(b.count.cmp(&a.count)));
        let groups = rows.len();
        InspectionResult {
            insight: Insight {
                kind: "duplicate_strings".into(),
                title: "Duplicate strings".into(),
                severity: severity(wasted_total, self.total()),
                summary: format!(
                    "{dup_total} duplicate strings in {groups} distinct values waste {}",
                    mb(wasted_total)
                ),
                wasted_bytes: Some(wasted_total),
                count: Some(dup_total),
            },
            rows,
        }
    }

    fn string_value_arrays(&self) -> FxHashSet<u32> {
        let strings = self.instances_of(&self.string_mask());
        strings.par_iter().filter_map(|&s| self.field_ref(s, "value")).collect()
    }

    fn prim_arrays(&self) -> Vec<u32> {
        let mask = self.classes_where(|c| matches!(self.classes[c as usize].kind, ClassKind::PrimArray(_)));
        self.instances_of(&mask)
    }

    fn dup_arrays(&self) -> InspectionResult {
        let string_values = self.string_value_arrays();
        let arrays = self.prim_arrays();
        let mut keyed: Vec<(u128, u32)> = arrays
            .par_iter()
            .filter(|a| !string_values.contains(a))
            .filter_map(|&a| {
                let SubRecord::PrimArray { ty: t, len, data, .. } = self.record(a)? else { return None };
                if len == 0 {
                    return None;
                }
                let mut h = xxhash_rust::xxh3::Xxh3::new();
                h.update(&[t]);
                h.update(data);
                Some((h.digest128(), a))
            })
            .collect();
        keyed.par_sort_unstable();
        let mut rows = vec![];
        let (mut wasted_total, mut dup_total) = (0u64, 0u64);
        for g in keyed.chunk_by(|a, b| a.0 == b.0) {
            if g.len() < 2 {
                continue;
            }
            let a0 = g[0].1;
            let wasted = (g.len() as u64 - 1) * self.shallow_size(a0);
            wasted_total += wasted;
            dup_total += g.len() as u64 - 1;
            let row = self.object_row(a0);
            rows.push(InspectionRow {
                label: format!("{} ({})", row.class_name, row.preview.clone().unwrap_or_default()),
                count: g.len() as u64,
                bytes: wasted,
                object: Some(row),
            });
        }
        rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(b.count.cmp(&a.count)));
        InspectionResult {
            insight: Insight {
                kind: "duplicate_arrays".into(),
                title: "Duplicate primitive arrays".into(),
                severity: severity(wasted_total, self.total()),
                summary: format!("{dup_total} duplicate arrays waste {}", mb(wasted_total)),
                wasted_bytes: Some(wasted_total),
                count: Some(dup_total),
            },
            rows,
        }
    }

    fn zero_arrays(&self) -> InspectionResult {
        let arrays = self.prim_arrays();
        let zero: Vec<u32> = arrays
            .par_iter()
            .copied()
            .filter(|&a| match self.record(a) {
                Some(SubRecord::PrimArray { len, data, .. }) => len > 0 && data.iter().all(|&b| b == 0),
                _ => false,
            })
            .collect();
        let mut by_class: FxHashMap<u32, (u64, u64)> = FxHashMap::default();
        for &a in &zero {
            let e = by_class.entry(self.class_of(a)).or_default();
            e.0 += 1;
            e.1 += self.shallow_size(a);
        }
        let wasted: u64 = by_class.values().map(|v| v.1).sum();
        let mut rows: Vec<_> = by_class
            .into_iter()
            .map(|(c, (n, b))| InspectionRow {
                label: self.class_name(c).to_owned(),
                count: n,
                bytes: b,
                object: None,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.bytes));
        InspectionResult {
            insight: Insight {
                kind: "zero_arrays".into(),
                title: "Zero-filled arrays".into(),
                severity: severity(wasted, self.total()),
                summary: format!("{} primitive arrays contain only zeros ({})", zero.len(), mb(wasted)),
                wasted_bytes: Some(wasted),
                count: Some(zero.len() as u64),
            },
            rows,
        }
    }

    fn sparse_arrays(&self) -> InspectionResult {
        let mask = self.classes_where(|c| self.classes[c as usize].kind == ClassKind::ObjArray);
        let arrays = self.instances_of(&mask);
        let ref_size = self.meta.sizes.ref_size as u64;
        let mut found: Vec<(u32, u64)> = arrays
            .par_iter()
            .filter_map(|&a| {
                let len = self.array_len(a)? as u64;
                if len < 16 {
                    return None;
                }
                let used = self.non_null_elements(a);
                (used * 4 < len).then(|| (a, (len - used) * ref_size))
            })
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.1));
        let wasted: u64 = found.iter().map(|f| f.1).sum();
        let rows = found
            .iter()
            .take(MAX_ROWS)
            .map(|&(a, w)| {
                let row = self.object_row(a);
                InspectionRow {
                    label: format!(
                        "{} with {} of {} slots used",
                        row.class_name,
                        self.non_null_elements(a),
                        self.array_len(a).unwrap_or(0)
                    ),
                    count: 1,
                    bytes: w,
                    object: Some(row),
                }
            })
            .collect();
        InspectionResult {
            insight: Insight {
                kind: "sparse_arrays".into(),
                title: "Sparse object arrays".into(),
                severity: severity(wasted, self.total()),
                summary: format!(
                    "{} object arrays are less than 25% full; null slots use {}",
                    found.len(),
                    mb(wasted)
                ),
                wasted_bytes: Some(wasted),
                count: Some(found.len() as u64),
            },
            rows,
        }
    }

    /// Class index → index into [`COLLECTIONS`] (including subclasses).
    fn collection_kinds(&self) -> Vec<Option<usize>> {
        (0..self.classes.len() as u32)
            .map(|c| {
                self.superclasses(c).find_map(|s| {
                    COLLECTIONS.iter().position(|(n, _, _)| *n == self.classes[s as usize].name)
                })
            })
            .collect()
    }

    fn empty_collections(&self) -> InspectionResult {
        let kinds = self.collection_kinds();
        let mask: Vec<bool> = kinds.iter().map(|k| k.is_some()).collect();
        let objs = self.instances_of(&mask);
        let empty: Vec<(u32, u64)> = objs
            .par_iter()
            .filter_map(|&o| {
                let (_, size_f, backing_f) = COLLECTIONS[kinds[self.class_of(o) as usize]?];
                if backing_f.is_empty() || self.field_int(o, size_f)? != 0 {
                    return None;
                }
                let backing = self.field_ref(o, backing_f)?;
                (self.array_len(backing)? > 0).then(|| (o, self.shallow_size(backing)))
            })
            .collect();
        let mut by_class: FxHashMap<u32, (u64, u64)> = FxHashMap::default();
        for &(o, w) in &empty {
            let e = by_class.entry(self.class_of(o)).or_default();
            e.0 += 1;
            e.1 += w;
        }
        let wasted: u64 = by_class.values().map(|v| v.1).sum();
        let mut rows: Vec<_> = by_class
            .into_iter()
            .map(|(c, (n, b))| InspectionRow {
                label: self.class_name(c).to_owned(),
                count: n,
                bytes: b,
                object: None,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.bytes));
        InspectionResult {
            insight: Insight {
                kind: "empty_collections".into(),
                title: "Empty collections with allocated storage".into(),
                severity: severity(wasted, self.total()),
                summary: format!(
                    "{} empty collections hold {} of unused backing arrays",
                    empty.len(),
                    mb(wasted)
                ),
                wasted_bytes: Some(wasted),
                count: Some(empty.len() as u64),
            },
            rows,
        }
    }

    fn bad_hash(&self) -> InspectionResult {
        let kinds = self.collection_kinds();
        let mask: Vec<bool> = kinds
            .iter()
            .map(|k| {
                k.is_some_and(|i| matches!(COLLECTIONS[i].0, "java.util.HashMap" | "java.util.Hashtable"))
            })
            .collect();
        let objs = self.instances_of(&mask);
        let mut found: Vec<(u32, i64, u64)> = objs
            .par_iter()
            .filter_map(|&o| {
                let (_, size_f, _) = COLLECTIONS[kinds[self.class_of(o) as usize]?];
                let size = self.field_int(o, size_f)?;
                if size < 16 {
                    return None;
                }
                let table = self.field_ref(o, "table")?;
                let used = self.non_null_elements(table).max(1);
                (size as u64 >= used * 4).then_some((o, size, used))
            })
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.1 as u64 / f.2));
        let rows = found
            .iter()
            .take(MAX_ROWS)
            .map(|&(o, size, used)| InspectionRow {
                label: format!("{size} entries in {used} buckets"),
                count: size as u64,
                bytes: self.retained_size(o),
                object: Some(self.object_row(o)),
            })
            .collect();
        InspectionResult {
            insight: Insight {
                kind: "bad_hash_distribution".into(),
                title: "Hash maps with poor key distribution".into(),
                severity: if found.is_empty() { Severity::Info } else { Severity::Warning },
                summary: format!(
                    "{} hash maps average 4+ entries per used bucket (weak hashCode?)",
                    found.len()
                ),
                wasted_bytes: None,
                count: Some(found.len() as u64),
            },
            rows,
        }
    }

    fn thread_locals(&self) -> InspectionResult {
        let mask = self
            .classes_where(|c| self.classes[c as usize].name == "java.lang.ThreadLocal$ThreadLocalMap$Entry");
        let entries = self.instances_of(&mask);
        // (value class, stale, retained of value)
        let info: Vec<(u32, bool, u64)> = entries
            .par_iter()
            .filter_map(|&e| {
                let v = self.field_ref(e, "value")?;
                let stale = self.field_ref(e, "referent").is_none();
                Some((self.class_of(v), stale, self.retained_size(v)))
            })
            .collect();
        let mut by_class: FxHashMap<u32, (u64, u64, u64)> = FxHashMap::default();
        for &(c, stale, r) in &info {
            let e = by_class.entry(c).or_default();
            e.0 += 1;
            e.1 += stale as u64;
            e.2 += r;
        }
        let stale: u64 = info.iter().filter(|i| i.1).count() as u64;
        let stale_bytes: u64 = info.iter().filter(|i| i.1).map(|i| i.2).sum();
        let total_bytes: u64 = info.iter().map(|i| i.2).sum();
        let mut rows: Vec<_> = by_class
            .into_iter()
            .map(|(c, (n, s, b))| InspectionRow {
                label: format!("{} ({s} stale)", self.class_name(c)),
                count: n,
                bytes: b,
                object: None,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.bytes));
        InspectionResult {
            insight: Insight {
                kind: "thread_locals".into(),
                title: "ThreadLocal values".into(),
                severity: if stale_bytes > 1 << 20 { Severity::Warning } else { Severity::Info },
                summary: format!(
                    "{} ThreadLocal entries retain {}; {stale} stale entries (cleared key) retain {}",
                    info.len(),
                    mb(total_bytes),
                    mb(stale_bytes)
                ),
                wasted_bytes: Some(stale_bytes),
                count: Some(info.len() as u64),
            },
            rows,
        }
    }

    fn classloader_leaks(&self) -> InspectionResult {
        let mut by_name: FxHashMap<&str, Vec<u64>> = FxHashMap::default();
        for c in &self.classes {
            if c.address != 0 && c.kind == ClassKind::Instance {
                by_name.entry(&c.name).or_default().push(c.loader);
            }
        }
        let mut per_loader: FxHashMap<u64, u64> = FxHashMap::default();
        let mut dup_names = 0u64;
        for loaders in by_name.values_mut() {
            loaders.sort_unstable();
            loaders.dedup();
            if loaders.len() > 1 {
                dup_names += 1;
                for &l in loaders.iter() {
                    *per_loader.entry(l).or_default() += 1;
                }
            }
        }
        let idx = self.index();
        let mut rows: Vec<_> = per_loader
            .into_iter()
            .map(|(l, n)| {
                let obj = idx.find(l);
                InspectionRow {
                    label: self.loader_label(l),
                    count: n,
                    bytes: obj.map_or(0, |o| self.retained_size(o)),
                    object: obj.map(|o| self.object_row(o)),
                }
            })
            .collect();
        rows.sort_by(|a, b| b.count.cmp(&a.count).then(b.bytes.cmp(&a.bytes)));
        InspectionResult {
            insight: Insight {
                kind: "classloader_leaks".into(),
                title: "Classes loaded by multiple class loaders".into(),
                severity: if dup_names >= 50 { Severity::Warning } else { Severity::Info },
                summary: format!(
                    "{dup_names} class names are defined by more than one loader ({} loaders involved); \
                     many duplicates usually mean a leaked redeployed class loader",
                    rows.len()
                ),
                wasted_bytes: None,
                count: Some(dup_names),
            },
            rows,
        }
    }

    fn big_collections(&self) -> InspectionResult {
        let kinds = self.collection_kinds();
        let mask: Vec<bool> = kinds.iter().map(|k| k.is_some()).collect();
        let mut found: Vec<(u32, u64)> = self
            .instances_of(&mask)
            .par_iter()
            .filter_map(|&o| {
                let (_, size_f, _) = COLLECTIONS[kinds[self.class_of(o) as usize]?];
                let size = self.field_int(o, size_f)?;
                (size >= 10_000).then_some((o, size as u64))
            })
            .collect();
        let arr_mask = self.classes_where(|c| self.classes[c as usize].kind == ClassKind::ObjArray);
        let big_arrays: Vec<(u32, u64)> = self
            .instances_of(&arr_mask)
            .par_iter()
            .filter_map(|&a| {
                let len = self.array_len(a)? as u64;
                (len >= 100_000).then_some((a, len))
            })
            .collect();
        found.extend(big_arrays);
        found.sort_by_key(|f| std::cmp::Reverse(self.retained_size(f.0)));
        let bytes: u64 = found.iter().map(|f| self.retained_size(f.0)).sum();
        let rows = found
            .iter()
            .take(MAX_ROWS)
            .map(|&(o, n)| InspectionRow {
                label: format!("{} with {n} elements", self.class_name(self.class_of(o))),
                count: n,
                bytes: self.retained_size(o),
                object: Some(self.object_row(o)),
            })
            .collect();
        InspectionResult {
            insight: Insight {
                kind: "big_collections".into(),
                title: "Large collections".into(),
                severity: severity(bytes, self.total().max(1) * 2),
                summary: format!(
                    "{} collections or arrays with 10k+ elements retain {}",
                    found.len(),
                    mb(bytes)
                ),
                wasted_bytes: None,
                count: Some(found.len() as u64),
            },
            rows,
        }
    }

    fn inner_classes(&self) -> InspectionResult {
        let mask = self.classes_where(|c| {
            let c = &self.classes[c as usize];
            c.fields.iter().any(|f| f.name == "this$0" && f.ty == ty::OBJECT)
        });
        let mut by_class: FxHashMap<u32, (u64, u64)> = FxHashMap::default();
        for o in self.instances_of(&mask) {
            let e = by_class.entry(self.class_of(o)).or_default();
            e.0 += 1;
            if let Some(outer) = self.field_ref(o, "this$0") {
                e.1 += self.shallow_size(outer);
            }
        }
        let count: u64 = by_class.values().map(|v| v.0).sum();
        let mut rows: Vec<_> = by_class
            .into_iter()
            .map(|(c, (n, b))| InspectionRow {
                label: self.class_name(c).to_owned(),
                count: n,
                bytes: b,
                object: None,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.count));
        InspectionResult {
            insight: Insight {
                kind: "inner_class_retention".into(),
                title: "Non-static inner class instances".into(),
                severity: Severity::Info,
                summary: format!(
                    "{count} instances of non-static inner classes keep a reference to their outer instance"
                ),
                wasted_bytes: None,
                count: Some(count),
            },
            rows,
        }
    }
}
