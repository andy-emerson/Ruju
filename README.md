# Ruju

Ruju is a Rust (ru) reimplementation of Julia (ju), targeting WebAssembly as a
first-class platform. Ruju is not a new language: the goal is Julia itself —
the same syntax and semantics, verified against Julia's own source and test
suite — with the runtime rebuilt in Rust so it runs natively on the web.

Ruju is early-stage. Today it is a working runtime that executes a small
subset of Julia source; the layers that make it *all* of Julia are the next
phase (see [Status](#status)).

## What runs today

This Julia program is parsed, lowered, and interpreted entirely inside the
WebAssembly module — the JavaScript host just passes the source in through the
`rj_eval` export and gets `5050` back:

```julia
acc = 0
i = 1
while i <= 100
    acc = acc + i
    i = i + 1
end
acc
```

The bootstrap front-end covers integer and float literals, variables,
assignment, arithmetic and bitwise operators (incl. `===`),
`if`/`elseif`/`else`/`while`, `struct` definitions and field access, array
literals with 1-based indexing and `push!`/`length`, `try`/`catch`/`finally`
with real exception values, and top-level globals that persist across
evaluations. Underneath it sit the real subsystems: a tagged object model, a
generational GC, the type system with Julia's subtype algorithm, and multiple
dispatch.

## Motivation

Julia in the browser, without the hacks. Julia's runtime is roughly 6 MB of
C/C++ that was never meant to leave the operating system: dragging it into the
browser today means Emscripten patches, POSIX shims, and OS-layer workarounds.
Rewriting that runtime in Rust produces a codebase that compiles cleanly to
WebAssembly through the standard toolchain — `wasm32-unknown-unknown`, no
special toolchain at all — and Rust's ownership model maps cleanly onto
sharing data across WASM linear-memory boundaries.

The rest of the language is written in Julia itself, and that part doesn't
need rewriting — it needs a runtime that can carry it to the web.

## Approach

- Reimplement Julia's C/C++ runtime — garbage collector, type system, method
  dispatch, interpreter, intrinsics — in Rust, ported subsystem by subsystem
  from the C reference and verified against Julia's own test suite.
- AOT-compile the Julia-written layers (`base`, `stdlib`, the compiler) to WASM
  at build time — no JIT in the browser.
- Keep the language compatible with Julia.
- Produce a `.wasm` module whose memory layout is composable — it does not assume
  sole ownership of WASM linear memory (exports are `rj_`-prefixed, references
  are offset-based).

## Status

A working runtime exists and runs via WebAssembly:

- a tagged object model and a generational, pooled mark-sweep GC with
  shadow-stack rooting;
- the type system — DataTypes, tuples (incl. varargs), unions, parametrics,
  `Type{T}` kinds, and the `where` machinery (`UnionAll`/`TypeVar`) — with a
  subtype algorithm checked against 106 assertions from JuliaLang/julia's
  `test/subtype.jl`;
- multiple dispatch and a tree-walking interpreter over lowered IR, with
  exception handling (`try`/`catch`/`finally`, reified exception objects);
- `GenericMemory`/`Array` buffers living in linear memory, with growth;
- a `Main` module with global bindings that persist across evaluations;
- a hand-written bootstrap front-end that runs a subset of real Julia source.

The AOT compiler that will turn the Julia-written layers into WASM is the next
major phase and is not yet built. See `design/strategy.md` for the plan and
`design/implementation.md` for per-subsystem status and fidelity.

## Repository layout

Ruju's own code sits at the top level; the Julia source it ports from and
tests against is vendored under `reference/`.

| Path | What it is |
| - | - |
| `runtime/` | the Rust runtime (object model, GC, types, subtyping, dispatch, interpreter, the `rj_` WASM ABI) |
| `intrinsics/` | pure arithmetic/comparison intrinsics |
| `design/` | `strategy.md` (where we are going), `implementation.md` (where we are), `methodology.md` (how we get there) — see `design/README.md` |
| `reference/julia/` | a pinned, verbatim subset of JuliaLang/julia used as the porting reference and oracle: `src/` (the C runtime we reimplement) plus `base/`, `stdlib/`, `Compiler/`, `JuliaSyntax/`, `JuliaLowering/`, and `test/` |

`reference/README.md` records the pinned upstream commit and version.

## Building and testing

Ruju is a Cargo workspace. The unit tests run natively; building for
`wasm32-unknown-unknown` produces the `.wasm` module that the two Node scripts
load and exercise through a JavaScript host.

```sh
cargo test  -p ruju-runtime                                    # native unit tests
cargo build -p ruju-runtime --target wasm32-unknown-unknown --release
node runtime/harness.mjs               # wasm -> JS end-to-end checks
node runtime/verify_julia_subtype.mjs  # subtype answers vs JuliaLang/julia
node runtime/repl.mjs                  # interactive: type Julia at the runtime
```

## Contributing and license

See [CONTRIBUTING.md](CONTRIBUTING.md). Ruju is MIT-licensed
([LICENSE.md](LICENSE.md)), as is the Julia source it ports from.
