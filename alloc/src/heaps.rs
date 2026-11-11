use std::alloc::{GlobalAlloc, Layout};
use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use dlmalloc::Dlmalloc;

use crate::lock::{Lock, Park};

/// The granule a WebAssembly memory grows by.
pub const PAGE_SIZE: usize = 64 << 10;

/// The pages of wasm32's whole address space.
const ADDRESS_SPACE_PAGES: usize = 1 << (u32::BITS - PAGE_SIZE.trailing_zeros());

/// The bytes withheld from a region that ends at the top of the address space,
/// where a pointer one past a block's end would wrap to null.
const ADDRESS_SPACE_END_RESERVE: usize = 16;

/// A memory that grows by whole pages.
///
/// # Safety
///
/// `grow` hands out only zeroed, writable pages that no caller has been given
/// before, each within the first [`ADDRESS_SPACE_PAGES`] pages from `base`.
pub unsafe trait Memory: Sync + 'static {
    /// The address of page 0.
    fn base(&self) -> usize;

    /// Takes `count` pages, returning the index of the first, or `None` once
    /// the memory cannot grow.
    fn grow(&self, count: usize) -> Option<usize>;
}

/// Heaps of each thread's own, which take their memory from [`Heaps::os_alloc`]
/// as an operating system's.
///
/// # Safety
///
/// Each method upholds [`GlobalAlloc`]'s contract for the blocks it serves.
/// [`Heaps`] calls `alloc`, `alloc_zeroed` and `realloc` only on a thread that
/// `serves_caller` names, with no size above `MAX_BLOCK`, and `free` only on
/// a block of these heaps.
pub unsafe trait ThreadHeaps: Sync + 'static {
    /// The largest block the heaps serve.
    const MAX_BLOCK: usize;

    fn serves_caller(&self) -> bool;

    unsafe fn alloc(&self, layout: Layout) -> *mut u8;

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8;

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8;

    unsafe fn free(&self, ptr: *mut u8);
}

/// The pages the shared heap grows by.
struct Source<M: 'static>(&'static M);

// SAFETY: every region `alloc` returns is fresh pages of the memory, which
// `Memory` guarantees no one else holds; no region is ever released.
unsafe impl<M: Memory> dlmalloc::Allocator for Source<M> {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        let count = size.div_ceil(PAGE_SIZE);
        let Some(first) = self.0.grow(count) else {
            return (ptr::null_mut(), 0, 0);
        };
        let start = self.0.base() + first * PAGE_SIZE;
        let mut len = count * PAGE_SIZE;
        if start.wrapping_add(len) == 0 {
            len -= ADDRESS_SPACE_END_RESERVE;
        }
        (start as *mut u8, len, 0)
    }

    fn remap(&self, _: *mut u8, _: usize, _: usize, _: bool) -> *mut u8 {
        ptr::null_mut()
    }

    fn free_part(&self, _: *mut u8, _: usize, _: usize) -> bool {
        false
    }

    fn free(&self, _: *mut u8, _: usize) -> bool {
        false
    }

    fn can_release_part(&self, _: u32) -> bool {
        false
    }

    fn allocates_zeros(&self) -> bool {
        true
    }

    fn page_size(&self) -> usize {
        PAGE_SIZE
    }
}

/// The per-thread heaps `T`, and a dlmalloc heap all threads share behind a
/// lock, which holds the per-thread heaps' memory and serves the blocks they
/// do not.
///
/// The lock is held only for dlmalloc's bookkeeping and the memory's growth:
/// blocks are zeroed and copied outside it, so a thread waits on another's
/// allocation for no longer than that, however large the block. No dlmalloc
/// call made under it panics on a block its caller's contract describes: blocks
/// are freed and resized through its `c_` calls, as `free` and `realloc`
/// assert the layout.
pub struct Heaps<M: 'static, P, T> {
    memory: &'static M,
    lock: Lock,
    park: P,
    shared: UnsafeCell<Dlmalloc<Source<M>>>,
    threads: T,
    /// Whether `threads` owns each page of the memory; an entry is set before
    /// `threads` is handed its page, and cleared before its page is taken back.
    thread_owned: [AtomicU32; ADDRESS_SPACE_PAGES / u32::BITS as usize],
}

