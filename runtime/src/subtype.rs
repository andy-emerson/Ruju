//! The environment-based subtype algorithm — a faithful core of `src/subtype.c`.
//!
//! Julia decides `A <: B` by recursing with a *type-variable environment*
//! (`jl_stenv_t`): each `where`-bound variable becomes a binding with a current
//! lower/upper bound that the algorithm narrows as it goes. A variable
//! introduced by a `UnionAll` on the **left** of `<:` is *universal* ("for all
//! T"); one introduced on the **right** is *existential* ("there exists T").
//! This is `subtype_unionall`'s `R` flag, stored here as
//! [`VarBinding::existential`].
//!
//! Ported faithfully: the forall/exists treatment of `Union`, covariant tuples,
//! nominal and invariant-parametric `DataType`s, `subtype_var` →
//! `var_lt`/`var_gt` (narrowing a variable's bounds via `simple_meet` /
//! `simple_join`), and the universal-vs-existential dispatch in `subtype`.
//!
//! Also ported: the diagonal rule, unbounded varargs in tuple tails, the
//! `Type{T}` kind rules (the pin's TypeEq semantics), and — engine slice 1 —
//! the **global union-decision machine**: every `Union` arm choice is a
//! numbered bit in one of two bit-stacks ([`UnionState`]: `Lunions` for
//! left-of-`<:` unions, `Runions` for right), and two nested driver loops —
//! [`forall_exists_subtype`] (∀, left) wrapping [`exists_subtype`] (∃, right)
//! — re-run the whole query once per bit combination, enumerated as a
//! lazily-grown binary counter (`subtype.c:35–60, 237–260, 2359–2404`). Local
//! backtracking cannot revisit a choice made by an *ancestor* in the
//! recursion; the machine hoists the enumeration above the query, so each
//! left arm gets a fresh right-side search and fresh existential bindings.
//!
//! Engine slice 3 (2026-07) added the vararg **length algebra**: the
//! `Loffset` channel (`X = Y + Loffset` between two vararg lengths), the
//! `INT`/`BOUND` vararg kinds in the tuple length classification,
//! `check_vararg_length`'s N-discharge, and `subtype_tuple_varargs`' length
//! equation — so `Tuple{Vararg{T,N}}` with a typevar `N` (`NTuple`) works.
//!
//! Deliberately omitted for now (tracked in `design/implementation.md`):
//! type intersection and the `Intersect` meet node (slice 4), `envout`
//! (slice 5), and the repeated-element/separable tuple fast paths (pure
//! optimizations).
//!
//! GC rooting (the C's discipline, engine slice 1 first commit): the entry
//! roots the query types (the C leaves this to callers; our host boundary
//! passes raw offsets); each binding's mutable `lb`/`ub` stays rooted through
//! a per-frame mirror (`subtype_unionall`'s `JL_GC_PUSH` on `vb.lb`/`vb.ub`);
//! env snapshots root their saved bounds (`jl_savedenv_t.roots`,
//! `subtype.c:331–337`); and the kind rule roots its fresh intermediates.
//! Everything else reached mid-query is a subterm of one of those.

use crate::gc::{self, Frame, Rooted};
use crate::object::Value;
use crate::region::{Offset, NULL};
use crate::types::{self, id};

/// Position of a subterm relative to the enclosing constructor, mirroring
/// `jl_param_pos_t`. Threaded through for faithfulness (the diagonal rule keys
/// off covariant occurrences); the core dispatch here does not yet branch on it.
#[derive(Clone, Copy, PartialEq)]
pub enum Param {
    None,
    Covariant,
    Invariant,
}

/// One `jl_varbinding_t`: a `where` variable and its *current* bounds, narrowed
/// during the search. `existential` is `subtype_unionall`'s `R` — set for a
/// variable introduced on the right of `<:` (∃), clear for the left (∀).
#[derive(Clone, Copy)]
struct VarBinding {
    var: Offset,
    lb: Offset,
    ub: Offset,
    existential: bool,
    /// Covariant occurrences of the variable in the current consistency-check
    /// scope, saturating at 2 (`occurs_cov`).
    occurs_cov: i8,
    /// Largest `occurs_cov` reached in any already-closed consistency-check
    /// scope (`cov_diag`). The diagonal-rule test is `max(occurs_cov, cov_diag)
    /// > 1`, so checking one variable's bound does not make another diagonal.
    cov_diag: i8,
    /// Invariant-position occurrences at a depth below the variable's
    /// introduction, saturating at 2 (`occurs_inv`, `subtype.c:72,898–900`).
    /// Recorded now; consumed by the envout fill and `Type{x}` widening
    /// (slices 4–5) — an invariant occurrence *at* `depth0` still counts as
    /// covariant, which is why the counter changes `occurs_cov` too.
    occurs_inv: i8,
    /// Whether the variable occurs invariantly in its `UnionAll` body —
    /// `var_occurs_invariant(u->body, u->var)`, computed once at push as the
    /// pin does (`subtype.c:1381,1385`) and consumed by the diagonal decision
    /// and `env_unchanged`'s became-diagonal check.
    body_occurs_inv: bool,
    /// Invariant-constructor nesting depth at which the variable was introduced
    /// (`depth0`). Distinguishes `∀A ∃B` from `∃B ∀A` when an existential and a
    /// universal variable interact.
    depth0: i32,
    /// The variable must be integer-valued — it occurs as `N` in `Vararg{_,N}`
    /// (`intvalued`, `subtype.c:94`; set by `subtype_tuple_varargs`).
    intvalued: bool,
    /// Another variable's diagonal constraint forces this one concrete
    /// (`concrete`, `subtype.c:85`; set through the binding of a diagonal
    /// variable's typevar lower bound, `:1411–1415`, and consumed at this
    /// binding's own pop). Like `intvalued`, not part of the C's saved-env
    /// record — restores preserve it.
    concrete: bool,
    /// Maximum positive vararg-length offset seen (`max_offset`,
    /// `subtype.c:86–87`); `-1` once the variable occurs outside a vararg-`N`
    /// slot. Bookkeeping the pin's intersection consumes; carried in the env
    /// snapshot (the C's saved-env slot 4) and kept faithful here.
    max_offset: i8,
    /// Absolute shadow-stack index of this binding's rooted `{lb, ub}` mirror
    /// (a 2-slot [`Frame`] owned by `subtype_unionall`). Narrowing writes
    /// through it, so the current bounds are always GC roots.
    root_base: usize,
}

/// One union decision bit-stack (`jl_unionstate_t`, `subtype.c:48–53`): a
/// lazily-grown binary counter over the union decision points a traversal
/// discovers. Bit `i` = 0 chooses `Union.a` at the `i`th decision point,
/// 1 chooses `Union.b`.
///
/// - `depth` — index of the next decision point to *read* in the current
///   traversal; reset to 0 at the start of every pass.
/// - `used` — number of bits currently meaningful (how deep the previous
///   pass got). `depth >= used` means a new decision point was discovered.
/// - `more` — the deepest choice point read as 0 (an untried alternative
///   remains); 0 ⇒ the enumeration is exhausted.
#[derive(Default)]
struct UnionState {
    depth: i32,
    more: i32,
    used: i32,
    /// The bit store (the C chains 16×u32 chunks, `jl_bits_stack_t`; a
    /// growable `Vec` is the same store without the chaining).
    bits: Vec<u32>,
}

impl UnionState {
    /// `statestack_get`.
    fn get(&self, i: i32) -> bool {
        let (w, b) = ((i as usize) >> 5, (i as usize) & 31);
        w < self.bits.len() && self.bits[w] & (1 << b) != 0
    }

    /// `statestack_set` (grows the store on demand).
    fn set(&mut self, i: i32, val: bool) {
        let (w, b) = ((i as usize) >> 5, (i as usize) & 31);
        if w >= self.bits.len() {
            self.bits.resize(w + 1, 0);
        }
        if val {
            self.bits[w] |= 1 << b;
        } else {
            self.bits[w] &= !(1 << b);
        }
    }
}

/// A snapshot of one union state (`jl_saved_unionstate_t`): counters plus the
/// first `used` bits. `push_unionstate`/`pop_unionstate` (`subtype.c:273–306`)
/// shield an inner computation's union state from its surroundings.
struct SavedUnionState {
    depth: i32,
    more: i32,
    used: i32,
    bits: Vec<u32>,
}

fn push_unionstate(src: &UnionState) -> SavedUnionState {
    let words = (src.used as usize + 31) / 32;
    let mut bits = vec![0u32; words];
    bits.copy_from_slice(&src.bits[..words.min(src.bits.len())]);
    SavedUnionState { depth: src.depth, more: src.more, used: src.used, bits }
}

fn pop_unionstate(dst: &mut UnionState, saved: &SavedUnionState) {
    dst.depth = saved.depth;
    dst.more = saved.more;
    dst.used = saved.used;
    let words = saved.bits.len();
    if dst.bits.len() < words {
        dst.bits.resize(words, 0);
    }
    dst.bits[..words].copy_from_slice(&saved.bits);
}

/// The subtype environment (`jl_stenv_t`): the stack of variable bindings,
/// the current invariant-constructor nesting depth, and the two union
/// decision states the driver loops enumerate.
struct Env {
    vars: Vec<VarBinding>,
    invdepth: i32,
    /// Decisions for unions on the left of `<:` (`Lunions`).
    lunions: UnionState,
    /// Decisions for unions on the right of `<:` (`Runions`).
    runions: UnionState,
    /// The vararg length-offset channel (`jl_stenv_t.Loffset`,
    /// `subtype.c:138–140`): while comparing two vararg length expressions,
    /// the left length equals the right length **plus** `loffset`
    /// (`X = Y + Loffset`). Nonzero only inside `subtype_tuple_varargs`'
    /// N-equation; `flip_offset` negates it for the reverse direction.
    loffset: i32,
}

impl Env {
    fn new() -> Env {
        Env {
            vars: Vec::new(),
            invdepth: 0,
            lunions: UnionState::default(),
            runions: UnionState::default(),
            loffset: 0,
        }
    }

    /// Innermost binding for `var`, if it is in scope (`lookup_binding`).
    fn lookup(&self, var: Offset) -> Option<usize> {
        self.vars.iter().rposition(|b| b.var == var)
    }

    fn state(&mut self, r: bool) -> &mut UnionState {
        if r {
            &mut self.runions
        } else {
            &mut self.lunions
        }
    }

