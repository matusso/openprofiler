//! Control-flow structuring: turns the block graph into nested Java statements (if/else, loops,
//! switch, try/catch, synchronized) using dominators and post-dominators.

use crate::ir::*;
use crate::method::Body;
use std::fmt::Write as _;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum LoopKind {
    Forever,
    While(Expr),
    DoWhile(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Catch {
    /// `None` = catch-all (`Throwable`).
    pub types: Vec<Option<Arc<str>>>,
    pub var: Option<VarId>,
    pub body: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    pub keys: Vec<i32>,
    pub default: bool,
    pub body: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Stmt(Stmt),
    If {
        cond: Expr,
        then: Vec<Node>,
        els: Vec<Node>,
    },
    Loop {
        label: u32,
        kind: LoopKind,
        body: Vec<Node>,
    },
    /// `for (init; cond; update)`; `continue` runs `update`.
    For {
        label: u32,
        init: Option<Stmt>,
        cond: Expr,
        update: Vec<Stmt>,
        body: Vec<Node>,
    },
    /// `for (T var : iterable)`.
    ForEach {
        label: u32,
        var: VarId,
        iterable: Expr,
        body: Vec<Node>,
    },
    Switch {
        label: u32,
        value: Expr,
        cases: Vec<Case>,
    },
    Try {
        body: Vec<Node>,
        catches: Vec<Catch>,
    },
    Sync {
        lock: Expr,
        body: Vec<Node>,
    },
    /// `Some(label)` = labeled jump to an enclosing loop or switch.
    Break(Option<u32>),
    Continue(Option<u32>),
    Return(Option<Expr>),
    Throw(Expr),
    Comment(String),
}

pub struct Structured {
    pub nodes: Vec<Node>,
    /// Labels that are referenced by a labeled break/continue.
    pub used_labels: Vec<u32>,
    /// Set when some control flow could not be expressed; the code is then approximate.
    pub warning: Option<String>,
}

const NONE: usize = usize::MAX;

/// Immediate dominators (Cooper, Harvey, Kennedy). `idom[root] == root`; unreachable = NONE.
fn dominators(n: usize, root: usize, succs: &[Vec<usize>]) -> Vec<usize> {
    let mut order = vec![];
    let mut seen = vec![false; n];
    let mut stack = vec![(root, 0usize)];
    seen[root] = true;
    while let Some((v, i)) = stack.pop() {
        if i < succs[v].len() {
            stack.push((v, i + 1));
            let w = succs[v][i];
            if !seen[w] {
                seen[w] = true;
                stack.push((w, 0));
            }
        } else {
            order.push(v);
        }
    }
    order.reverse();
    let mut rpo = vec![NONE; n];
    for (i, &v) in order.iter().enumerate() {
        rpo[v] = i;
    }
    let mut preds = vec![vec![]; n];
    for v in 0..n {
        if rpo[v] == NONE {
            continue;
        }
        for &w in &succs[v] {
            preds[w].push(v);
        }
    }
    let mut idom = vec![NONE; n];
    idom[root] = root;
    let intersect = |idom: &[usize], mut a: usize, mut b: usize| {
        while a != b {
            while rpo[a] > rpo[b] {
                a = idom[a];
            }
            while rpo[b] > rpo[a] {
                b = idom[b];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &v in order.iter().skip(1) {
            let mut new = NONE;
            for &p in &preds[v] {
                if idom[p] == NONE {
                    continue;
                }
                new = if new == NONE { p } else { intersect(&idom, p, new) };
            }
            if new != NONE && idom[v] != new {
                idom[v] = new;
                changed = true;
            }
        }
    }
    idom
}

fn dominates(idom: &[usize], a: usize, mut b: usize) -> bool {
    loop {
        if a == b {
            return true;
        }
        let p = idom[b];
        if p == NONE || p == b {
            return false;
        }
        b = p;
    }
}

/// Immediate post-dominators over `succs`; blocks with `exits[b]` (return/throw) lead to a
/// virtual exit even when they also have exception edges. NONE = only the virtual exit.
fn post_dominators(succs: &[Vec<usize>], exits: &[bool]) -> Vec<usize> {
    let n = succs.len();
    let exit = n;
    let mut rev = vec![vec![]; n + 1];
    for (v, ss) in succs.iter().enumerate() {
        if ss.is_empty() || exits[v] {
            rev[exit].push(v);
        }
        for &s in ss {
            rev[s].push(v);
        }
    }
    let ipdom = dominators(n + 1, exit, &rev);
    ipdom[..n].iter().map(|&p| if p == exit { NONE } else { p }).collect()
}

#[derive(Clone)]
enum Frame {
    Loop { header: BlockId, cont: BlockId, exit: Option<BlockId>, label: u32 },
    Switch { exit: Option<BlockId>, label: u32 },
}

#[derive(Clone, Default)]
struct Ctx {
    stop: Option<BlockId>,
    frames: Vec<Frame>,
    /// Blocks a join point may be in (None = any).
    region: Option<Arc<Vec<bool>>>,
    tries: Vec<usize>,
    /// Loop header being entered (not a `continue`).
    entering: Option<BlockId>,
    /// Do-while latch: emit its statements, then stop.
    latch: Option<BlockId>,
}

struct S<'a> {
    body: &'a mut Body,
    succs: Vec<Vec<BlockId>>,
    idom: Vec<usize>,
    ipdom: Vec<usize>,
    ipdom_exc: Vec<usize>,
    loops: Vec<Option<Arc<Vec<bool>>>>,
    latches: Vec<Vec<BlockId>>,
    preds: Vec<Vec<BlockId>>,
    try_entry: Vec<BlockId>,
    done: Vec<bool>,
    next_label: u32,
    used_labels: Vec<u32>,
    warning: Option<String>,
    budget: usize,
}

pub fn structure(body: &mut Body) -> Structured {
    let n = body.blocks.len();
    let succs: Vec<Vec<BlockId>> = body.blocks.iter().map(|b| b.term.succs()).collect();
    let mut with_exc = succs.clone();
    for (b, blk) in body.blocks.iter().enumerate() {
        for (h, _) in &blk.handlers {
            if !with_exc[b].contains(h) {
                with_exc[b].push(*h);
            }
        }
    }
    // Dominators over normal and exception edges, from a virtual root reaching the entry and
    // (for handlers no try block reaches) every handler.
    let mut droot = with_exc.clone();
    let mut reachable = vec![false; n];
    let mut stack = vec![body.entry];
    while let Some(x) = stack.pop() {
        if !std::mem::replace(&mut reachable[x], true) {
            stack.extend(with_exc[x].iter().copied());
        }
    }
    let mut roots = vec![body.entry];
    for t in &body.tries {
        roots.extend(t.handlers.iter().map(|h| h.1).filter(|&h| !reachable[h]));
    }
    droot.push(roots);
    let idom_all = dominators(n + 1, n, &droot);
    // Children of the virtual root dominate themselves.
    let idom: Vec<usize> =
        idom_all[..n].iter().enumerate().map(|(b, &d)| if d == n { b } else { d }).collect();
    let exits: Vec<bool> =
        body.blocks.iter().map(|b| matches!(b.term, Term::Return(_) | Term::Throw(_))).collect();
    let ipdom = post_dominators(&succs, &exits);
    let ipdom_exc = post_dominators(&with_exc, &exits);

    // Natural loops; bodies include try blocks whose handlers lead back to the header.
    let mut loops: Vec<Option<Vec<bool>>> = vec![None; n];
    let mut latches = vec![vec![]; n];
    let mut preds = vec![vec![]; n];
    for (b, ss) in succs.iter().enumerate() {
        for &s in ss {
            preds[s].push(b);
        }
    }
    let mut exc_preds = preds.clone();
    for (b, ss) in with_exc.iter().enumerate() {
        for &s in ss {
            if !exc_preds[s].contains(&b) {
                exc_preds[s].push(b);
            }
        }
    }
    for (b, ss) in succs.iter().enumerate() {
        for &h in ss {
            if idom[b] == NONE || !dominates_opt(&idom, h, b) {
                continue;
            }
            latches[h].push(b);
            let set = loops[h].get_or_insert_with(|| vec![false; n]);
            set[h] = true;
            let mut stack = vec![b];
            while let Some(x) = stack.pop() {
                if set[x] {
                    continue;
                }
                set[x] = true;
                stack.extend(exc_preds[x].iter().copied());
            }
        }
    }
    let try_entry = body
        .tries
        .iter()
        .map(|t| t.blocks.iter().copied().min_by_key(|&b| body.blocks[b].start).unwrap_or(NONE))
        .collect();
    let preds_all = preds.clone();
    let mut s = S {
        body,
        succs,
        idom,
        ipdom,
        ipdom_exc,
        loops: loops.into_iter().map(|l| l.map(Arc::new)).collect(),
        latches,
        preds: preds_all,
        try_entry,
        done: vec![false; n],
        next_label: 0,
        used_labels: vec![],
        warning: None,
        budget: 200_000,
    };
    let mut nodes = vec![];
    let entry = s.body.entry;
    s.walk(Some(entry), &Ctx::default(), &mut nodes);
    let used_labels = std::mem::take(&mut s.used_labels);
    let mut warning = s.warning.take();
    if warning.is_none() {
        let missed = s.body.tries.iter().flat_map(|t| &t.handlers).any(|h| !s.done[h.1]);
        if missed {
            warning = Some("some exception handlers could not be placed".into());
        }
    }
    let mut nodes = cleanup(nodes, &s.body.vars);
    lift_sync(&mut nodes);
    inline_temps(&mut nodes, &s.body.vars);
    if s.body.ret == "Z" {
        merge_bool_returns(&mut nodes);
    }
    Structured { nodes, used_labels, warning }
}

fn dominates_opt(idom: &[usize], a: usize, b: usize) -> bool {
    idom[a] != NONE && dominates(idom, a, b)
}

impl S<'_> {
    fn fail(&mut self, msg: String) {
        self.warning.get_or_insert(msg);
    }

    fn label(&mut self) -> u32 {
        self.next_label += 1;
        self.next_label
    }

    /// `break`/`continue` when `b` is the target of an enclosing frame.
    fn jump(&mut self, b: BlockId, ctx: &Ctx) -> Option<Node> {
        let innermost_loop = ctx.frames.iter().rposition(|f| matches!(f, Frame::Loop { .. }));
        for (i, f) in ctx.frames.iter().enumerate().rev() {
            let innermost = i + 1 == ctx.frames.len();
            match f {
                Frame::Loop { cont, header, exit, label } => {
                    if b == *cont || b == *header {
                        let lbl = (Some(i) != innermost_loop).then_some(*label);
                        if let Some(l) = lbl {
                            self.used_labels.push(l);
                        }
                        return Some(Node::Continue(lbl));
                    }
                    if Some(b) == *exit {
                        let lbl = (!innermost).then_some(*label);
                        if let Some(l) = lbl {
                            self.used_labels.push(l);
                        }
                        return Some(Node::Break(lbl));
                    }
                }
                Frame::Switch { exit, label } => {
                    if Some(b) == *exit {
                        let lbl = (!innermost).then_some(*label);
                        if let Some(l) = lbl {
                            self.used_labels.push(l);
                        }
                        return Some(Node::Break(lbl));
                    }
                }
            }
        }
        None
    }

    fn is_frame_target(&self, b: BlockId, ctx: &Ctx) -> bool {
        ctx.frames.iter().any(|f| match f {
            Frame::Loop { cont, header, exit, .. } => b == *cont || b == *header || Some(b) == *exit,
            Frame::Switch { exit, .. } => Some(b) == *exit,
        })
    }

    fn join(&self, b: BlockId, ctx: &Ctx) -> Option<BlockId> {
        let j = self.ipdom[b];
        let usable = j != NONE
            && (Some(j) == ctx.stop
                || (!self.is_frame_target(j, ctx) && !ctx.region.as_ref().is_some_and(|r| !r[j])));
        let j = if usable && !(Some(j) != ctx.stop && self.can_duplicate(j, None)) {
            Some(j)
        } else {
            // The post-dominator is missing or just a shared exit (return): look for where the
            // branches really meet, if they do.
            let m = self.merge_point(b, ctx);
            if m.is_some() || !usable { m } else { Some(j) }
        };
        // A join past where the enclosing region stops: the branches end at that stop instead.
        if let (Some(j), Some(stop)) = (j, ctx.stop)
            && self.succs.len() <= 4000
            && j != stop
            && self.reaches(stop, j)
            && self.reaches(b, stop)
        {
            return Some(stop);
        }
        j
    }

    /// Whether `to` is reachable from `from` over normal edges.
    fn reaches(&self, from: BlockId, to: BlockId) -> bool {
        let mut seen = vec![false; self.succs.len()];
        let mut stack = vec![from];
        while let Some(x) = stack.pop() {
            if x == to {
                return true;
            }
            if seen[x] {
                continue;
            }
            seen[x] = true;
            stack.extend(self.succs[x].iter().copied());
            stack.extend(self.body.blocks[x].handlers.iter().map(|h| h.0));
        }
        false
    }

    /// When paths only meet after one of them has left (returned), the post-dominator is the
    /// exit: use the first block both branches reach that leads to every other shared block.
    fn merge_point(&self, b: BlockId, ctx: &Ctx) -> Option<BlockId> {
        const LIMIT: usize = 2000;
        let succs = &self.succs[b];
        if succs.len() < 2 || self.succs.len() > LIMIT {
            return None;
        }
        let allowed = |x: BlockId| {
            x != b
                && !self.is_frame_target(x, ctx)
                && Some(x) != ctx.stop
                && !ctx.region.as_ref().is_some_and(|r| !r[x])
                && dominates(&self.idom, b, x)
        };
        let touches_stop = std::cell::Cell::new(false);
        let reach_from = |start: BlockId| {
            let mut set = vec![false; self.succs.len()];
            let mut stack = vec![start];
            while let Some(x) = stack.pop() {
                if Some(x) == ctx.stop {
                    touches_stop.set(true);
                }
                if set[x] || !allowed(x) {
                    continue;
                }
                set[x] = true;
                stack.extend(self.succs[x].iter().copied());
                stack.extend(self.body.blocks[x].handlers.iter().map(|h| h.0));
            }
            set
        };
        let sets: Vec<Vec<bool>> = succs.iter().map(|&s| reach_from(s)).collect();
        let common: Vec<BlockId> = (0..self.succs.len()).filter(|&x| sets.iter().all(|s| s[x])).collect();
        // Shared exit blocks (returns) are fine to duplicate; prefer a real merge point.
        let real: Vec<BlockId> =
            common.iter().copied().filter(|&x| !self.can_duplicate(x, Some(ctx))).collect();
        if real.is_empty() && touches_stop.get() {
            // The branches only meet where the enclosing region continues.
            return ctx.stop;
        }
        if common.is_empty() {
            return None;
        }
        let pool = if real.is_empty() { &common } else { &real };
        pool.iter().copied().find(|&j| {
            let r = reach_from(j);
            pool.iter().all(|&x| r[x])
        })
    }

    /// Whether a block's code can raise an exception (conservatively: anything but pure
    /// assignments, stores into `this` and returns of pure values).
    fn may_throw(&self, b: BlockId) -> bool {
        let blk = &self.body.blocks[b];
        let is_this = |e: &Expr| matches!(e, Expr::Var(v) if self.body.vars[*v as usize].is_this);
        let quiet = blk.stmts.iter().all(|s| match s {
            Stmt::Assign(_, e) => e.is_pure(),
            Stmt::IPut(o, _, v) => is_this(o) && v.is_pure(),
            Stmt::Line(_) => true,
            _ => false,
        });
        let term_quiet = match &blk.term {
            Term::Return(e) => e.as_ref().is_none_or(Expr::is_pure),
            Term::Throw(_) => false,
            Term::Goto(_) => true,
            Term::If { cond, .. } => cond.is_pure(),
            Term::Switch { value, .. } => value.is_pure(),
        };
        !(quiet && term_quiet)
    }

    /// Blocks reachable from `from` without passing through `avoid`.
    fn reach(&self, from: BlockId, avoid: &[BlockId]) -> Vec<bool> {
        let mut set = vec![false; self.succs.len()];
        let mut stack = vec![from];
        while let Some(x) = stack.pop() {
            if set[x] || avoid.contains(&x) {
                continue;
            }
            set[x] = true;
            stack.extend(self.succs[x].iter().copied());
            stack.extend(self.body.blocks[x].handlers.iter().map(|h| h.0));
        }
        set
    }

    /// Small blocks that end the method may be emitted more than once (shared returns).
    fn can_duplicate(&self, b: BlockId, ctx: Option<&Ctx>) -> bool {
        self.duplicable(b, ctx, 3)
    }

    fn duplicable(&self, b: BlockId, ctx: Option<&Ctx>, depth: u32) -> bool {
        let blk = &self.body.blocks[b];
        if blk.stmts.len() > 2 || self.loops[b].is_some() {
            return false;
        }
        // A copy inside a try body that the original isn't covered by would have its exceptions
        // caught there (`throw t` after a try-with-resources cleanup).
        let uncovered = ctx.is_some_and(|c| c.tries.iter().any(|&r| !self.body.tries[r].blocks.contains(&b)));
        if uncovered {
            let quiet = blk.stmts.iter().all(|s| matches!(s, Stmt::Assign(_, e) if e.is_pure()))
                && match &blk.term {
                    Term::Return(e) => e.as_ref().is_none_or(Expr::is_pure),
                    Term::Goto(_) => true,
                    _ => false,
                };
            if !quiet {
                return false;
            }
        }
        match blk.term {
            Term::Return(_) | Term::Throw(_) => true,
            // A small block that only leads to where the region ends anyway, or to another
            // small exit block.
            Term::Goto(t) => {
                ctx.is_some_and(|c| Some(t) == c.stop || self.is_frame_target(t, c))
                    || (depth > 0 && t != b && self.duplicable(t, ctx, depth - 1))
            }
            _ => false,
        }
    }

    fn walk(&mut self, start: Option<BlockId>, ctx: &Ctx, out: &mut Vec<Node>) {
        let mut cur = start;
        let mut entering = ctx.entering;
        while let Some(b) = cur {
            if self.budget == 0 {
                self.fail("method too complex".into());
                return;
            }
            self.budget -= 1;
            if Some(b) == ctx.stop {
                return;
            }
            let is_entry = entering == Some(b);
            if !is_entry && let Some(j) = self.jump(b, ctx) {
                out.push(j);
                return;
            }
            if let Some(r) = self.entered_try(b, ctx)
                && !(!is_entry && self.loops[b].is_some() && self.loop_contains_handlers(b, r))
            {
                cur = self.try_stmt(r, ctx, out);
                continue;
            }
            if !is_entry && self.loops[b].is_some() && !self.done[b] {
                cur = self.loop_stmt(b, ctx, out);
                continue;
            }
            entering = None;
            if self.done[b] && !self.can_duplicate(b, Some(ctx)) {
                self.fail(format!(
                    "block at {:#x} is reached from several places",
                    self.body.blocks[b].start
                ));
                out.push(Node::Comment(format!("goto L{:04x}", self.body.blocks[b].start)));
                return;
            }
            self.done[b] = true;
            out.extend(self.body.blocks[b].stmts.iter().cloned().map(Node::Stmt));
            if ctx.latch == Some(b) {
                return;
            }
            match self.body.blocks[b].term.clone() {
                Term::Goto(t) => cur = Some(t),
                Term::Return(e) => {
                    out.push(Node::Return(e));
                    return;
                }
                Term::Throw(e) => {
                    out.push(Node::Throw(e));
                    return;
                }
                Term::If { cond, then, els } => {
                    let join = self.join(b, ctx);
                    let c = Ctx { stop: join, entering: None, ..ctx.clone() };
                    let (mut t, mut e) = (vec![], vec![]);
                    self.walk(Some(then), &c, &mut t);
                    self.walk(Some(els), &c, &mut e);
                    out.push(Node::If { cond, then: t, els: e });
                    cur = join;
                }
                Term::Switch { value, cases, default } => {
                    let join = self.join(b, ctx);
                    let label = self.label();
                    let targets = self.case_order(&cases, default, join, ctx);
                    let mut frames = ctx.frames.clone();
                    frames.push(Frame::Switch { exit: join, label });
                    let mut out_cases = vec![];
                    for (i, &t) in targets.iter().enumerate() {
                        let c = Ctx {
                            stop: targets.get(i + 1).copied(),
                            frames: frames.clone(),
                            entering: None,
                            latch: None,
                            ..ctx.clone()
                        };
                        let mut body = vec![];
                        if Some(t) != join {
                            self.walk(Some(t), &c, &mut body);
                        } else {
                            body.push(Node::Break(None));
                        }
                        out_cases.push(Case {
                            keys: cases.iter().filter(|c| c.1 == t).map(|c| c.0).collect(),
                            default: t == default,
                            body,
                        });
                    }
                    out.push(Node::Switch { label, value, cases: out_cases });
                    cur = join;
                }
            }
        }
    }

    /// The outermost try region starting at `b` that the walk is not inside yet.
    fn entered_try(&self, b: BlockId, ctx: &Ctx) -> Option<usize> {
        (0..self.body.tries.len())
            .filter(|&r| self.try_entry[r] == b && !ctx.tries.contains(&r))
            .min_by_key(|&r| self.body.tries[r].depth)
    }

    /// Case targets ordered by key, keeping fallthrough chains (a case flowing into another
    /// case) adjacent; `default` goes last unless a chain needs it elsewhere.
    fn case_order(
        &self,
        cases: &[(i32, BlockId)],
        default: BlockId,
        join: Option<BlockId>,
        ctx: &Ctx,
    ) -> Vec<BlockId> {
        let mut targets: Vec<BlockId> = cases.iter().map(|c| c.1).collect();
        if Some(default) != join {
            targets.push(default);
        }
        targets.sort_unstable();
        targets.dedup();
        let key = |t: BlockId| {
            cases.iter().filter(|c| c.1 == t).map(|c| c.0 as i64).min().unwrap_or(if t == default {
                i64::MAX
            } else {
                i64::MAX - 1
            })
        };
        let c = Ctx { stop: join, ..ctx.clone() };
        // Fallthrough: the first other case target reached from a case before the join.
        let mut ft: Vec<Option<BlockId>> = vec![None; targets.len()];
        for (i, &t) in targets.iter().enumerate() {
            let mut seen = vec![false; self.succs.len()];
            let mut stack: Vec<BlockId> = self.succs[t].clone();
            let mut hit = None;
            while let Some(x) = stack.pop() {
                if seen[x] || Some(x) == join || self.is_frame_target(x, ctx) {
                    continue;
                }
                seen[x] = true;
                if x != t && targets.contains(&x) {
                    if !self.can_duplicate(x, Some(&c)) {
                        hit = Some(x);
                        break;
                    }
                    continue;
                }
                stack.extend(self.succs[x].iter().copied());
            }
            ft[i] = hit;
        }
        let idx = |t: BlockId| targets.iter().position(|&x| x == t);
        let has_incoming: Vec<bool> = targets.iter().map(|&t| ft.contains(&Some(t))).collect();
        let mut heads: Vec<usize> = (0..targets.len()).filter(|&i| !has_incoming[i]).collect();
        heads.sort_by_key(|&i| key(targets[i]));
        let mut order = vec![];
        let mut placed = vec![false; targets.len()];
        for h in heads {
            let mut cur = Some(h);
            while let Some(i) = cur {
                if placed[i] {
                    break;
                }
                placed[i] = true;
                order.push(targets[i]);
                cur = ft[i].and_then(idx);
            }
        }
        for (i, &t) in targets.iter().enumerate() {
            if !placed[i] {
                order.push(t);
            }
        }
        order
    }

    fn loop_contains_handlers(&self, header: BlockId, r: usize) -> bool {
        let Some(set) = &self.loops[header] else { return false };
        self.body.tries[r].handlers.iter().all(|h| set[h.1])
    }

    fn choose_exit(&self, h: BlockId, set: &[bool]) -> Option<BlockId> {
        let mut cands = vec![];
        for (b, ss) in self.succs.iter().enumerate() {
            if !set[b] {
                continue;
            }
            for &s in ss {
                if !set[s] && !cands.contains(&s) {
                    cands.push(s);
                }
            }
        }
        if cands.len() <= 1 {
            return cands.first().copied();
        }
        // Where all exits meet outside the loop (e.g. a search loop's "found"/"not found"
        // paths joining before the code after it).
        let reach_out = |from: BlockId| {
            let mut seen = vec![false; self.succs.len()];
            let mut stack = vec![from];
            while let Some(x) = stack.pop() {
                if seen[x] || set[x] {
                    continue;
                }
                seen[x] = true;
                stack.extend(self.succs[x].iter().copied());
            }
            seen
        };
        let sets: Vec<Vec<bool>> = cands.iter().map(|&c| reach_out(c)).collect();
        let common: Vec<BlockId> = (0..self.succs.len()).filter(|&x| sets.iter().all(|s| s[x])).collect();
        let converge = common.iter().copied().filter(|&c| !self.can_duplicate(c, None)).find(|&c| {
            let r = reach_out(c);
            common.iter().all(|&x| r[x])
        });
        if let Some(c) = converge {
            return Some(c);
        }
        if let Term::If { then, els, .. } = &self.body.blocks[h].term {
            for x in [*then, *els] {
                if cands.contains(&x) {
                    return Some(x);
                }
            }
        }
        let p = self.ipdom[h];
        if p != NONE && cands.contains(&p) {
            return Some(p);
        }
        // Prefer a target that doesn't immediately end the method.
        cands
            .iter()
            .copied()
            .filter(|&c| !self.can_duplicate(c, None))
            .max_by_key(|&c| self.body.blocks[c].start)
            .or_else(|| cands.first().copied())
    }

    fn loop_stmt(&mut self, h: BlockId, ctx: &Ctx, out: &mut Vec<Node>) -> Option<BlockId> {
        let set = self.loops[h].clone().unwrap_or_default();
        let exit = self.choose_exit(h, &set);
        let label = self.label();
        let mut region = (*set).clone();
        region[h] = false;
        let mut c = Ctx {
            stop: None,
            frames: ctx.frames.clone(),
            region: Some(Arc::new(region)),
            tries: ctx.tries.clone(),
            entering: Some(h),
            latch: None,
        };
        let frame = |cont| Frame::Loop { header: h, cont, exit, label };
        let hb = &self.body.blocks[h];
        // while (cond) { ... }
        if hb.stmts.is_empty()
            && let Term::If { cond, then, els } = hb.term.clone()
        {
            let inside = if !set[els] && Some(els) == exit {
                Some((then, cond))
            } else if !set[then] && Some(then) == exit {
                Some((els, cond.negate()))
            } else {
                None
            };
            if let Some((inside, cond)) = inside {
                self.done[h] = true;
                c.entering = None;
                // A latch with only assignments is a `for` update; jumps to it are `continue`.
                if let [l] = self.latches[h][..]
                    && l != h
                    && l != inside
                    && self.body.blocks[l].term == Term::Goto(h)
                    && !self.body.blocks[l].stmts.is_empty()
                    && self.body.blocks[l].stmts.iter().all(|s| matches!(s, Stmt::Assign(..)))
                    && self.body.blocks[l].try_region == self.body.blocks[h].try_region
                {
                    c.frames.push(frame(l));
                    let mut body = vec![];
                    self.walk(Some(inside), &c, &mut body);
                    self.done[l] = true;
                    let update = self.body.blocks[l].stmts.clone();
                    out.push(Node::For { label, init: None, cond, update, body });
                    return exit;
                }
                c.frames.push(frame(h));
                let mut body = vec![];
                self.walk(Some(inside), &c, &mut body);
                out.push(Node::Loop { label, kind: LoopKind::While(cond), body });
                return exit;
            }
        }
        // do { ... } while (cond)
        if let [l] = self.latches[h][..]
            && let Term::If { cond, then, els } = self.body.blocks[l].term.clone()
            && ((then == h && Some(els) == exit) || (els == h && Some(then) == exit))
            && (l == h || self.body.blocks[l].stmts.is_empty() || self.preds[l].len() <= 1)
        {
            let cond = if then == h { cond } else { cond.negate() };
            let has_stmts = !self.body.blocks[l].stmts.is_empty() || l == h;
            c.frames.push(frame(if has_stmts { NONE } else { l }));
            c.latch = has_stmts.then_some(l);
            let mut body = vec![];
            self.walk(Some(h), &c, &mut body);
            if !has_stmts {
                self.done[l] = true;
            }
            out.push(Node::Loop { label, kind: LoopKind::DoWhile(cond), body });
            return exit;
        }
        c.frames.push(frame(h));
        let mut body = vec![];
        self.walk(Some(h), &c, &mut body);
        out.push(Node::Loop { label, kind: LoopKind::Forever, body });
        exit
    }

    fn try_stmt(&mut self, r: usize, ctx: &Ctx, out: &mut Vec<Node>) -> Option<BlockId> {
        let entry = self.try_entry[r];
        let covered = &self.body.tries[r].blocks;
        // Without a post-dominator (handlers that can throw elsewhere), the try still ends where
        // normal flow from its entry leaves the region, when that is one place...
        let mut outs: Vec<BlockId> = vec![];
        let mut seen = vec![false; self.succs.len()];
        let mut stack = vec![entry];
        while let Some(b) = stack.pop() {
            if std::mem::replace(&mut seen[b], true) {
                continue;
            }
            for &s in &self.succs[b] {
                if covered.contains(&s) {
                    stack.push(s);
                } else if !outs.contains(&s) {
                    outs.push(s);
                }
            }
        }
        // That code must stay out of the body when a handler's flow comes back to it, or when
        // it can throw (the handlers would catch it). Otherwise it reads better as the rest of
        // the body.
        let fallout = (outs.len() == 1).then(|| outs[0]).filter(|&o| {
            self.body.tries[r].handlers.iter().any(|h| self.reaches(h.1, o)) || self.may_throw(o)
        });
        let exit = Some(self.ipdom_exc[entry]).filter(|&e| e != NONE).or(fallout);
        let exit = match exit {
            // A merge point that is an enclosing frame's target is handled by break/continue.
            Some(e) if Some(e) == ctx.stop || self.is_frame_target(e, ctx) => Some(e),
            Some(e) if ctx.region.as_ref().is_some_and(|reg| !reg[e]) => None,
            e => e,
        };
        let avoid: Vec<BlockId> = exit.into_iter().collect();
        let region = self.reach(entry, &avoid);
        let mut c = ctx.clone();
        c.tries.push(r);
        c.stop = exit;
        // A try starting at the loop header being entered: the header is still being entered.
        c.entering = ctx.entering.filter(|&e| e == entry);
        c.region = Some(Arc::new(match &ctx.region {
            Some(outer) => region.iter().zip(outer.iter()).map(|(a, b)| *a && *b).collect(),
            None => region,
        }));
        let mut body = vec![];
        self.walk(Some(entry), &c, &mut body);
        let mut catches: Vec<Catch> = vec![];
        let handlers = self.body.tries[r].handlers.clone();
        for (ty, hb) in handlers {
            if self.done[hb] {
                // Same handler block for several types: `catch (A | B e)`.
                if let Some(last) = catches.last_mut() {
                    last.types.push(ty);
                }
                continue;
            }
            let mut var = None;
            if let Some(Stmt::Assign(v, Expr::CaughtException(_))) = self.body.blocks[hb].stmts.first() {
                var = Some(*v);
                self.body.blocks[hb].stmts.remove(0);
            }
            let hc = Ctx { stop: exit, tries: ctx.tries.clone(), entering: None, ..c.clone() };
            let mut hbody = vec![];
            self.walk(Some(hb), &hc, &mut hbody);
            catches.push(Catch { types: vec![ty], var, body: hbody });
        }
        out.push(Node::Try { body, catches });
        exit
    }
}

// ---------------------------------------------------------------- cleanup

fn ends_with_jump(nodes: &[Node]) -> bool {
    match nodes.last() {
        Some(Node::Return(_) | Node::Throw(_) | Node::Break(_) | Node::Continue(_)) => true,
        Some(Node::If { then, els, .. }) => !els.is_empty() && ends_with_jump(then) && ends_with_jump(els),
        Some(Node::Try { body, catches }) => {
            ends_with_jump(body) && catches.iter().all(|c| ends_with_jump(&c.body))
        }
        Some(Node::Sync { body, .. }) => ends_with_jump(body),
        _ => false,
    }
}

/// Removes `continue;` that only jumps to where the loop would go anyway.
fn size(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|n| match n {
            Node::If { then, els, .. } => 1 + size(then) + size(els),
            Node::Loop { body, .. }
            | Node::For { body, .. }
            | Node::ForEach { body, .. }
            | Node::Sync { body, .. } => 1 + size(body),
            Node::Switch { cases, .. } => 1 + cases.iter().map(|c| size(&c.body)).sum::<usize>(),
            Node::Try { body, catches } => {
                1 + size(body) + catches.iter().map(|c| size(&c.body)).sum::<usize>()
            }
            _ => 1,
        })
        .sum()
}

fn strip_trailing_continue(nodes: &mut Vec<Node>) {
    match nodes.last_mut() {
        Some(Node::Continue(None)) => {
            nodes.pop();
        }
        Some(Node::If { then, els, .. }) => {
            strip_trailing_continue(then);
            strip_trailing_continue(els);
        }
        Some(Node::Try { body, catches }) => {
            strip_trailing_continue(body);
            for c in catches {
                strip_trailing_continue(&mut c.body);
            }
        }
        _ => {}
    }
}

fn cleanup(nodes: Vec<Node>, vars: &[VarInfo]) -> Vec<Node> {
    let mut out = vec![];
    for n in nodes {
        match n {
            Node::If { cond, then, els } => {
                let (then, els) = (cleanup(then, vars), cleanup(els, vars));
                // Empty or shorter early-exit branch first: `if (bad) return; rest`.
                let swap = (then.is_empty() && !els.is_empty())
                    || (!els.is_empty()
                        && ends_with_jump(&els)
                        && (!ends_with_jump(&then) || size(&els) < size(&then)));
                let (cond, mut then, mut els) =
                    if swap { (cond.negate(), els, then) } else { (cond, then, els) };
                // `v = c ? a : b`
                if let ([Node::Stmt(Stmt::Assign(v1, a))], [Node::Stmt(Stmt::Assign(v2, b))]) =
                    (&then[..], &els[..])
                    && v1 == v2
                    && is_simple(a)
                    && is_simple(b)
                {
                    let boolean = vars.get(*v1 as usize).and_then(|v| v.ty.as_deref()) == Some("Z");
                    let t = match (a, b) {
                        (Expr::Lit { bits: 1, .. }, Expr::Lit { bits: 0, .. }) if boolean => cond,
                        (Expr::Lit { bits: 0, .. }, Expr::Lit { bits: 1, .. }) if boolean => cond.negate(),
                        _ => Expr::Ternary(Box::new(cond), Box::new(a.clone()), Box::new(b.clone())),
                    };
                    out.push(Node::Stmt(Stmt::Assign(*v1, t)));
                    continue;
                }
                // `if (c) { ...; return; } else { rest }` → `if (c) { ...; return; } rest`
                if !els.is_empty() && ends_with_jump(&then) {
                    out.push(Node::If { cond, then, els: vec![] });
                    out.append(&mut els);
                } else if then.is_empty() && els.is_empty() {
                    out.push(Node::If { cond, then, els });
                } else {
                    out.push(Node::If { cond, then: std::mem::take(&mut then), els });
                }
            }
            Node::For { label, init, cond, update, body } => {
                let mut body = cleanup(body, vars);
                strip_trailing_continue(&mut body);
                invert_last_continue(&mut body);
                out.push(Node::For { label, init, cond, update, body });
            }
            Node::Loop { label, kind, body } => {
                let mut body = cleanup(body, vars);
                strip_trailing_continue(&mut body);
                invert_last_continue(&mut body);
                let kind = match kind {
                    // `while (true) { if (c) break; ... }` → `while (!c) { ... }`
                    LoopKind::Forever => match body.first() {
                        Some(Node::If { cond, then, els })
                            if els.is_empty() && then == &[Node::Break(None)] =>
                        {
                            let c = cond.clone().negate();
                            body.remove(0);
                            LoopKind::While(c)
                        }
                        _ => LoopKind::Forever,
                    },
                    k => k,
                };
                out.push(Node::Loop { label, kind, body });
            }
            Node::Switch { label, value, cases } => {
                let cases = cases.into_iter().map(|c| Case { body: cleanup(c.body, vars), ..c }).collect();
                out.push(Node::Switch { label, value, cases });
            }
            Node::Try { body, catches } => {
                let catches =
                    catches.into_iter().map(|c| Catch { body: cleanup(c.body, vars), ..c }).collect();
                out.push(Node::Try { body: cleanup(body, vars), catches });
            }
            Node::Sync { lock, body } => out.push(Node::Sync { lock, body: cleanup(body, vars) }),
            n => out.push(n),
        }
    }
    out
}

/// `if (c) continue; rest` at the end of a loop body → `if (!c) { rest }`.
fn invert_last_continue(body: &mut Vec<Node>) {
    let Some(i) = body.iter().rposition(
        |n| matches!(n, Node::If { then, els, .. } if els.is_empty() && then == &[Node::Continue(None)]),
    ) else {
        return;
    };
    if i + 1 >= body.len() {
        body.remove(i);
        return;
    }
    let rest: Vec<Node> = body.drain(i + 1..).collect();
    if let Some(Node::If { cond, .. }) = body.pop() {
        body.push(Node::If { cond: cond.negate(), then: rest, els: vec![] });
    }
}

/// `if (c) return true; return e;` → `return c || e;` (and the `false` / `&&` form).
pub fn merge_bool_returns(nodes: &mut Vec<Node>) {
    for n in nodes.iter_mut() {
        match n {
            Node::If { then, els, .. } => {
                merge_bool_returns(then);
                merge_bool_returns(els);
            }
            Node::Loop { body, .. }
            | Node::For { body, .. }
            | Node::ForEach { body, .. }
            | Node::Sync { body, .. } => merge_bool_returns(body),
            Node::Switch { cases, .. } => cases.iter_mut().for_each(|c| merge_bool_returns(&mut c.body)),
            Node::Try { body, catches } => {
                merge_bool_returns(body);
                catches.iter_mut().for_each(|c| merge_bool_returns(&mut c.body));
            }
            _ => {}
        }
    }
    while nodes.len() >= 2 {
        let k = nodes.len();
        let (Node::If { cond, then, els }, Node::Return(Some(tail))) = (&nodes[k - 2], &nodes[k - 1]) else {
            break;
        };
        let [Node::Return(Some(Expr::Lit { bits, .. }))] = then.as_slice() else { break };
        if !els.is_empty() || !matches!(bits, 0 | 1) {
            break;
        }
        let merged = match (bits, tail) {
            (1, Expr::Lit { bits: 0, .. }) => cond.clone(),
            (0, Expr::Lit { bits: 1, .. }) => cond.clone().negate(),
            (1, t) => Expr::bin(BinOp::LogOr, cond.clone(), t.clone()),
            (_, t) => Expr::bin(BinOp::LogAnd, cond.clone().negate(), t.clone()),
        };
        nodes.truncate(k - 2);
        nodes.push(Node::Return(Some(merged)));
    }
}

fn is_simple(e: &Expr) -> bool {
    !matches!(e, Expr::Ternary(..)) && e.is_pure()
}

/// `monitor-enter x; try { ...; monitor-exit x } catch (Throwable t) { monitor-exit x; throw t; }`
/// → `synchronized (x) { ... }`.
fn lift_sync(nodes: &mut Vec<Node>) {
    let mut i = 0;
    while i < nodes.len() {
        match &mut nodes[i] {
            Node::If { then, els, .. } => {
                lift_sync(then);
                lift_sync(els);
            }
            Node::Loop { body, .. } | Node::For { body, .. } | Node::Sync { body, .. } => lift_sync(body),
            Node::Switch { cases, .. } => cases.iter_mut().for_each(|c| lift_sync(&mut c.body)),
            Node::Try { body, catches } => {
                lift_sync(body);
                catches.iter_mut().for_each(|c| lift_sync(&mut c.body));
            }
            _ => {}
        }
        if let Node::Stmt(Stmt::MonitorEnter(lock)) = &nodes[i]
            && let Some(Node::Try { body, catches }) = nodes.get(i + 1)
            && catches.len() == 1
            && catches[0].types == [None]
            && matches!(catches[0].body.first(), Some(Node::Stmt(Stmt::MonitorExit(_))))
        {
            let lock = lock.clone();
            let mut inner = body.clone();
            remove_monitor_exits(&mut inner);
            nodes.splice(i..i + 2, [Node::Sync { lock, body: inner }]);
            // A trailing exit on the normal path follows the try.
            if let Some(Node::Stmt(Stmt::MonitorExit(_))) = nodes.get(i + 1) {
                nodes.remove(i + 1);
            }
        }
        i += 1;
    }
}

fn remove_monitor_exits(nodes: &mut Vec<Node>) {
    nodes.retain(|n| !matches!(n, Node::Stmt(Stmt::MonitorExit(_))));
    for n in nodes {
        match n {
            Node::If { then, els, .. } => {
                remove_monitor_exits(then);
                remove_monitor_exits(els);
            }
            Node::Loop { body, .. } | Node::For { body, .. } => remove_monitor_exits(body),
            Node::Switch { cases, .. } => cases.iter_mut().for_each(|c| remove_monitor_exits(&mut c.body)),
            _ => {}
        }
    }
}

/// Counts references to each variable in a statement tree.
pub fn count_refs(nodes: &[Node], counts: &mut std::collections::HashMap<VarId, usize>) {
    let add = |e: &Expr, counts: &mut std::collections::HashMap<VarId, usize>| {
        e.visit_vars(&mut |v| *counts.entry(v).or_default() += 1)
    };
    for n in nodes {
        match n {
            Node::Stmt(s) => {
                if let Stmt::Assign(v, _) = s {
                    *counts.entry(*v).or_default() += 1;
                }
                s.visit_exprs(&mut |e| add(e, counts));
            }
            Node::If { cond, then, els } => {
                add(cond, counts);
                count_refs(then, counts);
                count_refs(els, counts);
            }
            Node::Loop { kind, body, .. } => {
                if let LoopKind::While(c) | LoopKind::DoWhile(c) = kind {
                    add(c, counts);
                }
                count_refs(body, counts);
            }
            Node::For { init, cond, update, body, .. } => {
                let stmts: Vec<Node> = init.iter().chain(update).cloned().map(Node::Stmt).collect();
                count_refs(&stmts, counts);
                add(cond, counts);
                count_refs(body, counts);
            }
            Node::ForEach { var, iterable, body, .. } => {
                *counts.entry(*var).or_default() += 1;
                add(iterable, counts);
                count_refs(body, counts);
            }
            Node::Switch { value, cases, .. } => {
                add(value, counts);
                cases.iter().for_each(|c| count_refs(&c.body, counts));
            }
            Node::Try { body, catches } => {
                count_refs(body, counts);
                for c in catches {
                    if let Some(v) = c.var {
                        *counts.entry(v).or_default() += 1;
                    }
                    count_refs(&c.body, counts);
                }
            }
            Node::Sync { lock, body } => {
                add(lock, counts);
                count_refs(body, counts);
            }
            Node::Return(Some(e)) | Node::Throw(e) => add(e, counts),
            _ => {}
        }
    }
}

/// `t = e; return t;` → `return e;` for temporaries (typically ternary results) used once.
fn inline_temps(nodes: &mut Vec<Node>, vars: &[VarInfo]) {
    for _ in 0..4 {
        let mut counts = std::collections::HashMap::new();
        count_refs(nodes, &mut counts);
        let before = size(nodes);
        inline_temps_in(nodes, vars, &counts);
        if size(nodes) == before {
            break;
        }
    }
}

fn inline_temps_in(
    nodes: &mut Vec<Node>,
    vars: &[VarInfo],
    counts: &std::collections::HashMap<VarId, usize>,
) {
    for i in 0..nodes.len() {
        match &mut nodes[i] {
            Node::If { then, els, .. } => {
                inline_temps_in(then, vars, counts);
                inline_temps_in(els, vars, counts);
            }
            Node::Loop { body, .. }
            | Node::For { body, .. }
            | Node::ForEach { body, .. }
            | Node::Sync { body, .. } => inline_temps_in(body, vars, counts),
            Node::Switch { cases, .. } => {
                cases.iter_mut().for_each(|c| inline_temps_in(&mut c.body, vars, counts))
            }
            Node::Try { body, catches } => {
                inline_temps_in(body, vars, counts);
                catches.iter_mut().for_each(|c| inline_temps_in(&mut c.body, vars, counts));
            }
            _ => {}
        }
        if i + 1 >= nodes.len() {
            continue;
        }
        let Node::Stmt(Stmt::Assign(v, _)) = &nodes[i] else { continue };
        let v = *v;
        let vi = &vars[v as usize];
        if vi.name.is_some() || vi.is_this || vi.param.is_some() || counts.get(&v) != Some(&2) {
            continue;
        }
        let Node::Stmt(Stmt::Assign(_, e)) = &nodes[i] else { continue };
        // A pure value can move anywhere into the next statement; others only into a bare use.
        let pure = e.is_pure();
        let is_v = |x: &Expr| *x == Expr::Var(v);
        // The value is the first thing the statement evaluates that could observe a difference.
        let direct = matches!(&nodes[i + 1],
            Node::Return(Some(Expr::Var(x))) | Node::Throw(Expr::Var(x)) | Node::Stmt(Stmt::Assign(_, Expr::Var(x))) if *x == v)
            || match &nodes[i + 1] {
                Node::Stmt(Stmt::IPut(o, _, x)) => is_v(x) && o.is_pure(),
                Node::Stmt(Stmt::SPut(_, x)) => is_v(x),
                Node::Stmt(Stmt::APut(a, k, x, _)) => is_v(x) && a.is_pure() && k.is_pure(),
                Node::Stmt(Stmt::Expr(Expr::Invoke(_, _, args) | Expr::New(_, args))) => {
                    args.iter().position(is_v).is_some_and(|p| args[..p].iter().all(Expr::is_pure))
                }
                _ => false,
            };
        let next_simple =
            matches!(&nodes[i + 1], Node::Return(Some(_)) | Node::Throw(_) | Node::Stmt(_) | Node::If { .. });
        let mut used_next = std::collections::HashMap::new();
        match &nodes[i + 1] {
            Node::If { cond, .. } => cond.visit_vars(&mut |x| *used_next.entry(x).or_insert(0usize) += 1),
            n @ (Node::Return(_) | Node::Throw(_) | Node::Stmt(_)) => {
                count_refs(std::slice::from_ref(n), &mut used_next)
            }
            _ => {}
        }
        if used_next.get(&v) != Some(&1) || !(direct || (pure && next_simple)) {
            continue;
        }
        // Lambda bodies printed inline refer to captured values by name.
        let mut captured = false;
        let mut find = |x: &Expr| {
            x.visit(&mut |y| {
                if let Expr::Lambda(_, caps) = y {
                    captured |= caps.contains(&Expr::Var(v));
                }
            })
        };
        match &nodes[i + 1] {
            Node::Return(Some(x)) | Node::Throw(x) | Node::If { cond: x, .. } => find(x),
            Node::Stmt(st) => st.visit_exprs(&mut find),
            _ => {}
        }
        if captured {
            continue;
        }
        let Node::Stmt(Stmt::Assign(_, e)) = std::mem::replace(&mut nodes[i], Node::Comment(String::new()))
        else {
            continue;
        };
        let mut e = Some(e);
        let mut sub = |x: &mut Expr| {
            if *x == Expr::Var(v)
                && let Some(e) = e.take()
            {
                *x = e;
            }
        };
        match &mut nodes[i + 1] {
            Node::Return(Some(x)) | Node::Throw(x) | Node::If { cond: x, .. } => x.visit_mut(&mut sub),
            Node::Stmt(st) => st.visit_exprs_mut(&mut |x| x.visit_mut(&mut sub)),
            _ => {}
        }
    }
    nodes.retain(|n| !matches!(n, Node::Comment(c) if c.is_empty()));
}

/// Text dump of a method's blocks (for debugging the decompiler).
pub fn dump(body: &Body) -> String {
    let mut o = String::new();
    for (i, b) in body.blocks.iter().enumerate() {
        let _ = writeln!(
            o,
            "B{i} @{:#x}..{:#x} preds={:?} try={:?} handlers={:?}",
            b.start,
            b.end,
            b.preds,
            b.try_region,
            b.handlers.iter().map(|h| h.0).collect::<Vec<_>>()
        );
        for s in &b.stmts {
            let _ = writeln!(o, "    {s:?}");
        }
        let _ = writeln!(o, "    -> {:?}", b.term);
    }
    for (i, v) in body.vars.iter().enumerate() {
        let _ = writeln!(o, "v{i}: {:?} {:?} reg={}", v.name, v.ty, v.reg);
    }
    for (i, t) in body.tries.iter().enumerate() {
        let _ = writeln!(
            o,
            "try{i} blocks={:?} handlers={:?} parent={:?}",
            t.blocks,
            t.handlers.iter().map(|h| h.1).collect::<Vec<_>>(),
            t.parent
        );
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dominator_trees() {
        // 0 → 1 → 3, 0 → 2 → 3
        let succs = vec![vec![1, 2], vec![3], vec![3], vec![]];
        let idom = dominators(4, 0, &succs);
        assert_eq!(idom, vec![0, 0, 0, 0]);
        assert_eq!(post_dominators(&succs, &[false, false, false, true]), vec![3, 3, 3, NONE]);
    }
}
