use std::io::Read;

use bviz_input::Input;
use bviz_invocation::Recorded;

use crate::concurrent_interner::{ConcurrentInterner, PoolOverflow, Writer};
use crate::model::Profile;
use crate::parse::build::{Builder, Merge, seal};
use crate::parse::decode::{gzip_size_hint, latin1_cow, latin1_to_utf8};
use crate::parse::pipeline::{MIN_PARALLEL_BYTES, parse_chunk};
use crate::parse::{BYTES_PER_DISTINCT, recover_events, recover_other_data, timed};

use super::{PieceBuf, PieceSink, StreamChunk, StreamEnd, gzip_input, seal_stream, stream_pieces};

/// A profile decoding to fewer bytes is parsed whole, as the builds with
/// threads parse one under their parallel threshold: the whole-buffer parse
/// reads it strictly as one JSON document, so a short profile is accepted or
/// refused alike on every build, and holding so short a text whole costs
/// little.
pub(crate) const MIN_STREAMED_BYTES: usize = MIN_PARALLEL_BYTES;

/// Decoded bytes per piece. A piece's buffer and the staging of its parse are
/// all the decode holds of the text, so a piece is a small part of any profile
/// streamed, while large enough that each piece's builder costs little beside
/// its events.
const SERIAL_PIECE_BYTES: usize = 4 << 20;

/// `decoded_size`, for a profile streamed at that size.
fn streamed(decoded_size: usize) -> Option<usize> {
    (decoded_size >= MIN_STREAMED_BYTES).then_some(decoded_size)
}

/// Share of a profile's decoded bytes the serial decode's string pool first
/// holds, as [`POOL_SHARE_NUM`]/[`POOL_SHARE_DEN`]: WebAssembly memory commits
/// every byte of a pool, so it is sized from the distinct-text density
/// [`INTERN_PREFAULT_NUM`](crate::parse::INTERN_PREFAULT_NUM) records, with
/// headroom. A profile whose distinct text outgrows it is decoded again into a
/// pool that holds every decoded byte.
const POOL_SHARE_NUM: usize = 1;
const POOL_SHARE_DEN: usize = 2;

/// The serial decode of a gzipped profile held whole in memory, sized from
/// that buffer's own [`gzip_size_hint`]. `None` — leaving the input to the
/// whole-buffer path — for a missing or zero trailer, a profile decoding to
/// under [`MIN_STREAMED_BYTES`], or what [`parse_gz_serial_into`] declines.
pub(crate) fn parse_gz_serial(bytes: &[u8]) -> Option<Recorded<Profile>> {
    let decoded_size = streamed(gzip_size_hint(bytes)?)?;
    timed("decode+parse (serial)", || {
        parse_gz_serial_from(bytes, || Some(bytes), decoded_size)
    })
}

/// The serial decode streamed from a seekable gzipped `input`, sized from its
/// trailer. `None` for an input it declines, as [`parse_gz_serial`] declines
/// one.
pub(crate) fn parse_gz_serial_input(input: &Input) -> Option<Recorded<Profile>> {
    let (reader, decoded_size) = gzip_input(input)?;
    let decoded_size = streamed(decoded_size)?;
    let reopen = || Some(gzip_input(input)?.0);
    timed("decode+parse (serial, streamed)", || {
        parse_gz_serial_from(reader, reopen, decoded_size)
    })
}

/// Parses `reader`, a stream declaring `decoded_size` uncompressed bytes, into
/// a string pool holding [`POOL_SHARE_NUM`]/[`POOL_SHARE_DEN`] of them, and
/// the stream `reopen` gives into one holding them all when that outgrows it.
fn parse_gz_serial_from<R: Read>(
    reader: R,
    reopen: impl FnOnce() -> Option<R>,
    decoded_size: usize,
) -> Option<Recorded<Profile>> {
    parse_gz_serial_retrying(
        reader,
        reopen,
        first_pool(decoded_size)?,
        || pool(decoded_size, decoded_size),
        SERIAL_PIECE_BYTES,
        decoded_size,
    )
}

/// The pool a serial decode of `decoded_size` bytes interns into first.
pub(crate) fn first_pool(decoded_size: usize) -> Option<ConcurrentInterner> {
    pool(decoded_size / POOL_SHARE_DEN * POOL_SHARE_NUM, decoded_size)
}

/// A pool of `text_bytes` for a decode of `decoded_size` bytes.
fn pool(text_bytes: usize, decoded_size: usize) -> Option<ConcurrentInterner> {
    ConcurrentInterner::new(text_bytes, decoded_size / BYTES_PER_DISTINCT).ok()
}

/// Parses `reader` into the `first` pool and, if its distinct text outgrows
/// that, the stream `reopen` gives into the pool `full` makes.
pub(crate) fn parse_gz_serial_retrying<R: Read>(
    reader: R,
    reopen: impl FnOnce() -> Option<R>,
    first: ConcurrentInterner,
    full: impl FnOnce() -> Option<ConcurrentInterner>,
    chunk_bytes: usize,
    decoded_size: usize,
) -> Option<Recorded<Profile>> {
    match parse_gz_serial_into(reader, first, chunk_bytes, decoded_size) {
        Err(Declined::PoolExhausted) => {
            parse_gz_serial_into(reopen()?, full()?, chunk_bytes, decoded_size).ok()
        }
        parsed => parsed.ok(),
    }
}

