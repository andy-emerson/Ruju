# The design documents

Three documents, three questions:

| Document | Question | One-line description |
| - | - | - |
| [`strategy.md`](strategy.md) | **Where are we going?** | the goal, the key design decisions, and the dependency map whose frontier says what can be worked on *now* |
| [`implementation.md`](implementation.md) | **Where are we?** | the per-module comparison of Julia's C/C++ runtime against the Rust reimplementation — architectural maps, side-by-side mini-maps, status tables, audit findings |
| [`methodology.md`](methodology.md) | **How do we get there?** | the process that moves code from written to pushed — the claim ladder, the increment loop, divergence handling, pin discipline, audits |

## The cycle

The three documents are not just complementary answers — they are stations
on a cycle, and every increment is one turn of it:

1. **`strategy.md` chooses.** The human picks an item from the dependency
   map's unblocked frontier.
2. **`methodology.md` governs.** The increment follows the loop — reference
   read and cited first, code and right-reason tests together, verification,
   the reference recheck, honest labeling.
3. **`implementation.md` records.** The same commit updates the touched
   module's maps, status rows, citations, and obligations.
4. **The cycle closes back into `strategy.md`.** What was recorded re-derives
   the frontier: a completed gate unblocks nodes, a new obligation becomes
   future work, a recorded divergence may add an edge. Step 3's output is
   step 1's next menu — the next increment begins from a strategy the last
   increment rewrote.

The increment's two boundary tables are where the documents touch: the
**pre-table** (the costed options for what to do next) is `strategy.md`
rendered actionable — its rows are the frontier; the **post-table** (the
deviation settlement) is `implementation.md`'s intake — every row points at
a record. That makes drift detectable, not merely forbidden: a pre-table row
with no frontier source means the strategy is stale, and a post-table row
with an empty "Recorded" column means the implementation ledger is.

The documents are load-bearing, not decorative: `implementation.md` is the
evidence ledger that the claim ladder requires, and `strategy.md`'s frontier
is the menu the next session starts from. A document that lags the code is
the first step of the next over-claim — which is why updating them is a step
*inside* the increment loop, not an afterthought.

## Reading order for a new contributor

`README.md` (repository root) → this file → `strategy.md` (what the project
is attempting and what is in flight) → `implementation.md` for any module you
intend to touch → `methodology.md` before your first commit. Conventions for
commits and attribution are in the root `CONTRIBUTING.md`; agent-facing
instructions are in `CLAUDE.md`.
