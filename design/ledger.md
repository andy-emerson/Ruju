# Ruju subsystem ledger

A complete map of Julia's C/C++ runtime (`reference/julia/src/`) — every
subsystem subdivided into its atomic pieces. Two independent axes per piece:

- **Status** — how much exists: **Planned** (nothing yet) · **Partial** (present
  but incomplete) · **Done** (complete for what it covers).
- **Fidelity** — its relationship to Julia: **Faithful** (Julia's *design*, even
  if simplified or incomplete — same shape, possibly less of it) · **Divergence**
  (a *deliberately different* design, for the WASM target or the composable-memory
  model).

A *simplification* is **Faithful + Partial**, not a divergence. "Done · Faithful"
means *verified against the C reference*, not "tests pass" — audits found the
difference matters. When unsure, read the file named in the section heading.

**Global conventions (project-wide divergences, stated once).** Every piece below
assumes these and does not repeat them; a row is **Divergence** only for a
departure *beyond* them.

- References are region-relative **offsets**, not native pointers (bounded,
  composable memory).
- A single bounded heap **region**; exported symbols are `rj_`-prefixed.
- **Single-threaded** for now (the `Sync` on global state relies on it).

---

## Object model & values — `julia.h`, `datatype.c`, `simplevector.c`

| Piece | Status | Fidelity | Notes (Julia → ours) |
| - | - | - | - |
| Tagged header (tag-before-object, GC bits) | Done | Faithful | `jl_taggedvalue_t` |
| `DataType` struct | Partial | Faithful | ~6 of `jl_datatype_t`'s ~17 fields |
| Field layout | Partial | Faithful | pointer bitmap only; no field-type/offset table |
| Boxing | Partial | Faithful | `Int64`/`Bool`/`Float64`; no small-int cache; `Bool` boxes are freshly allocated, not the `jl_true`/`jl_false` singletons (identity gap, observable once `===` exists); other primitives later |
| `SimpleVector` | Done | Faithful | `jl_svec_t` |
| Singletons | Partial | Faithful | tracked outside the type; no `instance` field |

## Type system — `jltypes.c`, `datatype.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| `jl_init_types` bootstrap | Done | Faithful | incl. the `DataType : DataType` cycle |
| Hierarchy & primitive sizes | Done | Faithful | verified vs `boot.jl` |
| `TypeName` | Partial | Faithful | name + cache; missing module/wrapper/names/hash |
| `apply_type` instantiation | Partial | Faithful | tuples + parametrics; no `UnionAll` instantiation |
| Uniquing (hash-consing) | Partial | Faithful | on `TypeName`; linear scan vs sorted/hashed |
| `Union` | Partial | Faithful | normalized (`jl_type_union`): flatten, subtype-dedup, canonical sort; sort misses `union_sort_cmp`'s singleton/isbits tiers; dedup uses full `issubtype` vs the C's typevar-aware `simple_subtype`; type `===` needs structural `jl_types_egal` (Julia does **not** intern unions); no `Vararg` merge |
| `Bottom` | Partial | Faithful | a `DataType`; Julia uses a `TypeofBottom` instance |
| `UnionAll` / `TypeVar` | Partial | Faithful | `jl_unionall_t`/`jl_tvar_t` objects (var + bounds + body); no `where`-var renaming/aliasing or `innervars` |
| `Type{T}` kinds | Planned | Faithful | — |
| Abstract `Tuple` (`jl_anytuple_type`) | Planned | Faithful | tuple super is `Any` for now |

## Subtyping — `subtype.c`

