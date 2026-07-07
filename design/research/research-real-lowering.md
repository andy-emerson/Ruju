# M2 "Real lowering" — strategy research

Research date: 2026-07-07. Pin: JuliaLang/julia `d99fded7bf84695d3f7afa1e88db0058529a70bb`
(2026-06-08, v1.14.0-DEV) per `/home/user/Ruju/reference/README.md`.

Claim labels: **VERIFIED-FROM-PIN** (read in `/home/user/Ruju/reference/julia/`),
**VERIFIED-UPSTREAM-AT-PIN** (fetched from GitHub at the pinned commit — files upstream
has but the vendored subset omits), **WEB** (post-pin/adoption info from the web),
**INFERRED** (my judgment from the evidence).

---

## TL;DR

1. **Ground truth overturns the roadmap's premise.** At the pin, production lowering is
   **flisp**, not JuliaLowering. JuliaSyntax is the default *parser* (installed as
   `Core._parse` at runtime init), but `Core._lower` defaults to the flisp lowerer;
   JuliaLowering is experimental, **not even compiled into the default sysimage**
   (`base/Base.jl:339` — `const JuliaLowering = nothing`), opt-in via an incremental
   sysimage build plus `JULIA_USE_FLISP_LOWERING=false` (`base/Base.jl:422-426`). The
   roadmap's M2 phrasing "JuliaSyntax + JuliaLowering produce real `CodeInfo`"
   (`design/roadmap.md:47,60`) names a pipeline that is not yet Julia's own production
   pipeline.
2. **Strategy A (interpret JuliaSyntax+JuliaLowering) is circular as stated**: running
   those packages under interpretation requires most of `base/`, and loading `base/`
   requires... lowering. Julia breaks this circle with flisp; Ruju has no equivalent.
