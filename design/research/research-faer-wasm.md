# faer-on-wasm verification research

Empirical verification of the plan to use **faer** (pure-Rust linear algebra) as the
BLAS/LAPACK replacement behind an LBT-shaped shim for Ruju's wasm target, with upstream
contributions kept language-agnostic.

All commands were run on 2026-07-07 in
`/tmp/claude-0/-home-user-Ruju/a5c16fbd-2475-5d13-8b8f-10d828600289/scratchpad/faer-research/`
with `rustc 1.94.1 (e408947bf 2026-03-25)`, `cargo 1.94.1`, `node v22.22.2`,
target `wasm32-unknown-unknown`.
Claims are **empirical** (observed output) unless labeled **WEB** (from web sources) or
**UNCERTAIN**.

## TL;DR

- **faer 0.24.4 does NOT compile for wasm32 today** — but only because of a
  4-line 32-bit-`usize` overflow bug (`n >> 32` with `n: usize`) in the *subspace
  operator* modules (`operator/{eigen,self_adjoint_eigen,svd}`). After the trivial,
  language-agnostic fix (`((n as u64) >> 32) as u32`) **everything builds, runs, and
  produces bit-identical results to native x86-64** for matmul, LU solve, QR, SVD,
  self-adjoint EVD, and general (non-Hermitian) complex EVD, executed in node from a
  bare no_std wasm module with zero imports.
- **Binary sizes** (opt-level=z, fat LTO, panic=abort, strip): matmul-only **51 KiB**;
  +LU **106 KiB**; +QR+SVD+EVD+general-EVD **~396 KiB** (435 KiB with relaxed-simd FMA).