    /// `next_union_state` (`subtype.c:237–246`): the binary-counter
    /// increment. Truncate to the deepest untried choice point, flip its bit
    /// to 1, and let `pick_union_decision` re-initialize anything deeper as
    /// it is rediscovered. Returns false when the enumeration is exhausted.
    fn next_union_state(&mut self, r: bool) -> bool {
        let st = self.state(r);
        if st.more == 0 {
            return false;
        }
        st.used = st.more;
        let i = st.used - 1;
        st.set(i, true);
        true
    }

    /// `pick_union_decision` (`subtype.c:248–260`): read (or discover) the
    /// decision bit at the current traversal depth. Reading a 0 records this
    /// as the deepest choice point that still has an untried alternative.
    fn pick_union_decision(&mut self, r: bool) -> bool {
        let st = self.state(r);
        if st.depth >= st.used {
            let i = st.used;
            st.set(i, false);
            st.used += 1;
        }
        let ui = st.get(st.depth);
        st.depth += 1;
        if !ui {
            st.more = st.depth; // deepest available choice, memorized
        }
        ui
    }

    /// `pick_union_element` (`subtype.c:262–271`): descend a nested `Union`
    /// spine, one recorded decision per level, to a single leaf arm.
    fn pick_union_element(&mut self, mut u: Offset, r: bool) -> Offset {
        loop {
            u = if self.pick_union_decision(r) {
                types::union_b(u)
            } else {
                types::union_a(u)
            };
            if !types::is_union(u) {
                return u;
            }
        }
    }

    /// Narrow a binding's lower bound, keeping its rooted mirror current.
    fn set_lb(&mut self, idx: usize, v: Offset) {
        self.vars[idx].lb = v;
        gc::set_slot(self.vars[idx].root_base, Value(v));
    }

    /// Narrow a binding's upper bound, keeping its rooted mirror current.
    fn set_ub(&mut self, idx: usize, v: Offset) {
        self.vars[idx].ub = v;
        gc::set_slot(self.vars[idx].root_base + 1, Value(v));
    }
}

/// A rooted snapshot of the environment's bindings (`jl_savedenv_t`,
/// `subtype.c:331–337`): the saved `lb`/`ub` values occupy their own
/// shadow-stack frame (the C's GC-rooted `roots` array), so bounds the
/// search narrows past before a restore cannot be reclaimed meanwhile.
/// `rdepth` rides along (`se->rdepth`, `subtype.c:382`): restoring the env
/// without restoring the right-union bit cursor would desynchronize nested
/// re-traversals from the bits they are meant to re-read (`:476`).
struct SavedVars {
    vars: Vec<VarBinding>,
    rdepth: i32,
    _roots: Frame,
}

/// Snapshot the environment's bindings and root their bounds (`save_env`).
fn save_vars(e: &Env) -> SavedVars {
    let roots = Frame::new(e.vars.len() * 2);
    for (i, b) in e.vars.iter().enumerate() {
        roots.set(i * 2, Value(b.lb));
        roots.set(i * 2 + 1, Value(b.ub));
    }
    SavedVars { vars: e.vars.clone(), rdepth: e.runions.depth, _roots: roots }
}

/// Re-snapshot into an existing save (`re_save_env`): after a successful ∀
/// pass, the accumulated constraints become the state later restores return
/// to — constraints on outer-scope existentials persist across left arms.
/// The binding count matches the original save (balanced push/pop), so the
/// rooting frame is reused in place.
fn re_save_vars(e: &Env, saved: &mut SavedVars) {
    debug_assert_eq!(e.vars.len(), saved.vars.len(), "re-save at a different binding depth");
    for (i, b) in e.vars.iter().enumerate() {
        saved._roots.set(i * 2, Value(b.lb));
        saved._roots.set(i * 2 + 1, Value(b.ub));
    }
    saved.vars.clear();
    saved.vars.extend_from_slice(&e.vars);
    saved.rdepth = e.runions.depth;
}

/// Restore the environment to a snapshot taken at the same binding depth
/// (`restore_env`), re-syncing each binding's live bounds mirror and the
/// right-union bit cursor. `concrete` and `intvalued` are *not* part of the
/// C's saved record (`jl_savedenv_t` carries bounds + the four counters,
/// `subtype.c:319–320`) — they persist through restores, so the live values
/// are kept.
fn restore_vars(e: &mut Env, saved: &SavedVars) {
    debug_assert_eq!(e.vars.len(), saved.vars.len());
    for (b, s) in e.vars.iter_mut().zip(saved.vars.iter()) {
        let (concrete, intvalued) = (b.concrete, b.intvalued);
        *b = *s;
        b.concrete = concrete;
        b.intvalued = intvalued;
        gc::set_slot(b.root_base, Value(b.lb));
        gc::set_slot(b.root_base + 1, Value(b.ub));
    }
    e.runions.depth = saved.rdepth;
}

/// Entry point: decide `a <: b` (`jl_subtype_env` → `forall_exists_subtype`).
pub fn subtype(a: Offset, b: Offset) -> bool {
    // The C expects the caller to root the query types; our host boundary
    // passes raw offsets, so the engine entry roots them for the query's
    // duration (any type reached below is a subterm of these, of a rooted
    // bound, or is itself freshly rooted at its allocation site).
    let _ra = Rooted::new(Value(a));
    let _rb = Rooted::new(Value(b));
    let mut e = Env::new();
    forall_exists_subtype(a, b, &mut e, Param::None)
}

/// The ∀ driver (`forall_exists_subtype`, `subtype.c:2383–2404`): enumerate
/// left-union arm combinations; each gets a complete fresh ∃ search. Failed
/// ∃ attempts roll the env back to the snapshot; each *successful* ∀ pass
/// re-saves, so constraints recorded on outer existentials accumulate across
/// left arms (all arms must be satisfied by one assignment of any variable
/// bound outside the split).
fn forall_exists_subtype(x: Offset, y: Offset, e: &mut Env, param: Param) -> bool {
    // The depth recursion has the following shape, after simplification:
    // ∀₁ { ∃₁ }
    debug_assert_eq!(e.runions.depth, 0);
    debug_assert_eq!(e.lunions.depth, 0);
    let mut se = save_vars(e);
    e.lunions.used = 0;
    loop {
        let sub = exists_subtype(x, y, e, &se, param);
        if !sub || !e.next_union_state(false) {
            return sub;
        }
        re_save_vars(e, &mut se);
    }
}

/// The ∃ driver (`exists_subtype`, `subtype.c:2359–2381`): under one fixed
/// set of left-arm bits, enumerate right-union arm combinations until one
/// full traversal succeeds. The right enumeration restarts fresh per ∀ pass
/// (`Runions.used = 0`); the fixed L bits are re-read each attempt by
/// resetting only the cursor and `more`.
fn exists_subtype(x: Offset, y: Offset, e: &mut Env, se: &SavedVars, param: Param) -> bool {
    e.runions.used = 0;
    loop {
        e.runions.depth = 0;
        e.runions.more = 0;
        e.lunions.depth = 0;
        e.lunions.more = 0;
        if sub(x, y, e, param) {
            return true;
        }
        let more = e.next_union_state(true);
        restore_vars(e, se);
        if !more {
            return false;
        }
    }
}

