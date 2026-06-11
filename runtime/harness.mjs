// Minimal JS host for the Ruju runtime skeleton.
//
// Loads the compiled .wasm module and calls into the `rj_`-prefixed C ABI,
// proving the Rust runtime -> WebAssembly -> JavaScript path end to end.
//
//   node runtime/harness.mjs
//
// (build first: cargo build -p ruju-runtime --target wasm32-unknown-unknown --release)

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const wasmPath = resolve(
  here,
  "..",
  "target",
  "wasm32-unknown-unknown",
  "release",
  "ruju_runtime.wasm",
);

const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const x = instance.exports;

function check(label, got, want) {
  const ok = got === want;
  console.log(`${ok ? "ok  " : "FAIL"} ${label}: ${got}${ok ? "" : ` (expected ${want})`}`);
  if (!ok) process.exitCode = 1;
}

// Builtin type ids (must match runtime/src/types.rs `id`).
const T = {
  Any: 0, Number: 1, Real: 2, Integer: 3, Signed: 4, Unsigned: 5,
  AbstractFloat: 6, AbstractChar: 7, Bool: 8, Int8: 9, Int64: 12,
  UInt8: 14, Float64: 21, Char: 22, Symbol: 23, Nothing: 24, DataType: 25,
  Bottom: 26,
};
const ty = (id) => x.rj_builtin_type(id);

x.rj_init();

// --- values and the integer-add intrinsic (i64 crosses as BigInt) ---
check("rj_demo_add(2, 3)", x.rj_demo_add(2n, 3n), 5n);
check("rj_demo_add(i64::MAX, 1)", x.rj_demo_add(9223372036854775807n, 1n), -9223372036854775808n);

// --- object model: tags and typeof ---
check("typeof(DataType) === DataType", x.rj_typeof(ty(T.DataType)), ty(T.DataType));
check("typeof(nothing) === Nothing", x.rj_typeof(x.rj_nothing()), ty(T.Nothing));

// --- nominal subtyping over the hierarchy ---
check("Int64 <: Signed", x.rj_subtype(ty(T.Int64), ty(T.Signed)), 1);
check("Int64 <: Number", x.rj_subtype(ty(T.Int64), ty(T.Number)), 1);
check("Float64 <: AbstractFloat", x.rj_subtype(ty(T.Float64), ty(T.AbstractFloat)), 1);
check("Bool <: Integer", x.rj_subtype(ty(T.Bool), ty(T.Integer)), 1);
check("Int64 NOT <: Float64", x.rj_subtype(ty(T.Int64), ty(T.Float64)), 0);
check("supertype(Int64) === Signed", x.rj_supertype(ty(T.Int64)), ty(T.Signed));

// --- primitive sizes (julia.h) ---
check("sizeof(Int8)", x.rj_datatype_size(ty(T.Int8)), 1);
check("sizeof(Float64)", x.rj_datatype_size(ty(T.Float64)), 8);
check("sizeof(Char)", x.rj_datatype_size(ty(T.Char)), 4);

// --- type names are real interned Symbols ---
check("name(Int64) is Symbol of len 5", x.rj_symbol_len(x.rj_type_name(ty(T.Int64))), 5);
check("typeof(name(Int64)) === Symbol", x.rj_typeof(x.rj_type_name(ty(T.Int64))), ty(T.Symbol));

// --- parametric subtyping: tuples (covariant), unions, Bottom ---
const tupII = x.rj_tuple_type2(ty(T.Int64), ty(T.Int64));
const tupIR = x.rj_tuple_type2(ty(T.Integer), ty(T.Real));
check("Tuple{Int,Int} <: Tuple{Integer,Real}", x.rj_subtype(tupII, tupIR), 1);
const tupFI = x.rj_tuple_type2(ty(T.Float64), ty(T.Int64));
check("Tuple{Float64,Int} NOT <: Tuple{Integer,Real}", x.rj_subtype(tupFI, tupIR), 0);
const u = x.rj_union_type(ty(T.Int64), ty(T.Float64));
check("Int64 <: Union{Int64,Float64}", x.rj_subtype(ty(T.Int64), u), 1);
check("Union{Int64,Float64} <: Real", x.rj_subtype(u, ty(T.Real)), 1);
check("Union{Int64,Char} NOT <: Real", x.rj_subtype(x.rj_union_type(ty(T.Int64), ty(T.Char)), ty(T.Real)), 0);
check("Bottom <: Int64", x.rj_subtype(ty(T.Bottom), ty(T.Int64)), 1);
check("Int64 NOT <: Bottom", x.rj_subtype(ty(T.Int64), ty(T.Bottom)), 0);
// Union normalization: a subsumed member is dropped (Union{Int,Real} == Real).
check("Union{Int64,Real} normalizes to Real", x.rj_union_type(ty(T.Int64), ty(T.Real)), ty(T.Real));
// Nested unions flatten and dedup: Union{Int, Union{Float64,Int}} stays 2-member.
const flatU = x.rj_union_type(ty(T.Int64), x.rj_union_type(ty(T.Float64), ty(T.Int64)));
check("nested union flattens to Union{Int,Float64}", x.rj_subtype(flatU, x.rj_union_type(ty(T.Int64), ty(T.Float64))), 1);
check("...and back", x.rj_subtype(x.rj_union_type(ty(T.Int64), ty(T.Float64)), flatU), 1);

