//! Dominator computation.
//!
//! [`semi_nca`] is the production algorithm (Semi-NCA, Georgiadis 2005): one iterative DFS, a
//! semidominator pass with iterative path compression, and a nearest-common-ancestor pass. It
//! uses 7 `u32` arrays per node. [`iterative_oracle`] is the Cooper–Harvey–Kennedy dataflow
//! algorithm, used in tests as a correctness oracle.

use crate::store::Big;

pub const NONE: u32 = u32::MAX;

/// A directed graph with dense node ids.
pub trait Graph: Sync {
    fn node_count(&self) -> usize;
    /// Successors of `v`.
    fn succ(&self, v: u32) -> &[u32];
    /// Calls `f` for each predecessor of `v`.
    fn pred(&self, v: u32, f: &mut dyn FnMut(u32));
}

/// Result of a dominator computation.
pub struct Dominators {
    /// Immediate dominator per node (`root` maps to itself, unreachable nodes to [`NONE`]).
    pub idom: Big<u32>,
    /// Reachable nodes in DFS preorder (first `reachable` entries); `preorder[0] == root`.
    /// Every node's idom precedes it.
    pub preorder: Big<u32>,
    pub reachable: usize,
}

impl Dominators {
    pub fn preorder(&self) -> &[u32] {
        &self.preorder[..self.reachable]
    }
}

pub fn semi_nca<G: Graph + ?Sized>(g: &G, root: u32) -> Dominators {
    let n = g.node_count();
    // All working arrays live in anonymous mappings so they are released exactly when dropped.
    let mut pre = Big::<u32>::filled(n, NONE);
    let mut vertex = Big::<u32>::zeroed(n);
    let mut parent = Big::<u32>::zeroed(n);
    let mut m = 0usize;

    // Iterative DFS assigning preorder numbers.
    let mut stack: Vec<(u32, u32)> = vec![(root, 0)];
    pre[root as usize] = 0;
    vertex[0] = root;
    m += 1;
    while let Some(top) = stack.last_mut() {
        let (v, i) = *top;
        let s = g.succ(v);
        if (i as usize) < s.len() {
            top.1 += 1;
            let w = s[i as usize];
            if pre[w as usize] == NONE {
                pre[w as usize] = m as u32;
                parent[m] = pre[v as usize];
                vertex[m] = w;
                m += 1;
                stack.push((w, 0));
            }
        } else {
            stack.pop();
        }
    }
    drop(stack);

    let mut semi = Big::<u32>::zeroed(m);
    let mut label = Big::<u32>::zeroed(m);
    for i in 0..m {
        semi[i] = i as u32;
        label[i] = i as u32;
    }
    let mut ancestor = Big::<u32>::filled(m, NONE);
    let mut path: Vec<u32> = Vec::new();

    for w in (1..m).rev() {
        let mut sw = semi[w];
        g.pred(vertex[w], &mut |u| {
            let pu = pre[u as usize];
            if pu == NONE {
                return;
            }
            // eval(pu) with iterative path compression
            let x = if ancestor[pu as usize] == NONE {
                pu
            } else {
                let mut y = pu;
                path.clear();
                while ancestor[ancestor[y as usize] as usize] != NONE {
                    path.push(y);
                    y = ancestor[y as usize];
                }
                while let Some(z) = path.pop() {
                    let a = ancestor[z as usize];
                    if semi[label[a as usize] as usize] < semi[label[z as usize] as usize] {
                        label[z as usize] = label[a as usize];
                    }
                    ancestor[z as usize] = ancestor[a as usize];
                }
                label[pu as usize]
            };
            if semi[x as usize] < sw {
                sw = semi[x as usize];
            }
        });
        semi[w] = sw;
        ancestor[w] = parent[w];
    }
    drop(ancestor);
    drop(label);
    drop(pre);

    let mut idom = parent;
    for w in 1..m {
        let mut d = idom[w];
        while d > semi[w] {
            d = idom[d as usize];
        }
        idom[w] = d;
    }
    drop(semi);

    let mut out = Big::<u32>::filled(n, NONE);
    for w in 0..m {
        out[vertex[w] as usize] = vertex[idom[w] as usize];
    }
    Dominators { idom: out, preorder: vertex, reachable: m }
}

