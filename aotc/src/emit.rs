//! IR → WASM emission over `wasm-encoder`.
//!
//! Two functions per compiled method, after the pin's `CodeInstance`
//! `invoke`/`specptr` split (`julia.h:460–461,523–535`):
//!
//! - the **specsig** entry — a native wasm signature from the inferred
//!   argtypes/rettype (`Int64 → i64`), the fast compiled→compiled path;
//! - the **boxed wrapper** (fptr1) — `(param i32 argv) (param i32 nargs)
//!   (result i32)`, unboxing each argument from the rooted argv slice in
//!   linear memory, calling the specsig, boxing the result. This is what
//!   dispatch calls.
//!
//! The module imports the runtime's memory and its `rj_box_int` /
//! `rj_unbox_int` exports; it defines no memory, no table, and no globals of
//! its own (the composable-memory commitment, decision D2c). Placing the
//! functions into the shared funcref table is the loader's job.
//!
//! Locals: every value-producing statement gets a wasm local of its inferred
//! type — `i64` for `Int64`, `i32` for `Bool` — so isbits values stay unboxed
//! throughout the body (the standing "nothing relies on heap identity for
//! primitives" invariant). φs are deconstructed into per-edge `local.set`s.

use crate::fixture::{Fixture, Node, Operand};
use crate::relooper::{Cfg, Emit, Relooper, Term};
use wasm_encoder::{
    BlockType, CodeSection, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction, MemArg, MemoryType, Module, TypeSection, ValType,
};

fn valtype(julia: &str) -> Option<ValType> {
    match julia {
        "Int64" => Some(ValType::I64),
        "Bool" => Some(ValType::I32),
        _ => None,
    }
}

/// The whitelisted intrinsic vocabulary (D2a mitigation 1). Every entry is
/// implemented by `ruju-intrinsics` and reference-verified there; the
/// wasm instruction is the same two's-complement/IEEE operation.
fn intrinsic(name: &str) -> Option<Instruction<'static>> {
    Some(match name {
        "add_int" => Instruction::I64Add,
        "sub_int" => Instruction::I64Sub,
        "mul_int" => Instruction::I64Mul,
        "and_int" => Instruction::I64And,
        "or_int" => Instruction::I64Or,
        "xor_int" => Instruction::I64Xor,
        "sle_int" => Instruction::I64LeS,
        "slt_int" => Instruction::I64LtS,
        "ule_int" => Instruction::I64LeU,
        "ult_int" => Instruction::I64LtU,
        "eq_int" => Instruction::I64Eq,
        "ne_int" => Instruction::I64Ne,
        _ => return None,
    })
}

struct FnEmitter<'f> {
    fx: &'f Fixture,
    /// wasm local per 1-based stmt id (params precede these).
    ssa_local: Vec<Option<u32>>,
    insns: Vec<Instruction<'static>>,
    err: Option<String>,
}

impl<'f> FnEmitter<'f> {
    fn fail(&mut self, msg: String) {
        if self.err.is_none() {
            self.err = Some(msg);
        }
    }

    fn operand(&mut self, op: &Operand) {
        match op {
            Operand::Ssa { id } => match self.ssa_local[(*id - 1) as usize] {
                Some(l) => self.insns.push(Instruction::LocalGet(l)),
                None => self.fail(format!("ssa %{id} has no value local")),
            },
            Operand::Arg { n } => {
                if *n < 2 {
                    // `_1` is `#self#` — a zero-size function singleton the
                    // specsig never materializes; referencing it is outside
                    // the slice's vocabulary.
                    self.fail("argument _1 (#self#) is not representable".into());
                } else {
                    self.insns.push(Instruction::LocalGet(*n - 2));
                }
            }
            Operand::Const { t, v } => match t.as_str() {
                "Int64" => match v.parse::<i64>() {
                    Ok(x) => self.insns.push(Instruction::I64Const(x)),
                    Err(e) => self.fail(format!("bad Int64 literal {v:?}: {e}")),
                },
                "Bool" => self.insns.push(Instruction::I32Const((v == "true") as i32)),
                _ => self.fail(format!("unsupported constant type {t:?}")),
            },
            Operand::Nothing => self.fail("`nothing` used as a value".into()),
        }
    }
}

impl<'f> Emit for FnEmitter<'f> {
    fn open_loop(&mut self, _x: usize) {
        self.insns.push(Instruction::Loop(BlockType::Empty));
    }

