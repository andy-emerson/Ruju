//! Builtins: `===` (egal) and structural type equality.
//!
//! A faithful core of Julia's egal machinery, ported from the pin:
//! `jl_egal_` (`src/julia.h:1877` — identity, then type tags, then the
//! unboxed comparison), `jl_egal__unboxed_` (`src/julia.h:1866` — symbols,
//! `Bool`, and `Nothing` compare by identity only; mutables likewise),
//! `jl_egal__bitstag` (`src/builtins.c:247` — payload bits by width, svec
//! elementwise, the DataType name+parameters case, `Union` componentwise,
//! `UnionAll` via `egal_types` with `tvar_names = 1`), and `egal_types`
//! (`src/builtins.c:169` — structural type equality under a typevar
//! environment; `jl_types_egal` is the `tvar_names = 0` entry,
//! `src/builtins.c:230`).
//!
//! The identity-only rules are sound here because of the object model:
//! symbols are interned, `Bool`'s two values are the bootstrap permboxes,
//! and `nothing` is `Nothing.instance`. Omitted relative to the C (no such
//! values exist yet): strings, mutable/immutable struct fields
//! (`compare_fields`), `Vararg`, `TypeEq`, modules, and the
//! concrete-DataType fast path (uniquing makes the parameter comparison
//! reach the same answer).

use crate::object::{self, Value};
use crate::region::{self, Offset, NULL};
use crate::types::{self, id};

/// `a === b` (`jl_egal`).
pub fn egal(a: Value, b: Value) -> bool {
    if a == b {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    let t = object::type_of(a);
    if object::type_of(b) != t {
        return false;
    }
    egal_unboxed(t, a.raw(), b.raw())
}

/// Structural type equality (`jl_types_egal`): alpha-equivalent `where`
/// types are equal — variable *names* do not matter (`tvar_names = 0`).
pub fn types_egal(a: Offset, b: Offset) -> bool {
    egal_types(a, b, &mut Vec::new(), false)
}

/// The per-kind comparison after identity and type tags match
/// (`jl_egal__unboxed_` + `jl_egal__bitstag`).
fn egal_unboxed(t: Offset, a: Offset, b: Offset) -> bool {
    // Identity-only kinds: interned symbols, the Bool permboxes, the
    // `nothing` singleton (jl_egal__unboxed_), and the mutable/nominal
    // TypeName and free TypeVar (the bitstag switch returns 0 for tvar).
    if t == types::builtin(id::SYMBOL)
        || t == types::builtin(id::BOOL)
        || t == types::builtin(id::NOTHING)
        || t == types::builtin(id::TVAR)
        || t == types::builtin(id::TYPENAME)
    {
        return false;
    }
    if t == types::builtin(id::SVEC) {
        let (la, lb) = (types::svec_len(a), types::svec_len(b));
        return la == lb
            && (0..la).all(|i| egal(Value(types::svec_ref(a, i)), Value(types::svec_ref(b, i))));
    }
    if t == types::builtin(id::UNION) {
        // compare_fields over jl_uniontype_t's two references: componentwise
        // and order-sensitive (normalization supplies the canonical order).
        return egal(Value(types::union_a(a)), Value(types::union_a(b)))
            && egal(Value(types::union_b(a)), Value(types::union_b(b)));
    }
    if t == types::builtin(id::UNIONALL) {
        // `===` on `where` types is name-sensitive: tvar_names = 1.
        return egal_types(a, b, &mut Vec::new(), true);
    }
    if t == types::builtin(id::DATATYPE) {
        // Same constructor (TypeName identity), then parameters. The C's
        // concrete-type fast path is omitted: uniqued instantiations reach
        // the same answer through the parameter comparison.
        if types::name_of(a) != types::name_of(b) {
            return false;
        }
        let (pa, pb) = (types::parameters_of(a), types::parameters_of(b));
        if pa == NULL || pb == NULL {
            return pa == pb;
        }
        let (la, lb) = (types::svec_len(pa), types::svec_len(pb));
        return la == lb
            && (0..la).all(|i| egal(Value(types::svec_ref(pa, i)), Value(types::svec_ref(pb, i))));
    }
    if types::is_primitive(t) {
        // Payload bits by the type's width (Float64 NaNs with equal bits are
        // egal; -0.0 and 0.0 are not — bitwise, unlike `==`).
        return bits_equal(a, b, types::size_of(t));
    }
    // No other value kinds exist yet; struct fields (compare_fields) arrive
    // with the struct-support increment.
    false
}

/// `egal_types` (`src/builtins.c:169`): structural equality of types under
/// an environment pairing the `where` variables already entered on each
/// side. A bound variable equals exactly its partner; a free variable
/// equals nothing but itself (caught by identity before recursion).
fn egal_types(a: Offset, b: Offset, env: &mut Vec<(Offset, Offset)>, tvar_names: bool) -> bool {
    if a == b {
        return true;
    }
    let t = object::type_of(Value(a));
    if object::type_of(Value(b)) != t {
        return false;
    }
    if t == types::builtin(id::DATATYPE) {
        if types::name_of(a) != types::name_of(b) {
            return false;
        }
        let (pa, pb) = (types::parameters_of(a), types::parameters_of(b));
        if pa == NULL || pb == NULL {
            return pa == pb;
        }
        let (la, lb) = (types::svec_len(pa), types::svec_len(pb));
        return la == lb
            && (0..la).all(|i| egal_types(types::svec_ref(pa, i), types::svec_ref(pb, i), env, tvar_names));
    }
    if t == types::builtin(id::TVAR) {
        // Innermost binding wins; an unbound (free) variable matches nothing.
        for &(va, vb) in env.iter().rev() {
            if va == a {
                return vb == b;
            }
        }
        return false;
    }
    if t == types::builtin(id::UNIONALL) {
        let (va, vb) = (types::unionall_var(a), types::unionall_var(b));
        if tvar_names && types::tvar_name(va) != types::tvar_name(vb) {
            return false;
        }
        if !egal_types(types::tvar_lb(va), types::tvar_lb(vb), env, tvar_names)
            || !egal_types(types::tvar_ub(va), types::tvar_ub(vb), env, tvar_names)
        {
            return false;
        }
        env.push((va, vb));
        let ans = egal_types(types::unionall_body(a), types::unionall_body(b), env, tvar_names);
        env.pop();
        return ans;
    }
    if t == types::builtin(id::UNION) {
        return egal_types(types::union_a(a), types::union_a(b), env, tvar_names)
            && egal_types(types::union_b(a), types::union_b(b), env, tvar_names);
    }
    // Non-type values inside parameters fall through to the bitstag tail.
    egal_unboxed(t, a, b)
}

/// Bitwise payload equality over `size` bytes (`bits_equal`).
fn bits_equal(a: Offset, b: Offset, size: u32) -> bool {
    (0..size).all(|i| unsafe { *region::ptr_mut::<u8>(a + i) == *region::ptr_mut::<u8>(b + i) })
}
