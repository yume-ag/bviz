//! What bviz tells the page it runs in, as events on the page's window, and
//! what it reads of the page.

use web_sys::Event;

/// Tells the page that bviz's window stands.
pub fn ready() {
    dispatch("bviz:ready", None);
}

/// Tells the page that bviz's window could not open, for the reason `why`.
pub fn failed(why: &str) {
    dispatch("bviz:failed", Some(why));
}

/// Tells the page that bviz's window has closed.
pub fn closed() {
    dispatch("bviz:closed", None);
}

/// The fragment of the page's URL, with its `#`; empty where it has none.
pub fn fragment() -> String {
    web_sys::window()
        .and_then(|window| window.location().hash().ok())
        .unwrap_or_default()
}

/// Dispatches the event `name`, carrying `message` as its property of that
/// name where there is one.
pub(crate) fn dispatch(name: &str, message: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(event) = Event::new(name) else {
        return;
    };
    if let Some(message) = message {
        js_sys::Reflect::set(&event, &"message".into(), &message.into()).ok();
    }
    // A page that cannot be told has nothing else to be told through.
    window.dispatch_event(&event).ok();
}
