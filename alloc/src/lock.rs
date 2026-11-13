use std::sync::atomic::{AtomicU32, Ordering};

/// How the threads that take a [`Lock`] wait for it.
pub trait Park {
    /// Whether the calling thread may block in [`Park::wait`]; one that may
    /// not spins until it takes the lock.
    fn can_park(&self) -> bool;

    /// Blocks while `state` holds `value`; it may return early.
    fn wait(&self, state: &AtomicU32, value: u32);

    fn wake_one(&self, state: &AtomicU32);
}

/// The bit of a [`Lock`]'s state set while a thread holds it.
const LOCKED: u32 = 1;

/// The bit of a [`Lock`]'s state set from when a release wakes a parked
/// thread until a counted thread next acquires the lock or parks; a release
/// while it is set wakes no one.
const WOKEN: u32 = 2;

/// The unit in which a [`Lock`]'s state counts the threads parked, or about
/// to park, waiting for it.
const PARKED: u32 = 4;

/// The times a thread polls a held [`Lock`] before it parks. The critical
/// sections it guards are short, so a spinning thread usually takes the lock
/// without a wait and notify, which in Firefox take one process-wide lock.
/// Measured on a graph layout's allocation load, longer spins took the CPU the
/// holder needed, and shorter ones parked too often.
const SPINS: u32 = 1000;

/// A lock that parks a thread only after spinning, and never parks one that
/// [`Park::can_park`] refuses. C code shares it as a `uint32_t`.
#[repr(transparent)]
pub struct Lock(AtomicU32);

/// A held [`Lock`], released as this drops, unwinding included.
pub struct Held<'a, P: Park> {
    lock: &'a Lock,
    park: &'a P,
}

impl Lock {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    pub fn lock<'a, P: Park>(&'a self, park: &'a P) -> Held<'a, P> {
        self.acquire(park);
        Held { lock: self, park }
    }

