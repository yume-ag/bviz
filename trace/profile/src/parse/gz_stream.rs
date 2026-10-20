use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufReader, Read};

use bviz_input::Input;
use bviz_parallel::worker_count;
use flate2::read::MultiGzDecoder;

use crate::concurrent_interner::{ConcurrentInterner, PoolOverflow};
use crate::model::Profile;

use super::build::{Builder, Merge, seal};
use super::decode::{gzip_size_hint, gzip_size_hint_at, latin1_cow, latin1_to_utf8};
use super::pipeline::{
    CHUNKS_PER_WORKER, Chunk, MIN_CHUNK_BYTES, MIN_PARALLEL_BYTES, TraceEventsArray, array_close,
    find_trace_events_array, parse_chunk,
};
use super::{
    BYTES_PER_DISTINCT, GZIP_MAGIC, intern_prefault_bytes, merge_other_data, recover_other_data,
    timed,
};

/// Read-buffer capacity for the compressed input, sized to amortize read
/// syscalls over the streamed inflate of a large profile.
const GZIP_READ_BUFFER_BYTES: usize = 1 << 20;

/// Ceiling on a pipelined chunk buffer, bounding the memory the bounded queue
/// plus in-flight worker chunks can hold on an arbitrarily large profile.
const PIPELINE_MAX_CHUNK_BYTES: usize = 32 << 20;

/// A run of decoded bytes without a newline longer than this aborts the
/// pipelined decode: chunks are cut at newlines, so a profile that is not
/// one-event-per-line would otherwise grow one chunk without bound. The
/// whole-buffer fallback parses such input instead.
const PIPELINE_MAX_EVENT_BYTES: usize = 64 << 20;

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
/// its trailer. `None` for an input the pipeline declines.
pub(super) fn parse_gz_pipelined_input(input: &Input) -> Option<Profile> {
    if !input.head()?.starts_with(&GZIP_MAGIC) {
        return None;
    }
    let decoded_size = gzip_size_hint_at(input.seekable()?, input.size()?)?;
    let reader = BufReader::with_capacity(GZIP_READ_BUFFER_BYTES, input.reader()?);
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

/// The bytes behind a piece: normally a buffer from the ring, carrying its
/// filled length; a run too long for a ring buffer (an event line longer than a
/// chunk) rides in a one-off buffer instead, dropped after its parse rather
/// than returned to the ring.
enum PieceBuf {
    Ring { data: Vec<u8>, len: usize },
    OneOff(Vec<u8>),
}

impl PieceBuf {
    /// The whole writable region, independent of the filled length.
    fn region(&mut self) -> &mut [u8] {
        match self {
            PieceBuf::Ring { data, .. } => data,
            PieceBuf::OneOff(v) => v,
        }
    }

    /// The filled prefix a consumer may read.
    fn bytes(&self) -> &[u8] {
        match self {
            PieceBuf::Ring { data, len } => &data[..*len],
            PieceBuf::OneOff(v) => v,
        }
    }

    fn set_len(&mut self, n: usize) {
        match self {
            PieceBuf::Ring { len, .. } => *len = n,
            PieceBuf::OneOff(v) => v.truncate(n),
        }
    }
}

/// A streamed chunk: its decoded bytes read as Latin-1, and a buffer that goes
/// back to the producer's ring once parsed (a one-off run keeps nothing to
/// return).
struct StreamChunk<'pool> {
    buf: PieceBuf,
    pool: &'pool std::sync::mpsc::Sender<Vec<u8>>,
}

impl Chunk for StreamChunk<'_> {
    fn text(&self) -> Cow<'_, str> {
        latin1_cow(self.buf.bytes())
    }

    fn release(self) {
        if let PieceBuf::Ring { data, .. } = self.buf {
            let _ = self.pool.send(data);
        }
    }
}

/// What the producer keeps besides the event chunks: the two regions outside
/// the `traceEvents` array, which is where [`merge_other_data`] finds the
/// profile's metadata.
struct StreamEnds {
    head: Vec<u8>,
    tail: Vec<u8>,
    pieces: usize,
}

