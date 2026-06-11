# Instructions for Claude

Ruju is a Rust (ru) reimplementation of Julia (ju), targeting **WebAssembly**
as a first-class platform. Ruju is not a new language: the goal is Julia
itself, with the C/C++ runtime (the `src/` tree) rebuilt in Rust. Julia's
source is vendored under `reference/` as the porting reference and oracle.

Ruju is built in human–agent collaboration. You — the agent — write the code;
the human holds design authority, decides, and is responsible for what merges.
Propose designs and surface trade-offs freely, then build what the human
decides.

The conventions in `CONTRIBUTING.md` bind every contributor, including you,
and `design/methodology.md` governs the engineering process — the claim
ladder and the increment loop. The two rules that matter most day to day:
the **faithfulness bar** (port the *design* of the C reference; record
divergences in `design/implementation.md`; Done · Faithful means
*reference-verified*, not "tests pass") and **small, single-subsystem
increments**.

Start by reading `README.md`, then `design/strategy.md` (the dependency
frontier — what can be worked on now) and `design/implementation.md` (the
per-module status and fidelity evidence). Those two documents are the source
of truth for *what to do next* and *how faithful each piece is* — keep them
current as you work.

## Repository layout

| Path | Owner | What it is |
| - | - | - |
| `runtime/`, `intrinsics/` | **ours** | the Rust runtime + a pure-intrinsics crate, plus `runtime/harness.mjs` (JS host) and `runtime/verify_julia_subtype.mjs` (the oracle) |
| `design/` | **ours** | `strategy.md`, `implementation.md`, `methodology.md` (see `design/README.md`) |
| `reference/julia/` | **Julia (MIT)** | a pinned, verbatim subset of JuliaLang/julia (incl. `test/`, the oracle source) — see `reference/README.md`; licensing in `LICENSE.md` |

We port from `reference/julia/src/` (the C/C++ runtime). `reference/julia/`'s
`base/`, `stdlib/`, `Compiler/`, `JuliaSyntax/`, `JuliaLowering/` are the
Julia-written layers Ruju will eventually AOT-compile and run; for now they
are reference, so the runtime is built appropriately for that eventual payload.

Conceptually, `runtime/` is the replacement for `reference/julia/src/`.

## Building and testing

```sh
cargo test  -p ruju-runtime                                  # native unit tests
cargo build -p ruju-runtime                                  # also surfaces warnings (keep it clean)
cargo build -p ruju-runtime --target wasm32-unknown-unknown --release
node runtime/harness.mjs                                          # wasm -> JS end-to-end checks
node runtime/verify_julia_subtype.mjs                             # oracle vs JuliaLang/julia
```

Tests touch global runtime state and are serialized with a mutex; the default
multi-threaded runner is fine. Re-run twice to confirm determinism.

## Working notes

- **Verify against JuliaLang/julia, not just our own tests.** Our hand-written
  tests can encode the same misunderstanding as the code. The reliable check is
  `runtime/verify_julia_subtype.mjs`, whose expected answers are copied verbatim
  from `reference/julia/test/subtype.jl` (when present) or fetched from upstream.
  When an audit is due, compare and contrast against the live JuliaLang/julia
  repository as well as the pinned `reference/`.
- **Root across allocations.** Any heap value held across an allocation must be
  rooted (`Rooted` / `Frame`), because allocation can trigger a collection. The
  auto-collect stress test exists to catch violations.
- Run a whitespace check on changed files before committing (no trailing
  whitespace or stray tabs).

## Commits and git authoring

- **Author/committer every commit as `andy-emerson <emerson.andrew@gmail.com>`** —
  the responsible human. Verify with `git log -1 --format='%an <%ae>'`.
- **Credit yourself as a co-author on every commit** (a `Co-Authored-By`
  trailer). Use your default attribution trailer — your name and version as
  your tooling provides them, not hand-typed per commit. Co-authorship is
  acknowledgment, not copyright; see `CONTRIBUTING.md`.
- **Message format** is `component: Brief summary` plus a short prose body —
  see `CONTRIBUTING.md`. Don't enumerate tests/docs unless they are the point
  of the change.
- **Develop on `main`** unless told otherwise, and verify local and remote are
  in sync after pushing.
