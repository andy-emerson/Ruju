//! Ruju runtime (skeleton).
//!
//! Wires together a bounded-region allocator ([`region`]), the tagged object
//! model and builtin type system ([`object`], [`types`], [`symbol`]), value
//! boxing ([`value`]), and shadow-stack GC rooting ([`gc`]), and exposes an
//! `rj_`-prefixed C ABI for a JavaScript host. Garbage collection is not yet
//! implemented; values are real tagged heap objects whose headers point at
//! their DataType, per `src/julia.h` and the GC-rooting decision in
//! `design/strategy.md`.

mod builtins;
mod dispatch;
mod frontend;
mod gc;
mod interp;
mod object;
mod region;
mod array;
mod errors;
mod memory;
mod module;
mod subtype;
mod symbol;
mod types;
mod value;

use gc::Rooted;
use intrinsics::add_int;
use object::{type_of, Value};
use region::Offset;
use types::id;
use value::{box_int, unbox_int};

/// Initialize (or reset) the region, GC bookkeeping, and the core types, in that
/// order: the heap must be empty and the collector's registry/free list cleared
/// before bootstrap allocates the type objects.
fn init_runtime() {
    region::init();
    gc::reset_heap();
    dispatch::reset();
    types::bootstrap();
    module::init_main();
    install_methods();
}

// Demo generic functions, installed at startup so the interpreter has something
// to dispatch on (the JuliaSyntax/JuliaLowering pipeline that would define real
// methods is not wired in yet).
const F_CLASSIFY: u32 = 0; // classify(x) -> a tag by the type of x
const F_COMBINE: u32 = 1; // combine(a, b) -> a tag by the pair of types
const F_DOUBLE: u32 = 2; // double(x) -> x + x, using the argument

fn install_methods() {
    use interp::{Body, Builtin, Op, Stmt};
    let t = |i| types::builtin(i);
    let ret = |n: usize, k: i64| Body {
        nslots: n,
        code: vec![Stmt::Return(Op::Int(k))],
    };

    // classify: Int64 and Bool are more specific than Integer.
    dispatch::add_method(F_CLASSIFY, types::tuple_type(&[t(id::INTEGER)]), ret(1, 10));
    dispatch::add_method(F_CLASSIFY, types::tuple_type(&[t(id::INT64)]), ret(1, 20));
    dispatch::add_method(F_CLASSIFY, types::tuple_type(&[t(id::BOOL)]), ret(1, 30));

    // combine: a two-argument tuple signature, with a more specific overload.
    dispatch::add_method(F_COMBINE, types::tuple_type(&[t(id::INTEGER), t(id::INTEGER)]), ret(2, 1));
    dispatch::add_method(F_COMBINE, types::tuple_type(&[t(id::INT64), t(id::INT64)]), ret(2, 2));

    // double: uses its argument (`x + x`).
    dispatch::add_method(
        F_DOUBLE,
        types::tuple_type(&[t(id::INT64)]),
        Body {
            nslots: 1,
            code: vec![
                Stmt::Call(Builtin::Add, vec![Op::Slot(0), Op::Slot(0)]),
                Stmt::Return(Op::Ssa(0)),
            ],
        },
    );
}

/// Build the lowered IR `return func(slot0[, slot1])` for an `nargs`-argument
/// generic call, used to drive dispatch from the interpreter.
fn generic_call_body(func: u32, nargs: usize) -> interp::Body {
    use interp::{Op, Stmt};
    let args = (0..nargs).map(Op::Slot).collect();
    interp::Body {
        nslots: nargs,
        code: vec![Stmt::CallGeneric(func, args), Stmt::Return(Op::Ssa(0))],
    }
}

/// Interpret `classify(x)` for a boxed `Int64` argument (dispatches to 20).
#[no_mangle]
pub extern "C" fn rj_call_classify_i64(n: i64) -> i64 {
    ensure_init();
    let body = generic_call_body(F_CLASSIFY, 1);
    let arg = box_int(n);
    unbox_int(interp::eval_with_args(&body, &[arg]).unwrap_or(Value::NULL))
}

/// Interpret `classify(x)` for a boxed `Bool` argument (dispatches to 30).
#[no_mangle]
pub extern "C" fn rj_call_classify_bool() -> i64 {
    ensure_init();
    let body = generic_call_body(F_CLASSIFY, 1);
    let arg = value::box_bool(true);
    unbox_int(interp::eval_with_args(&body, &[arg]).unwrap_or(Value::NULL))
}

/// Interpret `double(x)` for a boxed `Int64` argument (returns `2n`).
#[no_mangle]
pub extern "C" fn rj_call_double(n: i64) -> i64 {
    ensure_init();
    let body = generic_call_body(F_DOUBLE, 1);
    let arg = box_int(n);
    unbox_int(interp::eval_with_args(&body, &[arg]).unwrap_or(Value::NULL))
}

/// Interpret `combine(a, b)` for two boxed `Int64` arguments (dispatches to 2).
#[no_mangle]
pub extern "C" fn rj_call_combine(a: i64, b: i64) -> i64 {
    ensure_init();
    let body = generic_call_body(F_COMBINE, 2);
    let ra = Rooted::new(box_int(a));
    let rb = Rooted::new(box_int(b));
    unbox_int(interp::eval_with_args(&body, &[ra.get(), rb.get()]).unwrap_or(Value::NULL))
}

// Scratch buffer the host writes source into before calling `rj_eval`.
struct SourceBuf(core::cell::UnsafeCell<[u8; 8192]>);
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for SourceBuf {}
static SOURCE: SourceBuf = SourceBuf(core::cell::UnsafeCell::new([0u8; 8192]));

/// Linear-memory address of the source scratch buffer. The host writes UTF-8
/// Julia source here, then calls [`rj_eval`].
#[no_mangle]
pub extern "C" fn rj_source_ptr() -> u32 {
    SOURCE.0.get() as u32
}

/// Parse, lower, and evaluate the first `len` bytes of the source buffer as
/// Julia source, returning the `Int64` result (0 on a parse/eval error).
#[no_mangle]
pub extern "C" fn rj_eval(len: u32) -> i64 {
    ensure_init();
    let bytes = unsafe { core::slice::from_raw_parts(SOURCE.0.get() as *const u8, len as usize) };
    match core::str::from_utf8(bytes).ok().and_then(|s| frontend::eval_source(s).ok()) {
        Some(v) => unbox_int(v),
        None => 0,
    }
}

/// Evaluate the source buffer and return the region offset of the result's
/// type (`0` on a parse/eval error). Lets a host pick the right reader
/// (`rj_eval` vs `rj_eval_f64`) instead of guessing from bit patterns.
#[no_mangle]
pub extern "C" fn rj_eval_typeof(len: u32) -> u32 {
    ensure_init();
    let bytes = unsafe { core::slice::from_raw_parts(SOURCE.0.get() as *const u8, len as usize) };
    match core::str::from_utf8(bytes).ok().and_then(|s| frontend::eval_source(s).ok()) {
        Some(v) => type_of(v),
        None => 0,
    }
}

/// As [`rj_eval`], but for source whose result is a `Float64`.
#[no_mangle]
pub extern "C" fn rj_eval_f64(len: u32) -> f64 {
    ensure_init();
    let bytes = unsafe { core::slice::from_raw_parts(SOURCE.0.get() as *const u8, len as usize) };
    match core::str::from_utf8(bytes).ok().and_then(|s| frontend::eval_source(s).ok()) {
        Some(v) => value::unbox_float64(v),
        None => 0.0,
    }
}

/// Ensure the runtime is initialized.
fn ensure_init() {
    if !region::is_initialized() {
        init_runtime();
    }
}

/// Initialize (or reset) the runtime: the bounded heap region, the collector
/// state, and the core types. Must be called before any other entry point;
/// calling it again resets everything.
#[no_mangle]
pub extern "C" fn rj_init() {
    init_runtime();
}

/// Bytes currently allocated in the region.
#[no_mangle]
pub extern "C" fn rj_heap_used() -> u32 {
    region::used() as u32
}

/// Number of live GC roots on the shadow stack (returns to zero between calls).
#[no_mangle]
pub extern "C" fn rj_root_count() -> u32 {
    gc::root_count() as u32
}

/// Number of live (uncollected) heap objects.
#[no_mangle]
pub extern "C" fn rj_live_objects() -> u32 {
    gc::live_objects() as u32
}

/// Run a full mark-and-sweep collection; returns the number of objects reclaimed.
#[no_mangle]
pub extern "C" fn rj_gc_collect() -> u32 {
    ensure_init();
    gc::collect()
}

/// Allocate `n` boxed integers without rooting them — i.e. immediate garbage.
/// Used to exercise the collector from the host.
#[no_mangle]
pub extern "C" fn rj_alloc_garbage(n: u32) {
    ensure_init();
    for i in 0..n {
        let _ = box_int(i as i64);
    }
}

/// Region offset of the builtin type with the given id (see `types::id`).
#[no_mangle]
pub extern "C" fn rj_builtin_type(id: u32) -> u32 {
    types::builtin(id)
}

/// Region offset of the `nothing` singleton value.
#[no_mangle]
pub extern "C" fn rj_nothing() -> u32 {
    types::nothing_instance()
}

/// The type of the value at region offset `v`, as a DataType offset (`typeof`).
#[no_mangle]
pub extern "C" fn rj_typeof(v: u32) -> u32 {
    type_of(Value(v as Offset))
}

/// The supertype of the DataType at region offset `t` (`Any`'s super is itself).
#[no_mangle]
pub extern "C" fn rj_supertype(t: u32) -> u32 {
    types::supertype(t as Offset)
}

/// Subtyping: 1 if the type at `a` is a subtype of the type at `b`. Handles the
/// nominal hierarchy plus tuples, unions, and `Bottom`.
#[no_mangle]
pub extern "C" fn rj_subtype(a: u32, b: u32) -> u32 {
    types::issubtype(a as Offset, b as Offset) as u32
}

/// `a === b` (`jl_egal`): 1 if the values are egal.
#[no_mangle]
pub extern "C" fn rj_egal(a: u32, b: u32) -> u32 {
    ensure_init();
    builtins::egal(Value(a), Value(b)) as u32
}

/// Structural type equality (`jl_types_egal`): alpha-equivalent `where` types
/// are equal regardless of variable names.
#[no_mangle]
pub extern "C" fn rj_types_egal(a: u32, b: u32) -> u32 {
    ensure_init();
    builtins::types_egal(a as Offset, b as Offset) as u32
}

/// Construct the empty tuple type `Tuple{}`.
#[no_mangle]
pub extern "C" fn rj_tuple_type0() -> u32 {
    ensure_init();
    types::tuple_type(&[])
}

/// Construct the tuple type `Tuple{a}`.
#[no_mangle]
pub extern "C" fn rj_tuple_type1(a: u32) -> u32 {
    ensure_init();
    types::tuple_type(&[a as Offset])
}

/// Construct the tuple type `Tuple{a, b}`.
#[no_mangle]
pub extern "C" fn rj_tuple_type2(a: u32, b: u32) -> u32 {
    ensure_init();
    types::tuple_type(&[a as Offset, b as Offset])
}

/// Construct the tuple type `Tuple{a, b, c}`.
#[no_mangle]
pub extern "C" fn rj_tuple_type3(a: u32, b: u32, c: u32) -> u32 {
    ensure_init();
    types::tuple_type(&[a as Offset, b as Offset, c as Offset])
}

/// Construct the tuple type `Tuple{a, b, c, d}`.
#[no_mangle]
pub extern "C" fn rj_tuple_type4(a: u32, b: u32, c: u32, d: u32) -> u32 {
    ensure_init();
    types::tuple_type(&[a as Offset, b as Offset, c as Offset, d as Offset])
}

/// Construct an unbounded `Vararg{elem}`, for use as the last element of a tuple
/// type.
#[no_mangle]
pub extern "C" fn rj_vararg(elem: u32) -> u32 {
    ensure_init();
    types::vararg_type(elem as Offset)
}

/// Construct `Vararg{elem, n}` with a concrete count; a trailing one expands
/// when the enclosing tuple type is built.
#[no_mangle]
pub extern "C" fn rj_vararg_n(elem: u32, n: i64) -> u32 {
    ensure_init();
    types::vararg_type_n(elem as Offset, n)
}

/// Construct `Union{a, b}`.
#[no_mangle]
pub extern "C" fn rj_union_type(a: u32, b: u32) -> u32 {
    ensure_init();
    types::union_type(a as Offset, b as Offset)
}

/// Construct the demo parametric type `Box{elem}` (invariant, uniqued).
#[no_mangle]
pub extern "C" fn rj_box_type(elem: u32) -> u32 {
    ensure_init();
    types::box_type(elem as Offset)
}

/// Construct the demo two-parameter type `Pair{a, b}` (invariant, uniqued).
#[no_mangle]
pub extern "C" fn rj_pair_type(a: u32, b: u32) -> u32 {
    ensure_init();
    types::pair_type(a as Offset, b as Offset)
}

/// Construct `Type{t}` (invariant, uniqued).
#[no_mangle]
pub extern "C" fn rj_type_type(t: u32) -> u32 {
    ensure_init();
    types::type_type(t as Offset)
}

/// Instantiate the `UnionAll` at `u` with the type `p` (`jl_instantiate_unionall`).
#[no_mangle]
pub extern "C" fn rj_instantiate(u: u32, p: u32) -> u32 {
    ensure_init();
    types::instantiate_unionall(u as Offset, p as Offset)
}

/// Construct the type `Array{elem}` (invariant, uniqued).
#[no_mangle]
pub extern "C" fn rj_array_type(elem: u32) -> u32 {
    ensure_init();
    types::array_type(elem as Offset)
}

/// Allocate an `Array{elem}` of `len` elements over a fresh zeroed buffer.
/// Returns the array value's offset, or 0 on error.
#[no_mangle]
pub extern "C" fn rj_array_new(elem: u32, len: u32) -> u32 {
    ensure_init();
    array::alloc_1d(elem as Offset, len).map_or(0, |a| a.raw())
}

/// The element count of the array at `a`.
#[no_mangle]
pub extern "C" fn rj_array_len(a: u32) -> u32 {
    array::len(Value(a))
}

