use std::borrow::Cow;
use std::io::Read;

use flate2::read::MultiGzDecoder;

use crate::concurrent_interner::{ConcurrentInterner, PoolOverflow};
use crate::model::Profile;

use super::build::{Merge, seal};
use super::decode::{latin1_cow, latin1_to_utf8};
use super::pipeline::{Chunk, TraceEventsArray, array_close, find_trace_events_array};
use super::{merge_other_data, recover_other_data, timed};

pub(super) mod pipelined;

/// Read-buffer capacity for the compressed input, sized to amortize read
/// syscalls over the streamed inflate of a large profile.
const GZIP_READ_BUFFER_BYTES: usize = 1 << 20;

/// A run of decoded bytes without a newline longer than this aborts a streamed
/// decode: pieces are cut at newlines, so a profile that is not
/// one-event-per-line would otherwise grow one piece without bound. The
/// whole-buffer fallback parses such input instead.
const STREAM_MAX_EVENT_BYTES: usize = 64 << 20;

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
/// [`STREAM_MAX_EVENT_BYTES`].
fn read_to_array_start(dec: &mut impl Read, chunk_bytes: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buf = vec![0u8; chunk_bytes];
    let mut filled = fill(dec, &mut buf, 0).ok()?;
    let array_start = loop {
        if let TraceEventsArray::Found { content_start, .. } =
            find_trace_events_array(&buf[..filled])
        {
            break content_start;
        }
        if filled < buf.len() || buf.len() >= STREAM_MAX_EVENT_BYTES {
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
/// the ring once its piece is parsed, or straight to the stash when
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

/// Where [`stream_pieces`] delivers the pieces it cuts, in array order.
trait PieceSink {
    /// `None` when the piece cannot be taken, which ends the stream.
    fn send(&mut self, buf: PieceBuf) -> Option<()>;

    /// How many pieces [`Self::send`] has taken.
    fn sent(&self) -> usize;
}

/// Streams the inflate of `reader` and sends newline-aligned pieces of the
/// `traceEvents` array to `sink`, each decoded into one of a ring of `ring`
/// buffers that return through `pool` once their piece is parsed. `None` on a
/// decode error, an input the chunker can't navigate (no array, no closing
/// bracket, an over-long line), or a piece the sink refuses.
fn stream_pieces(
    reader: impl Read,
    chunk_bytes: usize,
    ring: usize,
    sink: &mut impl PieceSink,
    pool: std::sync::mpsc::Receiver<Vec<u8>>,
) -> Option<StreamEnds> {
    let mut bufs = BufferPool::new(pool, ring, chunk_bytes);
    let mut dec = MultiGzDecoder::new(reader);
    let (head, mut carry) = read_to_array_start(&mut dec, chunk_bytes)?;
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
                pieces: sink.sent(),
            });
        }
        if cut_at_newline(&mut buf, &mut carry) {
            sink.send(buf)?;
        } else if buf.bytes().len() >= STREAM_MAX_EVENT_BYTES {
            return None;
        } else {
            carry.clear();
            carry.extend_from_slice(buf.bytes());
            bufs.put_back(buf);
        }
    }
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
