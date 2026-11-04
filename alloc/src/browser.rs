use std::alloc::{GlobalAlloc, Layout};
use std::arch::wasm32;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::heaps::{HeapId, Heaps, Memory, Pages, Threads};

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

/// A byte in each thread's thread-local storage, which names the thread by its
/// address alone: wasm-bindgen allocates a thread's TLS block and stack before
/// the thread's TLS base is set, and frees its stack after pointing that base
/// outside the memory, so the allocator must never read a thread-local.
#[thread_local]
static THREAD_MARKER: u8 = 0;

fn thread_marker() -> usize {
    (&raw const THREAD_MARKER).addr()
}

/// The main thread's [`THREAD_MARKER`] address, once it is marked.
static MAIN_THREAD: AtomicUsize = AtomicUsize::new(UNMARKED);

/// No thread's marker, as no TLS block ends at the last byte of the memory.
const UNMARKED: usize = usize::MAX;

struct BrowserThreads;

// SAFETY: the main thread's TLS block lives as long as the page, other live
// threads' blocks are disjoint from it, and a thread with no block yet keeps
// the module's initial TLS base, one with none any more a base outside the
// memory, so only the main thread has its marker address.
unsafe impl Threads for BrowserThreads {
    fn heap(&self) -> HeapId {
        if thread_marker() == MAIN_THREAD.load(Ordering::Relaxed) {
            HeapId::Main
        } else {
            HeapId::Shared
        }
    }

    /// Waits only once the main thread is marked: a thread that sees it marked
    /// is a worker, as the main thread never takes the shared heap's lock once
    /// marked, while before then the caller may be the main thread, on which
    /// `memory.atomic.wait` traps.
    fn wait(&self, state: &AtomicU32, value: u32) {
        if MAIN_THREAD.load(Ordering::Relaxed) != UNMARKED {
            const NO_TIMEOUT: i64 = -1;
            // SAFETY: `state` is a live, aligned atomic in the shared memory.
            unsafe {
                wasm32::memory_atomic_wait32(state.as_ptr().cast(), value as i32, NO_TIMEOUT)
            };
        } else {
            std::hint::spin_loop();
        }
    }

    fn wake_one(&self, state: &AtomicU32) {
        // SAFETY: `state` is a live, aligned atomic in the shared memory.
        unsafe { wasm32::memory_atomic_notify(state.as_ptr().cast(), 1) };
    }
}

static PAGES: Pages<Wasm32Memory> = Pages::new(Wasm32Memory);

static HEAPS: Heaps<Wasm32Memory, BrowserThreads> = Heaps::new(&PAGES, BrowserThreads);

/// dlmalloc in a heap for the browser's main thread and one its workers share.
pub struct Allocator;

// SAFETY: `Heaps` upholds the contract.
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

/// Gives the calling thread, the browser's main thread, a heap of its own, as it
/// draws every frame and so must never wait for a worker's allocation. Mark it
/// before it starts a worker: until then it allocates from the heap the workers
/// share, contending with them for its lock.
///
/// # Panics
///
/// If a thread has been marked before.
pub fn mark_main_thread() {
    let marked = MAIN_THREAD.compare_exchange(
        UNMARKED,
        thread_marker(),
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    assert!(
        marked.is_ok(),
        "only one thread is the browser's main thread"
    );
}
