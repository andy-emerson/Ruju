# Ratified decisions — post-M1 research pass (2026-07)

Design authority: andy-emerson. Source evidence: the four research reports
(research-subtype-engine.md, research-real-lowering.md, research-aot-backend.md,
research-faer-wasm.md). To be transcribed into `design/strategy.md` (decisions
+ rejected alternatives) and reflected in the roadmap rewrite, on the next
session's branch, after the session-opening audit.

## D1 — M2 is build-time pre-lowering ("Real CodeInfo")

Run the **pinned, native** Julia binary offline at build time to parse+lower
Julia source; serialize the resulting `CodeInfo` in a Ruju-owned,
pin-versioned format; Ruju loads lowered code as data. Grow `interp.rs` to
`interpreter.c`'s full lowered statement set (phi/phic/upsilon, `GlobalRef`,
`QuoteNode`, `:method`, `:pop_exception`, …) — work every alternative also
required.

- **Rejected**: interpret JuliaSyntax+JuliaLowering in-wasm (circular: needs
  most of `base/`, which needs a lowerer; also targets a component that is
  not production at the pin — lowering is still flisp, `ast.c:1248–1260`);
  AOT-first (inverts the roadmap onto its highest-variance item); flisp port
  (**held as the recorded hedge** — self-hosted exact production lowering if
  the data approach hits a wall).
- **Consequences**: M2 reframed from "in-wasm faithful front-end" to "real
  `CodeInfo`" (size ~L, was XL). In-browser `eval` of new source becomes a
  separate later milestone (pre-lowering the lowering packages themselves,
  post-M5). `frontend.rs` is retained as a dev/demo convenience (already a
  recorded divergence). New build-chain dependency: a native Julia at the
  pinned version — build-time only, temporary by design (self-hosting
  removes it post-M5). **Emscripten is used at no stage, in no role.**
- **Fidelity note**: the consumed `CodeInfo` is upstream's own lowering
  output — maximal fidelity by construction, and byte-comparable for
  per-statement divergence detection.

## D2a — Typed IR from the pinned `Compiler/` at build time

The AOT pipeline obtains inferred, optimized `IRCode` by loading the pinned
`Compiler/` as a package in the build-time Julia (`typeinf_ircode`), then
serializing it for the Rust backend. Type inference is never reimplemented.

- **Accepted risk class (named)**: cross-implementation miscompiles —
  inference's promises kept by a different runtime. Specific wires:
  64-bit-host layout folding vs Ruju's 4-byte refs (`sizeof`/`fieldoffset`);
  method-table divergence during the base/-subset transition; intrinsic
  constant-folding vs recorded intrinsic divergences (e.g. `fptosi`);
  host-Julia/pin version coupling.
- **Mitigation regime**: whitelisted IR vocabulary first → `AbstractInterpreter`
  overlay for Ruju-world queries → self-hosted base/ closes the table gap.
  Investigate a 32-bit host Julia build as an additional layout mitigation.
  The thin slice (D2d) probes this risk class before any automation trusts
  the pipeline.

## D2b — Emitter: `wasm-encoder` + "Beyond Relooper" + optional `wasm-opt`

- **Rejected**: Cranelift (verified: no wasm32 backend — frontend only);
  LLVM/inkwell (held as a later escalation if profiling demands it, not a
  start); emitting Rust source (double-lowering debuggability, build cost).

## D2c — Linking: two modules for the MVP; single-file merge stays on the table

Runtime module exports linear memory + funcref table + `rj_` entry points;
generated module imports them; host wires at instantiation. This is the first
real exercise of the composable-memory commitment (offset-based refs, no
sole-ownership assumption). **Explicitly retained option**: Binaryen
`wasm-merge` to a single deployment artifact later — the two-module MVP is
forward-compatible with it; the reverse migration is the one we avoid.

## D2d — The AOT thin slice runs early, parallel with the engine

Hand-transcribed typed-IR fixture → ~500-line `ruju-aotc` → registered in
dispatch → benchmarked. Go/no-go thresholds per research-aot-backend.md
(exact correctness; ≥100× vs interpreter at n=10⁷; within 3× of
native-Rust-in-wasm). It no longer waits for dispatch hardening.

## D3 — Pre-AOT hardening folds into owning increments

- Subtype-env GC rooting → **first commit of the engine session** (confirmed
  hazard: allocation during search with nothing rooted; the C roots exactly
  these).
- Linear-memory shadow stack (the gcframe ABI is not freezable while the
  shadow stack is a host Rust `Vec` — an M-sized increment the ledger
  under-sold) + region-base export → **prerequisites of thin-slice stage 2**
  (the first allocating compiled function forces them).

## faer — independent track (user-owned)

faer adopted as the BLAS/LAPACK replacement behind an LBT-shaped shim.
Scope revised by empirical verification: the required upstream change is a
4-line 32-bit fix (+ wasm CI); pulp's simd128/RelaxedSimd backend already
exists; `Par::Seq` is first-class; sizes measured 51–396 KiB; results
bit-identical native↔wasm. Work proceeds in a separate clone per
FAER-WASM-ROADMAP.md (thin fork, everything upstreams). The LBT shim itself
is Ruju-side (Phase B), building on upstream's `faer-ffi`; new ledger row:
Ruju's future `ccall` resolves an internal symbol registry before host
imports — the shim's registration hook.

## Next-session opening order

1. Session-start audit of the M1 additions (`memory.rs`, `array.rs`,
   `module.rs`, `errors.rs`, interpreter rework, bootstrap hierarchy) —
   required by `methodology.md` before new work.
2. Transcribe this decision record into `strategy.md`; rewrite
   `design/roadmap.md` + the Gantt artifact (new build-time-pipeline node,
   M2 redefinition, thin slice pulled forward, new hardening increments,
   in-browser-eval milestone added, faer rows collapsed).
3. Then two parallel tracks: engine slice 1 (with the rooting fix) and the
   AOT thin slice.
