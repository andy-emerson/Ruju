//! IR → WASM emission over `wasm-encoder`.
//!
//! Two functions per compiled method, after the pin's `CodeInstance`
//! `invoke`/`specptr` split (`julia.h:219–221,460–461,523–535`):
//!
//! - the **specsig** entry — a native wasm signature from the inferred
//!   argtypes/rettype (`Int64 → i64`), the fast compiled→compiled path;
//! - the **boxed wrapper** (fptr1) — `(param i32 argv) (param i32 nargs)
//!   (result i32)`, unboxing each argument from the rooted argv slice in
//!   linear memory, calling the specsig, boxing the result. This is what
//!   dispatch calls.
//!
//! The module imports the runtime's memory and its `rj_` boundary exports; it
//! defines no memory, no table, and no globals of its own (the
//! composable-memory commitment, decision D2c). Placing the functions into
//! the shared funcref table is the loader's job.
//!
//! Locals: every value-producing statement gets a wasm local of its inferred
//! type — `i64` for `Int64`, `i32` for `Bool` and for heap references (region
//! offsets) — so isbits values stay unboxed throughout the body.
//! φs are deconstructed into per-edge `local.set`s.
//!
//! **The gcframe contract (thin-slice stage 2, decision D3).** A function
//! whose body allocates claims one shadow-stack slot per reference-typed
//! statement local: the prologue loads the top cell (address from the
//! imported `rj_gc_shadow_top_addr`), zeroes the claimed slots, and bumps the
//! top; every write to a ref-typed local **writes through** to its slot, so
//! the live reference is always visible to the collector; every return
//! restores the top. Field reads go straight to linear memory —
//! `i64.load(region_base + ref)`, tag-before-object layout — through the
//! imported `rj_region_base`.

use crate::fixture::{Fixture, Node, Operand};
use crate::relooper::{Cfg, Emit, Relooper, Term};
use wasm_encoder::{
    BlockType, CodeSection, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction, MemArg, MemoryType, Module, TypeSection, ValType,
};

/// The one reference type in the stage-2 vocabulary. Runtime-side it is the
/// bootstrap `RefValue{Int64}` stand-in (same name, same layout: one inline
/// Int64 field at data offset 0); `rj_new_ref_int` allocates it.
const REF_TY: &str = "Base.RefValue{Int64}";

/// Byte offset of an object's data from its Value offset. Ruju follows
/// `jl_taggedvalue_t`'s tag-**before**-object layout: the offset points at
/// the data and the 8-byte header sits behind it, so data begins at +0.
/// (The first draft assumed +8 — a header-first layout Ruju does not have —
/// and read garbage: the exact cross-implementation layout-folding hazard
/// decision D2a names. Layout facts must come from the runtime's object
/// model, never from assumption.)
const DATA_OFF: u64 = 0;

// Function index space: imports first, in this order, then the two locals.
const BOX: u32 = 0; // rj_box_int:            (i64) -> i32
const UNBOX: u32 = 1; // rj_unbox_int:        (i32) -> i64
const NEWREF: u32 = 2; // rj_new_ref_int:     (i64) -> i32
const TOPADDR: u32 = 3; // rj_gc_shadow_top_addr: () -> i32
const REGBASE: u32 = 4; // rj_region_base:        () -> i32
const SPECSIG: u32 = 5;

fn valtype(julia: &str) -> Option<ValType> {
    match julia {
        "Int64" => Some(ValType::I64),
        "Bool" => Some(ValType::I32),
        REF_TY => Some(ValType::I32), // a u32 region offset
        _ => None,
    }
}

