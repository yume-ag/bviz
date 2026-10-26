use std::io::Read;

use bviz_input::Input;
use bviz_invocation::Recorded;

use crate::concurrent_interner::{ConcurrentInterner, Writer};
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

/// The serial decode of a gzipped profile held whole in memory, sized from
/// that buffer's own [`gzip_size_hint`]. `None` — leaving the input to the
/// whole-buffer path — for a missing or zero trailer, a profile decoding to
/// under [`MIN_STREAMED_BYTES`], or what [`parse_gz_serial_into`] declines.
pub(crate) fn parse_gz_serial(bytes: &[u8]) -> Option<Recorded<Profile>> {
    let decoded_size = streamed(gzip_size_hint(bytes)?)?;
    timed("decode+parse (serial)", || {
        parse_gz_serial_from(bytes, decoded_size)
    })
}

/// The serial decode streamed from a seekable gzipped `input`, sized from its
/// trailer. `None` for an input it declines, as [`parse_gz_serial`] declines
/// one.
pub(crate) fn parse_gz_serial_input(input: &Input) -> Option<Recorded<Profile>> {
    let (reader, decoded_size) = gzip_input(input)?;
    let decoded_size = streamed(decoded_size)?;
    timed("decode+parse (serial, streamed)", || {
        parse_gz_serial_from(reader, decoded_size)
    })
}

/// Parses a stream declaring `decoded_size` uncompressed bytes into a string
/// pool sized from it.
fn parse_gz_serial_from(reader: impl Read, decoded_size: usize) -> Option<Recorded<Profile>> {
    let interner = ConcurrentInterner::new(decoded_size, decoded_size / BYTES_PER_DISTINCT).ok()?;
    parse_gz_serial_into(reader, interner, SERIAL_PIECE_BYTES, decoded_size)
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
    fn fold(&mut self, builder: Builder, text_bytes: usize) {
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
/// path recovers it. `None` if the decode or any piece's parse fails, if no
/// event was read at all — an empty `traceEvents` array — if the text past the
/// last piece of a cut-off stream fails to recover, or if the distinct text
/// outgrows `interner`'s pool.
pub(crate) fn parse_gz_serial_into(
    reader: impl Read,
    interner: ConcurrentInterner,
    chunk_bytes: usize,
    decoded_size: usize,
) -> Option<Recorded<Profile>> {
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
    let end = timed("  decode+parse+fold chunks", || {
        stream_pieces(reader, chunk_bytes, ring, &mut sink, pool_rx)
    })?;
    match end {
        StreamEnd::Closed(ends) => {
            if ends.pieces == 0 {
                return None;
            }
            let merge = sink.into_merge();
            seal_stream(interner, merge, ends)
                .ok()
                .map(Recorded::Complete)
        }
        StreamEnd::CutOff { head, rest } => {
            sink.fold_cut_off(&rest)?;
            let merge = sink.into_merge();
            let other_data = recover_other_data(&latin1_to_utf8(head));
            timed("  finish", || seal(interner, merge, other_data))
                .ok()
                .map(Recorded::CutOff)
        }
    }
}
