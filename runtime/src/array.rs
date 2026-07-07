//! `Array{T}` — the one-dimensional array over a [`GenericMemory`](crate::memory)
//! buffer.
//!
//! Faithful core of `array.c` / `jl_array_t` (`julia.h:190`): an array is a
//! small header over a memory — `{ref: {ptr_or_offset, mem}, dimsize}` — whose
//! buffer it replaces as it grows. The body here is
//! `[mem: u32 @0 | offset: u32 @4 | length: u32 @8]`: `mem` is the GC-traced
//! buffer reference, `offset` is the element offset of the array's first
//! element within the buffer (the C's `ptr_or_offset`, kept as an element
//! index the way the C itself does for isbits-union arrays), and `length` is
//! `dimsize[0]`.
//!
//! Growth ports `jl_array_grow_end` (`array.c:191–238`): when the buffer is
//! full, capacity follows the C's sequence — to 4, then ×1.5 below 48, then
//! ×1.2 — the live prefix is copied into the new buffer, and the `mem` field
//! is swapped **through the write barrier** (`jl_gc_wb(a, newmem)`, `:231`).
//! `del_end` (`:240–255`) zeroes the deleted tail so boxed slots stop being
//! traced. `push` is `jl_array_ptr_1d_push` (`:257`) generalized to any
//! element type (the C's is `Any`-only; Julia's generic `push!` lives in
//! `base/`, which we don't run yet).
//!
//! Simplifications (recorded in `design/implementation.md`): one-dimensional
//! only (`Array{T}`, `N` fixed 1); `offset` is always 0 until `popfirst!`/
//! `deleteat!` arrive; no shared-buffer views (`jl_array_isshared`).

use crate::errors;
use crate::gc;
use crate::memory;
use crate::object::{self, Value};
use crate::region::{self, Offset};
use crate::types;

/// Byte offsets of the fields within the object body.
const MEM: u32 = 0;
const OFFSET: u32 = 4;
const LENGTH: u32 = 8;
const BODY: usize = 12;

/// The buffer of `a` (`a->ref.mem`).
pub fn mem_of(a: Value) -> Value {
    object::get_ref(a, MEM)
}

fn offset_of(a: Value) -> u32 {
    unsafe { *region::ptr_mut::<u32>(a.raw() + OFFSET) }
}

/// The array's element count (`jl_array_nrows`, `dimsize[0]`).
pub fn len(a: Value) -> u32 {
    unsafe { *region::ptr_mut::<u32>(a.raw() + LENGTH) }
}

fn set_len(a: Value, n: u32) {
    unsafe {
        *region::ptr_mut::<u32>(a.raw() + LENGTH) = n;
    }
}

/// The array's element type (the single parameter of `Array{T}`).
pub fn elem_type_of(a: Value) -> Offset {
    types::svec_ref(types::parameters_of(object::type_of(a)), 0)
}

/// Allocate `Array{elem}` of `len` elements over a fresh zeroed buffer
/// (`jl_alloc_array_1d` → `_new_array`).
pub fn alloc_1d(elem: Offset, len: u32) -> Result<Value, Value> {
    let _e = gc::Rooted::new(Value(elem));
    let atype = types::array_type(elem); // uniqued; immortal via the pinned typename
    let m = memory::alloc(elem, len)?;
    let _m = gc::Rooted::new(m); // buffer survives the array-object allocation
    let a = object::alloc(atype, BODY);
    if a.is_null() {
        return Err(errors::out_of_memory());
    }
    unsafe {
        *region::ptr_mut::<u32>(a.raw() + MEM) = m.raw();
        *region::ptr_mut::<u32>(a.raw() + OFFSET) = 0;
        *region::ptr_mut::<u32>(a.raw() + LENGTH) = len;
    }
    Ok(a)
}

/// Read element `i` (0-based; `jl_arrayref` over `memoryrefget`). Bounds are
/// the *array's*, which may be shorter than its buffer.
pub fn aref(a: Value, i: u32) -> Result<Value, Value> {
    if i >= len(a) {
        return Err(errors::bounds_error(a, i as i64 + 1));
    }
    memory::get(mem_of(a), offset_of(a) + i)
}

/// Write element `i` (0-based; `jl_arrayset` over `memoryrefset`).
pub fn aset(a: Value, i: u32, v: Value) -> Result<(), Value> {
    if i >= len(a) {
        return Err(errors::bounds_error(a, i as i64 + 1));
    }
    memory::set(mem_of(a), offset_of(a) + i, v)
}

/// Grow the array by `inc` elements (`jl_array_grow_end`, `array.c:191–238`).
/// When the buffer is exhausted, capacity follows the C's growth sequence and
/// the live prefix moves to a fresh buffer, swapped in through the write
/// barrier.
pub fn grow_end(a: Value, inc: u32) -> Result<(), Value> {
    let n = len(a);
    let newnrows = n
        .checked_add(inc)
        .ok_or_else(|| errors::error_exception("invalid Array size: too large for system address width"))?;
    let reqmaxsize = offset_of(a) as u64 + newnrows as u64;
    let oldmaxsize = memory::len(mem_of(a)) as u64;
    if reqmaxsize > oldmaxsize {
        // The C's capacity sequence: 4, then grow by 50% below 48, then by 20%.
        let mut newmaxsize = if oldmaxsize < 4 {
            4
        } else if oldmaxsize < 48 {
            oldmaxsize * 3 / 2
        } else {
            oldmaxsize * 6 / 5
        };
        if newmaxsize < reqmaxsize {
            newmaxsize = reqmaxsize;
        }
        let newmaxsize =
            u32::try_from(newmaxsize)
                .map_err(|_| errors::error_exception("invalid Array size: too large"))?;
        let _a = gc::Rooted::new(a); // the array (and via it, the old buffer)
        let newmem = memory::alloc(elem_type_of(a), newmaxsize)?;
        memory::copy_prefix(newmem, mem_of(_a.get()), n);
        // Swap the buffer in through the barrier (`jl_gc_wb(a, newmem)`).
        object::set_ref(_a.get(), MEM, newmem);
    }
    set_len(a, newnrows);
    Ok(())
}

/// Shrink by `dec` elements (`jl_array_del_end`, `array.c:240–255`), zeroing
/// the deleted tail so boxed slots read as unset and stop being traced.
pub fn del_end(a: Value, dec: u32) -> Result<(), Value> {
    let n = len(a);
    if n < dec {
        return Err(errors::bounds_error(a, 0));
    }
    let n = n - dec;
    set_len(a, n);
    let off = offset_of(a);
    memory::zero_range(mem_of(a), off + n, off + n + dec);
    Ok(())
}

/// Append one element (`jl_array_ptr_1d_push`, `array.c:257`, generalized to
/// any element type): grow by one, then store.
pub fn push(a: Value, v: Value) -> Result<(), Value> {
    let _a = gc::Rooted::new(a);
    let _v = gc::Rooted::new(v); // survives the growth reallocation
    grow_end(_a.get(), 1)?;
    aset(_a.get(), len(_a.get()) - 1, _v.get())
}