/// The main algorithm (`subtype` in `subtype.c:1903`).
fn sub(mut x: Offset, mut y: Offset, e: &mut Env, param: Param) -> bool {
    if x == y && e.loffset == 0 {
        // Reflexive / uniqued-identical fast path — except under a nonzero
        // length offset, where the same typevar `N` on both sides must still
        // discharge `N = N + Loffset` through the variable machinery.
        return true;
    }

    // Union on the left (`subtype.c:1905–1932`): pick ONE arm per the current
    // `Lunions` bits and continue — the ∀ obligation ("every arm") is
    // discharged by the outer driver re-running the query, not by `&&` here.
    if types::is_union(x) {
        if obviously_egal(x, y) {
            return true;
        }
        // Typevar-right fast path (`:1908–1931`, minus the intersection arm):
        // with no right-union decisions pending and a ground union on the
        // left, handle the variable against the whole union — matching or
        // rejecting it wholesale via the binding's bounds. Skipped when the
        // binding's upper bound references another existential, whose
        // accumulated env changes could falsify the local check.
        if e.runions.depth == 0 && types::is_typevar(y) && !has_free_typevars(x) {
            let handle = match e.lookup(y) {
                Some(i) => {
                    !e.vars[i].existential || !has_existential_typevar(e.vars[i].ub, e)
                }
                None => true,
            };
            if handle {
                return subtype_var(y, x, e, true, param);
            }
        }
        x = e.pick_union_element(x, false);
    }
    // Union on the right (`subtype.c:1934–1951`): a left `UnionAll`
    // introduces its ∀ variable *before* the union splits; a left typevar
    // makes even the split-or-not decision a recorded machine choice (the
    // `convert(Type{T},T)` pattern — try the whole union against the
    // variable first, revisitably). Otherwise pick ONE arm per the current
    // `Runions` bits; the ∃ obligation is the inner driver loop's.
    if types::is_union(y) {
        if obviously_in_union(y, x) {
            return true;
        }
        if types::is_unionall(x) {
            return subtype_unionall(y, x, e, false, param);
        }
        let mut ui = true;
        if types::is_typevar(x) {
            // For a forall var there is no need to split y unless it has
            // free typevars (`:1940–1948`).
            let xx_existential = e.lookup(x).map_or(false, |i| e.vars[i].existential);
            ui = (xx_existential || has_free_typevars(y)) && e.pick_union_decision(true);
        }
        if ui {
            y = e.pick_union_element(y, true);
        }
    }

    // An internal `Intersect` meet node is only ever produced as an
    // existential upper bound, so it can appear on the right (`x <: a ∩ b`)
    // but never on the left (`subtype.c:1948–1961`).
    debug_assert!(!types::is_intersect(x), "Intersect can only appear on the right");
    if types::is_intersect(y) {
        // `x <: a ∩ b`  iff  `x <: a` and `x <: b` (dual to Union-left).
        return sub(x, types::intersect_a(y), e, param)
            && sub(x, types::intersect_b(y), e, param);
    }

    // Type variables, handled before the ground cases (as in subtype.c).
    if types::is_typevar(x) {
        if types::is_typevar(y) {
            return subtype_two_vars(x, y, e, param);
        }
        if types::is_unionall(y) {
            // Unwrap `y::UnionAll` eagerly only for a ∀-var `x` whose bound
            // is not `y` itself (`subtype.c:2036–2048`): an ∃-var must go
            // through `subtype_var` so the UnionAll lands in its narrowed
            // upper bound instead of being opened against the variable.
            let xb = e.lookup(x);
            let unwrap = xb.map_or(true, |i| !e.vars[i].existential);
            let xub = xb.map_or(x, |i| e.vars[i].ub);
            if unwrap && xub != y {
                return subtype_unionall(x, y, e, true, param);
            }
        }
        return subtype_var(x, y, e, false, param);
    }
    if types::is_typevar(y) {
        return subtype_var(y, x, e, true, param);
    }

    // Ground fast paths (`Any` on the right, `Bottom` on the left).
    if y == types::builtin(id::ANY) {
        return true;
    }
    if x == types::builtin(id::BOTTOM) {
        return true;
    }

    // `where` types: a left UnionAll is universal (∀, R=0); a right one is
    // existential (∃, R=1).
    if types::is_unionall(x) {
        return subtype_unionall(y, x, e, false, param);
    }
    if types::is_unionall(y) {
        return subtype_unionall(x, y, e, true, param);
    }

    // Kind rules for `Type{T}` (`subtype.c:2094-2121`; the pin phrases them on
    // its TypeEq node — same semantics). Both-`Type{}` comparisons fall through
    // to the ordinary invariant-parametric path below.
    if types::is_type_type(x) && !types::is_type_type(y) {
        let t0 = types::svec_ref(types::parameters_of(x), 0);
        if !types::is_typevar(t0) {
            // `Type{Int}` dispatches as the singleton type of its parameter:
            // `subtype(jl_typeof(tp0), y)` — hence `Type{Int} <: DataType`.
            return sub(crate::object::type_of(crate::object::Value(t0)), y, e, param);
        }
        // `Type{T}` over a typevar is the kind of every matching type:
        // `Type{T} <: y` reduces to `Kind <: y` ("Type === Kind").
        return sub(types::builtin(id::TYPE), y, e, param);
    }
    if types::is_type_type(y) && !types::is_type_type(x) {
        let t0 = types::svec_ref(types::parameters_of(y), 0);
        if types::is_typevar(t0) {
            if !types::is_kind(x) {
                return false;
            }
            // Every instance of a kind is a type: recurse as the full
            // `Type{T'} where T'` against y, binding y's variable. The C
            // recurses via the immortal `jl_type_type` (`subtype.c:2111`);
            // we build it fresh, so each intermediate is rooted across the
            // allocations (and the query) that follow it.
            let v = types::make_typevar("T", types::builtin(id::BOTTOM), types::builtin(id::ANY));
            let _rv = Rooted::new(Value(v));
            let tt = types::type_type(v);
            let _rt = Rooted::new(Value(tt));
            let ua = types::unionall_type(v, tt);
            let _ru = Rooted::new(Value(ua));
            return sub(ua, y, e, param);
        }
        // `Type{Concrete}` has no broader non-`Type{}` subtypes. (The C exempts
        // TypeofBottom; our Bottom already returned through the fast path.)
        return false;
    }

    // Non-type leaves: boxed values as type parameters (the vararg length
    // algebra's boxed longs). The C's tail (`subtype.c:2151–2153`): equal
    // longs modulo the length offset; anything else compares by egal —
    // for us, distinct kinds are simply unequal.
    if types::is_boxed_long(x) || types::is_boxed_long(y) {
        return types::is_boxed_long(x)
            && types::is_boxed_long(y)
            && crate::value::unbox_int(crate::object::Value(x))
                == crate::value::unbox_int(crate::object::Value(y)) + e.loffset as i64;
    }

    datatype_subtype(x, y, e, param)
}

/// `subtype_unionall`: introduce `u`'s variable into the environment with its
/// declared bounds, then descend into the body. `r` is Julia's `R` flag —
/// `true` when the UnionAll is on the right (the variable is existential).
fn subtype_unionall(t: Offset, u: Offset, e: &mut Env, r: bool, param: Param) -> bool {
    let var = types::unionall_var(u);
    let body = types::unionall_body(u);
    let lb = types::tvar_lb(var);
    let ub = types::tvar_ub(var);
    // The binding's mutable bounds stay rooted for the frame's lifetime
    // through this mirror; narrowing writes through it (`Env::set_lb`/
    // `set_ub`) — the C roots `vb.lb`/`vb.ub` per frame for the same reason.
    let mirror = Frame::new(2);
    mirror.set(0, Value(lb));
    mirror.set(1, Value(ub));
    let idx = e.vars.len();
    e.vars.push(VarBinding {
        var,
        lb,
        ub,
        existential: r,
        occurs_cov: 0,
        cov_diag: 0,
        occurs_inv: 0,
        body_occurs_inv: var_occurs_invariant(body, var, false),
        depth0: e.invdepth,
        intvalued: false,
        concrete: false,
        max_offset: 0,
        root_base: mirror.slot_index(0),
    });
    let mut ans = if r {
        sub(t, body, e, param)
    } else {
        sub(body, t, e, param)
    };

    // The diagonal rule (`subtype.c:1400–1420`): a variable occurring more
    // than once and only in covariant position (never invariantly in the
    // body) is constrained to concrete types, so its inferred lower bound
    // must be a leaf type. E.g. `Tuple{Int,Int} <: Tuple{T,T} where T` but
    // not `Tuple{Int,Float64} <: ...`. A variable another variable's
    // diagonal constraint marked `concrete` faces the same bar at its own
    // pop, even without being diagonal itself.
    let vb = e.vars[idx];
    let cov = vb.occurs_cov.max(vb.cov_diag); // cov_count
    let diagonal = cov > 1 && !vb.body_occurs_inv;
    if ans && (vb.concrete || (diagonal && is_leaf_typevar(var))) {
        if vb.concrete && !diagonal && !is_leaf_bound(vb.ub) {
            // A non-diagonal var can only be a subtype of a diagonal var if
            // its upper bound is concrete (`:1406–1410`).
            ans = false;
        } else if types::is_typevar(vb.lb) {
            // A typevar lower bound does not reject — each value of the
            // referenced (universal) variable is a single type — but the
            // concreteness constraint propagates to that variable's binding
            // (`:1411–1415`; closes the tail of audit finding 15).
            if let Some(j) = e.lookup(vb.lb) {
                e.vars[j].concrete = true;
            }
        } else if !is_leaf_bound(vb.lb) {
            ans = false;
        }
    }

    // An internal `Intersect` meet node is exact for subtyping but must not
    // appear in a result type (`subtype.c:1428–1433`); it only ever occurs
    // as the top layer of an existential `ub`. Nothing consumes the popped
    // bound yet — the write-through keeps the placement (and the widened
    // value's rooting) correct for the envout fill that lands with slice 5.
    if ans && types::is_intersect(e.vars[idx].ub) {
        let widened = widen_intersect(e.vars[idx].ub);
        e.set_ub(idx, widened);
    }

    e.vars.pop();
    ans
}

/// `subtype_var`: `b` is a type variable; relate it to the non-variable `a`.
/// `r` follows Julia — `true` constrains `a <: b` (`var_gt`), `false`
/// constrains `b <: a` (`var_lt`).
fn subtype_var(b: Offset, a: Offset, e: &mut Env, r: bool, param: Param) -> bool {
    // Constant folding under a length offset (`subtype.c:1122–1131`):
    // `N (bound) vs 3` under `Loffset = k` becomes `N vs 3±k` at offset 0,
    // so the boxed constraint the binding absorbs already carries the offset.
    if e.loffset != 0 && types::is_boxed_long(a) {
        let old = if r { -e.loffset } else { e.loffset };
        let na = crate::value::box_int(
            crate::value::unbox_int(crate::object::Value(a)) + old as i64,
        );
        let _rna = Rooted::new(na);
        e.loffset = 0;
        let ans = subtype_var(b, na.raw(), e, r, param);
        e.loffset = if r { -old } else { old };
        return ans;
    }
    match e.lookup(b) {
        Some(idx) => {
            if r {
                var_gt(a, e, idx, param)
            } else {
                var_lt(a, e, idx, param)
            }
        }
        None => {
            // A free variable (not bound by an enclosing UnionAll) is compared
            // by its declared bounds (`singleton_typevar_subtype`).
            if r {
                sub(a, types::tvar_lb(b), e, param)
            } else {
                sub(types::tvar_ub(b), a, e, param)
            }
        }
    }
}

/// Fast paths shared by comparisons against an expanded ∀-variable bound
/// (`subtype_left_var`, `subtype.c:875–891`), minus the boxed-long `Loffset`
/// arm (slice 3). The union-egal arm uses [`obviously_egal`] — a sound subset
/// of the C's `jl_egal`; misses fall through to the full algorithm.
fn subtype_left_var(x: Offset, y: Offset, e: &mut Env, param: Param) -> bool {
    // Boxed lengths compare through the offset channel *before* the identity
    // fast path (`subtype.c:877–878` precedes `:879`).
    if types::is_boxed_long(x) && types::is_boxed_long(y) {
        return crate::value::unbox_int(crate::object::Value(x))
            == crate::value::unbox_int(crate::object::Value(y)) + e.loffset as i64;
    }
    if x == y && !types::is_unionall(y) {
        return true;
    }
    if x == types::builtin(id::BOTTOM) || y == types::builtin(id::ANY) {
        return true;
    }
    if types::is_union(x) && obviously_egal(x, y) {
        return true;
    }
    if x == types::builtin(id::ANY) && types::is_datatype(y) {
        return false;
    }
    sub(x, y, e, param)
}

/// `push_forall_bound_scope` (`subtype.c:957–983`): when a ∀-variable's
/// declared bound is expanded in `var_lt`/`var_gt`, occurrences contributed
/// by the bound (which can only mention forall-side vars) must not combine
/// with occurrences in the enclosing tuple body. Forall-side counters are
/// reset before the recursive call and folded into `cov_diag` afterward;
/// exists-side vars keep accumulating in the current scope.
fn push_forall_bound_scope(e: &mut Env) -> Vec<i8> {
    let saved: Vec<i8> = e.vars.iter().map(|v| v.occurs_cov).collect();
    for v in e.vars.iter_mut() {
        if !v.existential {
            v.occurs_cov = 0;
        }
    }
    saved
}

fn pop_forall_bound_scope(e: &mut Env, saved: &[i8]) {
    for (i, v) in e.vars.iter_mut().enumerate() {
        if i >= saved.len() {
            break;
        }
        if !v.existential {
            v.cov_diag = v.cov_diag.max(v.occurs_cov);
            v.occurs_cov = saved[i];
        }
    }
}

