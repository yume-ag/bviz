use std::borrow::Cow;

use bviz_parallel::worker_count;
use serde::de::{DeserializeSeed, IgnoredAny};

use crate::concurrent_interner::{ConcurrentInterner, PoolOverflow, Writer};
use crate::model::Profile;

use super::build::{Builder, Merge, seal};
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
pub(super) const MIN_PARALLEL_BYTES: usize = 8 << 20;

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
pub(super) const CHUNKS_PER_WORKER: usize = 8;

/// Floor on chunk size: below this, per-chunk builder overhead (maps, pools)
/// outweighs the balancing gain from finer chunks.
pub(super) const MIN_CHUNK_BYTES: usize = 4 << 20;

/// One chunk handed to a worker: its bytes as text, plus the cleanup owed once
/// the parse has read them. The pre-split path borrows the shared buffer and
/// owes nothing; the streamed path hands its buffer back to the producer to
/// refill.
pub(super) trait Chunk {
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
pub(super) fn parse_chunk(
    chunk: impl Chunk,
    writer: &mut Writer,
    args_cap: &mut usize,
) -> Option<Builder> {
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

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Byte index of the `traceEvents` array's closing bracket, taken to be the
/// last `]` — an assumption about the input, holding while no value outside
/// the array carries a bracket, as the flat string maps of `otherData` do not.
/// A bracket that breaks it can only sit past the true close, so the array's own
/// `]` is left among the events, where [`parse_range_into`] rejects it.
pub(super) fn array_close(bytes: &[u8]) -> Option<usize> {
    bytes.iter().rposition(|&b| b == b']')
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
