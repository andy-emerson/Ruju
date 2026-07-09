//! ruju-aotc — the build-time AOT backend (thin slice, issue #11).
//!
//! A deliberately dumb typed-IR → WASM translator: inference and optimization
//! happen offline in the pinned Julia compiler (decision D2a); this crate
//! consumes the serialized result and emits a module against the runtime's
//! `rj_` ABI (decisions D2b/D2c: `wasm-encoder` + "Beyond Relooper",
//! two-module linking). Host-side only — it never compiles to wasm itself.
//!
//! Upstream has no C to port here (the pin carries no `codegen.cpp`); the
//! recorded divergence and its faithfulness targets are in
//! `design/implementation.md` (AOT backend section).

pub mod emit;
pub mod fixture;
pub mod relooper;

#[cfg(test)]
mod tests {
    use crate::{emit, fixture::Fixture};

    const FIXTURE: &str = include_str!("../fixtures/f_sumsq.json");

    #[test]
    fn fixture_parses() {
        let fx = Fixture::parse(FIXTURE).unwrap();
        assert_eq!(fx.name, "f");
        assert_eq!(fx.blocks.len(), 4);
        assert_eq!(fx.stmts.len(), 10);
    }

    #[test]
    fn emits_valid_module() {
        let fx = Fixture::parse(FIXTURE).unwrap();
        let bytes = emit::emit_module(&fx).unwrap(); // includes wasmparser validation
        let wat = wasmprinter::print_bytes(&bytes).unwrap();
        // The loop must be a real wasm loop over unboxed i64 locals, with the
        // branch structure the relooper owes us.
        assert!(wat.contains("loop"), "no loop emitted:\n{wat}");
        assert!(wat.contains("i64.mul"), "no unboxed multiply:\n{wat}");
        assert!(wat.contains("br 1") || wat.contains("br 0"), "no back edge:\n{wat}");
        assert!(wat.contains("(export \"f\""), "specsig not exported:\n{wat}");
        assert!(wat.contains("(export \"f_boxed\""), "wrapper not exported:\n{wat}");
        assert!(wat.contains("(import \"env\" \"memory\""), "memory not imported:\n{wat}");
    }

    #[test]
    fn rejects_vocabulary_violations() {
        let fx = Fixture::parse(FIXTURE).unwrap();
        let mut bad = fx;
        bad.rettype = "Float64".into();
        assert!(emit::emit_module(&bad).is_err());
    }

    const ALLOC_FIXTURE: &str = include_str!("../fixtures/g_refloop.json");

    #[test]
    fn emits_gcframe_for_allocating_function() {
        let fx = Fixture::parse(ALLOC_FIXTURE).unwrap();
        let bytes = emit::emit_module(&fx).unwrap(); // includes validation
        let wat = wasmprinter::print_bytes(&bytes).unwrap();
        // Allocation goes through the runtime; refs are rooted via the
        // shadow-stack ABI; field reads hit linear memory directly.
        assert!(wat.contains("rj_new_ref_int"), "no allocation import:\n{wat}");
        assert!(wat.contains("rj_gc_shadow_top_addr"), "no gcframe ABI:\n{wat}");
        assert!(wat.contains("rj_region_base"), "no region base:\n{wat}");
        assert!(wat.contains("i64.load"), "no direct field read:\n{wat}");
        assert!(wat.contains("i32.store"), "no root write-through:\n{wat}");
    }
}