// SAFETY: `shared` is reached only under `lock`.
unsafe impl<M: Memory, P: Park + Sync, T: ThreadHeaps> Sync for Heaps<M, P, T> {}

impl<M: Memory, P: Park, T: ThreadHeaps> Heaps<M, P, T> {
    pub const fn new(memory: &'static M, park: P, threads: T) -> Self {
        Self {
            memory,
            lock: Lock::new(),
            park,
            shared: UnsafeCell::new(Dlmalloc::new_with_allocator(Source(memory))),
            threads,
            thread_owned: [const { AtomicU32::new(0) }; ADDRESS_SPACE_PAGES / u32::BITS as usize],
        }
    }

    fn with_shared<R>(&self, f: impl FnOnce(&mut Dlmalloc<Source<M>>) -> R) -> R {
        let _held = self.lock.lock(&self.park);
        // SAFETY: the lock is held.
        f(unsafe { &mut *self.shared.get() })
    }

    /// # Safety
    ///
    /// `layout.align()` is a power of two.
    unsafe fn shared_alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds the contract.
        self.with_shared(|heap| unsafe { heap.c_memalign(layout.align(), layout.size()) })
    }

    /// # Safety
    ///
    /// `ptr` is a live block of the shared heap, given up.
    unsafe fn shared_free(&self, ptr: *mut u8) {
        // SAFETY: the caller upholds the contract.
        self.with_shared(|heap| unsafe { heap.c_free(ptr) });
    }

    /// Whether `threads` owns the page of `block`. Relaxed: whoever frees a
    /// block learned of it through synchronization that follows its
    /// allocation, which follows its page's entry.
    fn thread_owns(&self, block: *mut u8) -> bool {
        let page = (block.addr() - self.memory.base()) / PAGE_SIZE;
        let word = self.thread_owned[page / u32::BITS as usize].load(Ordering::Relaxed);
        word & (1 << (page % u32::BITS as usize)) != 0
    }

    fn set_thread_owns(&self, region: *mut u8, size: usize, owned: bool) {
        let first = (region.addr() - self.memory.base()) / PAGE_SIZE;
        for page in first..first + size / PAGE_SIZE {
            let word = &self.thread_owned[page / u32::BITS as usize];
            let bit = 1 << (page % u32::BITS as usize);
            if owned {
                word.fetch_or(bit, Ordering::Relaxed);
            } else {
                word.fetch_and(!bit, Ordering::Relaxed);
            }
        }
    }

    /// Hands `threads` whole pages of the shared heap, `size` bytes rounded
    /// up, aligned to at least `align`; null if the memory cannot grow.
    pub fn os_alloc(&self, size: usize, align: usize) -> *mut u8 {
        let size = size.next_multiple_of(PAGE_SIZE);
        let align = align.max(PAGE_SIZE).next_power_of_two();
        // SAFETY: `align` is a power of two.
        let region = unsafe { self.shared_alloc(Layout::from_size_align_unchecked(size, align)) };
        if !region.is_null() {
            self.set_thread_owns(region, size, true);
        }
        region
    }

    /// Takes back pages [`Heaps::os_alloc`] handed out.
    ///
    /// # Safety
    ///
    /// `region` and `size` are those of one `os_alloc` call, whose pages hold
    /// no live block any more.
    pub unsafe fn os_free(&self, region: *mut u8, size: usize) {
        self.set_thread_owns(region, size.next_multiple_of(PAGE_SIZE), false);
        // SAFETY: the caller upholds the contract.
        unsafe { self.shared_free(region) };
    }

    fn threads_serve(&self, size: usize) -> bool {
        size <= T::MAX_BLOCK && self.threads.serves_caller()
    }
}

