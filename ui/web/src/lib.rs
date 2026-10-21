//! The page bviz runs in, in the browser.

#![cfg(target_family = "wasm")]

mod crash;
mod page;

pub use crash::report_crashes;
pub use page::{closed, failed, ready};
