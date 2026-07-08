//! A minimal interpreter for Julia's lowered IR (`CodeInfo`).
//!
//! Executes a faithful subset of Julia's lowered statement forms via an
//! instruction-pointer loop, mirroring `eval_body` in `src/interpreter.c`:
//! `SlotNumber` locals, `SSAValue` results, `GotoNode`, `GotoIfNot`,
//! `ReturnNode`, `:call` expressions, `:new`/field access, exception handling
//! (`EnterNode`/`:leave`/`throw`, with exceptions as values), and array
//! operations. This is *lowered* CodeInfo, which uses mutable slots rather
//! than SSA phi nodes.
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

/// A statement operand: an SSA result, a local slot, an inline literal, a
/// boxed constant, or a global binding (the value forms of `eval_value`,
/// `interpreter.c:201–226`, for the shapes Ruju represents).
#[derive(Clone, Copy)]
pub enum Op {
    Ssa(usize),
    Slot(usize),
    /// Inline unboxed literal, boxed on read — a bootstrap-front-end
    /// convenience; pre-lowered code carries [`Op::Const`] instead.
    Int(i64),
    Float(f64),
    /// A boxed constant (`QuoteNode` / an already-evaluated literal,
    /// `interpreter.c:217`). The referenced value must stay GC-reachable
    /// independently of this Rust-side IR (immortal, a singleton, or rooted
    /// by the IR's owner) — the same contract as [`Stmt::New`]'s type.
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    Const(crate::region::Offset),
    /// `GlobalRef` (`interpreter.c:220–221` → `jl_eval_globalref`, `:174`):
    /// resolve the interned symbol's binding in `Main` at evaluation time;
    /// unbound throws (`jl_undefined_var_error` — an `ErrorException` here
    /// until `UndefVarError`'s world-age field is representable, recorded).
    /// The module is implicitly `Main` until nested modules land (recorded).
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    Global(crate::region::Offset),
}

/// A lowered statement. Its result becomes `SSAValue(index)`.
#[derive(Clone)]
pub enum Stmt {
    /// `ssa[i] = op` — a bare value form as a statement (the `eval_body`
    /// default arm: `locals[nslots + ip] = eval_value(stmt)`).
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    Value(Op),
    /// `ssa[i] = builtin(args...)`
    Call(Builtin, Vec<Op>),
    /// `ssa[i] = <dispatch generic function `id`>(args...)`
    CallGeneric(u32, Vec<Op>),
    /// `ssa[i] = (args[0])(args[1..])` — the real `:call` shape
    /// (`interpreter.c:242` → `jl_apply`): the callee is itself an evaluated
    /// operand (typically an [`Op::Global`]), and dispatch keys off
    /// `typeof(callee)` as `jl_apply_generic` does. A non-callable callee
    /// throws (a `MethodError` in Julia; an `ErrorException` here until the
    /// `MethodError` type lands with dispatch hardening — recorded).
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    CallValue(Vec<Op>),
    /// `slot[k] = op` (the assigned value is also `ssa[i]`)
    Assign(usize, Op),
    /// `slot[k] = (args[0])(args[1..])` — an assignment whose right-hand
    /// side is a call expression, as lowering emits (`Expr(:(=), slot,
    /// Expr(:call, …))`; the C's `:(=)` arm evaluates the rhs through
    /// `eval_value`, which handles `:call`). The call's value is stored to
    /// the slot and is also `ssa[i]`.
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    AssignCall(usize, Vec<Op>),
    /// `global name = op` — assign the interned symbol's `Main` binding (the
    /// GlobalRef arm of `:=`, `interpreter.c:592–606`, via `jl_set_global` —
    /// minus world age and constness, as `module.rs` records). The assigned
    /// value is also `ssa[i]`.
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    AssignGlobal(crate::region::Offset, Op),
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
    /// `Expr(:isdefined, op)` (`interpreter.c:251–260`): whether a slot has
    /// been assigned (`locals[n] != NULL`) or a global is bound — without
    /// evaluating it (an unbound global here is `false`, not a throw). SSA
    /// results and constants are always defined. Boxed `Bool` result.
    /// (`:splatnew` waits on runtime tuple values, `:static_parameter` on
    /// the sparams environment — recorded in the ledger.)
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    IsDefined(Op),
    /// `Expr(:method, name)` — declare (or fetch) the generic function bound
    /// to the interned symbol in `Main` (`eval_methoddef`'s 1-arg arm,
    /// `interpreter.c:80–97,366` → `jl_declare_const_gf`, minus constness as
    /// `module.rs` records): unbound creates a fresh function value and binds
    /// it; bound-to-a-function returns it; bound-to-anything-else throws.
    /// The function value is this statement's SSA result.
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    MethodFunc(crate::region::Offset),
    /// `Expr(:method, fname, atypes, meth)` — define a method
    /// (`eval_methoddef`'s 3-arg arm, `interpreter.c:99–111,642` →
    /// `jl_method_def`): the callee operand must evaluate to a function
    /// value, the signature operand to a tuple type. The body rides inline
    /// in the statement until the heap-`CodeInfo` reshape (the C evaluates
    /// it as a `CodeInfo` value). Adaptations, recorded: our signatures are
    /// `Tuple{argtypes...}` (Julia's argdata svec carries `typeof(f)` and
    /// the sparam typevars), the result is `nothing` (no `jl_method_t`
    /// objects yet), and the C confines the 3-arg form to toplevel frames
    /// (`:641`) — we have no toplevel/method frame distinction yet.
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    MethodDef(Op, Op, Body),
    /// Bind the current caught exception as this statement's SSA value
    /// (`Expr(:the_exception)` / `jl_current_exception`), for `catch e`.
    Caught,
    /// `slot[k] = Expr(:the_exception)` — the assignment form lowering emits
    /// to bind `catch e` variables; value is also `ssa[i]`.
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it; tests exercise it now
    AssignCaught(usize),
    /// Re-throw the current exception (`jl_rethrow`) — the exception path of a
    /// `finally` block resumes unwinding after the cleanup runs. Re-throws the
    /// exception-stack top without pushing a duplicate (`throw_internal(ct,
    /// NULL)`).
    Rethrow,
    /// `Expr(:latestworld)`: advance the task's world age
    /// (`interpreter.c:650–652`). A no-op here — single world until world
    /// age lands with dispatch hardening (recorded).
    #[allow(dead_code)] // pre-lowered code (M2 C-1) constructs it
    LatestWorld,
    /// `Expr(:pop_exception, ssa)` (`interpreter.c:637–640` →
    /// `jl_restore_excstack`): leaving a catch scope truncates the exception
    /// stack back to the depth its `Enter` captured — the operand is that
    /// `Enter`'s SSA result. This is what keeps a nested `catch` inside a
    /// `finally` from clobbering the outer current exception.
    PopException(Op),
}

