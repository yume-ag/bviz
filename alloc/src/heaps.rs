use std::alloc::{GlobalAlloc, Layout};
use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use dlmalloc::Dlmalloc;

/// The granule a WebAssembly memory grows by.
pub const PAGE_SIZE: usize = 64 << 10;

/// The pages of wasm32's whole address space.
const ADDRESS_SPACE_PAGES: usize = 1 << (u32::BITS - PAGE_SIZE.trailing_zeros());

/// The alignment dlmalloc gives every block.
const NATURAL_ALIGN: usize = 2 * size_of::<usize>();

/// The bytes withheld from a region that ends at the top of the address space,
/// where a pointer one past a block's end would wrap to null.
const ADDRESS_SPACE_END_RESERVE: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapId {
    /// The heap of the one thread [`Threads::heap`] names main, which reaches
    /// it without a lock.
    Main,
    Shared,
}

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

/// Which heap each thread allocates from, and how threads wait for the shared
/// heap's lock.
///
/// # Safety
///
/// `heap` returns [`HeapId::Main`] on at most one thread over the value's life.
pub unsafe trait Threads: Sync + 'static {
    fn heap(&self) -> HeapId;

    /// Blocks while `state` holds `value`; it may return early.
    fn wait(&self, state: &AtomicU32, value: u32);

    fn wake_one(&self, state: &AtomicU32);
}

/// A memory and the heap that owns each of its pages.
pub struct Pages<M> {
    memory: M,
    /// Whether the main heap owns each page; every other page is the shared
    /// heap's. An entry is set before any block on its page exists, and is
    /// never cleared.
    main_owns: [AtomicBool; ADDRESS_SPACE_PAGES],
}

impl<M: Memory> Pages<M> {
    pub const fn new(memory: M) -> Self {
        Self {
            memory,
            main_owns: [const { AtomicBool::new(false) }; ADDRESS_SPACE_PAGES],
        }
    }

    fn owner(&self, block: *mut u8) -> HeapId {
        let page = (block as usize - self.memory.base()) / PAGE_SIZE;
        // Relaxed: whoever frees a block learned of it through synchronization
        // that follows its allocation, which follows its page's entry.
        if self.main_owns[page].load(Ordering::Relaxed) {
            HeapId::Main
        } else {
            HeapId::Shared
        }
    }
}

/// The memory one heap's dlmalloc grows by, recording each page it takes as
/// that heap's.
struct Source<M: 'static> {
    pages: &'static Pages<M>,
    heap: HeapId,
}

