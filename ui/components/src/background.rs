//! Helpers for running work off the UI thread and applying its result.

use gpui::{Context, Task};

/// Run `work` on the background executor, then apply its result to the view on
/// the UI thread and request a repaint. Returns the [`Task`]; keep it alive
/// (store it in a field) or the work is cancelled when the task drops.
pub fn load_in_background<V, T, W, A>(cx: &mut Context<V>, work: W, apply: A) -> Task<()>
where
    V: 'static,
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    A: FnOnce(&mut V, T, &mut Context<V>) + 'static,
{
    cx.spawn(async move |this, cx| {
        let result = cx.background_executor().spawn(async move { work() }).await;
        let _ = this.update(cx, |view, cx| {
            apply(view, result, cx);
            cx.notify();
        });
    })
}

/// Work started before the view that shows its result exists — from process
/// entry, say, where reading and parsing a document overlaps the application
/// shell's own startup (font-database scan, window and GPU-surface setup)
/// instead of following it. [`load_in_background`] takes the result from here,
/// so the UI thread waits on it only once it has a frame to draw.
pub struct Pending<T>(std::thread::JoinHandle<Result<T, String>>);

impl<T: Send + 'static> Pending<T> {
    pub fn start(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        Self(bviz_parallel::spawn(work))
    }

    /// Blocks until the work finishes, reporting a panic in it the way the work
    /// reports a failure of its own.
    pub fn join(self) -> Result<T, String> {
        self.0
            .join()
            .unwrap_or_else(|_| Err("the loader thread panicked".into()))
    }
}
