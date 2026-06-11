// A tiny interactive REPL over the Ruju runtime: type Julia, get answers,
// everything running inside the WebAssembly module via `rj_eval`.
//
//   node runtime/repl.mjs              # interactive
//   node runtime/repl.mjs '1 + 2 * 3'  # one-shot
//
// (build first: cargo build -p ruju-runtime --target wasm32-unknown-unknown --release)
//
// Supported subset (the bootstrap front-end, runtime/src/frontend.rs):
// integer and float literals, variables, assignment, arithmetic
// (`+ - * / ÷ %`), bitwise (`& | ⊻ << >> >>>`), comparisons (`< <= > >= ==`
// and `===`), `if`/`elseif`/`else`/`end`, `while`, and `struct`/`mutable
// struct` with constructor calls and `p.x` field access. `/` yields Float64,
// as in Julia. The value of the session is its last expression.
//
// Honest limitations of this tool (not the runtime):
// - Variables persist between lines by re-evaluating the accumulated session
//   source (evaluation is pure, so this is sound; `:reset` clears it).
// - The result's type comes from rj_eval_typeof, so Int64, Float64, and Bool
//   results print correctly; an erroring entry (parse error or e.g.
//   DivideError) is reported and dropped from the session. The 8 KiB source
//   buffer bounds a session.

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
const FLOAT64_T = x.rj_builtin_type(21); // types.rs id::FLOAT64
const BOOL_T = x.rj_builtin_type(8); // types.rs id::BOOL

// Evaluate the session source: ask the runtime for the result's type, then
// read with the matching decoder (evaluation is pure, so evaluating twice is
// sound). A type of 0 means a parse/eval error (e.g. DivideError).
function evaluate(src) {
  const bytes = new TextEncoder().encode(src);
  if (bytes.length > SRC_CAP) return { err: `session exceeds the ${SRC_CAP}-byte source buffer (:reset to clear)` };
  const write = () => new Uint8Array(x.memory.buffer, x.rj_source_ptr(), bytes.length).set(bytes);
  write();
  const t = x.rj_eval_typeof(bytes.length);
  if (t === 0) return { err: "error (parse error, unsupported syntax, or e.g. DivideError)" };
  write();
  if (t === FLOAT64_T) {
    const f = x.rj_eval_f64(bytes.length);
    return { text: Number.isInteger(f) ? f.toFixed(1) : String(f) };
  }
  const i = x.rj_eval(bytes.length);
  if (t === BOOL_T) return { text: i === 1n ? "true" : "false" };
  return { text: String(i) };
}

function show(r) {
  return r.err ? { text: r.err, ok: false } : { text: r.text, ok: true };
}

// Count block openers vs `end` to know when a multi-line entry is complete.
function openBlocks(src) {
  let n = 0;
  for (const tok of src.split(/[^A-Za-z_]+/)) {
    if (tok === "if" || tok === "while" || tok === "struct") n++;
    else if (tok === "end") n--;
  }
  return n;
}

// One-shot mode.
const arg = process.argv.slice(2).join(" ").trim();
if (arg) {
  console.log(show(evaluate(arg)).text);
  process.exit(0);
}

console.log("Ruju REPL — Julia source via rj_eval (subset: literals, variables,");
console.log("+ - * / ÷ %, & | ⊻ << >> >>>, comparisons incl. ===, if/elseif/else,");
console.log("while, struct/mutable struct with Point(1,2) and p.x). :reset clears");
console.log("the session, :quit exits.\n");

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
  const out = show(evaluate(candidate));
  console.log(out.text);
  if (out.ok) session = candidate; // keep the entry only if it evaluated
  rl.setPrompt("ruju> "); rl.prompt();
});
rl.on("close", () => process.exit(0));