3. **Recommendation: Strategy C now (build-time pre-lowering), evolving into a C→A
   hybrid later.** Run the pinned real Julia offline to parse+lower source, serialize
   the resulting `CodeInfo` in a Ruju-owned stable format, load it as data, and grow the
   interpreter to the full lowered-statement set. This is maximal fidelity (it *is*
   upstream's own lowering, without porting it), it retires `frontend.rs`, it honors the
   AOT carry-forward ("interpreter consumes the same CodeInfo shape the backend will",
   `design/roadmap.md:119`), and the build-time "run real Julia, serialize compiler data
   structures" harness is exactly the front half of the M4 AOT architecture. In-browser
   `eval` of new source comes later by pre-lowering JuliaSyntax+JuliaLowering themselves
   and interpreting them (A-on-C) — aligned with upstream's own flisp-free trajectory.
   Strategy B (AOT first) is rejected; Strategy D (port flisp) is a hedge, not the plan.

---

## Part 1 — Ground truth from the pin

### 1.1 How the pinned Julia parses and lowers today

**The dispatch architecture (VERIFIED-FROM-PIN).** Both parsing and lowering are
runtime-pluggable hooks with C fallbacks:

- `jl_parse` (`src/ast.c:1290-1328`) looks up module-local `#_internal_julia_parse`,
  then `Core._parse` (`ast.c:1293-1299`); if neither is set ("In bootstrap"), it calls
  the built-in flisp parser `jl_fl_parse` (`ast.c:1302`, defined at `ast.c:745`).
- `jl_lower` (`src/ast.c:1250-1282`) looks up module-local `_internal_julia_lower`, then
  `Core._lower` (`ast.c:1254-1258`); fallback is `jl_fl_lower` (`ast.c:1260`, defined at
  `ast.c:1199-1246`), which macro-expands then calls the flisp function
  `jl-lower-to-thunk` (`ast.c:1211`). The comment at `ast.c:1248-1249` is the single
  most load-bearing sentence: *"Main C entry point to lowering. Calls jl_fl_lower during
  bootstrap, and Core._lower otherwise (**this is also jl_fl_lower unless we have
  JuliaLowering**)."*
- The hook bindings live in `base/boot.jl:1127-1149` (`Core._parse = nothing`,
  `Core._lower = nothing`, `_setparser!`, `_setlowerer!`).

**What the hooks are set to (VERIFIED-FROM-PIN).**

- During sysimage bootstrap, Base installs the **flisp** wrappers for both:
  `Core._setparser!(fl_parse)`; `Core._setlowerer!(fl_lower)`
  (`base/Base_compiler.jl:402-403`; wrappers over `jl_fl_parse`/`jl_fl_lower` in
  `base/flfrontend.jl:21-25`). So **`base/` itself is parsed and lowered by flisp** when
  the sysimage is built.
- At runtime init (`Base.__init__`), **JuliaSyntax becomes the default parser**:
  `JuliaSyntax.enable_in_core!()` unless `JULIA_USE_FLISP_PARSER` is set
  (`base/Base.jl:419-421`). JuliaSyntax is compiled *into Base* during bootstrap
  (`base/Base.jl:336-337` includes `JuliaSyntax/src/JuliaSyntax.jl`).
- **JuliaLowering is NOT activated and NOT present by default**: `const JuliaLowering =
  nothing` (`base/Base.jl:339`, comment: "May be replaced in incremental sysimage build
  after-the-fact"); activation requires both that replacement *and*
  `JULIA_USE_FLISP_LOWERING=false` (`base/Base.jl:422-426`, comment: "This is not
  available by default, but JuliaLowering can be added to Base after-the-fact via an
  incremental sysimage build"). Its hook, `core_lowering_hook`, installs via
  `Core._setlowerer!` (`JuliaLowering/src/hooks.jl:54-62`) and errors below 1.13
  (`hooks.jl:56`).

**Verdict: at this pin, production lowering is flisp-based; JuliaSyntax-based parsing is
default at runtime (flisp parses during bootstrap); JuliaLowering is an opt-in
experiment.** (VERIFIED-FROM-PIN)

**flisp itself is upstream but not vendored.** `reference/julia/src/` contains no
`flisp/` directory and no `.scm` files (verified by `find`), yet `src/ast.c:16,27-28`
includes `flisp.h` and the compiled `julia_flisp.boot.inc`, and `src/Makefile:383-384`
builds `julia_flisp.boot` from `jlfrontend.scm julia-parser.scm julia-syntax.scm
match.scm utils.scm ast.scm macroexpand.scm` plus `flisp/*.scm`. The vendored subset
simply omits the frontend it still calls. Sizes at the pinned commit
(VERIFIED-UPSTREAM-AT-PIN, fetched from raw.githubusercontent at `d99fded`):

| Component | Lines |
| - | - |
| flisp C interpreter (`src/flisp/`: flisp.c 2490, cvalues.c 1430, print.c 819, read.c 737, iostream.c 480, julia_extensions.c 439, builtins.c 424, equal.c 384, string.c 300, table.c 221, types.c 96, + headers) | **~8,550** |
| Scheme frontend programs: `julia-syntax.scm` **5,749**, `julia-parser.scm` 2,778, `macroexpand.scm` 730, `ast.scm` 568, `match.scm` 247, `jlfrontend.scm` 223, `utils.scm` 134 | **~10,430** |
| C↔scm bridge: `src/ast.c` (scm_to_julia / julia_to_scm / macro expansion driver) | 1,363 (VERIFIED-FROM-PIN) |

For scale: Ruju's entire runtime is ~4.5k lines of Rust today, and its whole front-end
(`runtime/src/frontend.rs`) is 945 lines.

**Toplevel wiring (VERIFIED-FROM-PIN).** `Core.eval` → `jl_toplevel_eval_flex`
(`src/toplevel.c:609`): lowers if not already expanded (`toplevel.c:660` calls
`jl_lower`), directly evaluates the blessed toplevel forms — `:module`
(`toplevel.c:664`), `:export`/`:public` (`:669`), `:toplevel` (`:683`),
`:error`/`:incomplete` (`:689`) — and otherwise asserts `Expr(:thunk, CodeInfo)`
(`toplevel.c:706-708`) and calls `jl_eval_thunk` (`toplevel.c:719`), which chooses
codegen vs. interpreter per `body_attributes` (has_ccall/has_defs/has_loops/forced,
`toplevel.c:466-484, 737-760`). File evaluation is `jl_parse_eval_all`
(`toplevel.c:847`): parse-all once (`:860`), then lower each expression lazily
(`:885`). The flisp lowerer's `(lambda ...)` output is converted to a `jl_code_info_t`
inside `scm_to_julia` (`ast.c:552-553`) via `jl_new_code_info_from_ir`
(`src/method.c:451`).

### 1.2 The `CodeInfo` contract, interpreter statement set, and ircode serialization

**`jl_code_info_t` (`src/julia.h:301-342`, VERIFIED-FROM-PIN).** Fields: `code`
(Any-array of statements), `debuginfo`, `ssavaluetypes`, `ssaflags`, `slotnames`,
`slotflags`, `slottypes` (deprecated), `rettype`, `parent`, `edges`,
`min_world`/`max_world`, `method_for_inference_limit_heuristics`, `nargs`, and the
boolean/`uint8`/`uint16` properties (`propagate_inbounds`, `has_fcall`,
`has_image_globalref`, `nospecializeinfer`, `isva`, `inlining`, `constprop`, `purity`,
`inlining_cost`). Note the pin's shape differs from the "early 1.12-DEV" layout quoted
in `JuliaLowering/README.md:850-883` (`codelocs`/`linetable` → `debuginfo`; added
`nargs`, `isva`, `has_image_globalref`) — the struct is still evolving release to
release.

**Statement/value forms `src/interpreter.c` consumes (VERIFIED-FROM-PIN).**
Values (`eval_value`): SSAValue (`interpreter.c:201`), Slot/Argument (`:208`),
QuoteNode (`:217`), **GlobalRef** (`:220`), bare Symbol in toplevel thunks (`:223`),
PiNode (`:226`); `Expr` heads: `:call` (`:242`), `:invoke` (`:245`), `:invoke_modify`
(`:248`), `:isdefined` (`:251`), `:throw_undef_if_not` (`:281`), `:new` (`:293`),
`:splatnew` (`:302`), `:new_opaque_closure` (`:311`), `:static_parameter` (`:322`),
`:copyast` (`:347`), `:the_exception` (`:350`), `:boundscheck` (`:353`),
`:meta`/`:inbounds`/`:loopinfo`/etc. (`:356-357`), `:gc_preserve_begin/end` (`:360`),
1-arg `:method` (`:366`), `:foreigncall` (`:369`), `:cfunction` (`:372`).
Control flow (`eval_body`): GotoNode (`:497`), GotoIfNot (`:500`), ReturnNode (`:509`),
UpsilonNode (`:512`), EnterNode incl. scope (`:521-559`), `:=` assignment (`:592`),
`:leave` (`:608`), `:pop_exception` (`:637`), 3-arg `:method` — real method definition
routing to `jl_method_def` (`:642`, `interpreter.c:108`), PhiNode/PhiCNode via
`eval_phi` (`:388-424`). (Phi/PhiC/Upsilon are not *emitted by lowering* — lowering
output is slot-based — but the interpreter handles them because it also runs
inference-optimized IR; PhiC/Upsilon appear once `slot2ssa` has run. INFERRED from
`interpreter.c:235` assert + Julia devdocs.)

**Ruju's gap (VERIFIED, `runtime/src/interp.rs:61-122`).** `Op` is only
`Ssa|Slot|Int|Float` — no boxed constants, no QuoteNode, no GlobalRef, no arbitrary
value operands. `Stmt` covers Call(builtin)/CallGeneric(id)/Assign/Goto/GotoIfNot/
Return/New/GetField/SetField/Enter/Leave/Throw/ArrayLit/ArrayRef/ArraySet/Push/Len/
Caught/Rethrow. Missing vs. the reference set: GlobalRef and general constants,
calls through *values* (Ruju calls by function-id, not by evaluated callee),
QuoteNode, PiNode/Phi/PhiC/Upsilon, `:method` (both forms — **no method definition from
IR at all**), `:new_opaque_closure`, `:splatnew`, `:isdefined`, `:static_parameter`,
`:the_exception` exists but no exception *stack* (`:pop_exception`), scoped EnterNode,
`:foreigncall`/`:cfunction`, `:meta`/ssaflags, `:copyast`, toplevel forms
(module/using/import/export/const). Also `interp.rs`'s IR is positional-jump SSA over
its own enum, not the heap `CodeInfo`-of-boxed-statements shape — the exact divergence
`design/implementation.md:430-432` already records.

**ircode.c — is serialized CodeInfo a portable artifact? (VERIFIED-FROM-PIN)**
`jl_compress_ir(jl_method_t *m, jl_code_info_t*)` (`src/ircode.c:1015`) /
`jl_uncompress_ir(m, code_instance, data)` (`:1111`) implement a compact tag-based
binary encoding (tags at `ircode.c:19-60`). It is **not** designed as a stable
interchange format: it is rooted in a `jl_method_t` (TAG_METHODROOT, `ircode.c:36` —
values are stored as indices into the method's root table), and its symbol/system
encoding depends on the build's common-symbol tables (`common_symbols1.inc` at
`ircode.c:1749`, `common_symbols2.inc` at `:1791`) and sysimage-tagged singletons.
INFERRED: usable as a *design reference* for compact statement encoding, but Strategy C
should define its own pin-versioned serialization rather than reuse this format.

### 1.3 What it takes to RUN JuliaSyntax + JuliaLowering under interpretation

**Sizes (VERIFIED-FROM-PIN):** JuliaSyntax `src/` = **12,798 lines / 18 files**;
JuliaLowering `src/` = **12,992 lines / 18 files** (desugaring.jl is the giant), plus a
21k-line test suite (44 files). JuliaLowering depends on JuliaSyntax
(`JuliaLowering/Project.toml:7-11`).

**Feature inventory (grep counts over `src/`, VERIFIED-FROM-PIN):**

| Feature | JuliaSyntax | JuliaLowering |
| - | - | - |
| String/`string(...)` use | 195 | 135 |
| `Char` handling | 53 (it's a lexer) | 2 |
| `Dict`/`Set` construction | 32 | 61 |
| closures / `do` / `->` | 50 | 542 |
| splatting `...` | 193 | 600 |
| kwargs / `;` signatures | 275 | 183 |
| `where` parametric methods | 70 | 51 |
| macro definitions | 52 | 61 |
| `@generated` | 0 | 8 |
| try/exceptions | 29 | 14 |
| `push!` (growing arrays) | 78 | 262 |
| mutable/plain structs, abstract types | 5/24/7 | 2/28/2 |
| `Base.` calls | 186 | 80 |

**Order-of-magnitude verdict (INFERRED):** these are ordinary modern Julia packages.
They need strings + Unicode, Dict/Set (hashing), the full iteration protocol, closures
and captured variables, kwargs/default-arg desugaring (each definition fanning out to
many methods — see `JuliaLowering/README.md:376-455`), varargs/splatting, exceptions,
macros (both *define* macros, and macro expansion itself must *call* macros:
`jl_expand_macros` invokes them via `jl_apply`, `ast.c:1091-1180`), `@generated`
(JuliaLowering), parametric multiple dispatch at high density, and IO/printing for
diagnostics. JuliaSyntax is included into Base *after* essentially all of Base exists
(`base/Base.jl:337` is line ~337 of a file whose includes precede it). **Running the
pipeline under interpretation ≈ running most of `base/`.** And there is a hard
circularity: you cannot *load* these packages (or the `base/` they need) without a
lowerer, and they *are* the lowerer.

### 1.4 JuliaLowering maturity and upstream trajectory

- Self-described "experimental port … work in progress; many types of syntax are not
  yet handled" (`JuliaLowering/README.md:5-8,29`); requires ≥1.13.0-DEV, "relies on
  Julia internals and may be broken on the latest Julia dev version from time to time"
  (`README.md:31`). 36 TODO/unimplemented markers in `desugaring.jl` alone.
  (VERIFIED-FROM-PIN)
- Yet it is *vendored into the upstream tree at the pin* with a 21k-line test suite,
  a `Core._lower`-shaped hook (`hooks.jl:9-62`), and Base plumbing that anticipates it
  (`boot.jl:1141-1146`, `loading.jl:2763-2764, 3094` — precompile workers forward the
  active frontend). (VERIFIED-FROM-PIN)
- WEB: as of early 2026, upstream is actively doing "preparatory work for a flisp-free
  bootstrap using JuliaLowering" (default constructor generation moved from C/flisp to
  Julia; a `TopLevelCodeIterator` / `AbstractCompilerFrontend` interface) — sources:
  [This Month in Julia World, Feb 2026](https://julialang.org/blog/2026/03/this-month-in-julia-world/),
  [JuliaLang/JuliaLowering.jl](https://github.com/JuliaLang/JuliaLowering.jl).
  Direction is clear; arrival is not dated. **Six lowering passes** are documented at
  `JuliaLowering/README.md:236-253`, ending in "Convert untyped IR to `CodeInfo`".

### 1.5 One more constraint: `eval` and `@generated` are load-bearing at runtime

- `base/` uses `@eval` **156 times across 43 files** (VERIFIED-FROM-PIN, grep) — Exprs
  are *constructed at bootstrap-runtime* (e.g. operator-family loops) and lowered on
  the spot. Pre-lowering `base/` is therefore not a static per-file transform.
- Since 1.12, generators must return `CodeInfo` (`src/method.c` in
  `jl_code_for_staged`: "As of Julia 1.12, generated functions must return `CodeInfo`.
  See `Base.generated_body_to_codeinfo`"); Expr-returning generators are converted
  Julia-side by `generated_body_to_codeinfo`, which **ccalls `jl_lower` at runtime**
  (`base/essentials.jl:1371,1440`). So a runtime with zero lowering capability cannot
  service old-style `@generated` on *new* signatures. (VERIFIED-FROM-PIN)

---

## Part 2 — The four strategies

### A. Interpret the pipeline (extend interpreter+runtime until JuliaSyntax+JuliaLowering run)

- **What it buys:** the roadmap's stated M2; in-wasm parsing+lowering; browser REPL.
- **Honest size:** the §1.3 inventory says the prerequisite is *most of `base/`* —
  strings, Dict, iteration, closures, kwargs, macros-calling-macros, `@generated`,
  high-density parametric dispatch — under a tree-walking interpreter, plus toplevel
  module machinery. That is not an M2-sized increment; it is most of M5's surface,
  interpreted.
- **The killer (INFERRED, from §1.3):** circularity. Loading `base/` +
  JuliaSyntax+JuliaLowering *source* requires a lowerer. Julia breaks the circle with
  flisp (`Base_compiler.jl:402-403`); Ruju as-is would have to grow `frontend.rs` into a
  near-complete hand-written Julia frontend just to bootstrap the real one — i.e.
  strategy A secretly contains a full second frontend that is then thrown away.
  A only becomes coherent if the pipeline is loaded *as pre-lowered data* — which is
  Strategy C applied to the pipeline itself (the C→A hybrid, below).
- **Risks:** JuliaLowering instability at the pin (§1.4); interpreter performance
  (lexing strings char-by-char under a boxed interpreter).

### B. AOT the pipeline first (pull M4 before M2)

- **Assessment: not rational (INFERRED).** (1) The AOT backend depends on dispatch
  hardening / M3 (`design/roadmap.md:48`) and is the project's single biggest unbuilt
  bet (`roadmap.md:91-96`); putting it *before* M2 puts the highest-variance item on
  the front of the critical path. (2) Compiling JuliaSyntax+JuliaLowering AOT still
  requires their **lowered (and typed) IR as compiler input** — i.e. B's build-time
  front *contains* Strategy C. (3) The compiled pipeline still needs the full runtime
  vocabulary of §1.3 at run time (strings, Dicts, dispatch); AOT removes the
  interpreter, not the runtime surface. B = C + M3 + M4 before any M2 payoff.
- **Only merit:** it forces the shared-artifact discipline early. C captures that merit
  without the inversion.

### C. Build-time pre-lowering (bootstrap through real Julia, load CodeInfo as data)

**Mechanism.** A dev-time tool runs the *pinned* real Julia binary; for each source
unit it calls the real `jl_parse`+`jl_lower` (e.g. via `Meta.lower` /
`Core._lower`, `base/meta.jl:275`) and serializes the resulting `CodeInfo` (fields are
plain Julia data: statement array of Exprs/GotoNode/GlobalRef/QuoteNode/…,
`slotnames`, `debuginfo`, `nargs`, `isva`, …) into a Ruju-defined, pin-versioned format.
Ruju's runtime gains a deserializer + the full lowered-statement interpreter and
executes thunks exactly as `jl_eval_thunk` does (`toplevel.c:719`), including 3-arg
`:method` → `jl_method_def` for **method definitions from source**.

- **Fidelity: maximal by construction.** The artifact *is* the output of upstream's
  production (flisp) lowering at the pinned commit — the same bits
  `interpreter.c` executes. No porting error is possible in the frontend because no
  frontend is ported. This is stronger than A (JuliaLowering ≠ production, §1.4) and
  stronger than D (a port can diverge; data cannot). Divergences shrink to the
  interpreter, which is auditable statement-by-statement against `interpreter.c`.
- **Precedent:** Julia itself never parses `base/` at user-runtime — the sysimage is a
  build-time artifact of exactly this kind; package precompile caches ship
  `jl_compress_ir`-compressed lowered/inferred CodeInfo (`ircode.c:1015`) that is
  *loaded as data*; and Julia's whole frontend story is already "bootstrap through a
  privileged offline lowerer" (flisp). C is the same move with the pinned Julia binary
  as the privileged lowerer. (VERIFIED-FROM-PIN + INFERRED)
- **Dev-time dependency on a real Julia binary:** acceptable — the project already
  treats Julia as its oracle (`runtime/verify_julia_subtype.mjs` fetches expected
  answers from upstream; `CLAUDE.md`), and the binary is pinned to the same commit as
  `reference/`. The dependency is *build-time only*; nothing ships to the browser.
- **Serialization format:** do **not** reuse ircode.c's encoding (method-rooted,
  common-symbol-table-coupled, version-unstable — §1.2). Define a simple explicit
  format (even JSON-ish first, binary later) that names each statement kind; version it
  with the pin commit; regenerate artifacts whenever the pin advances (already the
  audit discipline). `CodeInfo`'s own shape drifts across releases (§1.2), which
  penalizes *every* strategy equally — the pin is the stability boundary.
- **What it sacrifices:** no in-browser `eval` of *new* source strings, and no runtime
  lowering for Expr-returning `@generated` on unseen signatures (§1.5). For "run
  precompiled Julia in the browser" — the M5 threshold — neither is needed; a REPL
  needs the first. `@eval` in `base/` (156 uses) means base bootstrap cannot be
  statically pre-lowered per-file; the fix is **record-replay**: instrument `jl_lower`
  in the real Julia during a bootstrap run and record the lowered thunks in evaluation
  order (bootstrap is deterministic), then replay in Ruju. Generated-function outputs
  for signatures seen during build can be recorded the same way — which is literally
  what precompilation already does. Truly novel runtime signatures/evals wait for the
  in-wasm frontend phase. (INFERRED; the enabling facts VERIFIED-FROM-PIN in §1.5)
- **What it unblocks:** retiring `frontend.rs` (M2's definition, `roadmap.md:60`); real
  toplevel scoping (strategy.md rows 154-159 note it "rides with real lowering");
  method definitions from source; arbitrary upstream test files as conformance corpus
  (point the pre-lowerer at `reference/julia/test/*.jl`, massively widening the oracle
  beyond subtyping); and the M4 front — the AOT backend's likely architecture is "run
  Julia's own `Compiler/` at build time, serialize *typed* IR" — the same
  harness/serializer with more fields. **C is shared infrastructure with M4, built
  early and de-risked on the interpreter.**
- **Risks:** (1) statement-set breadth — the interpreter must handle everything
  real lowering emits (globals/consts, closures via lifted types + `Core.Box`,
  kwarg sorters, `:foreigncall` shims), which drags in real runtime surface (strings,
  NamedTuple, svec…) sooner than hand-written IR did — but that is M-porting work on
  Ruju's normal ladder, not research; (2) debugging through opaque generated IR;
  (3) the temptation to let the serialized format ossify into a second ABI — keep it a
  pin-versioned artifact, never a compatibility promise.

### D. Port flisp (interpreter + scm programs)

- **Size (VERIFIED-UPSTREAM-AT-PIN):** ~8.5k lines of C for the lisp VM + ~10.4k lines
  of scheme (which would run *unmodified* on a faithful VM port) + the `ast.c` bridge
  (1,363 lines) + `src/support/` pieces flisp leans on (ios/utf8/htable). The scm side
  is free if the VM is faithful; the VM port is roughly "twice the current Ruju runtime"
  in line count, in unidiomatic territory (its own GC, cvalues, tables). Compiling the
  C to wasm as a sidecar is possible but collides with Ruju's single-Rust-module thesis.
- **Pros:** upstream's *exact* production lowering AND a parser, in-wasm, self-hosted,
  no Julia binary anywhere, enables browser eval early; the scm programs are the
  ground truth itself.
- **Cons (INFERRED + WEB):** it ports the one component upstream is actively retiring
  (§1.4) — a wasting asset; `julia-parser.scm` is already non-default at the pin
  (runtime default is JuliaSyntax, `Base.jl:419-421`), so flisp *parsing* would itself
  be a divergence from pinned-default behavior; and the effort competes directly with
  C, which obtains the identical lowering output for ~zero porting risk.
- **Verdict:** not the plan, but the recorded **hedge**: if JuliaLowering stalls
  upstream for years *and* in-browser eval becomes urgent before A-on-C is affordable,
  D is the fallback that guarantees production semantics in-wasm.

---

## Part 3 — Recommendation and sequencing

**Recommended: C, staged, evolving into A-on-C. Reframe M2 as "real `CodeInfo`" rather
than "in-wasm frontend".** The faithfulness bar is about *whose lowering semantics
execute*, not *where lowering runs*.

1. **C-0 (no-regret core, needed under every strategy):** adopt the heap
   `CodeInfo`-shaped IR (boxed statement array, slots/SSA, GlobalRef/QuoteNode
   operands) and grow `interp.rs` toward `interpreter.c`'s statement set — priority:
   general constants/GlobalRef, calls through values, `:method` (1- and 3-arg),
   assignment/decl of globals, `:pop_exception`, `:isdefined`, `:splatnew`,
   `:static_parameter`. Every line here is required by A, B, C, and D alike, and honors
   the `roadmap.md:119` carry-forward.
2. **C-1 (achieves M2):** the pre-lowering tool (pinned Julia, offline) + Ruju
   deserializer; run hand-written and upstream-test Julia sources end-to-end
   (`harness.mjs` gains a "pre-lowered corpus" mode). Retire `frontend.rs` (or demote
   it to a REPL convenience until the in-wasm frontend exists). Add a lowering oracle:
   same source → pinned `Meta.lower` output vs. what Ruju executed.
3. **C-2 (toward M5, shared with M4):** record-replay of the `base/` bootstrap
   lowering stream (§1.5); extend the same harness to serialize *typed* IR from
   `Compiler/` for the AOT backend — one build-time front, two consumers.
4. **A-on-C (later, restores browser eval):** pre-lower JuliaSyntax + JuliaLowering
   (+ their base/ dependency cone) and interpret them in-wasm, installing them via the
   same `Core._parse`/`Core._lower` hook shape the C runtime already defines
   (`ast.c:1250-1258` — replicate that hook design in Ruju now, it costs nothing).
   By then, upstream's flisp-free bootstrap work (§1.4) will have matured JuliaLowering,
   and advancing the pin captures it for free.
5. **D stays a hedge** with the exact trigger recorded above.

**Why not B:** inverts the roadmap to put the highest-variance milestone first, and its
build-time front *is* C anyway. **Why not A directly:** circular without C, and sized
at "most of base/, interpreted" — several milestones disguised as one.
