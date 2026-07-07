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
/// pointer bitmap built for `DataType` in [`bootstrap`]
/// (name@0, super@4, parameters@8, instance@12).
#[repr(C)]
pub struct DataType {
    /// The type's name (a `TypeName` reference, `jl_typename_t`).
    pub name: Offset,
    /// The supertype (a `DataType` reference); `Any`'s super is itself.
    pub super_: Offset,
    /// Type parameters (a `SimpleVector` reference, or `NULL` for non-parametric
    /// types). For tuple types these are the element types.
    pub parameters: Offset,
    /// Field types (`jl_datatype_t.types`): a `SimpleVector`, or `NULL` for
    /// types without declared fields.
    pub types: Offset,
    /// The singleton instance for singleton types (`jl_datatype_t.instance`),
    /// or `NULL`. `Nothing`'s instance is `nothing`.
    pub instance: Offset,
    /// Raw layout metadata (offset, or `NULL`): the pointer bitmap, followed
    /// by per-field descriptors for struct types (see [`field_offset`]).
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
    pub const VARARG: u32 = 32; // jl_vararg_t: the `Vararg{T}` tail of a tuple type
    pub const COUNT: usize = 33;
}

/// Offsets of the bootstrapped core types and the immortal value permboxes.
#[derive(Clone, Copy)]
pub struct Builtins {
    pub types: [Offset; id::COUNT],
    /// The `true` permbox (`jl_true`): the value every `Bool`-true box shares.
    pub true_instance: Offset,
    /// The `false` permbox (`jl_false`).
    pub false_instance: Offset,
    /// The `TypeName` shared by every tuple type (`jl_tuple_typename`).
    pub tuple_typename: Offset,
    /// The `TypeName` of the demo parametric constructor `Box{T}` (invariant).
    pub box_typename: Offset,
    /// The `TypeName` of the demo two-parameter constructor `Pair{A,B}` (invariant).
    pub pair_typename: Offset,
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

/// Region offset of the `nothing` singleton value (`Nothing`'s `instance`).
pub fn nothing_instance() -> Offset {
    instance_of(builtin(id::NOTHING))
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
        (*p).types = NULL;
        (*p).instance = NULL;
        (*p).layout = layout;
        (*p).size = size;
        (*p).nfields = 0;
        (*p).flags = flags;
    }
}

/// Byte offset of the `instance` field within a DataType body (see the GC
/// pointer bitmap in [`bootstrap`]).
const DT_INSTANCE: u32 = 16;

/// The singleton instance of `t` (`jl_datatype_t.instance`), or `NULL`.
pub fn instance_of(t: Offset) -> Offset {
    unsafe { (*dt(t)).instance }
}

/// Set `t`'s singleton instance (through the write barrier — `t` may be old).
fn set_instance(t: Offset, inst: Offset) {
    object::set_ref(object::Value(t), DT_INSTANCE, object::Value(inst));
}

/// Whether `t` is a singleton type (`jl_is_datatype_singleton`): a DataType
/// with an `instance`.
pub fn is_datatype_singleton(t: Offset) -> bool {
    is_datatype(t) && instance_of(t) != NULL
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
    reset_structs();
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
    //    parameters@8, types@12, instance@16; TypeName instances embed name@0
    //    (Symbol), cache@4, and field names@8 (the `mutabl` byte at 12 is not
    //    a reference).
    write_dt(datatype, tn("DataType"), any, NULL, make_layout(&[0, 4, 8, 12, 16]), DT_SIZE as u32, 0);
    write_dt(symbol, tn("Symbol"), any, NULL, NULL, 0, 0);
    write_dt(typename, tn("TypeName"), any, NULL, make_layout(&[0, 4, 8]), 16, 0);
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
    // `Vararg` (jl_vararg_t) is the type of a `Vararg{T}` object — the covariant
    // tail of a tuple type. We represent only the unbounded form (element T@0,
    // the count parameter N absent); `Vararg{T,N}` is not yet modelled.
    types[id::VARARG as usize] = new_type(datatype, tn("Vararg"), any, 4, 0, &[0]);

    // 6. The shared tuple TypeName: every Tuple{...} type has this `name`, which
    //    is how tuples are identified (jl_tuple_typename). `Box` is a demo
    //    parametric constructor (an invariant single-parameter type).
    let tuple_typename = tn("Tuple");
    let box_typename = tn("Box");
    let pair_typename = tn("Pair");

    // 7. The `nothing` singleton: the sole (zero-size) instance of Nothing,
    //    recorded in the type's `instance` field (jl_datatype_t.instance).
    let nothing = object::alloc(types[id::NOTHING as usize], 0).raw();
    set_instance(types[id::NOTHING as usize], nothing);

    // 8. The `true`/`false` permboxes (`jl_true`/`jl_false`, jl_init_box_caches):
    //    boxing a Bool returns one of these two immortal values, never a fresh
    //    allocation. They are values of Bool, not a singleton `instance` —
    //    Bool has two instances, so its `instance` field stays NULL.
    let mk_bool = |b: u8| {
        let v = object::alloc(types[id::BOOL as usize], 1);
        unsafe {
            *region::ptr_mut::<u8>(v.raw()) = b;
        }
        v.raw()
    };
    let false_instance = mk_bool(0);
    let true_instance = mk_bool(1);

    BUILTINS.0.set(Some(Builtins {
        types,
        true_instance,
        false_instance,
        tuple_typename,
        box_typename,
        pair_typename,
    }));
}

