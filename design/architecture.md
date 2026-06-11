# Architecture

How Ruju's runtime is organized today — the modules that exist and how they
fit together. This describes the current implementation; the plan lives in
`roadmap.md`, and per-subsystem status and fidelity in `ledger.md`.

```mermaid
flowchart TD
    LIB["lib.rs — rj_ WASM ABI + init"]
    LIB --> FE["frontend.rs — lex / parse / lower"]
    FE --> INTERP["interp.rs — eval lowered IR"]
    INTERP --> DISP["dispatch.rs — multiple dispatch"]
    INTERP --> VAL["value.rs — boxing"]
    INTERP --> INTR["intrinsics crate — arithmetic / comparison"]
    DISP --> SUB["subtype.rs — subtyping (UnionAll / TypeVar)"]
    SUB <--> TYPES["types.rs — DataTypes & hierarchy"]
    VAL --> OBJ["object.rs — tagged values"]
    TYPES --> OBJ
    TYPES --> SYM["symbol.rs — interned symbols"]
    OBJ --> GC["gc.rs — generational mark-sweep"]
    OBJ --> REGION["region.rs — bounded memory region"]
    GC --> REGION
```

## Components

| Module | Role |
| - | - |
| `lib.rs` | the `rj_`-prefixed WASM ABI and runtime initialization |
| `frontend.rs` | hand-written bootstrap lexer / parser / lowering for a subset of Julia source |
| `interp.rs` | tree-walking interpreter over lowered IR |
| `dispatch.rs` | multiple dispatch — method table, applicability, specificity |
| `subtype.rs` | subtyping, including the `where` machinery (`UnionAll` / `TypeVar`) |
| `types.rs` | `DataType`s, the type hierarchy, tuples / unions / parametrics, uniquing |
| `value.rs` | boxing and unboxing of primitive values |
| `object.rs` | the tagged-value model — every object headers its `DataType` |
| `symbol.rs` | interned (immortal) symbols |
| `gc.rs` | generational, pooled mark-sweep GC with shadow-stack rooting |
| `region.rs` | the single bounded region of WASM linear memory (offset-based references) |
| `intrinsics` (crate) | pure arithmetic and comparison intrinsics |
