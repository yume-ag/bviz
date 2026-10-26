use std::collections::BTreeMap;
use std::io::Read;

use bviz_input::Input;
use bviz_parallel::worker_count;

use super::{
    PieceBuf, PieceSink, StreamChunk, StreamEnd, StreamEnds, gzip_input, seal_stream, stream_pieces,
};
use crate::concurrent_interner::ConcurrentInterner;
use crate::model::Profile;
use crate::parse::build::{Builder, Merge};
use crate::parse::decode::gzip_size_hint;
use crate::parse::pipeline::{CHUNKS_PER_WORKER, MIN_CHUNK_BYTES, MIN_PARALLEL_BYTES, parse_chunk};
use crate::parse::{BYTES_PER_DISTINCT, intern_prefault_bytes, timed};

/// Ceiling on a pipelined chunk buffer, bounding the memory the bounded queue
/// plus in-flight worker chunks can hold on an arbitrarily large profile.
const PIPELINE_MAX_CHUNK_BYTES: usize = 32 << 20;

/// The pipelined decode of a gzipped profile held whole in memory, sized from
/// that buffer's own [`gzip_size_hint`]. Returns `None` — leaving the input to
/// the whole-buffer path — for a missing or zero trailer, a size
/// [`pipeline_params`] declines, or any decode or parse failure.
pub(crate) fn parse_gz_pipelined(bytes: &[u8]) -> Option<Profile> {
    let decoded_size = gzip_size_hint(bytes)?;
    let (workers, chunk_bytes) = pipeline_params(decoded_size)?;
    parse_gz_stream_from(bytes, workers, chunk_bytes, decoded_size)
}

/// The pipelined decode streamed from a seekable gzipped `input`, sized from
/// its trailer. `None` for plain JSON, and for an input [`parse_gz_pipelined`]
/// declines.
pub(crate) fn parse_gz_pipelined_input(input: &Input) -> Option<Profile> {
    let (reader, decoded_size) = gzip_input(input)?;
    timed("decode+parse (pipelined, streamed)", || {
        parse_gz_pipelined_reader(reader, decoded_size)
    })
}

/// The pipelined decode over a streaming reader whose size hint was read ahead
/// of the stream: [`parse_gz_pipelined`] without the whole compressed
/// buffer in hand. `None` for the same size gates.
fn parse_gz_pipelined_reader(reader: impl Read + Send, decoded_size: usize) -> Option<Profile> {
    let (workers, chunk_bytes) = pipeline_params(decoded_size)?;
    parse_gz_stream_from(reader, workers, chunk_bytes, decoded_size)
}

/// The buffer a pipelined decode reads into, for a stream declaring
/// `decoded_size` uncompressed bytes.
pub fn pipeline_chunk_bytes(decoded_size: usize) -> usize {
    (decoded_size / (worker_count() * CHUNKS_PER_WORKER))
        .clamp(MIN_CHUNK_BYTES, PIPELINE_MAX_CHUNK_BYTES)
}

/// Worker count and per-chunk byte target for a pipelined decode of a stream
/// declaring `decoded_size` uncompressed bytes, or `None` when the pipeline
/// declines — a single core, or a stream under the parallel threshold.
pub(crate) fn pipeline_params(decoded_size: usize) -> Option<(usize, usize)> {
    let workers = worker_count();
    if workers < 2 || decoded_size < MIN_PARALLEL_BYTES {
        return None;
    }
    Some((workers, pipeline_chunk_bytes(decoded_size)))
}

/// One newline-aligned run of decoded profile bytes, numbered so builders can
/// be reassembled in array order after out-of-order parsing.
struct Piece {
    idx: usize,
    buf: PieceBuf,
}

/// The producer's end of the piece queue, and the one place a piece is
/// numbered: the indices the merger reorders by and the count it must account
/// for come from the same running total, so they cannot drift apart. A send
/// blocks while the queue is full, holding the producer to the workers' pace.
struct PieceQueue {
    tx: crossbeam_channel::Sender<Piece>,
    sent: usize,
}

impl PieceSink for PieceQueue {
    const RELEASES_ON_SEND: bool = false;

    /// `None` once the consumers are gone.
    fn send(&mut self, buf: PieceBuf) -> Option<()> {
        self.tx
            .send(Piece {
                idx: self.sent,
                buf,
            })
            .ok()?;
        self.sent += 1;
        Some(())
    }

    fn sent(&self) -> usize {
        self.sent
    }
}

/// Takes the decoded size from the stream's own [`gzip_size_hint`], or zero
/// when it gives none.
#[cfg(test)]
pub(crate) fn parse_gz_stream(bytes: &[u8], workers: usize, chunk_bytes: usize) -> Option<Profile> {
    parse_gz_stream_from(
        bytes,
        workers,
        chunk_bytes,
        gzip_size_hint(bytes).unwrap_or(0),
    )
}

