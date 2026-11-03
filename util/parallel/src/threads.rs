use std::sync::OnceLock;

#[cfg(not(target_family = "wasm"))]
use std::thread as platform;
#[cfg(target_family = "wasm")]
use wasm_thread as platform;

pub use platform::{Scope, ScopedJoinHandle};

use crate::{assert_can_block, worker_count};

/// A thread [`spawn`] started, waited on for what its work returns.
pub struct JoinHandle<T>(platform::JoinHandle<T>);

impl<T> JoinHandle<T> {
    /// Blocks until the thread finishes, with what its work returned or the
    /// panic that ended it.
    pub fn join(self) -> std::thread::Result<T> {
        assert_can_block();
        self.0.join()
    }

    /// What [`join`](Self::join) returns, blocking the thread that polls it.
    #[cfg(not(target_family = "wasm"))]
    pub async fn finished(self) -> std::thread::Result<T> {
        self.join()
    }

    /// What [`join`](Self::join) returns, awaited without blocking.
    #[cfg(target_family = "wasm")]
    pub async fn finished(self) -> std::thread::Result<T> {
        self.0.join_async().await
    }
}

/// Runs `work` on a new thread.
pub fn spawn<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> JoinHandle<T> {
    #[cfg_attr(
        not(target_family = "wasm"),
        expect(clippy::disallowed_methods, reason = "the one start of a thread")
    )]
    let started = platform::spawn(work);
    JoinHandle(started)
}

/// Runs `work` with a scope whose threads may borrow from the caller, returning
/// once every thread spawned in it has finished.
pub fn scope<'env, T>(work: impl for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T) -> T {
    assert_can_block();
    #[cfg_attr(
        not(target_family = "wasm"),
        expect(clippy::disallowed_methods, reason = "the one start of a thread scope")
    )]
    platform::scope(work)
}

/// Runs `work(0)` to `work(workers - 1)` on a thread each, returning their
/// results in that order.
pub fn fan_out<T: Send>(workers: usize, work: impl Fn(usize) -> T + Sync) -> Vec<T> {
    scope(|scope| {
        let work = &work;
        let handles: Vec<_> = (0..workers).map(|i| scope.spawn(move || work(i))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Stack for a worker running the graph layout engine over a whole graph: the
/// engine recurses over the frame it is given, so the depth follows the frame's
/// size, and placing a whole graph hands it far larger frames than laying out
/// one view does. Sized well clear of what the largest frames of a
/// whole-repository build graph reach.
const LAYOUT_RECURSION_STACK: usize = 8 << 20;

/// A pool of threads of its own.
pub struct Pool(rayon::ThreadPool);

impl Pool {
    /// Runs `op` on the pool, blocking the caller until it returns, as rayon's
    /// [`ThreadPool::install`](rayon::ThreadPool::install) does.
    pub fn install<OP, R>(&self, op: OP) -> R
    where
        OP: FnOnce() -> R + Send,
        R: Send,
    {
        assert_can_block();
        self.0.install(op)
    }
}

/// The process's one pool for parallel passes over bulk data that the reader
/// waits on, [`worker_count`] wide. Every such rayon entry point runs inside an
/// `install` or `scope` on it, so however many are in flight they fan out that
/// wide and no wider: a parallel iterator left outside runs instead on rayon's
/// global pool, a second set of workers, sized natively by logical threads.
pub fn workers() -> &'static Pool {
    assert_can_block();
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let builder = rayon::ThreadPoolBuilder::new().num_threads(worker_count());
        build(builder).expect("build the worker pool")
    })
}

/// The pool for a long pass that runs while the reader goes on working, one
/// worker narrower than [`workers`].
///
/// A worker takes up the jobs its own pool's passes queue before a pass
/// entering from outside, so on [`workers`] a pass the reader waits on would
/// wait out every job such a pass had queued there. A pool of its own keeps
/// that pass's latency to its own work, at the cost of fanning out past the
/// physical cores while both run. The reader may be waiting on the long pass
/// too, as a map is drawn only once placed, so the pool gives up a single
/// worker: that bounds the oversubscription at little cost to the pass.
pub fn background_workers() -> &'static Pool {
    assert_can_block();
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let builder = rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count().saturating_sub(1).max(1))
            .stack_size(LAYOUT_RECURSION_STACK);
        build(builder).expect("build the background worker pool")
    })
}

#[cfg(not(target_family = "wasm"))]
fn build(builder: rayon::ThreadPoolBuilder) -> Result<Pool, rayon::ThreadPoolBuildError> {
    builder.build().map(Pool)
}

#[cfg(target_family = "wasm")]
fn build(builder: rayon::ThreadPoolBuilder) -> Result<Pool, rayon::ThreadPoolBuildError> {
    builder.spawn_handler(start_worker).build().map(Pool)
}

/// Builds rayon's global pool, [`worker_count`] wide, on threads that the
/// browser can start: where rayon cannot start one, it runs the pool on
/// whichever thread first reaches it, which a parallel iterator left outside an
/// `install` would then hold up. Building it waits for its workers to start, so
/// it is built on the thread whose handle this returns; until that thread
/// finishes, nothing may reach the global pool.
#[cfg(target_family = "wasm")]
pub fn build_global_pool() -> JoinHandle<()> {
    spawn(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count())
            .spawn_handler(start_worker)
            .build_global()
            .expect("build rayon's global pool");
    })
}

/// Starts a rayon worker on a thread of its own, named and with the stack its
/// pool gives it.
#[cfg(target_family = "wasm")]
fn start_worker(worker: rayon::ThreadBuilder) -> std::io::Result<()> {
    let mut thread = wasm_thread::Builder::new();
    if let Some(name) = worker.name() {
        thread = thread.name(name.to_owned());
    }
    if let Some(stack) = worker.stack_size() {
        thread = thread.stack_size(stack);
    }
    thread.spawn(|| worker.run()).map(drop)
}
