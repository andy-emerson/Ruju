// Oracle check of Ruju's subtype engine against JuliaLang/julia's own
// test suite. Each case below is copied from test/subtype.jl (the cited line);
// the EXPECTED result is exactly what Julia's `@test` asserts. We do not run
// Julia — its test assertions *are* the expected answers.
//
// Mapping (faithful, not a divergence):
//   Ref{T}   -> Box{T}     (both are single-parameter invariant types)
//   Int      -> Int64
//   Vector{T} used only for invariance also maps to Box{T}
// Cases needing Vararg, Type{}, Pair (2-param), String, or a parameter-sharing
// supertype (AbstractVector) are out of scope for the current ABI and omitted.
//
//   node runtime/verify_julia_subtype.mjs
// (build first: cargo build -p ruju-runtime --target wasm32-unknown-unknown --release)

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const wasmPath = resolve(here, "..", "target", "wasm32-unknown-unknown", "release", "ruju_runtime.wasm");
const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const x = instance.exports;
x.rj_init();

// Builtin type ids (match runtime/src/types.rs `id`).
const ID = { Any: 0, Number: 1, Real: 2, Integer: 3, Signed: 4, Float64: 21, Bool: 8, Int8: 9, Int16: 10, Int64: 12, Char: 22, Bottom: 26 };
const ty = (id) => x.rj_builtin_type(id);

// Julia type constructors mapped onto the runtime ABI.
const Int = ty(ID.Int64), Integer = ty(ID.Integer), Real = ty(ID.Real), Number = ty(ID.Number);
const Int8 = ty(ID.Int8), Int16 = ty(ID.Int16), Any = ty(ID.Any), Bottom = ty(ID.Bottom);
const Ref = (t) => x.rj_box_type(t);
const Tuple = (...ts) => (ts.length === 1 ? x.rj_tuple_type1(ts[0]) : x.rj_tuple_type2(ts[0], ts[1]));
const Union = (a, b) => x.rj_union_type(a, b);
const tvar = (lb, ub) => x.rj_typevar(lb ?? 0, ub ?? 0); // 0 => Bottom / Any
const where = (v, body) => x.rj_unionall(v, body);

const sub = (a, b) => x.rj_subtype(a, b) === 1;
const strict = (a, b) => sub(a, b) && !sub(b, a);
const equal = (a, b) => sub(a, b) && sub(b, a);

