//! What bviz tells the page it runs in, as events on the page's window.

use web_sys::Event;

/// Tells the page that bviz's window stands.
pub fn ready() {
    dispatch("bviz:ready");
}

fn dispatch(name: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let Ok(event) = Event::new(name) {
        // A page that cannot be told has nothing else to be told through.
        window.dispatch_event(&event).ok();
    }
}
