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
    #[allow(dead_code)] // the front-end wiring for `try`/`catch` is the next slice
    Enter(usize),
    /// Pop `n` active handlers on normal control flow out of their `try` regions
    /// (`:leave`, `interpreter.c:608`).
    #[allow(dead_code)] // the front-end wiring for `try`/`catch` is the next slice
    Leave(usize),
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
fn apply(b: Builtin, args: &Frame) -> Result<Value, String> {
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
            | Builtin::Lshr => return Err("integer operator applied to Float64".to_string()),
            Builtin::Div | Builtin::Egal => unreachable!("handled above"),
        })
    } else {
        let (a, c) = (unbox_int(x), unbox_int(y));
        Ok(match b {
            Builtin::Add => box_int(add_int(a, c)),
            Builtin::Sub => box_int(sub_int(a, c)),
            Builtin::Mul => box_int(mul_int(a, c)),
            Builtin::IDiv => box_int(checked_sdiv_int(a, c).ok_or("DivideError")?),
            Builtin::Rem => box_int(checked_srem_int(a, c).ok_or("DivideError")?),
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
pub fn eval(body: &Body) -> Result<Value, String> {
    eval_with_args(body, &[])
}

/// Evaluate `body`, binding `args` to its leading slots (a method invocation).
/// The slots and SSA values live in one GC frame and are roots throughout; each
/// call's argument temporaries get their own short-lived frame. `Err` carries
/// a would-be exception upward until real exception handling exists.
pub fn eval_with_args(body: &Body, args: &[Value]) -> Result<Value, String> {
    let ssa_base = body.nslots;
    let frame = Frame::new(body.nslots + body.code.len());
    for (i, &a) in args.iter().enumerate() {
        frame.set(i, a);
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
                        let _ = e; // the exception value binding (`catch e`) is a later slice
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
                return Ok(read_op(*op, &frame, ssa_base));
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
                let result = dispatch::invoke(*func, &vals); // args stay rooted via argf
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::New(ty, args) => {
                let argf = Frame::new(args.len());
                for (j, op) in args.iter().enumerate() {
                    argf.set(j, read_op(*op, &frame, ssa_base));
                }
                let vals: Vec<Value> = (0..args.len()).map(|j| argf.get(j)).collect();
                let result = types::new_struct(*ty, &vals); // args stay rooted via argf
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::GetField(op, name_sym) => {
                let v = read_op(*op, &frame, ssa_base);
                let t = object::type_of(v);
                let i = guard!(types::field_index(t, *name_sym).ok_or_else(|| {
                    format!(
                        "type {} has no field {}",
                        crate::symbol::as_str(types::type_sym(t)),
                        crate::symbol::as_str(*name_sym)
                    )
                }));
                let r = guard!(types::get_nth_field(v, i));
                frame.set(ssa_base + ip, r);
            }
            Stmt::SetField(obj, name_sym, rhs) => {
                let v = read_op(*obj, &frame, ssa_base);
                let r = read_op(*rhs, &frame, ssa_base);
                let t = object::type_of(v);
                let i = guard!(types::field_index(t, *name_sym).ok_or_else(|| {
                    format!(
                        "type {} has no field {}",
                        crate::symbol::as_str(types::type_sym(t)),
                        crate::symbol::as_str(*name_sym)
                    )
                }));
                guard!(types::set_nth_field(v, i, r));
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
        }
        ip += 1;
    }
}
