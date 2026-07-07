# Porting Julia's subtype engine: the global union-decision machine

Reference archaeology for Ruju, 2026-07-07. All C citations are to the pinned
reference `/home/user/Ruju/reference/julia/src/subtype.c` (6324 lines) unless
another file is named; Rust citations to `/home/user/Ruju/runtime/src/subtype.rs`
(644 lines); test citations to `/home/user/Ruju/reference/julia/test/subtype.jl`.

Labels: **[PIN]** = VERIFIED-FROM-PIN (read in the pinned source at the cited
lines). **[INF]** = INFERRED (mechanism-level reasoning from verified code, e.g.
execution traces). **[UNC]** = UNCERTAIN (flagged for follow-up).

---

## TL;DR

- Julia does **not** backtrack unions locally. Every `Union` arm choice made
  anywhere in a query is a numbered bit in one of two global bit-stacks
  (`Lunions` for left-of-`<:` unions, `Runions` for right), and two nested
  driver loops — `forall_exists_subtype` (∀, left) wrapping `exists_subtype`
  (∃, right) — re-run the *entire* query once per bit combination, enumerated
  as a lazily-grown binary counter (subtype.c:35–60, 237–260, 2359–2404). [PIN]
- The two tracked oracle divergences (test/subtype.jl:371 and :410) fail in
  Ruju for one structural reason: the backtrack point they need (the right
  union arm, resp. the `∃T` binding) is an **ancestor** of the left union in
  the recursion tree, and local save/restore can only redo descendants. The
  global machine fixes both because the left-union enumeration is hoisted to
  the outermost loop, so each left arm gets a fresh right-side search and a
  fresh `T` binding. Traces in §1.4–1.5. [INF]
