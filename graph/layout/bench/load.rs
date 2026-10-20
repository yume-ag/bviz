//! Headless load benchmark: reads and decodes a streamed_proto file and prints
//! the wall time, from opening the file to the decoded messages, with no UI.

use bviz_graph_layout::streamed_proto;

#[global_allocator]
static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;

fn main() {
    // SAFETY: the first statement of main, before any other thread starts.
    unsafe { bviz_alloc::retain_freed_memory_across_load() };
    let mut args = std::env::args().skip(1);
    let usage = "usage: bench_load --query|--cquery|--aquery <file>";
    let kind = args.next().expect(usage);
    let path = args.next().expect(usage);
    let start = bviz_time::now();
    let input =
        bviz_input::Input::open(std::path::Path::new(&path)).expect("open streamed_proto file");
    let size = input.size();
    let stream = input.into_reader();
    let messages = match kind.as_str() {
        "--query" => streamed_proto::read_query_targets(stream, size).map(|m| m.len()),
        "--cquery" => streamed_proto::read_cquery_results(stream, size).map(|m| m.len()),
        "--aquery" => streamed_proto::read_action_graph_containers(stream, size).map(|m| m.len()),
        _ => panic!("{usage}"),
    }
    .expect("decode streamed_proto file");
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "[bench] load: {elapsed:.3}s  ({} input bytes, {messages} messages)",
        size.unwrap_or(0)
    );
}
