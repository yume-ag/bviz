//! Interleaved A/B benchmark of the two gzip decode shapes the loader uses:
//! inflating into one whole-profile buffer (the fallback path) against
//! streaming into a reused buffer the size the pipelined parse would pick for
//! this input, printing per-round wall time and output throughput. Alternating
//! the shapes within one process keeps background-load drift out of the
//! comparison. The whole-buffer side pays the first touch of a full-size
//! output allocation each round; the streaming side never holds more than one
//! chunk.

#[global_allocator]
static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;

use std::io::Read;

const ROUNDS: usize = 4;

fn report(label: &str, round: usize, start: bviz_time::Instant, out_bytes: usize) {
    let dt = start.elapsed().as_secs_f64();
    eprintln!(
        "[{round}] {label}: {dt:.3}s  {:.2} GB/s out ({out_bytes} bytes)",
        out_bytes as f64 / dt / 1e9
    );
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: bench_inflate <profile.gz>");
    let bytes = std::fs::read(&path).expect("read input");
    let decoded = bviz_trace_profile::gzip_size_hint(&bytes)
        .expect("input is not a gzip stream declaring its uncompressed size");
    let chunk_bytes = bviz_trace_profile::pipeline_chunk_bytes(decoded);
    eprintln!("streaming buffer: {chunk_bytes} bytes");
    let mut buf = vec![0u8; chunk_bytes];
    for round in 0..ROUNDS {
        let start = bviz_time::now();
        let mut out = Vec::with_capacity(decoded);
        flate2::read::MultiGzDecoder::new(&bytes[..])
            .read_to_end(&mut out)
            .expect("whole-buffer inflate");
        report("whole-buffer", round, start, out.len());
        drop(out);

        let start = bviz_time::now();
        let mut dec = flate2::read::MultiGzDecoder::new(&bytes[..]);
        let mut n = 0usize;
        loop {
            let k = dec.read(&mut buf).expect("streaming inflate");
            if k == 0 {
                break;
            }
            n += k;
        }
        report("chunked streaming", round, start, n);
    }
}