/// Cooper–Harvey–Kennedy iterative dominators. O(n²) worst case; for tests.
pub fn iterative_oracle<G: Graph + ?Sized>(g: &G, root: u32) -> Vec<u32> {
    let n = g.node_count();
    // Reverse postorder.
    let mut post = Vec::new();
    let mut seen = vec![false; n];
    let mut stack = vec![(root, 0usize)];
    seen[root as usize] = true;
    while let Some(top) = stack.last_mut() {
        let (v, i) = *top;
        let s = g.succ(v);
        if i < s.len() {
            top.1 += 1;
            let w = s[i];
            if !seen[w as usize] {
                seen[w as usize] = true;
                stack.push((w, 0));
            }
        } else {
            post.push(v);
            stack.pop();
        }
    }
    let mut po_num = vec![NONE; n];
    for (i, &v) in post.iter().enumerate() {
        po_num[v as usize] = i as u32;
    }
    let mut idom = vec![NONE; n];
    idom[root as usize] = root;
    let mut changed = true;
    while changed {
        changed = false;
        for &v in post.iter().rev() {
            if v == root {
                continue;
            }
            let mut new = NONE;
            g.pred(v, &mut |p| {
                if idom[p as usize] == NONE {
                    return;
                }
                if new == NONE {
                    new = p;
                    return;
                }
                let (mut a, mut b) = (p, new);
                while a != b {
                    while po_num[a as usize] < po_num[b as usize] {
                        a = idom[a as usize];
                    }
                    while po_num[b as usize] < po_num[a as usize] {
                        b = idom[b as usize];
                    }
                }
                new = a;
            });
            if idom[v as usize] != new {
                idom[v as usize] = new;
                changed = true;
            }
        }
    }
    idom
}

/// Simple adjacency-list graph for tests and small inputs.
pub struct VecGraph {
    pub succ: Vec<Vec<u32>>,
    pub pred: Vec<Vec<u32>>,
}

impl VecGraph {
    pub fn new(n: usize, edges: &[(u32, u32)]) -> Self {
        let mut succ = vec![vec![]; n];
        let mut pred = vec![vec![]; n];
        for &(a, b) in edges {
            succ[a as usize].push(b);
            pred[b as usize].push(a);
        }
        Self { succ, pred }
    }
}

impl Graph for VecGraph {
    fn node_count(&self) -> usize {
        self.succ.len()
    }

    fn succ(&self, v: u32) -> &[u32] {
        &self.succ[v as usize]
    }

    fn pred(&self, v: u32, f: &mut dyn FnMut(u32)) {
        for &p in &self.pred[v as usize] {
            f(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    #[test]
    fn classic_example() {
        // Lengauer–Tarjan paper graph (R=0, A..L = 1..12)
        let e = [
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 4),
            (2, 1),
            (2, 4),
            (2, 5),
            (3, 6),
            (3, 7),
            (4, 12),
            (5, 8),
            (6, 9),
            (7, 9),
            (7, 10),
            (8, 5),
            (8, 11),
            (9, 11),
            (10, 9),
            (11, 0),
            (11, 9),
            (12, 8),
        ];
        let g = VecGraph::new(13, &e);
        let d = semi_nca(&g, 0);
        assert_eq!(&d.idom[..], &iterative_oracle(&g, 0)[..]);
        assert_eq!(d.idom[11], 0);
        assert_eq!(d.idom[9], 0);
        assert_eq!(d.idom[6], 3);
        assert_eq!(d.idom[10], 7);
    }

    #[test]
    fn irreducible() {
        // 0 -> 1, 0 -> 2, 1 <-> 2 (irreducible loop), 2 -> 3, 1 -> 3, 3 -> 4
        let g = VecGraph::new(6, &[(0, 1), (0, 2), (1, 2), (2, 1), (2, 3), (1, 3), (3, 4)]);
        let d = semi_nca(&g, 0);
        assert_eq!(&d.idom[..], &iterative_oracle(&g, 0)[..]);
        assert_eq!(d.idom[1], 0);
        assert_eq!(d.idom[2], 0);
        assert_eq!(d.idom[4], 3);
        assert_eq!(d.idom[5], NONE);
    }

    #[test]
    fn random_graphs_match_oracle() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        for _ in 0..300 {
            let n = rng.random_range(2..80);
            let m = rng.random_range(0..n * 3);
            let mut edges = vec![];
            for _ in 0..m {
                edges.push((rng.random_range(0..n) as u32, rng.random_range(0..n) as u32));
            }
            // Ensure some spanning structure.
            for v in 1..n {
                if rng.random_bool(0.6) {
                    edges.push((rng.random_range(0..v) as u32, v as u32));
                }
            }
            let g = VecGraph::new(n, &edges);
            let d = semi_nca(&g, 0);
            assert_eq!(&d.idom[..], &iterative_oracle(&g, 0)[..], "edges {edges:?}");
            for (i, &v) in d.preorder().iter().enumerate().skip(1) {
                let p = d.preorder().iter().position(|&x| x == d.idom[v as usize]).unwrap();
                assert!(p < i);
            }
        }
    }
}
