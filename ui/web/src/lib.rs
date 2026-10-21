//! The page bviz runs in, in the browser.

#![cfg(target_family = "wasm")]

mod crash;
mod files;
mod page;

pub use crash::report_crashes;
pub use files::{Unread, choose_files, read_files};
pub use page::{closed, failed, ready};
