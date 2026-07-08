// The M2 lowering oracle: for each corpus source, EXPECTED is the pinned
// Julia *executing the source*; ACTUAL is Ruju executing the pinned Julia's
// own lowering of it (tools/prelower.jl -> loader.rs). Same source, two
// executors, one answer — the fidelity claim D1 makes ("it IS upstream's
// lowering output") tested end to end.
//
// With the pinned Julia present (tools/fetch-pinned-julia.sh), fixtures and
// expected values are regenerated live (drift in committed artifacts is
// detected); without it, the committed .lowered/.expected files are used.
//
//   node runtime/verify_julia_lowering.mjs
// (build first: cargo build -p ruju-runtime --target wasm32-unknown-unknown --release)

import { readFileSync, readdirSync, writeFileSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve, basename } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const corpusDir = resolve(here, "lowered-corpus");
const julia = resolve(root, "tools", "pinned-julia", "bin", "julia");
const haveJulia = existsSync(julia);

const wasmPath = resolve(root, "target", "wasm32-unknown-unknown", "release", "ruju_runtime.wasm");
const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const x = instance.exports;
x.rj_init();

const loadLowered = (text) => {
  const bytes = new TextEncoder().encode(text);
  new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  return x.rj_load_lowered(bytes.length);
};

let pass = 0, fail = 0;
const sources = readdirSync(corpusDir).filter((f) => f.endsWith(".jl")).sort();
console.log(haveJulia
  ? "pinned Julia found: regenerating fixtures and expected values"
  : "pinned Julia absent: using committed fixtures (run tools/fetch-pinned-julia.sh to regenerate)");
for (const srcFile of sources) {
  const name = basename(srcFile, ".jl");
  const srcPath = resolve(corpusDir, srcFile);
  const lowPath = resolve(corpusDir, `${name}.lowered`);
  const expPath = resolve(corpusDir, `${name}.expected`);
  if (haveJulia) {
    writeFileSync(lowPath, execFileSync(julia, ["--startup-file=no", resolve(root, "tools", "prelower.jl"), srcPath]));
    const expected = execFileSync(julia, ["--startup-file=no", "-e",
      `print(Int64(include_string(Main, read(${JSON.stringify(srcPath)}, String))))`]).toString().trim();
    writeFileSync(expPath, expected + "\n");
  }
  const expected = BigInt(readFileSync(expPath, "utf8").trim());
  const actual = loadLowered(readFileSync(lowPath, "utf8"));
  if (actual === expected) {
    pass++;
    console.log(`ok   ${name}: ${actual}`);
  } else {
    fail++;
    console.log(`MISMATCH  ${name}: julia says ${expected}, ruju executed ${actual}`);
  }
}
console.log(`\n${pass}/${pass + fail} pre-lowered programs match the pinned Julia; ${fail} mismatch`);
process.exitCode = fail ? 1 : 0;
