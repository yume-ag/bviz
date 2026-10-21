/// The calling thread, run as a pool. The parallel iterators a pass runs inside
/// [`install`](Pool::install) fall to rayon's global pool, which rayon runs on
/// the calling thread where no thread can start.
pub struct Pool(());

impl Pool {
    /// Runs `op` on the calling thread, taking what rayon's
    /// [`ThreadPool::install`](rayon::ThreadPool::install) takes.
    pub fn install<OP, R>(&self, op: OP) -> R
    where
        OP: FnOnce() -> R + Send,
        R: Send,
    {
        op()
    }
}

static POOL: Pool = Pool(());

/// The one pool there is.
pub fn workers() -> &'static Pool {
    &POOL
}

/// The one pool there is: a pass the reader waits on and a long one share the
/// thread either way.
pub fn background_workers() -> &'static Pool {
    &POOL
}

/// Runs `work(0)` to `work(workers - 1)` in turn on the calling thread,
/// returning their results in that order. A call must not wait on a later one,
/// which begins only once it has returned.
pub fn fan_out<T: Send>(workers: usize, work: impl Fn(usize) -> T + Sync) -> Vec<T> {
    (0..workers).map(work).collect()
}
