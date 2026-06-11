# Reference material

This directory holds a **verbatim, unmodified** subset of
[JuliaLang/julia](https://github.com/JuliaLang/julia), vendored as the porting
reference and test oracle for Ruju. It is not part of Ruju's own
source. It keeps its own license at `julia/LICENSE.md`; the project license is
at `../LICENSE.md`.

## Pinned upstream

| | |
| - | - |
| Repository | https://github.com/JuliaLang/julia |
| Commit | `d99fded7bf84695d3f7afa1e88db0058529a70bb` |
| Date | 2026-06-08 |
| Julia version | 1.14.0-DEV |
| License | MIT ("Expat"), see `julia/LICENSE.md` |

## What is included, and why

`julia/` preserves the upstream layout so the files cross-reference as they do
in Julia:

- `src/` — the C/C++ runtime we are reimplementing in Rust. This is the primary
  porting reference (`subtype.c`, `jltypes.c`, `gc-stock.c`, `interpreter.c`, …).
- `base/`, `stdlib/`, `Compiler/`, `JuliaSyntax/`, `JuliaLowering/` — the
  Julia-written layers that Ruju will eventually AOT-compile and run. Kept
  now as reference, so the runtime is built appropriately for that payload.
- `test/` — Julia's own test suite. The faithfulness oracle
  (`../runtime/verify_julia_subtype.mjs`) draws its expected answers from here
  (`test/subtype.jl`), and these are the conformance tests the eventual payload
  must pass.

## Audits

When checking faithfulness, compare against this pinned snapshot **and** against
the live JuliaLang/julia repository (for currency). Behavioural answers should
be checked with `../runtime/verify_julia_subtype.mjs`, whose expected results
come from Julia's own `test/` suite.

To advance the pin: re-vendor from a newer upstream commit and update the table
above.
