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
(in the `methodology.md` sense) — they are planning estimates, not evidence,
and the three `XL` research-grade items are the ones whose real cost is
genuinely unknowable up front.

## The increments

| Increment | Stream | Size | Waves | Depends on | Notes |
| - | - | - | - | - | - |
| GC exactness tail | breadth | S | W1–W2 | GC core | deferred sweep, `newpages` |
| Arrays & GenericMemory | breadth | L | W1–W3 | structs | `genericmemory.c`, `array.c` |
| Modules & bindings | breadth | M | W2–W4 | structs | `module.c`, `toplevel.c` |
| Exceptions (`enter`/`leave`) | breadth | M | W2–W3 | interpreter core | `interpreter.c`, `rtutils.c` |
| Subtype expressibility | type-depth | M | W1–W2 | subtyping core | varargs, `Type{T}`, `UnionAll` instantiation |
| Oracle expansion | type-depth | S | W1–W3 | expressibility | 53 → full `test/subtype.jl` |
| **Subtype engine** | type-depth | XL | W3–W6 | oracle | union-decision machine, `Intersect`/`Loffset`, `concrete` propagation — **research-grade** |
| Type intersection | type-depth | L | W6–W7 | subtype engine | `jl_type_intersection` |
| `type_morespecific` | type-depth | M | W7–W8 | intersection | dispatch specificity |
| Dispatch hardening | type-depth | L | W8–W9 | `morespecific`, expressibility | typemap cache, world age, ambiguity, `MethodError` |
| **Real lowering** | front-end | XL | W3–W7 | exceptions, structs, intrinsics | JuliaSyntax + JuliaLowering → `CodeInfo`; retires `frontend.rs` — **research-grade** |
| **AOT backend (Phase 1)** | compilation | XL | W9–W12 | dispatch hardening, GC exactness | build-time IR → WASM; interpreter stays as fallback — **research-grade** |
| base/ + stdlib AOT-compiled | compilation | XL | W11–W13 | modules, arrays, lowering, AOT | real Julia programs run |
| BLAS/LAPACK Phase A | performance | S | W13 | base/ | generic fallbacks — free with base/ |
| BLAS Phase B — Rust kernels | performance | L | W13–W14+ | Phase A | `gemm`/`getrf`/… behind the LBT surface |
| WebGPU offload (Phase C) | performance | L | W14+ | Phase B | large matrices behind the same interface |
| Tasks & threading | platform | L | W13–W14+ | base/ | WASM stack-switching, SharedArrayBuffer — platform-gated |

## Milestones

| # | Wave | Milestone | What it means |
| - | - | - | - |
| M1 | end W3 | Breadth online | Arrays, modules, exceptions land; the oracle grows with expressibility. Programs stay interpreted, but the vocabulary is finally Julia-shaped. |
| M2 | end W7 | Faithful front-end | `frontend.rs` retired; JuliaSyntax + JuliaLowering produce real `CodeInfo`. Source compatibility stops being a bootstrap subset. |
| M3 | end W9 | Type & dispatch faithful | Subtype engine healed against the grown oracle; intersection and `type_morespecific` in place; dispatch hardened (cache, world age, ambiguity, `MethodError`). |
| M4 | end W12 | AOT MVP | The build-time IR → WASM backend compiles the hot path; the interpreter remains the open-world fallback. |
| **M5** | **end W13** | **Real Julia in the browser** | **The end-state threshold.** `base/` and `stdlib` AOT-compiled and running through the `rj_` ABI. LinearAlgebra's generic fallbacks (BLAS Phase A) come free — linear algebra becomes a performance problem, not a correctness one. |
| M6 | W14+ | Fast & concurrent | BLAS Phase B (Rust kernels) and Phase C (WebGPU); tasks & threading as the WASM platform matures. Ongoing, platform-gated. |

## The critical path

```
subtype expressibility → subtype engine → type intersection →
type_morespecific → dispatch hardening → AOT backend → base/ + stdlib AOT
```

Breadth (arrays, modules, exceptions) and real lowering run in parallel with
this spine, but they are **joins into it, not detours off it**: `base/` cannot
be compiled until all of them hold. A parallel stream that gates the final
milestone is on the critical path in every way that matters — it just isn't
the longest single chain.

## Where the schedule lives or dies

Three increments carry nearly all the uncertainty. Every wave estimate past
`W9` is downstream of them, so a slip here slides everything after it.

- **Subtype engine.** The global union-decision machine that heals the
  oracle's one known divergence — the behavior of ~6.3k lines of C
  (`subtype.c`), including the `Intersect`/`Loffset` machinery the current
  port predates.
- **Real lowering.** Putting JuliaSyntax/JuliaLowering in place of the
  bootstrap parser is a large integration carrying its own semantics, not a
  drop-in.
- **AOT backend.** Replacing Julia's LLVM JIT with a build-time IR → WASM
  compiler is the single biggest unbuilt bet in the plan. The `XL` on that row
  and the next is doing an enormous amount of hidden work: AOT-compiling
  Julia's dynamic, open-world semantics is an unsolved problem upstream too,
  not merely a large one. This is the row to interrogate before trusting any
  wave to the right of it.
