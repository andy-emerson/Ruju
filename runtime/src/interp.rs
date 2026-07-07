//! A minimal interpreter for Julia's lowered IR (`CodeInfo`).
//!
//! Executes a faithful subset of Julia's lowered statement forms via an
//! instruction-pointer loop, mirroring `eval_body` in `src/interpreter.c`:
//! `SlotNumber` locals, `SSAValue` results, `GotoNode`, `GotoIfNot`,
//! `ReturnNode`, and `:call` expressions. This is *lowered* CodeInfo, which uses
//! mutable slots rather than SSA phi nodes.
//!
//! `:call` to a builtin resolves directly; `:call` to a generic function
//! ([`Stmt::CallGeneric`]) goes through multiple dispatch. IR is constructed
//! in-process for now, since the JuliaSyntax -> JuliaLowering pipeline is not
//! wired in.
//!
//! The interpreter keeps its slots and SSA values in a single GC [`Frame`], so
//! the whole working set stays rooted across the allocation every boxing
//! operation performs.

use crate::dispatch;
use crate::gc::Frame;
use crate::object::{self, Value};
use crate::types::{self, id};
use crate::value::{box_bool, box_float64, box_int, unbox_bool, unbox_float64, unbox_int};
use intrinsics::{
    add_float, add_int, and_int, ashr_int, checked_sdiv_int, checked_srem_int, div_float, eq_float,
    eq_int, le_float, lshr_int, lt_float, mul_float, mul_int, or_int, rem_float, shl_int, sitofp,
    sle_int, slt_int, sub_float, sub_int, xor_int,
};

/// A builtin operation invoked by a `:call` statement (no dispatch).
#[derive(Clone, Copy)]
pub enum Builtin {
    Add,
    Sub,
    Mul,
    /// `/`: float division; integer operands convert through `sitofp`
    /// (Julia's `Int / Int` promotes to `Float64` in `base/`).
    Div,
    /// `÷` (`checked_sdiv_int`): truncating; `DivideError` on 0 or typemin÷-1.
    IDiv,
    /// `%` (`checked_srem_int` / `rem_float`).
    Rem,
    And,
    Or,
    Xor,
    Shl,
    /// `>>` (`ashr_int`, sign-fill).
    Shr,
    /// `>>>` (`lshr_int`, zero-fill).
    Lshr,
    Slt,
    Sle,
    Eq,
    /// `===` (`jl_egal`): works on any two values, no unboxing.
    Egal,
}

/// A statement operand: an SSA result, a local slot, or an integer constant.
#[derive(Clone, Copy)]
pub enum Op {
    Ssa(usize),
    Slot(usize),
    Int(i64),
    Float(f64),
}

/// A lowered statement. Its result becomes `SSAValue(index)`.
#[derive(Clone)]
pub enum Stmt {
    /// `ssa[i] = builtin(args...)`
    Call(Builtin, Vec<Op>),
    /// `ssa[i] = <dispatch generic function `id`>(args...)`
    CallGeneric(u32, Vec<Op>),
    /// `slot[k] = op` (the assigned value is also `ssa[i]`)
    Assign(usize, Op),
    /// `ip = target`
    Goto(usize),
    /// `if !cond { ip = target }`
    GotoIfNot(Op, usize),
    /// return `op`
    Return(Op),
    /// `ssa[i] = new(type, args...)` — construct a struct instance
    /// (`jl_new_structv`; the type offset is rooted via the struct registry).
    New(crate::region::Offset, Vec<Op>),
    /// `ssa[i] = getfield(op, name)` — field read by interned symbol.
    GetField(Op, crate::region::Offset),
    /// `setfield!(obj, name, rhs)`; the statement's value is `rhs`.
    SetField(Op, crate::region::Offset, Op),
    /// Begin an exception handler (`EnterNode`, `interpreter.c:521`): if an
    /// exception is thrown before the matching `Leave`, control transfers to
    /// statement `catch_ip`. Adapted for the single-loop interpreter as an
    /// explicit handler stack plus a catch-destination jump, because WASM has no
    /// `setjmp`/`longjmp` machine-stack unwinding — a recorded divergence, of a
    /// kind with the mandatory shadow stack, and the shape compiled code reuses.
    Enter(usize),
    /// Pop `n` active handlers on normal control flow out of their `try` regions
    /// (`:leave`, `interpreter.c:608`).
    Leave(usize),
    /// Throw the operand value as an exception (`jl_throw`): divert to the
    /// innermost active handler, binding the value as the current exception; with
    /// no handler it propagates out of the frame.
    Throw(Op),
    /// `ssa[i] = [args...]` — a 1-D array literal: element type is the common
    /// concrete type of the elements, or `Any` when they differ (or none).
    ArrayLit(Vec<Op>),
    /// `ssa[i] = a[idx]` — 1-based `getindex` over `arrayref`.
    ArrayRef(Op, Op),
    /// `a[idx] = rhs` (1-based `setindex!`); the statement's value is `rhs`.
    ArraySet(Op, Op, Op),
    /// `push!(a, v)`; the statement's value is the array.
    Push(Op, Op),
    /// `ssa[i] = length(a)`.
    Len(Op),
    /// Bind the current caught exception as this statement's SSA value
    /// (`Expr(:the_exception)` / `jl_current_exception`), for `catch e`.
    Caught,
    /// Re-throw the current exception (`jl_rethrow`) — the exception path of a
    /// `finally` block resumes unwinding after the cleanup runs.
    Rethrow,
}