/// Read element `i` (0-based) as an `Int64` payload, or 0 on error.
#[no_mangle]
pub extern "C" fn rj_array_get_i64(a: u32, i: u32) -> i64 {
    array::aref(Value(a), i).map_or(0, crate::value::unbox_int)
}

/// Store `v` into element `i` (0-based), boxing the payload. 1 on success.
#[no_mangle]
pub extern "C" fn rj_array_set_i64(a: u32, i: u32, v: i64) -> u32 {
    let arr = Value(a);
    let _r = gc::Rooted::new(arr);
    let b = crate::value::box_int(v);
    array::aset(arr, i, b).is_ok() as u32
}

/// Delete the last `dec` elements, zeroing the vacated tail. 1 on success.
#[no_mangle]
pub extern "C" fn rj_array_del_end(a: u32, dec: u32) -> u32 {
    array::del_end(Value(a), dec).is_ok() as u32
}

/// Append `v` (boxed) to the array, growing its buffer as needed. 1 on success.
#[no_mangle]
pub extern "C" fn rj_array_push_i64(a: u32, v: i64) -> u32 {
    let arr = Value(a);
    let _r = gc::Rooted::new(arr);
    let b = crate::value::box_int(v);
    array::push(arr, b).is_ok() as u32
}

/// Construct the type `GenericMemory{elem}` (invariant, uniqued).
#[no_mangle]
pub extern "C" fn rj_memory_type(elem: u32) -> u32 {
    ensure_init();
    types::memory_type(elem as Offset)
}

/// Allocate a `GenericMemory{elem}` of `len` elements, zero-initialized.
/// Returns the memory value's offset, or 0 on error.
#[no_mangle]
pub extern "C" fn rj_memory_new(elem: u32, len: u32) -> u32 {
    ensure_init();
    memory::alloc(elem as Offset, len).map_or(0, |m| m.raw())
}

/// The element count of the memory at `m`.
#[no_mangle]
pub extern "C" fn rj_memory_len(m: u32) -> u32 {
    memory::len(Value(m))
}

/// Read element `i` of the memory at `m` as an `Int64` payload (unboxing the
/// element), or 0 on error — a host convenience over `memoryrefget`.
#[no_mangle]
pub extern "C" fn rj_memory_get_i64(m: u32, i: u32) -> i64 {
    memory::get(Value(m), i).map_or(0, crate::value::unbox_int)
}

/// Store `v` into element `i` of the `GenericMemory{Int64}` at `m` (boxing the
/// payload). Returns 1 on success, 0 on error.
#[no_mangle]
pub extern "C" fn rj_memory_set_i64(m: u32, i: u32, v: i64) -> u32 {
    let mem = Value(m);
    let _r = gc::Rooted::new(mem); // keep the memory alive across the boxing
    let b = crate::value::box_int(v);
    memory::set(mem, i, b).is_ok() as u32
}

/// Construct a `TypeVar` `lb <: T <: ub` named "T". Pass `0` for `lb`/`ub` to
/// default them to `Union{}` and `Any` respectively.
#[no_mangle]
pub extern "C" fn rj_typevar(lb: u32, ub: u32) -> u32 {
    ensure_init();
    let lb = if lb == 0 { types::builtin(id::BOTTOM) } else { lb as Offset };
    let ub = if ub == 0 { types::builtin(id::ANY) } else { ub as Offset };
    types::make_typevar("T", lb, ub)
}

/// Construct the `UnionAll` `body where var` from a `TypeVar` and a body type.
#[no_mangle]
pub extern "C" fn rj_unionall(var: u32, body: u32) -> u32 {
    ensure_init();
    types::unionall_type(var as Offset, body as Offset)
}

/// The instance size declared by the DataType at region offset `t`.
#[no_mangle]
pub extern "C" fn rj_datatype_size(t: u32) -> u32 {
    types::size_of(t as Offset)
}

/// The name `Symbol` of the DataType at region offset `t` (via its `TypeName`).
#[no_mangle]
pub extern "C" fn rj_type_name(t: u32) -> u32 {
    types::type_sym(t as Offset)
}

/// The byte length of the Symbol at region offset `s`.
#[no_mangle]
pub extern "C" fn rj_symbol_len(s: u32) -> u32 {
    symbol::len(s as Offset)
}

/// End-to-end demonstration: box two integers (real tagged `Int64` objects),
/// root them on the shadow stack, run the integer-add intrinsic on their unboxed
/// payloads, box the result, and return its unboxed value.
#[no_mangle]
pub extern "C" fn rj_demo_add(a: i64, b: i64) -> i64 {
    ensure_init();

    let lhs = Rooted::new(box_int(a));
    let rhs = Rooted::new(box_int(b));

    let sum = add_int(unbox_int(lhs.get()), unbox_int(rhs.get()));

    let result = Rooted::new(box_int(sum));
    unbox_int(result.get())
    // `result`, `rhs`, `lhs` drop here in LIFO order, popping the shadow stack.
}

/// Build the lowered IR for `(a + b) * c` and interpret it. Demonstrates
/// straight-line evaluation through the interpreter (SSA values, `:call`).
fn poly_body(a: i64, b: i64, c: i64) -> interp::Body {
    use interp::{Builtin, Op, Stmt};
    interp::Body {
        nslots: 0,
        code: vec![
            Stmt::Call(Builtin::Add, vec![Op::Int(a), Op::Int(b)]), // ssa0 = a + b
            Stmt::Call(Builtin::Mul, vec![Op::Ssa(0), Op::Int(c)]), // ssa1 = ssa0 * c
            Stmt::Return(Op::Ssa(1)),
        ],
    }
}

/// Build the lowered IR for `acc = 0; for i in 1:n; acc += i; end; acc` and
/// interpret it. Demonstrates slots, a back-edge `Goto`, and `GotoIfNot`, while
/// allocating a boxed value per operation — real churn against the collector.
fn sum_to_body(n: i64) -> interp::Body {
    use interp::{Builtin, Op, Stmt};
    interp::Body {
        nslots: 2, // slot0 = acc, slot1 = i
        code: vec![
            Stmt::Assign(0, Op::Int(0)),                              // 0: acc = 0
            Stmt::Assign(1, Op::Int(1)),                              // 1: i = 1
            Stmt::Call(Builtin::Slt, vec![Op::Slot(1), Op::Int(n + 1)]), // 2: ssa2 = i < n+1
            Stmt::GotoIfNot(Op::Ssa(2), 9),                          // 3: if !(i<n+1) goto 9
            Stmt::Call(Builtin::Add, vec![Op::Slot(0), Op::Slot(1)]), // 4: ssa4 = acc + i
            Stmt::Assign(0, Op::Ssa(4)),                             // 5: acc = ssa4
            Stmt::Call(Builtin::Add, vec![Op::Slot(1), Op::Int(1)]),  // 6: ssa6 = i + 1
            Stmt::Assign(1, Op::Ssa(6)),                             // 7: i = ssa6
            Stmt::Goto(2),                                           // 8: loop
            Stmt::Return(Op::Slot(0)),                               // 9: return acc
        ],
    }
}

/// Interpret `(a + b) * c`.
#[no_mangle]
pub extern "C" fn rj_interp_poly(a: i64, b: i64, c: i64) -> i64 {
    ensure_init();
    unbox_int(interp::eval(&poly_body(a, b, c)).unwrap_or(Value::NULL))
}

/// Interpret `sum(1:n)` (0 for n <= 0).
#[no_mangle]
pub extern "C" fn rj_interp_sum_to(n: i64) -> i64 {
    ensure_init();
    unbox_int(interp::eval(&sum_to_body(n)).unwrap_or(Value::NULL))
}

/// Build the lowered IR for `i = n; steps = 0; while i != 0; i -= 1; steps += 1;
/// end; steps` and interpret it. Exercises subtraction and the equality test.
fn count_down_body(n: i64) -> interp::Body {
    use interp::{Builtin, Op, Stmt};
    interp::Body {
        nslots: 2, // slot0 = i, slot1 = steps
        code: vec![
            Stmt::Assign(0, Op::Int(n)),                            // 0: i = n
            Stmt::Assign(1, Op::Int(0)),                            // 1: steps = 0
            Stmt::Call(Builtin::Eq, vec![Op::Slot(0), Op::Int(0)]), // 2: ssa2 = i == 0
            Stmt::GotoIfNot(Op::Ssa(2), 5),                        // 3: if i != 0 goto 5
            Stmt::Return(Op::Slot(1)),                             // 4: return steps
            Stmt::Call(Builtin::Sub, vec![Op::Slot(0), Op::Int(1)]), // 5: ssa5 = i - 1
            Stmt::Assign(0, Op::Ssa(5)),                           // 6: i = ssa5
            Stmt::Call(Builtin::Add, vec![Op::Slot(1), Op::Int(1)]), // 7: ssa7 = steps + 1
            Stmt::Assign(1, Op::Ssa(7)),                           // 8: steps = ssa7
            Stmt::Goto(2),                                         // 9: loop
        ],
    }
}

