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

/// Page size: `GC_PAGE_SZ = 1 << GC_PAGE_LG2`, `GC_PAGE_LG2 = 14`
/// (`gc-stock.h:47–49`, the default non-`GC_SMALL_PAGE` configuration).
const PAGE_SIZE: usize = 1 << 14;

/// The pin's pool size classes (`jl_gc_sizeclasses`,
/// `julia_internal.h:544–586`, the 32-bit `MAX_ALIGN > 4` branch): 4 and 8,
/// sixteen 8-spaced classes, eight 16-spaced classes, then the
/// packing-optimized tail for 16 KiB pages. Anything above 2032
/// (`GC_MAX_SZCLASS` territory) belongs to the big-object path — until that
/// lands (GC tail slice C), oversize allocations round to 16 and get their
/// own pool, recorded as the remaining gap.
const SIZE_CLASSES: &[usize] = &[
    4, 8,
    16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 96, 104, 112, 120, 128, 136,
    144, 160, 176, 192, 208, 224, 240, 256,
    272, 288, 304, 336, 368, 400, 448, 496,
    544, 576, 624, 672, 736, 816, 896, 1008,
    1088, 1168, 1248, 1360, 1488, 1632, 1808, 2032,
];

/// A per-size-class pool (`jl_gc_pool_t`): the indices of this class's pages
/// that still have free slots.
struct Pool {
    osize: usize,
    /// Indices into the page table of pages with `nfree > 0`.
    page_q: Vec<usize>,
}

/// Per-page metadata (`jl_gc_pagemeta_t`, `gc-stock.h:100–128`): the
/// page-local free list, free count, and the mark/age bookkeeping that lets
/// the sweep skip or release whole pages without walking them.
struct Page {
    start: Offset,
    osize: usize,
    /// Head of this page's free list (slots threaded through their headers).
    freelist: Offset,
    /// Free slots remaining on this page (`nfree`).
    nfree: u32,
    /// Live objects on this page (maintained at allocation; recounted when
    /// the page is walked by a sweep — cached for skipped pages).
    live_n: u32,
    /// Any cell on this page was marked this cycle (`has_marked`; set during
    /// marking, cleared by the sweep).
    has_marked: bool,
    /// Any young cell was live (or allocated) on this page (`has_young`).
    has_young: bool,
    /// Old (`OLD_MARKED`) objects counted by this cycle's marks (`nold`).
    nold: u32,
    /// Old objects at the end of the previous full sweep (`prev_nold`).
    prev_nold: u32,
    /// Whether the page is in use (false = released to `free_pages`).
    in_use: bool,
}

struct Heap {
    pools: UnsafeCell<Vec<Pool>>,
    pages: UnsafeCell<Vec<Page>>,
    /// Indices of released pages, reusable by any pool (`!has_marked` pages
    /// are returned whole — `gc-stock.c:882–887`).
    free_pages: UnsafeCell<Vec<usize>>,
    /// Old objects that have been mutated to reference a young object — the
    /// roots a minor collection must scan in addition to the shadow stack.
    remembered: UnsafeCell<Vec<Value>>,
    /// Whether the previous sweep was full (`prev_sweep_full`, for the
    /// quick-sweep skip predicate — `gc-stock.c:890–892`).
    prev_full: Cell<bool>,
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
    /// Pages walked by the most recent sweep (test introspection).
    pages_walked: Cell<usize>,
}
// Sound only because the runtime is single-threaded under wasm32 for now.
unsafe impl Sync for Heap {}
static HEAP: Heap = Heap {
    pools: UnsafeCell::new(Vec::new()),
    pages: UnsafeCell::new(Vec::new()),
    free_pages: UnsafeCell::new(Vec::new()),
    remembered: UnsafeCell::new(Vec::new()),
    prev_full: Cell::new(false),
    live: Cell::new(0),
    heap_size: Cell::new(0),
    heap_target: Cell::new(0),
    promoted: Cell::new(0),
    size_after_full: Cell::new(0),
    next_full: Cell::new(false),
    pages_walked: Cell::new(0),
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

fn free_pages() -> &'static mut Vec<usize> {
    unsafe { &mut *HEAP.free_pages.get() }
}