/// `var_lt`: constrain the variable at `idx` by `<: a`, narrowing its upper
/// bound when it is existential.
fn var_lt(a: Offset, e: &mut Env, idx: usize, param: Param) -> bool {
    record_occurrence(e, idx, param);
    // Under a nonzero length offset only a typevar can absorb the relation
    // (`subtype.c:1032–1035`); boxed longs were folded by `subtype_var`.
    debug_assert!(!types::is_boxed_long(a) || e.loffset == 0);
    if e.loffset != 0
        && !types::is_typevar(a)
        && a != types::builtin(id::BOTTOM)
        && a != types::builtin(id::ANY)
    {
        return false;
    }
    let bb = e.vars[idx];
    if !bb.existential {
        // ∀b . b <: a   ⟺   ub <: a (the variable's widest value). The
        // expanded bound's occurrences live in their own forall scope
        // (`subtype.c:1040–1043`).
        let saved = push_forall_bound_scope(e);
        let ans = subtype_left_var(bb.ub, a, e, param);
        pop_forall_bound_scope(e, &saved);
        return ans;
    }
    if bb.ub == a {
        return true;
    }
    if !ccheck(bb.lb, a, e) {
        return false; // lower bound must already satisfy the constraint
    }
    // `simple_meet` in exact mode: when neither `ub` nor `a` subsumes the
    // other, the bound becomes an `Intersect{ub, a}` node rather than
    // over-approximating to one side, which would let `b` escape its
    // declared range (`subtype.c:1059–1066`, #61917).
    let m = simple_meet(e.vars[idx].ub, a, 1);
    e.set_ub(idx, m);
    true
}

/// `var_gt`: constrain the variable at `idx` by `>: a`, raising its lower bound
/// when it is existential.
fn var_gt(a: Offset, e: &mut Env, idx: usize, param: Param) -> bool {
    record_occurrence(e, idx, param);
    // Symmetric offset guard (`subtype.c:1083–1086`).
    debug_assert!(!types::is_boxed_long(a) || e.loffset == 0);
    if e.loffset != 0
        && !types::is_typevar(a)
        && a != types::builtin(id::BOTTOM)
        && a != types::builtin(id::ANY)
    {
        return false;
    }
    let bb = e.vars[idx];
    if !bb.existential {
        // ∀b . a <: b   ⟺   a <: lb (the variable's narrowest value), with
        // the expanded bound's occurrences scoped (`subtype.c:1090–1093`).
        let saved = push_forall_bound_scope(e);
        let ans = subtype_left_var(a, bb.lb, e, param);
        pop_forall_bound_scope(e, &saved);
        return ans;
    }
    if bb.lb == a {
        return true;
    }
    if !ccheck(a, bb.ub, e) {
        return false; // upper bound must already admit the constraint
    }
    let j = simple_join(e.vars[idx].lb, a);
    e.set_lb(idx, j);
    true
}

/// Run a variable's bound-consistency check in its own diagonal-rule scope
/// (`subtype_ccheck`, `subtype.c:846–873`, with `push`/`pop_consistency_scope`):
/// covariant occurrences recorded inside fold into each variable's `cov_diag`
/// (via max) rather than accumulating in the outer `occurs_cov` — otherwise
/// checking one variable's bound would falsely make another diagonal. The
/// caller's `Lunions` is shielded (`:862, :871`) so the check's own left
/// enumeration cannot corrupt the live traversal's bits, and the check enters
/// at `Param::None` — a top-level occurrence inside a bound check is not a
/// covariant occurrence. (The `Loffset` boxed-long arm waits for slice 3;
/// the pin's `limit_slow = 1` explosion guard for slice 2.)
fn ccheck(a: Offset, b: Offset, e: &mut Env) -> bool {
    // As in `subtype_ccheck` (`subtype.c:848–849`), the boxed-long arm comes
    // first: identical boxed lengths are *not* equal under a nonzero offset.
    if types::is_boxed_long(a) && types::is_boxed_long(b) {
        return crate::value::unbox_int(crate::object::Value(a))
            == crate::value::unbox_int(crate::object::Value(b)) + e.loffset as i64;
    }
    if a == b {
        return true;
    }
    if a == types::builtin(id::BOTTOM) || b == types::builtin(id::ANY) {
        return true;
    }
    if a == types::builtin(id::ANY) && types::is_datatype(b) {
        return false;
    }
    if obviously_in_union(b, a) {
        return true;
    }
    let old_l = push_unionstate(&e.lunions);
    let saved: Vec<i8> = e.vars.iter().map(|v| v.occurs_cov).collect();
    for v in e.vars.iter_mut() {
        v.occurs_cov = 0;
    }
    let ok = local_forall_exists_subtype(a, b, e, Param::None, 1);
    // Bindings pushed inside are balanced out by now; fold the survivors.
    for (i, v) in e.vars.iter_mut().enumerate() {
        if i < saved.len() {
            v.cov_diag = v.cov_diag.max(v.occurs_cov);
            v.occurs_cov = saved[i];
        }
    }
    pop_unionstate(&mut e.lunions, &old_l);
    ok
}

/// Both sides are type variables. Constrain the *inner-most* existential one
/// (the later binding in scope, `var_outside`); for an existential interacting
/// with a universal, the `depth0` ordering chooses `∀A ∃B` (encode `B >: A`)
/// versus `∃B ∀A` (encode `B >: A.ub`). If both are universal, fall back to the
/// bounds-only test `xub <: y || x <: ylb`.
fn subtype_two_vars(x: Offset, y: Offset, e: &mut Env, param: Param) -> bool {
    if x == y {
        return true;
    }
    let xi = e.lookup(x);
    let yi = e.lookup(y);
    // Two distinct free variables are never subtypes, regardless of their
    // declared bounds (`xfree_singleton && yfree_singleton` in subtype.c).
    if xi.is_none() && yi.is_none() {
        return false;
    }
    let xr = xi.map_or(false, |i| e.vars[i].existential);
    let yr = yi.map_or(false, |i| e.vars[i].existential);
    if xr {
        let xidx = xi.unwrap();
        if yr {
            let yidx = yi.unwrap();
            // Both existential: constrain whichever is inner-most. `var_outside`
            // — x is outside y iff it was pushed earlier (smaller index).
            if xidx < yidx {
                record_occurrence(e, xidx, param);
                return var_gt(x, e, yidx, param);
            }
        }
        if let Some(j) = yi {
            record_occurrence(e, j, param);
        }
        return var_lt(y, e, xidx, param);
    }
    if yr {
        let yidx = yi.unwrap();
        if let Some(xidx) = xi {
            record_occurrence(e, xidx, param);
            // `∃B ∀A` (B introduced at a shallower invariant depth than the
            // universal A) needs a single B for all A: encode `B >: A.ub`.
            // Otherwise `∀A ∃B`: encode `B >: A`.
            if e.vars[yidx].depth0 < e.vars[xidx].depth0 {
                let xub = e.vars[xidx].ub;
                return var_gt(xub, e, yidx, param);
            }
        }
        return var_gt(x, e, yidx, param);
    }
    let xub = xi.map_or_else(|| types::tvar_ub(x), |i| e.vars[i].ub);
    let ylb = yi.map_or_else(|| types::tvar_lb(y), |i| e.vars[i].lb);
    sub(xub, y, e, param) || sub(x, ylb, e, param)
}

/// Record where the variable at `idx` occurred (`record_var_occurrence`,
/// `subtype.c:894–904`). Counters saturate at 2. An invariant occurrence
/// counts toward `occurs_inv` only when it sits *below* the variable's
/// introduction depth; an invariant occurrence at `depth0` (and every
/// covariant one) counts toward `occurs_cov`. (`max_offset` waits for
/// slice 3 with the vararg length algebra.)
fn record_occurrence(e: &mut Env, idx: usize, param: Param) {
    if param == Param::None {
        return;
    }
    let vb = &mut e.vars[idx];
    if param == Param::Invariant && e.invdepth > vb.depth0 {
        if vb.occurs_inv < 2 {
            vb.occurs_inv += 1;
        }
    } else if vb.occurs_cov < 2 {
        vb.occurs_cov += 1;
    }
    // Any counted occurrence poisons `max_offset` (`subtype.c:905–908`);
    // `subtype_tuple_varargs` snapshots and recovers it around the length
    // equation when the occurrence really was a vararg-`N` slot.
    vb.max_offset = -1;
}

/// Whether `var`'s declared lower bound is a leaf (concrete) type
/// (`is_leaf_typevar`): only then can the diagonal rule pin it to concrete
/// values.
fn is_leaf_typevar(var: Offset) -> bool {
    is_leaf_bound(types::tvar_lb(var))
}

/// Whether `v` is a concrete leaf (`is_leaf_bound`, `subtype.c:1138–1153`):
/// `Union{}`, a non-abstract `DataType` all of whose parameters are leaves,
/// or a non-type **value** (a boxed vararg length is a leaf — the C's
/// `!jl_is_type(v) && !jl_is_typevar(v)` tail). Unions, type variables, and
/// `UnionAll`s are not leaves.
fn is_leaf_bound(v: Offset) -> bool {
    if v == types::builtin(id::BOTTOM) {
        return true;
    }
    if types::is_intersect(v) {
        return false; // an internal meet node is never a concrete leaf (`:1142`)
    }
    if types::is_datatype(v) {
        if types::is_abstract(v) {
            return false;
        }
        let p = types::parameters_of(v);
        if p == NULL {
            return true; // a concrete primitive/leaf with no parameters
        }
        return (0..types::svec_len(p)).all(|i| is_leaf_bound(types::svec_ref(p, i)));
    }
    !types::is_union(v) && !types::is_unionall(v) && !types::is_typevar(v)
}