/// A lowered method body: its statements and its number of local slots. For a
/// method, the leading slots are its arguments.
#[derive(Clone)]
pub struct Body {
    pub nslots: usize,
    pub code: Vec<Stmt>,
}

fn read_op(op: Op, frame: &Frame, ssa_base: usize) -> Result<Value, Value> {
    match op {
        Op::Ssa(i) => Ok(frame.get(ssa_base + i)),
        Op::Slot(k) => Ok(frame.get(k)),
        Op::Int(c) => Ok(box_int(c)),
        Op::Float(c) => Ok(box_float64(c)),
        Op::Const(o) => Ok(Value(o)),
        Op::Global(sym) => crate::module::get_global(Value(crate::module::main_offset()), sym)
            .ok_or_else(|| {
                crate::errors::error_exception(&format!(
                    "UndefVarError: `{}` not defined in `Main`",
                    crate::symbol::as_str(sym)
                ))
            }),
    }
}

/// Read every operand into the (rooted) argument frame; the first failure
/// (an unbound global) aborts the statement.
fn read_args(args: &[Op], frame: &Frame, ssa_base: usize, argf: &Frame) -> Result<(), Value> {
    for (j, op) in args.iter().enumerate() {
        argf.set(j, read_op(*op, frame, ssa_base)?);
    }
    Ok(())
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
    let frame = Frame::new(body.nslots + body.code.len());
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
                        // The landing half of `throw_internal`: the caught
                        // exception goes on the (GC-rooted) exception stack,
                        // where `Caught` reads it and `PopException` retires it.
                        crate::errors::exc_push(e);
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
                let c = guard!(read_op(*cond, &frame, ssa_base));
                if !unbox_bool(c) {
                    ip = *target;
                    continue;
                }
            }
            Stmt::Return(op) => {
                let v = guard!(read_op(*op, &frame, ssa_base));
                let root = crate::gc::Rooted::new(v); // survives flush allocations
                flush(&frame)?;
                let v = root.get();
                drop(root);
                return Ok(v);
            }
            Stmt::Value(op) => {
                let v = guard!(read_op(*op, &frame, ssa_base));
                frame.set(ssa_base + ip, v);
            }
            Stmt::Assign(slot, op) => {
                let v = guard!(read_op(*op, &frame, ssa_base));
                frame.set(*slot, v);
                frame.set(ssa_base + ip, v);
            }
            Stmt::AssignCall(slot, args) => {
                let argf = Frame::new(args.len());
                guard!(read_args(args, &frame, ssa_base, &argf));
                let callee = argf.get(0);
                let vals: Vec<Value> = (1..args.len()).map(|j| argf.get(j)).collect();
                let result = call_value(callee, &vals);
                drop(argf);
                let v = guard!(result);
                frame.set(*slot, v);
                frame.set(ssa_base + ip, v);
            }
            Stmt::LatestWorld => {}
            Stmt::AssignGlobal(sym, op) => {
                let v = guard!(read_op(*op, &frame, ssa_base));
                // The frame slot roots the value across the binding store's
                // possible table growth.
                frame.set(ssa_base + ip, v);
                guard!(crate::module::set_global(
                    Value(crate::module::main_offset()),
                    *sym,
                    v
                ));
            }
            Stmt::Call(builtin, args) => {
                let argf = Frame::new(args.len());
                guard!(read_args(args, &frame, ssa_base, &argf));
                let result = apply(*builtin, &argf);
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::CallGeneric(func, args) => {
                let argf = Frame::new(args.len());
                guard!(read_args(args, &frame, ssa_base, &argf));
                let vals: Vec<Value> = (0..args.len()).map(|j| argf.get(j)).collect();
                let result = dispatch::invoke(*func, &vals);
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::CallValue(args) => {
                let argf = Frame::new(args.len());
                guard!(read_args(args, &frame, ssa_base, &argf));
                let callee = argf.get(0);
                let vals: Vec<Value> = (1..args.len()).map(|j| argf.get(j)).collect();
                let result = call_value(callee, &vals);
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::New(ty, args) => {
                let argf = Frame::new(args.len());
                guard!(read_args(args, &frame, ssa_base, &argf));
                let vals: Vec<Value> = (0..args.len()).map(|j| argf.get(j)).collect();
                let result = types::new_struct(*ty, &vals).map_err(crate::errors::wrap_msg);
                drop(argf);
                frame.set(ssa_base + ip, guard!(result));
            }
            Stmt::GetField(op, name_sym) => {
                let v = guard!(read_op(*op, &frame, ssa_base));
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
                let v = guard!(read_op(*obj, &frame, ssa_base));
                let r = guard!(read_op(*rhs, &frame, ssa_base));
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
                // "store current top of exception stack for restore in
                // pop_exception" (`interpreter.c:551–553`, boxed as the
                // EnterNode's SSA result).
                frame.set(ssa_base + ip, box_int(crate::errors::exc_state() as i64));
            }
            Stmt::Leave(n) => {
                for _ in 0..*n {
                    handlers.pop();
                }
            }
            Stmt::Throw(op) => {
                let exc = guard!(read_op(*op, &frame, ssa_base));
                if let Some(catch_ip) = handlers.pop() {
                    crate::errors::exc_push(exc); // rooted on the exception stack
                    ip = catch_ip;
                    continue;
                }
                return Err(exc);
            }
            Stmt::MethodFunc(sym) => {
                let main = Value(crate::module::main_offset());
                let v = match crate::module::get_global(main, *sym) {
                    Some(v) => {
                        let callable: Result<(), Value> = if dispatch::func_of(v).is_none() {
                            Err(crate::errors::error_exception(&format!(
                                "cannot define function {}; it already has a value",
                                crate::symbol::as_str(*sym)
                            )))
                        } else {
                            Ok(())
                        };
                        guard!(callable);
                        v
                    }
                    None => {
                        let f = dispatch::make_function(
                            crate::symbol::as_str(*sym),
                            dispatch::fresh_func_id(),
                        );
                        // The frame slot roots the fresh value across the
                        // binding store's possible table growth.
                        frame.set(ssa_base + ip, f);
                        guard!(crate::module::set_global(main, *sym, f));
                        f
                    }
                };
                frame.set(ssa_base + ip, v);
            }
            Stmt::MethodDef(fop, sigop, body) => {
                let f = guard!(read_op(*fop, &frame, ssa_base));
                let sig = guard!(read_op(*sigop, &frame, ssa_base));
                let func = guard!(dispatch::func_of(f).ok_or_else(|| {
                    crate::errors::error_exception("method: not a generic function")
                }));
                // The signature operand is a Tuple type (hand-built IR), or
                // the argdata svec real lowering constructs at run time:
                // `svec(svec(typeof(f), argtypes...), svec(sparams...), loc)`
                // (`jl_method_def`'s unpacking, `method.c:1265`). Our method
                // signatures drop the leading `typeof(f)` (recorded — they
                // align with it at dispatch hardening); static parameters
                // are not representable yet.
                let sig_tuple: Result<crate::region::Offset, Value> = if types::is_datatype(
                    sig.raw(),
                ) && types::is_tuple(sig.raw())
                {
                    Ok(sig.raw())
                } else if types::is_svec_value(sig) && types::svec_len(sig.raw()) >= 2 {
                    let sigv = types::svec_ref(sig.raw(), 0);
                    let sparams = types::svec_ref(sig.raw(), 1);
                    if !types::is_svec(object::type_of(Value(sigv)))
                        || !types::is_svec(object::type_of(Value(sparams)))
                    {
                        Err(crate::errors::error_exception("method: malformed argdata"))
                    } else if types::svec_len(sparams) != 0 {
                        Err(crate::errors::error_exception(
                            "method: static parameters not supported yet",
                        ))
                    } else {
                        let _rs = crate::gc::Rooted::new(sig); // argtypes are its subterms
                        let argtypes: Vec<crate::region::Offset> =
                            (1..types::svec_len(sigv)).map(|i| types::svec_ref(sigv, i)).collect();
                        Ok(types::tuple_type(&argtypes))
                    }
                } else {
                    Err(crate::errors::error_exception(
                        "method: signature must be a Tuple type or argdata svec",
                    ))
                };
                let sig_tuple = guard!(sig_tuple);
                dispatch::add_method(func, sig_tuple, body.clone());
                frame.set(ssa_base + ip, Value(types::nothing_instance()));
            }
            Stmt::Caught => {
                frame.set(ssa_base + ip, crate::errors::exc_current());
            }
            Stmt::AssignCaught(slot) => {
                let v = crate::errors::exc_current();
                frame.set(*slot, v);
                frame.set(ssa_base + ip, v);
            }
            Stmt::Rethrow => {
                let exc = crate::errors::exc_current();
                if let Some(catch_ip) = handlers.pop() {
                    // The top already IS the current exception; no re-push.
                    ip = catch_ip;
                    continue;
                }
                return Err(exc);
            }
            Stmt::PopException(op) => {
                let state = guard!(read_op(*op, &frame, ssa_base));
                crate::errors::exc_restore(unbox_int(state) as usize);
            }
            Stmt::ArrayLit(args) => {
                let argf = Frame::new(args.len());
                guard!(read_args(args, &frame, ssa_base, &argf));
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
                let av = guard!(read_op(*a, &frame, ssa_base));
                let i = guard!(read_op(*idx, &frame, ssa_base));
                let r = guard!(index_checked(av, i).and_then(|i0| crate::array::aref(av, i0)));
                frame.set(ssa_base + ip, r);
            }
            Stmt::ArraySet(a, idx, rhs) => {
                let av = guard!(read_op(*a, &frame, ssa_base));
                let i = guard!(read_op(*idx, &frame, ssa_base));
                let r = guard!(read_op(*rhs, &frame, ssa_base));
                guard!(index_checked(av, i).and_then(|i0| crate::array::aset(av, i0, r)));
                frame.set(ssa_base + ip, r);
            }
            Stmt::Push(a, v) => {
                let av = guard!(read_op(*a, &frame, ssa_base));
                let vv = guard!(read_op(*v, &frame, ssa_base));
                guard!(expect_array(av));
                guard!(crate::array::push(av, vv));
                frame.set(ssa_base + ip, av);
            }
            Stmt::Len(a) => {
                let av = guard!(read_op(*a, &frame, ssa_base));
                guard!(expect_array(av));
                frame.set(ssa_base + ip, box_int(crate::array::len(av) as i64));
            }
            Stmt::IsDefined(op) => {
                let defined = match op {
                    Op::Slot(k) => frame.get(*k) != Value::NULL,
                    Op::Global(sym) => {
                        crate::module::get_global(Value(crate::module::main_offset()), *sym)
                            .is_some()
                    }
                    _ => true, // SSA results, literals, and constants
                };
                frame.set(ssa_base + ip, box_bool(defined));
            }
        }
        ip += 1;
    }
}

/// Resolve and call a callable value on already-rooted arguments: a generic
/// function dispatches (`jl_apply_generic`), a native builtin runs directly
/// (`jl_f_*`), anything else throws (a `MethodError` in Julia; an
/// `ErrorException` until that type lands with dispatch hardening).
fn call_value(callee: Value, args: &[Value]) -> Result<Value, Value> {
    match dispatch::callable_of(callee) {
        Some(dispatch::FnKind::Generic(func)) => dispatch::invoke(func, args),
        Some(dispatch::FnKind::Native(f)) => f(args),
        None => Err(crate::errors::error_exception(&format!(
            "MethodError: objects of type {} are not callable",
            crate::symbol::as_str(types::type_sym(object::type_of(callee)))
        ))),
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
