// AOT thin-slice harness (issue #11; spec: design/research/research-aot-backend.md §7).
//
// Proves the full chain: typed IR (data) → ruju-aotc → wasm function →
// registered in dispatch → called through both the specsig export and the
// dispatch path → benchmarked against the go/no-go thresholds.
//
//   cargo build -p ruju-runtime --target wasm32-unknown-unknown --release
//   cargo run -p ruju-aotc -- aotc/fixtures/f_sumsq.json target/aot/f_sumsq.wasm
//   cargo run -p ruju-aotc -- aotc/fixtures/g_refloop.json target/aot/g_refloop.wasm
//   node runtime/harness_aot.mjs
//
// Benchmark size/repetitions: RUJU_AOT_BENCH_N (default 10^7),
// RUJU_AOT_BENCH_REPS (default 5).

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const runtimePath = resolve(root, "target", "wasm32-unknown-unknown", "release", "ruju_runtime.wasm");
const compiledPath = resolve(root, "target", "aot", "f_sumsq.wasm");

function check(label, got, want) {
  const ok = got === want;
  console.log(`${ok ? "ok  " : "FAIL"} ${label}: ${got}${ok ? "" : ` (expected ${want})`}`);
  if (!ok) process.exitCode = 1;
}

const { instance } = await WebAssembly.instantiate(readFileSync(runtimePath), {});
const x = instance.exports;
x.rj_init();

// --- two-module linking (decision D2c): the compiled module imports the ---
// --- runtime's memory and its rj_ boundary; the host wires them.        ---
const env = {
  memory: x.memory,
  rj_box_int: x.rj_box_int,
  rj_unbox_int: x.rj_unbox_int,
  rj_new_ref_int: x.rj_new_ref_int,
  rj_gc_shadow_top_addr: x.rj_gc_shadow_top_addr,
  rj_region_base: x.rj_region_base,
};
function instantiateCompiled(path, fixture) {
  let bytes;
  try {
    bytes = readFileSync(path);
  } catch {
    console.error(
      `harness_aot: ${path} missing — build it first:\n` +
        `  cargo run -p ruju-aotc -- ${fixture} ${path}`,
    );
    process.exit(1);
  }
  return WebAssembly.instantiate(bytes, { env });
}
const cf = (await instantiateCompiled(compiledPath, "aotc/fixtures/f_sumsq.json")).instance.exports;

// Sum of squares 1..n with Int64 wrap-around, in closed form.
const sumsq = (n) => BigInt.asIntN(64, (n * (n + 1n) * (2n * n + 1n)) / 6n);

// --- (a) the specsig export, against exact expected values ---
check("specsig f(0)", cf.f(0n), 0n);
check("specsig f(1)", cf.f(1n), 1n);
check("specsig f(10)", cf.f(10n), 385n);
check("specsig f(10^6)", cf.f(1000000n), sumsq(1000000n));
// Wrap-around: 10^7 overflows Int64 partway through the accumulation.
check("specsig f(10^7) wraps like Int64", cf.f(10000000n), sumsq(10000000n));

// --- interpreter agreement: the same loop as source through rj_eval ---
function evalJulia(src) {
  const bytes = new TextEncoder().encode(src);
  new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  return x.rj_eval(bytes.length);
}
const loopSrc = (n) =>
  `n = ${n}\nacc = 0\ni = 1\nwhile i <= n\nacc = acc + i * i\ni = i + 1\nend\nacc`;
for (const n of [0n, 1n, 10n, 1000000n]) {
  check(`interpreter agrees at n=${n}`, evalJulia(loopSrc(n)), cf.f(n));
}

// --- (b) the dispatch path: declare `f` in Main, place the boxed wrapper ---
// --- in the shared funcref table, register, call through real dispatch  ---
// --- driven by the pinned Julia's own lowering of `f(10)`.              ---
const T_INT64 = 12; // types.rs `id`
function writeSource(s) {
  const bytes = new TextEncoder().encode(s);
  new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  return bytes.length;
}
const func = x.rj_declare_generic(writeSource("f"));
check("rj_declare_generic returns an id", func !== 0, true);
const sig = x.rj_tuple_type1(x.rj_builtin_type(T_INT64));
const table = x.__indirect_function_table;
const fptr1 = table.grow(1);
table.set(fptr1, cf.f_boxed);
x.rj_register_compiled(func, sig, fptr1);

function loadLowered(text) {
  return x.rj_load_lowered(writeSource(text));
}
const callF = readFileSync(resolve(root, "aotc", "fixtures", "call_f_10.lowered"), "utf8");
check("dispatch path: pre-lowered f(10) -> compiled fptr1", loadLowered(callF), 385n);

// The boxed wrapper allocates (rj_box_int); run it under allocation stress —
// a collection per allocation — to prove the boundary's rooting holds.
x.rj_gc_stress(1);
check("dispatch path under GC stress", loadLowered(callF), 385n);
x.rj_gc_stress(0);
x.rj_gc_collect();
check("rj_root_count() balanced after compiled calls", x.rj_root_count(), 0);
check("dispatch still sound after stress", loadLowered(callF), 385n);

// --- stage 2: the allocating compiled function (gcframe emission) ---
// g(n) = Ref-carrying loop: a fresh allocation per iteration with the live
// ref held across it — the shape that proves compiled code roots through the
// linear-memory shadow stack (decision D3).
const cg = (
  await instantiateCompiled(
    resolve(root, "target", "aot", "g_refloop.wasm"),
    "aotc/fixtures/g_refloop.json",
  )
).instance.exports;

