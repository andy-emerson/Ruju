//! Modules and global bindings — the faithful core of `module.c`.
//!
//! A module (`jl_module_t`, `julia.h`) is `{name, parent, bindings}` here: the
//! C's further fields (the `bindingkeyset` hash index, usings, world-age
//! binding partitions, uuids) are omitted with the features that need them.
//! The C stores bindings in an svec of `jl_binding_t` objects looked up
//! through a hash keyset; ours is an `Array{Any}` of `[symbol, value, ...]`
//! pairs with a linear scan — same reachability shape (the module traces the
//! table, the table traces the values), simpler index (recorded).
//!
//! `get_global`/`set_global` follow `jl_get_global` (`module.c:1664`) and
//! `jl_set_global` (`:1670`) minus world age and constness: assignment
//! replaces the value (through the memory write barrier) or appends a new
//! binding. The `Main` module is created at init (`jl_new_module`,
//! `module.c:674` — self-parented like Julia's `Main`) and pinned as a GC
//! root.

use core::cell::Cell;

use crate::array;
use crate::gc;
use crate::object::{self, Value};
use crate::region::{Offset, NULL};
use crate::types::{self, id};

/// Byte offsets within a module body (all three are GC references, matching
/// the layout bitmap registered for `Module` in `types::bootstrap`).
const NAME: u32 = 0;
const PARENT: u32 = 4;
const BINDINGS: u32 = 8;
pub(crate) const BODY: usize = 12;

struct MainSlot(Cell<Offset>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for MainSlot {}
static MAIN: MainSlot = MainSlot(Cell::new(NULL));

/// Create a module named by the interned symbol `name_sym`. A `NULL` parent
/// self-parents the module, as Julia does for `Main`.
pub fn new_module(name_sym: Offset, parent: Offset) -> Result<Value, Value> {
    let _n = gc::Rooted::new(Value(name_sym));
    let bindings = array::alloc_1d(types::builtin(id::ANY), 0)?;
    let _b = gc::Rooted::new(bindings);
    let m = object::alloc(types::builtin(id::MODULE), BODY);
    if m.is_null() {
        return Err(crate::errors::out_of_memory());
    }
    unsafe {
        *crate::region::ptr_mut::<u32>(m.raw() + NAME) = name_sym;
        *crate::region::ptr_mut::<u32>(m.raw() + PARENT) =
            if parent == NULL { m.raw() } else { parent };
        *crate::region::ptr_mut::<u32>(m.raw() + BINDINGS) = bindings.raw();
    }
    Ok(m)
}

/// Create and pin the `Main` module (called from runtime init).
pub fn init_main() {
    let sym = crate::symbol::intern(types::builtin(id::SYMBOL), "Main");
    let m = new_module(sym, NULL).expect("Main module allocation");
    MAIN.0.set(m.raw());
}

/// The `Main` module, or `NULL` before init — the GC pins it as a root.
pub fn main_offset() -> Offset {
    MAIN.0.get()
}

fn bindings_of(m: Value) -> Value {
    object::get_ref(m, BINDINGS)
}

/// Look up the global `sym` in `m` (`jl_get_global`): the bound value, or
/// `None` for an unbound name.
pub fn get_global(m: Value, sym: Offset) -> Option<Value> {
    let b = bindings_of(m);
    let n = array::len(b);
    let mut i = 0;
    while i + 1 < n {
        if array::aref(b, i).ok()?.raw() == sym {
            return array::aref(b, i + 1).ok();
        }
        i += 2;
    }
    None
}

/// Bind or assign the global `sym` in `m` (`jl_set_global`): replace an
/// existing binding's value (the store goes through the write barrier in
/// `memory::set`) or append a new `[sym, value]` pair.
pub fn set_global(m: Value, sym: Offset, v: Value) -> Result<(), Value> {
    let _m = gc::Rooted::new(m);
    let _v = gc::Rooted::new(v);
    let b = bindings_of(m);
    let n = array::len(b);
    let mut i = 0;
    while i < n {
        if array::aref(b, i)?.raw() == sym {
            return array::aset(b, i + 1, v);
        }
        i += 2;
    }
    array::push(b, Value(sym))?;
    array::push(bindings_of(_m.get()), _v.get())
}
