//! The page bviz runs in, in the browser.

#![cfg(target_family = "wasm")]

mod cede;
mod crash;
mod files;
mod page;
#[cfg(target_feature = "atomics")]
mod served;
mod threads;

pub use cede::cede;
pub use crash::report_crashes;
pub use files::{Unread, choose_files, on_files_dropped, receive_files};
pub use page::{closed, failed, fragment, ready};
pub use threads::when_threads_start;
pub use web_sys::File;
