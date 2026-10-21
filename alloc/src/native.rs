use std::alloc::{GlobalAlloc, Layout};

use libmimalloc_sys::{mi_malloc, mi_realloc, mi_zalloc};

/// mimalloc, entered through its plain calls wherever a block's own alignment
/// suffices. mimalloc 3.3.2, as libmimalloc-sys 0.1.49 bundles it, serves an
/// aligned request from its size class's free list only up to 128 words, and
/// only while that list holds a block at the alignment; otherwise, unless the
/// block size is a power of two of at most 4 KiB, it allocates a block from a
/// larger class and aligns inside it. That never refills the requested class, so once
/// the class runs dry every later request of that size wastes the difference,
/// and most of a layout's allocations are such sizes.
pub struct MiMalloc;

/// The alignment every mimalloc block has: blocks are sized in whole machine
/// words from a page start aligned wider still.
const BLOCK_ALIGN: usize = std::mem::size_of::<usize>();

// SAFETY: every block is mimalloc's: a plain call serves only a layout whose
// alignment `BLOCK_ALIGN` meets, and `mimalloc::MiMalloc`, whose contract is
// this trait's, serves the rest.
unsafe impl GlobalAlloc for MiMalloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= BLOCK_ALIGN {
            // SAFETY: mi_malloc takes any size.
            unsafe { mi_malloc(layout.size()).cast() }
        } else {
            // SAFETY: the caller upholds `alloc`'s contract.
            unsafe { mimalloc::MiMalloc.alloc(layout) }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= BLOCK_ALIGN {
            // SAFETY: mi_zalloc takes any size.
            unsafe { mi_zalloc(layout.size()).cast() }
        } else {
            // SAFETY: the caller upholds `alloc_zeroed`'s contract.
            unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` is a live mimalloc block, which mimalloc frees whatever
        // call allocated it.
        unsafe { mimalloc::MiMalloc.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() <= BLOCK_ALIGN {
            // SAFETY: `ptr` is a live mimalloc block, and the block mi_realloc
            // returns has the alignment every block has.
            unsafe { mi_realloc(ptr.cast(), new_size).cast() }
        } else {
            // SAFETY: the caller upholds `realloc`'s contract.
            unsafe { mimalloc::MiMalloc.realloc(ptr, layout, new_size) }
        }
    }
}

// `mi_option_purge_delay` in mimalloc.h's `mi_option_e`; `libmimalloc-sys`
// exports `mi_option_set` only behind its `extended` feature and no constant
// for this option, so both are declared here.
const MI_OPTION_PURGE_DELAY: i32 = 15;

unsafe extern "C" {
    fn mi_option_set(option: i32, value: std::os::raw::c_long);
}

/// Configures mimalloc to hold freed memory long enough that a load burst
/// reuses its transient buffers (worker pools, decode buffers) instead of
/// paying the purge-and-refault round-trip mid-load, while still releasing
/// idle memory eventually.
///
/// # Safety
///
/// No other thread may be running: mimalloc writes the option
/// unsynchronized, and every thread allocating reads it.
pub unsafe fn retain_freed_memory_across_load() {
    // Comfortably past a large profile's load time.
    const PURGE_DELAY_MS: std::os::raw::c_long = 30_000;
    // SAFETY: mi_option_set takes any option and value, and the caller
    // guarantees no other thread reads the option it writes.
    unsafe { mi_option_set(MI_OPTION_PURGE_DELAY, PURGE_DELAY_MS) }
}

#[cfg(test)]
mod tests {
    use super::MI_OPTION_PURGE_DELAY;
    use std::os::raw::c_long;

    unsafe extern "C" {
        fn mi_option_get(option: i32) -> c_long;
    }

    /// mimalloc reads each option's `MIMALLOC_<name>` environment variable, so
    /// the value the test target gives `MIMALLOC_PURGE_DELAY` reads back only
    /// through the purge delay's own index.
    #[test]
    fn the_purge_delay_index_names_the_purge_delay() {
        let set: c_long = std::env::var("MIMALLOC_PURGE_DELAY")
            .expect("the test target sets MIMALLOC_PURGE_DELAY")
            .parse()
            .expect("a number of milliseconds");
        // SAFETY: mi_option_get takes any option.
        assert_eq!(unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) }, set);
    }
}
