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
//! it is the standard library's own.

#[cfg(not(target_family = "wasm"))]
mod native;

#[cfg(not(target_family = "wasm"))]
pub use native::{MiMalloc as Allocator, retain_freed_memory_across_load};

#[cfg(target_family = "wasm")]
pub use std::alloc::System as Allocator;

/// Does nothing: a WebAssembly memory never shrinks, so the allocator keeps
/// every page it is freed already.
///
/// # Safety
///
/// Safe to call at any time; it is unsafe as the desktop's is.
#[cfg(target_family = "wasm")]
pub unsafe fn retain_freed_memory_across_load() {}
