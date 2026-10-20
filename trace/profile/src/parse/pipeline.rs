use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;

use bviz_parallel::worker_count;
use flate2::read::MultiGzDecoder;
use serde::de::{DeserializeSeed, IgnoredAny};

use crate::concurrent_interner::{ConcurrentInterner, PoolOverflow, Writer};
use crate::model::Profile;

use super::build::{Builder, Merge, seal};
use super::decode::{gzip_size_hint, latin1_cow, latin1_to_utf8};
use super::scanner::scan_event;
use super::serde_events::{ArgEntry, RawEventSeed};
use super::{
    BYTES_PER_DISTINCT, BYTES_PER_SLICE, intern_prefault_bytes, merge_other_data,
    recover_other_data, timed,
};

/// Outcome of locating the `traceEvents` array in a decoded profile buffer.
pub(crate) enum TraceEventsArray {
    /// No `"traceEvents"` key in the buffer.
    NoKey,
    /// The key is present but no `[` opens an array after it.
    NoArray,
    Found {
        /// Byte index of the `"traceEvents"` key's opening quote.
        key_at: usize,
        /// Byte index just past the `[`, at the start of the array's contents.
        content_start: usize,
    },
}

const TRACE_EVENTS_KEY: &[u8] = b"\"traceEvents\"";

/// Byte index of the `"traceEvents"` key's opening quote.
pub(crate) fn find_trace_events_key(bytes: &[u8]) -> Option<usize> {
    find_bytes(bytes, TRACE_EVENTS_KEY)
}

/// Locates the `traceEvents` array: the `"traceEvents"` key, then the first `[`
/// after it.
pub(crate) fn find_trace_events_array(bytes: &[u8]) -> TraceEventsArray {
    let Some(key_at) = find_trace_events_key(bytes) else {
        return TraceEventsArray::NoKey;
    };
    let after_key = key_at + TRACE_EVENTS_KEY.len();
    match bytes[after_key..].iter().position(|&b| b == b'[') {
        Some(bracket) => TraceEventsArray::Found {
            key_at,
            content_start: after_key + bracket + 1,
        },
        None => TraceEventsArray::NoArray,
    }
}

/// Below this many bytes of `traceEvents` the parallel parser's thread and merge
/// overhead outweighs the gain, so [`parse_parallel`] declines and the serial
/// path runs.
const MIN_PARALLEL_BYTES: usize = 8 << 20;

/// Parses the `traceEvents` array across several threads when the profile is
/// large enough to repay it, returning `None` so the caller runs the serial
/// [`parse_streaming`] for a small input, a single core, or any input whose
/// shape the chunker can't navigate (no array, no usable split points, a chunk
/// that fails to parse).
pub(crate) fn parse_parallel(text: &str) -> Option<Profile> {
    let workers = worker_count();
    if workers < 2 {
        return None;
    }
    parse_chunks(text, workers, MIN_PARALLEL_BYTES)
}

/// How many chunks each worker gets on average. Event density varies across a
/// profile, so equal-byte chunks parse in unequal time; finer chunks pulled
/// from a shared queue bound the idle tail at roughly one chunk's parse time.
const CHUNKS_PER_WORKER: usize = 8;

/// Floor on chunk size: below this, per-chunk builder overhead (maps, pools)
/// outweighs the balancing gain from finer chunks.
const MIN_CHUNK_BYTES: usize = 4 << 20;

/// One chunk handed to a worker: its bytes as text, plus the cleanup owed once
/// the parse has read them. The pre-split path borrows the shared buffer and
/// owes nothing; the streamed path hands its buffer back to the producer to
/// refill.
trait Chunk {
    fn text(&self) -> Cow<'_, str>;
    fn release(self);
}

impl Chunk for &str {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(self)
    }
    fn release(self) {}
}

