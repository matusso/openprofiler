//! Derived views over a [`Profile`]: call trees, back traces, hot spots, flame layouts, call
//! graphs and tree diffs.
//!
//! All views are built in two steps: the sample rows that pass a [`SampleFilter`] are first
//! aggregated per distinct stack, and then each distinct stack is inserted once into the output
//! tree. The cost is therefore O(rows + Σ depth(distinct stacks)), independent of the number of
//! samples behind each stack.

use crate::profile::{Nanos, Profile, SampleKind, SampleSet, ThreadState};
use crate::stacks::StackId;
use crate::symbols::{FrameId, FrameKind, Symbols};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// Selects sample rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SampleFilter {
    /// Inclusive start, exclusive end, in epoch nanoseconds.
    #[serde(default)]
    pub range: Option<(Nanos, Nanos)>,
    /// Thread indices; `None` means all threads.
    #[serde(default)]
    pub threads: Option<Vec<u32>>,
    /// Thread states; `None` means all states.
    #[serde(default)]
    pub states: Option<Vec<ThreadState>>,
    /// Only rows whose allocated/exception class (string id) is in this list.
    #[serde(default)]
    pub classes: Option<Vec<u32>>,
}

impl SampleFilter {
    pub fn accepts(&self, s: &SampleSet, i: usize) -> bool {
        if let Some((a, b)) = self.range {
            let t = s.time[i];
            if t < a || t >= b {
                return false;
            }
        }
        if let Some(th) = &self.threads
            && !th.contains(&s.thread[i])
        {
            return false;
        }
        if let Some(st) = &self.states
            && !st.contains(&s.state[i])
        {
            return false;
        }
        if let Some(cl) = &self.classes
            && !cl.contains(&s.class[i])
        {
            return false;
        }
        true
    }
}

/// The granularity of tree nodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregation {
    #[default]
    Method,
    Class,
    Package,
}

