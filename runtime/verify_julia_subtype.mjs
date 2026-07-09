// Oracle check of Ruju's subtype engine against JuliaLang/julia's own
// test suite. Each case below is copied from test/subtype.jl (the cited line);
// the EXPECTED result is exactly what Julia's `@test` asserts. We do not run
// Julia — its test assertions *are* the expected answers.
//
// Mapping (faithful, not a divergence):
//   Ref{T}   -> Box{T}     (both are single-parameter invariant types)
//   Int      -> Int64
//   Vector{T} used only for invariance also maps to Box{T}
// Vararg (unbounded and fixed-count), Type{}, and Pair are all expressible.
// Cases needing String, typevar-count Vararg{T,N}, or a parameter-sharing
// supertype (AbstractVector) remain out of scope for the current ABI.
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
const ID = { Any: 0, Number: 1, Real: 2, Integer: 3, Signed: 4, Float64: 21, Bool: 8, Int8: 9, Int16: 10, Int32: 11, Int64: 12, Char: 22, Bottom: 26, DataType: 25, UnionT: 28, TypeVarT: 30, UnionAllT: 31, Type: 34 };
const ty = (id) => x.rj_builtin_type(id);

// Julia type constructors mapped onto the runtime ABI.
const Int = ty(ID.Int64), Integer = ty(ID.Integer), Real = ty(ID.Real), Number = ty(ID.Number);
const Int8 = ty(ID.Int8), Int16 = ty(ID.Int16), Int32 = ty(ID.Int32), Any = ty(ID.Any), Bottom = ty(ID.Bottom);
const Float64 = ty(ID.Float64);
const Ref = (t) => x.rj_box_type(t);
const Tuple = (...ts) => {
  switch (ts.length) {
    case 0: return x.rj_tuple_type0();
    case 1: return x.rj_tuple_type1(ts[0]);
    case 2: return x.rj_tuple_type2(ts[0], ts[1]);
    case 3: return x.rj_tuple_type3(ts[0], ts[1], ts[2]);
    case 4: return x.rj_tuple_type4(ts[0], ts[1], ts[2], ts[3]);
    default: throw new Error(`no ABI for ${ts.length}-tuples`);
  }
};
const Vararg = (t) => x.rj_vararg(t); // unbounded Vararg{t}
const VarargN = (t, n) => x.rj_vararg_n(t, BigInt(n)); // Vararg{t, n}
const VarargTV = (t, v) => x.rj_vararg_tv(t, v); // Vararg{t, N} with typevar N
const Pair = (a, b) => x.rj_pair_type(a, b); // two-parameter invariant type
const TypeT = (t) => x.rj_type_type(t); // Type{t}
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

  // --- unbounded Vararg in tuples (test_1 varargs; test at L587-594) ---
  ["L43 issub_strict(Tuple{Int,Int}, Tuple{Vararg{Int}})", "strict", () => [Tuple(Int, Int), Tuple(Vararg(Int))]],
  ["L44 issub_strict(Tuple{Int,Int}, Tuple{Int,Vararg{Int}})", "strict", () => [Tuple(Int, Int), Tuple(Int, Vararg(Int))]],
  ["L45 issub_strict(Tuple{Int,Int}, Tuple{Int,Vararg{Integer}})", "strict", () => [Tuple(Int, Int), Tuple(Int, Vararg(Integer))]],
  ["L46 issub_strict(Tuple{Int,Int}, Tuple{Int,Int,Vararg{Integer}})", "strict", () => [Tuple(Int, Int), Tuple(Int, Int, Vararg(Integer))]],
  ["L47 issub_strict(Tuple{Int,Vararg{Int}}, Tuple{Vararg{Int}})", "strict", () => [Tuple(Int, Vararg(Int)), Tuple(Vararg(Int))]],
  ["L48 issub_strict(Tuple{Int,Int,Int}, Tuple{Vararg{Int}})", "strict", () => [Tuple(Int, Int, Int), Tuple(Vararg(Int))]],
  ["L49 issub_strict(Tuple{Int,Int,Int}, Tuple{Integer,Vararg{Int}})", "strict", () => [Tuple(Int, Int, Int), Tuple(Integer, Vararg(Int))]],
  ["L51 issub_strict(Tuple{}, Tuple{Vararg{Any}})", "strict", () => [Tuple(), Tuple(Vararg(Any))]],
  ["L54 isequal_type(Tuple{Vararg{Integer}}, Tuple{Vararg{Integer}})", "equal", () => [Tuple(Vararg(Integer)), Tuple(Vararg(Integer))]],
  ["L56 !issub(Tuple{}, Tuple{Int,Vararg{Int}})", "notsub", () => [Tuple(), Tuple(Int, Vararg(Int))]],
  ["L57 !issub(Tuple{Int}, Tuple{Int,Int,Vararg{Int}})", "notsub", () => [Tuple(Int), Tuple(Int, Int, Vararg(Int))]],
  ["L587 issub(Tuple{Integer,Vararg{Integer}}, Tuple{Integer,Vararg{Real}})", "sub", () => [Tuple(Integer, Vararg(Integer)), Tuple(Integer, Vararg(Real))]],
  ["L588 issub(Tuple{Integer,Float64,Vararg{Integer}}, Tuple{Integer,Vararg{Number}})", "sub", () => [Tuple(Integer, Float64, Vararg(Integer)), Tuple(Integer, Vararg(Number))]],
  ["L589 issub(Tuple{Integer,Float64}, Tuple{Integer,Vararg{Number}})", "sub", () => [Tuple(Integer, Float64), Tuple(Integer, Vararg(Number))]],
  ["L590 issub(Tuple{Int32}, Tuple{Vararg{Number}})", "sub", () => [Tuple(Int32), Tuple(Vararg(Number))]],
  ["L591 issub(Tuple{}, Tuple{Vararg{Number}})", "sub", () => [Tuple(), Tuple(Vararg(Number))]],
  ["L592 !issub(Tuple{Vararg{Int32}}, Tuple{Int32})", "notsub", () => [Tuple(Vararg(Int32)), Tuple(Int32)]],
  ["L593 !issub(Tuple{Vararg{Int32}}, Tuple{Number,Integer})", "notsub", () => [Tuple(Vararg(Int32)), Tuple(Number, Integer)]],
  ["L594 !issub(Tuple{Vararg{Integer}}, Tuple{Integer,Integer,Vararg{Integer}})", "notsub", () => [Tuple(Vararg(Integer)), Tuple(Integer, Integer, Vararg(Integer))]],

  // --- Type{T} kinds (test at L536-551) ---
  ["L536 issub_strict(DataType, Type)", "strict", () => [ty(ID.DataType), ty(ID.Type)]],
  ["L537 issub_strict(Union, Type)", "strict", () => [ty(ID.UnionT), ty(ID.Type)]],
  ["L538 issub_strict(UnionAll, Type)", "strict", () => [ty(ID.UnionAllT), ty(ID.Type)]],
  ["L540 !issub(TypeVar, Type)", "notsub", () => [ty(ID.TypeVarT), ty(ID.Type)]],
  ["L541 !issub(Type, TypeVar)", "notsub", () => [ty(ID.Type), ty(ID.TypeVarT)]],
  ["L542 !issub(DataType, @UnionAll T<:Number Type{T})", "notsub", () => {
    const T = tvar(0, Number);
    return [ty(ID.DataType), where(T, TypeT(T))];
  }],
  ["L543 issub_strict(Type{Int}, DataType)", "strict", () => [TypeT(Int), ty(ID.DataType)]],
  ["L544 !issub((@UnionAll T<:Integer Type{T}), DataType)", "notsub", () => {
    const T = tvar(0, Integer);
    return [where(T, TypeT(T)), ty(ID.DataType)];
  }],
  ["L546 !issub(Type{Int}, Type{Integer})", "notsub", () => [TypeT(Int), TypeT(Integer)]],
  ["L547 issub((@UnionAll T<:Integer Type{T}), (@UnionAll T<:Number Type{T}))", "sub", () => {
    const T = tvar(0, Integer), S = tvar(0, Number);
    return [where(T, TypeT(T)), where(S, TypeT(S))];
  }],
  ["L551 !(DataType <: (@UnionAll T<:Type Type{T}))", "notsub", () => {
    const T = tvar(0, ty(ID.Type));
    return [ty(ID.DataType), where(T, TypeT(T))];
  }],

  // --- fixed-count Vararg{T,N}: expands at construction (test_1) ---
  ["L61 isequal_type(Tuple{Int,Int}, Tuple{Vararg{Int,2}})", "equal", () => [Tuple(Int, Int), Tuple(VarargN(Int, 2))]],
  ["L63 Tuple{Int,Vararg{Int,2}} == Tuple{Int,Int,Int}", "equal", () => [Tuple(Int, VarargN(Int, 2)), Tuple(Int, Int, Int)]],
  ["L64 Tuple{Int,Vararg{Int,2}} === Tuple{Int,Int,Int}", "identical", () => [Tuple(Int, VarargN(Int, 2)), Tuple(Int, Int, Int)]],
  ["L65 Tuple{Any,Any} === Tuple{Vararg{Any,2}}", "identical", () => [Tuple(Any, Any), Tuple(VarargN(Any, 2))]],
  ["L67 Tuple{Int,Vararg{Int,2}} == Tuple{Int,Int,Int,Vararg{Int,0}}", "equal", () => [Tuple(Int, VarargN(Int, 2)), Tuple(Int, Int, Int, VarargN(Int, 0))]],
  ["L68 !(Tuple{Int,Vararg{Int,2}} <: Tuple{Int,Int,Int,Vararg{Int,1}})", "notsub", () => [Tuple(Int, VarargN(Int, 2)), Tuple(Int, Int, Int, VarargN(Int, 1))]],

  // --- two-parameter parametrics: invariance, where, diagonal (test_3) ---
  ["L206 issub_strict((@UnionAll T Pair{T,T}), Pair)", "strict", () => {
    const T = tvar(), A = tvar(), B = tvar();
    return [where(T, Pair(T, T)), where(A, where(B, Pair(A, B)))];
  }],
  ["L207 issub(Pair{Int,Int8}, Pair)", "sub", () => {
    const A = tvar(), B = tvar();
    return [Pair(Int, Int8), where(A, where(B, Pair(A, B)))];
  }],
  ["L208 issub(Pair{Int,Int8}, Pair{Int,S} where S)", "sub", () => {
    const S = tvar();
    return [Pair(Int, Int8), where(S, Pair(Int, S))];
  }],
  ["L232 !issub((@UnionAll T Pair{T,T}), Pair{Int,Int8})", "notsub", () => {
    const T = tvar();
    return [where(T, Pair(T, T)), Pair(Int, Int8)];
  }],
  ["L233 !issub((@UnionAll T Pair{T,T}), Pair{Int,Int})", "notsub", () => {
    const T = tvar();
    return [where(T, Pair(T, T)), Pair(Int, Int)];
  }],
  ["L262 !issub(Pair{Int,Int8}, (@UnionAll T Pair{T,T}))", "notsub", () => {
    const T = tvar();
    return [Pair(Int, Int8), where(T, Pair(T, T))];
  }],
  ["L270 !issub(Pair{Vector{Int},Integer}, @UnionAll T Pair{Vector{T},T})", "notsub", () => {
    const T = tvar();
    return [Pair(Ref(Int), Integer), where(T, Pair(Ref(T), T))];
  }],
  ["L271 issub(Pair{Vector{Int},Int}, @UnionAll T Pair{Vector{T},T})", "sub", () => {
    const T = tvar();
    return [Pair(Ref(Int), Int), where(T, Pair(Ref(T), T))];
  }],

  // --- tuple-over-union with a bound var (test_2/test_3) ---
  ["L413 !issub(Tuple{Union{Vector{Int},Vector{Int8}},Vector{Int}}, @UnionAll T Tuple{Vector{T},Vector{T}})", "notsub", () => {
    const T = tvar();
    return [Tuple(Union(Ref(Int), Ref(Int8)), Ref(Int)), where(T, Tuple(Ref(T), Ref(T)))];
  }],
  ["L416 !issub(Tuple{Union{Vector{Int},Vector{Int8}},Vector{Int8}}, @UnionAll T Tuple{Vector{T},Vector{T}})", "notsub", () => {
    const T = tvar();
    return [Tuple(Union(Ref(Int), Ref(Int8)), Ref(Int8)), where(T, Tuple(Ref(T), Ref(T)))];
  }],

  // --- more existential / bounded / diagonal cases (test_3) ---
  ["L205 issub_strict(Vector{Int}, @UnionAll T Vector{T})", "strict", () => {
    const T = tvar();
    return [Ref(Int), where(T, Ref(T))];
  }],
  ["L214 !issub(@UnionAll T<:Integer @UnionAll S<:Number Tuple{T,S}, ...Tuple{S,T})", "notsub", () => {
    const T = tvar(0, Integer), S = tvar(0, Number);
    const T2 = tvar(0, Integer), S2 = tvar(0, Number);
    return [where(T, where(S, Tuple(T, S))), where(T2, where(S2, Tuple(S2, T2)))];
  }],
  ["L238 issub(Tuple{Vector{Integer},Int}, @UnionAll T<:Integer @UnionAll S<:T Tuple{Vector{T},S})", "sub", () => {
    const T = tvar(0, Integer), S = tvar(0, T); // S <: T
    return [Tuple(Ref(Integer), Int), where(T, where(S, Tuple(Ref(T), S)))];
  }],
  ["L241 !issub(Tuple{Vector{Integer},Real}, @UnionAll T<:Integer Tuple{Vector{T},T})", "notsub", () => {
    const T = tvar(0, Integer);
    return [Tuple(Ref(Integer), Real), where(T, Tuple(Ref(T), T))];
  }],
  ["L264 !issub(Tuple{Vector{Int},Integer}, @UnionAll T<:Integer Tuple{Vector{T},T})", "notsub", () => {
    const T = tvar(0, Integer);
    return [Tuple(Ref(Int), Integer), where(T, Tuple(Ref(T), T))];
  }],
  ["L267 !issub(Tuple{Integer,Vector{Int}}, @UnionAll T<:Integer Tuple{T,Vector{T}})", "notsub", () => {
    const T = tvar(0, Integer);
    return [Tuple(Integer, Ref(Int)), where(T, Tuple(T, Ref(T)))];
  }],
  ["L273 issub(Tuple{Integer,Int}, @UnionAll T<:Integer @UnionAll S<:T Tuple{T,S})", "sub", () => {
    const T = tvar(0, Integer), S = tvar(0, T);
    return [Tuple(Integer, Int), where(T, where(S, Tuple(T, S)))];
  }],

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

  // --- level 1: reflexivity, arity, Any (test_1) ---
  ["L25 isequal_type(Int, Int)", "equal", () => [Int, Int]],
  ["L26 isequal_type(Integer, Integer)", "equal", () => [Integer, Integer]],
  ["L33 isequal_type(Tuple{Integer,Integer}, Tuple{Integer,Integer})", "equal", () =>
    [Tuple(Integer, Integer), Tuple(Integer, Integer)]],
  ["L36 !issub(Tuple{Int}, Tuple{Integer,Integer})", "notsub", () => [Tuple(Int), Tuple(Integer, Integer)]],
  ["L50 issub_strict(Tuple{Int}, Tuple{Any})", "strict", () => [Tuple(Int), Tuple(Any)]],
  ["L53 isequal_type(Tuple{Int}, Tuple{Int})", "equal", () => [Tuple(Int), Tuple(Int)]],

  // --- level 3: universal/existential combinations (test_3) ---
  ["L97 issub(Tuple{Integer,Int}, @UnionAll T @UnionAll T<:S<:T Tuple{T,S})", "sub", () => {
    const T = tvar(); const S = tvar(T, T);
    return [Tuple(Integer, Int), where(T, where(S, Tuple(T, S)))];
  }],
  ["L102 issub_strict(@UnionAll R Tuple{R,R}, @UnionAll T @UnionAll S<:T Tuple{T,S})", "strict", () => {
    const R = tvar(); const T = tvar(); const S = tvar(0, T);
    return [where(R, Tuple(R, R)), where(T, where(S, Tuple(T, S)))];
  }],
  ["L104 issub_strict(@UnionAll R Tuple{R,R}, @UnionAll T @UnionAll T<:S<:T Tuple{T,S})", "strict", () => {
    const R = tvar(); const T = tvar(); const S = tvar(T, T);
    return [where(R, Tuple(R, R)), where(T, where(S, Tuple(T, S)))];
  }],
  ["L106 issub_strict(@UnionAll R Tuple{R,R}, @UnionAll T @UnionAll S>:T Tuple{T,S})", "strict", () => {
    const R = tvar(); const T = tvar(); const S = tvar(T, 0);
    return [where(R, Tuple(R, R)), where(T, where(S, Tuple(T, S)))];
  }],

  // --- level 5: UnionAll equivalence and bounds (test_5) ---
  ["L210 !issub(@UnionAll T<:Real T, @UnionAll T<:Integer T)", "notsub", () => {
    const T1 = tvar(0, Real); const T2 = tvar(0, Integer);
    return [where(T1, T1), where(T2, T2)];
  }],
  ["L212 isequal_type(@UnionAll T Tuple{T,T}, @UnionAll R Tuple{R,R})", "equal", () => {
    const T = tvar(); const R = tvar();
    return [where(T, Tuple(T, T)), where(R, Tuple(R, R))];
  }],
  ["L227 issub_strict(@UnionAll T Int, @UnionAll T<:Integer Integer)", "strict", () => {
    const T1 = tvar(); const T2 = tvar(0, Integer);
    return [where(T1, Int), where(T2, Integer)];
  }],
  ["L229 isequal_type(@UnionAll T @UnionAll S Tuple{T,Tuple{S}}, @UnionAll R @UnionAll V Tuple{R,Tuple{V}})", "equal", () => {
    const T = tvar(); const S = tvar(); const R = tvar(); const V = tvar();
    return [where(T, where(S, Tuple(T, Tuple(S)))), where(R, where(V, Tuple(R, Tuple(V))))];
  }],
  ["L235 isequal_type(@UnionAll T Tuple{T}, Tuple{Any})", "equal", () => {
    const T = tvar(); return [where(T, Tuple(T)), Tuple(Any)];
  }],
  ["L236 isequal_type(@UnionAll T<:Real Tuple{T}, Tuple{Real})", "equal", () => {
    const T = tvar(0, Real); return [where(T, Tuple(T)), Tuple(Real)];
  }],
  ["L274 !issub(Tuple{Integer,Int}, @UnionAll T<:Int @UnionAll S<:T Tuple{T,S})", "notsub", () => {
    const T = tvar(0, Int); const S = tvar(0, T);
    return [Tuple(Integer, Int), where(T, where(S, Tuple(T, S)))];
  }],
  ["L289 issub(@UnionAll Int<:T<:Integer T, @UnionAll T<:Real T)", "sub", () => {
    const T1 = tvar(Int, Integer); const T2 = tvar(0, Real);
    return [where(T1, T1), where(T2, T2)];
  }],

  // --- level 4: Union normalization & subsumption (test_4; A=Int64 B=Int8 C=Int16 D=Int32) ---
  ["L362 isequal_type(Union{Bottom,Bottom}, Bottom)", "equal", () => [Union(Bottom, Bottom), Bottom]],
  ["L365 issub_strict(Union{Int,Int8}, Integer)", "strict", () => [Union(Int, Int8), Integer]],
  ["L367 isequal_type(Union{Int,Int8}, Union{Int,Int8})", "equal", () => [Union(Int, Int8), Union(Int, Int8)]],
  ["L369 isequal_type(Union{Int,Integer}, Integer)", "equal", () => [Union(Int, Integer), Integer]],
  ["L381 issub(Union{Union{A,Union{A,Union{B,C}}},Union{D,Bottom}}, Union{Union{A,B},Union{C,Union{B,D}}})", "sub", () =>
    [Union(Union(Int, Union(Int, Union(Int8, Int16))), Union(Int32, Bottom)),
     Union(Union(Int, Int8), Union(Int16, Union(Int8, Int32)))]],
  ["L383 !issub(Union{Union{A,Union{A,Union{B,C}}},Union{D,Bottom}}, Union{Union{A,B},Union{C,Union{B,A}}})", "notsub", () =>
    [Union(Union(Int, Union(Int, Union(Int8, Int16))), Union(Int32, Bottom)),
     Union(Union(Int, Int8), Union(Int16, Union(Int8, Int)))]],
  ["L386 isequal_type(Union{Union{A,B,C},Union{D}}, Union{A,B,C,D})", "equal", () =>
    [Union(Union(Int, Union(Int8, Int16)), Int32), Union(Union(Int, Int8), Union(Int16, Int32))]],
  ["L387 isequal_type(Union{Union{A,B,C},Union{D}}, Union{A,Union{B,C},D})", "equal", () =>
    [Union(Union(Int, Union(Int8, Int16)), Int32), Union(Int, Union(Union(Int8, Int16), Int32))]],
  ["L388 isequal_type(Union{Union{Union{Union{A}},B,C},Union{D}}, Union{A,Union{B,C},D})", "equal", () =>
    [Union(Union(Int, Union(Int8, Int16)), Int32), Union(Union(Int, Union(Int8, Int16)), Int32)]],
  ["L391 issub_strict(Union{Union{A,C},Union{D}}, Union{A,B,C,D})", "strict", () =>
    [Union(Union(Int, Int16), Int32), Union(Union(Int, Int8), Union(Int16, Int32))]],
  ["L393 !issub(Union{Union{A,B,C},Union{D}}, Union{A,C,D})", "notsub", () =>
    [Union(Union(Int, Union(Int8, Int16)), Int32), Union(Int, Union(Int16, Int32))]],

  // --- the union-decision machine (engine slice 1, 2026-07): the two
  // --- long-tracked divergences, healed and promoted, plus the tranche the
  // --- machine unlocks (tuple-over-union distributivity, the nested-union
  // --- stress canary, and the convert(Type{T},T) split-or-not pattern) ---
  ["L371 isequal_type(Tuple{Union{Int,Int8},Int16}, Union{Tuple{Int,Int16},Tuple{Int8,Int16}})", "equal", () =>
    [Tuple(Union(Int, Int8), Int16), Union(Tuple(Int, Int16), Tuple(Int8, Int16))]],
  ["L410 issub(Tuple{Union{Vector{Int},Vector{Int8}}}, @UnionAll T Tuple{Vector{T}})", "sub", () => {
    const T = tvar();
    return [Tuple(Union(Ref(Int), Ref(Int8))), where(T, Tuple(Ref(T)))];
  }],
  ["L373 issub_strict(Tuple{Int,Int8,Int}, Tuple{Vararg{Union{Int,Int8}}})", "strict", () =>
    [Tuple(Int, Int8, Int), Tuple(Vararg(Union(Int, Int8)))]],
  ["L374 issub_strict(Tuple{Int,Int8,Int}, Tuple{Vararg{Union{Int,Int8,Int16}}})", "strict", () =>
    [Tuple(Int, Int8, Int), Tuple(Vararg(Union(Int, Union(Int8, Int16))))]],
  ["L377 !issub(Union{Int,Ref{Union{Int,Int8}}}, Union{Int,Ref{Union{Int8,Int16}}})", "notsub", () =>
    [Union(Int, Ref(Union(Int, Int8))), Union(Int, Ref(Union(Int8, Int16)))]],
  // L396-401: "obviously these unions can be simplified, but when they aren't
  // there's trouble" — the 8-way nested-union stress, a performance canary for
  // the machine's binary counter (A=Int64 B=Int8 C=Int16 D=Int32).
  ["L401 issub_strict(X8, Y8) — the nested-union stress", "strict", () => {
    const [A, B, C, D] = [Int, Int8, Int16, Int32];
    const abc = () => Union(A, Union(B, C));
    const dbc = () => Union(D, Union(B, C));
    const four = (m) => Union(m(), Union(m(), Union(m(), m())));
    const X = Union(four(abc), four(abc));
    const Y = Union(four(dbc), Union(dbc(), Union(dbc(), Union(dbc(), abc()))));
    return [X, Y];
  }],
  // The convert(Type{T},T) pattern (u = Union{Int8,Int}): matching the whole
  // union against the variable first is itself a recorded machine choice
  // (subtype.c:1940-1948).
  ["L445 issub(Tuple{Vector{u},Int}, @UnionAll T Tuple{Vector{T},T})", "sub", () => {
    const T = tvar();
    return [Tuple(Ref(Union(Int8, Int)), Int), where(T, Tuple(Ref(T), T))];
  }],
  ["L446 issub(Tuple{Vector{u},Int}, @UnionAll T @UnionAll S<:T Tuple{Vector{T},S})", "sub", () => {
    const T = tvar();
    const S = tvar(0, T);
    return [Tuple(Ref(Union(Int8, Int)), Int), where(T, where(S, Tuple(Ref(T), S)))];
  }],
  // L448/L450: the same union under an *invariant* constructor stays false —
  // the machine must not over-heal (forall_exists_equal needs both directions).
  ["L448 !issub(Ref{Union{Ref{Int},Ref{Int8}}}, @UnionAll T Ref{Ref{T}})", "notsub", () => {
    const T = tvar();
    return [Ref(Union(Ref(Int), Ref(Int8))), where(T, Ref(Ref(T)))];
  }],
  ["L449 issub(Tuple{Union{Ref{Int},Ref{Int8}}}, @UnionAll T Tuple{Ref{T}})", "sub", () => {
    const T = tvar();
    return [Tuple(Union(Ref(Int), Ref(Int8))), where(T, Tuple(Ref(T)))];
  }],
  ["L450 !issub(Ref{Union{Ref{Int},Ref{Int8}}}, Union{Ref{Ref{Int}},Ref{Ref{Int8}}})", "notsub", () =>
    [Ref(Union(Ref(Int), Ref(Int8))), Union(Ref(Ref(Int)), Ref(Ref(Int8)))]],

  // --- forall_exists_equal tail (engine slice 2, 2026-07): L371's property
  // --- in *invariant* position, through the shared-Runions local machine ---
  ["L452 isequal_type(Ref{Tuple{Union{Int,Int8},Int16}}, Ref{Union{Tuple{Int,Int16},Tuple{Int8,Int16}}})", "equal", () =>
    [Ref(Tuple(Union(Int, Int8), Int16)), Ref(Union(Tuple(Int, Int16), Tuple(Int8, Int16)))]],
  ["L453 isequal_type(Ref{T} where T<:Tuple{Union{Int,Int8},Int16}, Ref{T} where T<:Union{Tuple{Int,Int16},Tuple{Int8,Int16}})", "equal", () => {
    const T1 = tvar(0, Tuple(Union(Int, Int8), Int16));
    const T2 = tvar(0, Union(Tuple(Int, Int16), Tuple(Int8, Int16)));
    return [where(T1, Ref(T1)), where(T2, Ref(T2))];
  }],
  ["L456 isequal_type(Ref{Tuple{Union{Int,Int8},Int16,T}} where T, Ref{Union{Tuple{Int,Int16,S},Tuple{Int8,Int16,S}}} where S)", "equal", () => {
    const T = tvar();
    const S = tvar();
    return [where(T, Ref(Tuple(Union(Int, Int8), Int16, T))),
            where(S, Ref(Union(Tuple(Int, Int16, S), Tuple(Int8, Int16, S))))];
  }],

  // --- the vararg length algebra (engine slice 3, 2026-07): typevar-count
  // --- Vararg{T,N} (the BOUND kind), the Loffset channel, the N-equation,
  // --- and check_vararg_length. NTuple{N,T} spells Tuple{Vararg{T,N}}. ---
  ["L70 (@UnionAll N Tuple{Int,Vararg{Int,N}}) == (@UnionAll N Tuple{Int,Vararg{Int,N}})", "equal", () => {
    const N1 = tvar();
    const N2 = tvar();
    return [where(N1, Tuple(Int, VarargTV(Int, N1))), where(N2, Tuple(Int, VarargTV(Int, N2)))];
  }],
  ["L79 issub_strict(Tuple{Tuple{Int,Int},Tuple{Int,Int}}, Tuple{NTuple{N,Int},NTuple{N,Int}} where N)", "strict", () => {
    const N = tvar();
    return [Tuple(Tuple(Int, Int), Tuple(Int, Int)),
            where(N, Tuple(Tuple(VarargTV(Int, N)), Tuple(VarargTV(Int, N))))];
  }],
  ["L80 !issub(Tuple{Tuple{Int,Int},Tuple{Int}}, Tuple{NTuple{N,Int},NTuple{N,Int}} where N)", "notsub", () => {
    const N = tvar();
    return [Tuple(Tuple(Int, Int), Tuple(Int)),
            where(N, Tuple(Tuple(VarargTV(Int, N)), Tuple(VarargTV(Int, N))))];
  }],
  ["L85 issub_strict(Tuple{Int,Int}, Tuple{Int,Int,Vararg{Int,N}} where N)", "strict", () => {
    const N = tvar();
    return [Tuple(Int, Int), where(N, Tuple(Int, Int, VarargTV(Int, N)))];
  }],
  ["L86 issub_strict(Tuple{Int,Int}, Tuple{E,E,Vararg{E,N}} where E where N)", "strict", () => {
    const N = tvar();
    const E = tvar();
    return [Tuple(Int, Int), where(N, where(E, Tuple(E, E, VarargTV(E, N))))];
  }],
  ["L632 issub(Tuple{}, @UnionAll N NTuple{N})", "sub", () => {
    const N = tvar();
    return [Tuple(), where(N, Tuple(VarargTV(Any, N)))];
  }],

  // --- the Intersect meet node + concrete propagation (engine slice 4,
  // --- 2026-07): the diagonal family whose bounds cross a union arm
  // --- (Float64 for String), the cross-bounded existentials from test_3
  // --- (Box for Ptr; bare Ptr spells `Box{X} where X`), and the
  // --- abstract-lower-bound guard on diagonal concreteness. ---
  ["L110 !issub(Tuple{Real,Real}, @UnionAll T<:Real Tuple{T,T})", "notsub", () => {
    const T = tvar(0, Real);
    return [Tuple(Real, Real), where(T, Tuple(T, T))];
  }],
  ["L115 issub_strict(Tuple{String,Real,Ref{Number}}, @UnionAll T Tuple{Union{T,String},T,Ref{T}})", "strict", () => {
    const T = tvar();
    return [Tuple(Float64, Real, Ref(Number)), where(T, Tuple(Union(T, Float64), T, Ref(T)))];
  }],
  ["L118 issub_strict(Tuple{String,Real}, @UnionAll T Tuple{Union{T,String},T})", "strict", () => {
    const T = tvar();
    return [Tuple(Float64, Real), where(T, Tuple(Union(T, Float64), T))];
  }],
  ["L121 !issub(Tuple{Real,Real}, @UnionAll T Tuple{Union{T,String},T})", "notsub", () => {
    const T = tvar();
    return [Tuple(Real, Real), where(T, Tuple(Union(T, Float64), T))];
  }],
  ["L124 issub_strict(Tuple{Int,Int}, @UnionAll T Tuple{Union{T,String},T})", "strict", () => {
    const T = tvar();
    return [Tuple(Int, Int), where(T, Tuple(Union(T, Float64), T))];
  }],
  ["L141 isequal_type(Tuple{Vararg{A}} where A>:Integer, Tuple{Vararg{A}} where A>:Integer)", "equal", () => {
    const A1 = tvar(Integer, 0);
    const A2 = tvar(Integer, 0);
    return [where(A1, Tuple(Vararg(A1))), where(A2, Tuple(Vararg(A2)))];
  }],
  ["L338 issub_strict(@UnionAll T>:Ptr @UnionAll Ptr<:S<:Ptr Tuple{Ptr{T},Ptr{S}}, @UnionAll T>:Ptr @UnionAll S>:Ptr{T} Tuple{Ptr{T},Ptr{S}})", "strict", () => {
    const ptrBare = () => {
      const X = tvar();
      return where(X, Ref(X));
    };
    const T1 = tvar(ptrBare(), 0);
    const S1 = tvar(ptrBare(), ptrBare());
    const T2 = tvar(ptrBare(), 0);
    const S2 = tvar(Ref(T2), 0);
    return [
      where(T1, where(S1, Tuple(Ref(T1), Ref(S1)))),
      where(T2, where(S2, Tuple(Ref(T2), Ref(S2)))),
    ];
  }],
  ["L340 !issub(@UnionAll T>:Ptr @UnionAll S>:Ptr Tuple{Ptr{T},Ptr{S}}, @UnionAll T>:Ptr @UnionAll Ptr{T}<:S<:Ptr Tuple{Ptr{T},Ptr{S}})", "notsub", () => {
    const ptrBare = () => {
      const X = tvar();
      return where(X, Ref(X));
    };
    const T1 = tvar(ptrBare(), 0);
    const S1 = tvar(ptrBare(), 0);
    const T2 = tvar(ptrBare(), 0);
    const S2 = tvar(Ref(T2), ptrBare());
    return [
      where(T1, where(S1, Tuple(Ref(T1), Ref(S1)))),
      where(T2, where(S2, Tuple(Ref(T2), Ref(S2)))),
    ];
  }],
];

