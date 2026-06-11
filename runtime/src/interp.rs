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
    add_float, add_int, eq_float, eq_int, le_float, lt_float, mul_float, mul_int, sle_int, slt_int,
    sub_float, sub_int,
};

/// A builtin operation invoked by a `:call` statement (no dispatch).
#[derive(Clone, Copy)]
pub enum Builtin {
    Add,
    Sub,
    Mul,
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
/// homogeneous (no implicit promotion yet).
fn apply(b: Builtin, args: &Frame) -> Value {
    let x = args.get(0);
    let y = args.get(1);
    if let Builtin::Egal = b {
        return box_bool(crate::builtins::egal(x, y)); // any values, no unboxing
    }
    if object::type_of(x) == types::builtin(id::FLOAT64) {
        let (a, c) = (unbox_float64(x), unbox_float64(y));
        match b {
            Builtin::Add => box_float64(add_float(a, c)),
            Builtin::Sub => box_float64(sub_float(a, c)),
            Builtin::Mul => box_float64(mul_float(a, c)),
            Builtin::Slt => box_bool(lt_float(a, c)),
            Builtin::Sle => box_bool(le_float(a, c)),
            Builtin::Eq => box_bool(eq_float(a, c)),
            Builtin::Egal => unreachable!("handled before unboxing"),
        }
    } else {
        let (a, c) = (unbox_int(x), unbox_int(y));
        match b {
            Builtin::Add => box_int(add_int(a, c)),
            Builtin::Sub => box_int(sub_int(a, c)),
            Builtin::Mul => box_int(mul_int(a, c)),
            Builtin::Slt => box_bool(slt_int(a, c)),
            Builtin::Sle => box_bool(sle_int(a, c)),
            Builtin::Eq => box_bool(eq_int(a, c)),
            Builtin::Egal => unreachable!("handled before unboxing"),
        }
    }
}

/// Evaluate `body` with no arguments.
pub fn eval(body: &Body) -> Value {
    eval_with_args(body, &[])
}

/// Evaluate `body`, binding `args` to its leading slots (a method invocation).
/// The slots and SSA values live in one GC frame and are roots throughout; each
/// call's argument temporaries get their own short-lived frame.
pub fn eval_with_args(body: &Body, args: &[Value]) -> Value {
    let ssa_base = body.nslots;
    let frame = Frame::new(body.nslots + body.code.len());
    for (i, &a) in args.iter().enumerate() {
        frame.set(i, a);
    }

    let mut ip = 0usize;
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
                return read_op(*op, &frame, ssa_base);
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
                frame.set(ssa_base + ip, result);
            }
            Stmt::CallGeneric(func, args) => {
                let argf = Frame::new(args.len());
                for (j, op) in args.iter().enumerate() {
                    argf.set(j, read_op(*op, &frame, ssa_base));
                }
                let vals: Vec<Value> = (0..args.len()).map(|j| argf.get(j)).collect();
                let result = dispatch::invoke(*func, &vals); // args stay rooted via argf
                drop(argf);
                frame.set(ssa_base + ip, result);
            }
        }
        ip += 1;
    }
}
