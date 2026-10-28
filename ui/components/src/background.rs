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
/// frame to draw. Built without, it runs where it is first joined, so the task
/// that starts it does not also run it.
pub struct Pending<T>(Started<T>);

#[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
type Started<T> = std::thread::JoinHandle<Result<T, String>>;

#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
type Started<T> = Deferred<T>;

impl<T: Send + 'static> Pending<T> {
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    pub fn start(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        Self(bviz_parallel::spawn(work))
    }

    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    pub fn start(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        Self(Deferred::new(work))
    }

    /// Blocks until the work finishes, reporting a panic in it the way the work
    /// reports a failure of its own.
    pub fn join(self) -> Result<T, String> {
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        let finished = self.0.join();
        #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
        let finished = self.0.run();
        finished.unwrap_or_else(|_| Err("the loader thread panicked".into()))
    }
}

/// Work held until it is asked for, then run by the thread that asks.
#[cfg(any(test, all(target_family = "wasm", not(target_feature = "atomics"))))]
struct Deferred<T>(Box<dyn FnOnce() -> Result<T, String> + Send>);

#[cfg(any(test, all(target_family = "wasm", not(target_feature = "atomics"))))]
impl<T> Deferred<T> {
    fn new(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        Self(Box::new(work))
    }

    /// The work's outcome, its panic caught as a thread's would be, so a
    /// caller sees it the same way where the work ran on a thread of its own.
    fn run(self) -> std::thread::Result<Result<T, String>> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn deferred_work_runs_only_once_run() {
        let ran = Arc::new(AtomicBool::new(false));
        let deferred = Deferred::new({
            let ran = Arc::clone(&ran);
            move || {
                ran.store(true, Ordering::Relaxed);
                Ok(7)
            }
        });
        assert!(!ran.load(Ordering::Relaxed));

        assert_eq!(deferred.run().ok(), Some(Ok(7)));
        assert!(ran.load(Ordering::Relaxed));
    }

    #[test]
    fn a_panic_in_deferred_work_is_caught() {
        let deferred = Deferred::<()>::new(|| panic!("reading the file"));
        assert!(deferred.run().is_err());
    }
}