// SAFETY: every region `alloc` returns is fresh pages of the memory, which
// `Memory` guarantees no one else holds; no region is ever released.
unsafe impl<M: Memory> dlmalloc::Allocator for Source<M> {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        let count = size.div_ceil(PAGE_SIZE);
        let Some(first) = self.pages.memory.grow(count) else {
            return (ptr::null_mut(), 0, 0);
        };
        if self.heap == HeapId::Main {
            for owned in &self.pages.main_owns[first..first + count] {
                owned.store(true, Ordering::Relaxed);
            }
        }
        let start = self.pages.memory.base() + first * PAGE_SIZE;
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

/// A freed block, as it waits on a remote-free stack.
struct Freed {
    next: *mut Freed,
}

/// Blocks other threads freed into a heap. Only the owner takes from it, and
/// only the whole stack at once, so a push never races a pop of the block it
/// reads as the head.
struct RemoteFrees(AtomicPtr<Freed>);

impl RemoteFrees {
    /// # Safety
    ///
    /// `block` is a block of this stack's heap that no one uses any more.
    unsafe fn push(&self, block: *mut u8) {
        let block = block.cast::<Freed>();
        let mut head = self.0.load(Ordering::Relaxed);
        loop {
            // SAFETY: the block is the caller's alone, and dlmalloc aligns
            // every block for, and sizes it to at least, a pointer.
            unsafe { block.write(Freed { next: head }) };
            match self
                .0
                .compare_exchange_weak(head, block, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// Frees into `dlmalloc`, the stack's own heap, every block on the stack.
    fn drain<M: Memory>(&self, dlmalloc: &mut Dlmalloc<Source<M>>) {
        if self.0.load(Ordering::Relaxed).is_null() {
            return;
        }
        let mut block = self.0.swap(ptr::null_mut(), Ordering::Acquire);
        while !block.is_null() {
            // SAFETY: `push` wrote the block's link before publishing it, and
            // the Acquire swap took the whole stack, so no one else reads it.
            let next = unsafe { (*block).next };
            // SAFETY: a pushed block is an unused block of this heap.
            unsafe { dlmalloc.c_free(block.cast()) };
            block = next;
        }
    }
}

struct Heap<M: 'static> {
    dlmalloc: UnsafeCell<Dlmalloc<Source<M>>>,
    remote: RemoteFrees,
}

impl<M: Memory> Heap<M> {
    const fn new(pages: &'static Pages<M>, heap: HeapId) -> Self {
        Self {
            dlmalloc: UnsafeCell::new(Dlmalloc::new_with_allocator(Source { pages, heap })),
            remote: RemoteFrees(AtomicPtr::new(ptr::null_mut())),
        }
    }
}

/// The bit of a [`Lock`]'s state set while a thread holds it.
const LOCKED: u32 = 1;

/// The bit of a [`Lock`]'s state set from when a release wakes a parked
/// thread until a counted thread next acquires the lock or parks; a release
/// while it is set wakes no one.
const WOKEN: u32 = 2;

/// The unit in which a [`Lock`]'s state counts the threads parked, or about
/// to park, waiting for it.
const PARKED: u32 = 4;

/// The times a thread polls a held [`Lock`] before it parks. The shared heap's
/// critical sections are short, so a spinning thread usually takes the lock
/// without a wait and notify, which in Firefox take one process-wide lock.
/// Measured on a graph layout's allocation load, longer spins took the CPU the
/// holder needed, and shorter ones parked too often.
const SPINS: u32 = 1000;

/// A lock that parks a thread only after spinning.
struct Lock(AtomicU32);

struct Held<'a, T: Threads> {
    lock: &'a Lock,
    threads: &'a T,
}

impl Lock {
    const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    fn lock<'a, T: Threads>(&'a self, threads: &'a T) -> Held<'a, T> {
        if self
            .0
            .compare_exchange_weak(0, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_contended(threads);
        }
        Held {
            lock: self,
            threads,
        }
    }

    #[cold]
    fn lock_contended<T: Threads>(&self, threads: &T) {
        let state = &self.0;
        for _ in 0..SPINS {
            let seen = state.load(Ordering::Relaxed);
            if seen & LOCKED == 0
                && state
                    .compare_exchange_weak(
                        seen,
                        seen | LOCKED,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                return;
            }
            std::hint::spin_loop();
        }
        let mut seen = state.fetch_add(PARKED, Ordering::Relaxed) + PARKED;
        loop {
            seen = if seen & LOCKED == 0 {
                let taken = (seen - PARKED) & !WOKEN | LOCKED;
                match state.compare_exchange_weak(seen, taken, Ordering::Acquire, Ordering::Relaxed)
                {
                    Ok(_) => return,
                    Err(now) => now,
                }
            } else if seen & WOKEN != 0 {
                let unwoken = seen & !WOKEN;
                match state.compare_exchange_weak(
                    seen,
                    unwoken,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => unwoken,
                    Err(now) => now,
                }
            } else {
                threads.wait(state, seen);
                state.load(Ordering::Relaxed)
            };
        }
    }
}

impl<T: Threads> Drop for Held<'_, T> {
    fn drop(&mut self) {
        let state = &self.lock.0;
        let mut seen = state.fetch_sub(LOCKED, Ordering::Release) - LOCKED;
        while seen >= PARKED && seen & (LOCKED | WOKEN) == 0 {
            match state.compare_exchange_weak(
                seen,
                seen | WOKEN,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return self.threads.wake_one(state),
                Err(now) => seen = now,
            }
        }
    }
}

/// The main and the shared heap. The main heap's dlmalloc is reached only by
/// the thread `threads` names main, the shared heap's only under `lock`, so
/// neither is ever borrowed twice at once. No dlmalloc call made here panics,
/// so none reenters through a panic hook that allocates; blocks are freed and
/// resized through its `c_` calls, as `free` and `realloc` assert the layout.
/// Neither heap's free memory serves the other, so the memory grows to the sum
/// of their peaks.
pub struct Heaps<M: 'static, T> {
    pages: &'static Pages<M>,
    main: Heap<M>,
    shared: Heap<M>,
    lock: Lock,
    threads: T,
}