/// Static "occurs in invariant position" check (`var_occurs_invariant`): does
/// `var` appear under a non-tuple (invariant) constructor within `v`? Tuple
/// parameters are covariant, so they keep the current `inside` flag; any other
/// parametric constructor sets it.
fn var_occurs_invariant(v: Offset, var: Offset, inside: bool) -> bool {
    if v == var {
        return inside;
    }
    if types::is_union(v) {
        return var_occurs_invariant(types::union_a(v), var, inside)
            || var_occurs_invariant(types::union_b(v), var, inside);
    }
    if types::is_vararg(v) {
        // A `Vararg`'s element is covariant (a tuple tail): keep `inside`.
        return var_occurs_invariant(types::vararg_elem(v), var, inside);
    }
    if types::is_unionall(v) {
        let uv = types::unionall_var(v);
        if uv == var {
            return false; // shadowed by an inner binding of the same variable
        }
        if var_occurs_invariant(types::tvar_lb(uv), var, inside)
            || var_occurs_invariant(types::tvar_ub(uv), var, inside)
        {
            return true;
        }
        return var_occurs_invariant(types::unionall_body(v), var, inside);
    }
    if types::is_datatype(v) {
        let p = types::parameters_of(v);
        if p == NULL {
            return false;
        }
        let inside_params = inside || !types::is_tuple(v);
        return (0..types::svec_len(p))
            .any(|i| var_occurs_invariant(types::svec_ref(p, i), var, inside_params));
    }
    false
}

/// Unbox a boxed `Int64` type parameter (a vararg length).
fn unbox_long(v: Offset) -> i64 {
    crate::value::unbox_int(Value(v))
}

/// The C's `jl_vararg_kind_t`: how a tuple's last parameter binds its length.
#[derive(Clone, Copy, PartialEq)]
enum VarargKind {
    /// Not a vararg.
    None,
    /// `Vararg{T}` — unbounded.
    Unbound,
    /// `Vararg{T,3}` — ground integer count.
    Int,
    /// `Vararg{T,N}` — typevar count; the length algebra owns `N`.
    Bound,
}

fn vararg_kind(t: Offset) -> VarargKind {
    if !types::is_vararg(t) {
        return VarargKind::None;
    }
    let n = types::vararg_num(t);
    if n == NULL {
        VarargKind::Unbound
    } else if types::is_boxed_long(n) {
        VarargKind::Int
    } else {
        VarargKind::Bound
    }
}

/// `check_vararg_length` (`subtype.c:1568–1583`): when a fixed-length tail
/// meets `Vararg{T,N}`, discharge the equation `n == N` — the boxed length is
/// equated with `N` invariantly (both directions, as the C does).
fn check_vararg_length(v: Offset, n: i64, e: &mut Env) -> bool {
    let num = types::vararg_num(v);
    if num == NULL {
        return true; // only check when N is present in the last parameter
    }
    let boxed = crate::value::box_int(n);
    let _r = Rooted::new(boxed);
    e.invdepth += 1;
    let ans = sub(boxed.0, num, e, Param::Invariant) && sub(num, boxed.0, e, Param::None);
    e.invdepth -= 1;
    ans
}

/// Subtyping between two ground types: nominal walk to a common type
/// constructor, then covariant tuple elements or invariant parameters.
fn datatype_subtype(x: Offset, y: Offset, e: &mut Env, param: Param) -> bool {
    if x == y || y == types::builtin(id::ANY) {
        return true;
    }
    // Walk x's supertype chain until its name matches y's (same constructor).
    let yname = types::name_of(y);
    let mut xd = x;
    while types::name_of(xd) != yname {
        let s = types::supertype(xd);
        if s == xd {
            return false; // reached `Any` without a match
        }
        xd = s;
    }
    if types::is_tuple(xd) {
        // Tuples keep the caller's param (`subtype.c:1896–1897`: only
        // `PARAM_NONE` promotes to covariant), so occurrence recording under
        // invariant tuple equality stays faithful.
        let p = if param == Param::None { Param::Covariant } else { param };
        return tuple_subtype(xd, y, e, p);
    }
    // Same constructor: parameters are invariant, one level deeper.
    let px = types::parameters_of(xd);
    let py = types::parameters_of(y);
    if px == NULL || py == NULL {
        return true; // non-parametric type, names already matched
    }
    let n = types::svec_len(px);
    e.invdepth += 1;
    let mut ans = true;
    for i in 0..n {
        let xi = types::svec_ref(px, i);
        let yi = types::svec_ref(py, i);
        if xi != yi && !forall_exists_equal(xi, yi, e) {
            ans = false;
            break;
        }
    }
    e.invdepth -= 1;
    ans
}

/// Covariant tuple subtyping (`subtype_tuple`, `subtype.c:1839–1900`): the
/// full length classification over all four vararg kinds — `NONE` (fixed),
/// `UNBOUND` (`Vararg{T}`), `INT` (ground count, which survives construction
/// only over an element with free typevars), and `BOUND` (typevar count,
/// consulting the binding's pinned lower bound) — then the elementwise tail.
fn tuple_subtype(x: Offset, y: Offset, e: &mut Env, param: Param) -> bool {
    let px = types::parameters_of(x);
    let py = types::parameters_of(y);
    let lx = if px == NULL { 0 } else { types::svec_len(px) };
    let ly = if py == NULL { 0 } else { types::svec_len(py) };
    if lx == 0 && ly == 0 {
        return true;
    }
    let mut vvx = VarargKind::None;
    let mut vvy = VarargKind::None;
    let mut xva = NULL;
    let mut xbb: Option<usize> = None;
    if lx > 0 {
        xva = types::svec_ref(px, lx - 1);
        vvx = vararg_kind(xva);
        if vvx == VarargKind::Bound {
            xbb = e.lookup(types::vararg_num(xva));
        }
    }
    let yva = if ly > 0 { types::svec_ref(py, ly - 1) } else { NULL };
    if ly > 0 {
        vvy = vararg_kind(yva);
    }
    let xbb_long_lb = xbb.map_or(false, |i| types::is_boxed_long(e.vars[i].lb));
    if vvx != VarargKind::None && vvx != VarargKind::Int && !xbb_long_lb {
        // Left length is genuinely open (unbounded, or a bound var not yet
        // pinned to an integer).
        if vvx == VarargKind::Unbound || xbb.map_or(false, |i| !e.vars[i].existential) {
            // Unbounded on the LHS (includes N == 0), bounded on the RHS.
            if vvy == VarargKind::None || vvy == VarargKind::Int {
                return false;
            } else if lx < ly {
                return false;
            }
        } else if vvy == VarargKind::None
            && !check_vararg_length(xva, ly as i64 + 1 - lx as i64, e)
        {
            return false;
        }
    } else {
        // Left length is known: count it out and compare.
        let mut nx = lx as i64;
        if vvx == VarargKind::Int {
            nx += unbox_long(types::vararg_num(xva)) - 1;
        } else if let Some(i) = xbb {
            debug_assert!(types::is_boxed_long(e.vars[i].lb));
            nx += unbox_long(e.vars[i].lb) - 1;
        } else {
            debug_assert!(vvx == VarargKind::None);
        }
        let mut ny = ly as i64;
        if vvy == VarargKind::Int {
            ny += unbox_long(types::vararg_num(yva)) - 1;
        } else if vvy != VarargKind::None {
            ny -= 1;
        }
        if vvy == VarargKind::None || vvy == VarargKind::Int {
            if nx != ny {
                return false;
            }
        } else if ny > nx {
            return false;
        }
    }
    subtype_tuple_tail(px, py, lx, ly, e, param)
}

/// The elementwise tail walk (`subtype_tuple_tail`, `subtype.c:1740–1837`).
/// `vx`/`vy` count how far into a trailing `Vararg` each side has advanced;
/// once both are inside one, [`subtype_tuple_varargs`] finishes the
/// comparison (with the counts, which its length equation consumes). A
/// fixed-length walk ending against a still-pending `Vararg{T,N}` discharges
/// `(lx+1-ly) == N` through [`check_vararg_length`] (`:1828–1832`).
fn subtype_tuple_tail(px: Offset, py: Offset, lx: u32, ly: u32, e: &mut Env, param: Param) -> bool {
    let (mut i, mut j) = (0u32, 0u32);
    let (mut vx, mut vy) = (0u32, 0u32);
    let mut xi = NULL;
    let mut yi = NULL;
    loop {
        if i < lx {
            xi = types::svec_ref(px, i);
            if i == lx - 1 && (vx > 0 || types::is_vararg(xi)) {
                vx += 1;
            }
        }
        if j < ly {
            yi = types::svec_ref(py, j);
            if j == ly - 1 && (vy > 0 || types::is_vararg(yi)) {
                vy += 1;
            }
        }
        if i >= lx {
            break;
        }

        let mut all_varargs = vx > 0 && vy > 0;
        if !all_varargs && vy == 1 && types::vararg_elem(yi) == types::builtin(id::ANY) {
            // `Tuple{...} <: Tuple{..., Vararg{Any}}`: the remaining left
            // elements are all `<: Any`, so match the tails directly
            // (`subtype.c:1767–1781`) — counting the skipped elements into
            // `vy`, which the length equation needs.
            let xlast = types::svec_ref(px, lx - 1);
            if types::is_vararg(xlast) {
                all_varargs = true;
                vy += lx - i - 1;
                vx = 1;
                xi = xlast;
            } else {
                break;
            }
        }
        if all_varargs {
            return subtype_tuple_varargs(xi, yi, vx as i64, vy as i64, e, param);
        }
        if j >= ly {
            return vx > 0;
        }
        let xii = if vx > 0 { types::vararg_elem(xi) } else { xi };
        let yii = if vy > 0 { types::vararg_elem(yi) } else { yi };
        if !sub(xii, yii, e, param) {
            return false;
        }
        if i < lx - 1 || vx == 0 {
            i += 1;
        }
        if j < ly - 1 || vy == 0 {
            j += 1;
        }
    }
    if vy > 0 && vx == 0 && lx as i64 + 1 >= ly as i64 {
        // Tuple{...,tn} <: Tuple{...,Vararg{T,N}}: check (lx+1-ly) == N.
        if !check_vararg_length(yi, lx as i64 + 1 - ly as i64, e) {
            return false;
        }
    }
    true
}

