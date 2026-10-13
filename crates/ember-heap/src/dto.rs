//! Serializable result types. JSON shapes match `docs/spec/query-api.md` (Heap section).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeapSummary {
    pub objects: u64,
    pub classes: u32,
    pub total_shallow: u64,
    pub gc_roots: u64,
    pub id_size: u32,
    /// Epoch nanoseconds.
    pub dump_time: Option<i64>,
    pub analysis_state: String,
    pub reachable_objects: u64,
    pub reachable_shallow: u64,
    pub edges: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectSetDto {
    pub id: u32,
    pub parent: Option<u32>,
    pub label: String,
    pub objects: u64,
    pub shallow: u64,
    pub retained: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    #[default]
    Class,
    Package,
    Classloader,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassSort {
    Instances,
    #[default]
    Shallow,
    Retained,
    Name,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClassQuery {
    #[serde(default)]
    pub set: Option<u32>,
    #[serde(default)]
    pub group_by: GroupBy,
    #[serde(default)]
    pub sort: ClassSort,
    /// Case-insensitive substring filter on the row name.
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClassRow {
    /// Class index (only for `group_by: class`).
    pub class_id: Option<u32>,
    pub name: String,
    pub instances: u64,
    pub shallow: u64,
    pub retained: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    pub total_rows: u64,
    pub rows: Vec<T>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectSort {
    Shallow,
    #[default]
    Retained,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectQuery {
    #[serde(default)]
    pub set: Option<u32>,
    #[serde(default)]
    pub class_id: Option<u32>,
    #[serde(default)]
    pub sort: ObjectSort,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectRow {
    pub id: u32,
    pub address: String,
    pub class_name: String,
    pub shallow: u64,
    pub retained: Option<u64>,
    pub preview: Option<String>,
    /// Number of children in the dominator tree.
    pub child_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RefRow {
    pub object: ObjectRow,
    pub field: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergedRefRow {
    pub class_name: String,
    pub field: String,
    /// Distinct referencing (incoming) or referenced (outgoing) objects.
    pub objects: u64,
    pub refs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathStep {
    pub object: ObjectRow,
    /// Field of the next step (towards the root) that references this object.
    pub field: Option<String>,
    /// Set on the last step: the GC root kind(s).
    pub root_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldValue {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub value: String,
    #[serde(rename = "ref")]
    pub reference: Option<u32>,
    /// `true` for static fields of a class object.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_static: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ElementValue {
    pub index: u32,
    pub value: String,
    #[serde(rename = "ref")]
    pub reference: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectDetail {
    pub object: ObjectRow,
    pub fields: Vec<FieldValue>,
    pub array_length: Option<u32>,
    pub elements: Option<Vec<ElementValue>>,
    pub gc_root_kinds: Vec<String>,
    /// Immediate dominator (`None` for objects dominated only by the super root or unreachable).
    pub dominator: Option<u32>,
    pub reachable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SelectStep {
    ClassId(u32),
    ClassName(String),
    Package(String),
    RetainedBy(u32),
    ReferencedBySet(bool),
    ReferencingSet(bool),
    DominatedBySet(bool),
    Objects(Vec<u32>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Insight {
    pub kind: String,
    pub title: String,
    pub severity: Severity,
    pub summary: String,
    pub wasted_bytes: Option<u64>,
    pub count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InspectionRow {
    pub label: String,
    pub count: u64,
    /// Wasted or retained bytes, depending on the inspection.
    pub bytes: u64,
    pub object: Option<ObjectRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InspectionResult {
    pub insight: Insight,
    pub rows: Vec<InspectionRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistogramDiffRow {
    pub name: String,
    pub a_instances: u64,
    pub b_instances: u64,
    pub a_shallow: u64,
    pub b_shallow: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DominatorClassRow {
    pub name: String,
    pub objects: u64,
    pub retained: u64,
}
