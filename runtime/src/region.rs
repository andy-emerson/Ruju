//! A bump allocator over a bounded region of WASM linear memory.
//!
//! Per the project's memory-composability discipline, the runtime owns only this
//! region rather than the whole address space, and every reference is expressed
//! as an offset from the region base — never a hardcoded address. Collection is
//! not yet implemented, so this skeleton allocator only ever grows.

use core::cell::{Cell, UnsafeCell};

/// Size of the bounded heap region.
const REGION_SIZE: usize = 1 << 20; // 1 MiB

/// Total capacity of the bounded region in bytes.
pub fn capacity() -> usize {
    REGION_SIZE
}

/// Allocation alignment, in bytes.
const ALIGN: usize = 8;

/// A region-relative offset to a heap object. Offset `0` is reserved as the null
/// reference, so a freshly initialized region never hands it out.
pub type Offset = u32;

/// The null reference.
pub const NULL: Offset = 0;

struct Heap {
    buf: UnsafeCell<[u8; REGION_SIZE]>,
    top: Cell<usize>,
    initialized: Cell<bool>,
}

// Sound only because the runtime is single-threaded under wasm32 for now; a
// multithreaded build will replace this with proper synchronization.
unsafe impl Sync for Heap {}

static HEAP: Heap = Heap {
    buf: UnsafeCell::new([0u8; REGION_SIZE]),
    top: Cell::new(0),
    initialized: Cell::new(false),
};

#[inline]
fn align_up(n: usize) -> usize {
    (n + (ALIGN - 1)) & !(ALIGN - 1)
}

/// Initialize (or reset) the region. The first word is reserved so that [`NULL`]
/// (offset 0) is never returned by [`alloc`].
pub fn init() {
    HEAP.top.set(ALIGN);
    HEAP.initialized.set(true);
}

/// Whether [`init`] has been called.
pub fn is_initialized() -> bool {
    HEAP.initialized.get()
}

/// Bump-allocate `size` bytes and return a region-relative offset, or [`NULL`]
/// if the region is exhausted (no collection yet).
pub fn alloc(size: usize) -> Offset {
    let start = HEAP.top.get();
    let new_top = start + align_up(size);
    if new_top > REGION_SIZE {
        return NULL;
    }
    HEAP.top.set(new_top);
    start as Offset
}

/// Bytes currently in use (the region-relative high-water mark).
pub fn used() -> usize {
    HEAP.top.get()
}

/// A typed pointer to the object at `off`. The caller guarantees that `off` and
/// the type `T` describe valid contents within the region.
pub(crate) fn ptr_mut<T>(off: Offset) -> *mut T {
    unsafe { (HEAP.buf.get() as *mut u8).add(off as usize) as *mut T }
}
