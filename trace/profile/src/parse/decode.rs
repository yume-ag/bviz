use std::borrow::Cow;
use std::io::{ErrorKind, Read};

use bviz_input::ReadAt;
use bviz_invocation::Recorded;
use flate2::read::MultiGzDecoder;

use super::{GZIP_MAGIC, LoadError, timed};

/// A profile's text, cut off where a gzip stream ends before its end: then no
/// complete profile, only whole events for [`super::recover`] to take.
pub(crate) fn decode(bytes: &[u8]) -> Result<Recorded<String>, LoadError> {
    let raw = if bytes.starts_with(&GZIP_MAGIC) {
        timed("  decompress", || gunzip(bytes))?
    } else {
        Recorded::Complete(bytes.to_vec())
    };
    Ok(timed("  latin1->utf8", || raw.map(latin1_to_utf8)))
}

/// Inflates a gzip stream into one buffer, reserved from [`gzip_size_hint`] so
/// the common single-member case decodes without growth reallocations (a
/// stream the hint understates merely grows past the reservation).
fn gunzip(bytes: &[u8]) -> Result<Recorded<Vec<u8>>, LoadError> {
    let mut out = Vec::with_capacity(gzip_size_hint(bytes).unwrap_or(0));
    match MultiGzDecoder::new(bytes).read_to_end(&mut out) {
        Ok(_) => Ok(Recorded::Complete(out)),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof && !out.is_empty() => {
            Ok(Recorded::CutOff(out))
        }
        Err(e) => Err(LoadError::Decompress(e)),
    }
}

/// The gzip framing alone: a 10-byte header plus an 8-byte trailer, with no
/// deflate payload between them. Below this no ISIZE field is framed at all,
/// whatever the tail bytes hold.
const GZIP_FRAMING_MIN: usize = 18;

/// Width of the gzip ISIZE trailer field (RFC 1952).
const GZIP_ISIZE_BYTES: usize = 4;

/// Ceiling on the ratio of decoded to compressed bytes a gzipped profile is
/// taken to reach, bounding its ISIZE trailer as a sizing hint. Bazel profiles
/// typically compress well under 20:1, which this clears severalfold; a profile
/// past it still loads, at worst through the whole-buffer path.
pub(crate) const MAX_PLAUSIBLE_GZIP_RATIO: usize = 64;

/// The sizing hint a little-endian ISIZE `trailer` gives for a gzip stream of
/// `compressed_len` bytes: the declared length, bounded by
/// [`MAX_PLAUSIBLE_GZIP_RATIO`] times the compressed one. `None` for a zero
/// trailer.
fn size_hint_from_trailer(trailer: &[u8; GZIP_ISIZE_BYTES], compressed_len: u64) -> Option<usize> {
    let ceiling = usize::try_from(compressed_len)
        .unwrap_or(usize::MAX)
        .saturating_mul(MAX_PLAUSIBLE_GZIP_RATIO);
    match u32::from_le_bytes(*trailer) as usize {
        0 => None,
        n => Some(n.min(ceiling)),
    }
}

/// A hint at the uncompressed length of a gzip stream, from its ISIZE trailer.
/// Only a hint: the trailer covers the last member alone, holds the length mod
/// 2^32, and is arbitrary bytes in a truncated stream, so it is bounded by
/// [`MAX_PLAUSIBLE_GZIP_RATIO`] and never trusted as a capacity. `None` for
/// input that is not gzip, is too short to frame, or carries a zero trailer.
pub fn gzip_size_hint(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(&GZIP_MAGIC) || bytes.len() < GZIP_FRAMING_MIN {
        return None;
    }
    size_hint_from_trailer(
        &bytes[bytes.len() - GZIP_ISIZE_BYTES..].try_into().ok()?,
        bytes.len() as u64,
    )
}

/// [`gzip_size_hint`] of a gzip stream of `compressed_len` bytes, read from
/// `source` without pulling the whole stream into memory.
pub(crate) fn gzip_size_hint_at(source: &dyn ReadAt, compressed_len: u64) -> Option<usize> {
    if compressed_len < GZIP_FRAMING_MIN as u64 {
        return None;
    }
    let mut trailer = [0u8; GZIP_ISIZE_BYTES];
    source
        .read_exact_at(&mut trailer, compressed_len - GZIP_ISIZE_BYTES as u64)
        .ok()?;
    size_hint_from_trailer(&trailer, compressed_len)
}

/// Transcodes an ISO-8859-1 (Latin-1) byte buffer — how Bazel writes profile
/// strings — to UTF-8. A Bazel profile is overwhelmingly ASCII, so the all-ASCII
/// case (already valid UTF-8) borrows the buffer with no per-byte work; otherwise
/// [`latin1_expand`] rewrites it.
pub(crate) fn latin1_cow(raw: &[u8]) -> Cow<'_, str> {
    if raw.is_ascii() {
        // SAFETY: ASCII is valid UTF-8.
        return Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(raw) });
    }
    Cow::Owned(latin1_expand(raw))
}

/// [`latin1_cow`] over an owned buffer: the all-ASCII case reuses it in place,
/// otherwise it takes the expanded string.
pub(crate) fn latin1_to_utf8(raw: Vec<u8>) -> String {
    match latin1_cow(&raw) {
        Cow::Borrowed(_) => String::from_utf8(raw).expect("ascii is valid utf-8"),
        Cow::Owned(expanded) => expanded,
    }
}

/// Rewrites Latin-1 bytes as UTF-8: each leading ASCII run is copied wholesale
/// and only a byte >= 0x80 expands to the two-byte UTF-8 encoding of the same
/// code point.
fn latin1_expand(raw: &[u8]) -> String {
    // High bytes each grow by one; reserve a little slack so a profile with few
    // of them doesn't reallocate.
    let mut out = String::with_capacity(raw.len() + raw.len() / 16 + 16);
    let mut rest = raw;
    while !rest.is_empty() {
        let split = rest.iter().position(|&b| b >= 0x80).unwrap_or(rest.len());
        // SAFETY: the prefix up to the first high byte is ASCII, hence valid UTF-8.
        out.push_str(unsafe { std::str::from_utf8_unchecked(&rest[..split]) });
        rest = &rest[split..];
        if let Some((&b, tail)) = rest.split_first() {
            out.push(b as char);
            rest = tail;
        }
    }
    out
}
