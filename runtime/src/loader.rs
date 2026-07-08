//! Loading pre-lowered `CodeInfo` as data — the consuming half of M2's
//! build-time pre-lowering (decision D1, `design/strategy.md`).
//!
//! The pinned native Julia runs offline (`tools/prelower.jl`) and serializes
//! each toplevel thunk's `CodeInfo` into the line format parsed here; the
//! loader builds [`interp::Body`] values and executes them in order. The
//! format is **pin-versioned data, not an ABI**: the header names the format
//! version and the reference pin, artifacts regenerate whenever the pin
//! advances, and an unrecognized form is a loud error, never a guess.
//!
//! Adaptations from the producer side (each recorded in
//! `design/implementation.md`): module qualifiers on `GlobalRef`s drop to
//! `Main` (single module); method-body slots are pre-shifted past `#self#`
//! (our dispatch does not yet pass the callee); `LineNumberNode` constants
//! serialize as `nothing`.
//!
//! The prelude ([`install_prelude`]) binds what pre-lowered code reaches by
//! name: the callable `Core` builtins (`jl_f_*` analogs in `builtins.rs`),
//! the operators as *generic functions with methods over the typed
//! intrinsics* (the faithful shape — in Julia these are `base/` generic
//! functions), and the core type names (`Core` re-exports them).

use crate::dispatch;
use crate::interp::{Body, Builtin, Op, Stmt};
use crate::object::Value;
use crate::region::Offset;
use crate::types::{self, id};

/// The format version this loader understands.
const FORMAT_VERSION: &str = "1";

fn err(line_no: usize, msg: &str) -> String {
    format!("lowered format: line {}: {}", line_no + 1, msg)
}

fn intern(s: &str) -> Offset {
    crate::symbol::intern(types::builtin(id::SYMBOL), s)
}

/// Parse one operand token.
fn parse_op(tok: &str, line_no: usize) -> Result<Op, String> {
    let (kind, rest) = tok.split_once(':').unwrap_or((tok, ""));
    match kind {
        "ssa" => Ok(Op::Ssa(rest.parse().map_err(|_| err(line_no, "bad ssa"))?)),
        "slot" => Ok(Op::Slot(rest.parse().map_err(|_| err(line_no, "bad slot"))?)),
        "int" => Ok(Op::Int(rest.parse().map_err(|_| err(line_no, "bad int"))?)),
        "f64" => {
            let bits = u64::from_str_radix(rest, 16).map_err(|_| err(line_no, "bad f64"))?;
            Ok(Op::Float(f64::from_bits(bits)))
        }
        "bool" => Ok(Op::Const(crate::value::box_bool(rest == "1").raw())),
        "nothing" => Ok(Op::Const(types::nothing_instance())),
        "sym" => Ok(Op::Const(intern(rest))),
        "global" => Ok(Op::Global(intern(rest))),
        "module" => Ok(Op::Const(crate::module::main_offset())),
        _ => Err(err(line_no, &format!("unknown operand `{}`", tok))),
    }
}

fn parse_ops(toks: &[&str], line_no: usize) -> Result<Vec<Op>, String> {
    toks.iter().map(|t| parse_op(t, line_no)).collect()
}

