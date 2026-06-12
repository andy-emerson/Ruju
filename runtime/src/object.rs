//! The tagged-value object model.
//!
//! Faithful adaptation of `jl_taggedvalue_t` from `src/julia.h`. In Julia every
//! heap object is preceded by a one-word header holding the object's type
//! pointer, with the low 4 bits reserved (2 for the GC mark state, 2 for an
//! in-image flag); `jl_typetagof` recovers the type by masking those bits.
//!
//! Ruju adapts this to its bounded region: the header stores the type's
//! region **offset** rather than a native address (keeping the no-hardcoded-
//! addresses discipline), with the low 2 bits reserved for the GC mark state.
//! There is no system image yet, so the in-image bits are unused. A [`Value`] is
//! the offset of an object's data — the byte just past its header — mirroring a
//! Julia `jl_value_t*`.

use crate::gc;
use crate::region::{self, Offset, NULL};
use crate::types;

/// Size of the object header in bytes (one machine word; keeps the following
/// data 8-byte aligned in the 32-bit wasm region).
pub const HEADER_SIZE: usize = 8;

/// Allocation alignment for object data, in bytes.
const ALIGN: usize = 8;

/// Low header bits reserved for GC state; the remaining bits are the type offset.
const GC_MASK: u32 = 0b11;

#[inline]
fn align_up(n: usize) -> usize {
    (n + (ALIGN - 1)) & !(ALIGN - 1)
}

/// A reference to a heap value: the region offset of its data (`jl_value_t*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Value(pub Offset);

impl Value {
    /// The null reference.
    pub const NULL: Value = Value(NULL);

    /// Whether this value is null (e.g. an allocation that failed).
    pub fn is_null(self) -> bool {
        self.0 == NULL
    }

    /// The underlying region offset.
    pub fn raw(self) -> Offset {
        self.0
    }
}

/// Pointer to the header word, which sits `HEADER_SIZE` bytes before the value.
fn header_ptr(v: Value) -> *mut u32 {
    region::ptr_mut::<u32>(v.0 - HEADER_SIZE as u32)
}

/// The type of `v`, as the region offset of its DataType object (`jl_typeof`:
/// the header with the low GC bits masked off).
pub fn type_of(v: Value) -> Offset {
    unsafe { *header_ptr(v) & !GC_MASK }
}

/// Point `v`'s header at type offset `type_off`, preserving its GC bits. Used to
/// close the self-referential `DataType : DataType` cycle during bootstrap
/// (mirrors `jl_set_typeof`).
pub fn set_type(v: Value, type_off: Offset) {
    unsafe {
        let p = header_ptr(v);
        *p = (type_off & !GC_MASK) | (*p & GC_MASK);
    }
}

/// The GC mark state stored in the header's low bits.
pub fn gc_bits(v: Value) -> u32 {
    unsafe { *header_ptr(v) & GC_MASK }
}

/// Set the GC mark state in the header's low bits.
pub fn set_gc_bits(v: Value, bits: u32) {
    unsafe {
        let p = header_ptr(v);
        *p = (*p & !GC_MASK) | (bits & GC_MASK);
    }
}

/// Allocate a tagged object of type `type_off` with `size` bytes of
/// (zero-initialized) data, returning a reference to the data, or [`Value::NULL`]
/// if the region is exhausted. Mirrors `jl_gc_alloc`. The chunk is obtained from
/// the collector's free list when possible, and the object is registered so the
/// collector can sweep it.
pub fn alloc(type_off: Offset, size: usize) -> Value {
    gc::maybe_collect(); // proactive heap-target trigger (gc-stock.c:356)
    let total = (HEADER_SIZE + align_up(size)) as u32;
    let mut header_off = gc::alloc_chunk(total);
    if header_off == NULL && types::is_bootstrapped() {
        // Under allocation pressure, collect and retry — Julia's behavior. Safe
        // at any allocation point because the live set is precisely rooted (the
        // shadow stack, the pinned builtins, and immortal symbols). Try a cheap
        // minor collection first, then escalate to a full one.
        gc::collect();
        header_off = gc::alloc_chunk(total);
        if header_off == NULL {
            gc::collect_full();
            header_off = gc::alloc_chunk(total);
        }
    }
    if header_off == NULL {
        return Value::NULL;
    }
    let value_off = header_off + HEADER_SIZE as u32;
    unsafe {
        *region::ptr_mut::<u32>(header_off) = type_off & !GC_MASK;
    }
    Value(value_off)
}

/// A typed pointer to a value's data.
pub(crate) fn data_ptr<T>(v: Value) -> *mut T {
    region::ptr_mut::<T>(v.0)
}

/// Read the reference field at byte `offset` within `v`'s data.
pub fn get_ref(v: Value, offset: u32) -> Value {
    Value(unsafe { *region::ptr_mut::<u32>(v.0 + offset) })
}

/// Write the reference field at byte `offset` within `v`'s data, recording the
/// store with the GC write barrier first (in case `v` is old and `r` is young).
#[allow(dead_code)] // consumed by composites and the collector
pub fn set_ref(v: Value, offset: u32, r: Value) {
    gc::write_barrier(v, r);
    unsafe {
        *region::ptr_mut::<u32>(v.0 + offset) = r.0;
    }
}
