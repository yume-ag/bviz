//! What the page learns of bviz crashing.

use std::panic::{self, PanicHookInfo};

/// Dispatches `bviz:crashed` on the page should the module abort, and has the
/// page record each panic's message as `bvizLastPanic` to name the crash by:
/// an abort leaves no exception whose message would. A panic bviz catches is
/// recorded as well, so the record alone tells of no crash.
///
/// The console-logging hook gpui's web platform installs is installed here
/// first, beneath the recording, so the platform does not later replace it.
pub fn report_crashes() {
    console_error_panic_hook::set_once();
    let logged = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        record(info);
        logged(info);
    }));
    wasm_bindgen::handler::set_on_abort(aborted);
}

fn record(info: &PanicHookInfo) {
    let message = info.to_string();
    js_sys::Reflect::set(&js_sys::global(), &"bvizLastPanic".into(), &message.into()).ok();
}

fn aborted() {
    crate::page::dispatch("bviz:crashed", None);
}