Oracle-verified: `runtime/verify_julia_subtype.mjs` runs a curated set of
assertions copied verbatim from JuliaLang/julia's own `test/subtype.jl` (mapping
`Ref{T}`→`Box{T}`, `Int`→`Int64`) — currently 53/53, plus 1 tracked known
divergence (tuple-over-union distributivity, which needs Julia's global
union-decision machine; it self-reports if a fix makes it pass).

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| `jl_subtype` structural core | Partial | Faithful | reflexive/`Any`/`Bottom`, Union forall–exists, covariant tuples, nominal, invariant parametrics, `UnionAll`/`TypeVar` via the env. Audit 2026-06 fixes landed: free-vs-free typevars now unconditionally false; `forall_exists_equal` reverse check at `PARAM_NONE` + same-name-datatype fast path. Remaining divergences: unions split before typevar/UnionAll handling (Julia prioritizes the latter); no two-union greedy path; local union backtracking vs the global decision machine (see oracle's known divergence) |
| Existential env (`jl_stenv_t`) | Partial | Faithful | `var_lt`/`var_gt` narrow per-var `lb`/`ub`; ∀/∃ via the `existential` flag; `invdepth`/`depth0` order interacting existentials (`var_outside`, ∀∃-vs-∃∀). No `where`-var renaming or `innervars` leak handling |
| Diagonal rule | Partial | Faithful | `occurs_cov` + `cov_diag` consistency-scope folding (`subtype_ccheck`), static `var_occurs_invariant`, `is_leaf_bound`; `ccheck` enters at `PARAM_NONE` (fixed, audit 2026-06); typevar lower bounds accepted (fixed — Julia also propagates `concrete=1` to that var, which we still don't); pinned C has newer machinery the port predates (`Intersect` #61917, `push_forall_bound_scope`, `Loffset`) |
| Union backtracking | Partial | Faithful | env save/restore on the exists branch; not Julia's `Lunions`/`Runions` bit-stack iterator |
| `simple_meet` / `simple_join` | Partial | Faithful | join defers to the normalized `union_type` (keeps free vars, so `S>:T` survives); meet over-estimates to `b` for typevar operands (no `Intersect` node) |
| `jl_type_intersection` | Planned | Faithful | — |
| `jl_type_morespecific` | Partial | Faithful | subtype-based approximation |
| Varargs | Planned | Faithful | `Vararg{T,N}` in tuple types |
| Fast paths (`obviously_egal`) | Planned | Faithful | — |

## Method dispatch — `gf.c`, `typemap.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Method table | Partial | Faithful | Rust-side table; Julia's is heap `jl_methtable_t` |
| Applicability | Partial | Faithful | `argtuple <: sig` — matches Julia's concrete-tuple dispatch (intersection serves abstract match queries/ambiguity); missing: typemap cache, world age |
| Specificity | Partial | Faithful | subtype-based |
| Method cache (`typemap`) | Planned | Faithful | linear scan per call now |
| World age | Planned | Faithful | — |
| Ambiguity / `MethodError` | Planned | Faithful | first match; `NULL` on miss |
| `@generated`, kwargs, vararg methods | Planned | Faithful | — |

## Interpreter — `interpreter.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| `eval_body` loop | Done | Faithful | instruction-pointer loop |
| Statements (`Goto`/`GotoIfNot`/`Return`/`:call`/`:(=)`) | Partial | Faithful | `GotoIfNot` skips the `Bool` `TypeError` |
| Operands (SSA / slot / const) | Done | Faithful | — |
| Phi / phic / upsilon | Planned | Faithful | SSA-form nodes |
| Exception handling (`enter`/`leave`) | Planned | Faithful | — |
| `:new` / `getfield` / `setfield!` / globals / closures | Planned | Faithful | — |
| IR source | Partial | **Divergence** | hand-built Rust IR via a Rust front-end; faithful path is heap `CodeInfo` from `JuliaLowering` |

## Builtins — `builtins.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| `typeof`, `<:` | Partial | Faithful | via the ABI |
| `isa`, `===` | Planned | Faithful | — |
| `getfield`/`setfield!`/`nfields`/`fieldtype` | Planned | Faithful | needs struct support |
| `tuple`, `apply`, `invoke`, array builtins | Planned | Faithful | `invoke`-like dispatch exists |

## Intrinsics — `runtime_intrinsics.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Integer arithmetic | Partial | Faithful | `add/sub/mul` + `slt/sle/eq` of ~50+ |
| Bitwise / shifts | Planned | Faithful | — |
| Float arithmetic & compare | Partial | Faithful | `add/sub/mul` + `lt/le/eq` |
| Conversions (`sitofp`, `trunc`, …) | Planned | Faithful | — |
| Pointer / memory intrinsics | Planned | Faithful | — |
| Operator → intrinsic dispatch | Partial | Faithful | type-switched in `apply`; faithful is generic-function operators (`+` etc.) over the typed intrinsics |

## Garbage collector — `gc-stock.c`, `gc-common.c`, `gc-pages.c`, `gc-stacks.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Pool allocation (size classes, pages, free lists) | Partial | Faithful | `jl_gc_pool_t` design (freelist threads the header word = `jl_taggedvalue_t.next`); but 12-entry geometric size table vs Julia's ~50-entry table, 4 KiB pages vs default 16 KiB, one global freelist per class vs per-page freelists + `newpages` + `pagemeta` (audit 2026-06) |
| Big-object path | Planned | Faithful | large objects use a pool for now |
| Precise marking | Done | Faithful | type-layout driven |
| Sweeping (page walk, free-list rebuild) | Partial | Faithful | eager every-page walk; Julia's lazy/quick-sweep page machinery (`pagemeta` has_marked/has_young) absent (audit 2026-06) |
| Non-moving collection | Done | Faithful | the stock GC is non-moving too |
| Generational state encodings | Done | Faithful | `GC_CLEAN/MARKED/OLD/OLD_MARKED`, verified |
| Promotion policy | Partial | Faithful | one-survival placeholder; Julia uses `PROMOTE_AGE` + per-object age |
| Write barrier + remembered set | Partial | Faithful | `jl_gc_wb`'s condition differs in both halves: Julia fires on parent `== GC_OLD_MARKED` with child unMARKED and re-tags via `jl_gc_queue_root`; ours fires on any old parent with child not-old, never re-tags (duplicates possible). Conservative-correct (audit 2026-06) |
| Collection trigger | Partial | Faithful | exhaustion-only placeholder; Julia is proactive at a heap-target |
| Full-vs-quick policy | Partial | Faithful | escalation placeholder; Julia uses a growth heuristic |
| Shadow-stack rooting (`gcframe`) | Done | Faithful | — |
| Machine-stack scanning | n/a | **Divergence** | impossible in WASM; the shadow stack is *mandatory* instead |
| Safepoints | Partial | Faithful | trivial (single-threaded); multithreaded protocol later |
| Finalizers | Planned | Faithful | — |
| Weak references | Planned | Faithful | — |
| Heap snapshot / alloc profiler | Planned | Faithful | tooling |

## Tasks & concurrency — `task.c`, `threading.c`, `scheduler.c`, `safepoint.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Tasks (coroutines) | Planned | **Divergence** | WASM has no native stack switch (needs asyncify / the stack-switching proposal) |
| Threading | Planned | **Divergence** | WASM threads = SharedArrayBuffer + workers, a different model |
| Scheduler | Planned | Faithful | — |
| Locks / atomics | Planned | Faithful | WASM atomics exist |

## Symbols — `symbol.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Interned, immortal symbol table | Partial | Faithful | immortal ✓; a Rust `Vec` (linear) vs Julia's hash-keyed invasive binary tree embedded in `jl_sym_t` (`left`/`right`/`hash`) — our symbol body is `len + bytes`, no embedded tree links |

## Modules & top level — `module.c`, `toplevel.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Modules & bindings | Planned | Faithful | — |
| Global variables | Planned | Faithful | — |
| Top-level eval | Partial | Faithful | the front-end evaluates expressions; no module/global system |
| Imports / exports | Planned | Faithful | — |

## Runtime utilities — `rtutils.c`, `iddict.c`, `idset.c`, `smallintset.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Error / exception throwing | Planned | Faithful | — |
| Display / printing (`jl_show`) | Planned | Faithful | — |
| Internal hash collections | n/a | **Divergence** | Rust `std` collections used internally instead of Julia's C IdDict/IdSet |

## IR & methods — `ircode.c`, `method.c`, `opaque_closure.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| `CodeInfo` (de)serialization | Planned | Faithful | — |
| Method definitions (`jl_method_t`) | Partial | Faithful | Rust-side method bodies |
| OpaqueClosure | Planned | Faithful | — |

## Arrays & memory — `array.c`, `genericmemory.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| `GenericMemory` | Planned | Faithful | — |
| `Array` | Planned | Faithful | — |
| `arrayref`/`arrayset`/`length`/`push!` | Planned | Faithful | — |

## Serialization & system image — `staticdata.c`, `precompile.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| System image | Planned | **Divergence** | Ruju's "image" is the AOT-compiled artifact — a different model |
| Precompilation / package images | Planned | Faithful | — |

## Init & C API — `init.c`, `jlapi.c`, `jloptions.c`, `engine.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Runtime init | Partial | Faithful | `rj_init` (region + types + dispatch) |
| Embedding ABI | Partial | **Divergence** | the `rj_` C ABI is a WASM/JS-host interface, not the `jl_` C API |
| Options parsing | Planned | Faithful | — |

## Front-end (parsing & lowering) — `ast.c`, `JuliaSyntax`, `JuliaLowering`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Parsing | Partial | **Divergence** | Rust bootstrap lexer/parser; faithful path is `JuliaSyntax` (AOT) |
| Lowering to `CodeInfo` | Partial | **Divergence** | Rust lowering to our own IR; faithful path is `JuliaLowering` |

## FFI — `runtime_ccall.c`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| `ccall` | Planned | **Divergence** | in WASM, "C" calls are host/JS imports — a different model |

## AOT backend (Phase 1) — replaces removed `codegen.cpp`/`jitlayers.cpp`

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| IR → code | Planned | **Divergence** | Julia JITs via LLVM; Ruju plans a build-time IR → WASM compiler |

## Platform, profiling & support — `dlload.c`, `sys.c`, `timing.c`, `src/support/` (under `reference/julia/`)

| Piece | Status | Fidelity | Notes |
| - | - | - | - |
| Dynamic loading (`dlopen`) | n/a | **Divergence** | single WASM module; no dynamic loading |
| System info | Planned | Faithful | — |
| Support lib (hashing/ios/utf8/strtod) | n/a | **Divergence** | Rust `std`/crates provide equivalents |
| Timing / profiling | Planned | **Divergence** | native/signal-based profiling absent in WASM |
