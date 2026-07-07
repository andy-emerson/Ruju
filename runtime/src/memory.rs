//! `GenericMemory{T}` — the flat GC-managed buffer under arrays.
//!
//! Faithful core of `genericmemory.c` / `jl_genericmemory_t` (`julia.h:166`):
//! the object body is `[length: u32 @0 | ptr: u32 @4]` followed by the element
//! data **inline in the region**, with `ptr` holding the data's region offset —
//! exactly the C's pooled shape, where `m->ptr` is set to point just past the
//! header even when the data is attached (`jl_alloc_genericmemory_unchecked`,
//! `genericmemory.c:41–52`). Keeping the buffer in linear memory in this layout
//! is the arrays carry-forward constraint from `design/roadmap.md`: element
//! access is `region[ptr + i*elsz]`, which compiled code can lower to a bounds
//! check plus a load/store.
//!
//! Elements are stored in one of two ways, after `layout->flags.arrayelem_isboxed`:
//! **boxed** (references, one `u32` offset per element; the GC traces them via
//! the typename special-case in `gc-stock.c:2412` and stores go through the
//! write barrier as `jl_memoryrefset` does, `genericmemory.c:463`), or **inline
//! bits** for primitive element types (`jl_memoryrefget` re-boxes on read via
//! `jl_new_bits`). Zero-size singleton elements (e.g. `Nothing`) occupy no
//! bytes and read back as the type's `instance` (`genericmemory.c:361–364`).
//!
//! Simplifications (recorded in `design/implementation.md`): data is always
//! inline — the C's separately-malloced (`MALLOCD`) and string-owned buffers
//! don't exist in the bounded region; inline storage is limited to *primitive*
//! isbits (isbits structs/tuples and isbits unions stay boxed); no atomics or
//! locks; and the zero-length singleton-instance optimization is absent (an
//! empty memory is an ordinary allocation).

use crate::gc;
use crate::object::{self, Value};
use crate::region::{self, Offset, NULL};
use crate::types;

/// Byte offsets of the header fields within the object body.
const LENGTH: u32 = 0;
const PTR: u32 = 4;
/// The element data starts just past the two header fields.
const DATA: u32 = 8;

/// Element storage for a memory's element type: boxed references, inline
/// primitive bits of the given size, or a zero-size singleton.
enum Elem {
    Boxed,
    Bits(u32),
    Singleton,
}

/// Classify element storage (`arrayelem_isboxed`): inline iff the element type
/// is primitive bits or a zero-size singleton; everything else is a reference.
fn elem_kind(elem: Offset) -> Elem {
    if types::is_datatype(elem) && types::is_bits(elem) && types::size_of(elem) > 0 {
        return Elem::Bits(types::size_of(elem));
    }
    if types::is_datatype(elem)
        && types::size_of(elem) == 0
        && types::instance_of(elem) != NULL
    {
        return Elem::Singleton;
    }
    Elem::Boxed
}

fn elem_size(elem: Offset) -> u32 {
    match elem_kind(elem) {
        Elem::Boxed => 4,
        Elem::Bits(n) => n,
        Elem::Singleton => 0,
    }
}

/// The element type of the memory value `m` (`jl_tparam1` of its type; ours has
/// the single parameter).
pub fn elem_type_of(m: Value) -> Offset {
    let t = object::type_of(m);
    types::svec_ref(types::parameters_of(t), 0)
}

/// Allocate `GenericMemory{elem}` of `len` elements, zero-initialized
/// (`_new_genericmemory_`, `genericmemory.c:56`): overflow-checked size, one
/// region object holding header and data, `ptr` set to the inline data.
pub fn alloc(elem: Offset, len: u32) -> Result<Value, String> {
    let _e = gc::Rooted::new(Value(elem)); // rooted across the type + object allocs
    let elsz = elem_size(elem);
    let nbytes = (len as usize)
        .checked_mul(elsz as usize)
        .filter(|&n| n < u32::MAX as usize - DATA as usize)
        .ok_or("invalid GenericMemory size: the number of elements is either negative or too large")?;
    let mtype = types::memory_type(elem); // uniqued; immortal via the pinned typename
    let m = object::alloc(mtype, DATA as usize + nbytes);
    if m.is_null() {
        return Err("out of memory".to_string());
    }
    unsafe {
        *region::ptr_mut::<u32>(m.raw() + LENGTH) = len;
        *region::ptr_mut::<u32>(m.raw() + PTR) = m.raw() + DATA;
        // Zero the element data (`memset`, `genericmemory.c:71–72`): a recycled
        // chunk carries stale bytes, and a boxed slot must read as unset — a
        // stale non-null slot would be traced as a reference by the GC.
        core::ptr::write_bytes(region::ptr_mut::<u8>(m.raw() + DATA), 0, nbytes);
    }
    Ok(m)
}