/// Parse `n` statements starting at `lines[*pos]`, recursing into nested
/// method bodies.
fn parse_stmts(
    lines: &[&str],
    pos: &mut usize,
    n: usize,
) -> Result<Vec<Stmt>, String> {
    let mut code = Vec::with_capacity(n);
    for _ in 0..n {
        let line_no = *pos;
        let line = *lines
            .get(*pos)
            .ok_or_else(|| err(line_no, "unexpected end of input"))?;
        *pos += 1;
        let toks: Vec<&str> = line.split_whitespace().collect();
        let (&head, args) = toks
            .split_first()
            .ok_or_else(|| err(line_no, "empty statement"))?;
        let need = |n: usize| -> Result<(), String> {
            if args.len() == n {
                Ok(())
            } else {
                Err(err(line_no, "wrong operand count"))
            }
        };
        code.push(match head {
            "value" => {
                need(1)?;
                Stmt::Value(parse_op(args[0], line_no)?)
            }
            "call" => Stmt::CallValue(parse_ops(args, line_no)?),
            "assign" => {
                need(2)?;
                Stmt::Assign(
                    args[0].parse().map_err(|_| err(line_no, "bad slot"))?,
                    parse_op(args[1], line_no)?,
                )
            }
            "assigncall" => {
                let slot = args
                    .first()
                    .ok_or_else(|| err(line_no, "missing slot"))?
                    .parse()
                    .map_err(|_| err(line_no, "bad slot"))?;
                Stmt::AssignCall(slot, parse_ops(&args[1..], line_no)?)
            }
            "goto" => {
                need(1)?;
                Stmt::Goto(args[0].parse().map_err(|_| err(line_no, "bad target"))?)
            }
            "gotoifnot" => {
                need(2)?;
                Stmt::GotoIfNot(
                    parse_op(args[0], line_no)?,
                    args[1].parse().map_err(|_| err(line_no, "bad target"))?,
                )
            }
            "return" => {
                need(1)?;
                Stmt::Return(parse_op(args[0], line_no)?)
            }
            "enter" => {
                need(1)?;
                Stmt::Enter(args[0].parse().map_err(|_| err(line_no, "bad target"))?)
            }
            "leave" => {
                need(1)?;
                Stmt::Leave(args[0].parse().map_err(|_| err(line_no, "bad count"))?)
            }
            "pop_exception" => {
                need(1)?;
                Stmt::PopException(parse_op(args[0], line_no)?)
            }
            "the_exception" => Stmt::Caught,
            "assigncaught" => {
                need(1)?;
                Stmt::AssignCaught(args[0].parse().map_err(|_| err(line_no, "bad slot"))?)
            }
            "latestworld" => Stmt::LatestWorld,
            "method1" => {
                need(1)?;
                Stmt::MethodFunc(intern(args[0]))
            }
            // `method3 <fname-op> <argdata-op> <nslots> <nstmts>`, followed
            // by the nested body's statements (count-driven).
            "method3" => {
                need(4)?;
                let fop = parse_op(args[0], line_no)?;
                let sigop = parse_op(args[1], line_no)?;
                let nslots = args[2].parse().map_err(|_| err(line_no, "bad nslots"))?;
                let nstmts = args[3].parse().map_err(|_| err(line_no, "bad nstmts"))?;
                let inner = parse_stmts(lines, pos, nstmts)?;
                Stmt::MethodDef(fop, sigop, Body { nslots, code: inner })
            }
            _ => return Err(err(line_no, &format!("unknown statement `{}`", head))),
        });
    }
    Ok(code)
}

/// Parse the pre-lowered format into toplevel thunk bodies.
pub fn parse(src: &str) -> Result<Vec<Body>, String> {
    let lines: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let mut pos = 0usize;
    let header = *lines.first().ok_or("lowered format: empty input")?;
    let mut h = header.split_whitespace();
    if h.next() != Some("RUJU_LOWERED") || h.next() != Some(FORMAT_VERSION) {
        return Err(format!(
            "lowered format: bad header `{}` (expected RUJU_LOWERED {})",
            header, FORMAT_VERSION
        ));
    }
    pos += 1;
    let mut thunks = Vec::new();
    while pos < lines.len() {
        let line_no = pos;
        let toks: Vec<&str> = lines[pos].split_whitespace().collect();
        if toks.len() != 3 || toks[0] != "thunk" {
            return Err(err(line_no, "expected `thunk <nslots> <nstmts>`"));
        }
        pos += 1;
        let nslots = toks[1].parse().map_err(|_| err(line_no, "bad nslots"))?;
        let nstmts = toks[2].parse().map_err(|_| err(line_no, "bad nstmts"))?;
        let code = parse_stmts(&lines, &mut pos, nstmts)?;
        thunks.push(Body { nslots, code });
    }
    Ok(thunks)
}