fn remembered() -> &'static mut Vec<Value> {
    unsafe { &mut *HEAP.remembered.get() }
}

/// Clear all heap bookkeeping. Called when the region is reset.
pub fn reset_heap() {
    pools().clear();
    pages().clear();
    free_pages().clear();
    remembered().clear();
    HEAP.prev_full.set(false);
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
    pools().push(Pool { osize, page_q: Vec::new() });
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

/// Thread every slot of page `idx` onto its own free list (a fresh or
/// recycled page).
fn init_page_freelist(idx: usize) {
    let (start, osize) = (pages()[idx].start, pages()[idx].osize);
    let n = PAGE_SIZE / osize;
    let mut fl = region::NULL;
    for k in (0..n).rev() {
        let s = start + (k * osize) as u32;
        set_next_free(s, fl);
        fl = s;
    }
    let pg = &mut pages()[idx];
    pg.freelist = fl;
    pg.nfree = n as u32;
    pg.live_n = 0;
    pg.has_marked = false;
    pg.has_young = false;
    pg.nold = 0;
    pg.prev_nold = 0;
    pg.in_use = true;
}

/// Obtain a slot of at least `total` bytes from its size-class pool
/// (`jl_gc_pool_alloc`): pop the page-local free list of a page with room,
/// recycling a released page or carving a fresh one when the pool has none.
/// Returns the slot (header) offset, or [`region::NULL`] on exhaustion.
pub fn alloc_chunk(total: u32) -> Offset {
    let osize = size_class(total);
    let pi = pool_for(osize);

    // A page with free slots, from the pool's queue — or a recycled/fresh one.
    let idx = loop {
        if let Some(&idx) = pools()[pi].page_q.last() {
            if pages()[idx].nfree > 0 {
                break idx;
            }
            pools()[pi].page_q.pop(); // exhausted page
            continue;
        }
        let idx = if let Some(idx) = free_pages().pop() {
            pages()[idx].osize = osize; // recycled page joins this class
            idx
        } else {
            let start = region::alloc(PAGE_SIZE);
            if start == region::NULL {
                return region::NULL;
            }
            pages().push(Page {
                start,
                osize,
                freelist: region::NULL,
                nfree: 0,
                live_n: 0,
                has_marked: false,
                has_young: false,
                nold: 0,
                prev_nold: 0,
                in_use: true,
            });
            pages().len() - 1
        };
        init_page_freelist(idx);
        pools()[pi].page_q.push(idx);
        break idx;
    };

    let pg = &mut pages()[idx];
    let head = pg.freelist;
    pg.freelist = next_free(head);
    pg.nfree -= 1;
    pg.live_n += 1;
    pg.has_young = true; // fresh objects are young
    HEAP.live.set(HEAP.live.get() + 1);
    HEAP.heap_size.set(HEAP.heap_size.get() + osize);
    head
}

/// The page holding the object whose header is at `off` (binary search —
/// pages are carved from a monotonic bump allocator, so starts are sorted).
fn page_of(off: Offset) -> Option<usize> {
    let ps = pages();
    let mut lo = 0usize;
    let mut hi = ps.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if ps[mid].start + PAGE_SIZE as u32 <= off {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo < ps.len() && ps[lo].start <= off && ps[lo].in_use {
        Some(lo)
    } else {
        None
    }
}

/// Mark-side page metadata (`gc_setmark_pool_`, `gc-stock.c:291–309`): every
/// mark sets `has_marked`; a young mark sets `has_young`; an old mark
/// (2 → 3, the promotion-completion scan) increments `nold`.
fn on_marked(v: Value, was_old: bool) {
    if let Some(idx) = page_of(v.raw() - HEADER_SIZE as u32) {
        let pg = &mut pages()[idx];
        pg.has_marked = true;
        if was_old {
            pg.nold += 1;
        } else {
            pg.has_young = true;
        }
    }
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

/// Number of released pages awaiting reuse (introspection for tests).
#[allow(dead_code)]
pub fn free_page_count() -> usize {
    free_pages().len()
}

/// Pages walked by the most recent sweep (introspection for tests; skipped
/// and released pages are not walked).
#[allow(dead_code)]
pub fn pages_walked_last() -> usize {
    HEAP.pages_walked.get()
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
        on_marked(v, bits & BIT_OLD != 0);
        trace_fields(v, &mut work);
        if bits & BIT_OLD != 0 && has_young_ref(v) {
            remembered().push(v); // promotion scan found old → young edges
        }
    }
}

/// Sweep page by page (the state transitions of `gc-stock.c:164–191`, the
/// page protocol of `:878–898`). A page with no marked cell is **released
/// whole** — no walk, returned for reuse by any pool. A quick sweep **skips**
/// a page with no young cell whose old count is settled
/// (`!has_young && (!prev_sweep_full || prev_nold == nold)`), keeping its
/// free list and counts as they stand. Walked pages rebuild their free list:
/// `CLEAN` frees; `MARKED` promotes (the pin's promote-at-sweep); on a full
/// sweep unmarked `OLD` frees and `OLD_MARKED` demotes, giving old garbage
/// its one-full-cycle lag. Returns (survivors, live bytes, promoted bytes).
fn sweep(full: bool) -> (usize, usize, usize) {
    let mut survivors = 0usize;
    let mut live_bytes = 0usize;
    let mut promoted_bytes = 0usize;
    let prev_full = HEAP.prev_full.get();
    let mut walked = 0usize;

    let npages = pages().len();
    for idx in 0..npages {
        let (start, osize, in_use, has_marked, has_young, nold, prev_nold, cached_live) = {
            let pg = &pages()[idx];
            (pg.start, pg.osize, pg.in_use, pg.has_marked, pg.has_young, pg.nold, pg.prev_nold, pg.live_n)
        };
        if !in_use {
            continue;
        }
        // Whole-page release (`gc-stock.c:882–887`): `has_marked` is false
        // only when nothing on the page is live — it persists from the last
        // walk for stable all-old pages and is set by any mark this cycle.
        if !has_marked {
            let pg = &mut pages()[idx];
            pg.in_use = false;
            pg.live_n = 0;
            free_pages().push(idx);
            if let Some(pi) = pools().iter().position(|p| p.osize == osize) {
                pools()[pi].page_q.retain(|&q| q != idx);
            }
            continue;
        }
        // Quick-sweep page skip (`gc-stock.c:890–897`): no young cell and the
        // old count settled — the page (free list, counts, flags) stands.
        if !full && !has_young && (!prev_full || prev_nold == nold) {
            survivors += cached_live as usize;
            live_bytes += cached_live as usize * osize;
            continue;
        }
        walked += 1;
        // Walk the page, rebuilding its free list. On walked pages the pin
        // frees every unmarked cell — quick sweeps included (`:925–933`),
        // where unmarked `OLD` garbage dies early; live old objects are
        // always marked (3) here, the remset machinery guarantees it.
        let mut fl = region::NULL;
        let mut nfree = 0u32;
        let mut live_n = 0u32;
        for k in 0..(PAGE_SIZE / osize) {
            let slot = start + (k * osize) as u32;
            let v = Value(slot + HEADER_SIZE as u32);
            let bits = object::gc_bits(v);
            if bits & BIT_MARKED != 0 {
                if bits == BIT_MARKED {
                    object::set_gc_bits(v, BIT_OLD); // promote young survivor (`:935`)
                    promoted_bytes += osize;
                } else if full {
                    object::set_gc_bits(v, BIT_OLD); // demote 3 → 2: re-prove next full
                }
                live_n += 1;
            } else {
                set_next_free(slot, fl);
                fl = slot;
                nfree += 1;
            }
        }
        survivors += live_n as usize;
        live_bytes += live_n as usize * osize;
        let pg = &mut pages()[idx];
        pg.freelist = fl;
        pg.nfree = nfree;
        pg.live_n = live_n;
        pg.has_marked = live_n > 0; // recomputed only when walked (`:950`)
        pg.has_young = false;
        if full {
            // every survivor is old now: the settled count for future skips
            pg.prev_nold = live_n;
            pg.nold = 0;
        }
        if nfree > 0 {
            let pi = pool_for(osize);
            if !pools()[pi].page_q.contains(&idx) {
                pools()[pi].page_q.push(idx);
            }
        }
    }

    HEAP.prev_full.set(full);
    HEAP.pages_walked.set(walked);
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
