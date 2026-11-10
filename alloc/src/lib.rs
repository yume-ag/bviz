//! The global allocator of bviz's binaries, which each declare themselves:
//!
//! ```ignore
//! #[global_allocator]
//! static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;
//! ```
//!
//! On the desktop it is mimalloc. glibc malloc returns large freed buffers to
//! the OS eagerly, and that munmap/madvise churn is a noticeable share of a big
//! profile load; mimalloc retains freed ranges and reuses them. In the browser
//! it is dlmalloc: without threads the standard library's own, and with threads
//! two heaps of it, one for the main thread and one the workers share.
#![cfg_attr(
    all(target_family = "wasm", target_feature = "atomics"),
    feature(stdarch_wasm_atomic_wait, thread_local)
)]

#[cfg(not(target_family = "wasm"))]
mod native;

#[cfg(any(test, all(target_family = "wasm", target_feature = "atomics")))]
mod heaps;

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
mod browser;

#[cfg(not(target_family = "wasm"))]
pub use native::{MiMalloc as Allocator, retain_freed_memory_across_load};

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub use browser::{Allocator, mark_main_thread};

#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
pub use std::alloc::System as Allocator;

/// Does nothing, as a build without threads has only the main thread.
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
pub fn mark_main_thread() {}

/// Does nothing: a WebAssembly memory never shrinks, so the allocator keeps
/// every page it is freed already.
///
/// # Safety
///
/// Safe to call at any time; it is unsafe as the desktop's is.
#[cfg(target_family = "wasm")]
pub unsafe fn retain_freed_memory_across_load() {}
