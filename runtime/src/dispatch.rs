//! Multiple dispatch.
//!
//! A generic function is identified by a small id; each has a set of methods,
//! where a method pairs a signature (a tuple type) with a lowered-IR body.
//! Dispatch computes the argument types, selects the **applicable** methods
//! (`Tuple{argtypes...} <: sig`), and picks the **most specific** (the one whose
//! signature is a subtype of every other applicable signature), then evaluates
//! its body with the arguments bound to the leading slots. This mirrors Julia's
//! applicability-and-specificity selection over `Tuple` signatures.
//!
//! Scope: specificity is decided by subtyping, which is correct for the
//! non-parametric, nominal cases here; Julia's full `type_morespecific`
//! (`Type{T}`, varargs, the diagonal rule) is deferred with the rest of
//! `subtype.c`. Methods and their IR bodies live in a runtime-side table for
//! now, consistent with IR still being hand-built; their tuple signatures are
//! heap objects and are rooted by the collector.

use core::cell::UnsafeCell;

use crate::interp::{self, Body};
use crate::object::{self, Value};
use crate::region::Offset;
use crate::types;

struct Entry {
    func: u32,
    sig: Offset, // a tuple type
    body: Body,
    /// A compiled method's boxed entry point (the pin's `CodeInstance.invoke`
    /// analog): a funcref-table index with signature `(argv, nargs) -> ret`,
    /// argv a rooted slice of boxed-value offsets in linear memory. When
    /// present, dispatch calls it instead of interpreting `body`.
    fptr1: Option<u32>,
}

struct Table(UnsafeCell<Vec<Entry>>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Table {}
static TABLE: Table = Table(UnsafeCell::new(Vec::new()));

fn table() -> &'static mut Vec<Entry> {
    unsafe { &mut *TABLE.0.get() }
}

/// What a callable value's type resolves to: a generic function's method
/// table (by id), or a native builtin (`jl_f_*` — Julia's `Core` functions
/// are C builtins, not generic functions; ours are Rust fns).
#[derive(Clone, Copy)]
pub enum FnKind {
    Generic(u32),
    Native(fn(&[Value]) -> Result<Value, Value>),
}

/// Function values: each function is a zero-size singleton whose *type*
/// identifies it — `jl_apply_generic` dispatches on `typeof(f)`, and this
/// registry is the (type → callable) half of what Julia hangs off the
/// type's method table.
struct Functions(UnsafeCell<Vec<(Offset, FnKind)>>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Functions {}
static FUNCTIONS: Functions = Functions(UnsafeCell::new(Vec::new()));

fn functions() -> &'static mut Vec<(Offset, FnKind)> {
    unsafe { &mut *FUNCTIONS.0.get() }
}

/// Runtime-allocated generic-function ids, kept far above the small ids
/// hand-picked by tests and the bootstrap front-end.
struct NextFunc(core::cell::Cell<u32>);
unsafe impl Sync for NextFunc {}
static NEXT_FUNC: NextFunc = NextFunc(core::cell::Cell::new(1 << 20));

/// A fresh generic-function id (for `:method`-declared functions).
pub fn fresh_func_id() -> u32 {
    let id = NEXT_FUNC.0.get();
    NEXT_FUNC.0.set(id + 1);
    id
}

/// Clear all methods and function values (called when the runtime resets).
pub fn reset() {
    table().clear();
    functions().clear();
    NEXT_FUNC.0.set(1 << 20);
}

/// Create the callable value for generic function `func` (the shape of
/// `jl_new_generic_function`): a fresh zero-size immutable type under the
/// abstract `Function`, whose eager singleton instance *is* the function
/// value. Dispatch keys off that type.
pub fn make_function(name: &str, func: u32) -> Value {
    let t = types::define_struct(name, types::builtin(types::id::FUNCTION), &[], false);
    functions().push((t, FnKind::Generic(func)));
    Value(types::instance_of(t))
}

/// Create a native builtin function value (the `jl_f_*` registration shape:
/// a singleton under `Function` whose calls run a Rust fn directly).
pub fn make_native_function(name: &str, f: fn(&[Value]) -> Result<Value, Value>) -> Value {
    let t = types::define_struct(name, types::builtin(types::id::FUNCTION), &[], false);
    functions().push((t, FnKind::Native(f)));
    Value(types::instance_of(t))
}

/// What a callable value resolves to, by its type (`typeof(f)`, as
/// `jl_apply_generic` keys its method-table lookup), or `None` if the value
/// is not a registered function.
pub fn callable_of(v: Value) -> Option<FnKind> {
    let t = object::type_of(v);
    functions().iter().rev().find(|&&(ft, _)| ft == t).map(|&(_, k)| k)
}