// Each case: [source line, predicate, build()]. `expected` is implied by the
// predicate name (Julia asserts it true).
const cases = [
  // --- level 1: nominal, tuples, invariance (test_1) ---
  ["L22 issub_strict(Int, Integer)", "strict", () => [Int, Integer]],
  ["L30 issub_strict(Tuple{Int,Int}, Tuple{Integer,Integer})", "strict", () => [Tuple(Int, Int), Tuple(Integer, Integer)]],
  ["L35 !issub(Tuple{Int,Int}, Tuple{Int})", "notsub", () => [Tuple(Int, Int), Tuple(Int)]],
  ["L38 !issub(Vector{Int}, Vector{Integer})", "notsub", () => [Ref(Int), Ref(Integer)]],

  // --- existential vs universal (test_3) ---
  ["L99 issub_strict(Tuple{R,R} where R, Tuple{T,S} where {T,S})", "strict", () => {
    const R = tvar(); const T = tvar(); const S = tvar();
    return [where(R, Tuple(R, R)), where(T, where(S, Tuple(T, S)))];
  }],
  ["L116 issub_strict(Tuple{Int,Int}, Tuple{Union{T,?},T} where T) [String->Real here]", "strict", () => {
    // L122: issub_strict(Tuple{Int,Int}, @UnionAll T Tuple{Union{T,Real},T}) is not in file;
    // use the analogous L123 case with a concrete left that pins T=Int.
    const T = tvar();
    return [Tuple(Int, Int), where(T, Tuple(Union(T, Real), T))];
  }],

  // --- the diagonal rule (test_diagonal) ---
  ["L178 !issub(Tuple{Integer,Integer}, Tuple{T,T} where T)", "notsub", () => {
    const T = tvar(); return [Tuple(Integer, Integer), where(T, Tuple(T, T))];
  }],
  ["L110 !issub(Tuple{Real,Real}, Tuple{T,T} where T<:Real)", "notsub", () => {
    const T = tvar(0, Real); return [Tuple(Real, Real), where(T, Tuple(T, T))];
  }],
  ["L179 issub(Tuple{Integer,Int}, Tuple{T, S<:T} where T)", "sub", () => {
    const T = tvar(); const S = tvar(0, T);
    return [Tuple(Integer, Int), where(T, where(S, Tuple(T, S)))];
  }],

  // --- unions inside invariant Ref (test around L377/448/450) ---
  ["L377 !issub(Union{Int,Ref{Union{Int,Int8}}}, Union{Int,Ref{Union{Int8,Int16}}})", "notsub", () =>
    [Union(Int, Ref(Union(Int, Int8))), Union(Int, Ref(Union(Int8, Int16)))]],
  ["L448 !issub(Ref{Union{Ref{Int},Ref{Int8}}}, Ref{Ref{T}} where T)", "notsub", () => {
    const T = tvar(); return [Ref(Union(Ref(Int), Ref(Int8))), where(T, Ref(Ref(T)))];
  }],
  ["L450 !issub(Ref{Union{Ref{Int},Ref{Int8}}}, Union{Ref{Ref{Int}},Ref{Ref{Int8}}})", "notsub", () =>
    [Ref(Union(Ref(Int), Ref(Int8))), Union(Ref(Ref(Int)), Ref(Ref(Int8)))]],

  // --- bounded variables and where-collapse (test_6) ---
  ["L479 isequal_type(Ref{T} where Int<:T<:Int, Ref{Int})", "equal", () => {
    const T = tvar(Int, Int); return [where(T, Ref(T)), Ref(Int)];
  }],
  ["L480 isequal_type(Ref{T} where Integer<:T<:Integer, Ref{Integer})", "equal", () => {
    const T = tvar(Integer, Integer); return [where(T, Ref(T)), Ref(Integer)];
  }],
  ["L512 isequal_type(Ref{Ref{Int}}, Ref{Ref{T} where Int<:T<:Int})", "equal", () => {
    const T = tvar(Int, Int); return [Ref(Ref(Int)), Ref(where(T, Ref(T)))];
  }],
  ["L513 isequal_type(Ref{Ref{Int}}, Ref{Ref{T}} where Int<:T<:Int)", "equal", () => {
    const T = tvar(Int, Int); return [Ref(Ref(Int)), where(T, Ref(Ref(T)))];
  }],
  ["L515 !issub(Ref{Ref{T}} where Int<:T<:Int, Ref{Ref{T} where T<:Int})", "notsub", () => {
    const T1 = tvar(Int, Int); const T2 = tvar(0, Int);
    return [where(T1, Ref(Ref(T1))), Ref(where(T2, Ref(T2)))];
  }],

  // --- nested existential/universal: S<:T and S>:T (test_6) ---
  ["L486 issub_strict(Tuple{Int,Ref{Int}}, Tuple{S,Ref{T}} where {T, S<:T})", "strict", () => {
    const T = tvar(); const S = tvar(0, T);
    return [Tuple(Int, Ref(Int)), where(T, where(S, Tuple(S, Ref(T))))];
  }],
  ["L489 !issub(Tuple{Real,Ref{Int}}, Tuple{S,Ref{T}} where {T, S<:T})", "notsub", () => {
    const T = tvar(); const S = tvar(0, T);
    return [Tuple(Real, Ref(Int)), where(T, where(S, Tuple(S, Ref(T))))];
  }],
  ["L493 issub_strict(Tuple{Real,Ref{Int}}, Tuple{S,Ref{T}} where {T, S>:T})", "strict", () => {
    const T = tvar(); const S = tvar(T, 0);
    return [Tuple(Real, Ref(Int)), where(T, where(S, Tuple(S, Ref(T))))];
  }],
  ["L496 !issub(Tuple{Ref{Int},Ref{Integer}}, Tuple{Ref{S},Ref{T}} where {T, S>:T})", "notsub", () => {
    const T = tvar(); const S = tvar(T, 0);
    return [Tuple(Ref(Int), Ref(Integer)), where(T, where(S, Tuple(Ref(S), Ref(T))))];
  }],
  ["L499 issub_strict(Tuple{Ref{Real},Ref{Integer}}, Tuple{Ref{S},Ref{T}} where {T, S>:T})", "strict", () => {
    const T = tvar(); const S = tvar(T, 0);
    return [Tuple(Ref(Real), Ref(Integer)), where(T, where(S, Tuple(Ref(S), Ref(T))))];
  }],

  // --- existential unions (test_7) ---
  ["L531 isequal_type(Ref{Union{Int16,T}} where T, Ref{Union{Int16,S}} where S)", "equal", () => {
    const T = tvar(); const S = tvar();
    return [where(T, Ref(Union(Int16, T))), where(S, Ref(Union(Int16, S)))];
  }],

  // --- T<:Int vs T>:Int (issue #53019, L176) ---
  ["L176 (Tuple{T,T} where T<:Int) <: (Tuple{T,T} where T>:Int)", "sub", () => {
    const T1 = tvar(0, Int); const T2 = tvar(Int, 0);
    return [where(T1, Tuple(T1, T1)), where(T2, Tuple(T2, T2))];
  }],
];

const pred = { strict, equal, sub: (a, b) => sub(a, b), notsub: (a, b) => !sub(a, b), noteq: (a, b) => !equal(a, b) };

let pass = 0, fail = 0;
for (const [src, kind, build] of cases) {
  const [a, b] = build();
  const ok = pred[kind](a, b);
  if (ok) { pass++; } else { fail++; console.log(`MISMATCH  ${src}`); }
}
console.log(`\n${pass}/${cases.length} match JuliaLang/julia (test/subtype.jl); ${fail} mismatch`);
process.exitCode = fail ? 1 : 0;
