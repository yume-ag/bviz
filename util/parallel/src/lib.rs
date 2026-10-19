//! bviz's threads, and how wide to fan work out across the machine. Every OS
//! thread bviz starts goes through [`spawn`] or [`scope`].

use std::sync::OnceLock;
use std::thread::{JoinHandle, Scope};

/// Runs `work` on a new OS thread.
pub fn spawn<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> JoinHandle<T> {
    std::thread::spawn(work)
}

/// Runs `work` with a scope whose threads may borrow from the caller, returning
/// once every thread spawned in it has finished.
pub fn scope<'env, T>(work: impl for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T) -> T {
    std::thread::scope(work)
}

/// Worker count for a parallel pass over bulk data: the machine's physical core
/// count rather than its logical (SMT) thread count, because such a pass is
/// memory-bandwidth bound, so threads past the cores that own the memory
/// channels mostly stall on bandwidth, burning CPU for a wall-time gain that has
/// flattened. Clamped to the parallelism the process is actually granted, so a
/// CPU quota or affinity mask is honored, and to at least one worker, so a
/// caller may size, multiply or divide by it unguarded.
pub fn worker_count() -> usize {
    let granted = std::thread::available_parallelism().map_or(1, |n| n.get());
    num_cpus::get_physical().min(granted).max(1)
}

/// Stack for a worker running the graph layout engine over a whole graph: the
/// engine recurses over the frame it is given, so the depth follows the frame's
/// size, and placing a whole graph hands it far larger frames than laying out
/// one view does. Sized well clear of what the largest frames of a
/// whole-repository build graph reach.
const LAYOUT_RECURSION_STACK: usize = 8 << 20;

/// The process's one pool for parallel passes over bulk data that the reader
/// waits on, [`worker_count`] wide. Every such rayon entry point runs inside an
/// `install` or `scope` on it, so however many are in flight they fan out that
/// wide and no wider: a parallel iterator left outside runs instead on rayon's
/// implicit global pool, a second set of workers sized by logical threads.
pub fn workers() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count())
            .build()
            .expect("build the worker pool")
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
pub fn background_workers() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count().saturating_sub(1).max(1))
            .stack_size(LAYOUT_RECURSION_STACK)
            .build()
            .expect("build the background worker pool")
    })
}
