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

/// Work started before the view that shows its result exists, such as reading
/// and parsing a document from process entry; [`load_in_background`] takes the
/// result from here. Built with threads, the work runs on a thread of its own,
/// overlapping the application shell's own startup (font-database scan, window
/// and GPU-surface setup), so the UI thread waits on it only once it has a
/// frame to draw. Built without, it runs to its end as it starts.
pub struct Pending<T>(Started<T>);

#[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
type Started<T> = std::thread::JoinHandle<Result<T, String>>;

/// Boxed, so a `Pending` stays pointer-sized however large its outcome.
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
type Started<T> = Box<std::thread::Result<Result<T, String>>>;

impl<T: Send + 'static> Pending<T> {
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    pub fn start(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        Self(bviz_parallel::spawn(work))
    }

    /// The work's panic is caught as a thread's would be, so a caller sees it
    /// the same way where the work ran on a thread of its own.
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    pub fn start(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        Self(Box::new(std::panic::catch_unwind(
            std::panic::AssertUnwindSafe(work),
        )))
    }

    /// Blocks until the work finishes, reporting a panic in it the way the work
    /// reports a failure of its own.
    pub fn join(self) -> Result<T, String> {
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        let finished = self.0.join();
        #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
        let finished = *self.0;
        finished.unwrap_or_else(|_| Err("the loader thread panicked".into()))
    }
}
