//! A global prefix trie of call stacks.
//!
//! Every distinct stack is a node; a node stores its parent and its leaf frame, so a stack is the
//! path from the node up to the root. Samples refer to stacks by [`StackId`], which keeps sample
//! columns small and makes aggregation a pass over integers.

use crate::symbols::FrameId;
use rustc_hash::FxHashMap;

pub type StackId = u32;

/// The empty stack. Its frame is meaningless.
pub const ROOT_STACK: StackId = 0;

#[derive(Debug, Clone)]
pub struct StackTrie {
    parent: Vec<StackId>,
    frame: Vec<FrameId>,
    depth: Vec<u32>,
    children: FxHashMap<(StackId, FrameId), StackId>,
}

impl Default for StackTrie {
    fn default() -> Self {
        Self::new()
    }
}

impl StackTrie {
    pub fn new() -> Self {
        Self {
            parent: vec![ROOT_STACK],
            frame: vec![u32::MAX],
            depth: vec![0],
            children: FxHashMap::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.len() <= 1
    }

    /// Returns the stack `parent` + `frame`, creating it if needed.
    pub fn push(&mut self, parent: StackId, frame: FrameId) -> StackId {
        if let Some(&id) = self.children.get(&(parent, frame)) {
            return id;
        }
        let id = self.parent.len() as StackId;
        self.parent.push(parent);
        self.frame.push(frame);
        self.depth.push(self.depth[parent as usize] + 1);
        self.children.insert((parent, frame), id);
        id
    }

    /// Interns a stack given root-first frames.
    pub fn intern_root_first<I: IntoIterator<Item = FrameId>>(&mut self, frames: I) -> StackId {
        frames.into_iter().fold(ROOT_STACK, |p, f| self.push(p, f))
    }

    /// Interns a stack given leaf-first frames (the order JVMTI and JFR report them in).
    pub fn intern_leaf_first(&mut self, frames: &[FrameId]) -> StackId {
        self.intern_root_first(frames.iter().rev().copied())
    }

    pub fn parent(&self, id: StackId) -> StackId {
        self.parent[id as usize]
    }

    pub fn frame(&self, id: StackId) -> FrameId {
        self.frame[id as usize]
    }

    pub fn depth(&self, id: StackId) -> u32 {
        self.depth[id as usize]
    }

    /// Frames of the stack, root first, written into `out` (cleared first).
    pub fn frames_root_first(&self, mut id: StackId, out: &mut Vec<FrameId>) {
        out.clear();
        while id != ROOT_STACK {
            out.push(self.frame[id as usize]);
            id = self.parent[id as usize];
        }
        out.reverse();
    }

    /// Raw parent/frame arrays, used for serialization. Index 0 is the root sentinel.
    pub fn raw(&self) -> (&[StackId], &[FrameId]) {
        (&self.parent, &self.frame)
    }

    /// Rebuilds a trie from raw arrays as produced by [`StackTrie::raw`]. Parents must precede
    /// children (which `raw` guarantees).
    pub fn from_raw(parent: &[StackId], frame: &[FrameId]) -> Self {
        let mut t = Self::new();
        for i in 1..parent.len().min(frame.len()) {
            let id = t.push(parent[i], frame[i]);
            debug_assert_eq!(id as usize, i);
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trie_roundtrip() {
        let mut t = StackTrie::new();
        let a = t.intern_root_first([1, 2, 3]);
        let b = t.intern_leaf_first(&[3, 2, 1]);
        assert_eq!(a, b);
        let c = t.intern_root_first([1, 2, 4]);
        assert_eq!(t.parent(c), t.parent(a));
        let mut out = vec![];
        t.frames_root_first(c, &mut out);
        assert_eq!(out, vec![1, 2, 4]);
        let (p, f) = t.raw();
        let t2 = StackTrie::from_raw(p, f);
        assert_eq!(t2.len(), t.len());
        assert_eq!(t2.depth(c), 3);
    }
}
