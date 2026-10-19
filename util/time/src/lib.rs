//! The monotonic clock. [`Instant`] is web-time's: std's own where std has a
//! clock, so instants interoperate with every library that takes or returns
//! std's, and the browser's on `wasm32-unknown-unknown`.

pub use web_time::Instant;

/// The current instant: bviz's one call of [`Instant::now`].
pub fn now() -> Instant {
    Instant::now()
}