// SAFETY: see the type's documentation.
unsafe impl<M: Memory, T: Threads> Sync for Heaps<M, T> {}

impl<M: Memory, T: Threads> Heaps<M, T> {
    pub const fn new(pages: &'static Pages<M>, threads: T) -> Self {
        Self {
            pages,
            main: Heap::new(pages, HeapId::Main),
            shared: Heap::new(pages, HeapId::Shared),
            lock: Lock::new(),
            threads,
        }
    }

    fn heap(&self, id: HeapId) -> &Heap<M> {
        match id {
            HeapId::Main => &self.main,
            HeapId::Shared => &self.shared,
        }
    }

    /// Runs `allocate` on the calling thread's heap, `own`, once that heap has
    /// freed what other threads freed into it.
    fn with_own<R>(&self, own: HeapId, allocate: impl FnOnce(&mut Dlmalloc<Source<M>>) -> R) -> R {
        let heap = self.heap(own);
        let _held = (own == HeapId::Shared).then(|| self.lock.lock(&self.threads));
        // SAFETY: the calling thread is the main thread, or holds the lock.
        let dlmalloc = unsafe { &mut *heap.dlmalloc.get() };
        heap.remote.drain(dlmalloc);
        allocate(dlmalloc)
    }
}

// SAFETY: every block comes from one of the two dlmallocs, and goes back to
// the one whose pages hold it.
unsafe impl<M: Memory, T: Threads> GlobalAlloc for Heaps<M, T> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with_own(self.threads.heap(), |dlmalloc| {
            // SAFETY: the caller upholds `alloc`'s contract.
            unsafe { dlmalloc.malloc(layout.size(), layout.align()) }
        })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.with_own(self.threads.heap(), |dlmalloc| {
            // SAFETY: the caller upholds `alloc_zeroed`'s contract.
            unsafe { dlmalloc.calloc(layout.size(), layout.align()) }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _: Layout) {
        let own = self.threads.heap();
        match self.pages.owner(ptr) {
            owner if owner == own => self.with_own(own, |dlmalloc| {
                // SAFETY: the caller upholds `dealloc`'s contract.
                unsafe { dlmalloc.c_free(ptr) }
            }),
            // SAFETY: the caller gives up `ptr`, a block of `owner`'s pages.
            owner => unsafe { self.heap(owner).remote.push(ptr) },
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let own = self.threads.heap();
        let owner = self.pages.owner(ptr);
        if owner == own {
            return self.with_own(own, |dlmalloc| {
                // SAFETY: the caller upholds `realloc`'s contract.
                unsafe { realloc_in(dlmalloc, ptr, layout, new_size) }
            });
        }
        // SAFETY: the caller guarantees `new_size` rounded up to `layout`'s
        // alignment does not overflow.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // SAFETY: `new_size` is nonzero by `realloc`'s contract.
        let moved = unsafe { self.alloc(new_layout) };
        if !moved.is_null() {
            // SAFETY: both blocks hold the smaller size, and are distinct
            // blocks of different heaps.
            unsafe { ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size)) };
            // SAFETY: `realloc` frees `ptr` once it succeeds.
            unsafe { self.heap(owner).remote.push(ptr) };
        }
        moved
    }
}