    fn open_block(&mut self, _follow: usize) {
        self.insns.push(Instruction::Block(BlockType::Empty));
    }

    fn close(&mut self) {
        self.insns.push(Instruction::End);
    }

    fn stmts(&mut self, x: usize) {
        let b = &self.fx.blocks[x];
        for i in (b.first - 1)..b.last {
            let stmt = &self.fx.stmts[i as usize];
            match &stmt.stmt {
                Node::Nothing | Node::Phi { .. } => {} // φs are edge moves
                Node::Call { f, args } => {
                    for a in args {
                        self.operand(&a.clone());
                    }
                    match intrinsic(f) {
                        Some(insn) => self.insns.push(insn),
                        None => self.fail(format!("intrinsic {f:?} outside the whitelist")),
                    }
                    match self.ssa_local[i as usize] {
                        Some(l) => self.insns.push(Instruction::LocalSet(l)),
                        None => self.fail(format!("call result %{} has no local", i + 1)),
                    }
                }
                // Terminators are emitted by the walk, never as body stmts.
                Node::Goto { .. } | Node::GotoIfNot { .. } | Node::Return { .. } => {}
            }
        }
    }

    fn phi_moves(&mut self, x: usize, target: usize) {
        let t = &self.fx.blocks[target];
        let moves: Vec<(u32, Operand)> = ((t.first - 1)..t.last)
            .filter_map(|i| match &self.fx.stmts[i as usize].stmt {
                Node::Phi { edges, values } => {
                    let j = edges.iter().position(|&e| e as usize == x + 1)?;
                    Some((i, values[j].clone()))
                }
                _ => None,
            })
            .collect();
        // Parallel-copy hazard: a φ reading another φ of the same block would
        // need a temporary; outside the slice's vocabulary — fail loudly.
        for (_, v) in &moves {
            if let Operand::Ssa { id } = v {
                let s = id - 1;
                if s >= t.first - 1
                    && s < t.last
                    && matches!(self.fx.stmts[s as usize].stmt, Node::Phi { .. })
                {
                    self.fail(format!("φ-swap hazard in block {}", target + 1));
                }
            }
        }
        for (i, v) in moves {
            self.operand(&v);
            match self.ssa_local[i as usize] {
                Some(l) => self.insns.push(Instruction::LocalSet(l)),
                None => self.fail(format!("φ %{} has no local", i + 1)),
            }
        }
    }

    fn br(&mut self, depth: u32) {
        self.insns.push(Instruction::Br(depth));
    }

    fn open_if(&mut self, x: usize) {
        let b = &self.fx.blocks[x];
        let cond = match &self.fx.stmts[(b.last - 1) as usize].stmt {
            Node::GotoIfNot { cond, .. } => cond.clone(),
            _ => {
                self.fail(format!("block {} has no GotoIfNot terminator", x + 1));
                return;
            }
        };
        self.operand(&cond);
        self.insns.push(Instruction::If(BlockType::Empty));
    }

    fn else_arm(&mut self) {
        self.insns.push(Instruction::Else);
    }

    fn ret(&mut self, x: usize) {
        let b = &self.fx.blocks[x];
        let val = match &self.fx.stmts[(b.last - 1) as usize].stmt {
            Node::Return { val } => val.clone(),
            _ => {
                self.fail(format!("block {} has no Return terminator", x + 1));
                return;
            }
        };
        self.operand(&val);
        self.insns.push(Instruction::Return);
    }
}

