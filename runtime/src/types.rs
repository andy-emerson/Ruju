//! DataType objects, the builtin-type hierarchy, and nominal subtyping.
//!
//! Faithful (but nominal-only) port of `jl_datatype_t` / `jl_datatype_layout_t`
//! (`src/julia.h`) and `jl_init_types` (`src/jltypes.c`). A DataType is itself a
//! tagged heap object whose body carries its name (a `Symbol`), supertype, an
//! optional layout describing embedded reference fields, the instance size, and
//! flags. The core types are bootstrapped at startup, including the abstract
//! hierarchy (`Any` at the top), the full primitive tower, `Symbol`, `Nothing`,
//! and the self-referential `DataType : DataType` / `Any <: Any` cycles.
//!
//! Parametric types, `Union`/`UnionAll`/`TypeVar`, and the real subtyping
//! algorithm (`subtype.c`) are deliberately deferred; subtyping here is nominal
//! (walk the `super` chain).

use core::cell::Cell;

use crate::gc::Rooted;
use crate::object;
use crate::region::{self, Offset, NULL};
use crate::symbol;

/// The body of a DataType object: a reduced `jl_datatype_t` with an inlined
/// reference to its layout. Field byte offsets must stay in sync with the GC
/// pointer bitmap built for `DataType` in [`bootstrap`] (name@0, super@4).
#[repr(C)]
pub struct DataType {
    /// The type's name (a `TypeName` reference, `jl_typename_t`).
    pub name: Offset,
    /// The supertype (a `DataType` reference); `Any`'s super is itself.
    pub super_: Offset,
    /// Type parameters (a `SimpleVector` reference, or `NULL` for non-parametric
    /// types). For tuple types these are the element types.
    pub parameters: Offset,
    /// Raw layout metadata (offset, or `NULL`): the embedded-pointer bitmap.
    pub layout: Offset,
    /// Size in bytes of an instance's data (`layout->size`).
    pub size: u32,
    /// Number of fields (`0` for the nominal-tier primitives).
    pub nfields: u32,
    /// Bit flags: [`FLAG_ABSTRACT`], [`FLAG_PRIMITIVE`].
    pub flags: u32,
}

/// Set if the type is abstract (`isabstracttype`: no direct instances).
pub const FLAG_ABSTRACT: u32 = 1;
/// Set if declared as a `primitive type` (`isprimitivetype`).
pub const FLAG_PRIMITIVE: u32 = 2;

const DT_SIZE: usize = core::mem::size_of::<DataType>();

/// Stable ids for the builtin types, indexing [`Builtins::types`].
pub mod id {
    pub const ANY: u32 = 0;
    pub const NUMBER: u32 = 1;
    pub const REAL: u32 = 2;
    pub const INTEGER: u32 = 3;
    pub const SIGNED: u32 = 4;
    pub const UNSIGNED: u32 = 5;
    pub const ABSTRACTFLOAT: u32 = 6;
    pub const ABSTRACTCHAR: u32 = 7;
    pub const BOOL: u32 = 8;
    pub const INT8: u32 = 9;
    pub const INT16: u32 = 10;
    pub const INT32: u32 = 11;
    pub const INT64: u32 = 12;
    pub const INT128: u32 = 13;
    pub const UINT8: u32 = 14;
    pub const UINT16: u32 = 15;
    pub const UINT32: u32 = 16;
    pub const UINT64: u32 = 17;
    pub const UINT128: u32 = 18;
    pub const FLOAT16: u32 = 19;
    pub const FLOAT32: u32 = 20;
    pub const FLOAT64: u32 = 21;
    pub const CHAR: u32 = 22;
    pub const SYMBOL: u32 = 23;
    pub const NOTHING: u32 = 24;
    pub const DATATYPE: u32 = 25;
    pub const BOTTOM: u32 = 26; // Union{}, the bottom type
    pub const SVEC: u32 = 27; // SimpleVector, the parameter container
    pub const UNION: u32 = 28; // the type of Union{...} objects
    pub const TYPENAME: u32 = 29; // jl_typename_t: a DataType's name object
    pub const TVAR: u32 = 30; // jl_tvar_t: a `where` type variable
    pub const UNIONALL: u32 = 31; // jl_unionall_t: a `T where ...` type
    pub const COUNT: usize = 32;
}