- The pin has evolved past upstream-familiar shapes: `Type{T}` is a dedicated
  `TypeEq` node (julia.h:1642,1846) and `simple_meet` can return an internal
  `Intersect{a,b}` meet node (#61917; subtype.c:759–780, julia.h:595–604). Both
  matter for the port but the Intersect node is only *required* once precise
  existential upper bounds matter (§3).
- `Loffset` is the vararg length-offset channel (`X = Y + Loffset`,
  subtype.c:138–140) threaded through `subtype_ccheck`/`subtype_var`/
  `forall_exists_equal`; it and `intvalued` are where typevar-count
  `Vararg{T,N}` lives (§4). Independent of the union machine.
- Recommended plan: 5 slices; **slice 1 = the union-state machine + driver
  loops + dispatch-order fixes**, which heals both known divergences (they
  self-report in the oracle) and closes audit finding 11. Slices: (1) machine,
  (2) `local_forall_exists_subtype`/`forall_exists_equal`, (3) bounded varargs
  (`Loffset`/`intvalued`), (4) `Intersect` + `concrete` propagation,
  (5) `envout` for dispatch. §6.
- Biggest risks: env-snapshot discipline vs the Rust `Vec` env (must save
  Runions.depth with the env, restore must clear/keep the right things), and a
  **widened GC exposure** — the machine's saved envs hold `lb`/`ub` across the
  allocating search (`simple_join`→`union_type`, `make_typevar`, boxed longs),
  and subtype.rs currently roots *nothing* (§7).

---

## 1. The union decision machine

### 1.1 State: `jl_unionstate_t` and the statestack [PIN]

Header comment, subtype.c:35–41: "stack of bits to keep track of which
combination of Union components we are looking at (0 for Union.a, 1 for
Union.b). forall_exists_subtype and exists_subtype loop over all combinations
by updating a binary count in this structure. Union type decision points are
discovered while the algorithm works. If a new Union decision is encountered,
the `more` flag is set to tell the forall/exists loop to grow the stack."

- `jl_bits_stack_t` (subtype.c:43–46): chunks of 16×`uint32` (512 bits),
  malloc-chained on demand (`statestack_set`, :217–233). Depth is capped at
  32767 by the `int16_t depth` field (:205,219).
- `jl_unionstate_t` (:48–53): `{int16 depth, more, used; jl_bits_stack_t stack}`.
- Two instances live in `jl_stenv_t`: `Lunions` (left of `A <: B`) and
  `Runions` (right) (:128–129).
- `jl_saved_unionstate_t` (:55–60) + `push_unionstate`/`pop_unionstate`
  macros (:273–306) snapshot `{depth, more, used}` plus the first `used` bits
  into an `alloca` buffer — used to *shield* an inner computation's union
  state (e.g. `subtype_ccheck` shields `Lunions`, :862/871) or to freeze a
  partially-enumerated `Lunions` (:2229, 2248).

Field roles (from `pick_union_decision`, :248–260, and `next_union_state`,
:237–246):

- `depth` — index of the **next decision point to read** in the current
  traversal. Reset to 0 at the start of every pass (:2363–2366, 2207,
  2237–2238); incremented each time a union choice is consulted (:256).
- `used` — number of bits currently meaningful (how deep the previous pass
  got). When `depth >= used`, the traversal has *discovered a new decision
  point*: its bit is initialized to 0 and `used` grows (:251–254).
- `more` — set to `depth` (post-increment position) whenever a bit is read as
  0 (:257–258): it memorizes the **deepest choice point that still has an
  untried alternative** (0 = arm `a` tried, arm `b` remains). `more == 0` ⇒
  the enumeration is exhausted.

`next_union_state` (:237–246) is the binary-counter increment: if `more == 0`
return 0 (done); else truncate `used = more`, set bit `more-1` to 1, return 1.
Bits deeper than `more` are discarded (subsequent `pick_union_decision` calls
re-initialize them to 0 as they are rediscovered) — correct because decision
points deeper than the flipped one may not even exist under the new prefix.
This enumerates only combinations of decision points *actually reached*, in
depth-lexicographic order.

`pick_union_element` (:262–271) applies `pick_union_decision` repeatedly to
descend a nested `Union` spine and return exactly **one** leaf arm.

### 1.2 What `subtype()` itself does with unions [PIN]

Inside the recursive `subtype(x, y, e, param)` (:1903–2154), unions are *not*
looped over locally:

- Left union (:1905–1932): after an `obviously_egal` fast path (:1906) and a
  typevar-right fast path (:1908–1931 — if no right-union decisions are
  pending, `Runions.depth == 0`, and `y` is a typevar and `x` is ground,
  handle the variable against the *whole* union via `subtype_var`), it does
  `x = pick_union_element(x, e, 0)` — pick ONE arm per the current `Lunions`
  bits and continue. **The ∀ obligation ("every arm") is discharged by the
  outer driver loop re-running the query, not by `&&` here.**
- Right union (:1934–1951): after `obviously_in_union` (:1935), a left
  `UnionAll` takes priority (:1937–1938 — introduce the ∀ var *before*
  splitting the union), and a left typevar consults the machine for *whether*
  to split at all: `ui = ((xx && xx->existential) || jl_has_free_typevars(y))
  && pick_union_decision(e, 1)` (:1940–1948) — the "`convert(Type{T},T)`
  pattern" (test/subtype.jl:443–446): trying the whole union against the
  variable first is itself a recorded choice the machine can revisit. Then
  `y = pick_union_element(y, e, 1)`.

This is exactly audit finding 11 (design/implementation.md:331–338): Ruju
splits unions first, unconditionally; Julia gives typevar/UnionAll handling
priority and makes even the split-or-not decision a machine choice point.

### 1.3 The driver loops [PIN]

`forall_exists_subtype` (:2383–2404), entered from `jl_subtype_env` (:2888)
with a fresh env (`init_stenv`, :2406–2426):

```c
save_env(e, &se, 1);            // snapshot bounds+counters+Runions.depth
e->Lunions.used = 0;
while (1) {
    sub = exists_subtype(x, y, e, &se, param);   // full ∃ search under these L bits
    if (!sub || !next_union_state(e, 0)) break;  // advance the ∀ counter
    re_save_env(e, &se, 1);     // KEEP constraints from the successful pass
}
```

`exists_subtype` (:2359–2381):

```c
e->Runions.used = 0;            // right enumeration restarts fresh per ∀ pass
while (1) {
    e->Runions.depth = 0; e->Runions.more = 0;
    e->Lunions.depth = 0; e->Lunions.more = 0;   // re-READ the fixed L bits
    if (subtype(x, y, e, param)) return 1;
    if (next_union_state(e, 1)) restore_env(e, se, 1);  // flip deepest R bit, roll back env
    else { restore_env(e, se, 1); return 0; }
}
```

The comment at :2385–2387 gives the shape: `∀₁ { ∃₁ }`.

Mechanics worth pinning:

- **∀ outside, ∃ inside.** Each left-arm combination (Lunions bits) gets a
  complete fresh right-side search: `Runions.used = 0` at `exists_subtype`
  entry (:2361) erases all right decisions from the previous ∀ pass.
- **L bits are fixed during the ∃ search.** Only `Lunions.depth`/`more` are
  reset per attempt (:2365–2366) so the same bits are re-read; the successful
  attempt's `Lunions.more` is what the ∀ loop's `next_union_state(e, 0)`
  consults (:2397) — the ∀ counter advances relative to the decision points
  the *successful* traversal encountered.
- **What is saved/restored.** `jl_savedenv_t` (:331–337) holds, per live
  binding: GC-rooted `lb`, `ub`, `innervars` (roots array or an allocated
  svec when >8 vars, `alloc_env` :385–414) plus a 4-byte record
  `[occurs_inv, occurs_cov, cov_diag, max_offset]` (:319–320, `re_save_env`
  :351–383) plus `se->rdepth = e->Runions.depth` (:382). `restore_env`
  (:445–479) writes all of that back — including `e->Runions.depth = rdepth`
  (:476) — and zeroes `envout` entries from `envidx` on (:477–478). Note:
  `existential`/`concrete`/`intvalued`/`depth0` etc. are **not** saved; only
  the mutable-bounds-and-counters subset is.
- **Failed ∃ attempts roll back; successful ∀ passes accumulate.** Between ∃
  attempts, `restore_env` returns to the snapshot (:2373, 2377). After a
  successful ∀ pass, `re_save_env` (:2399) *re-snapshots the current env* —
  so constraints recorded on **outer-scope** existential bindings by earlier
  left arms persist into later left arms (all ∀ branches must be satisfied by
  one assignment of any variable bound *outside* the union split; variables
  bound *inside* the pass are pushed/popped per pass and get fresh values).
- `exists_subtype` preserves already-assigned `envout` slots when flipping an
  R bit, by temporarily setting `envidx = envsz` around `restore_env`
  (:2369–2375) — `subtype_unionall` needs previously assigned env values, and
  cross-arm disagreement is reconciled at :1538–1559 (AND-semantics on the
  `constrained` bit).

### 1.4 Trace: why local backtracking fails test/subtype.jl:371 [INF from PIN mechanics]

Query (hard direction of `isequal_type`):
`Tuple{Union{Int,Int8}, Int16} <: Union{Tuple{Int,Int16}, Tuple{Int8,Int16}}` — Julia says **true**.

**Ruju today** (subtype.rs:87–100): `sub` meets the right union first (the
left side is a Tuple, not a union, at top level). It saves `e.vars`, tries arm
`Tuple{Int,Int16}`: elementwise, element 1 is `Union{Int,Int8} <: Int`, which
subtype.rs handles as a **left-union `&&`** (subtype.rs:88–89):
`Int <: Int` ✓ but `Int8 <: Int` ✗ → arm fails. Restore, try arm
`Tuple{Int8,Int16}`: `Union{Int,Int8} <: Int8` fails on `Int` → **false**.
The defect: both left arms are forced to hold under a *single* right-arm
choice, because the right choice was made higher in the call stack and the
left `&&` runs entirely inside it. No amount of save/restore at the union
nodes can fix this — the needed backtrack point (the right arm) is an
ancestor of the failure, and local backtracking only re-executes descendants.

**The pin's machine:**
1. Pass 1 (all bits fresh): traversal reaches the right union first →
   `pick_union_element(y, e, 1)` reads R-bit0 = 0 → arm `Tuple{Int,Int16}`
   (:1950). Element 1 reaches the left union → `pick_union_element(x, e, 0)`
   reads L-bit0 = 0 → `Int` (:1932). `Int <: Int` ✓, `Int16 <: Int16` ✓ →
   pass succeeds with `Lunions.more = 1` (a 0-bit was read, :257–258).
2. ∀ loop: `next_union_state(e, 0)` flips L-bit0 to 1 (:2397, 237–246);
   `re_save_env` (no live bindings — no-op in effect).
3. Pass 2: `exists_subtype` resets `Runions.used = 0`. Attempt 1: right arm
   `Tuple{Int,Int16}` again (fresh R-bit0 = 0); left union now reads L-bit0 = 1
   → `Int8`. `Int8 <: Int` ✗. `Runions.more == 1` → `next_union_state(e, 1)`
   flips R-bit0; `restore_env`. Attempt 2: right arm `Tuple{Int8,Int16}`,
   left arm `Int8` → ✓.
4. ∀ loop: successful pass read no L 0-bits (`Lunions.more == 0`) → done →
   **true**. Each left arm chose its own right arm because the right
   enumeration restarts per ∀ pass.

### 1.5 Trace: why local backtracking fails test/subtype.jl:410 [INF from PIN mechanics]

Query: `Tuple{Union{Vector{Int},Vector{Int8}}} <: (Tuple{Vector{T}} where T)`
— Julia says **true** (same family: :449 with `Ref`).

**Ruju today**: right is a `UnionAll` → `subtype_unionall` pushes `∃T`
(subtype.rs:171–183); body comparison reaches
`Union{Vector{Int},Vector{Int8}} <: Vector{T}`; the left-union `&&`:
`Vector{Int} <: Vector{T}` pins `T = Int` (invariant `forall_exists_equal`
narrows `lb = ub = Int`); then `Vector{Int8} <: Vector{T}` needs `T = Int8`
against the already-pinned binding → ✗ → **false**. Again the needed
backtrack point (the `T` binding, i.e. the `subtype_unionall` frame) is an
ancestor of the left-union split.

**The pin's machine:** the `∃T` binding is created *inside* `subtype()`
(:2079–2080 → `subtype_unionall`, :1372) — i.e. inside each ∀ pass, and
popped at its end (:1421). Pass 1 (L-bit0 = 0): left arm `Vector{Int}`, fresh
`T` binding, `T = Int` ✓. ∀ counter flips L-bit0. Pass 2: left arm
`Vector{Int8}`, **fresh `T` binding again**, `T = Int8` ✓ → **true**. The
"per-union-branch choice of `T`" the implementation ledger describes
(design/implementation.md:355–364) is literally the fresh varbinding per ∀
pass. (Contrast :448/:450: with `Ref{Union{...}}` — the union sits under an
*invariant* constructor, `forall_exists_equal` requires both directions, and
the answer is correctly false; and :413/:416 fail in every pass because the
second tuple element pins `T` within the same pass. The machine preserves
those `false`s. [INF])

### 1.6 Nested/invariant contexts share the Runions machine [PIN]

`forall_exists_equal` → `local_forall_exists_subtype` runs with a **fresh
`Lunions`** but continues the **outer `Runions`** stack (only
`e->Lunions` is shielded, :2347/2355; the general path manipulates
`e->Runions.more` relative to the caller's `oldRmore`, :2191, 2245–2265).
Consequence: a right-union decision made deep inside an invariant equality
check is visible to, and revisitable by, the outer `exists_subtype` loop —
that is what makes the machine "global". The pruning guard: after success, if
new R decisions appeared but the env is unchanged (`env_unchanged`, :811–840)
or the search was `limited`, hide them (`e->Runions.more = oldRmore`,
:2262–2265) to avoid combinatorial explosion.

---

## 2. `jl_varbinding_t` field inventory

Struct at subtype.c:67–114. [PIN] "Rust" column: `VarBinding`
(subtype.rs:39–56) has `var/lb/ub/existential/occurs_cov/cov_diag/depth0`.

| Field (line) | Purpose | Consumers | In Rust? |
| - | - | - | - |
| `var` (:68) | the `jl_tvar_t`; NULL = temporarily deleted from env | `lookup_binding` :170–193 | **yes** |
| `lb`, `ub` (:69–70) | current bounds, narrowed during search | `var_lt`/`var_gt` :1022–1108, everywhere | **yes** |
| `existential` (:71) | the `R` flag (∃ right / ∀ left) | :1036, 1087, 1985–1986, `env_unchanged` :826 | **yes** |
| `occurs_inv` (:72) | # invariant-position occurrences, saturating at 2, only when `invdepth > depth0` (:898–900) | diagonal/envout logic :1393, 1495, 1498, 1522; intersection :4155 | **no** (Rust uses only the static body check) |
| `occurs_cov` (:73–78) | # covariant occurrences in the *current consistency scope*, saturating at 2 | `cov_count` :326–329; diagonal :1404; scopes :925–983 | **yes** |
| `cov_diag` (:79–84) | max `occurs_cov` reached in any closed consistency scope; diagonal test = `max(occurs_cov, cov_diag) > 1` | `cov_count`; `pop_consistency_scope` :942–943 | **yes** |
| `concrete` (:85) | another var's diagonal constraint forces this one concrete | set :1411–1415; tested :1405–1410; intersection :4047, 4055, 4816 | **no** (the tracked missing propagation) |
| `max_offset` (:86–87) | max positive vararg-length offset seen (≤32); −1 if the var occurs outside a `Vararg` N slot (:905–908) | saved env slot 4 (:319, 378, 472); `subtype_tuple_varargs` :1602–1603, 1688, 1732–1735; intersection :4279–4286, 4317–4328; `merge_env` min-merge :5034–5036 | **no** |
| `constraintkind` (:88–93) | intersection-only: which of 3 strategies computes `var ∩ type` in covariant position | `intersect_var` :3478–3511; `intersect_unionall` :4144–4189 | **no** (intersection) |
| `intvalued` (:94) | var must be integer-valued (occurs as `N` in `Vararg{_,N}`) | set :1682, 1701–1709, 4250, 4258; consumed :1496–1497 (envout token), `finish_unionall` :3698–3709 | **no** |
| `limited` (:95) | intersection recursion guard: env grew past 120 bindings, result may be truncated | set :4020–4028; propagated :4147–4150 | **no** (intersection) |
| `intersected` (:96) | var has been through an intersection round; gates `max_offset` reset/restore | :907, 1688, 1732–1735, 4278–4283, 5035 | **no** |
| `widened_to_kind` (:97) | `Type{X}` was widened to a union of kinds (intersection, constraintkind 1) | :3494, 4171–4189 | **no** (intersection) |
| `tainted_inner` (:98–103) | bounds reference a popped deeper-depth0 tvar → binding leaky | set :1470; consumed in envout :1525 | **no** |
| `body_occurs_inv` (:104–108) | cached static `var_occurs_invariant(u->body, u->var)` | diagonal :1404; `env_unchanged` :832 | **computed at pop** (subtype.rs:195), not cached — same answer, O(body) once per pop instead of once per push [INF: equivalent] |
| `depth0` (:109) | invariant nesting depth at introduction | ∃∀-vs-∀∃ :2020–2021; `record_var_occurrence` :898; vararg checks :1678 | **yes** (i32 vs int16) |
| `innervars` (:110–112) | tvars our bounds depend on whose UnionAlls must move outside ours | `push_innervar` :157–165; `lookup_binding` innervar flag :181–191; `unalias_unionall` :1310–1317; saved-env root #3 :373; heavy use in intersection `finish_unionall` | **no** |
| `prev` (:113) | intrusive stack link | — | Vec index (equivalent) |

Note there is **no `concretevar` field in the pin** — older Julia had one;
here the cross-var mechanism is `concrete` set through `lookup(e, vb.lb)` at
:1411–1415. [PIN]

---

## 3. The `Intersect` node (#61917 era)

**What it is** [PIN]: an internal, transient "meet" dual to `Union`:
`Intersect{a, b}` denotes `a ∩ b` (julia.h:595–604; the datatype is created in
jltypes.c:3350–3354 and is Core-internal, `mayinlinealloc = 0`). It exists so
an existential variable's narrowed **upper bound** can be *exact* when the GLB
of two constraints is not expressible as one existing type.

**Where produced** [PIN]:
- `simple_meet(a, b, overesi)` (subtype.c:759–780). With `overesi == 1`
  (the subtype path): if either operand is already an `Intersect`, nest
  (:765–768); otherwise `simple_intersect(a, b, 1)` (jltypes.c:864–960)
  returns `jl_new_struct(jl_intersect_type, a, b)` when neither side subsumes
  the other and they are not disjoint (jltypes.c:944–952). With `overesi == 0`
  (`merge_env`) it under-estimates to `Union{}`; with `overesi == 2` it
  over-approximates by a union of survivors (legacy behavior).
- The only subtype-path producer is `var_lt`: `bb->ub = simple_meet(bb->ub, a, 1)`
  (:1059–1066), with the motivating comment: over-approximating to one side
  "would let `b` escape its declared range (e.g. equating `∃b<:Foo` with an
  outer `∀a<:Bar` even though `Bar ⊄ Foo`). See #61917."

**Where consumed** [PIN]:
- `subtype()` right-hand case (:1952–1961): `x <: a ∩ b` iff `x <: a && x <: b`
  (dual of left-union). An `Intersect` on the **left** is asserted impossible
  (:1956) — it only ever appears as the top layer of a `vb->ub` spine
  (comment :783–788).
- `subtype_ccheck` reaches it through `var_gt`'s `subtype_ccheck(a, bb->ub, e)`
  (:1098) and `equal_var` (:2294) — the consistency checks against a
  now-exact upper bound are where the precision pays.
- `is_leaf_bound` returns 0 for it (:1142) — a meet node is never a concrete
  leaf for the diagonal rule.
- `widen_intersect` (:789–800) peels the spine with `simple_meet(…, 2)`
  before the bound can escape: called on `vb.ub` in `subtype_unionall` after
  the body check, before result-typevar construction (:1428–1432), and
  asserted gone in intersection's `finish_unionall` (:3682). `obviously_egal`
  (:520) and `var_occurs_inside` (:3540) treat it structurally like a union
  pair. `forall_exists_equal`'s callers see it via `obvious_subtype` (:2557,
  2576).

