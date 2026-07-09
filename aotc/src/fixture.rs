//! The serialized typed-IR fixture (`ruju-aotc-fixture-1`).
//!
//! A JSON transcription of the pinned compiler's `IRCode` for one
//! specialization: basic blocks (statement ranges, preds/succs), statements,
//! and per-statement inferred types. Produced by `tools/aot_fixture.jl`
//! running under the pinned Julia — the backend is a pure consumer and cannot
//! tell (by design) whether a fixture was generated or hand-transcribed.
//!
//! The vocabulary is deliberately closed (D2a mitigation 1: a whitelisted IR
//! vocabulary): anything outside it fails loudly at parse or emission.

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Fixture {
    pub format: String,
    pub name: String,
    pub argtypes: Vec<String>,
    pub rettype: String,
    pub blocks: Vec<Block>,
    pub stmts: Vec<Stmt>,
}

#[derive(Deserialize)]
pub struct Block {
    /// 1-based statement range, inclusive.
    pub first: u32,
    pub last: u32,
    /// 1-based predecessor block ids (0 = the entry pseudo-edge).
    pub preds: Vec<i32>,
    /// 1-based successor block ids.
    pub succs: Vec<u32>,
}

#[derive(Deserialize)]
pub struct Stmt {
    /// The inferred type, by name (`Int64`, `Bool`, `Nothing`, `Any`).
    #[serde(rename = "type")]
    pub ty: String,
    pub stmt: Node,
}

#[derive(Deserialize)]
#[serde(tag = "k")]
pub enum Node {
    #[serde(rename = "nothing")]
    Nothing,
    /// SSA φ: `edges[j]` is the 1-based predecessor block, `values[j]` the
    /// value the φ takes when entered over that edge.
    #[serde(rename = "phi")]
    Phi { edges: Vec<u32>, values: Vec<Operand> },
    /// An intrinsic call (the only call kind in the thin slice's vocabulary).
    #[serde(rename = "call")]
    Call { f: String, args: Vec<Operand> },
    /// `goto dest if not cond` — falls through to the next block otherwise.
    #[serde(rename = "gotoifnot")]
    GotoIfNot { cond: Operand, dest: u32 },
    #[serde(rename = "goto")]
    Goto { dest: u32 },
    #[serde(rename = "return")]
    Return { val: Operand },
}

#[derive(Deserialize, Clone)]
#[serde(tag = "k")]
pub enum Operand {
    #[serde(rename = "ssa")]
    Ssa { id: u32 },
    /// 1-based argument number as Julia counts them: `_1` is `#self#`.
    #[serde(rename = "arg")]
    Arg { n: u32 },
    #[serde(rename = "const")]
    Const { t: String, v: String },
    #[serde(rename = "nothing")]
    Nothing,
}

pub const FORMAT: &str = "ruju-aotc-fixture-1";

impl Fixture {
    pub fn parse(json: &str) -> Result<Fixture, String> {
        let fx: Fixture = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if fx.format != FORMAT {
            return Err(format!("fixture format {:?}, expected {:?}", fx.format, FORMAT));
        }
        if fx.blocks.is_empty() || fx.stmts.is_empty() {
            return Err("empty fixture".into());
        }
        Ok(fx)
    }
}
