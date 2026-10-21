//! bviz's threads, and how wide to fan work out across the machine. The threads
//! bviz starts itself go through [`spawn`] or [`scope`]; those of its pools,
//! through rayon's own pool builder.
//!
//! On wasm32-unknown-unknown the standard library cannot start a thread, even
//! with the atomics feature: that takes a spawn backed by a Web Worker, which
//! this crate does not provide. Built without atomics, the crate has neither
//! `spawn` nor `scope`, and runs its fan-outs and pools on the calling thread.
//! Built with them, it compiles the threaded paths, and `spawn`, `scope`,
//! `fan_out` and building either pool panic when called.

#[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
mod threads;
#[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
pub use threads::{Pool, background_workers, fan_out, scope, spawn, workers};

#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
mod one_thread;
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
pub use one_thread::{Pool, background_workers, fan_out, workers};

/// Worker count for a parallel pass over bulk data: the machine's physical core
/// count rather than its logical (SMT) thread count, because such a pass is
/// memory-bandwidth bound, so threads past the cores that own the memory
/// channels mostly stall on bandwidth, burning CPU for a wall-time gain that has
/// flattened. At least one worker, so a caller may size, multiply or divide by
/// it unguarded.
pub fn worker_count() -> usize {
    physical_cores().max(1)
}

/// The physical cores, clamped to the parallelism the process is actually
/// granted, so a CPU quota or affinity mask is honored.
#[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
fn physical_cores() -> usize {
    let granted = std::thread::available_parallelism().map_or(1, |n| n.get());
    num_cpus::get_physical().min(granted)
}

/// The one thread a build without threads has.
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
fn physical_cores() -> usize {
    1
}