/// The memory's element count (`m->length`).
pub fn len(m: Value) -> u32 {
    unsafe { *region::ptr_mut::<u32>(m.raw() + LENGTH) }
}

/// The region offset of the element data (`m->ptr`).
fn data(m: Value) -> Offset {
    unsafe { *region::ptr_mut::<u32>(m.raw() + PTR) }
}

/// Read element `i` (`jl_memoryrefget`, `genericmemory.c:343`): boxed elements
/// load the reference (an unset slot is an `UndefRefError`); singleton elements
/// return the type's `instance`; bits elements re-box (`jl_new_bits`).
pub fn get(m: Value, i: u32) -> Result<Value, String> {
    if i >= len(m) {
        return Err(format!("BoundsError: memory index {}", i + 1));
    }
    let elem = elem_type_of(m);
    match elem_kind(elem) {
        Elem::Boxed => {
            let r = unsafe { *region::ptr_mut::<u32>(data(m) + 4 * i) };
            if r == NULL {
                return Err("UndefRefError: access to undefined reference".to_string());
            }
            Ok(Value(r))
        }
        Elem::Singleton => Ok(Value(types::instance_of(elem))),
        Elem::Bits(fsz) => {
            // Re-box the bits; the memory must survive the allocation.
            let m_root = gc::Rooted::new(m);
            let b = object::alloc(elem, fsz as usize);
            if b.is_null() {
                return Err("out of memory".to_string());
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    region::ptr_mut::<u8>(data(m_root.get()) + fsz * i),
                    region::ptr_mut::<u8>(b.raw()),
                    fsz as usize,
                );
            }
            Ok(b)
        }
    }
}

/// Write element `i` (`jl_memoryrefset`, `genericmemory.c:446`): the value must
/// be an instance of the element type; boxed stores go through the write
/// barrier on the memory object (`jl_gc_wb(owner, rhs)`, `:463`); bits stores
/// copy the payload.
pub fn set(m: Value, i: u32, v: Value) -> Result<(), String> {
    if i >= len(m) {
        return Err(format!("BoundsError: memory index {}", i + 1));
    }
    let elem = elem_type_of(m);
    let vt = object::type_of(v);
    if elem != types::builtin(types::id::ANY) && vt != elem && !types::issubtype(vt, elem) {
        return Err(format!(
            "TypeError: memoryrefset!: expected {}, got {}",
            crate::symbol::as_str(types::type_sym(elem)),
            crate::symbol::as_str(types::type_sym(vt)),
        ));
    }
    match elem_kind(elem) {
        Elem::Boxed => {
            gc::write_barrier(m, v);
            unsafe {
                *region::ptr_mut::<u32>(data(m) + 4 * i) = v.raw();
            }
        }
        Elem::Singleton => {} // no bytes to store
        Elem::Bits(fsz) => unsafe {
            core::ptr::copy_nonoverlapping(
                region::ptr_mut::<u8>(v.raw()),
                region::ptr_mut::<u8>(data(m) + fsz * i),
                fsz as usize,
            );
        },
    }
    Ok(())
}

/// Visit each element reference of a boxed-element memory — the mark-phase
/// analog of `gc_mark_objarray` over `m->ptr .. m->ptr + length`
/// (`gc-stock.c:2448–2456`). Bits and singleton memories hold no references.
pub fn each_element_ref(m: Value, mut f: impl FnMut(Value)) {
    if let Elem::Boxed = elem_kind(elem_type_of(m)) {
        let d = data(m);
        for i in 0..len(m) {
            f(Value(unsafe { *region::ptr_mut::<u32>(d + 4 * i) }));
        }
    }
}