/// Why the serial decode leaves a stream to the whole-buffer path.
pub(crate) enum Declined {
    /// The distinct text outgrew the string pool.
    PoolExhausted,
    /// A decode or parse failure, an empty `traceEvents` array, a cut-off text
    /// that fails to recover, or a profile whose seal overflows the pool's
    /// handles ([`PoolOverflow::TooLarge`]).
    Unparsed,
}

impl Declined {
    /// Why a decode into `interner` that stopped short declines.
    fn stopped(interner: &ConcurrentInterner) -> Declined {
        if interner.exhausted() {
            Declined::PoolExhausted
        } else {
            Declined::Unparsed
        }
    }
}

/// Parses each piece as it is cut and folds its builder into the merge at
/// once, so pieces arrive and fold in array order on the one thread.
struct FoldSink<'i> {
    writer: Writer<'i>,
    pool: std::sync::mpsc::Sender<Vec<u8>>,
    args_cap: usize,
    merge: Merge,
    sent: usize,
    decoded_size: usize,
}

impl FoldSink<'_> {
    /// Folds the builder parsed from `text_bytes` decoded bytes. The first
    /// reserves the merged args pool for the whole stream at its args density.
    fn fold(&mut self, mut builder: Builder, text_bytes: usize) {
        // The folded run holds the staging buffer until the seal.
        builder.staging.shrink_to_fit();
        if self.sent == 0 {
            let projected = builder.args_pool.len() as u128 * self.decoded_size as u128
                / text_bytes.max(1) as u128;
            self.merge
                .reserve_args(usize::try_from(projected).unwrap_or(usize::MAX));
        }
        self.merge.fold(builder);
        self.sent += 1;
    }

    /// Folds the whole events of `rest`, the text a cut-off stream leaves past
    /// its last piece, as [`recover_events`] takes them from a cut-off
    /// profile. `None` when that text fails to recover, or the stream yielded
    /// no event at all.
    fn fold_cut_off(&mut self, rest: &PieceBuf) -> Option<()> {
        let mut builder = Builder::new();
        let text = latin1_cow(rest.bytes());
        let (recovered, _) = recover_events(&text, 0, &mut builder, &mut self.writer).ok()?;
        if recovered {
            builder.seal_slices();
            self.fold(builder, rest.bytes().len());
        }
        (self.sent > 0).then_some(())
    }

    fn into_merge(self) -> Merge {
        self.merge
    }
}

impl PieceSink for FoldSink<'_> {
    const RELEASES_ON_SEND: bool = true;

    /// `None` when the piece fails to parse.
    fn send(&mut self, buf: PieceBuf) -> Option<()> {
        let text_bytes = buf.bytes().len();
        let chunk = StreamChunk {
            buf,
            pool: &self.pool,
        };
        let builder = parse_chunk(chunk, &mut self.writer, &mut self.args_cap)?;
        self.fold(builder, text_bytes);
        Some(())
    }

    fn sent(&self) -> usize {
        self.sent
    }
}

/// Decodes, parses and folds a stream declaring `decoded_size` uncompressed
/// bytes in turn on the calling thread, in pieces of about `chunk_bytes`
/// decoded into one reused buffer, so no more of the decoded text than one
/// piece is held at a time. Text that stops inside the `traceEvents` array
/// loads cut off, with the whole events before the cut, as the whole-buffer
/// path recovers it.
pub(crate) fn parse_gz_serial_into(
    reader: impl Read,
    interner: ConcurrentInterner,
    chunk_bytes: usize,
    decoded_size: usize,
) -> Result<Recorded<Profile>, Declined> {
    let (pool_tx, pool_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut sink = FoldSink {
        writer: interner.writer(),
        pool: pool_tx,
        args_cap: 0,
        merge: Merge::with_capacity(decoded_size.div_ceil(chunk_bytes), 0),
        sent: 0,
        decoded_size,
    };
    // Each piece is parsed, returning its buffer, before the next is cut.
    let ring = 1;
    let streamed = timed("  decode+parse+fold chunks", || {
        stream_pieces(reader, chunk_bytes, ring, &mut sink, pool_rx)
    });
    let Some(end) = streamed else {
        return Err(Declined::stopped(&interner));
    };
    let sealed = match end {
        StreamEnd::Closed(ends) => {
            if ends.pieces == 0 {
                return Err(Declined::Unparsed);
            }
            let merge = sink.into_merge();
            seal_stream(interner, merge, ends).map(Recorded::Complete)
        }
        StreamEnd::CutOff { head, rest } => {
            if sink.fold_cut_off(&rest).is_none() {
                return Err(Declined::stopped(&interner));
            }
            let merge = sink.into_merge();
            let other_data = recover_other_data(&latin1_to_utf8(head));
            timed("  finish", || seal(interner, merge, other_data)).map(Recorded::CutOff)
        }
    };
    sealed.map_err(|overflow| match overflow {
        PoolOverflow::Exhausted => Declined::PoolExhausted,
        PoolOverflow::TooLarge => Declined::Unparsed,
    })
}
