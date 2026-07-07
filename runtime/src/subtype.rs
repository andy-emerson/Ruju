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
//! Also ported: the diagonal rule, unbounded varargs in tuple tails, and the
//! `Type{T}` kind rules (the pin's TypeEq semantics). Deliberately omitted for
//! now (tracked in `design/implementation.md`): type intersection and the
//! newer `Intersect`/`Loffset` machinery. Union backtracking uses a simple
//! save/restore of the environment rather than Julia's union-state bit-stack
//! iterator — equivalent on the cases handled here, but not the full machine.
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
    /// Invariant-constructor nesting depth at which the variable was introduced
    /// (`depth0`). Distinguishes `∀A ∃B` from `∃B ∀A` when an existential and a
    /// universal variable interact.
    depth0: i32,
    /// Absolute shadow-stack index of this binding's rooted `{lb, ub}` mirror
    /// (a 2-slot [`Frame`] owned by `subtype_unionall`). Narrowing writes
    /// through it, so the current bounds are always GC roots.
    root_base: usize,
}

/// The subtype environment (`jl_stenv_t`): the stack of variable bindings and
/// the current invariant-constructor nesting depth.
struct Env {
    vars: Vec<VarBinding>,
    invdepth: i32,
}

impl Env {
    /// Innermost binding for `var`, if it is in scope (`lookup_binding`).
    fn lookup(&self, var: Offset) -> Option<usize> {
        self.vars.iter().rposition(|b| b.var == var)
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
struct SavedVars {
    vars: Vec<VarBinding>,
    _roots: Frame,
}

/// Snapshot the environment's bindings and root their bounds (`save_env`).
fn save_vars(e: &Env) -> SavedVars {
    let roots = Frame::new(e.vars.len() * 2);
    for (i, b) in e.vars.iter().enumerate() {
        roots.set(i * 2, Value(b.lb));
        roots.set(i * 2 + 1, Value(b.ub));
    }
    SavedVars { vars: e.vars.clone(), _roots: roots }
}

/// Restore the environment to a snapshot taken at the same binding depth
/// (`restore_env`), re-syncing each binding's live bounds mirror.
fn restore_vars(e: &mut Env, saved: &SavedVars) {
    e.vars.clear();
    e.vars.extend_from_slice(&saved.vars);
    for b in &e.vars {
        gc::set_slot(b.root_base, Value(b.lb));
        gc::set_slot(b.root_base + 1, Value(b.ub));
    }
}

/// Entry point: decide `a <: b`.
pub fn subtype(a: Offset, b: Offset) -> bool {
    // The C expects the caller to root the query types; our host boundary
    // passes raw offsets, so the engine entry roots them for the query's
    // duration (any type reached below is a subterm of these, of a rooted
    // bound, or is itself freshly rooted at its allocation site).
    let _ra = Rooted::new(Value(a));
    let _rb = Rooted::new(Value(b));
    let mut e = Env {
        vars: Vec::new(),
        invdepth: 0,
    };
    sub(a, b, &mut e, Param::None)
}

/// The main algorithm (`subtype` in `subtype.c`).
fn sub(x: Offset, y: Offset, e: &mut Env, param: Param) -> bool {
    if x == y {
        return true; // reflexive / uniqued-identical fast path
    }

    // Union on the left is a forall: every member must be a subtype.
    if types::is_union(x) {
        return sub(types::union_a(x), y, e, param) && sub(types::union_b(x), y, e, param);
    }
    // Union on the right is an exists: try each member, restoring the
    // environment between attempts (a simplified `exists_subtype`).
    if types::is_union(y) {
        let saved = save_vars(e);
        if sub(x, types::union_a(y), e, param) {
            return true;
        }
        restore_vars(e, &saved);
        return sub(x, types::union_b(y), e, param);
    }

    // Type variables, handled before the ground cases (as in subtype.c).
    if types::is_typevar(x) {
        if types::is_typevar(y) {
            return subtype_two_vars(x, y, e, param);
        }
        if types::is_unionall(y) {
            // x is a variable, y a `where`: introduce y's variable (∃) first.
            return subtype_unionall(x, y, e, true, param);
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
        depth0: e.invdepth,
        root_base: mirror.slot_index(0),
    });
    let mut ans = if r {
        sub(t, body, e, param)
    } else {
        sub(body, t, e, param)
    };

    // The diagonal rule: a variable occurring more than once and only in
    // covariant position (never invariantly in the body) is constrained to
    // concrete types, so its inferred lower bound must be a leaf type. E.g.
    // `Tuple{Int,Int} <: Tuple{T,T} where T` but not `Tuple{Int,Float64} <: ...`.
    let vb = e.vars[idx];
    let body_occurs_inv = var_occurs_invariant(body, var, false);
    let cov = vb.occurs_cov.max(vb.cov_diag); // cov_count
    let diagonal = cov > 1 && !body_occurs_inv;
    // A typevar lower bound does not reject: each value of the referenced
    // (universal) variable is a single type, so the diagonal is satisfied.
    // Julia additionally propagates `concrete = 1` to that variable's binding —
    // the cross-var propagation `design/implementation.md` records as missing.
    if ans && diagonal && is_leaf_typevar(var) && !types::is_typevar(vb.lb) && !is_leaf_bound(vb.lb) {
        ans = false;
    }

    e.vars.pop();
    ans
}

/// `subtype_var`: `b` is a type variable; relate it to the non-variable `a`.
/// `r` follows Julia — `true` constrains `a <: b` (`var_gt`), `false`
/// constrains `b <: a` (`var_lt`).
fn subtype_var(b: Offset, a: Offset, e: &mut Env, r: bool, param: Param) -> bool {
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

/// `var_lt`: constrain the variable at `idx` by `<: a`, narrowing its upper
/// bound when it is existential.
fn var_lt(a: Offset, e: &mut Env, idx: usize, param: Param) -> bool {
    record_occurrence(e, idx, param);
    let bb = e.vars[idx];
    if !bb.existential {
        // ∀b . b <: a   ⟺   ub <: a (the variable's widest value).
        return sub(bb.ub, a, e, param);
    }
    if bb.ub == a {
        return true;
    }
    if !ccheck(bb.lb, a, e) {
        return false; // lower bound must already satisfy the constraint
    }
    let m = simple_meet(e.vars[idx].ub, a);
    e.set_ub(idx, m);
    true
}

/// `var_gt`: constrain the variable at `idx` by `>: a`, raising its lower bound
/// when it is existential.
fn var_gt(a: Offset, e: &mut Env, idx: usize, param: Param) -> bool {
    record_occurrence(e, idx, param);
    let bb = e.vars[idx];
    if !bb.existential {
        // ∀b . a <: b   ⟺   a <: lb (the variable's narrowest value).
        return sub(a, bb.lb, e, param);
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

/// Run a variable's bound-consistency `sub` check in its own diagonal-rule
/// scope (`subtype_ccheck` with `push`/`pop_consistency_scope`): covariant
/// occurrences recorded inside fold into each variable's `cov_diag` (via max)
/// rather than accumulating in the outer `occurs_cov`. Without this, checking
/// one variable's bound would falsely make another variable diagonal.
/// The check enters at `Param::None`, as `subtype_ccheck` does — a top-level
/// variable occurrence inside a bound check is not a covariant occurrence.
fn ccheck(a: Offset, b: Offset, e: &mut Env) -> bool {
    let saved: Vec<i8> = e.vars.iter().map(|v| v.occurs_cov).collect();
    for v in e.vars.iter_mut() {
        v.occurs_cov = 0;
    }
    let ok = sub(a, b, e, Param::None);
    // Bindings pushed inside `sub` are balanced out by now; fold the survivors.
    for (i, v) in e.vars.iter_mut().enumerate() {
        if i < saved.len() {
            v.cov_diag = v.cov_diag.max(v.occurs_cov);
            v.occurs_cov = saved[i];
        }
    }
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

/// Record a covariant occurrence of the variable at `idx` for the diagonal rule
/// (`record_var_occurrence`). The counter saturates at 2; invariant occurrences
/// are recognised statically (`var_occurs_invariant`) rather than counted here.
fn record_occurrence(e: &mut Env, idx: usize, param: Param) {
    if param == Param::Covariant && e.vars[idx].occurs_cov < 2 {
        e.vars[idx].occurs_cov += 1;
    }
}

/// Whether `var`'s declared lower bound is a leaf (concrete) type
/// (`is_leaf_typevar`): only then can the diagonal rule pin it to concrete
/// values.
fn is_leaf_typevar(var: Offset) -> bool {
    is_leaf_bound(types::tvar_lb(var))
}

/// Whether `v` is a concrete leaf type (`is_leaf_bound`): `Union{}`, or a
/// non-abstract `DataType` all of whose parameters are themselves leaves.
/// Unions, type variables, and `UnionAll`s are not leaves.
fn is_leaf_bound(v: Offset) -> bool {
    if v == types::builtin(id::BOTTOM) {
        return true;
    }
    if !types::is_datatype(v) || types::is_abstract(v) {
        return false;
    }
    let p = types::parameters_of(v);
    if p == NULL {
        return true; // a concrete primitive/leaf with no parameters
    }
    (0..types::svec_len(p)).all(|i| is_leaf_bound(types::svec_ref(p, i)))
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
        return tuple_subtype(xd, y, e);
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
    let _ = param;
    ans
}

/// Covariant tuple subtyping (`subtype_tuple`, `subtype.c:1837`): a length
/// classification prefix, then the elementwise tail. Handles a trailing
/// unbounded `Vararg` on either side; bounded `Vararg{T,N}` is not yet
/// represented, so the `JL_VARARG_INT`/`JL_VARARG_BOUND` classifications and the
/// length-equation branches are absent (a faithful partial).
fn tuple_subtype(x: Offset, y: Offset, e: &mut Env) -> bool {
    let px = types::parameters_of(x);
    let py = types::parameters_of(y);
    let lx = if px == NULL { 0 } else { types::svec_len(px) };
    let ly = if py == NULL { 0 } else { types::svec_len(py) };
    if lx == 0 && ly == 0 {
        return true;
    }
    // A trailing unbounded `Vararg` is Julia's `JL_VARARG_UNBOUND`; anything else
    // last is `JL_VARARG_NONE`.
    let vvx = lx > 0 && types::is_vararg(types::svec_ref(px, lx - 1));
    let vvy = ly > 0 && types::is_vararg(types::svec_ref(py, ly - 1));
    // Length classification (`subtype.c:1860-1894`, unbounded subset).
    if vvx {
        // Unbounded on the left includes `N == 0` (`subtype.c:1862-1867`).
        if !vvy {
            return false; // right side is fixed-length
        }
        if lx < ly {
            return false; // both unbounded, but x's prefix is shorter
        }
    } else {
        let nx = lx;
        let ny = if vvy { ly - 1 } else { ly };
        if !vvy {
            if nx != ny {
                return false; // both fixed: arities must match
            }
        } else if ny > nx {
            return false; // x too short to cover y's fixed prefix
        }
    }
    subtype_tuple_tail(px, py, lx, ly, e)
}

/// The elementwise tail walk (`subtype_tuple_tail`, `subtype.c:1740`), for the
/// unbounded-`Vararg` subset. `vx`/`vy` count how far into a trailing `Vararg`
/// each side has advanced; once both are inside one, `subtype_tuple_varargs`
/// finishes the comparison.
fn subtype_tuple_tail(px: Offset, py: Offset, lx: u32, ly: u32, e: &mut Env) -> bool {
    let (mut i, mut j) = (0u32, 0u32);
    let (mut vx, mut vy) = (0u32, 0u32);
    loop {
        let mut xi = NULL;
        if i < lx {
            xi = types::svec_ref(px, i);
            if i == lx - 1 && (vx > 0 || types::is_vararg(xi)) {
                vx += 1;
            }
        }
        let mut yi = NULL;
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
            // `Tuple{...} <: Tuple{..., Vararg{Any}}`: the remaining left elements
            // are all `<: Any`, so match the tails directly (`subtype.c:1767`).
            let xlast = types::svec_ref(px, lx - 1);
            if types::is_vararg(xlast) {
                all_varargs = true;
                xi = xlast;
                vx = 1;
            } else {
                break;
            }
        }
        if all_varargs {
            return subtype_tuple_varargs(xi, yi, e);
        }
        if j >= ly {
            return vx > 0;
        }
        let xii = if vx > 0 { types::vararg_elem(xi) } else { xi };
        let yii = if vy > 0 { types::vararg_elem(yi) } else { yi };
        if !sub(xii, yii, e, Param::Covariant) {
            return false;
        }
        if i < lx - 1 || vx == 0 {
            i += 1;
        }
        if j < ly - 1 || vy == 0 {
            j += 1;
        }
    }
    // With only unbounded varargs there is no `N` length equation to discharge
    // (`subtype.c:1828-1832` handled the bounded case).
    true
}

/// `Tuple{..., Vararg{S}} <: Tuple{..., Vararg{T}}` for unbounded varargs
/// (`subtype_tuple_varargs`, `subtype.c:1587`, `N`-absent path): reduce to
/// `S <: T`, checked twice so a diagonal variable in `S` is constrained as it
/// must be across ≥2 arguments (`subtype.c:1651-1656`). The repeated-element and
/// separable fast paths are omitted as pure optimizations.
fn subtype_tuple_varargs(vtx: Offset, vty: Offset, e: &mut Env) -> bool {
    let xp0 = types::vararg_elem(vtx);
    let yp0 = types::vararg_elem(vty);
    sub(xp0, yp0, e, Param::Covariant) && sub(xp0, yp0, e, Param::Covariant)
}

/// Invariant equality of two type parameters (`forall_exists_equal`): subtype in
/// both directions. The forward direction runs at `Invariant`; the reverse runs
/// at `Param::None`, as in the C — the occurrences were already recorded going
/// forward. The environment is restored if the first direction narrows it but
/// the second fails.
fn forall_exists_equal(x: Offset, y: Offset, e: &mut Env) -> bool {
    if x == y {
        return true;
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
    let saved = save_vars(e);
    if !sub(x, y, e, Param::Invariant) {
        restore_vars(e, &saved);
        return false;
    }
    sub(y, x, e, Param::None)
}

/// Greatest lower bound (`simple_meet`). For ground operands the GLB is the
/// subtype side; when a type variable is involved we over-estimate by `b`
/// (subtype-path bias), since there is no `Intersect` node. Crucially, the
/// ground check uses a *fresh* environment, so it never narrows the existential
/// variables of the live query.
fn simple_meet(a: Offset, b: Offset) -> Offset {
    let any = types::builtin(id::ANY);
    let bottom = types::builtin(id::BOTTOM);
    if a == any || b == bottom || a == b {
        return b;
    }
    if b == any || a == bottom {
        return a;
    }
    if !types::is_typevar(a) && !types::is_typevar(b) {
        if types::issubtype(a, b) {
            return a;
        }
        if types::issubtype(b, a) {
            return b;
        }
    }
    b
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
