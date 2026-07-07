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

1. **Session-start audit** of the M1 additions (`memory.rs`, `array.rs`,
   `module.rs`, `errors.rs`, the interpreter rework, the bootstrap hierarchy
   changes) — required by `methodology.md` before new module work.
2. Then two parallel tracks, both fully specified in this corpus:
   **engine slice 1** (first commit: the subtype-env rooting fix — the
   confirmed hazard in `research-subtype-engine.md` §7) and the
   **AOT thin slice** (spec in `research-aot-backend.md`; its stage 2
   forces the linear-memory shadow stack + region-base export).

The faer track proceeds independently in its own repository
(starter kit + roadmap delivered 2026-07); Ruju-side work for it (the LBT
shim, the internal ccall symbol registry) waits for Phase B.