**What breaks without it** [INF]: with Ruju's current over-estimate
(`simple_meet` returns `b`, subtype.rs:609–627), an existential `ub` silently
*forgets* one of two incomparable constraints, so a later `ccheck`
(`lb <: ub`, `a <: ub`) can accept a value outside the forgotten constraint —
false positives of the `∃b<:Foo` vs `∀a<:Bar` kind in :1064–1065's comment.
None of the current 106 oracle cases exercises it (both operands incomparable
*and* non-disjoint requires two overlapping abstract constraints, e.g.
crossing typevar bounds); it becomes load-bearing once bounded existentials
meet multiple `where` constraints, and intersection's `merge_env` needs the
`overesi` distinction. [UNC] which oracle-expressible test first requires it —
candidates live in test_3's bounded cross-constraints (test/subtype.jl:333–339)
but I did not find a currently-expressible failing case; treat slice 4 as
verified by targeted new oracle cases rather than an existing tranche.

---

## 4. `Loffset` — the vararg length-offset channel

**Semantics** [PIN]: `jl_stenv_t.Loffset` (subtype.c:138–140): "Used to
represent the length difference between 2 vararg. intersect(X, Y) ==>
X = Y + Loffset". During a comparison of two vararg length expressions, the
left length equals the right length **plus** `Loffset`.

