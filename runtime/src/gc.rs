//! Garbage collection: shadow-stack rooting and a precise, non-moving
//! mark-and-sweep collector.
//!
//! Rooting uses an explicit shadow stack — WebAssembly exposes no way to scan
//! the call stack, so every root is tracked explicitly, the WASM-mandatory form
//! of Julia's `JL_GC_PUSH`/`JL_GC_POP` gcframe mechanism, here expressed through
//! a [`Rooted`] RAII guard (see `design/runtime-aot-and-gc.md`).
//!
//! Collection is mark-and-sweep over the bounded region, after `gc-stock.c`'s
//! algorithm (non-moving). The mark roots are the shadow stack plus the pinned
//! builtin types and the `nothing` singleton; tracing follows each object's
//! DataType pointer bitmap. Sweeping returns unmarked chunks to a free list that
//! [`object::alloc`](crate::object::alloc) reuses, so the region stops being
//! append-only. Allocation is size-classed only by exact reuse for now; the
//! size-classed *pools* of `gc-stock.c` are a later refinement.

use core::cell::{Cell, UnsafeCell};

use crate::object::{self, Value, HEADER_SIZE};
use crate::region::{self, Offset};
use crate::types;

/// GC state, encoded in the header's low 2 bits (after Julia's mark bits):
/// `0b00` young-unmarked, `0b01` young-marked, `0b10` old, `0b11` old-marked.
const BIT_MARKED: u32 = 1;
const BIT_OLD: u32 = 2;

// --- shadow stack (roots) ---------------------------------------------------

struct Shadow {
    slots: UnsafeCell<Vec<Value>>,
}
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Shadow {}
static SHADOW: Shadow = Shadow {
    slots: UnsafeCell::new(Vec::new()),
};

#[inline]
fn slots() -> &'static mut Vec<Value> {
    unsafe { &mut *SHADOW.slots.get() }
}

/// A RAII GC root. While alive, its [`Value`] occupies a slot on the shadow
/// stack and is treated as a root; dropping it pops the slot. Roots are released
/// in LIFO order, which `Drop` enforces naturally.
pub struct Rooted {
    index: usize,
}

impl Rooted {
    /// Root `v`, pushing a slot onto the shadow stack.
    pub fn new(v: Value) -> Rooted {
        let s = slots();
        let index = s.len();
        s.push(v);
        Rooted { index }
    }

    /// The currently rooted value.
    pub fn get(&self) -> Value {
        slots()[self.index]
    }

    /// Replace the rooted value, e.g. after an allocation produces a new object.
    #[allow(dead_code)] // needed once nested allocation updates roots
    pub fn set(&self, v: Value) {
        slots()[self.index] = v;
    }
}

impl Drop for Rooted {
    fn drop(&mut self) {
        let s = slots();
        debug_assert_eq!(self.index + 1, s.len(), "GC roots must be released LIFO");
        s.pop();
    }
}

/// The number of live roots on the shadow stack.
pub fn root_count() -> usize {
    slots().len()
}

/// A contiguous block of GC roots — the analog of a Julia gcframe with several
/// slots (`JL_GC_PUSHARGS`). The interpreter keeps its locals and SSA values in
/// one, so the whole working set is rooted for the duration of evaluation. Like
/// [`Rooted`], frames must be released in LIFO order, enforced on `Drop`.
pub struct Frame {
    base: usize,
    len: usize,
}

impl Frame {
    /// Push a frame of `n` null-initialized root slots.
    pub fn new(n: usize) -> Frame {
        let s = slots();
        let base = s.len();
        s.resize(base + n, Value::NULL);
        Frame { base, len: n }
    }

    /// Read slot `i` of the frame.
    pub fn get(&self, i: usize) -> Value {
        slots()[self.base + i]
    }

    /// Write slot `i` of the frame.
    pub fn set(&self, i: usize, v: Value) {
        slots()[self.base + i] = v;
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let s = slots();
        debug_assert_eq!(self.base + self.len, s.len(), "GC frames must be released LIFO");
        s.truncate(self.base);
    }
}

// --- pooled heap (size-classed pools over pages, after gc-stock.c) ----------

/// Page size; pages are carved from the region and hold one size class each
/// (`GC_PAGE_SZ`, here with `GC_PAGE_LG2 = 12`).
const PAGE_SIZE: usize = 4096;