    pub fn try_acquire(&self) -> bool {
        let seen = self.0.load(Ordering::Relaxed);
        seen & LOCKED == 0
            && self
                .0
                .compare_exchange_weak(seen, seen | LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
    }

    pub fn acquire<P: Park>(&self, park: &P) {
        if self
            .0
            .compare_exchange_weak(0, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.acquire_contended(park);
        }
    }

    #[cold]
    fn acquire_contended<P: Park>(&self, park: &P) {
        for _ in 0..SPINS {
            if self.try_acquire() {
                return;
            }
            std::hint::spin_loop();
        }
        if !park.can_park() {
            while !self.try_acquire() {
                std::hint::spin_loop();
            }
            return;
        }
        let state = &self.0;
        let mut seen = state.fetch_add(PARKED, Ordering::Relaxed) + PARKED;
        loop {
            seen = if seen & LOCKED == 0 {
                let taken = (seen - PARKED) & !WOKEN | LOCKED;
                match state.compare_exchange_weak(seen, taken, Ordering::Acquire, Ordering::Relaxed)
                {
                    Ok(_) => return,
                    Err(now) => now,
                }
            } else if seen & WOKEN != 0 {
                let unwoken = seen & !WOKEN;
                match state.compare_exchange_weak(
                    seen,
                    unwoken,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => unwoken,
                    Err(now) => now,
                }
            } else {
                park.wait(state, seen);
                state.load(Ordering::Relaxed)
            };
        }
    }

    /// # Safety
    ///
    /// The lock is held, and its holder gives it up.
    pub unsafe fn release<P: Park>(&self, park: &P) {
        let state = &self.0;
        let mut seen = state.fetch_sub(LOCKED, Ordering::Release) - LOCKED;
        while seen >= PARKED && seen & (LOCKED | WOKEN) == 0 {
            match state.compare_exchange_weak(
                seen,
                seen | WOKEN,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return park.wake_one(state),
                Err(now) => seen = now,
            }
        }
    }
}

impl<P: Park> Drop for Held<'_, P> {
    fn drop(&mut self) {
        // SAFETY: `Lock::lock` acquired the lock for this guard alone.
        unsafe { self.lock.release(self.park) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestPark;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    #[test]
    fn an_uncontended_lock_wakes_no_one() {
        let lock = Lock::new();
        let park = TestPark::default();
        for _ in 0..1000 {
            drop(lock.lock(&park));
        }
        assert_eq!(park.wakes.load(Ordering::Relaxed), 0);
        assert_eq!(lock.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_release_wakes_a_parked_thread_once() {
        let lock = Lock::new();
        let park = TestPark::default();
        let held = lock.lock(&park);
        bviz_parallel::scope(|scope| {
            let waiter = scope.spawn(|| drop(lock.lock(&park)));
            while lock.0.load(Ordering::Relaxed) < PARKED {
                thread::yield_now();
            }
            drop(held);
            waiter.join().unwrap();
        });
        assert_eq!(park.wakes.load(Ordering::Relaxed), 1);
        assert_eq!(lock.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_thread_that_cannot_park_takes_the_lock_without_parking() {
        let lock = Lock::new();
        let park = TestPark::default();
        let held = lock.lock(&park);
        bviz_parallel::scope(|scope| {
            let spinner = scope.spawn(|| {
                park.forbid_parking_here();
                drop(lock.lock(&park));
            });
            while park.forbidden.get().is_none() {
                thread::yield_now();
            }
            thread::sleep(std::time::Duration::from_millis(10));
            assert!(lock.0.load(Ordering::Relaxed) < PARKED);
            drop(held);
            spinner.join().unwrap();
        });
        assert_eq!(park.sleeps.load(Ordering::Relaxed), 0);
        assert_eq!(lock.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_panic_under_the_lock_releases_it() {
        let lock = Lock::new();
        let park = TestPark::default();
        let unwound = std::panic::catch_unwind(|| {
            let _held = lock.lock(&park);
            panic!("under the lock");
        });
        assert!(unwound.is_err());
        assert!(lock.try_acquire());
    }

    /// Each thread increments a counter by a load and a store with a yield
    /// between them, so an increment made outside the lock is lost.
    fn contend(lock: &Lock, park: &(impl Park + Sync), threads: usize) -> usize {
        const INCREMENTS: usize = 2_000;
        let counter = AtomicUsize::new(0);
        bviz_parallel::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    for _ in 0..INCREMENTS {
                        let _held = lock.lock(park);
                        let count = counter.load(Ordering::Relaxed);
                        thread::yield_now();
                        counter.store(count + 1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(lock.0.load(Ordering::Relaxed), 0);
        counter.into_inner() / INCREMENTS
    }

    #[test]
    fn threads_contending_for_the_lock_hold_it_one_at_a_time() {
        const THREADS: usize = 4;
        assert_eq!(
            contend(&Lock::new(), &TestPark::default(), THREADS),
            THREADS
        );
    }

    /// A park whose waits return early every [`Self::SPURIOUS_EVERY`]th call
    /// and are preempted before they compare, and whose wakes arrive late.
    #[derive(Default)]
    struct FaultyPark {
        park: TestPark,
        waits: AtomicUsize,
    }

    impl FaultyPark {
        const SPURIOUS_EVERY: usize = 3;
    }

    impl Park for FaultyPark {
        fn can_park(&self) -> bool {
            self.park.can_park()
        }

        fn wait(&self, state: &AtomicU32, value: u32) {
            if self
                .waits
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(Self::SPURIOUS_EVERY)
            {
                return;
            }
            thread::yield_now();
            self.park.wait(state, value);
        }

        fn wake_one(&self, state: &AtomicU32) {
            thread::yield_now();
            self.park.wake_one(state);
        }
    }

    /// As [`threads_contending_for_the_lock_hold_it_one_at_a_time`], with
    /// enough threads that some park, under [`FaultyPark`].
    #[test]
    fn the_lock_survives_spurious_and_late_wakes() {
        const THREADS: usize = 10;
        let park = FaultyPark::default();
        assert_eq!(contend(&Lock::new(), &park, THREADS), THREADS);
        assert_ne!(park.park.sleeps.into_inner(), 0);
    }
}
