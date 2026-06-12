//! Garbage collection: shadow-stack rooting and a precise, non-moving
//! mark-and-sweep collector.
//!
//! Rooting uses an explicit shadow stack — WebAssembly exposes no way to scan
//! the call stack, so every root is tracked explicitly, the WASM-mandatory form
//! of Julia's `JL_GC_PUSH`/`JL_GC_POP` gcframe mechanism, here expressed through
//! a [`Rooted`] RAII guard (see the GC-rooting decision in
//! `design/strategy.md`).
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
    /// Live + freshly allocated bytes (`gc_heap_stats.heap_size`).
    heap_size: Cell<usize>,
    /// Collect when `heap_size` reaches this (`gc_heap_stats.heap_target`,
    /// checked at allocation — `gc-stock.c:356`).
    heap_target: Cell<usize>,
    /// Young bytes promoted since the last full sweep (`promoted_bytes`).
    promoted: Cell<usize>,
    /// `heap_size` as of the last full sweep (`heap_size_after_last_full_gc`).
    size_after_full: Cell<usize>,
    /// The full-vs-quick decision for the next automatic collection
    /// (`next_sweep_full`).
    next_full: Cell<bool>,
}
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Heap {}
static HEAP: Heap = Heap {
    pools: UnsafeCell::new(Vec::new()),
    pages: UnsafeCell::new(Vec::new()),
    remembered: UnsafeCell::new(Vec::new()),
    live: Cell::new(0),
    heap_size: Cell::new(0),
    heap_target: Cell::new(0),
    promoted: Cell::new(0),
    size_after_full: Cell::new(0),
    next_full: Cell::new(false),
};

/// The minimum collection target (`default_collect_interval`,
/// `gc-stock.c:33–35` — Julia's ~12 MiB on 32-bit, scaled to the bounded
/// region).
fn collect_interval_floor() -> usize {
    region::capacity() / 8
}

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
    HEAP.heap_size.set(0);
    HEAP.heap_target.set(collect_interval_floor());
    HEAP.promoted.set(0);
    HEAP.size_after_full.set(0);
    HEAP.next_full.set(false);
}

/// Live + freshly allocated bytes (introspection).
#[allow(dead_code)]
pub fn heap_size() -> usize {
    HEAP.heap_size.get()
}

/// The current collection target (introspection).
#[allow(dead_code)]
pub fn heap_target() -> usize {
    HEAP.heap_target.get()
}

/// `overallocation` (`gc-stock.c:3032–3050`): the permitted growth before
/// the next full collection — `4·n^(7/8) + n/8`, superlinear for small heaps
/// and ~12.5% for large ones, capped at 5% of `max_val` once it would
/// exceed it.
fn overallocation(old_val: u64, val: u64, max_val: u64) -> u64 {
    if old_val == 0 {
        return collect_interval_floor() as u64;
    }
    let exp2 = 64 - old_val.leading_zeros() as u64;
    let mut inc = (1u64 << (exp2 * 7 / 8)) * 4 + old_val / 8;
    if inc + val > max_val && inc > max_val / 20 {
        inc = max_val / 20;
    }
    inc
}

/// The allocation-time collection trigger (`gc-stock.c:356`): proactive at
/// the heap target, full or quick per `next_sweep_full`. Called before each
/// allocation once the core types exist.
pub fn maybe_collect() {
    if types::is_bootstrapped() && HEAP.heap_size.get() >= HEAP.heap_target.get() {
        collect_inner(HEAP.next_full.get());
    }
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
        HEAP.heap_size.set(HEAP.heap_size.get() + osize);
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
    HEAP.heap_size.set(HEAP.heap_size.get() + osize);
    head
}

// --- write barrier ----------------------------------------------------------

/// The write barrier (`jl_gc_wb`, `gc-wb-stock.h:14`): fires only when the
/// parent is `GC_OLD_MARKED` (3 — old *and* already scanned, so its young
/// references would otherwise be missed) and the child's mark bit is clear.
/// A merely-`OLD` (2) parent needs no barrier: it gets its promotion-
/// completion scan at the next mark regardless.
pub fn write_barrier(parent: Value, child: Value) {
    if !child.is_null()
        && object::gc_bits(parent) == (BIT_OLD | BIT_MARKED)
        && object::gc_bits(child) & BIT_MARKED == 0
    {
        queue_root(parent);
    }
}