/// Allocate a `TypeName` object (`jl_typename_t` subset):
/// `[name (Symbol) @0 | cache @4 | field names (svec) @8 | mutabl (u8) @12]`.
fn make_typename(typename_type: Offset, name_sym: Offset) -> Offset {
    let v = object::alloc(typename_type, 16).raw();
    write_ref(v, 0, name_sym);
    write_ref(v, 4, NULL);
    write_ref(v, 8, NULL);
    unsafe {
        *region::ptr_mut::<u32>(v + 12) = 0;
    }
    v
}

/// Byte offsets within a TypeName.
const TN_NAMES: u32 = 8;
const TN_MUTABL: u32 = 12;

/// Whether `t`'s TypeName declares it `mutable struct` (`tn->mutabl`).
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn is_mutable(t: Offset) -> bool {
    unsafe { *region::ptr_mut::<u32>(name_of(t) + TN_MUTABL) != 0 }
}

/// The field-name Symbols of `t` (a svec on its TypeName, `tn->names`), or
/// `NULL`.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn field_names(t: Offset) -> Offset {
    read_ref(name_of(t), TN_NAMES)
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

/// Whether the DataType at `t` was declared `primitive type` (`isprimitivetype`).
pub fn is_primitive(t: Offset) -> bool {
    unsafe { (*dt(t)).flags & FLAG_PRIMITIVE != 0 }
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

/// Construct the demo two-parameter type `Pair{a, b}` (invariant), uniqued —
/// like `Box`, a stand-in for a nominal parametric type, with two invariant
/// parameters so the oracle can exercise multi-parameter `where` and diagonal
/// cases. Subtyping reuses the invariant-parametric path unchanged.
pub fn pair_type(a: Offset, b: Offset) -> Offset {
    let bi = builtins();
    apply_type(bi.pair_typename, bi.types[id::ANY as usize], &[a, b])
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

/// Canonical type ordering for union members (`union_sort_cmp`, `jltypes.c`):
/// singleton DataTypes first, then isbits DataTypes, then other DataTypes,
/// then non-DataTypes (UnionAlls compared by their unwrapped bodies) — ties
/// broken by [`name_cmp`].
fn type_cmp(a: Offset, b: Offset) -> core::cmp::Ordering {
    let (a_dt, b_dt) = (is_datatype(a), is_datatype(b));
    if a_dt != b_dt {
        return b_dt.cmp(&a_dt); // DataTypes sort before non-DataTypes
    }
    if !a_dt {
        return name_cmp(unwrap_unionall(a), unwrap_unionall(b));
    }
    let (a_s, b_s) = (is_datatype_singleton(a), is_datatype_singleton(b));
    if a_s != b_s {
        return b_s.cmp(&a_s); // singletons first
    }
    if !a_s {
        let (a_b, b_b) = (is_bits(a), is_bits(b));
        if a_b != b_b {
            return b_b.cmp(&a_b); // then isbits
        }
    }
    name_cmp(a, b)
}

/// Tie-breaking ordering, a faithful subset of `datatype_name_cmp`
/// (`jltypes.c`): DataTypes before non-DataTypes, then by type-name text,
/// parameter count, and parameters recursively. (Module qualification is
/// absent in this single-module model.)
fn name_cmp(a: Offset, b: Offset) -> core::cmp::Ordering {
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
        let ord = name_cmp(svec_ref(pa, i), svec_ref(pb, i));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Descend through `UnionAll` wrappers to the body (`jl_unwrap_unionall`).
fn unwrap_unionall(mut t: Offset) -> Offset {
    while is_unionall(t) {
        t = unionall_body(t);
    }
    t
}

/// Whether instances of `t` are plain bits (`jl_isbits`, faithful subset):
/// a concrete primitive, or a tuple all of whose element types are isbits.
/// Parametric constructors like `Box` hold references, so they are not.
fn is_bits(t: Offset) -> bool {
    if !is_datatype(t) || is_abstract(t) {
        return false;
    }
    if (unsafe { (*dt(t)).flags }) & FLAG_PRIMITIVE != 0 {
        return true;
    }
    if is_tuple(t) {
        let p = parameters_of(t);
        if p == NULL {
            return false;
        }
        return (0..svec_len(p)).all(|i| is_bits(svec_ref(p, i)));
    }
    false
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

/// A `TypeVar`'s name (a `Symbol`).
pub fn tvar_name(v: Offset) -> Offset {
    read_ref(v, 0)
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

/// Allocate an unbounded `Vararg{elem}` (`jl_vararg_t` with `N` absent): the
/// covariant element type at `@0`. Appears only as the last parameter of a tuple
/// type. Bounded `Vararg{T,N}` is not yet represented (`design/implementation.md`),
/// and — like Julia's `jl_wrap_vararg` results — these are not uniqued.
pub fn vararg_type(elem: Offset) -> Offset {
    let _r = Rooted::new(object::Value(elem));
    let v = object::alloc(builtin(id::VARARG), 4).raw();
    write_ref(v, 0, elem);
    v
}

/// Whether the value at `t` is a `Vararg` (`jl_is_vararg`).
pub fn is_vararg(t: Offset) -> bool {
    object::type_of(object::Value(t)) == builtin(id::VARARG)
}

/// The element type `T` of `Vararg{T}` (`jl_unwrap_vararg`).
pub fn vararg_elem(t: Offset) -> Offset {
    read_ref(t, 0)
}

// --- source-defined struct registry -------------------------------------------
//
// Struct types defined from source are reachable from nowhere until their
// first instance exists, so the registry roots them (the collector visits it)
// and gives the front-end name → type lookup. A REPL session re-evaluating
// its accumulated source reuses the identical definition, keeping type
// identity stable across entries.

struct StructRegistry(core::cell::UnsafeCell<Vec<(Offset, Offset)>>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for StructRegistry {}
static STRUCTS: StructRegistry = StructRegistry(core::cell::UnsafeCell::new(Vec::new()));

fn structs() -> &'static mut Vec<(Offset, Offset)> {
    unsafe { &mut *STRUCTS.0.get() }
}

/// Clear the registry (offsets into a region that is being reset).
fn reset_structs() {
    structs().clear();
}

/// Visit every registered struct type; the collector roots them.
pub fn each_registered_struct(mut f: impl FnMut(Offset)) {
    for &(_, t) in structs().iter() {
        f(t);
    }
}

/// The registered struct type named `name_sym`, or `None`.
pub fn lookup_struct(name_sym: Offset) -> Option<Offset> {
    structs().iter().rev().find(|&&(n, _)| n == name_sym).map(|&(_, t)| t)
}

/// Define a struct from source. An *identical* existing definition is reused;
/// a different one under the same name is an error, as in Julia.
pub fn define_struct_from_source(
    name: &str,
    fields: &[(&str, Offset)],
    mutabl: bool,
) -> Result<Offset, String> {
    let b = builtins();
    let name_sym = symbol::intern(b.types[id::SYMBOL as usize], name);
    if let Some(t) = lookup_struct(name_sym) {
        let names = field_names(t);
        let same = is_mutable(t) == mutabl
            && nfields_of(t) == fields.len() as u32
            && fields.iter().enumerate().all(|(i, &(fname, ft))| {
                svec_ref(names, i as u32) == symbol::intern(b.types[id::SYMBOL as usize], fname)
                    && field_type(t, i as u32) == ft
            });
        if same {
            return Ok(t);
        }
        return Err(format!("invalid redefinition of constant {}", name));
    }
    let t = define_struct(name, b.types[id::ANY as usize], fields, mutabl);
    structs().push((name_sym, t));
    Ok(t)
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

// --- struct types: field layout, construction, field access ------------------
//
// A faithful core of `jl_compute_field_offsets` (`datatype.c:636`),
// `jl_new_structv` (`datatype.c:1675`), `jl_get_nth_field` (`datatype.c:1854`),
// and `set_nth_field` (`datatype.c:1912`). A field whose declared type is a
// concrete, immutable, pointer-free type is stored **inline** (unboxed bits,
// re-boxed on read); every other field is a reference. Omitted relative to the
// C: inline isbits-union fields (selector bytes), inline immutables that
// themselves contain pointers (`first_ptr`/`hasptr`), atomics and field locks,
// `n_uninitialized`, and `#undef` checking — each arrives with the values that
// need it.

/// Per-field descriptor (a `jl_fielddesc32_t` without the bitfield packing).
struct FieldDesc {
    offset: u32,
    size: u32,
    isptr: bool,
}

/// Whether a field of declared type `ft` is stored inline
/// (pointer-free isbits: a concrete primitive or an immutable struct whose
/// own fields are all inline).
fn is_inline_field(ft: Offset) -> bool {
    if !is_datatype(ft) || is_abstract(ft) || is_mutable(ft) {
        return false;
    }
    if is_primitive(ft) {
        return true;
    }
    // An immutable struct is inlinable when it has a layout with no pointers.
    nfields_of(ft) > 0 && layout_npointers(ft) == 0 || instance_of(ft) != NULL
}

/// Alignment of an inline value of type `t` (`jl_datatype_align`): a
/// primitive aligns to its size (capped at 8, our `MAX_ALIGN`); a struct to
/// the max of its fields' alignments — **not** its size; a singleton to 1.
fn type_alignment(t: Offset) -> u32 {
    if is_primitive(t) {
        return size_of(t).clamp(1, 8);
    }
    let nf = nfields_of(t);
    if nf == 0 {
        return 1;
    }
    (0..nf)
        .map(|i| if field_isptr(t, i) { 4 } else { type_alignment(field_type(t, i)) })
        .max()
        .unwrap_or(1)
}

/// Compute offsets, sizes, pointer slots, and total size for the given field
/// types (`jl_compute_field_offsets`, `datatype.c:735–833`): each field is
/// placed at its type's alignment (`LLT_ALIGN(sz, al)`); references are
/// 4-byte offsets; the total size is padded to the struct's own alignment
/// (the max field alignment, `datatype.c:831`), so a nested inline copy of
/// `size_of` bytes is exact.
fn compute_field_offsets(field_types: &[Offset]) -> (u32, Vec<u32>, Vec<FieldDesc>) {
    let mut descs = Vec::with_capacity(field_types.len());
    let mut ptrs = Vec::new();
    let mut off: u32 = 0;
    let mut alignm: u32 = 1;
    for &ft in field_types {
        let (fsz, al, isptr) = if is_inline_field(ft) {
            (size_of(ft), type_alignment(ft), false)
        } else {
            (4, 4, true)
        };
        off = (off + al - 1) & !(al - 1);
        alignm = alignm.max(al);
        if isptr {
            ptrs.push(off);
        }
        descs.push(FieldDesc { offset: off, size: fsz, isptr });
        off += fsz;
    }
    off = (off + alignm - 1) & !(alignm - 1);
    (off, ptrs, descs)
}

/// Allocate a struct layout blob: the GC's `[npointers, ptr offsets...]`
/// prefix (unchanged shape — the collector reads only this), followed by
/// `nfields` descriptor triples `[offset, size, isptr]`.
fn make_struct_layout(ptrs: &[u32], descs: &[FieldDesc]) -> Offset {
    let words = 1 + ptrs.len() + 3 * descs.len();
    let off = region::alloc(4 * words);
    unsafe {
        let p = region::ptr_mut::<u32>(off);
        *p = ptrs.len() as u32;
        for (i, &po) in ptrs.iter().enumerate() {
            *p.add(1 + i) = po;
        }
        let base = 1 + ptrs.len();
        for (i, d) in descs.iter().enumerate() {
            *p.add(base + 3 * i) = d.offset;
            *p.add(base + 3 * i + 1) = d.size;
            *p.add(base + 3 * i + 2) = d.isptr as u32;
        }
    }
    off
}

/// Number of declared fields of `t`.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn nfields_of(t: Offset) -> u32 {
    unsafe { (*dt(t)).nfields }
}

/// The declared type of field `i` (`jl_field_type`).
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn field_type(t: Offset, i: u32) -> Offset {
    svec_ref(unsafe { (*dt(t)).types }, i)
}

/// Read the `i`-th field descriptor word triple from the layout blob.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
fn field_desc(t: Offset, i: u32) -> (u32, u32, bool) {
    let l = unsafe { (*dt(t)).layout };
    let base = 1 + layout_npointers(t) + 3 * i;
    unsafe {
        let p = region::ptr_mut::<u32>(l);
        (*p.add(base as usize), *p.add(base as usize + 1), *p.add(base as usize + 2) != 0)
    }
}

/// Byte offset of field `i` (`jl_field_offset`).
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn field_offset(t: Offset, i: u32) -> u32 {
    field_desc(t, i).0
}

/// Whether field `i` is a reference (`jl_field_isptr`).
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn field_isptr(t: Offset, i: u32) -> bool {
    field_desc(t, i).2
}

/// Index of the field named `name_sym`, by interned-symbol identity.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn field_index(t: Offset, name_sym: Offset) -> Option<u32> {
    let names = field_names(t);
    if names == NULL {
        return None;
    }
    (0..svec_len(names)).find(|&i| svec_ref(names, i) == name_sym)
}

/// `isa(v, t)` for our value universe: `typeof(v) <: t`.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn is_a(v: object::Value, t: Offset) -> bool {
    issubtype(object::type_of(v), t)
}

/// Construct an instance of struct type `t` from `args` (`jl_new_structv`):
/// arity and per-field `isa` checks, singleton return, inline fields stored
/// as bits, reference fields stored through [`set_nth_field`]'s barrier path,
/// uncovered tail zeroed by the allocator.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn new_struct(t: Offset, args: &[object::Value]) -> Result<object::Value, String> {
    if !is_datatype(t) || is_abstract(t) {
        return Err("new: not a concrete struct type".to_string());
    }
    let nf = nfields_of(t);
    if args.len() as u32 != nf {
        return Err(format!("invalid struct allocation: {} of {} fields", args.len(), nf));
    }
    for (i, &a) in args.iter().enumerate() {
        let ft = field_type(t, i as u32);
        if !is_a(a, ft) {
            return Err(format!("TypeError: new: expected field {} to match its declared type", i + 1));
        }
    }
    let inst = instance_of(t);
    if inst != NULL {
        return Ok(object::Value(inst));
    }
    // Root the arguments and the new object across allocation.
    let v = object::alloc(t, size_of(t) as usize);
    if v.is_null() {
        return Err("out of memory".to_string());
    }
    let v_root = Rooted::new(v);
    for (i, &a) in args.iter().enumerate() {
        set_field_raw(v_root.get(), t, i as u32, a);
    }
    Ok(v_root.get())
}

/// The unchecked store shared by construction and `setfield!`
/// (`set_nth_field`): references go through the write barrier; inline fields
/// copy payload bits.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
fn set_field_raw(v: object::Value, t: Offset, i: u32, rhs: object::Value) {
    let (offs, fsz, isptr) = field_desc(t, i);
    if isptr {
        object::set_ref(v, offs, rhs);
    } else {
        unsafe {
            core::ptr::copy_nonoverlapping(
                region::ptr_mut::<u8>(rhs.raw()),
                region::ptr_mut::<u8>(v.raw() + offs),
                fsz as usize,
            );
        }
    }
}

/// Read field `i` of `v` (`jl_get_nth_field`): reference fields load the
/// reference; inline fields re-box the bits as the field's declared type
/// (`jl_new_bits`).
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn get_nth_field(v: object::Value, i: u32) -> Result<object::Value, String> {
    let t = object::type_of(v);
    if i >= nfields_of(t) {
        return Err(format!("BoundsError: field index {}", i + 1));
    }
    let (offs, fsz, isptr) = field_desc(t, i);
    if isptr {
        return Ok(object::get_ref(v, offs));
    }
    let ft = field_type(t, i);
    let inst = instance_of(ft);
    if inst != NULL {
        return Ok(object::Value(inst)); // inline singleton field
    }
    // jl_new_bits: allocate a fresh box of the field type and copy the bits.
    let v_root = Rooted::new(v);
    let b = object::alloc(ft, fsz as usize);
    if b.is_null() {
        return Err("out of memory".to_string());
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            region::ptr_mut::<u8>(v_root.get().raw() + offs),
            region::ptr_mut::<u8>(b.raw()),
            fsz as usize,
        );
    }
    Ok(b)
}

/// `setfield!` (`jl_f_setfield` + `get_checked_fieldindex`): only mutable
/// structs may be assigned; the value must match the declared field type.
#[allow(dead_code)] // consumers arrive with struct slice 2 (front-end + interpreter)
pub fn set_nth_field(v: object::Value, i: u32, rhs: object::Value) -> Result<(), String> {
    let t = object::type_of(v);
    if !is_mutable(t) {
        return Err(format!(
            "setfield!: immutable struct of type {} cannot be changed",
            symbol::as_str(type_sym(t))
        ));
    }
    if i >= nfields_of(t) {
        return Err(format!("BoundsError: field index {}", i + 1));
    }
    let ft = field_type(t, i);
    if !is_a(rhs, ft) {
        return Err("TypeError: setfield!: value does not match the field type".to_string());
    }
    set_field_raw(v, t, i, rhs);
    Ok(())
}

/// Define a struct type (`jl_new_datatype` + `jl_compute_field_offsets`):
/// field names and types determine the inline/reference layout and the GC
/// pointer bitmap. A zero-field immutable struct is a singleton with an eager
/// `instance`.
#[allow(dead_code)] // used by tests; the front-end wires in with slice 2
pub fn define_struct(
    name: &str,
    super_: Offset,
    fields: &[(&str, Offset)],
    mutabl: bool,
) -> Offset {
    let b = builtins();
    let name_sym = symbol::intern(b.types[id::SYMBOL as usize], name);
    let tname = Rooted::new(object::Value(make_typename(b.types[id::TYPENAME as usize], name_sym)));
    unsafe {
        *region::ptr_mut::<u32>(tname.get().raw() + TN_MUTABL) = mutabl as u32;
    }

    let ftypes: Vec<Offset> = fields.iter().map(|&(_, ft)| ft).collect();
    let (size, ptrs, descs) = compute_field_offsets(&ftypes);

    // Field-name symbols (interned, immortal) and the field-type svec.
    let name_syms: Vec<Offset> = fields
        .iter()
        .map(|&(fname, _)| symbol::intern(b.types[id::SYMBOL as usize], fname))
        .collect();
    let names_svec = Rooted::new(object::Value(make_svec(&name_syms)));
    object::set_ref(tname.get(), TN_NAMES, names_svec.get());
    let types_svec = Rooted::new(object::Value(make_svec(&ftypes)));

    let layout = if descs.is_empty() && ptrs.is_empty() {
        NULL
    } else {
        make_struct_layout(&ptrs, &descs)
    };
    let v = object::alloc(b.types[id::DATATYPE as usize], DT_SIZE);
    let t = v.raw();
    write_dt(t, tname.get().raw(), super_, NULL, layout, size, 0);
    unsafe {
        (*dt(t)).types = types_svec.get().raw();
        (*dt(t)).nfields = fields.len() as u32;
    }
    if fields.is_empty() && !mutabl {
        let t_root = Rooted::new(object::Value(t));
        let inst = object::alloc(t, 0).raw();
        set_instance(t_root.get().raw(), inst);
    }
    t
}
