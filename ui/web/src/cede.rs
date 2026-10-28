//! Handing the page's one thread back to the browser.

use std::task::Poll;

/// Lets the browser handle what waits on the page's one thread, such as input
/// and painting, before the awaiting task goes on: gpui's browser dispatcher
/// runs a woken task in a timer callback of its own. A hidden page goes on at
/// once, since a browser runs a hidden page's timers about once a second at
/// most, which would stretch out work there with nothing to show for it.
pub async fn cede() {
    if page_hidden() {
        return;
    }
    let mut ceded = false;
    std::future::poll_fn(|cx| {
        if std::mem::replace(&mut ceded, true) {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}

fn page_hidden() -> bool {
    (web_sys::window().and_then(|window| window.document())).is_some_and(|page| page.hidden())
}