/// Offsets of the bootstrapped core types and the `nothing` singleton.
#[derive(Clone, Copy)]
pub struct Builtins {
    pub types: [Offset; id::COUNT],
    pub nothing_instance: Offset,
    /// The `TypeName` shared by every tuple type (`jl_tuple_typename`).
    pub tuple_typename: Offset,
    /// The `TypeName` of the demo parametric constructor `Box{T}` (invariant).
    pub box_typename: Offset,
}

struct BuiltinsSlot(Cell<Option<Builtins>>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for BuiltinsSlot {}
static BUILTINS: BuiltinsSlot = BuiltinsSlot(Cell::new(None));

/// The bootstrapped core types. Panics if [`bootstrap`] has not run.
pub fn builtins() -> Builtins {
    BUILTINS.0.get().expect("types::bootstrap has not been called")
}

/// Whether [`bootstrap`] has completed. The allocator checks this before
/// triggering an automatic collection, since the core types (which the
/// collector roots from) do not exist mid-bootstrap.
pub fn is_bootstrapped() -> bool {
    BUILTINS.0.get().is_some()
}

/// Region offset of the builtin type with the given [`id`].
pub fn builtin(type_id: u32) -> Offset {
    builtins().types[type_id as usize]
}

/// Region offset of the `nothing` singleton value.
pub fn nothing_instance() -> Offset {
    builtins().nothing_instance
}

// --- DataType body access ---------------------------------------------------

fn dt(t: Offset) -> *mut DataType {
    region::ptr_mut::<DataType>(t)
}

#[allow(clippy::too_many_arguments)]
fn write_dt(
    t: Offset,
    name: Offset,
    super_: Offset,
    parameters: Offset,
    layout: Offset,
    size: u32,
    flags: u32,
) {
    unsafe {
        let p = dt(t);
        (*p).name = name;
        (*p).super_ = super_;
        (*p).parameters = parameters;
        (*p).layout = layout;
        (*p).size = size;
        (*p).nfields = 0;
        (*p).flags = flags;
    }
}

/// Allocate a raw (untagged) layout: `npointers` followed by their byte offsets
/// within an instance. This is GC metadata, not a Julia object, so it is never
/// traced — mirroring `jl_datatype_t.layout` pointing at a C struct.
fn make_layout(ptr_offsets: &[u32]) -> Offset {
    let off = region::alloc(4 + ptr_offsets.len() * 4);
    unsafe {
        let p = region::ptr_mut::<u32>(off);
        *p = ptr_offsets.len() as u32;
        for (i, &po) in ptr_offsets.iter().enumerate() {
            *p.add(1 + i) = po;
        }
    }
    off
}

/// Allocate a DataType object describing the given type.
fn new_type(datatype: Offset, name: Offset, super_: Offset, size: u32, flags: u32, ptrs: &[u32]) -> Offset {
    let v = object::alloc(datatype, DT_SIZE);
    let layout = if ptrs.is_empty() { NULL } else { make_layout(ptrs) };
    write_dt(v.raw(), name, super_, NULL, layout, size, flags);
    v.raw()
}

// --- bootstrap --------------------------------------------------------------

// (id, name, super_id, flags, size); ordered so each supertype precedes its uses.
const TABLE: &[(u32, &str, u32, u32, u32)] = &[
    (id::NUMBER, "Number", id::ANY, FLAG_ABSTRACT, 0),
    (id::REAL, "Real", id::NUMBER, FLAG_ABSTRACT, 0),
    (id::INTEGER, "Integer", id::REAL, FLAG_ABSTRACT, 0),
    (id::SIGNED, "Signed", id::INTEGER, FLAG_ABSTRACT, 0),
    (id::UNSIGNED, "Unsigned", id::INTEGER, FLAG_ABSTRACT, 0),
    (id::ABSTRACTFLOAT, "AbstractFloat", id::REAL, FLAG_ABSTRACT, 0),
    (id::ABSTRACTCHAR, "AbstractChar", id::ANY, FLAG_ABSTRACT, 0),
    (id::BOOL, "Bool", id::INTEGER, FLAG_PRIMITIVE, 1),
    (id::INT8, "Int8", id::SIGNED, FLAG_PRIMITIVE, 1),
    (id::INT16, "Int16", id::SIGNED, FLAG_PRIMITIVE, 2),
    (id::INT32, "Int32", id::SIGNED, FLAG_PRIMITIVE, 4),
    (id::INT64, "Int64", id::SIGNED, FLAG_PRIMITIVE, 8),
    (id::INT128, "Int128", id::SIGNED, FLAG_PRIMITIVE, 16),
    (id::UINT8, "UInt8", id::UNSIGNED, FLAG_PRIMITIVE, 1),
    (id::UINT16, "UInt16", id::UNSIGNED, FLAG_PRIMITIVE, 2),
    (id::UINT32, "UInt32", id::UNSIGNED, FLAG_PRIMITIVE, 4),
    (id::UINT64, "UInt64", id::UNSIGNED, FLAG_PRIMITIVE, 8),
    (id::UINT128, "UInt128", id::UNSIGNED, FLAG_PRIMITIVE, 16),
    (id::FLOAT16, "Float16", id::ABSTRACTFLOAT, FLAG_PRIMITIVE, 2),
    (id::FLOAT32, "Float32", id::ABSTRACTFLOAT, FLAG_PRIMITIVE, 4),
    (id::FLOAT64, "Float64", id::ABSTRACTFLOAT, FLAG_PRIMITIVE, 8),
    (id::CHAR, "Char", id::ABSTRACTCHAR, FLAG_PRIMITIVE, 4),
    (id::NOTHING, "Nothing", id::ANY, 0, 0),
];

/// Bootstrap the core types into the region (mirrors `jl_init_types`). Must run
/// after [`region::init`](crate::region::init) and before any value is boxed.
pub fn bootstrap() {
    symbol::reset();
    let mut types = [NULL; id::COUNT];

    // 1. Allocate the bootstrap type objects; their bodies are patched once
    //    Symbols and TypeNames can be created. DataType is its own type.
    let datatype = {
        let v = object::alloc(0, DT_SIZE);
        object::set_type(v, v.raw());
        v.raw()
    };
    types[id::DATATYPE as usize] = datatype;
    let symbol = object::alloc(datatype, DT_SIZE).raw();
    types[id::SYMBOL as usize] = symbol;
    let typename = object::alloc(datatype, DT_SIZE).raw();
    types[id::TYPENAME as usize] = typename;
    let any = object::alloc(datatype, DT_SIZE).raw();
    types[id::ANY as usize] = any;

    // 2. Symbols and TypeNames can now be made. A DataType's `name` is a
    //    TypeName (jl_typename_t), which carries the Symbol and a uniquing cache.
    let sym = |s: &str| symbol::intern(symbol, s);
    let tn = |s: &str| make_typename(typename, sym(s));

    // 3. Patch the bootstrap bodies. DataType instances embed name@0, super@4,
    //    parameters@8; TypeName instances embed name@0 (Symbol) and cache@4.
    write_dt(datatype, tn("DataType"), any, NULL, make_layout(&[0, 4, 8]), DT_SIZE as u32, 0);
    write_dt(symbol, tn("Symbol"), any, NULL, NULL, 0, 0);
    write_dt(typename, tn("TypeName"), any, NULL, make_layout(&[0, 4]), 8, 0);
    write_dt(any, tn("Any"), any, NULL, NULL, 0, FLAG_ABSTRACT);

    // 4. The remaining hierarchy and primitive tower, in dependency order.
    for &(tid, name, super_id, flags, size) in TABLE {
        let t = new_type(datatype, tn(name), types[super_id as usize], size, flags, &[]);
        types[tid as usize] = t;
    }

    // 5. Bottom (Union{}), the SimpleVector parameter container, and the Union
    //    object type. SimpleVectors have a variable number of embedded
    //    references, so the collector traces them specially rather than via a
    //    fixed layout; Union objects hold two type references (a@0, b@4).
    types[id::BOTTOM as usize] = new_type(datatype, tn("Union{}"), any, 0, FLAG_ABSTRACT, &[]);
    types[id::SVEC as usize] = new_type(datatype, tn("SimpleVector"), any, 0, 0, &[]);
    types[id::UNION as usize] = new_type(datatype, tn("Union"), any, 8, 0, &[0, 4]);

    // `TypeVar` (jl_tvar_t) holds a name Symbol and its lower/upper bounds
    // (name@0, lb@4, ub@8). `UnionAll` (jl_unionall_t) binds one TypeVar over a
    // body type (var@0, body@4). Together they are the `where` machinery.
    types[id::TVAR as usize] = new_type(datatype, tn("TypeVar"), any, 12, 0, &[0, 4, 8]);
    types[id::UNIONALL as usize] = new_type(datatype, tn("UnionAll"), any, 8, 0, &[0, 4]);

    // 6. The shared tuple TypeName: every Tuple{...} type has this `name`, which
    //    is how tuples are identified (jl_tuple_typename). `Box` is a demo
    //    parametric constructor (an invariant single-parameter type).
    let tuple_typename = tn("Tuple");
    let box_typename = tn("Box");

    // 7. The `nothing` singleton: the sole (zero-size) instance of Nothing.
    let nothing_instance = object::alloc(types[id::NOTHING as usize], 0).raw();

    BUILTINS.0.set(Some(Builtins {
        types,
        nothing_instance,
        tuple_typename,
        box_typename,
    }));
}

/// Allocate a `TypeName` object: `[name (Symbol) @0 | cache @4]`. The cache slot
/// is reserved for hash-consed type uniquing and is `NULL` for now.
fn make_typename(typename_type: Offset, name_sym: Offset) -> Offset {
    let v = object::alloc(typename_type, 8).raw();
    write_ref(v, 0, name_sym);
    write_ref(v, 4, NULL);
    v
}

// --- queries ----------------------------------------------------------------

/// A DataType's instance `size`.
pub fn size_of(t: Offset) -> u32 {
    unsafe { (*dt(t)).size }
}

/// A DataType's supertype (`Any`'s super is itself).
pub fn supertype(t: Offset) -> Offset {
    unsafe { (*dt(t)).super_ }
}

/// Whether the value at `t` is a `DataType` (as opposed to a `Union`,
/// `TypeVar`, or `UnionAll`). Tuple and parametric types are DataTypes.
pub fn is_datatype(t: Offset) -> bool {
    object::type_of(object::Value(t)) == builtin(id::DATATYPE)
}

/// Whether the DataType at `t` is declared abstract (`isabstracttype`).
pub fn is_abstract(t: Offset) -> bool {
    unsafe { (*dt(t)).flags & FLAG_ABSTRACT != 0 }
}

/// A DataType's `TypeName` (the `name` field).
pub fn name_of(t: Offset) -> Offset {
    unsafe { (*dt(t)).name }
}

/// The `Symbol` of a `TypeName`.
pub fn typename_sym(tn: Offset) -> Offset {
    read_ref(tn, 0)
}

/// The `Symbol` naming a type (its `TypeName`'s symbol).
pub fn type_sym(t: Offset) -> Offset {
    typename_sym(name_of(t))
}

// --- raw reference field access (for svec, tuple, and union objects) --------

fn read_ref(obj: Offset, byte_off: u32) -> Offset {
    unsafe { *region::ptr_mut::<u32>(obj + byte_off) }
}

fn write_ref(obj: Offset, byte_off: u32, val: Offset) {
    unsafe {
        *region::ptr_mut::<u32>(obj + byte_off) = val;
    }
}

// --- SimpleVector (svec): the type-parameter container ----------------------

/// Allocate a `SimpleVector` of the given element offsets: `[len | elems...]`.
fn make_svec(elems: &[Offset]) -> Offset {
    let st = builtins().types[id::SVEC as usize];
    let v = object::alloc(st, 4 + 4 * elems.len()).raw();
    unsafe {
        *region::ptr_mut::<u32>(v) = elems.len() as u32;
    }
    for (i, &e) in elems.iter().enumerate() {
        write_ref(v, (4 + 4 * i) as u32, e);
    }
    v
}

/// Whether `t` is the `SimpleVector` type (the collector traces these specially).
pub fn is_svec(t: Offset) -> bool {
    t == builtins().types[id::SVEC as usize]
}

/// Length of the SimpleVector at offset `s`.
pub fn svec_len(s: Offset) -> u32 {
    unsafe { *region::ptr_mut::<u32>(s) }
}

/// The `i`-th element of the SimpleVector at offset `s`.
pub fn svec_ref(s: Offset, i: u32) -> Offset {
    read_ref(s, 4 + 4 * i)
}

// --- tuple and union constructors / queries ---------------------------------

// --- type uniquing (hash-consing) -------------------------------------------
//
// Instantiated parametric types are cached on their `TypeName` (in the `cache`
// slot at byte offset 4), so structurally identical types are `===`, as in
// Julia's hash-consed allocation. The cache is a `SimpleVector` of types reached
// through the TypeName, so it needs no extra GC roots; the only subtlety is that
// extending it mutates the (old) TypeName to point at a (young) svec, which goes
// through the write barrier. The lookup is a linear scan for now (Julia keeps a
// sorted/hashed cache).

const TN_CACHE: u32 = 4; // byte offset of the cache field within a TypeName

/// Find a cached parametric type under `typename` whose parameters equal
/// `params` (by offset, since parameters are themselves canonical), or `NULL`.
fn cache_lookup(typename: Offset, params: &[Offset]) -> Offset {
    let cache = read_ref(typename, TN_CACHE);
    if cache == NULL {
        return NULL;
    }
    for i in 0..svec_len(cache) {
        let ty = svec_ref(cache, i);
        let p = parameters_of(ty);
        if p != NULL
            && svec_len(p) as usize == params.len()
            && (0..params.len()).all(|k| svec_ref(p, k as u32) == params[k])
        {
            return ty;
        }
    }
    NULL
}

/// Append `ty` to `typename`'s instantiation cache, growing the svec.
fn cache_insert(typename: Offset, ty: Offset) {
    let _ty_root = Rooted::new(object::Value(ty)); // root across the svec allocation
    let old = read_ref(typename, TN_CACHE);
    let mut entries: Vec<Offset> = Vec::new();
    if old != NULL {
        for i in 0..svec_len(old) {
            entries.push(svec_ref(old, i));
        }
    }
    entries.push(ty);
    let new = make_svec(&entries);
    // The TypeName may be old and the new cache svec young: barrier the store.
    crate::gc::write_barrier(object::Value(typename), object::Value(new));
    write_ref(typename, TN_CACHE, new);
}

/// Instantiate a parametric type `typename{params...}` with the given supertype,
/// uniqued on the TypeName cache. Tuple types are covariant; every other
/// parametric type is **invariant**, which follows automatically: identical
/// instantiations are the same object (uniquing), and distinct ones are related
/// only through the nominal supertype chain, so `Box{Int} <: Box{Integer}` is
/// false while `Box{Int} <: Box{Int}` is true.
pub fn apply_type(typename: Offset, super_: Offset, params: &[Offset]) -> Offset {
    if let cached @ 1.. = cache_lookup(typename, params) {
        return cached;
    }
    // Root the parameter vector across the DataType allocation.
    let p = Rooted::new(object::Value(make_svec(params)));
    let v = object::alloc(builtin(id::DATATYPE), DT_SIZE);
    write_dt(v.raw(), typename, super_, p.get().raw(), NULL, 0, 0);
    cache_insert(typename, v.raw());
    v.raw()
}

/// Construct the tuple type `Tuple{elems...}` (covariant), uniqued.
pub fn tuple_type(elems: &[Offset]) -> Offset {
    let b = builtins();
    apply_type(b.tuple_typename, b.types[id::ANY as usize], elems)
}

/// Construct the demo parametric type `Box{elem}` (invariant), uniqued.
pub fn box_type(elem: Offset) -> Offset {
    let b = builtins();
    apply_type(b.box_typename, b.types[id::ANY as usize], &[elem])
}

pub(crate) fn parameters_of(t: Offset) -> Offset {
    unsafe { (*dt(t)).parameters }
}

/// Whether `t` is a tuple type — identified by the shared tuple `TypeName`,
/// exactly as Julia uses `dt->name == jl_tuple_typename`.
pub fn is_tuple(t: Offset) -> bool {
    name_of(t) == builtins().tuple_typename
}

/// Construct `Union{a, b}`, normalized. See [`union_of`].
pub fn union_type(a: Offset, b: Offset) -> Offset {
    union_of(&[a, b])
}

/// Construct the normalized union of `parts` — a faithful core of
/// `jl_type_union` (`jltypes.c`): flatten nested unions, drop any member
/// subsumed by another (`Union{Int,Real}` → `Real`), and order the survivors
/// canonically so the result is independent of argument order. Returns `Union{}`
/// for an empty set and the lone member for a singleton.
///
/// Not yet faithful: the result is not interned in a global type cache, so two
/// separately-built equal unions are structurally identical but not `===`
/// (Julia caches unions); `Vararg` union merging is absent.
pub fn union_of(parts: &[Offset]) -> Offset {
    let bottom = builtin(id::BOTTOM);

    // 1. Flatten nested unions into a flat component list.
    let mut comps: Vec<Offset> = Vec::new();
    for &p in parts {
        flatten_union(p, &mut comps);
    }

    // 2. Drop `Union{}` and any member that is a subtype of another (keeping the
    //    more general). For equal members, keep exactly one (the earlier index).
    let mut keep = vec![true; comps.len()];
    for i in 0..comps.len() {
        if comps[i] == bottom {
            keep[i] = false;
            continue;
        }
        for j in 0..comps.len() {
            if i != j && keep[j] && issubtype(comps[i], comps[j]) {
                // `i <: j`; drop `i` unless they are mutually subtypes and `i`
                // is the earlier index (so one of an equal pair survives).
                if !(issubtype(comps[j], comps[i]) && i < j) {
                    keep[i] = false;
                    break;
                }
            }
        }
    }
    let mut survivors: Vec<Offset> = comps
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, &c)| c)
        .collect();
    if survivors.is_empty() {
        return bottom;
    }

    // 3. Canonical order, then build a right-nested `Union` tree.
    survivors.sort_by(|&x, &y| type_cmp(x, y));
    survivors.dedup();
    if survivors.len() == 1 {
        return survivors[0];
    }
    build_union(&survivors)
}