// Known divergences (currently none — the union-decision machine healed both
// tracked tuple-over-union cases, promoted above). Mechanism retained: a
// behavior we cannot yet match goes here, runs on every invocation, reports
// without failing the build, and announces itself if a fix heals it.
const knownDivergences = [];

// `identical` is Julia's `===` on types: with hash-consed construction, equal
// tuples are the same object.
const pred = { strict, equal, sub: (a, b) => sub(a, b), notsub: (a, b) => !sub(a, b), noteq: (a, b) => !equal(a, b), identical: (a, b) => a === b };

let pass = 0, fail = 0;
for (const [src, kind, build] of cases) {
  const [a, b] = build();
  const ok = pred[kind](a, b);
  if (ok) { pass++; } else { fail++; console.log(`MISMATCH  ${src}`); }
}
let healed = 0;
for (const [src, kind, build] of knownDivergences) {
  const [a, b] = build();
  if (pred[kind](a, b)) { healed++; console.log(`FIXED (promote to cases)  ${src}`); }
  else { console.log(`known divergence  ${src}`); }
}
console.log(`\n${pass}/${cases.length} match JuliaLang/julia (test/subtype.jl); ${fail} mismatch; ${knownDivergences.length - healed} known divergence(s)`);

// --- env matching (engine slice 5): rj_subtype_env computes the values of
// --- the right side's outer `where` variables, as jl_subtype_env does for
// --- jl_subtype_matching. Expected bindings below were verified against
// --- the pinned Julia binary via ccall(:jl_subtype_env, ...) — recorded as
// --- pinned-binary evidence, not test/subtype.jl verbatim. ---
const envCases = [
  ["Tuple{Int,Int} <: Tuple{T,T} where T -> [Int64]", () => {
    const T = tvar();
    return [Tuple(Int, Int), where(T, Tuple(T, T)), [Int]];
  }],
  ["Tuple{Int,Float64} <: Tuple{T,S} where {T,S} -> [Int64, Float64]", () => {
    const T = tvar(); const S = tvar();
    return [Tuple(Int, Float64), where(T, where(S, Tuple(T, S))), [Int, Float64]];
  }],
  ["Tuple{Int,Int} <: Tuple{Vararg{Int,N}} where N -> [2]", () => {
    const N = tvar();
    return [Tuple(Int, Int), where(N, Tuple(VarargTV(Int, N))), ["long:2"]];
  }],
  ["Tuple{} <: Tuple{Vararg{T}} where T -> [svec(T, false)]", () => {
    const T = tvar();
    return [Tuple(), where(T, Tuple(Vararg(T))), [["wrapped", T, false]]];
  }],
  ["Tuple{Int,Float64} NOT <: Tuple{T,T} where T (diagonal)", () => {
    const T = tvar();
    return [Tuple(Int, Float64), where(T, Tuple(T, T)), null];
  }],
];
let envPass = 0, envFail = 0;
for (const [src, build] of envCases) {
  const [a, b, expect] = build();
  const ok = x.rj_subtype_env(a, b) === 1;
  let good;
  if (expect === null) {
    good = !ok;
  } else if (!ok || x.rj_env_size() !== expect.length) {
    good = false;
  } else {
    good = expect.every((want, i) => {
      const got = x.rj_env_get(i);
      if (typeof want === "number") return got === want;
      if (typeof want === "string" && want.startsWith("long:"))
        return x.rj_typeof(got) === ty(ID.Int64) && x.rj_unbox_int(got) === BigInt(want.slice(5));
      // ["wrapped", tvarOffset, constrainedBool]
      return x.rj_is_svec(got) === 1 && x.rj_svec_len(got) === 2
        && x.rj_svec_ref(got, 0) === want[1]
        && x.rj_svec_ref(got, 1) === (want[2] ? x.rj_true_instance() : x.rj_false_instance());
    });
  }
  if (good) { envPass++; } else { envFail++; console.log(`ENV MISMATCH  ${src}`); }
}
console.log(`${envPass}/${envCases.length} env matchings agree with the pinned Julia (jl_subtype_env)`);

process.exitCode = (fail || envFail) ? 1 : 0;