/// Parse and execute pre-lowered toplevel thunks in order (the loading half
/// of `jl_toplevel_eval`'s thunk case, `toplevel.c:706–719`); the result is
/// the last thunk's value. Globals flow through `Main` — no seed/flush.
pub fn load_and_eval(src: &str) -> Result<Value, Value> {
    let thunks =
        parse(src).map_err(crate::errors::wrap_msg)?;
    let mut last = Value(types::nothing_instance());
    for t in &thunks {
        last = crate::interp::eval(t)?;
    }
    Ok(last)
}

/// Bind what pre-lowered code reaches by name in `Main`: the callable
/// `Core` builtins, the operators (generic functions whose methods wrap the
/// typed intrinsics — their faithful shape), and the core type names.
pub fn install_prelude() {
    let main = Value(crate::module::main_offset());
    let bind = |name: &str, v: Value| {
        crate::module::set_global(main, intern(name), v).expect("prelude binding");
    };

    // Native Core builtins (`jl_f_*` analogs).
    bind("svec", dispatch::make_native_function("svec", crate::builtins::f_svec));
    bind("Typeof", dispatch::make_native_function("Typeof", crate::builtins::f_typeof));
    bind("typeof", dispatch::make_native_function("typeof", crate::builtins::f_typeof));
    bind("isa", dispatch::make_native_function("isa", crate::builtins::f_isa));
    bind("throw", dispatch::make_native_function("throw", crate::builtins::f_throw));
    bind("convert", dispatch::make_native_function("convert", crate::builtins::f_convert));
    bind(
        "declare_global",
        dispatch::make_native_function("declare_global", crate::builtins::f_declare_global),
    );
    bind(
        "get_binding_type",
        dispatch::make_native_function("get_binding_type", crate::builtins::f_get_binding_type),
    );
    bind(
        "setglobal!",
        dispatch::make_native_function("setglobal!", crate::builtins::f_setglobal),
    );

    // Operators as generic functions over the typed intrinsics. Julia's are
    // `base/` generic functions; the method-per-signature shape is faithful,
    // the coverage (Int64/Float64 pairs) is the bootstrap subset.
    let t = |i| types::builtin(i);
    let binop = |b: Builtin| Body {
        nslots: 2,
        code: vec![
            Stmt::Call(b, vec![Op::Slot(0), Op::Slot(1)]),
            Stmt::Return(Op::Ssa(0)),
        ],
    };
    let flipped = |b: Builtin| Body {
        nslots: 2,
        code: vec![
            Stmt::Call(b, vec![Op::Slot(1), Op::Slot(0)]),
            Stmt::Return(Op::Ssa(0)),
        ],
    };
    let ops: &[(&str, Builtin)] = &[
        ("+", Builtin::Add),
        ("-", Builtin::Sub),
        ("*", Builtin::Mul),
        ("/", Builtin::Div),
        ("÷", Builtin::IDiv),
        ("%", Builtin::Rem),
        ("==", Builtin::Eq),
        ("<", Builtin::Slt),
        ("<=", Builtin::Sle),
        ("===", Builtin::Egal),
    ];
    for &(name, b) in ops {
        let func = dispatch::fresh_func_id();
        for elem in [t(id::INT64), t(id::FLOAT64)] {
            dispatch::add_method(func, types::tuple_type(&[elem, elem]), binop(b));
        }
        bind(name, dispatch::make_function(name, func));
    }
    for (name, b) in [(">", Builtin::Slt), (">=", Builtin::Sle)] {
        let func = dispatch::fresh_func_id();
        for elem in [t(id::INT64), t(id::FLOAT64)] {
            dispatch::add_method(func, types::tuple_type(&[elem, elem]), flipped(b));
        }
        bind(name, dispatch::make_function(name, func));
    }

    // Core type names (`Core` exports them; ours live in Main).
    for (name, i) in [
        ("Any", id::ANY),
        ("Int64", id::INT64),
        ("Int", id::INT64),
        ("Float64", id::FLOAT64),
        ("Bool", id::BOOL),
        ("Nothing", id::NOTHING),
        ("Integer", id::INTEGER),
        ("Real", id::REAL),
        ("Number", id::NUMBER),
        ("Function", id::FUNCTION),
        ("Exception", id::EXCEPTION),
        ("DivideError", id::DIVIDEERROR),
    ] {
        bind(name, Value(types::builtin(i)));
    }
}