/// One chunk parser, running until the queue closes or the merger is gone.
/// Each piece is parsed into its own [`Builder`], keeping the workers
/// independent of each other, and passed on under the piece's number.
fn parse_pieces(
    rx: &crossbeam_channel::Receiver<Piece>,
    builder_tx: std::sync::mpsc::Sender<(usize, Option<Builder>)>,
    pool_tx: std::sync::mpsc::Sender<Vec<u8>>,
    interner: &ConcurrentInterner,
    shard: usize,
    workers: usize,
    prefault_bytes: usize,
) {
    // Fault this worker's arena share before the receive loop, overlapping the
    // producer's initial inflate.
    interner.prefault_shard(shard, workers, prefault_bytes);
    let mut writer = interner.writer();
    let mut args_cap = 0;
    while let Ok(piece) = rx.recv() {
        let chunk = StreamChunk {
            buf: piece.buf,
            pool: &pool_tx,
        };
        let builder = parse_chunk(chunk, &mut writer, &mut args_cap);
        if builder_tx.send((piece.idx, builder)).is_err() {
            break;
        }
    }
}

/// Folds parsed builders into one [`Merge`]. Chunks finish out of order, so each
/// builder waits until its turn in array order comes and is folded then, while
/// later chunks are still decoding and parsing — after the last chunk only the
/// final seal remains. `runs_hint` pre-sizes the merge for the pieces expected.
/// Returns the merge and how many builders it folded, or `None` if any chunk
/// failed to parse.
fn fold_pieces(
    builder_rx: std::sync::mpsc::Receiver<(usize, Option<Builder>)>,
    runs_hint: usize,
) -> Option<(Merge, usize)> {
    let mut merge = Merge::with_capacity(runs_hint, 0);
    let mut pending: BTreeMap<usize, Builder> = BTreeMap::new();
    let mut next = 0usize;
    let mut failed = false;
    while let Ok((idx, builder)) = builder_rx.recv() {
        match builder {
            Some(builder) if !failed => {
                pending.insert(idx, builder);
            }
            Some(_) => {}
            None => {
                failed = true;
                pending.clear();
            }
        }
        while let Some(builder) = pending.remove(&next) {
            merge.fold(builder);
            next += 1;
        }
    }
    (!failed).then_some((merge, next))
}

/// Runs the producer decoding `reader`, the `workers` chunk parsers, and the
/// merger against each other. The piece queue is bounded by the worker count,
/// so the producer decodes no further than that many pieces ahead of the
/// parsers. Returns the stream's ends and the folded merge, each `None` when
/// its own stage failed.
fn decode_parse_fold(
    reader: impl Read + Send,
    interner: &ConcurrentInterner,
    workers: usize,
    chunk_bytes: usize,
    decoded_size: usize,
) -> (Option<StreamEnds>, Option<(Merge, usize)>) {
    let (tx, rx) = crossbeam_channel::bounded::<Piece>(workers);
    let (builder_tx, builder_rx) = std::sync::mpsc::channel::<(usize, Option<Builder>)>();
    let (pool_tx, pool_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let prefault_bytes = intern_prefault_bytes(decoded_size);
    timed("  decode+parse+fold chunks", || {
        bviz_parallel::scope(|scope| {
            // One buffer more than the piece queue and the parsers can have in
            // flight together, so whenever the producer blocks on the pool a
            // return is already on its way.
            let ring = 2 * workers + 1;
            let producer = scope.spawn(move || {
                let mut queue = PieceQueue { tx, sent: 0 };
                stream_pieces(reader, chunk_bytes, ring, &mut queue, pool_rx)
                    .and_then(StreamEnd::closed)
            });
            for shard in 0..workers {
                let builder_tx = builder_tx.clone();
                let pool_tx = pool_tx.clone();
                let rx = &rx;
                scope.spawn(move || {
                    parse_pieces(
                        rx,
                        builder_tx,
                        pool_tx,
                        interner,
                        shard,
                        workers,
                        prefault_bytes,
                    )
                });
            }
            drop(pool_tx);
            // The workers hold the only remaining senders, so the merger's
            // channel closes when the last of them finishes.
            drop(builder_tx);
            let merger =
                scope.spawn(move || fold_pieces(builder_rx, decoded_size.div_ceil(chunk_bytes)));
            let ends = producer.join().unwrap();
            (ends, merger.join().unwrap())
        })
    })
}

/// Parses a stream declaring `decoded_size` uncompressed bytes in pieces of
/// about `chunk_bytes`. Transcoding runs per chunk on the workers, off the
/// decode leg, and chunk buffers are reused as pieces finish parsing, so the
/// whole decoded text never exists in memory at once. `None` if the decode or
/// any chunk parse fails, if the folded builders do not account for every
/// piece cut, if no piece was cut at all — an empty `traceEvents` array — or if
/// the distinct text outgrows a string pool sized from `decoded_size`, which
/// only the stream's trailer vouches for.
fn parse_gz_stream_from(
    reader: impl Read + Send,
    workers: usize,
    chunk_bytes: usize,
    decoded_size: usize,
) -> Option<Profile> {
    let interner = ConcurrentInterner::new(decoded_size, decoded_size / BYTES_PER_DISTINCT).ok()?;
    parse_gz_stream_into(reader, interner, workers, chunk_bytes, decoded_size)
}

/// [`parse_gz_stream_from`] interning into the given `interner`.
pub(crate) fn parse_gz_stream_into(
    reader: impl Read + Send,
    interner: ConcurrentInterner,
    workers: usize,
    chunk_bytes: usize,
    decoded_size: usize,
) -> Option<Profile> {
    let (ends, merged) = decode_parse_fold(reader, &interner, workers, chunk_bytes, decoded_size);
    let ends = ends?;
    let (merge, folded) = merged?;
    if folded != ends.pieces || folded == 0 {
        return None;
    }
    seal_stream(interner, merge, ends).ok()
}