**Written** [PIN]:
- `subtype_tuple_varargs` (:1725–1729): comparing `Vararg{_,N_x}` vs
  `Vararg{_,N_y}` after the two tails consumed `vx` resp. `vy` elements, the
  length equation is `N_x − vx == N_y − vy`; when both are boxed longs it is
  evaluated directly (:1714–1715), when one is a long it is re-boxed with the
  difference folded in (:1717–1724), otherwise `e->Loffset = vx − vy` and
  `forall_exists_equal(xp1, yp1, e)` carries the offset.
- `flip_offset` (:481) negates it for the reverse direction inside
  `forall_exists_equal` (:2351–2353) — `X = Y + k ⟺ Y = X − k`.
- Intersection: `intersect_varargs` (:4243–4273) sets it symmetrically.

**Read** [PIN]:
- Boxed-long comparisons: `subtype` (:2151–2152), `subtype_ccheck` (:848–849),
  `subtype_left_var` (:877–878): `unbox(x) == unbox(y) + Loffset`.
- `var_lt`/`var_gt` (:1032–1035, 1083–1086): under a nonzero offset, a bound
  that is neither a typevar nor `Bottom`/`Any` cannot satisfy an offset
  relation → fail fast (only a typevar can absorb an offset; longs are
  handled a level up).