/// Restricts a tree to the calls below (callees) or above (callers) a given node key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Focus {
    /// "Cumulated outgoing calls": merge every subtree rooted at the outermost occurrence.
    Callees(u32),
    /// Back traces of the key, from its innermost occurrence outwards.
    Callers(u32),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TreeOptions {
    #[serde(default)]
    pub filter: SampleFilter,
    #[serde(default)]
    pub aggregation: Aggregation,
    /// Leaf-to-root tree (hot spots with back traces).
    #[serde(default)]
    pub inverted: bool,
    /// Fold recursive calls into their outermost occurrence.
    #[serde(default)]
    pub collapse_recursion: bool,
    #[serde(default)]
    pub focus: Option<Focus>,
    /// Class-name prefixes whose frames are compacted: a run of filtered frames is reduced to its
    /// entry point (`java.`, `jdk.internal.`).
    #[serde(default)]
    pub compact_prefixes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TreeNode {
    pub parent: u32,
    /// Method id, class string id or package string id depending on aggregation; `u32::MAX` for
    /// the root.
    pub key: u32,
    pub kind: FrameKind,
    pub self_weight: u64,
    pub total_weight: u64,
    pub self_count: u64,
    pub total_count: u64,
    /// Children, sorted by descending total weight once the tree is finished.
    pub children: Vec<u32>,
}

/// A materialized call tree. Node 0 is the synthetic root; node ids are dense and deterministic
/// for a given profile and options, so they can be handed to clients as stable ids.
#[derive(Debug, Clone, Default)]
pub struct CallTree {
    pub aggregation: Aggregation,
    pub inverted: bool,
    pub nodes: Vec<TreeNode>,
    child_index: FxHashMap<(u32, u32), u32>,
}

pub const ROOT_NODE: u32 = 0;

impl CallTree {
    fn new(aggregation: Aggregation, inverted: bool) -> Self {
        Self {
            aggregation,
            inverted,
            nodes: vec![TreeNode { parent: u32::MAX, key: u32::MAX, ..Default::default() }],
            child_index: FxHashMap::default(),
        }
    }

    fn child(&mut self, parent: u32, key: u32, kind: FrameKind) -> u32 {
        if let Some(&c) = self.child_index.get(&(parent, key)) {
            return c;
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(TreeNode { parent, key, kind, ..Default::default() });
        self.nodes[parent as usize].children.push(id);
        self.child_index.insert((parent, key), id);
        id
    }

    pub fn root(&self) -> &TreeNode {
        &self.nodes[0]
    }

    pub fn node(&self, id: u32) -> Option<&TreeNode> {
        self.nodes.get(id as usize)
    }

    pub fn total_weight(&self) -> u64 {
        self.nodes[0].total_weight
    }

    /// Path of keys from the root (exclusive) to `id` (inclusive).
    pub fn key_path(&self, mut id: u32) -> Vec<u32> {
        let mut out = vec![];
        while id != ROOT_NODE && (id as usize) < self.nodes.len() {
            out.push(self.nodes[id as usize].key);
            id = self.nodes[id as usize].parent;
        }
        out.reverse();
        out
    }

    /// Finds the node reached by following `keys` from the root.
    pub fn find_path(&self, keys: &[u32]) -> Option<u32> {
        let mut cur = ROOT_NODE;
        for k in keys {
            cur = *self.child_index.get(&(cur, *k))?;
        }
        Some(cur)
    }

    fn finish(&mut self) {
        // Children are always created after their parents, so a reverse sweep accumulates totals.
        for i in (1..self.nodes.len()).rev() {
            let (tw, tc, p) = {
                let n = &self.nodes[i];
                (n.total_weight, n.total_count, n.parent as usize)
            };
            if !self.inverted {
                self.nodes[p].total_weight += tw;
                self.nodes[p].total_count += tc;
            }
        }
        if self.inverted {
            // In an inverted tree the root total is the sum of its children (the hot spots).
            let (w, c) = self.nodes[0]
                .children
                .iter()
                .map(|&c| &self.nodes[c as usize])
                .fold((0, 0), |(w, c), n| (w + n.total_weight, c + n.total_count));
            self.nodes[0].total_weight = w;
            self.nodes[0].total_count = c;
        }
        let nodes = &self.nodes;
        let mut sorted: Vec<Vec<u32>> = nodes
            .iter()
            .map(|n| {
                let mut c = n.children.clone();
                c.sort_by(|a, b| {
                    nodes[*b as usize].total_weight.cmp(&nodes[*a as usize].total_weight).then(a.cmp(b))
                });
                c
            })
            .collect();
        for (n, c) in self.nodes.iter_mut().zip(sorted.iter_mut()) {
            n.children = std::mem::take(c);
        }
    }
}

/// Maps frames to aggregation keys.
pub struct KeyMapper<'a> {
    symbols: &'a Symbols,
    aggregation: Aggregation,
    compact: Vec<String>,
    cache: FxHashMap<FrameId, (u32, FrameKind, bool)>,
    packages: FxHashMap<u32, u32>,
    extra_strings: Vec<String>,
}

impl<'a> KeyMapper<'a> {
    pub fn new(symbols: &'a Symbols, aggregation: Aggregation, compact: &[String]) -> Self {
        Self {
            symbols,
            aggregation,
            compact: compact.to_vec(),
            cache: FxHashMap::default(),
            packages: FxHashMap::default(),
            extra_strings: vec![],
        }
    }

    /// `(key, kind, is_compacted)` for a frame.
    pub fn map(&mut self, frame: FrameId) -> (u32, FrameKind, bool) {
        if let Some(&v) = self.cache.get(&frame) {
            return v;
        }
        let f = self.symbols.frame(frame);
        let m = self.symbols.method(f.method);
        let class = self.symbols.str(m.class);
        let compacted = self.compact.iter().any(|p| class.starts_with(p.as_str()));
        let key = match self.aggregation {
            Aggregation::Method => f.method,
            Aggregation::Class => m.class,
            Aggregation::Package => {
                let class_id = m.class;
                if let Some(&p) = self.packages.get(&class_id) {
                    p
                } else {
                    let pkg = self.symbols.package_name(class_id);
                    let id = match self.symbols.lookup_str(pkg) {
                        Some(id) => id,
                        None => {
                            // Package strings not present in the table get synthetic ids above
                            // the string table; `label` resolves them.
                            self.extra_strings.push(pkg.to_owned());
                            (self.symbols.strings().len() + self.extra_strings.len() - 1) as u32
                        }
                    };
                    self.packages.insert(class_id, id);
                    id
                }
            }
        };
        let v = (key, f.kind, compacted);
        self.cache.insert(frame, v);
        v
    }

    pub fn into_extra_strings(self) -> Vec<String> {
        self.extra_strings
    }
}

/// Aggregates the filtered rows of `kind` into per-stack `(weight, count)`.
pub fn aggregate_stacks(
    profile: &Profile,
    kind: SampleKind,
    filter: &SampleFilter,
) -> Vec<(StackId, u64, u64)> {
    let Some(set) = profile.samples.get(&kind) else { return vec![] };
    let mut acc: FxHashMap<StackId, (u64, u64)> = FxHashMap::default();
    let unfiltered = *filter == SampleFilter::default();
    for i in 0..set.len() {
        if !unfiltered && !filter.accepts(set, i) {
            continue;
        }
        let e = acc.entry(set.stack[i]).or_default();
        e.0 += set.weight[i];
        e.1 += set.count[i] as u64;
    }
    let mut v: Vec<_> = acc.into_iter().map(|(s, (w, c))| (s, w, c)).collect();
    v.sort_unstable_by_key(|e| e.0);
    v
}

/// The output of [`build_tree`]: the tree plus any synthetic label strings (package names that
/// were not interned in the profile's symbol table).
#[derive(Debug, Clone, Default)]
pub struct BuiltTree {
    pub tree: CallTree,
    pub extra_strings: Vec<String>,
}

/// Maps a stack to its sequence of `(key, kind)` after compaction and consecutive-duplicate
/// collapsing, root first.
fn key_sequence(
    profile: &Profile,
    mapper: &mut KeyMapper<'_>,
    stack: StackId,
    frames: &mut Vec<FrameId>,
    out: &mut Vec<(u32, FrameKind)>,
) {
    profile.stacks.frames_root_first(stack, frames);
    out.clear();
    let mut prev_compacted = false;
    for &f in frames.iter() {
        let (key, kind, compacted) = mapper.map(f);
        if compacted && prev_compacted {
            continue;
        }
        prev_compacted = compacted;
        if let Some(&(last, _)) = out.last()
            && last == key
        {
            // Consecutive frames with the same key (e.g. class aggregation) merge into one node.
            continue;
        }
        out.push((key, kind));
    }
}

pub fn build_tree(profile: &Profile, kind: SampleKind, opts: &TreeOptions) -> BuiltTree {
    let stacks = aggregate_stacks(profile, kind, &opts.filter);
    let mut mapper = KeyMapper::new(&profile.symbols, opts.aggregation, &opts.compact_prefixes);
    let mut tree = CallTree::new(opts.aggregation, opts.inverted);
    let mut frames = Vec::new();
    let mut seq = Vec::new();
    let mut path: Vec<u32> = Vec::new();
    for (stack, w, c) in stacks {
        key_sequence(profile, &mut mapper, stack, &mut frames, &mut seq);
        let slice: &[(u32, FrameKind)] = match opts.focus {
            None => &seq,
            Some(Focus::Callees(k)) => match seq.iter().position(|(key, _)| *key == k) {
                Some(i) => &seq[i..],
                None => continue,
            },
            Some(Focus::Callers(k)) => match seq.iter().rposition(|(key, _)| *key == k) {
                Some(i) => &seq[..=i],
                None => continue,
            },
        };
        let inverted = opts.inverted || matches!(opts.focus, Some(Focus::Callers(_)));
        tree.inverted = inverted;
        let mut cur = ROOT_NODE;
        path.clear();
        let iter: Box<dyn Iterator<Item = &(u32, FrameKind)>> =
            if inverted { Box::new(slice.iter().rev()) } else { Box::new(slice.iter()) };
        for &(key, fk) in iter {
            if opts.collapse_recursion
                && !inverted
                && let Some(pos) = path.iter().position(|&n| tree.nodes[n as usize].key == key)
            {
                path.truncate(pos + 1);
                cur = path[pos];
                continue;
            }
            cur = tree.child(cur, key, fk);
            path.push(cur);
            if inverted {
                let n = &mut tree.nodes[cur as usize];
                n.total_weight += w;
                n.total_count += c;
            }
        }
        if cur == ROOT_NODE {
            continue;
        }
        let n = &mut tree.nodes[cur as usize];
        n.self_weight += w;
        n.self_count += c;
        if !inverted {
            n.total_weight += w;
            n.total_count += c;
        }
    }
    tree.finish();
    BuiltTree { tree, extra_strings: mapper.into_extra_strings() }
}

/// Human-readable label for a tree key.
pub fn key_label(symbols: &Symbols, aggregation: Aggregation, key: u32, extra: &[String]) -> String {
    if key == u32::MAX {
        return "All threads".into();
    }
    match aggregation {
        Aggregation::Method => symbols.method_label(key),
        Aggregation::Class | Aggregation::Package => {
            let n = symbols.strings().len();
            let s = if (key as usize) < n {
                symbols.str(key).to_owned()
            } else {
                extra.get(key as usize - n).cloned().unwrap_or_default()
            };
            if s.is_empty() && aggregation == Aggregation::Package { "<default package>".into() } else { s }
        }
    }
}

/// One hot spot: a key with its self weight (where the samples' leaf frames were) and its total
/// (inherent) weight (samples with the key anywhere on the stack).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HotSpot {
    pub key: u32,
    pub self_weight: u64,
    pub self_count: u64,
    pub total_weight: u64,
    pub total_count: u64,
}