/// Interpret the countdown loop; returns the number of steps (`n` for `n >= 0`).
#[no_mangle]
pub extern "C" fn rj_interp_count_down(n: i64) -> i64 {
    ensure_init();
    unbox_int(interp::eval(&count_down_body(n)).unwrap_or(Value::NULL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    // The runtime is single-threaded by design (shared global region/type
    // state), so serialize tests that touch it — the default test runner is
    // multi-threaded and would otherwise interleave them.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn demo_add_round_trips_and_balances_roots() {
        let _g = serial();
        rj_init();
        assert_eq!(rj_demo_add(2, 3), 5);
        assert_eq!(rj_demo_add(i64::MAX, 1), i64::MIN); // wrapping intrinsic
        assert_eq!(gc::root_count(), 0, "roots must be released after the call");
        assert!(rj_heap_used() > 0);
    }

    #[test]
    fn object_model_tags_values_with_their_datatype() {
        let _g = serial();
        rj_init();
        let v = box_int(7);
        assert_eq!(type_of(v), types::builtin(id::INT64));
        // DataType is its own type; the bootstrap cycle is closed.
        assert_eq!(rj_typeof(types::builtin(id::DATATYPE)), types::builtin(id::DATATYPE));
        // `nothing` is an instance of Nothing.
        assert_eq!(rj_typeof(rj_nothing()), types::builtin(id::NOTHING));
    }

    #[test]
    fn hierarchy_and_nominal_subtyping() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);

        // Int64 <: Signed <: Integer <: Real <: Number <: Any.
        for sup in [id::SIGNED, id::INTEGER, id::REAL, id::NUMBER, id::ANY] {
            assert!(types::issubtype(t(id::INT64), t(sup)));
        }
        // Float64 <: AbstractFloat <: Real; Bool <: Integer; UInt8 <: Unsigned.
        assert!(types::issubtype(t(id::FLOAT64), t(id::ABSTRACTFLOAT)));
        assert!(types::issubtype(t(id::BOOL), t(id::INTEGER)));
        assert!(types::issubtype(t(id::UINT8), t(id::UNSIGNED)));
        // Everything is a subtype of Any; Any only of itself.
        assert!(types::issubtype(t(id::CHAR), t(id::ANY)));
        assert!(types::issubtype(t(id::ANY), t(id::ANY)));
        // Negative cases across branches.
        assert!(!types::issubtype(t(id::INT64), t(id::FLOAT64)));
        assert!(!types::issubtype(t(id::INT64), t(id::UNSIGNED)));
        assert!(!types::issubtype(t(id::NUMBER), t(id::INT64)));
    }

    #[test]
    fn type_kind_subtyping() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let sub = types::issubtype;
        let tt = types::type_type;
        let (int, integer, datatype, ty) = (t(id::INT64), t(id::INTEGER), t(id::DATATYPE), t(id::TYPE));

        // The kinds sit under Type (test/subtype.jl:536-538); TypeVar does not (:540).
        assert!(sub(datatype, ty) && !sub(ty, datatype));
        assert!(sub(t(id::UNION), ty));
        assert!(sub(t(id::UNIONALL), ty));
        assert!(!sub(t(id::TVAR), ty) && !sub(ty, t(id::TVAR)));
        // Type{Int} dispatches as typeof(Int) (:543) and is invariant (:546).
        assert!(sub(tt(int), datatype) && !sub(datatype, tt(int)));
        assert!(!sub(tt(int), tt(integer)));
        assert!(sub(tt(int), ty)); // Type{Int} <: Type via the shared name
        assert_eq!(tt(int), tt(int)); // uniqued
        // Type{T} where T<:Integer is not under DataType — a union's Type is
        // not a DataType (:544); and Type{Int} binds an existential T (:547-ish).
        let v = types::make_typevar("T", t(id::BOTTOM), integer);
        let ua = types::unionall_type(v, tt(v));
        assert!(!sub(ua, datatype));
        assert!(sub(tt(int), {
            let s = types::make_typevar("S", t(id::BOTTOM), t(id::ANY));
            types::unionall_type(s, tt(s))
        }));
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn unionall_instantiation_matches_direct_construction() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let uall = types::unionall_type;
        let inst = types::instantiate_unionall;
        let tv = || types::make_typevar("T", types::builtin(id::BOTTOM), t(id::ANY));
        let (int, int8, bool_) = (t(id::INT64), t(id::INT8), t(id::BOOL));

        // Uniquing makes instantiation === direct construction: identical offsets.
        // Box{T} where T  @Int  ==  Box{Int}
        let bt = tv();
        assert_eq!(inst(uall(bt, types::box_type(bt)), int), types::box_type(int));
        // Tuple{T,T} where T  @Int  ==  Tuple{Int,Int}
        let tt = tv();
        assert_eq!(
            inst(uall(tt, types::tuple_type(&[tt, tt])), int),
            types::tuple_type(&[int, int])
        );
        // Nested parametric: Tuple{T, Box{T}} where T  @Int
        let nt = tv();
        assert_eq!(
            inst(uall(nt, types::tuple_type(&[nt, types::box_type(nt)])), int),
            types::tuple_type(&[int, types::box_type(int)])
        );
        // Union member: Union{T,Int8} where T  @Int  ==  Union{Int,Int8}. Unions
        // are not interned, so compare structurally rather than by offset.
        let ut = tv();
        let inst_u = inst(uall(ut, types::union_type(ut, int8)), int);
        let direct_u = types::union_type(int, int8);
        assert!(types::issubtype(inst_u, direct_u) && types::issubtype(direct_u, inst_u));
        // Second parameter of a Pair: Pair{Int,S} where S  @Bool
        let st = tv();
        assert_eq!(
            inst(uall(st, types::pair_type(int, st)), bool_),
            types::pair_type(int, bool_)
        );
        // A variable that does not occur leaves the body identical.
        let zt = tv();
        assert_eq!(inst(uall(zt, types::box_type(int)), bool_), types::box_type(int));

        assert_eq!(gc::root_count(), 0, "roots released after instantiation");
    }

    #[test]
    fn array_growth_follows_the_c_sequence() {
        let _g = serial();
        rj_init();
        let int = types::builtin(id::INT64);
        let a = array::alloc_1d(int, 0).unwrap();
        let root = gc::Rooted::new(a);
        assert_eq!(array::len(a), 0);
        // Push 0..100: contents stay intact across every buffer reallocation.
        for i in 0..100i64 {
            array::push(root.get(), box_int(i)).unwrap();
        }
        assert_eq!(array::len(root.get()), 100);
        for i in 0..100u32 {
            assert_eq!(
                crate::value::unbox_int(array::aref(root.get(), i).unwrap()),
                i as i64
            );
        }
        // Capacity followed 0 -> 4 -> 6 -> 9 -> 13 -> ... (grow-by-half below
        // 48, by a fifth above): strictly more than 100, well under 2x.
        let cap = memory::len(array::mem_of(root.get()));
        assert!(cap >= 100 && cap < 200, "capacity {} out of the C's envelope", cap);
        // aset/aref respect the array's length, not the buffer's.
        assert!(array::aref(root.get(), 100).is_err());
        assert!(array::aset(root.get(), cap - 1, box_int(0)).is_err());
        // del_end shrinks and zeroes the tail.
        array::del_end(root.get(), 90).unwrap();
        assert_eq!(array::len(root.get()), 10);
        assert!(array::aref(root.get(), 10).is_err());
        drop(root);
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn array_boxed_elements_survive_growth_and_collection() {
        let _g = serial();
        rj_init();
        let any = types::builtin(id::ANY);
        let a = array::alloc_1d(any, 0).unwrap();
        let root = gc::Rooted::new(a);
        // Interleave pushes with collections: the array roots its buffer, the
        // buffer roots the elements, growth swaps buffers mid-stream.
        for i in 0..40i64 {
            array::push(root.get(), box_int(1000 + i)).unwrap();
            if i % 10 == 9 {
                gc::collect_full();
            }
        }
        for i in 0..40u32 {
            assert_eq!(
                crate::value::unbox_int(array::aref(root.get(), i).unwrap()),
                1000 + i as i64
            );
        }
        // Promote the array old, then push young: grow_end swaps the mem field
        // through the write barrier, and a minor collect must keep everything.
        gc::collect();
        gc::collect();
        array::push(root.get(), box_int(4242)).unwrap();
        gc::collect();
        rj_alloc_garbage(64);
        assert_eq!(
            crate::value::unbox_int(array::aref(root.get(), 40).unwrap()),
            4242
        );
        // A deleted boxed tail reads as unset (zeroed, not dangling).
        array::del_end(root.get(), 1).unwrap();
        array::grow_end(root.get(), 1).unwrap();
        assert!(array::aref(root.get(), 40).is_err(), "cleared slot must be unset");
        drop(root);
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn memory_bits_elements_roundtrip() {
        let _g = serial();
        rj_init();
        let int = types::builtin(id::INT64);
        let m = memory::alloc(int, 5).unwrap();
        let _r = gc::Rooted::new(m);
        assert_eq!(memory::len(m), 5);
        // Zero-initialized, like object::alloc generally.
        assert_eq!(crate::value::unbox_int(memory::get(m, 0).unwrap()), 0);
        for i in 0..5u32 {
            memory::set(m, i, box_int(10 + i as i64)).unwrap();
        }
        for i in 0..5u32 {
            assert_eq!(crate::value::unbox_int(memory::get(m, i).unwrap()), 10 + i as i64);
        }
        // Bounds are checked on both sides of the ref.
        assert!(memory::get(m, 5).is_err());
        assert!(memory::set(m, 5, box_int(0)).is_err());
        // The element type is enforced (jl_memoryrefset's isa check).
        assert!(memory::set(m, 0, crate::value::box_float64(1.5)).is_err());
        // Memory{T} is uniqued and invariant.
        assert_eq!(types::memory_type(int), types::memory_type(int));
        assert!(!types::issubtype(
            types::memory_type(int),
            types::memory_type(types::builtin(id::INTEGER))
        ));
        assert!(types::issubtype(types::memory_type(int), types::builtin(id::ANY)));
        drop(_r);
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn memory_boxed_elements_traced_and_barriered() {
        let _g = serial();
        rj_init();
        let any = types::builtin(id::ANY);
        let m = memory::alloc(any, 3).unwrap();
        let root = gc::Rooted::new(m);
        // An unset boxed slot is an UndefRefError, not a null deref.
        assert!(memory::get(m, 0).is_err());
        // Boxed elements keep identity: get returns the same object.
        let b = box_int(77);
        memory::set(m, 0, b).unwrap();
        assert_eq!(memory::get(m, 0).unwrap(), b);
        // Elements survive a full collection only through the memory's trace.
        memory::set(m, 1, box_int(88)).unwrap();
        gc::collect_full();
        assert_eq!(crate::value::unbox_int(memory::get(root.get(), 0).unwrap()), 77);
        assert_eq!(crate::value::unbox_int(memory::get(root.get(), 1).unwrap()), 88);
        // Promote the memory old (marked survivors promote at sweep), then store
        // a young value: the write barrier must remember the old->young edge or
        // a minor collection frees the element out from under us.
        gc::collect();
        gc::collect();
        memory::set(root.get(), 2, box_int(99)).unwrap();
        gc::collect(); // minor: reaches the young box via the remset alone
        rj_alloc_garbage(64); // stomp anything wrongly freed
        assert_eq!(crate::value::unbox_int(memory::get(root.get(), 2).unwrap()), 99);
        drop(root);
        // Unrooted, the memory is reclaimed — after two full sweeps: it was
        // promoted old above, and old garbage has the pin's one-full-cycle lag
        // (a full sweep demotes OLD_MARKED to OLD; the next one frees it).
        let live0 = {
            gc::collect_full();
            gc::collect_full();
            rj_live_objects()
        };
        let m2 = memory::alloc(any, 64).unwrap();
        let _ = m2;
        gc::collect_full();
        assert_eq!(rj_live_objects(), live0, "unrooted memory must be reclaimed");
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn recycled_page_sweep_stops_at_the_bump_cursor() {
        let _g = serial();
        rj_init();
        // Fill pages with garbage, then release them whole (no walk): the dead
        // cells keep their stale headers.
        rj_alloc_garbage(2000);
        gc::collect_full();
        // A recycled page joins a pool with only its bump cursor: allocate one
        // live object on it, leaving stale headers in the virgin tail.
        let v = box_int(31415);
        let root = gc::Rooted::new(v);
        // Walk the page (the object is marked): the sweep must stop at the
        // cursor rather than misread the stale tail as free-listable cells.
        gc::collect_full();
        gc::collect_full();
        assert_eq!(crate::value::unbox_int(root.get()), 31415);
        // Allocation through the rebuilt free lists and cursors stays sound.
        rj_alloc_garbage(2000);
        gc::collect_full();
        assert_eq!(crate::value::unbox_int(root.get()), 31415);
        drop(root);
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn parametric_typenames_survive_collection() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        // Note the Pair TypeName's offset, then drop every reference to Pair
        // types: the typename (and its instantiation cache) must survive on the
        // pinned-roots list alone. A swept object's header becomes a freelist
        // link, so `type_of` detects a wrongly-freed typename deterministically.
        let tn = types::name_of(types::pair_type(t(id::INT64), t(id::INT8)));
        gc::collect_full();
        gc::collect_full();
        assert_eq!(
            type_of(Value(tn)),
            types::builtin(id::TYPENAME),
            "the Pair TypeName must not be swept while unreferenced"
        );
        // The instantiation cache also survives: uniquing still holds.
        let p1 = types::pair_type(t(id::INT64), t(id::INT8));
        assert_eq!(crate::symbol::as_str(types::type_sym(p1)), "Pair");
        assert_eq!(p1, types::pair_type(t(id::INT64), t(id::INT8)));
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn pair_invariant_and_diagonal_subtyping() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let pair = types::pair_type;
        let uall = types::unionall_type;
        let tv = || types::make_typevar("T", types::builtin(id::BOTTOM), t(id::ANY));
        let sub = types::issubtype;
        let (int, int8) = (t(id::INT64), t(id::INT8));

        // `Pair{Int,Int8} <: Pair{T,S} where {T,S}` but the diagonal
        // `Pair{T,T} where T` excludes it (test/subtype.jl:207,262).
        let a = tv();
        let b = tv();
        let bare_pair = uall(a, uall(b, pair(a, b)));
        assert!(sub(pair(int, int8), bare_pair));
        let d = tv();
        let diag_pair = uall(d, pair(d, d));
        assert!(!sub(pair(int, int8), diag_pair));
        // `Pair{T,T} where T <: Pair{Int,Int}` is false: no single T is both
        // invariantly (subtype.jl:233).
        assert!(!sub(diag_pair, pair(int, int)));
        // Invariance: distinct instantiations are unrelated but reflexive.
        assert!(!sub(pair(int, int8), pair(t(id::INTEGER), t(id::SIGNED))));
        assert!(sub(pair(int, int8), pair(int, int8)));

        assert_eq!(gc::root_count(), 0, "roots released after subtype queries");
    }

    #[test]
    fn tuple_varargs_subtyping() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let tup = |elems: &[Offset]| types::tuple_type(elems);
        let va = types::vararg_type;
        let sub = types::issubtype;
        let (int, integer, real, any) =
            (t(id::INT64), t(id::INTEGER), t(id::REAL), t(id::ANY));

        // A fixed tuple is a strict subtype of a matching Vararg tail
        // (test/subtype.jl:43,47).
        assert!(sub(tup(&[int, int]), tup(&[va(int)])));
        assert!(!sub(tup(&[va(int)]), tup(&[int, int])));
        assert!(sub(tup(&[int, va(int)]), tup(&[va(int)])));
        // Element subtyping flows through the vararg (L45); width widens (L44).
        assert!(sub(tup(&[int, int]), tup(&[int, va(integer)])));
        // The empty tuple is under any unbounded Vararg (L51,591).
        assert!(sub(tup(&[]), tup(&[va(any)])));
        // Unbounded left, fixed/short right is rejected (L592,594).
        assert!(!sub(tup(&[va(int)]), tup(&[int])));
        assert!(!sub(tup(&[va(integer)]), tup(&[integer, integer, va(integer)])));
        // A non-matching element still fails through the vararg (L593).
        assert!(!sub(tup(&[va(int)]), tup(&[t(id::NUMBER), integer])));
        // Vararg{S} <: Vararg{T} reduces to S <: T, strictly (L587 analog).
        assert!(sub(tup(&[integer, va(integer)]), tup(&[integer, va(real)])));
        assert!(!sub(tup(&[integer, va(real)]), tup(&[integer, va(integer)])));

        assert_eq!(gc::root_count(), 0, "roots released after subtype queries");
    }

    #[test]
    fn subtype_queries_survive_stress_collection() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let tup = |elems: &[Offset]| types::tuple_type(elems);
        let uall = types::unionall_type;
        let tv = |n: &str| types::make_typevar(n, types::builtin(id::BOTTOM), t(id::ANY));
        let (int, int8, dt) = (t(id::INT64), t(id::INT8), t(id::DATATYPE));

        // Build the queries first (the constructors root their own working
        // sets), then run them with a collection forced at *every*
        // allocation. Mid-query allocation sites — the kind rule's fresh
        // `Type{T'} where T'` and `simple_join`'s fresh unions — must not
        // let the query types, the env's narrowed bounds, or the saved
        // snapshots be reclaimed (subtype.c roots exactly these:
        // `jl_savedenv_t.roots`, the `JL_GC_PUSH` on `vb.lb`/`vb.ub`).
        let s1 = tv("S");
        let type_s = uall(s1, types::type_type(s1));
        let t2 = tv("T");
        let s2 = tv("S");
        let diag_kind_body = tup(&[t2, t2, types::type_type(s2)]);
        let diag_kind = uall(t2, uall(s2, diag_kind_body));
        let lhs_kind = tup(&[int, int8, dt]);
        let d3 = tv("T");
        let diag = uall(d3, tup(&[d3, d3]));
        let u4 = tv("S");
        let exists_union = uall(u4, types::union_type(int8, u4));
        let lhs_union = types::union_type(int, int8);

        {
            // The queries' own roots: the test stands in for the host, which
            // holds offsets across allocating calls only under this contract.
            // (Named locals drop in reverse declaration order — LIFO, as the
            // shadow stack requires.)
            let _r0 = gc::Rooted::new(Value(type_s));
            let _r1 = gc::Rooted::new(Value(diag_kind));
            let _r2 = gc::Rooted::new(Value(lhs_kind));
            let _r3 = gc::Rooted::new(Value(diag));
            let _r4 = gc::Rooted::new(Value(exists_union));
            let _r5 = gc::Rooted::new(Value(lhs_union));
            gc::set_stress(true);
            // The kind rule allocates its fresh `Type{T'} where T'` mid-query.
            assert!(types::issubtype(dt, type_s), "DataType <: (Type{{S}} where S)");
            // Diagonal rejection must read T's freshly-allocated union lower
            // bound *after* the kind rule's allocations freed-or-kept it.
            assert!(
                !types::issubtype(lhs_kind, diag_kind),
                "diagonal T with a fresh union lb survives later allocations"
            );
            // Plain diagonal accept/reject under stress.
            assert!(types::issubtype(tup(&[int, int]), diag));
            assert!(!types::issubtype(tup(&[int, int8]), diag));
            // Union backtracking with env narrowing under stress.
            assert!(types::issubtype(lhs_union, exists_union));
            gc::set_stress(false);
        }
        assert_eq!(gc::root_count(), 0, "roots released after stressed queries");
    }

    #[test]
    fn union_decision_machine_distributivity() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let tup = |elems: &[Offset]| types::tuple_type(elems);
        let uall = types::unionall_type;
        let tv = || types::make_typevar("T", types::builtin(id::BOTTOM), types::builtin(id::ANY));
        let (int, int8, int16) = (t(id::INT64), t(id::INT8), t(id::INT16));
        let u = |a, b| types::union_type(a, b);
        let sub = types::issubtype;

        // test/subtype.jl:371 — the backtrack point (the right union arm) is an
        // ancestor of the left union: each left arm must choose its own right
        // arm, which only the global machine's ∀-outside-∃ enumeration can do.
        let lhs = tup(&[u(int, int8), int16]);
        let rhs = u(tup(&[int, int16]), tup(&[int8, int16]));
        assert!(sub(lhs, rhs) && sub(rhs, lhs), "L371 heals under the machine");

        // test/subtype.jl:410/:449 — a fresh ∃T binding per ∀ pass lets each
        // union branch pick its own T.
        let t1 = tv();
        let y = uall(t1, tup(&[types::box_type(t1)]));
        let x = tup(&[u(types::box_type(int), types::box_type(int8))]);
        assert!(sub(x, y), "L410/L449 heals under the machine");

        // test/subtype.jl:448/:450 — under an *invariant* constructor the same
        // union stays false; the machine must not over-heal.
        let t2 = tv();
        let y_inv = uall(t2, types::box_type(types::box_type(t2)));
        let x_inv = types::box_type(u(types::box_type(int), types::box_type(int8)));
        assert!(!sub(x_inv, y_inv), "L448 stays false (invariant position)");
        let y_u = u(
            types::box_type(types::box_type(int)),
            types::box_type(types::box_type(int8)),
        );
        assert!(!sub(x_inv, y_u), "L450 stays false (invariant position)");

        // test/subtype.jl:445 — the convert(Type{T},T) pattern: matching the
        // whole union against the variable is a recorded, revisitable choice.
        let t3 = tv();
        let y_cv = uall(t3, tup(&[types::box_type(t3), t3]));
        let x_cv = tup(&[types::box_type(u(int8, int)), int]);
        assert!(sub(x_cv, y_cv), "L445 convert-pattern");

        assert_eq!(gc::root_count(), 0, "roots released after machine queries");
    }

    #[test]
    fn consistency_scope_keeps_bound_occurrences_separate() {
        // test/subtype.jl:127-138 with Float64 standing in for String (no
        // String type yet; the property is type-agnostic). The diagonal
        // counter is scoped per consistency check: occurrences of T inside
        // S's *bound* do not combine with T's occurrence in the outer body,
        // so `Tuple{S,T} where {T, S<:Tuple{T}}` is not diagonal in T — but
        // `S<:Tuple{T,T}` still is, both occurrences sharing the bound's
        // Tuple frame.
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let tup = |elems: &[Offset]| types::tuple_type(elems);
        let uall = types::unionall_type;
        let (int, f64) = (t(id::INT64), t(id::FLOAT64));
        let sub = types::issubtype;

        // y1 = Tuple{S, T} where {T, S <: Tuple{T}}
        let t1 = types::make_typevar("T", t(id::BOTTOM), t(id::ANY));
        let s1 = types::make_typevar("S", t(id::BOTTOM), tup(&[t1]));
        let y1 = uall(t1, uall(s1, tup(&[s1, t1])));
        assert!(sub(tup(&[tup(&[f64]), int]), y1), "L129: bound occurrence is scoped");
        assert!(sub(tup(&[tup(&[int]), int]), y1), "L131: same, agreeing element");

        // y2 = Tuple{S, T} where {T, S <: Tuple{T, T}} — diagonal inside the bound.
        let t2 = types::make_typevar("T", t(id::BOTTOM), t(id::ANY));
        let s2 = types::make_typevar("S", t(id::BOTTOM), tup(&[t2, t2]));
        let y2 = uall(t2, uall(s2, tup(&[s2, t2])));
        assert!(!sub(tup(&[tup(&[int, int]), f64]), y2), "L135: still diagonal in the bound frame");
        assert!(sub(tup(&[tup(&[int, int]), int]), y2), "L137: diagonal satisfied concretely");
    }

    #[test]
    fn interpreter_global_and_const_operands() {
        use crate::interp::{eval, Body, Builtin, Op, Stmt};
        let _g = serial();
        rj_init();
        let sym = |s: &str| crate::symbol::intern(types::builtin(id::SYMBOL), s);

        // global x = 41; return x + 1 — the write goes through Main's
        // bindings; the read resolves at evaluation time (GlobalRef).
        let x = sym("x");
        let b = Body {
            nslots: 0,
            code: vec![
                Stmt::AssignGlobal(x, Op::Int(41)),
                Stmt::Call(Builtin::Add, vec![Op::Global(x), Op::Int(1)]),
                Stmt::Return(Op::Ssa(1)),
            ],
        };
        assert_eq!(crate::value::unbox_int(eval(&b).expect("global round-trip")), 42);
        // ...and the binding persists in Main after the frame is gone.
        let main = object::Value(crate::module::main_offset());
        assert_eq!(crate::value::unbox_int(crate::module::get_global(main, x).unwrap()), 41);

        // Reading an unbound global is a catchable exception, not a crash.
        let b = Body {
            nslots: 0,
            code: vec![
                Stmt::Enter(3),
                Stmt::Return(Op::Global(sym("surely_undefined"))),
                Stmt::Leave(1),
                Stmt::Return(Op::Int(-1)),
            ],
        };
        assert_eq!(crate::value::unbox_int(eval(&b).expect("caught by the handler")), -1);

        // A boxed constant operand (the QuoteNode analog), kept reachable by
        // the test as the IR's owner — the documented Const contract.
        let c = box_int(7);
        let _rc = gc::Rooted::new(c);
        let b = Body {
            nslots: 0,
            code: vec![
                Stmt::Call(Builtin::Add, vec![Op::Const(c.raw()), Op::Int(1)]),
                Stmt::Return(Op::Ssa(0)),
            ],
        };
        assert_eq!(crate::value::unbox_int(eval(&b).expect("const operand")), 8);
    }

    #[test]
    fn interpreter_calls_through_values() {
        use crate::interp::{eval, Body, Builtin, Op, Stmt};
        let _g = serial();
        rj_init();
        let sym = |s: &str| crate::symbol::intern(types::builtin(id::SYMBOL), s);

        // double(x::Int64) = x + x, as a callable value bound to Main.double —
        // then `double(21)` in Julia's lowered shape: the callee is a Global
        // operand evaluated like any other, dispatch keys off typeof(callee).
        const F_DOUBLE: u32 = 900;
        dispatch::add_method(
            F_DOUBLE,
            types::tuple_type(&[types::builtin(id::INT64)]),
            Body {
                nslots: 1,
                code: vec![
                    Stmt::Call(Builtin::Add, vec![Op::Slot(0), Op::Slot(0)]),
                    Stmt::Return(Op::Ssa(0)),
                ],
            },
        );
        let fval = dispatch::make_function("double", F_DOUBLE);
        let dsym = sym("double");
        let main = object::Value(crate::module::main_offset());
        crate::module::set_global(main, dsym, fval).expect("bind Main.double");
        assert!(types::issubtype(object::type_of(fval), types::builtin(id::FUNCTION)));

        let b = Body {
            nslots: 0,
            code: vec![
                Stmt::CallValue(vec![Op::Global(dsym), Op::Int(21)]),
                Stmt::Return(Op::Ssa(0)),
            ],
        };
        assert_eq!(crate::value::unbox_int(eval(&b).expect("call through value")), 42);

        // A non-callable callee throws a catchable MethodError-shaped error.
        let b = Body {
            nslots: 0,
            code: vec![
                Stmt::Enter(3),
                Stmt::CallValue(vec![Op::Int(3), Op::Int(4)]),
                Stmt::Leave(1),
                Stmt::Return(Op::Int(-1)),
            ],
        };
        // ip 2 (Leave) is skipped on the throw; the catch lands at ip 3.
        assert_eq!(crate::value::unbox_int(eval(&b).expect("caught")), -1);
    }

    #[test]
    fn interpreter_try_catch_transfers_control() {
        use crate::interp::{eval, Body, Builtin, Op, Stmt};
        let _g = serial();
        rj_init();
        // try; slot0 = a ÷ b; catch; return 999; end; return slot0
        let mk = |a: i64, b: i64| Body {
            nslots: 1,
            code: vec![
                Stmt::Enter(5),                                        // 0: catch at ip 5
                Stmt::Call(Builtin::IDiv, vec![Op::Int(a), Op::Int(b)]), // 1: throws if b==0
                Stmt::Assign(0, Op::Ssa(1)),                          // 2: slot0 = quotient
                Stmt::Leave(1),                                       // 3: normal: pop handler
                Stmt::Return(Op::Slot(0)),                            // 4: normal return
                Stmt::Return(Op::Int(999)),                           // 5: catch: recover
            ],
        };
        // b == 0 raises DivideError inside the try; control lands in the catch.
        let caught = eval(&mk(1, 0)).expect("catch recovers");
        assert_eq!(crate::value::unbox_int(caught), 999);
        // No throw: Leave pops the handler and the normal path returns the quotient.
        let normal = eval(&mk(6, 2)).expect("normal path");
        assert_eq!(crate::value::unbox_int(normal), 3);
        assert_eq!(gc::root_count(), 0, "roots released after eval");
    }

    #[test]
    fn interpreter_catch_binds_the_exception() {
        use crate::interp::{eval, Body, Op, Stmt};
        let _g = serial();
        rj_init();
        // try; throw(42); catch e; return e; end
        let body = Body {
            nslots: 0,
            code: vec![
                Stmt::Enter(2),         // 0: catch at ip 2
                Stmt::Throw(Op::Int(42)), // 1: throw 42 -> ip 2
                Stmt::Caught,           // 2: (catch) ssa2 = the exception
                Stmt::Return(Op::Ssa(2)), // 3: return e
            ],
        };
        let e = eval(&body).expect("the caught exception is bound");
        assert_eq!(crate::value::unbox_int(e), 42);
        // An uncaught throw propagates out of the frame.
        let uncaught = Body {
            nslots: 0,
            code: vec![Stmt::Throw(Op::Int(7))],
        };
        assert!(eval(&uncaught).is_err());
        assert_eq!(gc::root_count(), 0, "roots released after eval");
    }

    #[test]
    fn toplevel_globals_persist_in_main() {
        let _g = serial();
        rj_init();
        let run = |s: &str| crate::value::unbox_int(crate::frontend::eval_source(s).unwrap());
        // A top-level assignment binds a Main global that later evals see.
        assert_eq!(run("gx = 41"), 41);
        assert_eq!(run("gx + 1"), 42);
        // Rebinding replaces the value; heap values (arrays) persist too, and
        // survive the collections later evals may trigger.
        assert_eq!(run("gx = 5"), 5);
        assert_eq!(run("ga = [1, 2, 3]\nga[3]"), 3);
        gc::collect_full();
        assert_eq!(run("push!(ga, gx)\nga[4] + length(ga)"), 9);
        // The module API agrees with what source-level eval sees.
        let main = Value(crate::module::main_offset());
        let sym = crate::symbol::intern(types::builtin(id::SYMBOL), "gx");
        let v = crate::module::get_global(main, sym).expect("gx is bound");
        assert_eq!(crate::value::unbox_int(v), 5);
        // Unbound names are None, not garbage.
        let missing = crate::symbol::intern(types::builtin(id::SYMBOL), "nope");
        assert!(crate::module::get_global(main, missing).is_none());
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn source_arrays_end_to_end() {
        let _g = serial();
        rj_init();
        let run = |s: &str| crate::value::unbox_int(crate::frontend::eval_source(s).unwrap());
        // Literal, 1-based indexing, and setindex!.
        assert_eq!(run("a = [10, 20, 30]\na[2]"), 20);
        assert_eq!(run("a = [1, 2, 3]\na[1] = 99\na[1] + a[3]"), 102);
        // length and push! (whose statement value is the array).
        assert_eq!(run("a = [1, 2]\nlength(a)"), 2);
        assert_eq!(run("a = [5]\npush!(a, 6)\npush!(a, 7)\na[3] * length(a)"), 21);
        // A push!-driven sum loop: literals + growth + indexing together.
        assert_eq!(
            run("a = []\ni = 1\nwhile i <= 50\npush!(a, i * i)\ni = i + 1\nend\ns = 0\nj = 1\nwhile j <= length(a)\ns = s + a[j]\nj = j + 1\nend\ns"),
            (1..=50i64).map(|k| k * k).sum::<i64>()
        );
        // An out-of-bounds read is a catchable BoundsError.
        assert_eq!(run("a = [1]\nx = 0\ntry\nx = a[2]\ncatch\nx = 777\nend\nx"), 777);
        assert_eq!(gc::root_count(), 0, "roots released after eval");
    }

    #[test]
    fn source_try_catch_recovers() {
        let _g = serial();
        rj_init();
        let run = |s: &str| crate::value::unbox_int(crate::frontend::eval_source(s).unwrap());
        // A DivideError inside the try is recovered in the catch.
        assert_eq!(run("x = 0\ntry\nx = 1 ÷ 0\ncatch\nx = 999\nend\nx"), 999);
        // No error: the body runs and the catch is skipped.
        assert_eq!(run("x = 0\ntry\nx = 6 ÷ 2\ncatch\nx = 999\nend\nx"), 3);
        // throw(v) from source; catch e binds the thrown value.
        assert_eq!(run("x = 0\ntry\nthrow(42)\ncatch e\nx = e\nend\nx"), 42);
        // A thrown value can carry data through the handler.
        assert_eq!(run("x = 0\ntry\nthrow(6 * 7)\ncatch e\nx = e + 1\nend\nx"), 43);
        // An uncaught throw propagates out as an eval error.
        assert!(crate::frontend::eval_source("throw(1)").is_err());
        // A builtin error binds its reified exception object to `catch e` —
        // and never a stale earlier exception.
        let v = crate::frontend::eval_source(
            "x = 0\ntry\nthrow(5)\ncatch e\nx = e\nend\ntry\nx = 1 ÷ 0\ncatch e\nx = e\nend\nx",
        )
        .unwrap();
        assert_eq!(
            v.raw(),
            types::instance_of(types::builtin(id::DIVIDEERROR)),
            "a DivideError binds its singleton to catch e"
        );
        assert_eq!(gc::root_count(), 0, "roots released after eval");
    }

    #[test]
    fn exceptions_are_reified_objects() {
        let _g = serial();
        rj_init();
        let run = |s: &str| crate::frontend::eval_source(s).unwrap();
        // An out-of-bounds index binds a BoundsError carrying the array and
        // the offending 1-based index as its `a`/`i` fields (boot.jl:378).
        let e = run("a = [1, 2]\nc = 0\ntry\nc = a[5]\ncatch e\ne\nend\ntry\nc = a[5]\ncatch err\nc = err\nend\nc");
        assert_eq!(type_of(e), types::builtin(id::BOUNDSERROR));
        let carried_a = crate::object::get_ref(e, 0);
        let carried_i = crate::object::get_ref(e, 4);
        assert!(types::is_array(type_of(carried_a)), "BoundsError.a is the array");
        assert_eq!(crate::value::unbox_int(carried_i), 5, "BoundsError.i is 1-based");
        // An uncaught exception formats at the host boundary.
        let msg = crate::frontend::eval_source("[1][3]").unwrap_err();
        assert!(msg.starts_with("BoundsError"), "host rendering: {}", msg);
        let msg = crate::frontend::eval_source("1 ÷ 0").unwrap_err();
        assert_eq!(msg, "DivideError");
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn source_finally_runs_on_both_paths() {
        let _g = serial();
        rj_init();
        let run = |s: &str| crate::value::unbox_int(crate::frontend::eval_source(s).unwrap());
        // Normal path: cleanup runs after the body.
        assert_eq!(run("x = 0\ntry\nx = x + 1\nfinally\nx = x + 10\nend\nx"), 11);
        // Exception path: cleanup runs, then the exception resumes unwinding
        // and the outer catch binds it.
        assert_eq!(
            run("x = 0\ntry\ntry\nthrow(5)\nfinally\nx = x + 10\nend\ncatch e\nx = x + e\nend\nx"),
            15
        );
        // Combined try/catch/finally desugars: handler and cleanup both run.
        assert_eq!(
            run("x = 0\ntry\nthrow(7)\ncatch e\nx = e\nfinally\nx = x + 100\nend\nx"),
            107
        );
        // An uncaught rethrow after finally propagates out.
        assert!(crate::frontend::eval_source("try\nthrow(1)\nfinally\nend").is_err());
        assert_eq!(gc::root_count(), 0);
    }

    #[test]
    fn primitive_sizes_match_julia() {
        let _g = serial();
        rj_init();
        let sz = |i| types::size_of(types::builtin(i));
        assert_eq!((sz(id::BOOL), sz(id::INT8), sz(id::INT16)), (1, 1, 2));
        assert_eq!((sz(id::INT32), sz(id::INT64), sz(id::INT128)), (4, 8, 16));
        assert_eq!((sz(id::FLOAT16), sz(id::FLOAT32), sz(id::FLOAT64)), (2, 4, 8));
        assert_eq!((sz(id::CHAR), sz(id::UINT64)), (4, 8));
    }

    #[test]
    fn type_names_are_real_interned_symbols() {
        let _g = serial();
        rj_init();
        // The name of Int64 is the Symbol "Int64" (length 5); interning is shared.
        assert_eq!(symbol::len(types::type_sym(types::builtin(id::INT64))), 5);
        assert_eq!(symbol::len(types::type_sym(types::builtin(id::DATATYPE))), 8);
        assert_eq!(
            types::type_sym(types::builtin(id::BOOL)),
            types::type_sym(types::builtin(id::BOOL)),
            "interning yields a single name Symbol",
        );
    }

    #[test]
    fn composite_struct_layout_records_pointer_fields() {
        let _g = serial();
        rj_init();

        // A struct with two reference fields at byte offsets 0 and 4.
        let pair = types::define_struct("IntPair", types::builtin(id::ANY), &[("a", types::builtin(id::ANY)), ("b", types::builtin(id::ANY))], true);
        assert_eq!(types::layout_npointers(pair), 2);
        assert_eq!(types::layout_ptr_offset(pair, 0), 0);
        assert_eq!(types::layout_ptr_offset(pair, 1), 4);

        // Instantiate it and populate the reference fields with boxed integers.
        let a = Rooted::new(box_int(10));
        let b = Rooted::new(box_int(20));
        let p = Rooted::new(object::alloc(pair, 8));
        object::set_ref(p.get(), 0, a.get());
        object::set_ref(p.get(), 4, b.get());

        assert_eq!(type_of(object::get_ref(p.get(), 0)), types::builtin(id::INT64));
        assert_eq!(unbox_int(object::get_ref(p.get(), 0)), 10);
        assert_eq!(unbox_int(object::get_ref(p.get(), 4)), 20);
    }

    #[test]
    fn collector_reclaims_garbage_and_keeps_roots() {
        let _g = serial();
        rj_init();
        gc::collect_full(); // clear any bootstrap/cache-growth garbage first

        // A rooted survivor, plus the pinned builtin types, are the live set.
        let survivor = Rooted::new(box_int(12345));
        let live = gc::live_objects();

        // Allocate unrooted garbage.
        for i in 0..50 {
            let _ = box_int(i);
        }
        assert_eq!(gc::live_objects(), live + 50);

        // Collection reclaims exactly the garbage; the survivor and the
        // bootstrapped types remain.
        assert_eq!(gc::collect(), 50);
        assert_eq!(gc::live_objects(), live);

        // Non-moving: the survivor's offset and payload are intact.
        assert_eq!(unbox_int(survivor.get()), 12345);
        // And it is still a valid, correctly-typed object.
        assert_eq!(type_of(survivor.get()), types::builtin(id::INT64));
    }

    #[test]
    fn pooled_allocation_reuses_pages_under_churn() {
        let _g = serial();
        rj_init();
        let base = region::used();
        // Allocate and collect repeatedly; the region high-water mark must
        // stabilize because freed slots are reused from the pool's free list and
        // pages are not endlessly re-carved.
        for _ in 0..5 {
            for i in 0..500 {
                let _ = box_int(i);
            }
            gc::collect();
        }
        let grew = region::used() - base;
        assert!(grew < 64 * 1024, "pooled pages should be reused, not re-carved (grew {grew})");
    }

    #[test]
    fn generational_promotes_survivors_to_old() {
        let _g = serial();
        rj_init();
        let s = Rooted::new(box_int(99));
        assert!(!gc::is_old(s.get()), "new objects start young");
        gc::collect(); // minor
        assert!(gc::is_old(s.get()), "a survivor is promoted to the old generation");
        assert_eq!(unbox_int(s.get()), 99, "promotion is non-moving");
    }

    #[test]
    fn full_collection_reclaims_old_garbage_that_minor_keeps() {
        let _g = serial();
        rj_init();
        gc::collect_full(); // promote the bootstrap objects to old
        let live0 = gc::live_objects();
        {
            let f = gc::Frame::new(100);
            for i in 0..100 {
                f.set(i, box_int(i as i64));
            }
            gc::collect(); // promote the 100 survivors to old
            assert!(gc::is_old(f.get(0)) && gc::is_old(f.get(99)));
            assert_eq!(gc::live_objects(), live0 + 100);
        } // the frame drops: the 100 old objects are now unreachable garbage
        gc::collect(); // minor: old garbage is retained
        assert_eq!(gc::live_objects(), live0 + 100, "a minor collection keeps old garbage");
        gc::collect_full(); // full: old garbage reclaimed
        assert_eq!(gc::live_objects(), live0, "a full collection reclaims old garbage");
    }

    #[test]
    fn generational_write_barrier_preserves_old_to_young_edges() {
        let _g = serial();
        rj_init();
        // An old mutable cell with two reference fields.
        let cty = types::define_struct("Cell2", types::builtin(id::ANY), &[("a", types::builtin(id::ANY)), ("b", types::builtin(id::ANY))], true);
        let cell = Rooted::new(object::alloc(cty, 8));
        object::set_ref(cell.get(), 0, Value(types::nothing_instance()));
        object::set_ref(cell.get(), 4, Value(types::nothing_instance()));
        gc::collect(); // promote the cell (and its type) to old
        assert!(gc::is_old(cell.get()));

        // Store a freshly allocated YOUNG object into the OLD cell. The barrier
        // in set_ref records this old->young edge; without it the next minor
        // collection would not trace the old cell and would free the child.
        let child = box_int(777);
        assert!(!gc::is_old(child));
        object::set_ref(cell.get(), 0, child);

        gc::collect(); // minor
        assert_eq!(
            unbox_int(object::get_ref(cell.get(), 0)),
            777,
            "the write barrier kept the old->young reference alive",
        );
    }

    #[test]
    fn freed_chunks_are_reused() {
        let _g = serial();
        rj_init();
        gc::collect_full(); // clear bootstrap/cache-growth garbage first

        // Fill, collect, then refill with same-size objects: the region's
        // high-water mark must not grow, proving the free list is reused.
        for _ in 0..40 {
            let _ = box_int(7);
        }
        let high_water = region::used();
        assert_eq!(gc::collect(), 40);
        for _ in 0..40 {
            let _ = box_int(8);
        }
        assert_eq!(region::used(), high_water, "same-size allocations should reuse freed chunks");
    }

    #[test]
    fn collection_preserves_the_type_graph() {
        let _g = serial();
        rj_init();
        // Collecting with only garbage present must not disturb the bootstrapped
        // types: typeof/subtyping/sizes still hold afterwards.
        for i in 0..100 {
            let _ = box_int(i);
        }
        gc::collect();
        assert_eq!(rj_typeof(types::builtin(id::DATATYPE)), types::builtin(id::DATATYPE));
        assert!(types::issubtype(types::builtin(id::INT64), types::builtin(id::NUMBER)));
        assert_eq!(types::size_of(types::builtin(id::FLOAT64)), 8);
        assert_eq!(symbol::len(types::type_sym(types::builtin(id::INT64))), 5);
    }

    #[test]
    fn auto_collection_keeps_the_runtime_bounded() {
        let _g = serial();
        rj_init();
        // Far more allocation than the ~1 MiB region holds. Without
        // auto-collection this would exhaust the region; with precise rooting, a
        // collection triggered mid-evaluation must not disturb the interpreter's
        // working set, so the result stays correct.
        assert_eq!(rj_interp_sum_to(50_000), 1_250_025_000);
        // Raw garbage far beyond region capacity is reclaimed continuously.
        rj_alloc_garbage(300_000);
        assert!(gc::live_objects() < 100_000, "auto-collection should keep the heap bounded");
    }

    #[test]
    fn tuple_types_are_uniqued() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);

        // Structurally identical tuple types are the same object (===).
        let a = types::tuple_type(&[t(id::INT64), t(id::INT64)]);
        let b = types::tuple_type(&[t(id::INT64), t(id::INT64)]);
        assert_eq!(a, b, "identical tuple types must be ===");
        let c = types::tuple_type(&[t(id::INT64), t(id::FLOAT64)]);
        assert_ne!(a, c, "different tuple types are distinct");

        // Uniquing is recursive: a nested tuple resolves to the same object.
        let o1 = types::tuple_type(&[types::tuple_type(&[t(id::INT64)]), t(id::INT64)]);
        let o2 = types::tuple_type(&[types::tuple_type(&[t(id::INT64)]), t(id::INT64)]);
        assert_eq!(o1, o2, "nested tuple types unique recursively");

        // The cache survives collection. After a full GC the cache is old; a
        // minor GC keeps it, and the write barrier covers fresh insertions into
        // the now-old TypeName.
        gc::collect_full();
        for _ in 0..30 {
            let _ = box_int(0);
        }
        gc::collect();
        assert_eq!(types::tuple_type(&[t(id::INT64), t(id::INT64)]), a, "the type cache survives GC");
    }

    #[test]
    fn parametric_types_are_invariant() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);

        // Identical instantiations are the same object (uniqued) and reflexive.
        let bi = types::box_type(t(id::INT64));
        assert_eq!(bi, types::box_type(t(id::INT64)), "identical Box instantiations are uniqued");
        assert!(types::issubtype(bi, bi));

        // Invariance: even though Int64 <: Integer, the boxes are unrelated.
        let bint = types::box_type(t(id::INTEGER));
        assert!(types::issubtype(t(id::INT64), t(id::INTEGER)), "precondition: Int64 <: Integer");
        assert!(!types::issubtype(bi, bint), "Box is invariant: Box-of-Int64 not a subtype of Box-of-Integer");
        assert!(!types::issubtype(bint, bi), "nor the reverse");

        // But a box is still a subtype of Any, and a covariant tuple of invariant
        // boxes composes the two rules correctly.
        assert!(types::issubtype(bi, t(id::ANY)));
        let tup_i = types::tuple_type(&[bi]);
        let tup_int = types::tuple_type(&[bint]);
        assert!(types::issubtype(tup_i, tup_i));
        assert!(!types::issubtype(tup_i, tup_int), "a tuple of invariant boxes is not a subtype");
    }

    #[test]
    fn parametric_subtyping_tuples_unions_bottom() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let bottom = t(id::BOTTOM);

        // Bottom is below everything; only Bottom is below Bottom.
        assert!(types::issubtype(bottom, t(id::INT64)));
        assert!(!types::issubtype(t(id::INT64), bottom));
        assert!(types::issubtype(bottom, bottom));

        // Covariant tuples.
        let ii = types::tuple_type(&[t(id::INT64), t(id::INT64)]);
        let ir = types::tuple_type(&[t(id::INTEGER), t(id::REAL)]);
        assert!(types::issubtype(ii, ir)); // Int<:Integer, Int<:Real
        let fi = types::tuple_type(&[t(id::FLOAT64), t(id::INT64)]);
        assert!(!types::issubtype(fi, ir)); // Float64 not <: Integer
        let one = types::tuple_type(&[t(id::INT64)]);
        assert!(!types::issubtype(one, ii)); // length mismatch
        assert!(types::issubtype(ii, ii)); // reflexive over distinct objects

        // Unions: forall on the left, exists on the right.
        let u = types::union_type(t(id::INT64), t(id::FLOAT64));
        assert!(types::issubtype(t(id::INT64), u)); // Int <: Union{Int,Float64}
        assert!(types::issubtype(u, t(id::REAL))); // both members <: Real
        assert!(types::issubtype(u, u)); // union reflexive
        let uic = types::union_type(t(id::INT64), t(id::CHAR));
        assert!(!types::issubtype(uic, t(id::REAL))); // Char not <: Real

        // Union simplification.
        assert_eq!(types::union_type(bottom, t(id::INT64)), t(id::INT64));
        assert_eq!(types::union_type(t(id::INT64), t(id::INT64)), t(id::INT64));

        // Tuples nest with unions, covariantly.
        let nested = types::tuple_type(&[t(id::INT64), types::union_type(t(id::REAL), t(id::CHAR))]);
        assert!(types::issubtype(ii, nested));
    }

    // Union normalization (jl_type_union): flatten nested unions, drop members
    // subsumed by another, and order canonically so the result is independent of
    // argument order.
    #[test]
    fn unions_are_normalized() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);

        // A member subsumed by another is dropped: Union{Int,Real} == Real.
        assert_eq!(types::union_type(t(id::INT64), t(id::REAL)), t(id::REAL));
        assert_eq!(types::union_type(t(id::REAL), t(id::INT64)), t(id::REAL));
        // Everything collapses to Any; nothing collapses to Union{}.
        assert_eq!(types::union_type(t(id::INT64), t(id::ANY)), t(id::ANY));
        assert_eq!(types::union_of(&[t(id::BOTTOM), t(id::BOTTOM)]), t(id::BOTTOM));

        // Nested unions flatten and dedup: Union{Int, Union{Float64, Int}} has
        // exactly the two distinct members Int and Float64.
        let inner = types::union_type(t(id::FLOAT64), t(id::INT64));
        let outer = types::union_type(t(id::INT64), inner);
        assert!(types::is_union(outer));
        assert!(!types::is_union(types::union_a(outer)), "a flat union has non-union members");
        assert!(!types::is_union(types::union_b(outer)));
        assert!(types::issubtype(outer, types::union_type(t(id::INT64), t(id::FLOAT64))));
        assert!(types::issubtype(types::union_type(t(id::INT64), t(id::FLOAT64)), outer));

        // Order independence: Union{Int,Float64} and Union{Float64,Int} build the
        // same canonical member order.
        let u1 = types::union_type(t(id::INT64), t(id::FLOAT64));
        let u2 = types::union_type(t(id::FLOAT64), t(id::INT64));
        assert_eq!(types::union_a(u1), types::union_a(u2));
        assert_eq!(types::union_b(u1), types::union_b(u2));

        assert_eq!(gc::root_count(), 0, "roots balanced after union normalization");
    }

    // jl_egal / jl_types_egal: identity, payload bits, structure, and the
    // tvar_names asymmetry between `===` and types-equal.
    #[test]
    fn egal_and_types_egal() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let eg = |a: Value, b: Value| builtins::egal(a, b);

        // Distinct boxes, equal bits: egal compares the payload, not identity.
        let (a, b) = (box_int(5), box_int(5));
        assert_ne!(a, b, "two boxes are distinct objects");
        assert!(eg(a, b));
        assert!(!eg(box_int(5), box_int(6)));
        // Type tags must match: Int64(1) is not Bool(true) or Float64.
        assert!(!eg(box_int(1), value::box_bool(true)));
        assert!(!eg(box_int(0), value::box_float64(0.0)));

        // Bitwise float semantics — where `===` and `==` disagree:
        // NaN === NaN (equal bits) though NaN == NaN is false …
        assert!(eg(value::box_float64(f64::NAN), value::box_float64(f64::NAN)));
        assert!(!intrinsics::eq_float(f64::NAN, f64::NAN));
        // … and -0.0 !== 0.0 (different bits) though -0.0 == 0.0 is true.
        assert!(!eg(value::box_float64(-0.0), value::box_float64(0.0)));
        assert!(intrinsics::eq_float(-0.0, 0.0));

        // Identity-only kinds: the permboxes and `nothing`.
        assert!(eg(value::box_bool(true), value::box_bool(true)));
        assert!(!eg(value::box_bool(true), value::box_bool(false)));
        assert!(eg(value::nothing(), value::nothing()));

        // Types: uniqued instantiations are identical; distinct instantiations
        // differ; structurally equal unions built separately are egal even
        // though they are distinct objects (Julia does not intern unions).
        assert!(eg(Value(types::box_type(t(id::INT64))), Value(types::box_type(t(id::INT64)))));
        assert!(!eg(Value(types::box_type(t(id::INT64))), Value(types::box_type(t(id::INTEGER)))));
        let u1 = types::union_type(t(id::INT64), t(id::NOTHING));
        let u2 = types::union_type(t(id::NOTHING), t(id::INT64));
        assert!(eg(Value(u1), Value(u2)));

        // The tvar_names asymmetry: (Tuple{T,T} where T) vs (Tuple{R,R} where R)
        // is types_egal (alpha-equivalent) but NOT === (names matter to egal).
        let wh = |name: &str| {
            let v = types::make_typevar(name, t(id::BOTTOM), t(id::ANY));
            types::unionall_type(v, types::tuple_type(&[v, v]))
        };
        let (wt, wr) = (wh("T"), wh("R"));
        assert!(builtins::types_egal(wt, wr));
        assert!(!eg(Value(wt), Value(wr)));
        // Same name, separately built: both equal.
        let wt2 = wh("T");
        assert!(builtins::types_egal(wt, wt2));
        assert!(eg(Value(wt), Value(wt2)));
        // Free typevars: equal only to themselves.
        let (fv1, fv2) = (types::make_typevar("X", t(id::BOTTOM), t(id::ANY)),
                          types::make_typevar("X", t(id::BOTTOM), t(id::ANY)));
        assert!(builtins::types_egal(fv1, fv1));
        assert!(!builtins::types_egal(fv1, fv2));

        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    // jl_datatype_t.instance, the Bool permboxes, and union_sort_cmp's tiers.
    #[test]
    fn singletons_and_bool_permboxes() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);

        // `nothing` is Nothing's `instance`; Nothing is a singleton type, Bool
        // and Integer are not.
        assert_eq!(types::nothing_instance(), types::instance_of(t(id::NOTHING)));
        assert!(types::is_datatype_singleton(t(id::NOTHING)));
        assert!(!types::is_datatype_singleton(t(id::BOOL)));
        assert!(!types::is_datatype_singleton(t(id::INTEGER)));

        // jl_box_bool: boxing returns the jl_true/jl_false permboxes — the
        // same object every time, never a fresh allocation.
        assert_eq!(value::box_bool(true), value::box_bool(true));
        assert_eq!(value::box_bool(false), value::box_bool(false));
        assert_ne!(value::box_bool(true), value::box_bool(false));
        assert!(value::unbox_bool(value::box_bool(true)));
        assert!(!value::unbox_bool(value::box_bool(false)));
        assert_eq!(type_of(value::box_bool(true)), t(id::BOOL));

        // A zero-field immutable struct is a singleton with an eager instance.
        let unit = types::define_struct("UnitLike", t(id::ANY), &[], false);
        assert!(types::is_datatype_singleton(unit));
        assert_eq!(type_of(object::Value(types::instance_of(unit))), unit);

        // union_sort_cmp tiers: the singleton Nothing sorts before the isbits
        // Int64 (alphabetically Int64 < Nothing — only the tier explains this
        // order), and isbits Int64 sorts before the non-isbits Box{Int64}.
        let u = types::union_type(t(id::INT64), t(id::NOTHING));
        assert_eq!(types::union_a(u), t(id::NOTHING));
        assert_eq!(types::union_b(u), t(id::INT64));
        let v = types::union_type(types::box_type(t(id::INT64)), t(id::INT64));
        assert_eq!(types::union_a(v), t(id::INT64));

        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    // The `where` machinery (UnionAll/TypeVar) and the environment-based
    // subtype algorithm: a variable bound on the right is existential (∃), on
    // the left universal (∀), exactly as in subtype.c's `subtype_unionall`.
    #[test]
    fn where_types_unionall_and_typevars() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let any = t(id::ANY);
        let bottom = t(id::BOTTOM);

        // `Box{T} where T` — an unbounded existential when on the right.
        let var = |lb, ub| types::make_typevar("T", lb, ub);
        let unbounded = || {
            let tv = var(bottom, any);
            types::unionall_type(tv, types::box_type(tv))
        };

        // Box{Int} <: (Box{T} where T)   — ∃T, true (T = Int).
        assert!(types::issubtype(types::box_type(t(id::INT64)), unbounded()));
        // (Box{T} where T) <: Box{Int}   — ∀T, false (T need not be Int).
        assert!(!types::issubtype(unbounded(), types::box_type(t(id::INT64))));
        // A `where` type is still a subtype of Any.
        assert!(types::issubtype(unbounded(), any));

        // Bounded vars and invariant matching across two `where`s.
        // (Box{T} where T<:Integer) <: (Box{S} where S<:Number)  — true.
        let lhs_ti = {
            let tv = var(bottom, t(id::INTEGER));
            types::unionall_type(tv, types::box_type(tv))
        };
        let rhs_sn = {
            let sv = var(bottom, t(id::NUMBER));
            types::unionall_type(sv, types::box_type(sv))
        };
        assert!(types::issubtype(lhs_ti, rhs_sn));
        // (Box{T} where T<:Number) <: (Box{S} where S<:Integer)  — false
        // (T could be Float64, with no matching S<:Integer).
        let lhs_tn = {
            let tv = var(bottom, t(id::NUMBER));
            types::unionall_type(tv, types::box_type(tv))
        };
        let rhs_si = {
            let sv = var(bottom, t(id::INTEGER));
            types::unionall_type(sv, types::box_type(sv))
        };
        assert!(!types::issubtype(lhs_tn, rhs_si));

        // Two distinct *free* type variables are never subtypes, regardless of
        // how permissive their declared bounds are (subtype.c returns 0
        // unconditionally when both sides are free singletons; audit finding 10).
        let free_x = var(bottom, t(id::INT64)); // ub would satisfy lb below
        let free_y = var(t(id::NUMBER), any);
        assert!(!types::issubtype(free_x, free_y));
        assert!(!types::issubtype(var(bottom, any), var(bottom, any)));

        // Covariant tuple under a `where`: Tuple{Int,Float64} <: (Tuple{T,S} where S where T).
        let tv = var(bottom, any);
        let sv = var(bottom, any);
        let inner = types::tuple_type(&[tv, sv]);
        let two_var_tuple = types::unionall_type(tv, types::unionall_type(sv, inner));
        let if_ = types::tuple_type(&[t(id::INT64), t(id::FLOAT64)]);
        assert!(types::issubtype(if_, two_var_tuple));

        // The diagonal rule: a variable occurring twice, only covariantly, is
        // constrained to concrete types. Tuple{Int,Int} <: (Tuple{T,T} where T)
        // is true (T = Int, concrete), but Tuple{Int,Float64} <: (Tuple{T,T}
        // where T) is false — the join Union{Int,Float64} is not a leaf type.
        let diag = || {
            let dv = var(bottom, any);
            types::unionall_type(dv, types::tuple_type(&[dv, dv]))
        };
        let ii = types::tuple_type(&[t(id::INT64), t(id::INT64)]);
        assert!(types::issubtype(ii, diag()), "homogeneous tuple satisfies the diagonal var");
        let i_f = types::tuple_type(&[t(id::INT64), t(id::FLOAT64)]);
        assert!(!types::issubtype(i_f, diag()), "heterogeneous tuple fails the diagonal rule");
        // (The two-distinct-vars case `Tuple{Int,Float64} <: Tuple{T,S}` above is
        // unaffected: each variable occurs once, so neither is diagonal.)

        // Two existential vars live in the environment simultaneously, each
        // bound through an invariant Box (exercising invdepth/depth0): each
        // resolves independently to its concrete element, so this holds.
        // Tuple{Box{Int},Box{Float64}} <: (Tuple{Box{T},Box{S}} where S where T).
        let tvb = var(bottom, any);
        let svb = var(bottom, any);
        let inner_boxes = types::tuple_type(&[types::box_type(tvb), types::box_type(svb)]);
        let two_box = types::unionall_type(tvb, types::unionall_type(svb, inner_boxes));
        let concrete_boxes = types::tuple_type(&[types::box_type(t(id::INT64)), types::box_type(t(id::FLOAT64))]);
        assert!(types::issubtype(concrete_boxes, two_box), "independent existentials each bind their box element");
        // But a single shared var cannot match two different box elements:
        // Tuple{Box{Int},Box{Float64}} NOT <: (Tuple{Box{T},Box{T}} where T).
        let uvb = var(bottom, any);
        let shared_box = types::unionall_type(uvb, types::tuple_type(&[types::box_type(uvb), types::box_type(uvb)]));
        assert!(!types::issubtype(concrete_boxes, shared_box), "one var cannot equal two distinct box elements");

        assert_eq!(gc::root_count(), 0, "roots balanced after where-subtyping");
    }

    // Two cases from JuliaLang/julia's test/subtype.jl that the oracle harness
    // (runtime/verify_julia_subtype.mjs) initially failed, pinned here as
    // regressions. Ref{T} maps to Box{T} (both single-parameter invariant).
    #[test]
    fn oracle_regressions_from_subtype_jl() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let (bottom, any) = (t(id::BOTTOM), t(id::ANY));
        let var = |lb, ub| types::make_typevar("T", lb, ub);
        let ua = types::unionall_type;
        let tup = |a, b| types::tuple_type(&[a, b]);

        // L179: Tuple{Integer,Int} <: (Tuple{T, S} where S<:T where T). The
        // occurrence of T inside S's bound must not make T diagonal (cov_diag).
        let big = var(bottom, any);
        let small = var(bottom, big); // S <: T
        let l179 = ua(big, ua(small, tup(big, small)));
        assert!(types::issubtype(tup(t(id::INTEGER), t(id::INT64)), l179));

        // L496: Tuple{Ref{Int},Ref{Integer}} is NOT <: (Tuple{Ref{S},Ref{T}}
        // where S>:T where T). simple_join must keep the S>:T link rather than
        // collapse it, so the contradiction (S=Int, T=Integer) is caught.
        let tt = var(bottom, any);
        let ss = var(tt, any); // S >: T
        let l496 = ua(tt, ua(ss, tup(types::box_type(ss), types::box_type(tt))));
        let lhs = tup(types::box_type(t(id::INT64)), types::box_type(t(id::INTEGER)));
        assert!(!types::issubtype(lhs, l496));

        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    #[test]
    fn parametric_types_survive_collection() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        // Root a tuple type across a collection; its parameters svec must survive.
        let tup = Rooted::new(Value(types::tuple_type(&[t(id::INT64), t(id::REAL)])));
        for i in 0..50 {
            let _ = box_int(i); // garbage
        }
        gc::collect();
        // The tuple type and its parameters are intact: subtyping still resolves.
        let ii = types::tuple_type(&[t(id::INT64), t(id::INT64)]);
        assert!(types::issubtype(ii, tup.get().raw()));
    }

    #[test]
    fn typenames_identify_types_and_are_shared_by_tuples() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);

        // A type's `name` is a TypeName object (typed TypeName) holding its symbol.
        let tn = types::name_of(t(id::INT64));
        assert_eq!(type_of(Value(tn)), t(id::TYPENAME));
        assert_eq!(symbol::len(types::typename_sym(tn)), 5); // "Int64"

        // Distinct nominal types have distinct TypeNames.
        assert_ne!(types::name_of(t(id::INT64)), types::name_of(t(id::FLOAT64)));

        // Every tuple type shares the one `Tuple` TypeName (jl_tuple_typename).
        let t1 = types::tuple_type(&[t(id::INT64)]);
        let t2 = types::tuple_type(&[t(id::INT64), t(id::REAL)]);
        assert_eq!(types::name_of(t1), types::name_of(t2));
        assert!(types::is_tuple(t1) && types::is_tuple(t2));
        assert!(!types::is_tuple(t(id::INT64)));

        // TypeNames (and the symbols they hold) survive collection.
        for i in 0..30 {
            let _ = box_int(i);
        }
        gc::collect();
        assert_eq!(symbol::len(types::type_sym(t(id::INT64))), 5);
    }

    #[test]
    fn frontend_runs_julia_source() {
        let _g = serial();
        rj_init();
        let run = |s: &str| unbox_int(frontend::eval_source(s).unwrap());

        // Arithmetic with precedence and parentheses.
        assert_eq!(run("1 + 2 * 3"), 7);
        assert_eq!(run("(1 + 2) * 3"), 9);
        assert_eq!(run("10 - 3 - 2"), 5); // left-associative
        assert_eq!(run("-(3 + 4)"), -7); // unary minus

        // Variables and assignment; the program returns its last expression.
        assert_eq!(run("x = 10\ny = 20\nx + y"), 30);

        // if / elseif / else selecting a branch.
        assert_eq!(run("x = 5\nif x < 0\ns = 1\nelseif x < 10\ns = 2\nelse\ns = 3\nend\ns"), 2);
        assert_eq!(run("x = 50\nif x < 10\ns = 1\nelse\ns = 2\nend\ns"), 2);

        // A real loop from source: sum(1:100).
        assert_eq!(
            run("acc = 0\ni = 1\nwhile i <= 100\nacc = acc + i\ni = i + 1\nend\nacc"),
            5050
        );

        // `===` from source goes through the egal builtin, not numeric `==`.
        let runb = |s: &str| value::unbox_bool(frontend::eval_source(s).unwrap());
        assert!(runb("x = 7\nx === 7"));
        assert!(!runb("1 === 2"));
        assert!(runb("1.5 === 1.5"));
        assert!(!runb("1 === 1.5")); // different type tags, never equal
    }

    // Intrinsics breadth from source: ÷ % / bitwise shifts, precedence, and
    // the DivideError path.
    #[test]
    fn frontend_runs_integer_and_float_operators() {
        let _g = serial();
        rj_init();
        let run = |s: &str| unbox_int(frontend::eval_source(s).unwrap());
        let runf = |s: &str| value::unbox_float64(frontend::eval_source(s).unwrap());

        assert_eq!(run("7 \u{f7} 2"), 3); // ÷ truncates
        assert_eq!(run("-7 \u{f7} 2"), -3);
        assert_eq!(run("7 % 2"), 1);
        assert_eq!(run("-7 % 2"), -1); // sign of the dividend
        assert_eq!(run("6 & 3"), 2);
        assert_eq!(run("6 | 3"), 7);
        assert_eq!(run("6 \u{22bb} 3"), 5); // ⊻ xor
        assert_eq!(run("1 << 10"), 1024);
        assert_eq!(run("-8 >> 1"), -4); // arithmetic: sign-fill
        assert_eq!(run("-8 >>> 60"), 15); // logical: zero-fill
        assert_eq!(run("1 << 64"), 0); // count >= width

        // Precedence: `&` multiplicative, `|` additive, shifts tighter than `*`.
        assert_eq!(run("4 | 2 + 1"), 7); // 4 | (2+1)
        assert_eq!(run("6 & 3 + 1"), 3); // (6 & 3) + 1 — & binds tighter than +
        assert_eq!(run("2 * 1 << 3"), 16); // 2 * (1<<3)

        // `/` always produces Float64, converting integer operands (Julia's
        // Int / Int promotion).
        assert_eq!(runf("1 / 2"), 0.5);
        assert_eq!(runf("7.0 / 2.0"), 3.5);
        assert_eq!(runf("5.5 % 2.0"), 1.5); // float % is fmod

        // DivideError surfaces as an eval error (no exceptions yet).
        assert!(frontend::eval_source("1 \u{f7} 0").is_err());
        assert!(frontend::eval_source("x = 5\nx % 0").is_err());
    }

    // Struct slice 1: computed field layout (inline isbits vs references),
    // new/getfield/setfield!, mutability, and the GC pointer bitmap.
    #[test]
    fn structs_layout_construction_and_field_access() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);

        // Immutable Point{Int64, Int64}: both fields inline, no GC pointers.
        let point = types::define_struct(
            "PointII",
            t(id::ANY),
            &[("x", t(id::INT64)), ("y", t(id::INT64))],
            false,
        );
        assert_eq!(types::nfields_of(point), 2);
        assert_eq!(types::size_of(point), 16); // two inline 8-byte fields
        assert_eq!(types::layout_npointers(point), 0);
        assert!(!types::field_isptr(point, 0) && !types::field_isptr(point, 1));
        assert_eq!(types::field_offset(point, 1), 8);

        let p = types::new_struct(point, &[box_int(3), box_int(4)]).unwrap();
        assert_eq!(type_of(p), point);
        // getfield re-boxes the inline bits as the declared field type.
        assert_eq!(unbox_int(types::get_nth_field(p, 0).unwrap()), 3);
        assert_eq!(unbox_int(types::get_nth_field(p, 1).unwrap()), 4);
        assert_eq!(type_of(types::get_nth_field(p, 0).unwrap()), t(id::INT64));
        // Field lookup by name (interned-symbol identity).
        let ysym = symbol::intern(t(id::SYMBOL), "y");
        assert_eq!(types::field_index(point, ysym), Some(1));
        // setfield! on an immutable struct is an error.
        assert!(types::set_nth_field(p, 0, box_int(9)).is_err());

        // Mutable struct with an abstract-typed field: stored as a reference,
        // visible in the GC bitmap, and assignable through the barrier.
        let cell = types::define_struct("CellLike", t(id::ANY), &[("v", t(id::INTEGER))], true);
        assert_eq!(types::layout_npointers(cell), 1);
        assert!(types::field_isptr(cell, 0));
        let c_root = gc::Rooted::new(types::new_struct(cell, &[box_int(7)]).unwrap());
        assert_eq!(unbox_int(types::get_nth_field(c_root.get(), 0).unwrap()), 7);
        types::set_nth_field(c_root.get(), 0, box_int(8)).unwrap();
        assert_eq!(unbox_int(types::get_nth_field(c_root.get(), 0).unwrap()), 8);
        // The declared type is enforced: a Float64 is not an Integer.
        assert!(types::set_nth_field(c_root.get(), 0, value::box_float64(1.0)).is_err());
        // The reference field survives a collection (bitmap-driven tracing).
        gc::collect();
        assert_eq!(unbox_int(types::get_nth_field(c_root.get(), 0).unwrap()), 8);

        // Arity and isa checks at construction.
        assert!(types::new_struct(point, &[box_int(1)]).is_err());
        assert!(types::new_struct(point, &[box_int(1), value::box_float64(2.0)]).is_err());

        // Mixed layout: inline Bool (1 byte) then a reference — alignment puts
        // the reference at offset 4.
        let mixed = types::define_struct(
            "MixedBF",
            t(id::ANY),
            &[("flag", t(id::BOOL)), ("val", t(id::NUMBER))],
            true,
        );
        assert_eq!(types::field_offset(mixed, 0), 0);
        assert_eq!(types::field_offset(mixed, 1), 4);
        let m = gc::Rooted::new(
            types::new_struct(mixed, &[value::box_bool(true), value::box_float64(2.5)]).unwrap(),
        );
        assert!(value::unbox_bool(types::get_nth_field(m.get(), 0).unwrap()));
        assert_eq!(value::unbox_float64(types::get_nth_field(m.get(), 1).unwrap()), 2.5);

        drop(m);
        drop(c_root);

        // Reference-verified alignment cases (datatype.c:735–833): a nested
        // struct aligns to its *fields'* max alignment, not its size — and
        // the total size pads to the struct alignment.
        let inner = types::define_struct(
            "Inner32x2",
            t(id::ANY),
            &[("a", t(id::INT32)), ("b", t(id::INT32))],
            false,
        );
        assert_eq!(types::size_of(inner), 8);
        let outer = types::define_struct(
            "OuterI8Inner",
            t(id::ANY),
            &[("tag", t(id::INT8)), ("inner", inner)],
            false,
        );
        // Inner{Int32,Int32} has alignment 4 (not 8): it sits at offset 4.
        assert_eq!(types::field_offset(outer, 1), 4);
        assert_eq!(types::size_of(outer), 12);
        // {Int64, Bool} is 9 bytes of fields, 16 bytes of struct.
        let padded = types::define_struct(
            "PaddedI64B",
            t(id::ANY),
            &[("n", t(id::INT64)), ("flag", t(id::BOOL))],
            false,
        );
        assert_eq!(types::size_of(padded), 16);
        // Nested inline construction round-trips through the exact copy size.
        let iv = types::new_struct(inner, &[value::box_int32(11), value::box_int32(22)]).unwrap();
        let ov_root = gc::Rooted::new(
            types::new_struct(outer, &[value::box_int8(1), iv]).unwrap(),
        );
        let got_inner = types::get_nth_field(ov_root.get(), 1).unwrap();
        assert_eq!(type_of(got_inner), inner);
        assert_eq!(value::unbox_int32(types::get_nth_field(got_inner, 0).unwrap()), 11);
        assert_eq!(value::unbox_int32(types::get_nth_field(got_inner, 1).unwrap()), 22);

        drop(ov_root);
        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    // The exact generational state machine (gc-stock.c:164–191): the
    // promotion-completion scan, the OLD_MARKED-only barrier with
    // queue_root's refire guard, and the full-sweep demotion lag.
    #[test]
    fn gc_exact_state_machine() {
        let _g = serial();
        rj_init();
        let t = |i| types::builtin(i);
        let cell = types::define_struct("GCCell", t(id::ANY), &[("v", t(id::ANY))], true);
        const CLEAN: u32 = 0;
        const MARKED: u32 = 1;
        const OLD: u32 = 2;
        const OLD_MARKED: u32 = 3;

        let c = Rooted::new(types::new_struct(cell, &[value::nothing()]).unwrap());
        assert_eq!(object::gc_bits(c.get()), CLEAN);
        gc::collect();
        assert_eq!(object::gc_bits(c.get()), OLD, "young survivor promotes");

        // A store into a merely-OLD parent does not fire the barrier — its
        // promotion-completion scan at the next mark covers the child.
        let x = types::new_struct(cell, &[value::nothing()]).unwrap(); // young
        types::set_nth_field(c.get(), 0, x).unwrap();
        assert_eq!(gc::remset_len(), 0, "no barrier for an OLD(2) parent");
        // The next minor mark performs that scan: 2 → 3, child traced and
        // kept. (The pre-exactness machine reached this child too, but via a
        // conservatively over-firing any-old barrier; now the barrier stays
        // silent and the promotion-completion scan — which it lacked — is
        // what keeps the child alive.)
        // The promotion scan (2 → 3 + trace) keeps x alive and, having seen
        // young x, pushes c for the next cycle (gc_mark_push_remset:
        // nptr == 0x3). The post-quick-sweep pass then puts c back in the
        // *queued* state, GC_MARKED, so the barrier won't refire on it
        // (gc-stock.c:3405–3414).
        gc::collect();
        assert_eq!(object::gc_bits(c.get()), MARKED, "remset entry re-queued post-sweep");
        let x_now = types::get_nth_field(c.get(), 0).unwrap();
        assert_eq!(object::gc_bits(x_now), OLD, "child survived the minor and promoted");
        assert_eq!(gc::remset_len(), 1, "scan rebuilt the remset conservatively");

        // Stores into a queued (MARKED) parent are barrier-silent — its
        // remset entry already covers it.
        let y = types::new_struct(cell, &[value::nothing()]).unwrap();
        types::set_nth_field(c.get(), 0, y).unwrap();
        assert_eq!(gc::remset_len(), 1, "no barrier while queued");
        let z = types::new_struct(cell, &[value::nothing()]).unwrap();
        types::set_nth_field(c.get(), 0, z).unwrap();
        assert_eq!(gc::remset_len(), 1);

        // The minor collection restores the entry to OLD_MARKED, traces it
        // (keeping z, reachable only through c), re-pushes it (z young at
        // scan), and re-queues it post-sweep.
        gc::collect();
        assert_eq!(object::gc_bits(c.get()), MARKED, "re-queued: z was young at scan");
        assert_eq!(gc::remset_len(), 1);
        let z_now = types::get_nth_field(c.get(), 0).unwrap();
        assert_eq!(object::gc_bits(z_now), OLD);
        // Next cycle the child is old at scan time: the entry drops, and c —
        // restored to 3 and no longer re-queued — keeps OLD_MARKED.
        gc::collect();
        assert_eq!(gc::remset_len(), 0, "entry dropped once the child is old");
        assert_eq!(object::gc_bits(c.get()), OLD_MARKED);

        // NOW the barrier: a store into an OLD_MARKED parent fires exactly
        // once — queue_root re-tags 3 → 1, which is itself the refire guard.
        let w = types::new_struct(cell, &[value::nothing()]).unwrap();
        types::set_nth_field(c.get(), 0, w).unwrap();
        assert_eq!(gc::remset_len(), 1, "barrier fired on the OLD_MARKED parent");
        assert_eq!(object::gc_bits(c.get()), MARKED, "queue_root cleared the OLD bit");
        let w2 = types::new_struct(cell, &[value::nothing()]).unwrap();
        types::set_nth_field(c.get(), 0, w2).unwrap();
        assert_eq!(gc::remset_len(), 1, "barrier must not refire while queued");

        // A full sweep clears the remset outright (gc-stock.c:3415): its old
        // objects are demoted to OLD and rescanned at the next mark anyway.
        gc::collect_full();
        assert_eq!(gc::remset_len(), 0, "full sweep clears the remset");
        assert_eq!(object::gc_bits(c.get()), OLD, "full sweep demoted c");
        let w2_now = types::get_nth_field(c.get(), 0).unwrap();
        assert_eq!(object::gc_bits(w2_now), OLD, "child reached via the restored entry");

        // Old garbage at OLD_MARKED: quick sweeps never touch it; the first
        // full cycle demotes it (3 → 2, kept); the second frees it — the
        // documented one-cycle lag, as in Julia. Re-prove c to OLD_MARKED
        // via one more quick mark first.
        gc::collect();
        assert_eq!(object::gc_bits(c.get()), OLD_MARKED, "promotion scan re-proved c");
        drop(c);
        let live0 = gc::live_objects();
        gc::collect();
        assert_eq!(gc::live_objects(), live0, "quick sweep keeps OLD_MARKED garbage");
        gc::collect_full(); // demotes the unreached 3 to 2 (kept); frees unreached 2s
        let live1 = gc::live_objects();
        gc::collect_full(); // the demoted 2 is unmarked now: freed
        assert!(gc::live_objects() < live1, "second full cycle frees demoted old garbage");

        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    // The big-object path (gc-stock.c:436–465, :495–560): allocation past
    // the largest size class, the young/oldest bigval generations, block
    // recycling, and the full-sweep demote-and-merge.
    #[test]
    fn gc_big_objects() {
        let _g = serial();
        rj_init();
        let blob_t = types::builtin(id::SYMBOL); // layout-free blob, GC-safe

        // Unrooted big garbage dies at the next collection; its block recycles
        // into the next big allocation instead of carving fresh region space.
        let g = object::alloc(blob_t, 2400); // total 2408 > 2032: big path
        assert!(!g.is_null());
        gc::collect();
        let used1 = region::used();
        let g2 = object::alloc(blob_t, 2400);
        assert!(!g2.is_null());
        assert_eq!(region::used(), used1, "freed big block was recycled");

        // A rooted big object walks the generational lists: promote, settle
        // on the oldest list (quick sweeps skip it), demote-and-merge on a
        // full sweep, die once dropped.
        let b = Rooted::new(object::alloc(blob_t, 4096));
        gc::collect(); // young-marked: promotes, stays on the young list
        assert_eq!(object::gc_bits(b.get()), 2, "promoted at sweep");
        gc::collect(); // promotion scan 2→3; quick sweep parks it on oldest
        assert_eq!(object::gc_bits(b.get()), 3);
        gc::collect(); // oldest list untouched by quick sweeps
        assert_eq!(object::gc_bits(b.get()), 3, "settled bigval skipped");
        gc::collect_full(); // demoted to OLD, merged back to the young list
        assert_eq!(object::gc_bits(b.get()), 2, "full sweep demotes and merges");
        let live0 = gc::live_objects();
        drop(b);
        gc::collect_full(); // unmarked OLD on the young list: freed
        assert!(gc::live_objects() < live0, "dropped bigval freed");
        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    // The page protocol (gc-stock.c:878–898): whole-page release on
    // !has_marked, page reuse across size classes, and the quick-sweep skip
    // of settled all-old pages.
    #[test]
    fn gc_page_protocol() {
        let _g = serial();
        rj_init();
        // Burn through more than a page of one size class as pure garbage.
        for i in 0..2_000 {
            let _ = box_int(i); // 16-byte class; a 16 KiB page holds 1024
        }
        gc::collect_full();
        assert!(gc::free_page_count() > 0, "fully-dead pages were released whole");

        // Pages holding young cells are walked; a settled all-old heap is
        // skipped entirely — quick sweeps touch zero pages.
        for i in 0..100 {
            let _ = box_int(i); // young garbage on some page
        }
        gc::collect();
        assert!(gc::pages_walked_last() > 0, "young pages must be walked");
        gc::collect();
        assert_eq!(
            gc::pages_walked_last(),
            0,
            "a settled all-old heap is skipped page-for-page"
        );
        assert!(gc::live_objects() > 0, "the skipped pages hold the live heap");
        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    // The collection policy (gc-stock.c:356, :3032, :3377–3400): proactive
    // heap-target trigger at allocation, overallocation-based target growth.
    #[test]
    fn gc_collection_policy() {
        let _g = serial();
        rj_init();
        // The target starts at the floor (default_collect_interval scaled to
        // the region: capacity/8).
        assert_eq!(gc::heap_target(), region::capacity() / 8);
        // Allocating past the target collects proactively: garbage dies with
        // no manual collect call and the heap never nears exhaustion.
        let live_before = gc::live_objects();
        for i in 0..20_000 {
            let _ = box_int(i as i64); // ~320 KiB of garbage vs a 128 KiB target
        }
        assert!(
            gc::live_objects() < live_before + 10_000,
            "proactive trigger collected the garbage"
        );
        assert!(
            gc::heap_size() < region::capacity() / 2,
            "heap stayed bounded without reaching exhaustion"
        );
        // Post-collection the target tracks the live size plus permitted growth.
        assert!(gc::heap_target() >= gc::heap_size());
        assert_eq!(gc::root_count(), 0, "roots balanced");
    }

    // Struct slice 2: `struct` syntax, constructor calls, and field access
    // from real Julia source through the front-end and interpreter.
    #[test]
    fn frontend_runs_struct_source() {
        let _g = serial();
        rj_init();
        let run = |s: &str| unbox_int(frontend::eval_source(s).unwrap());
        let runf = |s: &str| value::unbox_float64(frontend::eval_source(s).unwrap());

        // Immutable struct: typed fields inline; construct and read.
        assert_eq!(
            run("struct Point\nx::Int64\ny::Int64\nend\np = Point(3, 4)\np.x * p.x + p.y * p.y"),
            25
        );
        // Mutable struct: field assignment, used in a loop.
        assert_eq!(
            run("mutable struct Counter\nn::Int64\nend\nc = Counter(0)\ni = 1\nwhile i <= 10\nc.n = c.n + i\ni = i + 1\nend\nc.n"),
            55
        );
        // Untyped fields are Any (boxed references); floats flow through.
        assert_eq!(runf("mutable struct Box2\nv\nend\nb = Box2(1.5)\nb.v = b.v * 2.0\nb.v"), 3.0);
        // Nested structs and chained field access.
        assert_eq!(
            run("struct Inner\nk::Int64\nend\nstruct Outer\ni::Inner\nend\no = Outer(Inner(42))\no.i.k"),
            42
        );
        // Redefinition with identical shape is reuse; different shape errors.
        assert_eq!(run("struct Pt2\na::Int64\nend\nstruct Pt2\na::Int64\nend\nPt2(7).a"), 7);
        assert!(frontend::eval_source("struct Pt3\na::Int64\nend\nstruct Pt3\nb::Int64\nend").is_err());
        // setfield! on an immutable struct errors; unknown fields error.
        assert!(frontend::eval_source("struct Frozen\nv::Int64\nend\nf = Frozen(1)\nf.v = 2").is_err());
        assert!(frontend::eval_source("struct One\na::Int64\nend\nOne(1).b").is_err());
        // Construction type-checks against declared field types.
        assert!(frontend::eval_source("struct TypedF\na::Int64\nend\nTypedF(1.5)").is_err());
        // === on struct values is identity (compare_fields not yet ported).
        let runb = |s: &str| value::unbox_bool(frontend::eval_source(s).unwrap());
        assert!(runb("mutable struct MRef\nv::Int64\nend\nm = MRef(1)\nm === m"));
    }

    // The remaining primitive boxings round-trip, carry their type, and egal
    // by bits within a width but never across types.
    #[test]
    fn primitive_boxing_round_trips() {
        let _g = serial();
        rj_init();
        assert_eq!(value::unbox_int8(value::box_int8(-5)), -5);
        assert_eq!(value::unbox_int16(value::box_int16(-300)), -300);
        assert_eq!(value::unbox_int32(value::box_int32(70_000)), 70_000);
        assert_eq!(value::unbox_uint8(value::box_uint8(200)), 200);
        assert_eq!(value::unbox_uint16(value::box_uint16(60_000)), 60_000);
        assert_eq!(value::unbox_uint32(value::box_uint32(4_000_000_000)), 4_000_000_000);
        assert_eq!(value::unbox_uint64(value::box_uint64(u64::MAX)), u64::MAX);
        assert_eq!(value::unbox_float32(value::box_float32(1.5)), 1.5);
        assert_eq!(value::unbox_char(value::box_char(0x41)), 0x41);
        assert_eq!(type_of(value::box_int8(1)), types::builtin(id::INT8));
        assert_eq!(type_of(value::box_char(7)), types::builtin(id::CHAR));
        assert!(builtins::egal(value::box_int8(7), value::box_int8(7)));
        assert!(!builtins::egal(value::box_int8(7), value::box_uint8(7))); // types differ
    }

    #[test]
    fn frontend_runs_float_arithmetic() {
        let _g = serial();
        rj_init();
        let runf = |s: &str| value::unbox_float64(frontend::eval_source(s).unwrap());

        // Float literals, the typed intrinsics, and precedence.
        assert_eq!(runf("1.5 + 2.5"), 4.0);
        assert_eq!(runf("2.0 * 3.0"), 6.0);
        assert_eq!(runf("1.0 + 2.0 * 3.0"), 7.0);
        assert_eq!(runf("10.0 - 3.0 - 2.0"), 5.0);

        // A float loop (float comparison in the condition).
        assert_eq!(runf("x = 0.0\nwhile x < 3.0\nx = x + 1.0\nend\nx"), 3.0);

        // The result is a Float64, and integer source still yields an Int64.
        assert_eq!(type_of(frontend::eval_source("1.5 + 1.5").unwrap()), types::builtin(id::FLOAT64));
        assert_eq!(type_of(frontend::eval_source("2 + 3").unwrap()), types::builtin(id::INT64));
    }

    #[test]
    fn multiple_dispatch_selects_the_most_specific_method() {
        let _g = serial();
        rj_init();

        // classify: Int64 and Bool beat the abstract Integer overload.
        let xi = Rooted::new(box_int(7));
        assert_eq!(unbox_int(dispatch::invoke(F_CLASSIFY, &[xi.get()]).unwrap()), 20);
        let xb = Rooted::new(value::box_bool(true));
        assert_eq!(unbox_int(dispatch::invoke(F_CLASSIFY, &[xb.get()]).unwrap()), 30);

        // double uses its argument: 21 + 21 = 42.
        let x = Rooted::new(box_int(21));
        assert_eq!(unbox_int(dispatch::invoke(F_DOUBLE, &[x.get()]).unwrap()), 42);

        // combine: two-argument tuple dispatch, with partial applicability.
        let a = Rooted::new(box_int(1));
        let b = Rooted::new(box_int(2));
        assert_eq!(unbox_int(dispatch::invoke(F_COMBINE, &[a.get(), b.get()]).unwrap()), 2); // (Int64,Int64)
        // Tuple{Bool,Int64}: Bool is not <: Int64, so only (Integer,Integer) applies.
        let bb = Rooted::new(value::box_bool(true));
        assert_eq!(unbox_int(dispatch::invoke(F_COMBINE, &[bb.get(), b.get()]).unwrap()), 1);

        // Same selection, driven through the interpreter's CallGeneric path.
        assert_eq!(rj_call_classify_i64(7), 20);
        assert_eq!(rj_call_classify_bool(), 30);
        assert_eq!(rj_call_double(21), 42);
        assert_eq!(rj_call_combine(1, 2), 2);

        // Methods (their tuple signatures) survive collection.
        for i in 0..50 {
            let _ = box_int(i);
        }
        gc::collect();
        assert_eq!(rj_call_classify_i64(7), 20);
    }

    #[test]
    fn interpreter_runs_lowered_ir() {
        let _g = serial();
        rj_init();
        // Straight-line: (a + b) * c.
        assert_eq!(rj_interp_poly(2, 3, 4), 20);
        assert_eq!(rj_interp_poly(-1, 1, 100), 0);
        // Loop with control flow: sum(1:n).
        assert_eq!(rj_interp_sum_to(5), 15);
        assert_eq!(rj_interp_sum_to(100), 5050);
        assert_eq!(rj_interp_sum_to(1), 1);
        assert_eq!(rj_interp_sum_to(0), 0); // loop body never executes
        // Countdown loop (subtraction + equality).
        assert_eq!(rj_interp_count_down(7), 7);
        assert_eq!(rj_interp_count_down(0), 0);
        assert_eq!(gc::root_count(), 0, "interpreter frames must be released LIFO");
    }

    #[test]
    fn interpreter_churns_the_collector_safely() {
        let _g = serial();
        rj_init();
        // A survivor rooted across an interpreter run that allocates heavily.
        let survivor = Rooted::new(box_int(777));
        let before = gc::live_objects();
        assert_eq!(rj_interp_sum_to(200), 20100); // ~800 boxed allocations
        assert!(gc::live_objects() > before, "the interpreter produced garbage");

        // Reclaim it; the survivor and the type graph remain intact.
        gc::collect();
        assert_eq!(unbox_int(survivor.get()), 777);
        assert!(types::issubtype(types::builtin(id::INT64), types::builtin(id::NUMBER)));
        // A fresh run is still correct after collection.
        assert_eq!(rj_interp_sum_to(10), 55);
    }
}
