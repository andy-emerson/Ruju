# Ruju — state and roadmap

The forward plan, written from the current state. Start here if you are new to
the project; its companion is the subsystem ledger (`ledger.md`). The two design
decisions that shape the whole runtime are recorded below.

## What this is

Ruju reimplements Julia's C/C++ runtime (`reference/julia/src/`) in Rust,
targeting WebAssembly (see the top-level `README.md`). The C runtime is retained
as the **reference** we port from, file by file. The Julia-written layers
(`reference/julia/base/`, `stdlib/`, `Compiler/`, `JuliaSyntax/`,
`JuliaLowering/`) stay, to be AOT-compiled to WASM eventually.

The Rust runtime is a Cargo workspace at the repo root:

| Crate / file | Ports | Role |
| - | - | - |
| `runtime/src/object.rs` | `julia.h` taggedvalue | tagged heap objects, the GC header |
| `runtime/src/types.rs` | `jltypes.c` / `julia.h`, `subtype.c` | DataTypes, `TypeName`, tuples/unions/parametrics, uniquing, subtyping |
| `runtime/src/symbol.rs` | `symbol.c` | interned (immortal) symbols |
| `runtime/src/gc.rs` | `gc-stock.c` | generational, pooled mark-sweep; shadow-stack rooting |
| `runtime/src/interp.rs` | `interpreter.c` | lowered-IR interpreter (`eval_body`) |
| `runtime/src/dispatch.rs` | `gf.c` | multiple dispatch |
| `runtime/src/frontend.rs` | (replaces flisp; **bootstrap**, not JuliaSyntax) | Rust lexer/parser/lowering for a Julia subset |
| `runtime/src/value.rs`, `region.rs`, `lib.rs` | — | boxing, the bounded region, the `rj_` C ABI |
| `intrinsics/src/lib.rs` | `runtime_intrinsics.c` | pure arithmetic/comparison intrinsics |

## Key design decisions

Two decisions shape the whole runtime. Both were made up front and have held up.

**Cold path — interpreter fallback, AOT for the hot path.** Removing the
in-browser JIT raises the question of what runs when code needs a type/method
combination that was not compiled ahead of time. We ship an **interpreter** that
executes lowered IR against any concrete types (open-world correctness, merely
slow) and will **AOT-compile the hot path** later. *Rejected:* runtime WASM
codegen (reintroduces a heavyweight backend and an async/sync mismatch) and a
closed-world `juliac`-style subset (abandons dynamic Julia). *Sequencing:*
**Phase 0 — interpreter only** (validates values, dispatch, allocator, and GC
before any AOT backend exists; this is where we are); **Phase 1 — AOT hot path**,
with the interpreter as the fallback. Both share one value representation and one
dispatch service.

**GC rooting — a mandatory shadow stack with RAII.** WASM exposes no way to scan
the machine stack or locals, so conservative stack scanning is impossible and
every root must be explicit. We port Julia's `gcframe` shadow stack but make it
**mandatory** (no scan fallback), expressed through RAII (`Rooted` / `Frame`) so
it is hard to get wrong. *Rejected:* conservative stack scanning (impossible in
WASM) and a handle table (more indirection than needed). Because roots live in
addressable slots, the door stays open to a moving collector later — though we
are non-moving now, like Julia's stock GC.

## How to build, test, and run

```sh
# native unit tests (logic)
cargo test --workspace
# the real target
cargo build -p ruju-runtime --target wasm32-unknown-unknown --release
# load and exercise the .wasm from a JS host
node runtime/harness.mjs
```

The wasm target must be installed once: `rustup target add wasm32-unknown-unknown`.
There is no CI yet — run the three commands above before every commit.

## Where we are

A working end-to-end vertical slice: **real Julia source text runs**
(`node runtime/harness.mjs` evaluates `sum(1:100)` from a source string), via
front-end → interpreter → dispatch → a generational pooled GC. Every subsystem
is a faithful-in-shape **subset**; see the ledger for per-subsystem gaps. The
two largest deliberately-simplified areas are the **GC policies** (promotion
age, collection trigger, full-vs-quick are placeholders, not Julia's heuristics)
and the **front-end** (a Rust bootstrap, not JuliaSyntax/JuliaLowering).

## Roadmap

Ordered by dependency. Each is its own multi-commit effort; sub-increment the
hard ones (as subtyping and the GC were).

1. **Existential types** — `TypeVar` + `UnionAll` (the `where` machinery), then
   the real `subtype.c` environment (variance, bounds), then `Type{T}` kinds,
   varargs, and the diagonal rule. The research-grade core; unblocks full
   `type_morespecific`. *Sub-increment: `TypeVar` + simple `UnionAll` subtyping
   first.*
2. **Struct support** — `DataType.types` (field types), and `new`/`getfield`/
   `setfield!` in the interpreter. Lets composite values exist; needed before
   real `base/` code.
3. **Values + intrinsics breadth** — `Float64` and the other primitive boxings,
   plus the broader `Core.Intrinsics` set (div/rem, bitwise, float, conversions).
   Lets numeric Julia run; mostly mechanical.
4. **Dispatch hardening** — `type_morespecific`, ambiguity/`MethodError`, a
   method cache (typemap). Needs (1).
5. **GC tuning** — real promotion age, proactive heap-target trigger,
   full-vs-quick heuristic. Pays off the three GC-policy placeholders (🔸); wants
   a real workload to tune against (we now have one).
6. **Real lowering** — wire `JuliaSyntax` → `JuliaLowering` once the runtime can
   host them (needs much of 1–3), replacing `frontend.rs`.
7. **Phase-1 AOT backend** — the IR→WASM compiler (the cold-path decision's
   Phase 1, above). Then: threading, finalizers, big-object GC, a moving
   collector.

**Recommended next:** either start (1) `TypeVar`/`UnionAll` (the deep correctness
path, hardest), or do the breadth first — (2) structs and (3) `Float64`+
intrinsics make real programs run and are bounded, while (1) can follow. Both are
defensible; (2)+(3) give more visible progress sooner, (1) is the deeper
investment. Dispatch (4) needs (1) regardless.
