use std::alloc::{GlobalAlloc, Layout};
use std::arch::wasm32;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::heaps::{Heaps, Memory, ThreadHeaps};
use crate::lock::{Lock, Park};

/// The largest block bviz asks of mimalloc: the largest that shares an arena
/// page with others. mimalloc gives a larger block a run of arena slices of its
/// own, which once freed only a block fitting in it reuses; the shared heap
/// merges freed neighbours instead, so it keeps the memory, which never
/// shrinks, smaller.
const MAX_BLOCK: usize = 128 << 10;

/// The alignment of every mimalloc block of at least this size; a smaller
/// block is aligned to its size rounded up to a power of two.
const MAX_ALIGN: usize = 16;

unsafe extern "C" {
    fn mi_malloc(size: usize) -> *mut u8;
    fn mi_zalloc(size: usize) -> *mut u8;
    fn mi_realloc(block: *mut u8, size: usize) -> *mut u8;
    fn mi_malloc_aligned(size: usize, align: usize) -> *mut u8;
    fn mi_zalloc_aligned(size: usize, align: usize) -> *mut u8;
    fn mi_realloc_aligned(block: *mut u8, size: usize, align: usize) -> *mut u8;
    fn mi_free(block: *mut u8);
    fn mi_thread_done();
    fn bviz_mimalloc_process_init();
    static bviz_mimalloc_max_block: usize;
    static bviz_mimalloc_max_align: usize;
    fn bviz_tls_base() -> usize;
    static __stack_low: u8;
    static __stack_high: u8;
}

/// Whether `mi_malloc(size)` returns a block aligned to `align`.
fn mimalloc_aligns(size: usize, align: usize) -> bool {
    align <= MAX_ALIGN && align <= size.next_power_of_two()
}

struct Wasm32Memory;

// SAFETY: memory.grow appends zeroed pages to the module's one memory, which
// spans at most wasm32's address space, and returns each page once.
unsafe impl Memory for Wasm32Memory {
    fn base(&self) -> usize {
        0
    }

    fn grow(&self, count: usize) -> Option<usize> {
        let first = wasm32::memory_grow(0, count);
        (first != usize::MAX).then_some(first)
    }
}

/// A worker's TLS base before wasm-bindgen allocates its TLS block: the
/// initial value wasm-ld gives `__tls_base`.
const TLS_BASE_UNSET: usize = 0;

/// Whether the calling thread has a TLS block of its own, which mimalloc's
/// thread-locals live in.
#[inline]
fn has_tls_block() -> bool {
    // SAFETY: reads the calling thread's `__tls_base`, which is always set.
    unsafe { bviz_tls_base() != TLS_BASE_UNSET }
}

/// Whether the calling thread is the page's main thread: the one thread whose
/// stack is the module's own, as wasm-bindgen allocates every other's.
fn on_main_thread() -> bool {
    let probe = 0u8;
    let at = std::hint::black_box(&raw const probe).addr();
    (&raw const __stack_low).addr() <= at && at < (&raw const __stack_high).addr()
}

struct BrowserThreads;

impl Park for BrowserThreads {
    /// The main thread cannot: `memory.atomic.wait` traps there.
    fn can_park(&self) -> bool {
        !on_main_thread()
    }

    fn wait(&self, state: &AtomicU32, value: u32) {
        const NO_TIMEOUT: i64 = -1;
        // SAFETY: `state` is a live, aligned atomic in the shared memory.
        unsafe { wasm32::memory_atomic_wait32(state.as_ptr().cast(), value as i32, NO_TIMEOUT) };
    }

    fn wake_one(&self, state: &AtomicU32) {
        // SAFETY: `state` is a live, aligned atomic in the shared memory.
        unsafe { wasm32::memory_atomic_notify(state.as_ptr().cast(), 1) };
    }
}

/// Whether mimalloc's process state is set up, which only
/// [`start_on_main_thread`] does.
static STARTED: AtomicBool = AtomicBool::new(false);

struct Mimalloc;