/// `Tuple{..., Vararg{S,N}} <: Tuple{..., Vararg{T,M}}`
/// (`subtype_tuple_varargs`, `subtype.c:1587–1738`): the element comparison
/// (checked twice so a diagonal variable in `S` is constrained as it must be
/// across ≥2 arguments), then the **length equation** `N − vx == M − vy` —
/// ground long against ground long directly; a long against a variable by
/// folding the count difference into the constant; variable against variable
/// through [`forall_exists_equal`] under the `Loffset` channel. Bound `N`
/// variables are marked `intvalued`, and their `max_offset` bookkeeping is
/// snapshotted around the equation as the pin does. The repeated-element and
/// separable fast paths are omitted as pure optimizations (recorded).
fn subtype_tuple_varargs(
    vtx: Offset,
    vty: Offset,
    mut vx: i64,
    mut vy: i64,
    e: &mut Env,
    param: Param,
) -> bool {
    let xp0 = types::vararg_elem(vtx);
    let mut xp1 = types::vararg_num(vtx);
    let yp0 = types::vararg_elem(vty);
    let mut yp1 = types::vararg_num(vty);

    let xlv = if xp1 != NULL && types::is_typevar(xp1) { e.lookup(xp1) } else { None };
    let ylv = if yp1 != NULL && types::is_typevar(yp1) { e.lookup(yp1) } else { None };
    let max_offsetx = xlv.map_or(0, |i| e.vars[i].max_offset);
    let max_offsety = ylv.map_or(0, |i| e.vars[i].max_offset);

    let xl = xlv.map_or(xp1, |i| e.vars[i].lb);
    let yl = ylv.map_or(yp1, |i| e.vars[i].lb);

    let mut skip_elements = false;
    if xp1 == NULL {
        // Unconstrained length on the left, constrained on the right.
        if yl != NULL && types::is_boxed_long(yl) {
            return false;
        }
    } else if types::is_boxed_long(xl) && unbox_long(xl) + 1 == vx {
        // The LHS is exhausted: the RHS must be exhausted too, or unbounded
        // (in which case its length still gets constrained to 0 below).
        if yl != NULL {
            if types::is_boxed_long(yl) {
                return unbox_long(yl) + 1 == vy;
            }
        } else {
            skip_elements = true; // the C's `goto constrain_length`
        }
    }
    if !skip_elements {
        if !sub(xp0, yp0, e, param) {
            return false;
        }
        if !sub(xp0, yp0, e, Param::Covariant) {
            return false;
        }
    }

    // constrain_length:
    if yp1 == NULL {
        return true;
    }
    if xp1 == NULL {
        // x's length is unconstrained; y's must not have become fixed, and an
        // untouched bound variable becomes the "N::Int, unconstrained" token:
        // lb = Any with `intvalued` set (`subtype.c:1663–1691`).
        let mut yl = yp1;
        let mut ylv2: Option<usize> = None;
        if types::is_typevar(yl) {
            ylv2 = e.lookup(yl);
            if let Some(i) = ylv2 {
                yl = e.vars[i].lb;
            }
        }
        if types::is_boxed_long(yl) {
            return false;
        }
        if let Some(i) = ylv2 {
            if e.vars[i].depth0 != e.invdepth
                || e.vars[i].lb != types::builtin(id::BOTTOM)
                || e.vars[i].ub != types::builtin(id::ANY)
            {
                return false;
            }
            e.vars[i].intvalued = true;
        }
        e.invdepth += 1;
        let ans = sub(types::builtin(id::ANY), yp1, e, Param::Invariant);
        if let Some(i) = ylv2 {
            e.vars[i].max_offset = max_offsety;
        }
        e.invdepth -= 1;
        return ans;
    }

    // Vararg{T,N} <: Vararg{T2,N2}: equate N and N2.
    e.invdepth += 1;
    let bxp1 = if types::is_typevar(xp1) { e.lookup(xp1) } else { None };
    let byp1 = if types::is_typevar(yp1) { e.lookup(yp1) } else { None };
    if let Some(i) = bxp1 {
        e.vars[i].intvalued = true;
        if types::is_boxed_long(e.vars[i].lb) {
            xp1 = e.vars[i].lb;
        }
    }
    if let Some(i) = byp1 {
        e.vars[i].intvalued = true;
        if types::is_boxed_long(e.vars[i].lb) {
            yp1 = e.vars[i].lb;
        }
    }
    let ans;
    if types::is_boxed_long(xp1) && types::is_boxed_long(yp1) {
        ans = unbox_long(xp1) - vx == unbox_long(yp1) - vy;
    } else {
        // At most one side is a ground long (a long on both sides took the
        // direct comparison above); fold the count difference into it so the
        // offset channel carries only the variable-vs-variable residue.
        let mut _boxroot: Option<Rooted> = None;
        if types::is_boxed_long(xp1) && vx != vy {
            let b = crate::value::box_int(unbox_long(xp1) + vy - vx);
            _boxroot = Some(Rooted::new(b));
            xp1 = b.0;
            vx = vy;
        }
        if types::is_boxed_long(yp1) && vy != vx {
            let b = crate::value::box_int(unbox_long(yp1) + vx - vy);
            _boxroot = Some(Rooted::new(b));
            yp1 = b.0;
            vy = vx;
        }
        debug_assert_eq!(e.loffset, 0);
        e.loffset = (vx - vy) as i32;
        ans = forall_exists_equal(xp1, yp1, e);
        e.loffset = 0;
    }
    if let Some(i) = ylv {
        e.vars[i].max_offset = max_offsety;
    }
    if let Some(i) = xlv {
        e.vars[i].max_offset = max_offsetx;
    }
    e.invdepth -= 1;
    ans
}

/// Invariant equality of two type parameters (`forall_exists_equal`,
/// `subtype.c:2311–2357`): subtype in both directions, each through
/// [`local_forall_exists_subtype`]. The forward direction runs at
/// `Invariant` with `limit_slow = -1`; the reverse runs at `Param::None`
/// unlimited, as in the C — the occurrences were already recorded going
/// forward. The caller's `Lunions` is shielded around both directions
/// (`:2347, 2355`); `Runions` is shared — that sharing is what makes the
/// machine global (a right decision made deep inside an invariant check is
/// revisitable by the outer ∃ loop).
fn forall_exists_equal(x: Offset, y: Offset, e: &mut Env) -> bool {
    if obviously_egal(x, y) {
        // Structurally identical sides are equal only at offset zero
        // (`subtype.c:2313`): `N == N + k` fails for `k ≠ 0`.
        return e.loffset == 0;
    }

    // A tuple of definite length can never invariant-equal one of indefinite
    // length (`:2315–2317`).
    if (is_indefinite_length_tuple(x, e) && is_definite_length_tuple(y, e))
        || (is_definite_length_tuple(x, e) && is_indefinite_length_tuple(y, e))
    {
        return false;
    }

    // Same-name nested constructor fast path: distinct constructors can never
    // be invariant-equal, and for a same-name non-tuple constructor the
    // parameter comparison forwards to `forall_exists_equal` pairwise, which
    // is symmetric — a single subtype call suffices.
    if types::is_datatype(x) && types::is_datatype(y) {
        if types::name_of(x) != types::name_of(y) {
            return false;
        }
        if !types::is_tuple(x) {
            return sub(x, y, e, Param::Invariant);
        }
    }

    // The two-union greedy path (`:2331–2339`): first try comparing the
    // unions componentwise — itself a recorded right decision, so on failure
    // the machine memorizes that this branch is to be skipped and the retry
    // takes the general path. Sound only because the machine owns the bit.
    if types::is_union(x) && types::is_union(y) && !e.pick_union_decision(true) {
        return forall_exists_equal(types::union_a(x), types::union_a(y), e)
            && forall_exists_equal(types::union_b(x), types::union_b(y), e);
    }

    // `TypeVar == Type` fast path (`:2341–2345`), gated on a zero offset —
    // `equal_var` pins bounds directly and cannot carry the length algebra —
    // and on `x` being a *type* (`jl_is_type(x)`: a boxed length constrains
    // through the variable machinery, not the merged path).
    if e.loffset == 0
        && types::is_typevar(y)
        && !types::is_typevar(x)
        && !types::is_boxed_long(x)
    {
        return equal_var(y, x, e);
    }

    let old_l = push_unionstate(&e.lunions);
    let mut ans = local_forall_exists_subtype(x, y, e, Param::Invariant, -1);
    if ans {
        // The reverse direction sees the negated offset (`flip_offset`,
        // `subtype.c:2351–2353`): `X = Y + k  ⟺  Y = X − k`.
        e.loffset = -e.loffset;
        ans = local_forall_exists_subtype(y, x, e, Param::None, 0);
        e.loffset = -e.loffset;
    }
    pop_unionstate(&mut e.lunions, &old_l);
    ans
}

/// `equal_var` (`subtype.c:2270–2309`, minus the intersection/innervar arms):
/// `TypeVar == Type` as a merged `var_gt`+`var_lt` that skips the redundant
/// checks — after `ccheck(x, ub)` proves `x <: ub`, the upper bound is set to
/// `x` directly ("skip `simple_meet` here as we have proven `x <: vb->ub`").
fn equal_var(v: Offset, x: Offset, e: &mut Env) -> bool {
    match e.lookup(v) {
        None => x == v, // a free variable equals only itself
        Some(idx) => {
            record_occurrence(e, idx, Param::Invariant);
            if !e.vars[idx].existential {
                // ∀v: both directions against the fixed declared bounds.
                let lb = e.vars[idx].lb;
                let ub = e.vars[idx].ub;
                return local_forall_exists_subtype(
                    x,
                    lb,
                    e,
                    Param::Invariant,
                    (!has_free_typevars(x)) as i32,
                ) && local_forall_exists_subtype(ub, x, e, Param::None, 0);
            }
            if e.vars[idx].lb == x {
                return var_lt(x, e, idx, Param::None);
            }
            if !ccheck(x, e.vars[idx].ub, e) {
                return false;
            }
            let j = simple_join(e.vars[idx].lb, x);
            e.set_lb(idx, j);
            if e.vars[idx].ub == x {
                return true;
            }
            if !ccheck(e.vars[idx].lb, x, e) {
                return false;
            }
            e.set_ub(idx, x);
            true
        }
    }
}

/// `is_indefinite_length_tuple_type` (`subtype.c:2156–2163`): a tuple type
/// whose last parameter is an **unbounded** `Vararg` (a `BOUND` typevar-`N`
/// vararg is neither definite nor indefinite).
fn is_indefinite_length_tuple(x: Offset, _e: &Env) -> bool {
    let x = unwrap_unionall(x);
    if !types::is_datatype(x) || !types::is_tuple(x) {
        return false;
    }
    let p = types::parameters_of(x);
    let n = if p == NULL { 0 } else { types::svec_len(p) };
    n > 0 && vararg_kind(types::svec_ref(p, n - 1)) == VarargKind::Unbound
}

/// `is_definite_length_tuple_type` (`subtype.c:2166–2177`): a tuple type of
/// fixed arity — no trailing `Vararg` (`NONE`) or a ground-count one (`INT`).
/// A typevar is judged by its declared upper bound.
fn is_definite_length_tuple(x: Offset, _e: &Env) -> bool {
    let x = if types::is_typevar(x) { types::tvar_ub(x) } else { x };
    let x = unwrap_unionall(x);
    if !types::is_datatype(x) || !types::is_tuple(x) {
        return false;
    }
    let p = types::parameters_of(x);
    let n = if p == NULL { 0 } else { types::svec_len(p) };
    if n == 0 {
        return true;
    }
    let k = vararg_kind(types::svec_ref(p, n - 1));
    k == VarargKind::None || k == VarargKind::Int
}

