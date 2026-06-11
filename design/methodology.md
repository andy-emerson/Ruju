# Methodology

How we get there: the process that moves code from written to pushed, in a
project that demands precision and runs on human–agent trust. The increment
loop below is the arc of a cycle — it begins by consuming the strategy's
frontier (step 1) and ends by rewriting it (steps 6–8), so each increment
hands the next one its menu.

The founding observation, confirmed by every audit this project has run:
**the failure mode is never fabrication — it is a claim quietly sitting one
rung above its evidence.** "Tests pass" drifts into "faithful." The process
below exists to make that drift impossible, or failing that, visible.

**Contents:**
[The claim ladder](#the-claim-ladder) ·
[The increment loop](#the-increment-loop) ·
[Divergences](#divergences) ·
[Pin discipline](#pin-discipline) ·
[Audits](#audits)

## The claim ladder

Every statement about fidelity carries a grade. A claim may never be reported
at a rung above its evidence.

| Grade | Meaning | Evidence |
| - | - | - |
| **Stated** | asserted; no evidence yet | — |
| **Tested** | behaves correctly on our own tests | native suite + harness pass, zero warnings |
| **Oracle-verified** | behavior matches Julia's own asserted answers | `verify_julia_subtype.mjs` (and successors) — expected values copied from Julia's `test/`, cited by line |
| **Reference-verified** | design compared against the pinned C, structure by structure | line citations into `reference/julia/src/`, recorded in `implementation.md` |

Our own tests can encode the same misunderstanding as the code — *Tested* is
where verification starts, not where it ends. **Done · Faithful in the status
tables requires Reference-verified**, with the evidence (citations, side-by-
side comparison) recorded in `implementation.md`. No upgrade without evidence.

## The increment loop

1. **Pick from the frontier.** The human selects the increment from
   `strategy.md`'s unblocked frontier and holds design authority over scope.
2. **Read the reference first.** Before writing, open the C being ported and
   cite the specific lines/functions the increment targets. The reference is
   read at the start so the post-write check is confirmation, not discovery.
   Port from the pin, never from memory of upstream.
3. **Write the code and its tests together.** Tests must be able to fail —
   choose cases where the wrong behavior and the right behavior disagree
   (e.g. a union-order test where alphabetical and tier order differ). A bug
   fix ships in the same commit as the test that would have caught it.
4. **Verify it works.** In order: `cargo test --workspace` (run twice — GC
   tests are serialized; watch for flakiness), native build with **zero
   warnings**, wasm build, `node runtime/harness.mjs`, the oracle, and a
   whitespace check on changed files. Any failure stops the loop here.
5. **Recheck the reference.** With working code in hand, compare it against
   the cited C once more. Three outcomes, two of them pushable:
   - *Faithful* — record the verification in `implementation.md`.
   - *Knowingly divergent* — record a divergence (see below).
   - *Unknowingly divergent* — not pushable. Fix it or convert it to a
     recorded divergence. **The push gate is "honestly labeled," not
     "faithful."**
6. **Update the documents in the same commit.** `implementation.md`: mini-maps,
   status rows, citations, dated annotations (`fixed, audit 2026-06`).
   `strategy.md`: frontier status if the increment opened or closed work.
   Documents that lag the code are the first step of the next over-claim.
7. **Commit and push** following the repository's commit conventions, push
   to `main`, and verify local and remote are in sync.
8. **Report with a deviation table.** Every increment report ends with a
   table — the settlement that answers the frontier table's promise: one
   table chose the slice, one assesses how it went. One row per deviation
   from the slice's plan and per claim sitting below Reference-verified:
   what was planned vs delivered, the claim's rung, a severity, and where
   it is recorded. Rows are sorted by severity — the human spot-checks from
   the top, which preserves what the old single-weakest-link rule was for
   (a sorted list of where auditing pays off) while fixing its fixed output
   size: it manufactured a disclosure when nothing was serious and
   truncated to one when three were. **An empty table is itself a claim**
   ("no deviations") that audits will check. Anything implying a future
   verification obligation is recorded in `implementation.md` **before**
   the push — the table is a pointer to records, never the record itself.

## Divergences

A deliberate or unavoidable departure from the C is never silent:

- **In the status tables** (`implementation.md`): the row says *Divergence* (a
  different design, e.g. for WASM) or *Faithful + Partial* (same shape, less
  of it), with a dated note saying exactly what differs.
- **In the oracle**: behavior we cannot yet match becomes a
  *known-divergence entry* — it runs on every invocation, reports without
  failing the build, and announces itself if a fix heals it (at which point
  it is promoted to a regular case). A divergence that stops being tested is
  a divergence that gets forgotten.

## Pin discipline

The dependency and architectural maps describe **the pinned commit** recorded
in `reference/README.md`, not upstream `master`. The pin can be newer than
the port (audit 2026-06 found exactly this: `Intersect` nodes and
`push_forall_bound_scope` in the pinned `subtype.c`, absent from the port) —
which is why ports cite the pin and rechecks reread it. Advancing the pin is
itself an increment: re-vendor, diff both maps, re-audit every module whose
reference changed, update `reference/README.md`.

## Audits

Full audits (the `implementation.md` deep pass, module by module) are
event-driven:

- at the start of each session: a quick orientation pass — the documents
  match the code, the frontier matches reality — before any work is picked
  up;
- when the pin advances (touched modules);
- before new work begins in a module that hasn't been audited since its
  last substantial change;
- whenever a status row is promoted to Done · Faithful (that promotion *is*
  an audit finding, with evidence);
- at the human's discretion, any time trust needs re-grounding.

Audits have found over-claims every time they have run. Assume the next one
will too; the goal is not a clean audit but an honest ledger.