check("alloc specsig g(0)", cg.g(0n), 0n);
check("alloc specsig g(1)", cg.g(1n), 1n);
check("alloc specsig g(10)", cg.g(10n), 385n);
check("alloc specsig g(10^5) [auto-GC churn]", cg.g(100000n), sumsq(100000n));

// Interpreter agreement: the same loop over a source-defined mutable struct.
const refLoopSrc = (n) =>
  `mutable struct RR\nx::Int64\nend\nn = ${n}\nr = RR(0)\ni = 1\n` +
  `while i <= n\nr = RR(r.x + i * i)\ni = i + 1\nend\nr.x`;
for (const n of [0n, 10n, 1000n]) {
  check(`interpreter (struct loop) agrees at n=${n}`, evalJulia(refLoopSrc(n)), cg.g(n));
}

// The rooting proof: a collection on *every* allocation. If the compiled
// gcframe write-through is wrong, the live ref's cell is freed mid-loop and
// the field reads come back corrupted.
x.rj_gc_stress(1);
check("alloc specsig g(50) under GC stress", cg.g(50n), sumsq(50n));
x.rj_gc_stress(0);
x.rj_gc_collect();
check("shadow top balanced after compiled frames", x.rj_root_count(), 0);

// Dispatch path for the allocating function, driven by the pinned Julia's
// own lowering of `gref(10)`.
const gfunc = x.rj_declare_generic(writeSource("gref"));
const gsig = x.rj_tuple_type1(x.rj_builtin_type(T_INT64));
const gptr1 = table.grow(1);
table.set(gptr1, cg.g_boxed);
x.rj_register_compiled(gfunc, gsig, gptr1);
const callGref = readFileSync(resolve(root, "aotc", "fixtures", "call_gref_10.lowered"), "utf8");
check("dispatch path: pre-lowered gref(10) -> compiled fptr1", loadLowered(callGref), 385n);
x.rj_gc_stress(1);
check("dispatch path (allocating) under GC stress", loadLowered(callGref), 385n);
x.rj_gc_stress(0);
check("roots balanced after stressed dispatch", x.rj_root_count(), 0);

// --- the interpreted twin `g` (same body, defined through the real ---
// --- pre-lowering pipeline) for the per-call-overhead comparison.  ---
const defG = readFileSync(resolve(root, "aotc", "fixtures", "g_def.lowered"), "utf8");
loadLowered(defG);
const callG = readFileSync(resolve(root, "aotc", "fixtures", "call_g_10.lowered"), "utf8");
check("interpreted twin g(10) via dispatch", loadLowered(callG), 385n);

// --- benchmarks (go/no-go thresholds) ---
const N = BigInt(process.env.RUJU_AOT_BENCH_N ?? 10000000);
const REPS = Number(process.env.RUJU_AOT_BENCH_REPS ?? 5);
const expected = sumsq(N);

function median(xs) {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)];
}
function bench(label, fn) {
  const times = [];
  for (let r = 0; r < REPS; r++) {
    const t0 = performance.now();
    const got = fn();
    const dt = performance.now() - t0;
    times.push(dt);
    if (got !== expected) {
      check(`${label} result`, got, expected);
      break;
    }
  }
  const med = median(times);
  console.log(`info ${label}: median ${med.toFixed(1)} ms over ${times.length} reps`);
  return med;
}

console.log(`info benchmarking at n=${N}, ${REPS} reps`);
const tCompiled = bench("compiled specsig", () => cf.f(N));
const tNative = bench("native Rust-in-wasm", () => x.rj_bench_native(N));
const tInterp = bench("interpreter (rj_eval)", () => evalJulia(loopSrc(N)));

const speedup = tInterp / tCompiled;
const vsNative = tCompiled / tNative;
console.log(`info compiled vs interpreter: ${speedup.toFixed(1)}x faster (threshold >= 100x)`);
console.log(`info compiled vs native-Rust-in-wasm: ${vsNative.toFixed(2)}x (threshold <= 3x)`);
check("go/no-go: >= 100x over interpreter", speedup >= 100, true);
check("go/no-go: within 3x of native", vsNative <= 3, true);

// Per-call overhead through dispatch: the compiled fptr1 path must cost no
// more than reaching the interpreted twin (same declare/dispatch front half,
// same 10-iteration workload; the interpreted body pays boxing per op).
const CALLS = 2000;
function benchCalls(text) {
  const t0 = performance.now();
  for (let i = 0; i < CALLS; i++) loadLowered(text);
  return performance.now() - t0;
}
benchCalls(callF); // warm up both paths
benchCalls(callG);
const tCallF = benchCalls(callF);
const tCallG = benchCalls(callG);
console.log(
  `info dispatch per-call: compiled ${((tCallF / CALLS) * 1000).toFixed(1)}us, ` +
    `interpreted ${((tCallG / CALLS) * 1000).toFixed(1)}us over ${CALLS} calls`,
);
check("go/no-go: fptr1 call path <= interpreted call path", tCallF <= tCallG, true);

// Allocating-loop throughput (info only — no stage-2 threshold): one
// rj_new_ref_int call per iteration, so this measures the allocation
// boundary and GC churn, not codegen.
{
  const t0 = performance.now();
  const got = cg.g(1000000n);
  const dt = performance.now() - t0;
  check("alloc benchmark result", got, sumsq(1000000n));
  const t1 = performance.now();
  evalJulia(refLoopSrc(100000n));
  const dtInterp = (performance.now() - t1) * 10; // scaled to the same n
  console.log(
    `info allocating loop at n=10^6: compiled ${dt.toFixed(1)} ms, ` +
      `interpreted ~${dtInterp.toFixed(0)} ms (scaled from n=10^5)`,
  );
}

console.log(process.exitCode ? "aot thin slice: FAILED" : "aot thin slice: OK");
