# The design documents

Three documents, three questions:

| Document | Question | One-line description |
| - | - | - |
| [`strategy.md`](strategy.md) | **Where are we going?** | the goal, the key design decisions, and the dependency map whose frontier says what can be worked on *now* |
| [`implementation.md`](implementation.md) | **Where are we?** | the per-module comparison of Julia's C/C++ runtime against the Rust reimplementation — architectural maps, side-by-side mini-maps, status tables, audit findings |
| [`methodology.md`](methodology.md) | **How do we get there?** | the process that moves code from written to pushed — the claim ladder, the increment loop, divergence handling, pin discipline, audits |

## How they work together

Every increment makes one trip around the three:

1. **`strategy.md` chooses.** The human picks an item from the dependency
   map's unblocked frontier.
2. **`methodology.md` governs.** The increment follows the loop — reference
   read and cited first, code and right-reason tests together, verification,
   the reference recheck, honest labeling.
3. **`implementation.md` records.** The same commit updates the touched
   module's maps, status rows, and citations; the strategy's frontier shifts
   if the increment opened or closed work.

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
