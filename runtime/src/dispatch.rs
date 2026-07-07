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
}

struct Table(UnsafeCell<Vec<Entry>>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Table {}
static TABLE: Table = Table(UnsafeCell::new(Vec::new()));

fn table() -> &'static mut Vec<Entry> {
    unsafe { &mut *TABLE.0.get() }
}

/// Clear all methods (called when the runtime resets).
pub fn reset() {
    table().clear();
}

/// Register a method: function `func`, signature tuple type `sig`, body `body`.
pub fn add_method(func: u32, sig: Offset, body: Body) {
    table().push(Entry { func, sig, body });
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
    let body = match select(func, &arg_types) {
        Some(i) => table()[i].body.clone(),
        None => return Ok(Value::NULL),
    };
    interp::eval_with_args(&body, args)
}