/// Object size classes in bytes (header + data, multiples of 16). Allocations
/// round up to the smallest class that fits; anything larger rounds to a
/// multiple of 16 and gets its own pool.
const SIZE_CLASSES: &[usize] = &[16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024];

/// A per-size-class pool: a free list threaded through free slots, plus the slot
/// size (the analog of `jl_gc_pool_t`).
struct Pool {
    osize: usize,
    /// Head of the free list (`0` = empty); each free slot's first word holds
    /// the offset of the next free slot.
    freelist: Offset,
}

/// Page metadata: where a page lives and which size class it serves (the analog
/// of `jl_gc_pagemeta_t`).
struct Page {
    start: Offset,
    osize: usize,
}

struct Heap {
    pools: UnsafeCell<Vec<Pool>>,
    pages: UnsafeCell<Vec<Page>>,
    /// Old objects that have been mutated to reference a young object — the
    /// roots a minor collection must scan in addition to the shadow stack.
    remembered: UnsafeCell<Vec<Value>>,
    live: Cell<usize>,
}
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Heap {}
static HEAP: Heap = Heap {
    pools: UnsafeCell::new(Vec::new()),
    pages: UnsafeCell::new(Vec::new()),
    remembered: UnsafeCell::new(Vec::new()),
    live: Cell::new(0),
};

fn pools() -> &'static mut Vec<Pool> {
    unsafe { &mut *HEAP.pools.get() }
}

fn pages() -> &'static mut Vec<Page> {
    unsafe { &mut *HEAP.pages.get() }
}

fn remembered() -> &'static mut Vec<Value> {
    unsafe { &mut *HEAP.remembered.get() }
}

/// Clear all heap bookkeeping. Called when the region is reset.
pub fn reset_heap() {
    pools().clear();
    pages().clear();
    remembered().clear();
    HEAP.live.set(0);
}

/// Number of live (uncollected) objects.
pub fn live_objects() -> usize {
    HEAP.live.get()
}

/// The slot size (size class) for an allocation of `total` bytes.
fn size_class(total: u32) -> usize {
    let t = total as usize;
    for &c in SIZE_CLASSES {
        if t <= c {
            return c;
        }
    }
    (t + 15) & !15
}

/// Index of the pool serving `osize`, creating it on first use.
fn pool_for(osize: usize) -> usize {
    if let Some(i) = pools().iter().position(|p| p.osize == osize) {
        return i;
    }
    pools().push(Pool { osize, freelist: region::NULL });
    pools().len() - 1
}

fn next_free(slot: Offset) -> Offset {
    unsafe { *region::ptr_mut::<u32>(slot) }
}

fn set_next_free(slot: Offset, next: Offset) {
    unsafe {
        *region::ptr_mut::<u32>(slot) = next;
    }
}

/// Obtain a slot of at least `total` bytes from the appropriate pool, carving a
/// fresh page from the region if the pool's free list is empty. Returns the slot
/// (header) offset, or [`region::NULL`] if the region cannot supply a new page.
/// Mirrors `jl_gc_pool_alloc`.
pub fn alloc_chunk(total: u32) -> Offset {
    let osize = size_class(total);
    let pi = pool_for(osize);

    // Fast path: pop the pool's free list.
    let head = pools()[pi].freelist;
    if head != region::NULL {
        pools()[pi].freelist = next_free(head);
        HEAP.live.set(HEAP.live.get() + 1);
        return head;
    }

    // Slow path: carve a fresh page and thread its slots onto the free list.
    let page = region::alloc(PAGE_SIZE);
    if page == region::NULL {
        return region::NULL;
    }
    let mut s = page;
    for _ in 0..(PAGE_SIZE / osize) {
        set_next_free(s, pools()[pi].freelist);
        pools()[pi].freelist = s;
        s += osize as u32;
    }
    pages().push(Page { start: page, osize });

    let head = pools()[pi].freelist;
    pools()[pi].freelist = next_free(head);
    HEAP.live.set(HEAP.live.get() + 1);
    head
}

// --- write barrier ----------------------------------------------------------

/// Record an old → young store so a minor collection still finds the young
/// object. Must be called before any mutation that stores a reference into a
/// possibly-old heap object (the analog of `jl_gc_wb`).
pub fn write_barrier(parent: Value, child: Value) {
    if !child.is_null()
        && object::gc_bits(parent) & BIT_OLD != 0
        && object::gc_bits(child) & BIT_OLD == 0
    {
        remembered().push(parent);
    }
}

