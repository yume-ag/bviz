//! The page bviz runs in, in the browser.

#![cfg(target_family = "wasm")]

mod page;

pub use page::ready;