// --- type uniquing: structurally identical types are the same object ---
check(
  "Tuple{Int64,Int64} is uniqued (===)",
  x.rj_tuple_type2(ty(T.Int64), ty(T.Int64)) === x.rj_tuple_type2(ty(T.Int64), ty(T.Int64)),
  true,
);
check(
  "distinct tuple types differ",
  x.rj_tuple_type2(ty(T.Int64), ty(T.Int64)) === x.rj_tuple_type2(ty(T.Int64), ty(T.Float64)),
  false,
);
// Invariant parametric types: Box{Int64} is NOT a subtype of Box{Integer}.
check("Box{Int64} === Box{Int64}", x.rj_box_type(ty(T.Int64)) === x.rj_box_type(ty(T.Int64)), true);
check("Box{Int64} NOT <: Box{Integer} (invariant)", x.rj_subtype(x.rj_box_type(ty(T.Int64)), x.rj_box_type(ty(T.Integer))), 0);
check("Box{Int64} <: Any", x.rj_subtype(x.rj_box_type(ty(T.Int64)), ty(T.Any)), 1);

// --- where types: UnionAll / TypeVar and the environment-based subtype ---
// `Box{T} where T` (unbounded): existential on the right, universal on the left.
const boxOf = (id) => x.rj_box_type(ty(id));
const whereBox = (lb, ub) => {
  const tv = x.rj_typevar(lb, ub); // 0,0 => Union{} <: T <: Any
  return x.rj_unionall(tv, x.rj_box_type(tv));
};
check("Box{Int} <: (Box{T} where T)  [exists]", x.rj_subtype(boxOf(T.Int64), whereBox(0, 0)), 1);
check("(Box{T} where T) NOT <: Box{Int}  [forall]", x.rj_subtype(whereBox(0, 0), boxOf(T.Int64)), 0);
check("(Box{T} where T) <: Any", x.rj_subtype(whereBox(0, 0), ty(T.Any)), 1);
// Bounded vars matched invariantly across two `where`s.
check(
  "(Box{T<:Integer}) <: (Box{S<:Number})",
  x.rj_subtype(whereBox(0, ty(T.Integer)), whereBox(0, ty(T.Number))),
  1,
);
check(
  "(Box{T<:Number}) NOT <: (Box{S<:Integer})",
  x.rj_subtype(whereBox(0, ty(T.Number)), whereBox(0, ty(T.Integer))),
  0,
);
// Diagonal rule: a covariant variable used twice is pinned to concrete types.
const diagWhere = () => {
  const tv = x.rj_typevar(0, 0);
  return x.rj_unionall(tv, x.rj_tuple_type2(tv, tv));
};
const tup = (a, b) => x.rj_tuple_type2(ty(a), ty(b));
check("Tuple{Int,Int} <: (Tuple{T,T} where T)  [diagonal]", x.rj_subtype(tup(T.Int64, T.Int64), diagWhere()), 1);
check("Tuple{Int,Float64} NOT <: (Tuple{T,T} where T)  [diagonal]", x.rj_subtype(tup(T.Int64, T.Float64), diagWhere()), 0);

// --- garbage collection (mark-sweep) ---
x.rj_gc_collect(); // clear any garbage from the checks above
const live0 = x.rj_live_objects();
x.rj_alloc_garbage(100);
check("live grew by 100 garbage", x.rj_live_objects(), live0 + 100);
const highWater = x.rj_heap_used();
check("collect reclaims the 100", x.rj_gc_collect(), 100);
check("live back to baseline", x.rj_live_objects(), live0);
x.rj_alloc_garbage(100);
check("freed chunks reused (no heap growth)", x.rj_heap_used(), highWater);
x.rj_gc_collect();
check("type graph survives collection", x.rj_typeof(ty(T.DataType)), ty(T.DataType));
check("subtyping survives collection", x.rj_subtype(ty(T.Int64), ty(T.Number)), 1);