/// Reads until `buf` is full or the stream ends; returns the filled length
/// (short of `buf.len()` exactly at end of stream).
fn fill(dec: &mut impl Read, buf: &mut [u8], mut filled: usize) -> std::io::Result<usize> {
    while filled < buf.len() {
        match dec.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Decodes until the `traceEvents` array opens, doubling the buffer while it
/// has not, and returns the bytes before the array's contents together with
/// those decoded past them. `None` on a decode error, on a stream that ends
/// before the array opens, and on a preamble reaching
/// [`PIPELINE_MAX_EVENT_BYTES`].
fn read_to_array_start(dec: &mut impl Read, chunk_bytes: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buf = vec![0u8; chunk_bytes];
    let mut filled = fill(dec, &mut buf, 0).ok()?;
    let array_start = loop {
        if let TraceEventsArray::Found { content_start, .. } =
            find_trace_events_array(&buf[..filled])
        {
            break content_start;
        }
        if filled < buf.len() || buf.len() >= PIPELINE_MAX_EVENT_BYTES {
            return None;
        }
        buf.resize(buf.len() * 2, 0);
        filled = fill(dec, &mut buf, filled).ok()?;
    };
    Some((
        buf[..array_start].to_vec(),
        buf[array_start..filled].to_vec(),
    ))
}

/// Supplies the buffer each piece is decoded into. A buffer comes back through
/// the ring once a worker has parsed its piece, or straight to the stash when
/// its chunk yielded no piece at all; the stash is drawn on before the ring, so
/// a buffer put aside is reused rather than waited for.
struct BufferPool {
    pool: std::sync::mpsc::Receiver<Vec<u8>>,
    stash: Vec<Vec<u8>>,
    chunk_bytes: usize,
}

impl BufferPool {
    /// Allocates the ring's `count` buffers of `chunk_bytes` each on the
    /// calling thread, so a producer building its pool before its first decode
    /// overlaps the allocation with whatever the consumers do before their
    /// first piece.
    fn new(
        pool: std::sync::mpsc::Receiver<Vec<u8>>,
        count: usize,
        chunk_bytes: usize,
    ) -> BufferPool {
        BufferPool {
            pool,
            stash: (0..count).map(|_| vec![0u8; chunk_bytes]).collect(),
            chunk_bytes,
        }
    }

    /// A buffer sized for `carry_len` carried bytes and a decode after them. A
    /// carry past half a ring buffer leaves too little room to decode into, so
    /// it moves to a one-off buffer of twice its length. `None` once the stash
    /// is empty and the ring is closed.
    fn take(&mut self, carry_len: usize) -> Option<PieceBuf> {
        if carry_len * 2 > self.chunk_bytes {
            return Some(PieceBuf::OneOff(vec![0u8; carry_len * 2]));
        }
        let data = match self.stash.pop() {
            Some(data) => data,
            None => self.pool.recv().ok()?,
        };
        Some(PieceBuf::Ring { data, len: 0 })
    }

    fn put_back(&mut self, buf: PieceBuf) {
        if let PieceBuf::Ring { data, .. } = buf {
            self.stash.push(data);
        }
    }
}

/// Copies `carry` to the front of `buf` and decodes after it until the buffer
/// is full or the stream ends, setting the filled length. `true` when the fill
/// ran short, which happens exactly at end of stream.
fn fill_piece(dec: &mut impl Read, buf: &mut PieceBuf, carry: &[u8]) -> std::io::Result<bool> {
    let region = buf.region();
    region[..carry.len()].copy_from_slice(carry);
    let filled = fill(dec, region, carry.len())?;
    let end_of_stream = filled < region.len();
    buf.set_len(filled);
    Ok(end_of_stream)
}

/// Splits the array's closing bracket and the trailing otherData that may
/// follow it off `buf`, leaving `buf` the event content before it. `None` when
/// the stream held no `]`.
fn split_off_tail(buf: &mut PieceBuf) -> Option<Vec<u8>> {
    let close = array_close(buf.bytes())?;
    let tail = buf.bytes()[close..].to_vec();
    buf.set_len(close);
    Some(tail)
}

/// Cuts `buf` after its last newline, moving the bytes past it into `carry` to
/// head the next piece. `false` when the buffer holds no newline, leaving both
/// untouched.
fn cut_at_newline(buf: &mut PieceBuf, carry: &mut Vec<u8>) -> bool {
    let Some(newline) = buf.bytes().iter().rposition(|&b| b == b'\n') else {
        return false;
    };
    let cut = newline + 1;
    carry.clear();
    carry.extend_from_slice(&buf.bytes()[cut..]);
    buf.set_len(cut);
    true
}

/// The producer's end of the piece queue, and the one place a piece is
/// numbered: the indices the merger reorders by and the count it must account
/// for come from the same running total, so they cannot drift apart.
struct PieceSink {
    tx: crossbeam_channel::Sender<Piece>,
    sent: usize,
}

impl PieceSink {
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
}

/// The producer side of the pipelined decode: streams the inflate and sends
/// newline-aligned pieces of the `traceEvents` array, blocking on the bounded
/// queue when the workers are behind and on the buffer ring when every buffer
/// is in flight. `None` on a decode error, an input the chunker can't navigate
/// (no array, no closing bracket, an over-long line), or a closed queue.
fn stream_pieces(
    reader: impl Read + Send,
    chunk_bytes: usize,
    workers: usize,
    tx: crossbeam_channel::Sender<Piece>,
    pool: std::sync::mpsc::Receiver<Vec<u8>>,
) -> Option<StreamEnds> {
    // One buffer more than the piece queue and the parsers can have in flight
    // together, so whenever the producer blocks on the pool a return is already
    // on its way.
    let ring = 2 * workers + 1;
    let mut bufs = BufferPool::new(pool, ring, chunk_bytes);
    let mut dec = MultiGzDecoder::new(reader);
    let (head, mut carry) = read_to_array_start(&mut dec, chunk_bytes)?;
    let mut sink = PieceSink { tx, sent: 0 };
    loop {
        let mut buf = bufs.take(carry.len())?;
        if fill_piece(&mut dec, &mut buf, &carry).ok()? {
            let tail = split_off_tail(&mut buf)?;
            if !buf.bytes().is_empty() {
                sink.send(buf)?;
            }
            return Some(StreamEnds {
                head,
                tail,
                pieces: sink.sent,
            });
        }
        if cut_at_newline(&mut buf, &mut carry) {
            sink.send(buf)?;
        } else if buf.bytes().len() >= PIPELINE_MAX_EVENT_BYTES {
            return None;
        } else {
            carry.clear();
            carry.extend_from_slice(buf.bytes());
            bufs.put_back(buf);
        }
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
            let producer =
                scope.spawn(move || stream_pieces(reader, chunk_bytes, workers, tx, pool_rx));
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

/// Recovers `otherData` from the stream's Latin-1 ends and seals the merge
/// into the finished profile.
fn seal_stream(
    interner: ConcurrentInterner,
    merge: Merge,
    ends: StreamEnds,
) -> Result<Profile, PoolOverflow> {
    let head = latin1_to_utf8(ends.head);
    let tail = latin1_to_utf8(ends.tail);
    let other_data = merge_other_data(recover_other_data(&head), recover_other_data(&tail));
    timed("  finish", || seal(interner, merge, other_data))
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
