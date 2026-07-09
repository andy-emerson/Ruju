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

**Type intersection** (`jl_type_intersection`) — the engine is complete as
researched (all five slices), so the M3 spine's head is now the frontier:
intersection → `type_morespecific` → dispatch hardening, the last gate on
the AOT backend. `research-subtype-engine.md` maps its consumers; the
`Intersect` node's intersection-mode arms (`constraintkind`, `intersected`,
`limited`) and `merge_env`'s under-estimation mode (`simple_meet` mode 0,
implemented but unconsumed) are where it picks up. Carried-over
interleaves: the **paper-and-polish batch** (still timeboxed, unstarted),
the **exception-channel decision** (#14, the human's call), and
**thin-slice stage 3** (#13, optional polish).

*(Executed 2026-07-09, increments two through four:)*

- ~~**Engine slice 5**~~ — `envout` (`jl_subtype_env`): the fill's full
  value-selection cascade with the ∀-arm AND-merge, right-flip
  preservation, restore clearing, and `widen_Type_if_concrete`
  (`occurs_inv`'s first consumer). Verified against the pinned binary's
  own `jl_subtype_env` — 10 native cases + the oracle's env section
  through the new `rj_subtype_env` ABI; the 134-case oracle bit-identical.
  Adaptation recorded: `tainted_inner`/`innervars` folded into the
  `has_universal_typevar` guard.

- ~~**Engine slice 3**~~ — the vararg length algebra: the `Loffset`
  channel, typevar-count `Vararg{T,N}` (the `BOUND` kind), the full
  four-kind tuple length classification, `check_vararg_length`, the
  N-equation, the ∃-var-left unwrap guard, and finding 23's expansion
  guard. Oracle 120→126 (the `NTuple` tranche, `test/subtype.jl:70,
  79–80, 85–86, 632`), all on the first run after the port.
- ~~**Engine slice 4**~~ — the `Intersect` meet node (#61917): exact
  existential upper bounds through the three-mode `simple_meet`, the
  `x <: a ∩ b` rule, `widen_intersect` at the escape point (its consumer
  is slice 5's envout), and the `concrete` cross-variable propagation
  (finding 15's tail). Oracle 126→**134/134** (the diagonal-through-union
  family `:110–124`, the abstract-lower-bound guard `:141`, and `test_3`'s
  cross-bounded existentials `:338–341`), plus pinned-Julia-verified
  native cases for the propagation itself.

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
