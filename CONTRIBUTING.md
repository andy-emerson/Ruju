# Contributing

Contributions are welcome — written by hand, with an AI agent, or anywhere in
between. Use whatever process produces good work; the bar is the same either
way.

## Working with AI

AI-assisted contributions are **encouraged** here as a first-class way to
contribute — nothing to hedge, hide, or apologize for. That openness works
because it comes paired with credit and responsibility, and because this
project is built to verify code on its merits regardless of who wrote it.

### Credit your collaborators

If a commit contains substantial work from an agent, credit it as a co-author,
the same way you would credit a person:

```
Co-Authored-By: Claude <noreply@anthropic.com>
```

This is credit, not a disclaimer. The same courtesy applies to other content —
issues, discussions, and comments. Co-authorship is acknowledgment, not
copyright.

### Own what you submit

You — the human — are the author of record: review and understand every change
before opening a pull request, and stand behind what merges. "The agent wrote
it" is a credit line, never an excuse.

### Use the agent well

Agents do their best work on bounded, well-described tasks, and this
repository is deliberately structured to provide them. What works here:

- **Design first; let the agent build.** Decide the design and the increment
  yourself, then hand the agent one subsystem slice at a time. Point it at
  `CLAUDE.md`, which carries the agent-facing instructions, and at
  `design/roadmap.md` and `design/ledger.md` for what to do and where it
  stands.
- **Ground it in the reference.** The C runtime being ported is vendored at
  `reference/julia/src/` precisely so an agent can read it. Have the agent
  port from the named C file, not from its general recollection of how
  runtimes work.
- **Verify outside the conversation.** Agent-written code tends to fail
  plausibly rather than obviously, and agents habitually overestimate their
  own fidelity — audits here have caught over-claims more than once. Run the
  test suite and the oracle yourself; treat "tests pass" as the beginning of
  your review, not the end of it.
- **Make it keep the ledger.** Have the agent record divergences and partial
  coverage in `design/ledger.md` as it works, then check that the entries
  understate rather than oversell.

## The faithfulness bar

Ruju ports the *design* of Julia's C runtime, not a guess at it. The reference
is vendored at `reference/julia/src/`; when in doubt, read the C.

- A deliberate departure (for WASM or the composable-memory model) is a
  **divergence** and must be recorded in `design/ledger.md`. A simplification
  is **faithful + partial** — same shape, less of it.
- In the ledger, **Done · Faithful means verified against the reference**, not
  "tests pass." Our own tests can encode the same misunderstanding as the
  code; the reliable check is `runtime/verify_julia_subtype.mjs`, whose
  expected answers come from Julia's own test suite.
- Prefer understatement.

## Scope and mechanics

- **Small, verifiable increments.** One subsystem slice per pull request:
  implement, test, update the ledger, commit. Don't batch unrelated changes.
- **`reference/julia/` is vendored verbatim** and is never edited; see
  `reference/README.md` for how the pin is advanced.
- **Before committing**, run the checks (see the README for setup):

  ```sh
  cargo test --workspace
  cargo build -p ruju-runtime --target wasm32-unknown-unknown --release
  node runtime/harness.mjs
  node runtime/verify_julia_subtype.mjs
  ```

  Keep the build free of warnings.
- **Commit messages** are `component: Brief summary` (e.g. `subtype: …`,
  `gc: …`), followed by a short prose body explaining the purpose, plus any
  co-author trailers.
