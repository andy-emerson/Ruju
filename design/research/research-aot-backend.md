# Feasibility research: Phase-1 AOT backend (Julia IR → WASM at build time)

Date: 2026-07-07. Repo: /home/user/Ruju (read-only). Claim labels:
**VERIFIED-FROM-PIN** (read in the vendored reference or our runtime, file:line
cited), **WEB** (URL cited), **INFERRED** (reasoned from verified facts),
**UNCERTAIN** (needs an experiment or a deeper read).

## TL;DR

The Phase-1 AOT backend is feasible and its architecture is largely forced by
decisions already made and precedents that already exist:

1. **Consume inferred+optimized typed IR produced offline by a *stock* Julia
   1.12+ running the vendored `Compiler/` as a package** (`typeinf_ircode` /
   `code_ircode` machinery), serialized as data. The Rust backend is then an
   IR-to-WASM translator, not a compiler front-half. WebAssemblyCompiler.jl
   proves the "typed Julia IR → wasm functions" pipeline works end to end today.
2. **Emit WASM directly with the `wasm-encoder` crate** (Bytecode Alliance,
   actively maintained), plus a small structured-control-flow reconstruction
   (Ramsey's "Beyond Relooper", a single-pass published algorithm), plus an
   optional `wasm-opt` post-pass via the `wasm-opt` crate. Cranelift is **not**
   an option — it has no wasm32 *backend* (wasm is a frontend only; backends
   are x64/aarch64/s390x/riscv64). LLVM-via-inkwell works but is a heavyweight
   build dependency for no phase-1 benefit.
3. **The interpreter fallback is the decisive relaxation.** juliac/StaticCompiler
   must *prove* the closed world (dynamic dispatch is a compile *error* for
   `--trim`; StaticCompiler has no GC and no runtime at all). Ruju only needs
   compilation to be an *optimization*: any statement inference can't make
   concrete compiles to a call into `rj_dispatch` (boxed convention), and any
   whole function the backend can't handle simply stays interpreted. This is
   exactly upstream Julia's own JIT contract (compile what's provable, fall
   back to `jl_apply_generic`), so it is also the *faithful* design.
4. **Two linked modules for the MVP** (runtime .wasm exports memory + funcref
   table; the compiled-code .wasm imports them), merged into one module later
   with Binaryen's `wasm-merge` when single-file shipping matters.
5. **One pre-existing ledger row is not yet honored**: the shadow stack is a
   host-side Rust `Vec` (`gc.rs:31–37`), not a linear-memory ABI. The ledger
   (`roadmap.md:122`) says freeze it "now, while the surface is tiny". The
   thin slice can start without it (a no-allocation function needs no gcframe),
   but slice 2 forces the freeze.

**Thin slice (go/no-go)**: hand-construct the typed IR of
`f(n) = (acc=0; i=1; while i<=n; acc+=i*i; i+=1; end; acc)` exactly as stock
Julia's `code_ircode(f, (Int64,))` produces it; a ~500-line Rust backend
(wasm-encoder) emits a `(func (param i64) (result i64))`; the harness
instantiates it against the runtime module, registers it in the dispatch
table via a funcref-table index, and calls it through *both* the export and
the dispatch path. Success: identical results to the interpreter on
{0, 1, 10, 10^6}; ≥100× faster than the interpreter at n=10^7; within 3× of a
native-Rust-in-wasm loop. Details in §7.

---

## 1. Where the runtime stands today (all VERIFIED-FROM-PIN)

What the backend must compile *against*:

- **Interpreter IR** (`runtime/src/interp.rs:70–121`): a Rust `Stmt` enum —
  `Call(Builtin)`, `CallGeneric(u32 func-id)`, `Assign`, `Goto`, `GotoIfNot`,
  `Return`, `New`, `Get/SetField`, `Enter/Leave/Throw/Caught/Rethrow`,
  `ArrayLit/Ref/Set/Push/Len`. It is lowered-CodeInfo-shaped (mutable slots +
  SSA results, ip loop — `interp.rs:1–14`), *untyped*, and hand-built by the
  bootstrap front-end (`frontend.rs`). Every operand is **boxed**: `Op::Int`
  boxes through `box_int` on each read (`interp.rs:131–138`), and each
  arithmetic op unboxes, computes, re-boxes (`interp.rs:184–206`). So the
  interpreter allocates on essentially every statement — the AOT speedup
  headroom is enormous (INFERRED: 2–3 orders of magnitude for isbits loops).
- **Dispatch** (`runtime/src/dispatch.rs:25–92`): a host-side `Vec<Entry>` of
  `(func: u32, sig: tuple-type Offset, body: interp::Body)`; `invoke` computes
  argtypes, selects most-specific by subtyping, and `body.clone()`s per call
  (`dispatch.rs:88` — a per-call deep clone; harmless now, worth noting).
  Method resolution as a pure `(f, argtypes) → method` query and a stable
  world are ledger obligations owned by dispatch hardening (`roadmap.md:120`).
