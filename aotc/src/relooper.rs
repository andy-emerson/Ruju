//! Structured control flow from an arbitrary reducible CFG — Ramsey,
//! "Beyond Relooper" (ICFP 2022, https://doi.org/10.1145/3547621).
//!
//! The algorithm: compute reverse postorder and the dominator tree; a node
//! that is the target of a back edge becomes a `loop`, a node with two or
//! more forward in-edges becomes a merge node placed behind a `block`; code
//! is emitted by walking the dominator tree, and every branch becomes either
//! a `br` to an enclosing frame (backward → its loop's header frame, forward
//! to a merge → its block frame) or inline emission of the dominated target.
//! Irreducible CFGs are out of scope (Julia's lowered IR is reducible); the
//! router detects and rejects them loudly rather than emitting node-splitting.
//!
//! This module owns the *shape* (which frames open where, where each branch
//! lands); the caller supplies the leaf emission (statements, φ-moves,
//! conditions) through the `Emit` trait, keeping the algorithm independent of
//! the instruction encoder.

/// The CFG, blocks numbered 0-based, entry = 0.
pub struct Cfg {
    pub succs: Vec<Vec<usize>>,
    pub preds: Vec<Vec<usize>>,
}

/// What the walk asks of its caller at the leaves.
pub trait Emit {
    /// Open a `loop` frame (header `x`).
    fn open_loop(&mut self, x: usize);
    /// Open a `block` frame whose end is the merge node `follow`.
    fn open_block(&mut self, follow: usize);
    /// Close the innermost frame.
    fn close(&mut self);
    /// Emit block `x`'s straight-line statements (φs excluded).
    fn stmts(&mut self, x: usize);
    /// Emit the φ-moves for the edge `x → target`.
    fn phi_moves(&mut self, x: usize, target: usize);
    /// Emit `br` to the frame `depth` levels out (0 = innermost).
    fn br(&mut self, depth: u32);
    /// Emit the condition of `x`'s GotoIfNot and open an `if` frame whose
    /// then-arm is entered when the condition holds.
    fn open_if(&mut self, x: usize);
    /// Switch the open `if` frame to its else-arm.
    fn else_arm(&mut self);
    /// Emit block `x`'s Return terminator.
    fn ret(&mut self, x: usize);
}

/// A block's terminator, as the walk needs to see it.
pub enum Term {
    /// `goto dest if not cond`: `then_` is the fallthrough (condition holds),
    /// `else_` the branch target.
    If { then_: usize, else_: usize },
    Goto(usize),
    Return,
}

enum Frame {
    Loop(usize),
    Block(usize),
    If,
}

pub struct Relooper {
    /// rpo position per block (entry = 0).
    pos: Vec<usize>,
    /// immediate dominator per block.
    idom: Vec<usize>,
    /// dominator-tree children, each list sorted by rpo position.
    dom_children: Vec<Vec<usize>>,
    is_loop_header: Vec<bool>,
    is_merge: Vec<bool>,
    frames: Vec<Frame>,
}

impl Relooper {
    pub fn new(cfg: &Cfg) -> Result<Relooper, String> {
        let n = cfg.succs.len();
        let order = rpo(cfg);
        if order.len() != n {
            return Err("unreachable blocks in CFG".into());
        }
        let mut pos = vec![0usize; n];
        for (p, &b) in order.iter().enumerate() {
            pos[b] = p;
        }
        let idom = idoms(cfg, &order, &pos);

        // Reducibility: every back edge (by rpo) must target a dominator of
        // its source; anything else is irreducible and out of scope.
        let mut is_loop_header = vec![false; n];
        let mut is_merge = vec![false; n];
        for u in 0..n {
            for &v in &cfg.succs[u] {
                if pos[v] <= pos[u] {
                    if !dominates(&idom, &pos, v, u) {
                        return Err(format!("irreducible CFG: back edge {u} -> {v}"));
                    }
                    is_loop_header[v] = true;
                }
            }
        }
        for v in 0..n {
            let fwd = cfg.preds[v].iter().filter(|&&p| pos[p] < pos[v]).count();
            if fwd >= 2 {
                is_merge[v] = true;
            }
        }

        let mut dom_children = vec![Vec::new(); n];
        for b in 0..n {
            if b != 0 {
                dom_children[idom[b]].push(b);
            }
        }
        for ch in &mut dom_children {
            ch.sort_by_key(|&c| pos[c]);
        }

        Ok(Relooper { pos, idom, dom_children, is_loop_header, is_merge, frames: Vec::new() })
    }

    /// Walk the whole function. `term(x)` reports each block's terminator.
    pub fn run(&mut self, e: &mut impl Emit, term: &impl Fn(usize) -> Term) -> Result<(), String> {
        self.do_tree(0, e, term)
    }

