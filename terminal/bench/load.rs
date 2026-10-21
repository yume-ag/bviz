//! Headless load benchmark: reads a build event binary file and prints the
//! wall time, from opening the file to the parsed output, with no UI.

#[global_allocator]
static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;

fn main() {
    // SAFETY: the first statement of main, before any other thread starts.
    unsafe { bviz_alloc::retain_freed_memory_across_load() };
    let path = std::env::args()
        .nth(1)
        .expect("usage: bench_load <build event binary file>");
    let start = bviz_time::now();
    let input =
        bviz_input::Input::open(std::path::Path::new(&path)).expect("open build event file");
    let size = input.size();
    let read = bviz_terminal::read(input.into_reader(), size).expect("read build event file");
    let elapsed = start.elapsed().as_secs_f64();
    let cut_off = if read.is_cut_off() { ", cut off" } else { "" };
    let output = read.into_inner();
    eprintln!(
        "[bench] load: {elapsed:.3}s  ({} input bytes, {} output bytes{cut_off})",
        size.unwrap_or(0),
        output.bytes.len()
    );
}
