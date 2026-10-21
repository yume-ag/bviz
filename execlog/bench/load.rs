//! Headless load benchmark: reads an execution log, then measures what its
//! spawns read, and prints the wall time of each, with no UI so they can be
//! measured in isolation and repeatedly.
//!
//! `bench_load <execution log> [stop after milliseconds]`: given a time, the
//! measure is asked to stop that long after it starts, and the benchmark
//! prints how long it took to.

use std::time::Duration;

#[global_allocator]
static GLOBAL: bviz_alloc::MiMalloc = bviz_alloc::MiMalloc;

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: bench_load <execution log> [stop after milliseconds]";
    let path = args.next().expect(usage);
    let stop_after = args
        .next()
        .map(|millis| Duration::from_millis(millis.parse().expect(usage)));
    let start = bviz_time::now();
    let read =
        bviz_execlog::ExecLog::from_path(std::path::Path::new(&path)).expect("load execution log");
    let decoded = start.elapsed().as_secs_f64();
    let cut_off = if read.is_cut_off() { ", cut off" } else { "" };
    let read = read.into_inner();
    let log = read.log();
    eprintln!(
        "[bench] load: {decoded:.3}s  ({} spawns, {} runners{cut_off})",
        log.spawn_count(),
        log.by_runner().len()
    );
    let (wanted, stop) = bviz_parallel::stop_on_drop();
    let _wanted = match stop_after {
        Some(after) => {
            bviz_parallel::spawn(move || {
                std::thread::sleep(after);
                drop(wanted);
            });
            None
        }
        None => Some(wanted),
    };
    let measuring = bviz_time::now();
    let measured = read.measure_inputs(&stop);
    let measure = measuring.elapsed().as_secs_f64();
    let ready = start.elapsed().as_secs_f64();
    match measured {
        Some(_) => eprintln!("[bench] inputs: {measure:.3}s, ready at {ready:.3}s"),
        None => eprintln!("[bench] inputs: stopped, returned after {measure:.3}s"),
    }
    peak_memory();
}

/// Prints the process's peak resident memory, where the system reports it.
fn peak_memory() {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    if let Some(peak) = status.lines().find(|line| line.starts_with("VmHWM")) {
        eprintln!("[bench] {peak}");
    }
}
