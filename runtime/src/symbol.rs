//! Interned symbols.
//!
//! A `Symbol` is a tagged heap object of type `Symbol` whose body is its byte
//! length followed by its UTF-8 text, mirroring Julia's interned `jl_sym_t`.
//! Interning is tracked in a runtime-side table so equal names share a single
//! Symbol object (pointer identity, as Julia relies on). The table is cleared
//! whenever the region is reset.

use core::cell::UnsafeCell;

use crate::object::{self, Value};
use crate::region::Offset;

#[repr(C)]
struct SymHeader {
    len: u32,
}

struct Interns(UnsafeCell<Vec<(String, Offset)>>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Interns {}
static INTERNS: Interns = Interns(UnsafeCell::new(Vec::new()));

fn table() -> &'static mut Vec<(String, Offset)> {
    unsafe { &mut *INTERNS.0.get() }
}

/// Drop all interned entries (offsets into a region that is being reset).
pub fn reset() {
    table().clear();
}

/// Intern `s` as a Symbol of type `symbol_type`, returning its region offset.
/// Returns the existing Symbol if one with the same text was already interned.
pub fn intern(symbol_type: Offset, s: &str) -> Offset {
    for (k, off) in table().iter() {
        if k == s {
            return *off;
        }
    }
    let bytes = s.as_bytes();
    let v = object::alloc(symbol_type, core::mem::size_of::<SymHeader>() + bytes.len());
    unsafe {
        (*object::data_ptr::<SymHeader>(v)).len = bytes.len() as u32;
        let data = object::data_ptr::<u8>(v).add(core::mem::size_of::<SymHeader>());
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
    }
    table().push((s.to_string(), v.raw()));
    v.raw()
}

/// The byte length of the symbol at offset `sym`.
pub fn len(sym: Offset) -> u32 {
    unsafe { (*object::data_ptr::<SymHeader>(Value(sym))).len }
}

/// The symbol's UTF-8 text. Borrows the region-backed bytes; valid until the
/// region is reset.
pub fn as_str(sym: Offset) -> &'static str {
    let n = len(sym) as usize;
    unsafe {
        let data = object::data_ptr::<u8>(Value(sym)).add(core::mem::size_of::<SymHeader>());
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(data, n))
    }
}

/// Visit every interned symbol offset. Symbols are immortal in Julia (a global
/// interned table, never collected), so the GC roots all of them.
pub fn each_interned(mut f: impl FnMut(Offset)) {
    for (_, off) in table().iter() {
        f(*off);
    }
}