/// `jl_unwrap_unionall`: strip `where` wrappers to the underlying body.
fn unwrap_unionall(mut t: Offset) -> Offset {
    while types::is_unionall(t) {
        t = types::unionall_body(t);
    }
    t
}

/// `env_unchanged` (`subtype.c:811–840`): did the search leave every
/// existential binding's bounds untouched relative to the snapshot, without
/// turning a previously non-diagonal leaf variable diagonal? Gates hiding
/// newly-discovered right decisions on success (pure pruning: equivalent
/// alternatives need not be revisited).
fn env_unchanged(e: &Env, se: &SavedVars) -> bool {
    debug_assert_eq!(e.vars.len(), se.vars.len());
    for (v, s) in e.vars.iter().zip(se.vars.iter()) {
        if !v.existential {
            continue;
        }
        if v.lb != s.lb || v.ub != s.ub {
            return false;
        }
        let saved_max = s.occurs_cov.max(s.cov_diag);
        if is_leaf_typevar(v.var)
            && !v.body_occurs_inv
            && v.occurs_cov.max(v.cov_diag) > 1
            && saved_max <= 1
        {
            return false; // a variable became diagonal from non-diagonal
        }
    }
    true
}

/// A subtype query nested inside a larger one (`local_forall_exists_subtype`,
/// `subtype.c:2189–2268`), continuing the caller's `Runions` stack with its
/// own `Lunions` enumeration. The regimes:
///
/// 1. `obviously_in_union` fast path (#49857).
/// 2. Both sides ground → a completely fresh machine (nothing here can
///    constrain the live query).
/// 3. Neither side mentions an in-scope existential → a full nested
///    [`forall_exists_subtype`] with both union states zeroed and `Runions`
///    restored after ("saves some bits in union stack") — safe for the same
///    reason.
/// 4. Exactly one side is an existential typevar → loop over `Lunions` only,
///    no env save/restore between passes: with no cross-side ∃ choice to
///    backtrack, the bound updates *are* the accumulation (`:2213–2223`).
/// 5. The general slow path (`:2224–2267`), with the pin's two heuristics
///    (slice 2): **freeze** — after a successful ∀ step that discovered no
///    new right decisions (or when `limited`), commit the env and the
///    `Lunions` prefix so later right-flips resume from it instead of
///    restarting at pass 0; **`limit_slow`** — saturate at 4 ∀ passes, then
///    freeze eagerly and hide the leftover right decisions from the caller.
///    Lossy by design (can only flip answers `true`→`false`, never unsound
///    `true`): the pin's explosion guard, `limit_slow == -1` resolving to
///    "either side is ground".
fn local_forall_exists_subtype(
    x: Offset,
    y: Offset,
    e: &mut Env,
    param: Param,
    limit_slow: i32,
) -> bool {
    if obviously_in_union(y, x) {
        return true;
    }
    let kindx = !has_free_typevars(x);
    let kindy = !has_free_typevars(y);
    if kindx && kindy {
        return types::issubtype(x, y); // fresh machine (`jl_subtype`, `:2196–2199`)
    }
    let has_exists = (!kindx && has_existential_typevar(x, e))
        || (!kindy && has_existential_typevar(y, e));
    if !has_exists {
        let old_r = push_unionstate(&e.runions);
        e.lunions.used = 0;
        e.lunions.depth = 0;
        e.lunions.more = 0;
        e.runions.used = 0;
        e.runions.depth = 0;
        e.runions.more = 0;
        let ans = forall_exists_subtype(x, y, e, param);
        pop_unionstate(&mut e.runions, &old_r);
        return ans;
    }
    if is_existential_typevar(x, e) != is_existential_typevar(y, e) {
        e.lunions.used = 0;
        loop {
            e.lunions.more = 0;
            e.lunions.depth = 0;
            let ans = sub(x, y, e, param);
            if !ans || !e.next_union_state(false) {
                return ans;
            }
        }
    }
    let limit_slow = if limit_slow == -1 {
        (kindx || kindy) as i32
    } else {
        limit_slow
    };
    let old_rmore = e.runions.more;
    let mut se = save_vars(e);
    let mut limited = false;
    let mut ini_count: i32 = 0;
    let mut latest_l: Option<SavedUnionState> = None;
    let mut ans;
    loop {
        let mut count = ini_count;
        if ini_count == 0 {
            e.lunions.used = 0;
        } else {
            // Resume from the frozen ∀ prefix rather than restarting at 0.
            pop_unionstate(&mut e.lunions, latest_l.as_ref().unwrap());
        }
        loop {
            e.lunions.more = 0;
            e.lunions.depth = 0;
            if count < 4 {
                count += 1;
            }
            ans = sub(x, y, e, param);
            if limit_slow != 0 && count == 4 {
                limited = true;
            }
            if !ans || !e.next_union_state(false) {
                break;
            }
            if limited || e.runions.more == old_rmore {
                // Re-save the env and freeze the ∃ decisions made for the
                // previous ∀ arms (`:2245–2251`).
                ini_count = count;
                latest_l = Some(push_unionstate(&e.lunions));
                re_save_vars(e, &mut se);
                e.runions.more = old_rmore;
            }
        }
        if ans || e.runions.more == old_rmore {
            break;
        }
        // A right decision discovered in here remains untried: flip it, roll
        // back to the latest snapshot, and re-enumerate.
        debug_assert!(e.runions.more > old_rmore);
        e.next_union_state(true);
        restore_vars(e, &se); // also restores the R bit cursor (`rdepth`)
        e.runions.more = old_rmore;
    }
    if !ans {
        debug_assert_eq!(e.runions.more, old_rmore);
    } else if e.runions.more > old_rmore && (limited || env_unchanged(e, &se)) {
        // Hide the leftover ∃ decisions if the env is unchanged/limited —
        // revisiting them cannot change the result (`:2262–2265`).
        e.runions.more = old_rmore;
    }
    ans
}

/// `is_existential_typevar` (`subtype.c:2179–2184`).
fn is_existential_typevar(x: Offset, e: &Env) -> bool {
    types::is_typevar(x) && e.lookup(x).map_or(false, |i| e.vars[i].existential)
}

/// Greatest lower bound (`simple_meet`). For ground operands the GLB is the
/// subtype side; when a type variable is involved we over-estimate by `b`
/// (subtype-path bias), since there is no `Intersect` node. Crucially, the
/// ground check uses a *fresh* environment, so it never narrows the existential
/// variables of the live query.
fn simple_meet(a: Offset, b: Offset, overesi: u8) -> Offset {
    let any = types::builtin(id::ANY);
    let bottom = types::builtin(id::BOTTOM);
    if a == any || b == bottom || obviously_egal(a, b) {
        return b;
    }
    if b == any || a == bottom {
        return a;
    }
    if overesi == 1 && (types::is_intersect(a) || types::is_intersect(b)) {
        // One operand is already an internal meet node: represent the
        // combined meet exactly by nesting (`subtype.c:765–768`).
        return types::intersect_type(a, b);
    }
    if !is_type_or_typevar(a) || !is_type_or_typevar(b) {
        return bottom; // distinct non-type values (equal ones were egal above)
    }
    // The C's kind/TypeEq arms (`:771–774`), phrased on our `Type{T}`:
    // `Kind ∩ Type{X}` where `typeof(X)` is that kind is `Type{X}`.
    if types::is_kind(a)
        && types::is_type_type(b)
        && crate::object::type_of(Value(types::svec_ref(types::parameters_of(b), 0))) == a
    {
        return b;
    }
    if types::is_kind(b)
        && types::is_type_type(a)
        && crate::object::type_of(Value(types::svec_ref(types::parameters_of(a), 0))) == b
    {
        return a;
    }
    if types::is_typevar(a) && obviously_egal(b, types::tvar_ub(a)) {
        return a;
    }
    if types::is_typevar(b) && obviously_egal(a, types::tvar_ub(b)) {
        return b;
    }
    simple_intersect(a, b, overesi)
}

/// `jl_is_type(t) || jl_is_typevar(t)` — the operands `simple_meet` can
/// analyze (everything else is a value, whose meet with anything unequal
/// is empty).
fn is_type_or_typevar(t: Offset) -> bool {
    types::is_datatype(t) || types::is_union(t) || types::is_unionall(t) || types::is_typevar(t)
}