pub fn hot_spots(profile: &Profile, kind: SampleKind, opts: &TreeOptions) -> (Vec<HotSpot>, Vec<String>) {
    let stacks = aggregate_stacks(profile, kind, &opts.filter);
    let mut mapper = KeyMapper::new(&profile.symbols, opts.aggregation, &opts.compact_prefixes);
    let mut acc: FxHashMap<u32, HotSpot> = FxHashMap::default();
    let mut frames = Vec::new();
    let mut seq = Vec::new();
    let mut seen: Vec<u32> = Vec::new();
    for (stack, w, c) in stacks {
        key_sequence(profile, &mut mapper, stack, &mut frames, &mut seq);
        let Some(&(leaf, _)) = seq.last() else { continue };
        let h = acc.entry(leaf).or_insert_with(|| HotSpot { key: leaf, ..Default::default() });
        h.self_weight += w;
        h.self_count += c;
        seen.clear();
        for &(k, _) in &seq {
            if seen.contains(&k) {
                continue;
            }
            seen.push(k);
            let h = acc.entry(k).or_insert_with(|| HotSpot { key: k, ..Default::default() });
            h.total_weight += w;
            h.total_count += c;
        }
    }
    let mut v: Vec<HotSpot> = acc.into_values().collect();
    v.sort_by(|a, b| {
        b.self_weight.cmp(&a.self_weight).then(b.total_weight.cmp(&a.total_weight)).then(a.key.cmp(&b.key))
    });
    (v, mapper.into_extra_strings())
}

