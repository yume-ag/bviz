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
    load_future_in_background(cx, async move { work() }, apply)
}

/// [`load_in_background`] of work that awaits as it goes, such as a
/// [`Pending`] being [`finished`](Pending::finished).
pub fn load_future_in_background<V, T, W, A>(cx: &mut Context<V>, work: W, apply: A) -> Task<()>
where
    V: 'static,
    T: Send + 'static,
    W: Future<Output = T> + Send + 'static,
    A: FnOnce(&mut V, T, &mut Context<V>) + 'static,
{
    cx.spawn(async move |this, cx| {
        let result = cx.background_executor().spawn(work).await;
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
/// frame to draw. Built without, it runs where it is first awaited, so the task
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
        Self::start_future(async move { work() })
    }

    /// [`start`](Self::start) of work that awaits as it goes, so it can hand
    /// the page's one thread back while it runs.
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    pub fn start_future(work: impl Future<Output = Result<T, String>> + Send + 'static) -> Self {
        Self(Deferred::new(work))
    }

    /// Blocks until the work finishes, reporting a panic in it the way the work
    /// reports a failure of its own.
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    pub fn join(self) -> Result<T, String> {
        self.0.join().unwrap_or_else(|_| Err(PANICKED.into()))
    }

    /// The work's outcome, as [`join`](Self::join) gives it where the build
    /// has threads, blocking the thread that polls it.
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    pub async fn finished(self) -> Result<T, String> {
        self.join()
    }

    /// The work's outcome, a panic in it reported the way the work reports a
    /// failure of its own.
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    pub async fn finished(self) -> Result<T, String> {
        self.0.run().await.unwrap_or_else(|_| Err(PANICKED.into()))
    }
}

/// What a [`Pending`] whose work panicked fails with.
const PANICKED: &str = "the loader thread panicked";

/// Work held until it is asked for, then run by the task that asks.
#[cfg(any(test, all(target_family = "wasm", not(target_feature = "atomics"))))]
struct Deferred<T>(std::pin::Pin<Box<dyn Future<Output = Result<T, String>> + Send>>);

#[cfg(any(test, all(target_family = "wasm", not(target_feature = "atomics"))))]
impl<T> Deferred<T> {
    fn new(work: impl Future<Output = Result<T, String>> + Send + 'static) -> Self {
        Self(Box::pin(work))
    }

    /// The work's outcome, a panic in any of its polls caught as a thread's
    /// would be, so a caller sees it the same way where the work ran on a
    /// thread of its own.
    async fn run(self) -> std::thread::Result<Result<T, String>> {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::task::Poll;
        let mut work = self.0;
        std::future::poll_fn(
            |cx| match catch_unwind(AssertUnwindSafe(|| work.as_mut().poll(cx))) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(outcome)) => Poll::Ready(Ok(outcome)),
                Err(panic) => Poll::Ready(Err(panic)),
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    /// The output of `future`, polled on this thread until it is ready.
    fn finish<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
        }
    }

    /// Returns pending on its first poll, as work handing its thread back does.
    async fn pend_once() {
        let mut pended = false;
        std::future::poll_fn(|_| match std::mem::replace(&mut pended, true) {
            true => Poll::Ready(()),
            false => Poll::Pending,
        })
        .await
    }

    #[test]
    fn deferred_work_runs_only_once_run() {
        let ran = Arc::new(AtomicBool::new(false));
        let deferred = Deferred::new({
            let ran = Arc::clone(&ran);
            async move {
                ran.store(true, Ordering::Relaxed);
                Ok(7)
            }
        });
        assert!(!ran.load(Ordering::Relaxed));

        assert_eq!(finish(deferred.run()).ok(), Some(Ok(7)));
        assert!(ran.load(Ordering::Relaxed));
    }

    #[test]
    fn deferred_work_resumes_where_it_awaited() {
        let deferred = Deferred::new(async {
            pend_once().await;
            Ok("read")
        });
        assert_eq!(finish(deferred.run()).ok(), Some(Ok("read")));
    }

    #[test]
    fn a_panic_in_deferred_work_is_caught_after_it_awaited() {
        let deferred = Deferred::<()>::new(async {
            pend_once().await;
            panic!("reading the file")
        });
        assert!(finish(deferred.run()).is_err());
    }
}