/// `simple_intersect` (`jltypes.c:864–979`), faithful-partial. Flatten both
/// unions (UnionAlls deliberately not unwrapped); drop components disjoint
/// from everything on the other side (our disjointness evidence is
/// [`obviously_disjoint`] alone — the C additionally consults full
/// intersection emptiness for ground pairs; weaker evidence only means less
/// simplification, never a wrong answer, because the fallout is an exact
/// `Intersect` node or a legal over-approximation); then decide by
/// componentwise subtyping — full `issubtype` standing in for the C's
/// typevar-aware `simple_subtype2`, the same recorded substitution as the
/// union-normalization dedup (audit finding 7) — whether one side subsumes
/// the other.
fn simple_intersect(a: Offset, b: Offset, overesi: u8) -> Offset {
    let bottom = types::builtin(id::BOTTOM);
    let mut comps: Vec<Offset> = Vec::new();
    flatten_union(a, &mut comps);
    let nta = comps.len();
    flatten_union(b, &mut comps);
    let nt = comps.len();

    // 1. A component disjoint from every component of the other side is dead.
    let mut alive = vec![false; nt];
    for i in 0..nta {
        for j in nta..nt {
            if (!alive[i] || !alive[j]) && !obviously_disjoint(comps[i], comps[j]) {
                alive[i] = true;
                alive[j] = true;
            }
        }
    }
    // 2. Componentwise subtyping: stemp[k] = -1 (strictly above some
    // other-side component), 1 (equal to one), 2 (strictly below one).
    let mut stemp = vec![0i8; nt];
    let mut all_disjoint = true;
    for i in 0..nta {
        if !alive[i] {
            continue;
        }
        all_disjoint = false;
        for j in nta..nt {
            if !alive[j] {
                continue;
            }
            let subab = types::issubtype(comps[i], comps[j]);
            let subba = types::issubtype(comps[j], comps[i]);
            if subba && !subab {
                stemp[i] = -1;
                if stemp[j] >= 0 {
                    stemp[j] = 2;
                }
            } else if subab && !subba {
                stemp[j] = -1;
                if stemp[i] >= 0 {
                    stemp[i] = 2;
                }
            } else if subab && subba {
                if stemp[i] == 0 {
                    stemp[i] = 1;
                }
                if stemp[j] == 0 {
                    stemp[j] = 1;
                }
            }
        }
    }
    let mut subs = [true, true];
    let mut rs = [true, true];
    if !all_disjoint {
        for k in 0..nt {
            let side = (k >= nta) as usize;
            subs[side] &= !alive[k] || stemp[k] > 0;
            rs[side] &= alive[k] && stemp[k] > 0;
        }
        // Every component of one side sits at-or-below the other: that side
        // *is* the meet.
        if rs[0] {
            return a;
        }
        if rs[1] {
            return b;
        }
    }
    if all_disjoint || (overesi == 0 && !subs[0] && !subs[1]) {
        return bottom;
    }
    if !subs[0] && !subs[1] && overesi == 1 {
        // Neither side subsumes the other and they are not provably
        // disjoint: the meet is not expressible as a single existing type.
        // Keep it exact as `Intersect{a, b}` rather than over-approximating
        // to one side, which would silently drop the other (#61917).
        return types::intersect_type(a, b);
    }
    // One side's surviving components all sit below the other — union them —
    // or over-approximate (mode 2): strictly-below `a` components plus all
    // surviving `b` components (`jl_typeintersect` may over-approximate, so
    // this is sound).
    let keep: Vec<Offset> = if subs[0] {
        (0..nta).filter(|&k| alive[k]).map(|k| comps[k]).collect()
    } else if subs[1] {
        (nta..nt).filter(|&k| alive[k]).map(|k| comps[k]).collect()
    } else {
        (0..nt)
            .filter(|&k| alive[k] && stemp[k] >= if k < nta { 2 } else { 0 })
            .map(|k| comps[k])
            .collect()
    };
    if keep.is_empty() {
        return bottom;
    }
    // The C isorts and right-nests without re-deduping; we rebuild through
    // the normalized union constructor — consistent with our union model
    // (recorded with finding 7's family).
    types::union_of(&keep)
}

/// Flatten a union spine into its non-union components (`flatten_type_union`
/// without the `UnionAll` unwrap, as `simple_intersect` requires).
fn flatten_union(t: Offset, out: &mut Vec<Offset>) {
    if types::is_union(t) {
        flatten_union(types::union_a(t), out);
        flatten_union(types::union_b(t), out);
    } else {
        out.push(t);
    }
}

/// A conservative subset of the C's `obviously_disjoint`: `true` only when
/// the two types provably share no instance. Nominal single inheritance
/// makes incomparable ground non-tuple, non-`Type{}` datatypes disjoint —
/// a common subtype's supertype chain would have to pass through both.
/// Tuples (covariant), `Type{}`s, typevars, `UnionAll`s, and anything with
/// free typevars conservatively report `false`.
fn obviously_disjoint(x: Offset, y: Offset) -> bool {
    if x == y || !types::is_datatype(x) || !types::is_datatype(y) {
        return false;
    }
    if types::is_tuple(x) || types::is_tuple(y) || types::is_type_type(x) || types::is_type_type(y)
    {
        return false;
    }
    if has_free_typevars(x) || has_free_typevars(y) {
        return false;
    }
    !types::issubtype(x, y) && !types::issubtype(y, x)
}

/// Over-approximate an internal `Intersect` spine by a real type
/// (`widen_intersect`, `subtype.c:786–800`), so it cannot escape subtyping
/// into a result type: peel recursively, re-meeting in mode 2.
fn widen_intersect(t: Offset) -> Offset {
    if !types::is_intersect(t) {
        return t;
    }
    let a = widen_intersect(types::intersect_a(t));
    let _ra = Rooted::new(Value(a));
    let b = widen_intersect(types::intersect_b(t));
    let _rb = Rooted::new(Value(b));
    simple_meet(a, b, 2)
}

/// Least upper bound (`simple_join`, `simple_union`). Defers to the normalized
/// [`union_type`](types::union_type), which drops ground members subsumed by
/// another but *keeps* free type variables (its dedup runs in a fresh
/// environment) — so a constraint like `S >: T` is preserved rather than
/// collapsed away.
fn simple_join(a: Offset, b: Offset) -> Offset {
    let any = types::builtin(id::ANY);
    let bottom = types::builtin(id::BOTTOM);
    if a == bottom || b == any || a == b {
        return b;
    }
    if b == bottom || a == any {
        return a;
    }
    types::union_type(a, b)
}

// --- structural fast-path helpers (`subtype.c:501–641, 1329–1344`) ----------

/// Cheap structural equality (`obviously_egal`, `subtype.c:501–538`): never
/// wrongly true, may be false for types that are semantically equal. Uniqued
/// forms usually hit the identity fast path; the recursion matters for
/// non-uniqued spines (unions, UnionAlls) and for uniqued constructors whose
/// parameters embed them. Distinct type variables are never obviously egal.
fn obviously_egal(a: Offset, b: Offset) -> bool {
    if a == b {
        return true;
    }
    if types::is_datatype(a) && types::is_datatype(b) {
        if types::name_of(a) != types::name_of(b) {
            return false;
        }
        let (pa, pb) = (types::parameters_of(a), types::parameters_of(b));
        if pa == NULL || pb == NULL {
            return false; // same non-parametric type would have been `==`
        }
        let n = types::svec_len(pa);
        if n != types::svec_len(pb) {
            return false;
        }
        return (0..n).all(|i| obviously_egal(types::svec_ref(pa, i), types::svec_ref(pb, i)));
    }
    if types::is_union(a) && types::is_union(b) {
        return obviously_egal(types::union_a(a), types::union_a(b))
            && obviously_egal(types::union_b(a), types::union_b(b));
    }
    if types::is_intersect(a) && types::is_intersect(b) {
        // The meet node shares the union pair's structural arm (`subtype.c:520`).
        return obviously_egal(types::intersect_a(a), types::intersect_a(b))
            && obviously_egal(types::intersect_b(a), types::intersect_b(b));
    }
    if types::is_unionall(a) && types::is_unionall(b) {
        return types::unionall_var(a) == types::unionall_var(b)
            && obviously_egal(types::unionall_body(a), types::unionall_body(b));
    }
    if types::is_vararg(a) && types::is_vararg(b) {
        if !obviously_egal(types::vararg_elem(a), types::vararg_elem(b)) {
            return false;
        }
        let (na, nb) = (types::vararg_num(a), types::vararg_num(b));
        if na == NULL && nb == NULL {
            return true;
        }
        return na != NULL
            && nb != NULL
            && crate::builtins::egal(crate::object::Value(na), crate::object::Value(nb));
    }
    // The C's tail (`subtype.c:538`): non-type values compare by egal — two
    // equal boxed vararg lengths are obviously egal.
    if types::is_boxed_long(a) && types::is_boxed_long(b) {
        return unbox_long(a) == unbox_long(b);
    }
    false
}

/// Whether every member of `x` is obviously a member of union `u`
/// (`obviously_in_union`, `subtype.c:621–641`) — the cheap
/// union-membership fast path both union arms and `ccheck` consult.
fn obviously_in_union(u: Offset, x: Offset) -> bool {
    if types::is_union(x) {
        return obviously_in_union(u, types::union_a(x))
            && obviously_in_union(u, types::union_b(x));
    }
    if types::is_union(u) {
        return obviously_in_union(types::union_a(u), x)
            || obviously_in_union(types::union_b(u), x);
    }
    obviously_egal(u, x)
}

/// Structural walk shared by [`has_free_typevars`] and
/// [`has_existential_typevar`]: does `t` contain a *free* occurrence (not
/// bound by a `UnionAll` within `t`) of a variable satisfying `pred`? A
/// `UnionAll`'s variable binds only in its body; its declared bounds are
/// checked with the variable still free, as `jl_has_free_typevars` does.
fn has_free_var_where(
    t: Offset,
    bound: &mut Vec<Offset>,
    pred: &dyn Fn(Offset) -> bool,
) -> bool {
    if types::is_typevar(t) {
        return !bound.contains(&t) && pred(t);
    }
    if types::is_union(t) {
        return has_free_var_where(types::union_a(t), bound, pred)
            || has_free_var_where(types::union_b(t), bound, pred);
    }
    if types::is_vararg(t) {
        if has_free_var_where(types::vararg_elem(t), bound, pred) {
            return true;
        }
        let n = types::vararg_num(t);
        return n != NULL && has_free_var_where(n, bound, pred);
    }
    if types::is_intersect(t) {
        // A meet node can carry typevars in either operand (it appears in
        // existential upper bounds, which `has_existential_typevar` walks).
        return has_free_var_where(types::intersect_a(t), bound, pred)
            || has_free_var_where(types::intersect_b(t), bound, pred);
    }
    if types::is_unionall(t) {
        let v = types::unionall_var(t);
        if has_free_var_where(types::tvar_lb(v), bound, pred)
            || has_free_var_where(types::tvar_ub(v), bound, pred)
        {
            return true;
        }
        bound.push(v);
        let r = has_free_var_where(types::unionall_body(t), bound, pred);
        bound.pop();
        return r;
    }
    if types::is_datatype(t) {
        let p = types::parameters_of(t);
        if p == NULL {
            return false;
        }
        return (0..types::svec_len(p))
            .any(|i| has_free_var_where(types::svec_ref(p, i), bound, pred));
    }
    false
}

/// `jl_has_free_typevars`: does `t` contain any type variable not bound by a
/// `UnionAll` within `t` itself? (`pub(crate)`: `types::tuple_type`'s vararg
/// expansion guard consults it, as `inst_datatype_inner` does the C's.)
pub(crate) fn has_free_typevars(t: Offset) -> bool {
    has_free_var_where(t, &mut Vec::new(), &|_| true)
}

/// `has_existential_typevar` (`subtype.c:1329–1344`): does `t` mention (as a
/// free occurrence) any variable whose binding in `e` is existential?
fn has_existential_typevar(t: Offset, e: &Env) -> bool {
    if !e.vars.iter().any(|b| b.existential) {
        return false;
    }
    has_free_var_where(t, &mut Vec::new(), &|v| {
        e.lookup(v).map_or(false, |i| e.vars[i].existential)
    })
}