/// Parses one chunk into a fresh [`Builder`] on a worker. Returns `None` on a
/// parse failure, and without parsing once the string pool is exhausted, since
/// that pool can no longer be sealed.
///
/// Both of the builder's growing buffers are pre-sized: `staging` from the
/// chunk's byte length (via [`BYTES_PER_SLICE`]), the arg pool from `args_cap`,
/// the previous chunk's arg count — a fair estimate, since chunks hold
/// near-equal bytes. `args_cap` is updated for the next chunk. The buffers
/// themselves cannot be reused across chunks; the merge consumes them as the
/// builder's live data.
///
/// The chunk is released between the parse and the seal — everything the
/// builder keeps was interned during the parse — so a streamed chunk's buffer
/// returns to the producer's ring without the inflate waiting out the seal's
/// per-lane sorts.
fn parse_chunk(chunk: impl Chunk, writer: &mut Writer, args_cap: &mut usize) -> Option<Builder> {
    if writer.pool_exhausted() {
        chunk.release();
        return None;
    }
    let mut builder = Builder::new();
    builder.args_pool.reserve(*args_cap);
    let ok = {
        let text = chunk.text();
        builder.staging.reserve(text.len() / BYTES_PER_SLICE);
        let ok = parse_range_into(&mut builder, writer, &text);
        *args_cap = builder.args_pool.len();
        ok
    };
    chunk.release();
    if ok {
        builder.seal_slices();
    }
    ok.then_some(builder)
}

/// A decoded profile buffer cut twice: `head` ends where the `traceEvents`
/// array's contents begin and `tail` begins at the array's closing `]`. The
/// events between are bare array content, so no chunk of them picks a bracket
/// up as event text, and every byte outside the array falls whole into one end
/// or the other.
struct EventRegion<'a> {
    head: &'a str,
    events: &'a str,
    tail: &'a str,
}

/// Cuts `text` around its `traceEvents` array. `None` when no array can be
/// located, or when its close does not lie past the start of its contents — an
/// empty array, or a `]` ahead of the `[`.
fn locate_event_region(text: &str) -> Option<EventRegion<'_>> {
    let TraceEventsArray::Found {
        content_start: array_start,
        ..
    } = find_trace_events_array(text.as_bytes())
    else {
        return None;
    };
    let array_end = array_close(text.as_bytes())?;
    if array_end <= array_start {
        return None;
    }
    Some(EventRegion {
        head: &text[..array_start],
        events: &text[array_start..array_end],
        tail: &text[array_end..],
    })
}

/// Splits `region` into byte chunks for `workers` parse workers, cutting at
/// the first newline at or after each evenly spaced target: a literal newline
/// never falls inside a JSON string, so it always lies between two events and a
/// chunk holds only whole events. A target is a byte offset that can land inside
/// a multi-byte character, so it is advanced to the next character boundary
/// before the search; every bound then lands one past a newline, and so on a
/// boundary as well. `None` when the cuts yield a single chunk, which leaves
/// nothing to parallelize.
fn cut_chunks(region: &str, workers: usize) -> Option<Vec<&str>> {
    let target_chunks = (workers * CHUNKS_PER_WORKER)
        .min(region.len() / MIN_CHUNK_BYTES)
        .max(workers);
    let mut bounds = vec![0usize];
    for k in 1..target_chunks {
        let mut target = region.len() * k / target_chunks;
        while !region.is_char_boundary(target) {
            target += 1;
        }
        if let Some(off) = region[target..].find('\n') {
            let cut = target + off + 1;
            if cut > *bounds.last().unwrap() {
                bounds.push(cut);
            }
        }
    }
    bounds.push(region.len());
    if bounds.len() < 3 {
        return None;
    }
    Some(bounds.windows(2).map(|w| &region[w[0]..w[1]]).collect())
}

/// Parses each chunk into its own [`Builder`] on a pool of up to `workers`
/// threads stealing chunks from a shared counter, all of them interning into
/// `interner`, and returns the builders in array order. `None` if any chunk
/// fails to parse.
fn parse_chunks_on_workers(
    chunks: &[&str],
    workers: usize,
    interner: &ConcurrentInterner,
    prefault_bytes: usize,
) -> Option<Vec<Builder>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let spawned = workers.min(chunks.len());
    let parsed: Vec<(usize, Option<Builder>)> = bviz_parallel::fan_out(spawned, |shard| {
        // Fault this worker's arena share up front, in parallel with the
        // others, before entering the steal loop.
        interner.prefault_shard(shard, spawned, prefault_bytes);
        let mut writer = interner.writer();
        let mut parsed = Vec::new();
        let mut args_cap = 0;
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(&chunk) = chunks.get(i) else { break };
            let builder = parse_chunk(chunk, &mut writer, &mut args_cap);
            parsed.push((i, builder));
        }
        parsed
    })
    .into_iter()
    .flatten()
    .collect();
    // Chunks were stolen out of order; restore array order for the merge.
    let mut builders: Vec<Option<Builder>> = Vec::new();
    builders.resize_with(chunks.len(), || None);
    for (i, b) in parsed {
        builders[i] = b;
    }
    builders.into_iter().collect()
}

