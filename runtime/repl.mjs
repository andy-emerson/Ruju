// A tiny interactive REPL over the Ruju runtime: type Julia, get answers,
// everything running inside the WebAssembly module via `rj_eval`.
//
//   node runtime/repl.mjs              # interactive
//   node runtime/repl.mjs '1 + 2 * 3'  # one-shot
//
// (build first: cargo build -p ruju-runtime --target wasm32-unknown-unknown --release)
//
// Supported subset (the bootstrap front-end, runtime/src/frontend.rs):
// integer and float literals, variables, assignment, `+ - *`, comparisons
// (`< <= > >= ==` and `===`), `if`/`elseif`/`else`/`end`, and `while`. The
// value of the session is its last expression.
//
// Honest limitations of this tool (not the runtime):
// - Variables persist between lines by re-evaluating the accumulated session
//   source (evaluation is pure, so this is sound; `:reset` clears it).
// - The eval ABI returns 0 on a parse/eval error, indistinguishable from a
//   computed 0; this REPL drops an entry when both interpretations read 0
//   and warns. The 8 KiB source buffer bounds a session.
// - Result type detection: the result is read as both Int64 and Float64 bit
//   patterns; an Int64's bits reinterpreted as a double are a denormal
//   (< 2^-1000) that no program here produces, so the readings disambiguate.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { createInterface } from "node:readline";

const here = dirname(fileURLToPath(import.meta.url));
const wasmPath = resolve(here, "..", "target", "wasm32-unknown-unknown", "release", "ruju_runtime.wasm");
const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const x = instance.exports;
x.rj_init();

const SRC_CAP = 8192;

function evalBoth(src) {
  const bytes = new TextEncoder().encode(src);
  if (bytes.length > SRC_CAP) return { err: `session exceeds the ${SRC_CAP}-byte source buffer (:reset to clear)` };
  new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  const i = x.rj_eval(bytes.length);
  new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  const f = x.rj_eval_f64(bytes.length);
  return { i, f };
}

function show({ i, f, err }) {
  if (err) return { text: err, ok: false };
  if (i === 0n && f === 0) return { text: "0  (note: a parse/eval error also reads as 0)", ok: true };
  // A genuine Int64 read as a double is a denormal; a genuine Float64 read as
  // an Int64 is astronomically large. Prefer the plausible reading.
  if (Math.abs(f) > 1e-300 || (i === 0n && f !== 0)) {
    return { text: Number.isInteger(f) ? f.toFixed(1) : String(f), ok: true };
  }
  return { text: String(i), ok: true };
}

// Count block openers vs `end` to know when a multi-line entry is complete.
function openBlocks(src) {
  let n = 0;
  for (const tok of src.split(/[^A-Za-z_]+/)) {
    if (tok === "if" || tok === "while") n++;
    else if (tok === "end") n--;
  }
  return n;
}

// One-shot mode.
const arg = process.argv.slice(2).join(" ").trim();
if (arg) {
  console.log(show(evalBoth(arg)).text);
  process.exit(0);
}

console.log("Ruju REPL — Julia source via rj_eval (subset: literals, variables,");
console.log("+ - *, comparisons, if/elseif/else, while). :reset clears the");
console.log("session, :quit exits.\n");

let session = ""; // accumulated source: how variables persist across lines
let pending = ""; // multi-line entry in progress

const rl = createInterface({ input: process.stdin, output: process.stdout, prompt: "ruju> " });
rl.prompt();
rl.on("line", (line) => {
  const t = line.trim();
  if (t === ":quit" || t === ":q") return rl.close();
  if (t === ":reset") {
    session = ""; pending = "";
    console.log("session cleared");
    rl.setPrompt("ruju> "); rl.prompt(); return;
  }
  pending += (pending ? "\n" : "") + line;
  if (openBlocks(pending) > 0) {
    rl.setPrompt("....> "); rl.prompt(); return; // entry continues
  }
  const candidate = session + (session ? "\n" : "") + pending;
  pending = "";
  const out = show(evalBoth(candidate));
  console.log(out.text);
  if (out.ok) session = candidate; // keep the entry only if it evaluated
  rl.setPrompt("ruju> "); rl.prompt();
});
rl.on("close", () => process.exit(0));