/// `jl_gc_queue_root` (`gc-stock.c:1493`): clear the parent's OLD bit — the
/// `OLD_MARKED` parent becomes `MARKED` ("in remset"), which is itself the
/// at-most-once guard (a second store sees bits ≠ 3 and does not refire) —
/// then push it on the remset.
fn queue_root(parent: Value) {
    object::set_gc_bits(parent, BIT_MARKED);
    remembered().push(parent);
}

/// Number of remset entries (introspection for tests).
#[allow(dead_code)]
pub fn remset_len() -> usize {
    remembered().len()
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
    // `nothing` is reached through `Nothing.instance` (traced via the DataType
    // bitmap); the Bool permboxes are reachable only through `Builtins`.
    work.push(Value(b.true_instance));
    work.push(Value(b.false_instance));
    work.push(Value(b.tuple_typename)); // shared across all tuple types
    work.push(Value(b.box_typename)); // demo parametric constructor
    crate::symbol::each_interned(|s| work.push(Value(s))); // symbols are immortal
    crate::dispatch::each_sig(|s| work.push(Value(s))); // method signatures
    crate::types::each_registered_struct(|t| work.push(Value(t))); // source-defined types
}

/// Visit every reference embedded in `v` (its type, then svec elements or
/// the layout's pointer fields).
fn each_ref(v: Value, mut f: impl FnMut(Value)) {
    let t = object::type_of(v);
    if t != region::NULL {
        f(Value(t)); // the type is itself a heap object
    }
    if types::is_svec(t) {
        for i in 0..types::svec_len(v.raw()) {
            f(Value(types::svec_ref(v.raw(), i)));
        }
    } else {
        for i in 0..types::layout_npointers(t) {
            f(object::get_ref(v, types::layout_ptr_offset(t, i)));
        }
    }
}

/// Push the objects that `v` references onto the work list.
fn trace_fields(v: Value, work: &mut Vec<Value>) {
    each_ref(v, |r| work.push(r));
}

/// Whether `v` references any young object (OLD bit clear) — the `nptr`
/// young-ref bit computed while scanning an old object
/// (`gc_mark_outrefs`; a young child seen at scan time may promote at the
/// coming sweep, in which case the conservative remset entry is dropped at
/// the next mark, as in Julia).
fn has_young_ref(v: Value) -> bool {
    let mut young = false;
    each_ref(v, |r| {
        if !r.is_null() && object::gc_bits(r) & BIT_OLD == 0 {
            young = true;
        }
    });
    young
}

/// Mark reachable objects — the same rule for minor and full collections
/// (`gc_setmark_tag`, `gc-stock.c:245`): an object whose mark bit is set
/// (1 or 3) is skipped; an unmarked `OLD` (2) object gets its
/// promotion-completion scan (2 → 3 **and trace** — the one scan that finds
/// a newly promoted object's young references); `CLEAN` becomes `MARKED`.
/// The generational win is that steady-state old objects sit at 3 and are
/// skipped; old → young edges created after that scan are exactly what the
/// remset carries.
fn mark() {
    let mut work: Vec<Value> = Vec::new();
    push_roots(&mut work);
    // Remset entries sit at MARKED (queue_root left them there): restore each
    // to OLD_MARKED and trace its fields (`gc-stock.c:2834`). This runs for
    // full collections too — the entries' young children must be reached.
    // The remset is *rebuilt*, not cleared (`gc_mark_push_remset`,
    // `gc-stock.c:1613`): any old object scanned this cycle that still
    // references a young object is pushed for the next cycle.
    let remset: Vec<Value> = core::mem::take(remembered());
    for p in remset {
        object::set_gc_bits(p, BIT_OLD | BIT_MARKED);
        trace_fields(p, &mut work);
        if has_young_ref(p) {
            remembered().push(p);
        }
    }
    while let Some(v) = work.pop() {
        if v.is_null() {
            continue;
        }
        let bits = object::gc_bits(v);
        if bits & BIT_MARKED != 0 {
            continue; // marked this cycle (or restored from the remset)
        }
        object::set_gc_bits(v, bits | BIT_MARKED); // 0 → 1, 2 → 3
        trace_fields(v, &mut work);
        if bits & BIT_OLD != 0 && has_young_ref(v) {
            remembered().push(v); // promotion scan found old → young edges
        }
    }
}