/// Recovers `otherData` from the text on either side of the array and folds the
/// builders into the finished profile.
fn seal_chunks(
    interner: ConcurrentInterner,
    builders: Vec<Builder>,
    head: &str,
    tail: &str,
) -> Result<Profile, PoolOverflow> {
    let other_data = merge_other_data(recover_other_data(head), recover_other_data(tail));
    let mut merge = Merge::with_capacity(
        builders.len(),
        builders.iter().map(|b| b.args_pool.len()).sum(),
    );
    for builder in builders {
        merge.fold(builder);
    }
    seal(interner, merge, other_data)
}

/// Parses the `traceEvents` array of `text` into a profile on `workers`
/// threads. `min_bytes` is the array size below which it declines; it also
/// returns `None` for an array it cannot locate or split, for a chunk that
/// fails to parse, and for distinct text the string pool cannot hold.
pub(crate) fn parse_chunks(text: &str, workers: usize, min_bytes: usize) -> Option<Profile> {
    let EventRegion { head, events, tail } = locate_event_region(text)?;
    if events.len() < min_bytes {
        return None;
    }
    let chunks = cut_chunks(events, workers)?;
    let interner = ConcurrentInterner::new(events.len(), events.len() / BYTES_PER_DISTINCT).ok()?;
    let prefault_bytes = intern_prefault_bytes(events.len());
    let builders = timed("  parse chunks", || {
        parse_chunks_on_workers(&chunks, workers, &interner, prefault_bytes)
    })?;
    timed("  assemble", || seal_chunks(interner, builders, head, tail)).ok()
}

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

/// The pipelined decode over a streaming reader whose size hint was read ahead
/// of the stream: [`parse_gz_pipelined`] without the whole compressed
/// buffer in hand. `None` for the same size gates.
pub(crate) fn parse_gz_pipelined_reader(
    reader: impl Read + Send,
    decoded_size: usize,
) -> Option<Profile> {
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

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Byte index of the `traceEvents` array's closing bracket, taken to be the
/// last `]` — an assumption about the input, holding while no value outside
/// the array carries a bracket, as the flat string maps of `otherData` do not.
/// A bracket that breaks it can only sit past the true close, so the array's own
/// `]` is left among the events, where [`parse_range_into`] rejects it.
fn array_close(bytes: &[u8]) -> Option<usize> {
    bytes.iter().rposition(|&b| b == b']')
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

/// Parses every `{...}` event object in one chunk of the `traceEvents` array
/// into `builder`, skipping the separators between them by hand: a chunk holds
/// bare array content, so it is not itself a JSON document.
///
/// Each event goes through [`scan_event`], with serde deserialization from the
/// same offset as the fallback for whatever the scanner defers. Returns `false`
/// on malformed content, so the caller falls back to the serial parser rather
/// than silently dropping events.
fn parse_range_into(builder: &mut Builder, interner: &mut Writer, chunk: &str) -> bool {
    let bytes = chunk.as_bytes();
    let mut scratch: Vec<ArgEntry> = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        match bytes[pos] {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => pos += 1,
            b'{' => {
                if let Some((ev, end)) = scan_event(chunk, pos, &mut scratch) {
                    builder.event(interner, ev, &scratch);
                    pos = end;
                    continue;
                }
                let mut de = serde_json::Deserializer::from_str(&chunk[pos..]);
                match RawEventSeed(&mut scratch).deserialize(&mut de) {
                    Ok(ev) => {
                        builder.event(interner, ev, &scratch);
                        // The deserializer doesn't expose its consumed-byte
                        // count directly, but converting it into a stream
                        // iterator carries the count over to `byte_offset`.
                        pos += de.into_iter::<IgnoredAny>().byte_offset();
                    }
                    Err(_) => return false,
                }
            }
            _ => return false,
        }
    }
    true
}