/// A lowered method body: its statements and its number of local slots. For a
/// method, the leading slots are its arguments.
#[derive(Clone)]
pub struct Body {
    pub nslots: usize,
    pub code: Vec<Stmt>,
}

fn read_op(op: Op, frame: &Frame, ssa_base: usize) -> Value {
    match op {
        Op::Ssa(i) => frame.get(ssa_base + i),
        Op::Slot(k) => frame.get(k),
        Op::Int(c) => box_int(c),
        Op::Float(c) => box_float64(c),
    }
}

/// Apply a numeric builtin, choosing the `Int64` or `Float64` intrinsic by the
/// operands' runtime type. This is a simplification: in Julia the operator is a
/// generic function that dispatches to the typed intrinsic. Operands are assumed
/// homogeneous (no implicit promotion yet), except `/`, which converts integer
/// operands through `sitofp` as Julia's `base/` promotion does. `Err` carries
/// the would-be exception (e.g. `DivideError`) until real exceptions exist.
fn apply(b: Builtin, args: &Frame) -> Result<Value, Value> {
    let x = args.get(0);
    let y = args.get(1);
    if let Builtin::Egal = b {
        return Ok(box_bool(crate::builtins::egal(x, y))); // any values, no unboxing
    }
    if let Builtin::Div = b {
        let as_f64 = |v| {
            if object::type_of(v) == types::builtin(id::FLOAT64) {
                unbox_float64(v)
            } else {
                sitofp(unbox_int(v))
            }
        };
        return Ok(box_float64(div_float(as_f64(x), as_f64(y))));
    }
    if object::type_of(x) == types::builtin(id::FLOAT64) {
        let (a, c) = (unbox_float64(x), unbox_float64(y));
        Ok(match b {
            Builtin::Add => box_float64(add_float(a, c)),
            Builtin::Sub => box_float64(sub_float(a, c)),
            Builtin::Mul => box_float64(mul_float(a, c)),
            Builtin::Rem => box_float64(rem_float(a, c)),
            Builtin::Slt => box_bool(lt_float(a, c)),
            Builtin::Sle => box_bool(le_float(a, c)),
            Builtin::Eq => box_bool(eq_float(a, c)),
            Builtin::IDiv
            | Builtin::And
            | Builtin::Or
            | Builtin::Xor
            | Builtin::Shl
            | Builtin::Shr
            | Builtin::Lshr => {
                return Err(crate::errors::error_exception("integer operator applied to Float64"))
            }
            Builtin::Div | Builtin::Egal => unreachable!("handled above"),
        })
    } else {
        let (a, c) = (unbox_int(x), unbox_int(y));
        Ok(match b {
            Builtin::Add => box_int(add_int(a, c)),
            Builtin::Sub => box_int(sub_int(a, c)),
            Builtin::Mul => box_int(mul_int(a, c)),
            Builtin::IDiv => {
                box_int(checked_sdiv_int(a, c).ok_or_else(crate::errors::divide_error)?)
            }
            Builtin::Rem => {
                box_int(checked_srem_int(a, c).ok_or_else(crate::errors::divide_error)?)
            }
            Builtin::And => box_int(and_int(a, c)),
            Builtin::Or => box_int(or_int(a, c)),
            Builtin::Xor => box_int(xor_int(a, c)),
            Builtin::Shl => box_int(shl_int(a, c)),
            Builtin::Shr => box_int(ashr_int(a, c)),
            Builtin::Lshr => box_int(lshr_int(a, c)),
            Builtin::Slt => box_bool(slt_int(a, c)),
            Builtin::Sle => box_bool(sle_int(a, c)),
            Builtin::Eq => box_bool(eq_int(a, c)),
            Builtin::Div | Builtin::Egal => unreachable!("handled above"),
        })
    }
}