// --- interpreter: lowered IR over the runtime, with GC churn ---
check("interp (2+3)*4", x.rj_interp_poly(2n, 3n, 4n), 20n);
check("interp sum(1:5)", x.rj_interp_sum_to(5n), 15n);
check("interp sum(1:100)", x.rj_interp_sum_to(100n), 5050n);
check("interp sum(1:0)", x.rj_interp_sum_to(0n), 0n);
check("interp countdown(7)", x.rj_interp_count_down(7n), 7n);
// A heavy interpreter run allocates a lot; collect afterward and re-run.
check("interp sum(1:200)", x.rj_interp_sum_to(200n), 20100n);
x.rj_gc_collect();
check("interp correct after collect", x.rj_interp_sum_to(50n), 1275n);
// Auto-collection: far more allocation than the region holds, reclaimed mid-run.
check("interp sum(1:50000) [auto-GC]", x.rj_interp_sum_to(50000n), 1250025000n);
x.rj_alloc_garbage(300000);
check("auto-GC keeps heap bounded", x.rj_live_objects() < 100000, true);

// --- multiple dispatch: most-specific selection, driven through the interpreter ---
check("dispatch classify(Int64) -> 20 (over Integer)", x.rj_call_classify_i64(7n), 20n);
check("dispatch classify(Bool) -> 30 (over Integer)", x.rj_call_classify_bool(), 30n);
check("dispatch double(21) -> 42 (uses argument)", x.rj_call_double(21n), 42n);
check("dispatch combine(Int64,Int64) -> 2", x.rj_call_combine(1n, 2n), 2n);

// --- real Julia source: text -> tokens -> IR -> result ---
// Write UTF-8 source into the runtime's buffer, then evaluate it.
function evalJulia(src) {
  const bytes = new TextEncoder().encode(src);
  new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  return x.rj_eval(bytes.length);
}
check("source: 1 + 2 * 3", evalJulia("1 + 2 * 3"), 7n);
check("source: (1 + 2) * 3", evalJulia("(1 + 2) * 3"), 9n);
check("source: if/else branch", evalJulia("x = 50\nif x < 10\ns = 1\nelse\ns = 2\nend\ns"), 2n);
check(
  "source: while sum(1:100)",
  evalJulia("acc = 0\ni = 1\nwhile i <= 100\nacc = acc + i\ni = i + 1\nend\nacc"),
  5050n,
);
// `===` (egal) from source; the Bool result reads back as 1/0.
check("source: 1 === 1", evalJulia("1 === 1"), 1n);
check("source: 1 === 2", evalJulia("1 === 2"), 0n);
check("source: x = 6 * 7; x === 42", evalJulia("x = 6 * 7\nx === 42"), 1n);
// egal across the ABI: type identity and structural union equality.
check("rj_egal(Int64, Int64)", x.rj_egal(ty(T.Int64), ty(T.Int64)), 1);
check("rj_egal(Int64, Signed)", x.rj_egal(ty(T.Int64), ty(T.Signed)), 0);
check(
  "rj_types_egal(Union{Int64,Nothing} x2, built separately)",
  x.rj_types_egal(x.rj_union_type(ty(T.Int64), ty(T.Nothing)), x.rj_union_type(ty(T.Nothing), ty(T.Int64))),
  1,
);
// Float64 source (result read as a double).
function evalJuliaF64(src) {
  const bytes = new TextEncoder().encode(src);
  new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  return x.rj_eval_f64(bytes.length);
}
check("source: 1.5 + 2.0", evalJuliaF64("1.5 + 2.0"), 3.5);
check("source: 2.0 * 3.0 + 1.0", evalJuliaF64("2.0 * 3.0 + 1.0"), 7.0);
check("source: float while-loop", evalJuliaF64("x = 0.0\nwhile x < 5.0\nx = x + 0.5\nend\nx"), 5.0);

// --- invariants ---
check("rj_root_count() balanced", x.rj_root_count(), 0);
console.log(`info heap high-water: ${x.rj_heap_used()} bytes, live objects: ${x.rj_live_objects()}`);

console.log(process.exitCode ? "runtime: FAILED" : "runtime: OK");
