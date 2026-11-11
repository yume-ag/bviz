use std::alloc::Layout;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::{self, ThreadId};

use crate::heaps::{Memory, PAGE_SIZE};
use crate::lock::Park;

/// A test's memory: enough for any test.
const RESERVED_PAGES: usize = 4096;

/// A region of the process's memory that a test's heaps grow into, freed as
/// this drops.
pub struct Reserved {
    region: usize,
    base: usize,
    taken: AtomicUsize,
}

impl Reserved {
    /// The allocation the memory is aligned within. Zeroed with no alignment,
    /// an allocation this large is fresh pages that calloc does not write, so
    /// only the pages a heap grows into are touched.
    fn region_layout() -> Layout {
        Layout::from_size_align((RESERVED_PAGES + 1) * PAGE_SIZE, 1).unwrap()
    }

    pub fn leak() -> &'static Self {
        // SAFETY: the layout is nonzero.
        let region = unsafe { std::alloc::alloc_zeroed(Self::region_layout()) } as usize;
        assert_ne!(region, 0, "the test region could not be reserved");
        Box::leak(Box::new(Self {
            region,
            base: region.next_multiple_of(PAGE_SIZE),
            taken: AtomicUsize::new(0),
        }))
    }

    /// Frees the memory.
    ///
    /// # Safety
    ///
    /// No block of it is used any more.
    pub unsafe fn release(&self) {
        // SAFETY: the region came from `region_layout`.
        unsafe { std::alloc::dealloc(self.region as *mut u8, Self::region_layout()) };
    }
}

// SAFETY: the region is zeroed, the test's alone, and `grow` hands out each
// page once.
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

/// Parks as a futex does, except on the one thread that forbids it.
#[derive(Default)]
pub struct TestPark {
    pub forbidden: OnceLock<ThreadId>,
    pub wakes: AtomicUsize,
    pub sleeps: AtomicUsize,
    parking: Mutex<()>,
    unparked: Condvar,
}

impl TestPark {
    pub fn forbid_parking_here(&self) {
        self.forbidden.set(thread::current().id()).unwrap();
    }
}

impl Park for TestPark {
    fn can_park(&self) -> bool {
        self.forbidden.get() != Some(&thread::current().id())
    }

    /// `wake_one` takes `parking` too, so it cannot slip between the check
    /// and the sleep.
    fn wait(&self, state: &AtomicU32, value: u32) {
        assert!(self.can_park(), "a thread that cannot park waited");
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

/// A block of `layout` filled with `fill`, from `alloc`, as an address that
/// crosses threads.
pub fn filled(alloc: &impl std::alloc::GlobalAlloc, layout: Layout, fill: u8) -> usize {
    // SAFETY: the layout is nonzero.
    let block = unsafe { alloc.alloc(layout) };
    assert!(!block.is_null());
    // SAFETY: the block holds `layout.size()` bytes.
    unsafe { block.write_bytes(fill, layout.size()) };
    block as usize
}

pub fn holds(block: *const u8, len: usize, fill: u8) -> bool {
    // SAFETY: the caller passes a live block of at least `len` bytes.
    unsafe { std::slice::from_raw_parts(block, len) }
        .iter()
        .all(|&byte| byte == fill)
}