/// Pre-laid-out flame graph rectangles in a columnar format. `x` and `width` are in weight units;
/// the client scales them by `total`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FlameLayout {
    pub total: u64,
    pub max_depth: u32,
    /// Distinct labels; `name` indexes into it.
    pub names: Vec<String>,
    pub node: Vec<u32>,
    /// Tree key (method id, class or package string id) per rectangle; `u32::MAX` for the root
    /// and for differential layouts.
    pub key: Vec<u32>,
    pub x: Vec<u64>,
    pub width: Vec<u64>,
    pub depth: Vec<u32>,
    pub name: Vec<u32>,
    pub self_weight: Vec<u64>,
    pub kind: Vec<FrameKind>,
    /// For differential flames: `b - a` total weight per rectangle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<Vec<i64>>,
}

/// Lays out `tree` depth-first. Rectangles narrower than `min_fraction` of the root are dropped
/// together with their subtrees.
pub fn flame_layout(tree: &CallTree, symbols: &Symbols, extra: &[String], min_fraction: f64) -> FlameLayout {
    let total = tree.total_weight();
    let mut out = FlameLayout { total, ..Default::default() };
    let min_w = (total as f64 * min_fraction).ceil() as u64;
    let mut names: FxHashMap<u32, u32> = FxHashMap::default();
    // (node, x, depth)
    let mut stack: Vec<(u32, u64, u32)> = vec![(ROOT_NODE, 0, 0)];
    while let Some((id, x, depth)) = stack.pop() {
        let n = &tree.nodes[id as usize];
        if id != ROOT_NODE && n.total_weight < min_w.max(1) {
            continue;
        }
        let name = *names.entry(n.key).or_insert_with(|| {
            out.names.push(key_label(symbols, tree.aggregation, n.key, extra));
            (out.names.len() - 1) as u32
        });
        out.node.push(id);
        out.key.push(n.key);
        out.x.push(x);
        out.width.push(n.total_weight);
        out.depth.push(depth);
        out.name.push(name);
        out.self_weight.push(n.self_weight);
        out.kind.push(n.kind);
        out.max_depth = out.max_depth.max(depth);
        // Inverted trees and the root have self weight that is not laid out as a gap; children
        // start at the node's left edge.
        let mut cx = x;
        let mut kids = Vec::with_capacity(n.children.len());
        for &c in &n.children {
            kids.push((c, cx, depth + 1));
            cx += tree.nodes[c as usize].total_weight;
        }
        stack.extend(kids.into_iter().rev());
    }
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffNode {
    pub parent: u32,
    pub label: String,
    pub a_self: u64,
    pub a_total: u64,
    pub b_self: u64,
    pub b_total: u64,
    pub children: Vec<u32>,
}

/// A tree aligned by label path across two profiles (which have unrelated symbol ids).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffTree {
    pub nodes: Vec<DiffNode>,
}

