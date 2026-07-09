# Roadmap

A sequencing view of the path from today's Phase-0 subset to the end state
(all of Julia, running in the browser). This is a **derived** document:
`strategy.md`'s dependency map is the source of truth for *what is unblocked*,
and this file only lays those same nodes on an effort axis to make the shape
of the whole journey legible at once.

**The axis is effort waves (`W1`–`W14`), not calendar dates.** This is
deliberate and consistent with `strategy.md`: for a mechanical port sitting
next to research-grade subtype and codegen work, invented durations would be
the exact false-precision the project's culture exists to prevent. A wave
encodes **order and relative size**, nothing more. When a gate closes and
unblocks several nodes at once, the honest move is to slide the waves — never
to convert them into a date.

**Contents:**
[How to read this](#how-to-read-this) ·
[The increments](#the-increments) ·
[Milestones](#milestones) ·
[The critical path](#the-critical-path) ·
[Where the schedule lives or dies](#where-the-schedule-lives-or-dies)

## How to read this

Each increment carries a **stream** (which subsystem it deepens), a rough
**size** (`S`/`M`/`L`/`XL`, relative effort only), the **waves** it spans, and
its **dependency** into the rest of the graph. Sizes and waves are *Stated*
(in the `methodology.md` sense) — they are planning estimates, not evidence.
*(2026-07: the research pass retired most of the original uncertainty — the
engine is demoted to staged hard engineering, M2 shrank to ~L, and the AOT
row's remaining hatch is a named, early-probed risk; see
`design/research/`.)*

## The increments

| Increment | Stream | Size | Waves | Depends on | Notes |
| - | - | - | - | - | - |
| GC exactness tail | breadth | S | W1–W2 | GC core | ~~done 2026-07~~: `newpages` landed; "deferred sweep" was a mischaracterized pin (finding 21) |
| Arrays & GenericMemory | breadth | L | W1–W3 | structs | ~~done 2026-07~~ (1-D subset, linear-memory buffer, growth, syntax) |
| Modules & bindings | breadth | M | W2–W4 | structs | ~~core done 2026-07~~; toplevel scoping rides with real lowering |
| Exceptions (`enter`/`leave`) | breadth | M | W2–W3 | interpreter core | ~~done 2026-07~~ (reified values, `finally`; exception stack later) |
| Subtype expressibility | type-depth | M | W1–W2 | subtyping core | ~~done 2026-07~~ (varargs, `Type{T}`, `UnionAll` inst.; typevar-`N` → engine) |
| Oracle expansion | type-depth | S | W1–W3 | expressibility | 53 → 106 → **120** (2026-07), the 2 pre-mapped engine divergences healed and promoted; keeps growing |
| **Subtype engine** | type-depth | XL | W3–W6 | oracle | ~~slices 1–2 done 2026-07~~ (rooting fix, the union-decision machine + drivers — both pre-mapped divergences healed on first run — and the `forall_exists_equal` tail with the explosion guards); ~~slices 3–4 done 2026-07-09~~ (the vararg length algebra: `Loffset`, typevar-`N` `Vararg`, the N-equation, finding 23 closed; the `Intersect` meet node + `concrete` propagation, finding 15 closed; oracle → 134); remaining per `design/research/research-subtype-engine.md` §6: slice 5 (`envout`) |
| Type intersection | type-depth | L | W6–W7 | subtype engine | `jl_type_intersection` |
| `type_morespecific` | type-depth | M | W7–W8 | intersection | dispatch specificity |
| Dispatch hardening | type-depth | L | W8–W9 | `morespecific`, expressibility | typemap cache, world age, ambiguity, `MethodError` |
| **Real `CodeInfo` (M2)** | front-end | L *(was XL)* | W3–W6 | build-time pipeline; interpreter completeness | decision D1: build-time pre-lowering; serialized-`CodeInfo` loader + full lowered statement set in `interp.rs`; `frontend.rs` kept as dev convenience. C-0 begun 2026-07 (operands, calls through values); the pinned-Julia artifact is building |
| Build-time pipeline | shared infra | M | W3–W4 | — | the offline harness M2 and M4 share: pinned Julia → serialized `CodeInfo` (M2) / typed `IRCode` (M4) |
| **AOT thin slice** | compilation | M | W3–W5 | build-time pipeline (fixture may be hand-transcribed) | ~~stages 1–2 done 2026-07-09~~ — **GO**: 401.8× interpreter, 0.95× native, both call paths, two-module linking, gcframe emission stress-proven (`implementation.md`, AOT section). Stage 3 (compiled→dispatch fallback calls) + the exception-channel decision remain |
| Linear-memory shadow stack + region-base export | runtime hardening | M | W4–W5 | — | ~~done 2026-07-09~~ (thin-slice stage 2): the slot arena + exported top cell, `Rooted`/`Frame` as veneers, one root set for both fronts; `rj_region_base` exported |
| **AOT backend (Phase 1)** | compilation | XL | W9–W12 | dispatch hardening; thin slice passed | decision D2: typed IR from the pinned `Compiler/` at build time (inference never reimplemented); `wasm-encoder` emission; two-module linking (merge kept on the table). Hatching now means the **named semantic-gap risk** (whitelist → overlay → self-hosted base), probed early by the thin slice — no longer "possibly impossible" |
| base/ + stdlib AOT-compiled | compilation | XL | W11–W13 | modules, arrays, lowering, AOT | real Julia programs run |
| BLAS/LAPACK Phase A | performance | S | W13 | base/ | generic fallbacks — free with base/ |
| BLAS Phase B — Rust kernels | performance | L | W13–W14+ | Phase A | `gemm`/`getrf`/… behind the LBT surface |
| WebGPU offload (Phase C) | performance | L | W14+ | Phase B | large matrices behind the same interface |
| Tasks & threading | platform | L | W13–W14+ | base/ | WASM stack-switching, SharedArrayBuffer — platform-gated |

## Milestones

| # | Wave | Milestone | What it means |
| - | - | - | - |
| M1 | end W3 | Breadth online — **REACHED 2026-07** | Arrays, modules, exceptions landed; oracle 53→106 with 2 engine divergences pre-mapped; exceptions are reified values with `finally`; GC tail closed (finding 21). Programs stay interpreted, but the vocabulary is Julia-shaped. |
| M2 | end W7 | **Real `CodeInfo`** *(redefined 2026-07 — decision D1)* — **REACHED 2026-07-08**, ahead of its wave | The build-time pipeline (pinned native Julia, offline) pre-lowers source; Ruju loads serialized `CodeInfo` as data and executes it — same-source agreement with the pinned Julia pinned by the lowering oracle (4/4 corpus: globals, method definitions + calls, try/catch, loops). Remaining depth rides the corpus (heap-`CodeInfo` in-memory form, tuple values, strings — issues #6/#7). In-browser `eval` remains post-M5 (M5.5). |
| M3 | end W9 | Type & dispatch faithful | Subtype engine healed against the grown oracle; intersection and `type_morespecific` in place; dispatch hardened (cache, world age, ambiguity, `MethodError`). |
| M4 | end W12 | AOT MVP | The build-time IR → WASM backend compiles the hot path; the interpreter remains the open-world fallback. |
| **M5** | **end W13** | **Real Julia in the browser** | **The end-state threshold.** `base/` and `stdlib` AOT-compiled and running through the `rj_` ABI. LinearAlgebra's generic fallbacks (BLAS Phase A) come free — linear algebra becomes a performance problem, not a correctness one. |
| M5.5 | W13+ | In-browser eval | JuliaSyntax + JuliaLowering, themselves pre-lowered, run under interpretation in-wasm: typing new code in the browser works. The self-hosting point — the build-time Julia dependency can be dropped. |
| M6 | W14+ | Fast & concurrent | BLAS Phase B (Rust kernels) and Phase C (WebGPU); tasks & threading as the WASM platform matures. Ongoing, platform-gated. |

## The critical path

```
subtype expressibility → subtype engine → type intersection →
type_morespecific → dispatch hardening → AOT backend → base/ + stdlib AOT
```

Breadth (done), real `CodeInfo`, and the build-time pipeline run in parallel
with this spine — and the **thin slice** now probes the AOT link early rather
than waiting for it. They are **joins into the spine, not detours off it**:
`base/` cannot be compiled until all of them hold. A parallel stream that gates the final
milestone is on the critical path in every way that matters — it just isn't
the longest single chain.

## Where the schedule lives or dies

Three increments carry nearly all the uncertainty. Every wave estimate past
`W9` is downstream of them, so a slip here slides everything after it.

*(Rewritten 2026-07 after the research pass — `design/research/`.)*

- **The AOT semantic gap** (now the top risk): stock-Julia inference's
  promises kept by a different runtime — layout folding vs 4-byte refs,
  method-table divergence, intrinsic-folding vs recorded divergences.
  Mitigation regime: whitelisted IR vocabulary → `AbstractInterpreter`
  overlay → self-hosted `base/`; probed by the **early thin slice**.
- **The base/ bootstrap grind**: the long tail of runtime surface `base/`
  needs before it loads — large but enumerable, not research.
- **Pin coupling**: serialized `CodeInfo`/`IRCode` formats tie to the pin;
  advancing the pin now includes revalidating the build-time pipeline.
- *Demoted*: the subtype engine (mechanism understood, staged,
  instrument-verified) and real lowering (reframed to data consumption) no
  longer carry schedule-killing uncertainty.

## AOT-readiness carry-forward

The interpreter and the eventual AOT'd code are two front-ends over **one**
heap, one IR, and one dispatch service (`strategy.md`). Most of what makes an
increment "AOT-ready" costs almost nothing while that increment is being built
the first time, and becomes a rewrite once the backend depends on it. The rows
below are those constraints, each **bound to the increment that owns it** —
recorded here so the constraint travels with the work when it is picked from
the frontier. Per `methodology.md`, a row graduates into an `implementation.md`
obligation at build time; the point is that **no separate "make X AOT-ready"
item is ever created — the retrofit item is avoided, not scheduled.**

The litmus behind every row: *could a compiled function, running as raw WASM
with no interpreter present, do this by touching only linear memory and
defined runtime entry points?* If meaning lives in a host-side Rust structure
instead, the compiler has nothing to emit.

| Carry-forward constraint | Owning increment | Cost now | Risk if deferred |
| - | - | - | - |
| ~~`GenericMemory` backed by a linear-memory buffer in Julia's layout~~ — **honored** (arrays slice 1, 2026-07): `[length, ptr]` + inline data in the region; element access is `region[ptr + i*elsz]` (`implementation.md`, Arrays) | Arrays & GenericMemory | ~none (it is how you build it once you know) | **high** — array access is the hottest path; a host-`Vec` backing kills the perf thesis at the array boundary and forces a full rewrite |
| `enter`/`leave` modeled as an explicit handler stack **in linear memory** (not a Rust `Result`/`panic`) so compiled and interpreted code unwind through one mechanism | Exceptions | low | **high** — a compiled function cannot return a Rust `Result`; the wrong mechanism means rewriting unwinding |
| Interpreter consumes the **same** `CodeInfo` shape the backend will (retiring `frontend.rs`'s ad-hoc IR) | Real lowering | already the plan | medium — otherwise two IRs and a permanent translation layer |
| Method resolution is a pure, reusable `(f, argtypes) → method` query, plus a defined stable **world** to compile against, so the backend can devirtualize at build time and share the runtime fallback | Dispatch hardening | low–med | medium — no build-time devirtualization means most AOT speed is left on the table |
| A defined **calling convention** for a method — gcframe threading, which args arrive boxed vs. unboxed, how the result returns — shared by interpreter and compiled methods | Dispatch hardening / AOT backend | low | medium — marshalling at every interpreter↔compiled fallback boundary otherwise |
| ~~The gcframe / shadow-stack layout frozen as an ABI contract~~ — **honored** (thin-slice stage 2, 2026-07-09): the slot arena + top cell in linear memory, byte-for-byte specified (`implementation.md`, GC section); compiled prologues/epilogues emit against it directly | thin-slice stage 2 | done | — |
| Ruju's future `ccall` resolves an **internal symbol registry** before host/JS imports — the hook where the LBT shim registers `dgemm_64_` and friends (decision: faer, 2026-07) | FFI increment | ~none | medium — retrofitting symbol resolution after ccall ships means reworking every call site |
| ~~Region base kept cheaply reachable by compiled code~~ — **honored** (thin-slice stage 2, 2026-07-09): `rj_region_base` exported; compiled functions cache it in a local, `base + offset` addressing | `rj_` ABI | done | — |
| Intrinsics stay pure and value-typed; nothing relies on **heap identity** for primitives (egal-by-bits already holds) so the backend is free to unbox into `i64`/`f64` locals | standing invariant | none (already true) | low — cheap to violate by accident, and a violation blocks unboxing wholesale |
| Layout features the backend will need before it can compile those field cases: inline isbits unions (selector bytes), inline immutables containing pointers (`hasptr`/`first_ptr`), atomics | Structs layout tail | med | medium — the backend cannot compile those field accesses until the layout supports them |

**How this reduces the backlog.** Every row above is an item that *does not
get added* if the owning increment honors it the first time. The cheapest way
to shrink the total is therefore not to find new quick wins but to refuse to
manufacture retrofit work — build each pre-AOT increment AOT-consciously, and
the AOT stage inherits a runtime it can compile against instead of one it must
first repair.