- **pulp already has a complete wasm simd128 backend** — the "pulp is x86/aarch64 only"
  assumption is FALSE at pulp 0.22.3. It has `Simd128` and `RelaxedSimd` (FMA via
  `f32x4_relaxed_madd`/`f64x2_relaxed_madd`), f32x4/f64x2/c32/c64 lanes, and a
  host-controllable runtime flag crate (`pulp-wasm-simd-flag`). The `gemm` crate
  (faer's matmul kernels) also ships wasm simd128 microkernels. Disassembly of our
  build shows real `f64x2.mul`/`v128.load`/`f32x4.relaxed_madd` instructions.
- **Coverage**: faer covers the dense-factorization core LinearAlgebra.jl actually
  calls (LU partial+full pivot, LLT, pivoted LLT, LDLT, Bunch-Kaufman, QR ± column
  pivoting, SVD, self-adjoint EVD, general EVD incl. real/complex Schur internals,
  generalized EVD via QZ). Biggest gaps: high-level Schur API + reordering
  (`gees`/`trexc`/`trsen`), Sylvester (`trsyl`), generalized SVD (`ggsvd3`), LQ
  family, `geevx` extras (balancing/condition numbers), eigen subset selection
  (`syevr` ranges, `stebz`/`stein`), condition estimators (`gecon`/`trcon`).
- **Threading**: `Par::Seq` is first-class (every compute fn takes a `Par` argument);
  the `rayon` feature **fails to build on wasm32** (`atomic-wait` has no wasm platform)
  — sequential is the wasm mode, and that is fully supported.
- **Precedent**: faer is already used on wasm in the wild (faer-rs#222, sparse Cholesky
  on wasm32 in Firefox via wasm-pack, fixed by the maintainer 2025-04-30).
- **Upstream plan**: (1) the 4-line 32-bit fix (benefits any 32-bit target: wasm32,
  armv7, i686); (2) optionally a faer `relaxed-simd` passthrough feature (consumers can
  already enable it today via feature unification — verified); (3) optionally a wasm32
  CI job. **No pulp backend contribution is needed.** Note faer development moved to
  Codeberg; GitHub is a mirror (WEB).

---

## 1. What was cloned

```
$ git clone --depth 1 https://github.com/sarah-quinones/faer-rs
$ git clone --depth 1 https://github.com/sarah-quinones/pulp
$ git -C faer-rs log -1 --format='%H %ad %s' --date=short
0539947ffb757a739d7e703a7d2fa0c792a909c1 2026-06-24 chore: Release      # faer crate version 0.24.4
$ git -C pulp log -1 --format='%H %ad %s' --date=short
5eb07fd7b68edf0a5e19f71737d315f72a510295 2026-06-20 bump patch version  # pulp crate version 0.22.3
```

Demand side: Ruju's `reference/julia/stdlib/` does **not** vendor LinearAlgebra sources;
`reference/julia/stdlib/LinearAlgebra.version` pins

```
LINEARALGEBRA_SHA1 = 08ee29ebe6a6ca12e8d71f9a9a3833cf6d95f4a8
LINEARALGEBRA_GIT_URL := https://github.com/JuliaLang/LinearAlgebra.jl.git
```

so `LinearAlgebra.jl` was cloned and checked out at exactly that SHA (commit dated
2026-05-29). All demand-side analysis below is against the pinned commit.

WEB: faer's `Cargo.toml` states `repository = "https://codeberg.org/sarah-quinones/faer"`,
and web search confirms the project migrated to Codeberg with GitHub as a mirror.
Upstream PRs should target Codeberg. (Codeberg's issue tracker returned HTTP 403 to our
fetcher — UNCERTAIN whether additional wasm issues exist there.)

## 2. Does faer compile to wasm32-unknown-unknown today? NO — then YES after a 4-line fix

faer 0.24.4 feature list (from `faer/Cargo.toml`):
`default = [std, rayon, sparse-linalg, rand, npy]`, plus `linalg`, `sparse`,
`nightly` (x86-only AVX-512), `perf-warn`, `unstable`. The dense solvers are gated by
`linalg`. There is no wasm-specific feature.

Consumer crate: `faer = { path = ..., default-features = false, features = ["linalg"] }`,
`crate-type = ["cdylib"]`, `#![no_std]` with a bump allocator over `memory.grow`
(heap start from `__heap_base`) and a `panic_handler` → the produced module needs
**zero imports**. Profile: `opt-level = "z"`, `lto = true`, `codegen-units = 1`,
`panic = "abort"`, `strip = true`.

First build **fails**:

```
$ cargo build --target wasm32-unknown-unknown --release
error: this arithmetic operation will overflow
   --> faer/src/./operator/eigen/mod.rs:319:13
    |
319 |             let n1 = (n >> 32) as u32;
    |                      ^^^^^^^^^ attempt to shift right by `32_i32`, which would overflow
(also operator/eigen/mod.rs:771, operator/self_adjoint_eigen/mod.rs:108,
 operator/svd/mod.rs:159)
error: could not compile `faer` (lib) due to 4 previous errors
```

`n` is `usize` (32-bit on wasm32); the code splits `n` into two u32 halves to convert
it to the scalar type `T`. This is in the *subspace operator* (partial/iterative
eigen/svd) module and hits **every 32-bit target**, not just wasm. Fix applied locally:

```
-		let n1 = (n >> 32) as u32;
+		let n1 = ((n as u64) >> 32) as u32;
```

(4 occurrences, 3 files). After the patch:

```
$ cargo build --target wasm32-unknown-unknown --release
    Finished `release` profile [optimized] target(s) in 26.77s
```

Also verified: `features = ["linalg", "std"]` builds for wasm32 (53.3 s, fresh build).
`features = ["linalg", "std", "rayon"]` **fails**:

```
error[E0433]: failed to resolve: use of unresolved module or unlinked crate `platform`
error: could not compile `atomic-wait` (lib) due to 3 previous errors
```

(`rayon` pulls `spindle` → `atomic-wait`, which has no wasm32 implementation.)
Sequential (`Par::Seq`) is the wasm mode.

## 3. Execution smoke test (node, zero imports) + native cross-check

`node run.mjs` instantiates the module with an empty import object:

```
exports: memory, __heap_base, lu_solve_sum, matmul_trace, qr_svd_evd_probe, __data_end
matmul_trace = 114
lu_solve_sum = 0.8857142857142857
qr_svd_evd_probe = 1.9483450492039642
```

- `matmul_trace`: trace of A·B for A[i,j]=i+2j+1, B[i,j]=3i+j−2 (3×3). Hand-computed:
  21+36+57 = **114** ✓. (The "expected 78" string in the harness output was a stale
  comment in the test script, not a failure.)
- `lu_solve_sum`: sum of the solution of a 3×3 system via `partial_piv_lu()`.
  Hand-computed exact answer 31/35 = 0.8857142857… ✓.
- `qr_svd_evd_probe`: runs `qr()`, `svd()`, `self_adjoint_eigen(Side::Lower)` on an SPD
  3×3, plus `eigenvalues()` (general complex EVD) of a 3×3 cyclic permutation matrix,
  and sums probe values.

Native cross-check (same crate, x86-64 with AVX paths active):

```
$ cargo run --release --features full --bin native
matmul_trace = 114
lu_solve_sum = 0.8857142857142857
qr_svd_evd_probe = 1.9483450492039642
```

**Bit-identical to the wasm run**, including the SVD/EVD probe.

## 4. Binary sizes (browser-delivery cost)

Staged feature flags on the consumer crate select which faer entry points are exported;
sizes of the final `.wasm` (bytes), same profile as above:

| contents | plain | `-C target-feature=+simd128` |
| - | -: | -: |
| matmul only | 52,660 (51.4 KiB) | 52,070 |
| + LU (partial-pivot solve) | 108,929 (106.4 KiB) | 107,262 |
| + QR, SVD, self-adjoint EVD, general EVD | 405,728 (396.2 KiB) | 401,507 |
| same, + pulp `relaxed-simd` (FMA) | — | 445,633 (435.2 KiB) |

So the full dense-decomposition suite costs **~400 KiB uncompressed** (before
`wasm-opt` and before gzip/brotli transport compression, both of which would shrink it
further — UNCERTAIN by how much; wasm-opt was not available in this environment).

## 5. pulp simd128 status: ALREADY DONE upstream

The research premise ("pulp has only x86/aarch64 + scalar; a simd128 backend is the
contribution") is **out of date**. At pulp 0.22.3:

- `pulp/src/wasm.rs` (2,516 lines) implements the full `Simd` trait for wasm32 with two
  ISA types:
  - `Simd128` — requires target feature `"simd128"`;
  - `RelaxedSimd` (behind cargo feature `relaxed-simd`, **on by default in pulp**) —
    requires `"simd128"` + `"relaxed-simd"`; implements `mul_add_*` via
    `f32x4_relaxed_madd` / `f64x2_relaxed_madd` (real FMA). Without it, `vfmaq_f32/f64`
    helpers fall back to mul+add (strict semantics).
  - Lane types: `f32s = f32x4`, `f64s = f64x2`, `c32s = f32x4`, `c64s = f64x2`, plus
    full integer/bitwise/comparison/shuffle coverage (NEON-style rotate tables).
- `pulp::Arch` dispatch is per-arch via `match_cfg!` in `lib.rs` (x86 / aarch64 /
  **wasm32** / scalar fallback). On wasm32, `Arch::new()` picks
  `RelaxedSimd > Simd128 > Scalar`.
- wasm has no CPUID, so detection is a **host-set runtime flag**: the tiny
  `pulp-wasm-simd-flag` crate exposes `enable_simd128()` / `enable_relaxed_simd()` /
  `is_simd128_enabled()`, defaulting to `cfg!(target_feature = "simd128")`. A JS host
  can feature-probe (e.g. `WebAssembly.validate` on a simd module) and flip the flag,
  or you simply compile with `-C target-feature=+simd128` and it is statically on.
- docs.rs metadata for pulp explicitly lists `wasm32-unknown-unknown` as a documented
  target.

**Verified in the produced binary** (wabt `readWasm` → wat, counting instructions):

```
full_simdno.wasm  f64x2.mul: 267  f64x2.add: 283  v128.load: 1231  relaxed_madd: 0
full_simdyes.wasm f64x2.mul: 267  f64x2.add: 281  v128.load: 1239  relaxed_madd: 0
full_relaxed.wasm f64x2.mul: 287  v128.load: 1865 relaxed_madd: 253
```

Notes: simd128 instructions are present even in the "plain" build because pulp emits
them inside `#[target_feature(enable = "simd128")]` functions and selects them at
runtime via the flag; with `-C target-feature=+simd128` selection is compile-time.
The `relaxed_madd` row required enabling pulp's `relaxed-simd` feature (see §7), was
built with `-C target-feature=+simd128,+relaxed-simd`, and **runs correctly in node
v22** (same three probe values).

Also relevant: the `gemm` crate (faer's matmul microkernels, v0.19) has a dedicated
wasm32 simd128 microkernel module (`gemm-f64/src/microkernel.rs`, `pub mod simd128`
with 1×1…2×4 kernels) and its own runtime flag
(`gemm_common::get_wasm_simd128()` = `cfg!(target_feature = "simd128") || flag`,
setter behind feature `wasm-simd128-enable`). With `+simd128` baked in, gemm's simd
kernels are unconditionally selected.

**Conclusion: no pulp backend contribution is needed.** If Ruju wants runtime (rather
than compile-time) SIMD selection, the language-agnostic hooks already exist
(`pulp-wasm-simd-flag`, `gemm/wasm-simd128-enable`); the only missing convenience is a
faer-level feature that forwards to them (§8).

## 6. faer capability audit (from source, `faer/src/linalg/`)

Modules present: `matmul` (incl. triangular variants), `mat_ops`, `reductions`,
`triangular_solve`, `triangular_inverse`, `householder`, `jacobi`, `kron`, plus:

| area | submodules | high-level API (`linalg/solvers.rs`) |
| - | - | - |
| LU | `lu/partial_pivoting`, `lu/full_pivoting` | `PartialPivLu`, `FullPivLu` (+ `solve`, `inverse`, `det`) |
| Cholesky | `cholesky/llt`, `ldlt`, `llt_pivoting`, `bunch_kaufman` | `Llt`, `Ldlt`, `Lblt` (Bunch-Kaufman) |
| QR | `qr/no_pivoting`, `qr/col_pivoting` | `Qr` (blocked Householder; `R()`, Q apply), `ColPivQr` |
| SVD | `svd/bidiag`, `svd/bidiag_svd` | `Svd` (`U()`, `S()`, `V()`), thin variants |
| EVD | `evd/hessenberg`, `evd/schur/{real_schur,complex_schur}` (multishift QR), `evd/tridiag`, `evd/tridiag_evd` | `SelfAdjointEigen`, `Eigen`, `eigenvalues()` |
| GEVD | `gevd/gen_hessenberg`, `gevd/qz_real`, `gevd/qz_cplx` | `GeneralizedEigen`; low-level `gevd_real`/`gevd_cplx` |

Scalar types (`faer-traits` impls observed): `f32`, `f64`, generic `Complex<T>`
(⇒ `c32`, `c64` — complex is fully supported across the decompositions, which are
generic over `ComplexField`), plus `fx128` (double-double extended precision) and
`Symbolic`. The trait hierarchy (`RealField`/`ComplexField`) means user-defined real
types also work (relevant precedent: that is more generality than LAPACK has).

Threading: `pub enum Par { Seq, #[cfg(feature="rayon")] Rayon(NonZeroUsize) }`
(`faer/src/lib.rs:929`). Every compute routine takes `Par` explicitly —
**single-threaded is a first-class, always-available mode**, not a degraded fallback.
As shown in §2, `rayon` does not build on wasm32-unknown-unknown today.

Bonus: **`faer-ffi`** — an upstream crate exporting a C ABI
(`libfaer_v0_23_*` symbols, cbindgen-generated header, its own `alloc`/`dealloc`
exports). Upstream already maintains a language-agnostic FFI surface, which is exactly
the shape an LBT-shim consumer wants to build against; worth tracking/contributing to
rather than inventing a parallel ABI.

## 7. Demand side: what LinearAlgebra.jl (at Ruju's pin 08ee29eb) actually calls

BLAS routines wrapped in `src/blas.jl`: gemm, gemmt, gemv, gbmv, sbmv/hbmv, symv/hemv,
symm/hemm, syrk/herk, syr2k/her2k, syr/her, spr, spmv/hpmv, trmm, trsm, trmv, trsv,
ger/geru/gerc, axpy, axpby, copy, scal, rot, dot/dotc/dotu, nrm2, asum, iamax.

LAPACK call sites **outside** `lapack.jl` (i.e., what the factorization/solver layer
actually uses; counts = number of call sites):

```
syevr 10 · trevc 6 · ormqr 6 · ormlq 6 · gemqrt 6 · hseqr 5 · getrs 5 · gebal 5
trsyl 4 · sygvd 4 · syevd 4 · syev 4 · ormtr 4 · ormhr 4 · ggev3 4 · ggev 4 · geevx 4
potrs 3 · gesdd 3 · trcon 2 · sytrs/sytri/sytrf/syconv 2ea · stein 2 · stebz 2
pstrf 2 · potri 2 · potrf 2 · laic1 2 · lacpy 2 · ggsvd3/ggsvd 2ea · getrf 2 · gesvd 2
gesv 2 · gebak 2 · bdsdc 2 · tzrzf 1 · trtrs 1 · trtri 1 · trsen 1 · trrfs 1 · tgsen 1
stev 1 · stegr 1 · ormrz 1 · orgtr 1 · orglq 1 · orghr 1 · hetrs/hetri/hetrf/hetrd 1ea
gges3 1 · gges 1 · getri 1 · geqrt 1 · geqp3 1 · gelqf 1 · gehrd 1 · gees 1
```

(Plus BLAS call sites: gemv, syrk/herk, gemm, trmm/trsm/trmv, symm/hemm, symv/hemv,
dot/dotc/dotu, nrm2, asum, axpy/axpby.) Note `gels/gelsd/gelsy`, `gtsv/gttrf`,
`gbtrf/gbtrs` are wrapped in `lapack.jl` but have **no** call sites in the
factorization layer — `\` routes through qr/lu objects, and
Bidiagonal/Tridiagonal solves are pure-Julia.

### Coverage matrix (LinearAlgebra demand → faer)

✓ = direct faer equivalent · △ = achievable via composition/low-level module ·
✗ = no equivalent (falls back to Julia generic code or is a real gap)

| routine(s) | purpose | faer | notes |
| - | - | - | - |
| gemm/gemmt/gemv/ger | matmul | ✓ | `linalg::matmul` (+ simd128 gemm kernels on wasm) |
| syrk/herk/syr2k/her2k/symm/hemm/trmm | structured matmul | ✓/△ | matmul with triangle dst (`syrk`-shape ✓); trmm via triangular matmul |
| trsm/trsv/trtrs | triangular solve | ✓ | `triangular_solve` |
| trtri | triangular inverse | ✓ | `triangular_inverse` |
| dot/nrm2/asum/axpy/scal/copy/iamax/rot | level-1 | ✓ | `mat_ops`/`reductions` (rot: trivial) |
| gbmv/sbmv/hbmv/spmv/spr | banded/packed | ✗ | no banded/packed storage in faer; LA only exposes these, core types don't need them |
| getrf/getrs/getri/gesv | LU | ✓ | `PartialPivLu` (+ `FullPivLu` beyond LAPACK) |
| potrf/potrs/potri | Cholesky | ✓ | `Llt` |
| pstrf | pivoted Cholesky | ✓ | `cholesky/llt_pivoting` |
| sytrf/sytrs/sytri, hetrf/…, syconv | symmetric-indefinite | ✓ | `Lblt` (Bunch-Kaufman); different storage/pivot encoding than LAPACK — shim must translate or own the object |
| geqrt/gemqrt (blocked WY), geqrf/ormqr/orgqr | QR | ✓ | `qr/no_pivoting` stores blocked Householder; apply/materialize Q supported |
| geqp3 | pivoted QR | ✓ | `qr/col_pivoting` |
| gelqf/orglq/ormlq | LQ | △ | no LQ in faer; QR of Aᴴ + adapter |
| gesdd/gesvd, bdsdc/bdsqr | SVD | ✓ | `Svd` (bidiag + D&C); no jobu/jobvt subset economy beyond thin — thin ✓ |
| syev/syevd/syevr, heev\* | self-adjoint EVD | ✓ | `SelfAdjointEigen`; **no il:iu/vl:vu subset selection** (syevr ranges) — UNCERTAIN/likely ✗, full spectrum only |
| stev/stegr/stebz/stein | tridiagonal EVD | △/✗ | `evd/tridiag_evd` internal (full spectrum); subset/bisection ✗ |
| geev | general EVD | ✓ | `Eigen`/`eigenvalues()` (real & complex) |
| geevx/gebal/gebak | balancing + condition numbers | ✗ | no exposed balancing or eigen condition estimates |
| gees + trexc/trsen | Schur + reordering | △/✗ | real/complex Schur exists (`evd/schur`, multishift QR) but no high-level Schur object; **no reordering (trexc/trsen)** found — blocks `ordschur` |
| trevc | eigvecs from Schur | △ | internal `evd_from_schur`; not exposed standalone |
| trsyl | Sylvester equation | ✗ | needed by `sylvester()`, `schur`-based matrix functions |
| ggev/ggev3 | generalized EVD | ✓ | `gevd_real`/`gevd_cplx` (QZ), `GeneralizedEigen` |
| gges/gges3, tgsen | generalized Schur + reordering | △/✗ | QZ internals exist; no high-level API, no reordering |
| sygvd/hegvd | sym-definite gen. EVD | △ | compose Cholesky reduction + `SelfAdjointEigen` |
| ggsvd/ggsvd3 | generalized SVD | ✗ | `svd(A,B)` would fall back |
| gecon/trcon | condition estimation | ✗ | `cond()` via svdvals works; `trcon` users fall back |
| trrfs | iterative refinement | ✗ | |
| tzrzf/ormrz, laic1 | complete orthogonal decomp (rank-deficient ldiv) | ✗ | pivoted-QR-based fallback path needed |
| lacpy | copy | ✓ | trivial |

**Biggest gaps, confirmed**: (1) Schur as a first-class factorization with reordering
(`gees`/`trexc`/`trsen`/`tgsen`) — this is what `schur()`, `ordschur()`, and the
Schur–Parlett matrix functions need; (2) `trsyl`; (3) `ggsvd3`; (4) `geevx`
balancing/condition extras; (5) spectrum-subset eigensolvers; (6) LQ (adapter is easy).
Everything else on LinearAlgebra's hot path (mul!, lu, cholesky, bunchkaufman, qr,
svd, eigen — symmetric and general, generalized eigen) has a faer implementation.

## 8. Proposed upstream contribution plan (all language-agnostic)

Target **codeberg.org/sarah-quinones/faer** (GitHub is a mirror — WEB).

1. **faer: 32-bit `usize` portability fix** (do first; unblocks everything).
   The exact 4-line patch verified here: `((n as u64) >> 32) as u32` at
   `operator/eigen/mod.rs:319,771`, `operator/self_adjoint_eigen/mod.rs:108`,
   `operator/svd/mod.rs:159`. Framing: "faer does not compile on any 32-bit target
   (wasm32, armv7, i686)". Nothing Julia-specific; trivially reviewable.
   (Side observation for the PR discussion: the split converts `n` via
   `n0 as f64 + n1 as f64` without scaling `n1` by 2^32; on 64-bit that looks like a
   latent bug for n ≥ 2^32 — worth flagging upstream. UNCERTAIN of intent.)
2. **faer: wasm32 CI check** (`cargo check --target wasm32-unknown-unknown
   --no-default-features --features linalg,std`) so 32-bit breakage can't regress.
   Cheap, benefits every wasm consumer.
3. **faer: optional `relaxed-simd`/wasm passthrough feature** forwarding to
   `pulp/relaxed-simd` (and possibly `gemm/wasm-simd128-enable`). *Not strictly
   required*: verified empirically that a consumer can enable it today purely via
   cargo feature unification by depending on `pulp = { features = ["relaxed-simd"] }`
   alongside faer (result: 253 `relaxed_madd` instructions, correct results in node).
   The feature is a convenience/discoverability contribution.
4. **pulp: nothing required.** The simd128 + relaxed-simd backend and the host-side
   runtime flag mechanism already exist and are exercised by our build. If anything:
   documentation of the JS-host feature-detection pattern.
5. **Longer-term, still language-agnostic (LAPACK-parity features any consumer
   wants)**: high-level Schur factorization API + Schur reordering (trexc/trsen
   equivalent), Sylvester solver, generalized SVD, LQ convenience, eigen subset
   selection. These fill the ✗ rows above and are the natural follow-on proposals
   once the small PRs establish a relationship. The routines Julia needs them for
   (`schur`, `ordschur`, `sylvester`, `svd(A,B)`) can meanwhile fall back to Julia
   generic code or remain unimplemented in the shim (LBT tolerates missing symbols
   until called).
6. **Shim-side note (Ruju, not upstream)**: `faer-ffi` shows upstream is receptive to
   a C-ABI surface; an LBT-shaped shim can either wrap faer's Rust API directly
   (we're in Rust anyway) or build on/extend `faer-ffi`. Threading: expose only
   sequential (`Par::Seq`) on wasm; the `rayon` feature must stay off
   (does not compile — §2).

## 9. Precedent (WEB unless noted)

- **faer-rs#222 "Sparse cholesky crash on wasm32"** (empirical: fetched via GitHub
  API): user ran faer 0.22 in Firefox via wasm-pack, single- and multi-threaded;
  maintainer fixed it; closed completed 2025-04-30. Demonstrates both real-world
  faer-on-wasm usage and maintainer willingness to support the target.
- pulp's docs.rs config lists `wasm32-unknown-unknown` as a documented target, and the
  repo contains a dedicated `pulp-wasm-simd-flag` crate — wasm is a deliberate,
  maintained platform, not an accident (empirical, from source).
- `gemm` 0.19 ships wasm32 simd128 microkernels and a `wasm-simd128-enable` feature
  (empirical, from vendored registry source).
- No high-profile faer-on-wasm demo apps surfaced in web search; searches mostly
  return generic rust-wasm material. Codeberg issue tracker inaccessible (403) —
  UNCERTAIN what additional wasm activity exists there.

## 10. Artifacts

- `faer-research/faer-rs` — clone @ 0539947 with the 4-line 32-bit patch applied
  (see `git diff` in the clone).
- `faer-research/pulp` — clone @ 5eb07fd (unmodified).
- `faer-research/LinearAlgebra.jl` — clone @ 08ee29eb (Ruju's pin; unmodified).
- `faer-research/consumer` — the wasm consumer crate (staged features `lu`, `full`),
  `run.mjs` node harness, `out/*.wasm` size-matrix binaries.
- `faer-research/stdtest` — std/rayon feature build probes.