/// Emit the compiled module for one fixture: imports (memory, box/unbox),
/// the specsig function, the boxed fptr1 wrapper, exports.
pub fn emit_module(fx: &Fixture) -> Result<Vec<u8>, String> {
    // The slice's specsig vocabulary: Int64 arguments, Int64 result.
    for t in &fx.argtypes {
        if t != "Int64" {
            return Err(format!("argtype {t:?} outside the thin slice's vocabulary"));
        }
    }
    if fx.rettype != "Int64" {
        return Err(format!("rettype {:?} outside the thin slice's vocabulary", fx.rettype));
    }
    let nargs = fx.argtypes.len() as u32;

    // CFG (0-based; drop the entry pseudo-pred 0).
    let cfg = Cfg {
        succs: fx
            .blocks
            .iter()
            .map(|b| b.succs.iter().map(|&s| (s - 1) as usize).collect())
            .collect(),
        preds: fx
            .blocks
            .iter()
            .map(|b| b.preds.iter().filter(|&&p| p > 0).map(|&p| (p - 1) as usize).collect())
            .collect(),
    };

    // Terminators. A block whose last statement is not a terminator falls
    // through to its single successor.
    let term = |x: usize| -> Term {
        let b = &fx.blocks[x];
        match &fx.stmts[(b.last - 1) as usize].stmt {
            Node::Goto { dest } => Term::Goto((*dest - 1) as usize),
            Node::GotoIfNot { dest, .. } => {
                // `goto dest if not cond`: the condition holding falls
                // through to the textually next block, as IRCode lays it out.
                Term::If { then_: x + 1, else_: (*dest - 1) as usize }
            }
            Node::Return { .. } => Term::Return,
            _ => Term::Goto(cfg.succs[x][0]),
        }
    };

    // A local per value-producing statement.
    let mut ssa_local = vec![None; fx.stmts.len()];
    let mut locals: Vec<ValType> = Vec::new();
    for (i, s) in fx.stmts.iter().enumerate() {
        if matches!(s.stmt, Node::Phi { .. } | Node::Call { .. }) {
            let vt = valtype(&s.ty)
                .ok_or_else(|| format!("statement %{} has unsupported type {:?}", i + 1, s.ty))?;
            ssa_local[i] = Some(nargs + locals.len() as u32);
            locals.push(vt);
        }
    }

    let mut fe = FnEmitter { fx, ssa_local, insns: Vec::new(), err: None };
    let mut rl = Relooper::new(&cfg)?;
    rl.run(&mut fe, &term)?;
    if let Some(e) = fe.err {
        return Err(e);
    }

    // ---- module assembly ----
    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I64; nargs as usize], vec![ValType::I64]); // 0: specsig
    types.ty().function(vec![ValType::I32, ValType::I32], vec![ValType::I32]); // 1: fptr1
    types.ty().function(vec![ValType::I64], vec![ValType::I32]); // 2: rj_box_int
    types.ty().function(vec![ValType::I32], vec![ValType::I64]); // 3: rj_unbox_int

    let mut imports = ImportSection::new();
    imports.import(
        "env",
        "memory",
        MemoryType { minimum: 0, maximum: None, memory64: false, shared: false, page_size_log2: None },
    );
    imports.import("env", "rj_box_int", wasm_encoder::EntityType::Function(2));
    imports.import("env", "rj_unbox_int", wasm_encoder::EntityType::Function(3));
    // Function index space: 0 = rj_box_int, 1 = rj_unbox_int, 2 = specsig,
    // 3 = the boxed wrapper.
    const BOX: u32 = 0;
    const UNBOX: u32 = 1;
    const SPECSIG: u32 = 2;

    let mut funcs = FunctionSection::new();
    funcs.function(0);
    funcs.function(1);

    let mut exports = ExportSection::new();
    exports.export(&fx.name, ExportKind::Func, SPECSIG);
    let boxed_name = format!("{}_boxed", fx.name);
    exports.export(&boxed_name, ExportKind::Func, SPECSIG + 1);

    let mut code = CodeSection::new();

    let mut f = Function::new(locals.iter().map(|&vt| (1u32, vt)));
    for insn in &fe.insns {
        f.instruction(insn);
    }
    // Every value-bearing exit is an explicit `return` inside the emitted
    // structure; the structural fallthrough after the outermost frame is
    // dead. Saying so keeps the validator honest — reaching it is a bug.
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);
    code.function(&f);

    // The boxed wrapper: argv is a rooted slice of u32 boxed-value offsets in
    // linear memory. Unbox each argument, call the specsig, box the result.
    // nargs (param 1) is unchecked here: arity is dispatch's job (the
    // signature subtype check), as with interpreted methods.
    let mut w = Function::new([]);
    for a in 0..nargs {
        w.instruction(&Instruction::LocalGet(0));
        w.instruction(&Instruction::I32Load(MemArg {
            offset: (a as u64) * 4,
            align: 2,
            memory_index: 0,
        }));
        w.instruction(&Instruction::Call(UNBOX));
    }
    w.instruction(&Instruction::Call(SPECSIG));
    w.instruction(&Instruction::Call(BOX));
    w.instruction(&Instruction::End);
    code.function(&w);

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&funcs);
    module.section(&exports);
    module.section(&code);
    let bytes = module.finish();

    wasmparser::validate(&bytes).map_err(|e| format!("emitted module invalid: {e}"))?;
    Ok(bytes)
}