fn free_slot(slot: Offset, pool: usize) {
    set_next_free(slot, pools()[pool].freelist);
    pools()[pool].freelist = slot;
}

/// Sweep by walking the pages (the state transitions of `gc-stock.c:164–191`).
/// Quick sweep: `CLEAN` is freed; `MARKED` young survivors promote (the
/// one-survival placeholder for Julia's `PROMOTE_AGE`); `OLD` and
/// `OLD_MARKED` are untouched — old garbage waits for a full collection.
/// Full sweep: anything with the mark bit clear (`CLEAN` *or* `OLD`) is
/// freed; survivors promote; `OLD_MARKED` demotes to `OLD`, so the *next*
/// full cycle re-proves its liveness — old garbage at 3 takes one extra full
/// cycle to free, as in Julia. Returns the number of live objects.
fn sweep(full: bool) -> (usize, usize, usize) {
    for p in pools().iter_mut() {
        p.freelist = region::NULL;
    }
    // Snapshot (start, osize) so the pages list isn't borrowed while pools mutate.
    let page_info: Vec<(Offset, usize)> = pages().iter().map(|p| (p.start, p.osize)).collect();

    let mut survivors = 0;
    let mut live_bytes = 0;
    let mut promoted_bytes = 0;
    for (start, osize) in page_info {
        let pi = pool_for(osize);
        for k in 0..(PAGE_SIZE / osize) {
            let slot = start + (k * osize) as u32;
            let v = Value(slot + HEADER_SIZE as u32);
            let bits = object::gc_bits(v);
            let live = bits & BIT_MARKED != 0 || (!full && bits & BIT_OLD != 0);
            if live {
                if bits == BIT_MARKED {
                    object::set_gc_bits(v, BIT_OLD); // promote young survivor
                    promoted_bytes += osize;
                } else if full && bits == (BIT_OLD | BIT_MARKED) {
                    object::set_gc_bits(v, BIT_OLD); // demote: re-prove next full cycle
                }
                // quick sweep leaves OLD and OLD_MARKED untouched
                survivors += 1;
                live_bytes += osize;
            } else {
                free_slot(slot, pi);
            }
        }
    }
    (survivors, live_bytes, promoted_bytes)
}

fn collect_inner(full: bool) -> u32 {
    mark();
    let before = HEAP.live.get();
    let (survivors, live_bytes, promoted_bytes) = sweep(full);
    HEAP.live.set(survivors);
    HEAP.heap_size.set(live_bytes);

    // Post-sweep remset handling (`gc-stock.c:3405–3417`): after a quick
    // sweep, entries are put back in the queued state (`GC_MARKED`) so the
    // barrier does not refire on them; a full sweep clears the remset — its
    // old objects were demoted to `OLD` and will be rescanned at next mark.
    if full {
        remembered().clear();
    } else {
        for &p in remembered().iter() {
            object::set_gc_bits(p, BIT_MARKED);
        }
    }

    // Collection policy (`gc-stock.c:3377–3400`): track promotion since the
    // last full sweep; go full next time if the promoted ratio exceeds 0.15
    // or the heap outgrew the post-full-GC baseline by `overallocation`.
    // (Omitted: the user_max/under_pressure limits — no CLI options — and the
    // MemBalancer rate machinery behind Julia's `target_allocs`.)
    if full {
        HEAP.promoted.set(0);
        HEAP.size_after_full.set(live_bytes);
    } else {
        HEAP.promoted.set(HEAP.promoted.get() + promoted_bytes);
    }
    let heap_size = live_bytes as u64;
    let baseline = HEAP.size_after_full.get() as u64;
    let old_ratio = if heap_size == 0 { 0.0 } else { HEAP.promoted.get() as f64 / heap_size as f64 };
    let expected = baseline + overallocation(baseline, 0, region::capacity() as u64);
    HEAP.next_full.set(old_ratio > 0.15 || heap_size > expected);

    // The next target: current size plus permitted growth, floored
    // (`target_heap = target_allocs + heap_size`, floor
    // `default_collect_interval` — `gc-stock.c:3342–3346`, subset).
    let target = (heap_size + overallocation(heap_size, 0, region::capacity() as u64)) as usize;
    HEAP.heap_target.set(target.max(collect_interval_floor()));

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