fn is_ref(julia: &str) -> bool {
    julia == REF_TY
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
    /// gcframe slot per 1-based stmt id, for reference-typed locals.
    ref_slot: Vec<Option<u32>>,
    /// Locals holding the shadow-top cell address, the claimed frame base,
    /// and the region base (present only when the body needs them).
    ta_local: u32,
    gcbase_local: u32,
    rb_local: u32,
    has_frame: bool,
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

    /// Store the value on the stack into statement `i`'s local — and, for a
    /// reference-typed local, write it through to the gcframe slot so the
    /// live reference is rooted before the next allocation can collect.
    fn set_ssa(&mut self, i: usize) {
        let l = match self.ssa_local[i] {
            Some(l) => l,
            None => return self.fail(format!("statement %{} has no local", i + 1)),
        };
        self.insns.push(Instruction::LocalSet(l));
        if let Some(k) = self.ref_slot[i] {
            self.insns.push(Instruction::LocalGet(self.gcbase_local));
            self.insns.push(Instruction::LocalGet(l));
            self.insns.push(Instruction::I32Store(MemArg {
                offset: (k as u64) * 4,
                align: 2,
                memory_index: 0,
            }));
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
                    for a in &args.clone() {
                        self.operand(a);
                    }
                    match intrinsic(f) {
                        Some(insn) => self.insns.push(insn),
                        None => self.fail(format!("intrinsic {f:?} outside the whitelist")),
                    }
                    self.set_ssa(i as usize);
                }
                Node::New { t, args } => {
                    if t != REF_TY || args.len() != 1 {
                        self.fail(format!(":new of {t:?}/{} args outside the vocabulary", args.len()));
                        continue;
                    }
                    let a = args[0].clone();
                    self.operand(&a);
                    self.insns.push(Instruction::Call(NEWREF));
                    self.set_ssa(i as usize);
                }
                Node::GetField { obj, field } => {
                    if field != "x" {
                        self.fail(format!("getfield :{field} outside the vocabulary"));
                        continue;
                    }
                    // i64.load(region_base + ref + DATA_OFF): field `x` is
                    // the single inline Int64 at the start of the data.
                    self.insns.push(Instruction::LocalGet(self.rb_local));
                    self.operand(&obj.clone());
                    self.insns.push(Instruction::I32Add);
                    self.insns.push(Instruction::I64Load(MemArg {
                        offset: DATA_OFF,
                        align: 3,
                        memory_index: 0,
                    }));
                    self.set_ssa(i as usize);
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
            self.set_ssa(i as usize);
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
        // Epilogue before every value-bearing exit: restore the shadow top.
        if self.has_frame {
            self.insns.push(Instruction::LocalGet(self.ta_local));
            self.insns.push(Instruction::LocalGet(self.gcbase_local));
            self.insns.push(Instruction::I32Store(MemArg { offset: 0, align: 2, memory_index: 0 }));
        }
        self.operand(&val);
        self.insns.push(Instruction::Return);
    }
}

/// Emit the compiled module for one fixture: imports (memory, the `rj_`
/// boundary), the specsig function, the boxed fptr1 wrapper, exports.
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

    // A local per value-producing statement; a gcframe slot per ref-typed one.
    let mut ssa_local = vec![None; fx.stmts.len()];
    let mut ref_slot = vec![None; fx.stmts.len()];
    let mut locals: Vec<ValType> = Vec::new();
    let mut nrefs: u32 = 0;
    let mut has_getfield = false;
    for (i, s) in fx.stmts.iter().enumerate() {
        if matches!(s.stmt, Node::GetField { .. }) {
            has_getfield = true;
        }
        if matches!(
            s.stmt,
            Node::Phi { .. } | Node::Call { .. } | Node::New { .. } | Node::GetField { .. }
        ) {
            let vt = valtype(&s.ty)
                .ok_or_else(|| format!("statement %{} has unsupported type {:?}", i + 1, s.ty))?;
            ssa_local[i] = Some(nargs + locals.len() as u32);
            locals.push(vt);
            if is_ref(&s.ty) {
                ref_slot[i] = Some(nrefs);
                nrefs += 1;
            }
        }
    }
    let has_frame = nrefs > 0;
    // Helper locals appended after the statement locals.
    let mut next = nargs + locals.len() as u32;
    let mut aux = |on: bool, locals: &mut Vec<ValType>| -> u32 {
        if !on {
            return u32::MAX;
        }
        locals.push(ValType::I32);
        let l = next;
        next += 1;
        l
    };
    let ta_local = aux(has_frame, &mut locals);
    let gcbase_local = aux(has_frame, &mut locals);
    let rb_local = aux(has_getfield, &mut locals);

    let mut fe = FnEmitter {
        fx,
        ssa_local,
        ref_slot,
        ta_local,
        gcbase_local,
        rb_local,
        has_frame,
        insns: Vec::new(),
        err: None,
    };

    // Prologue: claim and zero the gcframe (top-cell load, slot zeroing, top
    // bump), and cache the region base. Runs before any allocation.
    if has_frame {
        fe.insns.push(Instruction::Call(TOPADDR));
        fe.insns.push(Instruction::LocalSet(ta_local));
        fe.insns.push(Instruction::LocalGet(ta_local));
        fe.insns.push(Instruction::I32Load(MemArg { offset: 0, align: 2, memory_index: 0 }));
        fe.insns.push(Instruction::LocalSet(gcbase_local));
        for k in 0..nrefs {
            fe.insns.push(Instruction::LocalGet(gcbase_local));
            fe.insns.push(Instruction::I32Const(0));
            fe.insns.push(Instruction::I32Store(MemArg {
                offset: (k as u64) * 4,
                align: 2,
                memory_index: 0,
            }));
        }
        fe.insns.push(Instruction::LocalGet(ta_local));
        fe.insns.push(Instruction::LocalGet(gcbase_local));
        fe.insns.push(Instruction::I32Const((nrefs * 4) as i32));
        fe.insns.push(Instruction::I32Add);
        fe.insns.push(Instruction::I32Store(MemArg { offset: 0, align: 2, memory_index: 0 }));
    }
    if has_getfield {
        fe.insns.push(Instruction::Call(REGBASE));
        fe.insns.push(Instruction::LocalSet(rb_local));
    }

    let mut rl = Relooper::new(&cfg)?;
    rl.run(&mut fe, &term)?;
    if let Some(e) = fe.err {
        return Err(e);
    }

    // ---- module assembly ----
    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I64; nargs as usize], vec![ValType::I64]); // 0: specsig
    types.ty().function(vec![ValType::I32, ValType::I32], vec![ValType::I32]); // 1: fptr1
    types.ty().function(vec![ValType::I64], vec![ValType::I32]); // 2: box/newref
    types.ty().function(vec![ValType::I32], vec![ValType::I64]); // 3: unbox
    types.ty().function(vec![], vec![ValType::I32]); // 4: topaddr/regbase

    let mut imports = ImportSection::new();
    imports.import(
        "env",
        "memory",
        MemoryType { minimum: 0, maximum: None, memory64: false, shared: false, page_size_log2: None },
    );
    imports.import("env", "rj_box_int", wasm_encoder::EntityType::Function(2));
    imports.import("env", "rj_unbox_int", wasm_encoder::EntityType::Function(3));
    imports.import("env", "rj_new_ref_int", wasm_encoder::EntityType::Function(2));
    imports.import("env", "rj_gc_shadow_top_addr", wasm_encoder::EntityType::Function(4));
    imports.import("env", "rj_region_base", wasm_encoder::EntityType::Function(4));

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