/// Whether `v` is in the old generation.
#[allow(dead_code)] // GC introspection; used by tests
pub fn is_old(v: Value) -> bool {
    object::gc_bits(v) & BIT_OLD != 0
}

// --- mark and sweep ---------------------------------------------------------

fn push_roots(work: &mut Vec<Value>) {
    work.extend_from_slice(slots()); // shadow stack
    let b = types::builtins();
    for &t in b.types.iter() {
        work.push(Value(t));
    }
    work.push(Value(b.nothing_instance));
    work.push(Value(b.tuple_typename)); // shared across all tuple types
    work.push(Value(b.box_typename)); // demo parametric constructor
    crate::symbol::each_interned(|s| work.push(Value(s))); // symbols are immortal
    crate::dispatch::each_sig(|s| work.push(Value(s))); // method signatures
}

/// Push the objects that `v` references onto the work list.
fn trace_fields(v: Value, work: &mut Vec<Value>) {
    let t = object::type_of(v);
    if t != region::NULL {
        work.push(Value(t)); // the type is itself a heap object
    }
    if types::is_svec(t) {
        for i in 0..types::svec_len(v.raw()) {
            work.push(Value(types::svec_ref(v.raw(), i)));
        }
    } else {
        for i in 0..types::layout_npointers(t) {
            work.push(object::get_ref(v, types::layout_ptr_offset(t, i)));
        }
    }
}

/// Mark reachable objects. A minor mark does not trace into the old generation
/// (old objects are assumed live) except via the remembered set, which is where
/// any old → young edges are recorded; a full mark traces everything.
fn mark(full: bool) {
    let mut work: Vec<Value> = Vec::new();
    push_roots(&mut work);
    if !full {
        // Old objects with young children: trace them to reach those children.
        let remembered: Vec<Value> = remembered().clone();
        for p in remembered {
            trace_fields(p, &mut work);
        }
    }
    while let Some(v) = work.pop() {
        if v.is_null() {
            continue;
        }
        let bits = object::gc_bits(v);
        if !full && bits & BIT_OLD != 0 {
            continue; // minor: old objects are live and not retraced
        }
        if bits & BIT_MARKED != 0 {
            continue; // already marked
        }
        object::set_gc_bits(v, bits | BIT_MARKED);
        trace_fields(v, &mut work);
    }
}

fn free_slot(slot: Offset, pool: usize) {
    set_next_free(slot, pools()[pool].freelist);
    pools()[pool].freelist = slot;
}

/// Sweep by walking the pages. A minor sweep frees only dead young objects and
/// promotes the survivors to the old generation; a full sweep frees every
/// unmarked object (young or old). Survivors end up old and clean. Returns the
/// number of live objects. Mirrors `gc-stock.c`'s page sweep.
fn sweep(full: bool) -> usize {
    for p in pools().iter_mut() {
        p.freelist = region::NULL;
    }
    // Snapshot (start, osize) so the pages list isn't borrowed while pools mutate.
    let page_info: Vec<(Offset, usize)> = pages().iter().map(|p| (p.start, p.osize)).collect();

    let mut survivors = 0;
    for (start, osize) in page_info {
        let pi = pool_for(osize);
        for k in 0..(PAGE_SIZE / osize) {
            let slot = start + (k * osize) as u32;
            let v = Value(slot + HEADER_SIZE as u32);
            let bits = object::gc_bits(v);
            let live = if full {
                bits & BIT_MARKED != 0
            } else {
                // Minor: old objects always survive; young ones only if marked.
                bits & BIT_OLD != 0 || bits & BIT_MARKED != 0
            };
            if live {
                object::set_gc_bits(v, BIT_OLD); // promote/keep as old, clean
                survivors += 1;
            } else {
                free_slot(slot, pi);
            }
        }
    }
    survivors
}

fn collect_inner(full: bool) -> u32 {
    mark(full);
    let before = HEAP.live.get();
    let survivors = sweep(full);
    remembered().clear();
    HEAP.live.set(survivors);
    (before - survivors) as u32
}

/// Run a minor (young-generation) collection. Returns the number of objects
/// reclaimed.
pub fn collect() -> u32 {
    collect_inner(false)
}

/// Run a full (whole-heap) collection, reclaiming old garbage too.
pub fn collect_full() -> u32 {
    collect_inner(true)
}