- `subtype_var` (:1122–1131): when the constraint `a` **is** a boxed long,
  fold the offset into the constant (sign-flipped for `R`), zero `Loffset`,
  recurse into `var_gt`/`var_lt`, restore. So `N (bound) vs 3` under
  `Loffset = k` becomes `N vs 3±k` at offset 0.
- Intersection mirrors: `set_var_to_const` (:3218–3220), `bound_var_below`
  (:3247–3255); many `Loffset == 0` guards gate fast paths (:2341, 4488, 4828).

**Interaction with `intvalued`** [PIN]: `subtype_tuple_varargs` marks both
sides' `N` bindings `intvalued = 1` before the equation (:1700–1713); an
unconstrained right length gets `lb = Any` + `intvalued` as the "N::Int,
unconstrained" token (:1682–1691), which `subtype_unionall`'s envout turns
into the special `jl_wrap_vararg(NULL,NULL,0,0)` token (:1496–1497), and
intersection's `finish_unionall` validates (`int-valued typevar must either be
an Int, or have Bottom-Any bounds, or equal another typevar`, :3698–3709).
`record_var_occurrence` sets `max_offset = −1` for any occurrence outside a
vararg-N slot (:905–908), and `subtype_tuple_varargs` snapshots/restores
`max_offset` around the N-equation (:1602–1603, 1732–1735) so the subtype
round doesn't clobber what intersection's second round needs. Also note
`check_vararg_length` (:1568–1583; called :1828–1832, 1869) discharges the
`(lx+1−ly) <: N` equation when a fixed-length tuple meets `Vararg{T,N}`, and
`subtype_tuple`'s length classification handles the `JL_VARARG_INT`/`BOUND`
kinds (:1846–1894) that Ruju currently lacks (it expands ground `Vararg{T,n}`
at construction, so only `BOUND` — typevar `N` — is missing;
design/implementation.md:375).

This is where `Tuple{Int,Int} <: Tuple{Int,Int,Vararg{Int,N}} where N`
(test/subtype.jl:85–86), `NTuple{N,Int}` pairs (:79–80), and
`(@UnionAll N Tuple{Int,Vararg{Int,N}})` equality (:70) live.

---

## 5. `local_forall_exists_subtype` and the two-union greedy path

### 5.1 `local_forall_exists_subtype(x, y, e, param, limit_slow)` (:2189–2268) [PIN]

Called from `forall_exists_equal` (:2349, 2352), `subtype_ccheck` (:869,
`limit_slow = 1`), and `equal_var` (:2285–2291). Four regimes:

1. `obviously_in_union(y, x)` → 1 (:2194–2195, the #49857 fast path).
2. Both sides ground (no free typevars) → a completely fresh
   `jl_subtype(x, y)` machine (:2196–2199).
3. Neither side mentions an in-scope **existential** var (:2200–2211) → run a
   full nested `forall_exists_subtype` with both union states zeroed, then
   restore `Runions` from a snapshot ("saves some bits in union stack") —
   safe because nothing here can constrain outer existentials.
4. Exactly one side is an existential typevar (:2213–2223) → loop over
   `Lunions` only (∀ passes), no env save/restore between passes — the bound
   updates in `var_lt`/`var_gt` *are* the accumulation.
5. General slow path (:2224–2267): save env; nested loops as in §1.3 but
   sharing the caller's `Runions` counter relative to `oldRmore`, with two
   heuristics:
   - **Freeze**: after a successful inner ∀ step that discovered no new right
     decisions (`Runions.more == oldRmore`) — or when `limited` — commit:
     `ini_count = count`, snapshot `Lunions`, `re_save_env`, reset
     `Runions.more = oldRmore` (:2245–2251). Later right-flips resume from the
     frozen ∀ prefix (:2234–2235) instead of restarting at pass 0.
   - **`limit_slow`**: `count` counts ∀ passes saturating at 4; when
     `limit_slow` and `count == 4`, set `limited = 1` (:2239–2242), which
     forces freezing and, at exit, hides newly-discovered right decisions from
     the caller (:2262–2265). `limit_slow == -1` resolves to `kindx || kindy`
     (:2224–2225).

**Semantics-bearing or optimization?** [INF] Regimes 1–4 are pure
optimizations/specializations (regime 3's fresh machine is exactly the
semantics; regime 4 is sound because with only one existential side there is
no cross-side ∃ choice to backtrack). In regime 5, the *freeze on
`more == oldRmore`* is pure optimization (nothing new to backtrack). The
**`limited` path is lossy by design**: freezing ∃ decisions for earlier ∀
branches when right decisions *do* remain, and hiding pending right decisions
from the caller, prunes combinations an unlimited search would try — it can
only flip answers from `true` to `false` (incompleteness), never unsound
`true`. The pin accepts this as an explosion guard. A faithful-partial port
may omit `limit_slow` entirely (always unlimited) at the cost of worst-case
exponential time — the correct-answer set only grows. Also note
`env_unchanged` (:811–840) includes a became-diagonal check (:829–833), not
just bounds equality.

### 5.2 `forall_exists_equal` (:2311–2357) [PIN]

Order of gates: `obviously_egal` (:2313); the definite-vs-indefinite tuple
length short-circuit (:2315–2317, via :2156–2177); the same-name nested
constructor fast path (:2319–2329 — non-tuple same-name datatypes forward to
one `subtype(x, y, PARAM_INVARIANT)` since parameter comparison is already
symmetric; Ruju has this, subtype.rs:588–595); **the two-union greedy path**
(:2331–2339); the `equal_var` fast path (:2341–2345, offset-0 only); then the
general `local_forall_exists_subtype(x, y, PARAM_INVARIANT, −1)` and, with
`flip_offset` around it, `(y, x, PARAM_NONE, 0)` (:2347–2354), with `Lunions`
shielded by push/pop (:2347, 2355).

**The two-union greedy path**: when both sides are unions, *register a right
decision point* — `if (pick_union_decision(e, 1) == 0)` — and on the 0 branch
compare componentwise (`a.a == b.a && a.b == b.b`, :2335–2338). If that fails,
the failure propagates and the **outer** `exists_subtype`/local loop flips
that very bit, so the retry takes the general path. The greedy attempt is
itself a choice in the global machine (comment :2332–2334: "If failed,
`exists_subtype` would memorize that this branch should be skipped"). This is
why the greedy path **cannot be ported without the machine**: locally it is a
wrong `false` whenever unions are equal but not aligned componentwise
(e.g. `Ref{Union{A,B}} == Ref{Union{B,A}}` after non-normalized construction).
[INF] With Ruju's normalized union construction, aligned components are the
common case, which is why its absence hasn't bitten the oracle yet
(implementation.md finding 13 notes it as still absent).

`equal_var` (:2270–2309): merged `var_gt`+`var_lt` for `TypeVar == Type` that
avoids the duplicated `<:` check: for an existential binding, `ccheck(x, ub)`,
`lb = simple_join(lb, x)`, then `ccheck(lb, x)` and set `ub = x` directly
("skip `simple_meet` here as we have proven `x <: vb->ub`", :2305–2307).
Optimization; the general path is equivalent. [INF]

---

## 6. Staged port plan

Ordering principle: slice 1 must heal the two self-reporting divergences;
every slice keeps `cargo test -p ruju-runtime`, `node runtime/harness.mjs`,
and the oracle green, and extends the oracle from the cited tranche.

### Slice 1 — the union-state machine and driver loops (heals :371 and :410)

**Adds**: `UnionState` (bit-stack over a `Vec<u32>`; `depth`/`more`/`used` as
i16 or i32) ×2 in `Env`; `statestack_get/set`, `next_union_state`,
`pick_union_decision`, `pick_union_element` (:203–271); `SavedEnv` =
per-binding `(lb, ub, occurs_cov, cov_diag)` + `rdepth`
(`save_env`/`re_save_env`/`restore_env`, :319–320, 351–479 — omit
`occurs_inv`/`max_offset`/`innervars` slots until their fields exist);
`exists_subtype` + `forall_exists_subtype` (:2359–2404) as the entry driver
(`subtype()` in subtype.rs:73–79 becomes `init_stenv` + `forall_exists_subtype`);
replace subtype.rs:87–100's inline splits with `pick_union_element` and the
dispatch-order fixes: `obviously_egal` guard, typevar-right fast path
(:1908–1931, minus the intersection arm), `obviously_in_union` (:614–641,
:1935), UnionAll-left priority (:1937–1938), the typevar-left
split-or-not decision (:1940–1948). In `forall_exists_equal`, replace the
save/&&-restore (subtype.rs:596–601) with the regime-2/3 subset of
`local_forall_exists_subtype`: ground → fresh machine; no in-scope existential
on either side → nested `forall_exists_subtype` with shielded union states
(:2196–2211); otherwise, for this slice, ∀-loop over `Lunions` with env
save/restore per right flip **without** the freeze/limit heuristics (regime 5
minus :2239–2251's `count` machinery — correct, possibly slower). `ccheck`
gains the `Lunions` shield (:862, 871).

**Verifies**: the two `knownDivergences` flip to `FIXED (promote to cases)` in
`verify_julia_subtype.mjs` (:389–394). Promote them; add test/subtype.jl:373–374
(`Tuple{Int,Int8,Int} <: Tuple{Vararg{Union{Int,Int8}}}`), :400–406 (the X/Y
8-way-union stress — also a performance canary for the counter), :449, and
:445–446 (the convert-pattern cases exercising the :1940–1948 gate).

**Risks**: the ∀-accumulate / ∃-rollback asymmetry (§1.3) is easy to invert —
`re_save_env` after ∀ success, `restore_env` between ∃ attempts; getting it
backwards passes most tests and corrupts cross-arm constraint accumulation.
`Runions.depth` must ride in `SavedEnv` (:382/:476). The diagonal machinery
(`ccheck` scopes, `cov_diag`) must be *re-run* per pass — counters are part of
the snapshot, so a pass that flips arms re-derives diagonality; forgetting to
save/restore the counters breaks `Tuple{T,T}` cases the oracle already pins.
The existing recursion is compatible (the C is recursive too; only the entry
and equal paths gain loops).

### Slice 2 — full `local_forall_exists_subtype` + `forall_exists_equal` tail

**Adds**: regimes 1 and 4 (:2194–2195, 2213–2223); the freeze heuristic and
`env_unchanged` (:811–840, 2245–2251, 2262–2265); optionally `limit_slow`
(document as the pin's explosion guard; acceptable to land disabled);
the two-union greedy path (:2331–2339) — now sound because slice 1's machine
owns the flipped bit; `equal_var` (:2270–2309); the definite/indefinite tuple
gate (:2315–2317); `push_forall_bound_scope` (:957–983) in `var_lt`/`var_gt`
(:1040–1043, 1090–1093) and the `occurs_inv` counter + `record_var_occurrence`
invariant arm (:894–904), which slice 1 exposed to more traffic.

**Verifies**: test/subtype.jl:452 (`Ref{Tuple{Union{Int,Int8},Int16}}` ==
`Ref{Union{...}}` — :371's property in invariant position), :453–454, :457–458
(shared `S` across union arms under `Ref`), plus the consistency-scope
diagonal case :127–129. All expressible in the current ABI (Ref→Box).

**Risks**: `occurs_inv` interacts with the envout-free port (it also gates
`widen_Type_if_concrete`, :1393 — skip that line until `TypeEq` widening
exists); the greedy path must only fire when a right-union decision can be
revisited, i.e. never behind regime-2's fresh machine without its own state.

### Slice 3 — bounded varargs: `Vararg{T,N}` with typevar `N`

**Adds**: `Loffset` on `Env` + `flip_offset` (:138–140, 481); the long-vs-long
comparisons (:848–849, 877–878, 2151–2152); `subtype_var`'s constant folding
(:1122–1131) and the `var_lt`/`var_gt` offset guards (:1032–1035, 1083–1086);
`intvalued` + `max_offset` fields; `check_vararg_length` (:1568–1583); the
full `subtype_tuple` length classification (`JL_VARARG_INT/BOUND`, :1846–1894)
and `subtype_tuple_tail`'s :1828–1832; `subtype_tuple_varargs`' N-equation
(:1594–1735). Requires boxed `Int64` values as type parameters in `types.rs`
(N in a Vararg) — an ABI addition (`rj_vararg_n` already takes a BigInt but
expands at construction; typevar-N needs the unexpanded form).

**Verifies**: test/subtype.jl:70, 79–80, 85–86 (NTuple ≈ `Tuple{Vararg{T,N}}`
is constructible via the ABI's vararg + where), :632 (`Tuple{} <: NTuple{N}`).

**Risks**: `forall_exists_equal` under nonzero `Loffset` must skip the
`equal_var` fast path (:2341's `e->Loffset == 0` guard) — porting slice 2
first makes this a one-line guard rather than a redesign. `max_offset`
save/restore in the saved-env layout changes `SavedEnv`'s stride (add the
fourth byte now or in slice 2).

### Slice 4 — the `Intersect` meet node + `concrete` propagation

**Adds**: an `Intersect` value kind in `types.rs` (two fields, never uniqued,
never user-visible — mirror jltypes.c:3348–3354's "recognized by identity"
note); `simple_meet`'s three-mode contract (`overesi` 0/1/2) with the
`simple_intersect` subsumption/disjointness analysis (jltypes.c:864–960) or a
faithful-partial: keep Ruju's ground subsumption, produce `Intersect` when
neither side subsumes and not obviously disjoint; `widen_intersect`
(:789–800) called at `subtype_unionall` exit (:1428–1432); the `x <: a ∩ b`
right-hand rule (:1952–1961); `is_leaf_bound` returns false for it (:1142);
and the `concrete` field with its propagation (:1405–1420) — closing the
"concrete-flag propagation still absent" note (implementation.md finding 15).

**Verifies**: no existing oracle line is known to require it (§3 [UNC]) — add
targeted cases from test_3's cross-bounded existentials (test/subtype.jl:333–339,
expressible with Box for Ptr) and assert unchanged answers on the full oracle;
the `concrete` propagation is pinned by :112–115-style diagonal-through-var
cases.

**Risks**: `Intersect` must never escape into a constructed type —
`widen_intersect` before any `make_typevar`/`unionall_type` with `vb.ub`; in
Ruju today nothing builds result types from bounds except the kind rule's
fresh `Type{T'} where T'` (subtype.rs:156–158), so the exposure is small but
will grow with envout (slice 5).

### Slice 5 — `envout` (`jl_subtype_env`) for dispatch/intersection

**Adds**: `envout`/`envsz`/`envidx` on `Env` (:131–133), the fill logic at
`subtype_unionall` exit (:1489–1560) including `wrap_tvar_env` (:1224–1227),
cross-∀-arm merging (:1538–1559), `exists_subtype`'s envout preservation
(:2369–2375), `restore_env`'s envout clearing (:477–478), and
`tainted_inner`/`innervars`/`unalias_unionall` (:1299–1327, 1440–1487) as the
bounds-leak bookkeeping becomes observable. This is the doorway to
`jl_type_intersection` and real method matching (`gf.c`), not an oracle item.

**Verifies**: new ABI (`rj_subtype_env`) + cases mirroring
`jl_subtype_matching`; the subtype oracle must stay bit-identical.

---

## 7. Risk register

1. **Save/restore discipline vs the Vec env.** The C saves only
   bounds+counters of the bindings live *at save time* (linked-list walk,
   :367–380) and relies on strict push/pop balance so restore points see the
   same bindings. Ruju's `e.vars.clone()` (subtype.rs:94, 596) accidentally
   also snapshots *length*, which is stronger; keep it, but the new machine
   must snapshot **at matched binding depths** — `exists_subtype`'s restore
   targets the ∀-loop's snapshot taken *outside* any binding pushed during the
   pass. Off-by-one-frame restores will silently mis-accumulate. [INF]
2. **`Runions.depth` in the snapshot.** Restoring the env without restoring
   `rdepth` (:382, 476) desynchronizes the bit cursor from the re-traversal in
   nested contexts (`local_forall_exists_subtype` resumes at nonzero depth).
   This has no analog in the current Rust code and is the most likely
   "mysteriously wrong on case 3 of 4" bug class. [INF]
3. **Counters are state, not derived.** `occurs_cov`/`cov_diag` (and later
   `occurs_inv`/`max_offset`) must be in the saved record (:319–320); Ruju's
   ccheck currently manages them ad hoc (subtype.rs:279–293). When the machine
   re-runs a pass, stale counters from a failed sibling pass would fabricate
   diagonality. [INF]
4. **Recursion is fine; the entry points move.** The C `subtype` is recursive
   too — no CPS rewrite needed. But `types::issubtype` is called *reentrantly*
   from `union_of`'s subsumption dedup (types.rs:787–828) and from
   `simple_meet`'s ground checks (subtype.rs:618–624); each such call must
   spin up a **fresh machine** (as `jl_subtype` does, :2912–2915), never share
   the live env's union states. Today that's implicit (fresh `Env`); after the
   port, make the separation explicit or the outer counters get corrupted. [INF]
5. **GC rooting — pre-existing exposure, widened by this work.**
   subtype.rs contains no `Rooted`/`Frame` at all (verified by grep). Existing
   allocation sites reachable mid-query: `simple_join` → `types::union_type`
   (subtype.rs:643), and the kind rule's `make_typevar`/`type_type`/
   `unionall_type` (subtype.rs:156–158) — all called with unrooted query
   types, bindings' `lb`/`ub`, and the caller's whole recursion spine of
   `Offset`s. Any collection triggered by those allocations can move nothing
   (non-moving GC) but can **free** unreachable-from-roots objects — the query
   types themselves are only safe if the JS caller's offsets happen to be
   rooted elsewhere; in `verify_julia_subtype.mjs` they are not. The engine
   work widens this: saved envs at *multiple loop levels* hold `lb`/`ub`
   copies across arbitrarily many allocating passes (the C roots exactly
   these: `jl_savedenv_t.roots`/gcframe, :331–337, 385–414), and slices 3–4
   add allocation *inside* the hot path (boxed longs at :1124/:1718–1722,
   `Intersect` nodes at :768). Port the C's discipline: the Rust `SavedEnv`
   and the live `Env` must be GC roots (a shadow-stack frame owned by the
   machine entry, or registering `Env` in a root list), and flag the
   pre-existing `union_type` exposure in implementation.md regardless. [INF;
   the auto-collect stress test per CLAUDE.md is the enforcement vehicle]
6. **Interaction with already-ported semantics.** (a) The diagonal rule: the
   machine re-derives `cov_count` per pass — the existing `cov_diag` folding
   (subtype.rs:279–293 vs :925–947) is compatible, but `subtype_ccheck` must
   switch to the machine-aware form (:846–873) in slice 1 or diagonal answers
   can flip between passes. (b) The `Type{T}` kind rules (subtype.rs:137–163)
   sit *after* union handling in both codebases; the pin's versions live at
   :2081–2122 phrased on `TypeEq` — no reordering needed, but the fresh
   `Type{T'} where T'` allocation lands inside the loops now (risk 5).
   (c) Ruju's `Bottom` is a plain DataType, not `TypeofBottom` — the pin's
   :2081–2087 TypeofBottom arm stays structurally absorbed by the Bottom-left
   fast path (recorded divergence, implementation.md:299). [INF]
7. **Pin-vs-upstream drift.** The pin's `TypeEq` (julia.h:1642) and
   `Intersect` (julia.h:595–604) nodes and `jl_is_typeapp` (subtype.c:2873)
   are not in upstream JuliaLang/julia's `subtype.c` shape as of the training
   horizon; implementation.md already records the TypeEq phrasing as
   pin-specific (:299). When the next audit compares against *live* upstream
   (per CLAUDE.md's working notes), expect these regions to differ textually
   while the union machine (§1) matches upstream closely. [UNC — verify
   against live upstream during the audit, not from memory]
8. **Performance canary.** The machine is worst-case exponential in discovered
   decision points; the pin's guards (`limit_slow`, freeze, `env_unchanged`,
   `obviously_*` fast paths, the :1797–1815 repeated/separable tuple element
   fast paths keyed on `Runions.depth == 0`) exist for real workloads. For the
   oracle's scale none are needed for correctness; land test/subtype.jl:400–406
   as the canary and add guards only when it gets slow. [INF]