- **GC / rooting** (`runtime/src/gc.rs`): mandatory shadow stack, RAII
  `Rooted`/`Frame`. **Finding: the shadow stack is a host Rust
  `Vec<Value>`** (`gc.rs:31–37`, `slots()` at `:40–43`) — it lives in linear
  memory only incidentally (Rust's heap), with no defined layout. The ledger
  row "gcframe / shadow-stack layout frozen as a documented ABI contract —
  freeze now, while the surface is tiny" (`roadmap.md:122`) is therefore
  **not yet honored**. Compiled code cannot emit pushes into a Rust `Vec`
  except by calling exported helpers; the freeze means moving root slots to a
  known linear-memory area with a stack-pointer global (see §6.2).
- **Heap addressing**: all references are u32 offsets from a region base; the
  region is a 1 MiB static buffer (`region.rs:11,28–42`) addressed via
  `ptr_mut` (`region.rs:80–82`). `GenericMemory` is `[length:u32@0, ptr:u32@4]`
  + inline element data, element access `region[ptr + i*elsz]`
  (`memory.rs:1–38`) — the linear-memory carry-forward already honored
  (`roadmap.md:117`). `Array` is `[mem@0, offset@4, length@8]` over it
  (`array.rs:33–37`). The region base itself is a Rust static whose wasm
  address is fixed at link time but **not currently exported** — the "region
  base kept cheaply reachable (a known global)" ledger row (`roadmap.md:123`)
  needs a one-line export (`rj_region_base()` or a wasm global).
- **`rj_` ABI + host**: `#[no_mangle] extern "C"` exports (`lib.rs:96` ff.);
  the JS harness instantiates one module with **no imports**
  (`runtime/harness.mjs:24`) and calls `rj_*`. i64 crosses as BigInt
  (`harness.mjs:44`).
- **The reference has no JIT to port**: the vendored `reference/julia/src/`
  (87 files) contains **no** `codegen.cpp`, `jitlayers.cpp`, `aotcompile.cpp`,
  or any `llvm-*` file (verified by listing; `design/implementation.md:620`
  records the AOT backend as the replacement for the removed pair). The
  backend is by construction a **recorded divergence**, not a port — the
  faithfulness target is *behavior* (Julia semantics at the IR level), plus
  faithful reuse of upstream's compilation *architecture* where it exists in
  Julia-level code (`Compiler/`) and C data structures (`CodeInstance`).

## 2. The vendored `Compiler/` surface (VERIFIED-FROM-PIN)

The key discovery: **the vendored compiler is the standard-library Compiler.jl
package and runs on stock Julia**, not only inside a bespoke runtime.

- `reference/julia/Compiler/src/Compiler.jl:1–33` + `Compiler/Project.toml`:
  the package installs on any Julia ≥ `v1.12.0-DEV.1581`; on matching versions
  it can replace the sysimage compiler, otherwise it loads as an ordinary
  package. So a build-time pipeline can `using Compiler` (pinned to
  `reference/julia/Compiler`) in a stock 1.12/1.13 Julia and get *exactly the
  pinned inference/optimization behavior*, version-locked to the reference.
- **Entry points** (`Compiler/src/typeinfer.jl`):
  - `typeinf_ircode(interp, mi, optimize_until) → (IRCode, rettype)`
    (`typeinfer.jl:1349–1360`): runs inference (`typeinf_frame`), then
    `run_passes_ipo_safe` up to any stage — the exact hook the backend wants.
  - `typeinf_code(...) → CodeInfo` (`:1326`), `typeinf_type` (`:1550`),
    `typeinf_ext_toplevel(methods, worlds, trim_mode)` (`:1840` — the batch
    entry juliac-style drivers use).
  - User-facing wrapper: `Base.code_ircode(f, types)` / `code_ircode_by_type`
    (`reference/julia/base/reflection.jl:483–518`), which resolves the method
    match and calls `typeinf_ircode` through the interp-compiler indirection.
- **Optimization pipeline** (`Compiler/src/optimize.jl:1057–1087`):
  `convert_to_ircode → slot2reg → compact! → ssa_inlining_pass! → compact! →
  sroa_pass! → adce_pass! → compact!` — i.e. SSA conversion, inlining, SROA,
  aggressive DCE all happen *in Julia code we already vendor*, and
  `optimize_until` lets the backend take IR at any stage.
- **IR objects**: `IRCode` (`Compiler/src/ssair/ir.jl`) is the SSA CFG form
  (statements, types per statement, `CFG` of `BasicBlock`s, `PhiNode`s);
  `CodeInfo` is the flat lowered/typed array form; `ir_to_codeinf!`
  round-trips (`typeinfer.jl:1381`). `CodeInstance` is the per-(method,
  argtypes, world) compilation product with the two entry pointers —
  `invoke` (boxed jlcall: `jl_value_t *(*)(jl_value_t*, jl_value_t**, uint32)`,
  `reference/julia/src/julia.h:219–221`) and `specptr` (the unboxed specsig,
  "mandatory if specsig is valid", `julia.h:460–461,523–535`). This
  two-entry-point design is the calling-convention precedent §6.3 adopts.
- **Custom-interpreter hook**: `AbstractInterpreter` (types.jl,
  abstractinterpretation.jl) lets the build-time driver override method-table
  lookup and native paths (overlays) — the same mechanism GPUCompiler and
  WebAssemblyCompiler.jl use to redirect, e.g., `Base` internals that Ruju
  implements differently (WEB, §3.3; VERIFIED-FROM-PIN that the machinery is
  in the pin).

**Answer to the mission's key question**: yes — the vendored compiler can run
in a stock Julia at build time and produce inferred+optimized `IRCode` for a
given (method, argtypes) pair, via `typeinf_ircode`/`code_ircode`, with no C
runtime changes. (VERIFIED-FROM-PIN for the API surface; the residual
UNCERTAIN is only operational — package-loading friction on a specific stock
version — and is cheap to probe.)

## 3. State of the art (WEB)

### 3.1 juliac / `--trim` (Julia 1.12)

- Julia 1.12 ships an experimental `--trim` that compiles a system image
  keeping only code reachable from declared entry points; the driver is
  JuliaC.jl (installable as a package). ~1.1 MB executables for hello-world,
  vs ~90% larger under PackageCompiler.
  [Julia 1.12 highlights](https://julialang.org/blog/2025/10/julia-1.12-highlights/index.html),
  [JuliaLang/JuliaC.jl](https://github.com/JuliaLang/JuliaC.jl).
- **The constraint that matters for us**: reachable code "must not have any
  dynamic dispatches, otherwise the trimming will be unsafe and it will error
  during compilation" (JuliaC README). Verification lives in the pin as
  `Compiler/src/verifytrim.jl` (VERIFIED-FROM-PIN, file exists in tree) —
  trim is *verified* closed-world, still LLVM-emitting, still bundling
  libjulia. Practitioner reports confirm the sharp edges
  ([LWN](https://lwn.net/Articles/1006117/),
  [AoC 2025 review](https://viralinstruction.com/posts/aoc2025/)).
- **Lesson**: even upstream, with total knowledge of its own runtime, does not
  attempt open-world AOT — it either ships the JIT or proves the closed world.
  Ruju's interpreter fallback is the third road: *unproven* world, compiled
  hot path, interpreted residue. Strategy already records this trichotomy
  (`design/strategy.md:31–39`).

### 3.2 StaticCompiler.jl + GPUCompiler.jl

- StaticCompiler compiles single type-stable functions to standalone
  native objects via GPUCompiler (which drives Julia's own codegen to LLVM IR
  with a custom AbstractInterpreter). Without libjulia there is **no GC**
  (no heap allocation), no dynamic dispatch, no error handling except
  overridden throws, no globals
  ([StaticCompiler.jl](https://github.com/tshort/StaticCompiler.jl),
  [StaticTools.jl](https://github.com/brenhinkeller/StaticTools.jl)).
- **How much the interpreter fallback relaxes this** (INFERRED, high
  confidence): every StaticCompiler restriction is a consequence of *having no
  runtime to fall back into*. Ruju has the full runtime in the same address
  space: allocation → `rj_` alloc entry points (GC included); dynamic dispatch
  → `rj_dispatch` (which may run the interpreter); exceptions → the shared
  linear-memory handler stack (`interp.rs:92–95`, already built AOT-shaped);
  unsupported constructs → don't compile that function at all. The problem
  Ruju's backend solves is therefore **strictly smaller than StaticCompiler's**
  per function (never needs totality) and strictly smaller than juliac's per
  program (never needs a verified closed world). The price is carrying the
  boxed/unboxed boundary in every compiled function (§6.3–6.4).

### 3.3 Julia→WASM precedents

- **WebAssemblyCompiler.jl (tshort)** — the direct precedent for the
  recommended architecture. Verified from its source: it obtains typed IR via
  `code_typed(f, tt, interp = StaticInterpreter())` (src/compiler.jl:39, a
  Mixtape-style custom AbstractInterpreter with overlays/quirks), walks
  `Core.Compiler.compute_basic_blocks` CFG (src/compiler.jl:117–123), and
  emits through **Binaryen's C API** (`Binaryen_jll` + LibBinaryen,
  src/WebAssemblyCompiler.jl:1–24), targeting **WASM-GC** (structref/arrays,
  "heap allocation is handled by WebAssembly's garbage collector"). It
  supports mutable/immutable structs, `Vector{T}`/`Vector{Any}`, strings,
  dicts, varargs, kwargs, globals, JS interop; it requires type-stable code
  ("no dynamic dispatches") and lacks exceptions, unions, multi-dim arrays,
  `Ptr`, BLAS. Status: explicitly experimental; needs bleeding-edge browsers
  (docs/src/index.md; [docs](https://tshort.github.io/WebAssemblyCompiler.jl/stable/),
  [repo](https://github.com/tshort/WebAssemblyCompiler.jl)).
  **Lessons**: (a) typed-IR-as-input works and the IR-getting machinery is
  exactly the pinned `Compiler/` surface; (b) its restriction list is a
  preview of what a no-fallback design costs — each bullet is something Ruju's
  interpreter absorbs; (c) its WASM-GC value representation is the *opposite*
  of Ruju's one-value-representation decision — Ruju compiled code must use
  the runtime's linear-memory heap, so WASM-GC is not on the table
  (INFERRED from `strategy.md:39–48`; also WASM-GC has no interior pointers /
  byte-addressable arrays, which would break `region[ptr + i*elsz]`).
- **Keno/julia-wasm (2019)** — the whole C runtime + LLVM-emitted code built
  to wasm via emscripten; ran a REPL in-browser; extremely-early-alpha,
  dormant ([repo](https://github.com/Keno/julia-wasm)). Precedent that the
  runtime-in-wasm half is viable; its pain points (tasks/stack switching,
  binary size) are the ones Ruju's strategy already defers
  (`strategy.md:203–205`). A 2023 MIT MEng thesis (Huffman, "Julia in
  WebAssembly") continued this line
  ([pdf](https://dspace.mit.edu/bitstream/handle/1721.1/150151/huffman-rhuffman-meng-eecs-2023-thesis.pdf)).
- **Charlotte.jl / WebAssembly.jl (MikeInnes, ~0.6 era)** — early direct
  Julia→wasm codegen, long unmaintained ([WebAssembly.jl](https://github.com/MikeInnes/WebAssembly.jl)).
  Historical interest only.

**Net**: nobody has shipped open-world Julia on wasm. Everyone who got
*something* running either shipped the whole JIT-less runtime with an
interpreter (Keno: actually the full LLVM-precompiled sysimage) or accepted
closed-world type-stable subsets (tshort). Ruju's split — interpreter for the
open world, AOT for the provable hot path, one heap — is genuinely the unfilled
quadrant, and each half separately has a working precedent. (INFERRED)

## 4. WASM emission options for a Rust build-time backend (WEB)

| Option | Verdict | Evidence |
| - | - | - |
| **(a) `wasm-encoder`** (bytecode-alliance/wasm-tools) | **Recommended.** Pure-Rust byte-level emitter for every wasm section incl. typed funcref tables; ~1.4M downloads/month, actively maintained, used across the BA toolchain. No IR or verification help — pair with `wasmparser`/`wasm-tools validate` in tests and `wasmprinter` for debugging. | [crates.io](https://crates.io/crates/wasm-encoder), [wasm-tools](https://github.com/bytecodealliance/wasm-tools) |
| (b) `walrus` | Good for *editing* existing modules (e.g. injecting compiled functions into the runtime .wasm for single-module output). rustwasm org sunset Sept 2025; walrus specifically moved under the wasm-bindgen org and remains a wasm-bindgen dependency — usable but not the foundation to bet on. | [walrus](https://github.com/wasm-bindgen/walrus), [rustwasm sunset](https://blog.rust-lang.org/inside-rust/2025/07/21/sunsetting-the-rustwasm-github-org/) |
| (c) **Cranelift** | **Not applicable — commonly-confused point verified**: Cranelift *consumes* wasm (frontend) and emits native code; its backends are x64, aarch64, s390x, riscv64 (+ the Pulley interpreter); **no 32-bit target, no wasm backend**. "Wasmtime's cranelift feature can be compiled *to* WebAssembly" means the compiler itself runs in wasm and still emits native code. | [Cranelift README](https://github.com/bytecodealliance/wasmtime/blob/main/cranelift/README.md), [Wasmtime tiers](https://docs.wasmtime.dev/stability-tiers.html) |
| (d) LLVM via `inkwell` | Works (LLVM has a first-class wasm backend — it is how Rust/Clang target wasm) but drags a full LLVM build/link into the build tool, needs wasm-ld for objects, and re-imports the dependency the project deliberately removed. Only worth revisiting if phase-1 codegen quality ever becomes the bottleneck — the design premise ("correct > fast codegen") says it won't. | INFERRED + LLVM target common knowledge |
| (e) WAT text + `wat2wasm` | Fine as a *debug* path; the `wat`/`wasmprinter` crates make round-tripping trivial. Not the production path (string plumbing, no structural guarantees). | [wasm-tools](https://github.com/bytecodealliance/wasm-tools) |
| (f) Binaryen | Two distinct uses: (1) **optimizer post-pass** — the `wasm-opt` crate (brson/wasm-opt-rs) builds Binaryen via cargo and exposes `OptimizationOptions::run` with all passes: adopt as an optional pipeline stage; (2) **`wasm-merge`** — merges modules, fusing imports to exports in linear time: the later single-module story. Binaryen's C API also has a built-in relooper (CFG→structured), which is a fallback if (g) proves annoying — but it means hand-building Binaryen IR through FFI. | [wasm-opt-rs](https://github.com/brson/wasm-opt-rs), [wasm-merge](https://github.com/WebAssembly/binaryen/blob/main/src/tools/wasm-merge.cpp), [web.dev on Binaryen](https://web.dev/articles/binaryen) |
| (g) Structured control flow | The one real algorithmic gap in direct emission: `IRCode` is an arbitrary CFG; wasm requires structured `block`/`loop`/`br`. Solved, published, single-pass: Ramsey, "Beyond Relooper" (ICFP 2022, functional pearl; shipped in GHC's wasm backend) — dominator tree + reverse-postorder, a few hundred lines. | [DOI 10.1145/3547621](https://dl.acm.org/doi/10.1145/3547621) |

**Recommendation**: (a) + (g) for emission, (f1) as an optional post-pass,
(f2) later for single-module packaging. Zero non-Rust build dependencies in
the required path (wasm-opt is optional), full control of the byte-level
output the ABI contracts need, and every piece independently testable with
`wasm-tools validate` + Node.

## 5. What IR should the backend consume?

**Recommendation: inferred + optimized `IRCode` (post `run_passes_ipo_safe`),
produced offline by stock Julia running the pinned `Compiler/`, serialized to a
neutral format; raw lowered `CodeInfo` stays the interpreter's diet.**

Reasons (INFERRED from verified facts above):

- Raw lowered CodeInfo is untyped; consuming it would mean writing type
  inference in Rust — a second research-grade XL on top of the backend, and an
  unfaithful one (inference *is* Julia-written upstream; the faithful move is
  to run the vendored inference, not re-implement it).
- Post-optimization IR arrives with inlining, SROA, and DCE already done by
  the pinned passes (`optimize.jl:1057–1087`) — the backend gets upstream's
  optimization quality for free and can stay a dumb translator ("correct >
  fast codegen"; wasm-opt and the browser's tiering JIT do the rest).
- `IRCode` is SSA with explicit CFG and per-statement types — the natural
  input for Ramsey-style structured-control-flow reconstruction and for
  boxed/unboxed local assignment. (`ir_to_codeinf!` exists if the flat form is
  ever preferred for serialization; `typeinfer.jl:1381`.)
- This composes exactly with the parallel pre-lowering research front: one
  build-time Julia process does lowering (JuliaSyntax/JuliaLowering) *and*
  inference/optimization; lowered-but-untyped CodeInfo streams to the runtime
  for the interpreter; (method, argtypes)-specialized typed IRCode streams to
  the backend. One producer, two consumers, one IR family — consistent with
  the "interpreter consumes the same CodeInfo shape" ledger row
  (`roadmap.md:119`).

**The honest risk (UNCERTAIN, the deepest one in this design)**: build-time
inference runs against *stock Julia's* `Base`, method tables, and 64-bit
layouts, but the code will execute against *Ruju's* runtime (32-bit offsets,
subset `base/`, its own layouts). Divergence between "what inference assumed"
and "what the runtime does" is a soundness hole, not a slowdown. Mitigations,
in order of increasing cost: (1) restrict phase-1 compilation to a whitelisted
IR vocabulary (intrinsics + builtins Ruju verifiably implements, the thin
slice's subset); (2) a custom `AbstractInterpreter` with a Ruju method-table
overlay so inference sees Ruju's world, the same mechanism GPUCompiler and
WebAssemblyCompiler already use (machinery VERIFIED-FROM-PIN in
`Compiler/src/methodtable.jl`, types.jl); (3) once `base/` is Ruju-hosted
(M5), infer against *that* code so the worlds coincide by construction.
Layout facts (field offsets, isbits sizes) must come from Ruju's own type
system at emission time, never from stock Julia's — the serialized IR should
carry *types by name/structure*, and the backend resolves offsets against the
runtime's registry. Pointer-size assumptions baked into inferred IR (e.g.
`Int === Int64` is fine — Julia semantics; `sizeof(Ptr)` is not) need a
whitelist rule.

**Note for sequencing**: the backend does *not* wait for real lowering. Its
input for the thin slice can be hand-constructed typed IR (transcribed from
stock `code_ircode` output), because the backend is a pure IR consumer either
way. The build-time-Julia *producer* and the Rust *consumer* are separately
testable increments.

## 6. Honoring the runtime contracts (the AOT ledger)

### 6.1 Value representation and locals

One value representation (`strategy.md:38–39`): compiled code sees the same
boxed u32-offset world the interpreter does, but may keep **isbits values
unboxed in wasm locals** — `i64` for Int64/Bool(zext), `f64` for Float64 —
because the standing invariant "nothing relies on heap identity for
primitives" already holds (`roadmap.md:124`). Refs are `i32` locals holding
region offsets. Boxing happens only at boundaries: dispatch-fallback calls,
`Any`-typed fields/array elements, returns of abstract type. This is exactly
Julia's own specsig-vs-jlcall split (`julia.h:460–461`).

### 6.2 gcframe / shadow stack

Contract (`roadmap.md:121–122` + litmus at `:110–113`): compiled code must be
able to root values by touching only linear memory / defined entry points.
Today's `Vec<Value>` shadow stack (`gc.rs:31–43`) fails the litmus for direct
emission. Two-step plan:

- **MVP (slice 2)**: keep the Rust `Vec` but add `rj_gc_push_frame(n) → base`,
  `rj_gc_set_slot(base, i, v)`, `rj_gc_pop_frame(base)` exports; compiled code
  calls them around allocation-crossing regions. Correct, ~1 call per
  frame push/pop — acceptable while proving the pipeline.
- **The freeze (owning increment per the ledger: now/GC)**: move root slots to
  a dedicated linear-memory area with layout
  `[stack_top: u32 global] [frames: n × u32 slots]`, i.e. Julia's gcframe
  minus the machine-stack linkage: a contiguous arena + one mutable wasm
  global exported as `rj_gc_shadow_top`. Compiled prologue:
  `top += n*4; store slots; ...; epilogue: top -= n*4` — two instructions per
  push/pop, byte-for-byte specified. `Rooted`/`Frame` in Rust become veneers
  over the same arena, so interpreter and compiled code share one root set
  (mark's `push_roots` at `gc.rs:563–584` then walks the arena instead of the
  Vec). INFERRED design; matches Julia's `jl_pgcstack` chain in spirit with
  the RAII discipline already proven in the codebase.
- Rule for emission (from `CLAUDE.md`/working notes): every ref live across
  any call that can allocate must sit in a gcframe slot, not only an i32
  local — the auto-collect stress test is the enforcement instrument.

### 6.3 Calling convention (two entry points, after `CodeInstance`)

Faithful adoption of the pin's design (`julia.h:219–221,460–461,523–535`):

- **Boxed entry ("fptr1")**: `(func (param i32 argv_offset) (param i32 nargs) (result i32))`
  — argv is a rooted slice of u32 value offsets in linear memory (e.g. a
  gcframe range); result is a boxed offset. This is what `rj_dispatch` calls,
  what the interpreter's `CallGeneric` reaches, and what any caller with only
  abstract knowledge uses. Errors: the reified-exception channel — with the
  linear-memory handler stack (ledger row `roadmap.md:118`) the convention is
  "returns 0 / sets current-exception" or a dedicated trap-flag global
  (UNCERTAIN — small design decision to make at slice 2; must be one
  mechanism shared with the interpreter's `Err(Value)` channel).
- **Specsig entry**: native wasm signature from the inferred argtypes/rettype,
  e.g. `f(::Int64)::Int64` ⇒ `(func (param i64) (result i64))`. Used for
  compiled→compiled calls when the callee was devirtualized. The backend also
  emits a tiny boxed→specsig **wrapper** (unbox args, call specsig, box
  result) to serve as the fptr1 entry — again exactly upstream's scheme.

### 6.4 Devirtualization and the interpreter-fallback boundary

- At build time, for each `:invoke`/call site with concrete argtypes, the
  producer resolves `(f, argtypes) → method` at a **fixed world** (the ledger
  obligation on dispatch hardening, `roadmap.md:120`) — note inlining has
  already consumed many such sites inside `run_passes_ipo_safe`. Resolved +
  compiled ⇒ direct `call` to the callee's specsig function. Resolved but
  *not* compiled ⇒ boxed call through the method's registered fptr1 (which may
  be the interpreter trampoline). Unresolved (abstract argtypes) ⇒
  `rj_dispatch(func_id, argv, nargs)` — the open-world hatch that makes the
  whole design non-closed-world.
- **Compiled code invalidation**: Phase 1 compiles against one world snapshot;
  method redefinition after AOT means the dispatch service must prefer newer
  interpreted methods over stale compiled fptrs. Since registration goes
  through the same table, the runtime can simply drop/shadow compiled entries
  whose world is superseded — degrade to interpreter, never wrong. (INFERRED;
  cheap because dispatch is one service, `strategy.md:38–39`.)

### 6.5 Method registration & funcref tables (both callers reach one method)

Sketch (INFERRED; all wasm mechanisms are core-spec):

- The runtime module is built with `--export-table` so its
  `__indirect_function_table` (funcref) is shared. The compiled module
  *imports* memory and that table; its functions are placed into table slots
  via active element segments at instantiation (or `table.grow` + JS
  `Table.set` by the loader — simplest for the MVP).
- Registration: the loader (or a start function) calls
  `rj_register_compiled(func_id, sig_offset, fptr1_idx, specsig_idx)`;
  `dispatch::Entry` grows optional `fptr1: u32` / `specsig: u32` table
  indices alongside `body`. `interp.rs` `CallGeneric` → `dispatch::invoke`
  checks `fptr1` first: if present, Rust calls it as
  `core::mem::transmute::<usize, extern "C" fn(u32, u32) -> u32>(idx)` —
  on wasm32-unknown-unknown a function pointer *is* a table index, so this is
  a plain `call_indirect` from Rust (INFERRED, standard rustc-wasm behavior;
  verify in slice 1 disassembly). Compiled callers reach interpreted methods
  the mirror way: their boxed call lands on a runtime-exported trampoline
  `rj_interp_invoke(method_id, argv, nargs)`.
- Thus one method table serves both fronts: signature + (interpreted body |
  compiled fptrs), selection logic unchanged (`dispatch.rs:59–79`).

### 6.6 Single module or two?

- **MVP: two modules.** Runtime exports `memory` + table + `rj_*`; the
  backend's output imports them. The harness instantiates runtime first, then
  the compiled module with `{env: {memory, __indirect_function_table, rj_dispatch, rj_gc_*}}`.
  No custom linking anywhere; cross-module calls are ordinary near-calls in
  modern engines. One caveat to verify in slice 1: the Rust module currently
  *defines* its memory — exporting it is default; the compiled module must
  declare an *imported* memory with matching limits (mechanical).
- **Later: one module**, by running Binaryen `wasm-merge` (fuses imports to
  exports, linear time) and then `wasm-opt`, enabling cross-module inlining
  and one-file shipping ([wasm-merge](https://github.com/WebAssembly/binaryen/blob/main/src/tools/wasm-merge.cpp)).
  Alternative single-module route — walrus-editing functions into the runtime
  binary — is strictly more fragile; keep as fallback.

## 7. The thin-slice experiment (go/no-go)

**Goal**: prove the full chain — typed IR (data) → Rust backend → wasm
function → registered → called by harness *and* by dispatch → faster —
with the smallest honest slice, before any of the XL work is committed.

**Function**: `function f(n); acc=0; i=1; while i<=n; acc+=i*i; i+=1; end; acc; end`
— isbits-only, no allocation, no calls after inlining (`+`,`*`,`<=` on Int64
inline to intrinsics), one loop, one phi-pair. Its stock
`code_ircode(f, (Int64,))` is ~10 statements: two `PhiNode`s (acc, i),
`slt_int`/`mul_int`/`add_int` intrinsic calls, `GotoIfNot`, `GotoNode`,
`ReturnNode` — all intrinsics Ruju already has in the pure `intrinsics` crate
(`interp.rs:25–29`).

**Steps** (each independently verifiable):

1. *IR fixture.* Run `Base.code_ircode(f, (Int64,))` on a stock Julia 1.12
   with the **pinned** `reference/julia/Compiler` loaded as a package (this
   simultaneously probes §2's operational UNCERTAIN). Transcribe the IRCode
   into a small serde-able Rust struct set (`AotIr { blocks, stmts, types }`)
   as a JSON/`ron` fixture checked into the experiment. (Hand-construction is
   the fallback if the stock-Julia step stalls; the backend cannot tell.)
2. *Backend.* A new crate (`ruju-aotc`, host-side, never wasm): consumes the
   fixture, assigns wasm locals from statement types (i64 here), reconstructs
   structured control flow (for this CFG a hand-rolled loop/if suffices;
   implement the Ramsey algorithm skeleton anyway — it is the piece that must
   not be faked twice), emits via `wasm-encoder`: one specsig func
   `(param i64)(result i64)`, one boxed wrapper `(param i32 i32)(result i32)`
   calling `rj_box_int`/`rj_unbox_int` imports, exports + element segment.
   Validate with `wasm-tools validate`; snapshot the WAT via `wasmprinter`.
3. *Runtime hooks* (small, but they are real repo changes — flag to the
   human): export the table (`-C link-args=--export-table`), add
   `rj_unbox_int`/`rj_box_int` exports if not present, add
   `rj_register_compiled` + the `fptr1` field in `dispatch::Entry`, and make
   `dispatch::invoke` prefer it.
4. *Harness.* Extend `runtime/harness.mjs`: instantiate runtime, instantiate
   `f.wasm` with `{env:{memory, table, rj_*}}`, register, then check
   (a) direct specsig export: `f(10) === 385n`, `f(0) === 0n`,
   `f(10^6)` equals the interpreter's answer; (b) the dispatch path: evaluate
   source `f(10)` through `rj_eval` so `CallGeneric` → compiled fptr1 fires
   (proving both callers reach the method); (c) GC invariants hold
   (`rj_root_count` unchanged, auto-collect stress on around the calls).
5. *Benchmark.* n = 10^7, ≥5 repetitions, median, via `harness.mjs` timers:
   (i) interpreter (`rj_eval` of the while-loop source), (ii) compiled specsig
   export, (iii) reference point: the same loop hand-written in Rust inside
   the runtime and exported (`rj_bench_native`) — the "what wasm can do" line.

**Measurements & thresholds** (go/no-go):

| Measurement | Threshold | Rationale |
| - | - | - |
| Correctness | exact equality with interpreter on {0, 1, 10, 10^6}, incl. wrap-around case at large n | non-negotiable |
| Compiled vs native-Rust-in-wasm loop | within **3×** | proves the emitted code is real machine-shaped wasm, not accidentally boxed; expect ~1× after wasm-opt (INFERRED) |
| Compiled vs interpreter | ≥ **100×** | the interpreter boxes per op (`interp.rs:184–206`); if AOT can't clear 100× here it never pays for its complexity (INFERRED: expect 500×+) |
| Dispatch-path call overhead | fptr1 path ≤ interpreter's per-call overhead | the registration design is only right if reaching compiled code costs no more than reaching interpreted code |
| Determinism | two runs, identical results; `cargo test` still green twice | project norm (`CLAUDE.md`) |

**Explicit non-goals of the slice** (deferred to slice 2/3): gcframe emission
(no allocation here — slice 2 compiles an allocating function, e.g.
`g(n) = (a = Ref-like struct; loop mutating it)`, forcing the shadow-stack
freeze of §6.2 and the exception-channel decision of §6.3); compiled→dispatch
fallback calls (slice 3: `f` calling a generic `h` with abstract argtype);
wasm-merge single-module packaging; the serialization producer as a durable
tool.

**Timing within the roadmap**: the roadmap gates the AOT backend on dispatch
hardening + GC exactness (`roadmap.md:48`, map edge `DISPX --> AOT`,
`strategy.md:138–139`) — that gate holds for the *increment*, not for this
*experiment*: the slice touches dispatch only additively (one optional field)
and GC not at all, and it retires the plan's single biggest unknown
(`roadmap.md:91–96`) years-of-waves early. Recommend running it as a
frontier-adjacent research spike with the human's sign-off, results recorded
in `design/` either way. (INFERRED/judgment.)

## 8. Risk register

| Risk | Severity | Mitigation |
| - | - | - |
| **World mismatch**: stock-Julia inference vs Ruju runtime semantics (§5) | High (soundness) | vocabulary whitelist → AbstractInterpreter overlay → self-hosted `base/` at M5; layouts always resolved runtime-side |
| Shadow-stack ABI not yet frozen (`gc.rs:31–43` vs `roadmap.md:122`) | Medium | ledger row already exists; slice 2 forces it; freeze is small *now* |
| Exception channel across compiled frames undecided | Medium | handler stack already linear-memory-shaped (`interp.rs:92–95`); decide return-flag vs global at slice 2 |
| Rust-fn-pointer ⇔ table-index assumption (§6.5) | Low | verify in slice 1 disassembly; fallback: a runtime-side `call_indirect` shim written once |
| `wasm-encoder`-emitted module + imported memory limits mismatch | Low | mechanical; caught by instantiation in the harness |
| Binaryen/wasm-opt as C++ build dep | Low | optional pipeline stage only; required path is pure Rust |
| Invalidation (redefinition after AOT) | Medium, later | world-tagged registration; shadow stale fptrs, degrade to interpreter (§6.4) |
| IRCode serialization format churn (tracking the pin) | Medium, chronic | version the fixture format; the producer is pinned to `reference/` by construction |

## 9. Verdict

**Feasible, with the architecture essentially determined**: offline
inference/optimization by the pinned Julia-written compiler on stock Julia,
a deliberately dumb pure-Rust IRCode→wasm translator on `wasm-encoder` +
Beyond-Relooper, wasm-opt as an optional polish pass, two linked modules
merging to one later, and the interpreter fallback converting every
closed-world impossibility that killed or constrained juliac, StaticCompiler,
and WebAssemblyCompiler into a mere missed optimization. The single deepest
risk is semantic (build-time world vs runtime world), not mechanical, and it
is bounded by starting from a whitelisted IR vocabulary and shrinks
structurally as Ruju approaches self-hosting. The thin slice is small
(~1–2 focused increments), touches the runtime only additively, and directly
de-risks the plan's biggest XL. Recommend running it.
