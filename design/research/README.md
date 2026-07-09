# Research corpus — post-M1 (2026-07)

Four deep-research reports produced between M1 and M2, plus the decision
record they informed. Claims inside are labeled **VERIFIED-FROM-PIN** /
**WEB** / **INFERRED** / **UNCERTAIN** — the claim ladder applied to research.
These are *execution documents*: the engine port plan and the thin-slice spec
are meant to be worked from, not just read.

| Document | What it settles |
| - | - |
| `DECISIONS-2026-07.md` | The ratified decision record (D1–D3, faer) — transcribed into `strategy.md`; this file is the fuller version with consequences and named risk classes |
| `research-subtype-engine.md` | The union-decision machine's mechanics, why local backtracking fails the two known divergences, the `jl_varbinding_t` gap, and a staged port plan whose slice 1 heals both |
| `research-real-lowering.md` | Ground truth on the pin's lowering (still flisp; JuliaLowering experimental), the four M2 strategies, and the build-time pre-lowering recommendation |
| `research-aot-backend.md` | The forced AOT architecture (build-time `Compiler/` → typed IR → `wasm-encoder` backend), emitter verification (Cranelift has no wasm backend), contracts, and the thin-slice go/no-go spec |
| `research-faer-wasm.md` | Empirical faer-on-wasm verification: the 4-line fix, measured sizes, pulp simd128 already complete, LinearAlgebra coverage matrix from the pinned stdlib |

## Next session opens here

**Engine slices 3–5** (issues #3–#5, `research-subtype-engine.md` §6) are
now the front of the queue — the M3 spine (intersection →
`type_morespecific` → dispatch hardening), which is also what the `AOT`
node waits on. Two carried-over small items to interleave: the
**paper-and-polish batch** (below, still timeboxed and unstarted) and the
**exception-channel decision** for compiled frames (research-aot-backend §6.3
— a design decision for the human; nothing in the compiled vocabulary
throws yet). **Thin-slice stage 3** (compiled→dispatch fallback calls) is
optional polish on a proven pipeline — it can wait for the backend proper.

*(Previous opener executed 2026-07-09:)*

- ~~**The AOT thin slice**, stages 1–2 (issue #11)~~ — **GO** on every
  threshold: exact correctness incl. Int64 wrap-around, both call paths
  (specsig export + real dispatch driven by the pinned Julia's own lowering
  of `f(10)`, under GC stress), 401.8× the interpreter, 0.95×
  native-Rust-in-wasm, fptr1 3.8µs vs interpreted 47.2µs per call. Stage 2
  landed D3's hardening — the **linear-memory shadow stack** (slot arena +
  exported top cell; `Rooted`/`Frame` as veneers, one root set for both
  fronts) and the **region-base export** — and a compiled allocating
  function correct under a collection per allocation. The fixture pipeline
  doubles as the D2a probe, and the named risk *materialized and was
  caught*: a header-first layout assumption read garbage on first harness
  contact (Ruju is tag-before-object) — evidence that the
  whitelist-plus-harness regime does its job. Evidence: `implementation.md`
  (AOT + GC sections). The **paper-and-polish batch** was *not* reached
  in either stage: a `design/` note on the language choice and threat
  model (why manual rooting; what the stress test buys; the
  branded-lifetime alternative), `linguist-vendored` on `reference/julia/`
  in `.gitattributes`, and the pinned-Julia release relocation decision.

*(Previous opener executed 2026-07-07/08 — kept as the record of what this
corpus fed:)*

1. ~~**Session-start audit** of the M1 additions~~ — done (findings 22–28,
   `implementation.md`).
2. ~~**Engine slice 1**~~ (rooting fix, then the union-decision machine —
   both pre-mapped divergences healed on first run) and ~~**slice 2**~~ (the
   `forall_exists_equal` tail) — done. ~~**M2**~~ — REACHED 2026-07-08
   (C-0 vocabulary, C-1 pre-lowering pipeline, lowering oracle 4/4; the
   pinned-Julia artifact builds via
   `.github/workflows/build-pinned-julia.yml`).

The faer track proceeds independently in its own repository
(starter kit + roadmap delivered 2026-07); Ruju-side work for it (the LBT
shim, the internal ccall symbol registry) waits for Phase B.