    fn do_tree(
        &mut self,
        x: usize,
        e: &mut impl Emit,
        term: &impl Fn(usize) -> Term,
    ) -> Result<(), String> {
        let merge_children: Vec<usize> = self.dom_children[x]
            .iter()
            .copied()
            .filter(|&c| self.is_merge[c])
            .collect();
        if self.is_loop_header[x] {
            self.frames.push(Frame::Loop(x));
            e.open_loop(x);
            self.node_within(x, &merge_children, e, term)?;
            e.close();
            self.frames.pop();
            Ok(())
        } else {
            self.node_within(x, &merge_children, e, term)
        }
    }

    fn node_within(
        &mut self,
        x: usize,
        merge_children: &[usize],
        e: &mut impl Emit,
        term: &impl Fn(usize) -> Term,
    ) -> Result<(), String> {
        if let Some((&last, rest)) = merge_children.split_last() {
            // The furthest merge child's code follows the block frame; every
            // branch to it inside becomes a br out of that frame.
            self.frames.push(Frame::Block(last));
            e.open_block(last);
            self.node_within(x, rest, e, term)?;
            e.close();
            self.frames.pop();
            self.do_tree(last, e, term)
        } else {
            e.stmts(x);
            match term(x) {
                Term::If { then_, else_ } => {
                    e.open_if(x);
                    self.frames.push(Frame::If);
                    self.do_branch(x, then_, e, term)?;
                    e.else_arm();
                    self.do_branch(x, else_, e, term)?;
                    e.close();
                    self.frames.pop();
                    Ok(())
                }
                Term::Goto(t) => self.do_branch(x, t, e, term),
                Term::Return => {
                    e.ret(x);
                    Ok(())
                }
            }
        }
    }

    fn do_branch(
        &mut self,
        x: usize,
        target: usize,
        e: &mut impl Emit,
        term: &impl Fn(usize) -> Term,
    ) -> Result<(), String> {
        e.phi_moves(x, target);
        if self.pos[target] <= self.pos[x] {
            // Backward: br to the target's loop frame.
            let depth = self.depth_of(|f| matches!(f, Frame::Loop(h) if *h == target))?;
            e.br(depth);
            Ok(())
        } else if self.is_merge[target] {
            // Forward to a merge node: br out of its block frame.
            let depth = self.depth_of(|f| matches!(f, Frame::Block(m) if *m == target))?;
            e.br(depth);
            Ok(())
        } else {
            // Dominated, single-entry: emit inline.
            debug_assert_eq!(self.idom[target], x);
            self.do_tree(target, e, term)
        }
    }

    fn depth_of(&self, pred: impl Fn(&Frame) -> bool) -> Result<u32, String> {
        for (d, f) in self.frames.iter().rev().enumerate() {
            if pred(f) {
                return Ok(d as u32);
            }
        }
        Err("branch target not on the frame stack".into())
    }
}

/// Reverse postorder from the entry (iterative DFS, edges in listed order).
fn rpo(cfg: &Cfg) -> Vec<usize> {
    let n = cfg.succs.len();
    let mut visited = vec![false; n];
    let mut post = Vec::with_capacity(n);
    // (node, next-successor-index) stack.
    let mut stack = vec![(0usize, 0usize)];
    visited[0] = true;
    while let Some(&mut (u, ref mut i)) = stack.last_mut() {
        if *i < cfg.succs[u].len() {
            let v = cfg.succs[u][*i];
            *i += 1;
            if !visited[v] {
                visited[v] = true;
                stack.push((v, 0));
            }
        } else {
            post.push(u);
            stack.pop();
        }
    }
    post.reverse();
    post
}

/// Immediate dominators — Cooper–Harvey–Kennedy, "A Simple, Fast Dominance
/// Algorithm" (iterating to a fixed point in rpo order).
fn idoms(cfg: &Cfg, order: &[usize], pos: &[usize]) -> Vec<usize> {
    let n = cfg.succs.len();
    const UNDEF: usize = usize::MAX;
    let mut idom = vec![UNDEF; n];
    idom[0] = 0;
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().skip(1) {
            let mut new_idom = UNDEF;
            for &p in &cfg.preds[b] {
                if idom[p] == UNDEF {
                    continue;
                }
                new_idom = if new_idom == UNDEF { p } else { intersect(&idom, pos, p, new_idom) };
            }
            if new_idom != UNDEF && idom[b] != new_idom {
                idom[b] = new_idom;
                changed = true;
            }
        }
    }
    idom
}

fn intersect(idom: &[usize], pos: &[usize], mut a: usize, mut b: usize) -> usize {
    while a != b {
        while pos[a] > pos[b] {
            a = idom[a];
        }
        while pos[b] > pos[a] {
            b = idom[b];
        }
    }
    a
}

fn dominates(idom: &[usize], pos: &[usize], a: usize, mut b: usize) -> bool {
    // Walk b's dominator chain toward the entry; a dominates b iff met.
    loop {
        if a == b {
            return true;
        }
        if pos[b] == 0 {
            return false;
        }
        b = idom[b];
    }
}