/// Collect the leaf components of `t`, descending through nested `Union`s.
fn flatten_union(t: Offset, out: &mut Vec<Offset>) {
    if is_union(t) {
        flatten_union(union_a(t), out);
        flatten_union(union_b(t), out);
    } else {
        out.push(t);
    }
}

/// Build the right-nested `Union{p0, Union{p1, ...}}` from >= 2 canonical parts,
/// rooting the accumulator across each allocation.
fn build_union(parts: &[Offset]) -> Offset {
    let mut acc = parts[parts.len() - 1];
    for k in (0..parts.len() - 1).rev() {
        let acc_root = Rooted::new(object::Value(acc));
        let part_root = Rooted::new(object::Value(parts[k]));
        let v = object::alloc(builtin(id::UNION), 8).raw();
        write_ref(v, 0, part_root.get().raw());
        write_ref(v, 4, acc_root.get().raw());
        acc = v;
    }
    acc
}

/// Canonical type ordering for union members, a faithful subset of
/// `datatype_name_cmp` (`jltypes.c`): DataTypes before non-DataTypes, then by
/// type-name text, parameter count, and parameters recursively. (Module
/// qualification is absent in this single-module model.)
fn type_cmp(a: Offset, b: Offset) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let (a_dt, b_dt) = (is_datatype(a), is_datatype(b));
    if !a_dt || !b_dt {
        return b_dt.cmp(&a_dt); // DataTypes sort first; others compare equal
    }
    let ord = symbol::as_str(type_sym(a)).cmp(symbol::as_str(type_sym(b)));
    if ord != Ordering::Equal {
        return ord;
    }
    let (pa, pb) = (parameters_of(a), parameters_of(b));
    let na = if pa == NULL { 0 } else { svec_len(pa) };
    let nb = if pb == NULL { 0 } else { svec_len(pb) };
    if na != nb {
        return na.cmp(&nb);
    }
    for i in 0..na {
        let ord = type_cmp(svec_ref(pa, i), svec_ref(pb, i));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Whether the value at `t` is a `Union{...}` object.
pub fn is_union(t: Offset) -> bool {
    object::type_of(object::Value(t)) == builtin(id::UNION)
}

pub(crate) fn union_a(t: Offset) -> Offset {
    read_ref(t, 0)
}

pub(crate) fn union_b(t: Offset) -> Offset {
    read_ref(t, 4)
}

// --- `where` machinery: TypeVar and UnionAll --------------------------------
//
// A faithful port of `jl_tvar_t` / `jl_unionall_t` (`src/julia.h`). A `TypeVar`
// is a bounded variable `lb <: T <: ub`; a `UnionAll` quantifies one variable
// over a body type (`T where lb<:T<:ub`). They are ordinary tagged heap objects,
// so the collector traces their embedded references via the layout bitmaps set
// in [`bootstrap`]. The real subtype algorithm over them lives in
// [`crate::subtype`].

/// Allocate a `TypeVar` `lb <: name <: ub` (`name@0, lb@4, ub@8`).
pub fn typevar(name_sym: Offset, lb: Offset, ub: Offset) -> Offset {
    // Root the embedded references across the allocation.
    let _r0 = Rooted::new(object::Value(name_sym));
    let _r1 = Rooted::new(object::Value(lb));
    let _r2 = Rooted::new(object::Value(ub));
    let v = object::alloc(builtin(id::TVAR), 12).raw();
    write_ref(v, 0, name_sym);
    write_ref(v, 4, lb);
    write_ref(v, 8, ub);
    v
}

/// Allocate a `TypeVar` by interning `name`, with bounds `Union{} <: T <: Any`
/// unless given.
pub fn make_typevar(name: &str, lb: Offset, ub: Offset) -> Offset {
    let sym = symbol::intern(builtin(id::SYMBOL), name);
    typevar(sym, lb, ub)
}

/// Allocate a `UnionAll` `var . body` (`var@0, body@4`).
pub fn unionall_type(var: Offset, body: Offset) -> Offset {
    let _r0 = Rooted::new(object::Value(var));
    let _r1 = Rooted::new(object::Value(body));
    let v = object::alloc(builtin(id::UNIONALL), 8).raw();
    write_ref(v, 0, var);
    write_ref(v, 4, body);
    v
}

/// Whether the value at `t` is a `TypeVar`.
pub fn is_typevar(t: Offset) -> bool {
    object::type_of(object::Value(t)) == builtin(id::TVAR)
}

/// Whether the value at `t` is a `UnionAll`.
pub fn is_unionall(t: Offset) -> bool {
    object::type_of(object::Value(t)) == builtin(id::UNIONALL)
}

/// A `TypeVar`'s declared lower bound.
pub fn tvar_lb(v: Offset) -> Offset {
    read_ref(v, 4)
}

/// A `TypeVar`'s declared upper bound.
pub fn tvar_ub(v: Offset) -> Offset {
    read_ref(v, 8)
}

/// A `UnionAll`'s bound variable (a `TypeVar`).
pub fn unionall_var(u: Offset) -> Offset {
    read_ref(u, 0)
}

/// A `UnionAll`'s body type.
pub fn unionall_body(u: Offset) -> Offset {
    read_ref(u, 4)
}

// --- subtyping --------------------------------------------------------------

/// Subtyping `a <: b`. Delegates to the environment-based algorithm in
/// [`crate::subtype`] (a faithful core of `subtype.c`), which handles `Union`,
/// covariant tuples, nominal and invariant-parametric types, and the `where`
/// machinery (`UnionAll`/`TypeVar`) via the forall/exists rules.
pub fn issubtype(a: Offset, b: Offset) -> bool {
    crate::subtype::subtype(a, b)
}

/// Number of embedded reference fields an instance of `t` has (`npointers`).
pub fn layout_npointers(t: Offset) -> u32 {
    let l = unsafe { (*dt(t)).layout };
    if l == NULL {
        0
    } else {
        unsafe { *region::ptr_mut::<u32>(l) }
    }
}

/// Byte offset of the `i`-th embedded reference field of an instance of `t`.
pub fn layout_ptr_offset(t: Offset, i: u32) -> u32 {
    let l = unsafe { (*dt(t)).layout };
    unsafe { *region::ptr_mut::<u32>(l).add(1 + i as usize) }
}

/// Define a composite struct type with the given name, supertype, instance
/// size, and embedded reference-field byte offsets (its GC pointer bitmap).
#[allow(dead_code)] // used by tests and forthcoming user-defined types
pub fn define_struct(name: &str, super_: Offset, size: u32, ptr_offsets: &[u32]) -> Offset {
    let b = builtins();
    let name_sym = symbol::intern(b.types[id::SYMBOL as usize], name);
    // Root the TypeName across the DataType allocation.
    let tname = Rooted::new(object::Value(make_typename(b.types[id::TYPENAME as usize], name_sym)));
    new_type(b.types[id::DATATYPE as usize], tname.get().raw(), super_, size, 0, ptr_offsets)
}