/// The generic-function id of a callable value (`None` for natives and
/// non-functions) — the `:method` definition path needs the method table.
pub fn func_of(v: Value) -> Option<u32> {
    match callable_of(v) {
        Some(FnKind::Generic(f)) => Some(f),
        _ => None,
    }
}

/// Visit every function singleton type; the collector roots them (their
/// instances are reachable through the types' `instance` fields).
pub fn each_function(mut f: impl FnMut(Offset)) {
    for &(t, _) in functions().iter() {
        f(t);
    }
}

/// Register a method: function `func`, signature tuple type `sig`, body `body`.
pub fn add_method(func: u32, sig: Offset, body: Body) {
    table().push(Entry { func, sig, body, fptr1: None });
}

/// Register a **compiled** method: as [`add_method`], but the body is a
/// funcref-table index (the boxed fptr1 entry emitted by `ruju-aotc`) instead
/// of interpretable IR. Selection is unchanged — one method table serves both
/// execution fronts.
pub fn add_compiled_method(func: u32, sig: Offset, fptr1: u32) {
    table().push(Entry { func, sig, body: Body { nslots: 0, code: Vec::new() }, fptr1: Some(fptr1) });
}

/// Visit every method signature (a tuple type); the collector roots them.
pub fn each_sig(mut f: impl FnMut(Offset)) {
    for e in table().iter() {
        f(e.sig);
    }
}

/// Select the most-specific applicable method for `func` given the argument
/// types, returning its table index, or `None` if none applies.
fn select(func: u32, arg_types: &[Offset]) -> Option<usize> {
    let argtuple = types::tuple_type(arg_types);

    // Applicable methods, as (index, signature). Collected eagerly so the table
    // is not borrowed across the specificity check.
    let applicable: Vec<(usize, Offset)> = table()
        .iter()
        .enumerate()
        .filter(|(_, e)| e.func == func && types::issubtype(argtuple, e.sig))
        .map(|(i, e)| (i, e.sig))
        .collect();

    // Most specific: a signature that is a subtype of every other applicable one.
    for &(i, si) in &applicable {
        if applicable.iter().all(|&(_, sj)| types::issubtype(si, sj)) {
            return Some(i);
        }
    }
    // Ambiguous (no unique most-specific): Phase-0 falls back to the first match.
    applicable.first().map(|&(i, _)| i)
}

/// Dispatch `func` on `args` (by their runtime types) and evaluate the chosen
/// method body. Returns `Ok(Value::NULL)` if no method applies (a MethodError
/// in Julia); `Err` carries an exception value thrown by the body. The caller
/// must keep `args` rooted across this call.
pub fn invoke(func: u32, args: &[Value]) -> Result<Value, Value> {
    let arg_types: Vec<Offset> = args.iter().map(|&v| object::type_of(v)).collect();
    let i = match select(func, &arg_types) {
        Some(i) => i,
        None => return Ok(Value::NULL),
    };
    if let Some(fptr1) = table()[i].fptr1 {
        return call_compiled(fptr1, args);
    }
    let body = table()[i].body.clone();
    interp::eval_with_args(&body, args)
}

/// Call a compiled method through its boxed entry point. On
/// wasm32-unknown-unknown a Rust `fn` pointer *is* an index into the module's
/// (exported, shared) funcref table, so the transmute compiles to a plain
/// `call_indirect` — the design of research-aot-backend.md §6.5, verified
/// end-to-end by the harness. The argv slice lives in linear memory (the Rust
/// heap is linear memory) and its entries stay live because the caller keeps
/// `args` rooted. The thin slice's compiled vocabulary cannot throw; the
/// shared exception channel is slice 2's recorded decision.
#[cfg(target_arch = "wasm32")]
fn call_compiled(fptr1: u32, args: &[Value]) -> Result<Value, Value> {
    let argv: Vec<u32> = args.iter().map(|&v| v.0).collect();
    let f: extern "C" fn(u32, u32) -> u32 = unsafe { core::mem::transmute(fptr1 as usize) };
    Ok(Value(f(argv.as_ptr() as u32, argv.len() as u32) as Offset))
}

#[cfg(not(target_arch = "wasm32"))]
fn call_compiled(_fptr1: u32, _args: &[Value]) -> Result<Value, Value> {
    unreachable!("compiled methods exist only under wasm32 (no native fptr1 registration path)")
}