// SAFETY: mimalloc takes any size and power-of-two alignment, and keeps its
// thread-locals in the TLS block `serves_caller` requires. No thread reaches
// mimalloc without one: wasm-bindgen runs a thread without one only as it
// allocates the thread's stack and TLS block, both from the shared heap, and
// as `__wbindgen_thread_destroy` frees the stack, after freeing the TLS block
// and then setting `__tls_base` to `i32::MIN`, with nothing allocated after.
unsafe impl ThreadHeaps for Mimalloc {
    const MAX_BLOCK: usize = MAX_BLOCK;

    #[inline]
    fn serves_caller(&self) -> bool {
        STARTED.load(Ordering::Acquire) && has_tls_block()
    }

    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the trait's.
        unsafe {
            if mimalloc_aligns(layout.size(), layout.align()) {
                mi_malloc(layout.size())
            } else {
                mi_malloc_aligned(layout.size(), layout.align())
            }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the trait's.
        unsafe {
            if mimalloc_aligns(layout.size(), layout.align()) {
                mi_zalloc(layout.size())
            } else {
                mi_zalloc_aligned(layout.size(), layout.align())
            }
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as the trait's.
        unsafe {
            if mimalloc_aligns(new_size, layout.align()) {
                mi_realloc(ptr, new_size)
            } else {
                mi_realloc_aligned(ptr, new_size, layout.align())
            }
        }
    }

    #[inline]
    unsafe fn free(&self, ptr: *mut u8) {
        // SAFETY: as the trait's.
        unsafe { mi_free(ptr) }
    }
}

static HEAPS: Heaps<Wasm32Memory, BrowserThreads, Mimalloc> =
    Heaps::new(&Wasm32Memory, BrowserThreads, Mimalloc);

/// Starts mimalloc, after which every thread with a TLS block allocates from
/// mimalloc; until then all use the shared heap. Call it on the page's main
/// thread, whose TLS block wasm-bindgen allocates before any Rust code runs,
/// so that the main thread is mimalloc's; on any other thread it does nothing.
pub fn start_on_main_thread() {
    if on_main_thread() && !STARTED.load(Ordering::Relaxed) {
        // SAFETY: constants of the mimalloc build.
        let built = unsafe { (bviz_mimalloc_max_block, bviz_mimalloc_max_align) };
        assert_eq!(
            built,
            (MAX_BLOCK, MAX_ALIGN),
            "mimalloc is not built as bviz_alloc assumes"
        );
        // SAFETY: only the main thread gets here, once, with its TLS block.
        unsafe { bviz_mimalloc_process_init() };
        STARTED.store(true, Ordering::Release);
    }
}

#[unsafe(no_mangle)]
extern "C" fn bviz_mimalloc_os_alloc(size: usize, align: usize) -> *mut u8 {
    HEAPS.os_alloc(size, align)
}

/// # Safety
///
/// As [`Heaps::os_free`]'s.
#[unsafe(no_mangle)]
unsafe extern "C" fn bviz_mimalloc_os_free(region: *mut u8, size: usize) {
    // SAFETY: the caller upholds the contract.
    unsafe { HEAPS.os_free(region, size) }
}

#[unsafe(no_mangle)]
extern "C" fn bviz_lock_acquire(lock: &Lock) {
    lock.acquire(&BrowserThreads);
}

#[unsafe(no_mangle)]
extern "C" fn bviz_lock_try_acquire(lock: &Lock) -> bool {
    lock.try_acquire()
}

/// # Safety
///
/// As [`Lock::release`]'s.
#[unsafe(no_mangle)]
unsafe extern "C" fn bviz_lock_release(lock: &Lock) {
    // SAFETY: the caller upholds the contract.
    unsafe { lock.release(&BrowserThreads) }
}

/// The threaded browser build's allocator.
pub struct Allocator;

// SAFETY: `HEAPS` upholds the contract.
unsafe impl GlobalAlloc for Allocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract.
        unsafe { HEAPS.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract.
        unsafe { HEAPS.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract.
        unsafe { HEAPS.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract.
        unsafe { HEAPS.realloc(ptr, layout, new_size) }
    }
}

/// Ends the calling worker thread's mimalloc heap, handing the memory it holds
/// to the threads that remain. Call it as the worker's wasm thread ends, the
/// last code the thread runs.
pub fn end_worker_thread() {
    if Mimalloc.serves_caller() {
        // SAFETY: the thread has a TLS block, and allocates no more.
        unsafe { mi_thread_done() };
    }
}