pub fn diff_trees(a: &BuiltTree, sa: &Symbols, b: &BuiltTree, sb: &Symbols) -> DiffTree {
    let mut out = DiffTree {
        nodes: vec![DiffNode {
            parent: u32::MAX,
            label: "All threads".into(),
            a_self: a.tree.root().self_weight,
            a_total: a.tree.total_weight(),
            b_self: b.tree.root().self_weight,
            b_total: b.tree.total_weight(),
            children: vec![],
        }],
    };
    // Work list: (out node, Option<a node>, Option<b node>)
    let mut work = vec![(0u32, Some(ROOT_NODE), Some(ROOT_NODE))];
    while let Some((o, an, bn)) = work.pop() {
        let mut by_label: Vec<(String, Option<u32>, Option<u32>)> = vec![];
        let mut idx: FxHashMap<String, usize> = FxHashMap::default();
        if let Some(an) = an {
            for &c in &a.tree.nodes[an as usize].children {
                let l = key_label(sa, a.tree.aggregation, a.tree.nodes[c as usize].key, &a.extra_strings);
                idx.insert(l.clone(), by_label.len());
                by_label.push((l, Some(c), None));
            }
        }
        if let Some(bn) = bn {
            for &c in &b.tree.nodes[bn as usize].children {
                let l = key_label(sb, b.tree.aggregation, b.tree.nodes[c as usize].key, &b.extra_strings);
                match idx.get(&l) {
                    Some(&i) => by_label[i].2 = Some(c),
                    None => {
                        idx.insert(l.clone(), by_label.len());
                        by_label.push((l, None, Some(c)));
                    }
                }
            }
        }
        for (label, ac, bc) in by_label {
            let id = out.nodes.len() as u32;
            let an = ac.map(|c| &a.tree.nodes[c as usize]);
            let bn = bc.map(|c| &b.tree.nodes[c as usize]);
            out.nodes.push(DiffNode {
                parent: o,
                label,
                a_self: an.map_or(0, |n| n.self_weight),
                a_total: an.map_or(0, |n| n.total_weight),
                b_self: bn.map_or(0, |n| n.self_weight),
                b_total: bn.map_or(0, |n| n.total_weight),
                children: vec![],
            });
            out.nodes[o as usize].children.push(id);
            work.push((id, ac, bc));
        }
    }
    // Sort children by the larger of the two totals.
    let snapshot: Vec<(u64, u64)> = out.nodes.iter().map(|n| (n.a_total, n.b_total)).collect();
    for n in &mut out.nodes {
        n.children.sort_by_key(|&c| {
            let (a, b) = snapshot[c as usize];
            std::cmp::Reverse(a.max(b))
        });
    }
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CallGraph {
    pub nodes: Vec<HotSpot>,
    /// `(caller index, callee index, weight, count)` into `nodes`.
    pub edges: Vec<(u32, u32, u64, u64)>,
}

/// Method-level call graph. Each sample contributes its weight once per distinct edge.
pub fn call_graph(profile: &Profile, kind: SampleKind, opts: &TreeOptions) -> (CallGraph, Vec<String>) {
    let (spots, extra) = hot_spots(profile, kind, opts);
    let index: FxHashMap<u32, u32> = spots.iter().enumerate().map(|(i, h)| (h.key, i as u32)).collect();
    let stacks = aggregate_stacks(profile, kind, &opts.filter);
    let mut mapper = KeyMapper::new(&profile.symbols, opts.aggregation, &opts.compact_prefixes);
    let mut edges: FxHashMap<(u32, u32), (u64, u64)> = FxHashMap::default();
    let mut frames = Vec::new();
    let mut seq = Vec::new();
    let mut seen: Vec<(u32, u32)> = Vec::new();
    for (stack, w, c) in stacks {
        key_sequence(profile, &mut mapper, stack, &mut frames, &mut seq);
        seen.clear();
        for pair in seq.windows(2) {
            let (Some(&a), Some(&b)) = (index.get(&pair[0].0), index.get(&pair[1].0)) else { continue };
            if seen.contains(&(a, b)) {
                continue;
            }
            seen.push((a, b));
            let e = edges.entry((a, b)).or_default();
            e.0 += w;
            e.1 += c;
        }
    }
    let mut edges: Vec<_> = edges.into_iter().map(|((a, b), (w, c))| (a, b, w, c)).collect();
    edges.sort_by(|x, y| y.2.cmp(&x.2).then((x.0, x.1).cmp(&(y.0, y.1))));
    (CallGraph { nodes: spots, edges }, extra)
}

/// Per-thread totals for a sample kind (index into `profile.threads`, weight, count).
pub fn thread_totals(profile: &Profile, kind: SampleKind, filter: &SampleFilter) -> Vec<(u32, u64, u64)> {
    let Some(set) = profile.samples.get(&kind) else { return vec![] };
    let mut acc: FxHashMap<u32, (u64, u64)> = FxHashMap::default();
    for i in 0..set.len() {
        if filter.accepts(set, i) {
            let e = acc.entry(set.thread[i]).or_default();
            e.0 += set.weight[i];
            e.1 += set.count[i] as u64;
        }
    }
    let mut v: Vec<_> = acc.into_iter().map(|(t, (w, c))| (t, w, c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// Weight per time bucket (for timeline sparklines of a sample kind).
pub fn sample_histogram(
    profile: &Profile,
    kind: SampleKind,
    filter: &SampleFilter,
    buckets: usize,
) -> Vec<u64> {
    let Some(set) = profile.samples.get(&kind) else { return vec![0; buckets] };
    let (start, end) = filter.range.unwrap_or((profile.meta.start, profile.meta.end + 1));
    let span = (end - start).max(1) as f64;
    let mut out = vec![0u64; buckets.max(1)];
    for i in 0..set.len() {
        if filter.accepts(set, i) {
            let b = (((set.time[i] - start) as f64 / span) * out.len() as f64) as usize;
            let last = out.len() - 1;
            out[b.min(last)] += set.weight[i];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Profile, SampleKind, ThreadState};
    use crate::symbols::FrameKind;

    fn profile() -> Profile {
        let mut p = Profile::new("test");
        let s = &mut p.symbols;
        let main = s.intern_method("app.Main", "main", "([Ljava/lang/String;)V");
        let work = s.intern_method("app.Worker", "work", "()V");
        let helper = s.intern_method("app.Worker", "helper", "()V");
        let sort = s.intern_method("java.util.Arrays", "sort", "([I)V");
        let f = |s: &mut crate::Symbols, m| s.intern_frame(m, -1, FrameKind::Jit);
        let (fm, fw, fh, fs) = (f(s, main), f(s, work), f(s, helper), f(s, sort));
        let st1 = p.stacks.intern_root_first([fm, fw, fh]);
        let st2 = p.stacks.intern_root_first([fm, fw, fs]);
        let st3 = p.stacks.intern_root_first([fm, fw]);
        let t = p.thread_index(1, || "main".into());
        let set = p.samples_mut(SampleKind::Cpu);
        for (i, st) in [st1, st1, st1, st2, st3].into_iter().enumerate() {
            set.push(i as i64 * 10, t, ThreadState::Runnable, st, 10, 1, None);
        }
        p.meta.start = 0;
        p.meta.end = 40;
        p
    }

    #[test]
    fn call_tree_totals() {
        let p = profile();
        let t = build_tree(&p, SampleKind::Cpu, &TreeOptions::default()).tree;
        assert_eq!(t.total_weight(), 50);
        let main = t.root().children[0];
        assert_eq!(t.nodes[main as usize].total_weight, 50);
        let work = t.nodes[main as usize].children[0];
        assert_eq!(t.nodes[work as usize].self_weight, 10);
        let helper = t.nodes[work as usize].children[0];
        assert_eq!(t.nodes[helper as usize].total_weight, 30);
        assert_eq!(p.symbols.method_label(t.nodes[helper as usize].key), "app.Worker.helper");
    }

    #[test]
    fn class_aggregation_merges_consecutive() {
        let p = profile();
        let opts = TreeOptions { aggregation: Aggregation::Class, ..Default::default() };
        let t = build_tree(&p, SampleKind::Cpu, &opts).tree;
        let main = t.root().children[0];
        let worker = t.nodes[main as usize].children[0];
        assert_eq!(p.symbols.str(t.nodes[worker as usize].key), "app.Worker");
        // work+helper merge into one Worker node; Arrays is its only child.
        assert_eq!(t.nodes[worker as usize].total_weight, 50);
        assert_eq!(t.nodes[worker as usize].self_weight, 40);
        assert_eq!(t.nodes[worker as usize].children.len(), 1);
    }

    #[test]
    fn hotspots_and_inverted() {
        let p = profile();
        let (hs, _) = hot_spots(&p, SampleKind::Cpu, &TreeOptions::default());
        assert_eq!(p.symbols.method_label(hs[0].key), "app.Worker.helper");
        assert_eq!(hs[0].self_weight, 30);
        let work = hs.iter().find(|h| p.symbols.method_label(h.key) == "app.Worker.work").unwrap();
        assert_eq!(work.self_weight, 10);
        assert_eq!(work.total_weight, 50);
        let inv = build_tree(&p, SampleKind::Cpu, &TreeOptions { inverted: true, ..Default::default() }).tree;
        assert_eq!(inv.total_weight(), 50);
        let top = inv.root().children[0];
        assert_eq!(inv.nodes[top as usize].total_weight, 30);
        let caller = inv.nodes[top as usize].children[0];
        assert_eq!(p.symbols.method_label(inv.nodes[caller as usize].key), "app.Worker.work");
        assert_eq!(inv.nodes[caller as usize].total_weight, 30);
    }

    #[test]
    fn filters_and_focus() {
        let p = profile();
        let opts = TreeOptions {
            filter: SampleFilter { range: Some((0, 25)), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(build_tree(&p, SampleKind::Cpu, &opts).tree.total_weight(), 30);
        let work = p.symbols.methods().iter().position(|m| p.symbols.str(m.name) == "work").unwrap() as u32;
        let t = build_tree(
            &p,
            SampleKind::Cpu,
            &TreeOptions { focus: Some(Focus::Callees(work)), ..Default::default() },
        )
        .tree;
        assert_eq!(t.root().children.len(), 1);
        assert_eq!(t.nodes[t.root().children[0] as usize].key, work);
        let compact = TreeOptions { compact_prefixes: vec!["java.".into()], ..Default::default() };
        assert_eq!(build_tree(&p, SampleKind::Cpu, &compact).tree.total_weight(), 50);
    }

    #[test]
    fn flame_and_diff() {
        let p = profile();
        let bt = build_tree(&p, SampleKind::Cpu, &TreeOptions::default());
        let fl = flame_layout(&bt.tree, &p.symbols, &bt.extra_strings, 0.0);
        assert_eq!(fl.total, 50);
        assert_eq!(fl.node.len(), bt.tree.nodes.len());
        assert_eq!(fl.max_depth, 3);
        let d = diff_trees(&bt, &p.symbols, &bt, &p.symbols);
        assert_eq!(d.nodes.len(), bt.tree.nodes.len());
        assert!(d.nodes.iter().all(|n| n.a_total == n.b_total));
        let (g, _) = call_graph(&p, SampleKind::Cpu, &TreeOptions::default());
        assert_eq!(g.nodes.len(), 4);
        assert_eq!(g.edges.len(), 3);
    }

    #[test]
    fn collapse_recursion() {
        let mut p = Profile::new("t");
        let a = p.symbols.intern_method("A", "a", "");
        let b = p.symbols.intern_method("A", "b", "");
        let fa = p.symbols.intern_frame(a, -1, FrameKind::Jit);
        let fb = p.symbols.intern_frame(b, -1, FrameKind::Jit);
        let st = p.stacks.intern_root_first([fa, fb, fa, fb, fa]);
        p.samples_mut(SampleKind::Cpu).push(0, 0, ThreadState::Runnable, st, 1, 1, None);
        let t =
            build_tree(&p, SampleKind::Cpu, &TreeOptions { collapse_recursion: true, ..Default::default() })
                .tree;
        assert_eq!(t.nodes.len(), 3);
    }
}
