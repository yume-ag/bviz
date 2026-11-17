//! The page bviz runs in, in the browser.

#![cfg(target_family = "wasm")]

mod cede;
mod crash;
#[cfg(not(target_feature = "atomics"))]
mod fetched;
mod files;
mod page;
#[cfg(target_feature = "atomics")]
mod served;
mod threads;

pub use cede::cede;
pub use crash::report_crashes;
#[cfg(not(target_feature = "atomics"))]
pub use fetched::{Cancel, fetch};
pub use files::{Unread, choose_files, on_files_dropped, receive_files};
pub use page::{closed, failed, fragment, ready};
pub use threads::when_threads_start;
pub use web_sys::File;

/// What a JavaScript exception says of itself.
fn js_message(error: wasm_bindgen::JsValue) -> String {
    use wasm_bindgen::JsCast;
    match error.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.message()),
        None => format!("{error:?}"),
    }
}