/// Evaluate `body` with no arguments.
pub fn eval(body: &Body) -> Result<Value, Value> {
    eval_core(body, &[], |_| Ok(()))
}

/// Evaluate `body`, binding `args` to its leading slots (a method invocation).
pub fn eval_with_args(body: &Body, args: &[Value]) -> Result<Value, Value> {
    let seeds: Vec<(usize, Value)> = args.iter().copied().enumerate().collect();
    eval_core(body, &seeds, |_| Ok(()))
}

/// Evaluate a top-level `body`: seed the given slots first (globals flowing
/// in), and pass the frame to `flush` at successful return — while it is still
/// rooted — so final slot values can flow out to module bindings.
pub fn eval_toplevel(
    body: &Body,
    seed: &[(usize, Value)],
    flush: impl FnOnce(&Frame) -> Result<(), Value>,
) -> Result<Value, Value> {
    eval_core(body, seed, flush)
}

/// The interpreter core: seeded slots, the ip loop, and a rooted flush hook at
/// the return point. The slots and SSA values live in one GC frame and are
/// roots throughout; each call's argument temporaries get their own
/// short-lived frame. `Err` carries an uncaught exception out of the frame.
fn eval_core(
    body: &Body,
    seed: &[(usize, Value)],
    flush: impl FnOnce(&Frame) -> Result<(), Value>,
) -> Result<Value, Value> {
    let ssa_base = body.nslots;
    // One extra slot past the SSA values holds the current caught exception, so
    // it stays rooted (in the frame) across allocations inside a catch block.
    let exc_slot = body.nslots + body.code.len();
    let frame = Frame::new(exc_slot + 1);
    for &(i, v) in seed {
        frame.set(i, v);
    }

    let mut ip = 0usize;
    // Active exception handlers (catch destinations), innermost last. An explicit
    // stack + catch-dest jump stands in for `setjmp`/`longjmp` (absent in WASM);
    // it is also the shape compiled code will reuse.
    let mut handlers: Vec<usize> = Vec::new();
    // Divert a thrown error to the innermost active handler's catch block, or
    // propagate it out of the frame if none is active.
    macro_rules! guard {
        ($result:expr) => {
            match $result {
                Ok(v) => v,
                Err(e) => {
                    if let Some(catch_ip) = handlers.pop() {
                        frame.set(exc_slot, e); // the caught exception, for `catch e`
                        ip = catch_ip;
                        continue;
                    }
                    return Err(e);
                }
            }
        };
    }
    loop {
        match &body.code[ip] {
            Stmt::Goto(target) => {
                ip = *target;
                continue;
            }
            Stmt::GotoIfNot(cond, target) => {
                let c = read_op(*cond, &frame, ssa_base);
                if !unbox_bool(c) {
                    ip = *target;
                    continue;
                }
            }
            Stmt::Return(op) => {
                let v = read_op(*op, &frame, ssa_base);
                let root = crate::gc::Rooted::new(v); // survives flush allocations
                flush(&frame)?;
                let v = root.get();
                drop(root);
                return Ok(v);
            }
            Stmt::Assign(slot, op) => {
                let v = read_op(*op, &frame, ssa_base);
                frame.set(*slot, v);
                frame.set(ssa_base + ip, v);
            }
            Stmt::Call(builtin, args) => {
                let argf = Frame::new(args.len());
                for (j, op) in args.iter().enumerate() {
                    argf.set(j, read_op(*op, &frame, ssa_base));
                }
                let result = apply(*builtin, &argf);
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::CallGeneric(func, args) => {
                let argf = Frame::new(args.len());
                for (j, op) in args.iter().enumerate() {
                    argf.set(j, read_op(*op, &frame, ssa_base));
                }
                let vals: Vec<Value> = (0..args.len()).map(|j| argf.get(j)).collect();
                let result = dispatch::invoke(*func, &vals);
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::New(ty, args) => {
                let argf = Frame::new(args.len());
                for (j, op) in args.iter().enumerate() {
                    argf.set(j, read_op(*op, &frame, ssa_base));
                }
                let vals: Vec<Value> = (0..args.len()).map(|j| argf.get(j)).collect();
                let result = types::new_struct(*ty, &vals).map_err(crate::errors::wrap_msg);
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::GetField(op, name_sym) => {
                let v = read_op(*op, &frame, ssa_base);
                let t = object::type_of(v);
                let i = guard!(types::field_index(t, *name_sym).ok_or_else(|| {
                    crate::errors::wrap_msg(format!(
                        "type {} has no field {}",
                        crate::symbol::as_str(types::type_sym(t)),
                        crate::symbol::as_str(*name_sym)
                    ))
                }));
                let r = guard!(types::get_nth_field(v, i).map_err(crate::errors::wrap_msg));
                frame.set(ssa_base + ip, r);
            }
            Stmt::SetField(obj, name_sym, rhs) => {
                let v = read_op(*obj, &frame, ssa_base);
                let r = read_op(*rhs, &frame, ssa_base);
                let t = object::type_of(v);
                let i = guard!(types::field_index(t, *name_sym).ok_or_else(|| {
                    crate::errors::wrap_msg(format!(
                        "type {} has no field {}",
                        crate::symbol::as_str(types::type_sym(t)),
                        crate::symbol::as_str(*name_sym)
                    ))
                }));
                guard!(types::set_nth_field(v, i, r).map_err(crate::errors::wrap_msg));
                frame.set(ssa_base + ip, r);
            }
            Stmt::Enter(catch_ip) => {
                handlers.push(*catch_ip);
            }
            Stmt::Leave(n) => {
                for _ in 0..*n {
                    handlers.pop();
                }
            }
            Stmt::Throw(op) => {
                let exc = read_op(*op, &frame, ssa_base);
                if let Some(catch_ip) = handlers.pop() {
                    frame.set(exc_slot, exc); // rooted in the frame across the catch block
                    ip = catch_ip;
                    continue;
                }
                return Err(exc);
            }
            Stmt::Caught => {
                frame.set(ssa_base + ip, frame.get(exc_slot));
            }
            Stmt::Rethrow => {
                let exc = frame.get(exc_slot);
                if let Some(catch_ip) = handlers.pop() {
                    frame.set(exc_slot, exc);
                    ip = catch_ip;
                    continue;
                }
                return Err(exc);
            }
            Stmt::ArrayLit(args) => {
                let argf = Frame::new(args.len());
                for (j, op) in args.iter().enumerate() {
                    argf.set(j, read_op(*op, &frame, ssa_base));
                }
                // Common concrete element type, or Any (Julia's typed literals
                // come from base/'s promotion; this is the bootstrap subset).
                let any = types::builtin(id::ANY);
                let elem = if args.is_empty() {
                    any
                } else {
                    let t0 = object::type_of(argf.get(0));
                    if (1..args.len()).all(|j| object::type_of(argf.get(j)) == t0) {
                        t0
                    } else {
                        any
                    }
                };
                let a = guard!(crate::array::alloc_1d(elem, args.len() as u32));
                for j in 0..args.len() {
                    guard!(crate::array::aset(a, j as u32, argf.get(j)));
                }
                drop(argf);
                frame.set(ssa_base + ip, a);
            }
            Stmt::ArrayRef(a, idx) => {
                let (av, i) = (read_op(*a, &frame, ssa_base), read_op(*idx, &frame, ssa_base));
                let r = guard!(index_checked(av, i).and_then(|i0| crate::array::aref(av, i0)));
                frame.set(ssa_base + ip, r);
            }
            Stmt::ArraySet(a, idx, rhs) => {
                let (av, i) = (read_op(*a, &frame, ssa_base), read_op(*idx, &frame, ssa_base));
                let r = read_op(*rhs, &frame, ssa_base);
                guard!(index_checked(av, i).and_then(|i0| crate::array::aset(av, i0, r)));
                frame.set(ssa_base + ip, r);
            }
            Stmt::Push(a, v) => {
                let av = read_op(*a, &frame, ssa_base);
                let vv = read_op(*v, &frame, ssa_base);
                guard!(expect_array(av));
                guard!(crate::array::push(av, vv));
                frame.set(ssa_base + ip, av);
            }
            Stmt::Len(a) => {
                let av = read_op(*a, &frame, ssa_base);
                guard!(expect_array(av));
                frame.set(ssa_base + ip, box_int(crate::array::len(av) as i64));
            }
        }
        ip += 1;
    }
}

/// `v` must be an array (a `MethodError` otherwise, in spirit).
fn expect_array(v: Value) -> Result<(), Value> {
    if types::is_array(object::type_of(v)) {
        Ok(())
    } else {
        Err(crate::errors::error_exception("MethodError: expected an Array"))
    }
}

/// Convert a 1-based boxed index into a checked 0-based one; out of range is a
/// `BoundsError(a, i)` carrying the array and the offending index.
fn index_checked(a: Value, i: Value) -> Result<u32, Value> {
    expect_array(a)?;
    let i = unbox_int(i);
    if i < 1 || i > crate::array::len(a) as i64 {
        return Err(crate::errors::bounds_error(a, i));
    }
    Ok((i - 1) as u32)
}