// SAFETY: every block comes from `threads` or the shared heap, and goes back to
// the one whose pages hold it.
unsafe impl<M: Memory, P: Park + Sync, T: ThreadHeaps> GlobalAlloc for Heaps<M, P, T> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract.
        unsafe {
            if self.threads_serve(layout.size()) {
                self.threads.alloc(layout)
            } else {
                self.shared_alloc(layout)
            }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if self.threads_serve(layout.size()) {
            // SAFETY: the caller upholds `alloc_zeroed`'s contract.
            return unsafe { self.threads.alloc_zeroed(layout) };
        }
        // SAFETY: the caller upholds `alloc_zeroed`'s contract.
        let block = unsafe { self.shared_alloc(layout) };
        if !block.is_null() {
            // SAFETY: the block holds `layout.size()` bytes.
            unsafe { block.write_bytes(0, layout.size()) };
        }
        block
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _: Layout) {
        // SAFETY: the caller gives up `ptr`, a block of the heap whose pages
        // hold it.
        unsafe {
            if self.thread_owns(ptr) {
                self.threads.free(ptr);
            } else {
                self.shared_free(ptr);
            }
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if self.thread_owns(ptr) {
            if self.threads_serve(new_size) {
                // SAFETY: the caller upholds `realloc`'s contract for `ptr`, a
                // block of `threads`.
                return unsafe { self.threads.realloc(ptr, layout, new_size) };
            }
        } else if self.with_shared(|heap| {
            // SAFETY: `ptr` is a live block of the shared heap.
            unsafe { heap.c_realloc_in_place(ptr, new_size) }
        }) {
            return ptr;
        }
        // SAFETY: the caller guarantees `new_size` rounded up to `layout`'s
        // alignment does not overflow.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // SAFETY: `new_size` is nonzero by `realloc`'s contract.
        let moved = unsafe { self.alloc(new_layout) };
        if !moved.is_null() {
            // SAFETY: both blocks hold the smaller size, and are distinct.
            unsafe { ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size)) };
            // SAFETY: `realloc` frees `ptr` once it succeeds.
            unsafe { self.dealloc(ptr, layout) };
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Reserved, TestPark, filled, holds};
    use std::cell::Cell;
    use std::ops::Deref;
    use std::sync::{Mutex, OnceLock, mpsc};

    type TestHeaps = Heaps<Reserved, TestPark, Dlmallocs>;

    /// Regions of a test's [`Heaps::os_alloc`].
    struct Regions(&'static OnceLock<&'static TestHeaps>);

    // SAFETY: `os_alloc` hands out pages of the memory no block holds, and no
    // region is ever released.
    unsafe impl dlmalloc::Allocator for Regions {
        fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
            let region = self.0.get().unwrap().os_alloc(size, PAGE_SIZE);
            let len = if region.is_null() {
                0
            } else {
                size.next_multiple_of(PAGE_SIZE)
            };
            (region, len, 0)
        }

        fn remap(&self, _: *mut u8, _: usize, _: usize, _: bool) -> *mut u8 {
            ptr::null_mut()
        }

        fn free_part(&self, _: *mut u8, _: usize, _: usize) -> bool {
            false
        }

        fn free(&self, _: *mut u8, _: usize) -> bool {
            false
        }

        fn can_release_part(&self, _: u32) -> bool {
            false
        }

        fn allocates_zeros(&self) -> bool {
            false
        }

        fn page_size(&self) -> usize {
            PAGE_SIZE
        }
    }

    thread_local! {
        /// Whether the calling thread is one [`Dlmallocs`] does not serve.
        static UNSERVED: Cell<bool> = const { Cell::new(false) };
    }

    /// Per-thread heaps that are one locked dlmalloc, standing in for
    /// mimalloc: the routing is the same however many there are.
    struct Dlmallocs(Mutex<Dlmalloc<Regions>>);

    // SAFETY: the dlmalloc is reached only under the mutex.
    unsafe impl Sync for Dlmallocs {}

    impl Dlmallocs {
        /// The largest block they serve.
        const MAX_BLOCK: usize = 4096;
    }

    // SAFETY: dlmalloc upholds `GlobalAlloc`'s contract.
    unsafe impl ThreadHeaps for Dlmallocs {
        const MAX_BLOCK: usize = Self::MAX_BLOCK;

        fn serves_caller(&self) -> bool {
            !UNSERVED.get()
        }

        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: as `GlobalAlloc::alloc`'s.
            unsafe { self.0.lock().unwrap().malloc(layout.size(), layout.align()) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: as `GlobalAlloc::alloc_zeroed`'s.
            unsafe { self.0.lock().unwrap().calloc(layout.size(), layout.align()) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: as `GlobalAlloc::realloc`'s.
            unsafe {
                self.0
                    .lock()
                    .unwrap()
                    .realloc(ptr, layout.size(), layout.align(), new_size)
            }
        }

        unsafe fn free(&self, ptr: *mut u8) {
            // SAFETY: `ptr` is a live block of the heap.
            unsafe { self.0.lock().unwrap().c_free(ptr) }
        }
    }

    /// A test's heaps, whose memory is freed as this drops.
    struct TestRun(&'static TestHeaps);

    impl Deref for TestRun {
        type Target = TestHeaps;

        fn deref(&self) -> &TestHeaps {
            self.0
        }
    }

    impl Drop for TestRun {
        fn drop(&mut self) {
            // SAFETY: the test allocates from the memory no more.
            unsafe { self.memory.release() };
        }
    }

    fn heaps() -> TestRun {
        let cell = Box::leak(Box::new(OnceLock::new()));
        let heaps: &'static TestHeaps = Box::leak(Box::new(Heaps::new(
            Reserved::leak(),
            TestPark::default(),
            Dlmallocs(Mutex::new(Dlmalloc::new_with_allocator(Regions(cell)))),
        )));
        cell.set(heaps).ok().unwrap();
        TestRun(heaps)
    }

    const SMALL: Layout = Layout::new::<[usize; 4]>();

    const LARGE: Layout = match Layout::from_size_align(4 * Dlmallocs::MAX_BLOCK, 8) {
        Ok(layout) => layout,
        Err(_) => panic!(),
    };

    fn unserved<R>(f: impl FnOnce() -> R) -> R {
        UNSERVED.set(true);
        let out = f();
        UNSERVED.set(false);
        out
    }

    #[test]
    fn blocks_go_to_the_thread_heaps_up_to_their_largest() {
        let heaps = &heaps();
        let small = filled(&**heaps, SMALL, 1) as *mut u8;
        let large = filled(&**heaps, LARGE, 2) as *mut u8;
        let early = unserved(|| filled(&**heaps, SMALL, 3)) as *mut u8;
        assert!(heaps.thread_owns(small));
        assert!(!heaps.thread_owns(large));
        assert!(!heaps.thread_owns(early));
        // SAFETY: the blocks are live and of these layouts.
        unsafe {
            heaps.dealloc(small, SMALL);
            heaps.dealloc(large, LARGE);
            heaps.dealloc(early, SMALL);
        }
    }

    #[test]
    fn a_shared_block_is_zeroed_however_it_was_used() {
        let heaps = &heaps();
        let used = filled(&**heaps, LARGE, 0xa5) as *mut u8;
        // SAFETY: the block is live and of `LARGE`.
        unsafe { heaps.dealloc(used, LARGE) };
        // SAFETY: the layout is nonzero.
        let zeroed = unsafe { heaps.alloc_zeroed(LARGE) };
        assert_eq!(zeroed, used);
        assert!(holds(zeroed, LARGE.size(), 0));
    }

    #[test]
    fn a_shared_block_grows_in_place_into_free_space() {
        let heaps = &heaps();
        let block = filled(&**heaps, LARGE, 4) as *mut u8;
        let grown = 2 * LARGE.size();
        // SAFETY: the block is live and of `LARGE`.
        let resized = unsafe { heaps.realloc(block, LARGE, grown) };
        assert_eq!(resized, block);
        assert!(holds(resized, LARGE.size(), 4));
    }

    #[test]
    fn a_shared_block_that_cannot_grow_in_place_moves() {
        let heaps = &heaps();
        let block = filled(&**heaps, LARGE, 5) as *mut u8;
        let neighbour = filled(&**heaps, LARGE, 6) as *mut u8;
        let grown = 2 * LARGE.size();
        // SAFETY: the block is live and of `LARGE`.
        let moved = unsafe { heaps.realloc(block, LARGE, grown) };
        assert_ne!(moved, block);
        assert!(holds(moved, LARGE.size(), 5));
        assert!(holds(neighbour, LARGE.size(), 6));
    }

    #[test]
    fn realloc_past_the_thread_heaps_largest_moves_to_the_shared_heap_and_back() {
        let heaps = &heaps();
        let block = filled(&**heaps, SMALL, 7) as *mut u8;
        // SAFETY: the block is live and of `SMALL`.
        let large = unsafe { heaps.realloc(block, SMALL, LARGE.size()) };
        assert!(!heaps.thread_owns(large));
        assert!(holds(large, SMALL.size(), 7));
        let _neighbour = filled(&**heaps, LARGE, 8);
        // SAFETY: the block is live and of `LARGE`.
        let small = unsafe { heaps.realloc(large, LARGE, SMALL.size()) };
        assert_eq!(small, large, "a shrinking block stays where it is");
        assert!(holds(small, SMALL.size(), 7));
    }

    #[test]
    fn realloc_keeps_an_over_aligned_block_aligned() {
        let heaps = &heaps();
        for size in [SMALL.size(), LARGE.size()] {
            let mut layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
            let mut block = filled(&**heaps, layout, 9) as *mut u8;
            for grown in [2 * size, 8 * size, size] {
                let _neighbour = filled(&**heaps, Layout::from_size_align(size, 8).unwrap(), 0);
                // SAFETY: the block is live and of `layout`, and `grown` is
                // nonzero.
                block = unsafe { heaps.realloc(block, layout, grown) };
                layout = Layout::from_size_align(grown, PAGE_SIZE).unwrap();
                assert_eq!(block as usize % PAGE_SIZE, 0);
                assert!(holds(block, size, 9));
            }
        }
    }

    #[test]
    fn pages_taken_back_from_the_thread_heaps_are_the_shared_heaps() {
        let heaps = &heaps();
        let region = heaps.os_alloc(PAGE_SIZE, PAGE_SIZE);
        assert!(heaps.thread_owns(region));
        // SAFETY: the region came from `os_alloc` and holds no block.
        unsafe { heaps.os_free(region, PAGE_SIZE) };
        assert!(!heaps.thread_owns(region));
        let reused = filled(
            &**heaps,
            Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap(),
            1,
        );
        assert_eq!(reused as *mut u8, region);
        assert!(!heaps.thread_owns(region));
    }

    /// Every thread allocates blocks of assorted sizes and alignments from
    /// both heaps, passes each to the next thread in a ring, which grows or
    /// shrinks it, and frees what it receives once it finds the block intact.
    #[test]
    fn threads_resizing_and_freeing_each_others_blocks_keep_every_block_intact() {
        const THREADS: usize = 4;
        const BLOCKS: usize = 5_000;
        const MAX_SIZE: usize = 4 * Dlmallocs::MAX_BLOCK;
        const ALIGNMENTS: [usize; 3] = [8, 64, 4096];
        type Sent = (usize, Layout, u8);
        let heaps = &heaps();

        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..THREADS).map(|_| mpsc::channel::<Sent>()).unzip();
        let run = |id: usize, next: mpsc::Sender<Sent>, received: mpsc::Receiver<Sent>| {
            let mut rng = fastrand::Rng::with_seed(id as u64);
            for _ in 0..BLOCKS {
                let layout = Layout::from_size_align(
                    rng.usize(1..=MAX_SIZE),
                    ALIGNMENTS[rng.usize(..ALIGNMENTS.len())],
                )
                .unwrap();
                let fill = rng.u8(..);
                next.send((filled(&**heaps, layout, fill), layout, fill))
                    .unwrap();
            }
            drop(next);
            for (block, layout, fill) in received {
                let block = block as *mut u8;
                assert_eq!(block as usize % layout.align(), 0);
                assert!(holds(block, layout.size(), fill));
                let new_size = rng.usize(1..=MAX_SIZE);
                // SAFETY: the sender gave the block up.
                let block = unsafe { heaps.realloc(block, layout, new_size) };
                assert_eq!(block as usize % layout.align(), 0);
                assert!(holds(block, layout.size().min(new_size), fill));
                let layout = Layout::from_size_align(new_size, layout.align()).unwrap();
                // SAFETY: the block is live and of `layout`.
                unsafe { heaps.dealloc(block, layout) };
            }
        };

        let ring = receivers
            .into_iter()
            .enumerate()
            .map(|(id, received)| (id, senders[(id + 1) % THREADS].clone(), received));
        let workers: Vec<_> = ring.collect();
        drop(senders);
        bviz_parallel::scope(|scope| {
            for (id, next, received) in workers {
                scope.spawn(move || run(id, next, received));
            }
        });
    }
}