/// Resizes `ptr`, a block of `dlmalloc`, keeping its alignment, which
/// `c_realloc` keeps only up to [`NATURAL_ALIGN`].
///
/// # Safety
///
/// As [`GlobalAlloc::realloc`]'s.
unsafe fn realloc_in<A: dlmalloc::Allocator>(
    dlmalloc: &mut Dlmalloc<A>,
    ptr: *mut u8,
    layout: Layout,
    new_size: usize,
) -> *mut u8 {
    if layout.align() <= NATURAL_ALIGN {
        // SAFETY: `ptr` is a live block of `dlmalloc`.
        return unsafe { dlmalloc.c_realloc(ptr, new_size) };
    }
    // SAFETY: `layout.align()` is a power of two.
    let moved = unsafe { dlmalloc.c_memalign(layout.align(), new_size) };
    if !moved.is_null() {
        // SAFETY: both blocks hold the smaller size, and are distinct.
        unsafe { ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size)) };
        // SAFETY: `ptr` is a live block of `dlmalloc`, given up once moved.
        unsafe { dlmalloc.c_free(ptr) };
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Deref;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::thread::{self, ThreadId};

    /// A test's memory: enough for any test.
    const RESERVED_PAGES: usize = 4096;

    /// The allocation a test's memory is aligned within. Zeroed with no
    /// alignment, an allocation this large is fresh pages that calloc does not
    /// write, so only the pages a heap grows into are touched.
    fn region_layout() -> Layout {
        Layout::from_size_align((RESERVED_PAGES + 1) * PAGE_SIZE, 1).unwrap()
    }

    struct Reserved {
        region: usize,
        base: usize,
        taken: AtomicUsize,
    }

    // SAFETY: the region is zeroed, the test's alone, and `grow` hands out
    // each page once.
    unsafe impl Memory for Reserved {
        fn base(&self) -> usize {
            self.base
        }

        fn grow(&self, count: usize) -> Option<usize> {
            self.taken
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |taken| {
                    Some(taken + count).filter(|&end| end <= RESERVED_PAGES)
                })
                .ok()
        }
    }

    #[derive(Default)]
    struct TestThreads {
        main: OnceLock<ThreadId>,
        wakes: AtomicUsize,
        sleeps: AtomicUsize,
        parking: Mutex<()>,
        unparked: Condvar,
    }

    // SAFETY: `main` is set once.
    unsafe impl Threads for TestThreads {
        fn heap(&self) -> HeapId {
            if self.main.get() == Some(&thread::current().id()) {
                HeapId::Main
            } else {
                HeapId::Shared
            }
        }

        /// Sleeps as a futex wait does: `wake_one` takes `parking` too, so it
        /// cannot slip between the check and the sleep.
        fn wait(&self, state: &AtomicU32, value: u32) {
            let parking = self.parking.lock().unwrap();
            if state.load(Ordering::SeqCst) == value {
                self.sleeps.fetch_add(1, Ordering::Relaxed);
                drop(self.unparked.wait(parking).unwrap());
            }
        }

        fn wake_one(&self, _: &AtomicU32) {
            self.wakes.fetch_add(1, Ordering::Relaxed);
            let _parking = self.parking.lock().unwrap();
            self.unparked.notify_one();
        }
    }

    type TestHeaps = Heaps<Reserved, TestThreads>;

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
            // SAFETY: the region came from `region_layout`, and the test
            // allocates from it no more.
            unsafe { std::alloc::dealloc(self.pages.memory.region as *mut u8, region_layout()) };
        }
    }

    fn heaps() -> TestRun {
        // SAFETY: the layout is nonzero.
        let region = unsafe { std::alloc::alloc_zeroed(region_layout()) } as usize;
        assert_ne!(region, 0, "the test region could not be reserved");
        let memory = Reserved {
            region,
            base: region.next_multiple_of(PAGE_SIZE),
            taken: AtomicUsize::new(0),
        };
        let pages = Box::leak(Box::new(Pages::new(memory)));
        TestRun(Box::leak(Box::new(Heaps::new(
            pages,
            TestThreads::default(),
        ))))
    }

    fn mark_main(heaps: &TestHeaps) {
        heaps.threads.main.set(thread::current().id()).unwrap();
    }

    const WORDS: Layout = Layout::new::<[usize; 4]>();

    /// A block of `layout` filled with `fill`, as an address that crosses
    /// threads.
    fn filled(heaps: &TestHeaps, layout: Layout, fill: u8) -> usize {
        // SAFETY: the layout is nonzero.
        let block = unsafe { heaps.alloc(layout) };
        assert!(!block.is_null());
        // SAFETY: the block holds `layout.size()` bytes.
        unsafe { block.write_bytes(fill, layout.size()) };
        block as usize
    }

    fn holds(block: *const u8, len: usize, fill: u8) -> bool {
        // SAFETY: the caller passes a live block of at least `len` bytes.
        unsafe { std::slice::from_raw_parts(block, len) }
            .iter()
            .all(|&byte| byte == fill)
    }

    fn has_remote_frees(heap: &Heap<Reserved>) -> bool {
        !heap.remote.0.load(Ordering::Relaxed).is_null()
    }

    fn on_worker<R: Send>(work: impl FnOnce() -> R + Send) -> R {
        bviz_parallel::scope(|scope| scope.spawn(work).join().unwrap())
    }

    #[test]
    fn an_uncontended_lock_wakes_no_one() {
        let lock = Lock::new();
        let threads = TestThreads::default();
        for _ in 0..1000 {
            drop(lock.lock(&threads));
        }
        assert_eq!(threads.wakes.load(Ordering::Relaxed), 0);
        assert_eq!(lock.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_release_wakes_a_parked_thread_once() {
        let lock = Lock::new();
        let threads = TestThreads::default();
        let held = lock.lock(&threads);
        bviz_parallel::scope(|scope| {
            let waiter = scope.spawn(|| drop(lock.lock(&threads)));
            while lock.0.load(Ordering::Relaxed) < PARKED {
                thread::yield_now();
            }
            drop(held);
            waiter.join().unwrap();
        });
        assert_eq!(threads.wakes.load(Ordering::Relaxed), 1);
        assert_eq!(lock.0.load(Ordering::Relaxed), 0);
    }

    /// Each thread increments a counter by a load and a store with a yield
    /// between them, so an increment made outside the lock is lost.
    #[test]
    fn threads_contending_for_the_lock_hold_it_one_at_a_time() {
        const THREADS: usize = 4;
        const INCREMENTS: usize = 2_000;
        let lock = Lock::new();
        let threads = TestThreads::default();
        let counter = AtomicUsize::new(0);
        bviz_parallel::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    for _ in 0..INCREMENTS {
                        let _held = lock.lock(&threads);
                        let count = counter.load(Ordering::Relaxed);
                        thread::yield_now();
                        counter.store(count + 1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(counter.into_inner(), THREADS * INCREMENTS);
        assert_eq!(lock.0.into_inner(), 0);
    }

    /// Threads whose waits return early every [`Self::SPURIOUS_EVERY`]th call
    /// and are preempted before they compare, and whose wakes arrive late.
    #[derive(Default)]
    struct FaultyThreads {
        threads: TestThreads,
        waits: AtomicUsize,
    }

    impl FaultyThreads {
        const SPURIOUS_EVERY: usize = 3;
    }

    // SAFETY: `heap` is `TestThreads`'.
    unsafe impl Threads for FaultyThreads {
        fn heap(&self) -> HeapId {
            self.threads.heap()
        }

        fn wait(&self, state: &AtomicU32, value: u32) {
            if self
                .waits
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(Self::SPURIOUS_EVERY)
            {
                return;
            }
            thread::yield_now();
            self.threads.wait(state, value);
        }

        fn wake_one(&self, state: &AtomicU32) {
            thread::yield_now();
            self.threads.wake_one(state);
        }
    }

    /// As [`threads_contending_for_the_lock_hold_it_one_at_a_time`], with
    /// enough threads that some park, under [`FaultyThreads`].
    #[test]
    fn the_lock_survives_spurious_and_late_wakes() {
        const THREADS: usize = 10;
        const INCREMENTS: usize = 2_000;
        let lock = Lock::new();
        let threads = FaultyThreads::default();
        let counter = AtomicUsize::new(0);
        bviz_parallel::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    for _ in 0..INCREMENTS {
                        let _held = lock.lock(&threads);
                        let count = counter.load(Ordering::Relaxed);
                        thread::yield_now();
                        counter.store(count + 1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(counter.into_inner(), THREADS * INCREMENTS);
        assert_eq!(lock.0.into_inner(), 0);
        assert_ne!(threads.threads.sleeps.into_inner(), 0);
    }

    #[test]
    fn a_block_freed_on_another_heaps_thread_waits_for_its_owner() {
        let heaps = &heaps();
        mark_main(heaps);

        let main_block = filled(heaps, WORDS, 1);
        assert_eq!(heaps.pages.owner(main_block as *mut u8), HeapId::Main);
        // SAFETY: the block is the main thread's, given up here.
        on_worker(|| unsafe { heaps.dealloc(main_block as *mut u8, WORDS) });
        assert!(has_remote_frees(&heaps.main));
        filled(heaps, WORDS, 2);
        assert!(!has_remote_frees(&heaps.main));

        let worker_block = on_worker(|| filled(heaps, WORDS, 3));
        assert_eq!(heaps.pages.owner(worker_block as *mut u8), HeapId::Shared);
        // SAFETY: the worker gave the block up.
        unsafe { heaps.dealloc(worker_block as *mut u8, WORDS) };
        assert!(has_remote_frees(&heaps.shared));
        on_worker(|| filled(heaps, WORDS, 4));
        assert!(!has_remote_frees(&heaps.shared));
    }

    #[test]
    fn realloc_moves_a_block_to_the_calling_threads_heap() {
        let heaps = &heaps();
        mark_main(heaps);
        let grown = 1000;

        let worker_block = on_worker(|| filled(heaps, WORDS, 5)) as *mut u8;
        // SAFETY: the worker gave the block up.
        let moved = unsafe { heaps.realloc(worker_block, WORDS, grown) };
        assert_eq!(heaps.pages.owner(moved), HeapId::Main);
        assert!(holds(moved, WORDS.size(), 5));
        assert!(has_remote_frees(&heaps.shared));

        let moved = moved as usize;
        let back = on_worker(|| {
            // SAFETY: the main thread gave the block up.
            unsafe { heaps.realloc(moved as *mut u8, WORDS, grown) as usize }
        }) as *mut u8;
        assert_eq!(heaps.pages.owner(back), HeapId::Shared);
        assert!(holds(back, WORDS.size(), 5));
        assert!(has_remote_frees(&heaps.main));
    }

    #[test]
    fn realloc_keeps_an_over_aligned_block_aligned() {
        let heaps = &heaps();
        let mut layout = Layout::from_size_align(WORDS.size(), PAGE_SIZE).unwrap();
        let mut block = filled(heaps, layout, 9) as *mut u8;
        for size in [PAGE_SIZE, 2 * WORDS.size()] {
            // SAFETY: the block is live and of `layout`, and `size` is nonzero.
            block = unsafe { heaps.realloc(block, layout, size) };
            layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
            assert_eq!(block as usize % PAGE_SIZE, 0);
            assert!(holds(block, WORDS.size(), 9));
        }
    }

    #[test]
    fn blocks_allocated_before_the_mark_stay_the_shared_heaps() {
        let heaps = &heaps();
        let early = filled(heaps, WORDS, 6) as *mut u8;
        assert_eq!(heaps.pages.owner(early), HeapId::Shared);
        mark_main(heaps);

        assert_eq!(
            heaps.pages.owner(filled(heaps, WORDS, 7) as *mut u8),
            HeapId::Main
        );
        assert!(holds(early, WORDS.size(), 6));
        // SAFETY: the block is the main thread's, given up here.
        unsafe { heaps.dealloc(early, WORDS) };
        assert!(has_remote_frees(&heaps.shared));
        on_worker(|| filled(heaps, WORDS, 8));
        assert!(!has_remote_frees(&heaps.shared));
    }

    /// Every thread allocates blocks of assorted sizes and alignments, passes
    /// each to the next thread in a ring, and frees what it receives once it
    /// finds the block intact.
    #[test]
    fn threads_freeing_each_others_blocks_keep_every_block_intact() {
        const THREADS: usize = 4;
        const BLOCKS: usize = 5_000;
        const MAX_SIZE: usize = 4096;
        const ALIGNMENTS: [usize; 3] = [8, 64, 4096];
        type Sent = (usize, Layout, u8);
        let heaps = &heaps();
        mark_main(heaps);

        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..THREADS).map(|_| mpsc::channel::<Sent>()).unzip();
        let run = |id: usize, next: mpsc::Sender<Sent>, received: mpsc::Receiver<Sent>| {
            let mut rng = fastrand::Rng::with_seed(id as u64);
            for _ in 0..BLOCKS {
                let size = rng.usize(1..=MAX_SIZE);
                let align = ALIGNMENTS[rng.usize(..ALIGNMENTS.len())];
                let layout = Layout::from_size_align(size, align).unwrap();
                let fill = rng.u8(..);
                next.send((filled(heaps, layout, fill), layout, fill))
                    .unwrap();
            }
            drop(next);
            for (block, layout, fill) in received {
                let block = block as *mut u8;
                assert_eq!(block as usize % layout.align(), 0);
                assert!(holds(block, layout.size(), fill));
                // SAFETY: the sender gave the block up.
                unsafe { heaps.dealloc(block, layout) };
            }
        };

        let mut ring = receivers
            .into_iter()
            .enumerate()
            .map(|(id, received)| (id, senders[(id + 1) % THREADS].clone(), received));
        let (main_id, main_next, main_received) = ring.next().unwrap();
        let workers: Vec<_> = ring.collect();
        drop(senders);
        bviz_parallel::scope(|scope| {
            for (id, next, received) in workers {
                scope.spawn(move || run(id, next, received));
            }
            run(main_id, main_next, main_received);
        });
    }
}
