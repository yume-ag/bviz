//! Headless load benchmark: parses a profile and prints the wall time, with no
//! UI so the parse can be measured in isolation and repeatedly. Set
//! `BVIZ_TRACE_TIMING=1` for the per-stage breakdown.

#[global_allocator]
static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;

fn main() {
    // SAFETY: the first statement of main, before any other thread starts.
    unsafe { bviz_alloc::retain_freed_memory_across_load() };
    let path = std::env::args()
        .nth(1)
        .expect("usage: bench_load <profile>");
    let start = bviz_time::now();
    let profile = bviz_trace_profile::Profile::from_path(std::path::Path::new(&path))
        .expect("load profile")
        .into_inner();
    let elapsed = start.elapsed().as_secs_f64();
    let slices: usize = profile
        .threads()
        .iter()
        .map(|t| profile.slices(t).len())
        .sum();
    eprintln!(
        "[bench] load: {elapsed:.3}s  ({} threads, {slices} slices)",
        profile.threads().len()
    );
}
