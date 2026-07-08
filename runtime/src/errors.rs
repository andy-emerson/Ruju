//! Exception values — the runtime side of `rtutils.c`.
//!
//! Julia's runtime throws heap values (`jl_throw`); the error helpers here
//! construct them: `DivideError`/`UndefRefError`/`OutOfMemoryError` return the
//! bootstrap singletons (boot.jl declares them fieldless), `BoundsError`
//! carries the container and the (1-based) index as its `a`/`i` fields
//! (`jl_bounds_error_int`, `rtutils.c:222`), and `ErrorException` carries a
//! message — an interned `Symbol` until a `String` type exists (a recorded
//! adaptation; interning unique messages pins them immortally, acceptable for
//! the bootstrap front-end's error volume).

use crate::gc::Rooted;
use crate::object::{self, Value};
use crate::types::{self, id};

// --- the exception stack (`jl_excstack_t`) ----------------------------------
//
// Julia keeps a per-task stack of the exceptions currently being handled:
// a throw landing in a handler pushes (`throw_internal`), `Expr(:enter)`
// captures the depth (`jl_excstack_state`, `rtutils.c:365`), leaving a catch
// scope truncates back to it (`jl_restore_excstack`, `:371`), and the
// current exception is the top or `nothing` (`jl_current_exception`,
// `jlapi.c:179–184`). Single-task, so one global stack is the task's.
// The C stores (exception, backtrace) pairs; we have no backtraces yet
// (recorded). Entries are GC roots (the collector walks them).

struct ExcStack(core::cell::UnsafeCell<Vec<Value>>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for ExcStack {}
static EXC_STACK: ExcStack = ExcStack(core::cell::UnsafeCell::new(Vec::new()));

fn exc_stack() -> &'static mut Vec<Value> {
    unsafe { &mut *EXC_STACK.0.get() }
}

/// Push a caught exception (the landing half of `throw_internal`).
pub fn exc_push(v: Value) {
    exc_stack().push(v);
}

/// The current exception: the stack top, or `nothing` outside any catch
/// (`jl_current_exception`).
pub fn exc_current() -> Value {
    exc_stack().last().copied().unwrap_or(Value(types::nothing_instance()))
}

/// The current stack depth (`jl_excstack_state`) — captured by `Enter`.
pub fn exc_state() -> usize {
    exc_stack().len()
}

/// Truncate back to a captured depth (`jl_restore_excstack`) — the
/// `:pop_exception` statement leaving a catch scope.
pub fn exc_restore(state: usize) {
    let s = exc_stack();
    debug_assert!(s.len() >= state);
    s.truncate(state);
}

/// Clear the stack (the region is being reset).
pub fn exc_reset() {
    exc_stack().clear();
}

/// Visit every in-flight exception; the collector roots them.
pub fn each_exception(mut f: impl FnMut(Value)) {
    for &v in exc_stack().iter() {
        f(v);
    }
}

/// The `DivideError()` singleton (`jl_diverror_exception`).
pub fn divide_error() -> Value {
    Value(types::instance_of(types::builtin(id::DIVIDEERROR)))
}

/// The `UndefRefError()` singleton (`jl_undefref_exception`).
pub fn undef_ref_error() -> Value {
    Value(types::instance_of(types::builtin(id::UNDEFREFERROR)))
}

/// The `OutOfMemoryError()` singleton.
pub fn out_of_memory() -> Value {
    Value(types::instance_of(types::builtin(id::OUTOFMEMORYERROR)))
}

/// `BoundsError(a, i)` with the container and a boxed 1-based index
/// (`jl_bounds_error_int`). Falls back to the out-of-memory singleton if the
/// exception itself cannot be allocated.
pub fn bounds_error(a: Value, i: i64) -> Value {
    let _ra = Rooted::new(a);
    let boxed = crate::value::box_int(i);
    let _ri = Rooted::new(boxed);
    let e = object::alloc(types::builtin(id::BOUNDSERROR), 8);
    if e.is_null() {
        return out_of_memory();
    }
    object::set_ref(e, 0, _ra.get());
    object::set_ref(e, 4, _ri.get());
    e
}

/// `ErrorException(msg)` over an interned message Symbol (`jl_error`).
pub fn error_exception(msg: &str) -> Value {
    let sym = crate::symbol::intern(types::builtin(id::SYMBOL), msg);
    let _rs = Rooted::new(Value(sym));
    let e = object::alloc(types::builtin(id::ERROREXCEPTION), 4);
    if e.is_null() {
        return out_of_memory();
    }
    object::set_ref(e, 0, _rs.get());
    e
}

/// Adapter for runtime layers that still report `String` errors: wrap the
/// message as an `ErrorException` value.
pub fn wrap_msg(msg: String) -> Value {
    error_exception(&msg)
}

/// Render an uncaught exception for the host boundary (the embedding surface
/// formats; inside the runtime exceptions travel as values).
pub fn format(exc: Value) -> String {
    let t = object::type_of(exc);
    let name = crate::symbol::as_str(types::type_sym(t)).to_string();
    if t == types::builtin(id::ERROREXCEPTION) {
        let msg = object::get_ref(exc, 0);
        return format!("{}: {}", name, crate::symbol::as_str(msg.raw()));
    }
    if t == types::builtin(id::BOUNDSERROR) {
        let i = object::get_ref(exc, 4);
        if !i.is_null() && object::type_of(i) == types::builtin(id::INT64) {
            return format!("{}: attempt to access at index [{}]", name, crate::value::unbox_int(i));
        }
    }
    name
}
